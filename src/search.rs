use crate::core::TimelineRef;
use crate::corpus::{self, SourceEntry, TimelineRun};
use crate::format::{self, json_string, parse_event_header, EventHeader};
use crate::time::{self, Range};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Instant;

const SEARCH_BUFFER_SIZE: usize = 1024 * 1024;
const TIMELINE_SNIPPET_BYTES: usize = 240;
const SNIPPET_CONTEXT_BYTES: usize = 120;
const BODY_CHUNK_SIZE: usize = 64 * 1024;
const HISTORY_SAMPLES_PER_DATE: usize = 4;
const HISTORY_NEXT_ACTION_LIMIT: usize = 12;
const HISTORY_MAP_FACET_LIMIT: usize = 12;
const HISTORY_MAP_DATE_LIMIT: usize = 12;
const MAX_TIMELINE_PAGE_EVENTS: usize = 100_000;
const MAX_CONTEXT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct SearchRequest {
    pub query: String,
    pub limit: usize,
    pub offset: u64,
    pub operation: String,
    pub source_id: Option<String>,
    pub session: Option<String>,
    pub role: Option<String>,
    pub kind: Option<String>,
    pub sender: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// `from` and `to` read as one range (`time::Range`).
    pub range: Range,
    pub date_limit: usize,
    pub date_offset: u64,
    pub offset_minutes: i64,
    pub fold_ascii_case: bool,
    pub raw: bool,
    /// `--order desc`: the newest matches first. The default stays chronological.
    pub newest_first: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TimelineRequest {
    pub offset_minutes: i64,
    pub date: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub range: Range,
    pub source_id: Option<String>,
    pub session: Option<String>,
    pub role: Option<String>,
    pub kind: Option<String>,
    pub sender: Option<String>,
    pub limit: usize,
    pub offset: u64,
    pub order: TimelineOrder,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TimelineOrder {
    Asc,
    #[default]
    Desc,
}

#[derive(Default)]
struct SearchStats {
    scanned_bytes: u64,
    scanned_files: u64,
    matched_events: u64,
    returned: Vec<SearchHit>,
    facets: Facets,
    timeline: BTreeMap<String, TimelineBucket>,
    scanned_records: u64,
    unreadable_records: u64,
    unreachable_sources: BTreeSet<String>,
}

#[derive(Default)]
struct SourceReaders {
    open: BTreeMap<String, Option<File>>,
}

impl SourceReaders {
    fn get(
        &mut self,
        source_id: &str,
        catalog: &BTreeMap<String, SourceEntry>,
    ) -> Option<&mut File> {
        self.open
            .entry(source_id.to_string())
            .or_insert_with(|| {
                catalog
                    .get(source_id)
                    .and_then(|entry| File::open(&entry.path).ok())
            })
            .as_mut()
    }
}

#[derive(Default)]
struct Facets {
    source: BTreeMap<String, u64>,
    session: BTreeMap<String, u64>,
    kind: BTreeMap<String, u64>,
    sender: BTreeMap<String, u64>,
    date: BTreeMap<String, u64>,
}

#[derive(Clone, Debug)]
struct SearchHit {
    header: EventHeader,
    source_path: String,
    /// The path of the field whose value holds the query; none for raw records and listings.
    field: Option<String>,
    snippet: String,
    /// The instant of `header.timestamp` at the corpus offset, for ordering.
    instant: Option<i128>,
}

#[derive(Default)]
struct TimelineBucket {
    matched_events: u64,
    source: BTreeMap<String, u64>,
    session: BTreeMap<String, u64>,
    kind: BTreeMap<String, u64>,
    samples: Vec<SearchHit>,
    sample_kinds: BTreeSet<String>,
}

struct ScanWorkspace {
    chunk: Vec<u8>,
    combined: Vec<u8>,
    overlap: Vec<u8>,
    folded: Vec<u8>,
    /// One corpus body, read whole to search its values.
    body: Vec<u8>,
    fold_ascii_case: bool,
}

impl ScanWorkspace {
    fn new(query_len: usize, fold_ascii_case: bool) -> Self {
        let chunk_size = BODY_CHUNK_SIZE.max(query_len);
        Self {
            chunk: vec![0; chunk_size],
            combined: Vec::with_capacity(chunk_size.saturating_add(query_len)),
            overlap: Vec::with_capacity(query_len.saturating_sub(1)),
            folded: Vec::with_capacity(chunk_size.saturating_add(query_len)),
            body: Vec::new(),
            fold_ascii_case,
        }
    }

    fn haystack(&mut self) -> &[u8] {
        if !self.fold_ascii_case {
            return &self.combined;
        }
        self.folded.clear();
        self.folded
            .extend(self.combined.iter().map(u8::to_ascii_lowercase));
        &self.folded
    }
}

pub fn run(root: &Path, request: SearchRequest) -> io::Result<()> {
    if request.limit == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--limit must be at least 1",
        ));
    }
    if request.query.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "query is empty",
        ));
    }
    let started = Instant::now();
    let catalog = corpus::source_catalog(root)?;
    let scope = corpus::scope(
        &catalog,
        request.source_id.as_deref(),
        request.session.as_deref(),
    );
    let files = scope.files(root)?;
    let stats = scan_parallel(&files, &catalog, &request)?;

    let duration_ms = started.elapsed().as_millis() as u64;
    let output = if is_history_request(&request) {
        history_json(root, &request, &scope, &files, &stats, duration_ms)?
    } else {
        search_json(root, &request, &scope, &files, &stats, duration_ms)?
    };
    print!("{}", output);
    Ok(())
}

pub fn timeline(root: &Path, request: TimelineRequest) -> io::Result<()> {
    if request.limit == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--limit must be at least 1",
        ));
    }
    let started = Instant::now();
    let catalog = corpus::source_catalog(root)?;
    if !corpus::timeline_catalog_present(root) {
        println!(
            "{{\"disposition\":\"timeline_unavailable\",\"mode\":\"timeline\",\"reason\":\"timeline.tsv is not present; run an explicit corpus sync to create the disposable map\"}}"
        );
        return Ok(());
    }
    let runs_by_source = corpus::timeline_catalog(root)?;
    if request.date.is_some() {
        return timeline_events(root, &catalog, &runs_by_source, &request, started);
    }
    if request.session.is_some()
        || request.role.is_some()
        || request.kind.is_some()
        || request.sender.is_some()
    {
        println!(
            "{{\"disposition\":\"timeline_filter_unavailable\",\"mode\":\"timeline_map\",\"reason\":\"session, role, kind, and sender filters require an explicit --date so event rows can be verified\"}}"
        );
        return Ok(());
    }
    let scope = corpus::scope(&catalog, request.source_id.as_deref(), None);
    let mut dates = BTreeMap::<String, TimelineMapBucket>::new();
    for (source_id, runs) in &runs_by_source {
        if !scope.contains(source_id) {
            continue;
        }
        for run in runs {
            if !request.range.is_unbounded()
                && (run.date == "undated"
                    || !request
                        .range
                        .touches_date(&run.date, request.offset_minutes))
            {
                continue;
            }
            let bucket = dates.entry(run.date.clone()).or_default();
            bucket.event_count = bucket.event_count.saturating_add(run.bucket.event_count);
            bucket.session_count = bucket
                .session_count
                .saturating_add(run.bucket.session_count);
            bucket.source_ids.insert(source_id.clone());
            bucket.runs = bucket.runs.saturating_add(1);
            for (kind, count) in &run.bucket.kind_counts {
                *bucket.kind_counts.entry(kind.clone()).or_default() = bucket
                    .kind_counts
                    .get(kind)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(*count);
            }
            let source_path = catalog
                .get(source_id)
                .map(|entry| entry.path.clone())
                .unwrap_or_default();
            if let Some(reference) = run.bucket.first.as_ref() {
                choose_timeline_pointer(
                    &mut bucket.first,
                    timeline_hit(source_id, &source_path, reference, request.offset_minutes),
                    false,
                );
            }
            if let Some(reference) = run.bucket.last.as_ref() {
                choose_timeline_pointer(
                    &mut bucket.last,
                    timeline_hit(source_id, &source_path, reference, request.offset_minutes),
                    true,
                );
            }
        }
    }
    print_timeline_map(
        &scope,
        &runs_by_source,
        &dates,
        &request,
        started.elapsed().as_millis() as u64,
    );
    Ok(())
}

#[derive(Default)]
struct TimelineMapBucket {
    event_count: u64,
    session_count: u64,
    source_ids: BTreeSet<String>,
    kind_counts: BTreeMap<String, u64>,
    runs: u64,
    first: Option<SearchHit>,
    last: Option<SearchHit>,
}

fn timeline_date_order(left: &str, right: &str, order: TimelineOrder) -> Ordering {
    match (left == "undated", right == "undated") {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let ordering = left.cmp(right);
            if order == TimelineOrder::Desc {
                ordering.reverse()
            } else {
                ordering
            }
        }
    }
}

fn choose_timeline_pointer(target: &mut Option<SearchHit>, candidate: SearchHit, latest: bool) {
    let replace = target
        .as_ref()
        .map(|current| {
            let ordering = timeline_hit_order(current, &candidate);
            if latest {
                ordering == Ordering::Less
            } else {
                ordering == Ordering::Greater
            }
        })
        .unwrap_or(true);
    if replace {
        *target = Some(candidate);
    }
}

