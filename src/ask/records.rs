//! The records `ask` reads. The person's messages and the agents' replies that hold the search
//! strings come from one pass of `said` and one of `search` over the corpus, for all the strings
//! at once; the records around each are read from the corpus too, with Ebira's own reading of
//! who wrote them.

use super::text;
use crate::core::{human_text_from_body, Sender, IMPORTED_TURN_PREFIX};
use crate::corpus;
use crate::format::{self, EventHeader};
use crate::said::{self, SaidRequest};
use crate::search::{self, SearchRequest};
use crate::time::{self, Range};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct Filters {
    pub cwd: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceRef {
    pub source_id: String,
    pub byte_start: u64,
    pub byte_len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    Person,
    Agent,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub source_ref: SourceRef,
    pub source_path: String,
    pub line: u64,
    pub session: String,
    /// As recorded.
    pub timestamp: String,
    pub who: Who,
    /// `claude`, `codex`, or `other`.
    pub app: String,
    /// The last two parts of the working directory.
    pub project: String,
    /// The person's whole message, or the search snippet of an agent's record.
    pub text: String,
    pub images: usize,
    /// The search strings that found it.
    pub terms: Vec<String>,
    /// Why it was added when the search itself did not add it: a later message found by a
    /// reversal check, with what it may change.
    pub note: String,
}

/// What one search string found: the newest records (or the oldest, as asked), and how many
/// records hold it in all.
pub struct Found {
    pub hits: Vec<Hit>,
    pub total: u64,
}

/// A bounded tail of a related session's own human messages, independent of search terms.
pub struct Recent {
    pub session: String,
    pub total: usize,
    pub stale_sources: u64,
    pub unscanned_bytes: u64,
    pub hits: Vec<Hit>,
}

/// Who wrote a line around a record, as the log marks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Label {
    /// The person's own message.
    Person,
    /// The user turn, written by someone else: another agent's prompt, or unmarked text.
    UserTurn,
    Agent,
    Tool,
    Result,
    System,
    Summary,
    /// A record of a shape not read here; shown only when it is the record found.
    Record,
}

impl Label {
    /// How the evidence shows the line to the answer model.
    pub fn shown(self) -> &'static str {
        match self {
            Label::Person => "person (context)",
            Label::UserTurn => "user turn",
            Label::Agent => "agent",
            Label::Tool => "tool call",
            Label::Result => "tool result",
            Label::System => "system",
            Label::Summary => "summary",
            Label::Record => "record",
        }
    }

    /// How results name the writer.
    pub fn id(self) -> &'static str {
        match self {
            Label::Person => "person",
            Label::UserTurn => "agent_prompt",
            Label::Agent => "agent",
            Label::Tool => "tool_call",
            Label::Result => "tool_result",
            Label::System => "system",
            Label::Summary => "summary",
            Label::Record => "record",
        }
    }
}

/// One readable record near a hit, with its place in the log.
#[derive(Clone, Debug)]
pub struct Line {
    pub label: Label,
    pub timestamp: String,
    pub text: String,
    pub is_hit: bool,
    pub byte_start: u64,
    pub byte_len: u64,
}

/// The corpus `ask` reads, with its date offset. Each read holds the corpus lock only while it
/// reads, so a sync is not kept waiting through the model calls.
pub struct Corpus {
    root: PathBuf,
    offset_minutes: i64,
}

impl Corpus {
    pub fn open(root: &Path) -> io::Result<Corpus> {
        let _lock = corpus::read_lock(root)?;
        Ok(Corpus {
            root: root.to_path_buf(),
            offset_minutes: corpus::corpus_offset_minutes(root)?,
        })
    }

    #[cfg(test)]
    pub fn for_tests(offset_minutes: i64) -> Corpus {
        Corpus {
            root: PathBuf::new(),
            offset_minutes,
        }
    }

    /// A timestamp as `yyyy-mm-dd hh:mm` at the corpus offset, or as written.
    pub fn local(&self, timestamp: &str) -> String {
        time::local_minute(timestamp, self.offset_minutes).unwrap_or_else(|| timestamp.to_string())
    }

