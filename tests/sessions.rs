//! `sessions` lists the conversations: when each ran, where, how much the person said, and how
//! it began and ended. A subagent's transcript counts in its parent's session, and the person's
//! messages are counted as `said` lists them.

use std::path::{Path, PathBuf};
use std::process::Command;

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

const FIRST: &str = "11111111-1111-4111-8111-111111111111";
const SECOND: &str = "22222222-2222-4222-8222-222222222222";
const THREAD: &str = "01a00000-0000-7000-8000-000000000001";
const EXEC: &str = "01a00000-0000-7000-8000-000000000002";

fn claude(session: &str, uuid: &str, time: &str, role: &str, text: &str) -> String {
    let content = if role == "user" {
        format!("\"{text}\"")
    } else {
        format!(r#"[{{"type":"text","text":"{text}"}}]"#)
    };
    format!(
        r#"{{"type":"{role}","sessionId":"{session}","uuid":"{uuid}","timestamp":"2026-09-{time}Z","cwd":"/work/app","message":{{"role":"{role}","content":{content}}}}}"#
    ) + "\n"
}

struct Corpus {
    root: PathBuf,
    corpus: String,
}

impl Corpus {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("ebira-sessions-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let projects = root.join(".claude").join("projects").join("app");
        let subagents = projects.join(FIRST).join("subagents");
        std::fs::create_dir_all(&subagents).expect("create project folder");
        std::fs::write(
            projects.join(format!("{FIRST}.jsonl")),
            [
                claude(FIRST, "u1", "01T00:00:00", "user", "start the cache work"),
                claude(FIRST, "a1", "01T01:00:00", "assistant", "Started."),
                // The agent changed into a folder inside the project.
                claude(FIRST, "a2", "01T01:30:00", "assistant", "In src.")
                    .replace("\"cwd\":\"/work/app\"", "\"cwd\":\"/work/app/src\""),
                claude(FIRST, "u2", "01T02:00:00", "user", "cache done, thanks"),
            ]
            .concat(),
        )
        .expect("write the first session");
        std::fs::write(
            subagents.join("agent-a1.jsonl"),
            [
                claude(
                    FIRST,
                    "s1",
                    "01T00:30:00",
                    "user",
                    "look at the cache layer",
                ),
                claude(FIRST, "s2", "01T00:40:00", "assistant", "Looked."),
            ]
            .concat(),
        )
        .expect("write the subagent");
        std::fs::write(
            projects.join(format!("{SECOND}.jsonl")),
            claude(SECOND, "w1", "03T00:00:00", "user", "release notes please"),
        )
        .expect("write the second session");
        let codex = root
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("09")
            .join("02");
        std::fs::create_dir_all(&codex).expect("create Codex folder");
        // Codex writes the person's message twice, as an event and as a response item.
        std::fs::write(
            codex.join(format!("rollout-2026-09-02T00-00-00-{THREAD}.jsonl")),
            [
                format!(r#"{{"timestamp":"2026-09-02T00:00:00.000Z","type":"session_meta","payload":{{"id":"{THREAD}","cwd":"/work/other","originator":"codex_desktop"}}}}"#),
                r#"{"timestamp":"2026-09-02T00:00:05.000Z","type":"event_msg","payload":{"type":"user_message","message":"fix the parser"}}"#.to_string(),
                r#"{"timestamp":"2026-09-02T00:00:05.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix the parser"}]}}"#.to_string(),
                r#"{"timestamp":"2026-09-02T00:00:09.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Fixed."}]}}"#.to_string(),
                // Codex also writes a working directory as a URL.
                r#"{"timestamp":"2026-09-02T00:00:10.000Z","type":"turn_context","payload":{"cwd":"file:///work/other/sub"}}"#.to_string(),
                String::new(),
            ]
            .join("\n"),
        )
        .expect("write the Codex thread");
        // A thread another program started with `codex exec`: the person wrote nothing in it.
        std::fs::write(
            codex.join(format!("rollout-2026-09-02T05-00-00-{EXEC}.jsonl")),
            [
                format!(r#"{{"timestamp":"2026-09-02T05:00:00.000Z","type":"session_meta","payload":{{"id":"{EXEC}","cwd":"/work/other","originator":"codex_exec","source":"exec"}}}}"#),
                r#"{"timestamp":"2026-09-02T05:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"make the texture"}]}}"#.to_string(),
                r#"{"timestamp":"2026-09-02T05:00:09.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Made."}]}}"#.to_string(),
                String::new(),
            ]
            .join("\n"),
        )
        .expect("write the exec thread");
        let corpus = root.join("corpus").to_str().expect("UTF-8").to_string();
        let corpus = Self { root, corpus };
        let claude_projects = corpus.root.join(".claude").join("projects");
        let codex_sessions = corpus.root.join(".codex").join("sessions");
        let sync = corpus
            .command("+09:00")
            .args(["sync", "--corpus", &corpus.corpus, "--source"])
            .arg(&claude_projects)
            .arg("--source")
            .arg(&codex_sessions)
            .output()
            .expect("ebira runs");
        assert!(
            sync.status.success(),
            "{}",
            String::from_utf8_lossy(&sync.stderr)
        );
        corpus
    }

    fn command(&self, offset: &str) -> Command {
        let mut command = Command::new(EBIRA);
        command
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root)
            .env("EBIRA_TZ_OFFSET", offset)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("EBIRA_CORPUS");
        command
    }

    fn run(&self, args: &[&str]) -> String {
        let output = self
            .command("")
            .args(args)
            .args(["--corpus", &self.corpus])
            .output()
            .expect("ebira runs");
        assert!(
            output.status.success(),
            "ebira {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("UTF-8")
    }
}

impl Drop for Corpus {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn block<'a>(text: &'a str, session: &str) -> &'a str {
    let start = text
        .find(&format!("session={session}"))
        .unwrap_or_else(|| panic!("{session} in {text}"));
    let rest = &text[start..];
    &rest[..rest.find("\n\n").unwrap_or(rest.len())]
}

#[test]
fn every_session_with_its_span_place_and_messages() {
    let corpus = Corpus::new("list");
    let listed = corpus.run(&["sessions", "--format", "text"]);
    assert!(
        listed.starts_with(
            "# ebira sessions: 3 of 3 sessions, newest first, times in UTC+09:00 (stale sources: 0; 1 without a message from the person left out, --all lists them)\n"
        ),
        "{listed}"
    );
    assert!(!listed.contains(EXEC), "{listed}");
    let at = |session: &str| listed.find(&format!("session={session}")).expect("listed");
    assert!(
        at(SECOND) < at(THREAD) && at(THREAD) < at(FIRST),
        "newest first: {listed}"
    );

    let first = block(&listed, FIRST);
    assert!(
        listed.contains(&format!(
            "[2026-09-01 09:00 → 2026-09-01 11:00] claude session={FIRST}"
        )),
        "{listed}"
    );
    assert!(first.contains("\n  in: /work/app\n"), "{first}");
    assert!(
        first.contains(
            "2 messages from the person (38 characters), 3 agent records, 0 compactions, 2 logs"
        ),
        "the subagent's transcript counts in its parent's session: {first}"
    );
    assert!(
        first.contains("\n  first [2026-09-01 09:00]: start the cache work (source-id=claude-"),
        "{first}"
    );
    assert!(
        first.contains("\n  last [2026-09-01 11:00]: cache done, thanks ("),
        "{first}"
    );

    let thread = block(&listed, THREAD);
    assert!(
        thread.starts_with(&format!("session={THREAD}\n  in: /work/other\n")),
        "{thread}"
    );
    assert!(
        thread.contains("1 message from the person (14 characters)"),
        "a message Codex wrote twice counts once: {thread}"
    );
}

#[test]
fn filters_order_and_pages() {
    let corpus = Corpus::new("filters");
    let other = corpus.run(&["sessions", "--cwd", "OTHER", "--format", "text"]);
    assert!(
        other.starts_with("# ebira sessions: 1 of 1 sessions"),
        "{other}"
    );
    assert!(other.contains(THREAD), "{other}");

    let cache = corpus.run(&["sessions", "--query", "cache", "--format", "text"]);
    assert!(
        cache.starts_with("# ebira sessions: 1 of 1 sessions"),
        "{cache}"
    );
    assert!(cache.contains(", 2 holding the query,"), "{cache}");

    let codex = corpus.run(&["sessions", "--app", "codex", "--format", "text"]);
    assert!(
        codex.starts_with("# ebira sessions: 1 of 1 sessions"),
        "{codex}"
    );
    let all = corpus.run(&["sessions", "--app", "codex", "--all", "--format", "text"]);
    assert!(
        all.starts_with("# ebira sessions: 2 of 2 sessions") && !all.contains("left out"),
        "{all}"
    );
    assert!(
        block(&all, EXEC).contains("0 messages from the person (0 characters), 1 agent record,"),
        "{all}"
    );

    let later = corpus.run(&["sessions", "--from", "2026-09-02", "--format", "text"]);
    assert!(
        later.starts_with("# ebira sessions: 2 of 2 sessions"),
        "{later}"
    );
    assert!(!later.contains(FIRST), "{later}");

    let oldest = corpus.run(&[
        "sessions", "--order", "asc", "--limit", "1", "--format", "text",
    ]);
    assert!(
        oldest.starts_with("# ebira sessions: 1 of 3 sessions, oldest first"),
        "{oldest}"
    );
    assert!(oldest.contains(FIRST), "{oldest}");
    assert!(oldest.ends_with("\n# more: --offset 1\n"), "{oldest}");
}

#[test]
fn json_counts_the_persons_messages_as_said_does() {
    let corpus = Corpus::new("json");
    let listed = corpus.run(&["sessions"]);
    assert!(listed.contains("\"mode\":\"sessions\""), "{listed}");
    assert!(listed.contains("\"total_sessions\":3"), "{listed}");
    assert!(
        listed.contains("\"left_out_without_person_messages\":1"),
        "{listed}"
    );
    assert!(listed.contains("\"places\":[\"/work/other\"]"), "{listed}");
    assert!(
        listed.contains("\"local_time_zone\":\"UTC+09:00\""),
        "{listed}"
    );
    let counted = listed
        .match_indices("\"person_messages\":")
        .map(|(at, key)| {
            let rest = &listed[at + key.len()..];
            rest[..rest.find(',').expect("number end")]
                .parse::<u64>()
                .expect("a count")
        })
        .sum::<u64>();
    let said = corpus.run(&["said", "--limit", "1"]);
    assert!(
        said.contains(&format!("\"total_messages\":{counted},")),
        "{counted} against {said}"
    );
    let first = &listed[listed
        .find(&format!("\"session\":\"{FIRST}\""))
        .expect("first")..];
    assert!(
        first.contains(r#""first_message":{"timestamp":"2026-09-01T00:00:00Z","local_time":"2026-09-01 09:00","text":"start the cache work","text_truncated":false,"source_ref":{"source_id":"claude-"#),
        "{first}"
    );
}

#[test]
fn a_session_list_needs_a_corpus_and_a_known_app() {
    let corpus = Corpus::new("errors");
    let wrong = corpus
        .command("")
        .args(["sessions", "--app", "gemini", "--corpus", &corpus.corpus])
        .output()
        .expect("ebira runs");
    assert!(!wrong.status.success());
    let none = corpus
        .command("")
        .args(["sessions", "--corpus"])
        .arg(Path::new(&corpus.root).join("no-corpus"))
        .output()
        .expect("ebira runs");
    assert!(!none.status.success());
}
