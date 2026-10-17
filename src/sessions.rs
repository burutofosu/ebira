//! `sessions`: the conversations the corpus holds, one entry for each session, with when it
//! ran, where, how much the person said in it, and how it began and ended. A Claude Code
//! subagent's transcript and a Codex child thread record their parent's session, so they are
//! counted in it. The person's messages are counted as `said` lists them: a copy of a message
//! and an imported copy are not counted again.

use crate::core::{
    human_text_from_body, is_compaction, persons_message, PersonsMessage, SeenMessages, Sender,
};
use crate::corpus::{self, SourceEntry};
use crate::format::{parse_event_header, EventHeader};
use crate::time::{self, Range};
use crate::{json, output};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::thread;

const IO_BUFFER_SIZE: usize = 1024 * 1024;
/// Characters of a first or last message shown in text.
const TEXT_PREVIEW_CHARS: usize = 160;
/// Characters of a first or last message given in JSON.
const JSON_PREVIEW_CHARS: usize = 600;

pub struct SessionsRequest {
    pub app: Option<String>,
    /// Sessions with a working directory that holds this text, without ASCII case.
    pub cwd: Option<String>,
    /// Sessions in which the person wrote this text.
    pub query: Option<String>,
    pub fold_ascii_case: bool,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Only the records in this time are counted; a session with none is left out.
    pub range: Range,
    pub newest_first: bool,
    /// Also the sessions in which the person wrote nothing.
    pub all: bool,
    pub limit: usize,
    pub offset: u64,
    pub text_format: bool,
    pub offset_minutes: i64,
}

/// A message of the person's, kept for the first and the last of a session.
#[derive(Clone)]
struct Message {
    instant: Option<i128>,
    timestamp: String,
    text: String,
    source_id: String,
    source_path: String,
    line: u64,
    byte_start: u64,
    byte_len: u64,
}

#[derive(Default)]
struct Session {
    id: String,
    app: String,
    /// Each working directory, with the earliest time it was seen.
    cwds: BTreeMap<String, Option<i128>>,
    sources: BTreeSet<String>,
    started: Option<(i128, String)>,
    ended: Option<(i128, String)>,
    records: u64,
    agent_records: u64,
    compactions: u64,
    person_messages: u64,
    person_chars: u64,
    matched_messages: u64,
    first_message: Option<Message>,
    last_message: Option<Message>,
}

impl Session {
    fn observe(&mut self, header: &EventHeader, entry: Option<&SourceEntry>, offset_minutes: i64) {
        self.records += 1;
        if header.sender == Sender::Assistant.as_str() {
            self.agent_records += 1;
        }
        if is_compaction(&header.event_type) {
            self.compactions += 1;
        }
        if self.app.is_empty() {
            if let Some(entry) = entry {
                self.app = entry.app.clone();
            }
        }
        self.sources.insert(header.source_id.clone());
        let instant = time::instant(&header.timestamp, offset_minutes);
        if !header.cwd.is_empty() {
            let seen = self.cwds.entry(header.cwd.clone()).or_insert(instant);
            if instant.is_some() && (seen.is_none() || instant < *seen) {
                *seen = instant;
            }
        }
        if let Some(at) = instant {
            if self.started.as_ref().is_none_or(|(first, _)| at < *first) {
                self.started = Some((at, header.timestamp.clone()));
            }
            if self.ended.as_ref().is_none_or(|(last, _)| at > *last) {
                self.ended = Some((at, header.timestamp.clone()));
            }
        }
    }

    /// Takes in what another file holds of the same session, the person's messages aside.
    fn absorb(&mut self, other: Session) {
        if self.app.is_empty() {
            self.app = other.app;
        }
        for (cwd, at) in other.cwds {
            let seen = self.cwds.entry(cwd).or_insert(at);
            if at.is_some() && (seen.is_none() || at < *seen) {
                *seen = at;
            }
        }
        self.sources.extend(other.sources);
        if let Some((at, timestamp)) = other.started {
            if self.started.as_ref().is_none_or(|(first, _)| at < *first) {
                self.started = Some((at, timestamp));
            }
        }
        if let Some((at, timestamp)) = other.ended {
            if self.ended.as_ref().is_none_or(|(last, _)| at > *last) {
                self.ended = Some((at, timestamp));
            }
        }
        self.records += other.records;
        self.agent_records += other.agent_records;
        self.compactions += other.compactions;
    }