/// Hits in time order: by the instant they name, then by source and position. Records whose
/// timestamp cannot be read come last.
fn timeline_hit_order(left: &SearchHit, right: &SearchHit) -> Ordering {
    instant_order(left, right)
        .then_with(|| left.header.source_id.cmp(&right.header.source_id))
        .then_with(|| left.header.event_index.cmp(&right.header.event_index))
}

fn instant_order(left: &SearchHit, right: &SearchHit) -> Ordering {
    match (left.instant, right.instant) {
        (Some(left_at), Some(right_at)) => left_at
            .cmp(&right_at)
            .then_with(|| left.header.timestamp.cmp(&right.header.timestamp)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => (left.header.timestamp.is_empty(), &left.header.timestamp)
            .cmp(&(right.header.timestamp.is_empty(), &right.header.timestamp)),
    }
}

/// Search pages: chronological by default, newest first with `--order desc`. Records without
/// a timestamp sort last either way.
fn search_hit_order(left: &SearchHit, right: &SearchHit, newest_first: bool) -> Ordering {
    if !newest_first {
        return timeline_hit_order(left, right);
    }
    let readable_first = left.instant.is_none().cmp(&right.instant.is_none());
    readable_first.then_with(|| timeline_hit_order(right, left))
}

fn timeline_order(left: &SearchHit, right: &SearchHit, order: TimelineOrder) -> Ordering {
    let ordering = timeline_hit_order(left, right);
    if order == TimelineOrder::Desc {
        ordering.reverse()
    } else {
        ordering
    }
}

struct TimelinePage {
    hits: Vec<SearchHit>,
    total: u64,
    order: TimelineOrder,
    capacity: usize,
}

impl TimelinePage {
    fn new(request: &TimelineRequest) -> io::Result<Self> {
        let offset = usize::try_from(request.offset).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "timeline offset is too large for this process",
            )
        })?;
        let capacity = offset.checked_add(request.limit).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "timeline page range is too large",
            )
        })?;
        if capacity > MAX_TIMELINE_PAGE_EVENTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "timeline page range exceeds {} events",
                    MAX_TIMELINE_PAGE_EVENTS
                ),
            ));
        }
        Ok(Self {
            hits: Vec::with_capacity(capacity),
            total: 0,
            order: request.order,
            capacity,
        })
    }

    fn push(&mut self, hit: SearchHit) {
        self.total = self.total.saturating_add(1);
        let position = self
            .hits
            .binary_search_by(|current| timeline_order(current, &hit, self.order))
            .unwrap_or_else(|position| position);
        if position >= self.capacity && self.hits.len() >= self.capacity {
            return;
        }
        self.hits.insert(position, hit);
        if self.hits.len() > self.capacity {
            self.hits.pop();
        }
    }
}

fn timeline_hit(
    source_id: &str,
    source_path: &str,
    reference: &TimelineRef,
    offset_minutes: i64,
) -> SearchHit {
    SearchHit {
        instant: time::instant(&reference.timestamp, offset_minutes),
        header: EventHeader {
            event_index: reference.event_index,
            source_id: source_id.to_string(),
            line: reference.line,
            byte_start: reference.byte_start,
            byte_len: reference.byte_len,
            session: reference.session.clone(),
            turn: reference.turn.clone(),
            role: reference.role.clone(),
            kind: reference.kind.clone(),
            timestamp: reference.timestamp.clone(),
            ..EventHeader::default()
        },
        source_path: source_path.to_string(),
        field: None,
        snippet: String::new(),
    }
}

fn print_timeline_map(
    scope: &corpus::Scope,
    runs_by_source: &BTreeMap<String, Vec<TimelineRun>>,
    dates: &BTreeMap<String, TimelineMapBucket>,
    request: &TimelineRequest,
    duration_ms: u64,
) {
    let expected_sources = scope
        .sources
        .iter()
        .filter(|entry| entry.event_count > 0)
        .count();
    let covered_sources = scope
        .sources
        .iter()
        .filter(|entry| entry.event_count > 0 && runs_by_source.contains_key(&entry.source_id))
        .count();
    let incomplete_sources = scope
        .sources
        .iter()
        .filter(|entry| !corpus::source_is_complete(entry))
        .count();
    let staleness = corpus::Staleness::of(scope.sources.iter().copied());
    let missing_sources = expected_sources.saturating_sub(covered_sources);
    let disposition = if dates.is_empty() && !scope.sources.is_empty() {
        "timeline_map_empty"
    } else if missing_sources > 0 || incomplete_sources > 0 {
        "timeline_map_partial"
    } else {
        "timeline_map_ready"
    };
    let page_start = usize::try_from(request.offset)
        .unwrap_or(usize::MAX)
        .min(dates.len());
    let limit = request.limit;
    let mut date_entries = dates.iter().collect::<Vec<_>>();
    date_entries.sort_by(|(left, _), (right, _)| timeline_date_order(left, right, request.order));
    let selected = date_entries
        .into_iter()
        .skip(page_start)
        .take(limit)
        .collect::<Vec<_>>();
    let page_end = request.offset.saturating_add(selected.len() as u64);
    let has_more = page_end < dates.len() as u64;
    let mut output = String::new();
    output.push('{');
    field(&mut output, "disposition", disposition, true);
    field(&mut output, "mode", "timeline_map", false);
    field(
        &mut output,
        "order",
        match request.order {
            TimelineOrder::Asc => "asc",
            TimelineOrder::Desc => "desc",
        },
        false,
    );
    field(
        &mut output,
        "coverage",
        if missing_sources == 0 && incomplete_sources == 0 {
            "complete"
        } else {
            "partial"
        },
        false,
    );
    number(&mut output, "source_count", expected_sources as u64);
    number(&mut output, "covered_sources", covered_sources as u64);
    number(&mut output, "missing_sources", missing_sources as u64);
    number(&mut output, "incomplete_sources", incomplete_sources as u64);
    number(&mut output, "stale_sources", staleness.stale_sources);
    number(
        &mut output,
        "unscanned_source_bytes",
        staleness.unscanned_source_bytes,
    );
    number(&mut output, "date_count", dates.len() as u64);
    number(&mut output, "returned_dates", selected.len() as u64);
    number(&mut output, "offset", request.offset);
    number(&mut output, "duration_ms", duration_ms);
    boolean(&mut output, "truncated", has_more);
    if has_more {
        number(&mut output, "next_offset", page_end);
    }
    output.push_str(",\"dates\":[");
    for (index, (date, bucket)) in selected.into_iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push('{');
        field(&mut output, "date", date, true);
        number(&mut output, "event_count", bucket.event_count);
        number(&mut output, "session_run_occurrences", bucket.session_count);
        number(&mut output, "source_count", bucket.source_ids.len() as u64);
        number(&mut output, "runs", bucket.runs);
        output.push_str(",\"kind_counts\":");
        output.push_str(&map_json(&bucket.kind_counts));
        output.push_str(",\"first\":");
        output.push_str(
            &bucket
                .first
                .as_ref()
                .map(timeline_map_hit_json)
                .unwrap_or_else(|| "null".to_string()),
        );
        output.push_str(",\"last\":");
        output.push_str(
            &bucket
                .last
                .as_ref()
                .map(timeline_map_hit_json)
                .unwrap_or_else(|| "null".to_string()),
        );
        output.push('}');
    }
    output.push_str("],\"next_actions\":[");
    let mut action_dates = dates.keys().collect::<Vec<_>>();
    action_dates.sort_by(|left, right| timeline_date_order(left, right, request.order));
    for (index, date) in action_dates
        .into_iter()
        .skip(page_start)
        .take(limit.min(12))
        .enumerate()
    {
        if index != 0 {
            output.push(',');
        }
        output.push_str(&format!(
            "{{\"action\":\"list_date_events\",\"date\":{},\"limit\":{},\"request\":{}}}",
            json_string(date),
            limit,
            timeline_request_json(request, Some(date), 0),
        ));
    }
    output.push_str("]}\n");
    print!("{}", output);
}

fn timeline_events(
    root: &Path,
    catalog: &BTreeMap<String, SourceEntry>,
    runs_by_source: &BTreeMap<String, Vec<TimelineRun>>,
    request: &TimelineRequest,
    started: Instant,
) -> io::Result<()> {
    let date = request.date.as_deref().unwrap_or("undated");
    let scope = corpus::scope(
        catalog,
        request.source_id.as_deref(),
        request.session.as_deref(),
    );
    let mut runs = Vec::new();
    for (source_id, source_runs) in runs_by_source {
        if !scope.contains(source_id) {
            continue;
        }
        for run in source_runs {
            if run.date == date {
                runs.push(run);
            }
        }
    }
    let mut page = TimelinePage::new(request)?;
    let mut scanned_bytes = 0u64;
    let mut scanned_runs = 0u64;
    for run in &runs {
        let path = corpus::timeline_corpus_file(root, &run.corpus_file)?;
        let source_path = catalog
            .get(&run.source_id)
            .map(|entry| entry.path.clone())
            .unwrap_or_default();
        scanned_bytes =
            scanned_bytes.saturating_add(run.corpus_end.saturating_sub(run.corpus_start));
        scanned_runs = scanned_runs.saturating_add(1);
        scan_timeline_run(&path, run, &source_path, request, &mut page)?;
    }
    let total = page.total;
    let start = usize::try_from(request.offset)
        .unwrap_or(usize::MAX)
        .min(page.hits.len());
    let page_hits = page
        .hits
        .into_iter()
        .skip(start)
        .take(request.limit.max(1))
        .collect::<Vec<_>>();
    let page_end = request.offset.saturating_add(page_hits.len() as u64);
    let has_more = page_end < total;
    let incomplete = runs.iter().any(|run| {
        catalog
            .get(&run.source_id)
            .map(|entry| !corpus::source_is_complete(entry))
            .unwrap_or(true)
    });
    let disposition = if incomplete {
        "timeline_events_partial"
    } else if total == 0 {
        "no_events_for_date"
    } else {
        "timeline_events_ready"
    };
    let staleness = corpus::Staleness::of(scope.sources.iter().copied());
    let mut output = String::new();
    output.push('{');
    field(&mut output, "disposition", disposition, true);
    field(&mut output, "mode", "timeline_events", false);
    field(&mut output, "date", date, false);
    number(&mut output, "scanned_runs", scanned_runs);
    number(&mut output, "scanned_bytes", scanned_bytes);
    number(&mut output, "stale_sources", staleness.stale_sources);
    number(
        &mut output,
        "unscanned_source_bytes",
        staleness.unscanned_source_bytes,
    );
    number(&mut output, "total_events", total);
    number(&mut output, "returned", page_hits.len() as u64);
    number(&mut output, "offset", request.offset);
    number(
        &mut output,
        "duration_ms",
        started.elapsed().as_millis() as u64,
    );
    boolean(&mut output, "truncated", has_more);
    if has_more {
        number(&mut output, "next_offset", page_end);
    }
    output.push_str(",\"events\":[");
    hits_json(&mut output, &page_hits);
    output.push_str("]}\n");
    print!("{}", output);
    Ok(())
}

