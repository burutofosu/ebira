//! The demonstration in the README, run against the transcript in `examples/`.

use std::path::Path;
use std::process::Command;

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

/// Runs ebira with a home that holds no transcripts and with UTC dates, as the README shows.
fn run(args: &[&str]) -> String {
    let home = std::env::temp_dir().join("ebira-test-empty-home");
    let output = Command::new(EBIRA)
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("EBIRA_CORPUS")
        .env_remove("EBIRA_TZ_OFFSET")
        .output()
        .expect("ebira runs");
    assert!(
        output.status.success(),
        "ebira {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stdout).expect("ebira writes UTF-8")
}

#[test]
fn the_readme_demonstration_holds() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("claude-session.jsonl");
    let root = std::env::temp_dir().join(format!("ebira-example-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let corpus = root.join("demo-corpus");
    let corpus = corpus.to_str().expect("utf-8 path");
    run(&[
        "sync",
        "--corpus",
        corpus,
        "--source",
        example.to_str().expect("utf-8 path"),
    ]);

    let said = run(&["said", "--corpus", corpus, "--format", "text"]);
    assert!(
        said.starts_with("# ebira said: 3 of 3 messages from the person, newest first"),
        "{said}"
    );
    let expected = [
        (
            "[2026-09-01 09:32] claude typed session=9f1c2a7e",
            "byte=1854+214",
            "Now make the retry limit configurable.",
        ),
        (
            "[2026-09-01 09:00] claude queued session=9f1c2a7e",
            "byte=872+201",
            "Log each retry with its delay.",
        ),
        (
            "[2026-09-01 09:00] claude typed session=9f1c2a7e",
            "byte=0+279",
            "Add retries to the upload client. Keep the total wait under 30 seconds, and never retry a 4xx response.",
        ),
    ];
    let mut position = 0;
    for (header, range, text) in expected {
        let found = said[position..]
            .find(header)
            .map(|at| position + at)
            .unwrap_or_else(|| panic!("said misses {header:?}: {said}"));
        let line_end = said[found..]
            .find('\n')
            .map(|at| found + at)
            .expect("the header line ends");
        assert!(
            said[found..line_end].ends_with(range),
            "{header:?} has another byte range: {said}"
        );
        assert!(
            said[line_end + 1..].starts_with(text),
            "{header:?} is not followed by its text: {said}"
        );
        position = line_end;
    }
    for text in [
        "Summary:",
        "task-notification",
        "Background tests passed",
        "edited src/upload.rs",
    ] {
        assert!(!said.contains(text), "said lists {text:?}: {said}");
    }

    let found = run(&[
        "search", "--corpus", corpus, "--query", "4xx", "--sender", "human",
    ]);
    assert!(
        found.contains("\"byte_start\":0,\"byte_len\":279") && found.contains("\"via\":\"typed\""),
        "{found}"
    );
    let source_id = found
        .split("\"source_id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the match names its source");
    let original = run(&[
        "context",
        "--corpus",
        corpus,
        "--source-id",
        source_id,
        "--byte-start",
        "0",
        "--byte-len",
        "279",
    ]);
    assert!(
        original.contains("\"disposition\":\"ready\"")
            && original.contains("never retry a 4xx response"),
        "{original}"
    );
    std::fs::remove_dir_all(root).expect("remove temp directory");
}
