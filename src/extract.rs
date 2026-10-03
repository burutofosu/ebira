//! Versioned local extraction API with a selection-independent scoring cache.
use crate::{extract_inputs, output, private_fs};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn emit(value: Value) -> io::Result<()> {
    output::write_line(&value.to_string())
}

fn object_keys(v: &Value, allowed: &[&str], label: &str) -> io::Result<()> {
    let obj = v
        .as_object()
        .ok_or_else(|| invalid(&format!("{label} must be an object")))?;
    if let Some(key) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid(&format!("unsupported {label} field: {key}")));
    }
    Ok(())
}
fn validate(r: &Value) -> io::Result<()> {
    object_keys(
        r,
        &[
            "version",
            "scope",
            "context",
            "targets",
            "questions",
            "select",
            "runtime",
            "cache",
        ],
        "request",
    )?;
    object_keys(
        &r["scope"],
        &[
            "all",
            "sessions",
            "source_ids",
            "include_children",
            "event_ids",
        ],
        "scope",
    )?;
    if let Some(c) = r.get("context") {
        object_keys(c, &["mode"], "context")?;
        if c.get("mode")
            .is_some_and(|v| !matches!(v.as_str(), Some("event" | "paired")))
        {
            return Err(invalid("context.mode must be event or paired"));
        }
    }
    if let Some(t) = r.get("targets") {
        object_keys(t, &["kinds", "senders"], "targets")?;
        for (key, allowed) in [
            (
                "kinds",
                &[
                    "user",
                    "assistant",
                    "command",
                    "output",
                    "patch",
                    "summary",
                    "unknown",
                    "invalid",
                ][..],
            ),
            (
                "senders",
                &[
                    "human",
                    "agent",
                    "system",
                    "summary",
                    "assistant",
                    "unknown",
                ][..],
            ),
        ] {
            if let Some(v) = t.get(key) {
                if !v.as_array().is_some_and(|a| {
                    a.iter()
                        .all(|v| v.as_str().is_some_and(|s| allowed.contains(&s)))
                }) {
                    return Err(invalid("targets requires arrays of known kinds/senders"));
                }
            }
        }
    }
    if r.get("cache").is_some_and(|v| !v.is_boolean()) {
        return Err(invalid("cache must be boolean"));
    }
    object_keys(
        &r["runtime"],
        &[
            "backend",
            "model_path",
            "device",
            "max_tokens",
            "worker_path",
            "python",
            "timeout_seconds",
            "answers",
        ],
        "runtime",
    )?;
    if r["runtime"]
        .get("timeout_seconds")
        .is_some_and(|v| !v.as_u64().is_some_and(|s| (1..=3600).contains(&s)))
    {
        return Err(invalid("timeout_seconds must be 1..3600"));
    }

    if r["version"] != 1 {
        return Err(invalid("extract requires version: 1"));
    }
    let qs = r["questions"]
        .as_array()
        .ok_or_else(|| invalid("questions must be an array"))?;
    if qs.is_empty() || qs.len() > 64 {
        return Err(invalid("questions must contain 1..64 entries"));
    }
    let mut ids = BTreeSet::new();
    for q in qs {
        object_keys(q, &["id", "type", "question", "choices"], "question")?;
        let id = q["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("question id required"))?;
        if !ids.insert(id) {
            return Err(invalid("duplicate question id"));
        }
        if q["question"].as_str().filter(|s| !s.is_empty()).is_none() {
            return Err(invalid("question text required"));
        }
        match q["type"].as_str() {
            Some("noul") => {
                if q.get("choices").is_some() {
                    return Err(invalid(
                        "choices is supported for choice and score questions",
                    ));
                }
            }
            Some("choice" | "score") => {
                if q["choices"]
                    .as_array()
                    .filter(|a| {
                        a.len() >= 2
                            && a.iter().all(|v| v.as_str().is_some_and(|s| !s.is_empty()))
                            && a.iter()
                                .map(Value::to_string)
                                .collect::<BTreeSet<_>>()
                                .len()
                                == a.len()
                    })
                    .is_none()
                {
                    return Err(invalid("choice/score requires at least two string choices"));
                }
            }
            _ => return Err(invalid("question type must be noul, choice, or score")),
        }
    }
    if let Some(select) = r.get("select") {
        let conditions = select
            .as_array()
            .ok_or_else(|| invalid("select must be an array of AND conditions"))?;
        for c in conditions {
            object_keys(c, &["question", "op", "value"], "select condition")?;
            if !ids.contains(c["question"].as_str().unwrap_or("")) {
                return Err(invalid("select references unknown question"));
            }
            let q = qs.iter().find(|q| q["id"] == c["question"]).unwrap();
            if q["type"] == "choice" {
                if c["op"] != "eq" || !q["choices"].as_array().unwrap().contains(&c["value"]) {
                    return Err(invalid(
                        "choice selection requires eq and a declared choice",
                    ));
                }
            } else if !c["value"].is_number() {
                return Err(invalid("numeric selection requires a number"));
            }
            if !matches!(c["op"].as_str(), Some("gte" | "lte" | "eq")) || c.get("value").is_none() {
                return Err(invalid("select requires op gte/lte/eq and value"));
            }
        }
    }
    if !matches!(r["runtime"]["backend"].as_str(), Some("clef" | "mock")) {
        return Err(invalid("runtime.backend must explicitly be clef or mock"));
    }
    if r["runtime"]["backend"] == "mock" && !r["runtime"]["answers"].is_object() {
        return Err(invalid("mock runtime requires an answers object"));
    }
    Ok(())
}

fn selected(answers: &Value, conditions: &Value) -> bool {
    conditions.as_array().is_none_or(|cs| {
        cs.iter().all(|c| {
            let a = &answers[c["question"].as_str().unwrap_or("")];
            let value = match a["type"].as_str() {
                Some("noul") => &a["noul"],
                Some("score") => &a["score"],
                Some("choice") => &a["choice"],
                _ => &Value::Null,
            };
            match c["op"].as_str() {
                Some("eq") => {
                    if a["type"] == "choice" {
                        value == &c["value"]
                    } else {
                        value
                            .as_f64()
                            .zip(c["value"].as_f64())
                            .is_some_and(|(a, b)| a == b)
                    }
                }
                Some("gte") => value
                    .as_f64()
                    .zip(c["value"].as_f64())
                    .is_some_and(|(a, b)| a >= b),
                Some("lte") => value
                    .as_f64()
                    .zip(c["value"].as_f64())
                    .is_some_and(|(a, b)| a <= b),
                _ => false,
            }
        })
    })
}
fn valid_answers(answers: &Value, qs: &Value) -> bool {
    qs.as_array().unwrap().iter().all(|q| {
        let a = &answers[q["id"].as_str().unwrap()];
        if a["type"] != q["type"] {
            return false;
        }
        match q["type"].as_str().unwrap() {
            "noul" => a["noul"].as_f64().is_some_and(|v| (0.0..=1.0).contains(&v)),
            "score" => a["score"].as_f64().is_some_and(|v| {
                v >= 0.0 && v <= (q["choices"].as_array().unwrap().len() - 1) as f64
            }),
            "choice" => q["choices"].as_array().unwrap().contains(&a["choice"]),
            _ => false,
        }
    })
}

struct Worker {
    child: Child,
    input: Sender<String>,
    output: Receiver<io::Result<String>>,
    timeout: Duration,
}
impl Worker {
    fn start(runtime: &Value) -> io::Result<Self> {
        let script = runtime["worker_path"]
            .as_str()
            .ok_or_else(|| invalid("clef requires local runtime.worker_path"))?;
        if !Path::new(script).is_file() {
            return Err(invalid("worker_path must be an existing local file"));
        }
        let mut child = Command::new(runtime["python"].as_str().unwrap_or("python3"))
            .arg(script)
            .env("HF_HUB_OFFLINE", "1")
            .env("TRANSFORMERS_OFFLINE", "1")
            .env("HF_HUB_DISABLE_TELEMETRY", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let (input, pending) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for request in pending {
                if writeln!(stdin, "{request}")
                    .and_then(|_| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, output) = mpsc::channel();
        std::thread::spawn(move || loop {
            let mut line = String::new();
            match (&mut stdout).take(1024 * 1024).read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if !line.ends_with('\n') {
                        let _ = tx.send(Err(invalid("worker response exceeds limit")));
                        break;
                    }
                    if tx.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        });
        let timeout = Duration::from_secs(
            runtime["timeout_seconds"]
                .as_u64()
                .unwrap_or(300)
                .clamp(1, 3600),
        );
        Ok(Self {
            child,
            input,
            output,
            timeout,
        })
    }
    fn call(&mut self, request: Value) -> io::Result<Value> {
        self.input
            .send(request.to_string())
            .map_err(|_| invalid("worker input closed"))?;
        let line = match self.output.recv_timeout(self.timeout) {
            Ok(result) => result?,
            Err(_) => {
                let _ = self.child.kill();
                return Err(invalid("worker timed out or exited"));
            }
        };
        let v: Value = serde_json::from_str(&line)
            .map_err(|_| invalid("invalid or missing worker response"))?;
        if v["id"] != request["id"] {
            return Err(invalid("worker response id mismatch"));
        }
        Ok(v)
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn run(root: &Path, request_path: &str) -> io::Result<()> {
    let mut request_text = String::new();
    if request_path == "-" {
        io::stdin()
            .take(4 * 1024 * 1024 + 1)
            .read_to_string(&mut request_text)?;
    } else {
        fs::File::open(request_path)?
            .take(4 * 1024 * 1024 + 1)
            .read_to_string(&mut request_text)?;
    }
    if request_text.len() > 4 * 1024 * 1024 {
        return Err(invalid("request exceeds 4 MiB limit"));
    }
    let r: Value =
        serde_json::from_str(&request_text).map_err(|_| invalid("invalid request JSON"))?;
    validate(&r)?;
    let inputs = extract_inputs::gather(root, &r["scope"], &r["context"])?;
    let inputs: Vec<_> = inputs
        .into_iter()
        .filter(|i| {
            [("kinds", "kind"), ("senders", "sender")]
                .iter()
                .all(|(key, field)| {
                    r["targets"][key]
                        .as_array()
                        .is_none_or(|a| a.is_empty() || a.contains(&i.metadata[field]))
                })
        })
        .collect();
    let runtime = &r["runtime"];
    let mock = runtime["backend"] == "mock";
    let mut worker = if mock {
        None
    } else {
        Some(Worker::start(runtime)?)
    };
    let identity = if let Some(w) = worker.as_mut() {
        let reply = w.call(json!({"id":"identity","op":"identity","runtime":runtime}))?;
        if reply.get("error").is_some_and(|e| !e.is_null()) {
            emit(json!({"type":"error","error":reply["error"]}))?;
            return Err(invalid("local worker identity failed"));
        }
        reply["model_identity"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("worker omitted model_identity"))?
            .to_string()
    } else {
        format!("MOCK:{}", digest(runtime.to_string().as_bytes()))
    };
    let cache_dir = root.join("extract-cache-v1");
    let cache = r["cache"].as_bool().unwrap_or(true);
    if cache {
        private_fs::create_dir_all(&cache_dir)?;
    }
    let mut scored = 0;
    let mut matched = 0;
    let mut cached = 0;
    let mut unscored = 0;
    emit(
        json!({"type":"progress","version":1,"total":inputs.len(),"scored":0,"backend":runtime["backend"],"model_identity":identity}),
    )?;
    for (index, input) in inputs.iter().enumerate() {
        if input.metadata["input_complete"] == false {
            unscored += 1;
            emit(
                json!({"type":"item","event_id":input.event_id,"status":"unscored","metadata":input.metadata,"error":"incomplete_input"}),
            )?;
            continue;
        }
        let key = digest(json!({"api":1,"code":include_str!("extract.rs"),"inputs_code":include_str!("extract_inputs.rs"),"input":input.state,"metadata":input.metadata,"questions":r["questions"],"identity":identity,"runtime":runtime}).to_string().as_bytes());
        let path = cache_dir.join(format!("{key}.json"));
        let stored = if cache {
            fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .filter(|v| {
                    v["model_identity"] == identity && valid_answers(&v["answers"], &r["questions"])
                })
        } else {
            None
        };
        let hit = stored.is_some();
        let reply = match stored {
            Some(v) => { cached += 1; v },
            None if mock => json!({"answers":runtime["answers"],"model_identity":identity}),
            None => match worker.as_mut().unwrap().call(json!({"id":input.event_id,"op":"score","state":input.state,"questions":r["questions"],"runtime":runtime})) {
                Ok(v) => v, Err(_) => json!({"error":"worker_io_error"}),
            },
        };
        if reply.get("error").is_some_and(|v| !v.is_null())
            || reply["model_identity"] != identity
            || !valid_answers(&reply["answers"], &r["questions"])
        {
            unscored += 1;
            emit(
                json!({"type":"item","event_id":input.event_id,"status":"unscored","metadata":input.metadata,"error":reply.get("error").cloned().unwrap_or(json!("invalid_answers"))}),
            )?;
        } else {
            if cache && !hit {
                let mut f = private_fs::create(&path)?;
                f.write_all(reply.to_string().as_bytes())?;
            }
            scored += 1;
            let pass = selected(&reply["answers"], &r["select"]);
            if pass {
                matched += 1;
            }
            emit(
                json!({"type":"item","event_id":input.event_id,"status":"scored","selected":pass,"answers":reply["answers"],"input_tokens":reply["input_tokens"],"cached":hit,"metadata":input.metadata}),
            )?;
        }
        emit(
            json!({"type":"progress","processed":index+1,"total":inputs.len(),"scored":scored,"unscored":unscored}),
        )?;
    }
    emit(
        json!({"type":"done","version":1,"coverage":{"candidates":inputs.len(),"scored":scored,"unscored":unscored,"selected":matched,"cached":cached},"complete":unscored==0,"backend":runtime["backend"],"model_identity":identity}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn thresholds_and_exclusion_are_independent() {
        let a = json!({"relevant":{"type":"noul","noul":0.8},"exclude":{"type":"noul","noul":0.1}});
        assert!(selected(
            &a,
            &json!([{"question":"relevant","op":"gte","value":0.7},{"question":"exclude","op":"lte","value":0.2}])
        ));
        assert!(!selected(
            &a,
            &json!([{"question":"relevant","op":"gte","value":0.9}])
        ));
    }
    #[test]
    fn validation_rejects_unknown_question_types() {
        assert!(validate(&json!({"version":1,"questions":[{"id":"x","question":"X?","type":"bool"}],"runtime":{"backend":"mock","answers":{}}})).is_err());
    }
}
