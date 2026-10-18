//! `ebira ask-setup`: a page on this computer (127.0.0.1 only) to choose the service `ebira ask`
//! uses, check and save its key, and choose its models.
//!
//! The page and its script are compiled into the program. A key travels from the page to this
//! process, then to the Windows user environment (HKCU\Environment) and to the service's model
//! list, which costs nothing; the page only ever gets it back masked. The service and the models
//! are saved there too, as the variables `ebira ask` reads when no flag names them. Every API
//! route needs the token from the address `ask-setup` printed, and the Host header must name this
//! address, so other sites open in the browser cannot reach it.

use super::api::{Client, Endpoint};
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

/// The services the page offers: the name `--api` takes, a label, and what a key looks like.
const SERVICES: [(&str, &str, &str); 5] = [
    ("anthropic", "Anthropic (Claude)", "sk-ant-api03-…"),
    ("openai", "OpenAI", "sk-…"),
    ("gemini", "Google Gemini", "AIza…"),
    ("openrouter", "OpenRouter", "sk-or-v1-…"),
    (
        "compatible",
        "Another service or a local server (OpenAI Chat Completions)",
        "none needed for most local servers",
    ),
];

/// The variables `ebira ask` reads when no flag names them. The page saves all but the key
/// variable's name, which it clears: it keeps each key in its service's own variable.
const API_SETTING: &str = "EBIRA_ASK_API";
const URL_SETTING: &str = "EBIRA_ASK_API_URL";
const KEY_SETTING: &str = "EBIRA_ASK_API_KEY_ENV";
const STRONG_SETTING: &str = "EBIRA_ASK_STRONG_MODEL";
const CHEAP_SETTING: &str = "EBIRA_ASK_CHEAP_MODEL";
const SETTINGS: [&str; 5] = [
    API_SETTING,
    URL_SETTING,
    KEY_SETTING,
    STRONG_SETTING,
    CHEAP_SETTING,
];

/// The models `ebira ask` uses on the Claude API unless others are chosen.
const DEFAULT_STRONG: &str = "claude-opus-5-5";
const DEFAULT_CHEAP: &str = "claude-haiku-5-5";

/// Words in the ids of models that do not answer chat requests (embeddings, speech, images,
/// video, moderation), which the page leaves out of the lists it offers.
const NOT_CHAT: [&str; 12] = [
    "embed",
    "tts",
    "whisper",
    "dall-e",
    "image",
    "audio",
    "speech",
    "moderation",
    "transcribe",
    "realtime",
    "video",
    "veo-",
];