    /// The zone of the local times: `UTC+09:00`.
    pub fn zone(&self) -> String {
        time::zone(self.offset_minutes)
    }

    pub fn local_date(&self, timestamp: &str) -> Option<String> {
        time::local_date(timestamp, self.offset_minutes)
    }

    pub fn today(&self) -> String {
        let now = super::usage::now_ms();
        time::rfc3339_from_unix_ms(now, self.offset_minutes)
            .chars()
            .take(10)
            .collect()
    }

    pub fn instant(&self, timestamp: &str) -> Option<i128> {
        time::instant(timestamp, self.offset_minutes)
    }

    fn range(&self, filters: &Filters) -> io::Result<Range> {
        Range::parse(
            filters.from.as_deref(),
            filters.to.as_deref(),
            self.offset_minutes,
        )
    }

    /// The person's messages that hold each term, at most `limit` per term, newest first or
    /// oldest first: one pass over the corpus for all the terms.
    pub fn person(
        &self,
        terms: &[String],
        filters: &Filters,
        limit: usize,
        newest_first: bool,
    ) -> io::Result<Vec<Found>> {
        let _lock = corpus::read_lock(&self.root)?;
        let request = SaidRequest {
            cwd: filters.cwd.clone(),
            queries: terms.to_vec(),
            fold_ascii_case: true,
            from: filters.from.clone(),
            to: filters.to.clone(),
            range: self.range(filters)?,
            limit,
            newest_first,
            offset_minutes: self.offset_minutes,
            ..SaidRequest::default()
        };
        let found = said::find(&self.root, &request)?;
        Ok(terms
            .iter()
            .zip(found.lists)
            .map(|(term, list)| Found {
                total: list.messages.len() as u64,
                hits: list
                    .messages
                    .into_iter()
                    .take(limit)
                    .map(|said| Hit {
                        source_ref: SourceRef {
                            source_id: said.header.source_id.clone(),
                            byte_start: said.header.byte_start,
                            byte_len: said.header.byte_len,
                        },
                        source_path: said.source_path,
                        line: said.header.line,
                        session: said.header.session.clone(),
                        project: text::project_from_cwd(&said.header.cwd),
                        timestamp: said.header.timestamp,
                        who: Who::Person,
                        app: said.app,
                        text: said.text,
                        images: said.images,
                        terms: vec![term.clone()],
                        note: String::new(),
                    })
                    .collect(),
            })
            .collect())
    }

    /// The agents' replies that hold each term, newest first, at most `limit` per term, and the
    /// time through which the corpus has read the logs: one pass for all the terms.
    pub fn agents(
        &self,
        terms: &[String],
        filters: &Filters,
        limit: usize,
    ) -> io::Result<(Vec<Found>, Option<String>)> {
        let _lock = corpus::read_lock(&self.root)?;
        let request = SearchRequest {
            query: terms.first().cloned().unwrap_or_default(),
            limit,
            cwd: filters.cwd.clone(),
            sender: Some(Sender::Assistant.as_str().to_string()),
            from: filters.from.clone(),
            to: filters.to.clone(),
            range: self.range(filters)?,
            offset_minutes: self.offset_minutes,
            fold_ascii_case: true,
            newest_first: true,
            // As `said` leaves out the person's imported copies: stamped at the import, they
            // would pass for the newest records and push the originals out.
            skip_imported: true,
            ..SearchRequest::default()
        };
        let catalog = corpus::source_catalog(&self.root)?;
        let found = search::find_many(&self.root, &catalog, &request, terms)?;
        let synced_through = found
            .synced_through_ms()
            .map(|ms| time::rfc3339_from_unix_ms(ms as i64, self.offset_minutes));
        let lists = terms
            .iter()
            .enumerate()
            .map(|(index, term)| Found {
                total: found.total(index),
                hits: found
                    .hits(index)
                    .iter()
                    .map(|hit| {
                        let header = &hit.header;
                        Hit {
                            source_ref: SourceRef {
                                source_id: header.source_id.clone(),
                                byte_start: header.byte_start,
                                byte_len: header.byte_len,
                            },
                            source_path: hit.source_path.clone(),
                            line: header.line,
                            session: header.session.clone(),
                            timestamp: header.timestamp.clone(),
                            who: Who::Agent,
                            app: catalog
                                .get(&header.source_id)
                                .map(|entry| entry.app.clone())
                                .unwrap_or_default(),
                            project: text::project_from_cwd(&header.cwd),
                            text: hit.snippet.clone().unwrap_or_default(),
                            images: 0,
                            terms: vec![term.clone()],
                            note: String::new(),
                        }
                    })
                    .collect(),
            })
            .collect();
        Ok((lists, synced_through))
    }

