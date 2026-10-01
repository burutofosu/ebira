//! `ebira said`: the person's own messages, verbatim.
//!
//! Claude Code and Codex logs mark who put each record into the conversation. Records whose
//! sender is the person (typed, pasted, typed while the agent was working, slash commands,
//! answers to an agent's question) are listed with their text and source reference. Copies of
//! the same message (same text and timestamp, e.g. a session file stored twice) appear once.

use crate::core::{human_text_from_body, Sender};
use crate::corpus::{self, SourceEntry};
use crate::format::{json_string, parse_event_header, EventHeader};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

const IO_BUFFER_SIZE: usize = 1024 * 1024;
const MAX_HUMAN_BODY_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct SaidRequest {
    pub session: Option<String>,
    pub source_id: Option<String>,
    /// Case-insensitive substring of the working directory the message was sent in.
    pub cwd: Option<String>,
    /// `claude` or `codex`: the agent whose log recorded the message.
    pub agent: Option<String>,
    pub query: Option<String>,
    pub fold_ascii_case: bool,
    pub from: Option<String>,
    pub to: Option<String>,
    pub range: crate::time::Range,
    pub limit: usize,
    pub offset: u64,
    pub newest_first: bool,
    /// Text budget for one page, in characters. A message longer than the whole budget is
    /// cut and marked `text_truncated`.
    pub max_chars: usize,
    pub text_format: bool,
    pub offset_minutes: i64,
    /// Codex threads can import another agent's conversation; those copies of the person's
    /// messages carry the import time. They are left out unless asked for.
    pub include_imported: bool,
}

struct Said {
    header: EventHeader,
    source_path: String,
    /// `claude`, `codex`, or `other`, from the catalog.
    app: String,
    text: String,
    images: usize,
}

pub fn run(root: &Path, request: &SaidRequest) -> io::Result<()> {
    if request.limit == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--limit must be at least 1",
        ));
    }
    let started = Instant::now();
    let catalog = corpus::source_catalog(root)?;
    let scope = scoped_sources(&catalog, request);
    let files = scoped_files(root, &scope, request)?;
    let query = request
        .query
        .as_deref()
        .map(|query| fold(query, request.fold_ascii_case));

    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    let mut copies = 0u64;
    let mut imported = 0u64;
    let mut scanned_bytes = 0u64;
    for path in &files {
        scanned_bytes = scanned_bytes.saturating_add(path.metadata().map(|m| m.len()).unwrap_or(0));
        scan_file(path, &catalog, request, query.as_deref(), &mut |said| {
            if said.header.via == "imported" && !request.include_imported {
                imported += 1;
            // One message stored twice can differ in surrounding whitespace only (Codex writes
            // the text as an event and as a response item); the text shown stays as sent.
            } else if seen.insert((said.header.timestamp.clone(), said.text.trim().to_string())) {
                found.push(said);
            } else {
                copies += 1;
            }
        })?;
    }
    found.sort_by(|left, right| {
        let ordering = crate::time::compare(
            &left.header.timestamp,
            &right.header.timestamp,
            request.offset_minutes,
        )
        .then_with(|| left.header.source_id.cmp(&right.header.source_id))
        .then_with(|| left.header.event_index.cmp(&right.header.event_index));
        if request.newest_first {
            ordering.reverse()
        } else {
            ordering
        }
    });

    let total = found.len() as u64;
    let start = usize::try_from(request.offset)
        .unwrap_or(usize::MAX)
        .min(found.len());
    let mut budget = request.max_chars;
    let mut page = Vec::new();
    for said in found.into_iter().skip(start) {
        if page.len() == request.limit {
            break;
        }
        let length = said.text.chars().count();
        if !page.is_empty() && length > budget {
            break;
        }
        let cut = length > budget;
        let text = if cut {
            said.text.chars().take(budget).collect()
        } else {
            said.text.clone()
        };
        budget = budget.saturating_sub(length);
        page.push((said, text, cut));
    }
    let next_offset = request.offset.saturating_add(page.len() as u64);
    let has_more = next_offset < total;
    let (stale_sources, unscanned_bytes) = freshness(&scope);
    let duration_ms = started.elapsed().as_millis() as u64;

    if request.text_format {
        print!(
            "{}",
            text_output(
                request,
                &page,
                total,
                next_offset,
                has_more,
                copies + imported,
                stale_sources
            )
        );
        return Ok(());
    }
    let mut output = String::from("{");
    field(
        &mut output,
        "disposition",
        if total == 0 {
            "no_human_messages"
        } else if has_more {
            "result_page_truncated"
        } else {
            "ready"
        },
        true,
    );
    field(&mut output, "mode", "said", false);
    field(
        &mut output,
        "order",
        if request.newest_first { "desc" } else { "asc" },
        false,
    );
    output.push_str(",\"applied_filters\":{");
    optional_field(&mut output, "session", request.session.as_deref(), true);
    optional_field(
        &mut output,
        "source_id",
        request.source_id.as_deref(),
        false,
    );
    optional_field(&mut output, "cwd", request.cwd.as_deref(), false);
    optional_field(&mut output, "agent", request.agent.as_deref(), false);
    optional_field(&mut output, "query", request.query.as_deref(), false);
    optional_field(&mut output, "from", request.from.as_deref(), false);
    optional_field(&mut output, "to", request.to.as_deref(), false);
    output.push('}');
    number(&mut output, "total_messages", total);
    number(&mut output, "copies_skipped", copies);
    number(&mut output, "imported_skipped", imported);
    number(&mut output, "returned", page.len() as u64);
    number(&mut output, "offset", request.offset);
    boolean(&mut output, "truncated", has_more);
    if has_more {
        number(&mut output, "next_offset", next_offset);
    }
    number(&mut output, "max_chars", request.max_chars as u64);
    number(&mut output, "scanned_files", files.len() as u64);
    number(&mut output, "scanned_bytes", scanned_bytes);
    number(&mut output, "sources_in_scope", scope.len() as u64);
    number(&mut output, "stale_sources", stale_sources);
    number(&mut output, "unscanned_source_bytes", unscanned_bytes);
    number(&mut output, "duration_ms", duration_ms);
    output.push_str(",\"messages\":[");
    for (index, (said, text, cut)) in page.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str(&message_json(said, text, *cut, request.offset_minutes));
    }
    output.push_str("]}\n");
    print!("{}", output);
    Ok(())
}