fn scan_timeline_run(
    path: &Path,
    run: &TimelineRun,
    source_path: &str,
    request: &TimelineRequest,
    page: &mut TimelinePage,
) -> io::Result<()> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(SEARCH_BUFFER_SIZE, file);
    reader.seek(SeekFrom::Start(run.corpus_start))?;
    let mut current_offset = run.corpus_start;
    let mut header_line = Vec::new();
    while current_offset < run.corpus_end {
        header_line.clear();
        let read = reader.read_until(b'\n', &mut header_line)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{}: timeline run ended before header", path.display()),
            ));
        }
        current_offset = current_offset.saturating_add(read as u64);
        let header = parse_event_header_checked(path, &header_line)?;
        let body_len = header.body_len;
        let mut body = Vec::new();
        read_whole(&mut reader, body_len, &mut body)?;
        let head = values_head(&body, TIMELINE_SNIPPET_BYTES);
        current_offset = current_offset.saturating_add(body_len);
        let mut separator = [0u8; 1];
        reader.read_exact(&mut separator)?;
        if separator[0] != b'\n' {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: missing event separator", path.display()),
            ));
        }
        current_offset = current_offset.saturating_add(1);
        if time::date_bucket(&header.timestamp, request.offset_minutes) != run.date
            || request
                .session
                .as_deref()
                .map(|session| header.session != session)
                .unwrap_or(false)
            || request
                .role
                .as_deref()
                .map(|role| header.role != role)
                .unwrap_or(false)
            || request
                .kind
                .as_deref()
                .map(|kind| header.kind != kind)
                .unwrap_or(false)
            || request
                .sender
                .as_deref()
                .map(|sender| header.sender != sender)
                .unwrap_or(false)
            || !request
                .range
                .contains(&header.timestamp, request.offset_minutes)
        {
            continue;
        }
        page.push(SearchHit {
            instant: time::instant(&header.timestamp, request.offset_minutes),
            header,
            source_path: source_path.to_string(),
            field: None,
            snippet: head,
        });
    }
    if current_offset != run.corpus_end {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: timeline run offset mismatch", path.display()),
        ));
    }
    Ok(())
}

fn query_bytes(request: &SearchRequest) -> Vec<u8> {
    if request.fold_ascii_case {
        request.query.to_ascii_lowercase().into_bytes()
    } else {
        request.query.as_bytes().to_vec()
    }
}

fn scan_sequential(
    files: &[std::path::PathBuf],
    catalog: &BTreeMap<String, SourceEntry>,
    request: &SearchRequest,
) -> io::Result<SearchStats> {
    let mut stats = SearchStats::default();
    let mut sources = SourceReaders::default();
    let query = query_bytes(request);
    for path in files {
        scan_file(path, catalog, request, &query, &mut stats, &mut sources)?;
    }
    stats.returned = search_page(stats.returned, request);
    Ok(stats)
}

fn scan_parallel(
    files: &[std::path::PathBuf],
    catalog: &BTreeMap<String, SourceEntry>,
    request: &SearchRequest,
) -> io::Result<SearchStats> {
    if files.len() < 2 || request.offset > 4096 {
        return scan_sequential(files, catalog, request);
    }

    let worker_limit = request
        .limit
        .saturating_add(usize::try_from(request.offset).unwrap_or(usize::MAX));
    let worker_limit = worker_limit.max(1);
    let worker_count = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .min(8)
        .min(files.len());
    let chunk_size = files.len().div_ceil(worker_count);
    let catalog = Arc::new(catalog.clone());
    let query = Arc::new(query_bytes(request));
    let mut worker_request = request.clone();
    worker_request.offset = 0;
    worker_request.limit = worker_limit;
    let worker_request = Arc::new(worker_request);
    let (sender, receiver) = mpsc::channel();
    let mut handles = Vec::new();

    for (chunk_index, chunk) in files.chunks(chunk_size).enumerate() {
        let chunk = chunk.to_vec();
        let catalog = Arc::clone(&catalog);
        let query = Arc::clone(&query);
        let worker_request = Arc::clone(&worker_request);
        let sender = sender.clone();
        handles.push(thread::spawn(move || {
            let mut stats = SearchStats::default();
            let mut sources = SourceReaders::default();
            let result = chunk.iter().try_for_each(|path| {
                scan_file(
                    path,
                    &catalog,
                    &worker_request,
                    &query,
                    &mut stats,
                    &mut sources,
                )
            });
            let _ = sender.send((chunk_index, result.map(|_| stats)));
        }));
    }
    drop(sender);

    let mut parts = (0..handles.len())
        .map(|_| None)
        .collect::<Vec<Option<SearchStats>>>();
    for (chunk_index, result) in receiver {
        parts[chunk_index] = Some(result?);
    }
    for handle in handles {
        if handle.join().is_err() {
            return Err(io::Error::other("history worker thread panicked"));
        }
    }

    let mut merged = SearchStats::default();
    let mut candidate_hits = Vec::new();
    for part in parts.into_iter().flatten() {
        candidate_hits.extend(part.returned.iter().cloned());
        merge_stats(&mut merged, part);
    }
    merged.returned = search_page(candidate_hits, request);
    Ok(merged)
}

fn search_page(mut candidates: Vec<SearchHit>, request: &SearchRequest) -> Vec<SearchHit> {
    candidates.sort_by(|left, right| search_hit_order(left, right, request.newest_first));
    let start = usize::try_from(request.offset)
        .unwrap_or(usize::MAX)
        .min(candidates.len());
    candidates
        .into_iter()
        .skip(start)
        .take(request.limit)
        .collect()
}

fn candidate_limit(request: &SearchRequest) -> usize {
    usize::try_from(request.offset)
        .unwrap_or(usize::MAX)
        .saturating_add(request.limit)
        .max(1)
}

fn add_search_candidate(stats: &mut SearchStats, hit: SearchHit, limit: usize, newest_first: bool) {
    let position = stats
        .returned
        .binary_search_by(|current| search_hit_order(current, &hit, newest_first))
        .unwrap_or_else(|position| position);
    if position >= limit && stats.returned.len() >= limit {
        return;
    }
    stats.returned.insert(position, hit);
    if stats.returned.len() > limit {
        stats.returned.pop();
    }
}

fn merge_stats(target: &mut SearchStats, source: SearchStats) {
    target.scanned_bytes = target.scanned_bytes.saturating_add(source.scanned_bytes);
    target.scanned_files = target.scanned_files.saturating_add(source.scanned_files);
    target.matched_events = target.matched_events.saturating_add(source.matched_events);
    target.scanned_records = target
        .scanned_records
        .saturating_add(source.scanned_records);
    target.unreadable_records = target
        .unreadable_records
        .saturating_add(source.unreadable_records);
    target
        .unreachable_sources
        .extend(source.unreachable_sources);
    merge_counts(&mut target.facets.source, source.facets.source);
    merge_counts(&mut target.facets.session, source.facets.session);
    merge_counts(&mut target.facets.kind, source.facets.kind);
    merge_counts(&mut target.facets.sender, source.facets.sender);
    merge_counts(&mut target.facets.date, source.facets.date);
    for (date, source_bucket) in source.timeline {
        let target_bucket = target.timeline.entry(date).or_default();
        target_bucket.matched_events = target_bucket
            .matched_events
            .saturating_add(source_bucket.matched_events);
        merge_counts(&mut target_bucket.source, source_bucket.source);
        merge_counts(&mut target_bucket.session, source_bucket.session);
        merge_counts(&mut target_bucket.kind, source_bucket.kind);
        for sample in source_bucket.samples {
            let new_kind = target_bucket
                .sample_kinds
                .insert(sample.header.kind.clone());
            if target_bucket.samples.len() < HISTORY_SAMPLES_PER_DATE
                || (new_kind && target_bucket.samples.len() < HISTORY_SAMPLES_PER_DATE * 2)
            {
                target_bucket.samples.push(sample);
            }
        }
    }
}