    pub fn recent_person(
        &self,
        session: &str,
        filters: &Filters,
        limit: usize,
    ) -> io::Result<Recent> {
        let _lock = corpus::read_lock(&self.root)?;
        let found = said::find(
            &self.root,
            &SaidRequest {
                session: Some(session.to_string()),
                cwd: filters.cwd.clone(),
                from: filters.from.clone(),
                to: filters.to.clone(),
                range: self.range(filters)?,
                limit,
                newest_first: true,
                offset_minutes: self.offset_minutes,
                ..SaidRequest::default()
            },
        )?;
        let mut recent = Recent {
            session: session.to_string(),
            total: found.lists[0].messages.len(),
            stale_sources: found.stale_sources,
            unscanned_bytes: found.unscanned_bytes,
            hits: Vec::new(),
        };
        recent.hits = found.lists.into_iter().next().unwrap().messages.into_iter().take(limit)
            .map(|said| Hit {
                source_ref: SourceRef { source_id: said.header.source_id.clone(), byte_start: said.header.byte_start, byte_len: said.header.byte_len },
                source_path: said.source_path,
                line: said.header.line,
                session: said.header.session.clone(),
                project: text::project_from_cwd(&said.header.cwd),
                timestamp: said.header.timestamp,
                who: Who::Person,
                app: said.app,
                text: said.text,
                images: said.images,
                terms: Vec::new(),
                note: "Recent human message from a related session, included without a keyword match; check for later decisions and changes, and distinguish unrelated topics.".to_string(),
            }).collect();
        Ok(recent)
    }

    /// The readable records around one: up to `before` records of its log before it and
    /// `after` after it, the record itself marked.
    pub fn around(&self, source: &SourceRef, before: usize, after: usize) -> io::Result<Vec<Line>> {
        let _lock = corpus::read_lock(&self.root)?;
        let events = neighbourhood(
            &self.root,
            &source.source_id,
            source.byte_start,
            before,
            after,
        )?;
        Ok(readable_lines(&events, source.byte_start))
    }
}

/// An event of the corpus, and where its body is.
struct Located {
    header: EventHeader,
    file: usize,
    body_at: u64,
}

