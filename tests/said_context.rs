//! `said --context`: each of the person's messages with the end of the agent's reply before it
//! and the agent's last reply in its turn, in either app's log. Tool calls and thinking are not
//! replies, and Codex's second copy of a message or a reply does not split the turn.

use std::path::{Path, PathBuf};
use std::process::Command;

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

/// Runs ebira with a home inside `home`, so nothing outside the test is read.
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
        .env_remove("EBIRA_TZ_OFFSET");
    command
}

const CLAUDE: &str = concat!(
    r#"{"type":"user","sessionId":"c","timestamp":"2026-09-15T00:00:00Z","cwd":"/work/tabi","message":{"role":"user","content":"地図どれ使う？"}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"c","timestamp":"2026-09-15T00:01:00Z","cwd":"/work/tabi","message":{"role":"assistant","content":[{"type":"text","text":"Leaflet がおすすめです。"}]}}"#,
    "\n",
    r#"{"type":"user","sessionId":"c","timestamp":"2026-09-15T00:05:00Z","cwd":"/work/tabi","message":{"role":"user","content":"それでいこう"}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"c","timestamp":"2026-09-15T00:06:00Z","cwd":"/work/tabi","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"npm i leaflet"}}]}}"#,
    "\n",
    r#"{"type":"user","sessionId":"c","timestamp":"2026-09-15T00:07:00Z","cwd":"/work/tabi","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"added 1 package"}]}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"c","timestamp":"2026-09-15T00:08:00Z","cwd":"/work/tabi","message":{"role":"assistant","content":[{"type":"thinking","thinking":"THINKING_IS_NOT_A_REPLY"},{"type":"text","text":"Leaflet で実装しました。"}]}}"#,
    "\n",
    r#"{"type":"user","sessionId":"c","timestamp":"2026-09-15T00:10:00Z","cwd":"/work/tabi","message":{"role":"user","content":"次はボタンの色"}}"#,
    "\n",
);

/// The older Codex format: each message and each reply written twice, as an item and as an
/// event, the event copy without the turn's id.
const CODEX: &str = concat!(
    r#"{"timestamp":"2026-09-16T00:00:00.000Z","type":"session_meta","payload":{"id":"x","timestamp":"2026-09-16T00:00:00.000Z","cwd":"/work/tabi","originator":"codex_cli_rs","cli_version":"0.60.0","source":"cli"}}"#,
    "\n",
    r#"{"timestamp":"2026-09-16T00:00:00.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
    "\n",
    r#"{"timestamp":"2026-09-16T00:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"タイルはどこから取る？"}]}}"#,
    "\n",
    r#"{"timestamp":"2026-09-16T00:00:01.000Z","type":"event_msg","payload":{"type":"user_message","message":"タイルはどこから取る？","images":[]}}"#,
    "\n",
    r#"{"timestamp":"2026-09-16T00:01:00.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"MapLibre がおすすめです。"}]}}"#,
    "\n",
    r#"{"timestamp":"2026-09-16T00:01:00.000Z","type":"event_msg","payload":{"type":"agent_message","message":"MapLibre がおすすめです。"}}"#,
    "\n",
    r#"{"timestamp":"2026-09-16T00:05:00.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}"#,
    "\n",
    r#"{"timestamp":"2026-09-16T00:05:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"OK それ採用"}]}}"#,
    "\n",
    r#"{"timestamp":"2026-09-16T00:05:01.000Z","type":"event_msg","payload":{"type":"user_message","message":"OK それ採用","images":[]}}"#,
    "\n",
    r#"{"timestamp":"2026-09-16T00:08:00.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"MapLibre で実装しました。"}]}}"#,
    "\n",
    r#"{"timestamp":"2026-09-16T00:08:00.000Z","type":"event_msg","payload":{"type":"agent_message","message":"MapLibre で実装しました。"}}"#,
    "\n",
);

struct Logs(PathBuf);

impl Drop for Logs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Logs {
    /// Both logs, synced into a corpus under the home.
    fn synced(name: &str) -> Logs {
        let root =
            std::env::temp_dir().join(format!("ebira-said-context-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let claude = root.join(".claude/projects/C--work-tabi/c.jsonl");
        let codex = root.join(".codex/sessions/2026/09/16/rollout-2026-09-16T00-00-00-x.jsonl");
        for (path, log) in [(&claude, CLAUDE), (&codex, CODEX)] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, log).unwrap();
        }
        let logs = Logs(root);
        logs.run(&[
            "sync",
            "--source",
            claude.to_str().unwrap(),
            "--source",
            codex.to_str().unwrap(),
        ]);
        logs
    }