pub fn run(scratch: PathBuf, ledger: &Ledger, budget: Option<f64>, open: bool) -> io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = random_token();
    let url = format!("http://127.0.0.1:{port}/?t={token}");
    crate::output::diagnostic(format_args!(
        "The setup page for `ebira ask`: {url}\nOpen it if no browser opens. Close it on the page, \
         or with Ctrl+C; it closes by itself after 30 minutes unused."
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

/// What is saved: the Windows user environment, or, on a computer without one, this process's
/// environment, which the page can show but not change.
struct Saved {
    windows: bool,
    values: Vec<(String, String)>,
}

impl Saved {
    fn read() -> Saved {
        match keys::read_user_variables() {
            Some(values) => Saved {
                windows: true,
                values,
            },
            None => Saved {
                windows: false,
                values: std::env::vars().collect(),
            },
        }
    }

    /// The saved value, or "" when there is none.
    fn get(&self, name: &str) -> String {
        keys::lookup(&self.values, name).unwrap_or_default()
    }

    /// The key `ebira ask` would read from `variable`, and where it is kept: this process's
    /// environment wins over the saved one when it holds another.
    fn key(&self, variable: &str) -> Option<(String, Source)> {
        let here = process_value(variable).unwrap_or_default();
        let saved = self.get(variable);
        if !here.is_empty() && (!self.windows || here != saved) {
            Some((here, Source::Process))
        } else if !saved.is_empty() {
            Some((
                saved,
                if self.windows {
                    Source::WindowsUser
                } else {
                    Source::Process
                },
            ))
        } else {
            None
        }
    }

    /// The settings this process's environment holds otherwise than saved, which `ebira ask`
    /// started from it would use instead.
    fn overridden(&self) -> Vec<&'static str> {
        if !self.windows {
            return Vec::new();
        }
        SETTINGS
            .into_iter()
            .filter(|name| process_value(name).is_some_and(|value| value != self.get(name)))
            .collect()
    }
}

/// The variable as this process has it, trimmed; none when it is not set at all.
fn process_value(name: &str) -> Option<String> {
    std::env::var_os(name).map(|value| value.to_string_lossy().trim().to_string())
}

/// What the page asks to check or save.
struct Choice {
    api: String,
    url: String,
    key: String,
    strong: String,
    cheap: String,
}

impl Choice {
    fn parse(body: &[u8]) -> Choice {
        let request = value::parse(&String::from_utf8_lossy(body)).unwrap_or(Value::Null);
        let field = |name: &str| request.str_at(name).trim().to_string();
        let api = field("api");
        Choice {
            api: if api.is_empty() {
                "anthropic".to_string()
            } else {
                api
            },
            url: field("url"),
            key: field("key"),
            strong: field("strong"),
            cheap: field("cheap"),
        }
    }

    fn label(&self) -> &'static str {
        label(&self.api)
    }

    /// The service, before anything is sent to it.
    fn endpoint(&self) -> Result<Endpoint, String> {
        if self.api == "compatible" && self.url.is_empty() {
            return Err(
                "Enter the server's base URL: the part before /chat/completions, such as \
                 http://localhost:11434/v1."
                    .to_string(),
            );
        }
        let url = (self.api == "compatible").then_some(self.url.as_str());
        Endpoint::named(&self.api, url, None)
    }

    /// Whether the typed key could be one for the service; nothing is sent before it is.
    fn check_key_shape(&self) -> Result<(), &'static str> {
        match (self.key.is_empty(), self.api.as_str()) {
            (true, _) => Ok(()),
            (false, "anthropic") => keys::check_format(&self.key),
            (false, _) => keys::check_other_format(&self.key, false),
        }
    }

    /// The settings to save: a default is saved as nothing, so that `ebira ask` follows the
    /// defaults of later versions.
    fn settings(&self) -> [(&'static str, Option<String>); 5] {
        let anthropic = self.api == "anthropic";
        let model = |id: &str, default: &str| {
            (!id.is_empty() && !(anthropic && id == default)).then(|| id.to_string())
        };
        [
            (API_SETTING, (!anthropic).then(|| self.api.clone())),
            (
                URL_SETTING,
                (self.api == "compatible").then(|| self.url.clone()),
            ),
            (KEY_SETTING, None),
            (STRONG_SETTING, model(&self.strong, DEFAULT_STRONG)),
            (CHEAP_SETTING, model(&self.cheap, DEFAULT_CHEAP)),
        ]
    }

    /// The variables to write, leaving out those that already hold their value: the settings,
    /// and the key when one was typed.
    fn changes(&self, saved: &Saved, key_variable: &str) -> Vec<(String, Option<String>)> {
        let mut changes: Vec<(String, Option<String>)> = Vec::new();
        if !self.key.is_empty() {
            changes.push((key_variable.to_string(), Some(self.key.clone())));
        }
        for (name, value) in self.settings() {
            changes.push((name.to_string(), value));
        }
        changes.retain(|(name, value)| saved.get(name) != value.as_deref().unwrap_or(""));
        changes
    }

    /// For a computer without a Windows user environment: the lines that set the same, with the
    /// key left for the person to fill in.
    fn exports(&self, key_variable: &str) -> String {
        let mut lines: Vec<String> = self
            .settings()
            .into_iter()
            .filter_map(|(name, value)| {
                value.map(|value| format!("export {name}={}", quote(&value)))
            })
            .collect();
        if !(self.api == "compatible" && self.key.is_empty()) {
            lines.push(format!("export {key_variable}=<your key>"));
        }
        lines.join("\n")
    }
}

