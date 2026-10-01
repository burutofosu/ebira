use crate::core::{
    human_text_from_body, is_tool_call, persons_message, EventKind, PersonsMessage, RecoveryEvent,
    RecoverySnapshot, RecoveryState, SeenMessages, Sender, SourceRef,
};
use crate::corpus::{self, SourceEntry};
use crate::format::{body_fields, parse_event_header, push_field};
use crate::jsonl::{parse_record, Field};
use crate::{json, output};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const IO_BUFFER_SIZE: usize = 1024 * 1024;
const MAX_RESUME_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Person's messages kept from the scanned window, newest last.
const KEPT_HUMAN_MESSAGES: usize = 12;
/// `--brief` bounds: every value is cut at a stated size and says so.
const BRIEF_HUMAN_MESSAGES: usize = 10;
const BRIEF_HUMAN_TEXT_BUDGET: usize = 10_000;
const BRIEF_HUMAN_TEXT_LIMIT: usize = 4_000;
const BRIEF_ASSISTANT_TEXTS: usize = 3;
const BRIEF_ASSISTANT_TEXT_LIMIT: usize = 1_500;
const BRIEF_COMMANDS: usize = 8;
const BRIEF_COMMAND_LIMIT: usize = 300;
const MAX_ORIGINAL_RECORD_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Default)]
struct ScanResult {
    current: Option<RecoverySnapshot>,
    previous: Option<RecoverySnapshot>,
    matched_events: u64,
    latest_event_index: Option<u64>,
    /// The person's messages (`core::persons_message`), newest last.
    humans: Vec<Arc<RecoveryEvent>>,
    humans_seen: u64,
    copies_skipped: u64,
    imported_skipped: u64,
    compactions: u64,
    last_compaction_at: String,
    summaries: u64,
    /// The latest assistant replies and tool calls in the scanned window, across turns.
    recent_assistants: Vec<Arc<RecoveryEvent>>,
    recent_commands: Vec<Arc<RecoveryEvent>>,
}

const KEPT_RECENT_ASSISTANTS: usize = 24;

fn keep_last(values: &mut Vec<Arc<RecoveryEvent>>, event: &RecoveryEvent, limit: usize) {
    values.push(Arc::new(event.clone()));
    if values.len() > limit {
        values.remove(0);
    }
}

