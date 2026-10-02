use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

const EXE: &str = env!("CARGO_BIN_EXE_ebira");
const QUESTION: &str = "AGENT_QUESTION /~|%\t日本語\nWhich option?";
const KEY: &str = "choice/~|%\t日本語\n";

fn quoted(text: &str) -> String {
    let mut value = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => value.push_str("\\\""),
            '\\' => value.push_str("\\\\"),
            '\n' => value.push_str("\\n"),
            '\r' => value.push_str("\\r"),
            '\t' => value.push_str("\\t"),
            c => value.push(c),
        }
    }
    value.push('"');
    value
}

#[derive(Clone, Copy, Debug)]
enum Flavor {
    Claude,
    Codex,
}

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    corpus: PathBuf,
    flavor: Flavor,
}

impl Fixture {
    fn new(name: &str, flavor: Flavor) -> Self {
        let root = std::env::temp_dir().join(format!(
            "ebira-question-{name}-{flavor:?}-{}",
            std::process::id()
        ));
        let source = root.join(match flavor {
            Flavor::Claude => ".claude/projects/test/session.jsonl",
            Flavor::Codex => ".codex/sessions/rollout-session.jsonl",
        });
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let fixture = Self {
            corpus: root.join("corpus"),
            root,
            source,
            flavor,
        };
        fs::write(&fixture.source, fixture.initial()).unwrap();
        fixture
    }

