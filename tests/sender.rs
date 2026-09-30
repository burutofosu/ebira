//! Who sent each record: the person's own messages are listed by `said`, filtered by
//! `--sender human`, and recovered by `resume`, apart from agent-written and injected text.

use std::path::PathBuf;
use std::process::Command;

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

/// Runs ebira with a home that holds no transcripts, so nothing outside the test is read.
fn ebira() -> Command {
    let home = std::env::temp_dir().join("ebira-test-empty-home");
    let mut command = Command::new(EBIRA);
    command
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("EBIRA_CORPUS");
    command
}

fn run(args: &[&str]) -> String {
    let output = ebira().args(args).output().expect("ebira runs");
    assert!(
        output.status.success(),
        "ebira {:?} failed: {}{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("ebira writes UTF-8")
}

struct Logs {
    root: PathBuf,
}

impl Logs {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("ebira-sender-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join(".claude").join("projects").join("C--work-game");
        let subagents = project.join("s-main").join("subagents");
        std::fs::create_dir_all(&subagents).expect("create claude directories");
        std::fs::write(
            project.join("s-main.jsonl"),
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"please build the map"},"sessionId":"s-main","timestamp":"2026-09-01T00:00:00Z","cwd":"C:/work/game"}"#, "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"on it"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]},"sessionId":"s-main","timestamp":"2026-09-01T00:00:01Z","cwd":"C:/work/game"}"#, "\n",
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"a.txt"}]},"sessionId":"s-main","timestamp":"2026-09-01T00:00:02Z"}"#, "\n",
                r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"also check the docs"},"sessionId":"s-main","timestamp":"2026-09-01T00:00:03Z"}"#, "\n",
                r#"{"type":"user","message":{"role":"user","content":"<task-notification><status>completed</status></task-notification>"},"sessionId":"s-main","timestamp":"2026-09-01T00:00:04Z"}"#, "\n",
                r#"{"type":"system","subtype":"compact_boundary","sessionId":"s-main","timestamp":"2026-09-01T00:00:05Z"}"#, "\n",
                r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"This session is being continued from a previous conversation"},"sessionId":"s-main","timestamp":"2026-09-01T00:00:06Z"}"#, "\n",
                r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<system-reminder>ctx-only</system-reminder>"},{"type":"text","text":"keep the old colors"}]},"sessionId":"s-main","timestamp":"2026-09-01T00:00:07Z","cwd":"C:/work/game"}"#, "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"kept the colors"}]},"sessionId":"s-main","timestamp":"2026-09-01T00:00:08Z","cwd":"C:/work/game"}"#, "\n",
            ),
        )
        .expect("write main session");
        std::fs::write(
            subagents.join("agent-a1.jsonl"),
            concat!(
                r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"Survey the parser"},"sessionId":"s-main","timestamp":"2026-09-01T00:00:02.500Z"}"#, "\n",
                r#"{"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"surveyed"}]},"sessionId":"s-main","timestamp":"2026-09-01T00:00:02.600Z"}"#, "\n",
            ),
        )
        .expect("write subagent transcript");
        let copy = root.join(".claude").join("projects").join("C--work-copy");
        std::fs::create_dir_all(&copy).expect("create copy directory");
        std::fs::write(
            copy.join("s-copy.jsonl"),
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"please build the map"},"sessionId":"s-copy","timestamp":"2026-09-01T00:00:00Z","cwd":"C:/work/other"}"#, "\n",
            ),
        )
        .expect("write copied session");

        let codex = root
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("09")
            .join("01");
        std::fs::create_dir_all(&codex).expect("create codex directories");
        std::fs::write(
            codex.join("rollout-2026-09-01T01-00-00-c-top.jsonl"),
            concat!(
                r#"{"timestamp":"2026-09-01T01:00:00Z","type":"session_meta","payload":{"id":"c-top","originator":"Codex Desktop","source":"vscode","cwd":"C:/work/game"}}"#, "\n",
                r#"{"timestamp":"2026-09-01T01:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>cwd</environment_context>"}]}}"#, "\n",
                r#"{"timestamp":"2026-09-01T01:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<in-app-browser-context source=\"ambient-ui-state\">tabs</in-app-browser-context>\n## My request: shrink the timeline"}]}}"#, "\n",
                r#"{"timestamp":"2026-09-01T01:00:03Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"shrunk"}]}}"#, "\n",
            ),
        )
        .expect("write codex thread");
        std::fs::write(
            codex.join("rollout-2026-09-01T01-10-00-c-child.jsonl"),
            concat!(
                r#"{"timestamp":"2026-09-01T01:10:00Z","type":"session_meta","payload":{"session_id":"c-top","id":"c-child","parent_thread_id":"c-top","originator":"Codex Desktop","source":{"subagent":{"thread_spawn":{"parent_thread_id":"c-top"}}}}}"#, "\n",
                r#"{"timestamp":"2026-09-01T01:10:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"audit the parser"}]}}"#, "\n",
            ),
        )
        .expect("write codex child thread");
        std::fs::write(
            codex.join("rollout-2026-09-01T01-20-00-c-exec.jsonl"),
            concat!(
                r#"{"timestamp":"2026-09-01T01:20:00Z","type":"session_meta","payload":{"id":"c-exec","originator":"codex_exec","source":"exec"}}"#, "\n",
                r#"{"timestamp":"2026-09-01T01:20:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Answer in one line"}]}}"#, "\n",
            ),
        )
        .expect("write codex exec thread");
        std::fs::write(
            codex.join("rollout-2026-09-01T02-00-00-c-import.jsonl"),
            concat!(
                r#"{"timestamp":"2026-09-01T02:00:00Z","type":"session_meta","payload":{"id":"c-import","originator":"Codex Desktop","source":"vscode","cwd":"C:/work/game"}}"#, "\n",
                r#"{"timestamp":"2026-09-01T02:00:00.100Z","type":"event_msg","payload":{"type":"task_started","turn_id":"external-import-turn-1"}}"#, "\n",
                r#"{"timestamp":"2026-09-01T02:00:00.100Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"please build the map"}]}}"#, "\n",
                r#"{"timestamp":"2026-09-01T02:00:00.100Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"This session is being continued from a previous conversation that ran out of context."}]}}"#, "\n",
            ),
        )
        .expect("write imported codex thread");

        let logs = Self { root };
        run(&[
            "sync",
            "--source",
            &logs.path(".claude/projects"),
            "--source",
            &logs.path(".codex/sessions"),
            "--corpus",
            &logs.corpus(),
        ]);
        logs
    }

    fn path(&self, relative: &str) -> String {
        self.root
            .join(relative)
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }

    fn corpus(&self) -> String {
        self.path("corpus")
    }
}