    fn person(&mut self, message: Message, matched: bool) {
        self.person_messages += 1;
        self.person_chars += message.text.chars().count() as u64;
        self.matched_messages += u64::from(matched);
        let earlier = |kept: &Option<Message>| {
            kept.as_ref()
                .is_none_or(|kept| order_key(&message) < order_key(kept))
        };
        if earlier(&self.first_message) {
            self.first_message = Some(message.clone());
        }
        let later = self
            .last_message
            .as_ref()
            .is_none_or(|kept| order_key(&message) >= order_key(kept));
        if later {
            self.last_message = Some(message);
        }
    }

    /// Where the session worked, in the order first seen: each working directory as a path
    /// (Codex also writes them as `file:` URLs), without those inside another one listed,
    /// which an agent changed into along the way.
    fn places(&self) -> Vec<String> {
        let mut cwds = self.cwds.iter().collect::<Vec<_>>();
        cwds.sort_by_key(|(cwd, at)| (at.is_none(), **at, cwd.as_str()));
        let mut places: Vec<String> = Vec::new();
        for (cwd, _) in cwds {
            let place = as_path(cwd);
            if !places.iter().any(|kept| same_path(kept, &place)) {
                places.push(place);
            }
        }
        places
            .iter()
            .filter(|place| !places.iter().any(|other| inside(place, other)))
            .cloned()
            .collect()
    }

    /// Whether a working directory of the session, as written or as a path, holds `wanted`
    /// (lowercase), without ASCII case.
    fn worked_in(&self, wanted: &str) -> bool {
        self.cwds.keys().any(|cwd| {
            cwd.to_ascii_lowercase().contains(wanted)
                || as_path(cwd).to_ascii_lowercase().contains(wanted)
        })
    }
}

/// A working directory as a path: `file:///C:/work` is `C:\work`.
fn as_path(cwd: &str) -> String {
    let Some(rest) = cwd.strip_prefix("file://") else {
        return cwd.to_string();
    };
    let rest = percent_decoded(rest);
    let bytes = rest.as_bytes();
    // `/C:/work` names a Windows drive.
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return rest[1..].replace('/', "\\");
    }
    rest
}

fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |at: usize| bytes.get(at).and_then(|byte| (*byte as char).to_digit(16));
        if bytes[index] == b'%' {
            if let (Some(high), Some(low)) = (hex(index + 1), hex(index + 2)) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Paths compared as Windows and the logs write them: without ASCII case, either separator.
fn comparable(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn same_path(left: &str, right: &str) -> bool {
    comparable(left) == comparable(right)
}

/// Whether `path` lies inside `other`.
fn inside(path: &str, other: &str) -> bool {
    let (path, other) = (comparable(path), comparable(other));
    path.len() > other.len() && path.starts_with(&other) && path[other.len()..].starts_with('/')
}

/// Dated messages in time order, undated ones after them.
fn order_key(message: &Message) -> (bool, i128, u64) {
    (
        message.instant.is_none(),
        message.instant.unwrap_or(0),
        message.byte_start,
    )
}

pub fn run(root: &Path, request: &SessionsRequest) -> io::Result<()> {
    if request.limit == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--limit must be at least 1",
        ));
    }
    let catalog = corpus::source_catalog(root)?;
    let mut scope = corpus::scope(&catalog, None, None);
    if let Some(app) = request.app.as_deref() {
        scope.sources.retain(|entry| entry.app == app);
        scope.whole = false;
    }
    let files = scope.files(root)?;
    let in_scope = scope
        .sources
        .iter()
        .map(|entry| entry.source_id.as_str())
        .collect::<BTreeSet<_>>();
    let query = request
        .query
        .as_deref()
        .map(|query| fold(query, request.fold_ascii_case));
    // The files are read on several threads; their results are joined in file order, so a
    // message stored twice is counted where a reading in order would count it.
    let workers = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .clamp(1, 16);
    let chunk = files.len().div_ceil(workers).max(1);
    let scans = thread::scope(|scope| {
        let handles = files
            .chunks(chunk)
            .map(|part| {
                let catalog = &catalog;
                let in_scope = &in_scope;
                scope.spawn(move || {
                    part.iter()
                        .map(|path| scan_file(path, catalog, in_scope, request))
                        .collect::<io::Result<Vec<_>>>()
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("sessions worker"))
            .collect::<io::Result<Vec<_>>>()
    })?;
    let mut sessions = BTreeMap::<String, Session>::new();
    let mut seen = SeenMessages::default();
    for scan in scans.into_iter().flatten() {
        for (id, partial) in scan.sessions {
            sessions
                .entry(id.clone())
                .or_insert_with(|| Session {
                    id,
                    ..Session::default()
                })
                .absorb(partial);
        }
        for found in scan.persons {
            if !seen.first(
                &found.message.timestamp,
                &found.message.text,
                &found.via,
                &found.call_id,
            ) {
                continue;
            }
            let matched = query.as_deref().is_some_and(|query| {
                fold(&found.message.text, request.fold_ascii_case).contains(query)
            });
            if let Some(session) = sessions.get_mut(&found.session) {
                session.person(found.message, matched);
            }
        }
    }
    let cwd = request.cwd.as_deref().map(str::to_ascii_lowercase);
    let mut list = sessions
        .into_values()
        .filter(|session| {
            cwd.as_deref()
                .is_none_or(|wanted| session.worked_in(wanted))
        })
        .filter(|session| query.is_none() || session.matched_messages > 0)
        .collect::<Vec<_>>();
    // Threads another program started, in which the person wrote nothing, are left out unless
    // asked for, and counted.
    let before = list.len();
    if !request.all {
        list.retain(|session| session.person_messages > 0);
    }
    let left_out = (before - list.len()) as u64;
    list.sort_by(|left, right| {
        let key = |session: &Session| session.started.as_ref().map(|(at, _)| *at);
        let ordering = match (key(left), key(right)) {
            (Some(left_at), Some(right_at)) => {
                let by_time = left_at.cmp(&right_at);
                if request.newest_first {
                    by_time.reverse()
                } else {
                    by_time
                }
            }
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        ordering.then_with(|| left.id.cmp(&right.id))
    });
    let total = list.len() as u64;
    let start = usize::try_from(request.offset)
        .unwrap_or(usize::MAX)
        .min(list.len());
    let page = &list[start..start.saturating_add(request.limit).min(list.len())];
    let next_offset = request.offset.saturating_add(page.len() as u64);
    let has_more = next_offset < total;
    let staleness = corpus::Staleness::of(scope.sources.iter().copied());
    if request.text_format {
        return output::write(&text(
            request,
            page,
            total,
            left_out,
            has_more,
            next_offset,
            &staleness,
        ));
    }
    let filters = json::Object::new()
        .optional("app", request.app.as_deref())
        .optional("cwd", request.cwd.as_deref())
        .optional("query", request.query.as_deref())
        .optional("from", request.from.as_deref())
        .optional("to", request.to.as_deref())
        .boolean("all", request.all)
        .finish();
    output::write_line(
        &json::Object::new()
            .name(
                "disposition",
                if total == 0 {
                    "no_sessions"
                } else if has_more {
                    "result_page_truncated"
                } else {
                    "ready"
                },
            )
            .name("mode", "sessions")
            .name("order", if request.newest_first { "desc" } else { "asc" })
            .raw("applied_filters", &filters)
            .number("total_sessions", total)
            .number("returned", page.len() as u64)
            .number("offset", request.offset)
            .boolean("truncated", has_more)
            .optional_number("next_offset", has_more.then_some(next_offset))
            .number("left_out_without_person_messages", left_out)
            .name("local_time_zone", &time::zone(request.offset_minutes))
            .number("sources_in_scope", scope.sources.len() as u64)
            .number("stale_sources", staleness.stale_sources)
            .number("unscanned_source_bytes", staleness.unscanned_source_bytes)
            .raw(
                "sessions",
                &json::array(
                    page.iter()
                        .map(|session| session_json(session, request.offset_minutes)),
                ),
            )
            .finish(),
    )
}

/// What one segment file holds of each session, and the person's messages in it, in order,
/// before copies of a message are told apart.
#[derive(Default)]
struct FileScan {
    sessions: BTreeMap<String, Session>,
    persons: Vec<Found>,
}

struct Found {
    session: String,
    message: Message,
    via: String,
    call_id: String,
}

/// Reads the records of one segment file into the sessions they belong to.
fn scan_file(
    path: &Path,
    catalog: &BTreeMap<String, SourceEntry>,
    in_scope: &BTreeSet<&str>,
    request: &SessionsRequest,
) -> io::Result<FileScan> {
    let mut scan = FileScan::default();
    let sessions = &mut scan.sessions;
    let mut reader = BufReader::with_capacity(IO_BUFFER_SIZE, File::open(path)?);
    let mut header_line = Vec::new();
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
        if !in_scope.contains(header.source_id.as_str())
            || !request
                .range
                .contains(&header.timestamp, request.offset_minutes)
        {
            skip(&mut reader, header.body_len + 1)?;
            continue;
        }
        let entry = catalog.get(&header.source_id);
        let session = sessions
            .entry(header.session.clone())
            .or_insert_with(|| Session {
                id: header.session.clone(),
                ..Session::default()
            });
        session.observe(&header, entry, request.offset_minutes);
        if persons_message(&header.sender, &header.via) != Some(PersonsMessage::Own) {
            skip(&mut reader, header.body_len + 1)?;
            continue;
        }
        let mut body = vec![0; header.body_len as usize];
        reader.read_exact(&mut body)?;
        skip(&mut reader, 1)?;
        let (text, _) = human_text_from_body(&String::from_utf8_lossy(&body));
        scan.persons.push(Found {
            session: header.session.clone(),
            message: Message {
                instant: time::instant(&header.timestamp, request.offset_minutes),
                timestamp: header.timestamp.clone(),
                text,
                source_id: header.source_id.clone(),
                source_path: entry.map(|entry| entry.path.clone()).unwrap_or_default(),
                line: header.line,
                byte_start: header.byte_start,
                byte_len: header.byte_len,
            },
            via: header.via,
            call_id: header.call_id,
        });
    }
    Ok(scan)
}

fn session_json(session: &Session, offset_minutes: i64) -> String {
    let time =
        |kept: &Option<(i128, String)>| kept.as_ref().map(|(_, timestamp)| timestamp.clone());
    let started = time(&session.started);
    let ended = time(&session.ended);
    let local = |timestamp: &Option<String>| {
        timestamp
            .as_deref()
            .map(|timestamp| time::shown_minute(timestamp, offset_minutes))
    };
    json::Object::new()
        .name("session", &session.id)
        .name("app", &session.app)
        .optional("started", started.as_deref())
        .optional("started_local", local(&started).as_deref())
        .optional("ended", ended.as_deref())
        .optional("ended_local", local(&ended).as_deref())
        .raw(
            "places",
            &json::strings(session.places().iter().map(String::as_str)),
        )
        .number("person_messages", session.person_messages)
        .number("person_chars", session.person_chars)
        .number("matched_messages", session.matched_messages)
        .number("agent_records", session.agent_records)
        .number("records", session.records)
        .number("compactions", session.compactions)
        .raw(
            "sources",
            &json::strings(session.sources.iter().map(String::as_str)),
        )
        .raw(
            "first_message",
            &message_json(session.first_message.as_ref(), offset_minutes),
        )
        .raw(
            "last_message",
            &message_json(session.last_message.as_ref(), offset_minutes),
        )
        .finish()
}

fn message_json(message: Option<&Message>, offset_minutes: i64) -> String {
    let Some(message) = message else {
        return "null".to_string();
    };
    let (text, cut) = clip(&message.text, JSON_PREVIEW_CHARS, false);
    json::Object::new()
        .name("timestamp", &message.timestamp)
        .name(
            "local_time",
            &time::shown_minute(&message.timestamp, offset_minutes),
        )
        .text("text", &text)
        .boolean("text_truncated", cut)
        .raw(
            "source_ref",
            &json::source_ref(
                &message.source_id,
                &message.source_path,
                Some(message.line),
                message.byte_start,
                message.byte_len,
            ),
        )
        .finish()
}

fn text(
    request: &SessionsRequest,
    page: &[Session],
    total: u64,
    left_out: u64,
    has_more: bool,
    next_offset: u64,
    staleness: &corpus::Staleness,
) -> String {
    let mut out = format!(
        "# ebira sessions: {} of {} sessions, {}, times in {} (stale sources: {}{})\n",
        page.len(),
        total,
        if request.newest_first {
            "newest first"
        } else {
            "oldest first"
        },
        time::zone(request.offset_minutes),
        staleness.stale_sources,
        if left_out > 0 {
            format!("; {left_out} without a message from the person left out, --all lists them")
        } else {
            String::new()
        },
    );
    let shown = |kept: &Option<(i128, String)>| {
        kept.as_ref()
            .map(|(_, timestamp)| time::shown_minute(timestamp, request.offset_minutes))
            .unwrap_or_else(|| "undated".to_string())
    };
    for session in page {
        out.push_str(&format!(
            "\n[{} → {}] {} session={}\n",
            shown(&session.started),
            shown(&session.ended),
            if session.app.is_empty() {
                "?"
            } else {
                &session.app
            },
            session.id,
        ));
        let places = session.places();
        if !places.is_empty() {
            out.push_str(&format!("  in: {}\n", places.join(", ")));
        }
        out.push_str(&format!(
            "  {} from the person ({}){}, {}, {}, {}\n",
            counted(session.person_messages, "message"),
            counted(session.person_chars, "character"),
            if request.query.is_some() {
                format!(", {} holding the query", session.matched_messages)
            } else {
                String::new()
            },
            counted(session.agent_records, "agent record"),
            counted(session.compactions, "compaction"),
            counted(session.sources.len() as u64, "log"),
        ));
        for (label, message) in [
            ("first", &session.first_message),
            ("last", &session.last_message),
        ] {
            if let Some(message) = message {
                let (text, _) = clip(&message.text, TEXT_PREVIEW_CHARS, true);
                out.push_str(&format!(
                    "  {label} [{}]: {text} (source-id={} byte={}+{})\n",
                    time::shown_minute(&message.timestamp, request.offset_minutes),
                    message.source_id,
                    message.byte_start,
                    message.byte_len,
                ));
            }
        }
    }
    if has_more {
        out.push_str(&format!("\n# more: --offset {next_offset}\n"));
    }
    out
}

/// `1 message`, `2 messages`.
fn counted(count: u64, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// The first `limit` characters, on one line when `one_line`, and whether the text was cut.
fn clip(text: &str, limit: usize, one_line: bool) -> (String, bool) {
    let text = if one_line {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        text.trim().to_string()
    };
    if text.chars().count() <= limit {
        return (text, false);
    }
    let mut clipped = text.chars().take(limit).collect::<String>();
    clipped.push('…');
    (clipped, true)
}

fn fold(value: &str, fold_ascii_case: bool) -> String {
    if fold_ascii_case {
        value.to_ascii_lowercase()
    } else {
        value.to_string()
    }
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
