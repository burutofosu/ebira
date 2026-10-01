use crate::core::{
    human_text_from_body, is_tool_call, EventKind, RecoveryEvent, RecoverySnapshot, RecoveryState,
    Sender, SourceRef,
};
use crate::corpus::{self, SourceEntry};
use crate::format::{body_fields, json_string, parse_event_header, push_field};
use crate::jsonl::{parse_record, Field};
use std::collections::{BTreeMap, BTreeSet};
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
    humans: Vec<Arc<RecoveryEvent>>,
    humans_seen: u64,
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
                print_not_found("source_id", source_id);
                return Ok(());
            }
            source_id.to_string()
        }
        None => {
            let candidates = if let Some(session) = requested_session {
                session_candidates(&catalog, &last_event_at, session, offset_minutes)
            } else {
                {
                    let mut all = catalog.keys().cloned().collect::<Vec<_>>();
                    sort_by_recency(&catalog, &last_event_at, &mut all, offset_minutes);
                    all
                }
            };
            if candidates.is_empty() {
                if let Some(session) = requested_session {
                    print_not_found("session", session);
                } else {
                    print_not_found("source_id", "");
                }
                return Ok(());
            }
            let chosen = requested_session
                .and_then(|session| main_transcript(&catalog, &candidates, session));
            match chosen {
                Some(source_id) => source_id,
                None if candidates.len() == 1 => candidates[0].clone(),
                None => {
                    print_ambiguous(&catalog, &last_event_at, &candidates, requested_session);
                    return Ok(());
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
            print_not_found("session", session);
        } else {
            print_not_found("source_id", &source_id);
        }
        return Ok(());
    };
    if brief {
        print!("{}", brief_json(&source_id, entry, &current, &scanned));
        return Ok(());
    }

    let boundary_reached = corpus::source_is_complete(entry);
    let disposition = resume_disposition(entry);
    let source_path = &entry.path;
    let latest_user = current
        .users
        .iter()
        .rev()
        .find(|event| event.sender == Sender::Human)
        .or_else(|| current.users.last());
    let latest_event = current.events.last();
    let completion = completion_state(&current, entry);
    let current_turn_state = current_turn_state(&current);
    let mut output = String::new();
    output.push('{');
    field(&mut output, "disposition", disposition, true);
    field(&mut output, "mode", "resume", false);
    field(&mut output, "source_id", &source_id, false);
    field(&mut output, "source_path", source_path, false);
    field(&mut output, "source_disposition", &entry.disposition, false);
    field(&mut output, "completion_state", completion, false);
    field(&mut output, "current_turn_state", current_turn_state, false);
    field(
        &mut output,
        "source_boundary_state",
        if boundary_reached { "reached" } else { "open" },
        false,
    );
    output.push_str(",\"observed_boundary\":{");
    field(&mut output, "source_id", &source_id, true);
    number(&mut output, "byte_end", entry.committed_byte_end);
    number(&mut output, "source_size", entry.size);
    number(&mut output, "last_line", entry.last_line);
    number(&mut output, "event_count", entry.event_count);
    output.push('}');
    field(
        &mut output,
        "next_recall",
        if boundary_reached {
            "sync_tail_after_observed_boundary"
        } else {
            "sync_source_boundary_then_resume"
        },
        false,
    );
    boolean(&mut output, "checkpoint_valid", entry.checkpoint_valid);
    number(&mut output, "committed_byte_end", entry.committed_byte_end);
    number(&mut output, "source_size", entry.size);
    field(&mut output, "session_id", &current.session, false);
    field(&mut output, "latest_turn_id", &current.turn, false);
    number(&mut output, "matched_events", scanned.matched_events);
    output.push_str(",\"current\":");
    output.push_str(&snapshot_json(&current, source_path));
    output.push_str(",\"latest_user_message\":");
    output.push_str(
        latest_user
            .map(|event| event_json(event, source_path))
            .unwrap_or_else(|| "null".to_string())
            .as_str(),
    );
    output.push_str(",\"latest_human_message\":");
    output.push_str(
        scanned
            .humans
            .last()
            .map(|event| event_json(event, source_path))
            .unwrap_or_else(|| "null".to_string())
            .as_str(),
    );
    output.push_str(",\"compactions\":");
    output.push_str(&compactions_json(&scanned));
    output.push_str(",\"latest_event\":");
    output.push_str(
        latest_event
            .map(|event| event_json(event, source_path))
            .unwrap_or_else(|| "null".to_string())
            .as_str(),
    );
    output.push_str(",\"previous_turn\":");
    output.push_str(
        scanned
            .previous
            .as_ref()
            .map(|turn| snapshot_json(turn, source_path))
            .unwrap_or_else(|| "null".to_string())
            .as_str(),
    );
    output.push_str(",\"next_cursor\":{");
    field(&mut output, "source_id", &source_id, true);
    number(
        &mut output,
        "after_event_index",
        scanned
            .latest_event_index
            .unwrap_or(entry.event_count.saturating_sub(1)),
    );
    number(&mut output, "after_byte", entry.committed_byte_end);
    output.push_str("}}\n");
    print!("{}", output);
    Ok(())
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
            let (body, read_truncated) = read_body(&mut reader, header.body_len)?;
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
            if event.sender == Sender::Human {
                humans_seen += 1;
                keep_last(&mut humans, &event, KEPT_HUMAN_MESSAGES);
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
        compactions,
        last_compaction_at,
        summaries,
        recent_assistants,
        recent_commands,
    })
}

