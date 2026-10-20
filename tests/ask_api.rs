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

/// The settings `ebira ask-setup` saves. Set to nothing here, they keep what may be saved on
/// this computer out of the tests: the defaults apply.
const SETTINGS: [&str; 5] = [
    "EBIRA_ASK_API",
    "EBIRA_ASK_API_URL",
    "EBIRA_ASK_API_KEY_ENV",
    "EBIRA_ASK_STRONG_MODEL",
    "EBIRA_ASK_CHEAP_MODEL",
];

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
    for name in SETTINGS {
        command.env(name, "");
    }
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
            "--no-sweep",
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

const SCREEN: &str = "You screen search results";
const SCREENED: &str = r#"{"scores":[{"id":"C1","score":3}]}"#;
const ANSWER: &str =
    r#"{"answer":"Remove it entirely.","findings":[],"timeline":[],"gaps":[],"more_terms":[]}"#;

/// A Chat Completions reply holding `content`.
fn chat(model: &str, content: &str) -> String {
    format!(
        r#"{{"model":"{model}","choices":[{{"index":0,"message":{{"role":"assistant","content":{}}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":10,"completion_tokens":2}}}}"#,
        quoted(content)
    )
}

/// A loopback stand-in for a service that speaks the Chat Completions API: it keeps each
/// request's head and body, and `reply` gives the status and body of each response.
struct ChatService {
    url: String,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    done: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl ChatService {
    fn start(reply: impl Fn(&str) -> (u16, String) + Send + 'static) -> ChatService {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let done = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (server_done, received) = (done.clone(), requests.clone());
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
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
                let head = String::from_utf8_lossy(&data[..header_end]).to_string();
                let body =
                    String::from_utf8(data[header_end..header_end + length].to_vec()).unwrap();
                let (status, reply) = reply(&body);
                received.lock().unwrap().push((head, body));
                let reason = if status == 200 { "OK" } else { "Bad Request" };
                write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).unwrap();
            }
        });
        ChatService {
            url,
            requests,
            done,
            server: Some(server),
        }
    }

    /// Stops the service; each request's head and body, in the order they came.
    fn finish(mut self) -> Vec<(String, String)> {
        self.done.store(true, Ordering::SeqCst);
        self.server.take().unwrap().join().unwrap();
        let requests = self.requests.lock().unwrap().clone();
        requests
    }
}

/// Synthetic logs synced into a corpus, for the Chat Completions cases.
fn synced(name: &str) -> (Logs, PathBuf) {
    synced_log(
        name,
        concat!(
            r#"{"type":"user","sessionId":"s","timestamp":"2026-09-01T00:00:00Z","message":{"role":"user","content":"keep remote access"}}"#,
            "\n",
            r#"{"type":"user","sessionId":"s","timestamp":"2026-09-02T00:00:00Z","message":{"role":"user","content":"LATEST_DECISION: remove it entirely"}}"#,
            "\n",
        ),
    )
}

/// `log`, a Claude Code transcript, synced into a corpus.
fn synced_log(name: &str, log: &str) -> (Logs, PathBuf) {
    let logs =
        Logs(std::env::temp_dir().join(format!("ebira-ask-api-{name}-{}", std::process::id())));
    let source = logs.0.join(".claude/projects/example/session.jsonl");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, log).unwrap();
    let corpus = logs.0.join("corpus");
    let sync = ebira(&logs.0)
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
    (logs, corpus)
}

fn ask_compatible(logs: &Logs, corpus: &Path, url: &str) -> std::process::Output {
    ask_compatible_about(logs, corpus, url, "What is decided?", "remote")
}

/// Asks through the compatible service with the strings alone: the sweep is off, so a test
/// of another part of `ask` counts the calls that part makes.
fn ask_compatible_about(
    logs: &Logs,
    corpus: &Path,
    url: &str,
    question: &str,
    term: &str,
) -> std::process::Output {
    ask_compatible_with(logs, corpus, url, question, term, &["--no-sweep"])
}

