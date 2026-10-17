//! A log an app moves to another folder keeps its source id, so the references made before
//! the move still read the same records; a different log that only shares the name does not.

use std::path::PathBuf;
use std::process::Command;

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

const SESSION: &str = "5f0c2a9e-7d1b-4c3a-9e8f-2b6d1a0c4e77";

fn record(uuid: &str, time: &str, text: &str) -> String {
    format!(
        r#"{{"type":"user","sessionId":"{SESSION}","uuid":"{uuid}","timestamp":"2026-09-20T{time}Z","cwd":"/work/app","message":{{"role":"user","content":"{text}"}}}}"#
    ) + "\n"
}

fn transcript() -> String {
    [
        record("u1", "01:00:00", "keep the cache for a day"),
        record("u2", "01:05:00", "the cache key includes the locale"),
        record("u3", "01:10:00", "clear the cache on deploy"),
    ]
    .concat()
}

/// `data` as one Zstandard frame of raw blocks.
fn compressed(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x28, 0xB5, 0x2F, 0xFD, 0xA0];
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(&(1 | (data.len() as u32) << 3).to_le_bytes()[..3]);
    out.extend_from_slice(data);
    out
}

struct Logs {
    root: PathBuf,
    projects: PathBuf,
    corpus: String,
}

impl Logs {
    /// The transcript in one project folder, synced.
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("ebira-moved-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let projects = root.join(".claude").join("projects");
        std::fs::create_dir_all(projects.join("first")).expect("create project folder");
        std::fs::write(
            projects.join("first").join(format!("{SESSION}.jsonl")),
            transcript(),
        )
        .expect("write transcript");
        let corpus = root
            .join("corpus")
            .to_str()
            .expect("UTF-8 path")
            .to_string();
        let logs = Self {
            root,
            projects,
            corpus,
        };
        let sync = logs.run(&["sync", "--source", logs.projects.to_str().expect("UTF-8")]);
        assert!(sync.contains("\"disposition\":\"complete\""), "{sync}");
        logs
    }

