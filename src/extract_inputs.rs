//! Read logical events and source fields for extraction.
use crate::corpus::{self, Freshness, SourceEntry};
use crate::format::{self, EventHeader};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

type Fields = Vec<(String, String)>;

const MAX_EVENT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Input {
    pub event_id: String,
    /// JSON-encoded selected fields and, when requested, same-call context.
    pub state: String,
    pub metadata: Value,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn strings(value: &Value, key: &str) -> io::Result<BTreeSet<String>> {
    let Some(v) = value.get(key) else {
        return Ok(BTreeSet::new());
    };
    let a = v
        .as_array()
        .ok_or_else(|| invalid(format!("scope.{key} must be an array")))?;
    a.iter()
        .map(|v| {
            v.as_str()
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| invalid(format!("scope.{key} must contain nonempty strings")))
        })
        .collect()
}

/// Explicit sessions/source IDs use intersection semantics when both are supplied.
/// `event_ids` narrows targets; pairing uses the selected scope.
pub fn gather(root: &Path, scope: &Value, context: &Value) -> io::Result<Vec<Input>> {
    if !scope.is_object() {
        return Err(invalid("scope must be an object"));
    }
    let sessions = strings(scope, "sessions")?;
    let sources = strings(scope, "source_ids")?;
    let events = strings(scope, "event_ids")?;
    let all = bool_option(scope, "all", false)?;
    let include_children = bool_option(scope, "include_children", false)?;
    if sessions.is_empty() && sources.is_empty() && !all {
        return Err(invalid(
            "scope requires sessions, source_ids, or explicit all:true",
        ));
    }
    if !context.is_null() && !context.is_object() {
        return Err(invalid("context must be an object"));
    }
    let mode = match context.get("mode") {
        None => "event",
        Some(v) => v
            .as_str()
            .ok_or_else(|| invalid("context.mode must be a string"))?,
    };
    if !matches!(mode, "event" | "paired") {
        return Err(invalid("context.mode must be event or paired"));
    }
    let _lock = corpus::lock_shared(root)?;
    corpus::require_corpus(root)?;
    let catalog = corpus::source_catalog(root)?;
    for id in &sources {
        if !catalog.contains_key(id) {
            return Err(invalid(format!("unknown source_id: {id}")));
        }
    }
    let mut session_sources = BTreeSet::new();
    for session in &sessions {
        let matches = corpus::session_sources(&catalog, session);
        if matches.sources.is_empty() {
            return Err(invalid(format!("unknown session: {session}")));
        }
        session_sources.extend(matches.sources.into_iter().map(|s| s.source_id.clone()));
    }
    let mut loaded = Vec::new();
    for entry in catalog.values().filter(|e| {
        (sources.is_empty() || sources.contains(&e.source_id))
            && (sessions.is_empty() || session_sources.contains(&e.source_id))
    }) {
        let files = corpus::source_files(root, &entry.source_id)?;
        if !corpus::source_projection_complete(entry, &files)? {
            return Err(invalid(format!(
                "incomplete corpus projection for source {}; sync first",
                entry.source_id
            )));
        }
        let (mut raw, freshness) = corpus::open_log(entry);
        let ancestry = source_child(entry, raw.as_mut());
        if !include_children && ancestry == Some(true) {
            continue;
        }
        let source_input_start = loaded.len();
        for path in files {
            let mut reader = BufReader::new(File::open(&path)?);
            while let Some((header, fields)) = read_event(&mut reader)? {
                if header.source_id != entry.source_id {
                    return Err(invalid("segment source mismatch"));
                }
                if header
                    .byte_start
                    .checked_add(header.byte_len)
                    .is_none_or(|end| end > entry.committed_byte_end)
                {
                    return Err(invalid("event reference exceeds committed source bytes"));
                }
                if !sessions.is_empty() && !sessions.contains(&header.session) {
                    continue;
                }
                if !include_children
                    && matches!(
                        header.via.as_str(),
                        "codex_child" | "claude_subagent" | "subagent_prompt"
                    )
                {
                    continue;
                }
                let mut errors = Vec::new();
                if !include_children && ancestry.is_none() {
                    errors.push("source ancestry unavailable".to_owned());
                }
                if freshness != Freshness::Current {
                    errors.push(format!("source is {}", freshness.as_str()));
                }
                if !corpus::source_is_complete(entry) {
                    errors.push("source ingestion is incomplete".to_owned());
                }
                if matches!(header.kind.as_str(), "invalid" | "unknown") {
                    errors.push(format!("{} event", header.kind));
                }
                let mut fields = fields;
                let mut hydration = "projected";
                if header.body_cut {
                    match hydrate(&header, &fields, raw.as_mut()) {
                        Ok(full) => {
                            fields = full;
                            hydration = "raw_selected_fields";
                        }
                        Err(e) => errors.push(e.to_string()),
                    }
                }
                let event_id = format!("{}:{}", header.source_id, header.event_index);
                let source_ref = json!({"source_id":header.source_id, "source_path":entry.path,
                    "line":header.line,"byte_start":header.byte_start,"byte_len":header.byte_len});
                let state = json!({"event_id":event_id,"kind":header.kind,"sender":header.sender,
                    "session":header.session,"call_id":header.call_id,
                    "fields":fields.iter().map(|(path,value)|json!({"path":path,"value":value})).collect::<Vec<_>>()});
                let metadata = json!({"source_ref":source_ref,"kind":header.kind,"sender":header.sender,
                    "session":header.session,"turn":header.turn,"call_id":header.call_id,"event_type":header.event_type,
                    "input_complete":errors.is_empty(),"error":errors.join("; "),
                    "input_completeness":{"hydration":hydration,"projected_body_cut":header.body_cut,
                        "freshness":freshness.as_str(),"unscanned_source_bytes":freshness.unscanned_bytes(),
                        "source_ingestion_complete":corpus::source_is_complete(entry),
                        "child_source":ancestry,"context_mode":mode}});
                loaded.push((
                    header,
                    Input {
                        event_id,
                        state: state.to_string(),
                        metadata,
                    },
                ));
            }
        }
        let after = corpus::freshness(entry);
        if after != freshness {
            for (_, input) in &mut loaded[source_input_start..] {
                mark_incomplete(input, "source changed while extraction inputs were read");
                input.metadata["input_completeness"]["freshness_after_read"] =
                    json!(after.as_str());
            }
        }
    }
    // Event index order is stable within a source, independent of segment layout.
    loaded
        .sort_by(|a, b| (&a.0.source_id, a.0.event_index).cmp(&(&b.0.source_id, b.0.event_index)));
    let mut seen = BTreeSet::new();
    for (_, input) in &loaded {
        if !seen.insert(input.event_id.clone()) {
            return Err(invalid("duplicate event_id in corpus"));
        }
    }
    if mode == "paired" {
        pair(&mut loaded)?;
    }
    for id in &events {
        if !seen.contains(id) {
            return Err(invalid(format!("unknown event_id in selected scope: {id}")));
        }
    }
    Ok(loaded
        .into_iter()
        .map(|(_, i)| i)
        .filter(|i| events.is_empty() || events.contains(&i.event_id))
        .collect())
}

