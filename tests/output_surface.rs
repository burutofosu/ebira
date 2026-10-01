use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

const PROSE_NAMES: &[&str] = &[
    "summary",
    "summary_ja",
    "guidance",
    "agent_guidance",
    "reason",
    "advice",
    "explanation",
    "hint",
    "note",
    "narrative",
    "interpretation",
];

/// Runs ebira with a home that holds no transcripts, so nothing outside the test is read.
fn ebira() -> Command {
    ebira_with_home(&std::env::temp_dir().join("ebira-test-empty-home"))
}

fn ebira_with_home(home: &Path) -> Command {
    let mut command = Command::new(EBIRA);
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("EBIRA_CORPUS");
    command
}

fn run(args: &[&str]) -> String {
    let output = ebira().args(args).output().expect("ebira runs");
    assert!(
        output.status.success(),
        "ebira {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("ebira writes UTF-8")
}

fn run_failure(args: &[&str]) -> String {
    let output = ebira().args(args).output().expect("ebira runs");
    assert!(
        !output.status.success(),
        "ebira {:?} unexpectedly succeeded: {}",
        args,
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stdout).expect("ebira writes UTF-8")
}

fn run_with_default_corpus(args: &[&str], corpus: &str) -> String {
    let output = ebira()
        .args(args)
        .env("EBIRA_CORPUS", corpus)
        .output()
        .expect("ebira runs");
    assert!(
        output.status.success(),
        "ebira {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("ebira writes UTF-8")
}

fn top_level_keys(json: &str) -> Vec<String> {
    let bytes = json.as_bytes();
    let mut keys = Vec::new();
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                let start = index + 1;
                let end = string_end(bytes, start);
                if depth == 1 && is_key(bytes, end) {
                    keys.push(json[start..end].to_string());
                }
                index = end + 1;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        index += 1;
    }
    keys
}

fn all_keys(json: &str) -> Vec<String> {
    let bytes = json.as_bytes();
    let mut keys = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            let start = index + 1;
            let end = string_end(bytes, start);
            if is_key(bytes, end) {
                keys.push(json[start..end].to_string());
            }
            index = end + 1;
            continue;
        }
        index += 1;
    }
    keys
}

fn string_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return index,
            _ => index += 1,
        }
    }
    bytes.len()
}

fn is_key(bytes: &[u8], end: usize) -> bool {
    let mut index = end + 1;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    bytes.get(index) == Some(&b':')
}

fn assert_surface(label: &str, json: &str, expected: &[&str]) {
    let mut found = top_level_keys(json);
    found.sort();
    found.dedup();
    let mut want = expected
        .iter()
        .map(|key| key.to_string())
        .collect::<Vec<_>>();
    want.sort();
    let added = found
        .iter()
        .filter(|key| !want.contains(key))
        .collect::<Vec<_>>();
    let removed = want
        .iter()
        .filter(|key| !found.contains(key))
        .collect::<Vec<_>>();
    assert!(
        added.is_empty(),
        "{label} emitted unexpected fields: {added:?}"
    );
    assert!(
        removed.is_empty(),
        "{label} no longer emits: {removed:?}. Readers depend on these."
    );
}

fn assert_no_prose(label: &str, json: &str) {
    let found = all_keys(json)
        .into_iter()
        .filter(|key| PROSE_NAMES.contains(&key.as_str()))
        .collect::<Vec<_>>();
    assert!(found.is_empty(), "{label} emitted prose fields: {found:?}");
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("ebira-surface-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create temp directory");
        std::fs::write(
            root.join("log.jsonl"),
            concat!(
                r#"{"type":"turn_started","session_id":"s1","turn_id":"t1","timestamp":"2026-08-16T00:00:00Z","cwd":"workspace"}"#, "\n",
                r#"{"event_msg":{"type":"user_message","message":"surface probe with --tool-output-chars 300"},"role":"user","session_id":"s1","turn_id":"t1"}"#, "\n",
                r#"{"response_item":{"type":"function_call","call_id":"c1","name":"echo","arguments":{"command":"echo hi"}},"turn_id":"t1"}"#, "\n",
                r#"{"response_item":{"type":"function_call_output","call_id":"c1","stdout":"hi"},"turn_id":"t1"}"#, "\n",
                r#"{"response_item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answered"}]},"turn_id":"t1"}"#, "\n",
            ),
        )
        .expect("write source");
        let source = root.join("log.jsonl");
        let corpus = root.join("corpus");
        run(&[
            "sync",
            "--source",
            source.to_str().expect("utf-8 path"),
            "--corpus",
            corpus.to_str().expect("utf-8 path"),
            "--tool-output-chars",
            "300",
        ]);
        Self { root }
    }

    fn corpus(&self) -> String {
        self.root
            .join("corpus")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.path());
    }
}