pub fn resume(
    root: &Path,
    requested_source_id: Option<&str>,
    requested_session: Option<&str>,
    brief: bool,
) -> io::Result<()> {
    let catalog = corpus::source_catalog(root)?;
    let offset_minutes = corpus::corpus_offset_minutes(root)?;
    let last_event_at = if requested_source_id.is_none() {
        candidate_last_event_times(root, offset_minutes)?
    } else {
        BTreeMap::new()
    };
    let source_id = match requested_source_id {
        Some(source_id) => {
            if !catalog.contains_key(source_id) {
                return print_not_found("source_id", source_id);
            }
            source_id.to_string()
        }
        None => {
            let session =
                requested_session.map(|session| corpus::session_sources(&catalog, session));
            let mut candidates = match &session {
                Some(session) => session
                    .sources
                    .iter()
                    .map(|entry| entry.source_id.clone())
                    .collect(),
                None => catalog.keys().cloned().collect::<Vec<_>>(),
            };
            sort_by_recency(&catalog, &last_event_at, &mut candidates, offset_minutes);
            if candidates.is_empty() {
                if let Some(session) = requested_session {
                    print_not_found("session", session)?;
                } else {
                    print_not_found("source_id", "")?;
                }
                return Ok(());
            }
            let chosen = session
                .and_then(|session| session.main)
                .map(|entry| entry.source_id.clone());
            match chosen {
                Some(source_id) => source_id,
                None if candidates.len() == 1 => candidates[0].clone(),
                None => {
                    return print_ambiguous(
                        &catalog,
                        &last_event_at,
                        &candidates,
                        requested_session,
                    );
                }
            }
        }
    };

    let entry = catalog
        .get(&source_id)
        .expect("source was checked against catalog");
    let mut scanned = scan_source(root, &source_id, requested_session)?;
    if requested_session.is_none() {
        scanned.matched_events = entry.event_count;
    }
    let Some(current) = scanned.current.take() else {
        if let Some(session) = requested_session {
            print_not_found("session", session)?;
        } else {
            print_not_found("source_id", &source_id)?;
        }
        return Ok(());
    };
    if brief {
        return output::write(&brief_json(&source_id, entry, &current, &scanned));
    }

    let boundary_reached = corpus::source_is_complete(entry);
    let freshness = corpus::freshness(entry);
    let source_path = &entry.path;
    let observed_boundary = json::Object::new()
        .name("source_id", &source_id)
        .number("byte_end", entry.committed_byte_end)
        .number("source_size", entry.size)
        .number("last_line", entry.last_line)
        .number("event_count", entry.event_count)
        .finish();
    let next_cursor = json::Object::new()
        .name("source_id", &source_id)
        .number(
            "after_event_index",
            scanned
                .latest_event_index
                .unwrap_or(entry.event_count.saturating_sub(1)),
        )
        .number("after_byte", entry.committed_byte_end)
        .finish();
    let mut output = json::Object::new();
    output
        .name("disposition", resume_disposition(entry))
        .name("mode", "resume")
        .name("source_id", &source_id)
        .name("source_path", source_path)
        .name("source_disposition", &entry.disposition)
        .name("current_turn_state", current_turn_state(&current))
        .name(
            "source_boundary_state",
            if boundary_reached { "reached" } else { "open" },
        )
        .raw("observed_boundary", &observed_boundary);
    freshness_fields(&mut output, boundary_reached, freshness);
    output
        .boolean("checkpoint_valid", entry.checkpoint_valid)
        .name("session_id", &current.session)
        .name("latest_turn_id", &current.turn)
        .number("matched_events", scanned.matched_events)
        .raw("current", &snapshot_json(&current, source_path))
        .raw(
            "latest_human_message",
            &json::or_null(
                scanned
                    .humans
                    .last()
                    .map(|event| event_json(event, source_path)),
            ),
        )
        .raw("compactions", &compactions_json(&scanned))
        .raw(
            "latest_event",
            &json::or_null(
                current
                    .events
                    .last()
                    .map(|event| event_json(event, source_path)),
            ),
        )
        .raw(
            "previous_turn",
            &json::or_null(
                scanned
                    .previous
                    .as_ref()
                    .map(|turn| snapshot_json(turn, source_path)),
            ),
        )
        .raw("next_cursor", &next_cursor);
    output::write_line(&output.finish())
}

/// Every segment of the source is read, however the source was named: the person's messages
/// and the compactions of the whole session belong to the result, not only the latest turns.
fn scan_source(
    root: &Path,
    source_id: &str,
    session_filter: Option<&str>,
) -> io::Result<ScanResult> {
    let files = corpus::source_files(root, source_id)?;
    scan_paths(&files, session_filter)
}

