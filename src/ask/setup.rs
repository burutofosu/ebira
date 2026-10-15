//! `ebira ask-setup`: a page on this computer (127.0.0.1 only) to check, save, or remove the API
//! key `ebira ask` uses.
//!
//! The page and its script are compiled into the program. The key travels from the page to this
//! process, then to the Windows user environment (HKCU\Environment) and to the API's model list,
//! which costs nothing; the page only ever gets it back masked. Every API route needs the token
//! from the address `ask-setup` printed, and the Host header must name this address, so other
//! sites open in the browser cannot reach it.

use super::api::Client;
use super::keys::{self, Source};
use super::usage::Ledger;
use super::value::{self, Value};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PAGE: &str = include_str!("setup/page.html");
const SCRIPT: &str = include_str!("setup/page.js");
const IDLE_LIMIT_SECONDS: u64 = 30 * 60;
const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;
const SECURITY_HEADERS: &str = "Cache-Control: no-store\r\n\
X-Content-Type-Options: nosniff\r\n\
Referrer-Policy: no-referrer\r\n\
X-Frame-Options: DENY\r\n\
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; connect-src 'self'; \
img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n";

/// The models `ebira ask` uses by default, and what each does.
const DEFAULT_MODELS: [(&str, &str); 2] = [
    ("claude-opus-5-5", "search strings and answers"),
    ("claude-haiku-5-5", "screening and drafts"),
];

pub fn run(scratch: PathBuf, ledger: &Ledger, budget: Option<f64>, open: bool) -> io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = random_token();
    let url = format!("http://127.0.0.1:{port}/?t={token}");
    crate::output::diagnostic(format_args!(
        "The API key page: {url}\nOpen it if no browser opens. Close it on the page, or with Ctrl+C; \
         it closes by itself after 30 minutes unused."
    ));
    if open {
        open_browser(&url);
    }
    listener.set_nonblocking(true)?;
    let server = Server {
        port,
        token,
        scratch,
        ledger,
        budget,
        quit: AtomicBool::new(false),
        last_seen: AtomicU64::new(now()),
    };
    let mut ended = "closed";
    thread::scope(|scope| {
        while !server.quit.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let server = &server;
                    scope.spawn(move || server.handle(stream));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if now().saturating_sub(server.last_seen.load(Ordering::SeqCst))
                        > IDLE_LIMIT_SECONDS
                    {
                        ended = "idle";
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => {
                    ended = "failed";
                    break;
                }
            }
        }
    });
    crate::output::write_line(
        &crate::json::Object::new()
            .name("disposition", ended)
            .name("mode", "ask_setup")
            .finish(),
    )
}

struct Server<'a> {
    port: u16,
    token: String,
    scratch: PathBuf,
    ledger: &'a Ledger,
    budget: Option<f64>,
    quit: AtomicBool,
    last_seen: AtomicU64,
}

struct HttpRequest {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

struct Reply {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    quit: bool,
}

impl Reply {
    fn new(status: &'static str, content_type: &'static str, body: Vec<u8>) -> Reply {
        Reply {
            status,
            content_type,
            body,
            quit: false,
        }
    }

    fn text(status: &'static str, text: &str) -> Reply {
        Reply::new(
            status,
            "text/plain; charset=utf-8",
            text.as_bytes().to_vec(),
        )
    }

