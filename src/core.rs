use crate::format::{body_fields, push_field};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub trait FieldView {
    fn path(&self) -> &str;
    fn value(&self) -> &str;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EventKind {
    User,
    Assistant,
    Command,
    Output,
    Patch,
    Summary,
    #[default]
    Unknown,
    Invalid,
}

impl EventKind {
    pub const ALL: [EventKind; 8] = [
        Self::User,
        Self::Assistant,
        Self::Command,
        Self::Output,
        Self::Patch,
        Self::Summary,
        Self::Unknown,
        Self::Invalid,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Command => "command",
            Self::Output => "output",
            Self::Patch => "patch",
            Self::Summary => "summary",
            Self::Unknown => "unknown",
            Self::Invalid => "invalid",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "command" => Self::Command,
            "output" => Self::Output,
            "patch" => Self::Patch,
            "summary" => Self::Summary,
            "invalid" => Self::Invalid,
            _ => Self::Unknown,
        }
    }
}

/// Who put a record into the log. `Human` is the person at the keyboard: text they typed or
/// pasted and sent, including prompts typed while the agent was working. `Agent` is another
/// agent writing in the user role (a parent's prompt to a subagent, a program-started thread,
/// a relayed agent message). `System` is text the tools add on their own. `Summary` is a
/// compaction summary written by the agent. `Unknown` means the log format gives no marker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sender {
    #[default]
    Unknown,
    Human,
    Agent,
    System,
    Summary,
    Assistant,
}

impl Sender {
    pub const ALL: [Sender; 6] = [
        Self::Human,
        Self::Agent,
        Self::System,
        Self::Summary,
        Self::Assistant,
        Self::Unknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Human => "human",
            Self::Agent => "agent",
            Self::System => "system",
            Self::Summary => "summary",
            Self::Assistant => "assistant",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "human" => Self::Human,
            "agent" => Self::Agent,
            "system" => Self::System,
            "summary" => Self::Summary,
            "assistant" => Self::Assistant,
            _ => Self::Unknown,
        }
    }
}

/// The `via` of the person's messages that Codex copied from another conversation into this
/// one, stamped with the time of the import.
pub const IMPORTED_VIA: &str = "imported";

/// One of the person's messages. `said`, `resume`, and `follow` share this definition: a
/// record whose sender is the person, either sent in this conversation or imported with
/// another one. Imported copies are listed only on request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersonsMessage {
    Own,
    Imported,
}

pub fn persons_message(sender: &str, via: &str) -> Option<PersonsMessage> {
    if sender != Sender::Human.as_str() {
        return None;
    }
    Some(if via == IMPORTED_VIA {
        PersonsMessage::Imported
    } else {
        PersonsMessage::Own
    })
}

/// The person's messages already listed. One message stored twice (a session file kept in two
/// places, or Codex writing it as an event and as a response item) has the same timestamp and
/// the same text apart from the whitespace around it, and is listed once.
#[derive(Default)]
pub struct SeenMessages(BTreeSet<(String, String)>);

impl SeenMessages {
    /// Whether this is the first message with this timestamp and text.
    pub fn first(&mut self, timestamp: &str, text: &str) -> bool {
        self.0
            .insert((timestamp.to_string(), text.trim().to_string()))
    }
}

/// Where a whole source file came from. It is decided from the path and, for Codex, from the
/// session_meta record that opens the file, before any other record is classified.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SourceOrigin {
    #[default]
    Generic,
    /// A Claude Code session transcript: `<project>/<session-id>.jsonl`.
    ClaudeMain,
    /// A Claude Code subagent transcript: `<session-id>/subagents/agent-*.jsonl`.
    ClaudeSubagent,
    /// A Codex thread started from the Codex app or extension.
    CodexThread,
    /// A Codex thread spawned by another thread (`parent_thread_id`).
    CodexChild,
    /// A Codex thread started by `codex exec`, usually by another agent.
    CodexExec,
}

#[derive(Clone, Debug, Default)]
pub struct RecordMeta {
    pub session: Option<String>,
    pub turn: Option<String>,
    pub timestamp: Option<String>,
    pub cwd: Option<String>,
    pub repository: Option<String>,
    pub call_id: Option<String>,
    pub event_kind: EventKind,
    pub event_type: Option<String>,
    pub role: Option<String>,
    pub sender: Sender,
    pub via: String,
}

impl RecordMeta {
    /// Only a person's message (or a message in a format without sender markers) opens a turn,
    /// so notifications, injected context and compaction summaries stay inside the current one.
    fn opens_user_turn(&self) -> bool {
        self.event_kind == EventKind::User && matches!(self.sender, Sender::Human | Sender::Unknown)
    }
}

#[derive(Default)]
pub struct TurnReducer {
    sessions: BTreeMap<String, SessionTurn>,
}

#[derive(Clone, Debug, Default)]
pub struct TurnReducerState {
    pub sessions: Vec<SessionTurn>,
}

#[derive(Clone, Debug, Default)]
pub struct SessionTurn {
    pub session: String,
    pub turn: String,
    pub has_user: bool,
}

#[derive(Clone, Debug, Default)]
pub struct IngestCheckpoint {
    pub reducer: TurnReducerState,
    pub active_session: Option<String>,
    pub active_cwd: Option<String>,
    pub next_event_index: u64,
}

#[derive(Clone, Debug)]
pub struct CanonicalEvent {
    pub event_index: u64,
    pub meta: RecordMeta,
    pub body: String,
}

#[derive(Clone, Debug, Default)]
pub struct TimelineRef {
    pub event_index: u64,
    pub line: u64,
    pub byte_start: u64,
    pub byte_len: u64,
    pub session: String,
    pub turn: String,
    pub role: String,
    pub kind: String,
    pub timestamp: String,
}

#[derive(Clone, Debug, Default)]
pub struct TimelineBucket {
    pub date: String,
    pub event_count: u64,
    pub session_count: u64,
    pub kind_counts: BTreeMap<String, u64>,
    pub first: Option<TimelineRef>,
    pub last: Option<TimelineRef>,
    pub(crate) sessions: BTreeSet<String>,
}

impl TimelineBucket {
    pub fn observe(&mut self, date: &str, reference: TimelineRef) {
        if self.date.is_empty() {
            self.date = date.to_string();
        }
        self.event_count = self.event_count.saturating_add(1);
        if !reference.session.is_empty() && self.sessions.insert(reference.session.clone()) {
            self.session_count = self.session_count.saturating_add(1);
        }
        *self.kind_counts.entry(reference.kind.clone()).or_default() += 1;
        if self.first.is_none() {
            self.first = Some(reference.clone());
        }
        self.last = Some(reference);
    }
}

pub struct IngestStateMachine {
    reducer: TurnReducer,
    active_session: Option<String>,
    active_cwd: Option<String>,
    next_event_index: u64,
}

impl Default for IngestStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl IngestStateMachine {
    pub fn new() -> Self {
        Self {
            reducer: TurnReducer::default(),
            active_session: None,
            active_cwd: None,
            next_event_index: 0,
        }
    }

    pub fn from_checkpoint(checkpoint: IngestCheckpoint) -> Self {
        let active_session = checkpoint.active_session.clone().or_else(|| {
            checkpoint
                .reducer
                .sessions
                .last()
                .map(|entry| entry.session.clone())
        });
        Self {
            reducer: TurnReducer::from_state(checkpoint.reducer),
            active_session,
            active_cwd: checkpoint.active_cwd,
            next_event_index: checkpoint.next_event_index,
        }
    }

    pub fn apply(
        &mut self,
        fallback_session: &str,
        mut meta: RecordMeta,
        body: String,
        record_start: u64,
    ) -> CanonicalEvent {
        let session = meta
            .session
            .take()
            .filter(|value| !value.is_empty())
            .or_else(|| self.active_session.clone())
            .unwrap_or_else(|| fallback_session.to_string());
        self.active_session = Some(session.clone());
        // A Codex rollout names its working directory in session_meta and turn_context only;
        // later records inherit it so that every message can be found by project.
        match meta.cwd.as_deref().filter(|value| !value.is_empty()) {
            Some(cwd) => self.active_cwd = Some(cwd.to_string()),
            None => meta.cwd = self.active_cwd.clone(),
        }
        let turn = self.reducer.choose(&session, &meta, record_start);
        if meta.sender == Sender::Human && turn.starts_with(IMPORTED_TURN_PREFIX) {
            // A copy of another agent's conversation: the person's words, stamped at import.
            meta.via = IMPORTED_VIA.to_string();
        }
        meta.session = Some(session);
        meta.turn = Some(turn);
        let event = CanonicalEvent {
            event_index: self.next_event_index,
            meta,
            body,
        };
        self.next_event_index += 1;
        event
    }

    pub fn checkpoint(&self) -> IngestCheckpoint {
        IngestCheckpoint {
            reducer: self.reducer.state(),
            active_session: self.active_session.clone(),
            active_cwd: self.active_cwd.clone(),
            next_event_index: self.next_event_index,
        }
    }
}

pub fn derived_turn_id(record_start: u64) -> String {
    format!("turn@{}", record_start)
}

impl TurnReducer {
    pub fn from_state(state: TurnReducerState) -> Self {
        Self {
            sessions: state
                .sessions
                .into_iter()
                .map(|entry| (entry.session.clone(), entry))
                .collect(),
        }
    }

    pub fn state(&self) -> TurnReducerState {
        TurnReducerState {
            sessions: self.sessions.values().cloned().collect(),
        }
    }

    pub fn choose(&mut self, session_id: &str, meta: &RecordMeta, record_start: u64) -> String {
        let known = self.sessions.get(session_id);
        let explicit = meta.turn.clone().filter(|value| !value.is_empty());
        let starts_turn = meta
            .event_type
            .as_deref()
            .map(|value| {
                let lower = value.to_ascii_lowercase();
                lower.contains("turn_start") || lower.contains("turnstarted")
            })
            .unwrap_or(false);
        let opens_turn =
            meta.opens_user_turn() && known.map(|state| state.has_user).unwrap_or(false);

        let turn = match explicit {
            Some(explicit) => explicit,
            None => match known {
                Some(state) if !starts_turn && !opens_turn => state.turn.clone(),
                _ => derived_turn_id(record_start),
            },
        };

        let carried_user = known
            .filter(|state| state.turn == turn)
            .map(|state| state.has_user)
            .unwrap_or(false);
        self.sessions.insert(
            session_id.to_string(),
            SessionTurn {
                session: session_id.to_string(),
                turn: turn.clone(),
                has_user: carried_user || meta.opens_user_turn(),
            },
        );
        turn
    }
}

