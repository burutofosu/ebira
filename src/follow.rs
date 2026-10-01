//! Follow one transcript and return the next messages written to it.
//!
//! An agent whose conversation is recorded by Claude Code or Codex can watch
//! the other agent's transcript directly: no shared folder, no transcription.
//! The command reads only the raw JSONL of one source, from a byte boundary,
//! and returns complete records that carry a message from the requested sender,
//! classified the same way the corpus classifies it.
//!
//! The transcript is named by a registered source id, by a session id whose
//! file lives under one of the registered source roots, or by a path.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::core::{
    classify_sender, human_text_from_body, record_meta, select_body, Sender, SourceOrigin,
};
use crate::corpus;
use crate::format::json_string;
use crate::jsonl::{parse_record, Field, ScalarKind};

pub struct FollowRequest {
    pub source_id: Option<String>,
    pub session: Option<String>,
    pub path: Option<String>,
    pub after_byte: Option<u64>,
    pub seconds: u64,
    /// A sender name, or `any`.
    pub sender: String,
    pub prefix: Option<String>,
    pub limit: usize,
}

struct Message {
    sender: &'static str,
    via: String,
    timestamp: String,
    byte_start: u64,
    byte_len: u64,
    text: String,
}

struct Target {
    source_id: String,
    path: PathBuf,
    origin: Origin,
}

/// Who wrote a transcript. A Claude Code transcript is known from its path; a Codex thread
/// from its first record, which says whether another thread or `codex exec` started it; a
/// transcript outside `.claude` and `.codex` from its first records. Until that is known, the
/// origin is decided again whenever new lines arrive, before any of them is classified.
#[derive(Clone, Copy)]
struct Origin {
    value: SourceOrigin,
    settled: bool,
}

impl Origin {
    fn of(path: &Path) -> Self {
        let value = corpus::origin_for_path(path);
        let settled = match value {
            SourceOrigin::ClaudeMain | SourceOrigin::ClaudeSubagent => true,
            SourceOrigin::Generic => false,
            _ => first_line_complete(path),
        };
        Origin { value, settled }
    }

    /// Called when the file holds at least one complete line.
    fn settle(&mut self, path: &Path) {
        if !self.settled {
            self.value = corpus::origin_for_path(path);
            self.settled = self.value != SourceOrigin::Generic;
        }
    }
}

fn first_line_complete(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut reader = io::BufReader::new(file.take(FIRST_LINE_LIMIT));
    let mut line = Vec::new();
    io::BufRead::read_until(&mut reader, b'\n', &mut line).is_ok() && line.last() == Some(&b'\n')
}

/// A Codex session_meta record carries the base instructions and can be large.
const FIRST_LINE_LIMIT: u64 = 16 * 1024 * 1024;

pub fn run(root: &Path, request: &FollowRequest) -> io::Result<()> {
    let target = resolve_target(root, request)?;
    let path = target.path.as_path();
    let mut origin = target.origin;
    let size = fs::metadata(path)?.len();
    let start = request.after_byte.unwrap_or(size).min(size);
    let mut cursor = align_to_line_start(path, start)?;
    let deadline = Instant::now() + Duration::from_secs(request.seconds);
    loop {
        let (messages, next) = scan(path, &mut origin, cursor, request)?;
        if !messages.is_empty() {
            print_result("received", &target, next, &messages);
            return Ok(());
        }
        cursor = next;
        if Instant::now() >= deadline {
            print_result("waiting", &target, cursor, &messages);
            return Ok(());
        }
        sleep(Duration::from_millis(500));
    }
}