/// The events of a source around the one that starts at `byte_start` in its log, with their
/// bodies. Only the bodies of the events returned are read.
fn neighbourhood(
    root: &Path,
    source_id: &str,
    byte_start: u64,
    before: usize,
    after: usize,
) -> io::Result<Vec<(EventHeader, Vec<u8>)>> {
    let files = corpus::source_files(root, source_id)?;
    let mut preceding: VecDeque<Located> = VecDeque::new();
    let mut window: Vec<Located> = Vec::new();
    'files: for (file, path) in files.iter().enumerate() {
        let mut reader = BufReader::with_capacity(1024 * 1024, File::open(path)?);
        let mut header_line = Vec::new();
        let mut position = 0u64;
        loop {
            header_line.clear();
            let read = reader.read_until(b'\n', &mut header_line)?;
            if read == 0 {
                break;
            }
            let header = format::parse_event_header(&header_line).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {}", path.display(), error),
                )
            })?;
            let body_at = position + read as u64;
            position = body_at + header.body_len + 1;
            skip(&mut reader, header.body_len + 1)?;
            if header.source_id != source_id {
                continue;
            }
            let located = Located {
                header,
                file,
                body_at,
            };
            if !window.is_empty() {
                window.push(located);
                if window.len() > after {
                    break 'files;
                }
            } else if located.header.byte_start == byte_start {
                window.push(located);
                if after == 0 {
                    break 'files;
                }
            } else if before > 0 {
                if preceding.len() == before {
                    preceding.pop_front();
                }
                preceding.push_back(located);
            }
        }
    }
    if window.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no record of {source_id} starts at byte {byte_start} in the corpus"),
        ));
    }
    let mut opened: Vec<Option<File>> = files.iter().map(|_| None).collect();
    let mut events = Vec::with_capacity(preceding.len() + window.len());
    for located in preceding.into_iter().chain(window) {
        if opened[located.file].is_none() {
            opened[located.file] = Some(File::open(&files[located.file])?);
        }
        let file = opened[located.file]
            .as_mut()
            .expect("the segment file was opened");
        file.seek(SeekFrom::Start(located.body_at))?;
        let mut body = vec![0; located.header.body_len as usize];
        file.read_exact(&mut body)?;
        events.push((located.header, body));
    }
    Ok(events)
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

/// The events as readable lines, the one at `hit_start` marked. Codex writes a message twice
/// (as a response item and as an event): one of them is kept, the hit when it is one. Copies of
/// another agent's conversation imported into the thread are left out: they carry the time of
/// the import, not of the words.
fn readable_lines(events: &[(EventHeader, Vec<u8>)], hit_start: u64) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    for (header, body) in events {
        let is_hit = header.byte_start == hit_start;
        if !is_hit && header.turn.starts_with(IMPORTED_TURN_PREFIX) {
            continue;
        }
        let body = String::from_utf8_lossy(body);
        let (label, text) = match readable(header, &body) {
            Some(found) => found,
            None if is_hit => (Label::Record, values_text(&format::body_fields(&body))),
            None => continue,
        };
        let line = Line {
            label,
            timestamp: header.timestamp.clone(),
            text,
            is_hit,
            byte_start: header.byte_start,
            byte_len: header.byte_len,
        };
        if let Some(index) = lines
            .iter()
            .rposition(|previous| previous.text == line.text)
        {
            if lines.len() - index <= 3 {
                if !is_hit {
                    continue;
                }
                lines.remove(index);
            }
        }
        lines.push(line);
    }
    lines
}

/// Who wrote an event and what it says, or none when it holds nothing to read.
fn readable(header: &EventHeader, body: &str) -> Option<(Label, String)> {
    if header.sender == Sender::Human.as_str() {
        let (text, images) = human_text_from_body(body);
        let text = match images {
            0 => text,
            1 => format!("{text}\n[1 image]"),
            count => format!("{text}\n[{count} images]"),
        };
        return non_empty(Label::Person, text);
    }
    // Records that are not messages (queue operations, hook summaries, titles, modes, token
    // counts) hold nothing to read.
    let message = matches!(
        header.kind.as_str(),
        "user" | "assistant" | "command" | "output" | "patch" | "summary"
    ) || matches!(header.sender.as_str(), "agent" | "assistant" | "summary");
    if !message {
        return None;
    }
    let fields = format::body_fields(body);
    if header.kind == "output" {
        return non_empty(Label::Result, values_text(&fields));
    }
    let (texts, calls) = parts(&fields);
    let label = match (header.kind.as_str(), header.sender.as_str()) {
        ("command" | "patch", _) => Label::Tool,
        (_, "summary") | ("summary", _) => Label::Summary,
        (_, "system") => Label::System,
        (_, "agent") => Label::UserTurn,
        (_, "assistant") | ("assistant", _) if texts.is_empty() && !calls.is_empty() => Label::Tool,
        (_, "assistant") | ("assistant", _) => Label::Agent,
        ("user", _) => Label::UserTurn,
        _ => return None,
    };
    let mut lines = texts;
    lines.extend(calls);
    if lines.is_empty() {
        return non_empty(label, values_text(&fields));
    }
    non_empty(label, lines.join("\n"))
}