pub fn record_meta<F: FieldView>(fields: &[F]) -> RecordMeta {
    let event_type = event_type_value(fields);
    let role = value_for_keys(fields, &["role"]);
    let event_kind = classify_event(fields, event_type.as_deref(), role.as_deref());
    RecordMeta {
        session: session_value(fields, event_type.as_deref()),
        turn: turn_value(fields, event_type.as_deref()),
        timestamp: value_for_keys(
            fields,
            &["timestamp", "createdat", "created_at", "time", "ts", "date"],
        ),
        cwd: value_for_keys(fields, &["cwd", "currentworkingdirectory"]),
        repository: value_for_keys(fields, &["repository", "repo", "repositorypath"]),
        call_id: value_for_keys(
            fields,
            &["callid", "toolcallid", "functioncallid", "itemid"],
        ),
        event_kind,
        event_type,
        role,
        sender: Sender::Unknown,
        via: String::new(),
    }
}

/// What a sync keeps of one record: the fields worth reading, written with
/// `format::push_field`, and whether a value was shortened on the way.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Projection {
    pub body: String,
    pub cut: bool,
}

pub fn select_body<F: FieldView>(
    fields: &[F],
    meta: &RecordMeta,
    tool_output_chars: usize,
) -> Projection {
    if meta.sender == Sender::Human {
        return Projection {
            body: select_human_body(fields),
            cut: false,
        };
    }
    let shorten = meta.event_kind == EventKind::Output && tool_output_chars > 0;
    let mut projection = Projection::default();
    for field in fields {
        if !should_render_field(field, meta) {
            continue;
        }
        if shorten {
            let (value, cut) = truncate_chars(field.value(), tool_output_chars);
            projection.cut |= cut;
            push_field(&mut projection.body, field.path(), &value);
        } else {
            push_field(&mut projection.body, field.path(), field.value());
        }
    }
    projection
}

/// A person's message keeps only its text: tool-injected blocks are removed, images are
/// counted, and every value is length-delimited so multi-line text reads back exactly.
fn select_human_body<F: FieldView>(fields: &[F]) -> String {
    let mut body = String::new();
    let mut images = 0usize;
    for field in fields {
        let path = field.path();
        if is_human_text_path(path) {
            if let Some(value) = person_text(field.value()) {
                push_field(&mut body, path, &value);
            }
        } else if normalize_key(leaf(path)) == "type"
            && matches!(field.value(), "image" | "input_image")
        {
            images += 1;
        }
    }
    if images > 0 {
        push_field(&mut body, HUMAN_IMAGES_PATH, &images.to_string());
    }
    body
}

pub const HUMAN_IMAGES_PATH: &str = "/images";

/// The text of a person's message as stored by `select_human_body`, and its image count.
pub fn human_text_from_body(body: &str) -> (String, usize) {
    let mut text = String::new();
    let mut images = 0usize;
    for (path, value) in body_fields(body) {
        if path == HUMAN_IMAGES_PATH {
            images = value.parse().unwrap_or(0);
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&value);
    }
    (text, images)
}

/// Decides who put a record into the log. It can also correct the kind: a compaction summary
/// becomes `Summary`, a prompt typed while the agent was working becomes `User`, and a queued
/// notification no longer counts as a command.
pub fn classify_sender<F: FieldView>(origin: SourceOrigin, fields: &[F], meta: &mut RecordMeta) {
    let (sender, via) = match origin {
        SourceOrigin::ClaudeMain | SourceOrigin::ClaudeSubagent => {
            claude_sender(origin, fields, meta)
        }
        SourceOrigin::CodexThread | SourceOrigin::CodexChild | SourceOrigin::CodexExec => {
            codex_sender(origin, fields, meta)
        }
        SourceOrigin::Generic => {
            let sender = if meta.event_kind == EventKind::Assistant {
                Sender::Assistant
            } else {
                Sender::Unknown
            };
            (sender, "")
        }
    };
    meta.sender = sender;
    meta.via = via.to_string();
}

fn field_at<'a, F: FieldView>(fields: &'a [F], path: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|field| field.path() == path)
        .map(|field| field.value())
}

fn flag_at<F: FieldView>(fields: &[F], path: &str) -> bool {
    field_at(fields, path) == Some("true")
}

fn claude_sender<F: FieldView>(
    origin: SourceOrigin,
    fields: &[F],
    meta: &mut RecordMeta,
) -> (Sender, &'static str) {
    match field_at(fields, "/type").unwrap_or("") {
        "user" => {
            if flag_at(fields, "/isCompactSummary") {
                meta.event_kind = EventKind::Summary;
                return (Sender::Summary, "compact_summary");
            }
            if meta.event_kind == EventKind::Output || has_content_item_type(fields, "tool_result")
            {
                return (Sender::System, "tool_result");
            }
            if flag_at(fields, "/isMeta") {
                return (Sender::System, "meta");
            }
            if origin == SourceOrigin::ClaudeSubagent || flag_at(fields, "/isSidechain") {
                return (Sender::Agent, "subagent_prompt");
            }
            let (sender, via) = claude_text_sender(
                &human_text(fields),
                has_content_item_type(fields, "image"),
                "typed",
            );
            if sender == Sender::Summary {
                meta.event_kind = EventKind::Summary;
            }
            (sender, via)
        }
        "attachment" => {
            if field_at(fields, "/attachment/type") != Some("queued_command") {
                // Reminders, file snapshots and hook context: never the agent's own commands.
                meta.event_kind = EventKind::Unknown;
                return (Sender::System, "attachment");
            }
            if origin == SourceOrigin::ClaudeSubagent {
                meta.event_kind = EventKind::Unknown;
                return (Sender::Agent, "subagent_prompt");
            }
            let (sender, via) = claude_text_sender(&human_text(fields), false, "queued");
            meta.event_kind = if sender == Sender::Summary {
                EventKind::Summary
            } else {
                EventKind::User
            };
            meta.role = Some("user".to_string());
            (sender, via)
        }
        "system" => {
            // Hook summaries and API errors name hook commands; they are not the agent's own.
            meta.event_kind = EventKind::Unknown;
            if field_at(fields, "/subtype") == Some("compact_boundary") {
                meta.event_type = Some("compact_boundary".to_string());
                return (Sender::System, "compact_boundary");
            }
            (Sender::System, "")
        }
        "assistant" => (Sender::Assistant, ""),
        _ => {
            meta.event_kind = EventKind::Unknown;
            (Sender::System, "")
        }
    }
}

fn claude_text_sender(text: &str, has_image: bool, typed: &'static str) -> (Sender, &'static str) {
    let text = text.trim_start();
    if text.is_empty() {
        return if has_image {
            (Sender::Human, typed)
        } else {
            (Sender::System, "injected")
        };
    }
    if is_compaction_summary_text(text) {
        return (Sender::Summary, "compact_summary");
    }
    if starts_with_tag(text, &["task-notification"]) {
        return (Sender::System, "notification");
    }
    if starts_with_tag(
        text,
        &["agent-message", "cross-session-message", "teammate-message"],
    ) {
        return (Sender::Agent, "agent_message");
    }
    if starts_with_tag(
        text,
        &[
            "local-command-stdout",
            "local-command-stderr",
            "local-command-caveat",
            "bash-stdout",
            "bash-stderr",
        ],
    ) || text.starts_with("Caveat: The messages below")
    {
        return (Sender::System, "command_output");
    }
    if starts_with_tag(text, &["user-prompt-submit-hook"]) {
        return (Sender::System, "hook");
    }
    if starts_with_tag(text, &["command-name", "command-message", "command-args"]) {
        return (Sender::Human, "slash_command");
    }
    if starts_with_tag(text, &["bash-input"]) {
        return (Sender::Human, "shell_command");
    }
    (Sender::Human, typed)
}

fn codex_sender<F: FieldView>(
    origin: SourceOrigin,
    fields: &[F],
    meta: &mut RecordMeta,
) -> (Sender, &'static str) {
    let (sender, via) = codex_record_sender(origin, fields, meta);
    if sender == Sender::Summary {
        meta.event_kind = EventKind::Summary;
    }
    (sender, via)
}

fn codex_record_sender<F: FieldView>(
    origin: SourceOrigin,
    fields: &[F],
    meta: &RecordMeta,
) -> (Sender, &'static str) {
    let delivered_by = match origin {
        SourceOrigin::CodexChild => Some("codex_child"),
        SourceOrigin::CodexExec => Some("codex_exec"),
        _ => None,
    };
    let record_type = field_at(fields, "/type").unwrap_or("");
    let payload_type = field_at(fields, "/payload/type").unwrap_or("");
    match (record_type, payload_type) {
        ("response_item", "message") => match field_at(fields, "/payload/role").unwrap_or("") {
            "user" => match delivered_by {
                Some(via) => (Sender::Agent, via),
                None => codex_text_sender(
                    &human_text(fields),
                    has_content_item_type(fields, "input_image"),
                ),
            },
            "assistant" => (Sender::Assistant, ""),
            _ => (Sender::System, "instructions"),
        },
        // The response item carries the images of the same message; this event repeats its text.
        ("event_msg", "user_message") => match delivered_by {
            Some(via) => (Sender::Agent, via),
            None => codex_text_sender(&human_text(fields), false),
        },
        ("response_item", "agent_message") => (Sender::Agent, "agent_message"),
        ("event_msg", "agent_message") => (Sender::Assistant, ""),
        ("response_item", _) if meta.event_kind == EventKind::Output => {
            (Sender::System, "tool_result")
        }
        ("response_item", _) => (Sender::Assistant, ""),
        ("compacted", _) => {
            if field_at(fields, "/payload/message").is_some_and(|text| !text.trim().is_empty()) {
                (Sender::Summary, "compact_summary")
            } else {
                (Sender::System, "compact_boundary")
            }
        }
        _ => (Sender::System, ""),
    }
}

