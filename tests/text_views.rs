//! The text views give times in the corpus's zone and name it, and `search` has one too; JSON
//! keeps each record's timestamp as written and adds its local time.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

/// A Claude Code session with two messages from the person and a reply, written in UTC.
const SESSION: &str = concat!(
    r#"{"type":"user","sessionId":"s1","uuid":"u1","timestamp":"2026-09-01T00:00:00Z","cwd":"/work","message":{"role":"user","content":"keep the retry budget at three"}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"s1","uuid":"a1","timestamp":"2026-09-01T00:00:30Z","cwd":"/work","message":{"role":"assistant","content":[{"type":"text","text":"The retry budget stays at three."}]}}"#,
    "\n",
    r#"{"type":"user","sessionId":"s1","uuid":"u2","timestamp":"2026-09-01T15:30:00Z","cwd":"/work","message":{"role":"user","content":"now the retry budget is four"}}"#,
    "\n",
);

struct Corpus {
    root: PathBuf,
    corpus: String,
}

impl Corpus {
    /// The session in a corpus built at `offset`.
    fn new(name: &str, offset: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("ebira-text-views-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join(".claude").join("projects").join("work");
        std::fs::create_dir_all(&source).expect("create source directory");
        std::fs::write(source.join("s1.jsonl"), SESSION).expect("write session");
        let corpus = root
            .join("corpus")
            .to_str()
            .expect("UTF-8 path")
            .to_string();
        let sync = command(&root, offset)
            .args(["sync", "--corpus", &corpus, "--source"])
            .arg(&source)
            .output()
            .expect("ebira runs");
        assert!(sync.status.success(), "{}", text(&sync.stderr));
        Self { root, corpus }
    }

    fn run(&self, args: &[&str]) -> Output {
        command(&self.root, "")
            .args(args)
            .args(["--corpus", &self.corpus])
            .output()
            .expect("ebira runs")
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "ebira {args:?} failed: {}{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        text(&output.stdout)
    }
}

impl Drop for Corpus {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// ebira with a home that holds no transcripts; `offset` is `EBIRA_TZ_OFFSET`, which only a
/// sync that builds a corpus reads.
fn command(root: &Path, offset: &str) -> Command {
    let mut command = Command::new(EBIRA);
    command
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("EBIRA_TZ_OFFSET", offset)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("EBIRA_CORPUS");
    command
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn search_has_a_text_view_with_local_times_and_references() {
    let corpus = Corpus::new("search", "+09:00");
    let found = corpus.ok(&["search", "--query", "retry budget", "--format", "text"]);
    let mut lines = found.lines();
    assert_eq!(
        lines.next(),
        Some(
            r#"# ebira search: records holding "retry budget", oldest first, times in UTC+09:00 (stale sources: 0, unreachable sources: 0, incomplete sources: 0)"#
        ),
        "{found}"
    );
    assert!(
        found
            .contains("\n[2026-09-01 09:00] human (user, via: typed) session=s1 source-id=claude-"),
        "{found}"
    );
    assert!(found.contains("keep the retry budget at three"), "{found}");
    assert!(
        found.contains("[2026-09-02 00:30] human"),
        "15:30 UTC is past midnight at +09:00: {found}"
    );
    assert!(
        found.contains(" byte=0+"),
        "a reference context takes: {found}"
    );
    assert!(!found.contains("# more"), "{found}");

    let page = corpus.ok(&[
        "search",
        "--query",
        "retry budget",
        "--limit",
        "1",
        "--format",
        "text",
    ]);
    assert!(page.contains("\n# more: --offset 1\n"), "{page}");

    let newest = corpus.ok(&[
        "search",
        "--query",
        "retry budget",
        "--order",
        "desc",
        "--format",
        "text",
    ]);
    assert!(newest.contains(", newest first, "), "{newest}");
    assert!(
        newest.find("is four").expect("newest") < newest.find("at three").expect("oldest"),
        "{newest}"
    );

    let original = corpus.ok(&[
        "search",
        "--query",
        "budget is four",
        "--raw",
        "--format",
        "text",
    ]);
    assert!(
        original.starts_with(
            r#"# ebira search: records holding "budget is four" in the original records, oldest first"#
        ),
        "{original}"
    );
    assert!(original.contains("[2026-09-02 00:30] human"), "{original}");
}

#[test]
fn no_match_in_text_says_what_the_json_disposition_says() {
    let corpus = Corpus::new("nomatch", "+09:00");
    let none = corpus.ok(&["search", "--query", "never written", "--format", "text"]);
    assert!(
        none.contains("\n# no match: no_match_in_projection; ") && none.contains("--raw"),
        "{none}"
    );
    let raw = corpus.ok(&[
        "search",
        "--query",
        "never written",
        "--raw",
        "--format",
        "text",
    ]);
    assert!(
        raw.contains("\n# no match: no_match_in_source_records\n"),
        "{raw}"
    );
}

#[test]
fn several_queries_have_a_text_view_too() {
    let corpus = Corpus::new("many", "+09:00");
    let found = corpus.ok(&[
        "search", "--query", "at three", "--query", "is four", "--format", "text",
    ]);
    assert!(
        found.starts_with("# ebira search: 2 queries, oldest first, times in UTC+09:00"),
        "{found}"
    );
    assert!(
        found.contains("\n## \"at three\": 2 of 2 records\n"),
        "{found}"
    );
    assert!(
        found.contains("\n## \"is four\": 1 of 1 records\n"),
        "{found}"
    );
}

#[test]
fn search_json_keeps_the_timestamp_and_adds_the_local_time() {
    let corpus = Corpus::new("json", "+09:00");
    let found = corpus.ok(&["search", "--query", "is four"]);
    assert!(
        found.contains(r#""timestamp":"2026-09-01T15:30:00Z","local_time":"2026-09-02 00:30""#),
        "{found}"
    );
}

#[test]
fn said_and_context_name_the_zone_of_their_times() {
    let corpus = Corpus::new("said-context", "+09:00");
    let said = corpus.ok(&["said", "--format", "text"]);
    assert!(
        said.starts_with(
            "# ebira said: 2 of 2 messages from the person, newest first, times in UTC+09:00 ("
        ),
        "{said}"
    );
    assert!(said.contains("\n[2026-09-02 00:30] claude typed"), "{said}");
    let many = corpus.ok(&[
        "said", "--query", "three", "--query", "four", "--format", "text",
    ]);
    assert!(
        many.starts_with("# ebira said: 2 queries, newest first, times in UTC+09:00 ("),
        "{many}"
    );

    let length = SESSION
        .lines()
        .take(2)
        .map(|line| line.len() + 1)
        .sum::<usize>();
    let readable = corpus.ok(&[
        "context",
        "--source-id",
        &source_id(&corpus),
        "--byte-start",
        "0",
        "--byte-len",
        &length.to_string(),
        "--format",
        "text",
    ]);
    assert!(
        readable.contains("\n[2026-09-01 09:00:00 UTC+09:00] human (user, via: typed)\n"),
        "{readable}"
    );
    assert!(
        readable.contains("\n[2026-09-01 09:00:30 UTC+09:00] assistant (assistant, via: "),
        "{readable}"
    );
}

#[test]
fn a_corpus_built_without_an_offset_names_utc() {
    let corpus = Corpus::new("utc", "");
    let found = corpus.ok(&["search", "--query", "is four", "--format", "text"]);
    assert!(found.contains(", times in UTC+00:00 "), "{found}");
    assert!(found.contains("[2026-09-01 15:30] human"), "{found}");
}

#[test]
fn only_json_and_text_are_formats() {
    let corpus = Corpus::new("formats", "+09:00");
    let wrong = corpus.run(&["search", "--query", "retry", "--format", "csv"]);
    assert!(!wrong.status.success());
    assert!(
        text(&wrong.stdout).contains("--format must be json or text"),
        "{}",
        text(&wrong.stdout)
    );
    let history = corpus.run(&["history", "--query", "retry", "--format", "text"]);
    assert!(!history.status.success(), "history writes JSON only");
}

fn source_id(corpus: &Corpus) -> String {
    let found = corpus.ok(&["search", "--query", "at three"]);
    let at = found
        .find("\"source_ref\":{\"source_id\":\"")
        .expect("a match")
        + 27;
    found[at..at + found[at..].find('"').expect("id end")].to_string()
}