fn non_empty(label: Label, text: String) -> Option<(Label, String)> {
    (!text.trim().is_empty()).then_some((label, text))
}

/// The keys of a tool's input that say what it did; file contents and long edits stay out.
const INPUT_KEYS: [&str; 10] = [
    "command",
    "filepath",
    "path",
    "pattern",
    "query",
    "url",
    "description",
    "prompt",
    "skill",
    "args",
];

/// The keys whose values are what a message says.
const TEXT_KEYS: [&str; 5] = ["text", "message", "summary", "lastagentmessage", "content"];

/// The keys whose values are what a tool's result says.
const RESULT_KEYS: [&str; 9] = [
    "text",
    "message",
    "summary",
    "lastagentmessage",
    "content",
    "output",
    "stdout",
    "stderr",
    "result",
];

/// The keys that name, number, or mark a record rather than say something.
const ID_KEYS: [&str; 7] = [
    "type",
    "role",
    "status",
    "name",
    "timestamp",
    "index",
    "version",
];

fn is_id(key: &str) -> bool {
    ID_KEYS.contains(&key) || key.ends_with("id") || key.ends_with("uuid")
}

/// What a record says, and the tool calls in it: `[tool] key: value | ...`.
fn parts(fields: &[(String, String)]) -> (Vec<String>, Vec<String>) {
    let mut texts = Vec::new();
    // Each call by the path of its block: the tool's name and what it was asked.
    let mut calls: Vec<(String, String, Vec<String>)> = Vec::new();
    let call = |calls: &mut Vec<(String, String, Vec<String>)>, block: &str| -> usize {
        match calls.iter().position(|(known, _, _)| known == block) {
            Some(index) => index,
            None => {
                calls.push((block.to_string(), String::new(), Vec::new()));
                calls.len() - 1
            }
        }
    };
    for (path, value) in fields {
        if value.trim().is_empty() {
            continue;
        }
        let key = normalize_key(path.rsplit('/').next().unwrap_or(path));
        if let Some(index) = path.find("/input/") {
            let at = call(&mut calls, &path[..index]);
            if INPUT_KEYS.contains(&key.as_str()) && calls[at].2.len() < 3 {
                let shown = text::clip_head(&text::one_line(value), 160);
                calls[at].2.push(format!("{key}: {shown}"));
            }
            continue;
        }
        let parent = path
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        match key.as_str() {
            "name" => {
                let at = call(&mut calls, parent);
                calls[at].1 = value.clone();
            }
            "arguments" | "input" => {
                let at = call(&mut calls, parent);
                calls[at]
                    .2
                    .push(text::clip_head(&text::one_line(value), 300));
            }
            key if TEXT_KEYS.contains(&key) => texts.push(value.clone()),
            _ => {}
        }
    }
    let calls = calls
        .into_iter()
        .filter(|(_, name, asked)| !name.is_empty() && !asked.is_empty())
        .map(|(_, name, asked)| format!("[{name}] {}", asked.join(" | ")))
        .collect();
    (texts, calls)
}

