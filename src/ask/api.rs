//! The Claude Messages API, or the OpenAI Chat Completions API that many other services and
//! local servers speak, over HTTPS through curl, which Windows 10 and later and most Linux
//! systems ship, so Ebira needs no HTTP library. The API key reaches curl on its standard input:
//! never on a command line, never on disk.

use super::value::{self, Value};
use crate::private_fs;
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const API_VERSION: &str = "2023-06-01";
/// `fallbacks: "default"`: a request a safety classifier declines is run again server-side on a
/// suitable model.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const ATTEMPTS: u32 = 5;

/// How a service is spoken to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// The Claude Messages API.
    Anthropic,
    /// The OpenAI Chat Completions API, which many services and local servers also speak.
    ChatCompletions,
}

/// The names `--api` takes.
pub const APIS: [&str; 5] = ["anthropic", "openai", "gemini", "openrouter", "compatible"];

/// The service `ask` sends its requests to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// One of `APIS`.
    pub name: String,
    pub dialect: Dialect,
    /// The base URL: the Messages API's host, or the path `/chat/completions` sits under.
    pub url: String,
    /// The environment variable that holds the key. A compatible server may take none.
    pub key_variable: String,
    /// Chat Completions: the output limit is named `max_completion_tokens` (OpenAI's reasoning
    /// models refuse `max_tokens`), and the effort is sent as `reasoning_effort`.
    completion_tokens: bool,
    reasoning_effort: bool,
}

impl Endpoint {
    /// The named service, at `url` and with its key in `key_variable` when they are given.
    pub fn named(
        name: &str,
        url: Option<&str>,
        key_variable: Option<&str>,
    ) -> Result<Endpoint, String> {
        let anthropic_url = std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());
        let (dialect, default_url, default_key, completion_tokens, reasoning_effort) = match name {
            "anthropic" => (
                Dialect::Anthropic,
                anthropic_url,
                super::keys::VARIABLE,
                false,
                false,
            ),
            "openai" => (
                Dialect::ChatCompletions,
                "https://api.openai.com/v1".to_string(),
                "OPENAI_API_KEY",
                true,
                true,
            ),
            "gemini" => (
                Dialect::ChatCompletions,
                "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
                "GEMINI_API_KEY",
                false,
                true,
            ),
            "openrouter" => (
                Dialect::ChatCompletions,
                "https://openrouter.ai/api/v1".to_string(),
                "OPENROUTER_API_KEY",
                false,
                false,
            ),
            "compatible" => (
                Dialect::ChatCompletions,
                String::new(),
                "EBIRA_ASK_API_KEY",
                false,
                false,
            ),
            _ => {
                return Err(format!(
                    "--api must be one of {}, got {name}",
                    APIS.join(", ")
                ))
            }
        };
        let url = url
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string)
            .unwrap_or(default_url);
        if url.is_empty() {
            return Err(
                "--api compatible needs --api-url: the base URL that /chat/completions \
                        sits under, such as http://localhost:11434/v1"
                    .to_string(),
            );
        }
        let key_variable = key_variable
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(default_key);
        Ok(Endpoint {
            name: name.to_string(),
            dialect,
            url: url.trim_end_matches('/').to_string(),
            key_variable: key_variable.to_string(),
            completion_tokens,
            reasoning_effort,
        })
    }
}

impl Default for Endpoint {
    /// The Claude API.
    fn default() -> Endpoint {
        Endpoint::named("anthropic", None, None).expect("the Claude API is always named")
    }
}

pub struct Client {
    key: String,
    endpoint: Endpoint,
    scratch: PathBuf,
}

#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub model: &'a str,
    pub effort: &'a str,
    pub max_tokens: u32,
    pub system: &'a str,
    pub user: &'a str,
    /// The JSON schema the reply must follow (structured outputs).
    pub schema: &'a Value,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

pub struct Reply {
    pub output: Value,
    /// The model that served the reply: a fallback model when the first one declined.
    pub model: String,
    pub usage: Usage,
}

#[derive(Debug)]
pub enum ApiError {
    /// Declined by the safety classifiers (after any fallback). The tokens read are billed.
    Refused {
        model: String,
        usage: Usage,
        category: String,
    },
    /// Generation reached the requested output limit. Billed, even without usable JSON.
    Truncated { model: String, usage: Usage },
    /// Not the JSON asked for. Billed.
    Unusable {
        model: String,
        usage: Usage,
        reason: String,
    },
    /// An HTTP error from the API. Not billed.
    Status {
        status: u16,
        message: String,
        request_id: String,
    },
    /// curl could not complete the request.
    Transport(String),
}