fn scan_paths(paths: &[PathBuf], session_filter: Option<&str>) -> io::Result<ScanResult> {
    let mut state = RecoveryState::default();
    let mut humans: Vec<Arc<RecoveryEvent>> = Vec::new();
    let mut humans_seen = 0u64;
    let mut seen = SeenMessages::default();
    let mut copies_skipped = 0u64;
    let mut imported_skipped = 0u64;
    let mut compactions = 0u64;
    let mut last_compaction_at = String::new();
    let mut summaries = 0u64;
    let mut recent_assistants = Vec::new();
    let mut recent_commands = Vec::new();
    for path in paths {
        let file = File::open(path)?;
        let mut reader = BufReader::with_capacity(IO_BUFFER_SIZE, file);
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
            let selected = session_filter
                .map(|session| header.session == session)
                .unwrap_or(true);
            if !selected {
                skip_body_and_separator(&mut reader, header.body_len)?;
                continue;
            }
            // The person's messages are read whole, as `said` reads them; the others up to a
            // bound, and marked when it cuts them.
            let limit = if header.sender == Sender::Human.as_str() {
                usize::MAX
            } else {
                MAX_RESUME_BODY_BYTES
            };
            let (body, read_truncated) = read_body(&mut reader, header.body_len, limit)?;
            let body_truncated = read_truncated || header.body_cut;
            let event = RecoveryEvent {
                event_index: header.event_index,
                source_ref: SourceRef {
                    source_id: header.source_id.clone(),
                    line: header.line,
                    byte_start: header.byte_start,
                    byte_len: header.byte_len,
                },
                session: header.session,
                turn: header.turn,
                role: header.role,
                kind: EventKind::from_str(&header.kind),
                event_type: header.event_type,
                timestamp: header.timestamp,
                cwd: header.cwd,
                repository: header.repository,
                call_id: header.call_id,
                body,
                body_truncated,
                embedded_history: false,
                sender: Sender::from_str(&header.sender),
                via: header.via,
            };
            match persons_message(event.sender.as_str(), &event.via) {
                Some(PersonsMessage::Own) => {
                    if seen.first(&event.timestamp, &human_text_from_body(&event.body).0) {
                        humans_seen += 1;
                        keep_last(&mut humans, &event, KEPT_HUMAN_MESSAGES);
                    } else {
                        copies_skipped += 1;
                    }
                }
                Some(PersonsMessage::Imported) => imported_skipped += 1,
                None => {}
            }
            if event.kind == EventKind::Assistant {
                keep_last(&mut recent_assistants, &event, KEPT_RECENT_ASSISTANTS);
            }
            // The call itself (Claude tool_use, Codex *_call), not begin/end progress events.
            let is_call = is_tool_call(event.kind, &event.event_type, &event.body);
            if is_call {
                keep_last(&mut recent_commands, &event, BRIEF_COMMANDS);
            }
            if crate::core::is_compaction(&event.event_type) {
                compactions += 1;
                last_compaction_at = event.timestamp.clone();
            }
            if event.kind == EventKind::Summary {
                summaries += 1;
            }
            let embedded = embedded_messages(&event);
            state.accept(event, embedded);
        }
    }
    state.finish();
    Ok(ScanResult {
        current: state.current().cloned(),
        previous: state.previous().cloned(),
        matched_events: state.matched_events(),
        latest_event_index: state.latest_event_index(),
        humans,
        humans_seen,
        copies_skipped,
        imported_skipped,
        compactions,
        last_compaction_at,
        summaries,
        recent_assistants,
        recent_commands,
    })
}

fn compactions_json(scanned: &ScanResult) -> String {
    json::Object::new()
        .number("count", scanned.compactions)
        .name("last_at", &scanned.last_compaction_at)
        .number("summaries_seen", scanned.summaries)
        .finish()
}

