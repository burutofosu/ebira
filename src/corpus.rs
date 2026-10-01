use crate::core::{
    classify_sender, record_meta, select_body, CanonicalEvent, EventKind, IngestCheckpoint,
    IngestStateMachine, RecordMeta, SessionTurn, SourceOrigin, TimelineBucket, TimelineRef,
    TurnReducerState,
};
use crate::format::{
    decode_token, encode_token, event_header_line, parse_event_header, push_field, EventHeader,
};
use crate::json;
use crate::jsonl::{parse_record, Field};
use crate::private_fs;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::UNIX_EPOCH;

const IO_BUFFER_SIZE: usize = 1024 * 1024;
const FLUSH_RECORD_INTERVAL: u64 = 4096;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

/// One line of a log, as `read_bounded_line` read it.
pub struct LineRead {
    /// The length of the whole line, its newline included.
    pub total_len: u64,
    /// The line ends with a newline; the last line of a log being written may not yet.
    pub complete: bool,
    /// The line is longer than a record can be; only its first `MAX_RECORD_BYTES` were kept.
    pub oversized: bool,
}

struct SourceRead<'a> {
    entry: &'a SourceEntry,
    old: Option<&'a SourceEntry>,
    active_files: Vec<PathBuf>,
    operation_disposition: &'static str,
    appended: bool,
}

#[derive(Clone, Debug, Default)]
pub struct BuildReport {
    pub disposition: &'static str,
    pub sources_seen: u64,
    pub sources_processed: u64,
    pub sources_appended: u64,
    pub sources_reused: u64,
    pub records: u64,
    pub invalid_records: u64,
    pub partial_records: u64,
    pub timeline_runs: u64,
    pub corpus_bytes: u64,
    pub unreadable_source_paths: u64,
    pub changed_sources: Vec<SourceChange>,
    pub changed_sources_total: u64,
    pub corpus: String,
}

#[derive(Clone, Debug, Default)]
pub struct SourceChange {
    pub source_id: String,
    pub disposition: String,
    pub previous_byte_end: u64,
    pub committed_byte_end: u64,
    pub observed_size: u64,
    pub records_added: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceAvailability {
    pub input_path: String,
    pub observed_path: String,
    pub disposition: String,
    pub source_id: String,
}

#[derive(Clone, Debug, Default)]
pub struct SourceAvailabilitySnapshot {
    pub observed_at_ms: u64,
    pub entries: Vec<SourceAvailability>,
}

#[derive(Default)]
struct SourceCollection {
    paths: Vec<PathBuf>,
    availability: Vec<SourceAvailability>,
}

#[derive(Clone, Debug, Default)]
pub struct SourceEntry {
    pub source_id: String,
    /// `claude`, `codex`, or `other`: the agent whose log this is.
    pub app: String,
    pub path: String,
    pub fallback_session: String,
    pub size: u64,
    pub modified_ms: u64,
    pub committed_byte_end: u64,
    pub event_count: u64,
    pub last_line: u64,
    pub last_session: String,
    pub turn_sessions: String,
    pub turn_turns: String,
    pub turn_has_user: String,
    pub disposition: String,
    pub checkpoint_valid: bool,
    pub output_preview: u64,
    pub volume_serial_number: u64,
    pub file_id: u64,
    pub synced_at_ms: u64,
    /// The working directory in effect at the checkpoint, carried into appended records.
    pub last_cwd: String,
}

#[derive(Clone, Debug, Default)]
pub struct TimelineRun {
    pub source_id: String,
    pub date: String,
    pub corpus_file: String,
    pub corpus_start: u64,
    pub corpus_end: u64,
    pub bucket: TimelineBucket,
}

#[derive(Default)]
struct SourceReport {
    entry: SourceEntry,
    records: u64,
    invalid_records: u64,
    partial_records: u64,
    corpus_bytes: u64,
    timeline_runs: Vec<TimelineRun>,
}

#[cfg(test)]
pub fn build(
    sources: &[String],
    corpus: &Path,
    incremental: bool,
    output_preview: usize,
    force_paths: &[String],
) -> io::Result<BuildReport> {
    build_with_preview(
        sources,
        corpus,
        incremental,
        Some(output_preview),
        force_paths,
    )
}

/// The sources `--rebuild-source` names: a log, or every log under a directory, among those
/// this sync reads. A path that names none of them is an error, not a rebuild of nothing.
fn forced_source_ids(force_paths: &[String], snapshot: &[SourceEntry]) -> io::Result<Vec<String>> {
    let mut ids = Vec::new();
    for path in force_paths {
        let named = fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
        let before = ids.len();
        ids.extend(
            snapshot
                .iter()
                .filter(|entry| Path::new(&entry.path).starts_with(&named))
                .map(|entry| entry.source_id.clone()),
        );
        if ids.len() == before {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "--rebuild-source names no log that this sync reads: {}",
                    path
                ),
            ));
        }
    }
    Ok(ids)
}

pub fn build_with_preview(
    sources: &[String],
    corpus: &Path,
    incremental: bool,
    requested_preview: Option<usize>,
    force_paths: &[String],
) -> io::Result<BuildReport> {
    if !incremental {
        remove_legacy_segments(corpus)?;
    }
    let mut registered_inputs = if incremental {
        load_source_inputs(corpus)?
    } else {
        Vec::new()
    };
    for input in sources {
        let input = normalize_source_input(input);
        if !registered_inputs.contains(&input) {
            registered_inputs.push(input);
        }
    }
    let previous = if incremental {
        load_source_catalog(corpus)?
    } else {
        BTreeMap::new()
    };
    let previous_availability = if incremental {
        load_source_availability(corpus)?
    } else {
        SourceAvailabilitySnapshot::default()
    };
    let mut collection = collect_sources(sources);
    attach_source_ids(&mut collection.availability, &collection.paths, &previous)?;
    let availability = merge_source_availability(
        previous_availability,
        collection.availability,
        sources,
        &registered_inputs,
    );
    if !collection.paths.is_empty() || (incremental && corpus.is_dir()) {
        private_fs::create_dir_all(corpus)?;
        write_source_availability(corpus, &availability)?;
    }
    let unreadable_source_paths = availability_entries_for_inputs(&availability, sources)
        .filter(|entry| source_path_is_unavailable(entry))
        .count();
    let source_paths = collection.paths;
    if source_paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no JSONL source files found",
        ));
    }

    let previous_timeline = if incremental {
        load_timeline_catalog(corpus)?
    } else {
        BTreeMap::new()
    };

    let snapshot = source_paths
        .iter()
        .filter_map(|path| source_entry_for_preview(path, requested_preview, &previous).transpose())
        .collect::<io::Result<Vec<_>>>()?;
    let segment_dir = segment_dir(corpus);
    private_fs::create_dir_all(corpus)?;
    private_fs::create_dir_all(&segment_dir)?;
    write_source_inputs_partial(corpus, &registered_inputs)?;
    let mut existing_files = corpus_file_map(&segment_dir)?;
    for files in existing_files.values_mut() {
        discard_partial_corpus_files(files)?;
    }
    let forced_ids = forced_source_ids(force_paths, &snapshot)?;
    // The date offset is fixed when the corpus is built: an incremental sync keeps the one in
    // the catalog, so every segment of one corpus puts records on the same dates.
    let offset_minutes = if incremental && !previous.is_empty() {
        corpus_offset_minutes(corpus)?
    } else {
        crate::time::configured_offset_minutes()?
    };
    let catalog_path = corpus.join("sources.tsv");
    let catalog_partial_path = corpus.join("sources.tsv.partial");
    let mut timeline_by_source = if incremental {
        previous_timeline.clone()
    } else {
        BTreeMap::<String, Vec<TimelineRun>>::new()
    };
    let mut catalog =
        BufWriter::with_capacity(IO_BUFFER_SIZE, private_fs::create(&catalog_partial_path)?);
    writeln!(catalog, "{}", source_catalog_header(offset_minutes))?;
    let snapshot_ids = snapshot
        .iter()
        .map(|entry| entry.source_id.as_str())
        .collect::<BTreeSet<_>>();
    // A source that could not be listed or read this time keeps its projection: it is reported
    // as unreadable, not taken for removed. A source that is gone is dropped.
    let unreadable = availability
        .entries
        .iter()
        .filter(|entry| entry.disposition == "unreadable")
        .map(|entry| PathBuf::from(&entry.observed_path))
        .collect::<Vec<_>>();
    for entry in previous.values() {
        let unreadable_now = unreadable
            .iter()
            .any(|root| Path::new(&entry.path).starts_with(root));
        if !snapshot_ids.contains(entry.source_id.as_str())
            && (unreadable_now || !source_input_covers_entry(sources, entry))
        {
            write_source_entry(&mut catalog, entry)?;
        }
    }
    for entry in &snapshot {
        write_source_entry(&mut catalog, entry)?;
    }
    catalog.flush()?;
    let mut report = BuildReport {
        disposition: "complete",
        sources_seen: snapshot.len() as u64,
        unreadable_source_paths: unreadable_source_paths as u64,
        corpus: corpus.to_string_lossy().into_owned(),
        ..BuildReport::default()
    };

    let mut reads: Vec<SourceRead> = Vec::new();
    for entry in snapshot.iter() {
        let old = previous.get(&entry.source_id);
        let active_files = existing_files
            .get(&entry.source_id)
            .cloned()
            .unwrap_or_default();
        let force_rebuild = forced_ids
            .iter()
            .any(|source_id| source_id == &entry.source_id);
        let preview_matches = old
            .map(|old| old.output_preview == entry.output_preview)
            .unwrap_or(false);
        let reuse_candidate = incremental
            && !force_rebuild
            && old
                .filter(|old| {
                    old.checkpoint_valid
                        && preview_matches
                        && same_snapshot(old, entry)
                        && old.committed_byte_end == entry.size
                        && !active_files.is_empty()
                })
                .is_some();
        let reusable = match old.filter(|_| reuse_candidate) {
            Some(old) => source_projection_complete(old, &active_files)?,
            None => false,
        };
        let append_candidate = incremental
            && !force_rebuild
            && old
                .filter(|old| {
                    old.checkpoint_valid
                        && preview_matches
                        && can_append(old, entry)
                        && !active_files.is_empty()
                })
                .is_some();
        let appendable = match old.filter(|_| !reusable && append_candidate) {
            Some(old) => {
                source_projection_complete(old, &active_files)?
                    && committed_end_is_record_boundary(&entry.path, old.committed_byte_end)?
            }
            None => false,
        };

        if reusable {
            let mut reused = old.cloned().expect("reusable source has catalog entry");
            reused.size = entry.size;
            reused.modified_ms = entry.modified_ms;
            reused.synced_at_ms = now_ms();
            write_source_entry(&mut catalog, &reused)?;
            let mut runs = previous_timeline
                .get(&entry.source_id)
                .cloned()
                .unwrap_or_default();
            reconcile_timeline_runs(&mut runs, &active_files, &entry.source_id, offset_minutes)?;
            timeline_by_source.insert(entry.source_id.clone(), runs);
            report.sources_reused += 1;
            report.corpus_bytes = report.corpus_bytes.saturating_add(
                active_files.iter().try_fold(0u64, |sum, path| {
                    Ok::<u64, io::Error>(sum.saturating_add(fs::metadata(path)?.len()))
                })?,
            );
            catalog.flush()?;
        } else if appendable {
            reads.push(SourceRead {
                entry,
                old,
                active_files,
                operation_disposition: "source_appended",
                appended: true,
            });
        } else {
            let operation_disposition = if force_rebuild {
                "source_rebuilt"
            } else if old.is_some() || !active_files.is_empty() {
                "source_rewritten"
            } else {
                "complete"
            };
            reads.push(SourceRead {
                entry,
                old: None,
                active_files,
                operation_disposition,
                appended: false,
            });
        }
    }

    let worker_count = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .min(reads.len().max(1));
    let segment_dir_ref = &segment_dir;
    let read_slice = &reads[..];
    let next_read = AtomicUsize::new(0);
    let mut completed = 0usize;
    let (sender, receiver) = mpsc::channel();
    thread::scope(|scope| -> io::Result<()> {
        for _ in 0..worker_count {
            let sender = sender.clone();
            let next_read = &next_read;
            scope.spawn(move || loop {
                let position = next_read.fetch_add(1, Ordering::Relaxed);
                let Some(read) = read_slice.get(position) else {
                    break;
                };
                let result = process_source(
                    read.entry,
                    read.old,
                    segment_dir_ref,
                    &read.active_files,
                    read.operation_disposition,
                    offset_minutes,
                );
                if sender.send((position, result)).is_err() {
                    break;
                }
            });
        }
        drop(sender);

        for (position, result) in receiver {
            let source_report = result?;
            let read = &read_slice[position];
            write_source_entry(&mut catalog, &source_report.entry)?;
            report.records += source_report.records;
            report.invalid_records += source_report.invalid_records;
            report.partial_records += source_report.partial_records;
            report.corpus_bytes = report
                .corpus_bytes
                .saturating_add(source_report.corpus_bytes);
            report.timeline_runs = report
                .timeline_runs
                .saturating_add(source_report.timeline_runs.len() as u64);
            if read.appended {
                report.sources_appended += 1;
                let mut runs = previous_timeline
                    .get(&read.entry.source_id)
                    .cloned()
                    .unwrap_or_default();
                reconcile_timeline_runs(
                    &mut runs,
                    &read.active_files,
                    &read.entry.source_id,
                    offset_minutes,
                )?;
                runs.extend(source_report.timeline_runs);
                timeline_by_source.insert(read.entry.source_id.clone(), runs);
            } else {
                report.sources_processed += 1;
                timeline_by_source
                    .insert(read.entry.source_id.clone(), source_report.timeline_runs);
            }
            catalog.flush()?;

            completed += 1;
            if completed.is_multiple_of(100) || completed == read_slice.len() {
                eprintln!(
                    "corpus: {}/{} sources read, rebuilt={}, appended={}, reused={}",
                    completed,
                    read_slice.len(),
                    report.sources_processed,
                    report.sources_appended,
                    report.sources_reused,
                );
            }
        }
        Ok(())
    })?;
    catalog.flush()?;
    drop(catalog);
    let final_entries = load_source_catalog_paths(std::slice::from_ref(&catalog_partial_path))?;
    collect_changed_sources(&mut report, &snapshot, &previous, &final_entries);
    let normalized_catalog_path = corpus.join("sources.tsv.normalized");
    let mut normalized_catalog = BufWriter::with_capacity(
        IO_BUFFER_SIZE,
        private_fs::create(&normalized_catalog_path)?,
    );
    writeln!(
        normalized_catalog,
        "{}",
        source_catalog_header(offset_minutes)
    )?;
    for entry in final_entries.values() {
        write_source_entry(&mut normalized_catalog, entry)?;
    }
    normalized_catalog.flush()?;
    drop(normalized_catalog);
    replace_file(&normalized_catalog_path, &catalog_path)?;
    if catalog_partial_path.exists() {
        fs::remove_file(&catalog_partial_path)?;
    }
    report.disposition = if final_entries.values().all(source_is_complete) {
        "complete"
    } else {
        "incomplete"
    };
    write_timeline_catalog(corpus, &final_entries, &timeline_by_source)?;
    write_source_inputs(corpus, &registered_inputs)?;
    remove_corpus_files_not_in_catalog(&segment_dir, &final_entries)?;
    Ok(report)
}

const CHANGED_SOURCE_OUTPUT_LIMIT: usize = 64;
const SOURCE_AVAILABILITY_OUTPUT_LIMIT: usize = 64;

