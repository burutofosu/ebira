//! Several `--query` in one call: one pass over the corpus, and for each query the result a
//! call for it alone gives.

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
        .env_remove("EBIRA_CORPUS")
        .env_remove("EBIRA_TZ_OFFSET");
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

/// The JSON text of the value under `key`, at any depth: the first such key in the text.
fn value_of<'a>(json: &'a str, key: &str) -> &'a str {
    let marker = format!("\"{key}\":");
    let start = json
        .find(&marker)
        .unwrap_or_else(|| panic!("no {key} in {json}"))
        + marker.len();
    &json[start..start + value_len(&json[start..])]
}

/// The length of the JSON value at the start of `text`.
fn value_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            match byte {
                b'\\' => index += 1,
                b'"' => {
                    in_string = false;
                    if depth == 0 {
                        return index + 1;
                    }
                }
                _ => {}
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return index + 1;
                    }
                }
                b',' if depth == 0 => return index,
                _ => {}
            }
        }
        index += 1;
    }
    bytes.len()
}

/// The elements of a JSON array, as text.
fn elements(array: &str) -> Vec<&str> {
    let inner = &array[1..array.len() - 1];
    let mut items = Vec::new();
    let mut rest = inner.trim_start();
    while !rest.is_empty() {
        let length = value_len(rest);
        items.push(&rest[..length]);
        rest = rest[length..].trim_start();
        rest = rest.strip_prefix(',').unwrap_or(rest).trim_start();
    }
    items
}

struct Logs {
    root: PathBuf,
}