#[test]
fn command_output_matches_declared_schema() {
    let fixture = Fixture::new("keys");
    let corpus = fixture.corpus();

    let build = run(&["sync", "--corpus", &corpus]);
    assert_surface(
        "sync",
        &build,
        &[
            "disposition",
            "mode",
            "counter_scope",
            "sources_seen",
            "sources_processed",
            "sources_appended",
            "sources_reused",
            "records",
            "invalid_records",
            "partial_records",
            "timeline_runs",
            "corpus_bytes",
            "corpus_bytes_scope",
            "unreadable_source_paths",
            "changed_sources_total",
            "changed_sources_truncated",
            "changed_sources",
            "managed_imports",
            "unavailable_managed_imports",
            "detected_sources",
            "rebuild_cause",
            "corpus",
        ],
    );

    let status_keys = [
        "disposition",
        "mode",
        "corpus",
        "rules_version",
        "rules_current",
        "sources",
        "complete_sources",
        "incomplete_sources",
        "corpus_files",
        "partial_files",
        "corpus_bytes",
        "catalog_bytes",
        "timeline_present",
        "timeline_sources",
        "timeline_runs",
        "timeline_bytes",
        "registered_source_inputs",
        "managed_imports",
        "unavailable_managed_imports",
        "imports",
        "unavailable_source_paths",
        "source_availability_observed_at_ms",
        "source_availability_total",
        "source_availability_truncated",
        "source_availability",
        "source_availability_bytes",
    ];
    let status = run(&["status", "--corpus", &corpus]);
    assert_surface("status", &status, &status_keys);

    let resume = run(&["resume", "--corpus", &corpus]);
    assert_surface(
        "resume",
        &resume,
        &[
            "disposition",
            "mode",
            "source_id",
            "source_path",
            "source_disposition",
            "completion_state",
            "current_turn_state",
            "source_boundary_state",
            "observed_boundary",
            "next_recall",
            "checkpoint_valid",
            "committed_byte_end",
            "source_size",
            "session_id",
            "latest_turn_id",
            "matched_events",
            "current",
            "latest_user_message",
            "latest_human_message",
            "compactions",
            "latest_event",
            "previous_turn",
            "next_cursor",
        ],
    );

    let source = fixture
        .path()
        .join("log.jsonl")
        .to_str()
        .expect("utf-8 path")
        .to_string();
    let imported = run(&[
        "import",
        "--source",
        &source,
        "--provenance",
        "surface-test",
        "--label",
        "surface",
        "--corpus",
        &corpus,
    ]);
    assert_surface(
        "import",
        &imported,
        &[
            "disposition",
            "mode",
            "import_id",
            "provenance",
            "label",
            "source_computer",
            "original_path",
            "imported_at_ms",
            "store",
            "registry",
            "managed_source",
            "file_manifest",
            "files_copied",
            "bytes_copied",
            "copy_disposition",
            "projection_state",
            "next_actions",
        ],
    );
    let with_import = run(&["status", "--corpus", &corpus]);
    assert_surface("status with an import", &with_import, &status_keys);
    assert!(
        with_import.contains("\"managed_imports\":1")
            && with_import.contains("\"imports\":[{\"import_id\"")
            && with_import.contains("\"label\":\"surface\""),
        "status lists the registered import: {with_import}"
    );
}