fn collect_changed_sources(
    report: &mut BuildReport,
    snapshot: &[SourceEntry],
    previous: &BTreeMap<String, SourceEntry>,
    current: &BTreeMap<String, SourceEntry>,
) {
    for observed in snapshot {
        let Some(entry) = current.get(&observed.source_id) else {
            continue;
        };
        let previous_entry = previous.get(&observed.source_id);
        let changed = previous_entry
            .map(|old| {
                old.committed_byte_end != entry.committed_byte_end
                    || old.event_count != entry.event_count
                    || old.disposition != entry.disposition
                    || old.output_preview != entry.output_preview
            })
            .unwrap_or(true)
            || !source_is_complete(entry);
        if !changed {
            continue;
        }
        report.changed_sources_total = report.changed_sources_total.saturating_add(1);
        if report.changed_sources.len() >= CHANGED_SOURCE_OUTPUT_LIMIT {
            continue;
        }
        report.changed_sources.push(SourceChange {
            source_id: entry.source_id.clone(),
            disposition: entry.disposition.clone(),
            previous_byte_end: previous_entry
                .map(|old| old.committed_byte_end)
                .unwrap_or(0),
            committed_byte_end: entry.committed_byte_end,
            observed_size: entry.size,
            records_added: previous_entry
                .map(|old| entry.event_count.saturating_sub(old.event_count))
                .unwrap_or(entry.event_count),
        });
    }
}

pub fn status(path: &Path) -> io::Result<()> {
    let corpus = path.to_path_buf();
    let rules_version = corpus_rules_version(&corpus)?;
    let managed = crate::imports::inventory(&corpus)?;
    let segment_dir = segment_dir(&corpus);
    let sources = load_source_catalog(&corpus)?;
    let registered_inputs = load_source_inputs(&corpus)?;
    let availability = load_source_availability(&corpus)?;
    let unavailable_source_paths = availability
        .entries
        .iter()
        .filter(|entry| source_path_is_unavailable(entry))
        .count();
    let active_ids = sources.keys().collect::<Vec<_>>();
    let complete_sources = sources
        .values()
        .filter(|entry| source_is_complete(entry))
        .count();
    let incomplete_sources = sources.len().saturating_sub(complete_sources);
    let timeline = load_timeline_catalog(&corpus)?;
    let timeline_runs = timeline.values().map(Vec::len).sum::<usize>();
    let mut files = 0u64;
    let mut partial_files = 0u64;
    let mut bytes = 0u64;
    if segment_dir.is_dir() {
        for entry in fs::read_dir(&segment_dir)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if active_ids
                .iter()
                .any(|source_id| is_source_corpus_name(name, source_id))
            {
                if name.ends_with(".corpus") {
                    files += 1;
                    bytes = bytes.saturating_add(fs::metadata(&path)?.len());
                } else if name.ends_with(".corpus.partial") {
                    partial_files += 1;
                }
            }
        }
    }
    let file_len = |name: &str| {
        fs::metadata(corpus.join(name))
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    };
    println!(
        "{}",
        json::Object::new()
            .name(
                "disposition",
                if managed.unavailable_imports != 0 {
                    "managed_imports_unavailable"
                } else if unavailable_source_paths != 0 {
                    "source_paths_unavailable"
                } else {
                    "observed"
                },
            )
            .name("mode", "compact_literal_corpus")
            .name("corpus", &corpus.to_string_lossy())
            .number("rules_version", rules_version)
            .boolean("rules_current", rules_version == RULES_VERSION)
            .number("sources", sources.len() as u64)
            .number("complete_sources", complete_sources as u64)
            .number("incomplete_sources", incomplete_sources as u64)
            .number("corpus_files", files)
            .number("partial_files", partial_files)
            .number("corpus_bytes", bytes)
            .number("catalog_bytes", file_len("sources.tsv"))
            .boolean("timeline_present", timeline_catalog_present(&corpus))
            .number("timeline_sources", timeline.len() as u64)
            .number("timeline_runs", timeline_runs as u64)
            .number("timeline_bytes", file_len("timeline.tsv"))
            .number("registered_source_inputs", registered_inputs.len() as u64)
            .number("managed_imports", managed.entries.len() as u64)
            .number("unavailable_managed_imports", managed.unavailable_imports)
            .raw("imports", &managed.entries_json())
            .number("unavailable_source_paths", unavailable_source_paths as u64)
            .optional_number(
                "source_availability_observed_at_ms",
                (availability.observed_at_ms != 0).then_some(availability.observed_at_ms),
            )
            .number(
                "source_availability_total",
                availability.entries.len() as u64
            )
            .boolean(
                "source_availability_truncated",
                availability.entries.len() > SOURCE_AVAILABILITY_OUTPUT_LIMIT,
            )
            .raw(
                "source_availability",
                &source_availability_json(&availability)
            )
            .number(
                "source_availability_bytes",
                file_len(SOURCE_AVAILABILITY_FILE),
            )
            .finish()
    );
    Ok(())
}

fn source_availability_json(snapshot: &SourceAvailabilitySnapshot) -> String {
    json::array(
        snapshot
            .entries
            .iter()
            .take(SOURCE_AVAILABILITY_OUTPUT_LIMIT)
            .map(|entry| {
                json::Object::new()
                    .name("input_path", &entry.input_path)
                    .name("observed_path", &entry.observed_path)
                    .name("disposition", &entry.disposition)
                    .name("source_id", &entry.source_id)
                    .finish()
            }),
    )
}

/// The sources a read covers: the one named by `--source-id`, the sources of a `--session`, or
/// all of them. Every command scopes what it reads and what it reports as covered this way.
pub struct Scope<'a> {
    pub sources: Vec<&'a SourceEntry>,
    /// Neither a source nor a session was named.
    pub whole: bool,
}

pub fn scope<'a>(
    catalog: &'a BTreeMap<String, SourceEntry>,
    source_id: Option<&str>,
    session: Option<&str>,
) -> Scope<'a> {
    let sources = match session {
        Some(session) => session_sources(catalog, session).sources,
        None => catalog.values().collect(),
    };
    Scope {
        sources: sources
            .into_iter()
            .filter(|entry| source_id.is_none_or(|source_id| entry.source_id == source_id))
            .collect(),
        whole: source_id.is_none() && session.is_none(),
    }
}

impl Scope<'_> {
    pub fn contains(&self, source_id: &str) -> bool {
        self.sources
            .iter()
            .any(|entry| entry.source_id == source_id)
    }

    /// The segment files to read: every segment for the whole corpus, else those of the
    /// sources in scope.
    pub fn files(&self, root: &Path) -> io::Result<Vec<PathBuf>> {
        if self.whole {
            return corpus_files(root);
        }
        let mut files = Vec::new();
        for entry in &self.sources {
            files.extend(source_files(root, &entry.source_id)?);
        }
        Ok(files)
    }
}

/// How a source's log stands now against what the corpus read of it, by the test a sync
/// makes before it reads the log again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// The log is as the corpus read it.
    Current,
    /// The log has grown; this many bytes after what the corpus read are not in it yet.
    Behind(u64),
    /// The log was replaced or rewritten; the next sync reads all of its bytes again.
    Rewritten(u64),
    /// The log cannot be read.
    Unreachable,
}

impl Freshness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Behind(_) => "behind",
            Self::Rewritten(_) => "rewritten",
            Self::Unreachable => "unreachable",
        }
    }

    /// Bytes of the log as it is now that the corpus does not hold.
    pub fn unscanned_bytes(self) -> u64 {
        match self {
            Self::Behind(bytes) | Self::Rewritten(bytes) => bytes,
            Self::Current | Self::Unreachable => 0,
        }
    }

    pub fn is_stale(self) -> bool {
        matches!(self, Self::Behind(_) | Self::Rewritten(_))
    }
}

pub fn freshness(entry: &SourceEntry) -> Freshness {
    let path = Path::new(&entry.path);
    match fs::metadata(path) {
        Ok(metadata) => freshness_of(entry, &metadata),
        Err(_) => Freshness::Unreachable,
    }
}

/// Opens a source's log for reading its records, with its freshness. A log that was replaced
/// or rewritten is not returned: the bytes at the corpus's references are another record now.
pub fn open_log(entry: &SourceEntry) -> (Option<File>, Freshness) {
    let Ok(file) = File::open(&entry.path) else {
        return (None, Freshness::Unreachable);
    };
    let freshness = match file.metadata() {
        Ok(metadata) => freshness_of(entry, &metadata),
        Err(_) => Freshness::Unreachable,
    };
    match freshness {
        Freshness::Current | Freshness::Behind(_) => (Some(file), freshness),
        Freshness::Rewritten(_) | Freshness::Unreachable => (None, freshness),
    }
}

fn freshness_of(entry: &SourceEntry, metadata: &fs::Metadata) -> Freshness {
    let path = Path::new(&entry.path);
    let (volume_serial_number, file_id) = identity_of(path, metadata);
    let observed = SourceEntry {
        path: entry.path.clone(),
        size: metadata.len(),
        modified_ms: modified_ms(metadata),
        volume_serial_number,
        file_id,
        ..SourceEntry::default()
    };
    if same_snapshot(entry, &observed) && entry.committed_byte_end >= observed.size {
        Freshness::Current
    } else if can_append(entry, &observed) {
        Freshness::Behind(observed.size.saturating_sub(entry.committed_byte_end))
    } else {
        Freshness::Rewritten(observed.size)
    }
}

/// What the logs of some sources hold now that the corpus does not.
#[derive(Debug, Default)]
pub struct Staleness {
    pub stale_sources: u64,
    pub unscanned_source_bytes: u64,
    pub unreachable: BTreeSet<String>,
}

impl Staleness {
    pub fn of<'a>(sources: impl IntoIterator<Item = &'a SourceEntry>) -> Self {
        let sources = sources.into_iter().collect::<Vec<_>>();
        // A file's metadata takes about a millisecond to read through WSL's view of a Windows
        // drive, so a thousand sources are checked on several threads.
        let workers = thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .clamp(1, 16);
        let chunk = sources.len().div_ceil(workers).max(1);
        let results = thread::scope(|scope| {
            let handles = sources
                .chunks(chunk)
                .map(|part| {
                    scope.spawn(move || {
                        part.iter()
                            .map(|entry| freshness(entry))
                            .collect::<Vec<_>>()
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().expect("freshness worker"))
                .collect::<Vec<_>>()
        });
        let mut staleness = Staleness::default();
        for (entry, freshness) in sources.iter().zip(results) {
            if freshness == Freshness::Unreachable {
                staleness.unreachable.insert(entry.source_id.clone());
            } else if freshness.is_stale() {
                staleness.stale_sources += 1;
                staleness.unscanned_source_bytes = staleness
                    .unscanned_source_bytes
                    .saturating_add(freshness.unscanned_bytes());
            }
        }
        staleness
    }
}

pub fn source_is_complete(entry: &SourceEntry) -> bool {
    entry.checkpoint_valid
        && entry.committed_byte_end >= entry.size
        && matches!(
            entry.disposition.as_str(),
            "complete" | "source_appended" | "source_rebuilt" | "source_rewritten"
        )
}

pub fn require_corpus(path: &Path) -> io::Result<()> {
    let corpus = path.to_path_buf();
    if !corpus.is_dir() {
        return Err(no_corpus_here(&corpus));
    }
    let catalog_path = corpus.join("sources.tsv");
    let file = File::open(&catalog_path).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{}: no current corpus catalog; {}",
                catalog_path.display(),
                REBUILD_ADVICE
            ),
        )
    })?;
    let mut lines = BufReader::with_capacity(IO_BUFFER_SIZE, file).lines();
    let header = lines.next().transpose()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: catalog is empty; {}",
                catalog_path.display(),
                REBUILD_ADVICE
            ),
        )
    })?;
    parse_catalog_header(&header, &catalog_path)?;
    Ok(())
}

/// The corpus is generated from the logs, so a damaged or outdated one is rebuilt, not repaired.
pub const REBUILD_ADVICE: &str = "rebuild it with `ebira sync --rebuild`";

pub fn no_corpus_here(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{}: no corpus here; build one with `ebira sync --corpus {}`",
            path.display(),
            path.display()
        ),
    )
}

const LOCK_FILE: &str = "ebira.lock";

/// Commands that write (`sync`, `import`, `timeline --rebuild`) hold the corpus lock alone, and
/// commands that read share it: a reader never sees a sync half done, and two writers never
/// interleave their updates of one catalog or registry. The lock is released when the returned
/// file is dropped, also when the process ends abnormally.
pub fn lock_exclusive(path: &Path) -> io::Result<File> {
    let root = path.to_path_buf();
    private_fs::create_dir_all(&root)?;
    let file = private_fs::write_options()
        .truncate(false)
        .read(true)
        .open(root.join(LOCK_FILE))?;
    file.lock()?;
    Ok(file)
}

/// Puts a finished file in place of the one it replaces, in one step: a reader opens the old
/// file or the new one, never a name with no file behind it.
pub fn replace_file(partial: &Path, complete: &Path) -> io::Result<()> {
    fs::rename(partial, complete)
}

/// Holds the corpus for reading. A reader waits while a sync or an import writes the corpus,
/// and checks that it is a current corpus only once it holds it; checked before, a corpus in
/// the middle of a rebuild looks missing or outdated.
pub fn read_lock(path: &Path) -> io::Result<Option<File>> {
    let lock = lock_shared(path)?;
    require_corpus(path)?;
    Ok(lock)
}

/// A corpus that no writer has locked yet has no lock file; reading it needs none.
pub fn lock_shared(path: &Path) -> io::Result<Option<File>> {
    let Ok(file) = File::open(path.join(LOCK_FILE)) else {
        return Ok(None);
    };
    file.lock_shared()?;
    Ok(Some(file))
}

/// The directories Claude Code and Codex write their transcripts to, where they exist:
/// `$CLAUDE_CONFIG_DIR/projects` (default `~/.claude/projects`), and `$CODEX_HOME/sessions`
/// and `$CODEX_HOME/archived_sessions` (default `~/.codex`).
pub fn default_sources() -> Vec<String> {
    let home = home_dir();
    let claude =
        env_dir("CLAUDE_CONFIG_DIR").or_else(|| home.as_ref().map(|home| home.join(".claude")));
    let codex = env_dir("CODEX_HOME").or_else(|| home.as_ref().map(|home| home.join(".codex")));
    let mut candidates = Vec::new();
    if let Some(claude) = claude {
        candidates.push(claude.join("projects"));
    }
    if let Some(codex) = codex {
        candidates.push(codex.join("sessions"));
        candidates.push(codex.join("archived_sessions"));
    }
    candidates
        .into_iter()
        .filter(|path| path.is_dir())
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env_dir("USERPROFILE").or_else(|| env_dir("HOME"))
    }
    #[cfg(not(windows))]
    {
        env_dir("HOME")
    }
}

/// The directory that holds the projected event segments, `<corpus>/segments`.
pub fn segment_dir(path: &Path) -> PathBuf {
    path.join(SEGMENT_DIR)
}

const SEGMENT_DIR: &str = "segments";

/// Before format 7 the segments lived in `corpus/`. A rebuild removes what is left there; the
/// segments are generated from the logs and cannot be read by this build.
fn remove_legacy_segments(corpus: &Path) -> io::Result<()> {
    let legacy = corpus.join("corpus");
    let Ok(entries) = fs::read_dir(&legacy) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if name.ends_with(".corpus") || name.ends_with(".corpus.partial") {
            fs::remove_file(&path)?;
        }
    }
    let _ = fs::remove_dir(&legacy);
    Ok(())
}

pub fn source_catalog(path: &Path) -> io::Result<BTreeMap<String, SourceEntry>> {
    load_source_catalog(path)
}

pub fn registered_sources(path: &Path) -> io::Result<Vec<String>> {
    let corpus = path.to_path_buf();
    let inputs = load_source_inputs(&corpus)?;
    if !inputs.is_empty() {
        return Ok(inputs);
    }
    let catalog = source_catalog(path)?;
    let mut sources = catalog
        .values()
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    sources.sort();
    sources.dedup();
    Ok(sources)
}

pub fn corpus_files(path: &Path) -> io::Result<Vec<PathBuf>> {
    let root = segment_dir(path);
    let catalog = load_source_catalog(path)?;
    let mut files = Vec::new();
    if !root.is_dir() {
        return Ok(files);
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if path.is_file()
            && name.ends_with(".corpus")
            && catalog
                .keys()
                .any(|source_id| is_source_corpus_name(name, source_id))
        {
            files.push(path);
        }
    }
    sort_corpus_files(&mut files);
    Ok(files)
}