/// `resume --brief`: the person's recent messages in full, the agent's latest words and
/// commands read from the original records, and the compaction count. Every cut value
/// carries a `*_truncated` flag and a source reference to read the rest.
fn brief_json(
    source_id: &str,
    entry: &SourceEntry,
    current: &RecoverySnapshot,
    scanned: &ScanResult,
) -> String {
    let source_path = &entry.path;
    let boundary_reached = corpus::source_is_complete(entry);
    // Replies and commands are read from the log itself, and only from the log the corpus read.
    let (mut log, freshness) = corpus::open_log(entry);
    let source_ref = |event: &RecoveryEvent| source_ref_json(&event.source_ref, source_path);

    let mut budget = BRIEF_HUMAN_TEXT_BUDGET;
    let mut humans = Vec::new();
    for event in scanned.humans.iter().rev().take(BRIEF_HUMAN_MESSAGES) {
        let (text, images) = human_text_from_body(&event.body);
        let (text, cut) = bounded(&text, BRIEF_HUMAN_TEXT_LIMIT.min(budget));
        budget = budget.saturating_sub(text.chars().count());
        humans.push(
            json::Object::new()
                .name("timestamp", &event.timestamp)
                .name("via", &event.via)
                .text("text", &text)
                .boolean("text_truncated", cut)
                .number("images", images as u64)
                .raw("source_ref", &source_ref(event))
                .finish(),
        );
        if budget == 0 {
            break;
        }
    }
    humans.reverse();
    let human_count = humans.len() as u64;

    let mut texts = Vec::new();
    for event in scanned.recent_assistants.iter().rev() {
        if texts.len() == BRIEF_ASSISTANT_TEXTS {
            break;
        }
        let text = original_fields(log.as_mut(), &event.source_ref)
            .map(|fields| assistant_text(&fields))
            .unwrap_or_default();
        if !text.trim().is_empty() {
            let (text, cut) = bounded(text.trim(), BRIEF_ASSISTANT_TEXT_LIMIT);
            texts.push(
                json::Object::new()
                    .name("timestamp", &event.timestamp)
                    .text("text", &text)
                    .boolean("text_truncated", cut)
                    .raw("source_ref", &source_ref(event))
                    .finish(),
            );
        }
    }
    texts.reverse();

    let commands = scanned.recent_commands.iter().map(|event| {
        let calls = original_fields(log.as_mut(), &event.source_ref)
            .map(|fields| tool_calls(&fields))
            .unwrap_or_default();
        let (name, input) = calls.into_iter().last().unwrap_or_else(|| {
            let values = body_fields(&event.body)
                .into_iter()
                .map(|(_, value)| value)
                .collect::<Vec<_>>();
            (event.event_type.clone(), values.join("\n"))
        });
        let (input, cut) = bounded(&input, BRIEF_COMMAND_LIMIT);
        json::Object::new()
            .name("timestamp", &event.timestamp)
            .name("name", &name)
            .text("input", &input)
            .boolean("input_truncated", cut)
            .raw("source_ref", &source_ref(event))
            .finish()
    });

    let observed_boundary = json::Object::new()
        .name("source_id", source_id)
        .number("byte_end", entry.committed_byte_end)
        .number("source_size", entry.size)
        .finish();
    let next_cursor = json::Object::new()
        .name("source_id", source_id)
        .number("after_byte", entry.committed_byte_end)
        .finish();
    let mut output = json::Object::new();
    output
        .name("disposition", resume_disposition(entry))
        .name("mode", "resume_brief")
        .name("source_id", source_id)
        .name("source_path", source_path)
        .name("session_id", &current.session)
        .name("latest_turn_id", &current.turn)
        .name("current_turn_state", current_turn_state(current))
        .raw(
            "cwd",
            &json::strings(current.cwd.iter().map(String::as_str)),
        )
        .raw("observed_boundary", &observed_boundary);
    freshness_fields(&mut output, boundary_reached, freshness);
    output
        .raw("compactions", &compactions_json(scanned))
        .number("human_messages_seen", scanned.humans_seen)
        .number("human_messages_returned", human_count)
        .number("copies_skipped", scanned.copies_skipped)
        .number("imported_skipped", scanned.imported_skipped)
        .raw("human_messages", &json::array(humans))
        .raw("latest_assistant_texts", &json::array(texts))
        .raw("latest_commands", &json::array(commands))
        .raw("next_cursor", &next_cursor);
    let mut text = output.finish();
    text.push('\n');
    text
}

/// How the transcript stands against the result: `source_freshness` and the bytes the corpus
/// has not read (`corpus::freshness`), and the next step. A transcript still being written is
/// `behind` until the next sync.
fn freshness_fields(
    output: &mut json::Object,
    boundary_reached: bool,
    freshness: corpus::Freshness,
) {
    output
        .name("source_freshness", freshness.as_str())
        .number("unscanned_source_bytes", freshness.unscanned_bytes())
        .name(
            "next_recall",
            match freshness {
                corpus::Freshness::Unreachable => "source_unreachable",
                corpus::Freshness::Current if boundary_reached => "none",
                _ => "sync_then_resume",
            },
        );
}

fn bounded(text: &str, limit: usize) -> (String, bool) {
    let mut characters = text.chars();
    let kept: String = characters.by_ref().take(limit).collect();
    let cut = characters.next().is_some();
    (kept, cut)
}

fn source_ref_json(source_ref: &SourceRef, source_path: &str) -> String {
    json::source_ref(
        &source_ref.source_id,
        source_path,
        Some(source_ref.line),
        source_ref.byte_start,
        source_ref.byte_len,
    )
}

/// A record read from its log, opened by `corpus::open_log` only when it is the log the corpus
/// read.
fn original_fields(log: Option<&mut File>, source_ref: &SourceRef) -> Option<Vec<Field>> {
    if source_ref.byte_len == 0 || source_ref.byte_len > MAX_ORIGINAL_RECORD_BYTES {
        return None;
    }
    let file = log?;
    file.seek(io::SeekFrom::Start(source_ref.byte_start)).ok()?;
    let mut bytes = vec![0; source_ref.byte_len as usize];
    file.read_exact(&mut bytes).ok()?;
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    parse_record(&bytes, source_ref.byte_start).ok()
}

