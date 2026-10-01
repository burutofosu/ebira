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

    let codex = run(&["said", "--corpus", &logs.corpus(), "--app", "codex"]);
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
        newest.contains("\"order\":\"desc\"")
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
            && by_file.contains("registered as one file")
            && by_file.contains("\"source_id\":\"claude-"),
        "a session in the corpus is its transcript there: {by_file}"
    );
    let by_directory = follow("s-main");
    assert!(
        by_directory.contains("please build the map"),
        "a session found inside a registered directory: {by_directory}"
    );
    // A session the corpus has not read yet is found by its file name.
    std::fs::write(
        logs.root
            .join(".claude/projects/C--work-game")
            .join("s-new.jsonl"),
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"written after the sync"},"sessionId":"s-new","uuid":"u9","timestamp":"2026-09-03T00:00:00Z"}"#,
            "
"
        ),
    )
    .expect("write a new transcript");
    let unread = follow("s-new");
    assert!(
        unread.contains("written after the sync") && unread.contains("\"source_id\":null"),
        "{unread}"
    );
}

#[test]
fn resume_reads_the_whole_session_however_it_is_named() {
    let root = std::env::temp_dir().join(format!("ebira-sender-segments-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join(".claude").join("projects").join("C--work");
    std::fs::create_dir_all(&project).expect("create project directory");
    let transcript = project.join("s-seg.jsonl");
    let corpus = root.join("corpus");
    let corpus = corpus.to_str().expect("utf-8 path");
    let turns = [
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"first request"},"sessionId":"s-seg","timestamp":"2026-09-01T00:00:00Z"}"#,
            "\n",
            r#"{"type":"system","subtype":"compact_boundary","sessionId":"s-seg","timestamp":"2026-09-01T00:00:01Z"}"#,
            "\n",
            r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"This session is being continued from a previous conversation"},"sessionId":"s-seg","timestamp":"2026-09-01T00:00:02Z"}"#,
            "\n",
        ),
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"second request"},"sessionId":"s-seg","timestamp":"2026-09-01T00:01:00Z"}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"second answer"}]},"sessionId":"s-seg","timestamp":"2026-09-01T00:01:01Z"}"#,
            "\n",
        ),
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"third request"},"sessionId":"s-seg","timestamp":"2026-09-01T00:02:00Z"}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"third answer"}]},"sessionId":"s-seg","timestamp":"2026-09-01T00:02:01Z"}"#,
            "\n",
        ),
    ];
    let mut written = String::new();
    for (index, turn) in turns.iter().enumerate() {
        written.push_str(turn);
        std::fs::write(&transcript, &written).expect("append a turn");
        if index == 0 {
            run(&[
                "sync",
                "--source",
                project.to_str().expect("utf-8 path"),
                "--corpus",
                corpus,
            ]);
        } else {
            run(&["sync", "--corpus", corpus]);
        }
    }

    let by_session = run(&[
        "resume",
        "--corpus",
        corpus,
        "--session",
        "s-seg",
        "--brief",
    ]);
    let source_id = by_session
        .split("\"source_id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the result names its source")
        .to_string();
    let by_source = run(&[
        "resume",
        "--corpus",
        corpus,
        "--source-id",
        &source_id,
        "--brief",
    ]);
    for result in [&by_session, &by_source] {
        assert!(
            result.contains("\"compactions\":{\"count\":1")
                && result.contains("first request")
                && result.contains("second request")
                && result.contains("third request"),
            "resume misses part of the session: {result}"
        );
    }
    std::fs::remove_dir_all(root).expect("remove test root");
}

/// The `byte_start` and `byte_len` of the message whose text is `text`: in its `source_ref`
/// after the text, or before the text in `follow`'s flat messages.
fn range_of(output: &str, text: &str) -> (u64, u64) {
    let marker = format!("\"text\":\"{text}\"");
    let at = output
        .find(&marker)
        .unwrap_or_else(|| panic!("{text:?} is not in {output}"));
    let after = &output[at + marker.len()..];
    let after = &after[..after.find("\"text\":").unwrap_or(after.len())];
    let number = |key: &str| {
        let key = format!("\"{key}\":");
        let start = match after.find(&key) {
            Some(index) => at + marker.len() + index,
            None => output[..at]
                .rfind(&key)
                .expect("the message has a byte range"),
        } + key.len();
        output[start..]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|digits| digits.parse().ok())
            .expect("a number")
    };
    (number("byte_start"), number("byte_len"))
}