impl ApiError {
    /// The model and tokens billed for a failed call, if any.
    pub fn billed(&self) -> Option<(&str, Usage)> {
        match self {
            ApiError::Refused { model, usage, .. }
            | ApiError::Unusable { model, usage, .. }
            | ApiError::Truncated { model, usage } => Some((model, *usage)),
            _ => None,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Refused {
                model, category, ..
            } => write!(
                formatter,
                "{model} declined to answer (category: {}); rephrase the question or name another model",
                if category.is_empty() {
                    "unknown"
                } else {
                    category
                }
            ),
            ApiError::Unusable { model, reason, .. } => {
                write!(formatter, "the reply from {model} could not be used: {reason}")
            }
            ApiError::Truncated { model, .. } => write!(formatter, "the reply from {model} could not be used: the output was cut at max_tokens"),
            ApiError::Status {
                status,
                message,
                request_id,
            } => {
                let what = match status {
                    400 if message.contains("credit balance") => "the credit balance is too low",
                    400 => "the API did not accept the request",
                    401 => "the API key is not valid",
                    403 => "this key may not do that",
                    404 => "the model or the URL was not found",
                    413 => "the request is too large",
                    429 => "the API is busy or a usage limit was reached",
                    529 => "the API is overloaded",
                    _ => "the API returned an error",
                };
                write!(formatter, "{what} (HTTP {status}): {message}")?;
                if !request_id.is_empty() {
                    write!(formatter, " [request-id {request_id}]")?;
                }
                Ok(())
            }
            ApiError::Transport(message) => write!(formatter, "{message}"),
        }
    }
}

impl Client {
    /// The client for `endpoint`, with the key in its variable (this process's environment, else
    /// the Windows user environment). A compatible server, such as a local one, may take no key.
    /// `scratch` holds each request's files while it runs.
    pub fn for_endpoint(endpoint: &Endpoint, scratch: PathBuf) -> Result<Client, String> {
        let key = match super::keys::find_named(&endpoint.key_variable) {
            Some((key, _)) => key,
            None if endpoint.name == "compatible" => String::new(),
            None => return Err(super::keys::missing_named(&endpoint.key_variable)),
        };
        Client::new(endpoint.clone(), key, scratch)
    }

    /// The client for `endpoint` with this key (`ask-setup` checks a key with it).
    pub fn for_key(endpoint: &Endpoint, key: String, scratch: PathBuf) -> Result<Client, String> {
        Client::new(endpoint.clone(), key, scratch)
    }

