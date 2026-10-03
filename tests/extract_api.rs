//! End-to-end extraction tests use synthetic transcripts and an explicit mock backend.
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const EXE: &str = env!("CARGO_BIN_EXE_ebira");
struct Fixture {
    root: PathBuf,
    source: PathBuf,
    corpus: PathBuf,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("ebira-extract-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join(".codex/sessions/rollout-session.jsonl");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let f = Self {
            corpus: root.join("corpus"),
            source,
            root,
        };
        f.write("first line\nsecond line\t日本語");
        f.sync();
        f
    }
    fn write(&self, output: &str) {
        let rows = [
            json!({"type":"session_meta","payload":{"id":"session","source":"vscode","originator":"Codex Desktop"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Please inspect this result"}]}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call-1","arguments":"{\"cmd\":\"example\"}"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":output}}),
        ];
        fs::write(
            &self.source,
            rows.iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
    }
    fn command(&self) -> Command {
        let mut c = Command::new(EXE);
        c.env("HOME", self.root.join("empty-home"))
            .env("USERPROFILE", self.root.join("empty-home"))
            .env_remove("CODEX_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("EBIRA_CORPUS")
            .env_remove("EBIRA_TZ_OFFSET");
        c
    }
    fn sync(&self) {
        let out = self
            .command()
            .args(["sync", "--source"])
            .arg(&self.source)
            .arg("--corpus")
            .arg(&self.corpus)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "sync: {} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn extract(&self, r: &Value, stdin: bool) -> Output {
        let mut command = self.command();
        command
            .arg("extract")
            .arg("--corpus")
            .arg(&self.corpus)
            .arg("--request");
        if stdin {
            let mut child = command
                .arg("-")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(r.to_string().as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        } else {
            let path = self.root.join("request.json");
            fs::write(&path, r.to_string()).unwrap();
            command.arg(path).output().unwrap()
        }
    }
    fn run(&self, r: &Value, stdin: bool) -> Vec<Value> {
        let out = self.extract(r, stdin);
        assert!(
            out.status.success(),
            "extract: {} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let rows: Vec<Value> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).expect("stdout is exclusively JSONL"))
            .collect();
        assert_eq!(rows.first().unwrap()["type"], "progress");
        assert_eq!(rows.last().unwrap()["type"], "done");
        rows
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn request() -> Value {
    json!({"version":1,"scope":{"all":true},"context":{"mode":"event"},"targets":{"kinds":["output"]},
        "questions":[{"id":"relevant","question":"Relevant?","type":"noul"},{"id":"excluded","question":"Exclude?","type":"noul"}],
        "runtime":{"backend":"mock","answers":{"relevant":{"type":"noul","noul":0.8},"excluded":{"type":"noul","noul":0.1}}},
        "select":[{"question":"relevant","op":"gte","value":0.7},{"question":"excluded","op":"lte","value":0.2}],"cache":true})
}
fn items(rows: &[Value]) -> Vec<&Value> {
    rows.iter().filter(|v| v["type"] == "item").collect()
}
fn coverage(rows: &[Value]) -> &Value {
    &rows.last().unwrap()["coverage"]
}

#[test]
fn multiline_output_is_one_logical_item_and_stdin_matches_file() {
    let f = Fixture::new("multiline");
    let r = request();
    let first = f.run(&r, false);
    assert_eq!(coverage(&first)["candidates"], 1);
    assert_eq!(coverage(&first)["selected"], 1);
    let item = items(&first)[0];
    assert_eq!(item["metadata"]["input_complete"], true);
    assert_eq!(item["metadata"]["call_id"], "call-1");
    assert_eq!(item["answers"]["relevant"]["noul"], 0.8);
    assert_eq!(item["cached"], false);
    let second = f.run(&r, true);
    assert_eq!(items(&second)[0]["event_id"], item["event_id"]);
    assert_eq!(coverage(&second)["cached"], 1);
}

#[test]
fn independent_selection_and_threshold_changes_reuse_scores() {
    let f = Fixture::new("threshold");
    let mut r = request();
    assert_eq!(coverage(&f.run(&r, false))["selected"], 1);
    r["select"][0]["value"] = json!(0.9);
    let high = f.run(&r, false);
    assert_eq!(coverage(&high)["selected"], 0);
    assert_eq!(coverage(&high)["cached"], 1);
    assert_eq!(coverage(&high)["scored"], 1);
    r["select"][0]["value"] = json!(0.7);
    r["select"][1]["value"] = json!(0.05);
    let excluded = f.run(&r, false);
    assert_eq!(coverage(&excluded)["selected"], 0);
    assert_eq!(coverage(&excluded)["cached"], 1);
}

#[test]
fn second_pass_narrows_by_stable_event_id_and_preserves_provenance() {
    let f = Fixture::new("second-pass");
    let mut r = request();
    r["targets"] = json!({});
    let first = f.run(&r, false);
    assert!(items(&first).len() >= 3);
    let output = items(&first)
        .into_iter()
        .find(|i| i["metadata"]["kind"] == "output")
        .unwrap();
    r["scope"]["event_ids"] = json!([output["event_id"]]);
    let second = f.run(&r, false);
    assert_eq!(coverage(&second)["candidates"], 1);
    assert_eq!(
        items(&second)[0]["metadata"]["source_ref"],
        output["metadata"]["source_ref"]
    );
    assert_eq!(items(&second)[0]["event_id"], output["event_id"]);
    assert_eq!(coverage(&second)["cached"], 1);
}

#[test]
fn rewritten_input_is_unscored_until_sync_then_invalidates_cache() {
    let f = Fixture::new("rewrite");
    let r = request();
    let first = f.run(&r, false);
    assert_eq!(coverage(&first)["scored"], 1);
    f.write("REPLACED source text whose contents differ");
    let stale = f.run(&r, false);
    assert_eq!(coverage(&stale)["scored"], 0);
    assert_eq!(coverage(&stale)["unscored"], 1);
    assert_eq!(stale.last().unwrap()["complete"], false);
    assert_eq!(items(&stale)[0]["status"], "unscored");
    f.sync();
    let fresh = f.run(&r, false);
    assert_eq!(coverage(&fresh)["scored"], 1);
    assert_eq!(coverage(&fresh)["cached"], 0);
}

#[test]
fn invalid_runtime_answers_are_unscored_and_skip_cache() {
    let f = Fixture::new("bad-answers");
    let mut r = request();
    r["runtime"]["answers"]["relevant"]["noul"] = json!(1.5);
    for _ in 0..2 {
        let rows = f.run(&r, false);
        assert_eq!(coverage(&rows)["unscored"], 1);
        assert_eq!(coverage(&rows)["cached"], 0);
        assert_eq!(coverage(&rows)["selected"], 0);
        assert_eq!(rows.last().unwrap()["complete"], false);
        assert_eq!(items(&rows)[0]["error"], "invalid_answers");
    }
}

#[test]
fn malformed_requests_return_errors() {
    let f = Fixture::new("schema");
    let mut invalid = Vec::new();
    for (field, value) in [
        ("version", json!(2)),
        ("questions", json!([])),
        ("scope", json!({})),
        ("context", json!({"mode":"unbounded"})),
        (
            "select",
            json!([{"question":"missing","op":"eq","value":true}]),
        ),
    ] {
        let mut r = request();
        r[field] = value;
        invalid.push(r);
    }
    let mut r = request();
    r["questions"][0]["type"] = json!("boolean");
    invalid.push(r);
    let mut r = request();
    r["questions"][1]["id"] = json!("relevant");
    invalid.push(r);
    let mut r = request();
    r["runtime"] = json!({"backend":"mock"});
    invalid.push(r);
    let mut r = request();
    r["scope"]["event_ids"] = json!(["nonexistent:999"]);
    invalid.push(r);
    for r in invalid {
        let out = f.extract(&r, false);
        assert!(!out.status.success(), "invalid request succeeded: {r}");
        assert!(!String::from_utf8_lossy(&out.stdout).contains("\"type\":\"done\""));
    }
}

#[test]
fn oversized_output_hydrates_the_full_selected_field() {
    let f = Fixture::new("hydration");
    let text = format!(
        "HEAD\n{}\nTAIL_AFTER_PROJECTED_LIMIT",
        "日本語 and multiline\n".repeat(500)
    );
    f.write(&text);
    f.sync();
    let rows = f.run(&request(), false);
    assert_eq!(coverage(&rows)["scored"], 1);
    assert_eq!(
        items(&rows)[0]["metadata"]["input_completeness"]["hydration"],
        "raw_selected_fields"
    );
    // Verify hydrated field contents through the worker protocol.
    let worker = f.root.join("verify_state.py");
    fs::write(&worker, r#"import json, sys
for line in sys.stdin:
    r = json.loads(line)
    if r['op'] == 'identity':
        out = {'id': r['id'], 'model_identity': 'test-state-verifier-v1'}
    else:
        state = json.loads(r['state'])
        values = [field['value'] for field in state['fields']]
        valid = any(v.startswith('HEAD\n') and v.endswith('TAIL_AFTER_PROJECTED_LIMIT') and len(v) > 5000 for v in values)
        out = {'id': r['id'], 'model_identity': 'test-state-verifier-v1', 'answers': {'relevant': {'type': 'noul', 'noul': 0.8}, 'excluded': {'type': 'noul', 'noul': 0.1}}} if valid else {'id': r['id'], 'error': 'hydrated_tail_missing'}
    print(json.dumps(out), flush=True)
"#).unwrap();
    let mut r = request();
    r["runtime"] = json!({"backend":"clef","worker_path":worker,"python":"python3"});
    r["cache"] = json!(false);
    let verified = f.run(&r, false);
    assert_eq!(coverage(&verified)["scored"], 1);
    assert_eq!(coverage(&verified)["unscored"], 0);
}

#[test]
fn choice_and_score_answers_obey_independent_typed_selection() {
    let f = Fixture::new("typed-selection");
    let mut r = request();
    r["questions"] = json!([
        {"id":"category","question":"Category?","type":"choice","choices":["keep","drop"]},
        {"id":"strength","question":"Strength?","type":"score","choices":["low","medium","high"]}
    ]);
    r["runtime"]["answers"] = json!({"category":{"type":"choice","choice":"keep"},"strength":{"type":"score","score":1.8}});
    r["select"] = json!([{"question":"category","op":"eq","value":"keep"},{"question":"strength","op":"gte","value":1.5}]);
    let rows = f.run(&r, false);
    assert_eq!(coverage(&rows)["selected"], 1);
    r["select"][0]["value"] = json!("drop");
    let rows = f.run(&r, false);
    assert_eq!(coverage(&rows)["selected"], 0);
    assert_eq!(coverage(&rows)["cached"], 1);
}

#[test]
fn paired_context_retains_same_call_and_missing_pair_is_partial_coverage() {
    let f = Fixture::new("pairing");
    let mut source = fs::read_to_string(&f.source).unwrap();
    source.push_str(&json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"unmatched","output":"orphan result"}}).to_string());
    source.push('\n');
    fs::write(&f.source, source).unwrap();
    f.sync();
    let mut r = request();
    r["context"]["mode"] = json!("paired");
    let rows = f.run(&r, false);
    assert_eq!(coverage(&rows)["candidates"], 2);
    assert_eq!(coverage(&rows)["scored"], 1);
    assert_eq!(coverage(&rows)["unscored"], 1);
    assert_eq!(rows.last().unwrap()["complete"], false);
    let scored = items(&rows)
        .into_iter()
        .find(|i| i["status"] == "scored")
        .unwrap();
    assert_eq!(scored["metadata"]["call_id"], "call-1");
    assert!(scored["metadata"]["paired_event_id"].is_string());
    assert_eq!(
        scored["metadata"]["paired_source_ref"]["source_id"],
        scored["metadata"]["source_ref"]["source_id"]
    );
    let failed = items(&rows)
        .into_iter()
        .find(|i| i["status"] == "unscored")
        .unwrap();
    assert_eq!(failed["metadata"]["call_id"], "unmatched");
    assert_eq!(failed["error"], "incomplete_input");
    assert!(failed["metadata"]["error"]
        .as_str()
        .unwrap()
        .contains("pair"));
    let again = f.run(&r, false);
    assert_eq!(coverage(&again)["cached"], 1);
    assert_eq!(coverage(&again)["unscored"], 1);
}
