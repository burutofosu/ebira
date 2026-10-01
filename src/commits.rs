use crate::corpus::{self, SourceEntry};
use crate::format::parse_event_header;
use crate::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

const SCAN_BUFFER_SIZE: usize = 1024 * 1024;
const BODY_CHUNK_SIZE: usize = 64 * 1024;
const MIN_HASH_LEN: usize = 7;
const MAX_HASH_LEN: usize = 40;
const MENTIONS_PER_COMMIT: usize = 16;

/// Keeps the `keep` earliest mentions, by the instants their timestamps name.
fn retain_earliest(mentions: &mut Vec<Mention>, keep: usize, offset_minutes: i64) {
    mentions.sort_by(|left, right| {
        crate::time::compare(&left.timestamp, &right.timestamp, offset_minutes)
    });
    mentions.truncate(keep);
}

pub struct CommitRequest {
    pub repo: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub range: crate::time::Range,
    pub offset_minutes: i64,
    pub limit: usize,
    pub offset: u64,
}

#[derive(Default)]
struct Mention {
    source_id: String,
    line: u64,
    byte_start: u64,
    byte_len: u64,
    session: String,
    timestamp: String,
}

#[derive(Default)]
struct Candidate {
    mentions: Vec<Mention>,
    mentions_seen: u64,
    written_as: BTreeSet<String>,
}

struct CommitFacts {
    date: String,
    subject: String,
}

pub fn run(root: &Path, request: &CommitRequest) -> io::Result<()> {
    if request.limit == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--limit must be at least 1",
        ));
    }
    let catalog = corpus::source_catalog(root)?;
    if catalog.is_empty() {
        return Err(corpus::no_corpus_here(root));
    }
    let repo = Path::new(&request.repo);
    git_dir(repo)?;

    let files = corpus::corpus_files(root)?;
    let mut scanned_files = 0u64;
    let mut scanned_bytes = 0u64;
    for path in &files {
        scanned_files += 1;
        scanned_bytes = scanned_bytes.saturating_add(path.metadata()?.len());
    }

    let (names, scanned_records) = scan_all(&files, request, None)?;
    let candidate_names = names.len() as u64;
    let resolved = resolve_commits(repo, &names.keys().cloned().collect::<Vec<_>>())?;
    let wanted = resolved.keys().cloned().collect::<BTreeSet<_>>();
    let facts = commit_facts(repo, &resolved)?;
    let (candidates, _) = scan_all(&files, request, Some(&wanted))?;

    let mut joined = facts.into_iter().collect::<Vec<_>>();
    joined.sort_by(|left, right| {
        crate::time::compare(&right.1.date, &left.1.date, request.offset_minutes)
            .then(left.0.cmp(&right.0))
    });
    let total = joined.len() as u64;
    let start = usize::try_from(request.offset)
        .unwrap_or(usize::MAX)
        .min(joined.len());
    let page = joined
        .into_iter()
        .skip(start)
        .take(request.limit)
        .collect::<Vec<_>>();

    print_result(
        root,
        request,
        &catalog,
        &candidates,
        &resolved,
        &page,
        Counts {
            scanned_files,
            scanned_bytes,
            scanned_records,
            candidate_names,
            resolved_names: resolved.len() as u64,
            total,
        },
    );
    Ok(())
}

struct Counts {
    scanned_files: u64,
    scanned_bytes: u64,
    scanned_records: u64,
    candidate_names: u64,
    resolved_names: u64,
    total: u64,
}

fn scan_all(
    files: &[PathBuf],
    request: &CommitRequest,
    wanted: Option<&BTreeSet<String>>,
) -> io::Result<(BTreeMap<String, Candidate>, u64)> {
    let worker_count = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .min(files.len().max(1));
    let next_file = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
    thread::scope(|scope| -> io::Result<(BTreeMap<String, Candidate>, u64)> {
        for _ in 0..worker_count {
            let sender = sender.clone();
            let next_file = &next_file;
            scope.spawn(move || {
                let mut local: BTreeMap<String, Candidate> = BTreeMap::new();
                let mut records = 0u64;
                let mut failure = None;
                loop {
                    let position = next_file.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(position) else {
                        break;
                    };
                    if let Err(error) =
                        scan_segment(path, request, wanted, &mut local, &mut records)
                    {
                        failure = Some(error);
                        break;
                    }
                }
                let _ = sender.send((local, records, failure));
            });
        }
        drop(sender);

        let mut merged: BTreeMap<String, Candidate> = BTreeMap::new();
        let mut scanned_records = 0u64;
        let mut first_failure = None;
        for (local, records, failure) in receiver {
            if failure.is_some() && first_failure.is_none() {
                first_failure = failure;
            }
            scanned_records = scanned_records.saturating_add(records);
            for (name, candidate) in local {
                let into = merged.entry(name).or_default();
                into.mentions_seen = into.mentions_seen.saturating_add(candidate.mentions_seen);
                into.written_as.extend(candidate.written_as);
                into.mentions.extend(candidate.mentions);
                retain_earliest(
                    &mut into.mentions,
                    MENTIONS_PER_COMMIT,
                    request.offset_minutes,
                );
            }
        }
        match first_failure {
            Some(error) => Err(error),
            None => Ok((merged, scanned_records)),
        }
    })
}

