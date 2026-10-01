//! Follow one transcript and return the next messages written to it.
//!
//! An agent whose conversation is recorded by Claude Code or Codex can watch
//! the other agent's transcript directly: no shared folder, no transcription.
//! The command reads only the raw JSONL of one source, from a byte boundary,
//! and returns complete records that carry a message from the requested sender.
//! It reads them with the corpus's own `LogReader`, from the state the corpus
//! holds at that boundary, so a message has the sender, `via`, timestamp, and
//! byte range the corpus gives the same record.
//!
//! The transcript is named by a registered source id, by a session id (its
//! transcript in the corpus, else a file named by it under the registered
//! source roots, for a session the corpus has not read yet), or by a path.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::core::{human_text_from_body, persons_message, PersonsMessage};
use crate::corpus::{self, LogReader, ReadRecord, SourceEntry};
use crate::json;
use crate::jsonl::{Field, ScalarKind};

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
    line: u64,
    byte_start: u64,
    /// The whole line, its newline included, as in a corpus `source_ref`.
    byte_len: u64,
    text: String,
    images: usize,
}

struct Target {
    source_id: String,
    path: PathBuf,
}

pub fn run(root: &Path, request: &FollowRequest) -> io::Result<()> {
    let catalog = corpus::source_catalog(root)?;
    let target = resolve_target(root, &catalog, request)?;
    let path = target.path.as_path();
    let size = fs::metadata(path)?.len();
    let start = request.after_byte.unwrap_or(size).min(size);
    let mut cursor = align_to_line_start(path, start)?;
    // Made once the transcript holds a complete record, so the agent that writes it is known
    // before any message is classified.
    let mut reader = None;
    let deadline = Instant::now() + Duration::from_secs(request.seconds);
    loop {
        if reader.is_none() {
            reader = corpus::log_reader_at(&catalog, path, cursor)?;
        }
        let (messages, next) = match reader.as_mut() {
            Some((reader, lines)) => scan(path, reader, lines, cursor, request)?,
            None => (Vec::new(), cursor),
        };
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

/// `--source` wins, then `--source-id` through the catalog, then `--session`:
/// the session's transcript in the corpus (`corpus::session_sources`), else the
/// newest file named by it under the registered source roots.
fn resolve_target(
    root: &Path,
    catalog: &BTreeMap<String, SourceEntry>,
    request: &FollowRequest,
) -> io::Result<Target> {
    if let Some(path) = &request.path {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("transcript not found: {}", path.display()),
            ));
        }
        return Ok(Target {
            source_id: String::new(),
            path,
        });
    }
    if let Some(source_id) = &request.source_id {
        let entry = catalog.get(source_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown source id: {}", source_id),
            )
        })?;
        return Ok(Target {
            source_id: entry.source_id.clone(),
            path: PathBuf::from(&entry.path),
        });
    }
    if let Some(session) = &request.session {
        if session.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--session is empty",
            ));
        }
        if let Some(entry) = corpus::session_sources(catalog, session).main {
            return Ok(Target {
                source_id: entry.source_id.clone(),
                path: PathBuf::from(&entry.path),
            });
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
        return Ok(Target {
            source_id: String::new(),
            path,
        });
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "follow needs --session, --source-id or --source",
    ))
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
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
        && corpus::file_names_session(path, session)
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
/// boundary after the last line read. `lines` counts the lines before `cursor`
/// and is moved past the lines read.
fn scan(
    path: &Path,
    reader: &mut LogReader,
    lines: &mut u64,
    cursor: u64,
    request: &FollowRequest,
) -> io::Result<(Vec<Message>, u64)> {
    let size = fs::metadata(path)?.len();
    if size <= cursor {
        return Ok((Vec::new(), cursor));
    }
    let mut input = BufReader::new(File::open(path)?);
    input.seek(SeekFrom::Start(cursor))?;
    let mut bounded = input.take(size - cursor);
    let mut line = Vec::new();
    let mut messages = Vec::new();
    let mut position = cursor;
    while let Some(read) = corpus::read_bounded_line(&mut bounded, &mut line)? {
        if !read.complete {
            break;
        }
        let record = reader.read(&line, position, read.oversized);
        let byte_start = position;
        position += read.total_len;
        *lines += 1;
        if let Some(message) = message(&record, request, *lines, byte_start, read.total_len) {
            messages.push(message);
            if messages.len() >= request.limit {
                break;
            }
        }
    }
    Ok((messages, position))
}

fn value<'a>(fields: &'a [Field], path: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|field| field.path == path)
        .map(|field| field.value.as_str())
}