fn label(api: &str) -> &'static str {
    SERVICES
        .iter()
        .find(|(name, _, _)| *name == api)
        .map(|(_, label, _)| *label)
        .unwrap_or("the service")
}

/// The variable that holds the service's key.
fn key_variable(api: &str) -> String {
    Endpoint::named(api, Some("http://localhost"), None)
        .map(|endpoint| endpoint.key_variable)
        .unwrap_or_else(|_| keys::VARIABLE.to_string())
}

/// A value for a POSIX shell, in single quotes.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The models that answer chat requests, in the order the Claude API lists them (newest first)
/// or, for other services, by name.
fn chat_models(api: &str, mut models: Vec<String>) -> Vec<String> {
    models.retain(|id| {
        let id = id.to_ascii_lowercase();
        !NOT_CHAT.iter().any(|word| id.contains(word))
    });
    if api != "anthropic" {
        models.sort();
    }
    models
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
                let body = &request.body;
                match (method, path) {
                    ("GET", "/api/status") => Reply::json("200 OK", &self.status()),
                    ("POST", "/api/check") => Reply::json("200 OK", &self.check(body)),
                    ("POST", "/api/save") => Reply::json("200 OK", &self.save(body)),
                    ("POST", "/api/delete") => Reply::json("200 OK", &self.delete(body)),
                    ("POST", "/api/reset") => Reply::json("200 OK", &self.reset()),
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

    /// The saved service, its URL and models, each service's key (masked) and where it is kept,
    /// and the spending so far.
    fn status(&self) -> Value {
        let saved = Saved::read();
        let mut api = saved.get(API_SETTING);
        let mut notes = Vec::new();
        if api.is_empty() {
            api = "anthropic".to_string();
        } else if !SERVICES.iter().any(|(name, _, _)| *name == api) {
            notes.push(format!(
                "{API_SETTING} is set to {api}, which `ebira ask` does not know: choose a service \
                 and save."
            ));
            api = "anthropic".to_string();
        }
        let overridden = saved.overridden();
        if !overridden.is_empty() {
            notes.push(format!(
                "This environment sets {}, which `ebira ask` started from it uses instead of what \
                 is saved here.",
                overridden.join(", ")
            ));
        }
        let services: Vec<Value> = SERVICES
            .iter()
            .map(|(name, label, placeholder)| {
                let variable = key_variable(name);
                let key = saved.key(&variable);
                value::object([
                    ("name", value::string(*name)),
                    ("label", value::string(*label)),
                    ("placeholder", value::string(*placeholder)),
                    ("variable", value::string(variable.as_str())),
                    (
                        "masked",
                        value::string(
                            key.as_ref()
                                .map(|(key, _)| keys::mask(key))
                                .unwrap_or_default(),
                        ),
                    ),
                    (
                        "where",
                        value::string(
                            key.as_ref()
                                .map(|(_, source)| source.describe())
                                .unwrap_or_default(),
                        ),
                    ),
                    (
                        "removable",
                        Value::Bool(saved.windows && !saved.get(&variable).is_empty()),
                    ),
                ])
            })
            .collect();
        value::object([
            ("ok", Value::Bool(true)),
            ("windows", Value::Bool(saved.windows)),
            ("api", value::string(api)),
            ("url", value::string(saved.get(URL_SETTING))),
            ("strong", value::string(saved.get(STRONG_SETTING))),
            ("cheap", value::string(saved.get(CHEAP_SETTING))),
            ("default_strong", value::string(DEFAULT_STRONG)),
            ("default_cheap", value::string(DEFAULT_CHEAP)),
            (
                "customized",
                Value::Bool(SETTINGS.iter().any(|name| !saved.get(name).is_empty())),
            ),
            ("services", Value::Array(services)),
            ("notice", value::string(notes.join(" "))),
            (
                "spending",
                value::string(self.ledger.spending().text(self.ledger.path(), self.budget)),
            ),
        ])
    }

    /// Asks the service for its models with the key typed, or with the one `ebira ask` would
    /// use when none is; listing them costs nothing.
    fn list_models(&self, choice: &Choice, saved: &Saved) -> Result<(Vec<String>, String), String> {
        let endpoint = choice.endpoint()?;
        let key = if choice.key.is_empty() {
            match saved.key(&endpoint.key_variable) {
                Some((key, _)) => key,
                None if choice.api == "compatible" => String::new(),
                None => {
                    return Err(format!(
                        "Enter the key: none is saved for {}.",
                        choice.label()
                    ))
                }
            }
        } else {
            choice.key.clone()
        };
        let client = Client::for_key(&endpoint, key.clone(), self.scratch.clone())?;
        let models = client
            .models()
            .map_err(|error| format!("{} did not answer: {error}", choice.label()))?;
        Ok((
            chat_models(&choice.api, models),
            if key.is_empty() {
                String::new()
            } else {
                keys::mask(&key)
            },
        ))
    }

    fn check(&self, body: &[u8]) -> Value {
        let choice = Choice::parse(body);
        if let Err(reason) = choice.check_key_shape() {
            return message(false, &format!("Not saved or sent: {reason}."));
        }
        let saved = Saved::read();
        let (models, masked) = match self.list_models(&choice, &saved) {
            Ok(listed) => listed,
            Err(error) => return message(false, &error),
        };
        let mut text = format!(
            "{} answers{}: {} models to choose from.",
            choice.label(),
            if masked.is_empty() {
                String::new()
            } else {
                format!(" to the key {masked}")
            },
            models.len()
        );
        if choice.api == "anthropic" {
            text.push(' ');
            text.push_str(&defaults_note(&models));
        }
        reply_with_models(true, &text, &models)
    }

    /// Checks the key with the service's model list, then saves the service, its URL, the
    /// models, and the key if one was typed. Nothing is saved when the check fails.
    fn save(&self, body: &[u8]) -> Value {
        let choice = Choice::parse(body);
        if let Err(reason) = choice.check_key_shape() {
            return message(false, &format!("Not saved: {reason}."));
        }
        if let Err(error) = choice.endpoint() {
            return message(false, &error);
        }
        let saved = Saved::read();
        let (models, _) = match self.list_models(&choice, &saved) {
            Ok(listed) => listed,
            Err(error) => return message(false, &format!("Not saved. {error}")),
        };
        if choice.api != "anthropic" && (choice.strong.is_empty() || choice.cheap.is_empty()) {
            return reply_with_models(
                false,
                &format!(
                    "Not saved: choose the strong model and the cheap one from {}'s list (the \
                     same one for both will do).",
                    choice.label()
                ),
                &models,
            );
        }
        let strong = non_empty(&choice.strong, DEFAULT_STRONG);
        let cheap = non_empty(&choice.cheap, DEFAULT_CHEAP);
        let mut text = format!(
            "`ebira ask` will use {}: {strong} to plan and answer, {cheap} to screen and draft.",
            choice.label()
        );
        let unlisted: Vec<&str> = [strong, cheap]
            .into_iter()
            .filter(|id| !models.iter().any(|model| model == id))
            .collect();
        if !unlisted.is_empty() {
            text.push_str(&format!(
                " Not in the service's list for this key: {}.",
                unlisted.join(", ")
            ));
        }
        let key_variable = key_variable(&choice.api);
        if !saved.windows {
            return reply_with_models(
                true,
                &format!(
                    "{text}\nThis computer has no Windows user environment for the page to save \
                     to: add these lines to your shell's profile instead.\n{}",
                    choice.exports(&key_variable)
                ),
                &models,
            );
        }
        let changes = choice.changes(&saved, &key_variable);
        let pairs: Vec<(&str, Option<&str>)> = changes
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_deref()))
            .collect();
        if !pairs.is_empty() {
            if let Err(error) = keys::set_user_variables(&pairs) {
                return message(
                    false,
                    &format!("The check passed, but nothing was saved: {error}"),
                );
            }
        }
        let mut reply = reply_with_models(
            true,
            &format!(
                "Saved in the Windows user environment. {text} Programs already running keep any \
                 older values they hold until they restart."
            ),
            &models,
        );
        if let Value::Object(fields) = &mut reply {
            fields.push(("saved".to_string(), Value::Bool(true)));
        }
        reply
    }

    /// Removes the service's saved key.
    fn delete(&self, body: &[u8]) -> Value {
        let choice = Choice::parse(body);
        let variable = key_variable(&choice.api);
        match keys::set_user_variables(&[(variable.as_str(), None)]) {
            Ok(()) => message(
                true,
                &format!(
                    "Removed {variable} from the Windows user environment. Programs already \
                     running keep the old key until they close."
                ),
            ),
            Err(error) => message(false, &error),
        }
    }

    /// Back to the defaults: the Claude API with its default models. Saved keys stay.
    fn reset(&self) -> Value {
        let pairs: Vec<(&str, Option<&str>)> = SETTINGS.iter().map(|name| (*name, None)).collect();
        match keys::set_user_variables(&pairs) {
            Ok(()) => message(
                true,
                &format!(
                    "Back to the defaults: `ebira ask` uses Anthropic, {DEFAULT_STRONG} and \
                     {DEFAULT_CHEAP}. Saved keys are kept."
                ),
            ),
            Err(error) => message(false, &error),
        }
    }
}