fn merge_counts(target: &mut BTreeMap<String, u64>, source: BTreeMap<String, u64>) {
    for (key, value) in source {
        *target.entry(key).or_default() =
            target.get(&key).copied().unwrap_or(0).saturating_add(value);
    }
}

fn is_history_request(request: &SearchRequest) -> bool {
    request.operation == "history"
}

fn search_json(
    root: &Path,
    request: &SearchRequest,
    scope: &corpus::Scope,
    files: &[std::path::PathBuf],
    stats: &SearchStats,
    duration_ms: u64,
) -> io::Result<String> {
    let page_end = request.offset.saturating_add(stats.returned.len() as u64);
    let has_more = stats.matched_events > page_end;
    let coverage = coverage_stats(root, scope, files, request, stats)?;
    let disposition = if stats.matched_events == 0 {
        coverage.empty_disposition()
    } else if stats.returned.is_empty() && request.offset > 0 {
        "result_page_empty"
    } else if has_more {
        "result_page_truncated"
    } else {
        "ready"
    };
    let mut output = String::new();
    output.push('{');
    field(&mut output, "disposition", disposition, true);
    field(&mut output, "mode", "literal_scan", false);
    field(
        &mut output,
        "operation",
        if request.operation.is_empty() {
            "search"
        } else {
            request.operation.as_str()
        },
        false,
    );
    field(&mut output, "search_scope", search_scope(request), false);
    field(&mut output, "order", match_order(request), false);
    field(&mut output, "query", &request.query, false);
    field(&mut output, "normalization", normalization(request), false);
    number(&mut output, "scanned_files", stats.scanned_files);
    number(&mut output, "scanned_bytes", stats.scanned_bytes);
    number(&mut output, "total_candidates", stats.matched_events);
    number(&mut output, "returned", stats.returned.len() as u64);
    number(&mut output, "duration_ms", duration_ms);
    number(&mut output, "offset", request.offset);
    coverage.write_fields(&mut output);
    output.push_str(",\"applied_filters\":");
    output.push_str(&applied_filters_json(request));
    boolean(&mut output, "truncated", has_more);
    if has_more {
        number(&mut output, "next_offset", page_end);
    }
    output.push_str(",\"facets\":");
    output.push_str(&facets_json(&stats.facets));
    output.push_str(",\"matches\":[");
    hits_json(&mut output, &stats.returned);
    output.push_str("],\"next_actions\":");
    output.push_str(&search_next_actions(
        root,
        request,
        page_end,
        has_more,
        &coverage,
        stats.matched_events,
    ));
    output.push_str("}\n");
    Ok(output)
}

fn history_json(
    root: &Path,
    request: &SearchRequest,
    scope: &corpus::Scope,
    files: &[std::path::PathBuf],
    stats: &SearchStats,
    duration_ms: u64,
) -> io::Result<String> {
    let coverage = coverage_stats(root, scope, files, request, stats)?;
    let page_end = request.offset.saturating_add(stats.returned.len() as u64);
    let has_more = stats.matched_events > page_end;
    let disposition = if stats.matched_events == 0 {
        coverage.empty_disposition()
    } else if coverage.incomplete_sources == 0 && coverage.unavailable_source_paths == 0 {
        "history_map_ready"
    } else {
        "history_map_partial"
    };
    let operation = if request.operation.is_empty() {
        "history"
    } else {
        request.operation.as_str()
    };
    let mut output = String::new();
    output.push('{');
    field(&mut output, "disposition", disposition, true);
    field(&mut output, "mode", "literal_history_map", false);
    field(&mut output, "operation", operation, false);
    field(&mut output, "search_scope", search_scope(request), false);
    field(&mut output, "match_order", match_order(request), false);
    field(&mut output, "date_order", "desc", false);
    field(&mut output, "query", &request.query, false);
    field(&mut output, "normalization", normalization(request), false);
    coverage.write_fields(&mut output);
    output.push_str(",\"applied_filters\":");
    output.push_str(&applied_filters_json(request));
    number(&mut output, "scanned_files", stats.scanned_files);
    number(&mut output, "scanned_bytes", stats.scanned_bytes);
    number(&mut output, "total_candidates", stats.matched_events);
    number(&mut output, "returned", stats.returned.len() as u64);
    number(&mut output, "offset", request.offset);
    number(&mut output, "duration_ms", duration_ms);
    boolean(&mut output, "truncated", has_more);
    if has_more {
        number(&mut output, "next_offset", page_end);
    }
    let date_limit = if request.date_limit == 0 {
        HISTORY_MAP_DATE_LIMIT
    } else {
        request.date_limit.max(1)
    };
    output.push_str(",\"map\":");
    output.push_str(&history_map_json(stats, request.date_offset, date_limit));
    output.push_str(",\"facets\":");
    output.push_str(&facets_json(&stats.facets));
    output.push_str(",\"matches\":[");
    hits_json(&mut output, &stats.returned);
    output.push_str("],\"next_actions\":");
    output.push_str(&history_next_actions(
        root,
        request,
        page_end,
        has_more,
        DatePage {
            timeline: &stats.timeline,
            offset: request.date_offset,
            limit: date_limit,
        },
        &coverage,
        stats.matched_events,
    ));
    output.push_str("}\n");
    Ok(output)
}

fn history_map_json(stats: &SearchStats, date_offset: u64, date_limit: usize) -> String {
    let mut dates = stats.timeline.iter().collect::<Vec<_>>();
    dates.sort_by(|(left, _), (right, _)| history_date_order(left, right));
    let start = usize::try_from(date_offset)
        .unwrap_or(usize::MAX)
        .min(dates.len());
    let selected = dates
        .into_iter()
        .skip(start)
        .take(date_limit)
        .collect::<Vec<_>>();
    let page_end = date_offset.saturating_add(selected.len() as u64);
    let truncated = page_end < stats.timeline.len() as u64;
    let mut output = format!("{{\"source_count\":{}", stats.facets.source.len());
    number(
        &mut output,
        "session_count",
        stats.facets.session.len() as u64,
    );
    number(&mut output, "date_count", stats.timeline.len() as u64);
    number(&mut output, "returned_dates", selected.len() as u64);
    number(&mut output, "date_offset", date_offset);
    number(&mut output, "date_limit", date_limit as u64);
    boolean(&mut output, "truncated", truncated);
    if truncated {
        number(&mut output, "next_date_offset", page_end);
    }
    number(&mut output, "kind_count", stats.facets.kind.len() as u64);
    output.push_str(",\"timeline\":[");
    for (index, (date, bucket)) in selected.into_iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push('{');
        field(&mut output, "date", date, true);
        number(&mut output, "matched_events", bucket.matched_events);
        number(&mut output, "distinct_sources", bucket.source.len() as u64);
        number(
            &mut output,
            "distinct_sessions",
            bucket.session.len() as u64,
        );
        output.push_str(",\"source_counts\":");
        output.push_str(&map_json_limited(&bucket.source, HISTORY_MAP_FACET_LIMIT));
        output.push_str(",\"session_counts\":");
        output.push_str(&map_json_limited(&bucket.session, HISTORY_MAP_FACET_LIMIT));
        output.push_str(",\"kinds\":");
        output.push_str(&map_json_limited(&bucket.kind, HISTORY_MAP_FACET_LIMIT));
        output.push('}');
    }
    output.push_str("]}");
    output
}

fn history_date_order(left: &str, right: &str) -> Ordering {
    match (left == "undated", right == "undated") {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => right.cmp(left),
    }
}

struct DatePage<'a> {
    timeline: &'a BTreeMap<String, TimelineBucket>,
    offset: u64,
    limit: usize,
}

fn history_next_actions(
    root: &Path,
    request: &SearchRequest,
    next_offset: u64,
    has_more: bool,
    dates: DatePage<'_>,
    coverage: &Coverage,
    matched_events: u64,
) -> String {
    let DatePage {
        timeline,
        offset: date_offset,
        limit: date_limit,
    } = dates;
    let map_has_more = date_offset.saturating_add(date_limit as u64) < timeline.len() as u64;
    let mut output = String::from("[");
    let mut action_count = 0usize;
    let mut buckets = timeline.iter().collect::<Vec<_>>();
    buckets.sort_by(|(left_date, left), (right_date, right)| {
        right
            .matched_events
            .cmp(&left.matched_events)
            .then_with(|| left_date.cmp(right_date))
    });
    for (date, bucket) in buckets.into_iter().take(HISTORY_NEXT_ACTION_LIMIT) {
        if date == "undated" {
            continue;
        }
        if action_count != 0 {
            output.push(',');
        }
        output.push('{');
        field(&mut output, "action", "expand_time_bucket", true);
        field(&mut output, "query", &request.query, false);
        field(&mut output, "from", date, false);
        field(&mut output, "to", date, false);
        number(&mut output, "limit", request.limit as u64);
        output.push_str(",\"request\":");
        output.push_str(&search_request_json(request, 0, Some((date, date)), 0));
        number(&mut output, "matched_events", bucket.matched_events);
        output.push('}');
        action_count += 1;
    }
    if has_more {
        if action_count != 0 {
            output.push(',');
        }
        output.push('{');
        field(&mut output, "action", "expand_literal_page", true);
        field(&mut output, "query", &request.query, false);
        number(&mut output, "offset", next_offset);
        number(&mut output, "limit", request.limit as u64);
        output.push_str(",\"request\":");
        output.push_str(&search_request_json(
            request,
            next_offset,
            None,
            request.date_offset,
        ));
        output.push('}');
    }
    if map_has_more {
        if action_count != 0 {
            output.push(',');
        }
        output.push('{');
        field(&mut output, "action", "expand_history_date_page", true);
        output.push_str(",\"request\":");
        output.push_str(&search_request_json(
            request,
            request.offset,
            None,
            date_offset.saturating_add(date_limit as u64),
        ));
        number(
            &mut output,
            "date_offset",
            date_offset.saturating_add(date_limit as u64),
        );
        number(&mut output, "date_limit", date_limit as u64);
        output.push('}');
    }
    for action in recovery_actions(root, request, coverage, matched_events) {
        if action_count != 0 {
            output.push(',');
        }
        output.push_str(&action);
        action_count += 1;
    }
    let _ = action_count;
    output.push(']');
    output
}