fn ask_compatible_with(
    logs: &Logs,
    corpus: &Path,
    url: &str,
    question: &str,
    term: &str,
    extra: &[&str],
) -> std::process::Output {
    let mut command = ebira(&logs.0);
    command.env("EBIRA_TEST_KEY", "test-key").args([
        "ask",
        question,
        "--api",
        "compatible",
        "--api-url",
        url,
        "--api-key-env",
        "EBIRA_TEST_KEY",
        "--strong-model",
        "strong-model",
        "--cheap-model",
        "cheap-model",
        "--term",
        term,
        "--keep",
        "1",
        "--corpus",
        corpus.to_str().unwrap(),
    ]);
    command.args(extra);
    command.output().unwrap()
}

/// The sweep's screen, as the request names it.
const SWEEP: &str = "You screen a person's own messages";

#[test]
fn the_sweep_screens_every_message_of_the_persons_and_reads_what_bears_on_the_question() {
    // The string finds the first decision; the one that replaced it holds no search string
    // and is found by the sweep alone, which screens every message the person wrote.
    let (logs, corpus) = synced_log(
        "sweep",
        concat!(
            r#"{"type":"user","sessionId":"s","timestamp":"2026-09-15T00:00:00Z","message":{"role":"user","content":"ログインはメールとパスワードで"}}"#,
            "
",
            r#"{"type":"assistant","sessionId":"s","timestamp":"2026-09-15T00:10:00Z","message":{"role":"assistant","content":[{"type":"text","text":"実装します。"}]}}"#,
            "
",
            r#"{"type":"user","sessionId":"s","timestamp":"2026-09-15T01:00:00Z","message":{"role":"user","content":"パスワード管理したくないから、メールにリンク送る方式にしよ"}}"#,
            "
",
        ),
    );
    let service = ChatService::start(|body| {
        if body.contains(SWEEP) {
            (
                200,
                chat(
                    "cheap-model",
                    r#"{"scores":[{"id":"C1","score":2},{"id":"C2","score":3}]}"#,
                ),
            )
        } else if body.contains(SCREEN) {
            (
                200,
                chat("cheap-model", r#"{"scores":[{"id":"C1","score":2}]}"#),
            )
        } else {
            (
                200,
                chat(
                    "strong-model",
                    r#"{"answer":"メールにリンクを送る方式です。","findings":[{"evidence":"E2","kind":"decision","quote":"メールにリンク送る方式にしよ","note":""}],"timeline":[],"gaps":[],"more_terms":[]}"#,
                ),
            )
        }
    });
    let output = ask_compatible_with(
        &logs,
        &corpus,
        &service.url,
        "ログイン方式は何になった？",
        "ログイン",
        &["--keep", "2"],
    );
    let requests = service.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sweeps: Vec<&String> = requests
        .iter()
        .map(|(_, body)| body)
        .filter(|body| body.contains(SWEEP))
        .collect();
    assert_eq!(sweeps.len(), 1, "one sweep of the person's two messages");
    let first = sweeps[0].find("C1 | 2026-09-15 00:00 | ");
    let second = sweeps[0].find("C2 | 2026-09-15 01:00 | ");
    assert!(
        first.is_some()
            && second.is_some()
            && first < second
            && sweeps[0].contains("ログインはメールとパスワードで")
            && sweeps[0].contains("パスワード管理したくないから"),
        "both messages, oldest first: {}",
        sweeps[0]
    );
    let answers: Vec<&String> = requests
        .iter()
        .map(|(_, body)| body)
        .filter(|body| !body.contains(SWEEP) && !body.contains(SCREEN))
        .collect();
    assert_eq!(answers.len(), 2, "the draft and the answer");
    for body in &answers {
        assert!(
            body.contains(
                "Every message the person wrote in the scope of this question (2 messages, "
            ),
            "the answer model is told what was screened: {body}"
        );
        assert!(
            body.contains("メールにリンク送る方式にしよ"),
            "the message the string did not find reaches it: {body}"
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with(r#"{"disposition":"answered","mode":"ask","api":"compatible","#),
        "{stdout}"
    );
    assert!(
        stdout.contains(r#""quote":"メールにリンク送る方式にしよ","quote_verified":true,"relevance":"","author":"person""#),
        "the quote is checked against the record the sweep found: {stdout}"
    );
    assert!(
        stdout.contains(r#""sweep":{"in_scope":2,"screened":2,"relevant":2,"threes":1,"twos":1,"#),
        "the sweep's counts: {stdout}"
    );
    assert!(
        stdout.contains(r#""coverage":{"sessions_in_scope":1,"sessions_with_evidence":1,"sessions_unread":0,"unread_swept":true,"unread":[]}"#),
        "the coverage: {stdout}"
    );
}

#[test]
fn another_api_is_spoken_to_as_chat_completions() {
    let (logs, corpus) = synced("chat");
    let service = ChatService::start(|body| {
        let model = if body.contains(r#""model":"strong-model""#) {
            "strong-model"
        } else {
            "cheap-model"
        };
        if body.contains(SCREEN) {
            (200, chat(model, SCREENED))
        } else {
            (200, chat(model, ANSWER))
        }
    });
    let output = ask_compatible(&logs, &corpus, &service.url);
    let requests = service.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!requests.is_empty());
    for (head, body) in &requests {
        assert!(
            head.starts_with("POST /v1/chat/completions HTTP/1.1"),
            "{head}"
        );
        let head = head.to_ascii_lowercase();
        assert!(head.contains("authorization: bearer test-key"), "{head}");
        assert!(!head.contains("anthropic-version"), "{head}");
        assert!(
            body.contains(r#""response_format":{"type":"json_schema","json_schema":{"name":"reply","strict":true"#),
            "{body}"
        );
        assert!(body.contains(r#""role":"system""#), "{body}");
    }
    assert!(requests
        .iter()
        .filter(|(_, body)| body.contains(SCREEN))
        .all(|(_, body)| body.contains(r#""model":"cheap-model""#)));
    assert!(
        requests
            .iter()
            .any(|(_, body)| !body.contains(SCREEN) && body.contains(r#""model":"strong-model""#)),
        "the strong model writes the answer"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(r#""api":"compatible""#), "{stdout}");
    assert!(stdout.contains("Remove it entirely."), "{stdout}");
    assert!(
        !stdout.contains(r#""unpriced_calls":0"#),
        "calls to a model without a known price are counted as unpriced: {stdout}"
    );
}

#[test]
fn settings_in_the_environment_choose_the_service_without_flags() {
    let (logs, corpus) = synced("chat-settings");
    let service = ChatService::start(|body| {
        let model = if body.contains(r#""model":"strong-model""#) {
            "strong-model"
        } else {
            "cheap-model"
        };
        if body.contains(SCREEN) {
            (200, chat(model, SCREENED))
        } else {
            (200, chat(model, ANSWER))
        }
    });
    let output = ebira(&logs.0)
        .env("EBIRA_ASK_API", "compatible")
        .env("EBIRA_ASK_API_URL", &service.url)
        .env("EBIRA_ASK_STRONG_MODEL", "strong-model")
        .env("EBIRA_ASK_CHEAP_MODEL", "cheap-model")
        .env("EBIRA_ASK_API_KEY", "saved-key")
        .args([
            "ask",
            "What is decided?",
            "--term",
            "remote",
            "--keep",
            "1",
            "--corpus",
            corpus.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let requests = service.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|(head, _)| head
        .to_ascii_lowercase()
        .contains("authorization: bearer saved-key")));
    assert!(
        requests
            .iter()
            .any(|(_, body)| !body.contains(SCREEN) && body.contains(r#""model":"strong-model""#)),
        "the strong model writes the answer"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(r#""api":"compatible""#));
}

#[test]
fn a_reply_without_the_fields_asked_for_is_not_an_answer() {
    let (logs, corpus) = synced("chat-empty");
    let service = ChatService::start(|body| {
        if body.contains(SCREEN) {
            (200, chat("cheap-model", SCREENED))
        } else {
            (200, chat("cheap-model", "{}"))
        }
    });
    let output = ask_compatible(&logs, &corpus, &service.url);
    let requests = service.finish();
    assert!(!requests.is_empty());
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not have the shape asked for: reply has no answer"),
        "{stderr}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("\"answered\""));
}

#[test]
fn a_service_without_strict_schemas_is_asked_for_a_json_object() {
    let (logs, corpus) = synced("chat-object");
    let service = ChatService::start(|body| {
        if body.contains("json_schema") {
            return (
                400,
                r#"{"error":{"message":"Invalid parameter: response_format of type json_schema is not supported with this model."}}"#.to_string(),
            );
        }
        if body.contains(SCREEN) {
            (200, chat("cheap-model", SCREENED))
        } else {
            (200, chat("strong-model", ANSWER))
        }
    });
    let output = ask_compatible(&logs, &corpus, &service.url);
    let requests = service.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let objects: Vec<&(String, String)> = requests
        .iter()
        .filter(|(_, body)| body.contains(r#""response_format":{"type":"json_object"}"#))
        .collect();
    assert!(!objects.is_empty());
    assert!(
        objects
            .iter()
            .all(|(_, body)| body.contains("follow this JSON schema")),
        "the schema moves into the instructions"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Remove it entirely."));
}

#[test]
fn when_none_is_judged_relevant_the_closest_are_read_and_said_not_to_settle_it() {
    // The search finds the agent's reply; the person's wish before it is what bears on the
    // question, and the screen, which sees the reply alone, judges it unrelated.
    let (logs, corpus) = synced_log(
        "closest",
        concat!(
            r#"{"type":"user","sessionId":"s","timestamp":"2026-09-18T01:40:00Z","message":{"role":"user","content":"そのうち crates.io に出したいな"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","timestamp":"2026-09-18T01:41:00Z","message":{"role":"assistant","content":[{"type":"text","text":"公開するときは README と LICENSE を整えましょう。"}]}}"#,
            "\n",
        ),
    );
    let service = ChatService::start(|body| {
        if body.contains(SCREEN) {
            (
                200,
                chat("cheap-model", r#"{"scores":[{"id":"C1","score":1}]}"#),
            )
        } else {
            (
                200,
                chat(
                    "strong-model",
                    r#"{"answer":"公開した記録はありません。","findings":[{"evidence":"E1","kind":"other","quote":"そのうち crates.io に出したいな","note":""}],"timeline":[],"gaps":[],"more_terms":[]}"#,
                ),
            )
        }
    });
    let output = ask_compatible_about(&logs, &corpus, &service.url, "いつ公開した？", "公開");
    let requests = service.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let answers: Vec<&String> = requests
        .iter()
        .map(|(_, body)| body)
        .filter(|body| !body.contains(SCREEN))
        .collect();
    assert_eq!(answers.len(), 2, "the draft and the answer");
    for body in answers {
        assert!(
            body.contains(
                "No record the search found (1 in all) was judged relevant to the question"
            ),
            "the answer model is told: {body}"
        );
        assert!(
            body.contains("そのうち crates.io に出したいな"),
            "the record before the one found reaches it: {body}"
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with(concat!(
            r#"{"disposition":"nothing_relevant","mode":"ask","api":"compatible","question":"いつ公開した？","#,
            r#""not_settled":"The record found does not settle the question: the one record found by the string searched was not judged relevant to it. The answer reads it, with the records around it. Searched: 公開.","#,
            r#""answer":"公開した記録はありません。""#
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(r#""quote":"そのうち crates.io に出したいな","quote_verified":true,"relevance":"","author":"person""#),
        "the quote is checked, and credited to the person: {stdout}"
    );
}

#[test]
fn when_no_record_holds_the_strings_it_says_so_and_spends_nothing() {
    let (logs, corpus) = synced("nothing");
    let service = ChatService::start(|_| (200, chat("cheap-model", ANSWER)));
    let output = ask_compatible_about(
        &logs,
        &corpus,
        &service.url,
        "What is decided?",
        "no such words",
    );
    let requests = service.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requests.is_empty(), "nothing to screen or to answer from");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with(concat!(
            r#"{"disposition":"no_records","mode":"ask","api":"compatible","question":"What is decided?","#,
            r#""not_settled":"The records do not settle the question: no record holds the string searched. Searched: no such words.","answer":null,"#
        )),
        "{stdout}"
    );
}

#[test]
fn another_api_needs_its_models_named_before_anything_is_sent() {
    let root = std::env::temp_dir().join(format!("ebira-ask-api-models-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let logs = Logs(root);
    let output = ebira(&logs.0)
        .env("OPENAI_API_KEY", "sk-test")
        .args([
            "ask",
            "What is decided?",
            "--api",
            "openai",
            "--term",
            "remote",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "--api openai needs its models named: --strong-model <id> and --cheap-model <id>"
        ) && stderr.contains("choose them once with `ebira ask-setup`"),
        "{stderr}"
    );
    let unknown = ebira(&logs.0)
        .args(["ask", "q", "--api", "bedrock", "--term", "x"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("--api must be one of"));
}

/// Logs of both apps synced into one corpus: each `(path under the home, log)`.
fn synced_logs(name: &str, logs_by_path: &[(&str, &str)]) -> (Logs, PathBuf) {
    let logs =
        Logs(std::env::temp_dir().join(format!("ebira-ask-api-{name}-{}", std::process::id())));
    let corpus = logs.0.join("corpus");
    let mut command = ebira(&logs.0);
    command.arg("sync");
    for (path, log) in logs_by_path {
        let source = logs.0.join(path);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, log).unwrap();
        command.args(["--source", source.to_str().unwrap()]);
    }
    let sync = command
        .args(["--corpus", corpus.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        sync.status.success(),
        "{}",
        String::from_utf8_lossy(&sync.stderr)
    );
    (logs, corpus)
}

#[test]
fn the_sweep_can_read_each_message_with_the_agents_words_and_screen_the_turns_replies() {
    // A short agreement takes its meaning from the agent's words around it, in either app's
    // log; Codex writes each message and reply twice, and the copies do not split the turn.
    let claude = concat!(
        r#"{"type":"user","sessionId":"c","timestamp":"2026-09-15T00:00:00Z","message":{"role":"user","content":"地図どれ使う？"}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"c","timestamp":"2026-09-15T00:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Leaflet がおすすめです。"}]}}"#,
        "\n",
        r#"{"type":"user","sessionId":"c","timestamp":"2026-09-15T00:05:00Z","message":{"role":"user","content":"それでいこう"}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"c","timestamp":"2026-09-15T00:06:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"npm i leaflet"}}]}}"#,
        "\n",
        r#"{"type":"user","sessionId":"c","timestamp":"2026-09-15T00:07:00Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"added 1 package"}]}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"c","timestamp":"2026-09-15T00:08:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Leaflet で実装しました。"}]}}"#,
        "\n",
        r#"{"type":"user","sessionId":"c","timestamp":"2026-09-15T00:10:00Z","message":{"role":"user","content":"次はボタンの色"}}"#,
        "\n",
    );
    let codex = concat!(
        r#"{"timestamp":"2026-09-16T00:00:00.000Z","type":"session_meta","payload":{"id":"x","timestamp":"2026-09-16T00:00:00.000Z","cwd":"/work/tabi","originator":"codex_cli_rs","cli_version":"0.60.0","source":"cli"}}"#,
        "\n",
        r#"{"timestamp":"2026-09-16T00:00:00.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
        "\n",
        r#"{"timestamp":"2026-09-16T00:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"タイルはどこから取る？"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-09-16T00:00:01.000Z","type":"event_msg","payload":{"type":"user_message","message":"タイルはどこから取る？","images":[]}}"#,
        "\n",
        r#"{"timestamp":"2026-09-16T00:01:00.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"MapLibre がおすすめです。"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-09-16T00:01:00.000Z","type":"event_msg","payload":{"type":"agent_message","message":"MapLibre がおすすめです。"}}"#,
        "\n",
        r#"{"timestamp":"2026-09-16T00:05:00.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}"#,
        "\n",
        r#"{"timestamp":"2026-09-16T00:05:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"OK それ採用"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-09-16T00:05:01.000Z","type":"event_msg","payload":{"type":"user_message","message":"OK それ採用","images":[]}}"#,
        "\n",
        r#"{"timestamp":"2026-09-16T00:08:00.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"MapLibre で実装しました。"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-09-16T00:08:00.000Z","type":"event_msg","payload":{"type":"agent_message","message":"MapLibre で実装しました。"}}"#,
        "\n",
    );
    let (logs, corpus) = synced_logs(
        "turns",
        &[
            (".claude/projects/C--work-tabi/c.jsonl", claude),
            (
                ".codex/sessions/2026/09/16/rollout-2026-09-16T00-00-00-x.jsonl",
                codex,
            ),
        ],
    );
    let service = ChatService::start(|body| {
        if body.contains(SWEEP) {
            (
                200,
                chat(
                    "cheap-model",
                    r#"{"scores":[{"id":"C1","score":2},{"id":"C2","score":3},{"id":"C3","score":0},{"id":"C4","score":2},{"id":"C5","score":3}]}"#,
                ),
            )
        } else if body.contains(SCREEN) {
            (
                200,
                chat(
                    "cheap-model",
                    r#"{"scores":[{"id":"C1","score":3},{"id":"C2","score":3},{"id":"C3","score":3}]}"#,
                ),
            )
        } else {
            (200, chat("strong-model", ANSWER))
        }
    });
    let output = ask_compatible_with(
        &logs,
        &corpus,
        &service.url,
        "地図ライブラリは何にした？",
        "地図",
        &["--sweep-context", "--turn-replies", "--evidence-only"],
    );
    let requests = service.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sweep = requests
        .iter()
        .map(|(_, body)| body)
        .find(|body| body.contains(SWEEP))
        .expect("a sweep");
    for shown in [
        "それでいこう [agent before] …Leaflet がおすすめです。 [agent after] Leaflet で実装しました。",
        "OK それ採用 [agent before] …MapLibre がおすすめです。 [agent after] MapLibre で実装しました。",
        "次はボタンの色 [agent before] …Leaflet で実装しました。",
    ] {
        assert!(sweep.contains(shown), "{shown} in the sweep: {sweep}");
    }
    assert!(
        sweep.contains("C5 | ") && !sweep.contains("C6 | "),
        "five messages, Codex's copies once: {sweep}"
    );
    let turn_screen = requests
        .iter()
        .map(|(_, body)| body)
        .find(|body| body.contains(SCREEN) && body.contains("MapLibre で実装しました。"))
        .expect("the relevant turns' replies are screened without a search string");
    assert_eq!(
        turn_screen.matches("MapLibre で実装しました。").count(),
        1,
        "a reply Codex wrote twice is screened once: {turn_screen}"
    );
    let others = requests
        .iter()
        .filter(|(_, body)| !body.contains(SWEEP) && !body.contains(SCREEN))
        .count();
    assert_eq!(others, 1, "the draft, and no final answer");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with(r#"{"disposition":"evidence","mode":"ask","#),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "Screened 4 of the agent's replies in the turns of the person's relevant messages; 3 judged relevant"
        ) && stdout.contains(r#""score":3,"author":"agent"}"#),
        "the replies of the four relevant turns, one each: {stdout}"
    );
    assert!(
        stdout.contains(r#""digest":false"#) && stdout.contains(r#""sources":{"#),
        "the evidence it would read, and where each source is: {stdout}"
    );
}