    fn new(endpoint: Endpoint, key: String, scratch: PathBuf) -> Result<Client, String> {
        if !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(
                "the API key holds unexpected characters (quotes or spaces around it?)".into(),
            );
        }
        private_fs::create_dir_all(&scratch)
            .map_err(|error| format!("cannot create {}: {error}", scratch.display()))?;
        Ok(Client {
            key,
            endpoint,
            scratch,
        })
    }

    /// One request whose reply is JSON following `request.schema`, tried again on overload and
    /// network errors.
    pub fn send(&self, request: &Request) -> Result<Reply, ApiError> {
        if self.endpoint.dialect == Dialect::ChatCompletions {
            return self.send_chat(request);
        }
        let fallbacks = takes_default_fallback(request.model);
        let body = request_body(request, fallbacks);
        let beta = fallbacks.then_some(FALLBACK_BETA);
        let response =
            self.with_retries(|| self.http("POST", "/v1/messages", Some(body.as_str()), beta))?;
        read_reply(&response.body)
    }

    /// A Chat Completions request. Services differ in what they take: one that refuses a part
    /// of the request (the strict schema, the effort, the name of the output limit) is asked
    /// again without it.
    fn send_chat(&self, request: &Request) -> Result<Reply, ApiError> {
        let mut shape = ChatShape {
            schema: true,
            effort: self.endpoint.reasoning_effort,
            completion_tokens: self.endpoint.completion_tokens,
            limit_switched: false,
        };
        loop {
            let body = chat_body(request, shape);
            let error = match self
                .with_retries(|| self.http("POST", "/chat/completions", Some(body.as_str()), None))
            {
                Ok(response) => return read_chat_reply(&response.body),
                Err(error) => error,
            };
            let next = match &error {
                ApiError::Status {
                    status: 400,
                    message,
                    ..
                } => shape.without(message),
                _ => None,
            };
            match next {
                Some(next) => shape = next,
                None => return Err(error),
            }
        }
    }

    /// The ids of the models this key can use. Listing them spends no tokens. Asked once, not
    /// again after a failure: a check should say at once that a server is not answering.
    pub fn models(&self) -> Result<Vec<String>, ApiError> {
        let path = match self.endpoint.dialect {
            Dialect::Anthropic => "/v1/models?limit=1000",
            Dialect::ChatCompletions => "/models",
        };
        let response = match self.http("GET", path, None, None) {
            Ok(response) if response.status == 200 => response,
            Ok(response) => return Err(status_error(&response)),
            Err((_, message)) => return Err(ApiError::Transport(message)),
        };
        let value = value::parse(&response.body)
            .map_err(|error| ApiError::Transport(format!("the model list is not JSON: {error}")))?;
        Ok(value
            .array_at("data")
            .iter()
            .map(|model| model.str_at("id").to_string())
            .filter(|id| !id.is_empty())
            .collect())
    }

    /// A 200 response, trying again with backoff after overload and network failures.
    fn with_retries(
        &self,
        attempt_once: impl Fn() -> Result<Response, (bool, String)>,
    ) -> Result<Response, ApiError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (retry, error, retry_after) = match attempt_once() {
                Ok(response) if response.status == 200 => return Ok(response),
                Ok(response) => (
                    matches!(
                        response.status,
                        408 | 409 | 429 | 500 | 502 | 503 | 504 | 529
                    ),
                    status_error(&response),
                    response.retry_after,
                ),
                Err((retry, message)) => (retry, ApiError::Transport(message), None),
            };
            if !retry || attempt >= ATTEMPTS {
                return Err(error);
            }
            thread::sleep(backoff(attempt, retry_after));
        }
    }

    fn http(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        beta: Option<&str>,
    ) -> Result<Response, (bool, String)> {
        let files = Scratch::new(&self.scratch);
        let not_written =
            |error: std::io::Error| (false, format!("cannot write a request file: {error}"));
        write_private(&files.body, body.unwrap_or("").as_bytes()).map_err(not_written)?;
        write_private(&files.reply, b"").map_err(not_written)?;
        write_private(&files.headers, b"").map_err(not_written)?;

        let mut config = String::new();
        let mut set = |name: &str, value: &str| {
            config.push_str(&format!("{name} = \"{}\"\n", config_quote(value)))
        };
        set("url", &format!("{}{path}", self.endpoint.url));
        set("request", method);
        match self.endpoint.dialect {
            Dialect::Anthropic => {
                set("header", &format!("x-api-key: {}", self.key));
                set("header", &format!("anthropic-version: {API_VERSION}"));
                if let Some(beta) = beta {
                    set("header", &format!("anthropic-beta: {beta}"));
                }
            }
            Dialect::ChatCompletions => {
                if !self.key.is_empty() {
                    set("header", &format!("authorization: Bearer {}", self.key));
                }
            }
        }
        if body.is_some() {
            set("header", "content-type: application/json");
            set("data-binary", &format!("@{}", files.body.display()));
        }
        set("output", &files.reply.display().to_string());
        set("dump-header", &files.headers.display().to_string());
        set("write-out", "%{http_code}");
        set("connect-timeout", "20");
        set("max-time", "900");
        config.push_str("silent\nshow-error\n");

        let mut child = Command::new("curl")
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| (false, format!("cannot start curl: {error}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(config.as_bytes())
                .map_err(|error| (false, format!("cannot pass the request to curl: {error}")))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| (false, format!("curl did not finish: {error}")))?;
        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            // Name resolution, connection, timeouts, TLS, empty or cut replies: worth another try.
            let retry = matches!(code, 5 | 6 | 7 | 16 | 18 | 28 | 35 | 52 | 55 | 56 | 92);
            return Err((
                retry,
                format!(
                    "the API could not be reached (curl exit code {code}): {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        let status = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u16>()
            .map_err(|_| (true, "the reply has no HTTP status".to_string()))?;
        let body = fs::read_to_string(&files.reply)
            .map_err(|error| (true, format!("cannot read the reply: {error}")))?;
        let headers = fs::read_to_string(&files.headers).unwrap_or_default();
        Ok(Response {
            status,
            body,
            retry_after: header(&headers, "retry-after").and_then(|value| value.parse().ok()),
            request_id: header(&headers, "request-id")
                .or_else(|| header(&headers, "x-request-id"))
                .unwrap_or_default(),
        })
    }
}

struct Response {
    status: u16,
    body: String,
    retry_after: Option<u64>,
    request_id: String,
}

/// The request, the reply, and its headers for one call, removed when the call is done.
struct Scratch {
    body: PathBuf,
    reply: PathBuf,
    headers: PathBuf,
}

static NEXT_CALL: AtomicU64 = AtomicU64::new(0);

impl Scratch {
    fn new(directory: &Path) -> Scratch {
        let id = format!(
            "{}-{}",
            std::process::id(),
            NEXT_CALL.fetch_add(1, Ordering::Relaxed)
        );
        Scratch {
            body: directory.join(format!("{id}.request.json")),
            reply: directory.join(format!("{id}.reply.json")),
            headers: directory.join(format!("{id}.headers.txt")),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for path in [&self.body, &self.reply, &self.headers] {
            let _ = fs::remove_file(path);
        }
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    private_fs::create(path)?.write_all(bytes)
}

/// A value inside double quotes in a curl config file.
fn config_quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The value of the last `name:` header (a reply can carry several header blocks).
fn header(headers: &str, name: &str) -> Option<String> {
    headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_string())
        .next_back()
}

fn backoff(attempt: u32, retry_after: Option<u64>) -> Duration {
    let base = 1u64 << attempt.min(6);
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::from(elapsed.subsec_millis()))
        .unwrap_or(0);
    let seconds = match retry_after {
        Some(asked) => asked.max(base).min(120),
        None => base.min(60),
    };
    Duration::from_millis(seconds * 1000 + jitter)
}

/// The models that take `fallbacks: "default"` on the Claude API (Claude Haiku 5.5 has no
/// server-side fallback).
fn takes_default_fallback(model: &str) -> bool {
    model.starts_with("claude-opus-5")
        || model.starts_with("claude-fable-5")
        || model == "claude-sonnet-5-5"
}

fn request_body(request: &Request, fallbacks: bool) -> String {
    let mut fields = vec![
        ("model".to_string(), value::string(request.model)),
        (
            "max_tokens".to_string(),
            value::number(u64::from(request.max_tokens)),
        ),
        ("system".to_string(), value::string(request.system)),
        (
            "messages".to_string(),
            Value::Array(vec![value::object([
                ("role", value::string("user")),
                ("content", value::string(request.user)),
            ])]),
        ),
        (
            "output_config".to_string(),
            value::object([
                ("effort", value::string(request.effort)),
                (
                    "format",
                    value::object([
                        ("type", value::string("json_schema")),
                        ("schema", request.schema.clone()),
                    ]),
                ),
            ]),
        ),
    ];
    if fallbacks {
        fields.push(("fallbacks".to_string(), value::string("default")));
    }
    Value::Object(fields).to_json()
}

fn read_reply(body: &str) -> Result<Reply, ApiError> {
    let reply = value::parse(body)
        .map_err(|error| ApiError::Transport(format!("the reply is not JSON: {error}")))?;
    let model = reply.str_at("model").to_string();
    let usage = read_usage(reply.get("usage"));
    match reply.str_at("stop_reason") {
        "refusal" => {
            let category = reply
                .get("stop_details")
                .map(|details| details.str_at("category"))
                .unwrap_or("");
            return Err(ApiError::Refused {
                model,
                usage,
                category: category.to_string(),
            });
        }
        "max_tokens" => return Err(ApiError::Truncated { model, usage }),
        _ => {}
    }
    let text: String = reply
        .array_at("content")
        .iter()
        .filter(|block| block.str_at("type") == "text")
        .map(|block| block.str_at("text"))
        .collect();
    match value::parse(text.trim()) {
        Ok(output) => Ok(Reply {
            output,
            model,
            usage,
        }),
        Err(error) => Err(ApiError::Unusable {
            model,
            usage,
            reason: format!("the reply is not the JSON asked for ({error})"),
        }),
    }
}

fn read_usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::default();
    };
    Usage {
        input: usage.u64_at("input_tokens"),
        output: usage.u64_at("output_tokens"),
        cache_read: usage.u64_at("cache_read_input_tokens"),
        cache_write: usage.u64_at("cache_creation_input_tokens"),
    }
}

/// What a Chat Completions request holds, as far as services differ in what they take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChatShape {
    /// The reply's shape is asked for as a strict `json_schema`; else as a `json_object`, with
    /// the schema in the instructions.
    schema: bool,
    /// The effort goes as `reasoning_effort`.
    effort: bool,
    /// The output limit is named `max_completion_tokens`; else `max_tokens`.
    completion_tokens: bool,
    /// The other name of the output limit was tried already.
    limit_switched: bool,
}