#[test]
fn said_resume_and_follow_mean_the_same_by_the_persons_messages() {
    let logs = Logs::new("person");
    let thread = logs.path(".codex/sessions/2026/09/01/rollout-2026-09-01T02-00-00-c-import.jsonl");
    let mut contents = std::fs::read_to_string(&thread).expect("read the imported thread");
    contents.push_str(concat!(
        r#"{"timestamp":"2026-09-01T02:05:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}"#, "\n",
        r#"{"timestamp":"2026-09-01T02:05:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"and add a legend"}]}}"#, "\n",
    ));
    std::fs::write(&thread, contents).expect("append the person's own message");
    run(&["sync", "--corpus", &logs.corpus()]);

    let said = run(&["said", "--corpus", &logs.corpus(), "--session", "c-import"]);
    let brief = run(&[
        "resume",
        "--corpus",
        &logs.corpus(),
        "--session",
        "c-import",
        "--brief",
    ]);
    let followed = run(&[
        "follow",
        "--corpus",
        &logs.corpus(),
        "--source",
        &thread,
        "--after-byte",
        "0",
        "--seconds",
        "0",
        "--sender",
        "human",
    ]);
    for (command, output) in [("said", &said), ("resume", &brief), ("follow", &followed)] {
        assert!(
            output.contains("and add a legend") && !output.contains("please build the map"),
            "{command} lists the person's own message and not the imported copy: {output}"
        );
    }
    assert!(
        followed.contains("\"source_id\":\"codex-"),
        "a transcript the corpus has read keeps its source id: {followed}"
    );
    assert!(said.contains("\"imported_skipped\":1"), "{said}");
    assert!(
        brief.contains("\"human_messages_seen\":1") && brief.contains("\"imported_skipped\":1"),
        "{brief}"
    );
    let range = range_of(&said, "and add a legend");
    assert_eq!(range_of(&brief, "and add a legend"), range, "{brief}");
    assert_eq!(range_of(&followed, "and add a legend"), range, "{followed}");

    let everything = run(&[
        "follow",
        "--corpus",
        &logs.corpus(),
        "--source",
        &thread,
        "--after-byte",
        "0",
        "--seconds",
        "0",
        "--sender",
        "any",
    ]);
    assert!(
        everything.contains("please build the map") && everything.contains("\"via\":\"imported\""),
        "with --sender any the copy is listed and marked: {everything}"
    );
}

#[test]
fn every_command_reports_what_the_corpus_has_not_read_the_same_way() {
    let logs = Logs::new("freshness");
    let transcript = logs.path(".claude/projects/C--work-game/s-main.jsonl");
    let appended = concat!(
        r#"{"type":"user","message":{"role":"user","content":"one more thing"},"sessionId":"s-main","timestamp":"2026-09-01T00:00:09Z","cwd":"C:/work/game"}"#,
        "\n"
    );
    let mut contents = std::fs::read_to_string(&transcript).expect("read transcript");
    contents.push_str(appended);
    std::fs::write(&transcript, contents).expect("append a message");
    let behind = format!("\"unscanned_source_bytes\":{}", appended.len());

    let corpus = logs.corpus();
    let said = run(&["said", "--corpus", &corpus, "--session", "s-main"]);
    let search = run(&[
        "search",
        "--corpus",
        &corpus,
        "--query",
        "the",
        "--session",
        "s-main",
    ]);
    let timeline = run(&[
        "timeline",
        "--corpus",
        &corpus,
        "--date",
        "2026-09-01",
        "--session",
        "s-main",
    ]);
    let resumed = run(&["resume", "--corpus", &corpus, "--session", "s-main"]);
    for (command, output) in [
        ("said", &said),
        ("search", &search),
        ("timeline", &timeline),
    ] {
        assert!(
            output.contains("\"stale_sources\":1") && output.contains(&behind),
            "{command}: {output}"
        );
    }
    assert!(
        search.contains("\"corpus_sources\":2"),
        "the session's coverage is its own sources, the main transcript and its subagent: {search}"
    );
    assert!(
        resumed.contains("\"source_freshness\":\"behind\"")
            && resumed.contains(&behind)
            && resumed.contains("\"next_recall\":\"sync_then_resume\""),
        "{resumed}"
    );

    run(&["sync", "--corpus", &corpus]);
    let said = run(&["said", "--corpus", &corpus, "--session", "s-main"]);
    let resumed = run(&["resume", "--corpus", &corpus, "--session", "s-main"]);
    assert!(said.contains("\"stale_sources\":0"), "{said}");
    assert!(
        resumed.contains("\"source_freshness\":\"current\"")
            && resumed.contains("\"next_recall\":\"none\""),
        "{resumed}"
    );
}