fn scan_segment(
    path: &Path,
    request: &CommitRequest,
    wanted: Option<&BTreeSet<String>>,
    out: &mut BTreeMap<String, Candidate>,
    scanned_records: &mut u64,
) -> io::Result<()> {
    let mut reader = BufReader::with_capacity(SCAN_BUFFER_SIZE, File::open(path)?);
    let mut header_line = Vec::new();
    let mut body = Vec::new();
    loop {
        header_line.clear();
        if reader.read_until(b'\n', &mut header_line)? == 0 {
            break;
        }
        let header = parse_event_header(&header_line)
            .map_err(|error| invalid_segment(path, &format!("invalid event header: {}", error)))?;
        if !timestamp_within(&header.timestamp, request) {
            skip_body(&mut reader, header.body_len, path)?;
            continue;
        }
        *scanned_records += 1;
        read_body(&mut reader, header.body_len, &mut body, path)?;
        for name in record_names(&body) {
            let key = name.to_ascii_lowercase();
            if let Some(wanted) = wanted {
                if !wanted.contains(&key) {
                    continue;
                }
            }
            let entry = out.entry(key).or_default();
            entry.mentions_seen = entry.mentions_seen.saturating_add(1);
            entry.written_as.insert(name);
            if wanted.is_some() {
                entry.mentions.push(Mention {
                    source_id: header.source_id.clone(),
                    line: header.line,
                    byte_start: header.byte_start,
                    byte_len: header.byte_len,
                    session: header.session.clone(),
                    timestamp: header.timestamp.clone(),
                });
                if entry.mentions.len() > MENTIONS_PER_COMMIT * 2 {
                    retain_earliest(
                        &mut entry.mentions,
                        MENTIONS_PER_COMMIT,
                        request.offset_minutes,
                    );
                }
            }
        }
    }
    Ok(())
}

/// The names a record mentions: hexadecimal runs in the values of its fields, never in the
/// paths and lengths that frame them.
fn record_names(body: &[u8]) -> Vec<String> {
    let mut names = crate::format::body_field_slices(body)
        .flat_map(|(_, value)| hash_candidates(value))
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

fn hash_candidates(body: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    let mut start = None;
    let mut has_letter = false;
    for (index, byte) in body.iter().enumerate() {
        let hex = byte.is_ascii_digit() || matches!(byte | 0x20, b'a'..=b'f');
        if hex {
            if start.is_none() {
                start = Some(index);
                has_letter = false;
            }
            if !byte.is_ascii_digit() {
                has_letter = true;
            }
            continue;
        }
        if let Some(begin) = start.take() {
            take_run(body, begin, index, has_letter, &mut found);
        }
    }
    if let Some(begin) = start {
        take_run(body, begin, body.len(), has_letter, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

fn take_run(body: &[u8], begin: usize, end: usize, has_letter: bool, found: &mut Vec<String>) {
    let length = end - begin;
    if !has_letter || !(MIN_HASH_LEN..=MAX_HASH_LEN).contains(&length) {
        return;
    }
    let before = begin.checked_sub(1).and_then(|index| body.get(index));
    if before == Some(&b'-') || body.get(end) == Some(&b'-') {
        return;
    }
    if let Ok(text) = std::str::from_utf8(&body[begin..end]) {
        found.push(text.to_string());
    }
}

fn timestamp_within(timestamp: &str, request: &CommitRequest) -> bool {
    request.range.contains(timestamp, request.offset_minutes)
}

fn read_body<R: Read>(
    reader: &mut R,
    body_len: u64,
    body: &mut Vec<u8>,
    path: &Path,
) -> io::Result<()> {
    body.clear();
    let mut remaining = body_len;
    let mut chunk = [0u8; BODY_CHUNK_SIZE];
    while remaining > 0 {
        let take =
            usize::try_from(remaining.min(BODY_CHUNK_SIZE as u64)).unwrap_or(BODY_CHUNK_SIZE);
        reader
            .read_exact(&mut chunk[..take])
            .map_err(|error| invalid_segment_io(path, "body ended before body_len", error))?;
        body.extend_from_slice(&chunk[..take]);
        remaining -= take as u64;
    }
    read_record_separator(reader, path)
}

fn skip_body<R: Read>(reader: &mut R, body_len: u64, path: &Path) -> io::Result<()> {
    let mut remaining = body_len;
    let mut chunk = [0u8; BODY_CHUNK_SIZE];
    while remaining > 0 {
        let take =
            usize::try_from(remaining.min(BODY_CHUNK_SIZE as u64)).unwrap_or(BODY_CHUNK_SIZE);
        reader
            .read_exact(&mut chunk[..take])
            .map_err(|error| invalid_segment_io(path, "body ended before body_len", error))?;
        remaining -= take as u64;
    }
    read_record_separator(reader, path)
}

fn read_record_separator<R: Read>(reader: &mut R, path: &Path) -> io::Result<()> {
    let mut separator = [0u8; 1];
    reader
        .read_exact(&mut separator)
        .map_err(|error| invalid_segment_io(path, "record separator is missing", error))?;
    if separator != *b"\n" {
        return Err(invalid_segment(path, "record separator is not a newline"));
    }
    Ok(())
}

fn invalid_segment(path: &Path, message: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{}: {}; {}",
            path.display(),
            message,
            corpus::REBUILD_ADVICE
        ),
    )
}

fn invalid_segment_io(path: &Path, message: &str, error: io::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{}: {}: {}; {}",
            path.display(),
            message,
            error,
            corpus::REBUILD_ADVICE
        ),
    )
}