/// Whether the default models are in the Claude API's list for this key.
fn defaults_note(models: &[String]) -> String {
    let missing: Vec<&str> = [DEFAULT_STRONG, DEFAULT_CHEAP]
        .into_iter()
        .filter(|id| !models.iter().any(|model| model == id))
        .collect();
    if missing.is_empty() {
        format!("The defaults, {DEFAULT_STRONG} and {DEFAULT_CHEAP}, are both available.")
    } else {
        format!(
            "Not available to this key: {}; choose others.",
            missing.join(", ")
        )
    }
}

fn non_empty<'a>(value: &'a str, default: &'a str) -> &'a str {
    if value.is_empty() {
        default
    } else {
        value
    }
}

fn message(ok: bool, text: &str) -> Value {
    value::object([("ok", Value::Bool(ok)), ("message", value::string(text))])
}

fn reply_with_models(ok: bool, text: &str, models: &[String]) -> Value {
    value::object([
        ("ok", Value::Bool(ok)),
        ("message", value::string(text)),
        (
            "models",
            Value::Array(models.iter().map(|id| value::string(id.as_str())).collect()),
        ),
    ])
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

    fn saved(values: &[(&str, &str)]) -> Saved {
        Saved {
            windows: true,
            values: values
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        }
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
        for route in ["POST /api/quit", "POST /api/reset", "POST /api/delete"] {
            let no_token = server.route(&request(&format!(
                "{route} HTTP/1.1\r\nHost: 127.0.0.1:4321\r\n\r\n"
            )));
            assert_eq!(no_token.status, "403 Forbidden", "{route}");
            assert!(!no_token.quit);
        }
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
    fn rejects_malformed_keys_and_choices_without_calling_a_service() {
        let ledger = ledger();
        let server = server(&ledger);
        let admin =
            server.save(br#"{"api":"anthropic","key":"sk-ant-admin01-0123456789abcdefghij"}"#);
        assert_eq!(admin.get("ok"), Some(&Value::Bool(false)));
        assert!(admin.str_at("message").contains("Admin key"));
        let spaced = server.check(br#"{"api":"openai","key":"sk proj"}"#);
        assert_eq!(spaced.get("ok"), Some(&Value::Bool(false)));
        let unknown = server.save(br#"{"api":"bedrock","key":"abc"}"#);
        assert!(unknown.str_at("message").contains("must be one of"));
        let no_url = server.save(br#"{"api":"compatible"}"#);
        assert!(no_url
            .str_at("message")
            .contains("Enter the server's base URL"));
    }

    #[test]
    fn saves_defaults_as_nothing_and_writes_only_what_changed() {
        let before = saved(&[
            ("EBIRA_ASK_API", "openai"),
            ("EBIRA_ASK_STRONG_MODEL", "gpt-x"),
            ("EBIRA_ASK_CHEAP_MODEL", "gpt-x-mini"),
        ]);
        let back = Choice::parse(
            br#"{"api":"anthropic","strong":"claude-opus-5-5","cheap":"","url":"ignored"}"#,
        );
        assert_eq!(
            back.changes(&before, "ANTHROPIC_API_KEY"),
            vec![
                ("EBIRA_ASK_API".to_string(), None),
                ("EBIRA_ASK_STRONG_MODEL".to_string(), None),
                ("EBIRA_ASK_CHEAP_MODEL".to_string(), None),
            ]
        );
        let cheaper = Choice::parse(
            br#"{"api":"openai","key":"sk-new","strong":"gpt-x","cheap":"gpt-x-nano"}"#,
        );
        assert_eq!(
            cheaper.changes(&before, "OPENAI_API_KEY"),
            vec![
                ("OPENAI_API_KEY".to_string(), Some("sk-new".to_string())),
                (
                    "EBIRA_ASK_CHEAP_MODEL".to_string(),
                    Some("gpt-x-nano".to_string())
                ),
            ]
        );
        let local = Choice::parse(
            br#"{"api":"compatible","url":"http://localhost:11434/v1","strong":"m","cheap":"m"}"#,
        );
        assert_eq!(
            local.exports("EBIRA_ASK_API_KEY"),
            "export EBIRA_ASK_API='compatible'\nexport EBIRA_ASK_API_URL='http://localhost:11434/v1'\n\
             export EBIRA_ASK_STRONG_MODEL='m'\nexport EBIRA_ASK_CHEAP_MODEL='m'"
        );
        assert_eq!(quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn offers_chat_models_only() {
        let listed = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        assert_eq!(
            chat_models(
                "openai",
                listed(&[
                    "gpt-x",
                    "text-embedding-3-small",
                    "gpt-x-mini-tts",
                    "dall-e-3",
                    "a-model"
                ])
            ),
            listed(&["a-model", "gpt-x"])
        );
        assert_eq!(
            chat_models(
                "anthropic",
                listed(&["claude-opus-5-5", "claude-haiku-5-5"])
            ),
            listed(&["claude-opus-5-5", "claude-haiku-5-5"]),
            "the Claude API's order, newest first, stays"
        );
        assert!(
            defaults_note(&listed(&["claude-opus-5-5", "claude-haiku-5-5"]))
                .contains("both available")
        );
        assert!(defaults_note(&listed(&["claude-haiku-5-5"]))
            .contains("Not available to this key: claude-opus-5-5"));
    }

    #[test]
    fn keys_set_here_win_over_saved_ones_only_when_they_differ() {
        let unset = "EBIRA_SETUP_TEST_UNSET";
        assert_eq!(
            saved(&[(unset, "sk-saved-0123456789abcdef")]).key(unset),
            Some(("sk-saved-0123456789abcdef".to_string(), Source::WindowsUser))
        );
        assert_eq!(saved(&[]).key(unset), None);
        // PATH stands in for a key set in this process.
        let here = process_value("PATH").unwrap();
        assert_eq!(
            saved(&[("PATH", "elsewhere")]).key("PATH"),
            Some((here.clone(), Source::Process))
        );
        assert_eq!(
            saved(&[("PATH", here.as_str())]).key("PATH"),
            Some((here, Source::WindowsUser)),
            "a value inherited from the saved one is the saved one"
        );
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