/// What a record says, one value per line: its text fields, else every value that is not an
/// id or a mark.
fn values_text(fields: &[(String, String)]) -> String {
    let keyed = fields
        .iter()
        .filter(|(_, value)| !value.trim().is_empty())
        .map(|(path, value)| {
            (
                normalize_key(path.rsplit('/').next().unwrap_or(path)),
                value.as_str(),
            )
        })
        .collect::<Vec<_>>();
    let texts = keyed
        .iter()
        .filter(|(key, _)| RESULT_KEYS.contains(&key.as_str()))
        .map(|(_, value)| *value)
        .collect::<Vec<_>>();
    if !texts.is_empty() {
        return texts.join("\n");
    }
    keyed
        .iter()
        .filter(|(key, _)| !is_id(key))
        .map(|(_, value)| *value)
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::push_field;

    fn event(
        kind: &str,
        sender: &str,
        byte_start: u64,
        fields: &[(&str, &str)],
    ) -> (EventHeader, Vec<u8>) {
        let mut body = String::new();
        for (path, value) in fields {
            push_field(&mut body, path, value);
        }
        let header = EventHeader {
            source_id: "claude-1".to_string(),
            kind: kind.to_string(),
            sender: sender.to_string(),
            timestamp: format!("2026-10-13T19:0{}:00Z", byte_start % 10),
            byte_start,
            byte_len: 5,
            body_len: body.len() as u64,
            ..EventHeader::default()
        };
        (header, body.into_bytes())
    }

    #[test]
    fn reads_who_wrote_each_record_and_marks_the_hit() {
        let events = vec![
            event(
                "user",
                "human",
                1,
                &[
                    ("/message/content/0/text", "凡例の色が薄くなってる"),
                    ("/images", "1"),
                ],
            ),
            event(
                "assistant",
                "assistant",
                2,
                &[
                    ("/message/content/0/name", "Bash"),
                    ("/message/content/0/input/command", "ls maps"),
                    ("/message/content/0/input/description", "List the maps"),
                    ("/message/content/0/input/content", "a whole file"),
                ],
            ),
            event(
                "output",
                "system",
                3,
                &[
                    ("/message/content/0/content", "legend.json"),
                    ("/message/content/0/tool_use_id", "t1"),
                ],
            ),
            event(
                "user",
                "agent",
                4,
                &[("/message/content", "Survey the parser")],
            ),
            event(
                "assistant",
                "assistant",
                5,
                &[("/message/content/0/text", "見つけました")],
            ),
            event(
                "user",
                "system",
                6,
                &[(
                    "/message/content",
                    "<task-notification>done</task-notification>",
                )],
            ),
            event("unknown", "unknown", 7, &[("/type", "queue-operation")]),
            event("unknown", "system", 8, &[("/operation", "dequeue")]),
            event(
                "unknown",
                "system",
                9,
                &[("/subtype", "stop_hook_summary"), ("/hookCount", "2")],
            ),
        ];
        let lines = readable_lines(&events, 1);
        let labels: Vec<Label> = lines.iter().map(|line| line.label).collect();
        assert_eq!(
            labels,
            vec![
                Label::Person,
                Label::Tool,
                Label::Result,
                Label::UserTurn,
                Label::Agent,
                Label::System
            ]
        );
        assert!(lines[0].is_hit);
        assert_eq!(lines[0].text, "凡例の色が薄くなってる\n[1 image]");
        assert_eq!(
            lines[1].text,
            "[Bash] command: ls maps | description: List the maps"
        );
        assert_eq!(lines[2].text, "legend.json", "an id says nothing");
        assert_eq!(lines[4].text, "見つけました");
    }

    #[test]
    fn keeps_one_of_a_message_written_twice_the_hit_if_it_is_one() {
        let events = vec![
            event(
                "user",
                "human",
                1,
                &[("/payload/content/0/text", "古い凡例は残しておいて")],
            ),
            event(
                "user",
                "human",
                2,
                &[("/payload/message", "古い凡例は残しておいて")],
            ),
            event(
                "command",
                "assistant",
                3,
                &[
                    ("/payload/name", "shell"),
                    ("/payload/arguments", "{\"command\":[\"ebira\",\"said\"]}"),
                ],
            ),
        ];
        let lines = readable_lines(&events, 2);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].is_hit && lines[0].byte_start == 2);
        assert_eq!(lines[1].label, Label::Tool);
        assert!(lines[1].text.starts_with("[shell] {\"command\""));
    }

    #[test]
    fn a_hit_without_text_is_still_shown() {
        let events = vec![event(
            "unknown",
            "unknown",
            9,
            &[("/payload/detail", "odd record")],
        )];
        let lines = readable_lines(&events, 9);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].label, Label::Record);
        assert_eq!(lines[0].text, "odd record");
    }
}