#[test]
fn removed_source_is_reported_as_unavailable() {
    let root = std::env::temp_dir().join(format!(
        "ebira-surface-missing-after-sync-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp directory");
    let present = root.join("present.jsonl");
    let missing = root.join("missing.jsonl");
    let record = concat!(
        r#"{"event_msg":{"type":"user_message","message":"availability probe"},"role":"user","session_id":"s","turn_id":"t"}"#,
        "\n"
    );
    std::fs::write(&present, record).expect("write present source");
    std::fs::write(&missing, record).expect("write source that will disappear");
    let corpus = root.join("corpus");
    run(&[
        "sync",
        "--source",
        present.to_str().expect("utf-8 path"),
        "--source",
        missing.to_str().expect("utf-8 path"),
        "--corpus",
        corpus.to_str().expect("utf-8 path"),
    ]);
    std::fs::remove_file(&missing).expect("remove registered source");

    let sync = run(&["sync", "--corpus", corpus.to_str().expect("utf-8 path")]);
    assert!(
        sync.contains("\"unreadable_source_paths\":1"),
        "the sync reports the path it could not reach: {sync}"
    );
    let status = run(&["status", "--corpus", corpus.to_str().expect("utf-8 path")]);
    assert!(
        status.contains("\"disposition\":\"source_paths_unavailable\"")
            && status.contains("\"unavailable_source_paths\":1")
            && status.contains("\"disposition\":\"missing\""),
        "later status keeps the placement fact without keeping stale content: {status}"
    );
    let raw = run(&[
        "search",
        "--corpus",
        corpus.to_str().expect("utf-8 path"),
        "--query",
        "not-in-either-source",
        "--raw",
    ]);
    assert!(
        raw.contains("\"unavailable_source_paths\":1")
            && raw.contains("\"absence_settled_through\":null"),
        "raw search settled absence with an unavailable source: {raw}"
    );
    std::fs::remove_dir_all(root).expect("remove temp directory");
}

#[test]
fn non_corpus_directory_returns_an_error() {
    let root =
        std::env::temp_dir().join(format!("ebira-surface-not-a-corpus-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create ordinary directory");
    let result = run_failure(&["status", "--corpus", root.to_str().expect("utf-8 path")]);
    assert!(
        result.contains("\"disposition\":\"error\"")
            && result.contains("no current corpus catalog"),
        "non-corpus directory was accepted: {result}"
    );
    std::fs::remove_dir_all(root).expect("remove temp directory");
}

#[test]
fn commits_rejects_a_corrupt_segment() {
    let fixture = Fixture::new("corrupt-commits-segment");
    let corpus = fixture.corpus();
    let repo = fixture.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create temporary repository");
    let git = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(&repo)
        .status()
        .expect("git init runs");
    assert!(git.success(), "git init failed");
    let segment = std::fs::read_dir(fixture.path().join("corpus").join("corpus"))
        .expect("read corpus segments")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("corpus"))
        .expect("corpus segment");
    std::fs::write(&segment, b"not-an-ebira-event-header\n").expect("corrupt disposable segment");

    let result = run_failure(&[
        "commits",
        "--corpus",
        &corpus,
        "--repo",
        repo.to_str().expect("utf-8 path"),
    ]);
    assert!(
        result.contains("\"disposition\":\"error\"")
            && result.contains("rebuild it with `ebira sync --rebuild`"),
        "commits accepted a corrupt segment: {result}"
    );
}

#[test]
fn sync_and_timeline_report_value_scopes() {
    let fixture = Fixture::new("self-reporting-scopes");
    let corpus = fixture.corpus();
    let sync = run(&["sync", "--corpus", &corpus]);
    assert!(
        sync.contains("\"counter_scope\":\"this_command\"")
            && sync.contains("\"corpus_bytes_scope\":\"current_projection\""),
        "sync counters name the command whose work they count: {sync}"
    );
    let timeline = run(&["timeline", "--corpus", &corpus, "--limit", "5"]);
    assert!(
        timeline.contains("\"snippet\":\"\"")
            && timeline.contains("\"snippet_state\":\"not_loaded_in_map\""),
        "timeline did not identify an unloaded snippet: {timeline}"
    );
}

#[test]
fn resume_candidates_report_order_and_coverage() {
    let root = std::env::temp_dir().join(format!(
        "ebira-surface-resume-candidates-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp directory");
    let older = root.join("older.jsonl");
    let newer = root.join("newer.jsonl");
    std::fs::write(
        &older,
        concat!(
            r#"{"event_msg":{"type":"user_message","message":"older"},"role":"user","session_id":"old","turn_id":"t","timestamp":"2026-08-20T00:00:00Z"}"#,
            "\n"
        ),
    )
    .expect("write older source");
    std::fs::write(
        &newer,
        concat!(
            r#"{"event_msg":{"type":"user_message","message":"newer"},"role":"user","session_id":"new","turn_id":"t","timestamp":"2026-08-21T00:00:00Z"}"#,
            "\n"
        ),
    )
    .expect("write newer source");
    let corpus = root.join("corpus");
    run(&[
        "sync",
        "--source",
        older.to_str().expect("utf-8 path"),
        "--source",
        newer.to_str().expect("utf-8 path"),
        "--corpus",
        corpus.to_str().expect("utf-8 path"),
    ]);
    let candidates = run(&["resume", "--corpus", corpus.to_str().expect("utf-8 path")]);
    assert_surface(
        "ambiguous resume",
        &candidates,
        &[
            "disposition",
            "mode",
            "candidate_order",
            "candidate_count",
            "candidates_returned",
            "candidates_truncated",
            "candidates",
        ],
    );
    let newer_position = candidates.find("newer.jsonl").expect("newer candidate");
    let older_position = candidates.find("older.jsonl").expect("older candidate");
    assert!(
        candidates.contains("\"candidate_order\":\"last_event_at_desc_then_modified_ms_desc\"")
            && candidates.contains("\"candidates_returned\":2")
            && candidates.contains("\"candidates_truncated\":false")
            && candidates.contains("\"last_event_at\":\"2026-08-21T00:00:00Z\"")
            && newer_position < older_position,
        "the latest observed event chooses the first candidate in one read: {candidates}"
    );
    std::fs::remove_dir_all(root).expect("remove temp directory");
}

#[test]
fn cli_uses_default_corpus_and_lists_valid_options() {
    let fixture = Fixture::new("default-corpus-and-help");
    let corpus = fixture.corpus();
    let status = run_with_default_corpus(&["status"], &corpus);
    assert!(
        status.contains("\"mode\":\"compact_literal_corpus\"")
            && status.contains(&format!("\"corpus\":{}", json_literal(&corpus))),
        "status resolves EBIRA_CORPUS when --corpus is omitted: {status}"
    );
    let sync = run_with_default_corpus(&["sync"], &corpus);
    assert!(
        sync.contains("\"counter_scope\":\"this_command\""),
        "sync resolves the same default when --corpus is omitted: {sync}"
    );

    let resume_help = run(&["resume", "--help"]);
    assert!(
        resume_help.contains("ebira resume")
            && !resume_help.contains("--limit")
            && !resume_help.contains("--raw")
            && !resume_help.contains("--ignore-case"),
        "resume help lists only options resume accepts: {resume_help}"
    );
    let search_help = run(&["search", "--help"]);
    assert!(
        search_help.contains("--limit")
            && search_help.contains("--raw")
            && search_help.contains("--ignore-case"),
        "search help retains its valid shared options: {search_help}"
    );
    assert_eq!(
        run(&["help", "search"]),
        search_help,
        "`ebira help <command>` and `<command> --help` agree"
    );
    let version = run(&["--version"]);
    assert_eq!(
        version.trim(),
        format!("ebira {}", env!("CARGO_PKG_VERSION"))
    );
    let overview = run(&[]);
    for command in [
        "sync", "status", "said", "resume", "search", "history", "timeline", "context", "follow",
        "commits", "import",
    ] {
        assert!(
            overview.contains(&format!("\n  {command} ")),
            "the overview lists {command}: {overview}"
        );
    }
    let unknown = run_failure(&["corpus", "--source", "x"]);
    assert!(
        unknown.contains("unknown command: corpus; run `ebira help`"),
        "{unknown}"
    );
    let wrong_option = run_failure(&["search", "--query", "x", "--output", "y"]);
    assert!(
        wrong_option.contains("unknown option for search: --output; run `ebira help search`"),
        "{wrong_option}"
    );
}

fn json_literal(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            character => output.push(character),
        }
    }
    output.push('"');
    output
}