    fn run(&self, args: &[&str]) -> String {
        let corpus = self.0.join("corpus");
        let output = ebira(&self.0)
            .args(args)
            .args(["--corpus", corpus.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ebira {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

/// The JSON value of `"key":` after `from` in `text`, up to the next comma or brace.
fn value_after<'a>(text: &'a str, from: &str, key: &str) -> &'a str {
    let at = text
        .find(from)
        .unwrap_or_else(|| panic!("{from} in {text}"));
    let rest = &text[at..];
    let start = rest.find(&format!("\"{key}\":")).unwrap() + key.len() + 3;
    let end = rest[start..]
        .find([',', '}'])
        .map_or(rest.len(), |end| start + end);
    &rest[start..end]
}

#[test]
fn each_message_comes_with_the_agents_reply_before_it_and_the_turns_last_reply() {
    let logs = Logs::synced("json");
    let listed = logs.run(&["said", "--order", "asc", "--context"]);
    assert!(
        listed.contains(r#""total_messages":5,"copies_skipped":2,"#),
        "Codex's copies are listed once: {listed}"
    );
    // A short agreement, with what it agreed to and what came of it; the tool call between is
    // not a reply, nor is the thinking.
    let agreed = &listed[listed.find(r#""text":"それでいこう""#).unwrap()..];
    let context = &agreed[agreed.find(r#""context":"#).unwrap()..];
    assert!(
        context.starts_with(r#""context":{"before":{"text":"Leaflet がおすすめです。","chars":16,"timestamp":"2026-09-15T00:01:00Z","local_time":"2026-09-15 00:01","#),
        "{context}"
    );
    assert!(
        context.contains(r#""after":{"text":"Leaflet で実装しました。","chars":16,"timestamp":"2026-09-15T00:08:00Z","local_time":"2026-09-15 00:08","replies_in_turn":1,"#),
        "{context}"
    );
    assert!(!listed.contains("THINKING_IS_NOT_A_REPLY"), "{listed}");
    // The first message of a log has no reply before it; the last has no turn after it.
    assert_eq!(
        value_after(&listed, r#""text":"地図どれ使う？""#, "before"),
        "null"
    );
    assert_eq!(
        value_after(&listed, r#""text":"次はボタンの色""#, "after"),
        "null"
    );
    // Codex: the copies of the message and of the reply do not split the turn or count twice.
    let codex = &listed[listed.find(r#""text":"OK それ採用""#).unwrap()..];
    assert!(
        codex.contains(r#""context":{"before":{"text":"MapLibre がおすすめです。","#)
            && codex.contains(r#""after":{"text":"MapLibre で実装しました。","#),
        "{codex}"
    );
    assert_eq!(value_after(codex, r#""after""#, "replies_in_turn"), "1");
    assert!(
        !logs.run(&["said"]).contains(r#""context""#),
        "only asked for"
    );
}

#[test]
fn the_text_view_shows_the_agents_words_under_each_message_within_the_budget() {
    let logs = Logs::synced("text");
    let shown = logs.run(&["said", "--order", "asc", "--context", "--format", "text"]);
    assert!(
        shown.contains(
            "それでいこう\n  agent before (2026-09-15 00:01): Leaflet がおすすめです。\n  agent after (2026-09-15 00:08, the turn's one reply): Leaflet で実装しました。\n"
        ),
        "{shown}"
    );
    // The agent's words count toward --max-chars: fewer messages fit a page.
    let plain = logs.run(&["said", "--order", "asc", "--max-chars", "40"]);
    let with_context = logs.run(&["said", "--order", "asc", "--max-chars", "40", "--context"]);
    let returned = |listed: &str| value_after(listed, "{", "returned").parse::<u64>().unwrap();
    assert!(
        returned(&with_context) < returned(&plain),
        "{plain}\n{with_context}"
    );
    assert!(
        with_context.contains(r#""truncated":true"#),
        "{with_context}"
    );
    // Several queries in one pass: each list's messages with their context.
    let queried = logs.run(&["said", "--query", "それ", "--query", "次", "--context"]);
    assert_eq!(queried.matches(r#""context":"#).count(), 3, "{queried}");
}