impl ChatShape {
    /// The shape without the part a refusal of the request (HTTP 400) names; none when it names
    /// none that is left to drop.
    fn without(self, message: &str) -> Option<ChatShape> {
        let message = message.to_ascii_lowercase();
        if self.effort && message.contains("reasoning") {
            return Some(ChatShape {
                effort: false,
                ..self
            });
        }
        if self.schema && (message.contains("response_format") || message.contains("schema")) {
            return Some(ChatShape {
                schema: false,
                ..self
            });
        }
        if !self.limit_switched && message.contains("max_") && message.contains("token") {
            return Some(ChatShape {
                completion_tokens: !self.completion_tokens,
                limit_switched: true,
                ..self
            });
        }
        None
    }
}

fn chat_body(request: &Request, shape: ChatShape) -> String {
    // The schema goes in the instructions as well: some services take `response_format` and
    // silently do nothing with it.
    let system = format!(
        "{}\n\nReply with one JSON object and nothing else. It must follow this JSON schema:\n{}",
        request.system,
        request.schema.to_json()
    );
    let format = if shape.schema {
        value::object([
            ("type", value::string("json_schema")),
            (
                "json_schema",
                value::object([
                    ("name", value::string("reply")),
                    ("strict", Value::Bool(true)),
                    ("schema", request.schema.clone()),
                ]),
            ),
        ])
    } else {
        value::object([("type", value::string("json_object"))])
    };
    let limit = if shape.completion_tokens {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    let mut fields = vec![
        ("model".to_string(), value::string(request.model)),
        (
            "messages".to_string(),
            Value::Array(vec![
                value::object([
                    ("role", value::string("system")),
                    ("content", value::string(system)),
                ]),
                value::object([
                    ("role", value::string("user")),
                    ("content", value::string(request.user)),
                ]),
            ]),
        ),
        (
            limit.to_string(),
            value::number(u64::from(request.max_tokens)),
        ),
        ("response_format".to_string(), format),
    ];
    if shape.effort {
        fields.push((
            "reasoning_effort".to_string(),
            value::string(reasoning_effort(request.effort)),
        ));
    }
    Value::Object(fields).to_json()
}

/// Ebira's effort as `reasoning_effort` takes it.
fn reasoning_effort(effort: &str) -> &'static str {
    match effort {
        "low" => "low",
        "medium" => "medium",
        _ => "high",
    }
}