fn applied_filters_json(request: &SearchRequest) -> String {
    let mut output = String::from("{");
    optional_field_first(&mut output, "session", request.session.as_deref());
    optional_field(&mut output, "source_id", request.source_id.as_deref());
    optional_field(&mut output, "role", request.role.as_deref());
    optional_field(&mut output, "kind", request.kind.as_deref());
    optional_field(&mut output, "sender", request.sender.as_deref());
    optional_field(&mut output, "from", request.from.as_deref());
    optional_field(&mut output, "to", request.to.as_deref());
    output.push('}');
    output
}

fn match_order(request: &SearchRequest) -> &'static str {
    if request.newest_first {
        "reverse_chronological"
    } else {
        "chronological"
    }
}

fn normalization(request: &SearchRequest) -> &'static str {
    if request.fold_ascii_case {
        "ascii_case_folded"
    } else {
        "none"
    }
}

fn search_scope(request: &SearchRequest) -> &'static str {
    if request.raw {
        "source_records"
    } else {
        "compact_corpus_body"
    }
}

fn recovery_actions(
    root: &Path,
    request: &SearchRequest,
    coverage: &Coverage,
    matched_events: u64,
) -> Vec<String> {
    let mut actions = Vec::new();
    if coverage.stale_sources > 0 {
        actions.push(format!(
            "{{\"action\":\"sync\",\"request\":{{\"corpus\":{}}},\"unscanned_source_bytes\":{}}}",
            json_string(&root.to_string_lossy()),
            coverage.unscanned_source_bytes,
        ));
    }
    if matched_events == 0 && !request.raw {
        let mut raw_request = request.clone();
        raw_request.raw = true;
        raw_request.offset = 0;
        actions.push(format!(
            "{{\"action\":\"scan_source_records\",\"request\":{}}}",
            search_request_json(&raw_request, 0, None, request.date_offset),
        ));
    }
    actions
}

fn search_next_actions(
    root: &Path,
    request: &SearchRequest,
    next_offset: u64,
    has_more: bool,
    coverage: &Coverage,
    matched_events: u64,
) -> String {
    let mut actions = Vec::new();
    if has_more {
        actions.push(format!(
            "{{\"action\":\"expand_literal_page\",\"request\":{}}}",
            search_request_json(request, next_offset, None, request.date_offset),
        ));
    }
    actions.extend(recovery_actions(root, request, coverage, matched_events));
    format!("[{}]", actions.join(","))
}

fn search_request_json(
    request: &SearchRequest,
    offset: u64,
    range_override: Option<(&str, &str)>,
    date_offset: u64,
) -> String {
    let (from, to) = range_override
        .map(|(from, to)| (Some(from), Some(to)))
        .unwrap_or((request.from.as_deref(), request.to.as_deref()));
    let mut output = String::from("{");
    field(&mut output, "query", &request.query, true);
    number(&mut output, "limit", request.limit as u64);
    number(&mut output, "offset", offset);
    field(&mut output, "operation", &request.operation, false);
    optional_field(&mut output, "source_id", request.source_id.as_deref());
    optional_field(&mut output, "session", request.session.as_deref());
    optional_field(&mut output, "role", request.role.as_deref());
    optional_field(&mut output, "kind", request.kind.as_deref());
    optional_field(&mut output, "sender", request.sender.as_deref());
    optional_field(&mut output, "from", from);
    optional_field(&mut output, "to", to);
    number(&mut output, "date_limit", request.date_limit as u64);
    number(&mut output, "date_offset", date_offset);
    boolean(&mut output, "raw", request.raw);
    boolean(&mut output, "ignore_case", request.fold_ascii_case);
    field(
        &mut output,
        "order",
        if request.newest_first { "desc" } else { "asc" },
        false,
    );
    output.push('}');
    output
}

fn timeline_request_json(request: &TimelineRequest, date: Option<&str>, offset: u64) -> String {
    let mut output = String::from("{");
    optional_field_first(&mut output, "date", date.or(request.date.as_deref()));
    optional_field(&mut output, "from", request.from.as_deref());
    optional_field(&mut output, "to", request.to.as_deref());
    optional_field(&mut output, "source_id", request.source_id.as_deref());
    optional_field(&mut output, "session", request.session.as_deref());
    optional_field(&mut output, "role", request.role.as_deref());
    optional_field(&mut output, "kind", request.kind.as_deref());
    optional_field(&mut output, "sender", request.sender.as_deref());
    number(&mut output, "limit", request.limit as u64);
    number(&mut output, "offset", offset);
    field(
        &mut output,
        "order",
        match request.order {
            TimelineOrder::Asc => "asc",
            TimelineOrder::Desc => "desc",
        },
        false,
    );
    output.push('}');
    output
}

fn hits_json(output: &mut String, hits: &[SearchHit]) {
    for (index, hit) in hits.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str(&hit_json(hit));
    }
}

fn neighbouring_span(
    root: &Path,
    source_id: &str,
    byte_start: u64,
    byte_len: u64,
    before: usize,
    after: usize,
) -> io::Result<(u64, u64, u64)> {
    let mut preceding: std::collections::VecDeque<(u64, u64)> = Default::default();
    let mut anchor_seen = false;
    let mut following = Vec::new();
    let mut included = 0u64;
    for path in corpus::source_files(root, source_id)? {
        let file = File::open(&path)?;
        let mut reader = BufReader::with_capacity(SEARCH_BUFFER_SIZE, file);
        let mut header_line = Vec::new();
        loop {
            header_line.clear();
            if reader.read_until(b'\n', &mut header_line)? == 0 {
                break;
            }
            let header = parse_event_header_checked(&path, &header_line)?;
            skip_body(&mut reader, header.body_len)?;
            let mut separator = [0u8; 1];
            reader.read_exact(&mut separator)?;
            if header.source_id != source_id {
                continue;
            }
            let bounds = (header.byte_start, header.byte_len);
            if anchor_seen {
                if following.len() < after {
                    following.push(bounds);
                }
            } else if header.byte_start == byte_start {
                anchor_seen = true;
            } else if before > 0 {
                if preceding.len() == before {
                    preceding.pop_front();
                }
                preceding.push_back(bounds);
            }
        }
        if anchor_seen && following.len() >= after {
            break;
        }
    }
    let mut start = byte_start;
    let mut end = byte_start.saturating_add(byte_len);
    if anchor_seen {
        included += 1;
        for (position, length) in preceding.iter().chain(following.iter()) {
            included += 1;
            start = start.min(*position);
            end = end.max(position.saturating_add(*length));
        }
    }
    Ok((start, end.saturating_sub(start), included))
}

pub fn context(
    root: &Path,
    source_id: &str,
    byte_start: u64,
    byte_len: u64,
    before: usize,
    after: usize,
) -> io::Result<()> {
    let catalog = corpus::source_catalog(root)?;
    let entry = catalog.get(source_id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("source_id not found in catalog: {}", source_id),
        )
    })?;
    let (byte_start, byte_len, neighbours) = if before == 0 && after == 0 {
        (byte_start, byte_len, 1)
    } else {
        neighbouring_span(root, source_id, byte_start, byte_len, before, after)?
    };
    let mut file = File::open(&entry.path)?;
    let source_size = file.metadata()?.len();
    let byte_end = byte_start.checked_add(byte_len).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "source context range overflows",
        )
    })?;
    if byte_end > source_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "source context range ends at {} but source size is {}",
                byte_end, source_size
            ),
        ));
    }
    if byte_len > MAX_CONTEXT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "source context range exceeds {} bytes; request smaller chunks",
                MAX_CONTEXT_BYTES
            ),
        ));
    }
    file.seek(SeekFrom::Start(byte_start))?;
    let length = usize::try_from(byte_len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "source byte length is too large for this process",
        )
    })?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    let raw = String::from_utf8_lossy(&bytes);

    let observed_modified_ms = file
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0);
    let closes_on_record = bytes.last() == Some(&b'\n');
    let source_moved = source_size < entry.size
        || (source_size == entry.size && observed_modified_ms != entry.modified_ms);
    let disposition = if source_moved {
        "source_changed_since_projection"
    } else {
        "ready"
    };
    let next_actions = if disposition == "ready" {
        String::from("[]")
    } else {
        format!(
            "[{{\"action\":\"sync\",\"request\":{{\"corpus\":{}}}}}]",
            json_string(&root.to_string_lossy()),
        )
    };
    println!(
        "{{\"disposition\":{},\"mode\":\"source_context\",\"source_ref\":{{\"source_id\":{},\"source_path\":{},\"byte_start\":{},\"byte_len\":{}}},\"records\":{},\"closes_on_record\":{},\"source_recorded\":{{\"size\":{},\"modified_ms\":{}}},\"source_observed\":{{\"size\":{},\"modified_ms\":{}}},\"raw\":{},\"next_actions\":{}}}",
        json_string(disposition),
        json_string(source_id),
        json_string(&entry.path),
        byte_start,
        byte_len,
        neighbours,
        closes_on_record,
        entry.size,
        entry.modified_ms,
        source_size,
        observed_modified_ms,
        json_string(&raw),
        next_actions,
    );
    Ok(())
}