fn codex_text_sender(text: &str, has_image: bool) -> (Sender, &'static str) {
    let text = text.trim_start();
    if text.is_empty() {
        // A screenshot sent without words is still the person's message.
        return if has_image {
            (Sender::Human, "typed")
        } else {
            (Sender::System, "injected")
        };
    }
    if is_compaction_summary_text(text) {
        return (Sender::Summary, "compact_summary");
    }
    if text.starts_with("# AGENTS.md") {
        return (Sender::System, "instructions");
    }
    if starts_with_tag(
        text,
        &[
            "environment_context",
            "codex_internal_context",
            "recommended_plugins",
            "user_instructions",
            "permissions",
            "collaboration_mode",
            "app-context",
            "turn_aborted",
        ],
    ) {
        return (Sender::System, "injected");
    }
    if starts_with_tag(text, &["heartbeat"]) {
        return (Sender::System, "automation");
    }
    if starts_with_tag(text, &["task-notification"]) {
        return (Sender::System, "notification");
    }
    if starts_with_tag(
        text,
        &["codex_delegation", "subagent_notification", "agent-message"],
    ) {
        return (Sender::Agent, "agent_message");
    }
    if starts_with_tag(text, &["send_user_message_question_reply"]) {
        return (Sender::Human, "question_reply");
    }
    (Sender::Human, "typed")
}

/// Claude Code opens every compaction summary with this sentence. It also marks summaries
/// that reach a log without their `isCompactSummary` flag, such as a Claude session imported
/// into a Codex thread.
fn is_compaction_summary_text(text: &str) -> bool {
    text.starts_with("This session is being continued from a previous conversation")
}

/// Codex numbers the turns of a conversation imported from another agent this way.
pub const IMPORTED_TURN_PREFIX: &str = "external-import-turn-";

fn starts_with_tag(text: &str, names: &[&str]) -> bool {
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    names.iter().any(|name| {
        rest.strip_prefix(name)
            .and_then(|after| after.chars().next())
            .map(|next| matches!(next, '>' | ' ' | '\n' | '\r' | '\t' | '/'))
            .unwrap_or(false)
    })
}

/// The text a person wrote in a record, with tool-injected blocks removed.
fn human_text<F: FieldView>(fields: &[F]) -> String {
    let mut text = String::new();
    for field in fields {
        if !is_human_text_path(field.path()) {
            continue;
        }
        let value = strip_injected_blocks(field.value());
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(value);
    }
    text
}

fn is_human_text_path(path: &str) -> bool {
    if matches!(
        path,
        "/message/content" | "/attachment/prompt" | "/payload/message"
    ) {
        return true;
    }
    [
        "/message/content/",
        "/attachment/prompt/",
        "/payload/content/",
    ]
    .iter()
    .any(|container| {
        path.strip_prefix(container)
            .and_then(|rest| rest.strip_suffix("/text"))
            .map(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
            .unwrap_or(false)
    })
}

/// Blocks that Claude Code and Codex add inside a person's message. An unterminated block is
/// kept, because it may be text the person pasted.
const INJECTED_BLOCKS: [(&str, &str); 2] = [
    ("<system-reminder>", "</system-reminder>"),
    ("<in-app-browser-context", "</in-app-browser-context>"),
];

/// A person's text as they sent it. Text without a tool-injected block is kept byte for byte;
/// where a block opened or closed the value, the whitespace that set it apart from the
/// person's words goes with it. `None` when nothing of the person's is left.
fn person_text(value: &str) -> Option<std::borrow::Cow<'_, str>> {
    let stripped = strip_injected_blocks(value);
    if stripped.trim().is_empty() {
        return None;
    }
    match stripped {
        std::borrow::Cow::Borrowed(text) => Some(std::borrow::Cow::Borrowed(text)),
        std::borrow::Cow::Owned(text) => {
            let opened = INJECTED_BLOCKS
                .iter()
                .any(|(open, _)| value.trim_start().starts_with(open));
            let closed = INJECTED_BLOCKS
                .iter()
                .any(|(_, close)| value.trim_end().ends_with(close));
            let mut kept = text.as_str();
            if opened {
                kept = kept.trim_start_matches(['\r', '\n']);
            }
            if closed {
                kept = kept.trim_end();
            }
            Some(std::borrow::Cow::Owned(kept.to_string()))
        }
    }
}

fn strip_injected_blocks(value: &str) -> std::borrow::Cow<'_, str> {
    if !INJECTED_BLOCKS.iter().any(|(open, _)| value.contains(open)) {
        return std::borrow::Cow::Borrowed(value);
    }
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some((at, close)) = INJECTED_BLOCKS
        .iter()
        .filter_map(|(open, close)| rest.find(open).map(|at| (at, *close)))
        .min_by_key(|(at, _)| *at)
    {
        let Some(end) = rest[at..].find(close) else {
            break;
        };
        output.push_str(&rest[..at]);
        rest = &rest[at + end + close.len()..];
    }
    output.push_str(rest);
    std::borrow::Cow::Owned(output)
}

pub const PROJECTION_BOUND_MARKER: &str = "…[truncated]";

/// The first `limit` characters, and whether anything was left out. A shortened value ends
/// with the marker so that whoever reads it sees the cut.
fn truncate_chars(value: &str, limit: usize) -> (String, bool) {
    let mut characters = value.chars();
    let prefix: String = characters.by_ref().take(limit).collect();
    if characters.next().is_some() {
        (format!("{}{}", prefix, PROJECTION_BOUND_MARKER), true)
    } else {
        (prefix, false)
    }
}

fn session_value<F: FieldView>(fields: &[F], event_type: Option<&str>) -> Option<String> {
    if let Some(value) = value_for_keys(fields, &["sessionid", "session_id", "session"]) {
        return Some(value);
    }
    let is_session_record = event_type
        .map(|value| value.to_ascii_lowercase().contains("session"))
        .unwrap_or(false);
    if is_session_record {
        return value_for_keys(fields, &["id"]);
    }
    None
}

fn turn_value<F: FieldView>(fields: &[F], event_type: Option<&str>) -> Option<String> {
    if let Some(value) = value_for_keys(fields, &["turnid", "turn_id", "turn"]) {
        return Some(value);
    }
    let is_turn_record = event_type
        .map(|value| value.to_ascii_lowercase().contains("turn"))
        .unwrap_or(false);
    if is_turn_record {
        return value_for_keys(fields, &["id"]);
    }
    None
}

fn value_for_keys<F: FieldView>(fields: &[F], keys: &[&str]) -> Option<String> {
    for wanted in keys {
        let wanted = normalize_key(wanted);
        let mut best: Option<(&F, u8)> = None;
        for field in fields {
            let Some(rank) = metadata_path_rank(field.path(), &wanted) else {
                continue;
            };
            if field.value().is_empty() {
                continue;
            }
            let replace = best
                .as_ref()
                .map(|(current, current_rank)| {
                    rank < *current_rank || (rank == *current_rank && field.path() < current.path())
                })
                .unwrap_or(true);
            if replace {
                best = Some((field, rank));
            }
        }
        if let Some((field, _)) = best {
            return Some(field.value().to_string());
        }
    }
    None
}

fn metadata_path_rank(path: &str, wanted: &str) -> Option<u8> {
    let leaf = path.rsplit('/').find(|segment| !segment.is_empty())?;
    if !key_matches(leaf, wanted) {
        return None;
    }
    if is_embedded_history_path(path) {
        return None;
    }
    let mut parents = 0usize;
    let mut envelope = false;
    let mut previous: Option<&str> = None;
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        if let Some(parent) = previous {
            parents += 1;
            let parent = normalize_key(parent);
            if is_payload_container(&parent) {
                return None;
            }
            if is_envelope_container(&parent) {
                envelope = true;
            }
        }
        previous = Some(segment);
    }
    if parents == 0 {
        return Some(0);
    }
    if envelope {
        return Some(1);
    }
    Some(2)
}

fn is_payload_container(segment: &str) -> bool {
    matches!(
        segment,
        "arguments"
            | "content"
            | "input"
            | "output"
            | "command"
            | "parameters"
            | "toolinput"
            | "tooloutput"
            | "usermessage"
            | "assistantmessage"
            | "functioncall"
            | "functionoutput"
            | "toolcall"
            | "toolresult"
    )
}

fn is_envelope_container(segment: &str) -> bool {
    matches!(
        segment,
        "eventmsg"
            | "responseitem"
            | "payload"
            | "metadata"
            | "envelope"
            | "record"
            | "event"
            | "message"
    )
}

fn is_embedded_history_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    let head = match rest.find('/') {
        Some(index) => &rest[..index],
        None => rest,
    };
    head.eq_ignore_ascii_case("replacement_history")
}

fn key_matches(value: &str, normalized: &str) -> bool {
    let mut wanted = normalized.bytes();
    for character in value.chars() {
        if !character.is_ascii_alphanumeric() {
            continue;
        }
        if wanted.next() != Some(character.to_ascii_lowercase() as u8) {
            return false;
        }
    }
    wanted.next().is_none()
}

/// The record's type, from the first of these paths that holds one: Codex's `/payload/type`,
/// the envelopes of other formats, then the top-level `/type` (Claude Code) and generic
/// top-level names. A `type` nested deeper in the record never names the record.
fn event_type_value<F: FieldView>(fields: &[F]) -> Option<String> {
    const PATHS: [&str; 7] = [
        "/payload/type",
        "/response_item/type",
        "/event_msg/type",
        "/type",
        "/event_type",
        "/eventtype",
        "/kind",
    ];
    PATHS.iter().find_map(|wanted| {
        fields
            .iter()
            .find(|field| field.path().eq_ignore_ascii_case(wanted))
            .map(|field| field.value())
            .filter(|value| {
                !value.eq_ignore_ascii_case("event_msg")
                    && !value.eq_ignore_ascii_case("response_item")
            })
            .map(str::to_string)
    })
}