impl Drop for Logs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const NOT_THE_PERSON: &[&str] = &[
    "Survey the parser",
    "audit the parser",
    "Answer in one line",
    "being continued",
    "task-notification",
    "environment_context",
    "ctx-only",
    "tabs",
];

#[test]
fn said_lists_only_what_the_person_sent() {
    let logs = Logs::new("said");
    let said = run(&["said", "--corpus", &logs.corpus()]);
    for text in [
        "please build the map",
        "also check the docs",
        "keep the old colors",
        "## My request: shrink the timeline",
    ] {
        assert!(said.contains(text), "said misses {text:?}: {said}");
    }
    for text in NOT_THE_PERSON {
        assert!(!said.contains(text), "said includes {text:?}: {said}");
    }
    assert!(said.contains("\"total_messages\":4"), "{said}");
    assert!(said.contains("\"copies_skipped\":1"), "{said}");
    assert!(said.contains("\"imported_skipped\":1"), "{said}");
    assert!(said.contains("\"via\":\"queued\""), "{said}");
    let with_imports = run(&["said", "--corpus", &logs.corpus(), "--include-imported"]);
    assert!(
        with_imports.contains("\"total_messages\":5")
            && with_imports.contains("\"via\":\"imported\""),
        "an imported copy is the person's words with the import time: {with_imports}"
    );
    assert!(
        !with_imports.contains("being continued"),
        "a summary imported without its flag is still a summary: {with_imports}"
    );
    let newest = said.find("shrink the timeline").expect("codex message");
    let oldest = said.find("please build the map").expect("first message");
    assert!(newest < oldest, "said lists the newest first: {said}");

    let codex = run(&["said", "--corpus", &logs.corpus(), "--agent", "codex"]);
    assert!(codex.contains("\"total_messages\":1"), "{codex}");
    let project = run(&["said", "--corpus", &logs.corpus(), "--cwd", "work/game"]);
    assert!(
        project.contains("\"total_messages\":4") && project.contains("shrink the timeline"),
        "a Codex message inherits the thread's working directory: {project}"
    );
    let session = run(&[
        "said",
        "--corpus",
        &logs.corpus(),
        "--session",
        "s-main",
        "--order",
        "asc",
    ]);
    assert!(session.contains("\"total_messages\":3"), "{session}");
    assert!(
        session.find("please build the map") < session.find("keep the old colors"),
        "{session}"
    );
    let text = run(&[
        "said",
        "--corpus",
        &logs.corpus(),
        "--format",
        "text",
        "--limit",
        "1",
    ]);
    assert!(
        text.starts_with("# ebira said") && text.contains("# more: --offset 1"),
        "{text}"
    );
}