pub fn source_files(path: &Path, source_id: &str) -> io::Result<Vec<PathBuf>> {
    Ok(corpus_source_files(&segment_dir(path), source_id)?
        .into_iter()
        .filter(|path| is_complete_corpus_file(path))
        .collect())
}

pub fn source_projection_complete(entry: &SourceEntry, files: &[PathBuf]) -> io::Result<bool> {
    let mut source_files = files
        .iter()
        .filter(|path| {
            corpus_source_id_from_path(path).as_deref() == Some(entry.source_id.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    sort_corpus_files(&mut source_files);
    if source_files.is_empty() {
        return Ok(false);
    }

    let mut covered_end = 0u64;
    for path in source_files {
        let Some((start, end)) = observed_corpus_source_range(&path, entry.committed_byte_end)
        else {
            return Ok(false);
        };
        if start != covered_end || end < start {
            return Ok(false);
        }
        covered_end = end;
    }
    Ok(covered_end == entry.committed_byte_end)
}

fn observed_corpus_source_range(path: &Path, fallback_end: u64) -> Option<(u64, u64)> {
    corpus_source_range(path, fallback_end)
}

struct TimelineAccumulator {
    source_id: String,
    corpus_file: String,
    offset_minutes: i64,
    current: Option<TimelineRun>,
    runs: Vec<TimelineRun>,
}

impl TimelineAccumulator {
    fn new(source_id: &str, corpus_file: &str, offset_minutes: i64) -> Self {
        Self {
            source_id: source_id.to_string(),
            corpus_file: corpus_file.to_string(),
            offset_minutes,
            current: None,
            runs: Vec::new(),
        }
    }

    fn observe(&mut self, corpus_start: u64, corpus_end: u64, header: &EventHeader) {
        let date = crate::time::date_bucket(&header.timestamp, self.offset_minutes);
        let new_run = self
            .current
            .as_ref()
            .map(|run| run.date != date)
            .unwrap_or(false);
        if new_run {
            self.finish_current();
        }
        if self.current.is_none() {
            self.current = Some(TimelineRun {
                source_id: self.source_id.clone(),
                date: date.clone(),
                corpus_file: self.corpus_file.clone(),
                corpus_start,
                corpus_end,
                bucket: TimelineBucket {
                    date: date.clone(),
                    ..TimelineBucket::default()
                },
            });
        }
        let reference = TimelineRef {
            event_index: header.event_index,
            line: header.line,
            byte_start: header.byte_start,
            byte_len: header.byte_len,
            session: header.session.clone(),
            turn: header.turn.clone(),
            role: header.role.clone(),
            kind: header.kind.clone(),
            timestamp: header.timestamp.clone(),
        };
        if let Some(run) = self.current.as_mut() {
            run.corpus_end = corpus_end;
            run.bucket.observe(&date, reference);
        }
    }

    fn finish_current(&mut self) {
        if let Some(run) = self.current.take() {
            self.runs.push(run);
        }
    }

    fn finish(mut self) -> Vec<TimelineRun> {
        self.finish_current();
        self.runs
    }
}

pub fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> io::Result<Option<LineRead>> {
    line.clear();
    let mut total_len = 0u64;
    let mut oversized = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if total_len == 0 {
                return Ok(None);
            }
            return Ok(Some(LineRead {
                total_len,
                complete: false,
                oversized,
            }));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline
            .map(|position| position + 1)
            .unwrap_or(available.len());
        total_len = total_len.saturating_add(take as u64);
        if !oversized {
            let remaining = MAX_RECORD_BYTES.saturating_sub(line.len());
            if take <= remaining {
                line.extend_from_slice(&available[..take]);
            } else {
                line.extend_from_slice(&available[..remaining]);
                oversized = true;
            }
        }
        reader.consume(take);
        if newline.is_some() {
            return Ok(Some(LineRead {
                total_len,
                complete: true,
                oversized,
            }));
        }
    }
}

/// Reads the records of one log in order, the way a sync projects them: who sent each one, its
/// session and turn, and its body. `follow` reads with it too, so a message it returns carries
/// what the corpus records for the same line.
pub struct LogReader {
    origin: SourceOrigin,
    machine: IngestStateMachine,
    fallback_session: String,
    output_preview: usize,
}

/// One line of a log, read.
pub struct ReadRecord {
    pub event: CanonicalEvent,
    /// The record's fields; none when the line is not a record that can be read.
    pub fields: Vec<Field>,
    /// The body was shortened.
    pub cut: bool,
    /// The line is not a record that can be read.
    pub invalid: bool,
}

impl LogReader {
    fn new(entry: &SourceEntry, checkpoint: IngestCheckpoint) -> Self {
        Self {
            origin: source_origin(entry),
            machine: IngestStateMachine::from_checkpoint(checkpoint),
            fallback_session: entry.fallback_session.clone(),
            output_preview: entry.output_preview as usize,
        }
    }

    /// Reads one complete line, newline included, that starts at `record_start`. An
    /// `oversized` line holds only its first `MAX_RECORD_BYTES`.
    pub fn read(&mut self, line: &[u8], record_start: u64, oversized: bool) -> ReadRecord {
        let fields = if oversized {
            None
        } else {
            parse_record(line, record_start).ok()
        };
        let Some(fields) = fields else {
            let mut text = String::from_utf8_lossy(line).into_owned();
            if oversized {
                text.push_str(crate::core::PROJECTION_BOUND_MARKER);
            }
            let mut body = String::new();
            push_field(&mut body, "/raw", &text);
            let meta = RecordMeta {
                event_kind: EventKind::Invalid,
                ..RecordMeta::default()
            };
            return ReadRecord {
                event: self
                    .machine
                    .apply(&self.fallback_session, meta, body, record_start),
                fields: Vec::new(),
                cut: oversized,
                invalid: true,
            };
        };
        let mut meta = record_meta(&fields);
        classify_sender(self.origin, &fields, &mut meta);
        let projection = select_body(&fields, &meta, self.output_preview);
        ReadRecord {
            event: self
                .machine
                .apply(&self.fallback_session, meta, projection.body, record_start),
            fields,
            cut: projection.cut,
            invalid: false,
        }
    }

    pub fn checkpoint(&self) -> IngestCheckpoint {
        self.machine.checkpoint()
    }
}

/// The reading state a sync left where it stopped reading a source.
fn checkpoint_of(entry: &SourceEntry) -> IngestCheckpoint {
    IngestCheckpoint {
        reducer: decode_turn_state(entry),
        active_session: non_empty(&entry.last_session),
        active_cwd: non_empty(&entry.last_cwd),
        next_event_index: entry.event_count,
    }
}

/// A reader for the log at `path` that has read it up to `cursor`, a line start, and the number
/// of lines before `cursor`: it starts from the catalog's checkpoint when the corpus has read
/// this very log no further than `cursor`, else from the start of the log. A log replaced since
/// the sync starts over, as the sync will. `None` while the log has no complete record, before
/// which the agent that writes it cannot be told.
pub fn log_reader_at(
    catalog: &BTreeMap<String, SourceEntry>,
    path: &Path,
    cursor: u64,
) -> io::Result<Option<(LogReader, u64)>> {
    if !first_line_complete(path) {
        return Ok(None);
    }
    let Some(entry) = source_entry(path, 300)? else {
        return Ok(None);
    };
    let (mut log, from, mut lines) = match catalog.get(&entry.source_id) {
        Some(known)
            if known.checkpoint_valid
                && known.committed_byte_end <= cursor
                && matches!(freshness(known), Freshness::Current | Freshness::Behind(_)) =>
        {
            (
                LogReader::new(known, checkpoint_of(known)),
                known.committed_byte_end,
                known.last_line,
            )
        }
        _ => (LogReader::new(&entry, IngestCheckpoint::default()), 0, 0),
    };
    let mut input = BufReader::with_capacity(IO_BUFFER_SIZE, File::open(path)?);
    input.seek(SeekFrom::Start(from))?;
    let mut bounded = input.take(cursor.saturating_sub(from));
    let mut line = Vec::new();
    let mut position = from;
    while let Some(read) = read_bounded_line(&mut bounded, &mut line)? {
        if !read.complete {
            break;
        }
        log.read(&line, position, read.oversized);
        position += read.total_len;
        lines += 1;
    }
    Ok(Some((log, lines)))
}

fn process_source(
    entry: &SourceEntry,
    old: Option<&SourceEntry>,
    segment_dir: &Path,
    active_files: &[PathBuf],
    operation_disposition: &'static str,
    offset_minutes: i64,
) -> io::Result<SourceReport> {
    let append = old.is_some();
    let start_offset = old.map(|old| old.committed_byte_end).unwrap_or(0);
    let start_offset = start_offset.min(entry.size);

    let output_name = if append {
        format!(
            "{}--{}-{}.corpus",
            entry.source_id, start_offset, entry.size
        )
    } else {
        rebuild_output_name(segment_dir, entry)
    };
    let output_path = segment_dir.join(&output_name);
    let partial_path = output_path.with_extension("corpus.partial");
    let input = File::open(&entry.path)?;
    let mut reader = BufReader::with_capacity(IO_BUFFER_SIZE, input);
    reader.seek(SeekFrom::Start(start_offset))?;
    let mut bounded = reader.take(entry.size.saturating_sub(start_offset));
    let segment = private_fs::create(&partial_path)?;
    let mut writer = BufWriter::with_capacity(IO_BUFFER_SIZE, segment);
    let mut line = Vec::new();
    let mut committed_byte_end = start_offset;
    let mut line_number = old.map(|old| old.last_line).unwrap_or(0);
    let mut records = 0u64;
    let mut invalid_records = 0u64;
    let mut partial_records = 0u64;
    let mut corpus_offset = 0u64;
    let mut timeline = TimelineAccumulator::new(&entry.source_id, &output_name, offset_minutes);
    let checkpoint = old.map(checkpoint_of).unwrap_or_default();
    let mut log = LogReader::new(entry, checkpoint);

    while let Some(line_read) = read_bounded_line(&mut bounded, &mut line)? {
        let record_start = committed_byte_end;
        let record_end = record_start.saturating_add(line_read.total_len);
        if !line_read.complete {
            partial_records += 1;
            break;
        }
        line_number += 1;
        let record = log.read(&line, record_start, line_read.oversized);
        if record.invalid {
            invalid_records += 1;
        }
        let body_cut = record.cut;
        let event = record.event;
        let body_bytes = event.body.as_bytes();
        let header = EventHeader {
            event_index: event.event_index,
            source_id: entry.source_id.clone(),
            line: line_number,
            byte_start: record_start,
            byte_len: line_read.total_len,
            session: event.meta.session.clone().unwrap_or_default(),
            turn: event.meta.turn.clone().unwrap_or_default(),
            role: event.meta.role.clone().unwrap_or_default(),
            kind: event.meta.event_kind.as_str().to_string(),
            event_type: event.meta.event_type.clone().unwrap_or_default(),
            timestamp: event.meta.timestamp.clone().unwrap_or_default(),
            cwd: event.meta.cwd.clone().unwrap_or_default(),
            repository: event.meta.repository.clone().unwrap_or_default(),
            call_id: event.meta.call_id.clone().unwrap_or_default(),
            sender: event.meta.sender.as_str().to_string(),
            via: event.meta.via.clone(),
            body_cut,
            body_len: body_bytes.len() as u64,
        };
        let header_line = event_header_line(&header);
        let event_corpus_start = corpus_offset;
        writer.write_all(header_line.as_bytes())?;
        writer.write_all(body_bytes)?;
        writer.write_all(b"\n")?;
        corpus_offset = corpus_offset
            .saturating_add(header_line.len() as u64)
            .saturating_add(body_bytes.len() as u64)
            .saturating_add(1);
        timeline.observe(event_corpus_start, corpus_offset, &header);
        committed_byte_end = record_end;
        records += 1;
        if records.is_multiple_of(FLUSH_RECORD_INTERVAL) {
            writer.flush()?;
        }
    }

    writer.flush()?;
    drop(writer);
    let output_name = if append && committed_byte_end != entry.size {
        format!(
            "{}--{}-{}.corpus",
            entry.source_id, start_offset, committed_byte_end
        )
    } else {
        output_name
    };
    let output_path = segment_dir.join(&output_name);
    let mut timeline_runs = timeline.finish();
    for run in timeline_runs.iter_mut() {
        run.corpus_file = output_name.clone();
    }
    if records > 0 || !append {
        replace_file(&partial_path, &output_path)?;
        if !append {
            remove_source_corpus_files_except(active_files, &output_path)?;
        }
    } else if partial_path.exists() {
        fs::remove_file(&partial_path)?;
    }

    let after = fs::metadata(&entry.path)?;
    let disposition = if after.len() > entry.size {
        "source_grew_during_sync"
    } else if after.len() < entry.size {
        "source_truncated_during_sync"
    } else if modified_ms(&after) != entry.modified_ms {
        "source_changed_during_sync"
    } else if partial_records > 0 {
        "tail_incomplete"
    } else {
        operation_disposition
    };
    let checkpoint = log.checkpoint();
    let mut updated = entry.clone();
    updated.committed_byte_end = committed_byte_end;
    updated.event_count = checkpoint.next_event_index;
    updated.last_line = line_number;
    updated.last_session = checkpoint
        .active_session
        .clone()
        .or_else(|| {
            checkpoint
                .reducer
                .sessions
                .last()
                .map(|item| item.session.clone())
        })
        .unwrap_or_else(|| entry.fallback_session.clone());
    updated.last_cwd = checkpoint.active_cwd.clone().unwrap_or_default();
    encode_turn_state(&mut updated, &checkpoint.reducer);
    updated.disposition = disposition.to_string();
    updated.checkpoint_valid = true;
    updated.synced_at_ms = now_ms();
    let retained_corpus_bytes = if append {
        active_files.iter().try_fold(0u64, |sum, path| {
            Ok::<u64, io::Error>(sum.saturating_add(fs::metadata(path)?.len()))
        })?
    } else {
        0
    };
    let corpus_bytes = retained_corpus_bytes.saturating_add(if output_path.is_file() {
        fs::metadata(output_path)?.len()
    } else {
        0
    });
    Ok(SourceReport {
        entry: updated,
        records,
        invalid_records,
        partial_records,
        corpus_bytes,
        timeline_runs,
    })
}

fn source_catalog_header(offset_minutes: i64) -> String {
    format!(
        "{}\ttz={}\trules={}",
        source_catalog_version(),
        crate::time::format_offset(offset_minutes),
        RULES_VERSION
    )
}

/// The version of how records are read: which fields are kept, who sent each record, how
/// turns are told apart, which date each record is on. The logs are the source of truth and the corpus is a projection of
/// them, so a corpus made under other rules is rebuilt by the next `ebira sync`.
///
/// Increase it with every change to what a sync writes for the same logs, together with
/// `RULES_FINGERPRINT` in the tests of `core.rs`, which fails until both are updated.
pub const RULES_VERSION: u64 = 3;

/// How a corpus on disk relates to this build of Ebira.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorpusState {
    Missing,
    Current,
    /// Written in another storage format; this build cannot read it.
    FormatChanged,
    /// Readable, but made under other reading rules.
    RulesChanged,
}

pub fn corpus_state(path: &Path) -> CorpusState {
    let catalog_path = path.join("sources.tsv");
    let Ok(file) = File::open(&catalog_path) else {
        return CorpusState::Missing;
    };
    let Some(Ok(header)) = BufReader::new(file).lines().next() else {
        return CorpusState::FormatChanged;
    };
    if parse_catalog_header(&header, &catalog_path).is_err() {
        return CorpusState::FormatChanged;
    }
    if catalog_rules_version(&header) == RULES_VERSION {
        CorpusState::Current
    } else {
        CorpusState::RulesChanged
    }
}

/// A catalog written before the rules were versioned declares none and counts as 0.
fn catalog_rules_version(header: &str) -> u64 {
    header
        .split('\t')
        .find_map(|part| part.strip_prefix("rules="))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

pub fn corpus_rules_version(root: &Path) -> io::Result<u64> {
    let file = File::open(root.join("sources.tsv"))?;
    let header = BufReader::new(file).lines().next().transpose()?;
    Ok(header.as_deref().map(catalog_rules_version).unwrap_or(0))
}

pub fn corpus_offset_minutes(root: &Path) -> io::Result<i64> {
    let corpus = root.to_path_buf();
    for name in ["sources.tsv", "sources.tsv.partial"] {
        let path = corpus.join(name);
        if !path.is_file() {
            continue;
        }
        let file = File::open(&path)?;
        let mut lines = BufReader::new(file).lines();
        let Some(line) = lines.next().transpose()? else {
            continue;
        };
        return parse_catalog_header(&line, &path);
    }
    Ok(0)
}

fn parse_catalog_header(line: &str, path: &Path) -> io::Result<i64> {
    let mut parts = line.split('\t');
    let version = format!(
        "{}\t{}",
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default()
    );
    if version != source_catalog_version() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: catalog is not {}; run `ebira sync`, which rebuilds it",
                path.display(),
                source_catalog_version(),
            ),
        ));
    }
    let offset = parts
        .next()
        .and_then(|part| part.strip_prefix("tz="))
        .and_then(crate::time::parse_offset);
    offset.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: catalog does not declare tz; {}",
                path.display(),
                REBUILD_ADVICE
            ),
        )
    })
}