impl Logs {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("ebira-several-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join(".claude").join("projects").join("C--work-game");
        let subagents = project.join("s-main").join("subagents");
        std::fs::create_dir_all(&subagents).expect("create claude directories");
        std::fs::write(
            project.join("s-main.jsonl"),
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"please build the Map with old colors"},"sessionId":"s-main","timestamp":"2026-09-01T00:00:00Z","cwd":"C:/work/game"}"#, "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"building the map"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls maps"}}]},"sessionId":"s-main","timestamp":"2026-09-01T00:00:01Z","cwd":"C:/work/game"}"#, "\n",
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"map.txt colors.txt"}]},"sessionId":"s-main","timestamp":"2026-09-01T00:00:02Z","cwd":"C:/work/game"}"#, "\n",
                r#"{"type":"user","message":{"role":"user","content":"keep the old colors"},"sessionId":"s-main","timestamp":"2026-09-01T00:00:03Z","cwd":"C:/work/game"}"#, "\n",
                r#"{"type":"user","message":{"role":"user","content":"keep the old colors"},"sessionId":"s-main","timestamp":"2026-09-01T00:00:03Z","cwd":"C:/work/game"}"#, "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"kept the colors on the map"}]},"sessionId":"s-main","timestamp":"2026-09-01T00:00:04Z","cwd":"C:/work/game"}"#, "\n",
                r#"{"type":"user","message":{"role":"user","content":"now the map legend"},"sessionId":"s-main","timestamp":"2026-09-01T00:00:05Z","cwd":"C:/work/game"}"#, "\n",
            ),
        )
        .expect("write main session");
        std::fs::write(
            subagents.join("agent-a1.jsonl"),
            concat!(
                r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"Survey the map code"},"sessionId":"s-main","timestamp":"2026-09-01T00:00:06Z","cwd":"C:/work/tools"}"#, "\n",
                r#"{"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"the map code has colors in one table"}]},"sessionId":"s-main","timestamp":"2026-09-01T00:00:07Z","cwd":"C:/work/tools"}"#, "\n",
            ),
        )
        .expect("write subagent transcript");
        let logs = Logs { root };
        run(&[
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
}

impl Drop for Logs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const QUERIES: [&str; 3] = ["map", "colors", "no such words"];

#[test]
fn search_gives_each_query_what_it_gets_alone() {
    let logs = Logs::new("search");
    let corpus = logs.corpus();
    for filters in [
        vec!["--ignore-case"],
        vec!["--sender", "assistant", "--order", "desc"],
        vec!["--limit", "1"],
    ] {
        let mut args: Vec<&str> = vec!["search", "--corpus", &corpus];
        args.extend(&filters);
        for query in QUERIES {
            args.extend(["--query", query]);
        }
        let many = run(&args);
        assert_eq!(
            value_of(&many, "queries"),
            r#"["map","colors","no such words"]"#
        );
        let results = elements(value_of(&many, "results"));
        assert_eq!(results.len(), QUERIES.len(), "{many}");
        for (query, result) in QUERIES.into_iter().zip(&results) {
            let mut single_args: Vec<&str> = vec!["search", "--corpus", &corpus, "--query", query];
            single_args.extend(&filters);
            let single = run(&single_args);
            assert_eq!(value_of(result, "query"), format!("\"{query}\""));
            for key in [
                "total_candidates",
                "returned",
                "truncated",
                "next_offset",
                "facets",
                "matches",
                "next_actions",
            ] {
                assert_eq!(
                    value_of(result, key),
                    value_of(&single, key),
                    "{key} of {query:?} with {filters:?}"
                );
            }
        }
        assert_eq!(
            value_of(&many, "scanned_files"),
            value_of(
                &run(&["search", "--corpus", &corpus, "--query", "map"]),
                "scanned_files"
            ),
            "the queries share one pass"
        );
    }
    let missing = run(&[
        "search",
        "--corpus",
        &corpus,
        "--query",
        "no such words",
        "--query",
        "nor these",
    ]);
    assert_eq!(
        value_of(&missing, "disposition"),
        "\"no_match_in_projection\""
    );
}

#[test]
fn said_gives_each_query_what_it_gets_alone() {
    let logs = Logs::new("said");
    let corpus = logs.corpus();
    for filters in [
        vec!["--ignore-case"],
        vec!["--order", "asc", "--limit", "1"],
    ] {
        let mut args: Vec<&str> = vec!["said", "--corpus", &corpus];
        args.extend(&filters);
        for query in QUERIES {
            args.extend(["--query", query]);
        }
        let many = run(&args);
        let results = elements(value_of(&many, "results"));
        assert_eq!(results.len(), QUERIES.len(), "{many}");
        for (query, result) in QUERIES.into_iter().zip(&results) {
            let mut single_args: Vec<&str> = vec!["said", "--corpus", &corpus, "--query", query];
            single_args.extend(&filters);
            let single = run(&single_args);
            for key in [
                "total_messages",
                "copies_skipped",
                "imported_skipped",
                "returned",
                "truncated",
                "next_offset",
                "messages",
            ] {
                assert_eq!(
                    value_of(result, key),
                    value_of(&single, key),
                    "{key} of {query:?} with {filters:?}"
                );
            }
        }
    }
    let many = run(&[
        "said",
        "--corpus",
        &corpus,
        "--query",
        "colors",
        "--query",
        "map",
        "--ignore-case",
    ]);
    let results = elements(value_of(&many, "results"));
    assert_eq!(value_of(results[0], "total_messages"), "2", "{many}");
    assert_eq!(
        value_of(results[0], "copies_skipped"),
        "1",
        "a message stored twice is listed once: {many}"
    );
    assert_eq!(value_of(results[1], "total_messages"), "2", "{many}");

    let text = run(&[
        "said", "--corpus", &corpus, "--query", "colors", "--query", "legend", "--format", "text",
    ]);
    assert!(
        text.starts_with("# ebira said: 2 queries")
            && text.contains("## \"colors\": 2 of 2 messages")
            && text.contains("## \"legend\": 1 of 1 messages")
            && text.contains("now the map legend"),
        "{text}"
    );
}

#[test]
fn history_gives_each_query_its_dates_and_executable_next_pages() {
    let logs = Logs::new("history");
    let corpus = logs.corpus();
    for filters in [
        vec!["--ignore-case"],
        vec!["--sender", "assistant", "--order", "desc", "--limit", "1"],
        vec!["--date-limit", "1", "--date-offset", "1"],
    ] {
        let mut args = vec!["history", "--corpus", &corpus];
        args.extend(&filters);
        for query in QUERIES {
            args.extend(["--query", query]);
        }
        let many = run(&args);
        assert_eq!(value_of(&many, "operation"), "\"history\"");
        let results = elements(value_of(&many, "results"));
        assert_eq!(results.len(), QUERIES.len());
        for (query, result) in QUERIES.into_iter().zip(results) {
            let mut single_args = vec!["history", "--corpus", &corpus, "--query", query];
            single_args.extend(&filters);
            let single = run(&single_args);
            for key in [
                "disposition",
                "total_candidates",
                "map",
                "matches",
                "facets",
                "truncated",
                "next_offset",
                "next_actions",
            ] {
                assert_eq!(
                    value_of(result, key),
                    value_of(&single, key),
                    "{key}: {query}"
                );
            }
            assert_eq!(
                value_of(&many, "scanned_bytes"),
                value_of(&single, "scanned_bytes"),
                "one shared scan"
            );
        }
    }
}

#[test]
fn raw_search_and_history_take_one_query() {
    let logs = Logs::new("one");
    let corpus = logs.corpus();
    let history = run_failure(&[
        "history", "--corpus", &corpus, "--query", "map", "--query", "colors", "--raw",
    ]);
    assert!(history.contains("--raw takes one --query"), "{history}");
    let raw = run_failure(&[
        "search", "--corpus", &corpus, "--query", "map", "--query", "colors", "--raw",
    ]);
    assert!(raw.contains("--raw takes one --query"), "{raw}");
}

#[test]
fn search_narrows_to_a_working_directory() {
    let logs = Logs::new("cwd");
    let corpus = logs.corpus();
    let everywhere = run(&[
        "search",
        "--corpus",
        &corpus,
        "--query",
        "colors",
        "--sender",
        "assistant",
    ]);
    assert_eq!(
        value_of(&everywhere, "total_candidates"),
        "2",
        "{everywhere}"
    );
    let game = run(&[
        "search",
        "--corpus",
        &corpus,
        "--query",
        "colors",
        "--sender",
        "assistant",
        "--cwd",
        "WORK/GAME",
    ]);
    assert_eq!(value_of(&game, "total_candidates"), "1", "{game}");
    assert!(game.contains("kept the colors on the map"), "{game}");
    assert_eq!(
        value_of(value_of(&game, "applied_filters"), "cwd"),
        "\"WORK/GAME\""
    );
}