    fn log(&self, folder: &str, name: &str) -> PathBuf {
        let folder = self.projects.join(folder);
        std::fs::create_dir_all(&folder).expect("create project folder");
        folder.join(name)
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

    /// The first match's source id, path, start, and length.
    fn find(&self, text: &str) -> Vec<String> {
        let found = self.find_all(text);
        found.into_iter().next().expect("a match")
    }

    /// Every match's source id, path, start, and length.
    fn find_all(&self, text: &str) -> Vec<Vec<String>> {
        let found = self.run(&["search", "--query", text]);
        found
            .match_indices("\"source_ref\":{")
            .map(|(at, _)| {
                let reference = &found[at..];
                ["source_id", "source_path", "byte_start", "byte_len"]
                    .iter()
                    .map(|key| {
                        let start =
                            reference.find(&format!("\"{key}\":")).expect(key) + key.len() + 3;
                        let rest = &reference[start..];
                        match rest.strip_prefix('"') {
                            Some(string) => string[..string.find('"').expect("end")].to_string(),
                            None => rest[..rest.find([',', '}']).expect("end")].to_string(),
                        }
                    })
                    .collect()
            })
            .collect()
    }

    fn context(&self, reference: &[String]) -> String {
        self.run(&[
            "context",
            "--source-id",
            &reference[0],
            "--byte-start",
            &reference[2],
            "--byte-len",
            &reference[3],
        ])
    }
}

impl Drop for Logs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn first_log(logs: &Logs) -> PathBuf {
    logs.projects.join("first").join(format!("{SESSION}.jsonl"))
}

#[test]
fn a_log_moved_to_another_folder_keeps_its_source_id_and_references() {
    let logs = Logs::new("moved");
    let before = logs.find("includes the locale");
    let moved = logs.log("second", &format!("{SESSION}.jsonl"));
    std::fs::rename(first_log(&logs), &moved).expect("move the transcript");
    // The session goes on in its new place.
    let mut text = transcript();
    text.push_str(&record("u4", "02:00:00", "and warm it after deploy"));
    std::fs::write(&moved, text).expect("append to the moved transcript");

    let sync = logs.run(&["sync"]);
    assert!(sync.contains("\"disposition\":\"source_moved\""), "{sync}");
    assert!(sync.contains("\"sources_seen\":1"), "{sync}");
    let after = logs.find("includes the locale");
    assert_eq!(after[0], before[0], "the source id stays");
    assert!(after[1].contains("second"), "{after:?}");
    assert_eq!(after[2..], before[2..], "the record is where it was");
    let raw = logs.context(&before);
    assert!(raw.contains("\"disposition\":\"ready\""), "{raw}");
    assert!(raw.contains("the cache key includes the locale"), "{raw}");
    assert_eq!(logs.find("warm it after deploy")[0], before[0]);
}

#[test]
fn a_log_moved_back_keeps_its_source_id() {
    let logs = Logs::new("moved-back");
    let before = logs.find("includes the locale");
    let moved = logs.log("second", &format!("{SESSION}.jsonl"));
    std::fs::rename(first_log(&logs), &moved).expect("move the transcript");
    logs.run(&["sync"]);
    std::fs::rename(&moved, first_log(&logs)).expect("move it back");

    let sync = logs.run(&["sync"]);
    assert!(sync.contains("\"disposition\":\"source_moved\""), "{sync}");
    assert!(sync.contains("\"sources_seen\":1"), "{sync}");
    assert_eq!(logs.find("includes the locale"), before);
    assert!(logs.context(&before).contains("\"disposition\":\"ready\""));
}

#[test]
fn a_log_that_turns_up_where_a_moved_one_was_gets_an_id_of_its_own() {
    let logs = Logs::new("turned-up");
    let before = logs.find("includes the locale");
    let moved = logs.log("second", &format!("{SESSION}.jsonl"));
    std::fs::rename(first_log(&logs), &moved).expect("move the transcript");
    logs.run(&["sync"]);
    // The session's transcript is written in its first folder again, beside the moved one.
    let mut text = transcript();
    text.push_str(&record("u5", "04:00:00", "resumed in the first folder"));
    std::fs::write(first_log(&logs), text).expect("write the transcript again");

    let sync = logs.run(&["sync"]);
    assert!(sync.contains("\"sources_seen\":2"), "{sync}");
    let returned = logs.find("resumed in the first folder");
    assert_ne!(returned[0], before[0], "the moved log keeps its id alone");
    assert!(returned[1].contains("first"), "{returned:?}");
    let both = logs.find_all("includes the locale");
    assert_eq!(both.len(), 2, "{both:?}");
    let kept = both
        .iter()
        .find(|found| found[0] == before[0])
        .expect("the moved log's record");
    assert!(kept[1].contains("second"), "{kept:?}");
    assert!(both.iter().any(|found| found[0] == returned[0]));
    assert!(logs.context(&before).contains("\"disposition\":\"ready\""));
}

#[test]
fn a_rebuild_keeps_the_source_id_of_a_moved_log() {
    let logs = Logs::new("rebuilt");
    let before = logs.find("includes the locale");
    std::fs::rename(
        first_log(&logs),
        logs.log("second", &format!("{SESSION}.jsonl")),
    )
    .expect("move the transcript");

    // Moved since the last sync, and found by its records.
    let sync = logs.run(&["sync", "--rebuild"]);
    assert!(sync.contains("\"rebuild_cause\":\"requested\""), "{sync}");
    assert_eq!(logs.find("includes the locale")[0], before[0]);
    // Known by its path.
    logs.run(&["sync", "--rebuild"]);
    let after = logs.find("includes the locale");
    assert_eq!(after[0], before[0]);
    assert!(after[1].contains("second"), "{after:?}");
    assert!(logs.context(&before).contains("\"disposition\":\"ready\""));
}

#[test]
fn a_moved_log_that_was_also_compressed_keeps_its_source_id() {
    let logs = Logs::new("moved-compressed");
    let before = logs.find("includes the locale");
    let moved = logs.log("archive", &format!("{SESSION}.jsonl.zst"));
    std::fs::write(&moved, compressed(transcript().as_bytes())).expect("write compressed");
    std::fs::remove_file(first_log(&logs)).expect("remove the original");

    let sync = logs.run(&["sync"]);
    assert!(sync.contains("\"disposition\":\"source_moved\""), "{sync}");
    let after = logs.find("includes the locale");
    assert_eq!(after[0], before[0]);
    assert!(after[1].ends_with(".jsonl.zst"), "{after:?}");
    assert!(logs.context(&before).contains("\"disposition\":\"ready\""));
}

#[test]
fn another_log_with_the_same_name_is_not_taken_for_the_moved_one() {
    let logs = Logs::new("same-name");
    let before = logs.find("includes the locale");
    std::fs::remove_file(first_log(&logs)).expect("remove the original");
    // Same file name, other records: lengths differ where the old records were.
    let other = [
        record("v1", "03:00:00", "a different conversation entirely"),
        record("v2", "03:05:00", "with other words in it"),
    ]
    .concat();
    std::fs::write(logs.log("second", &format!("{SESSION}.jsonl")), other).expect("write");

    let sync = logs.run(&["sync"]);
    assert!(!sync.contains("source_moved"), "{sync}");
    let after = logs.find("different conversation");
    assert_ne!(after[0], before[0], "another log gets its own source id");
    let gone = Command::new(EBIRA)
        .args([
            "context",
            "--corpus",
            &logs.corpus,
            "--source-id",
            &before[0],
        ])
        .args(["--byte-start", &before[2], "--byte-len", &before[3]])
        .output()
        .expect("ebira runs");
    assert!(
        !gone.status.success(),
        "the old log's references name nothing now"
    );
}

#[test]
fn a_copy_left_beside_its_original_is_its_own_log() {
    let logs = Logs::new("copy");
    let before = logs.find("includes the locale");
    std::fs::copy(
        first_log(&logs),
        logs.log("second", &format!("{SESSION}.jsonl")),
    )
    .expect("copy the transcript");

    let sync = logs.run(&["sync"]);
    assert!(!sync.contains("source_moved"), "{sync}");
    assert!(sync.contains("\"sources_seen\":2"), "{sync}");
    assert!(logs.context(&before).contains("\"disposition\":\"ready\""));
}
