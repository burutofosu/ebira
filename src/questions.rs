//! Correlate native question tools with their structured answers. Tool output text alone
//! is never evidence that a person answered a question.

use crate::core::{EventKind, Projection, RecordMeta, Sender, SourceOrigin};
use crate::format::{decode_token, encode_token, push_field};
use crate::json;
use crate::jsonl::{parse_record, Field, ScalarKind};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Claude,
    Codex,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Debug)]
struct Pending {
    kind: Kind,
    keys: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct QuestionTracker {
    pending: BTreeMap<String, Pending>,
}

impl QuestionTracker {
    /// A compact, escaped checkpoint, itself escaped as one source-catalog column.
    pub fn encode(&self) -> String {
        self.pending
            .iter()
            .map(|(id, pending)| {
                let mut fields = vec![pending.kind.name().to_string(), encode_token(id)];
                fields.extend(pending.keys.iter().map(|key| encode_token(key)));
                fields.join("\t")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn decode(value: &str) -> Result<Self, String> {
        let mut state = Self::default();
        if value.is_empty() {
            return Ok(state);
        }
        for line in value.split('\n') {
            let fields = line.split('\t').collect::<Vec<_>>();
            if fields.len() < 3 {
                return Err("question checkpoint has fewer than three fields".into());
            }
            let kind = match fields[0] {
                "claude" => Kind::Claude,
                "codex" => Kind::Codex,
                _ => return Err("question checkpoint has an unknown tool kind".into()),
            };
            let id = decode_token(fields[1])?;
            let keys = fields[2..]
                .iter()
                .map(|key| decode_token(key))
                .collect::<Result<Vec<_>, _>>()?;
            if id.is_empty()
                || keys.iter().any(String::is_empty)
                || keys.iter().collect::<BTreeSet<_>>().len() != keys.len()
                || state.pending.contains_key(&id)
            {
                return Err("question checkpoint has empty or duplicate identifiers".into());
            }
            state.pending.insert(id, Pending { kind, keys });
        }
        Ok(state)
    }

    pub fn project(
        &mut self,
        origin: SourceOrigin,
        fields: &[Field],
        meta: &mut RecordMeta,
    ) -> Option<Projection> {
        let kind = match origin {
            SourceOrigin::ClaudeMain | SourceOrigin::ClaudeSubagent => Kind::Claude,
            SourceOrigin::CodexThread | SourceOrigin::CodexChild | SourceOrigin::CodexExec => {
                Kind::Codex
            }
            SourceOrigin::Generic => return None,
        };
        let mut body = String::new();
        let mut answered_calls = Vec::new();
        match kind {
            Kind::Claude => {
                let blocks = indices(fields, "/message/content");
                if text(fields, "/type") == Some("assistant") {
                    for index in &blocks {
                        let base = format!("/message/content/{index}");
                        if text(fields, &format!("{base}/type")) != Some("tool_use") {
                            continue;
                        }
                        if let Some(id) = text(fields, &format!("{base}/id")) {
                            self.pending.remove(id);
                            if text(fields, &format!("{base}/name")) == Some("AskUserQuestion") {
                                self.remember(
                                    id,
                                    kind,
                                    keys(fields, &format!("{base}/input/questions"), "question"),
                                );
                            }
                        }
                    }
                } else if text(fields, "/type") == Some("user") {
                    let results = blocks
                        .into_iter()
                        .filter(|index| {
                            text(fields, &format!("/message/content/{index}/type"))
                                == Some("tool_result")
                        })
                        .collect::<Vec<_>>();
                    for index in &results {
                        let base = format!("/message/content/{index}");
                        let Some(id) = text(fields, &format!("{base}/tool_use_id")) else {
                            continue;
                        };
                        let Some(pending) = self.take(id, kind) else {
                            continue;
                        };
                        if flag(fields, &format!("{base}/is_error")) || flag(fields, "/isMeta") {
                            continue;
                        }
                        let mut answers = if results.len() == 1 {
                            answer_values(fields, "/toolUseResult/answers", &pending.keys, false)
                        } else {
                            Vec::new()
                        };
                        if answers.is_empty() {
                            // SDK-style results can carry the structured response as JSON text.
                            for encoded in [
                                (results.len() == 1)
                                    .then(|| text(fields, "/toolUseResult"))
                                    .flatten(),
                                text(fields, &format!("{base}/content")),
                            ]
                            .into_iter()
                            .flatten()
                            {
                                if let Ok(response) = parse_record(encoded.as_bytes(), 0) {
                                    answers =
                                        answer_values(&response, "/answers", &pending.keys, false);
                                    if !answers.is_empty() {
                                        break;
                                    }
                                }
                            }
                        }
                        append_answers(&mut body, &mut answered_calls, id, answers);
                    }
                }
            }
            Kind::Codex => {
                if text(fields, "/type") != Some("response_item") {
                    return None;
                }
                let id = text(fields, "/payload/call_id")?;
                match text(fields, "/payload/type") {
                    Some("function_call" | "custom_tool_call") => {
                        self.pending.remove(id);
                        if matches!(
                            text(fields, "/payload/name"),
                            Some("request_user_input" | "functions.request_user_input")
                        ) {
                            if let Some(arguments) = text(fields, "/payload/arguments")
                                .or_else(|| text(fields, "/payload/input"))
                            {
                                if let Ok(arguments) = parse_record(arguments.as_bytes(), 0) {
                                    self.remember(id, kind, keys(&arguments, "/questions", "id"));
                                }
                            }
                        }
                    }
                    Some("function_call_output" | "custom_tool_call_output") => {
                        if !self
                            .pending
                            .get(id)
                            .is_some_and(|pending| pending.kind == kind)
                        {
                            return None;
                        }
                        let response = text(fields, "/payload/output")
                            .and_then(|value| parse_record(value.as_bytes(), 0).ok());
                        // An acknowledgement is not an answer; leave a pending request intact.
                        if !flag(fields, "/payload/is_error")
                            && response.as_ref().is_some_and(|data| {
                                flag(data, "/accepted")
                                    && !data.iter().any(|field| field.path.starts_with("/answers/"))
                            })
                        {
                            return None;
                        }
                        let pending = self.take(id, kind)?;
                        if !flag(fields, "/payload/is_error") {
                            if let Some(response) = response {
                                append_answers(
                                    &mut body,
                                    &mut answered_calls,
                                    id,
                                    answer_values(&response, "/answers", &pending.keys, true),
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if body.is_empty() {
            return None;
        }
        meta.sender = Sender::Human;
        meta.via = "question_reply".to_string();
        meta.event_kind = EventKind::User;
        meta.role = Some("user".to_string());
        meta.call_id = Some(if answered_calls.len() == 1 {
            answered_calls.remove(0)
        } else {
            json::strings(answered_calls.iter().map(String::as_str))
        });
        Some(Projection { body, cut: false })
    }

    fn remember(&mut self, id: &str, kind: Kind, keys: Vec<String>) {
        if !id.is_empty() && !keys.is_empty() {
            self.pending.insert(id.to_string(), Pending { kind, keys });
        }
    }

    fn take(&mut self, id: &str, kind: Kind) -> Option<Pending> {
        if self.pending.get(id)?.kind != kind {
            return None;
        }
        self.pending.remove(id)
    }
}

fn text<'a>(fields: &'a [Field], path: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|field| field.path == path && field.kind == ScalarKind::String)
        .map(|field| field.value.as_str())
}

fn flag(fields: &[Field], path: &str) -> bool {
    fields
        .iter()
        .any(|field| field.path == path && field.kind == ScalarKind::Bool && field.value == "true")
}

fn indices(fields: &[Field], array: &str) -> BTreeSet<usize> {
    let prefix = format!("{array}/");
    fields
        .iter()
        .filter_map(|field| {
            field
                .path
                .strip_prefix(&prefix)?
                .split('/')
                .next()?
                .parse()
                .ok()
        })
        .collect()
}

fn keys(fields: &[Field], array: &str, key: &str) -> Vec<String> {
    let mut result = Vec::new();
    for index in indices(fields, array) {
        if let Some(value) = text(fields, &format!("{array}/{index}/{key}")) {
            if !value.is_empty() && !result.iter().any(|known| known == value) {
                result.push(value.to_string());
            }
        }
    }
    result
}

fn pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn answer_values(fields: &[Field], root: &str, keys: &[String], nested: bool) -> Vec<Vec<String>> {
    keys.iter()
        .filter_map(|key| {
            let root = format!(
                "{root}/{}{}",
                pointer(key),
                if nested { "/answers" } else { "" }
            );
            let values = if !nested && text(fields, &root).is_some() {
                vec![text(fields, &root).unwrap().to_string()]
            } else {
                indices(fields, &root)
                    .into_iter()
                    .filter_map(|index| {
                        text(fields, &format!("{root}/{index}")).map(str::to_string)
                    })
                    .collect()
            };
            let values = values
                .into_iter()
                .filter(|value| !value.trim().is_empty())
                .collect::<Vec<_>>();
            (!values.is_empty()).then_some(values)
        })
        .collect()
}

fn append_answers(
    body: &mut String,
    calls: &mut Vec<String>,
    call_id: &str,
    answers: Vec<Vec<String>>,
) {
    if answers.is_empty() {
        return;
    }
    let call_index = calls.len();
    calls.push(call_id.to_string());
    // Synthetic numeric paths keep question wording out of the person's searchable text,
    // and cannot contain tabs or newlines from question identifiers.
    for (question_index, values) in answers.into_iter().enumerate() {
        for (index, value) in values.into_iter().enumerate() {
            push_field(
                body,
                &format!("/answers/{call_index}/{question_index}/{index}"),
                &value,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{classify_sender, human_text_from_body, record_meta};

    fn read(
        tracker: &mut QuestionTracker,
        origin: SourceOrigin,
        record: &str,
    ) -> Option<(String, RecordMeta)> {
        let fields = parse_record(record.as_bytes(), 0).unwrap();
        let mut meta = record_meta(&fields);
        classify_sender(origin, &fields, &mut meta);
        tracker
            .project(origin, &fields, &mut meta)
            .map(|projection| {
                assert!(!projection.cut);
                assert_eq!(meta.sender, Sender::Human);
                assert_eq!(meta.via, "question_reply");
                assert_eq!(meta.event_kind, EventKind::User);
                (human_text_from_body(&projection.body).0, meta)
            })
    }

    #[test]
    fn codex_projects_multiple_answers_in_question_order_for_native_call_encodings() {
        for name in ["request_user_input", "functions.request_user_input"] {
            for (call_type, arguments_field, output_type) in [
                ("function_call", "arguments", "function_call_output"),
                ("custom_tool_call", "input", "custom_tool_call_output"),
            ] {
                let mut tracker = QuestionTracker::default();
                let call = format!(
                    r#"{{"type":"response_item","payload":{{"type":"{call_type}","name":"{name}","call_id":"ask","{arguments_field}":"{{\"questions\":[{{\"id\":\"second\",\"question\":\"AGENT_QUESTION\"}},{{\"id\":\"first\",\"question\":\"AGENT_QUESTION\"}}]}}"}}}}"#
                );
                assert!(read(&mut tracker, SourceOrigin::CodexThread, &call).is_none());
                let answer = format!(
                    r#"{{"type":"response_item","payload":{{"type":"{output_type}","call_id":"ask","output":"{{\"answers\":{{\"first\":{{\"answers\":[\"One\",\"Two\"]}},\"second\":{{\"answers\":[\"Three\"]}},\"unasked\":{{\"answers\":[\"IGNORE\"]}}}}}}"}}}}"#
                );
                let (text, meta) = read(&mut tracker, SourceOrigin::CodexThread, &answer).unwrap();
                assert_eq!(text, "Three\nOne\nTwo");
                assert_eq!(meta.call_id.as_deref(), Some("ask"));
                assert!(read(&mut tracker, SourceOrigin::CodexThread, &answer).is_none());
            }
        }
    }

    #[test]
    fn an_acknowledgement_waits_for_an_answer_but_an_error_consumes_the_request() {
        const CALL: &str = r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"ask","arguments":"{\"questions\":[{\"id\":\"q\"}]}"}}"#;
        const ACK: &str = r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"ask","output":"{\"accepted\":true}"}}"#;
        const ANSWER: &str = r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"ask","output":"{\"answers\":{\"q\":{\"answers\":[\"Yes\"]}}}"}}"#;
        let mut tracker = QuestionTracker::default();
        read(&mut tracker, SourceOrigin::CodexThread, CALL);
        assert!(read(&mut tracker, SourceOrigin::CodexThread, ACK).is_none());
        tracker = QuestionTracker::decode(&tracker.encode()).unwrap();
        assert_eq!(
            read(&mut tracker, SourceOrigin::CodexThread, ANSWER)
                .unwrap()
                .0,
            "Yes"
        );

        read(&mut tracker, SourceOrigin::CodexThread, CALL);
        let error = ACK.replace("\"output\":", "\"is_error\":true,\"output\":");
        assert!(read(&mut tracker, SourceOrigin::CodexThread, &error).is_none());
        assert!(read(&mut tracker, SourceOrigin::CodexThread, ANSWER).is_none());
    }

    #[test]
    fn claude_batches_use_each_results_own_answers_without_promoting_other_tool_output() {
        let mut tracker = QuestionTracker::default();
        let call = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"ask-1","name":"AskUserQuestion","input":{"questions":[{"question":"Which?"}]}},{"type":"tool_use","id":"ask-2","name":"AskUserQuestion","input":{"questions":[{"question":"Which?"}]}}]}}"#;
        read(&mut tracker, SourceOrigin::ClaudeMain, call);
        let response = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"ask-1","content":"{\"answers\":{\"Which?\":[\"A\",\"B\"]}}"},{"type":"tool_result","tool_use_id":"ask-2","content":"Cancelled"},{"type":"tool_result","tool_use_id":"unrelated","content":"{\"answers\":{\"Which?\":\"NOT_HUMAN\"}}"}]},"toolUseResult":"{\"answers\":{\"Which?\":\"AMBIGUOUS_TOP_LEVEL\"}}"}"#;
        let (text, meta) = read(&mut tracker, SourceOrigin::ClaudeMain, response).unwrap();
        assert_eq!(text, "A\nB");
        assert_eq!(meta.call_id.as_deref(), Some("ask-1"));
        assert!(tracker.encode().is_empty());
    }

    #[test]
    fn claude_accepts_structured_json_strings_but_not_renderer_text_or_meta_records() {
        const CALL: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"ask","name":"AskUserQuestion","input":{"questions":[{"question":"Which?"}]}}]}}"#;
        const ANSWER: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"ask","content":"AUTOMATIC_RENDERER"}]},"toolUseResult":"{\"answers\":{\"Which?\":\"日本語\"}}"}"#;
        let mut tracker = QuestionTracker::default();
        read(&mut tracker, SourceOrigin::ClaudeMain, CALL);
        assert_eq!(
            read(&mut tracker, SourceOrigin::ClaudeMain, ANSWER)
                .unwrap()
                .0,
            "日本語"
        );

        read(&mut tracker, SourceOrigin::ClaudeMain, CALL);
        let meta = ANSWER.replace("\"type\":\"user\"", "\"type\":\"user\",\"isMeta\":true");
        assert!(read(&mut tracker, SourceOrigin::ClaudeMain, &meta).is_none());
        read(&mut tracker, SourceOrigin::ClaudeMain, CALL);
        let renderer = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"ask","content":"User answered Which?=Yes"}]}}"#;
        assert!(read(&mut tracker, SourceOrigin::ClaudeMain, renderer).is_none());
    }

    #[test]
    fn corrupt_question_checkpoints_are_rejected() {
        for bad in [
            "codex\task",
            "other\task\tq",
            "codex\t\tq",
            "codex\task\t",
            "codex\task\tq\tq",
            "codex\task\tq\ncodex\task\tr",
            "codex\task\t%GG",
        ] {
            assert!(QuestionTracker::decode(bad).is_err(), "{bad}");
        }
    }
}