/// Visible reply text: Claude `text` blocks and Codex `output_text` blocks, not thinking.
fn assistant_text(fields: &[Field]) -> String {
    let mut text = String::new();
    for prefix in ["/message/content/", "/payload/content/"] {
        for field in fields {
            let Some(index) = field
                .path
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix("/type"))
            else {
                continue;
            };
            if !matches!(field.value.as_str(), "text" | "output_text") {
                continue;
            }
            let wanted = format!("{}{}/text", prefix, index);
            if let Some(value) = fields.iter().find(|candidate| candidate.path == wanted) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&value.value);
            }
        }
    }
    text
}

/// Tool calls in a record as (name, input): Claude `tool_use` blocks, Codex function and
/// custom tool calls. Inputs are flattened to `key: value` lines.
fn tool_calls(fields: &[Field]) -> Vec<(String, String)> {
    let mut calls = Vec::new();
    for field in fields {
        let Some(index) = field
            .path
            .strip_prefix("/message/content/")
            .and_then(|rest| rest.strip_suffix("/type"))
        else {
            continue;
        };
        if field.value != "tool_use" {
            continue;
        }
        let prefix = format!("/message/content/{}/", index);
        let name = fields
            .iter()
            .find(|candidate| candidate.path == format!("{}name", prefix))
            .map(|candidate| candidate.value.clone())
            .unwrap_or_default();
        let input_prefix = format!("{}input/", prefix);
        let input = fields
            .iter()
            .filter_map(|candidate| {
                candidate
                    .path
                    .strip_prefix(&input_prefix)
                    .map(|key| format!("{}: {}", key, candidate.value))
            })
            .collect::<Vec<_>>()
            .join("\n");
        calls.push((name, input));
    }
    let payload_type = fields
        .iter()
        .find(|field| field.path == "/payload/type")
        .map(|field| field.value.as_str())
        .unwrap_or("");
    if matches!(
        payload_type,
        "function_call" | "custom_tool_call" | "local_shell_call"
    ) {
        let value = |path: &str| {
            fields
                .iter()
                .find(|field| field.path == path)
                .map(|field| field.value.clone())
        };
        let name = value("/payload/name").unwrap_or_else(|| payload_type.to_string());
        let input = value("/payload/arguments")
            .or_else(|| value("/payload/input"))
            .unwrap_or_else(|| {
                fields
                    .iter()
                    .filter_map(|field| {
                        field
                            .path
                            .strip_prefix("/payload/action/")
                            .map(|key| format!("{}: {}", key, field.value))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        calls.push((name, input));
    }
    calls
}

fn embedded_messages(event: &RecoveryEvent) -> Vec<RecoveryEvent> {
    if !crate::core::carries_replacement_history(Some(&event.event_type)) {
        return Vec::new();
    }
    let fields = body_fields(&event.body);
    let mut result = Vec::new();
    for (path, value) in &fields {
        if !path.ends_with("/role") || !matches!(value.as_str(), "user" | "assistant") {
            continue;
        }
        let prefix = path.strip_suffix("/role").unwrap_or(path);
        let mut body = String::new();
        for (field_path, field_value) in &fields {
            if field_path == &format!("{}/text", prefix)
                || field_path == &format!("{}/message", prefix)
            {
                push_field(&mut body, field_path, field_value);
            }
        }
        if body.is_empty() {
            continue;
        }
        let mut embedded = event.clone();
        embedded.sender = Sender::Unknown;
        embedded.via = "embedded_history".to_string();
        embedded.kind = EventKind::from_str(value);
        embedded.role = value.clone();
        embedded.event_type = "embedded_history_message".to_string();
        if let Some((_, turn_id)) = fields
            .iter()
            .find(|(field_path, _)| field_path == &format!("{}/turn_id", prefix))
        {
            embedded.turn = turn_id.clone();
        }
        embedded.body = body;
        embedded.embedded_history = true;
        result.push(embedded);
    }
    result
}

/// Reads a body of `body_len` bytes, keeping at most `limit` of them; true when it kept fewer.
fn read_body<R: Read>(reader: &mut R, body_len: u64, limit: usize) -> io::Result<(String, bool)> {
    let stored_len = usize::try_from(body_len).unwrap_or(limit).min(limit);
    let mut bytes = vec![0; stored_len];
    reader.read_exact(&mut bytes)?;
    let mut read_truncated = stored_len as u64 != body_len;
    if body_len > stored_len as u64 {
        let mut limited = reader.take(body_len - stored_len as u64);
        io::copy(&mut limited, &mut io::sink())?;
        read_truncated = true;
    }
    let mut separator = [0u8; 1];
    reader.read_exact(&mut separator)?;
    if separator[0] != b'\n' {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "corpus event separator is missing",
        ));
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), read_truncated))
}