fn git_dir(repo: &Path) -> io::Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-dir"])
        .output()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "git could not be run: {}; install it or correct PATH",
                    error
                ),
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{}: not a git repository", repo.display()),
    ))
}

fn git_batch(repo: &Path, args: &[&str], names: &[String]) -> io::Result<String> {
    if names.is_empty() {
        return Ok(String::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let written = names.join("\n");
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(written.as_bytes());
        let _ = stdin.write_all(b"\n");
    });
    let output = child.wait_with_output()?;
    let _ = writer.join();
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The names that start exactly one commit of the repository, each with that commit. Git
/// lists the commits once; asking it about every name in a large corpus took minutes when the
/// repository sat on a slow file system, such as a Windows drive seen from WSL.
fn resolve_commits(repo: &Path, names: &[String]) -> io::Result<BTreeMap<String, String>> {
    let commits = repository_commits(repo)?;
    let mut resolved = BTreeMap::new();
    for name in names {
        let first = commits.partition_point(|commit| commit.as_str() < name.as_str());
        let mut matching = commits[first..]
            .iter()
            .take_while(|commit| commit.starts_with(name.as_str()));
        if let (Some(commit), None) = (matching.next(), matching.next()) {
            resolved.insert(name.clone(), commit.clone());
        }
    }
    Ok(resolved)
}

/// Every commit the refs and reflogs of the repository reach, sorted.
fn repository_commits(repo: &Path) -> io::Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--all", "--reflog"])
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git rev-list failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut commits = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_ascii_lowercase())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    commits.sort();
    commits.dedup();
    Ok(commits)
}

fn commit_facts(
    repo: &Path,
    resolved: &BTreeMap<String, String>,
) -> io::Result<BTreeMap<String, CommitFacts>> {
    let mut facts = BTreeMap::new();
    let objects = resolved
        .values()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let answered = git_batch(
        repo,
        &["log", "--no-walk", "--stdin", "--format=%H\t%aI\t%s"],
        &objects,
    )?;
    for line in answered.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(object), Some(date), Some(subject)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        facts.insert(
            object.to_string(),
            CommitFacts {
                date: date.to_string(),
                subject: subject.to_string(),
            },
        );
    }
    Ok(facts)
}

