//! `ebira said`: the person's own messages, verbatim.
//!
//! Claude Code and Codex logs mark who put each record into the conversation. Records whose
//! sender is the person (typed, pasted, typed while the agent was working, slash commands,
//! answers to an agent's question) are listed with their text and source reference, as
//! `core::persons_message` and `core::SeenMessages` define them.

use crate::core::{human_text_from_body, persons_message, PersonsMessage, SeenMessages, Sender};
use crate::corpus::{self, SourceEntry};
use crate::format::{parse_event_header, EventHeader};
use crate::{json, output};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::time::Instant;

const IO_BUFFER_SIZE: usize = 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct SaidRequest {
    pub session: Option<String>,
    pub source_id: Option<String>,
    /// Case-insensitive substring of the working directory the message was sent in.
    pub cwd: Option<String>,
    /// `claude` or `codex`: the app whose log recorded the message.
    pub app: Option<String>,
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
    let mut scope = corpus::scope(
        &catalog,
        request.source_id.as_deref(),
        request.session.as_deref(),
    );
    if let Some(app) = request.app.as_deref() {
        scope.sources.retain(|entry| entry.app == app);
        scope.whole = false;
    }
    let files = scope.files(root)?;
    let query = request
        .query
        .as_deref()
        .map(|query| fold(query, request.fold_ascii_case));

    let mut found = Vec::new();
    let mut seen = SeenMessages::default();
    let mut copies = 0u64;
    let mut imported = 0u64;
    let mut scanned_bytes = 0u64;
    for path in &files {
        scanned_bytes = scanned_bytes.saturating_add(path.metadata().map(|m| m.len()).unwrap_or(0));
        scan_file(path, &catalog, request, query.as_deref(), &mut |said| {
            let message = persons_message(&said.header.sender, &said.header.via);
            if message == Some(PersonsMessage::Imported) && !request.include_imported {
                imported += 1;
            } else if seen.first(&said.header.timestamp, &said.text) {
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
    let staleness = corpus::Staleness::of(scope.sources.iter().copied());
    let (stale_sources, unscanned_bytes) =
        (staleness.stale_sources, staleness.unscanned_source_bytes);
    let duration_ms = started.elapsed().as_millis() as u64;

    if request.text_format {
        return output::write(&text_output(
            request,
            &page,
            total,
            next_offset,
            has_more,
            copies + imported,
            stale_sources,
        ));
    }
    let filters = json::Object::new()
        .optional("session", request.session.as_deref())
        .optional("source_id", request.source_id.as_deref())
        .optional("cwd", request.cwd.as_deref())
        .optional("app", request.app.as_deref())
        .optional("query", request.query.as_deref())
        .optional("from", request.from.as_deref())
        .optional("to", request.to.as_deref())
        .finish();
    let messages = page
        .iter()
        .map(|(said, text, cut)| message_json(said, text, *cut, request.offset_minutes));
    let output = json::Object::new()
        .name(
            "disposition",
            if total == 0 {
                "no_human_messages"
            } else if has_more {
                "result_page_truncated"
            } else {
                "ready"
            },
        )
        .name("mode", "said")
        .name("order", if request.newest_first { "desc" } else { "asc" })
        .raw("applied_filters", &filters)
        .number("total_messages", total)
        .number("copies_skipped", copies)
        .number("imported_skipped", imported)
        .number("returned", page.len() as u64)
        .number("offset", request.offset)
        .boolean("truncated", has_more)
        .optional_number("next_offset", has_more.then_some(next_offset))
        .number("max_chars", request.max_chars as u64)
        .number("scanned_files", files.len() as u64)
        .number("scanned_bytes", scanned_bytes)
        .number("sources_in_scope", scope.sources.len() as u64)
        .number("stale_sources", stale_sources)
        .number("unscanned_source_bytes", unscanned_bytes)
        .number("duration_ms", duration_ms)
        .raw("messages", &json::array(messages))
        .finish();
    output::write_line(&output)
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
        let wanted = header.sender == human && header_matches(&header, request);
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

fn local_time(timestamp: &str, offset_minutes: i64) -> String {
    crate::time::local_minute(timestamp, offset_minutes)
        .unwrap_or_else(|| timestamp.chars().take(16).collect())
}

fn message_json(said: &Said, text: &str, cut: bool, offset_minutes: i64) -> String {
    let header = &said.header;
    json::Object::new()
        .name("timestamp", &header.timestamp)
        .name("local_time", &local_time(&header.timestamp, offset_minutes))
        .name("app", &said.app)
        .name("via", &header.via)
        .name("session", &header.session)
        .name("cwd", &header.cwd)
        .text("text", text)
        .number("chars", said.text.chars().count() as u64)
        .boolean("text_truncated", cut)
        .number("images", said.images as u64)
        .raw(
            "source_ref",
            &json::source_ref(
                &header.source_id,
                &said.source_path,
                Some(header.line),
                header.byte_start,
                header.byte_len,
            ),
        )
        .finish()
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
            header.session,
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