fn skip_body_and_separator<R: Read>(reader: &mut R, body_len: u64) -> io::Result<()> {
    let mut limited = reader.take(body_len);
    io::copy(&mut limited, &mut io::sink())?;
    if limited.limit() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "corpus body ended before body_len",
        ));
    }
    let mut separator = [0u8; 1];
    reader.read_exact(&mut separator)?;
    if separator[0] != b'\n' {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "corpus event separator is missing",
        ));
    }
    Ok(())
}

fn snapshot_json(snapshot: &RecoverySnapshot, source_path: &str) -> String {
    let partial = snapshot.reentered;
    let mut output = json::Object::new();
    output
        .name("session_id", &snapshot.session)
        .name("turn_id", &snapshot.turn)
        .boolean("completed", snapshot.completed)
        .raw(
            "cwd",
            &json::strings(snapshot.cwd.iter().map(String::as_str)),
        )
        .raw(
            "repositories",
            &json::strings(snapshot.repositories.iter().map(String::as_str)),
        );
    for (name, seen, events) in [
        ("events", snapshot.events_seen, &snapshot.events),
        ("user_messages", snapshot.users_seen, &snapshot.users),
        (
            "assistant_messages",
            snapshot.assistants_seen,
            &snapshot.assistants,
        ),
        ("commands", snapshot.commands_seen, &snapshot.commands),
        ("outputs", snapshot.outputs_seen, &snapshot.outputs),
    ] {
        output
            .number(&format!("{name}_seen"), seen)
            .number(&format!("{name}_returned"), events.len() as u64)
            .boolean(
                &format!("{name}_truncated"),
                partial || seen > events.len() as u64,
            )
            .raw(name, &events_json(events, source_path));
    }
    output.finish()
}

fn events_json(events: &[Arc<RecoveryEvent>], source_path: &str) -> String {
    json::array(events.iter().map(|event| event_json(event, source_path)))
}

fn event_json(event: &RecoveryEvent, source_path: &str) -> String {
    let source_ref = &event.source_ref;
    json::Object::new()
        .name(
            "event_id",
            &format!("{}:{}", source_ref.source_id, event.event_index),
        )
        .raw("source_ref", &source_ref_json(source_ref, source_path))
        .name("session", &event.session)
        .name("turn", &event.turn)
        .name("kind", event.kind.as_str())
        .name("role", &event.role)
        .name("sender", event.sender.as_str())
        .name("via", &event.via)
        .name("event_type", &event.event_type)
        .name("timestamp", &event.timestamp)
        .name("call_id", &event.call_id)
        .raw("fields", &fields_json(&event.body))
        .boolean("fields_truncated", event.body_truncated)
        .boolean("embedded_history", event.embedded_history)
        .finish()
}

/// The fields a corpus body keeps, in order, each with its path.
fn fields_json(body: &str) -> String {
    json::array(body_fields(body).iter().map(|(path, value)| {
        json::Object::new()
            .name("path", path)
            .text("value", value)
            .finish()
    }))
}

fn sort_by_recency(
    catalog: &BTreeMap<String, SourceEntry>,
    last_event_at: &BTreeMap<String, String>,
    candidates: &mut [String],
    offset_minutes: i64,
) {
    candidates.sort_by(|left, right| {
        candidate_event_order(
            last_event_at.get(right),
            last_event_at.get(left),
            offset_minutes,
        )
        .then_with(|| {
            let modified =
                |id: &String| catalog.get(id).map(|entry| entry.modified_ms).unwrap_or(0);
            modified(right).cmp(&modified(left))
        })
        .then_with(|| left.cmp(right))
    });
}