fn scan_file(
    path: &Path,
    catalog: &BTreeMap<String, SourceEntry>,
    request: &SearchRequest,
    query: &[u8],
    stats: &mut SearchStats,
    sources: &mut SourceReaders,
) -> io::Result<()> {
    let metadata = path.metadata()?;
    stats.scanned_files += 1;
    stats.scanned_bytes = stats.scanned_bytes.saturating_add(metadata.len());
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(SEARCH_BUFFER_SIZE, file);
    let mut header_line = Vec::new();
    let mut workspace = ScanWorkspace::new(query.len(), request.fold_ascii_case);
    let has_filters = request_has_filters(request) || request.raw;
    loop {
        header_line.clear();
        let read = reader.read_until(b'\n', &mut header_line)?;
        if read == 0 {
            break;
        }
        let parsed_header = if has_filters {
            Some(parse_event_header_checked(path, &header_line)?)
        } else {
            None
        };
        let body_len = match parsed_header.as_ref() {
            Some(header) => header.body_len,
            None => parse_body_len(&header_line).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {}", path.display(), error),
                )
            })?,
        };
        let eligible = parsed_header
            .as_ref()
            .map(|header| matches_filters(header, request))
            .unwrap_or(true);
        let body_match = if !eligible {
            workspace.overlap.clear();
            skip_body(&mut reader, body_len)?;
            None
        } else if request.raw {
            workspace.overlap.clear();
            skip_body(&mut reader, body_len)?;
            let header = parsed_header
                .as_ref()
                .expect("a raw scan parses every header");
            scan_source_record(header, catalog, query, &mut workspace, stats, sources)?
                .map(|snippet| (None, snippet))
        } else {
            scan_values(&mut reader, body_len, query, &mut workspace)?
                .map(|(field, snippet)| (Some(field), snippet))
        };
        if let Some((field, snippet)) = body_match {
            let header = match parsed_header {
                Some(header) => header,
                None => parse_event_header_checked(path, &header_line)?,
            };
            stats.matched_events += 1;
            let source_path = catalog
                .get(&header.source_id)
                .map(|entry| entry.path.clone())
                .unwrap_or_default();
            *stats
                .facets
                .source
                .entry(header.source_id.clone())
                .or_default() += 1;
            *stats
                .facets
                .session
                .entry(header.session.clone())
                .or_default() += 1;
            *stats.facets.kind.entry(header.kind.clone()).or_default() += 1;
            *stats
                .facets
                .sender
                .entry(header.sender.clone())
                .or_default() += 1;
            let date = time::date_bucket(&header.timestamp, request.offset_minutes);
            *stats.facets.date.entry(date.clone()).or_default() += 1;
            let hit = SearchHit {
                instant: time::instant(&header.timestamp, request.offset_minutes),
                header,
                source_path,
                field,
                snippet,
            };
            if is_history_request(request) {
                let bucket = stats.timeline.entry(date).or_default();
                bucket.matched_events += 1;
                *bucket
                    .source
                    .entry(hit.header.source_id.clone())
                    .or_default() += 1;
                *bucket
                    .session
                    .entry(hit.header.session.clone())
                    .or_default() += 1;
                *bucket.kind.entry(hit.header.kind.clone()).or_default() += 1;
                let new_kind = bucket.sample_kinds.insert(hit.header.kind.clone());
                if bucket.samples.len() < HISTORY_SAMPLES_PER_DATE
                    || (new_kind && bucket.samples.len() < HISTORY_SAMPLES_PER_DATE * 2)
                {
                    bucket.samples.push(hit.clone());
                }
            }
            add_search_candidate(stats, hit, candidate_limit(request), request.newest_first);
        }
        let mut separator = [0u8; 1];
        reader.read_exact(&mut separator)?;
        if separator[0] != b'\n' {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: missing event separator", path.display()),
            ));
        }
    }
    Ok(())
}

fn scan_source_record(
    header: &EventHeader,
    catalog: &BTreeMap<String, SourceEntry>,
    query: &[u8],
    workspace: &mut ScanWorkspace,
    stats: &mut SearchStats,
    sources: &mut SourceReaders,
) -> io::Result<Option<String>> {
    let Some(file) = sources.get(&header.source_id, catalog) else {
        stats.unreachable_sources.insert(header.source_id.clone());
        stats.unreadable_records += 1;
        return Ok(None);
    };
    if file.seek(SeekFrom::Start(header.byte_start)).is_err() {
        stats.unreadable_records += 1;
        return Ok(None);
    }
    workspace.overlap.clear();
    match scan_bytes(file, header.byte_len, query, workspace) {
        Ok(matched) => {
            stats.scanned_records += 1;
            stats.scanned_bytes = stats.scanned_bytes.saturating_add(header.byte_len);
            Ok(matched)
        }
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            stats.unreadable_records += 1;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn request_has_filters(request: &SearchRequest) -> bool {
    request.source_id.is_some()
        || request.session.is_some()
        || request.role.is_some()
        || request.kind.is_some()
        || request.sender.is_some()
        || request.from.is_some()
        || request.to.is_some()
}

struct Coverage {
    corpus_sources: u64,
    incomplete_sources: u64,
    output_bounded_sources: u64,
    unreachable_sources: u64,
    unavailable_source_paths: u64,
    unreadable_records: u64,
    stale_sources: u64,
    synced_through_ms: Option<u64>,
    unscanned_source_bytes: u64,
    scanned_records: u64,
    raw: bool,
    offset_minutes: i64,
}

impl Coverage {
    fn absence_settled_through_ms(&self) -> Option<u64> {
        if self.raw
            && self.incomplete_sources == 0
            && self.unreachable_sources == 0
            && self.unavailable_source_paths == 0
            && self.unreadable_records == 0
        {
            self.synced_through_ms
        } else {
            None
        }
    }

    fn disposition(&self) -> &'static str {
        if self.raw {
            if self.absence_settled_through_ms().is_some() {
                "source_records_scanned"
            } else {
                "source_records_partially_scanned"
            }
        } else if self.unavailable_source_paths > 0 {
            "projection_source_paths_unavailable"
        } else if self.stale_sources > 0 {
            "projection_behind_source"
        } else if self.incomplete_sources > 0 {
            "projection_partially_scanned"
        } else if self.output_bounded_sources > 0 {
            "projection_scanned_output_bounded"
        } else {
            "projection_scanned"
        }
    }

    fn empty_disposition(&self) -> &'static str {
        if self.raw {
            if self.absence_settled_through_ms().is_some() {
                "no_match_in_source_records"
            } else {
                "no_match_partial_source_scan"
            }
        } else {
            "no_match_in_projection"
        }
    }

    fn write_fields(&self, output: &mut String) {
        number(output, "corpus_sources", self.corpus_sources);
        number(output, "incomplete_sources", self.incomplete_sources);
        number(
            output,
            "output_bounded_sources",
            self.output_bounded_sources,
        );
        number(output, "unreachable_sources", self.unreachable_sources);
        number(
            output,
            "unavailable_source_paths",
            self.unavailable_source_paths,
        );
        number(output, "unreadable_records", self.unreadable_records);
        number(output, "stale_sources", self.stale_sources);
        number(
            output,
            "unscanned_source_bytes",
            self.unscanned_source_bytes,
        );
        if self.raw {
            number(output, "scanned_records", self.scanned_records);
        }
        field(output, "coverage_disposition", self.disposition(), false);
        let render = |instant: u64| time::rfc3339_from_unix_ms(instant as i64, self.offset_minutes);
        optional_field(
            output,
            "synced_through",
            self.synced_through_ms.map(render).as_deref(),
        );
        optional_field(
            output,
            "absence_settled_through",
            self.absence_settled_through_ms().map(render).as_deref(),
        );
    }
}

fn coverage_stats(
    root: &Path,
    scope: &corpus::Scope,
    files: &[std::path::PathBuf],
    request: &SearchRequest,
    stats: &SearchStats,
) -> io::Result<Coverage> {
    let available = files
        .iter()
        .filter_map(|path| corpus_source_id(path))
        .collect::<BTreeSet<_>>();
    let mut coverage = Coverage {
        corpus_sources: 0,
        incomplete_sources: 0,
        output_bounded_sources: 0,
        unreachable_sources: 0,
        unavailable_source_paths: 0,
        unreadable_records: stats.unreadable_records,
        stale_sources: 0,
        unscanned_source_bytes: 0,
        synced_through_ms: None,
        scanned_records: stats.scanned_records,
        raw: request.raw,
        offset_minutes: request.offset_minutes,
    };
    let mut unreachable = stats.unreachable_sources.clone();
    let availability = corpus::source_availability(root)?;
    for entry in availability.entries {
        if !corpus::source_path_is_unavailable(&entry) {
            continue;
        }
        if !scope.whole && !scope.contains(&entry.source_id) {
            continue;
        }
        coverage.unavailable_source_paths += 1;
        if !entry.source_id.is_empty() {
            unreachable.insert(entry.source_id);
        }
    }
    let staleness = corpus::Staleness::of(scope.sources.iter().copied());
    for entry in &scope.sources {
        coverage.corpus_sources += 1;
        coverage.synced_through_ms = Some(match coverage.synced_through_ms {
            Some(current) => current.min(entry.synced_at_ms),
            None => entry.synced_at_ms,
        });
        if entry.output_preview > 0 {
            coverage.output_bounded_sources += 1;
        }
        let incomplete = !available.contains(&entry.source_id)
            || !corpus::source_is_complete(entry)
            || !corpus::source_projection_complete(entry, files)?;
        if incomplete {
            coverage.incomplete_sources += 1;
        }
    }
    coverage.stale_sources = staleness.stale_sources;
    coverage.unscanned_source_bytes = staleness.unscanned_source_bytes;
    unreachable.extend(staleness.unreachable);
    coverage.unreachable_sources = unreachable.len() as u64;
    Ok(coverage)
}