#[test]
fn results_exclude_generated_prose() {
    let fixture = Fixture::new("prose");
    let corpus = fixture.corpus();

    assert_no_prose("status", &run(&["status", "--corpus", &corpus]));
    assert_no_prose("resume", &run(&["resume", "--corpus", &corpus]));
    assert_no_prose(
        "timeline",
        &run(&["timeline", "--corpus", &corpus, "--limit", "5"]),
    );
    assert_no_prose(
        "history",
        &run(&["history", "--corpus", &corpus, "--query", "probe"]),
    );
    assert_no_prose(
        "search",
        &run(&["search", "--corpus", &corpus, "--query", "probe"]),
    );
    assert_no_prose(
        "search (no match)",
        &run(&["search", "--corpus", &corpus, "--query", "absent-zzz"]),
    );
    assert_no_prose(
        "history (no match)",
        &run(&["history", "--corpus", &corpus, "--query", "absent-zzz"]),
    );
    let source = fixture
        .path()
        .join("log.jsonl")
        .to_str()
        .expect("utf-8 path")
        .to_string();
    assert_no_prose(
        "import",
        &run(&[
            "import",
            "--source",
            &source,
            "--provenance",
            "surface-test",
            "--label",
            "surface",
            "--corpus",
            &corpus,
        ]),
    );
    assert_no_prose(
        "status with an import",
        &run(&["status", "--corpus", &corpus]),
    );
}