#[test]
fn search_filters_by_sender() {
    let logs = Logs::new("search");
    let human = run(&[
        "search",
        "--corpus",
        &logs.corpus(),
        "--query",
        "the",
        "--sender",
        "human",
    ]);
    assert!(human.contains("keep the old colors"), "{human}");
    for text in NOT_THE_PERSON {
        assert!(
            !human.contains(text),
            "--sender human includes {text:?}: {human}"
        );
    }
    let agents = run(&[
        "search",
        "--corpus",
        &logs.corpus(),
        "--query",
        "parser",
        "--sender",
        "agent",
    ]);
    assert!(
        agents.contains("\"via\":\"subagent_prompt\"")
            && agents.contains("\"via\":\"codex_child\""),
        "{agents}"
    );
    let summaries = run(&[
        "search",
        "--corpus",
        &logs.corpus(),
        "--query",
        "continued",
        "--kind",
        "summary",
    ]);
    assert!(summaries.contains("\"sender\":\"summary\""), "{summaries}");
    let newest = run(&[
        "search",
        "--corpus",
        &logs.corpus(),
        "--query",
        "the",
        "--session",
        "s-main",
        "--order",
        "desc",
    ]);
    assert!(
        newest.contains("\"order\":\"reverse_chronological\"")
            && newest.find("keep the old colors") < newest.find("please build the map"),
        "{newest}"
    );
}