fn bool_option(value: &Value, name: &str, default: bool) -> io::Result<bool> {
    value.get(name).map_or(Ok(default), |v| {
        v.as_bool()
            .ok_or_else(|| invalid(format!("scope.{name} must be boolean")))
    })
}

/// Parse the header, exact body length and separator.
fn read_event<R: BufRead>(reader: &mut R) -> io::Result<Option<(EventHeader, Fields)>> {
    let mut line = Vec::new();
    if (&mut *reader)
        .take(64 * 1024)
        .read_until(b'\n', &mut line)?
        == 0
    {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(invalid("unterminated or oversized event header"));
    }
    let h = format::parse_event_header(&line).map_err(invalid)?;
    if h.body_len > MAX_EVENT_BYTES {
        return Err(invalid("projected event exceeds extraction bound"));
    }
    let mut body = vec![0; h.body_len as usize];
    reader.read_exact(&mut body)?;
    let mut separator = [0];
    reader.read_exact(&mut separator)?;
    if separator != *b"\n" {
        return Err(invalid("missing event separator"));
    }
    let body = std::str::from_utf8(&body).map_err(|_| invalid("event body requires UTF-8"))?;
    let fields = format::body_fields(body);
    // Require the parsed fields to reproduce the complete body.
    let mut roundtrip = String::new();
    for (path, value) in &fields {
        format::push_field(&mut roundtrip, path, value);
    }
    if roundtrip != body {
        return Err(invalid("malformed length-delimited event fields"));
    }
    Ok(Some((h, fields)))
}