#[test]
fn option_like_literal_is_searchable() {
    let fixture = Fixture::new("attached");
    let corpus = fixture.corpus();
    let found = run(&["search", "--corpus", &corpus, "--query=--tool-output-chars"]);
    assert!(
        found.contains("\"query\":\"--tool-output-chars\"") && found.contains("surface probe"),
        "the query is the literal, not the option: {found}"
    );
    let plain = run(&["search", "--corpus", &corpus, "--query", "probe"]);
    assert!(plain.contains("\"query\":\"probe\""), "{plain}");
}

#[test]
fn next_action_contains_only_request_fields() {
    let fixture = Fixture::new("actions");
    let corpus = fixture.corpus();
    let miss = run(&["search", "--corpus", &corpus, "--query", "absent-zzz"]);
    assert!(
        miss.contains("\"scan_source_records\""),
        "a projection miss routes to the source: {miss}"
    );
    for key in ["\"reason\"", "\"advice\"", "\"guidance\""] {
        assert!(
            !miss.contains(key),
            "next action contains a forbidden field {key}: {miss}"
        );
    }
}

#[test]
fn first_sync_reads_the_default_transcript_directories() {
    let root = std::env::temp_dir().join(format!(
        "ebira-surface-default-sources-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let claude = home.join(".claude").join("projects").join("-work");
    let codex = home
        .join(".codex")
        .join("sessions")
        .join("2026")
        .join("08")
        .join("16");
    std::fs::create_dir_all(&claude).expect("create Claude Code transcript directory");
    std::fs::create_dir_all(&codex).expect("create Codex session directory");
    std::fs::write(
        claude.join("s-claude.jsonl"),
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"default claude marker"},"sessionId":"s-claude","uuid":"u1","timestamp":"2026-08-16T00:00:00Z","cwd":"/work"}"#,
            "\n"
        ),
    )
    .expect("write Claude Code transcript");
    std::fs::write(
        codex.join("rollout-2026-08-16T00-00-00-s-codex.jsonl"),
        concat!(
            r#"{"timestamp":"2026-08-16T00:00:01Z","type":"session_meta","payload":{"id":"s-codex","cwd":"/work"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-16T00:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"default codex marker"}]}}"#,
            "\n"
        ),
    )
    .expect("write Codex rollout");
    let corpus = root.join("corpus");
    let corpus = corpus.to_str().expect("utf-8 path");
    let run_home = |args: &[&str]| {
        let output = ebira_with_home(&home)
            .args(args)
            .output()
            .expect("ebira runs");
        assert!(
            output.status.success(),
            "ebira {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8(output.stdout).expect("ebira writes UTF-8")
    };

    let first = run_home(&["sync", "--corpus", corpus]);
    assert!(
        first.contains("\"sources_seen\":2")
            && first.contains(".claude")
            && first.contains(".codex")
            && !first.contains("\"detected_sources\":[]"),
        "the first sync finds both agents' transcripts: {first}"
    );
    let again = run_home(&["sync", "--corpus", corpus]);
    assert!(
        again.contains("\"detected_sources\":[]") && again.contains("\"sources_seen\":2"),
        "later syncs use the registered sources: {again}"
    );
    let said = run_home(&["said", "--corpus", corpus]);
    assert!(
        said.contains("default claude marker")
            && said.contains("default codex marker")
            && said.contains("\"total_messages\":2"),
        "both transcripts were read as the person's messages: {said}"
    );
    std::fs::remove_dir_all(root).expect("remove temp directory");
}