    fn initial(&self) -> &'static str {
        match self.flavor {
            Flavor::Claude => concat!(
                r#"{"type":"user","sessionId":"session","timestamp":"2026-10-02T00:00:00Z","message":{"role":"user","content":"INITIAL_PERSON_MESSAGE"}}"#,
                "\n"
            ),
            Flavor::Codex => concat!(
                r#"{"type":"session_meta","payload":{"id":"session","source":"vscode","originator":"Codex Desktop"}}"#,
                "\n",
                r#"{"type":"response_item","timestamp":"2026-10-02T00:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"INITIAL_PERSON_MESSAGE"}]}}"#,
                "\n"
            ),
        }
    }

    fn call(&self, id: &str, native: bool) -> String {
        match self.flavor {
            Flavor::Claude => format!(
                "{{\"type\":\"assistant\",\"sessionId\":\"session\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":{},\"name\":{},\"input\":{{\"questions\":[{{\"question\":{}}}]}}}}]}}}}\n",
                quoted(id), quoted(if native { "AskUserQuestion" } else { "OtherTool" }), quoted(QUESTION)),
            Flavor::Codex => {
                let args = format!("{{\"questions\":[{{\"id\":{},\"question\":{}}}]}}", quoted(KEY), quoted(QUESTION));
                format!("{{\"type\":\"response_item\",\"payload\":{{\"type\":\"function_call\",\"name\":{},\"call_id\":{},\"arguments\":{}}}}}\n", quoted(if native { "request_user_input" } else { "exec_command" }), quoted(id), quoted(&args))
            }
        }
    }

    fn answer_map(&self, values: &[&str]) -> String {
        match self.flavor {
            Flavor::Claude => format!("{{{}:{}}}", quoted(QUESTION), quoted(&values.join(", "))),
            Flavor::Codex => format!(
                "{{{}:{{\"answers\":[{}]}}}}",
                quoted(KEY),
                values
                    .iter()
                    .map(|s| quoted(s))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    fn result(&self, id: &str, answers: &str, error: bool) -> String {
        match self.flavor {
            Flavor::Claude => format!(
                "{{\"type\":\"user\",\"sessionId\":\"session\",\"timestamp\":\"2026-10-02T00:00:02Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":{},\"is_error\":{},\"content\":\"AUTOMATIC_RENDERER_TEXT\"}}]}},\"toolUseResult\":{{\"questions\":[{{\"question\":{}}}],\"answers\":{}}}}}\n",
                quoted(id), error, quoted(QUESTION), answers),
            Flavor::Codex => format!(
                "{{\"type\":\"response_item\",\"timestamp\":\"2026-10-02T00:00:02Z\",\"payload\":{{\"type\":\"function_call_output\",\"call_id\":{},\"is_error\":{},\"output\":{}}}}}\n",
                quoted(id), error, quoted(&format!("{{\"answers\":{answers}}}"))),
        }
    }

    fn append(&self, text: &str) {
        self.append_bytes(text.as_bytes());
    }

    fn append_bytes(&self, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(&self.source)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    fn run(&self, args: &[&str]) -> String {
        let output = Command::new(EXE)
            .args(args)
            .arg("--corpus")
            .arg(&self.corpus)
            .env("HOME", self.root.join("empty-home"))
            .env("USERPROFILE", self.root.join("empty-home"))
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("EBIRA_CORPUS")
            .env_remove("EBIRA_TZ_OFFSET")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn sync(&self) -> String {
        self.run(&["sync", "--source", self.source.to_str().unwrap()])
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn native_question_answers_are_persons_words_and_keep_the_original_reference() {
    for flavor in [Flavor::Claude, Flavor::Codex] {
        let fixture = Fixture::new("answers", flavor);
        let answer = format!(
            "PERSON_ANSWER 日本語\n{}\tkeep the ending",
            "long-answer ".repeat(70)
        );
        fixture.append(&fixture.call("ask-1", true));
        let offset = fs::metadata(&fixture.source).unwrap().len();
        let record = fixture.result("ask-1", &fixture.answer_map(&[&answer]), false);
        fixture.append(&record);
        fixture.sync();
        let said = fixture.run(&["said", "--format", "text"]);
        assert!(
            said.contains(&answer),
            "answer must not be truncated as tool output: {said}"
        );
        assert!(
            !said.contains("AGENT_QUESTION") && !said.contains("AUTOMATIC_RENDERER_TEXT"),
            "{said}"
        );
        let listed = fixture.run(&["said"]);
        assert!(
            listed.contains("\"total_messages\":2")
                && listed.contains("\"via\":\"question_reply\""),
            "{listed}"
        );
        let found = fixture.run(&[
            "search",
            "--query",
            "PERSON_ANSWER",
            "--sender",
            "human",
            "--kind",
            "user",
            "--role",
            "user",
        ]);
        assert!(found.contains("\"total_candidates\":1"), "{found}");
        let source_id = found
            .split("\"source_id\":\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let original = fixture.run(&[
            "context",
            "--source-id",
            source_id,
            "--byte-start",
            &offset.to_string(),
            "--byte-len",
            &record.len().to_string(),
        ]);
        assert!(
            original.contains(&quoted(&record)),
            "the source reference remains the full original record: {original}"
        );
        let brief = fixture.run(&["resume", "--brief"]);
        assert!(
            brief.contains(&quoted(&answer)) && brief.contains("\"via\":\"question_reply\""),
            "{brief}"
        );
    }
}

#[test]
fn pending_questions_survive_sync_reuse_and_an_incomplete_reply() {
    for flavor in [Flavor::Claude, Flavor::Codex] {
        let fixture = Fixture::new("incremental", flavor);
        fixture.append(&fixture.call("ask-1", true));
        fixture.sync();
        assert!(fixture.sync().contains("\"sources_reused\":1"));
        let answer = fixture.result(
            "ask-1",
            &fixture.answer_map(&["DEFERRED_PERSON_ANSWER"]),
            false,
        );
        // Even an incomplete UTF-8 character must wait for the complete JSONL record.
        let split = answer.find("日本語").unwrap() + 1;
        let (part, rest) = answer.as_bytes().split_at(split);
        fixture.append_bytes(part);
        assert!(fixture.sync().contains("\"partial_records\":1"));
        fixture.append_bytes(rest);
        fixture.sync();
        let said = fixture.run(&["said"]);
        assert!(
            said.contains("DEFERRED_PERSON_ANSWER") && said.contains("\"total_messages\":2"),
            "{said}"
        );
        assert!(fixture.sync().contains("\"sources_reused\":1"));
        assert!(fixture.run(&["said"]).contains("\"total_messages\":2"));
    }
}

#[test]
fn follow_uses_question_checkpoints_and_replays_them_before_older_cursors() {
    for flavor in [Flavor::Claude, Flavor::Codex] {
        let fixture = Fixture::new("follow", flavor);
        fixture.append(&fixture.call("ask-1", true));
        fixture.sync();
        let cursor = fs::metadata(&fixture.source).unwrap().len().to_string();
        fixture.append(&fixture.result(
            "ask-1",
            &fixture.answer_map(&["FOLLOW_PERSON_ANSWER"]),
            false,
        ));
        let args = [
            "follow",
            "--source",
            fixture.source.to_str().unwrap(),
            "--after-byte",
            &cursor,
            "--seconds",
            "0",
            "--sender",
            "human",
        ];
        let unsynced = fixture.run(&args);
        assert!(
            unsynced.contains("FOLLOW_PERSON_ANSWER")
                && unsynced.contains("\"via\":\"question_reply\""),
            "{unsynced}"
        );
        fixture.sync();
        let indexed = fixture.run(&args);
        assert!(indexed.contains("FOLLOW_PERSON_ANSWER"), "{indexed}");
    }
}

#[test]
fn ordinary_unmatched_cancelled_and_errored_results_are_not_human() {
    for flavor in [Flavor::Claude, Flavor::Codex] {
        let fixture = Fixture::new("negative", flavor);
        let answers = fixture.answer_map(&["NOT_A_PERSON_ANSWER"]);
        fixture.append(&fixture.call("ordinary", false));
        fixture.append(&fixture.result("ordinary", &answers, false));
        fixture.append(&fixture.result("unmatched", &answers, false));
        fixture.append(&fixture.call("cancelled", true));
        fixture.append(&fixture.result("cancelled", "{}", false));
        fixture.append(&fixture.result("cancelled", &answers, false));
        fixture.append(&fixture.call("errored", true));
        fixture.append(&fixture.result("errored", &answers, true));
        fixture.append(&fixture.call("wrong-key", true));
        fixture.append(&fixture.result(
            "wrong-key",
            "{\"unasked\":{\"answers\":[\"NOT_A_PERSON_ANSWER\"]}}",
            false,
        ));
        fixture.append(&fixture.call("reused-id", true));
        fixture.append(&fixture.call("reused-id", false));
        fixture.append(&fixture.result("reused-id", &answers, false));
        fixture.sync();
        let said = fixture.run(&["said"]);
        assert!(
            said.contains("\"total_messages\":1") && !said.contains("NOT_A_PERSON_ANSWER"),
            "{said}"
        );
        let found = fixture.run(&[
            "search",
            "--query",
            "NOT_A_PERSON_ANSWER",
            "--sender",
            "human",
        ]);
        assert!(found.contains("\"total_candidates\":0"), "{found}");
    }
}

#[test]
fn separate_questions_can_receive_identical_answers_at_the_same_time() {
    for flavor in [Flavor::Claude, Flavor::Codex] {
        let fixture = Fixture::new("same-answer", flavor);
        fixture.append(&fixture.call("ask-1", true));
        fixture.append(&fixture.result("ask-1", &fixture.answer_map(&["Same answer"]), false));
        fixture.append(&fixture.call("ask-2", true));
        fixture.sync();
        let cursor = fs::metadata(&fixture.source).unwrap().len().to_string();
        fixture.append(&fixture.result("ask-2", &fixture.answer_map(&["Same answer"]), false));
        let followed = fixture.run(&[
            "follow",
            "--source",
            fixture.source.to_str().unwrap(),
            "--after-byte",
            &cursor,
            "--seconds",
            "0",
            "--sender",
            "human",
        ]);
        assert!(followed.contains("Same answer"), "{followed}");
        fixture.sync();
        let said = fixture.run(&["said"]);
        assert!(
            said.contains("\"total_messages\":3") && said.contains("\"copies_skipped\":0"),
            "{said}"
        );
        assert!(fixture
            .run(&["resume", "--brief"])
            .contains("\"via\":\"question_reply\""));
    }
}

#[test]
fn replacing_a_log_discards_pending_question_associations() {
    for flavor in [Flavor::Claude, Flavor::Codex] {
        let fixture = Fixture::new("replacement", flavor);
        fixture.append(&fixture.call("ask-1", true));
        fixture.sync();
        let replacement = fixture.source.with_extension("replacement");
        fs::write(
            &replacement,
            format!(
                "{}{}",
                fixture.initial(),
                fixture.result(
                    "ask-1",
                    &fixture.answer_map(&["UNMATCHED_AFTER_REPLACEMENT"]),
                    false
                )
            ),
        )
        .unwrap();
        fs::rename(replacement, &fixture.source).unwrap();
        fixture.sync();
        let said = fixture.run(&["said"]);
        assert!(
            said.contains("\"total_messages\":1") && !said.contains("UNMATCHED_AFTER_REPLACEMENT"),
            "{said}"
        );
    }
}

#[test]
fn an_old_corpus_is_automatically_rebuilt_to_include_question_answers() {
    let fixture = Fixture::new("migration", Flavor::Codex);
    fixture.append(&fixture.call("ask-1", true));
    fixture.append(&fixture.result(
        "ask-1",
        &fixture.answer_map(&["ANSWER_AFTER_UPGRADE"]),
        false,
    ));
    fixture.sync();
    let catalog = fixture.corpus.join("sources.tsv");
    let old = fs::read_to_string(&catalog)
        .unwrap()
        .lines()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                line.replace("v=8", "v=7").replace("rules=4", "rules=3")
            } else {
                line.split('\t').take(20).collect::<Vec<_>>().join("\t")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(catalog, old).unwrap();
    let rebuilt = fixture.sync();
    assert!(
        rebuilt.contains("\"rebuild_cause\":\"format_changed\""),
        "{rebuilt}"
    );
    let said = fixture.run(&["said"]);
    assert!(
        said.contains("ANSWER_AFTER_UPGRADE") && said.contains("\"total_messages\":2"),
        "{said}"
    );
    assert!(fixture.sync().contains("\"sources_reused\":1"));
}