fn corpus_source_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".corpus")?;
    Some(stem.split("--").next().unwrap_or(stem).to_string())
}

fn parse_event_header_checked(path: &Path, line: &[u8]) -> io::Result<EventHeader> {
    parse_event_header(line).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {}", path.display(), error),
        )
    })
}

fn parse_body_len(line: &[u8]) -> Result<u64, String> {
    let marker = b"\tbody_len=";
    let start = line
        .windows(marker.len())
        .position(|window| window == marker)
        .map(|position| position + marker.len())
        .ok_or_else(|| "header has no body_len".to_string())?;
    let end = line[start..]
        .iter()
        .position(|byte| *byte == b'\t' || *byte == b'\r' || *byte == b'\n')
        .map(|offset| start + offset)
        .unwrap_or(line.len());
    std::str::from_utf8(&line[start..end])
        .map_err(|_| "body_len is not UTF-8".to_string())?
        .parse()
        .map_err(|_| "body_len is not an integer".to_string())
}

fn matches_filters(header: &EventHeader, request: &SearchRequest) -> bool {
    if let Some(source_id) = request.source_id.as_deref() {
        if header.source_id != source_id {
            return false;
        }
    }
    if let Some(session) = request.session.as_deref() {
        if header.session != session {
            return false;
        }
    }
    if let Some(role) = request.role.as_deref() {
        if header.role != role {
            return false;
        }
    }
    if let Some(kind) = request.kind.as_deref() {
        if header.kind != kind {
            return false;
        }
    }
    if let Some(sender) = request.sender.as_deref() {
        if header.sender != sender {
            return false;
        }
    }
    request
        .range
        .contains(&header.timestamp, request.offset_minutes)
}

/// Reads a corpus body and finds `query` in the values of its fields, never in the paths and
/// lengths that frame them. Returns the path of the first field that holds it and the text of
/// that value around it.
fn scan_values<R: Read>(
    reader: &mut R,
    body_len: u64,
    query: &[u8],
    workspace: &mut ScanWorkspace,
) -> io::Result<Option<(String, String)>> {
    let ScanWorkspace {
        body,
        folded,
        fold_ascii_case,
        ..
    } = workspace;
    read_whole(reader, body_len, body)?;
    for (path, value) in format::body_field_slices(body) {
        let haystack = if *fold_ascii_case {
            folded.clear();
            folded.extend(value.iter().map(u8::to_ascii_lowercase));
            folded.as_slice()
        } else {
            value
        };
        if let Some(position) = find_bytes(haystack, query) {
            return Ok(Some((
                String::from_utf8_lossy(path).into_owned(),
                text_around(value, position, query.len()),
            )));
        }
    }
    Ok(None)
}

/// Up to `SNIPPET_CONTEXT_BYTES` on each side of a match, on character boundaries.
fn text_around(value: &[u8], position: usize, length: usize) -> String {
    let mut start = position.saturating_sub(SNIPPET_CONTEXT_BYTES);
    while start > 0 && value[start] & 0xC0 == 0x80 {
        start -= 1;
    }
    let mut end = position
        .saturating_add(length)
        .saturating_add(SNIPPET_CONTEXT_BYTES)
        .min(value.len());
    while end < value.len() && value[end] & 0xC0 == 0x80 {
        end += 1;
    }
    String::from_utf8_lossy(&value[start..end]).into_owned()
}

/// The start of a body's values, joined by newlines and cut at `limit` bytes; marked when cut.
fn values_head(body: &[u8], limit: usize) -> String {
    let mut head = Vec::new();
    let mut cut = false;
    for (_, value) in format::body_field_slices(body) {
        if !head.is_empty() {
            head.push(b'\n');
        }
        head.extend_from_slice(value);
        if head.len() > limit {
            cut = true;
            break;
        }
    }
    let mut end = head.len().min(limit);
    while end > 0 && end < head.len() && head[end] & 0xC0 == 0x80 {
        end -= 1;
    }
    let mut text = String::from_utf8_lossy(&head[..end]).into_owned();
    if cut {
        text.push_str(crate::core::PROJECTION_BOUND_MARKER);
    }
    text
}

/// Reads a body of `body_len` bytes into `body`, reusing its capacity.
fn read_whole<R: Read>(reader: &mut R, body_len: u64, body: &mut Vec<u8>) -> io::Result<()> {
    body.clear();
    let read = reader.take(body_len).read_to_end(body)?;
    if read as u64 != body_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "corpus body ended before body_len",
        ));
    }
    Ok(())
}

/// Finds `query` anywhere in `body_len` bytes, read in chunks: the original records of a raw
/// search, which are JSON rather than corpus fields.
fn scan_bytes<R: Read>(
    reader: &mut R,
    body_len: u64,
    query: &[u8],
    workspace: &mut ScanWorkspace,
) -> io::Result<Option<String>> {
    let mut remaining = body_len;
    let overlap_len = query.len().saturating_sub(1);
    let chunk_size = BODY_CHUNK_SIZE.max(query.len());
    let mut matched = None;
    while remaining > 0 {
        let chunk_len = usize::try_from(remaining.min(chunk_size as u64)).unwrap_or(chunk_size);
        reader.read_exact(&mut workspace.chunk[..chunk_len])?;
        remaining -= chunk_len as u64;
        if matched.is_none() {
            workspace.combined.clear();
            workspace.combined.extend_from_slice(&workspace.overlap);
            workspace
                .combined
                .extend_from_slice(&workspace.chunk[..chunk_len]);
            if let Some(position) = find_bytes(workspace.haystack(), query) {
                let start = position.saturating_sub(120);
                let end = workspace
                    .combined
                    .len()
                    .min(position.saturating_add(query.len()).saturating_add(120));
                matched =
                    Some(String::from_utf8_lossy(&workspace.combined[start..end]).into_owned());
            }
            if overlap_len > 0 {
                let keep = overlap_len.min(workspace.combined.len());
                workspace.overlap.clear();
                workspace
                    .overlap
                    .extend_from_slice(&workspace.combined[workspace.combined.len() - keep..]);
            }
        }
    }
    Ok(matched)
}

fn skip_body<R: Read>(reader: &mut R, body_len: u64) -> io::Result<()> {
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

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn hit_json(hit: &SearchHit) -> String {
    hit_json_with_snippet_state(hit, None)
}

fn timeline_map_hit_json(hit: &SearchHit) -> String {
    hit_json_with_snippet_state(hit, Some("not_loaded_in_map"))
}

fn hit_json_with_snippet_state(hit: &SearchHit, snippet_state: Option<&str>) -> String {
    let header = &hit.header;
    let mut output = format!(
        "{{\"event_id\":{},\"source_ref\":{{\"source_id\":{},\"source_path\":{},\"line\":{},\"byte_start\":{},\"byte_len\":{}}},\"session\":{},\"turn\":{},\"kind\":{},\"role\":{},\"sender\":{},\"via\":{},\"timestamp\":{},\"field\":{},\"snippet\":{}",
        json_string(&format!("{}:{}", header.source_id, header.event_index)),
        json_string(&header.source_id),
        json_string(&hit.source_path),
        header.line,
        header.byte_start,
        header.byte_len,
        json_string(&header.session),
        json_string(&header.turn),
        json_string(&header.kind),
        json_string(&header.role),
        json_string(&header.sender),
        json_string(&header.via),
        json_string(&header.timestamp),
        hit.field
            .as_deref()
            .map(json_string)
            .unwrap_or_else(|| "null".to_string()),
        json_string(&hit.snippet),
    );
    if let Some(state) = snippet_state {
        field(&mut output, "snippet_state", state, false);
    }
    output.push('}');
    output
}

fn facets_json(facets: &Facets) -> String {
    format!(
        "{{\"source\":{},\"session\":{},\"kind\":{},\"sender\":{},\"date\":{}}}",
        map_json(&facets.source),
        map_json(&facets.session),
        map_json(&facets.kind),
        map_json(&facets.sender),
        map_json(&facets.date),
    )
}

fn map_json(values: &BTreeMap<String, u64>) -> String {
    map_json_limited(values, 50)
}

fn map_json_limited(values: &BTreeMap<String, u64>, limit: usize) -> String {
    let mut entries = values.iter().collect::<Vec<_>>();
    entries.sort_by(|(left_value, left_count), (right_value, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| left_value.cmp(right_value))
    });
    let mut output = format!(
        "{{\"distinct\":{},\"truncated\":{},\"items\":[",
        values.len(),
        values.len() > limit
    );
    for (index, (value, count)) in entries.into_iter().take(limit).enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str(&format!(
            "{{\"value\":{},\"count\":{}}}",
            json_string(value),
            count
        ));
    }
    output.push_str("]}");
    output
}

fn field(output: &mut String, key: &str, value: &str, first: bool) {
    if !first {
        output.push(',');
    }
    output.push_str(&format!("\"{}\":{}", key, json_string(value)));
}