    fn json(status: &'static str, value: &Value) -> Reply {
        Reply::new(
            status,
            "application/json; charset=utf-8",
            value.to_json().into_bytes(),
        )
    }
}

impl Server<'_> {
    fn handle(&self, mut stream: TcpStream) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let Ok(request) = read_request(&mut stream) else {
            return;
        };
        self.last_seen.store(now(), Ordering::SeqCst);
        let reply = self.route(&request);
        let _ = write_reply(&mut stream, &reply);
        if reply.quit {
            self.quit.store(true, Ordering::SeqCst);
        }
    }

    fn own_address(&self, value: &str, with_scheme: bool) -> bool {
        let scheme = if with_scheme { "http://" } else { "" };
        value == format!("{scheme}127.0.0.1:{}", self.port)
            || value == format!("{scheme}localhost:{}", self.port)
    }

    fn route(&self, request: &HttpRequest) -> Reply {
        // A page from another site that resolves its own name to 127.0.0.1 still sends its Host.
        if !self.own_address(request.header("host").unwrap_or(""), false) {
            return Reply::text("403 Forbidden", "This page opens from 127.0.0.1 only.");
        }
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/") if query_value(&request.query, "t") == Some(self.token.as_str()) => {
                Reply::new(
                    "200 OK",
                    "text/html; charset=utf-8",
                    PAGE.as_bytes().to_vec(),
                )
            }
            ("GET", "/") => Reply::text(
                "403 Forbidden",
                "The token in the address does not match: open the address `ebira ask-setup` printed.",
            ),
            ("GET", "/page.js") => Reply::new(
                "200 OK",
                "text/javascript; charset=utf-8",
                SCRIPT.as_bytes().to_vec(),
            ),
            (method, path) if path.starts_with("/api/") => {
                let origin_ok = request
                    .header("origin")
                    .is_none_or(|origin| self.own_address(origin, true));
                if request.header("x-ebira-token") != Some(self.token.as_str()) || !origin_ok {
                    return Reply::json(
                        "403 Forbidden",
                        &message(false, "The page's token does not match: run `ebira ask-setup` again."),
                    );
                }
                match (method, path) {
                    ("GET", "/api/status") => Reply::json("200 OK", &self.status()),
                    ("POST", "/api/check") => Reply::json("200 OK", &self.check(&request.body, false)),
                    ("POST", "/api/save") => Reply::json("200 OK", &self.check(&request.body, true)),
                    ("POST", "/api/delete") => Reply::json("200 OK", &self.delete()),
                    ("POST", "/api/quit") => {
                        let mut reply = Reply::json("200 OK", &message(true, "Closed."));
                        reply.quit = true;
                        reply
                    }
                    _ => Reply::json("404 Not Found", &message(false, "Unknown request.")),
                }
            }
            _ => Reply::text("404 Not Found", "Not found."),
        }
    }

    /// The saved key (masked), where it is kept, and the spending so far.
    fn status(&self) -> Value {
        let (windows, saved) = match keys::query_user_variable(keys::VARIABLE) {
            Some(saved) => (true, saved.map(|key| (key, Source::WindowsUser))),
            None => (false, keys::find()),
        };
        let place = match (&saved, windows) {
            (Some((_, source)), _) => source.describe(),
            (None, true) => Source::WindowsUser.describe(),
            (None, false) => "nowhere this page can save to: set ANTHROPIC_API_KEY yourself",
        };
        value::object([
            ("ok", Value::Bool(true)),
            ("saved", Value::Bool(saved.is_some())),
            (
                "masked",
                value::string(
                    saved
                        .as_ref()
                        .map(|(key, _)| keys::mask(key))
                        .unwrap_or_default(),
                ),
            ),
            ("where", value::string(place)),
            ("windows", Value::Bool(windows)),
            (
                "spending",
                value::string(self.ledger.spending().text(self.ledger.path(), self.budget)),
            ),
        ])
    }

    /// Checks a key (the one given, or the saved one when none is) with the model list, which
    /// costs nothing, and saves it if asked.
    fn check(&self, body: &[u8], save: bool) -> Value {
        let given = value::parse(&String::from_utf8_lossy(body))
            .map(|request| request.str_at("key").trim().to_string())
            .unwrap_or_default();
        let key = if given.is_empty() && !save {
            match keys::read_user_variable(keys::VARIABLE)
                .or_else(|| keys::find().map(|(key, _)| key))
            {
                Some(key) => key,
                None => return message(false, "No key is saved yet."),
            }
        } else {
            given
        };
        if let Err(reason) = keys::check_format(&key) {
            return message(false, reason);
        }
        let client = match Client::with_key(key.clone(), self.scratch.clone()) {
            Ok(client) => client,
            Err(error) => return message(false, &error),
        };
        let models = match client.models() {
            Ok(models) => models,
            Err(error) => return message(false, &format!("The key could not be checked: {error}")),
        };
        let defaults: Vec<Value> = DEFAULT_MODELS
            .iter()
            .map(|(id, role)| {
                value::object([
                    ("id", value::string(*id)),
                    ("role", value::string(*role)),
                    (
                        "available",
                        Value::Bool(models.iter().any(|model| model == id)),
                    ),
                ])
            })
            .collect();
        let mut ok = true;
        let mut text = format!(
            "The API answers to this key ({} models available).",
            models.len()
        );
        if save {
            match keys::set_user_variable(keys::VARIABLE, &key) {
                Ok(()) => text.push_str(
                    " Saved as the Windows user environment variable ANTHROPIC_API_KEY: `ebira ask` \
                     reads it now, and other programs once they start again.",
                ),
                Err(error) => {
                    ok = false;
                    text = format!("The key works, but it was not saved: {error}");
                }
            }
        }
        value::object([
            ("ok", Value::Bool(ok)),
            ("message", value::string(text)),
            ("defaults", Value::Array(defaults)),
            ("masked", value::string(keys::mask(&key))),
        ])
    }

    fn delete(&self) -> Value {
        match keys::remove_user_variable(keys::VARIABLE) {
            Ok(()) => message(
                true,
                "Removed from the Windows user environment. Programs already running keep the old \
                 key until they close.",
            ),
            Err(error) => message(false, &error),
        }
    }
}

fn message(ok: bool, text: &str) -> Value {
    value::object([("ok", Value::Bool(ok)), ("message", value::string(text))])
}

fn read_request(stream: &mut impl Read) -> Result<HttpRequest, String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > MAX_HEAD {
            return Err("the request head is too large".to_string());
        }
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("the connection closed".to_string());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let target = first.next().unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err("the request body is too large".to_string());
    }
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(HttpRequest {
        method,
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body,
    })
}