#[test]
fn resume_by_session_uses_the_main_transcript() {
    let logs = Logs::new("resume");
    let resumed = run(&["resume", "--corpus", &logs.corpus(), "--session", "s-main"]);
    assert!(resumed.contains("\"disposition\":\"ready\""), "{resumed}");
    assert!(!resumed.contains("ambiguous_current_session"), "{resumed}");
    assert!(
        resumed.contains("\"source_path\":")
            && !resumed.contains("agent-a1.jsonl\",\"source_disposition\""),
        "{resumed}"
    );
    assert!(resumed.contains("\"latest_human_message\":{"), "{resumed}");
    assert!(
        resumed.contains("\"compactions\":{\"count\":1"),
        "{resumed}"
    );

    let brief = run(&[
        "resume",
        "--corpus",
        &logs.corpus(),
        "--session",
        "s-main",
        "--brief",
    ]);
    assert!(brief.contains("\"mode\":\"resume_brief\""), "{brief}");
    for text in [
        "please build the map",
        "also check the docs",
        "keep the old colors",
        "kept the colors",
    ] {
        assert!(brief.contains(text), "brief misses {text:?}: {brief}");
    }
    for text in ["being continued", "task-notification", "Survey the parser"] {
        assert!(!brief.contains(text), "brief includes {text:?}: {brief}");
    }
    assert!(brief.contains("\"name\":\"Bash\""), "{brief}");

    // A Codex child thread records its parent's session id; the parent's own rollout wins.
    let thread = run(&[
        "resume",
        "--corpus",
        &logs.corpus(),
        "--session",
        "c-top",
        "--brief",
    ]);
    assert!(
        thread.contains("\"mode\":\"resume_brief\"")
            && thread.contains("shrink the timeline")
            && !thread.contains("audit the parser"),
        "{thread}"
    );
}

#[test]
fn follow_returns_messages_from_the_requested_sender() {
    let logs = Logs::new("follow");
    let claude = logs.path(".claude/projects/C--work-game/s-main.jsonl");
    let follow = |source: &str, sender: &str| {
        run(&[
            "follow",
            "--corpus",
            &logs.corpus(),
            "--source",
            source,
            "--after-byte",
            "0",
            "--seconds",
            "0",
            "--sender",
            sender,
        ])
    };
    let human = follow(&claude, "human");
    assert!(
        human.contains("\"disposition\":\"received\"")
            && human.contains("\"sender\":\"human\"")
            && human.contains("please build the map")
            && human.contains("keep the old colors"),
        "{human}"
    );
    // A prompt typed while the agent was working reaches follow as it reaches said.
    assert!(
        human.contains("also check the docs") && human.contains("\"via\":\"queued\""),
        "follow --sender human misses the queued prompt: {human}"
    );
    for text in ["task-notification", "being continued", "ctx-only", "on it"] {
        assert!(
            !human.contains(text),
            "follow --sender human includes {text:?}: {human}"
        );
    }
    let replies = follow(&claude, "assistant");
    assert!(
        replies.contains("on it")
            && replies.contains("kept the colors")
            && !replies.contains("please build the map"),
        "{replies}"
    );
    let codex = logs.path(".codex/sessions/2026/09/01/rollout-2026-09-01T01-00-00-c-top.jsonl");
    let person = follow(&codex, "human");
    assert!(
        person.contains("## My request: shrink the timeline")
            && !person.contains("environment_context")
            && !person.contains("tabs"),
        "{person}"
    );
}

#[test]
fn follow_finds_a_session_registered_as_a_file_or_in_a_directory() {
    let logs = Logs::new("follow-session");
    let single = logs.root.join("single-files").join("s-file.jsonl");
    std::fs::create_dir_all(single.parent().expect("a parent directory"))
        .expect("create directory");
    std::fs::write(
        &single,
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"registered as one file"},"sessionId":"s-file","uuid":"u1","timestamp":"2026-09-02T00:00:00Z"}"#,
            "\n"
        ),
    )
    .expect("write the single transcript");
    run(&[
        "sync",
        "--corpus",
        &logs.corpus(),
        "--source",
        single.to_str().expect("utf-8 path"),
    ]);
    let follow = |session: &str| {
        run(&[
            "follow",
            "--corpus",
            &logs.corpus(),
            "--session",
            session,
            "--after-byte",
            "0",
            "--seconds",
            "0",
            "--sender",
            "human",
        ])
    };
    let by_file = follow("s-file");
    assert!(
        by_file.contains("\"disposition\":\"received\"")
            && by_file.contains("registered as one file"),
        "a session registered as a file: {by_file}"
    );
    let by_directory = follow("s-main");
    assert!(
        by_directory.contains("please build the map"),
        "a session found inside a registered directory: {by_directory}"
    );
}