/// `--source` wins, then `--source-id` through the catalog, then `--session`
/// by file name under the registered source roots (newest file wins).
fn resolve_target(root: &Path, request: &FollowRequest) -> io::Result<Target> {
    if let Some(path) = &request.path {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("transcript not found: {}", path.display()),
            ));
        }
        return Ok(target(String::new(), path));
    }
    if let Some(source_id) = &request.source_id {
        let catalog = corpus::source_catalog(root)?;
        let entry = catalog.get(source_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown source id: {}", source_id),
            )
        })?;
        return Ok(target(entry.source_id.clone(), PathBuf::from(&entry.path)));
    }
    if let Some(session) = &request.session {
        if session.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--session is empty",
            ));
        }
        let mut found: Vec<(u64, PathBuf)> = Vec::new();
        for input in corpus::registered_sources(root)? {
            collect_session_files(Path::new(&input), session, &mut found, 0)?;
        }
        found.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        let Some((_, path)) = found.into_iter().next() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no transcript file named by session {}", session),
            ));
        };
        return Ok(target(String::new(), path));
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "follow needs --session, --source-id or --source",
    ))
}

fn target(source_id: String, path: PathBuf) -> Target {
    let origin = Origin::of(&path);
    Target {
        source_id,
        path,
        origin,
    }
}

/// A registered source is a JSONL file or a directory of them: a file is matched by its own
/// name, a directory is searched.
fn collect_session_files(
    path: &Path,
    session: &str,
    found: &mut Vec<(u64, PathBuf)>,
    depth: usize,
) -> io::Result<()> {
    if depth > 8 {
        return Ok(());
    }
    if path.is_file() {
        if names_session(path, session) {
            if let Ok(metadata) = fs::metadata(path) {
                found.push((modified_ms(&metadata), path.to_path_buf()));
            }
        }
        return Ok(());
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry?;
        let child = entry.path();
        if child.is_dir() {
            collect_session_files(&child, session, found, depth + 1)?;
            continue;
        }
        if names_session(&child, session) {
            if let Ok(metadata) = entry.metadata() {
                found.push((modified_ms(&metadata), child));
            }
        }
    }
    Ok(())
}

fn names_session(path: &Path, session: &str) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.ends_with(".jsonl") && name.contains(session))
}

fn modified_ms(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Move a caller-supplied boundary forward to the start of the next line.
fn align_to_line_start(path: &Path, cursor: u64) -> io::Result<u64> {
    if cursor == 0 {
        return Ok(0);
    }
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    if cursor >= size {
        return Ok(size);
    }
    file.seek(SeekFrom::Start(cursor - 1))?;
    let mut previous = [0u8; 1];
    file.read_exact(&mut previous)?;
    if previous[0] == b'\n' {
        return Ok(cursor);
    }
    let mut position = cursor;
    let mut buffer = [0u8; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(size);
        }
        if let Some(index) = buffer[..read].iter().position(|byte| *byte == b'\n') {
            return Ok(position + index as u64 + 1);
        }
        position += read as u64;
    }
}

/// Read every complete line after `cursor`; return the messages found and the
/// boundary after the last line consumed. The origin is settled first, so the first
/// messages of a transcript that began empty are classified like the rest.
fn scan(
    path: &Path,
    origin: &mut Origin,
    cursor: u64,
    request: &FollowRequest,
) -> io::Result<(Vec<Message>, u64)> {
    let size = fs::metadata(path)?.len();
    if size <= cursor {
        return Ok((Vec::new(), cursor));
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(cursor))?;
    let mut bytes = Vec::with_capacity((size - cursor) as usize);
    file.read_to_end(&mut bytes)?;
    let complete = match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(index) => index + 1,
        None => return Ok((Vec::new(), cursor)),
    };
    origin.settle(path);
    let origin = origin.value;
    let mut messages = Vec::new();
    let mut offset = 0usize;
    while offset < complete {
        let end = bytes[offset..complete]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| offset + index)
            .unwrap_or(complete);
        let line = trim_cr(&bytes[offset..end]);
        if !line.is_empty() {
            let base = cursor + offset as u64;
            if let Ok(fields) = parse_record(line, base) {
                if let Some(message) =
                    message_from_fields(&fields, origin, request, base, line.len() as u64)
                {
                    messages.push(message);
                    if messages.len() >= request.limit {
                        return Ok((messages, cursor + end as u64 + 1));
                    }
                }
            }
        }
        offset = end + 1;
    }
    Ok((messages, cursor + complete as u64))
}

