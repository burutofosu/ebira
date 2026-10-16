//! `ebira said`: the person's own messages, verbatim.
//!
//! Claude Code and Codex logs mark who put each record into the conversation. Records whose
//! sender is the person (typed, pasted, typed while the agent was working, slash commands,
//! answers to an agent's question) are listed with their text and source reference, as
//! `core::persons_message` and `core::SeenMessages` define them.

use crate::core::{human_text_from_body, persons_message, PersonsMessage, SeenMessages, Sender};
use crate::corpus::{self, SourceEntry};
use crate::format::{self, parse_event_header, EventHeader};
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
    /// The texts to look for. Each gets the list a request for it alone would give, from one
    /// pass over the corpus; without any, every message is listed.
    pub queries: Vec<String>,
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

#[derive(Clone, Debug)]
pub(crate) struct Said {
    pub(crate) header: EventHeader,
    pub(crate) source_path: String,
    /// `claude`, `codex`, or `other`, from the catalog.
    pub(crate) app: String,
    pub(crate) text: String,
    pub(crate) images: usize,
}

/// The person's messages that hold one query (or all of them, without a query), with the
/// copies and imports left out of the list.
#[derive(Default)]
pub(crate) struct SaidList {
    /// In the order the request asks for.
    pub(crate) messages: Vec<Said>,
    pub(crate) copies: u64,
    pub(crate) imported: u64,
    seen: SeenMessages,
}

impl SaidList {
    fn add(&mut self, said: Said, include_imported: bool) {
        let message = persons_message(&said.header.sender, &said.header.via);
        if message == Some(PersonsMessage::Imported) && !include_imported {
            self.imported += 1;
        } else if self.seen.first(
            &said.header.timestamp,
            &said.text,
            &said.header.via,
            &said.header.call_id,
        ) {
            self.messages.push(said);
        } else {
            self.copies += 1;
        }
    }
}

/// What one pass over the corpus found: a list per query (one list without a query), and what
/// the pass read.
pub(crate) struct SaidFound {
    pub(crate) lists: Vec<SaidList>,
    scanned_files: u64,
    scanned_bytes: u64,
    sources_in_scope: u64,
    pub(crate) stale_sources: u64,
    pub(crate) unscanned_bytes: u64,
}

pub fn run(root: &Path, request: &SaidRequest) -> io::Result<()> {
    if request.limit == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--limit must be at least 1",
        ));
    }
    let started = Instant::now();
    let found = find(root, request)?;
    let duration_ms = started.elapsed().as_millis() as u64;
    if request.queries.len() > 1 {
        return if request.text_format {
            output::write(&many_text(request, &found))
        } else {
            output::write_line(&many_json(request, &found, duration_ms))
        };
    }

    let list = &found.lists[0];
    let page = Page::of(list, request);
    if request.text_format {
        let mut text = text_header(
            request,
            &page,
            list.copies + list.imported,
            found.stale_sources,
        );
        text.push_str(&text_messages(request, &page));
        return output::write(&text);
    }
    let filters = json::Object::new()
        .optional("session", request.session.as_deref())
        .optional("source_id", request.source_id.as_deref())
        .optional("cwd", request.cwd.as_deref())
        .optional("app", request.app.as_deref())
        .optional("query", request.queries.first().map(String::as_str))
        .optional("from", request.from.as_deref())
        .optional("to", request.to.as_deref())
        .finish();
    let output = json::Object::new()
        .name("disposition", page.disposition())
        .name("mode", "said")
        .name("order", if request.newest_first { "desc" } else { "asc" })
        .raw("applied_filters", &filters)
        .number("total_messages", page.total)
        .number("copies_skipped", list.copies)
        .number("imported_skipped", list.imported)
        .number("returned", page.entries.len() as u64)
        .number("offset", request.offset)
        .boolean("truncated", page.has_more)
        .optional_number("next_offset", page.has_more.then_some(page.next_offset))
        .number("max_chars", request.max_chars as u64)
        .number("scanned_files", found.scanned_files)
        .number("scanned_bytes", found.scanned_bytes)
        .number("sources_in_scope", found.sources_in_scope)
        .number("stale_sources", found.stale_sources)
        .number("unscanned_source_bytes", found.unscanned_bytes)
        .number("duration_ms", duration_ms)
        .raw("messages", &page.messages_json(request.offset_minutes))
        .finish();
    output::write_line(&output)
}