fn encode_turn_state(entry: &mut SourceEntry, state: &TurnReducerState) {
    let join = |values: Vec<String>| values.join("|");
    entry.turn_sessions = join(
        state
            .sessions
            .iter()
            .map(|item| encode_token(&item.session))
            .collect(),
    );
    entry.turn_turns = join(
        state
            .sessions
            .iter()
            .map(|item| encode_token(&item.turn))
            .collect(),
    );
    entry.turn_has_user = join(
        state
            .sessions
            .iter()
            .map(|item| u8::from(item.has_user).to_string())
            .collect(),
    );
}

/// The sources of a session, as every command resolves a `--session`.
pub struct SessionSources<'a> {
    /// The sources whose records declare the session. They take in the transcripts of the
    /// agents it started: Claude Code subagents and Codex child threads record their parent's
    /// session.
    pub sources: Vec<&'a SourceEntry>,
    /// The session's own transcript: the one source outside `subagents/` whose file name
    /// carries the session id (`<session-id>.jsonl`, `rollout-<time>-<thread-id>.jsonl`).
    pub main: Option<&'a SourceEntry>,
}

pub fn session_sources<'a>(
    catalog: &'a BTreeMap<String, SourceEntry>,
    session: &str,
) -> SessionSources<'a> {
    let sources = catalog
        .values()
        .filter(|entry| source_declares_session(entry, session))
        .collect::<Vec<_>>();
    let named = sources
        .iter()
        .copied()
        .filter(|entry| file_names_session(Path::new(&entry.path), session))
        .collect::<Vec<_>>();
    let main = (named.len() == 1).then(|| named[0]);
    SessionSources { sources, main }
}

/// Whether a transcript is named by the session: its file name carries the session id and it
/// lies outside `subagents/`.
pub fn file_names_session(path: &Path, session: &str) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.contains(session))
        && !path
            .to_string_lossy()
            .replace('\\', "/")
            .contains("/subagents/")
}

fn source_declares_session(entry: &SourceEntry, session: &str) -> bool {
    decode_turn_state(entry)
        .sessions
        .iter()
        .any(|item| item.session == session)
}

fn decode_turn_state(entry: &SourceEntry) -> TurnReducerState {
    let split = |value: &str| {
        if value.is_empty() {
            Vec::new()
        } else {
            value.split('|').map(str::to_string).collect::<Vec<_>>()
        }
    };
    let sessions = split(&entry.turn_sessions);
    let turns = split(&entry.turn_turns);
    let flags = split(&entry.turn_has_user);
    TurnReducerState {
        sessions: sessions
            .into_iter()
            .enumerate()
            .filter_map(|(index, session)| {
                Some(SessionTurn {
                    session: decode_token(&session).ok()?,
                    turn: decode_token(turns.get(index)?).ok()?,
                    has_user: flags.get(index).map(|flag| flag == "1").unwrap_or(false),
                })
            })
            .collect(),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// The catalog entry for a log, or `None` while it cannot yet be told which agent wrote it: a
/// log outside `.claude` and `.codex` directories without one complete record. The agent is
/// part of the source id, so it is decided once, from records that will not change.
fn source_entry(path: &Path, output_preview: u64) -> io::Result<Option<SourceEntry>> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let canonical_string = canonical.to_string_lossy().into_owned();
    let metadata = fs::metadata(&canonical)?;
    let (volume_serial_number, file_id) = file_identity(&canonical);
    let Some(app) = source_app(&canonical) else {
        return Ok(None);
    };
    let fallback_session = canonical
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("source")
        .to_string();
    Ok(Some(SourceEntry {
        source_id: format!("{}-{:016x}", app, fnv64(canonical_string.as_bytes())),
        app: app.to_string(),
        path: canonical_string,
        fallback_session,
        size: metadata.len(),
        modified_ms: modified_ms(&metadata),
        output_preview,
        volume_serial_number,
        file_id,
        ..SourceEntry::default()
    }))
}

fn source_entry_for_preview(
    path: &Path,
    requested_preview: Option<usize>,
    previous: &BTreeMap<String, SourceEntry>,
) -> io::Result<Option<SourceEntry>> {
    let Some(mut entry) = source_entry(path, requested_preview.unwrap_or(300) as u64)? else {
        return Ok(None);
    };
    if requested_preview.is_none() {
        if let Some(previous) = previous.get(&entry.source_id) {
            entry.output_preview = previous.output_preview;
        }
    }
    Ok(Some(entry))
}

fn write_source_entry(writer: &mut BufWriter<File>, entry: &SourceEntry) -> io::Result<()> {
    writeln!(
        writer,
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        encode_token(&entry.source_id),
        encode_token(&entry.app),
        encode_token(&entry.path),
        encode_token(&entry.fallback_session),
        entry.size,
        entry.modified_ms,
        entry.committed_byte_end,
        entry.event_count,
        entry.last_line,
        encode_token(&entry.last_session),
        encode_token(&entry.turn_sessions),
        encode_token(&entry.turn_turns),
        encode_token(&entry.turn_has_user),
        encode_token(&entry.disposition),
        u8::from(entry.checkpoint_valid),
        entry.output_preview,
        entry.volume_serial_number,
        entry.file_id,
        entry.synced_at_ms,
        encode_token(&entry.last_cwd),
    )
}

fn timeline_header() -> String {
    format!("#ebira-timeline\tv={}", crate::format::FORMAT_VERSION)
}
fn source_catalog_version() -> String {
    format!("#ebira-sources\tv={}", crate::format::FORMAT_VERSION)
}
const SOURCE_INPUTS_FILE: &str = "source-inputs.tsv";
const SOURCE_AVAILABILITY_FILE: &str = "source-availability.tsv";
const SOURCE_AVAILABILITY_VERSION: u64 = 1;

/// The canonical spelling of a source path. A path that no longer exists takes the canonical
/// spelling of its nearest existing ancestor, so a removed source still matches the input it
/// was registered as, however the path is written now (Windows short names and `\\?\`
/// prefixes, symbolic links).
fn normalize_source_input(input: &str) -> String {
    let path = Path::new(input);
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical.to_string_lossy().into_owned();
    }
    let mut missing = Vec::new();
    let mut current = path;
    while let (Some(parent), Some(name)) = (current.parent(), current.file_name()) {
        missing.push(name);
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let Ok(mut canonical) = fs::canonicalize(parent) {
            for name in missing.iter().rev() {
                canonical.push(name);
            }
            return canonical.to_string_lossy().into_owned();
        }
        current = parent;
    }
    input.to_string()
}

fn load_source_inputs(corpus: &Path) -> io::Result<Vec<String>> {
    let mut inputs = Vec::new();
    for path in [
        corpus.join(SOURCE_INPUTS_FILE),
        corpus.join(format!("{}.partial", SOURCE_INPUTS_FILE)),
    ] {
        if !path.is_file() {
            continue;
        }
        let file = File::open(path)?;
        for line in BufReader::with_capacity(IO_BUFFER_SIZE, file).lines() {
            let line = line?;
            if let Ok(input) = decode_token(&line) {
                if !inputs.contains(&input) {
                    inputs.push(input);
                }
            }
        }
    }
    Ok(inputs)
}

fn write_source_inputs(corpus: &Path, inputs: &[String]) -> io::Result<()> {
    let partial_path = corpus.join(format!("{}.partial", SOURCE_INPUTS_FILE));
    let complete_path = corpus.join(SOURCE_INPUTS_FILE);
    write_source_inputs_partial(corpus, inputs)?;
    replace_file(&partial_path, &complete_path)?;
    Ok(())
}

fn write_source_inputs_partial(corpus: &Path, inputs: &[String]) -> io::Result<()> {
    let partial_path = corpus.join(format!("{}.partial", SOURCE_INPUTS_FILE));
    let mut writer = BufWriter::with_capacity(IO_BUFFER_SIZE, private_fs::create(&partial_path)?);
    let mut sorted = inputs.to_vec();
    sorted.sort();
    sorted.dedup();
    for input in sorted {
        writeln!(writer, "{}", encode_token(&input))?;
    }
    writer.flush()?;
    Ok(())
}

pub fn source_availability(path: &Path) -> io::Result<SourceAvailabilitySnapshot> {
    load_source_availability(path)
}

fn load_source_availability(corpus: &Path) -> io::Result<SourceAvailabilitySnapshot> {
    let path = corpus.join(SOURCE_AVAILABILITY_FILE);
    if !path.is_file() {
        return Ok(SourceAvailabilitySnapshot::default());
    }
    let file = File::open(&path)?;
    let mut lines = BufReader::with_capacity(IO_BUFFER_SIZE, file).lines();
    let header = lines
        .next()
        .transpose()?
        .ok_or_else(|| invalid_availability(&path, "file is empty"))?;
    let prefix = format!(
        "#ebira-source-availability\tv={}\tobserved_at_ms=",
        SOURCE_AVAILABILITY_VERSION
    );
    let observed_at_ms = header
        .strip_prefix(&prefix)
        .ok_or_else(|| invalid_availability(&path, "header is not the current format"))?
        .parse::<u64>()
        .map_err(|_| invalid_availability(&path, "observed_at_ms is not an integer"))?;
    let mut entries = Vec::new();
    for (index, line) in lines.enumerate() {
        let line = line?;
        let columns = line.split('\t').collect::<Vec<_>>();
        if columns.len() != 4 {
            return Err(invalid_availability(
                &path,
                &format!("line {} does not have 4 columns", index + 2),
            ));
        }
        let disposition = decode_token(columns[2]).map_err(|error| {
            invalid_availability(&path, &format!("line {} disposition: {}", index + 2, error))
        })?;
        if !matches!(
            disposition.as_str(),
            "present" | "missing" | "unreadable" | "not_jsonl" | "unsupported"
        ) {
            return Err(invalid_availability(
                &path,
                &format!("line {} has unknown disposition", index + 2),
            ));
        }
        entries.push(SourceAvailability {
            input_path: decode_token(columns[0]).map_err(|error| {
                invalid_availability(&path, &format!("line {} input: {}", index + 2, error))
            })?,
            observed_path: decode_token(columns[1]).map_err(|error| {
                invalid_availability(&path, &format!("line {} path: {}", index + 2, error))
            })?,
            disposition,
            source_id: decode_token(columns[3]).map_err(|error| {
                invalid_availability(&path, &format!("line {} source_id: {}", index + 2, error))
            })?,
        });
    }
    Ok(SourceAvailabilitySnapshot {
        observed_at_ms,
        entries,
    })
}

fn invalid_availability(path: &Path, message: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{}: {}; rebuild the disposable source availability projection",
            path.display(),
            message
        ),
    )
}

fn write_source_availability(
    corpus: &Path,
    snapshot: &SourceAvailabilitySnapshot,
) -> io::Result<()> {
    let partial_path = corpus.join(format!("{}.partial", SOURCE_AVAILABILITY_FILE));
    let complete_path = corpus.join(SOURCE_AVAILABILITY_FILE);
    let mut writer = BufWriter::with_capacity(IO_BUFFER_SIZE, private_fs::create(&partial_path)?);
    writeln!(
        writer,
        "#ebira-source-availability\tv={}\tobserved_at_ms={}",
        SOURCE_AVAILABILITY_VERSION, snapshot.observed_at_ms
    )?;
    for entry in &snapshot.entries {
        writeln!(
            writer,
            "{}\t{}\t{}\t{}",
            encode_token(&entry.input_path),
            encode_token(&entry.observed_path),
            encode_token(&entry.disposition),
            encode_token(&entry.source_id),
        )?;
    }
    writer.flush()?;
    drop(writer);
    replace_file(&partial_path, &complete_path)?;
    Ok(())
}

fn availability_entries_for_inputs<'a>(
    snapshot: &'a SourceAvailabilitySnapshot,
    inputs: &[String],
) -> impl Iterator<Item = &'a SourceAvailability> {
    let observed = inputs
        .iter()
        .map(|input| normalize_source_input(input))
        .collect::<BTreeSet<_>>();
    snapshot
        .entries
        .iter()
        .filter(move |entry| observed.contains(&entry.input_path))
}

pub fn source_path_is_unavailable(entry: &SourceAvailability) -> bool {
    entry.disposition != "present"
}

fn merge_source_availability(
    previous: SourceAvailabilitySnapshot,
    observed: Vec<SourceAvailability>,
    observed_inputs: &[String],
    registered_inputs: &[String],
) -> SourceAvailabilitySnapshot {
    let observed_inputs = observed_inputs
        .iter()
        .map(|input| normalize_source_input(input))
        .collect::<BTreeSet<_>>();
    let registered_inputs = registered_inputs.iter().cloned().collect::<BTreeSet<_>>();
    let mut entries = previous
        .entries
        .into_iter()
        .filter(|entry| {
            registered_inputs.contains(&entry.input_path)
                && !observed_inputs.contains(&entry.input_path)
        })
        .chain(observed)
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.input_path
            .cmp(&right.input_path)
            .then_with(|| left.observed_path.cmp(&right.observed_path))
            .then_with(|| left.disposition.cmp(&right.disposition))
            .then_with(|| left.source_id.cmp(&right.source_id))
    });
    entries.dedup();
    SourceAvailabilitySnapshot {
        observed_at_ms: now_ms(),
        entries,
    }
}

fn attach_source_ids(
    availability: &mut [SourceAvailability],
    paths: &[PathBuf],
    previous: &BTreeMap<String, SourceEntry>,
) -> io::Result<()> {
    let mut current = BTreeMap::new();
    for path in paths {
        if let Some(entry) = source_entry(path, 300)? {
            current.insert(entry.path, entry.source_id);
        }
    }
    let old = previous
        .values()
        .map(|entry| (entry.path.clone(), entry.source_id.clone()))
        .collect::<BTreeMap<_, _>>();
    for entry in availability {
        entry.source_id = current
            .get(&entry.observed_path)
            .or_else(|| old.get(&entry.observed_path))
            .cloned()
            .unwrap_or_default();
    }
    Ok(())
}

pub fn timeline_catalog(path: &Path) -> io::Result<BTreeMap<String, Vec<TimelineRun>>> {
    load_timeline_catalog(path)
}

pub fn timeline_catalog_present(path: &Path) -> bool {
    path.join("timeline.tsv").is_file()
}

pub fn timeline_corpus_file(path: &Path, file_name: &str) -> io::Result<PathBuf> {
    let file = Path::new(file_name);
    if file.file_name().is_none() || file.components().count() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid timeline corpus file name: {}", file_name),
        ));
    }
    Ok(segment_dir(path).join(file))
}