fn trim_cr(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    }
}

fn value<'a>(fields: &'a [Field], path: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|field| field.path == path)
        .map(|field| field.value.as_str())
}

/// Recognise a message in either transcript format and decide who sent it.
///
/// Codex: `/type = response_item`, `/payload/type = message`, text parts under
/// `/payload/content/<n>/text`. Claude Code: `/type = assistant|user`, text
/// parts under `/message/content/<n>/text` whose type is `text`, or a plain
/// string content; a prompt typed while the agent was working is a
/// `queued_command` attachment with the text in `/attachment/prompt`. A person's
/// message loses the blocks the tools inject into it.
fn message_from_fields(
    fields: &[Field],
    origin: SourceOrigin,
    request: &FollowRequest,
    byte_start: u64,
    byte_len: u64,
) -> Option<Message> {
    let text = match value(fields, "/type")? {
        "response_item" if value(fields, "/payload/type") == Some("message") => {
            collect_text(fields, "/payload/content/", &["input_text", "output_text"])
        }
        "assistant" | "user" => plain_or_parts(fields, "/message/content"),
        "attachment" if value(fields, "/attachment/type") == Some("queued_command") => {
            plain_or_parts(fields, "/attachment/prompt")
        }
        _ => return None,
    };
    let mut meta = record_meta(fields);
    classify_sender(origin, fields, &mut meta);
    if request.sender != "any" && meta.sender.as_str() != request.sender {
        return None;
    }
    let text = if meta.sender == Sender::Human {
        human_text_from_body(&select_body(fields, &meta, 0)).0
    } else {
        text
    };
    if text.trim().is_empty() {
        return None;
    }
    if let Some(prefix) = &request.prefix {
        if !text.trim_start().starts_with(prefix.as_str()) {
            return None;
        }
    }
    Some(Message {
        sender: meta.sender.as_str(),
        via: meta.via,
        timestamp: value(fields, "/timestamp").unwrap_or("").to_string(),
        byte_start,
        byte_len,
        text,
    })
}

/// A content value that is either a plain string or an array of text parts.
fn plain_or_parts(fields: &[Field], content: &str) -> String {
    let plain = fields
        .iter()
        .find(|field| field.path == content && field.kind == ScalarKind::String)
        .map(|field| field.value.clone());
    match plain {
        Some(text) => text,
        None => collect_text(fields, &format!("{}/", content), &["text"]),
    }
}

/// Join the text parts of a content array whose part type is one of `kinds`.
fn collect_text(fields: &[Field], content_prefix: &str, kinds: &[&str]) -> String {
    let mut parts = Vec::new();
    for field in fields {
        let Some(rest) = field.path.strip_prefix(content_prefix) else {
            continue;
        };
        let Some(index) = rest.strip_suffix("/text") else {
            continue;
        };
        if index.contains('/') {
            continue;
        }
        let type_path = format!("{}{}/type", content_prefix, index);
        let part_type = value(fields, &type_path).unwrap_or("");
        if !kinds.contains(&part_type) {
            continue;
        }
        parts.push(field.value.as_str());
    }
    parts.join("\n")
}