fn scoped_sources<'a>(
    catalog: &'a BTreeMap<String, SourceEntry>,
    request: &SaidRequest,
) -> Vec<&'a SourceEntry> {
    catalog
        .values()
        .filter(|entry| {
            request
                .source_id
                .as_deref()
                .map(|source_id| entry.source_id == source_id)
                .unwrap_or(true)
        })
        .filter(|entry| {
            request
                .agent
                .as_deref()
                .map(|agent| entry.app == agent)
                .unwrap_or(true)
        })
        .filter(|entry| {
            request
                .session
                .as_deref()
                .map(|session| corpus::source_declares_session(entry, session))
                .unwrap_or(true)
        })
        .collect()
}

fn scoped_files(
    root: &Path,
    scope: &[&SourceEntry],
    request: &SaidRequest,
) -> io::Result<Vec<PathBuf>> {
    if request.source_id.is_none() && request.session.is_none() && request.agent.is_none() {
        return corpus::corpus_files(root);
    }
    let mut files = Vec::new();
    for entry in scope {
        files.extend(corpus::source_files(root, &entry.source_id)?);
    }
    Ok(files)
}

fn scan_file(
    path: &Path,
    catalog: &BTreeMap<String, SourceEntry>,
    request: &SaidRequest,
    query: Option<&str>,
    accept: &mut dyn FnMut(Said),
) -> io::Result<()> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(IO_BUFFER_SIZE, file);
    let mut header_line = Vec::new();
    let human = Sender::Human.as_str();
    loop {
        header_line.clear();
        if reader.read_until(b'\n', &mut header_line)? == 0 {
            break;
        }
        let header = parse_event_header(&header_line).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {}", path.display(), error),
            )
        })?;
        let wanted = header.sender == human
            && header.body_len <= MAX_HUMAN_BODY_BYTES
            && header_matches(&header, request);
        if !wanted {
            skip(&mut reader, header.body_len.saturating_add(1))?;
            continue;
        }
        let mut body = vec![0; header.body_len as usize];
        reader.read_exact(&mut body)?;
        skip(&mut reader, 1)?;
        let (text, images) = human_text_from_body(&String::from_utf8_lossy(&body));
        if let Some(query) = query {
            if !fold(&text, request.fold_ascii_case).contains(query) {
                continue;
            }
        }
        let (source_path, app) = catalog
            .get(&header.source_id)
            .map(|entry| (entry.path.clone(), entry.app.clone()))
            .unwrap_or_default();
        accept(Said {
            header,
            source_path,
            app,
            text,
            images,
        });
    }
    Ok(())
}

fn header_matches(header: &EventHeader, request: &SaidRequest) -> bool {
    if let Some(session) = request.session.as_deref() {
        if header.session != session {
            return false;
        }
    }
    if let Some(cwd) = request.cwd.as_deref() {
        if !header
            .cwd
            .to_ascii_lowercase()
            .contains(&cwd.to_ascii_lowercase())
        {
            return false;
        }
    }
    request
        .range
        .contains(&header.timestamp, request.offset_minutes)
}