fn optional_field(output: &mut String, key: &str, value: Option<&str>) {
    match value {
        Some(value) => field(output, key, value, false),
        None => output.push_str(&format!(",\"{}\":null", key)),
    }
}

fn optional_field_first(output: &mut String, key: &str, value: Option<&str>) {
    match value {
        Some(value) => field(output, key, value, true),
        None => output.push_str(&format!("\"{}\":null", key)),
    }
}

fn number(output: &mut String, key: &str, value: u64) {
    output.push_str(&format!(",\"{}\":{}", key, value));
}

fn boolean(output: &mut String, key: &str, value: bool) {
    output.push_str(&format!(",\"{}\":{}", key, value));
}

#[cfg(test)]
mod tests {
    use super::{
        history_map_json, run, scan_sequential, scan_timeline_run, search_page, Coverage,
        SearchHit, SearchRequest, SearchStats, TimelineBucket as SearchTimelineBucket,
        TimelineOrder, TimelinePage, TimelineRequest,
    };
    use crate::core::TimelineBucket;
    use crate::corpus::TimelineRun;
    use crate::format::{event_header_line, EventHeader};
    use std::fs::{self, File};
    use std::io::Write;

    #[test]
    fn timeline_defaults_to_recent_dates_first() {
        assert_eq!(TimelineOrder::default(), TimelineOrder::Desc);
    }

    fn coverage(raw: bool, output_bounded: u64, unreachable: u64, incomplete: u64) -> Coverage {
        Coverage {
            corpus_sources: 1,
            incomplete_sources: incomplete,
            output_bounded_sources: output_bounded,
            unreachable_sources: unreachable,
            unavailable_source_paths: 0,
            unreadable_records: 0,
            stale_sources: 0,
            unscanned_source_bytes: 0,
            synced_through_ms: Some(1_787_011_200_000),
            scanned_records: 1,
            raw,
            offset_minutes: 0,
        }
    }

    #[test]
    fn complete_source_scan_settles_absence() {
        assert!(coverage(false, 0, 0, 0)
            .absence_settled_through_ms()
            .is_none());
        assert!(coverage(false, 1, 0, 0)
            .absence_settled_through_ms()
            .is_none());
        assert_eq!(
            coverage(false, 0, 0, 0).empty_disposition(),
            "no_match_in_projection"
        );
        assert_eq!(
            coverage(false, 1, 0, 0).empty_disposition(),
            "no_match_in_projection"
        );
        assert_eq!(
            coverage(false, 1, 0, 0).disposition(),
            "projection_scanned_output_bounded"
        );
        assert_eq!(
            coverage(false, 0, 0, 1).disposition(),
            "projection_partially_scanned"
        );

        assert_eq!(
            coverage(true, 1, 0, 0).absence_settled_through_ms(),
            Some(1_787_011_200_000)
        );
        assert_eq!(
            coverage(true, 1, 0, 0).empty_disposition(),
            "no_match_in_source_records"
        );
        assert!(coverage(true, 0, 1, 0)
            .absence_settled_through_ms()
            .is_none());
        assert_eq!(
            coverage(true, 0, 1, 0).empty_disposition(),
            "no_match_partial_source_scan"
        );
        assert!(coverage(true, 0, 0, 1)
            .absence_settled_through_ms()
            .is_none());

        let mut stale = coverage(true, 0, 0, 0);
        stale.stale_sources = 1;
        stale.unscanned_source_bytes = 171;
        assert_eq!(
            stale.absence_settled_through_ms(),
            Some(1_787_011_200_000),
            "the boundary is still the boundary"
        );

        let mut stale_projection = coverage(false, 0, 0, 0);
        stale_projection.stale_sources = 1;
        assert_eq!(stale_projection.disposition(), "projection_behind_source");
    }

    #[test]
    fn raw_scan_finds_output_omitted_from_projection() {
        let root = std::env::temp_dir().join(format!("ebira-raw-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create temp directory");
        let source = root.join("session.jsonl");
        let output = root.join("corpus");

        let tail = "TAIL-ONLY-IN-SOURCE";
        let record = format!(
            "{{\"response_item\":{{\"type\":\"function_call_output\",\"call_id\":\"c1\",\"stdout\":\"HEAD {} {}\"}},\"session_id\":\"s\",\"turn_id\":\"t1\"}}\n",
            "filler ".repeat(200),
            tail
        );
        std::fs::write(&source, record.as_bytes()).expect("write source");
        let input = source.to_string_lossy().into_owned();
        crate::corpus::build(&[input], &output, false, 40, &[]).expect("build bounded corpus");

        let catalog = crate::corpus::source_catalog(&output).expect("catalog");
        let files = crate::corpus::corpus_files(&output).expect("corpus files");
        let request = |raw: bool| SearchRequest {
            query: tail.to_string(),
            limit: 10,
            raw,
            ..SearchRequest::default()
        };

        let projection = scan_sequential(&files, &catalog, &request(false)).expect("scan corpus");
        assert_eq!(
            projection.matched_events, 0,
            "the projection bounded this text away"
        );

        let source_scan = scan_sequential(&files, &catalog, &request(true)).expect("scan source");
        assert_eq!(source_scan.matched_events, 1, "the source still holds it");
        assert_eq!(source_scan.unreadable_records, 0);
        assert!(source_scan.unreachable_sources.is_empty());

        std::fs::remove_file(&source).expect("remove source");
        let lost = scan_sequential(&files, &catalog, &request(true)).expect("scan without source");
        assert_eq!(lost.matched_events, 0);
        assert_eq!(lost.unreachable_sources.len(), 1);
        assert!(lost.unreadable_records > 0);

        std::fs::remove_dir_all(&root).expect("remove temp directory");
    }

    #[test]
    fn zero_limit_is_rejected_before_scanning() {
        let result = run(
            std::path::Path::new("missing-corpus"),
            SearchRequest {
                query: "query".to_string(),
                limit: 0,
                ..SearchRequest::default()
            },
        );
        assert_eq!(
            result.expect_err("zero limit must fail").kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn search_page_orders_matches_by_timestamp() {
        let request = SearchRequest {
            limit: 2,
            ..SearchRequest::default()
        };
        let hit = |timestamp: &str, event_index: u64| SearchHit {
            instant: crate::time::instant(timestamp, 0),
            header: EventHeader {
                timestamp: timestamp.to_string(),
                event_index,
                source_id: "source".to_string(),
                ..EventHeader::default()
            },
            source_path: String::new(),
            field: None,
            snippet: String::new(),
        };
        let page = search_page(
            vec![
                hit("2026-02-22T11:00:00Z", 2),
                hit("2026-02-22T09:00:00Z", 0),
                hit("2026-02-22T10:00:00Z", 1),
            ],
            &request,
        );
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].header.event_index, 0);
        assert_eq!(page[1].header.event_index, 1);
    }

    #[test]
    fn history_map_returns_a_bounded_recent_date_page() {
        let mut stats = SearchStats::default();
        for date in ["2026-02-20", "2026-02-21", "2026-02-22"] {
            stats
                .timeline
                .insert(date.to_string(), SearchTimelineBucket::default());
        }
        let output = history_map_json(&stats, 0, 2);
        assert!(output.contains("\"returned_dates\":2"));
        assert!(output.contains("\"truncated\":true"));
        assert!(output.find("2026-02-22") < output.find("2026-02-21"));
        assert!(!output.contains("2026-02-20"));
        let next = history_map_json(&stats, 2, 2);
        assert!(next.contains("2026-02-20"));
    }

    #[test]
    fn timeline_date_run_applies_time_bounds() {
        let root =
            std::env::temp_dir().join(format!("ebira-timeline-time-filter-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp directory");
        let path = root.join("timeline.corpus");
        let mut file = File::create(&path).expect("create timeline corpus");
        for (event_index, timestamp) in [(0, "2026-02-22T10:00:00Z"), (1, "2026-02-22T11:00:00Z")] {
            let header = EventHeader {
                event_index,
                source_id: "source".to_string(),
                line: event_index + 1,
                session: "session".to_string(),
                turn: "turn".to_string(),
                kind: "user".to_string(),
                role: "user".to_string(),
                timestamp: timestamp.to_string(),
                body_len: 1,
                ..EventHeader::default()
            };
            file.write_all(event_header_line(&header).as_bytes())
                .expect("write header");
            file.write_all(b"x\n").expect("write body");
        }
        drop(file);
        let run = TimelineRun {
            source_id: "source".to_string(),
            date: "2026-02-22".to_string(),
            corpus_file: "timeline.corpus".to_string(),
            corpus_start: 0,
            corpus_end: fs::metadata(&path).expect("timeline metadata").len(),
            bucket: TimelineBucket::default(),
        };
        let request = TimelineRequest {
            date: Some("2026-02-22".to_string()),
            from: Some("2026-02-22T10:30:00Z".to_string()),
            range: crate::time::Range::parse(Some("2026-02-22T10:30:00Z"), None, 0)
                .expect("parse range"),
            limit: 10,
            ..TimelineRequest::default()
        };
        let mut page = TimelinePage::new(&request).expect("create timeline page");
        scan_timeline_run(&path, &run, "", &request, &mut page).expect("scan timeline date run");
        assert_eq!(page.total, 1);
        assert_eq!(page.hits[0].header.timestamp, "2026-02-22T11:00:00Z");
        fs::remove_dir_all(&root).expect("remove temp directory");
    }
}