#[test]
fn copied_transcripts_are_recognised_by_their_records() {
    let root = std::env::temp_dir().join(format!(
        "ebira-surface-copied-transcripts-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let copies = root.join("copies-from-another-computer");
    std::fs::create_dir_all(&copies).expect("create copy directory");
    std::fs::write(
        copies.join("claude-session.jsonl"),
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"<task-notification>done</task-notification>"},"sessionId":"c1","uuid":"u1","timestamp":"2026-08-16T00:00:00Z"}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"copied claude words"},"sessionId":"c1","uuid":"u2","timestamp":"2026-08-16T00:00:01Z"}"#,
            "\n"
        ),
    )
    .expect("write copied Claude Code transcript");
    let corpus = root.join("corpus");
    run(&[
        "sync",
        "--source",
        copies.to_str().expect("utf-8 path"),
        "--corpus",
        corpus.to_str().expect("utf-8 path"),
    ]);
    let said = run(&["said", "--corpus", corpus.to_str().expect("utf-8 path")]);
    assert!(
        said.contains("copied claude words")
            && !said.contains("task-notification")
            && said.contains("\"source_id\":\"claude-"),
        "a transcript outside .claude is still read as Claude Code: {said}"
    );
    std::fs::remove_dir_all(root).expect("remove temp directory");
}

fn rewrite_catalog_header(corpus: &str, edit: impl Fn(&str) -> String) {
    let catalog = Path::new(corpus).join("sources.tsv");
    let text = std::fs::read_to_string(&catalog).expect("read catalog");
    let (header, rest) = text.split_once('\n').expect("catalog header");
    std::fs::write(&catalog, format!("{}\n{}", edit(header), rest)).expect("write catalog");
}

#[test]
fn sync_rebuilds_a_corpus_made_under_other_reading_rules() {
    let fixture = Fixture::new("rules-version");
    let corpus = fixture.corpus();
    let current = run(&["status", "--corpus", &corpus]);
    assert!(current.contains("\"rules_current\":true"), "{current}");

    // A catalog written before the reading rules were versioned declares none.
    rewrite_catalog_header(&corpus, |header| {
        header
            .split('\t')
            .filter(|part| !part.starts_with("rules="))
            .collect::<Vec<_>>()
            .join("\t")
    });
    let stale = run(&["status", "--corpus", &corpus]);
    assert!(
        stale.contains("\"rules_version\":0,\"rules_current\":false"),
        "{stale}"
    );
    let rebuilt = run(&["sync", "--corpus", &corpus]);
    assert!(
        rebuilt.contains("\"rebuild_cause\":\"rules_changed\"")
            && rebuilt.contains("\"sources_processed\":1")
            && rebuilt.contains("\"sources_reused\":0"),
        "a corpus made under other rules is rebuilt from the logs: {rebuilt}"
    );
    let again = run(&["sync", "--corpus", &corpus]);
    assert!(
        again.contains("\"rebuild_cause\":null") && again.contains("\"sources_reused\":1"),
        "a current corpus is synced incrementally: {again}"
    );
    let requested = run(&["sync", "--corpus", &corpus, "--rebuild"]);
    assert!(
        requested.contains("\"rebuild_cause\":\"requested\"")
            && requested.contains("\"sources_processed\":1"),
        "{requested}"
    );
}