fn skip<R: Read>(reader: &mut R, length: u64) -> io::Result<()> {
    let mut limited = reader.take(length);
    io::copy(&mut limited, &mut io::sink())?;
    if limited.limit() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "corpus body ended before body_len",
        ));
    }
    Ok(())
}

fn fold(value: &str, fold_ascii_case: bool) -> String {
    if fold_ascii_case {
        value.to_ascii_lowercase()
    } else {
        value.to_string()
    }
}

/// Sources in scope whose file has grown past what the corpus has read.
fn freshness(scope: &[&SourceEntry]) -> (u64, u64) {
    let mut stale = 0u64;
    let mut unscanned = 0u64;
    for entry in scope {
        if let Ok(metadata) = std::fs::metadata(&entry.path) {
            if metadata.len() > entry.committed_byte_end {
                stale += 1;
                unscanned = unscanned.saturating_add(metadata.len() - entry.committed_byte_end);
            }
        }
    }
    (stale, unscanned)
}

fn local_time(timestamp: &str, offset_minutes: i64) -> String {
    crate::time::local_minute(timestamp, offset_minutes)
        .unwrap_or_else(|| timestamp.chars().take(16).collect())
}

fn message_json(said: &Said, text: &str, cut: bool, offset_minutes: i64) -> String {
    let header = &said.header;
    let mut output = String::from("{");
    field(&mut output, "timestamp", &header.timestamp, true);
    field(
        &mut output,
        "local_time",
        &local_time(&header.timestamp, offset_minutes),
        false,
    );
    field(&mut output, "agent", &said.app, false);
    field(&mut output, "via", &header.via, false);
    field(&mut output, "session", &header.session, false);
    field(&mut output, "cwd", &header.cwd, false);
    field(&mut output, "text", text, false);
    number(&mut output, "chars", said.text.chars().count() as u64);
    boolean(&mut output, "text_truncated", cut);
    number(&mut output, "images", said.images as u64);
    output.push_str(&format!(
        ",\"source_ref\":{{\"source_id\":{},\"source_path\":{},\"line\":{},\"byte_start\":{},\"byte_len\":{}}}",
        json_string(&header.source_id),
        json_string(&said.source_path),
        header.line,
        header.byte_start,
        header.byte_len,
    ));
    output.push('}');
    output
}

fn text_output(
    request: &SaidRequest,
    page: &[(Said, String, bool)],
    total: u64,
    next_offset: u64,
    has_more: bool,
    copies: u64,
    stale_sources: u64,
) -> String {
    let mut output = format!(
        "# ebira said: {} of {} messages from the person, {} (copies and imports skipped: {}, stale sources: {})\n",
        page.len(),
        total,
        if request.newest_first {
            "newest first"
        } else {
            "oldest first"
        },
        copies,
        stale_sources,
    );
    for (said, text, cut) in page {
        let header = &said.header;
        output.push_str(&format!(
            "\n[{}] {} {} session={} source-id={} byte={}+{}{}\n{}{}\n",
            local_time(&header.timestamp, request.offset_minutes),
            said.app,
            header.via,
            header.session.chars().take(8).collect::<String>(),
            header.source_id,
            header.byte_start,
            header.byte_len,
            if said.images > 0 {
                format!(" images={}", said.images)
            } else {
                String::new()
            },
            text,
            if *cut {
                format!(
                    "\n…[cut at {} characters; read the rest with: ebira context --source-id {} --byte-start {} --byte-len {}]",
                    text.chars().count(),
                    header.source_id,
                    header.byte_start,
                    header.byte_len
                )
            } else {
                String::new()
            },
        ));
    }
    if has_more {
        output.push_str(&format!("\n# more: --offset {}\n", next_offset));
    }
    output
}

fn field(output: &mut String, key: &str, value: &str, first: bool) {
    if !first {
        output.push(',');
    }
    output.push_str(&format!("\"{}\":{}", key, json_string(value)));
}

fn optional_field(output: &mut String, key: &str, value: Option<&str>, first: bool) {
    if !first {
        output.push(',');
    }
    match value {
        Some(value) => output.push_str(&format!("\"{}\":{}", key, json_string(value))),
        None => output.push_str(&format!("\"{}\":null", key)),
    }
}

fn number(output: &mut String, key: &str, value: u64) {
    output.push_str(&format!(",\"{}\":{}", key, value));
}

fn boolean(output: &mut String, key: &str, value: bool) {
    output.push_str(&format!(",\"{}\":{}", key, value));
}