/// A message in either transcript format, from the sender the corpus gives it.
///
/// Codex: `/type = response_item`, `/payload/type = message`, text parts under
/// `/payload/content/<n>/text`. Claude Code: `/type = assistant|user`, text
/// parts under `/message/content/<n>/text` whose type is `text`, or a plain
/// string content; a prompt typed while the agent was working is a
/// `queued_command` attachment with the text in `/attachment/prompt`. The
/// person's messages are those `said` lists (`core::persons_message`), with the
/// text `said` shows: without the blocks the tools inject, and imported copies
/// only for `--sender any`.
fn message(
    record: &ReadRecord,
    request: &FollowRequest,
    line: u64,
    byte_start: u64,
    byte_len: u64,
) -> Option<Message> {
    let fields = record.fields.as_slice();
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
    let meta = &record.event.meta;
    let persons = persons_message(meta.sender.as_str(), &meta.via);
    let wanted = match request.sender.as_str() {
        "any" => true,
        "human" => persons == Some(PersonsMessage::Own),
        sender => meta.sender.as_str() == sender,
    };
    if !wanted {
        return None;
    }
    let (text, images) = match persons {
        Some(_) => human_text_from_body(&record.event.body),
        None => (text, 0),
    };
    if text.trim().is_empty() && images == 0 {
        return None;
    }
    if let Some(prefix) = &request.prefix {
        if !text.trim_start().starts_with(prefix.as_str()) {
            return None;
        }
    }
    Some(Message {
        sender: meta.sender.as_str(),
        via: meta.via.clone(),
        timestamp: meta.timestamp.clone().unwrap_or_default(),
        line,
        byte_start,
        byte_len,
        text,
        images,
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
    let source_path = target.path.to_string_lossy();
    let messages = messages.iter().map(|message| {
        json::Object::new()
            .name("sender", message.sender)
            .name("via", &message.via)
            .name("timestamp", &message.timestamp)
            .text("text", &message.text)
            .number("images", message.images as u64)
            .raw(
                "source_ref",
                &json::source_ref(
                    &target.source_id,
                    &source_path,
                    Some(message.line),
                    message.byte_start,
                    message.byte_len,
                ),
            )
            .finish()
    });
    println!(
        "{}",
        json::Object::new()
            .name("disposition", disposition)
            .name("source_id", &target.source_id)
            .name("source_path", &source_path)
            .number("after_byte", after_byte)
            .raw("messages", &json::array(messages))
            .finish()
    );
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

    /// The messages after `cursor` from `sender`, read as `run` reads them without a corpus.
    fn follow(path: &Path, cursor: u64, sender: &str) -> Option<(Vec<Message>, u64)> {
        let (mut reader, mut lines) =
            corpus::log_reader_at(&BTreeMap::new(), path, cursor).expect("reader")?;
        Some(scan(path, &mut reader, &mut lines, cursor, &request(sender)).expect("scan"))
    }

    fn append(path: &Path, contents: &str) {
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(contents.as_bytes()))
            .expect("append records");
    }

    #[test]
    fn a_transcript_written_before_following_is_read_by_sender() {
        let path = transcript("before", CODEX_THREAD);
        let (messages, cursor) = follow(&path, 0, "human").expect("a complete record");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "the person's words");
        assert_eq!(cursor, CODEX_THREAD.len() as u64);
        let line = CODEX_THREAD.lines().nth(1).expect("message line");
        assert_eq!(
            (messages[0].line, messages[0].byte_len),
            (2, line.len() as u64 + 1),
            "the line number and the range, newline included, are those of corpus references"
        );
        fs::remove_file(path).expect("remove transcript");
    }

    #[test]
    fn a_transcript_first_written_while_following_is_read_by_sender() {
        let path = transcript("after", "");
        assert!(
            follow(&path, 0, "human").is_none(),
            "nothing is read before the transcript says which agent writes it"
        );
        append(&path, CODEX_THREAD);
        let (messages, _) = follow(&path, 0, "human").expect("a complete record");
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
        assert!(
            follow(&path, 0, "human").is_none(),
            "an empty rollout does not say who started it"
        );
        append(
            &path,
            concat!(
                r#"{"timestamp":"2026-09-01T00:00:00Z","type":"session_meta","payload":{"id":"child","parent_thread_id":"parent"}}"#,
                "\n",
                r#"{"timestamp":"2026-09-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"audit the parser"}]}}"#,
                "\n",
            ),
        );
        let (messages, _) = follow(&path, 0, "human").expect("a complete record");
        assert!(
            messages.is_empty(),
            "the parent thread's prompt is another agent's, not the person's"
        );
        let (messages, _) = follow(&path, 0, "agent").expect("a complete record");
        assert_eq!(messages.len(), 1);
        fs::remove_dir_all(
            directory
                .parent()
                .and_then(Path::parent)
                .expect("test root"),
        )
        .expect("remove test root");
    }

    #[test]
    fn an_imported_conversation_is_not_the_persons_next_message() {
        let imported = concat!(
            r#"{"timestamp":"2026-09-01T00:00:00Z","type":"session_meta","payload":{"id":"t2"}}"#,
            "\n",
            r#"{"timestamp":"2026-09-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"external-import-turn-1"}}"#,
            "\n",
        );
        let copy = concat!(
            r#"{"timestamp":"2026-09-01T00:00:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"words from the other agent's session"}]}}"#,
            "\n",
        );
        let own = concat!(
            r#"{"timestamp":"2026-09-01T00:05:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}"#,
            "\n",
            r#"{"timestamp":"2026-09-01T00:05:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"new words"}]}}"#,
            "\n",
        );
        let path = transcript("imported", &format!("{imported}{copy}{own}"));
        let (messages, _) = follow(&path, 0, "human").expect("a complete record");
        let texts = messages.iter().map(|m| m.text.as_str()).collect::<Vec<_>>();
        assert_eq!(texts, ["new words"]);
        // Started after the record that opened the import turn, the reader still knows it.
        let (messages, _) = follow(&path, imported.len() as u64, "any").expect("a complete record");
        let copy_message = messages
            .iter()
            .find(|m| m.text == "words from the other agent's session")
            .expect("the copy is a message");
        assert_eq!(
            (copy_message.sender, copy_message.via.as_str()),
            ("human", "imported")
        );
        fs::remove_file(path).expect("remove transcript");
    }
}