fn should_render_field<F: FieldView>(field: &F, meta: &RecordMeta) -> bool {
    let path = field.path().to_ascii_lowercase();
    let key = normalize_key(leaf(field.path()));
    if is_noise_field(&path) && meta.event_kind != EventKind::Unknown {
        return false;
    }
    if carries_replacement_history(meta.event_type.as_deref()) {
        return path == "/payload/message"
            || (path.contains("/replacement_history/")
                && matches!(key.as_str(), "role" | "text" | "message" | "turnid"));
    }
    // Model reasoning is not projected: Claude Code thinking blocks (text and signature) as
    // Codex reasoning records are not. It stays in the original record.
    if matches!(key.as_str(), "thinking" | "signature") && path.contains("/content/") {
        return false;
    }
    if is_noise_event(meta.event_type.as_deref()) {
        return is_identity_field(&key);
    }
    if is_identity_field(&key) && !is_inside_payload(field.path()) {
        return key == "name";
    }
    match meta.event_kind {
        EventKind::User | EventKind::Assistant | EventKind::Summary => {
            key == "message"
                || key == "text"
                || key == "content"
                || key == "lastagentmessage"
                || key == "input"
                || path.contains("/content/")
        }
        EventKind::Command | EventKind::Output | EventKind::Patch => true,
        EventKind::Unknown | EventKind::Invalid => true,
    }
}

fn is_inside_payload(path: &str) -> bool {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(normalize_key)
        .collect::<Vec<_>>();
    segments[..segments.len().saturating_sub(1)]
        .iter()
        .any(|segment| is_payload_container(segment))
}

fn is_identity_field(key: &str) -> bool {
    matches!(
        key,
        "type"
            | "eventtype"
            | "kind"
            | "role"
            | "sessionid"
            | "session"
            | "turnid"
            | "turn"
            | "timestamp"
            | "createdat"
            | "time"
            | "ts"
            | "date"
            | "cwd"
            | "currentworkingdirectory"
            | "repository"
            | "repo"
            | "repositorypath"
            | "callid"
            | "toolcallid"
            | "functioncallid"
            | "itemid"
            | "name"
    )
}

fn is_noise_field(path: &str) -> bool {
    [
        "encrypted_content",
        "encryptedcontent",
        "/reasoning",
        "/analysis",
        "token_count",
        "tokencount",
        "/rate_limits",
        "/ratelimits",
        "/base_instructions",
        "/baseinstructions",
        "/host_skills",
        "/hostskills",
        "/agents_md",
        "/agentsmd",
        "/dynamic_tools",
        "/dynamictools",
        "/workspace_roots",
        "/workspaceroots",
        "/comp_hash",
        "/comphash",
    ]
    .iter()
    .any(|needle| path.contains(needle))
}

fn is_noise_event(event_type: Option<&str>) -> bool {
    let value = event_type.unwrap_or("").to_ascii_lowercase();
    matches!(
        value.as_str(),
        "token_count"
            | "token_count_event"
            | "agent_reasoning"
            | "reasoning"
            | "world_state"
            | "turn_context"
            | "session_meta"
            | "thread_settings_applied"
            | "inter_agent_communication_metadata"
            | "compacted"
            | "context_compacted"
            | "compaction"
    )
}

/// A record that marks a compaction: Claude Code's `compact_boundary`, Codex's `compacted`.
/// Each compaction has exactly one.
pub fn is_compaction(event_type: &str) -> bool {
    matches!(event_type, "compact_boundary" | "compacted")
}

/// Codex's compaction record, which carries the history kept after the compaction.
pub fn carries_replacement_history(event_type: Option<&str>) -> bool {
    event_type == Some("compacted")
}

fn has_content_item_type<F: FieldView>(fields: &[F], wanted: &str) -> bool {
    fields.iter().any(|field| {
        normalize_key(leaf(field.path())) == "type" && field.value().eq_ignore_ascii_case(wanted)
    })
}

fn classify_event<F: FieldView>(
    fields: &[F],
    event_type: Option<&str>,
    role: Option<&str>,
) -> EventKind {
    let event_type = event_type.unwrap_or("").to_ascii_lowercase();
    let role = role.unwrap_or("").to_ascii_lowercase();
    if matches!(
        event_type.as_str(),
        "compacted" | "context_compacted" | "compaction"
    ) {
        return EventKind::Unknown;
    }
    if event_type.contains("output")
        || event_type.contains("result")
        || event_type.contains("toolreturn")
        || event_type.contains("call_end")
    {
        return EventKind::Output;
    }
    if event_type.contains("patch") {
        return EventKind::Patch;
    }
    if event_type.contains("command")
        || event_type.contains("function_call")
        || event_type.contains("functioncall")
        || event_type.contains("tool_call")
        || event_type.contains("toolcall")
        || event_type.contains("custom_tool_call")
    {
        return EventKind::Command;
    }
    if has_content_item_type(fields, "tool_result") {
        return EventKind::Output;
    }
    if role == "user"
        || event_type == "user"
        || event_type.contains("user_message")
        || event_type.contains("usermessage")
    {
        return EventKind::User;
    }
    if role == "assistant"
        || event_type == "assistant"
        || event_type.contains("assistant_message")
        || event_type.contains("assistantmessage")
        || event_type.contains("agent_message")
        || event_type.contains("agentmessage")
    {
        return EventKind::Assistant;
    }
    if fields.iter().any(|field| {
        let key = normalize_key(leaf(field.path()));
        key == "stdout" || key == "stderr" || key == "output"
    }) {
        return EventKind::Output;
    }
    if fields.iter().any(|field| {
        let path = field.path().to_ascii_lowercase();
        path.contains("patch") || path.contains("apply_patch")
    }) {
        return EventKind::Patch;
    }
    if fields.iter().any(|field| {
        let key = normalize_key(leaf(field.path()));
        key == "command" || key == "arguments"
    }) {
        return EventKind::Command;
    }
    EventKind::Unknown
}