fn read_chat_reply(body: &str) -> Result<Reply, ApiError> {
    let reply = value::parse(body)
        .map_err(|error| ApiError::Transport(format!("the reply is not JSON: {error}")))?;
    let model = reply.str_at("model").to_string();
    let usage = read_chat_usage(reply.get("usage"));
    let Some(choice) = reply.array_at("choices").first() else {
        return Err(ApiError::Unusable {
            model,
            usage,
            reason: "the reply holds no choice".to_string(),
        });
    };
    let message = choice.get("message");
    let refusal = message
        .map(|message| message.str_at("refusal"))
        .unwrap_or("");
    let finish = choice.str_at("finish_reason");
    if !refusal.is_empty() || finish == "content_filter" {
        return Err(ApiError::Refused {
            model,
            usage,
            category: if refusal.is_empty() {
                finish.to_string()
            } else {
                "refusal".to_string()
            },
        });
    }
    if finish == "length" {
        return Err(ApiError::Truncated { model, usage });
    }
    let text = message.map(message_text).unwrap_or_default();
    match value::parse(without_fence(text.trim())) {
        Ok(output) => Ok(Reply {
            output,
            model,
            usage,
        }),
        Err(error) => Err(ApiError::Unusable {
            model,
            usage,
            reason: format!("the reply is not the JSON asked for ({error})"),
        }),
    }
}

/// A message's text: a string, or the text parts of a list.
fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts.iter().map(|part| part.str_at("text")).collect(),
        _ => String::new(),
    }
}

/// JSON a model wrapped in a Markdown code fence, without the fence.
fn without_fence(text: &str) -> &str {
    let Some(inner) = text.strip_prefix("```") else {
        return text;
    };
    let inner = inner.strip_prefix("json").unwrap_or(inner).trim();
    inner.strip_suffix("```").map(str::trim).unwrap_or(inner)
}