fn print_result(disposition: &str, target: &Target, after_byte: u64, messages: &[Message]) {
    let mut output = String::new();
    output.push_str(&format!(
        "{{\"disposition\":{},\"source_id\":{},\"source_path\":{},\"after_byte\":{},\"messages\":[",
        json_string(disposition),
        json_string(&target.source_id),
        json_string(&target.path.to_string_lossy()),
        after_byte
    ));
    for (index, message) in messages.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str(&format!(
            "{{\"sender\":{},\"via\":{},\"timestamp\":{},\"byte_start\":{},\"byte_len\":{},\"text\":{}}}",
            json_string(message.sender),
            json_string(&message.via),
            json_string(&message.timestamp),
            message.byte_start,
            message.byte_len,
            json_string(&message.text)
        ));
    }
    output.push_str("]}");
    println!("{}", output);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CODEX_THREAD: &str = concat!(
        r#"{"timestamp":"2026-09-01T00:00:00Z","type":"session_meta","payload":{"id":"t1"}}"#,
        "\n",
        r#"{"timestamp":"2026-09-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"the person's words"}]}}"#,
        "\n",
    );

    fn request(sender: &str) -> FollowRequest {
        FollowRequest {
            source_id: None,
            session: None,
            path: None,
            after_byte: Some(0),
            seconds: 0,
            sender: sender.to_string(),
            prefix: None,
            limit: 20,
        }
    }

    /// A transcript kept outside `.claude` and `.codex`, so its format comes from its records.
    fn transcript(name: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ebira-follow-{}-{}.jsonl",
            name,
            std::process::id()
        ));
        fs::write(&path, contents).expect("write transcript");
        path
    }

    #[test]
    fn a_transcript_written_before_following_is_read_by_sender() {
        let path = transcript("before", CODEX_THREAD);
        let mut origin = Origin::of(&path);
        assert_eq!(origin.value, SourceOrigin::CodexThread);
        assert!(origin.settled);
        let (messages, _) = scan(&path, &mut origin, 0, &request("human")).expect("scan");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "the person's words");
        fs::remove_file(path).expect("remove transcript");
    }

    #[test]
    fn a_transcript_first_written_while_following_is_read_by_sender() {
        let path = transcript("after", "");
        let mut origin = Origin::of(&path);
        assert_eq!(origin.value, SourceOrigin::Generic);
        let (messages, cursor) = scan(&path, &mut origin, 0, &request("human")).expect("scan");
        assert!(messages.is_empty());
        assert_eq!(cursor, 0);

        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(CODEX_THREAD.as_bytes()))
            .expect("append records");
        let (messages, _) = scan(&path, &mut origin, cursor, &request("human")).expect("scan");
        assert_eq!(origin.value, SourceOrigin::CodexThread);
        assert_eq!(
            messages.len(),
            1,
            "the first message is the person's once the format is known"
        );
        assert_eq!(messages[0].sender, "human");
        fs::remove_file(path).expect("remove transcript");
    }

    #[test]
    fn a_codex_child_thread_written_while_following_is_known_from_its_first_record() {
        let directory = std::env::temp_dir()
            .join(format!("ebira-follow-child-{}", std::process::id()))
            .join(".codex")
            .join("sessions");
        fs::create_dir_all(&directory).expect("create session directory");
        let path = directory.join("rollout-child.jsonl");
        fs::write(&path, "").expect("write empty rollout");
        let mut origin = Origin::of(&path);
        assert!(
            !origin.settled,
            "an empty rollout does not say who started it"
        );

        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut file| {
                file.write_all(
                    concat!(
                        r#"{"timestamp":"2026-09-01T00:00:00Z","type":"session_meta","payload":{"id":"child","parent_thread_id":"parent"}}"#,
                        "\n",
                        r#"{"timestamp":"2026-09-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"audit the parser"}]}}"#,
                        "\n",
                    )
                    .as_bytes(),
                )
            })
            .expect("append records");
        let (messages, _) = scan(&path, &mut origin, 0, &request("human")).expect("scan");
        assert_eq!(origin.value, SourceOrigin::CodexChild);
        assert!(
            messages.is_empty(),
            "the parent thread's prompt is another agent's, not the person's"
        );
        let (messages, _) = scan(&path, &mut origin, 0, &request("agent")).expect("scan");
        assert_eq!(messages.len(), 1);
        fs::remove_dir_all(
            directory
                .parent()
                .and_then(Path::parent)
                .expect("test root"),
        )
        .expect("remove test root");
    }
}