fn leaf(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn normalize_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

#[derive(Clone, Debug, Default)]
pub struct SourceRef {
    pub source_id: String,
    pub line: u64,
    pub byte_start: u64,
    pub byte_len: u64,
}

#[derive(Clone, Debug)]
pub struct RecoveryEvent {
    pub event_index: u64,
    pub source_ref: SourceRef,
    pub session: String,
    pub turn: String,
    pub role: String,
    pub kind: EventKind,
    pub event_type: String,
    pub timestamp: String,
    pub cwd: String,
    pub repository: String,
    pub call_id: String,
    pub body: String,
    pub body_truncated: bool,
    pub embedded_history: bool,
    pub sender: Sender,
    pub via: String,
}

#[derive(Clone, Debug, Default)]
pub struct RecoverySnapshot {
    pub session: String,
    pub turn: String,
    pub events_seen: u64,
    pub events: Vec<Arc<RecoveryEvent>>,
    pub users: Vec<Arc<RecoveryEvent>>,
    pub assistants: Vec<Arc<RecoveryEvent>>,
    pub commands: Vec<Arc<RecoveryEvent>>,
    pub outputs: Vec<Arc<RecoveryEvent>>,
    pub users_seen: u64,
    pub assistants_seen: u64,
    pub commands_seen: u64,
    pub outputs_seen: u64,
    pub cwd: BTreeSet<String>,
    pub repositories: BTreeSet<String>,
    pub completed: bool,
    pub reentered: bool,
}

#[derive(Default)]
pub struct RecoveryState {
    sessions: BTreeMap<String, SessionRecovery>,
    latest_session: Option<String>,
    matched_events: u64,
    latest_event_index: Option<u64>,
}

#[derive(Default)]
struct SessionRecovery {
    current: Option<RecoverySnapshot>,
    previous: Option<RecoverySnapshot>,
    seen_turns: BTreeSet<String>,
}

impl RecoveryState {
    pub fn accept(&mut self, event: RecoveryEvent, embedded: Vec<RecoveryEvent>) {
        let event_index = event.event_index;
        self.latest_session = Some(event.session.clone());
        let recovery = self.sessions.entry(event.session.clone()).or_default();
        let continues = recovery
            .current
            .as_ref()
            .map(|current| current.turn == event.turn)
            .unwrap_or(false);
        if !continues {
            let returns = recovery
                .previous
                .as_ref()
                .map(|previous| previous.turn == event.turn)
                .unwrap_or(false);
            if returns {
                std::mem::swap(&mut recovery.current, &mut recovery.previous);
            } else {
                let reentered = !recovery.seen_turns.insert(event.turn.clone());
                if let Some(current) = recovery.current.take() {
                    recovery.previous = Some(current);
                }
                recovery.current = Some(RecoverySnapshot {
                    session: event.session.clone(),
                    turn: event.turn.clone(),
                    reentered,
                    ..RecoverySnapshot::default()
                });
            }
        }

        if let Some(current) = recovery.current.as_mut() {
            current.events_seen += 1;
            current.completed |= completion_event(&event.event_type);
            if !event.cwd.is_empty() {
                current.cwd.insert(event.cwd.clone());
            }
            if !event.repository.is_empty() {
                current.repositories.insert(event.repository.clone());
            }
            let event = Arc::new(event);
            current.events.push(Arc::clone(&event));
            trim_front(&mut current.events, MAX_RECOVERY_EVENTS);
            add_recovery_category(current, event);
        }
        self.matched_events += 1;
        self.latest_event_index = Some(
            self.latest_event_index
                .map(|index| index.max(event_index))
                .unwrap_or(event_index),
        );
        for embedded_event in embedded {
            self.accept_embedded(embedded_event);
        }
    }

    fn accept_embedded(&mut self, event: RecoveryEvent) {
        let Some(recovery) = self.sessions.get_mut(&event.session) else {
            return;
        };
        let matches = |snapshot: &RecoverySnapshot| snapshot.turn == event.turn;
        if recovery.current.as_ref().map(matches).unwrap_or(false) {
            if let Some(current) = recovery.current.as_mut() {
                add_recovery_category(current, Arc::new(event));
            }
        } else if recovery.previous.as_ref().map(matches).unwrap_or(false) {
            if let Some(previous) = recovery.previous.as_mut() {
                add_recovery_category(previous, Arc::new(event));
            }
        }
    }

    pub fn finish(&mut self) {}

    pub fn current(&self) -> Option<&RecoverySnapshot> {
        self.latest()?.current.as_ref()
    }

    pub fn previous(&self) -> Option<&RecoverySnapshot> {
        self.latest()?.previous.as_ref()
    }

    fn latest(&self) -> Option<&SessionRecovery> {
        self.sessions.get(self.latest_session.as_deref()?)
    }

    pub fn matched_events(&self) -> u64 {
        self.matched_events
    }

    pub fn latest_event_index(&self) -> Option<u64> {
        self.latest_event_index
    }
}

const MAX_RECOVERY_EVENTS: usize = 128;
const MAX_RECOVERY_MESSAGES: usize = 32;

fn add_recovery_category(current: &mut RecoverySnapshot, event: Arc<RecoveryEvent>) {
    match event.kind {
        EventKind::User => {
            current.users_seen += 1;
            current.users.push(event);
            trim_front(&mut current.users, MAX_RECOVERY_MESSAGES);
        }
        EventKind::Assistant => {
            current.assistants_seen += 1;
            current.assistants.push(Arc::clone(&event));
            trim_front(&mut current.assistants, MAX_RECOVERY_MESSAGES);
            if is_tool_call(event.kind, &event.event_type, &event.body) {
                current.commands_seen += 1;
                current.commands.push(event);
                trim_front(&mut current.commands, MAX_RECOVERY_MESSAGES);
            }
        }
        EventKind::Command if is_tool_call(event.kind, &event.event_type, &event.body) => {
            current.commands_seen += 1;
            current.commands.push(event);
            trim_front(&mut current.commands, MAX_RECOVERY_MESSAGES);
        }
        EventKind::Command => {}
        EventKind::Output => {
            current.outputs_seen += 1;
            current.outputs.push(event);
            trim_front(&mut current.outputs, MAX_RECOVERY_MESSAGES);
        }
        EventKind::Patch | EventKind::Summary | EventKind::Unknown | EventKind::Invalid => {}
    }
}

/// A request for a tool to run: a Claude Code assistant message with a `tool_use` item, or a
/// Codex `*_call` record. Progress records (`exec_command_begin`, `_end`) are not calls.
pub fn is_tool_call(kind: EventKind, event_type: &str, body: &str) -> bool {
    match kind {
        EventKind::Assistant => body_fields(body)
            .iter()
            .any(|(path, value)| value == "tool_use" && normalize_key(leaf(path)) == "type"),
        EventKind::Command => event_type.ends_with("call"),
        _ => false,
    }
}

fn trim_front<T>(values: &mut Vec<T>, limit: usize) {
    if values.len() > limit {
        let remove = values.len() - limit;
        values.drain(..remove);
    }
}

fn completion_event(event_type: &str) -> bool {
    let event_type = event_type.to_ascii_lowercase();
    [
        "turn_complete",
        "turn_completed",
        "turncomplete",
        "task_complete",
        "task_completed",
        "taskcomplete",
        "response_complete",
        "response_completed",
        "responsecomplete",
    ]
    .iter()
    .any(|marker| event_type.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::{
        record_meta, select_body, CanonicalEvent, EventKind, FieldView, IngestStateMachine,
        RecordMeta, RecoveryEvent, RecoveryState, SourceRef,
    };

    struct TestField {
        path: &'static str,
        value: &'static str,
    }

    impl FieldView for TestField {
        fn path(&self) -> &str {
            self.path
        }

        fn value(&self) -> &str {
            self.value
        }
    }

    #[test]
    fn classifies_and_selects_conversation_text() {
        let fields = vec![
            TestField {
                path: "/type",
                value: "user_message",
            },
            TestField {
                path: "/message",
                value: "日本語全文",
            },
        ];
        let meta = record_meta(&fields);
        assert_eq!(meta.event_kind, EventKind::User);
        assert!(select_body(&fields, &meta, 300).body.contains("日本語全文"));
    }

    #[test]
    fn truncates_output_only() {
        let fields = vec![
            TestField {
                path: "/type",
                value: "command_output",
            },
            TestField {
                path: "/output",
                value: "123456789",
            },
        ];
        let meta = record_meta(&fields);
        assert_eq!(meta.event_kind, EventKind::Output);
        assert!(select_body(&fields, &meta, 4)
            .body
            .contains("1234…[truncated]"));
    }

    #[test]
    fn explicit_message_role_has_priority_over_nested_arguments() {
        let fields = vec![
            TestField {
                path: "/type",
                value: "message",
            },
            TestField {
                path: "/role",
                value: "user",
            },
            TestField {
                path: "/metadata/arguments",
                value: "details",
            },
            TestField {
                path: "/text",
                value: "user message",
            },
        ];
        let meta = record_meta(&fields);
        assert_eq!(meta.event_kind, EventKind::User);
    }

    #[test]
    fn envelope_metadata_has_priority_over_nested_tool_arguments() {
        let fields = vec![
            TestField {
                path: "/response_item/arguments/session",
                value: "tool-session",
            },
            TestField {
                path: "/response_item/arguments/date",
                value: "tomorrow",
            },
            TestField {
                path: "/role",
                value: "assistant",
            },
            TestField {
                path: "/session_id",
                value: "real-session",
            },
            TestField {
                path: "/timestamp",
                value: "2026-08-17T00:00:00Z",
            },
        ];
        let meta = record_meta(&fields);
        assert_eq!(meta.session.as_deref(), Some("real-session"));
        assert_eq!(meta.timestamp.as_deref(), Some("2026-08-17T00:00:00Z"));
        assert_eq!(meta.role.as_deref(), Some("assistant"));
    }

    #[test]
    fn payload_metadata_is_not_promoted_to_envelope() {
        let fields = vec![
            TestField {
                path: "/response_item/arguments/session",
                value: "tool-session",
            },
            TestField {
                path: "/response_item/arguments/date",
                value: "tomorrow",
            },
            TestField {
                path: "/response_item/arguments/role",
                value: "user",
            },
        ];
        let meta = record_meta(&fields);
        assert_eq!(meta.session, None);
        assert_eq!(meta.timestamp, None);
        assert_eq!(meta.role, None);
    }

    #[test]
    fn compaction_body_keeps_multiline_values_length_delimited() {
        let fields = vec![
            TestField {
                path: "/type",
                value: "context_compaction",
            },
            TestField {
                path: "/replacement_history/1/role",
                value: "user",
            },
            TestField {
                path: "/replacement_history/1/text",
                value: "line one\nline two",
            },
        ];
        let meta = record_meta(&fields);
        let body = select_body(&fields, &meta, 300).body;
        assert!(body.contains("/replacement_history/1/text\t17\tline one\nline two\n"));
    }

    #[test]
    fn compaction_history_preserves_outer_event_classification() {
        let fields = vec![
            TestField {
                path: "/type",
                value: "context_compaction",
            },
            TestField {
                path: "/replacement_history/0/role",
                value: "user",
            },
            TestField {
                path: "/replacement_history/0/turn_id",
                value: "old-turn",
            },
        ];
        let meta = record_meta(&fields);
        assert_eq!(meta.event_kind, EventKind::Unknown);
        assert_eq!(meta.role, None);
        assert_eq!(meta.turn, None);
    }

    #[test]
    fn content_type_classifies_an_author_typed_record() {
        let user = vec![
            TestField {
                path: "/type",
                value: "user",
            },
            TestField {
                path: "/message/role",
                value: "user",
            },
            TestField {
                path: "/message/content/0/type",
                value: "text",
            },
            TestField {
                path: "/message/content/0/text",
                value: "fix the limiter",
            },
        ];
        let meta = record_meta(&user);
        assert_eq!(meta.event_kind, EventKind::User);
        assert_eq!(meta.role.as_deref(), Some("user"));
        assert!(select_body(&user, &meta, 300)
            .body
            .contains("fix the limiter"));

        let assistant = vec![
            TestField {
                path: "/type",
                value: "assistant",
            },
            TestField {
                path: "/message/role",
                value: "assistant",
            },
            TestField {
                path: "/message/content/0/type",
                value: "text",
            },
            TestField {
                path: "/message/content/0/text",
                value: "looking at it now",
            },
            TestField {
                path: "/message/content/1/type",
                value: "tool_use",
            },
            TestField {
                path: "/message/content/1/input/command",
                value: "grep -rn limiter src/",
            },
        ];
        let meta = record_meta(&assistant);
        assert_eq!(meta.event_kind, EventKind::Assistant);
        assert_eq!(meta.role.as_deref(), Some("assistant"));
        let body = select_body(&assistant, &meta, 300).body;
        assert!(body.contains("looking at it now"));
        assert!(body.contains("grep -rn limiter src/"));

        let result = vec![
            TestField {
                path: "/type",
                value: "user",
            },
            TestField {
                path: "/message/role",
                value: "user",
            },
            TestField {
                path: "/message/content/0/type",
                value: "tool_result",
            },
            TestField {
                path: "/message/content/0/content",
                value: "src/limiter.rs:12: burst window",
            },
        ];
        let meta = record_meta(&result);
        assert_eq!(
            meta.event_kind,
            EventKind::Output,
            "a tool result filed under a user record is not something a person said"
        );
    }

    #[test]
    fn command_arguments_survive_names_that_collide_with_identity_fields() {
        let fields = vec![
            TestField {
                path: "/response_item/type",
                value: "function_call",
            },
            TestField {
                path: "/response_item/call_id",
                value: "c1",
            },
            TestField {
                path: "/response_item/name",
                value: "schedule_meeting",
            },
            TestField {
                path: "/response_item/arguments/date",
                value: "argument-date",
            },
            TestField {
                path: "/response_item/arguments/role",
                value: "argument-role",
            },
            TestField {
                path: "/response_item/arguments/repository",
                value: "argument-repository",
            },
            TestField {
                path: "/response_item/arguments/command",
                value: "argument-command",
            },
        ];
        let meta = record_meta(&fields);
        assert_eq!(meta.event_kind, EventKind::Command);
        let body = select_body(&fields, &meta, 300).body;
        for (path, value) in [
            ("/response_item/name", "schedule_meeting"),
            ("/response_item/arguments/date", "argument-date"),
            ("/response_item/arguments/role", "argument-role"),
            ("/response_item/arguments/repository", "argument-repository"),
            ("/response_item/arguments/command", "argument-command"),
        ] {
            assert!(
                crate::format::body_fields(&body).contains(&(path.to_string(), value.to_string())),
                "projection dropped {}",
                path
            );
        }
        assert!(!body.contains("/response_item/call_id"));
        assert!(!body.contains("/response_item/type"));
    }

    #[test]
    fn unknown_event_preserves_all_scalar_fields() {
        let fields = vec![
            TestField {
                path: "/type",
                value: "vendor_record",
            },
            TestField {
                path: "/arbitrary/deep/value",
                value: "preserve-me",
            },
        ];
        let meta = record_meta(&fields);
        assert_eq!(meta.event_kind, EventKind::Unknown);
        assert!(
            crate::format::body_fields(&select_body(&fields, &meta, 300).body).contains(&(
                "/arbitrary/deep/value".to_string(),
                "preserve-me".to_string()
            ))
        );
    }

    #[test]
    fn interleaved_sessions_preserve_turn_state() {
        let mut machine = IngestStateMachine::new();
        let user = |session: &str| RecordMeta {
            session: Some(session.to_string()),
            event_kind: EventKind::User,
            ..RecordMeta::default()
        };
        let assistant = |session: &str| RecordMeta {
            session: Some(session.to_string()),
            event_kind: EventKind::Assistant,
            ..RecordMeta::default()
        };

        let a_open = machine.apply("fallback", user("a"), "a asks".to_string(), 0);
        let b_open = machine.apply("fallback", user("b"), "b asks".to_string(), 100);
        let a_reply = machine.apply("fallback", assistant("a"), "a answered".to_string(), 200);
        let b_reply = machine.apply("fallback", assistant("b"), "b answered".to_string(), 300);

        let turn = |event: &CanonicalEvent| event.meta.turn.clone().expect("turn");
        assert_eq!(
            turn(&a_open),
            turn(&a_reply),
            "a session that yields to another has not ended"
        );
        assert_eq!(turn(&b_open), turn(&b_reply));
        assert_ne!(turn(&a_open), turn(&b_open));

        assert_eq!(turn(&a_open), super::derived_turn_id(0));
        assert_eq!(turn(&b_open), super::derived_turn_id(100));

        let a_again = machine.apply("fallback", user("a"), "a asks again".to_string(), 400);
        assert_eq!(turn(&a_again), super::derived_turn_id(400));
    }

    #[test]
    fn reply_with_tool_call_is_recovered_in_both_categories() {
        let mut state = RecoveryState::default();
        let mut reply = recovery_event(0, "turn@0", EventKind::Assistant);
        reply.body = encoded(&[
            ("/message/content/0/type", "text"),
            ("/message/content/0/text", "on it"),
            ("/message/content/1/type", "tool_use"),
            ("/message/content/1/input/command", "ls -la"),
        ]);
        state.accept(reply, Vec::new());

        let current = state.current().expect("current turn");
        assert_eq!(current.assistants.len(), 1);
        assert_eq!(
            current.commands.len(),
            1,
            "a format that files the call inside the reply still ran a command"
        );

        let mut plain = recovery_event(1, "turn@0", EventKind::Assistant);
        plain.body = encoded(&[
            ("/message/content/0/type", "text"),
            ("/message/content/0/text", "done"),
        ]);
        state.accept(plain, Vec::new());
        let current = state.current().expect("current turn");
        assert_eq!(current.assistants.len(), 2);
        assert_eq!(current.commands.len(), 1);
    }

    #[test]
    fn reentered_turn_is_recovered_completely() {
        let event = |index: u64, session: &str, turn: &str, kind: EventKind| {
            let mut event = recovery_event(index, turn, kind);
            event.session = session.to_string();
            event
        };
        let mut state = RecoveryState::default();
        state.accept(event(0, "s", "tA", EventKind::User), Vec::new());
        state.accept(event(1, "s", "tB", EventKind::User), Vec::new());
        state.accept(event(2, "s", "tA", EventKind::Assistant), Vec::new());
        let current = state.current().expect("a current turn");
        assert_eq!(current.turn, "tA");
        assert_eq!(current.events_seen, 2, "both stretches of tA are held");
        assert_eq!(current.users_seen, 1, "the user message is not lost");
        assert!(!current.reentered, "tA was marked incomplete");
        let previous = state.previous().expect("a previous turn");
        assert_eq!(previous.turn, "tB");
    }

    #[test]
    fn window_overflow_marks_turn_partial() {
        let event = |index: u64, session: &str, turn: &str, kind: EventKind| {
            let mut event = recovery_event(index, turn, kind);
            event.session = session.to_string();
            event
        };
        let mut state = RecoveryState::default();
        state.accept(event(0, "s", "tA", EventKind::User), Vec::new());
        state.accept(event(1, "s", "tB", EventKind::User), Vec::new());
        state.accept(event(2, "s", "tC", EventKind::User), Vec::new());
        state.accept(event(3, "s", "tA", EventKind::Assistant), Vec::new());
        let current = state.current().expect("a current turn");
        assert_eq!(current.turn, "tA");
        assert_eq!(current.users_seen, 0, "the earlier stretch is gone");
        assert!(current.reentered, "partial turn was reported as complete");
    }

    #[test]
    fn interleaved_recovery_preserves_turn_boundaries() {
        let event = |index: u64, session: &str, turn: &str, kind: EventKind| {
            let mut event = recovery_event(index, turn, kind);
            event.session = session.to_string();
            event
        };
        let mut state = RecoveryState::default();
        state.accept(event(0, "a", "turn@0", EventKind::User), Vec::new());
        state.accept(event(1, "b", "turn@100", EventKind::User), Vec::new());
        state.accept(event(2, "a", "turn@0", EventKind::Assistant), Vec::new());
        state.accept(event(3, "b", "turn@100", EventKind::Assistant), Vec::new());

        let current = state.current().expect("current turn");
        assert_eq!(current.session, "b");
        assert_eq!(current.turn, "turn@100");
        assert_eq!(current.events_seen, 2);
        assert!(
            state.previous().is_none(),
            "b has only one turn, so it has no previous one"
        );
    }

    #[test]
    fn ingest_state_machine_survives_checkpoint() {
        let mut machine = IngestStateMachine::new();
        let first = machine.apply(
            "fallback-session",
            RecordMeta {
                event_kind: EventKind::User,
                ..RecordMeta::default()
            },
            "user".to_string(),
            0,
        );
        let checkpoint = machine.checkpoint();
        let mut resumed = IngestStateMachine::from_checkpoint(checkpoint);
        let second = resumed.apply(
            "fallback-session",
            RecordMeta {
                event_kind: EventKind::Assistant,
                ..RecordMeta::default()
            },
            "assistant".to_string(),
            120,
        );
        assert_eq!(first.event_index, 0);
        assert_eq!(second.event_index, 1);
        assert_eq!(first.meta.session, second.meta.session);
        assert_eq!(first.meta.turn, second.meta.turn);
    }

    #[test]
    fn recovery_state_tracks_current_and_previous_turns() {
        let mut state = RecoveryState::default();
        state.accept(recovery_event(0, "turn-1", EventKind::User), Vec::new());
        state.accept(
            recovery_event(1, "turn-1", EventKind::Assistant),
            Vec::new(),
        );
        let mut call = recovery_event(2, "turn-2", EventKind::Command);
        call.event_type = "function_call".to_string();
        state.accept(call, Vec::new());
        state.finish();
        assert_eq!(state.matched_events(), 3);
        assert_eq!(state.current().expect("current").turn, "turn-2");
        assert_eq!(state.previous().expect("previous").turn, "turn-1");
        assert_eq!(state.current().expect("current").commands.len(), 1);
    }

    fn encoded(pairs: &[(&str, &str)]) -> String {
        let mut body = String::new();
        for (path, value) in pairs {
            crate::format::push_field(&mut body, path, value);
        }
        body
    }

    fn classified(
        origin: super::SourceOrigin,
        pairs: &[(&'static str, &'static str)],
    ) -> (super::Sender, String, EventKind, String) {
        let fields = pairs
            .iter()
            .map(|(path, value)| TestField { path, value })
            .collect::<Vec<_>>();
        let mut meta = record_meta(&fields);
        super::classify_sender(origin, &fields, &mut meta);
        let body = select_body(&fields, &meta, 300).body;
        (meta.sender, meta.via, meta.event_kind, body)
    }

    #[test]
    fn claude_records_name_who_sent_them() {
        use super::{Sender, SourceOrigin::*};
        let user = |text: &'static str| {
            vec![
                ("/type", "user"),
                ("/message/role", "user"),
                ("/message/content/0/type", "text"),
                ("/message/content/0/text", text),
            ]
        };
        let (sender, via, kind, body) = classified(
            ClaudeMain,
            &user("<system-reminder>context</system-reminder>\nplease fix it"),
        );
        assert_eq!(
            (sender, via.as_str(), kind),
            (Sender::Human, "typed", EventKind::User)
        );
        assert_eq!(super::human_text_from_body(&body).0, "please fix it");

        let (sender, via, ..) = classified(
            ClaudeMain,
            &user("<system-reminder>only context</system-reminder>"),
        );
        assert_eq!((sender, via.as_str()), (Sender::System, "injected"));
        let (sender, via, ..) = classified(
            ClaudeMain,
            &user("<task-notification><task-id>b1</task-id>"),
        );
        assert_eq!((sender, via.as_str()), (Sender::System, "notification"));
        let (sender, via, ..) =
            classified(ClaudeMain, &user("<command-name>/compact</command-name>"));
        assert_eq!((sender, via.as_str()), (Sender::Human, "slash_command"));
        let (sender, via, ..) = classified(ClaudeSubagent, &user("Survey the code base"));
        assert_eq!((sender, via.as_str()), (Sender::Agent, "subagent_prompt"));

        let (sender, via, kind, _) = classified(
            ClaudeMain,
            &[
                ("/type", "user"),
                ("/isCompactSummary", "true"),
                ("/message/role", "user"),
                ("/message/content", "This session is being continued"),
            ],
        );
        assert_eq!(
            (sender, via.as_str(), kind),
            (Sender::Summary, "compact_summary", EventKind::Summary)
        );
        let (sender, via, ..) = classified(
            ClaudeMain,
            &[
                ("/type", "user"),
                ("/message/role", "user"),
                ("/message/content/0/type", "tool_result"),
                ("/message/content/0/content", "ls output"),
            ],
        );
        assert_eq!((sender, via.as_str()), (Sender::System, "tool_result"));
        let (sender, via, ..) = classified(
            ClaudeMain,
            &[
                ("/type", "user"),
                ("/isMeta", "true"),
                ("/message/role", "user"),
                ("/message/content/0/type", "text"),
                ("/message/content/0/text", "Base directory for this skill"),
            ],
        );
        assert_eq!((sender, via.as_str()), (Sender::System, "meta"));

        let queued = |prompt: &'static str| {
            vec![
                ("/attachment/type", "queued_command"),
                ("/attachment/prompt", prompt),
                ("/type", "attachment"),
            ]
        };
        let (sender, via, kind, body) = classified(ClaudeMain, &queued("stop and report"));
        assert_eq!(
            (sender, via.as_str(), kind),
            (Sender::Human, "queued", EventKind::User)
        );
        assert_eq!(super::human_text_from_body(&body).0, "stop and report");
        let (sender, via, kind, _) =
            classified(ClaudeMain, &queued("<task-notification><status>completed"));
        assert_eq!(
            (sender, via.as_str(), kind),
            (Sender::System, "notification", EventKind::User)
        );

        let fields = [("/type", "system"), ("/subtype", "compact_boundary")]
            .iter()
            .map(|(path, value)| TestField { path, value })
            .collect::<Vec<_>>();
        let mut meta = record_meta(&fields);
        super::classify_sender(ClaudeMain, &fields, &mut meta);
        assert_eq!(meta.event_type.as_deref(), Some("compact_boundary"));
    }

    #[test]
    fn codex_records_name_who_sent_them() {
        use super::{Sender, SourceOrigin::*};
        let user = |text: &'static str| {
            vec![
                ("/type", "response_item"),
                ("/payload/type", "message"),
                ("/payload/role", "user"),
                ("/payload/content/0/type", "input_text"),
                ("/payload/content/0/text", text),
            ]
        };
        let (sender, via, kind, _) = classified(CodexThread, &user("make the map smaller"));
        assert_eq!(
            (sender, via.as_str(), kind),
            (Sender::Human, "typed", EventKind::User)
        );

        let (sender, via, _, body) = classified(
            CodexThread,
            &user("<in-app-browser-context source=\"ambient-ui-state\">tab list</in-app-browser-context>\n## My request: fix the timeline"),
        );
        assert_eq!((sender, via.as_str()), (Sender::Human, "typed"));
        let text = super::human_text_from_body(&body).0;
        assert!(
            text.contains("fix the timeline") && !text.contains("tab list"),
            "{text}"
        );

        for (text, expected, expected_via) in [
            (
                "<environment_context>cwd</environment_context>",
                Sender::System,
                "injected",
            ),
            (
                "# AGENTS.md instructions for C:/repo",
                Sender::System,
                "instructions",
            ),
            (
                "<heartbeat><automation_id>x</automation_id>",
                Sender::System,
                "automation",
            ),
            (
                "<codex_delegation><input>check it</input>",
                Sender::Agent,
                "agent_message",
            ),
            (
                "<send_user_message_question_reply>[{\"answer\":\"no\"}]",
                Sender::Human,
                "question_reply",
            ),
        ] {
            let (sender, via, ..) = classified(CodexThread, &user(text));
            assert_eq!((sender, via.as_str()), (expected, expected_via), "{text}");
        }
        let (sender, via, kind, _) = classified(
            CodexThread,
            &user("This session is being continued from a previous conversation that ran out"),
        );
        assert_eq!(
            (sender, via.as_str(), kind),
            (Sender::Summary, "compact_summary", EventKind::Summary)
        );
        let (sender, via, ..) = classified(CodexChild, &user("audit the parser"));
        assert_eq!((sender, via.as_str()), (Sender::Agent, "codex_child"));
        let (sender, via, ..) = classified(CodexExec, &user("Answer in one line"));
        assert_eq!((sender, via.as_str()), (Sender::Agent, "codex_exec"));
        let (sender, via, ..) = classified(
            CodexThread,
            &[
                ("/type", "response_item"),
                ("/payload/type", "message"),
                ("/payload/role", "developer"),
                ("/payload/content/0/type", "input_text"),
                ("/payload/content/0/text", "<app-context>"),
            ],
        );
        assert_eq!((sender, via.as_str()), (Sender::System, "instructions"));
    }

    #[test]
    fn person_body_reads_back_exactly() {
        let (_, _, _, body) = classified(
            super::SourceOrigin::ClaudeMain,
            &[
                ("/type", "user"),
                ("/message/role", "user"),
                ("/message/content/0/type", "text"),
                (
                    "/message/content/0/text",
                    "line one\n/looks/like\ta path\nline three",
                ),
                ("/message/content/1/type", "image"),
                ("/message/content/1/source/data", "iVBORw0KGgo="),
            ],
        );
        let (text, images) = super::human_text_from_body(&body);
        assert_eq!(text, "line one\n/looks/like\ta path\nline three");
        assert_eq!(images, 1);
        assert!(
            !body.contains("iVBORw0KGgo="),
            "image data stays out of the projection"
        );
    }

    #[test]
    fn notifications_stay_inside_the_person_turn() {
        use super::Sender;
        let mut machine = IngestStateMachine::new();
        let message = |sender: Sender| RecordMeta {
            session: Some("s".to_string()),
            event_kind: EventKind::User,
            sender,
            ..RecordMeta::default()
        };
        let asked = machine.apply("f", message(Sender::Human), String::new(), 0);
        let notified = machine.apply("f", message(Sender::System), String::new(), 10);
        let summary = RecordMeta {
            session: Some("s".to_string()),
            event_kind: EventKind::Summary,
            sender: Sender::Summary,
            ..RecordMeta::default()
        };
        let summarized = machine.apply("f", summary, String::new(), 20);
        let asked_again = machine.apply("f", message(Sender::Human), String::new(), 30);
        let turn = |event: &CanonicalEvent| event.meta.turn.clone().expect("turn");
        assert_eq!(turn(&asked), turn(&notified));
        assert_eq!(turn(&asked), turn(&summarized));
        assert_eq!(turn(&asked_again), super::derived_turn_id(30));
    }

    #[test]
    fn codex_records_inherit_the_thread_working_directory() {
        let mut machine = IngestStateMachine::new();
        let first = RecordMeta {
            cwd: Some("C:/projects/game".to_string()),
            ..RecordMeta::default()
        };
        machine.apply("f", first, String::new(), 0);
        let later = machine.apply("f", RecordMeta::default(), String::new(), 10);
        assert_eq!(later.meta.cwd.as_deref(), Some("C:/projects/game"));
    }

    fn recovery_event(index: u64, turn: &str, kind: EventKind) -> RecoveryEvent {
        RecoveryEvent {
            event_index: index,
            source_ref: SourceRef {
                source_id: "source".to_string(),
                line: index + 1,
                byte_start: index * 10,
                byte_len: 10,
            },
            session: "session".to_string(),
            turn: turn.to_string(),
            role: kind.as_str().to_string(),
            kind,
            event_type: String::new(),
            timestamp: String::new(),
            cwd: String::new(),
            repository: String::new(),
            call_id: String::new(),
            body: String::new(),
            body_truncated: false,
            embedded_history: false,
            sender: super::Sender::Unknown,
            via: String::new(),
        }
    }

    /// What a sync writes for the same logs is fixed by `corpus::RULES_VERSION`. This digest of
    /// how representative records are classified and projected changes with any change to
    /// those rules. When it does, increase `corpus::RULES_VERSION` and record the new pair
    /// here: the next sync then rebuilds every corpus made under the old rules.
    const RULES_FINGERPRINT: (&str, u64, &str) = ("7", 3, "7021927ff4debeec");

    #[test]
    fn reading_rules_are_versioned() {
        use super::SourceOrigin::{self, *};
        let long_output: &'static str = Box::leak("x".repeat(400).into_boxed_str());
        let records: Vec<(SourceOrigin, Vec<(&'static str, &'static str)>)> = vec![
            (
                ClaudeMain,
                vec![
                    ("/type", "user"),
                    ("/message/role", "user"),
                    ("/message/content", "please fix it"),
                ],
            ),
            (
                ClaudeMain,
                vec![
                    ("/type", "user"),
                    ("/message/role", "user"),
                    ("/message/content/0/type", "text"),
                    (
                        "/message/content/0/text",
                        "<system-reminder>context</system-reminder>\n    keep this indent\n",
                    ),
                ],
            ),
            (
                ClaudeMain,
                vec![
                    ("/type", "user"),
                    ("/message/role", "user"),
                    ("/message/content/0/type", "image"),
                    ("/message/content/0/source/type", "base64"),
                ],
            ),
            (
                ClaudeMain,
                vec![
                    ("/type", "user"),
                    ("/message/role", "user"),
                    ("/message/content/0/type", "tool_result"),
                    ("/message/content/0/content", "a.txt"),
                ],
            ),
            (
                ClaudeMain,
                vec![
                    ("/type", "user"),
                    ("/isCompactSummary", "true"),
                    ("/message/role", "user"),
                    (
                        "/message/content",
                        "This session is being continued from a previous conversation",
                    ),
                ],
            ),
            (
                ClaudeMain,
                vec![
                    ("/type", "user"),
                    ("/message/role", "user"),
                    (
                        "/message/content",
                        "<task-notification>done</task-notification>",
                    ),
                ],
            ),
            (
                ClaudeMain,
                vec![
                    ("/type", "attachment"),
                    ("/attachment/type", "queued_command"),
                    ("/attachment/prompt", "also check the docs"),
                ],
            ),
            (
                ClaudeMain,
                vec![
                    ("/type", "user"),
                    ("/message/role", "user"),
                    ("/message/content", "<command-name>/review</command-name>"),
                ],
            ),
            (
                ClaudeMain,
                vec![
                    ("/type", "user"),
                    ("/isMeta", "true"),
                    ("/message/role", "user"),
                    ("/message/content", "Caveat"),
                ],
            ),
            (
                ClaudeMain,
                vec![
                    ("/type", "assistant"),
                    ("/message/role", "assistant"),
                    ("/message/content/0/type", "text"),
                    ("/message/content/0/text", "on it"),
                    ("/message/content/1/type", "tool_use"),
                    ("/message/content/1/name", "Bash"),
                    ("/message/content/1/input/command", "ls"),
                ],
            ),
            (
                ClaudeMain,
                vec![("/type", "system"), ("/subtype", "compact_boundary")],
            ),
            (
                ClaudeSubagent,
                vec![
                    ("/type", "user"),
                    ("/message/role", "user"),
                    ("/message/content", "Survey the parser"),
                ],
            ),
            (
                CodexThread,
                vec![
                    ("/type", "response_item"),
                    ("/payload/type", "message"),
                    ("/payload/role", "user"),
                    ("/payload/content/0/type", "input_text"),
                    (
                        "/payload/content/0/text",
                        "<in-app-browser-context>tabs</in-app-browser-context>\n## shrink it",
                    ),
                ],
            ),
            (
                CodexThread,
                vec![
                    ("/type", "response_item"),
                    ("/payload/type", "message"),
                    ("/payload/role", "user"),
                    ("/payload/content/0/type", "input_text"),
                    (
                        "/payload/content/0/text",
                        "<environment_context>cwd</environment_context>",
                    ),
                ],
            ),
            (
                CodexThread,
                vec![
                    ("/type", "response_item"),
                    ("/payload/type", "message"),
                    ("/payload/role", "user"),
                    ("/payload/content/0/type", "input_image"),
                    ("/payload/content/0/image_url", "data:image/png;base64,AAAA"),
                ],
            ),
            (
                CodexThread,
                vec![
                    ("/type", "event_msg"),
                    ("/payload/type", "user_message"),
                    ("/payload/message", "shrink it"),
                ],
            ),
            (
                CodexThread,
                vec![
                    ("/type", "response_item"),
                    ("/payload/type", "message"),
                    ("/payload/role", "assistant"),
                    ("/payload/content/0/type", "output_text"),
                    ("/payload/content/0/text", "done"),
                ],
            ),
            (
                CodexThread,
                vec![
                    ("/type", "response_item"),
                    ("/payload/type", "function_call_output"),
                    ("/payload/output", long_output),
                ],
            ),
            (
                CodexChild,
                vec![
                    ("/type", "response_item"),
                    ("/payload/type", "message"),
                    ("/payload/role", "user"),
                    ("/payload/content/0/type", "input_text"),
                    ("/payload/content/0/text", "audit the parser"),
                ],
            ),
            (
                CodexExec,
                vec![
                    ("/type", "response_item"),
                    ("/payload/type", "message"),
                    ("/payload/role", "user"),
                    ("/payload/content/0/type", "input_text"),
                    ("/payload/content/0/text", "Answer in one line"),
                ],
            ),
            (
                ClaudeMain,
                vec![
                    ("/type", "assistant"),
                    ("/message/role", "assistant"),
                    ("/message/content/0/type", "thinking"),
                    ("/message/content/0/thinking", "weighing two options"),
                    ("/message/content/0/signature", "c2lnbmF0dXJl"),
                    ("/message/content/1/type", "text"),
                    ("/message/content/1/text", "chose the first"),
                ],
            ),
            (
                ClaudeMain,
                vec![
                    ("/type", "attachment"),
                    ("/attachment/type", "queued_command"),
                    (
                        "/attachment/prompt",
                        "<task-notification>done</task-notification>",
                    ),
                ],
            ),
            (
                ClaudeMain,
                vec![
                    ("/message/content/0/type", "text"),
                    ("/type", "user"),
                    ("/message/role", "user"),
                    ("/message/content/0/text", "a nested type comes first"),
                ],
            ),
            (
                CodexThread,
                vec![
                    ("/type", "compacted"),
                    ("/payload/message", "Summary of the work so far"),
                    ("/payload/replacement_history/0/role", "user"),
                    (
                        "/payload/replacement_history/0/content/0/text",
                        "kept request",
                    ),
                ],
            ),
            (
                CodexThread,
                vec![("/type", "compacted"), ("/payload/message", "")],
            ),
            (
                Generic,
                vec![("/type", "note"), ("/text", "a format without senders")],
            ),
        ];
        let mut lines = Vec::new();
        for (origin, pairs) in &records {
            let (sender, via, kind, body) = classified(*origin, pairs);
            lines.push(format!(
                "{}|{}|{}|{}\n",
                sender.as_str(),
                via,
                kind.as_str(),
                body
            ));
        }
        // A sync also puts every record on a date for the timeline.
        for offset_minutes in [0, 9 * 60] {
            for timestamp in [
                "2026-08-17T15:00:00Z",
                "2026-08-17T23:00:00",
                "2026/08/17 10:00:00",
                "08/17/2026",
                "2026-08-17T25:00:00Z",
                "",
            ] {
                lines.push(format!(
                    "{}\n",
                    crate::time::date_bucket(timestamp, offset_minutes)
                ));
            }
        }
        let mut digest = 0xcbf29ce484222325u64;
        for byte in lines.concat().bytes() {
            digest ^= u64::from(byte);
            digest = digest.wrapping_mul(0x100000001b3);
        }
        let fingerprint = format!("{:016x}", digest);
        assert_eq!(
            (
                crate::format::FORMAT_VERSION,
                crate::corpus::RULES_VERSION,
                fingerprint.as_str()
            ),
            RULES_FINGERPRINT,
            "what a sync writes changed: increase corpus::RULES_VERSION (or format::FORMAT_VERSION \
             for a storage change), then record the versions with fingerprint \"{}\"",
            fingerprint
        );
    }

    #[test]
    fn a_codex_screenshot_without_words_is_the_persons_message() {
        use super::{Sender, SourceOrigin::CodexThread};
        let (sender, via, kind, body) = classified(
            CodexThread,
            &[
                ("/type", "response_item"),
                ("/payload/type", "message"),
                ("/payload/role", "user"),
                ("/payload/content/0/type", "input_image"),
                ("/payload/content/0/image_url", "data:image/png;base64,AAAA"),
            ],
        );
        assert_eq!(
            (sender, via.as_str(), kind),
            (Sender::Human, "typed", EventKind::User)
        );
        assert_eq!(super::human_text_from_body(&body), (String::new(), 1));
        let (sender, ..) = classified(
            CodexThread,
            &[
                ("/type", "response_item"),
                ("/payload/type", "message"),
                ("/payload/role", "user"),
                ("/payload/content/0/type", "input_text"),
                ("/payload/content/0/text", ""),
            ],
        );
        assert_eq!(
            sender,
            Sender::System,
            "an empty message without images is not the person's"
        );
    }

    #[test]
    fn the_persons_text_is_kept_as_sent() {
        use super::SourceOrigin::{ClaudeMain, CodexThread};
        let claude = |text: &'static str| {
            let (_, _, _, body) = classified(
                ClaudeMain,
                &[
                    ("/type", "user"),
                    ("/message/role", "user"),
                    ("/message/content", text),
                ],
            );
            super::human_text_from_body(&body).0
        };
        assert_eq!(claude("    let x = 1;\n"), "    let x = 1;\n");
        assert_eq!(
            claude("\n\nfirst line\n  second\n"),
            "\n\nfirst line\n  second\n"
        );
        assert_eq!(
            claude("<system-reminder>context</system-reminder>\n  indented request"),
            "  indented request",
            "a leading block takes the line break after it, not the person's indentation"
        );
        assert_eq!(
            claude("keep the colors\n\n<system-reminder>context</system-reminder>"),
            "keep the colors"
        );
        assert_eq!(
            claude("before\n<system-reminder>context</system-reminder>\nafter"),
            "before\n\nafter"
        );
        let (_, _, _, body) = classified(
            CodexThread,
            &[
                ("/type", "response_item"),
                ("/payload/type", "message"),
                ("/payload/role", "user"),
                ("/payload/content/0/type", "input_text"),
                (
                    "/payload/content/0/text",
                    "<in-app-browser-context>tabs</in-app-browser-context>\n## shrink it\n",
                ),
            ],
        );
        assert_eq!(super::human_text_from_body(&body).0, "## shrink it\n");
    }

    #[test]
    fn a_records_type_comes_from_fixed_paths_not_from_key_order() {
        let fields = [
            TestField {
                path: "/message/content/0/type",
                value: "text",
            },
            TestField {
                path: "/type",
                value: "user",
            },
        ];
        assert_eq!(record_meta(&fields).event_type.as_deref(), Some("user"));
        let codex = [
            TestField {
                path: "/type",
                value: "response_item",
            },
            TestField {
                path: "/payload/type",
                value: "message",
            },
        ];
        assert_eq!(record_meta(&codex).event_type.as_deref(), Some("message"));
    }

    #[test]
    fn a_codex_compaction_is_a_summary_when_it_carries_one() {
        use super::{Sender, SourceOrigin::CodexThread};
        let (sender, via, kind, body) = classified(
            CodexThread,
            &[
                ("/type", "compacted"),
                ("/payload/message", "Summary of the work so far"),
                ("/payload/replacement_history/0/role", "user"),
            ],
        );
        assert_eq!(
            (sender, via.as_str(), kind),
            (Sender::Summary, "compact_summary", EventKind::Summary)
        );
        assert!(crate::format::body_fields(&body).contains(&(
            "/payload/message".to_string(),
            "Summary of the work so far".to_string()
        )));
        let (sender, via, kind, _) = classified(
            CodexThread,
            &[("/type", "compacted"), ("/payload/message", "")],
        );
        assert_eq!(
            (sender, via.as_str(), kind),
            (Sender::System, "compact_boundary", EventKind::Unknown)
        );
        assert!(super::is_compaction("compacted") && super::is_compaction("compact_boundary"));
        assert!(!super::is_compaction("context_compacted"));
    }

    #[test]
    fn model_reasoning_is_not_projected() {
        use super::SourceOrigin::ClaudeMain;
        let (_, _, _, body) = classified(
            ClaudeMain,
            &[
                ("/type", "assistant"),
                ("/message/role", "assistant"),
                ("/message/content/0/type", "thinking"),
                ("/message/content/0/thinking", "weighing two options"),
                ("/message/content/0/signature", "c2lnbmF0dXJl"),
                ("/message/content/1/type", "text"),
                ("/message/content/1/text", "chose the first"),
            ],
        );
        assert!(body.contains("chose the first"), "{body}");
        assert!(
            !body.contains("weighing") && !body.contains("c2lnbmF0dXJl"),
            "{body}"
        );
    }

    #[test]
    fn a_format_without_markers_has_an_unknown_sender() {
        use super::{Sender, SourceOrigin::Generic};
        let (sender, ..) = classified(Generic, &[("/type", "note"), ("/text", "plain")]);
        assert_eq!(sender, Sender::Unknown);
        assert_eq!(Sender::Unknown.as_str(), "unknown");
    }
}