fn print_result(
    root: &Path,
    request: &CommitRequest,
    catalog: &BTreeMap<String, SourceEntry>,
    candidates: &BTreeMap<String, Candidate>,
    resolved: &BTreeMap<String, String>,
    page: &[(String, CommitFacts)],
    counts: Counts,
) {
    let mut by_object: BTreeMap<&str, Vec<&Candidate>> = BTreeMap::new();
    for (written, object) in resolved {
        if let Some(candidate) = candidates.get(written) {
            by_object
                .entry(object.as_str())
                .or_default()
                .push(candidate);
        }
    }

    let commits = page.iter().map(|(object, facts)| {
        let group = by_object.get(object.as_str()).cloned().unwrap_or_default();
        let mentions_seen: u64 = group.iter().map(|candidate| candidate.mentions_seen).sum();
        let mut ordered = group
            .iter()
            .flat_map(|candidate| candidate.mentions.iter())
            .collect::<Vec<_>>();
        ordered.sort_by(|left, right| {
            crate::time::compare(&left.timestamp, &right.timestamp, request.offset_minutes)
        });
        ordered.truncate(MENTIONS_PER_COMMIT);
        let mut written = group
            .iter()
            .flat_map(|candidate| candidate.written_as.iter().cloned())
            .collect::<Vec<_>>();
        written.sort();
        written.dedup();
        let mentions = ordered.iter().map(|mention| {
            let path = catalog
                .get(&mention.source_id)
                .map(|entry| entry.path.as_str())
                .unwrap_or("");
            json::Object::new()
                .name("session", &mention.session)
                .name("timestamp", &mention.timestamp)
                .raw(
                    "source_ref",
                    &json::source_ref(
                        &mention.source_id,
                        path,
                        Some(mention.line),
                        mention.byte_start,
                        mention.byte_len,
                    ),
                )
                .finish()
        });
        json::Object::new()
            .name("commit", object)
            .name("author_date", &facts.date)
            .text("subject", &facts.subject)
            .raw(
                "written_as",
                &json::strings(written.iter().map(String::as_str)),
            )
            .number("mentions_seen", mentions_seen)
            .number("mentions_returned", ordered.len() as u64)
            .raw("mentions", &json::array(mentions))
            .finish()
    });
    let filters = json::Object::new()
        .optional("from", request.from.as_deref())
        .optional("to", request.to.as_deref())
        .finish();
    let next_offset = request.offset.saturating_add(page.len() as u64);
    let has_more = next_offset < counts.total;
    println!(
        "{}",
        json::Object::new()
            .name("disposition", "commits_joined")
            .name("mode", "commit_join")
            .name("repo", &request.repo)
            .name("corpus", &root.to_string_lossy())
            .number("scanned_files", counts.scanned_files)
            .number("scanned_bytes", counts.scanned_bytes)
            .number("scanned_records", counts.scanned_records)
            .number("candidate_names", counts.candidate_names)
            .number("resolved_names", counts.resolved_names)
            .number("total_commits", counts.total)
            .number("returned", page.len() as u64)
            .number("offset", request.offset)
            .boolean("truncated", has_more)
            .optional_number("next_offset", has_more.then_some(next_offset))
            .raw("applied_filters", &filters)
            .raw("commits", &json::array(commits))
            .finish()
    );
}

#[cfg(test)]
mod tests {
    use super::{hash_candidates, record_names};

    #[test]
    fn names_come_from_values_not_from_field_paths() {
        let mut body = String::new();
        crate::format::push_field(&mut body, "/snapshot/deadbeef42", "see a878f10d");
        crate::format::push_field(&mut body, "/text", "and a878f10d again, then 5644ee8");
        assert_eq!(record_names(body.as_bytes()), ["5644ee8", "a878f10d"]);
    }

    #[test]
    fn hash_candidates_require_complete_hex_runs() {
        let body = b"see a878f10d and 5644ee8 at line 1234567 in \
                     e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let found = hash_candidates(body);
        assert!(found.contains(&"a878f10d".to_string()));
        assert!(found.contains(&"5644ee8".to_string()));
        assert!(
            !found.contains(&"1234567".to_string()),
            "digits alone are line numbers and byte counts, not object names"
        );
        assert_eq!(
            found.len(),
            2,
            "the 63-character digest is offered whole or not at all: {:?}",
            found
        );
    }

    #[test]
    fn hash_candidates_exclude_hyphenated_identifiers() {
        let found = hash_candidates(b"session 019f99ea-55e4-71f3-b408-fcd543cd8d2f ended");
        assert!(found.is_empty(), "{:?}", found);
    }

    #[test]
    fn retained_references_are_earliest_first() {
        let at = |stamp: &str| super::Mention {
            timestamp: stamp.to_string(),
            ..super::Mention::default()
        };
        let mut mentions = vec![
            at("2026-08-13T12:25:25Z"),
            at(""),
            at("2026-08-13T11:46:01Z"),
            at("2026-08-13T12:26:33Z"),
        ];
        super::retain_earliest(&mut mentions, 3, 0);
        let kept = mentions
            .iter()
            .map(|mention| mention.timestamp.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            kept,
            [
                "2026-08-13T11:46:01Z",
                "2026-08-13T12:25:25Z",
                "2026-08-13T12:26:33Z"
            ],
            "references are not ordered by recorded time"
        );
    }

    #[test]
    fn hash_candidates_reject_short_runs() {
        assert!(hash_candidates(b"abc123 dead is short").is_empty());
    }
}
