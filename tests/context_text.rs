use std::path::{Path, PathBuf};
use std::process::Command;

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

fn command(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(EBIRA)
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("EBIRA_CORPUS")
        .env_remove("EBIRA_TZ_OFFSET")
        .output()
        .unwrap()
}

struct Logs {
    root: PathBuf,
    source: PathBuf,
    corpus: String,
    source_id: String,
    length: usize,
}

impl Logs {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("ebira-context-text-{name}-{}", std::process::id()));
        let source = root.join(".claude/projects/test/session.jsonl");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        let long_output = format!("{}END_OF_ORIGINAL_OUTPUT", "x".repeat(1200));
        let log = format!(concat!(
            "{{\"type\":\"attachment\",\"sessionId\":\"s\",\"timestamp\":\"2026-10-16T13:00:00Z\",\"attachment\":{{\"type\":\"queued_command\",\"prompt\":\"新しい決定\\n二行目\",\"origin\":{{\"kind\":\"human\"}},\"humanTurn\":true}}}}\n",
            "{{\"type\":\"assistant\",\"sessionId\":\"s\",\"timestamp\":\"2026-10-16T13:00:01Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"thinking\",\"thinking\":\"HIDDEN_REASONING\",\"signature\":\"OPAQUE_SIGNATURE\"}},{{\"type\":\"text\",\"text\":\"確認します\"}},{{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"Bash\",\"input\":{{\"command\":\"echo evidence\"}}}}]}}}}\n",
            "{{\"type\":\"user\",\"sessionId\":\"s\",\"timestamp\":\"2026-10-16T13:00:02Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t\",\"content\":\"{}\"}}]}}}}\n"
        ), long_output);
        std::fs::write(&source, &log).unwrap();
        let corpus = root.join("corpus").to_str().unwrap().to_owned();
        let sync = command(
            &root,
            &[
                "sync",
                "--source",
                source.to_str().unwrap(),
                "--corpus",
                &corpus,
            ],
        );
        assert!(
            sync.status.success(),
            "{}",
            String::from_utf8_lossy(&sync.stderr)
        );
        let said = command(&root, &["said", "--corpus", &corpus]);
        let said = String::from_utf8(said.stdout).unwrap();
        let source_id = said
            .split("\"source_id\":\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_owned();
        Self {
            root,
            source,
            corpus,
            source_id,
            length: log.len(),
        }
    }

    fn context(&self, format: &str, length: usize) -> std::process::Output {
        command(
            &self.root,
            &[
                "context",
                "--corpus",
                &self.corpus,
                "--source-id",
                &self.source_id,
                "--byte-start",
                "0",
                "--byte-len",
                &length.to_string(),
                "--format",
                format,
            ],
        )
    }
}

impl Drop for Logs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn readable_context_keeps_original_values_sender_and_byte_references() {
    let logs = Logs::new("values");
    let result = logs.context("text", logs.length);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(text.contains("human (user"), "{text}");
    assert!(text.contains("新しい決定\n二行目"), "{text}");
    assert!(text.contains("assistant (assistant"), "{text}");
    assert!(text.contains("echo evidence"), "{text}");
    assert!(text.contains("system (output"), "{text}");
    assert!(
        text.contains("END_OF_ORIGINAL_OUTPUT"),
        "output must not stop at the corpus preview"
    );
    assert!(!text.contains("HIDDEN_REASONING") && !text.contains("OPAQUE_SIGNATURE"));
    assert_eq!(text.matches("# source_ref:").count(), 3);
    let raw = String::from_utf8(logs.context("json", logs.length).stdout).unwrap();
    assert!(
        raw.contains("HIDDEN_REASONING"),
        "the original JSON view remains lossless"
    );
}

#[test]
fn readable_context_does_not_misrepresent_partial_or_replaced_records() {
    let logs = Logs::new("boundaries");
    let partial = logs.context("text", logs.length - 1);
    assert!(!partial.status.success());
    assert!(String::from_utf8_lossy(&partial.stdout).contains("complete indexed records"));
    std::fs::write(&logs.source, "{}\n").unwrap();
    let changed = logs.context("text", logs.length);
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let text = String::from_utf8(changed.stdout).unwrap();
    assert!(text.contains("source_changed_since_projection"), "{text}");
    assert!(!text.contains("新しい決定"));
}
