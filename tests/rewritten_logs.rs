//! Logs are appended to. One rewritten in place where the corpus read it, then appended to, is
//! not taken for appended: `context` does not return the other record as the one a reference
//! names, and a sync reads the log again whole when the records it read last are gone.

use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::Command;

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

const SESSION: &str = "8a1d3c5e-2b4f-4d6a-9c8e-1f3b5d7a9c2e";

fn record(uuid: &str, time: &str, text: &str) -> String {
    format!(
        r#"{{"type":"user","sessionId":"{SESSION}","uuid":"{uuid}","timestamp":"2026-09-20T{time}Z","cwd":"/work/app","message":{{"role":"user","content":"{text}"}}}}"#
    ) + "\n"
}

struct Logs {
    root: PathBuf,
    log: PathBuf,
    corpus: String,
}

impl Logs {
    /// The records as one log, synced.
    fn new(name: &str, records: &[&str]) -> Self {
        let root =
            std::env::temp_dir().join(format!("ebira-rewritten-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let folder = root.join(".claude").join("projects").join("app");
        std::fs::create_dir_all(&folder).expect("create project folder");
        let log = folder.join(format!("{SESSION}.jsonl"));
        std::fs::write(&log, records.concat()).expect("write transcript");
        let corpus = root
            .join("corpus")
            .to_str()
            .expect("UTF-8 path")
            .to_string();
        let logs = Self { root, log, corpus };
        let sync = logs.run(&["sync", "--source", folder.to_str().expect("UTF-8")]);
        assert!(sync.contains("\"disposition\":\"complete\""), "{sync}");
        logs
    }

    fn run(&self, args: &[&str]) -> String {
        let output = Command::new(EBIRA)
            .args(args)
            .args(["--corpus", &self.corpus])
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("EBIRA_CORPUS")
            .env_remove("EBIRA_TZ_OFFSET")
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

    /// The first match's source id, start, and length.
    fn find(&self, text: &str) -> Option<[String; 3]> {
        let found = self.run(&["search", "--query", text]);
        let at = found.find("\"source_ref\":{")?;
        let reference = &found[at..];
        let value = |key: &str| {
            let start = reference.find(&format!("\"{key}\":")).expect(key) + key.len() + 3;
            let rest = &reference[start..];
            match rest.strip_prefix('"') {
                Some(string) => string[..string.find('"').expect("end")].to_string(),
                None => rest[..rest.find([',', '}']).expect("end")].to_string(),
            }
        };
        Some([value("source_id"), value("byte_start"), value("byte_len")])
    }

    fn context(&self, reference: &[String; 3]) -> String {
        self.run(&[
            "context",
            "--source-id",
            &reference[0],
            "--byte-start",
            &reference[1],
            "--byte-len",
            &reference[2],
        ])
    }

    /// Writes `bytes` over the log at `at`, keeping the file, then appends `tail`.
    fn rewrite_in_place(&self, at: usize, bytes: &str, tail: &str) {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&self.log)
            .expect("open the log");
        file.seek(SeekFrom::Start(at as u64)).expect("seek");
        file.write_all(bytes.as_bytes()).expect("write over");
        file.seek(SeekFrom::End(0)).expect("seek to the end");
        file.write_all(tail.as_bytes()).expect("append");
    }
}

impl Drop for Logs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_record_rewritten_in_place_is_not_read_as_the_one_referenced() {
    let first = record("u1", "01:00:00", "keep the cache for a day");
    let second = record("u2", "01:05:00", "the cache key includes the locale");
    let logs = Logs::new("context", &[first.as_str(), second.as_str()]);
    let named = logs
        .find("includes the locale")
        .expect("the record is found");
    let kept = logs.find("for a day").expect("the record is found");

    // Another record, written at another time, of the same length, where the second was.
    let other = record("u2", "01:06:00", "the cache key includes the region");
    assert_eq!(other.len(), second.len());
    logs.rewrite_in_place(
        first.len(),
        &other,
        &record("u3", "01:10:00", "clear the cache on deploy"),
    );

    let read = logs.context(&named);
    assert!(
        read.contains("\"disposition\":\"source_changed_since_projection\""),
        "{read}"
    );
    assert!(read.contains("\"rebuild_source\":["), "{read}");
    assert!(!read.contains("includes the region"), "{read}");
    let unchanged = logs.context(&kept);
    assert!(
        unchanged.contains("\"disposition\":\"ready\""),
        "{unchanged}"
    );

    // The record the corpus read last is gone, so the sync reads the log again whole.
    let sync = logs.run(&["sync"]);
    assert!(sync.contains("\"sources_appended\":0"), "{sync}");
    assert!(logs.find("includes the locale").is_none());
    let current = logs.find("includes the region").expect("the new record");
    assert!(logs.context(&current).contains("\"disposition\":\"ready\""));
    assert!(logs.find("on deploy").is_some());
}

#[test]
fn a_log_only_appended_to_is_read_on_from_where_the_corpus_stopped() {
    let first = record("u1", "01:00:00", "keep the cache for a day");
    let logs = Logs::new("appended", &[first.as_str()]);
    let named = logs.find("for a day").expect("the record is found");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&logs.log)
        .expect("open the log");
    file.write_all(record("u2", "01:05:00", "clear the cache on deploy").as_bytes())
        .expect("append");
    drop(file);

    assert!(logs.context(&named).contains("\"disposition\":\"ready\""));
    let sync = logs.run(&["sync"]);
    assert!(sync.contains("\"sources_appended\":1"), "{sync}");
    assert!(logs.find("on deploy").is_some());
}