#[test]
fn search_matches_what_was_written_not_how_the_corpus_stores_it() {
    let logs = Logs::new("values");
    let corpus = logs.corpus();
    let paths = run(&[
        "search",
        "--corpus",
        &corpus,
        "--query",
        "message/content",
        "--sender",
        "human",
    ]);
    assert!(
        paths.contains("\"total_candidates\":0"),
        "a field path is not text anyone wrote: {paths}"
    );
    let words = run(&["search", "--corpus", &corpus, "--query", "old colors"]);
    assert!(
        words.contains("\"field\":\"/message/content")
            && words.contains("\"snippet\":\"keep the old colors\""),
        "a hit names its field and quotes that value alone: {words}"
    );
    let day = run(&["timeline", "--corpus", &corpus, "--date", "2026-09-01"]);
    assert!(
        day.contains("\"snippet\":\"please build the map\"") && !day.contains("\t"),
        "a listing quotes values, not their framing: {day}"
    );
}

#[test]
fn follow_takes_every_record_that_is_the_persons_message() {
    let root = std::env::temp_dir().join(format!("ebira-sender-event-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let sessions = root.join(".codex").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create session directory");
    let log = sessions.join("rollout-2026-09-01T00-00-00-c-event.jsonl");
    std::fs::write(
        &log,
        concat!(
            r#"{"timestamp":"2026-09-01T00:00:00Z","type":"session_meta","payload":{"id":"c-event","originator":"Codex Desktop","source":"vscode"}}"#, "\n",
            r#"{"timestamp":"2026-09-01T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"hello"}}"#, "\n",
            r#"{"timestamp":"2026-09-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"twice written"}]}}"#, "\n",
            r#"{"timestamp":"2026-09-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"twice written"}}"#, "\n",
        ),
    )
    .expect("write rollout");
    let corpus = root.join("corpus");
    let corpus = corpus.to_str().expect("utf-8 path");
    run(&[
        "sync",
        "--corpus",
        corpus,
        "--source",
        sessions.to_str().expect("utf-8 path"),
    ]);
    let said = run(&["said", "--corpus", corpus, "--order", "asc"]);
    let followed = run(&[
        "follow",
        "--corpus",
        corpus,
        "--source",
        log.to_str().expect("utf-8 path"),
        "--after-byte",
        "0",
        "--seconds",
        "0",
        "--sender",
        "human",
    ]);
    for (command, output) in [("said", &said), ("follow", &followed)] {
        assert_eq!(
            output.matches("\"text\":\"hello\"").count(),
            1,
            "{command}: {output}"
        );
        assert_eq!(
            output.matches("\"text\":\"twice written\"").count(),
            1,
            "{command} lists a message written twice once: {output}"
        );
    }
    std::fs::remove_dir_all(root).expect("remove test root");
}

/// Puts `contents` in place of the file at `path` as another file, the way a log is replaced.
fn replace_with(path: &std::path::Path, contents: &str) {
    let staged = path.with_extension("staged");
    std::fs::write(&staged, contents).expect("write the replacement");
    std::fs::rename(&staged, path).expect("replace the file");
}

#[test]
fn resume_does_not_quote_a_replaced_log() {
    let logs = Logs::new("replaced");
    let transcript = logs.path(".claude/projects/C--work-game/s-main.jsonl");
    let brief = || {
        run(&[
            "resume",
            "--corpus",
            &logs.corpus(),
            "--session",
            "s-main",
            "--brief",
        ])
    };
    assert!(brief().contains("kept the colors"));
    // Same length, other words: every old reference now falls on a whole record of the new file.
    let contents = std::fs::read_to_string(&transcript).expect("read transcript");
    replace_with(
        std::path::Path::new(&transcript),
        &contents.replace("kept the colors", "kept the colour"),
    );
    let after = brief();
    assert!(
        !after.contains("kept the colour") && after.contains("\"source_freshness\":\"rewritten\""),
        "{after}"
    );
}

#[test]
fn follow_does_not_carry_the_state_of_a_replaced_log() {
    let root = std::env::temp_dir().join(format!("ebira-sender-restate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let sessions = root.join(".codex").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create session directory");
    let log = sessions.join("rollout-2026-09-01T00-00-00-c-restate.jsonl");
    let meta = r#"{"timestamp":"2026-09-01T00:00:00Z","type":"session_meta","payload":{"id":"c-restate","originator":"Codex Desktop","source":"vscode"}}"#;
    let message = |text: &str| {
        format!(
            r#"{{"timestamp":"2026-09-01T00:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
        )
    };
    let turn = |id: &str| {
        format!(
            r#"{{"timestamp":"2026-09-01T00:00:00Z","type":"event_msg","payload":{{"type":"task_started","turn_id":"{id}"}}}}"#
        )
    };
    // The corpus reads the log just after an imported turn opened, before any message in it.
    let original = format!("{meta}\n{}\n", turn("external-import-turn-1"));
    std::fs::write(&log, &original).expect("write rollout");
    let corpus = root.join("corpus");
    let corpus = corpus.to_str().expect("utf-8 path");
    run(&[
        "sync",
        "--corpus",
        corpus,
        "--source",
        sessions.to_str().expect("utf-8 path"),
    ]);
    // Another log takes its place: as long up to there, but in a turn of its own, and it goes
    // on with the person's message.
    assert_eq!(
        turn("turn-00000000000000001").len(),
        turn("external-import-turn-1").len()
    );
    let replacement = format!(
        "{meta}\n{}\n{}\n",
        turn("turn-00000000000000001"),
        message("new words")
    );
    replace_with(&log, &replacement);
    let followed = run(&[
        "follow",
        "--corpus",
        corpus,
        "--source",
        log.to_str().expect("utf-8 path"),
        "--after-byte",
        &original.len().to_string(),
        "--seconds",
        "0",
        "--sender",
        "human",
    ]);
    assert!(
        followed.contains("\"text\":\"new words\""),
        "the replaced log is read from its own start, not the corpus's checkpoint: {followed}"
    );
    std::fs::remove_dir_all(root).expect("remove test root");
}

/// The value of `"after_byte"` in a follow result.
fn after_byte(output: &str) -> String {
    let start = output.find("\"after_byte\":").expect("after_byte") + "\"after_byte\":".len();
    output[start..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .expect("a number")
        .to_string()
}

#[test]
fn follow_returns_a_message_written_twice_once_across_calls() {
    let root = std::env::temp_dir().join(format!("ebira-sender-across-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let sessions = root.join(".codex").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create session directory");
    let corpus = root.join("corpus");
    let corpus = corpus.to_str().expect("utf-8 path");
    let event = |text: &str| {
        format!(
            r#"{{"timestamp":"2026-09-01T00:00:01Z","type":"event_msg","payload":{{"type":"user_message","message":"{text}"}}}}"#
        )
    };
    let item = |text: &str| {
        format!(
            r#"{{"timestamp":"2026-09-01T00:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
        )
    };
    let meta = r#"{"timestamp":"2026-09-01T00:00:00Z","type":"session_meta","payload":{"id":"c-across","originator":"Codex Desktop","source":"vscode"}}"#;
    // Either record of the message can be written first.
    for (first, second) in [
        (event("hello"), item("hello")),
        (item("hello"), event("hello")),
    ] {
        let log = sessions.join("rollout-2026-09-01T00-00-00-c-across.jsonl");
        std::fs::write(&log, format!("{meta}\n{first}\n")).expect("write rollout");
        run(&[
            "sync",
            "--corpus",
            corpus,
            "--source",
            sessions.to_str().expect("utf-8 path"),
        ]);
        let follow = |after: &str| {
            run(&[
                "follow",
                "--corpus",
                corpus,
                "--source",
                log.to_str().expect("utf-8 path"),
                "--after-byte",
                after,
                "--seconds",
                "0",
                "--sender",
                "human",
            ])
        };
        let returned = follow("0");
        assert!(returned.contains("\"text\":\"hello\""), "{returned}");
        // The other record of the same message arrives after the call, then a new message.
        let mut contents = std::fs::read_to_string(&log).expect("read rollout");
        contents.push_str(&format!("{second}\n{}\n", item("next")));
        std::fs::write(&log, contents).expect("append to rollout");
        let continued = follow(&after_byte(&returned));
        assert!(
            !continued.contains("\"text\":\"hello\"") && continued.contains("\"text\":\"next\""),
            "a message already returned is not returned again: {continued}"
        );
        let said = run(&["said", "--corpus", corpus]);
        assert!(said.contains("\"total_messages\":1"), "{said}");
        std::fs::remove_file(&log).expect("remove rollout");
        let _ = std::fs::remove_dir_all(root.join("corpus"));
    }
    std::fs::remove_dir_all(root).expect("remove test root");
}
