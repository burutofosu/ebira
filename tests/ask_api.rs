//! Deterministic API-path checks over loopback and synthetic logs. No real key or remote API.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

fn ebira(root: &Path) -> Command {
    let mut command = Command::new(EBIRA);
    command
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("LOCALAPPDATA", root.join("local"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("ANTHROPIC_API_KEY", "sk-ant-test")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("EBIRA_CORPUS")
        .env_remove("EBIRA_TZ_OFFSET");
    command
}

fn quoted(text: &str) -> String {
    format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

fn response(stop: &str, text: &str) -> String {
    format!(
        r#"{{"model":"claude-haiku-5-5","stop_reason":"{stop}","content":[{{"type":"text","text":{}}}],"usage":{{"input_tokens":10,"output_tokens":2}}}}"#,
        quoted(text)
    )
}

struct Logs(PathBuf);
impl Drop for Logs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `offset` is the corpus's `EBIRA_TZ_OFFSET`, UTC when none.
fn run_case(
    name: &str,
    first_stop: &str,
    retry_succeeds: bool,
    expected_answers: usize,
    offset: Option<&str>,
) {
    let logs =
        Logs(std::env::temp_dir().join(format!("ebira-ask-api-{name}-{}", std::process::id())));
    let source = logs.0.join(".claude/projects/example/session.jsonl");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, concat!(
        r#"{"type":"user","sessionId":"s","timestamp":"2026-09-01T00:00:00Z","message":{"role":"user","content":"keep remote access"}}"#, "\n",
        r#"{"type":"user","sessionId":"s","timestamp":"2026-09-02T00:00:00Z","message":{"role":"user","content":"LATEST_DECISION: remove it entirely"}}"#, "\n",
    )).unwrap();
    let corpus = logs.0.join("corpus");
    let mut sync = ebira(&logs.0);
    if let Some(offset) = offset {
        sync.env("EBIRA_TZ_OFFSET", offset);
    }
    let sync = sync
        .args([
            "sync",
            "--source",
            source.to_str().unwrap(),
            "--corpus",
            corpus.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        sync.status.success(),
        "{}",
        String::from_utf8_lossy(&sync.stderr)
    );

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let done = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let stop = first_stop.to_string();
    let server_done = done.clone();
    let received = requests.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut answers = 0;
        while !server_done.load(Ordering::SeqCst) && Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            // Accepted sockets inherit the listener's nonblocking mode on Windows.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut data = Vec::new();
            let mut buffer = [0; 8192];
            let (header_end, length) = loop {
                let n = stream.read(&mut buffer).unwrap();
                assert!(n > 0, "HTTP header ended early");
                data.extend_from_slice(&buffer[..n]);
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                    let length = header
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse::<usize>()
                        .unwrap();
                    if header.contains("expect: 100-continue") {
                        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").unwrap();
                    }
                    break (end + 4, length);
                }
            };
            while data.len() < header_end + length {
                let n = stream.read(&mut buffer).unwrap();
                assert!(n > 0, "HTTP body ended early");
                data.extend_from_slice(&buffer[..n]);
            }
            let body = String::from_utf8(data[header_end..header_end + length].to_vec()).unwrap();
            let reply = if body.contains("You screen search results") {
                response("end_turn", r#"{"scores":[{"id":"C1","score":3}]}"#)
            } else {
                answers += 1;
                received.lock().unwrap().push(body);
                if answers == 1 || !retry_succeeds {
                    response(&stop, "{")
                } else {
                    response(
                        "end_turn",
                        r#"{"answer":"Remove it entirely.","findings":[],"timeline":[],"gaps":[],"more_terms":[]}"#,
                    )
                }
            };
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).unwrap();
        }
    });
    let output = ebira(&logs.0)
        .env("ANTHROPIC_BASE_URL", &url)
        .args([
            "ask",
            "What is decided?",
            "--quick",
            "--term",
            "remote",
            "--keep",
            "1",
            "--corpus",
            corpus.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    done.store(true, Ordering::SeqCst);
    server.join().unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        expected_answers,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        requests[0].contains("LATEST_DECISION"),
        "latest evidence reaches the answer model"
    );
    let zone = format!("UTC{}", offset.unwrap_or("+00:00"));
    assert!(
        requests[0].contains(&format!("Times: every time below is local time, {zone}")),
        "the answer model is told the zone of the times: {}",
        requests[0]
    );
    if offset == Some("+09:00") {
        assert!(
            requests[0].contains("2026-09-02 09:00"),
            "the records' times are at the corpus offset"
        );
    }
    assert!(requests[0].contains("\"max_tokens\":32000"));
    if expected_answers == 2 {
        assert!(requests[1].contains("\"max_tokens\":64000"));
        assert_eq!(
            requests[0].replace("\"max_tokens\":32000", "\"max_tokens\":64000"),
            requests[1],
            "only the output allowance changes"
        );
    }
    assert_eq!(
        output.status.success(),
        retry_succeeds,
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    if retry_succeeds {
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains(&format!("\"local_time_zone\":\"{zone}\"")),
            "the result names the zone of its local times"
        );
    }
    let ledger = if cfg!(windows) {
        logs.0.join("local/ebira/ask/usage.tsv")
    } else {
        logs.0.join("data/ebira/ask/usage.tsv")
    };
    let ledger = std::fs::read_to_string(ledger).unwrap();
    assert_eq!(
        ledger
            .lines()
            .skip(1)
            .filter(|line| line.split('\t').nth(1) == Some("answer"))
            .count(),
        expected_answers,
        "every billed attempt is retained"
    );
}

#[test]
fn truncated_answer_is_retried_once_and_both_attempts_are_billed() {
    run_case("retry", "max_tokens", true, 2, Some("+09:00"));
}

#[test]
fn repeated_truncation_is_a_failure_after_two_attempts() {
    run_case("bounded", "max_tokens", false, 2, None);
}

#[test]
fn other_unusable_replies_and_refusals_are_not_output_limit_retries() {
    run_case("invalid", "end_turn", false, 1, None);
    run_case("refused", "refusal", false, 1, None);
}