/// A session id names the main transcript and the transcripts of the agents it started:
/// Claude Code subagent transcripts under `subagents/`, and Codex child threads that record
/// their parent's session. The main transcript is the one outside `subagents/` whose file
/// name carries the session id (`<session-id>.jsonl`, `rollout-<time>-<thread-id>.jsonl`).
fn main_transcript(
    catalog: &BTreeMap<String, SourceEntry>,
    candidates: &[String],
    session: &str,
) -> Option<String> {
    let main = candidates
        .iter()
        .filter(|source_id| {
            catalog
                .get(*source_id)
                .map(|entry| {
                    let path = Path::new(&entry.path);
                    let named = path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .is_some_and(|stem| stem.contains(session));
                    named && !entry.path.replace('\\', "/").contains("/subagents/")
                })
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    (main.len() == 1).then(|| main[0].clone())
}

fn compactions_json(scanned: &ScanResult) -> String {
    let mut output = String::from("{");
    output.push_str(&format!("\"count\":{}", scanned.compactions));
    field(&mut output, "last_at", &scanned.last_compaction_at, false);
    number(&mut output, "summaries_seen", scanned.summaries);
    output.push('}');
    output
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
    let mut output = String::from("{");
    field(&mut output, "disposition", resume_disposition(entry), true);
    field(&mut output, "mode", "resume_brief", false);
    field(&mut output, "source_id", source_id, false);
    field(&mut output, "source_path", source_path, false);
    field(&mut output, "session_id", &current.session, false);
    field(&mut output, "latest_turn_id", &current.turn, false);
    field(
        &mut output,
        "current_turn_state",
        current_turn_state(current),
        false,
    );
    output.push_str(",\"cwd\":");
    output.push_str(&string_array(&current.cwd));
    output.push_str(",\"observed_boundary\":{");
    field(&mut output, "source_id", source_id, true);
    number(&mut output, "byte_end", entry.committed_byte_end);
    number(&mut output, "source_size", entry.size);
    output.push('}');
    field(
        &mut output,
        "next_recall",
        if boundary_reached {
            "sync_tail_after_observed_boundary"
        } else {
            "sync_source_boundary_then_resume"
        },
        false,
    );
    output.push_str(",\"compactions\":");
    output.push_str(&compactions_json(scanned));

    let mut budget = BRIEF_HUMAN_TEXT_BUDGET;
    let mut humans = Vec::new();
    for event in scanned.humans.iter().rev().take(BRIEF_HUMAN_MESSAGES) {
        let (text, images) = human_text_from_body(&event.body);
        let (text, cut) = bounded(&text, BRIEF_HUMAN_TEXT_LIMIT.min(budget));
        if text.is_empty() && images == 0 {
            break;
        }
        budget = budget.saturating_sub(text.chars().count());
        humans.push((Arc::clone(event), text, cut, images));
        if budget == 0 {
            break;
        }
    }
    humans.reverse();
    number(&mut output, "human_messages_seen", scanned.humans_seen);
    number(&mut output, "human_messages_returned", humans.len() as u64);
    output.push_str(",\"human_messages\":[");
    for (index, (event, text, cut, images)) in humans.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push('{');
        field(&mut output, "timestamp", &event.timestamp, true);
        field(&mut output, "via", &event.via, false);
        field(&mut output, "text", text, false);
        boolean(&mut output, "text_truncated", *cut);
        number(&mut output, "images", *images as u64);
        output.push_str(",\"source_ref\":");
        output.push_str(&source_ref_json(&event.source_ref, source_path));
        output.push('}');
    }
    output.push(']');

    output.push_str(",\"latest_assistant_texts\":[");
    let mut written = 0usize;
    let mut texts = Vec::new();
    for event in scanned.recent_assistants.iter().rev() {
        if texts.len() == BRIEF_ASSISTANT_TEXTS {
            break;
        }
        let text = original_fields(source_path, &event.source_ref)
            .map(|fields| assistant_text(&fields))
            .unwrap_or_default();
        if !text.trim().is_empty() {
            texts.push((Arc::clone(event), text));
        }
    }
    for (event, text) in texts.iter().rev() {
        let (text, cut) = bounded(text.trim(), BRIEF_ASSISTANT_TEXT_LIMIT);
        if written != 0 {
            output.push(',');
        }
        written += 1;
        output.push('{');
        field(&mut output, "timestamp", &event.timestamp, true);
        field(&mut output, "text", &text, false);
        boolean(&mut output, "text_truncated", cut);
        output.push_str(",\"source_ref\":");
        output.push_str(&source_ref_json(&event.source_ref, source_path));
        output.push('}');
    }
    output.push(']');

    output.push_str(",\"latest_commands\":[");
    for (index, event) in scanned.recent_commands.iter().enumerate() {
        let calls = original_fields(source_path, &event.source_ref)
            .map(|fields| tool_calls(&fields))
            .unwrap_or_default();
        let (name, input) = calls
            .into_iter()
            .last()
            .unwrap_or_else(|| (event.event_type.clone(), event.body.clone()));
        let (input, cut) = bounded(&input, BRIEF_COMMAND_LIMIT);
        if index != 0 {
            output.push(',');
        }
        output.push('{');
        field(&mut output, "timestamp", &event.timestamp, true);
        field(&mut output, "name", &name, false);
        field(&mut output, "input", &input, false);
        boolean(&mut output, "input_truncated", cut);
        output.push_str(",\"source_ref\":");
        output.push_str(&source_ref_json(&event.source_ref, source_path));
        output.push('}');
    }
    output.push(']');
    output.push_str(",\"next_cursor\":{");
    field(&mut output, "source_id", source_id, true);
    number(&mut output, "after_byte", entry.committed_byte_end);
    output.push_str("}}\n");
    output
}

fn bounded(text: &str, limit: usize) -> (String, bool) {
    let mut characters = text.chars();
    let kept: String = characters.by_ref().take(limit).collect();
    let cut = characters.next().is_some();
    (kept, cut)
}

fn source_ref_json(source_ref: &SourceRef, source_path: &str) -> String {
    format!(
        "{{\"source_id\":{},\"source_path\":{},\"line\":{},\"byte_start\":{},\"byte_len\":{}}}",
        json_string(&source_ref.source_id),
        json_string(source_path),
        source_ref.line,
        source_ref.byte_start,
        source_ref.byte_len,
    )
}

fn original_fields(source_path: &str, source_ref: &SourceRef) -> Option<Vec<Field>> {
    if source_ref.byte_len == 0 || source_ref.byte_len > MAX_ORIGINAL_RECORD_BYTES {
        return None;
    }
    let mut file = File::open(source_path).ok()?;
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

fn read_body<R: Read>(reader: &mut R, body_len: u64) -> io::Result<(String, bool)> {
    let stored_len = usize::try_from(body_len)
        .unwrap_or(MAX_RESUME_BODY_BYTES)
        .min(MAX_RESUME_BODY_BYTES);
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
    let mut output = String::new();
    output.push('{');
    field(&mut output, "session_id", &snapshot.session, true);
    field(&mut output, "turn_id", &snapshot.turn, false);
    number(&mut output, "events_seen", snapshot.events_seen);
    number(&mut output, "events_returned", snapshot.events.len() as u64);
    boolean(
        &mut output,
        "events_truncated",
        partial || snapshot.events_seen > snapshot.events.len() as u64,
    );
    number(&mut output, "user_messages_seen", snapshot.users_seen);
    number(
        &mut output,
        "user_messages_returned",
        snapshot.users.len() as u64,
    );
    boolean(
        &mut output,
        "user_messages_truncated",
        partial || snapshot.users_seen > snapshot.users.len() as u64,
    );
    number(
        &mut output,
        "assistant_messages_seen",
        snapshot.assistants_seen,
    );
    number(
        &mut output,
        "assistant_messages_returned",
        snapshot.assistants.len() as u64,
    );
    boolean(
        &mut output,
        "assistant_messages_truncated",
        partial || snapshot.assistants_seen > snapshot.assistants.len() as u64,
    );
    number(&mut output, "commands_seen", snapshot.commands_seen);
    number(
        &mut output,
        "commands_returned",
        snapshot.commands.len() as u64,
    );
    boolean(
        &mut output,
        "commands_truncated",
        partial || snapshot.commands_seen > snapshot.commands.len() as u64,
    );
    number(&mut output, "outputs_seen", snapshot.outputs_seen);
    number(
        &mut output,
        "outputs_returned",
        snapshot.outputs.len() as u64,
    );
    boolean(
        &mut output,
        "outputs_truncated",
        partial || snapshot.outputs_seen > snapshot.outputs.len() as u64,
    );
    boolean(&mut output, "completed", snapshot.completed);
    output.push_str(",\"cwd\":");
    output.push_str(&string_array(&snapshot.cwd));
    output.push_str(",\"repositories\":");
    output.push_str(&string_array(&snapshot.repositories));
    output.push_str(",\"user_messages\":");
    output.push_str(&events_json(&snapshot.users, source_path));
    output.push_str(",\"assistant_messages\":");
    output.push_str(&events_json(&snapshot.assistants, source_path));
    output.push_str(",\"commands\":");
    output.push_str(&events_json(&snapshot.commands, source_path));
    output.push_str(",\"outputs\":");
    output.push_str(&events_json(&snapshot.outputs, source_path));
    output.push_str(",\"events\":");
    output.push_str(&events_json(&snapshot.events, source_path));
    output.push('}');
    output
}

fn events_json(events: &[Arc<RecoveryEvent>], source_path: &str) -> String {
    let mut output = String::from("[");
    for (index, event) in events.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str(&event_json(event, source_path));
    }
    output.push(']');
    output
}

fn event_json(event: &RecoveryEvent, source_path: &str) -> String {
    let source_ref = &event.source_ref;
    format!(
        "{{\"event_id\":{},\"source_ref\":{{\"source_id\":{},\"source_path\":{},\"line\":{},\"byte_start\":{},\"byte_len\":{}}},\"session\":{},\"turn\":{},\"kind\":{},\"role\":{},\"sender\":{},\"via\":{},\"event_type\":{},\"timestamp\":{},\"call_id\":{},\"body\":{},\"body_truncated\":{},\"embedded_history\":{}}}",
        json_string(&format!("{}:{}", source_ref.source_id, event.event_index)),
        json_string(&source_ref.source_id),
        json_string(source_path),
        source_ref.line,
        source_ref.byte_start,
        source_ref.byte_len,
        json_string(&event.session),
        json_string(&event.turn),
        json_string(event.kind.as_str()),
        json_string(&event.role),
        json_string(event.sender.as_str()),
        json_string(&event.via),
        json_string(&event.event_type),
        json_string(&event.timestamp),
        json_string(&event.call_id),
        json_string(&event.body),
        event.body_truncated,
        event.embedded_history,
    )
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

fn session_candidates(
    catalog: &BTreeMap<String, SourceEntry>,
    last_event_at: &BTreeMap<String, String>,
    session: &str,
    offset_minutes: i64,
) -> Vec<String> {
    let mut candidates = Vec::new();
    for entry in catalog.values() {
        if corpus::source_declares_session(entry, session) {
            candidates.push(entry.source_id.clone());
        }
    }
    candidates.sort();
    candidates.dedup();
    sort_by_recency(catalog, last_event_at, &mut candidates, offset_minutes);
    candidates
}

fn print_ambiguous(
    catalog: &BTreeMap<String, SourceEntry>,
    last_event_at: &BTreeMap<String, String>,
    candidates: &[String],
    requested_session: Option<&str>,
) {
    let mut output = String::new();
    output.push('{');
    field(
        &mut output,
        "disposition",
        "ambiguous_current_session",
        true,
    );
    field(&mut output, "mode", "resume", false);
    if let Some(session) = requested_session {
        field(&mut output, "requested_session", session, false);
    }
    field(
        &mut output,
        "candidate_order",
        "last_event_at_desc_then_modified_ms_desc",
        false,
    );
    number(&mut output, "candidate_count", candidates.len() as u64);
    number(
        &mut output,
        "candidates_returned",
        candidates.len().min(50) as u64,
    );
    boolean(&mut output, "candidates_truncated", candidates.len() > 50);
    output.push_str(",\"candidates\":[");
    for (index, source_id) in candidates.iter().take(50).enumerate() {
        if index != 0 {
            output.push(',');
        }
        if let Some(entry) = catalog.get(source_id) {
            let last_event_at = last_event_at
                .get(source_id)
                .map(|timestamp| json_string(timestamp))
                .unwrap_or_else(|| "null".to_string());
            output.push_str(&format!(
                "{{\"source_id\":{},\"source_path\":{},\"last_session\":{},\"last_event_at\":{},\"modified_ms\":{},\"synced_at_ms\":{},\"disposition\":{}}}",
                json_string(source_id),
                json_string(&entry.path),
                json_string(&entry.last_session),
                last_event_at,
                entry.modified_ms,
                entry.synced_at_ms,
                json_string(&entry.disposition),
            ));
        }
    }
    output.push_str("]}\n");
    print!("{}", output);
}

fn print_not_found(kind: &str, value: &str) {
    println!(
        "{{\"disposition\":\"not_found\",\"mode\":\"resume\",\"{}\":{}}}",
        kind,
        json_string(value)
    );
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

fn completion_state(current: &RecoverySnapshot, entry: &SourceEntry) -> &'static str {
    if current.completed {
        "completed"
    } else if corpus::source_is_complete(entry) {
        "source_boundary_reached"
    } else {
        "open"
    }
}

fn field(output: &mut String, key: &str, value: &str, first: bool) {
    if !first {
        output.push(',');
    }
    output.push_str(&format!("\"{}\":{}", key, json_string(value)));
}

fn number(output: &mut String, key: &str, value: u64) {
    output.push_str(&format!(",\"{}\":{}", key, value));
}

fn boolean(output: &mut String, key: &str, value: bool) {
    output.push_str(&format!(",\"{}\":{}", key, value));
}

fn string_array(values: &BTreeSet<String>) -> String {
    let mut output = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str(&json_string(value));
    }
    output.push(']');
    output
}

#[cfg(test)]
mod tests {
    use super::{
        completion_state, current_turn_state, embedded_messages, read_body, resume_disposition,
        scan_source,
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
        let (read, truncated) = read_body(&mut reader, body.len() as u64).expect("read body");
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
        assert_eq!(
            completion_state(&current, &entry),
            "source_boundary_reached"
        );
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