fn hydrate(
    h: &EventHeader,
    projected: &[(String, String)],
    file: Option<&mut File>,
) -> io::Result<Vec<(String, String)>> {
    let file =
        file.ok_or_else(|| invalid("hydration requires an available, current raw source"))?;
    if h.byte_len > MAX_EVENT_BYTES {
        return Err(invalid("raw record exceeds extraction bound"));
    }
    file.seek(SeekFrom::Start(h.byte_start))?;
    let mut bytes = vec![0; h.byte_len as usize];
    file.read_exact(&mut bytes)?;
    let raw = crate::jsonl::parse_record(&bytes, h.byte_start)
        .map_err(|e| invalid(format!("raw record invalid: {e}")))?;
    let mut seen_paths = BTreeSet::new();
    if projected.iter().any(|(path, _)| !seen_paths.insert(path)) {
        return Err(invalid("ambiguous duplicate projected field path"));
    }
    projected
        .iter()
        .map(|(path, _)| {
            let matches: Vec<_> = raw.iter().filter(|f| f.path == *path).collect();
            if matches.len() != 1 {
                return Err(invalid("missing or ambiguous duplicate raw field path"));
            }
            Ok((path.clone(), matches[0].value.clone()))
        })
        .collect()
}

/// Apply ingestion origin rules; return None if Codex ancestry is unavailable.
/// open_log validates offsets against source rewrites.
fn source_child(entry: &SourceEntry, file: Option<&mut File>) -> Option<bool> {
    if entry.path.replace('\\', "/").contains("/subagents/") {
        return Some(true);
    }
    if entry.app != "codex" {
        return Some(false);
    }
    let file = file?;
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut reader = BufReader::new(file.take(MAX_EVENT_BYTES));
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).ok()?;
    let fields = crate::jsonl::parse_record(&line, 0).ok()?;
    Some(fields.iter().any(|f| {
        (f.path == "/payload/parent_thread_id" && !f.value.is_empty() && f.value != "null")
            || f.path.starts_with("/payload/source/subagent")
    }))
}