fn write_reply(stream: &mut impl io::Write, reply: &Reply) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{SECURITY_HEADERS}Connection: close\r\n\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&reply.body)?;
    stream.flush()
}

fn query_value<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

/// 128 bits from the operating system's random source, which seeds every `RandomState`.
fn random_token() -> String {
    (0..2)
        .map(|_| {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u128(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or(0),
            );
            format!("{:016x}", hasher.finish())
        })
        .collect()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// From WSL the Windows side opens the page (explorer.exe hands an address to the default
/// browser); on Windows `start` does; elsewhere `xdg-open` or `open`.
fn open_browser(url: &str) {
    let candidates: &[(&str, &[&str])] = if cfg!(windows) {
        &[("cmd", &["/C", "start", ""])]
    } else {
        &[("explorer.exe", &[]), ("xdg-open", &[]), ("open", &[])]
    };
    for (program, leading) in candidates {
        let started = Command::new(program)
            .args(*leading)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if started.is_ok() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(ledger: &Ledger) -> Server<'_> {
        Server {
            port: 4321,
            token: "abc123".to_string(),
            scratch: std::env::temp_dir(),
            ledger,
            budget: None,
            quit: AtomicBool::new(false),
            last_seen: AtomicU64::new(0),
        }
    }

    fn request(text: &str) -> HttpRequest {
        read_request(&mut text.as_bytes()).unwrap()
    }

    fn ledger() -> Ledger {
        Ledger::new(
            std::env::temp_dir()
                .join("ebira-ask-setup-test")
                .join("usage.tsv"),
        )
    }

    #[test]
    fn reads_a_request_with_a_body() {
        let parsed = request(
            "POST /api/check?x=1 HTTP/1.1\r\nHost: 127.0.0.1:4321\r\nContent-Length: 11\r\nX-Ebira-Token: abc123\r\n\r\n{\"key\":\"\"}x",
        );
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.path, "/api/check");
        assert_eq!(parsed.query, "x=1");
        assert_eq!(parsed.header("x-ebira-token"), Some("abc123"));
        assert_eq!(parsed.body, b"{\"key\":\"\"}x");
    }

    #[test]
    fn guards_the_page_and_the_routes() {
        let ledger = ledger();
        let server = server(&ledger);
        let page = server.route(&request(
            "GET /?t=abc123 HTTP/1.1\r\nHost: 127.0.0.1:4321\r\n\r\n",
        ));
        assert_eq!(page.status, "200 OK");
        let wrong_token = server.route(&request(
            "GET /?t=nope HTTP/1.1\r\nHost: 127.0.0.1:4321\r\n\r\n",
        ));
        assert_eq!(wrong_token.status, "403 Forbidden");
        let rebound = server.route(&request(
            "GET /?t=abc123 HTTP/1.1\r\nHost: evil.example:4321\r\n\r\n",
        ));
        assert_eq!(rebound.status, "403 Forbidden");
        let no_token = server.route(&request(
            "POST /api/quit HTTP/1.1\r\nHost: 127.0.0.1:4321\r\n\r\n",
        ));
        assert_eq!(no_token.status, "403 Forbidden");
        assert!(!no_token.quit);
        let foreign = server.route(&request(
            "POST /api/quit HTTP/1.1\r\nHost: 127.0.0.1:4321\r\nOrigin: http://evil.example\r\nX-Ebira-Token: abc123\r\n\r\n",
        ));
        assert_eq!(foreign.status, "403 Forbidden");
        let quit = server.route(&request(
            "POST /api/quit HTTP/1.1\r\nHost: localhost:4321\r\nOrigin: http://localhost:4321\r\nX-Ebira-Token: abc123\r\n\r\n",
        ));
        assert_eq!(quit.status, "200 OK");
        assert!(quit.quit);
    }

    #[test]
    fn rejects_malformed_keys_without_calling_the_api() {
        let ledger = ledger();
        let reply =
            server(&ledger).check(br#"{"key":"sk-ant-admin01-0123456789abcdefghij"}"#, true);
        assert_eq!(reply.get("ok"), Some(&Value::Bool(false)));
        assert!(reply.str_at("message").contains("Admin key"));
    }

    #[test]
    fn the_page_loads_its_script_from_this_server() {
        assert!(PAGE.contains("<script src=\"/page.js\"></script>"));
        assert!(!PAGE.contains("onclick"));
        assert!(SCRIPT.contains("X-Ebira-Token"));
    }

    #[test]
    fn writes_security_headers() {
        let mut out = Vec::new();
        write_reply(&mut out, &Reply::text("200 OK", "x")).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Content-Security-Policy: default-src 'none'"));
        assert!(text.contains("Content-Length: 1\r\n"));
        assert!(text.ends_with("\r\n\r\nx"));
    }

    #[test]
    fn tokens_are_long_and_differ() {
        let first = random_token();
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, random_token());
    }
}