/// Which of two last-event timestamps is later; one that is absent or cannot be read is the
/// earlier.
fn candidate_event_order(
    left: Option<&String>,
    right: Option<&String>,
    offset_minutes: i64,
) -> std::cmp::Ordering {
    let at = |value: Option<&String>| {
        value.and_then(|timestamp| crate::time::instant(timestamp, offset_minutes))
    };
    at(left).cmp(&at(right))
}

fn candidate_last_event_times(
    root: &Path,
    offset_minutes: i64,
) -> io::Result<BTreeMap<String, String>> {
    let timeline = corpus::timeline_catalog(root)?;
    let mut times = BTreeMap::new();
    for (source_id, runs) in timeline {
        for timestamp in runs
            .iter()
            .filter_map(|run| run.bucket.last.as_ref())
            .map(|reference| reference.timestamp.as_str())
            .filter(|timestamp| !timestamp.is_empty())
        {
            let replace = times
                .get(&source_id)
                .map(|current| {
                    candidate_event_order(
                        Some(&timestamp.to_string()),
                        Some(current),
                        offset_minutes,
                    ) == std::cmp::Ordering::Greater
                })
                .unwrap_or(true);
            if replace {
                times.insert(source_id.clone(), timestamp.to_string());
            }
        }
    }
    Ok(times)
}

fn print_ambiguous(
    catalog: &BTreeMap<String, SourceEntry>,
    last_event_at: &BTreeMap<String, String>,
    candidates: &[String],
    requested_session: Option<&str>,
) -> io::Result<()> {
    let listed = candidates.iter().take(50).filter_map(|source_id| {
        let entry = catalog.get(source_id)?;
        Some(
            json::Object::new()
                .name("source_id", source_id)
                .name("source_path", &entry.path)
                .name("last_session", &entry.last_session)
                .optional(
                    "last_event_at",
                    last_event_at.get(source_id).map(String::as_str),
                )
                .number("modified_ms", entry.modified_ms)
                .number("synced_at_ms", entry.synced_at_ms)
                .name("disposition", &entry.disposition)
                .finish(),
        )
    });
    output::write_line(
        &json::Object::new()
            .name("disposition", "ambiguous_current_session")
            .name("mode", "resume")
            .optional("requested_session", requested_session)
            .name(
                "candidate_order",
                "last_event_at_desc_then_modified_ms_desc",
            )
            .number("candidate_count", candidates.len() as u64)
            .number("candidates_returned", candidates.len().min(50) as u64)
            .boolean("candidates_truncated", candidates.len() > 50)
            .raw("candidates", &json::array(listed))
            .finish(),
    )
}

fn print_not_found(kind: &str, value: &str) -> io::Result<()> {
    output::write_line(
        &json::Object::new()
            .name("disposition", "not_found")
            .name("mode", "resume")
            .name(kind, value)
            .finish(),
    )
}

fn resume_disposition(entry: &SourceEntry) -> &'static str {
    if corpus::source_is_complete(entry) {
        "ready"
    } else {
        "incomplete"
    }
}

fn current_turn_state(current: &RecoverySnapshot) -> &'static str {
    if current.completed {
        "completed"
    } else {
        "active"
    }
}

#[cfg(test)]
mod tests {
    use super::{
        current_turn_state, embedded_messages, read_body, resume_disposition, scan_source,
    };
    use crate::core::{EventKind, RecoveryEvent, RecoverySnapshot, SourceRef};
    use crate::corpus::SourceEntry;
    use crate::format::{event_header_line, EventHeader};
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::Path;

    #[test]
    fn a_body_read_whole_is_not_truncated_by_its_text() {
        // Text that happens to contain the marker is not a cut; the header says what was cut.
        let mut body = String::new();
        crate::format::push_field(
            &mut body,
            "/output",
            &format!("abc{}", crate::core::PROJECTION_BOUND_MARKER),
        );
        let mut reader = std::io::Cursor::new(format!("{}\n", body).into_bytes());
        let (read, truncated) =
            read_body(&mut reader, body.len() as u64, super::MAX_RESUME_BODY_BYTES)
                .expect("read body");
        assert_eq!(read, body);
        assert!(!truncated);
    }

