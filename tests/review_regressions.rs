//! Extraction regression tests using synthetic records and a local test worker.
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
const EXE: &str = env!("CARGO_BIN_EXE_ebira");
struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str, records: &str) -> Self {
        let root = std::env::temp_dir().join(format!("ebira-review-{name}-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let f = Self(root);
        fs::write(f.0.join("source.jsonl"), records).unwrap();
        let out = f
            .command()
            .args(["sync", "--source"])
            .arg(f.0.join("source.jsonl"))
            .args(["--tool-output-chars", "4", "--corpus"])
            .arg(f.0.join("corpus"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        f
    }
    fn command(&self) -> Command {
        let mut c = Command::new(EXE);
        c.env("HOME", self.0.join("empty-home"))
            .env("USERPROFILE", self.0.join("empty-home"))
            .env_remove("CODEX_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("EBIRA_CORPUS")
            .env_remove("EBIRA_TZ_OFFSET");
        c
    }
    fn run(&self, request: Value) -> Vec<Value> {
        fs::write(self.0.join("request.json"), request.to_string()).unwrap();
        let mut child = self
            .command()
            .args(["extract", "--corpus"])
            .arg(self.0.join("corpus"))
            .arg("--request")
            .arg(self.0.join("request.json"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if start.elapsed() > Duration::from_secs(5) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("extraction exceeded external five-second test deadline");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn request() -> Value {
    json!({"version":1,"scope":{"all":true},"targets":{"kinds":["output"]},"questions":[{"id":"q","type":"noul","question":"Present?"}],"runtime":{"backend":"mock","answers":{"q":{"type":"noul","noul":1.0}}},"cache":false})
}
fn records(output: &str) -> String {
    format!(
        "{}\n{}\n",
        json!({"type":"session_meta","payload":{"id":"session"}}),
        json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":output}})
    )
}
#[test]
fn numeric_equal_accepts_integer_threshold_for_float_answer() {
    let f = Fixture::new("numeric-eq", &records("full output"));
    let mut r = request();
    r["select"] = json!([{"question":"q","op":"eq","value":1}]);
    let rows = f.run(r);
    assert_eq!(rows.last().unwrap()["coverage"]["selected"], 1);
}
#[test]
fn score_above_last_ordinal_index_is_unscored() {
    let f = Fixture::new("score-range", &records("full output"));
    let mut r = request();
    r["questions"] =
        json!([{"id":"q","type":"score","question":"Level?","choices":["low","high"]}]);
    r["runtime"]["answers"] = json!({"q":{"type":"score","score":1.9}});
    let rows = f.run(r);
    assert_eq!(rows.last().unwrap()["coverage"]["unscored"], 1);
}
#[test]
fn duplicate_raw_paths_are_unscored() {
    let f=Fixture::new("duplicate-path",concat!("{\"type\":\"session_meta\",\"payload\":{\"id\":\"session\"}}\n", "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"c\",\"output\":\"FIRST_LONG_VALUE\",\"output\":\"SECOND_LONG_VALUE\"}}\n"));
    let rows = f.run(request());
    assert_eq!(rows.last().unwrap()["coverage"]["unscored"], 1);
    let item = rows.iter().find(|v| v["type"] == "item").unwrap();
    assert_eq!(item["metadata"]["input_complete"], false);
}
#[test]
fn timeout_covers_blocked_large_stdin_write() {
    let f = Fixture::new("blocked-write", &records(&"A".repeat(100_000)));
    let worker = f.0.join("worker.py");
    fs::write(&worker,"import sys,json,time\nr=json.loads(sys.stdin.readline());print(json.dumps({'id':r['id'],'model_identity':'test-fixture'}),flush=True);time.sleep(4)\n").unwrap();
    let mut r = request();
    r["runtime"] = json!({"backend":"clef","worker_path":worker,"timeout_seconds":1});
    let start = Instant::now();
    let rows = f.run(r);
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "stdin write exceeded deadline"
    );
    assert_eq!(rows.last().unwrap()["coverage"]["unscored"], 1);
}