fn pair(loaded: &mut [(EventHeader, Input)]) -> io::Result<()> {
    let mut calls: BTreeMap<(String, String, String), Vec<usize>> = BTreeMap::new();
    for (i, (h, _)) in loaded.iter().enumerate() {
        if !h.call_id.is_empty() {
            calls
                .entry((h.source_id.clone(), h.session.clone(), h.call_id.clone()))
                .or_default()
                .push(i);
        }
    }
    let originals: Vec<_> = loaded.iter().map(|(_, i)| i.clone()).collect();
    for indices in calls.values() {
        // Pair exactly one request with one result for each call ID.
        let outputs: Vec<_> = indices
            .iter()
            .copied()
            .filter(|i| loaded[*i].0.kind == "output")
            .collect();
        let requests: Vec<_> = indices
            .iter()
            .copied()
            .filter(|i| {
                let h = &loaded[*i].0;
                h.event_type.ends_with("call") || h.kind == "assistant"
            })
            .collect();
        let valid = outputs.len() == 1 && requests.len() == 1 && outputs[0] != requests[0];
        for i in indices {
            let input = &mut loaded[*i].1;
            if !valid {
                mark_incomplete(input, "missing or ambiguous same-call pair");
                continue;
            }
            let other = if *i == outputs[0] {
                requests[0]
            } else {
                outputs[0]
            };
            if *i != outputs[0] && *i != requests[0] {
                continue;
            }
            let mut state: Value =
                serde_json::from_str(&input.state).map_err(|e| invalid(e.to_string()))?;
            state["paired_event"] = serde_json::from_str(&originals[other].state)
                .map_err(|e| invalid(e.to_string()))?;
            input.state = state.to_string();
            input.metadata["paired_event_id"] = json!(originals[other].event_id);
            input.metadata["paired_source_ref"] = originals[other].metadata["source_ref"].clone();
            if originals[other].metadata["input_complete"] != true {
                mark_incomplete(input, "paired event input incomplete");
            }
        }
    }
    Ok(())
}
fn mark_incomplete(input: &mut Input, reason: &str) {
    input.metadata["input_complete"] = json!(false);
    let old = input.metadata["error"].as_str().unwrap_or("");
    input.metadata["error"] = json!(if old.is_empty() {
        reason.to_owned()
    } else {
        format!("{old}; {reason}")
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn multiline_values_are_one_logical_event() {
        let mut body = String::new();
        format::push_field(&mut body, "/text", "one\ntwo\tthree");
        let h = EventHeader {
            source_id: "src".into(),
            body_len: body.len() as u64,
            ..Default::default()
        };
        let bytes = format!("{}{}\n", format::event_header_line(&h), body);
        let mut reader = Cursor::new(bytes);
        let (_, fields) = read_event(&mut reader).unwrap().unwrap();
        assert_eq!(fields[0].1, "one\ntwo\tthree");
        assert!(read_event(&mut reader).unwrap().is_none());
    }
    #[test]
    fn malformed_body_is_rejected() {
        let body = "/text\t99\tshort\n";
        let h = EventHeader {
            source_id: "src".into(),
            body_len: body.len() as u64,
            ..Default::default()
        };
        assert!(read_event(&mut Cursor::new(format!(
            "{}{}\n",
            format::event_header_line(&h),
            body
        )))
        .is_err());
    }
    #[test]
    fn empty_scope_and_unknown_modes_are_rejected_before_io() {
        assert!(gather(Path::new("/nonexistent"), &json!({}), &json!({})).is_err());
        assert!(gather(
            Path::new("/nonexistent"),
            &json!({"all":true}),
            &json!({"mode":"all"})
        )
        .is_err());
    }
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ebira-extract-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn build(&self, lines: &str, preview: usize) -> (std::path::PathBuf, std::path::PathBuf) {
            let source = self.0.join("log.jsonl");
            let root = self.0.join("corpus");
            std::fs::write(&source, lines).unwrap();
            corpus::build(
                &[source.to_string_lossy().into_owned()],
                &root,
                false,
                preview,
                &[],
            )
            .unwrap();
            (source, root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn raw_hydration_and_pairs_preserve_complete_output() {
        let f = Fixture::new();
        let lines = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec_command\",\"call_id\":\"c\",\"arguments\":\"echo hello\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"c\",\"output\":\"first line\\nsecond line full text\"}}\n");
        let (_, root) = f.build(lines, 4);
        let inputs = gather(&root, &json!({"all":true}), &json!({"mode":"paired"})).unwrap();
        let output = inputs
            .iter()
            .find(|i| i.metadata["kind"] == "output")
            .unwrap();
        assert_eq!(
            output.metadata["input_complete"], true,
            "{}",
            output.metadata
        );
        let state: Value = serde_json::from_str(&output.state).unwrap();
        assert!(state["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["value"] == "first line\nsecond line full text"));
        assert!(state["paired_event"]["fields"].is_array());
        let only = gather(
            &root,
            &json!({"all":true,"event_ids":[output.event_id]}),
            &json!({"mode":"paired"}),
        )
        .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].state, output.state);
    }
    #[test]
    fn rewritten_raw_is_unscored() {
        let f = Fixture::new();
        let (source,root) = f.build("{\"type\":\"user\",\"sessionId\":\"s\",\"message\":{\"role\":\"user\",\"content\":\"original\"}}\n",4);
        std::fs::write(source, "{}\n").unwrap();
        let inputs = gather(&root, &json!({"all":true}), &json!({})).unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].metadata["input_complete"], false);
        assert!(inputs[0].state.contains("original"));
        assert_eq!(
            inputs[0].metadata["input_completeness"]["freshness"],
            "rewritten"
        );
    }
    #[test]
    fn codex_children_are_opt_in() {
        let f = Fixture::new();
        let (_,root) = f.build(concat!("{\"type\":\"session_meta\",\"payload\":{\"id\":\"child\",\"parent_thread_id\":\"parent\"}}\n", "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"child task\"}]}}\n"),4);
        assert!(gather(&root, &json!({"all":true}), &json!({}))
            .unwrap()
            .is_empty());
        assert!(!gather(
            &root,
            &json!({"all":true,"include_children":true}),
            &json!({})
        )
        .unwrap()
        .is_empty());
    }
}