    #[test]
    fn completed_turn_with_appended_source_is_ready() {
        let entry = SourceEntry {
            checkpoint_valid: true,
            committed_byte_end: 10,
            size: 10,
            disposition: "source_appended".to_string(),
            ..SourceEntry::default()
        };
        assert_eq!(resume_disposition(&entry), "ready");
    }

    #[test]
    fn active_turn_at_observed_boundary_is_ready_with_tail_continuation() {
        let current = RecoverySnapshot::default();
        let entry = SourceEntry {
            checkpoint_valid: true,
            committed_byte_end: 10,
            size: 10,
            disposition: "source_appended".to_string(),
            ..SourceEntry::default()
        };
        assert_eq!(resume_disposition(&entry), "ready");
        assert!(crate::corpus::source_is_complete(&entry));
        assert_eq!(current_turn_state(&current), "active");
    }

    #[test]
    fn compaction_history_fields_keep_multiline_and_index_boundaries() {
        let event = RecoveryEvent {
            event_index: 7,
            source_ref: SourceRef::default(),
            session: "session".to_string(),
            turn: "turn".to_string(),
            role: String::new(),
            kind: EventKind::Unknown,
            event_type: "compacted".to_string(),
            timestamp: String::new(),
            cwd: String::new(),
            repository: String::new(),
            call_id: String::new(),
            body: concat!(
                "/replacement_history/1/role\t4\tuser\n",
                "/replacement_history/1/text\t10\tfirst\nline\n",
                "/replacement_history/10/role\t9\tassistant\n",
                "/replacement_history/10/text\t3\tten\n",
            )
            .to_string(),
            body_truncated: false,
            embedded_history: false,
            sender: crate::core::Sender::Unknown,
            via: String::new(),
        };
        let embedded = embedded_messages(&event);
        assert_eq!(embedded.len(), 2);
        assert_eq!(embedded[0].kind, EventKind::User);
        assert!(embedded[0].body.contains("first\nline"));
        assert!(!embedded[0].body.contains("ten"));
        assert_eq!(embedded[1].kind, EventKind::Assistant);
    }

    #[test]
    fn recent_resume_window_keeps_the_latest_turn() {
        let root =
            std::env::temp_dir().join(format!("ebira-resume-recent-window-{}", std::process::id()));
        let corpus = root.join("segments");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&corpus).expect("create corpus directory");
        write_event(
            &corpus.join("source--rewrite-1-90.corpus"),
            0,
            "t0",
            "unknown",
            "turn_started",
        );
        write_event(
            &corpus.join("source--90-180.corpus"),
            1,
            "t1",
            "user",
            "user_message",
        );
        write_event(
            &corpus.join("source--90-180.corpus"),
            2,
            "t1",
            "unknown",
            "turn_complete",
        );
        write_event(
            &corpus.join("source--180-270.corpus"),
            3,
            "t2",
            "user",
            "user_message",
        );

        let scanned = scan_source(&root, "source", None).expect("scan recent window");
        assert_eq!(scanned.current.expect("current turn").turn, "t2");
        assert_eq!(scanned.previous.expect("previous turn").turn, "t1");
        fs::remove_dir_all(&root).expect("remove test corpus");
    }

    fn write_event(path: &Path, event_index: u64, turn: &str, kind: &str, event_type: &str) {
        let body = b"/message\t5\tvalue\n";
        let header = EventHeader {
            event_index,
            source_id: "source".to_string(),
            line: event_index + 1,
            session: "session".to_string(),
            turn: turn.to_string(),
            kind: kind.to_string(),
            event_type: event_type.to_string(),
            body_len: body.len() as u64,
            ..EventHeader::default()
        };
        let mut file = if path.exists() {
            File::options()
                .append(true)
                .open(path)
                .expect("open corpus segment")
        } else {
            File::create(path).expect("create corpus segment")
        };
        file.write_all(event_header_line(&header).as_bytes())
            .expect("write event header");
        file.write_all(body).expect("write event body");
        file.write_all(b"\n").expect("write event separator");
    }
}