/// The person's messages for each of the request's queries, from one pass over the corpus.
pub(crate) fn find(root: &Path, request: &SaidRequest) -> io::Result<SaidFound> {
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
    let queries = request
        .queries
        .iter()
        .map(|query| fold(query, request.fold_ascii_case))
        .collect::<Vec<_>>();
    let mut lists = (0..queries.len().max(1))
        .map(|_| SaidList::default())
        .collect::<Vec<_>>();
    let mut scanned_bytes = 0u64;
    for path in &files {
        scanned_bytes = scanned_bytes.saturating_add(path.metadata().map(|m| m.len()).unwrap_or(0));
        scan_file(path, &catalog, request, &queries, &mut |said, matched| {
            if let Some((last, rest)) = matched.split_last() {
                for &index in rest {
                    lists[index].add(said.clone(), request.include_imported);
                }
                lists[*last].add(said, request.include_imported);
            }
        })?;
    }
    for list in &mut lists {
        list.messages.sort_by(|left, right| {
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
    }
    let staleness = corpus::Staleness::of(scope.sources.iter().copied());
    Ok(SaidFound {
        lists,
        scanned_files: files.len() as u64,
        scanned_bytes,
        sources_in_scope: scope.sources.len() as u64,
        stale_sources: staleness.stale_sources,
        unscanned_bytes: staleness.unscanned_source_bytes,
    })
}

/// One page of a list: the messages from the request's offset, within its limit and its
/// character budget, each with the text shown and whether it was cut.
struct Page<'a> {
    entries: Vec<(&'a Said, String, bool)>,
    total: u64,
    next_offset: u64,
    has_more: bool,
}

impl<'a> Page<'a> {
    fn of(list: &'a SaidList, request: &SaidRequest) -> Page<'a> {
        let total = list.messages.len() as u64;
        let start = usize::try_from(request.offset)
            .unwrap_or(usize::MAX)
            .min(list.messages.len());
        let mut budget = request.max_chars;
        let mut entries = Vec::new();
        for said in &list.messages[start..] {
            if entries.len() == request.limit {
                break;
            }
            let length = said.text.chars().count();
            if !entries.is_empty() && length > budget {
                break;
            }
            let cut = length > budget;
            let text = if cut {
                said.text.chars().take(budget).collect()
            } else {
                said.text.clone()
            };
            budget = budget.saturating_sub(length);
            entries.push((said, text, cut));
        }
        let next_offset = request.offset.saturating_add(entries.len() as u64);
        Page {
            has_more: next_offset < total,
            entries,
            total,
            next_offset,
        }
    }

    fn disposition(&self) -> &'static str {
        if self.total == 0 {
            "no_human_messages"
        } else if self.has_more {
            "result_page_truncated"
        } else {
            "ready"
        }
    }

    fn messages_json(&self, offset_minutes: i64) -> String {
        json::array(
            self.entries
                .iter()
                .map(|(said, text, cut)| message_json(said, text, *cut, offset_minutes)),
        )
    }
}

/// The result of several queries: what the pass read once, then one entry per query with the
/// page a request for it alone would give.
fn many_json(request: &SaidRequest, found: &SaidFound, duration_ms: u64) -> String {
    let filters = json::Object::new()
        .optional("session", request.session.as_deref())
        .optional("source_id", request.source_id.as_deref())
        .optional("cwd", request.cwd.as_deref())
        .optional("app", request.app.as_deref())
        .optional("from", request.from.as_deref())
        .optional("to", request.to.as_deref())
        .finish();
    let pages = found
        .lists
        .iter()
        .map(|list| Page::of(list, request))
        .collect::<Vec<_>>();
    let results = request
        .queries
        .iter()
        .zip(found.lists.iter().zip(&pages))
        .map(|(query, (list, page))| {
            json::Object::new()
                .text("query", query)
                .name("disposition", page.disposition())
                .number("total_messages", page.total)
                .number("copies_skipped", list.copies)
                .number("imported_skipped", list.imported)
                .number("returned", page.entries.len() as u64)
                .number("offset", request.offset)
                .boolean("truncated", page.has_more)
                .optional_number("next_offset", page.has_more.then_some(page.next_offset))
                .raw("messages", &page.messages_json(request.offset_minutes))
                .finish()
        });
    let disposition = if pages.iter().all(|page| page.total == 0) {
        "no_human_messages"
    } else if pages.iter().any(|page| page.has_more) {
        "result_page_truncated"
    } else {
        "ready"
    };
    json::Object::new()
        .name("disposition", disposition)
        .name("mode", "said")
        .name("order", if request.newest_first { "desc" } else { "asc" })
        .raw(
            "queries",
            &json::strings(request.queries.iter().map(String::as_str)),
        )
        .raw("applied_filters", &filters)
        .number("max_chars", request.max_chars as u64)
        .number("scanned_files", found.scanned_files)
        .number("scanned_bytes", found.scanned_bytes)
        .number("sources_in_scope", found.sources_in_scope)
        .number("stale_sources", found.stale_sources)
        .number("unscanned_source_bytes", found.unscanned_bytes)
        .number("duration_ms", duration_ms)
        .raw("results", &json::array(results))
        .finish()
}

fn many_text(request: &SaidRequest, found: &SaidFound) -> String {
    let mut output = format!(
        "# ebira said: {} queries, {} (stale sources: {})\n",
        request.queries.len(),
        if request.newest_first {
            "newest first"
        } else {
            "oldest first"
        },
        found.stale_sources,
    );
    for (query, list) in request.queries.iter().zip(&found.lists) {
        let page = Page::of(list, request);
        output.push_str(&format!(
            "\n## {}: {} of {} messages (copies and imports skipped: {})\n",
            json::string(query),
            page.entries.len(),
            page.total,
            list.copies + list.imported,
        ));
        output.push_str(&text_messages(request, &page));
    }
    output
}

/// Reads one segment file. Each message of the person that passes the filters is handed to
/// `accept` with the positions of the queries (folded already) its text holds; without
/// queries, every such message is handed over, at position 0.
fn scan_file(
    path: &Path,
    catalog: &BTreeMap<String, SourceEntry>,
    request: &SaidRequest,
    queries: &[String],
    accept: &mut dyn FnMut(Said, &[usize]),
) -> io::Result<()> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(IO_BUFFER_SIZE, file);
    let mut header_line = Vec::new();
    let mut matched = Vec::with_capacity(queries.len().max(1));
    let human = Sender::Human.as_str();
    loop {
        header_line.clear();
        if reader.read_until(b'\n', &mut header_line)? == 0 {
            break;
        }
        // Most events are not the person's: they are passed over without parsing the header.
        if format::header_sender(&header_line) != Some(human.as_bytes()) {
            let body_len = format::header_body_len(&header_line).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {}", path.display(), error),
                )
            })?;
            skip(&mut reader, body_len.saturating_add(1))?;
            continue;
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
        matched.clear();
        if queries.is_empty() {
            matched.push(0);
        } else {
            let folded = fold(&text, request.fold_ascii_case);
            matched.extend(
                queries
                    .iter()
                    .enumerate()
                    .filter(|(_, query)| folded.contains(query.as_str()))
                    .map(|(index, _)| index),
            );
            if matched.is_empty() {
                continue;
            }
        }
        let (source_path, app) = catalog
            .get(&header.source_id)
            .map(|entry| (entry.path.clone(), entry.app.clone()))
            .unwrap_or_default();
        accept(
            Said {
                header,
                source_path,
                app,
                text,
                images,
            },
            &matched,
        );
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

fn text_header(request: &SaidRequest, page: &Page, skipped: u64, stale_sources: u64) -> String {
    format!(
        "# ebira said: {} of {} messages from the person, {} (copies and imports skipped: {}, stale sources: {})\n",
        page.entries.len(),
        page.total,
        if request.newest_first {
            "newest first"
        } else {
            "oldest first"
        },
        skipped,
        stale_sources,
    )
}

fn text_messages(request: &SaidRequest, page: &Page) -> String {
    let mut output = String::new();
    for (said, text, cut) in &page.entries {
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
    if page.has_more {
        output.push_str(&format!("\n# more: --offset {}\n", page.next_offset));
    }
    output
}
