//! Time means one thing in every command: a timestamp written without a zone is local time
//! at the corpus offset, a `--from`/`--to` date is that whole local day, and a time is that
//! instant.

use std::path::PathBuf;
use std::process::{Command, Output};

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

/// Runs ebira at +09:00 with a home that holds no transcripts.
fn ebira(args: &[&str]) -> Output {
    let home = std::env::temp_dir().join("ebira-test-empty-home");
    Command::new(EBIRA)
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("EBIRA_CORPUS")
        .env("EBIRA_TZ_OFFSET", "+09:00")
        .output()
        .expect("ebira runs")
}

fn run(args: &[&str]) -> String {
    let output = ebira(args);
    assert!(
        output.status.success(),
        "ebira {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("ebira writes UTF-8")
}

struct Corpus {
    root: PathBuf,
}

impl Corpus {
    /// Five messages around two local midnights. The one written without a zone is fourth in
    /// the file and first in time.
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("ebira-time-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("logs")).expect("create log directory");
        let message = |text: &str, uuid: &str, timestamp: &str| {
            format!(
                r#"{{"type":"user","message":{{"role":"user","content":"{text}"}},"sessionId":"s1","uuid":"{uuid}","timestamp":"{timestamp}"}}"#
            )
        };
        let lines = [
            message("probe A late on the 17th", "u1", "2026-08-17T14:59:59Z"),
            message("probe B at local midnight", "u2", "2026-08-17T15:00:00Z"),
            message("probe C in the evening", "u3", "2026-08-18T20:00:00+09:00"),
            message(
                "probe E written without a zone",
                "u4",
                "2026-08-17T23:00:00",
            ),
            message("probe D on the 19th", "u5", "2026-08-18T15:00:00Z"),
        ];
        std::fs::write(root.join("logs").join("s1.jsonl"), lines.join("\n") + "\n")
            .expect("write log");
        let corpus = Self { root };
        run(&[
            "sync",
            "--source",
            corpus.root.join("logs").to_str().expect("utf-8 path"),
            "--corpus",
            &corpus.corpus(),
        ]);
        corpus
    }

    fn corpus(&self) -> String {
        self.root
            .join("corpus")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }
}

impl Drop for Corpus {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The probe letters in the order they appear in `output`.
fn letters(output: &str) -> String {
    output
        .match_indices("probe ")
        .map(|(index, _)| &output[index + 6..index + 7])
        .collect()
}

#[test]
fn a_date_is_a_whole_local_day_in_every_command() {
    let corpus = Corpus::new("days");
    let corpus = corpus.corpus();
    let said = |from: &str, to: &str| {
        letters(&run(&[
            "said", "--corpus", &corpus, "--from", from, "--to", to, "--order", "asc",
        ]))
    };
    assert_eq!(said("2026-08-17", "2026-08-17"), "EA");
    assert_eq!(said("2026-08-18", "2026-08-18"), "BC");
    assert_eq!(said("2026-08-19", "2026-08-19"), "D");
    let search = run(&[
        "search",
        "--corpus",
        &corpus,
        "--query",
        "probe",
        "--from",
        "2026-08-18",
        "--to",
        "2026-08-18",
        "--order",
        "asc",
    ]);
    assert_eq!(letters(&search), "BC", "{search}");
    let timeline = run(&["timeline", "--corpus", &corpus, "--date", "2026-08-17"]);
    assert!(
        timeline.contains("\"total_events\":2"),
        "the record without a zone is on its local date: {timeline}"
    );
}

#[test]
fn a_time_is_an_instant_and_ordering_agrees_with_dates() {
    let corpus = Corpus::new("instants");
    let corpus = corpus.corpus();
    let all = run(&["said", "--corpus", &corpus, "--order", "asc"]);
    assert_eq!(letters(&all), "EABCD", "{all}");
    let morning = run(&[
        "said",
        "--corpus",
        &corpus,
        "--from",
        "2026-08-18T00:00:00+09:00",
        "--to",
        "2026-08-18T12:00:00",
    ]);
    assert_eq!(letters(&morning), "B", "{morning}");
}

#[test]
fn bounds_and_offsets_that_cannot_be_read_are_errors() {
    let corpus = Corpus::new("errors");
    let corpus = corpus.corpus();
    let output = ebira(&["said", "--corpus", &corpus, "--from", "yesterday"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--from"));

    let root = std::env::temp_dir().join(format!("ebira-time-offset-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp directory");
    std::fs::write(root.join("empty.jsonl"), "").expect("write log");
    let home = std::env::temp_dir().join("ebira-test-empty-home");
    let output = Command::new(EBIRA)
        .args([
            "sync",
            "--source",
            root.join("empty.jsonl").to_str().expect("utf-8 path"),
            "--corpus",
            root.join("corpus").to_str().expect("utf-8 path"),
        ])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("EBIRA_CORPUS")
        .env("EBIRA_TZ_OFFSET", "nine")
        .output()
        .expect("ebira runs");
    assert!(
        !output.status.success(),
        "an unreadable offset is not taken as UTC"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("EBIRA_TZ_OFFSET"));
    std::fs::remove_dir_all(root).expect("remove temp directory");
}