/// Chat Completions usage. The prompt's cached part is counted apart, as Claude's is.
fn read_chat_usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::default();
    };
    let cached = usage
        .get("prompt_tokens_details")
        .map(|details| details.u64_at("cached_tokens"))
        .unwrap_or(0);
    Usage {
        input: usage.u64_at("prompt_tokens").saturating_sub(cached),
        output: usage.u64_at("completion_tokens"),
        cache_read: cached,
        cache_write: 0,
    }
}

fn status_error(response: &Response) -> ApiError {
    let message = value::parse(&response.body)
        .ok()
        .and_then(|reply| {
            reply
                .get("error")
                .map(|error| error.str_at("message").to_string())
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| super::text::clip_head(response.body.trim(), 300));
    ApiError::Status {
        status: response.status,
        message,
        request_id: response.request_id.clone(),
    }
}

struct Price {
    input: f64,
    output: f64,
    cache_read: f64,
}

/// First-party API rates in USD per million tokens, as published in October 2026.
fn price(model: &str, usage: &Usage) -> Option<Price> {
    let rate = |input, output, cache_read| {
        Some(Price {
            input,
            output,
            cache_read,
        })
    };
    match model {
        m if m.starts_with("claude-fable-5-1") || m.starts_with("claude-mythos-5-1") => {
            rate(10.0, 50.0, 0.25)
        }
        m if m.starts_with("claude-fable-5") || m.starts_with("claude-mythos-5") => {
            rate(10.0, 50.0, 1.0)
        }
        m if m.starts_with("claude-opus-5-5") => rate(4.0, 20.0, 0.20),
        m if m.starts_with("claude-opus-5") => rate(5.0, 25.0, 0.50),
        m if ["claude-opus-4-8", "claude-opus-4-7", "claude-opus-4-6"]
            .iter()
            .any(|id| m.starts_with(id)) =>
        {
            rate(5.0, 25.0, 0.50)
        }
        m if m.starts_with("claude-sonnet-5") => rate(2.0, 10.0, 0.20),
        m if m.starts_with("claude-sonnet-4-6") => rate(3.0, 15.0, 0.30),
        // Claude Haiku 5.5 prompts over 100K tokens are billed at five times the rate.
        m if m.starts_with("claude-haiku-5-5")
            && usage.input + usage.cache_read + usage.cache_write > 100_000 =>
        {
            rate(0.50, 2.50, 0.05)
        }
        m if m.starts_with("claude-haiku-5-5") => rate(0.10, 0.50, 0.01),
        m if m.starts_with("claude-haiku-4-5") => rate(1.0, 5.0, 0.10),
        _ => None,
    }
}

/// The estimated price of a call in USD; none for a model this table does not know.
pub fn cost(model: &str, usage: &Usage) -> Option<f64> {
    let price = price(model, usage)?;
    let tokens = usage.input as f64 * price.input
        + usage.output as f64 * price.output
        + usage.cache_read as f64 * price.cache_read
        + usage.cache_write as f64 * price.input * 1.25;
    Some(tokens / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        value::parse(
            r#"{"type":"object","properties":{},"required":[],"additionalProperties":false}"#,
        )
        .unwrap()
    }

    #[test]
    fn builds_a_structured_output_request_with_fallbacks_where_supported() {
        let schema = schema();
        let request = Request {
            model: "claude-opus-5-5",
            effort: "low",
            max_tokens: 4000,
            system: "sys",
            user: "地図の色",
            schema: &schema,
        };
        let body = value::parse(&request_body(
            &request,
            takes_default_fallback(request.model),
        ))
        .unwrap();
        assert_eq!(body.str_at("model"), "claude-opus-5-5");
        assert_eq!(body.u64_at("max_tokens"), 4000);
        assert_eq!(body.array_at("messages")[0].str_at("content"), "地図の色");
        let config = body.get("output_config").unwrap();
        assert_eq!(config.str_at("effort"), "low");
        assert_eq!(config.get("format").unwrap().str_at("type"), "json_schema");
        assert_eq!(body.str_at("fallbacks"), "default");
        assert!(!takes_default_fallback("claude-haiku-5-5"));
        assert!(takes_default_fallback("claude-sonnet-5-5"));
    }

    #[test]
    fn reads_a_reply_after_thinking_blocks() {
        let body = r#"{"model":"claude-opus-5-5","stop_reason":"end_turn","content":[{"type":"thinking","thinking":""},{"type":"text","text":"{\"answer\":\"はい\"}"}],"usage":{"input_tokens":1200,"output_tokens":300,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#;
        let reply = read_reply(body).unwrap();
        assert_eq!(reply.output.str_at("answer"), "はい");
        assert_eq!(reply.usage.input, 1200);
        assert_eq!(reply.model, "claude-opus-5-5");
    }

    #[test]
    fn reports_refusals_and_cut_replies_with_their_usage() {
        let refused = r#"{"model":"claude-opus-5-5","stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber"},"content":[],"usage":{"input_tokens":10,"output_tokens":0}}"#;
        match read_reply(refused) {
            Err(error @ ApiError::Refused { .. }) => {
                assert_eq!(error.billed().unwrap().1.input, 10)
            }
            _ => panic!("expected a refusal"),
        }
        let cut = r#"{"model":"claude-haiku-5-5","stop_reason":"max_tokens","content":[{"type":"text","text":"{\"sc"}],"usage":{"input_tokens":5,"output_tokens":6000}}"#;
        assert!(matches!(read_reply(cut), Err(ApiError::Truncated { .. })));
    }

    #[test]
    fn reads_the_last_header_block() {
        let headers =
            "HTTP/1.1 100 Continue\r\n\r\nHTTP/2 429\r\nretry-after: 7\r\nRequest-Id: req_1\r\n\r\n";
        assert_eq!(header(headers, "retry-after").as_deref(), Some("7"));
        assert_eq!(header(headers, "request-id").as_deref(), Some("req_1"));
        assert_eq!(header(headers, "x-missing"), None);
    }

    #[test]
    fn prices_calls() {
        let usage = Usage {
            input: 1_000_000,
            output: 100_000,
            cache_read: 0,
            cache_write: 0,
        };
        assert!((cost("claude-opus-5-5", &usage).unwrap() - 6.0).abs() < 1e-9);
        let small = Usage {
            input: 50_000,
            output: 1_000,
            ..Usage::default()
        };
        assert!((cost("claude-haiku-5-5", &small).unwrap() - 0.0055).abs() < 1e-9);
        assert!((cost("claude-haiku-5-5", &usage).unwrap() - 0.75).abs() < 1e-9);
        assert_eq!(cost("some-other-model", &usage), None);
    }

    #[test]
    fn quotes_config_values() {
        assert_eq!(config_quote(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn names_each_service_with_its_url_and_key() {
        let openai = Endpoint::named("openai", None, None).unwrap();
        assert_eq!(openai.dialect, Dialect::ChatCompletions);
        assert_eq!(openai.url, "https://api.openai.com/v1");
        assert_eq!(openai.key_variable, "OPENAI_API_KEY");
        let gemini = Endpoint::named("gemini", None, None).unwrap();
        assert_eq!(
            gemini.url,
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
        assert_eq!(gemini.key_variable, "GEMINI_API_KEY");
        let local = Endpoint::named(
            "compatible",
            Some(" http://localhost:11434/v1/ "),
            Some("LOCAL_KEY"),
        )
        .unwrap();
        assert_eq!(local.url, "http://localhost:11434/v1");
        assert_eq!(local.key_variable, "LOCAL_KEY");
        assert!(Endpoint::named("compatible", None, None)
            .unwrap_err()
            .contains("--api-url"));
        assert!(Endpoint::named("bedrock", None, None)
            .unwrap_err()
            .contains("anthropic, openai, gemini, openrouter, compatible"));
        assert_eq!(Endpoint::default().dialect, Dialect::Anthropic);
    }

    fn chat_request(schema: &Value) -> Request<'_> {
        Request {
            model: "small-model",
            effort: "xhigh",
            max_tokens: 6000,
            system: "sys",
            user: "地図の色",
            schema,
        }
    }

    #[test]
    fn builds_a_chat_completions_request_with_a_strict_schema() {
        let schema = schema();
        let shape = ChatShape {
            schema: true,
            effort: true,
            completion_tokens: true,
            limit_switched: false,
        };
        let body = value::parse(&chat_body(&chat_request(&schema), shape)).unwrap();
        assert_eq!(body.str_at("model"), "small-model");
        let messages = body.array_at("messages");
        assert_eq!(messages[0].str_at("role"), "system");
        assert!(messages[0].str_at("content").starts_with("sys\n\n"));
        assert!(
            messages[0]
                .str_at("content")
                .ends_with(&format!("follow this JSON schema:\n{}", schema.to_json())),
            "the schema is in the instructions too, for a service that ignores response_format"
        );
        assert_eq!(messages[1].str_at("content"), "地図の色");
        assert_eq!(body.u64_at("max_completion_tokens"), 6000);
        assert!(body.get("max_tokens").is_none());
        let format = body.get("response_format").unwrap();
        assert_eq!(format.str_at("type"), "json_schema");
        let json_schema = format.get("json_schema").unwrap();
        assert_eq!(json_schema.get("strict"), Some(&Value::Bool(true)));
        assert_eq!(json_schema.get("schema"), Some(&schema));
        assert_eq!(body.str_at("reasoning_effort"), "high");

        let plain = ChatShape {
            schema: false,
            effort: false,
            completion_tokens: false,
            limit_switched: true,
        };
        let body = value::parse(&chat_body(&chat_request(&schema), plain)).unwrap();
        assert_eq!(body.u64_at("max_tokens"), 6000);
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(
            body.get("response_format").unwrap().str_at("type"),
            "json_object"
        );
        assert!(body.array_at("messages")[0]
            .str_at("content")
            .contains("follow this JSON schema"));
    }

    #[test]
    fn drops_what_a_service_refuses_one_part_at_a_time() {
        let full = ChatShape {
            schema: true,
            effort: true,
            completion_tokens: true,
            limit_switched: false,
        };
        let without_effort = full
            .without("Unsupported parameter: 'reasoning_effort' is not supported with this model.")
            .unwrap();
        assert!(!without_effort.effort && without_effort.schema);
        let without_schema = without_effort
            .without("Invalid parameter: 'response_format' of type 'json_schema' is not supported")
            .unwrap();
        assert!(!without_schema.schema);
        let other_limit = without_schema
            .without("Unsupported parameter: 'max_completion_tokens'")
            .unwrap();
        assert!(!other_limit.completion_tokens && other_limit.limit_switched);
        assert_eq!(
            other_limit.without("Unsupported parameter: 'max_tokens'"),
            None
        );
        assert_eq!(full.without("The model `x` does not exist"), None);
    }

    #[test]
    fn reads_a_chat_completions_reply() {
        let body = r#"{"model":"small-model-2026","choices":[{"index":0,"message":{"role":"assistant","content":"{\"answer\":\"はい\"}","refusal":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":1200,"completion_tokens":300,"prompt_tokens_details":{"cached_tokens":1000}}}"#;
        let reply = read_chat_reply(body).unwrap();
        assert_eq!(reply.output.str_at("answer"), "はい");
        assert_eq!(reply.model, "small-model-2026");
        assert_eq!(
            reply.usage,
            Usage {
                input: 200,
                output: 300,
                cache_read: 1000,
                cache_write: 0
            }
        );
        let fenced = r#"{"model":"m","choices":[{"message":{"content":"```json\n{\"answer\":\"ok\"}\n```"},"finish_reason":"stop"}]}"#;
        assert_eq!(
            read_chat_reply(fenced).unwrap().output.str_at("answer"),
            "ok"
        );
        let parts = r#"{"model":"m","choices":[{"message":{"content":[{"type":"text","text":"{\"answer\":"},{"type":"text","text":"\"ok\"}"}]},"finish_reason":"stop"}]}"#;
        assert_eq!(
            read_chat_reply(parts).unwrap().output.str_at("answer"),
            "ok"
        );
    }

    #[test]
    fn reports_chat_refusals_cut_replies_and_other_text() {
        let refused = r#"{"model":"m","choices":[{"message":{"content":null,"refusal":"I can't help with that."},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
        match read_chat_reply(refused) {
            Err(error @ ApiError::Refused { .. }) => {
                assert_eq!(error.billed().unwrap().1.input, 10)
            }
            _ => panic!("expected a refusal"),
        }
        let filtered = r#"{"model":"m","choices":[{"message":{"content":""},"finish_reason":"content_filter"}]}"#;
        assert!(matches!(
            read_chat_reply(filtered),
            Err(ApiError::Refused { category, .. }) if category == "content_filter"
        ));
        let cut = r#"{"model":"m","choices":[{"message":{"content":"{\"sc"},"finish_reason":"length"}],"usage":{"prompt_tokens":5,"completion_tokens":6000}}"#;
        assert!(matches!(
            read_chat_reply(cut),
            Err(ApiError::Truncated { .. })
        ));
        let prose =
            r#"{"model":"m","choices":[{"message":{"content":"Sure!"},"finish_reason":"stop"}]}"#;
        assert!(matches!(
            read_chat_reply(prose),
            Err(ApiError::Unusable { .. })
        ));
        assert!(matches!(
            read_chat_reply(r#"{"model":"m","choices":[]}"#),
            Err(ApiError::Unusable { .. })
        ));
    }
}
