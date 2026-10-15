//! `ebira ask` without the API: the records an answer would read, the ledger, and what the
//! commands refuse. Nothing here calls the API.

use std::path::{Path, PathBuf};
use std::process::Command;

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

/// Runs ebira with a home and data directories inside `home`, so nothing outside the test is
/// read or written.
fn ebira(home: &Path) -> Command {
    let mut command = Command::new(EBIRA);
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("LOCALAPPDATA", home.join("local"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("EBIRA_CORPUS")
        .env_remove("EBIRA_TZ_OFFSET")
        .env_remove("EBIRA_ASK_BUDGET_USD");
    command
}

struct Logs {
    root: PathBuf,
}

impl Logs {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("ebira-ask-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join(".claude").join("projects").join("C--work-game");
        std::fs::create_dir_all(&project).expect("create claude directories");
        std::fs::write(
            project.join("s-main.jsonl"),
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"keep the old colors on the map"},"sessionId":"s-main","timestamp":"2026-09-01T00:00:00Z","cwd":"C:/work/game"}"#, "\n",
                r#"{"type":"queue-operation","operation":"dequeue","sessionId":"s-main","timestamp":"2026-09-01T00:00:00Z"}"#, "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls maps","description":"List the maps"}}]},"sessionId":"s-main","timestamp":"2026-09-01T00:00:01Z","cwd":"C:/work/game"}"#, "\n",
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"map.txt"}]},"sessionId":"s-main","timestamp":"2026-09-01T00:00:02Z","cwd":"C:/work/game"}"#, "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"kept the old colors"}]},"sessionId":"s-main","timestamp":"2026-09-01T00:00:03Z","cwd":"C:/work/game"}"#, "\n",
                r#"{"type":"user","message":{"role":"user","content":"now make the colors brighter"},"sessionId":"s-main","timestamp":"2026-09-02T00:00:00Z","cwd":"C:/work/game"}"#, "\n",
            ),
        )
        .expect("write the session");
        let logs = Logs { root };
        logs.run(&[
            "sync",
            "--source",
            &logs.path(".claude/projects"),
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

    fn output(&self, args: &[&str]) -> std::process::Output {
        ebira(&self.root).args(args).output().expect("ebira runs")
    }

    fn run(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "ebira {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("ebira writes UTF-8")
    }

    fn run_failure(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(
            !output.status.success(),
            "ebira {:?} unexpectedly succeeded: {}",
            args,
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8(output.stdout).expect("ebira writes UTF-8")
    }
}

impl Drop for Logs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn without_the_api_it_lists_the_records_an_answer_would_read() {
    let logs = Logs::new("records");
    let corpus = logs.corpus();
    let text = logs.run(&[
        "ask",
        "What did I say about the map colors?",
        "--corpus",
        &corpus,
        "--term",
        "COLORS",
        "--term",
        "map",
        "--no-api",
        "--format",
        "text",
    ]);
    assert!(
        text.starts_with("Question: What did I say about the map colors?"),
        "{text}"
    );
    assert!(text.contains("Nothing was spent"), "{text}");
    assert!(
        text.contains("▶ person 2026-09-01 00:00: keep the old colors on the map"),
        "the person's message, found with the case folded: {text}"
    );
    assert!(
        text.contains(
            "tool call 2026-09-01 00:00: [Bash] command: ls maps | description: List the maps"
        ),
        "the tool call after it: {text}"
    );
    assert!(
        !text.contains("dequeue"),
        "a queue operation is not a message: {text}"
    );
    assert!(text.contains("Search strings: COLORS, map"), "{text}");

    let json = logs.run(&[
        "ask", "colors", "--corpus", &corpus, "--term", "brighter", "--no-api",
    ]);
    assert!(
        json.starts_with(r#"{"disposition":"records","mode":"ask","question":"colors""#),
        "{json}"
    );
    assert!(json.contains(r#""author":"person""#), "{json}");
    assert!(json.contains("now make the colors brighter"), "{json}");
    assert!(
        json.contains(r#""person":{"returned":1,"total":1}"#),
        "{json}"
    );

    let none = logs.run(&[
        "ask",
        "q",
        "--corpus",
        &corpus,
        "--term",
        "no such words",
        "--no-api",
    ]);
    assert!(none.starts_with(r#"{"disposition":"no_records""#), "{none}");
}

#[test]
fn leaves_out_copies_imported_into_another_thread() {
    // A Codex thread that imported the Claude conversation holds copies of its records, stamped
    // at the import two days later; the originals carry the real times.
    let root = std::env::temp_dir().join(format!("ebira-ask-imported-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let claude = root.join(".claude").join("projects").join("C--work-game");
    let codex = root.join(".codex").join("sessions");
    std::fs::create_dir_all(&claude).expect("create claude directories");
    std::fs::create_dir_all(&codex).expect("create codex directories");
    std::fs::write(
        claude.join("s-main.jsonl"),
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"keep the old colors on the map"},"sessionId":"s-main","timestamp":"2026-09-01T00:00:00Z","cwd":"C:/work/game"}"#, "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"kept the old colors on the map"}]},"sessionId":"s-main","timestamp":"2026-09-01T00:00:03Z","cwd":"C:/work/game"}"#, "\n",
        ),
    )
    .expect("write the original conversation");
    std::fs::write(
        codex.join("rollout-2026-09-03T02-00-00-c-import.jsonl"),
        concat!(
            r#"{"timestamp":"2026-09-03T02:00:00Z","type":"session_meta","payload":{"id":"c-import","originator":"Codex Desktop","source":"vscode","cwd":"C:/work/game"}}"#, "\n",
            // As Codex writes an import: each message twice, the reply after the turn closes.
            r#"{"timestamp":"2026-09-03T02:00:00.100Z","type":"event_msg","payload":{"type":"task_started","turn_id":"external-import-turn-1"}}"#, "\n",
            r#"{"timestamp":"2026-09-03T02:00:00.100Z","type":"event_msg","payload":{"type":"user_message","message":"keep the old colors on the map"}}"#, "\n",
            r#"{"timestamp":"2026-09-03T02:00:00.100Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"keep the old colors on the map"}]}}"#, "\n",
            r#"{"timestamp":"2026-09-03T02:00:00.100Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"external-import-turn-1"}}"#, "\n",
            r#"{"timestamp":"2026-09-03T02:00:00.100Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"kept the old colors on the map"}]}}"#, "\n",
            r#"{"timestamp":"2026-09-03T02:05:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}"#, "\n",
            r#"{"timestamp":"2026-09-03T02:05:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"now draw the legend"}]}}"#, "\n",
        ),
    )
    .expect("write the thread that imported it");
    let corpus = root.join("corpus");
    let corpus = corpus.to_str().expect("utf-8 path");
    let run = |args: &[&str]| {
        let output = ebira(&root).args(args).output().expect("ebira runs");
        assert!(
            output.status.success(),
            "ebira {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("ebira writes UTF-8")
    };
    let claude_logs = root.join(".claude").join("projects");
    let codex_logs = codex.clone();
    run(&[
        "sync",
        "--source",
        claude_logs.to_str().expect("utf-8 path"),
        "--source",
        codex_logs.to_str().expect("utf-8 path"),
        "--corpus",
        corpus,
    ]);

    let said = run(&[
        "said",
        "--query",
        "old colors",
        "--corpus",
        corpus,
        "--format",
        "text",
    ]);
    assert!(said.contains("1 of 1 messages"), "{said}");
    assert!(
        !said.contains("codex"),
        "neither copy of the imported message is listed as said: {said}"
    );

    let json = run(&[
        "ask",
        "q",
        "--corpus",
        corpus,
        "--term",
        "old colors",
        "--no-api",
    ]);
    assert!(json.contains("\"source_id\":\"claude-"), "{json}");
    assert!(
        !json.contains("\"source_id\":\"codex-"),
        "the copies are left out, the agent's as well as the person's: {json}"
    );

    let text = run(&[
        "ask", "q", "--corpus", corpus, "--term", "legend", "--no-api", "--format", "text",
    ]);
    assert!(text.contains("now draw the legend"), "{text}");
    assert!(
        !text.contains("old colors"),
        "the imported copies before it are not shown as its neighbours: {text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn it_refuses_what_it_cannot_do_before_spending() {
    let logs = Logs::new("refusals");
    let corpus = logs.corpus();
    let no_terms = logs.run_failure(&["ask", "q", "--corpus", &corpus, "--no-api"]);
    assert!(no_terms.contains("--no-api needs --term"), "{no_terms}");
    let no_question = logs.run_failure(&["ask", "--corpus", &corpus, "--no-api"]);
    assert!(
        no_question.contains("ask needs a question"),
        "{no_question}"
    );
    let effort = logs.run_failure(&["ask", "q", "--corpus", &corpus, "--effort", "huge"]);
    assert!(effort.contains("--effort must be"), "{effort}");
    let keep = logs.run_failure(&["ask", "q", "--corpus", &corpus, "--keep", "0"]);
    assert!(keep.contains("--keep must be from 1 to 120"), "{keep}");
    let date = logs.run_failure(&["ask", "q", "--corpus", &corpus, "--from", "yesterday"]);
    assert!(date.contains("--from must be a date"), "{date}");
    let two = logs.run_failure(&["ask", "q", "r", "--corpus", &corpus]);
    assert!(two.contains("unknown option for ask: r"), "{two}");
}

#[test]
fn the_ledger_starts_empty_and_names_its_place() {
    let logs = Logs::new("usage");
    let usage = logs.run(&["ask-usage"]);
    assert!(
        usage.starts_with(r#"{"disposition":"ready","mode":"ask_usage","ledger":"#),
        "{usage}"
    );
    assert!(usage.contains(r#""total_usd":0.0000"#), "{usage}");
    assert!(usage.contains(r#""budget_usd":null"#), "{usage}");
    let text = logs.run(&["ask-usage", "--format", "text"]);
    assert!(text.starts_with("What `ebira ask` spent"), "{text}");
    let budget = ebira(&logs.root)
        .args(["ask-usage", "--format", "text"])
        .env("EBIRA_ASK_BUDGET_USD", "200")
        .output()
        .expect("ebira runs");
    assert!(String::from_utf8_lossy(&budget.stdout).contains("Left of $200"));
}

#[test]
fn help_lists_the_ask_commands() {
    let logs = Logs::new("help");
    let overview = logs.run(&["help"]);
    for command in ["ask", "ask-setup", "ask-usage"] {
        assert!(
            overview.contains(&format!("\n  {command} ")),
            "the overview lists {command}: {overview}"
        );
    }
    let ask = logs.run(&["help", "ask"]);
    assert!(ask.starts_with("ebira ask <question>"), "{ask}");
    assert!(ask.contains("go to the Anthropic API"), "{ask}");
    let setup = logs.run(&["help", "ask-setup"]);
    assert!(
        !setup.contains("--corpus"),
        "ask-setup reads no corpus: {setup}"
    );
}