pub fn rebuild_timeline(path: &Path) -> io::Result<()> {
    let offset_minutes = corpus_offset_minutes(path)?;
    let corpus = path.to_path_buf();
    let sources = load_source_catalog(&corpus)?;
    let files = corpus_files(&corpus)?;
    let mut runs_by_source = BTreeMap::<String, Vec<TimelineRun>>::new();
    let mut scanned_bytes = 0u64;
    for (index, file) in files.iter().enumerate() {
        let Some(source_id) = corpus_source_id_from_path(file) else {
            continue;
        };
        let (runs, bytes) = scan_existing_timeline_file(file, &source_id, offset_minutes)?;
        scanned_bytes = scanned_bytes.saturating_add(bytes);
        runs_by_source.entry(source_id).or_default().extend(runs);
        if (index + 1) % 100 == 0 || index + 1 == files.len() {
            eprintln!(
                "timeline: {}/{} corpus files scanned",
                index + 1,
                files.len()
            );
        }
    }
    write_timeline_catalog(&corpus, &sources, &runs_by_source)?;
    let run_count = runs_by_source.values().map(Vec::len).sum::<usize>();
    println!(
        "{}",
        json::Object::new()
            .name("disposition", "timeline_rebuilt")
            .name("mode", "timeline_catalog")
            .number("files", files.len() as u64)
            .number("runs", run_count as u64)
            .number("scanned_bytes", scanned_bytes)
            .name("corpus", &corpus.to_string_lossy())
            .finish()
    );
    Ok(())
}

fn scan_existing_timeline_file(
    path: &Path,
    source_id: &str,
    offset_minutes: i64,
) -> io::Result<(Vec<TimelineRun>, u64)> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut reader = BufReader::with_capacity(IO_BUFFER_SIZE, file);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let mut accumulator = TimelineAccumulator::new(source_id, file_name, offset_minutes);
    let mut offset = 0u64;
    let mut header_line = Vec::new();
    loop {
        header_line.clear();
        let read = reader.read_until(b'\n', &mut header_line)?;
        if read == 0 {
            break;
        }
        let start = offset;
        offset = offset.saturating_add(read as u64);
        let header = parse_event_header(&header_line).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {}", path.display(), error),
            )
        })?;
        skip_corpus_body(&mut reader, header.body_len)?;
        offset = offset.saturating_add(header.body_len);
        let mut separator = [0u8; 1];
        reader.read_exact(&mut separator)?;
        if separator[0] != b'\n' {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: missing event separator", path.display()),
            ));
        }
        offset = offset.saturating_add(1);
        accumulator.observe(start, offset, &header);
    }
    if offset != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: corpus size changed during timeline rebuild",
                path.display()
            ),
        ));
    }
    Ok((accumulator.finish(), size))
}

fn reconcile_timeline_runs(
    runs: &mut Vec<TimelineRun>,
    files: &[PathBuf],
    source_id: &str,
    offset_minutes: i64,
) -> io::Result<()> {
    for file in files.iter().filter(|path| is_complete_corpus_file(path)) {
        let file_name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let size = fs::metadata(file)?.len();
        let mut intervals = runs
            .iter()
            .filter(|run| run.corpus_file == file_name)
            .map(|run| (run.corpus_start, run.corpus_end))
            .collect::<Vec<_>>();
        intervals.sort_unstable();
        let mut covered_end = 0u64;
        let mut complete = size == 0;
        for (start, end) in intervals {
            if start > covered_end {
                break;
            }
            covered_end = covered_end.max(end);
            if covered_end >= size {
                complete = true;
                break;
            }
        }
        if complete {
            continue;
        }
        runs.retain(|run| run.corpus_file != file_name);
        let (recovered, _) = scan_existing_timeline_file(file, source_id, offset_minutes)?;
        runs.extend(recovered);
    }
    runs.sort_by(|left, right| {
        left.corpus_file
            .cmp(&right.corpus_file)
            .then_with(|| left.corpus_start.cmp(&right.corpus_start))
    });
    Ok(())
}

fn skip_corpus_body<R: Read>(reader: &mut R, body_len: u64) -> io::Result<()> {
    let mut limited = reader.take(body_len);
    io::copy(&mut limited, &mut io::sink())?;
    if limited.limit() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "corpus body ended before body_len",
        ));
    }
    Ok(())
}

fn corpus_source_id_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".corpus")?;
    Some(stem.split("--").next().unwrap_or(stem).to_string())
}

fn write_timeline_catalog(
    corpus: &Path,
    sources: &BTreeMap<String, SourceEntry>,
    runs_by_source: &BTreeMap<String, Vec<TimelineRun>>,
) -> io::Result<()> {
    let path = corpus.join("timeline.tsv");
    let partial_path = corpus.join("timeline.tsv.partial");
    let mut writer = BufWriter::with_capacity(IO_BUFFER_SIZE, private_fs::create(&partial_path)?);
    writeln!(writer, "{}", timeline_header())?;
    for source_id in sources.keys() {
        if let Some(runs) = runs_by_source.get(source_id) {
            for run in runs {
                write_timeline_run(&mut writer, run)?;
            }
        }
    }
    writer.flush()?;
    drop(writer);
    replace_file(&partial_path, &path)?;
    Ok(())
}

fn write_timeline_run(writer: &mut BufWriter<File>, run: &TimelineRun) -> io::Result<()> {
    let first = timeline_ref_values(run.bucket.first.as_ref());
    let last = timeline_ref_values(run.bucket.last.as_ref());
    let values = [
        encode_token(&run.source_id),
        encode_token(&run.date),
        encode_token(&run.corpus_file),
        run.corpus_start.to_string(),
        run.corpus_end.to_string(),
        run.bucket.event_count.to_string(),
        run.bucket.session_count.to_string(),
        kind_count(&run.bucket, "user").to_string(),
        kind_count(&run.bucket, "assistant").to_string(),
        kind_count(&run.bucket, "command").to_string(),
        kind_count(&run.bucket, "output").to_string(),
        kind_count(&run.bucket, "patch").to_string(),
        kind_count(&run.bucket, "unknown").to_string(),
        kind_count(&run.bucket, "invalid").to_string(),
        first.0.to_string(),
        first.1[0].clone(),
        first.1[1].clone(),
        first.1[2].clone(),
        first.1[3].clone(),
        encode_token(&first.1[4]),
        encode_token(&first.1[5]),
        encode_token(&first.1[6]),
        encode_token(&first.1[7]),
        encode_token(&first.1[8]),
        last.0.to_string(),
        last.1[0].clone(),
        last.1[1].clone(),
        last.1[2].clone(),
        last.1[3].clone(),
        encode_token(&last.1[4]),
        encode_token(&last.1[5]),
        encode_token(&last.1[6]),
        encode_token(&last.1[7]),
        encode_token(&last.1[8]),
    ];
    writeln!(writer, "{}", values.join("\t"))
}

fn kind_count(bucket: &TimelineBucket, kind: &str) -> u64 {
    bucket.kind_counts.get(kind).copied().unwrap_or(0)
}

fn timeline_ref_values(reference: Option<&TimelineRef>) -> (u8, [String; 9]) {
    let Some(reference) = reference else {
        return (0, Default::default());
    };
    (
        1,
        [
            reference.event_index.to_string(),
            reference.line.to_string(),
            reference.byte_start.to_string(),
            reference.byte_len.to_string(),
            reference.session.clone(),
            reference.turn.clone(),
            reference.role.clone(),
            reference.kind.clone(),
            reference.timestamp.clone(),
        ],
    )
}

fn load_timeline_catalog(corpus: &Path) -> io::Result<BTreeMap<String, Vec<TimelineRun>>> {
    let path = corpus.join("timeline.tsv");
    let mut result = BTreeMap::new();
    if !path.is_file() {
        return Ok(result);
    }
    let file = File::open(&path)?;
    for (line_index, line) in BufReader::with_capacity(IO_BUFFER_SIZE, file)
        .lines()
        .enumerate()
    {
        let line_number = line_index + 1;
        let line = line?;
        if line.is_empty() || line == timeline_header() {
            continue;
        }
        let parts = line.split('\t').collect::<Vec<_>>();
        let run = parse_timeline_run(&parts)
            .ok_or_else(|| invalid_catalog_line(&path, line_number, "invalid timeline row"))?;
        result
            .entry(run.source_id.clone())
            .or_insert_with(Vec::new)
            .push(run);
    }
    Ok(result)
}

fn parse_timeline_run(parts: &[&str]) -> Option<TimelineRun> {
    if parts.len() < 34 {
        return None;
    }
    let source_id = decode_token(parts[0]).ok()?;
    let date = decode_token(parts[1]).ok()?;
    let corpus_file = decode_token(parts[2]).ok()?;
    let corpus_start = parts[3].parse().ok()?;
    let corpus_end = parts[4].parse().ok()?;
    let event_count = parts[5].parse().ok()?;
    let session_count = parts[6].parse().ok()?;
    let mut kind_counts = BTreeMap::new();
    for (index, kind) in [
        "user",
        "assistant",
        "command",
        "output",
        "patch",
        "unknown",
        "invalid",
    ]
    .into_iter()
    .enumerate()
    {
        let count = parts[7 + index].parse().ok()?;
        if count != 0 {
            kind_counts.insert(kind.to_string(), count);
        }
    }
    let first = parse_timeline_ref(parts, 14)?;
    let last = parse_timeline_ref(parts, 24)?;
    let bucket = TimelineBucket {
        date: date.clone(),
        event_count,
        session_count,
        kind_counts,
        first,
        last,
        sessions: BTreeSet::new(),
    };
    Some(TimelineRun {
        source_id,
        date,
        corpus_file,
        corpus_start,
        corpus_end,
        bucket,
    })
}

fn parse_timeline_ref(parts: &[&str], start: usize) -> Option<Option<TimelineRef>> {
    if parts.get(start).copied()? != "1" {
        return Some(None);
    }
    Some(Some(TimelineRef {
        event_index: parts.get(start + 1)?.parse().ok()?,
        line: parts.get(start + 2)?.parse().ok()?,
        byte_start: parts.get(start + 3)?.parse().ok()?,
        byte_len: parts.get(start + 4)?.parse().ok()?,
        session: decode_token(parts.get(start + 5)?).ok()?,
        turn: decode_token(parts.get(start + 6)?).ok()?,
        role: decode_token(parts.get(start + 7)?).ok()?,
        kind: decode_token(parts.get(start + 8)?).ok()?,
        timestamp: decode_token(parts.get(start + 9)?).ok()?,
    }))
}

fn load_source_catalog(corpus: &Path) -> io::Result<BTreeMap<String, SourceEntry>> {
    let complete_path = corpus.join("sources.tsv");
    let partial_path = corpus.join("sources.tsv.partial");
    let mut catalog = load_source_catalog_paths(&[complete_path])?;
    if let Ok(resumed) = load_source_catalog_paths(std::slice::from_ref(&partial_path)) {
        catalog.extend(resumed);
    }
    Ok(catalog)
}

fn load_source_catalog_paths(paths: &[PathBuf]) -> io::Result<BTreeMap<String, SourceEntry>> {
    let mut result = BTreeMap::new();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let file = File::open(path)?;
        let mut header_seen = false;
        for (line_index, line) in BufReader::with_capacity(IO_BUFFER_SIZE, file)
            .lines()
            .enumerate()
        {
            let line_number = line_index + 1;
            let line = line?;
            if line.is_empty() {
                continue;
            }
            if !header_seen {
                header_seen = true;
                parse_catalog_header(&line, path)?;
                continue;
            }
            let parts = line.split('\t').collect::<Vec<_>>();
            if parts.len() != 20 {
                return Err(invalid_catalog_line(
                    path,
                    line_number,
                    "source row does not have the 20 columns this build writes",
                ));
            }
            let source_id = catalog_token(&parts, 0, None, path, line_number, "source_id")?;
            let app = catalog_token(&parts, 1, None, path, line_number, "app")?;
            let source_path = catalog_token(&parts, 2, None, path, line_number, "path")?;
            let fallback_session =
                catalog_token(&parts, 3, None, path, line_number, "fallback_session")?;
            let size = catalog_u64(&parts, 4, None, path, line_number, "size")?;
            let modified_ms = catalog_u64(&parts, 5, None, path, line_number, "modified_ms")?;
            let committed_byte_end =
                catalog_u64(&parts, 6, None, path, line_number, "committed_byte_end")?;
            let event_count = catalog_u64(&parts, 7, None, path, line_number, "event_count")?;
            let last_line = catalog_u64(&parts, 8, None, path, line_number, "last_line")?;
            let last_session =
                catalog_token(&parts, 9, Some(""), path, line_number, "last_session")?;
            let turn_sessions =
                catalog_token(&parts, 10, Some(""), path, line_number, "turn_sessions")?;
            let turn_turns = catalog_token(&parts, 11, Some(""), path, line_number, "turn_turns")?;
            let turn_has_user =
                catalog_token(&parts, 12, Some(""), path, line_number, "turn_has_user")?;
            let disposition = catalog_token(
                &parts,
                13,
                Some("legacy_catalog"),
                path,
                line_number,
                "disposition",
            )?;
            let checkpoint_valid =
                catalog_bool(&parts, 14, false, path, line_number, "checkpoint_valid")?;
            let output_preview =
                catalog_u64(&parts, 15, None, path, line_number, "output_preview")?;
            let volume_serial_number = catalog_u64(
                &parts,
                16,
                Some(0),
                path,
                line_number,
                "volume_serial_number",
            )?;
            let file_id = catalog_u64(&parts, 17, None, path, line_number, "file_id")?;
            let synced_at_ms = catalog_u64(&parts, 18, None, path, line_number, "synced_at_ms")?;
            let last_cwd = catalog_token(&parts, 19, Some(""), path, line_number, "last_cwd")?;
            result.insert(
                source_id.clone(),
                SourceEntry {
                    source_id,
                    app,
                    path: source_path,
                    fallback_session,
                    size,
                    modified_ms,
                    committed_byte_end,
                    event_count,
                    last_line,
                    last_session,
                    turn_sessions,
                    turn_turns,
                    turn_has_user,
                    disposition,
                    checkpoint_valid,
                    output_preview,
                    volume_serial_number,
                    file_id,
                    synced_at_ms,
                    last_cwd,
                },
            );
        }
    }
    Ok(result)
}

fn invalid_catalog_line(path: &Path, line_number: usize, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}:{}: {}", path.display(), line_number, reason),
    )
}

fn catalog_token(
    parts: &[&str],
    index: usize,
    default: Option<&str>,
    path: &Path,
    line_number: usize,
    field: &str,
) -> io::Result<String> {
    let value = parts
        .get(index)
        .copied()
        .or(default)
        .ok_or_else(|| invalid_catalog_line(path, line_number, field))?;
    decode_token(value).map_err(|_| invalid_catalog_line(path, line_number, field))
}

fn catalog_u64(
    parts: &[&str],
    index: usize,
    default: Option<u64>,
    path: &Path,
    line_number: usize,
    field: &str,
) -> io::Result<u64> {
    let Some(value) = parts.get(index).copied() else {
        return default.ok_or_else(|| invalid_catalog_line(path, line_number, field));
    };
    value
        .parse()
        .map_err(|_| invalid_catalog_line(path, line_number, field))
}

fn catalog_bool(
    parts: &[&str],
    index: usize,
    default: bool,
    path: &Path,
    line_number: usize,
    field: &str,
) -> io::Result<bool> {
    let Some(value) = parts.get(index).copied() else {
        return Ok(default);
    };
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(invalid_catalog_line(path, line_number, field)),
    }
}