#[test]
fn sync_rebuilds_a_corpus_written_in_another_format() {
    let fixture = Fixture::new("format-version");
    let corpus = fixture.corpus();
    rewrite_catalog_header(&corpus, |header| {
        header
            .split('\t')
            .map(|part| {
                if part.starts_with("v=") {
                    "v=0".to_string()
                } else {
                    part.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\t")
    });
    let refused = run_failure(&["search", "--corpus", &corpus, "--query", "probe"]);
    assert!(
        refused.contains("run `ebira sync`, which rebuilds it"),
        "{refused}"
    );
    let rebuilt = run(&["sync", "--corpus", &corpus]);
    assert!(
        rebuilt.contains("\"rebuild_cause\":\"format_changed\""),
        "{rebuilt}"
    );
    let found = run(&["search", "--corpus", &corpus, "--query", "surface probe"]);
    assert!(found.contains("\"total_candidates\":1"), "{found}");
}

#[test]
fn concurrent_syncs_and_reads_leave_one_consistent_corpus() {
    let fixture = Fixture::new("concurrent");
    let corpus = fixture.corpus();
    let logs = fixture.path().join("more");
    std::fs::create_dir_all(&logs).expect("create log directory");
    let record = |index: usize, turn: &str| {
        format!(
            "{{\"event_msg\":{{\"type\":\"user_message\",\"message\":\"concurrent marker {index} {turn}\"}},\"role\":\"user\",\"session_id\":\"s{index}\",\"turn_id\":\"{turn}\"}}\n"
        )
    };
    for index in 0..40 {
        std::fs::write(logs.join(format!("log-{index}.jsonl")), record(index, "t1"))
            .expect("write source");
    }
    run(&[
        "sync",
        "--corpus",
        &corpus,
        "--source",
        logs.to_str().expect("utf-8 path"),
    ]);
    for index in 0..40 {
        let path = logs.join(format!("log-{index}.jsonl"));
        let mut text = std::fs::read_to_string(&path).expect("read source");
        text.push_str(&record(index, "t2"));
        std::fs::write(&path, text).expect("append to source");
    }

    let children = (0..8)
        .map(|index| {
            let args: Vec<&str> = if index % 2 == 0 {
                vec!["sync", "--corpus", &corpus]
            } else {
                vec![
                    "search",
                    "--corpus",
                    &corpus,
                    "--query",
                    "concurrent marker",
                ]
            };
            ebira()
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start ebira")
        })
        .collect::<Vec<_>>();
    for child in children {
        let output = child.wait_with_output().expect("ebira finishes");
        assert!(
            output.status.success(),
            "a command run beside others failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let status = run(&["status", "--corpus", &corpus]);
    assert!(
        status.contains("\"incomplete_sources\":0") && status.contains("\"partial_files\":0"),
        "{status}"
    );
    let found = run(&[
        "search",
        "--corpus",
        &corpus,
        "--query",
        "concurrent marker",
        "--limit",
        "200",
    ]);
    assert!(found.contains("\"total_candidates\":80"), "{found}");
}

/// A directory that cannot be listed this time is reported, and what was read from it before
/// stays searchable; only a source that is gone loses its projection.
#[cfg(unix)]
#[test]
fn an_unreadable_directory_keeps_its_projection() {
    use std::os::unix::fs::PermissionsExt;
    let root =
        std::env::temp_dir().join(format!("ebira-surface-unreadable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let logs = root.join("logs");
    for name in ["open", "closed"] {
        std::fs::create_dir_all(logs.join(name)).expect("create log directory");
        std::fs::write(
            logs.join(name).join(format!("{name}.jsonl")),
            format!(
                "{{\"event_msg\":{{\"type\":\"user_message\",\"message\":\"{name} marker\"}},\"role\":\"user\",\"session_id\":\"{name}\",\"turn_id\":\"t\"}}\n"
            ),
        )
        .expect("write source");
    }
    let corpus = root.join("corpus");
    let corpus = corpus.to_str().expect("utf-8 path");
    run(&[
        "sync",
        "--source",
        logs.to_str().expect("utf-8 path"),
        "--corpus",
        corpus,
    ]);
    let closed = logs.join("closed");
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000))
        .expect("close the directory");
    if std::fs::read_dir(&closed).is_ok() {
        // Running with privileges that ignore permissions; nothing to observe.
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755))
            .expect("open the directory");
        std::fs::remove_dir_all(&root).expect("remove temp directory");
        return;
    }
    let sync = run(&["sync", "--corpus", corpus]);
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755))
        .expect("open the directory");
    assert!(sync.contains("\"unreadable_source_paths\":1"), "{sync}");
    let found = run(&["search", "--corpus", corpus, "--query", "closed marker"]);
    assert!(
        found.contains("\"total_candidates\":1"),
        "the projection of an unreadable directory is kept: {found}"
    );
    std::fs::remove_dir_all(&root).expect("remove temp directory");
}