fn committed_end_is_record_boundary(path: &str, committed_byte_end: u64) -> io::Result<bool> {
    if committed_byte_end == 0 {
        return Ok(true);
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(committed_byte_end - 1))?;
    let mut byte = [0u8; 1];
    match file.read_exact(&mut byte) {
        Ok(()) => Ok(byte[0] == b'\n'),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

fn same_snapshot(old: &SourceEntry, new: &SourceEntry) -> bool {
    old.path == new.path
        && same_file_identity(old, new)
        && old.size == new.size
        && old.modified_ms == new.modified_ms
}

fn can_append(old: &SourceEntry, new: &SourceEntry) -> bool {
    old.path == new.path
        && same_file_identity(old, new)
        && new.size >= old.committed_byte_end
        && (new.size > old.committed_byte_end || old.committed_byte_end < old.size)
}

fn same_file_identity(old: &SourceEntry, new: &SourceEntry) -> bool {
    (old.volume_serial_number == 0
        || new.volume_serial_number == 0
        || old.volume_serial_number == new.volume_serial_number)
        && (old.file_id == 0 || new.file_id == 0 || old.file_id == new.file_id)
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn modified_ms(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(windows)]
#[repr(C)]
struct FileTime {
    low_date_time: u32,
    high_date_time: u32,
}

#[cfg(windows)]
#[repr(C)]
struct ByHandleFileInformation {
    file_attributes: u32,
    creation_time: FileTime,
    last_access_time: FileTime,
    last_write_time: FileTime,
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandle(
        file: *mut std::ffi::c_void,
        information: *mut ByHandleFileInformation,
    ) -> i32;
}

#[cfg(windows)]
fn file_identity(path: &Path) -> (u64, u64) {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;

    let Ok(file) = File::open(path) else {
        return (0, 0);
    };
    let mut information = MaybeUninit::<ByHandleFileInformation>::zeroed();
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if result == 0 {
        return (0, 0);
    }
    let information = unsafe { information.assume_init() };
    let file_id =
        (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low);
    (u64::from(information.volume_serial_number), file_id)
}

#[cfg(unix)]
fn file_identity(path: &Path) -> (u64, u64) {
    let Ok(metadata) = fs::metadata(path) else {
        return (0, 0);
    };
    identity_of(path, &metadata)
}

#[cfg(not(any(windows, unix)))]
fn file_identity(_path: &Path) -> (u64, u64) {
    (0, 0)
}

/// The identity of a file whose metadata is already read: on Unix it is in the metadata.
#[cfg(unix)]
fn identity_of(_path: &Path, metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;

    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn identity_of(path: &Path, _metadata: &fs::Metadata) -> (u64, u64) {
    file_identity(path)
}

fn rebuild_output_name(segment_dir: &Path, entry: &SourceEntry) -> String {
    let prefix = format!(
        "{}--rewrite-{}-{}",
        entry.source_id, entry.modified_ms, entry.size
    );
    let mut suffix = String::new();
    let mut attempt = 0u64;
    loop {
        let name = format!("{}{}.corpus", prefix, suffix);
        let path = segment_dir.join(&name);
        let partial = path.with_extension("corpus.partial");
        if !path.exists() && !partial.exists() {
            return name;
        }
        attempt = attempt.saturating_add(1);
        suffix = format!("-{}", attempt);
    }
}

fn corpus_source_files(root: &Path, source_id: &str) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !root.is_dir() {
        return Ok(files);
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if path.is_file() && is_source_corpus_name(name, source_id) {
            files.push(path);
        }
    }
    sort_corpus_files(&mut files);
    Ok(files)
}

fn corpus_file_map(root: &Path) -> io::Result<BTreeMap<String, Vec<PathBuf>>> {
    let mut result = BTreeMap::<String, Vec<PathBuf>>::new();
    if !root.is_dir() {
        return Ok(result);
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let Some(base) = name
            .strip_suffix(".corpus.partial")
            .or_else(|| name.strip_suffix(".corpus"))
        else {
            continue;
        };
        let source_id = base.split("--").next().unwrap_or(base).to_string();
        result.entry(source_id).or_default().push(path);
    }
    for files in result.values_mut() {
        sort_corpus_files(files);
    }
    Ok(result)
}

fn discard_partial_corpus_files(files: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut kept = Vec::with_capacity(files.len());
    for path in files.iter() {
        let is_partial = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.ends_with(".corpus.partial"))
            .unwrap_or(false);
        if !is_partial {
            kept.push(path.clone());
            continue;
        }
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    sort_corpus_files(&mut kept);
    kept.dedup();
    *files = kept;
    Ok(())
}

fn sort_corpus_files(files: &mut [PathBuf]) {
    files.sort_by(|left, right| {
        corpus_file_order(left)
            .cmp(&corpus_file_order(right))
            .then_with(|| left.cmp(right))
    });
}

fn corpus_file_order(path: &Path) -> (u8, u64, String) {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let Some((_, suffix)) = name.split_once("--") else {
        return (0, 0, name.to_string());
    };
    let start = if suffix.starts_with("rewrite-") {
        0
    } else {
        suffix
            .split('-')
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(u64::MAX)
    };
    (1, start, name.to_string())
}

fn corpus_source_range(path: &Path, legacy_end: u64) -> Option<(u64, u64)> {
    let name = path.file_name()?.to_str()?.strip_suffix(".corpus")?;
    let Some((_, suffix)) = name.split_once("--") else {
        return Some((0, legacy_end));
    };
    if let Some(rewrite) = suffix.strip_prefix("rewrite-") {
        let mut values = rewrite.split('-');
        let _modified_ms = values.next()?.parse::<u64>().ok()?;
        let end = values.next()?.parse::<u64>().ok()?;
        return Some((0, end));
    }
    let mut values = suffix.split('-');
    let start = values.next()?.parse::<u64>().ok()?;
    let end = values.next()?.parse::<u64>().ok()?;
    Some((start, end))
}

fn is_source_corpus_name(name: &str, source_id: &str) -> bool {
    name == format!("{}.corpus", source_id)
        || name == format!("{}.corpus.partial", source_id)
        || name.starts_with(&format!("{}--", source_id))
}

fn is_complete_corpus_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(|name| name.ends_with(".corpus"))
        .unwrap_or(false)
}

fn remove_source_corpus_files_except(files: &[PathBuf], keep: &Path) -> io::Result<()> {
    for path in files {
        if path != keep && path.exists() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn remove_corpus_files_not_in_catalog(
    root: &Path,
    catalog: &BTreeMap<String, SourceEntry>,
) -> io::Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let Some(base) = name
            .strip_suffix(".corpus.partial")
            .or_else(|| name.strip_suffix(".corpus"))
        else {
            continue;
        };
        let source_id = base.split("--").next().unwrap_or(base);
        if !catalog.contains_key(source_id) {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn source_input_covers_entry(inputs: &[String], entry: &SourceEntry) -> bool {
    let entry_path = Path::new(&entry.path);
    inputs.iter().any(|input| {
        let input_path = PathBuf::from(normalize_source_input(input));
        if input_path.is_dir() {
            entry_path.starts_with(input_path)
        } else {
            entry_path == input_path
        }
    })
}

/// Decides the origin of a whole source before its records are read: Claude subagent
/// transcripts live under `subagents/`, and a Codex rollout opens with a session_meta record
/// that names its parent thread or the `codex exec` originator.
fn source_origin(entry: &SourceEntry) -> SourceOrigin {
    origin_of(&entry.app, Path::new(&entry.path))
}

fn origin_of(app: &str, path: &Path) -> SourceOrigin {
    match app {
        "claude" => {
            if path
                .to_string_lossy()
                .replace('\\', "/")
                .contains("/subagents/")
            {
                SourceOrigin::ClaudeSubagent
            } else {
                SourceOrigin::ClaudeMain
            }
        }
        "codex" => codex_origin(path),
        _ => SourceOrigin::Generic,
    }
}

fn codex_origin(path: &Path) -> SourceOrigin {
    let Ok(file) = File::open(path) else {
        return SourceOrigin::CodexThread;
    };
    let mut reader = BufReader::new(file.take(MAX_RECORD_BYTES as u64));
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).is_err() {
        return SourceOrigin::CodexThread;
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    let Ok(fields) = parse_record(&line, 0) else {
        return SourceOrigin::CodexThread;
    };
    let value = |wanted: &str| {
        fields
            .iter()
            .find(|field| field.path == wanted)
            .map(|field| field.value.as_str())
    };
    if value("/type") != Some("session_meta") {
        return SourceOrigin::CodexThread;
    }
    let spawned = value("/payload/parent_thread_id").is_some_and(|parent| !parent.is_empty())
        || fields
            .iter()
            .any(|field| field.path.starts_with("/payload/source/subagent"));
    if spawned {
        return SourceOrigin::CodexChild;
    }
    if value("/payload/originator") == Some("codex_exec")
        || value("/payload/source") == Some("exec")
    {
        return SourceOrigin::CodexExec;
    }
    SourceOrigin::CodexThread
}

/// Which agent wrote a log: decided by the nearest `.codex` or `.claude` directory above it,
/// else by its first records, so copies kept elsewhere (managed imports, logs from another
/// computer) are still read as Claude Code or Codex transcripts. `None` while a log outside
/// those directories has no complete record to tell by.
fn source_app(path: &Path) -> Option<&'static str> {
    let by_directory = path.ancestors().skip(1).find_map(|ancestor| {
        match ancestor
            .file_name()?
            .to_str()?
            .to_ascii_lowercase()
            .as_str()
        {
            ".codex" => Some("codex"),
            ".claude" => Some("claude"),
            _ => None,
        }
    });
    if by_directory.is_some() {
        return by_directory;
    }
    if !first_line_complete(path) {
        return None;
    }
    Some(sniff_kind(path).unwrap_or("other"))
}

/// Whether a log holds at least one complete record. A first line longer than any record can
/// be is complete too: it is projected as an invalid record.
pub fn first_line_complete(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file.take(MAX_RECORD_BYTES as u64 + 1));
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).is_ok()
        && (line.last() == Some(&b'\n') || line.len() > MAX_RECORD_BYTES)
}

const SNIFF_LINES: usize = 16;
const SNIFF_BYTES: u64 = 1024 * 1024;

fn sniff_kind(path: &Path) -> Option<&'static str> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file.take(SNIFF_BYTES));
    let mut line = Vec::new();
    for _ in 0..SNIFF_LINES {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 {
            break;
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        let Ok(fields) = parse_record(&line, 0) else {
            continue;
        };
        let value = |wanted: &str| {
            fields
                .iter()
                .find(|field| field.path == wanted)
                .map(|field| field.value.as_str())
        };
        let has = |wanted: &str| fields.iter().any(|field| field.path == wanted);
        let codex_item = matches!(
            value("/type"),
            Some("response_item" | "event_msg" | "turn_context" | "compacted")
        ) && fields
            .iter()
            .any(|field| field.path.starts_with("/payload/"));
        if value("/type") == Some("session_meta") || codex_item {
            return Some("codex");
        }
        if has("/sessionId") && has("/uuid") {
            return Some("claude");
        }
    }
    None
}

fn collect_sources(inputs: &[String]) -> SourceCollection {
    let mut collection = SourceCollection::default();
    for input in inputs {
        let input_path = normalize_source_input(input);
        collect_source_path(&input_path, Path::new(&input_path), true, &mut collection);
    }
    collection.paths.sort();
    collection.paths.dedup();
    collection.availability.sort_by(|left, right| {
        left.input_path
            .cmp(&right.input_path)
            .then_with(|| left.observed_path.cmp(&right.observed_path))
            .then_with(|| left.disposition.cmp(&right.disposition))
    });
    collection.availability.dedup();
    collection
}

fn collect_source_path(
    input_path: &str,
    path: &Path,
    is_input: bool,
    collection: &mut SourceCollection,
) {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            push_source_availability(
                collection,
                input_path,
                path,
                if error.kind() == io::ErrorKind::NotFound {
                    "missing"
                } else {
                    "unreadable"
                },
            );
            return;
        }
    };
    if metadata.is_file() {
        let is_jsonl = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.eq_ignore_ascii_case("jsonl"))
            .unwrap_or(false);
        if is_jsonl {
            collection.paths.push(path.to_path_buf());
            if is_input {
                push_source_availability(collection, input_path, path, "present");
            }
        } else if is_input {
            push_source_availability(collection, input_path, path, "not_jsonl");
        }
        return;
    }
    if !metadata.is_dir() {
        if is_input {
            push_source_availability(collection, input_path, path, "unsupported");
        }
        return;
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => {
            push_source_availability(collection, input_path, path, "unreadable");
            return;
        }
    };
    if is_input {
        push_source_availability(collection, input_path, path, "present");
    }
    let mut children = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => children.push(entry.path()),
            Err(_) => push_source_availability(collection, input_path, path, "unreadable"),
        }
    }
    children.sort();
    for child in children {
        collect_source_path(input_path, &child, false, collection);
    }
}

fn push_source_availability(
    collection: &mut SourceCollection,
    input_path: &str,
    path: &Path,
    disposition: &str,
) {
    collection.availability.push(SourceAvailability {
        input_path: input_path.to_string(),
        observed_path: normalize_source_input(&path.to_string_lossy()),
        disposition: disposition.to_string(),
        source_id: String::new(),
    });
}

fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{
        build, build_with_preview, corpus_files, discard_partial_corpus_files, process_source,
        read_bounded_line, registered_sources, source_catalog, source_declares_session,
        source_is_complete, source_projection_complete, timeline_catalog, SourceEntry,
        MAX_RECORD_BYTES,
    };
    use crate::format::{event_header_line, EventHeader};
    use std::fs::{self, File, OpenOptions};
    use std::io::{BufReader, Cursor, Write};
    use std::path::PathBuf;

    #[test]
    fn freshness_is_the_test_a_sync_makes() {
        use super::{freshness, source_entry, Freshness};
        let root = std::env::temp_dir().join(format!("ebira-freshness-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let path = root.join("log.jsonl");
        let record = "{\"type\":\"note\",\"text\":\"one\"}
";
        fs::write(&path, record).expect("write log");
        let read = || {
            let mut entry = source_entry(&path, 300)
                .expect("entry")
                .expect("a complete record");
            entry.committed_byte_end = entry.size;
            entry.checkpoint_valid = true;
            entry
        };
        let entry = read();
        assert_eq!(freshness(&entry), Freshness::Current);

        OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(record.as_bytes()))
            .expect("append");
        assert_eq!(
            freshness(&entry),
            Freshness::Behind(record.len() as u64),
            "an appended log is behind by what was appended"
        );

        let entry = read();
        let earlier = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .expect("modified time")
            - std::time::Duration::from_secs(60);
        let same_size = record.replace("one", "two").repeat(2);
        fs::write(&path, &same_size).expect("rewrite in place");
        File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(earlier))
            .expect("set modified time");
        assert_eq!(
            freshness(&entry),
            Freshness::Rewritten(same_size.len() as u64),
            "a log rewritten at the same size is read again"
        );
        fs::write(&path, "").expect("truncate");
        assert_eq!(freshness(&entry), Freshness::Rewritten(0));
        fs::remove_file(&path).expect("remove log");
        assert_eq!(freshness(&entry), Freshness::Unreachable);
        fs::remove_dir_all(root).expect("remove temp directory");
    }

    #[test]
    fn bounded_reader_preserves_incomplete_tail_without_committing_it() {
        let mut reader = BufReader::new(Cursor::new(b"partial".to_vec()));
        let mut line = Vec::new();
        let result = read_bounded_line(&mut reader, &mut line)
            .expect("read succeeds")
            .expect("tail is observed");
        assert!(!result.complete);
        assert_eq!(result.total_len, 7);
    }

    #[test]
    fn bounded_reader_marks_oversized_complete_record_without_growing_buffer() {
        let mut bytes = vec![b'x'; MAX_RECORD_BYTES + 1];
        bytes.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(bytes));
        let mut line = Vec::new();
        let result = read_bounded_line(&mut reader, &mut line)
            .expect("read succeeds")
            .expect("record is observed");
        assert!(result.complete);
        assert!(result.oversized);
        assert_eq!(result.total_len, (MAX_RECORD_BYTES + 2) as u64);
        assert_eq!(line.len(), MAX_RECORD_BYTES);
    }

    #[test]
    fn partial_segment_is_deleted() {
        let root =
            std::env::temp_dir().join(format!("ebira-partial-discard-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let partial = root.join("source.corpus.partial");
        let mut file = File::create(&partial).expect("create partial corpus");
        let header = event_header_line(&EventHeader {
            event_index: 0,
            source_id: "source".to_string(),
            line: 1,
            byte_start: 0,
            byte_len: 4,
            session: "session".to_string(),
            turn: "turn".to_string(),
            kind: "user".to_string(),
            body_len: 4,
            ..EventHeader::default()
        });
        file.write_all(header.as_bytes()).expect("write header");
        file.write_all(
            b"body
truncated-header",
        )
        .expect("write body");
        drop(file);

        let mut files = vec![partial.clone()];
        discard_partial_corpus_files(&mut files).expect("discard partial corpus");

        assert!(files.is_empty());
        assert!(!partial.exists());
        assert!(!root.join("source.corpus").exists());
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn unreadable_partial_segment_is_deleted() {
        let root = std::env::temp_dir().join(format!(
            "ebira-empty-partial-recovery-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let partial = root.join("source.corpus.partial");
        fs::write(&partial, b"incomplete header").expect("write incomplete partial");

        let mut files = vec![partial.clone()];
        discard_partial_corpus_files(&mut files).expect("discard unreadable partial");

        assert!(files.is_empty());
        assert!(!partial.exists());
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn partial_cleanup_preserves_promoted_segment() {
        let root = std::env::temp_dir().join(format!(
            "ebira-duplicate-partial-recovery-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let target = root.join("source.corpus");
        let partial = root.join("source.corpus.partial");
        let header = event_header_line(&EventHeader {
            event_index: 0,
            source_id: "source".to_string(),
            line: 1,
            byte_start: 0,
            byte_len: 4,
            session: "session".to_string(),
            turn: "turn".to_string(),
            kind: "user".to_string(),
            body_len: 4,
            ..EventHeader::default()
        });
        for path in [&target, &partial] {
            let mut file = File::create(path).expect("create corpus file");
            file.write_all(header.as_bytes()).expect("write header");
            file.write_all(b"body\n").expect("write body");
        }

        let mut files = vec![target.clone(), partial.clone()];
        discard_partial_corpus_files(&mut files).expect("discard duplicate partial");

        assert_eq!(files, vec![target.clone()]);
        assert!(!partial.exists());
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn source_completion_accepts_successful_append_disposition() {
        let entry = SourceEntry {
            checkpoint_valid: true,
            committed_byte_end: 10,
            size: 10,
            disposition: "source_appended".to_string(),
            ..SourceEntry::default()
        };
        assert!(source_is_complete(&entry));
    }

    #[test]
    fn rewrite_segment_is_read_before_appended_segment() {
        let mut files = vec![
            PathBuf::from("source--100-200.corpus"),
            PathBuf::from("source--rewrite-1-100.corpus"),
        ];
        super::sort_corpus_files(&mut files);
        assert_eq!(
            files[0].file_name().and_then(|name| name.to_str()),
            Some("source--rewrite-1-100.corpus")
        );
    }

    #[test]
    fn directory_input_discovers_added_file_on_sync() {
        let root =
            std::env::temp_dir().join(format!("ebira-directory-input-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let logs = root.join("logs");
        let output = root.join("corpus");
        fs::create_dir_all(&logs).expect("create log directory");
        fs::write(
            logs.join("a.jsonl"),
            b"{\"event_msg\":{\"type\":\"user_message\",\"message\":\"directory-a-unique\"},\"role\":\"user\",\"session_id\":\"s1\",\"turn_id\":\"t1\"}\n",
        )
        .expect("write first source");

        let input = logs.to_string_lossy().into_owned();
        build(&[input], &output, false, 300, &[]).expect("build directory source");
        let canonical_logs = fs::canonicalize(&logs)
            .expect("canonical log directory")
            .to_string_lossy()
            .into_owned();
        assert!(registered_sources(&output)
            .expect("registered inputs")
            .contains(&canonical_logs));

        fs::write(
            logs.join("b.jsonl"),
            b"{\"event_msg\":{\"type\":\"user_message\",\"message\":\"directory-b-unique\"},\"role\":\"user\",\"session_id\":\"s2\",\"turn_id\":\"t2\"}\n",
        )
        .expect("write added source");
        let registered = registered_sources(&output).expect("registered inputs after build");
        build(&registered, &output, true, 300, &[]).expect("sync directory source");

        let catalog = source_catalog(&output).expect("source catalog");
        assert_eq!(catalog.len(), 2);
        let corpus_files = corpus_files(&output).expect("corpus files");
        assert!(corpus_files.into_iter().any(|path| {
            fs::read(path)
                .expect("read corpus file")
                .windows(b"directory-b-unique".len())
                .any(|window| window == b"directory-b-unique")
        }));
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn sync_inherits_explicit_output_preview_and_keeps_full_output_mode() {
        let root = std::env::temp_dir().join(format!(
            "ebira-output-preview-inheritance-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let source = root.join("session.jsonl");
        let output = root.join("corpus");
        fs::write(
            &source,
            b"{\"type\":\"command_output\",\"output\":\"first full output\",\"session_id\":\"s\",\"turn_id\":\"t1\"}\n",
        )
        .expect("write source");
        let input = source.to_string_lossy().into_owned();
        build_with_preview(std::slice::from_ref(&input), &output, false, Some(0), &[])
            .expect("build full output corpus");
        let mut file = OpenOptions::new()
            .append(true)
            .open(&source)
            .expect("open source for append");
        file.write_all(
            b"{\"type\":\"command_output\",\"output\":\"second full output beyond the preview\",\"session_id\":\"s\",\"turn_id\":\"t2\"}\n",
        )
        .expect("append source");
        drop(file);

        let report =
            build_with_preview(&[input], &output, true, None, &[]).expect("sync inherited preview");
        assert_eq!(report.sources_appended, 1);
        let current_corpus_bytes = corpus_files(&output)
            .expect("corpus files")
            .iter()
            .try_fold(0u64, |sum, path| {
                Ok::<u64, std::io::Error>(sum.saturating_add(fs::metadata(path)?.len()))
            })
            .expect("measure corpus files");
        assert_eq!(report.corpus_bytes, current_corpus_bytes);
        let entry = source_catalog(&output)
            .expect("source catalog")
            .into_values()
            .next()
            .expect("source entry");
        assert_eq!(entry.output_preview, 0);
        let corpus = corpus_files(&output)
            .expect("corpus files")
            .into_iter()
            .flat_map(|path| fs::read(path).expect("read corpus"))
            .collect::<Vec<_>>();
        assert!(corpus
            .windows(b"second full output beyond the preview".len())
            .any(|window| window == b"second full output beyond the preview"));
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn directory_input_ignores_deleted_catalog_leaf_on_sync() {
        let root =
            std::env::temp_dir().join(format!("ebira-directory-delete-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let logs = root.join("logs");
        let output = root.join("corpus");
        fs::create_dir_all(&logs).expect("create log directory");
        let first = logs.join("a.jsonl");
        fs::write(
            &first,
            b"{\"event_msg\":{\"type\":\"user_message\",\"message\":\"directory-delete-a\"},\"role\":\"user\",\"session_id\":\"s1\",\"turn_id\":\"t1\"}\n",
        )
        .expect("write first source");

        let input = logs.to_string_lossy().into_owned();
        build(&[input], &output, false, 300, &[]).expect("build directory source");
        fs::remove_file(first).expect("remove old source");
        fs::write(
            logs.join("c.jsonl"),
            b"{\"event_msg\":{\"type\":\"user_message\",\"message\":\"directory-delete-c\"},\"role\":\"user\",\"session_id\":\"s3\",\"turn_id\":\"t3\"}\n",
        )
        .expect("write replacement source");

        let registered = registered_sources(&output).expect("registered inputs");
        build(&registered, &output, true, 300, &[]).expect("sync directory source");
        let catalog = source_catalog(&output).expect("source catalog");
        assert_eq!(catalog.len(), 1);
        assert!(corpus_files(&output).into_iter().flatten().any(|path| {
            fs::read(path)
                .expect("read corpus file")
                .windows(b"directory-delete-c".len())
                .any(|window| window == b"directory-delete-c")
        }));
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn unchanged_source_rebuild_recovers_missing_timeline_runs() {
        let root =
            std::env::temp_dir().join(format!("ebira-timeline-recovery-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let source = root.join("session.jsonl");
        let output = root.join("corpus");
        fs::write(
            &source,
            b"{\"type\":\"user_message\",\"message\":\"timeline-one\",\"session_id\":\"s\",\"turn_id\":\"t1\",\"timestamp\":\"2026-02-22T10:00:00Z\"}\n{\"type\":\"user_message\",\"message\":\"timeline-two\",\"session_id\":\"s\",\"turn_id\":\"t2\",\"timestamp\":\"2026-02-23T10:00:00Z\"}\n",
        )
        .expect("write source");
        let input = source.to_string_lossy().into_owned();
        build(&[input], &output, false, 300, &[]).expect("build source");
        fs::remove_file(output.join("timeline.tsv")).expect("remove timeline");

        let registered = registered_sources(&output).expect("registered inputs");
        build(&registered, &output, true, 300, &[]).expect("sync source");
        let timeline = timeline_catalog(&output).expect("timeline catalog");
        assert_eq!(timeline.values().map(Vec::len).sum::<usize>(), 2);
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn appended_source_rebuilds_missing_timeline_runs() {
        let root = std::env::temp_dir().join(format!(
            "ebira-timeline-append-recovery-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let source = root.join("session.jsonl");
        let output = root.join("corpus");
        fs::write(
            &source,
            b"{\"type\":\"user_message\",\"message\":\"timeline-one\",\"session_id\":\"s\",\"turn_id\":\"t1\",\"timestamp\":\"2026-02-22T10:00:00Z\"}\n",
        )
        .expect("write initial source");
        let input = source.to_string_lossy().into_owned();
        build(std::slice::from_ref(&input), &output, false, 300, &[]).expect("build source");
        let source_id = source_catalog(&output)
            .expect("source catalog")
            .into_keys()
            .next()
            .expect("source id");
        fs::remove_file(output.join("timeline.tsv")).expect("remove timeline");

        let mut file = OpenOptions::new()
            .append(true)
            .open(&source)
            .expect("open source for append");
        file.write_all(
            b"{\"type\":\"user_message\",\"message\":\"timeline-two\",\"session_id\":\"s\",\"turn_id\":\"t2\",\"timestamp\":\"2026-02-23T10:00:00Z\"}\n",
        )
        .expect("append source");
        drop(file);

        build(&[input], &output, true, 300, &[]).expect("sync source");
        let timeline = timeline_catalog(&output).expect("timeline catalog");
        let dates = timeline[&source_id]
            .iter()
            .map(|run| run.date.as_str())
            .collect::<Vec<_>>();
        assert!(dates.contains(&"2026-02-22"));
        assert!(dates.contains(&"2026-02-23"));
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn nonincremental_build_replaces_old_sources() {
        let root = std::env::temp_dir().join(format!("ebira-fresh-build-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let first = root.join("first.jsonl");
        let second = root.join("second.jsonl");
        let output = root.join("corpus");
        fs::write(
            &first,
            b"{\"type\":\"user_message\",\"message\":\"fresh-old\",\"session_id\":\"s1\",\"turn_id\":\"t1\"}\n",
        )
        .expect("write first source");
        fs::write(
            &second,
            b"{\"type\":\"user_message\",\"message\":\"fresh-new\",\"session_id\":\"s2\",\"turn_id\":\"t2\"}\n",
        )
        .expect("write second source");
        build(
            &[first.to_string_lossy().into_owned()],
            &output,
            false,
            300,
            &[],
        )
        .expect("build first source");
        build(
            &[second.to_string_lossy().into_owned()],
            &output,
            false,
            300,
            &[],
        )
        .expect("replace with second source");
        let catalog = source_catalog(&output).expect("source catalog");
        assert_eq!(catalog.len(), 1);
        let bytes = corpus_files(&output)
            .expect("corpus files")
            .into_iter()
            .flat_map(|path| fs::read(path).expect("read corpus"))
            .collect::<Vec<_>>();
        assert!(bytes
            .windows(b"fresh-new".len())
            .any(|window| window == b"fresh-new"));
        assert!(!bytes
            .windows(b"fresh-old".len())
            .any(|window| window == b"fresh-old"));
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn oversized_record_preserves_buffered_prefix() {
        let root =
            std::env::temp_dir().join(format!("ebira-oversized-body-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let source = root.join("huge.jsonl");
        let output = root.join("corpus");
        let filler = "x".repeat(MAX_RECORD_BYTES);
        fs::write(
            &source,
            format!(
                "{{\"type\":\"compacted\",\"session_id\":\"s1\",\"head\":\"OVERSIZED-HEAD-MARK\",\"pad\":\"{filler}\"}}\n"
            ),
        )
        .expect("write source");
        let input = source.to_string_lossy().into_owned();
        let report =
            build(std::slice::from_ref(&input), &output, false, 300, &[]).expect("build source");
        assert_eq!(report.invalid_records, 1, "the record is past the bound");

        let body = corpus_files(&output)
            .expect("corpus files")
            .into_iter()
            .map(|path| fs::read_to_string(path).expect("read segment"))
            .collect::<String>();
        assert!(
            body.contains("OVERSIZED-HEAD-MARK"),
            "buffered record prefix was lost"
        );
        assert!(
            body.contains(crate::core::PROJECTION_BOUND_MARKER),
            "and it is marked as bounded like any other projected text"
        );
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn missing_source_reduces_its_own_coverage() {
        let root = std::env::temp_dir().join(format!("ebira-missing-input-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("present")).expect("create temp directory");
        let output = root.join("corpus");
        fs::write(
            root.join("present/kept.jsonl"),
            b"{\"type\":\"user_message\",\"message\":\"kept-source\",\"session_id\":\"s1\",\"turn_id\":\"t1\"}\n",
        )
        .expect("write source");
        let present = root.join("present").to_string_lossy().into_owned();
        let gone = root.join("gone").to_string_lossy().into_owned();

        let report = build(&[present, gone], &output, false, 300, &[]).expect("build continues");
        assert_eq!(report.sources_seen, 1, "the source that is there is read");
        assert_eq!(
            report.unreadable_source_paths, 1,
            "the input that is not there is counted, not dropped"
        );

        let all_gone = root.join("nowhere").to_string_lossy().into_owned();
        assert!(build(&[all_gone], &output, false, 300, &[]).is_err());
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn unreadable_partial_catalog_is_discarded() {
        let root =
            std::env::temp_dir().join(format!("ebira-partial-catalog-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let source = root.join("source.jsonl");
        let output = root.join("corpus");
        fs::write(
            &source,
            b"{\"type\":\"user_message\",\"message\":\"partial-catalog\",\"session_id\":\"s1\",\"turn_id\":\"t1\"}\n",
        )
        .expect("write source");
        let input = source.to_string_lossy().into_owned();
        build(std::slice::from_ref(&input), &output, false, 300, &[]).expect("build source");
        fs::write(output.join("sources.tsv.partial"), b"broken\trow\n")
            .expect("leave a partial catalog behind");

        let catalog = super::source_catalog(&output).expect("the corpus still opens");
        assert_eq!(catalog.len(), 1, "the complete catalog still answers");
        build(std::slice::from_ref(&input), &output, true, 300, &[])
            .expect("sync after partial catalog");
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn malformed_source_catalog_stops_sync_before_projection_cleanup() {
        let root =
            std::env::temp_dir().join(format!("ebira-catalog-parse-error-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let first = root.join("first.jsonl");
        let second = root.join("second.jsonl");
        let output = root.join("corpus");
        fs::write(
            &first,
            b"{\"type\":\"user_message\",\"message\":\"catalog-old\",\"session_id\":\"s1\",\"turn_id\":\"t1\"}\n",
        )
        .expect("write first source");
        fs::write(
            &second,
            b"{\"type\":\"user_message\",\"message\":\"catalog-new\",\"session_id\":\"s2\",\"turn_id\":\"t2\"}\n",
        )
        .expect("write second source");
        let first_input = first.to_string_lossy().into_owned();
        build(std::slice::from_ref(&first_input), &output, false, 300, &[])
            .expect("build first source");
        let old_files = corpus_files(&output).expect("old corpus files");
        fs::write(output.join("sources.tsv"), b"broken\trow\n").expect("corrupt catalog");

        let second_input = second.to_string_lossy().into_owned();
        assert!(build(&[second_input], &output, true, 300, &[]).is_err());
        assert!(old_files.iter().all(|path| path.is_file()));
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn source_projection_detects_a_missing_append_segment() {
        let root =
            std::env::temp_dir().join(format!("ebira-projection-coverage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let first = root.join("source--rewrite-1-100.corpus");
        let last = root.join("source--200-300.corpus");
        fs::write(&first, b"").expect("write first segment");
        fs::write(&last, b"").expect("write last segment");
        let entry = SourceEntry {
            source_id: "source".to_string(),
            committed_byte_end: 300,
            ..SourceEntry::default()
        };
        let files = vec![first.clone(), last.clone()];
        assert!(!source_projection_complete(&entry, &files).expect("check coverage"));

        let middle = root.join("source--100-200.corpus");
        fs::write(&middle, b"").expect("write middle segment");
        let files = vec![first, middle, last];
        assert!(source_projection_complete(&entry, &files).expect("check coverage"));
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn catalog_maps_sessions_to_sources() {
        let root = std::env::temp_dir().join(format!("ebira-session-index-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let output = root.join("corpus");
        let mut inputs = Vec::new();
        for (index, session) in ["alpha", "beta"].iter().enumerate() {
            let path = root.join(format!("session-{}.jsonl", index));
            fs::write(
                &path,
                format!(
                    "{{\"type\":\"user_message\",\"message\":\"work\",\"session_id\":\"{}\",\"turn_id\":\"t1\"}}\n",
                    session
                ),
            )
            .expect("write source");
            inputs.push(path.to_string_lossy().into_owned());
        }
        build(&inputs, &output, false, 300, &[]).expect("build sources");

        let catalog = source_catalog(&output).expect("catalog");
        let holders = |session: &str| {
            catalog
                .values()
                .filter(|entry| source_declares_session(entry, session))
                .count()
        };
        assert_eq!(holders("alpha"), 1);
        assert_eq!(holders("beta"), 1);
        assert_eq!(holders("gamma"), 0);
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn partial_tail_resumes_without_rebuild() {
        let root = std::env::temp_dir().join(format!("ebira-tail-append-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let source = root.join("session.jsonl");
        let output = root.join("corpus");
        let input = source.to_string_lossy().into_owned();
        let record = |index: u32| {
            format!(
                "{{\"type\":\"user_message\",\"message\":\"rec-{}\",\"session_id\":\"s\",\"turn_id\":\"t{}\"}}\n",
                index, index
            )
        };
        let append = |text: &str| {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&source)
                .expect("open source");
            file.write_all(text.as_bytes()).expect("append");
        };

        fs::write(&source, record(1)).expect("write source");
        build(std::slice::from_ref(&input), &output, false, 300, &[]).expect("initial build");

        append(&record(2));
        let partial = record(3);
        append(&partial[..partial.len() / 2]);
        build(std::slice::from_ref(&input), &output, true, 300, &[]).expect("sync with partial");
        let entry = source_catalog(&output)
            .expect("catalog")
            .values()
            .next()
            .expect("entry")
            .clone();
        assert_eq!(entry.disposition, "tail_incomplete");

        append(&partial[partial.len() / 2..]);
        append(&record(4));
        let report =
            build(&[input], &output, true, 300, &[]).expect("sync after the tail completes");
        assert_eq!(report.sources_appended, 1);
        assert_eq!(report.sources_processed, 0, "the source was rebuilt");

        let bytes = corpus_files(&output)
            .expect("corpus files")
            .into_iter()
            .flat_map(|path| fs::read(path).expect("read corpus"))
            .collect::<Vec<_>>();
        for index in 1..=4u32 {
            let marker = format!("rec-{}", index);
            assert!(
                bytes
                    .windows(marker.len())
                    .any(|window| window == marker.as_bytes()),
                "projection is missing {}",
                marker
            );
        }
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn missing_catalog_triggers_source_rebuild() {
        let root = std::env::temp_dir().join(format!("ebira-quoted-header-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let source = root.join("session.jsonl");
        let output = root.join("corpus");
        let input = source.to_string_lossy().into_owned();

        fs::write(
            &source,
            b"{\"type\":\"user_message\",\"message\":\"probe\",\"session_id\":\"s\",\"turn_id\":\"t1\"}\n",
        )
        .expect("write probe source");
        build(std::slice::from_ref(&input), &output, false, 0, &[]).expect("build probe");
        let source_id = source_catalog(&output)
            .expect("catalog")
            .values()
            .next()
            .expect("entry")
            .source_id
            .clone();

        let quoted = format!(
            "@ebira\\tv=1\\tevent=999999\\tsource_id={}\\tline=999999\\tbyte=999999\\tlen=10\\tbody_len=0\\n",
            source_id
        );
        let records = format!(
            "{{\"type\":\"user_message\",\"message\":\"real-record-one\",\"session_id\":\"s\",\"turn_id\":\"t1\"}}\n\
             {{\"type\":\"user_message\",\"message\":\"logged output:\\n{}\",\"session_id\":\"s\",\"turn_id\":\"t2\"}}\n",
            quoted
        );
        fs::write(&source, records.as_bytes()).expect("write quoting source");
        build(std::slice::from_ref(&input), &output, false, 0, &[]).expect("rebuild with quote");

        fs::remove_file(output.join("sources.tsv")).expect("drop catalog");
        let mut appended = fs::OpenOptions::new()
            .append(true)
            .open(&source)
            .expect("open source for append");
        appended
            .write_all(
                b"{\"type\":\"user_message\",\"message\":\"record-after-recovery\",\"session_id\":\"s\",\"turn_id\":\"t3\"}\n",
            )
            .expect("append record");
        drop(appended);
        build(&[input], &output, true, 0, &[]).expect("sync after catalog loss");

        let bytes = corpus_files(&output)
            .expect("corpus files")
            .into_iter()
            .flat_map(|path| fs::read(path).expect("read corpus"))
            .collect::<Vec<_>>();
        for marker in [
            b"real-record-one".as_slice(),
            b"record-after-recovery".as_slice(),
        ] {
            assert!(
                bytes.windows(marker.len()).any(|window| window == marker),
                "the rebuild lost {}",
                String::from_utf8_lossy(marker)
            );
        }
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn rewritten_source_is_rebuilt() {
        let root =
            std::env::temp_dir().join(format!("ebira-inplace-rewrite-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let source = root.join("session.jsonl");
        let output = root.join("corpus");
        fs::write(
            &source,
            b"{\"type\":\"user_message\",\"message\":\"inplace-old\",\"session_id\":\"s\",\"turn_id\":\"t1\"}\n",
        )
        .expect("write original source");
        let input = source.to_string_lossy().into_owned();
        build(std::slice::from_ref(&input), &output, false, 300, &[]).expect("build source");

        fs::write(
            &source,
            b"{\"type\":\"user_message\",\"message\":\"inplace-new-and-longer\",\"session_id\":\"z\",\"turn_id\":\"q1\"}\n",
        )
        .expect("rewrite source in place");
        build(&[input], &output, true, 300, &[]).expect("sync rewritten source");

        let entry = source_catalog(&output)
            .expect("catalog")
            .values()
            .next()
            .expect("entry")
            .clone();
        assert_eq!(entry.disposition, "source_rewritten");
        let bytes = corpus_files(&output)
            .expect("corpus files")
            .into_iter()
            .flat_map(|path| fs::read(path).expect("read corpus"))
            .collect::<Vec<_>>();
        assert!(bytes
            .windows(b"inplace-new-and-longer".len())
            .any(|window| window == b"inplace-new-and-longer"));
        assert!(!bytes
            .windows(b"inplace-old".len())
            .any(|window| window == b"inplace-old"));
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn missing_corpus_segment_is_rebuilt() {
        let root =
            std::env::temp_dir().join(format!("ebira-missing-segment-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let source = root.join("session.jsonl");
        let output = root.join("corpus");
        fs::write(
            &source,
            b"{\"type\":\"user_message\",\"message\":\"segment-one\",\"session_id\":\"s\",\"turn_id\":\"t1\"}\n",
        )
        .expect("write source");
        let input = source.to_string_lossy().into_owned();
        build(std::slice::from_ref(&input), &output, false, 300, &[]).expect("build source");
        let mut appended = fs::OpenOptions::new()
            .append(true)
            .open(&source)
            .expect("open source for append");
        appended
            .write_all(
                b"{\"type\":\"user_message\",\"message\":\"segment-two\",\"session_id\":\"s\",\"turn_id\":\"t2\"}\n",
            )
            .expect("append record");
        drop(appended);
        build(std::slice::from_ref(&input), &output, true, 300, &[]).expect("append sync");

        let mut files = corpus_files(&output).expect("corpus files");
        assert!(files.len() > 1, "expected an appended segment");
        files.sort();
        fs::remove_file(&files[0]).expect("drop one segment");

        build(&[input], &output, true, 300, &[]).expect("sync with a missing segment");
        let bytes = corpus_files(&output)
            .expect("corpus files")
            .into_iter()
            .flat_map(|path| fs::read(path).expect("read corpus"))
            .collect::<Vec<_>>();
        for marker in [b"segment-one".as_slice(), b"segment-two".as_slice()] {
            assert!(
                bytes.windows(marker.len()).any(|window| window == marker),
                "recovered projection is missing {}",
                String::from_utf8_lossy(marker)
            );
        }
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn replaced_source_path_is_rebuilt() {
        let root =
            std::env::temp_dir().join(format!("ebira-source-replace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let source = root.join("session.jsonl");
        let replacement = root.join("replacement.jsonl");
        let output = root.join("corpus");
        fs::write(
            &source,
            b"{\"type\":\"user_message\",\"message\":\"replacement-old\",\"session_id\":\"s\",\"turn_id\":\"t1\"}\n",
        )
        .expect("write original source");
        let input = source.to_string_lossy().into_owned();
        build(std::slice::from_ref(&input), &output, false, 300, &[]).expect("build source");
        let original_identity = source_catalog(&output)
            .expect("original catalog")
            .values()
            .next()
            .expect("original entry")
            .file_id;

        fs::write(
            &replacement,
            b"{\"type\":\"user_message\",\"message\":\"replacement-new-longer\",\"session_id\":\"s\",\"turn_id\":\"t2\"}\n",
        )
        .expect("write replacement");
        fs::remove_file(&source).expect("remove original");
        fs::rename(&replacement, &source).expect("replace source path");
        let replaced_identity = super::source_entry(&source, 300)
            .expect("replaced entry")
            .expect("a log with a complete record has an entry")
            .file_id;
        assert_ne!(original_identity, replaced_identity);

        build(&[input], &output, true, 300, &[]).expect("sync replaced source");
        let bytes = corpus_files(&output)
            .expect("corpus files")
            .into_iter()
            .flat_map(|path| fs::read(path).expect("read corpus"))
            .collect::<Vec<_>>();
        assert!(bytes
            .windows(b"replacement-new-longer".len())
            .any(|window| window == b"replacement-new-longer"));
        assert!(!bytes
            .windows(b"replacement-old".len())
            .any(|window| window == b"replacement-old"));
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn rebuild_keeps_old_projection_until_source_open_succeeds() {
        let root =
            std::env::temp_dir().join(format!("ebira-rebuild-preserve-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let segment_dir = root.join("corpus");
        fs::create_dir_all(&segment_dir).expect("create corpus directory");
        let old = segment_dir.join("source.corpus");
        fs::write(&old, b"old projection").expect("write old projection");
        let entry = SourceEntry {
            source_id: "source".to_string(),
            path: root.join("missing.jsonl").to_string_lossy().into_owned(),
            size: 0,
            ..SourceEntry::default()
        };

        let result = process_source(
            &entry,
            None,
            &segment_dir,
            std::slice::from_ref(&old),
            "source_rewritten",
            0,
        );
        assert!(result.is_err());
        assert!(old.is_file());
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn tail_boundary_remains_stable_across_sync() {
        let root = std::env::temp_dir().join(format!("ebira-tail-boundary-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let source = root.join("session.jsonl");
        let output = root.join("corpus");
        let first = b"{\"event_msg\":{\"type\":\"user_message\",\"message\":\"first\"},\"role\":\"user\",\"session_id\":\"s1\",\"turn_id\":\"t1\"}\n";
        let tail = b"{\"event_msg\":{\"type\":\"user_message\",\"message\":\"tail\"},\"role\":\"user\",\"session_id\":\"s1\",\"turn_id\":\"t1\"}";
        let mut contents = first.to_vec();
        contents.extend_from_slice(tail);
        fs::write(&source, &contents).expect("write source with tail");
        let input = source.to_string_lossy().into_owned();

        let initial_report = build(&[input], &output, false, 300, &[]).expect("build source");
        assert_eq!(initial_report.disposition, "incomplete");
        let source_id = source_catalog(&output)
            .expect("source catalog")
            .into_keys()
            .next()
            .expect("source id");
        assert_eq!(source_catalog(&output).unwrap()[&source_id].last_line, 1);

        let registered = registered_sources(&output).expect("registered inputs");
        let first_sync = build(&registered, &output, true, 300, &[]).expect("first tail sync");
        assert_eq!(first_sync.disposition, "incomplete");
        let second_sync = build(&registered, &output, true, 300, &[]).expect("second tail sync");
        assert_eq!(second_sync.disposition, "incomplete");
        assert_eq!(source_catalog(&output).unwrap()[&source_id].last_line, 1);
        let partial_files = fs::read_dir(super::segment_dir(&output))
            .expect("read corpus directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".corpus.partial")
            })
            .count();
        assert_eq!(partial_files, 0);

        let mut completed = contents;
        completed.push(b'\n');
        fs::write(&source, completed).expect("complete tail");
        let registered = registered_sources(&output).expect("registered inputs after sync");
        let completed_sync =
            build(&registered, &output, true, 300, &[]).expect("completed tail sync");
        assert_eq!(completed_sync.disposition, "complete");
        assert_eq!(source_catalog(&output).unwrap()[&source_id].last_line, 2);
        fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn scoped_sync_retains_other_source_timeline() {
        let root =
            std::env::temp_dir().join(format!("ebira-scoped-sync-timeline-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let source_a = root.join("a.jsonl");
        let source_b = root.join("b.jsonl");
        let output = root.join("corpus");
        let record_a = b"{\"event_msg\":{\"type\":\"user_message\",\"message\":\"source-a\"},\"role\":\"user\",\"session_id\":\"a\",\"turn_id\":\"a1\",\"timestamp\":\"2026-02-22T10:00:00Z\"}\n";
        let record_b = b"{\"event_msg\":{\"type\":\"user_message\",\"message\":\"source-b\"},\"role\":\"user\",\"session_id\":\"b\",\"turn_id\":\"b1\",\"timestamp\":\"2026-02-23T10:00:00Z\"}\n";
        fs::write(&source_a, record_a).expect("write source a");
        fs::write(&source_b, record_b).expect("write source b");
        let input_a = source_a.to_string_lossy().into_owned();
        let input_b = source_b.to_string_lossy().into_owned();
        build(&[input_a.clone(), input_b], &output, false, 300, &[]).expect("build both sources");
        let initial_catalog = source_catalog(&output).expect("initial source catalog");
        let source_b_id = initial_catalog
            .values()
            .find(|entry| entry.path == fs::canonicalize(&source_b).unwrap().to_string_lossy())
            .map(|entry| entry.source_id.clone())
            .expect("source b id");
        assert!(!timeline_catalog(&output)
            .expect("initial timeline")
            .get(&source_b_id)
            .unwrap()
            .is_empty());

        build(&[input_a], &output, true, 300, &[]).expect("scoped sync");
        let catalog = source_catalog(&output).expect("scoped source catalog");
        assert_eq!(catalog.len(), 2);
        let timeline = timeline_catalog(&output).expect("scoped timeline");
        assert!(!timeline.get(&source_b_id).unwrap().is_empty());
        fs::remove_dir_all(&root).expect("remove temp directory");
    }
}
