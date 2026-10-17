//! Logs compressed with Zstandard (`.jsonl.zst`, as Codex keeps older threads) are read as the
//! JSONL they hold: the same records, at the same offsets, under the same source id.

use std::path::{Path, PathBuf};
use std::process::Command;

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

const SESSION: &str = "01a0f6c2-0000-7000-8000-00000000c0de";

/// A Codex thread: its opening record, a message from the person, and a reply.
fn rollout() -> String {
    [
        format!(
            r#"{{"timestamp":"2026-09-20T01:00:00.000Z","type":"session_meta","payload":{{"id":"{SESSION}","cwd":"/work/app","originator":"codex_desktop"}}}}"#
        ),
        r#"{"timestamp":"2026-09-20T01:00:05.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"keep the retry budget at three attempts"}]}}"#.to_string(),
        r#"{"timestamp":"2026-09-20T01:00:09.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"The retry budget stays at three attempts."}]}}"#.to_string(),
        String::new(),
    ]
    .join("\n")
}

/// `data` as one Zstandard frame of raw blocks, a form every decoder reads, with a block
/// boundary after each offset in `splits`.
fn compressed_in_blocks(data: &[u8], splits: &[usize]) -> Vec<u8> {
    let mut out = vec![0x28, 0xB5, 0x2F, 0xFD, 0xA0];
    out.extend((data.len() as u32).to_le_bytes());
    let mut bounds = splits.to_vec();
    bounds.push(data.len());
    let mut start = 0;
    for (index, end) in bounds.iter().enumerate() {
        let block = &data[start..*end];
        let last = u32::from(index + 1 == bounds.len());
        out.extend(&(last | (block.len() as u32) << 3).to_le_bytes()[..3]);
        out.extend_from_slice(block);
        start = *end;
    }
    out
}

fn compressed(data: &[u8]) -> Vec<u8> {
    compressed_in_blocks(data, &[])
}

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("ebira-compressed-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create test root");
    root
}

/// A day's directory of Codex sessions, laid out as Codex lays them out.
fn logs(root: &Path) -> PathBuf {
    let logs = root
        .join(".codex")
        .join("sessions")
        .join("2026")
        .join("09")
        .join("20");
    std::fs::create_dir_all(&logs).expect("create log directory");
    logs
}

fn log_name() -> String {
    format!("rollout-2026-09-20T01-00-00-{SESSION}.jsonl")
}

fn run(args: &[&str]) -> String {
    let home = std::env::temp_dir().join("ebira-test-empty-home");
    let output = Command::new(EBIRA)
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("EBIRA_CORPUS")
        .env_remove("EBIRA_TZ_OFFSET")
        .output()
        .expect("ebira runs");
    assert!(
        output.status.success(),
        "ebira {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("ebira writes UTF-8")
}

fn text(path: &Path) -> &str {
    path.to_str().expect("test path is UTF-8")
}

/// The value of the first `"key":` in a JSON result: a string's contents or a number's digits.
fn value<'a>(json: &'a str, key: &str) -> &'a str {
    let start = json
        .find(&format!("\"{key}\":"))
        .unwrap_or_else(|| panic!("{key} in {json}"))
        + key.len()
        + 3;
    let rest = &json[start..];
    if let Some(string) = rest.strip_prefix('"') {
        &string[..string.find('"').expect("string end")]
    } else {
        &rest[..rest.find([',', '}']).expect("number end")]
    }
}

/// The first match's source reference: its source id, path, start, and length.
fn first_match(json: &str) -> [String; 4] {
    let at = json
        .find("\"source_ref\":{")
        .unwrap_or_else(|| panic!("a match in {json}"));
    let reference = &json[at..];
    ["source_id", "source_path", "byte_start", "byte_len"]
        .map(|key| value(reference, key).to_string())
}

fn context(corpus: &str, reference: &[String; 4], more: &[&str]) -> String {
    let [source_id, _, byte_start, byte_len] = reference;
    let mut args = vec![
        "context",
        "--corpus",
        corpus,
        "--source-id",
        source_id,
        "--byte-start",
        byte_start,
        "--byte-len",
        byte_len,
    ];
    args.extend_from_slice(more);
    run(&args)
}

#[test]
fn a_compressed_rollout_is_read_like_its_jsonl() {
    let root = root("read");
    let logs = logs(&root);
    std::fs::write(
        logs.join(format!("{}.zst", log_name())),
        compressed(rollout().as_bytes()),
    )
    .expect("write compressed log");
    let corpus = root.join("corpus");
    let corpus = text(&corpus);
    let sync = run(&["sync", "--corpus", corpus, "--source", text(&logs)]);
    assert_eq!(value(&sync, "disposition"), "complete", "{sync}");
    assert_eq!(value(&sync, "sources_seen"), "1", "{sync}");

    let said = run(&["said", "--corpus", corpus, "--format", "text"]);
    assert!(
        said.contains("keep the retry budget at three attempts"),
        "{said}"
    );
    assert!(said.contains(&format!("session={SESSION}")), "{said}");

    let found = run(&["search", "--corpus", corpus, "--query", "stays at three"]);
    assert_eq!(value(&found, "total_candidates"), "1", "{found}");
    let reference = first_match(&found);
    let lines = rollout();
    let lines = lines.lines().collect::<Vec<_>>();
    assert_eq!(
        reference[2],
        (lines[0].len() + lines[1].len() + 2).to_string(),
        "offsets are offsets in the JSONL"
    );
    assert_eq!(reference[3], (lines[2].len() + 1).to_string());

    let raw = context(corpus, &reference, &[]);
    assert_eq!(value(&raw, "disposition"), "ready", "{raw}");
    assert_eq!(value(&raw, "source_freshness"), "current", "{raw}");
    assert!(
        raw.contains(&lines[2].replace('"', "\\\"")),
        "the original record: {raw}"
    );
    let readable = context(corpus, &reference, &["--before", "1", "--format", "text"]);
    assert!(readable.contains("keep the retry budget"), "{readable}");
    assert!(readable.contains("The retry budget stays"), "{readable}");

    let original = run(&[
        "search",
        "--corpus",
        corpus,
        "--query",
        "three attempts",
        "--raw",
    ]);
    assert_eq!(value(&original, "total_candidates"), "2", "{original}");
    assert_eq!(value(&original, "unreachable_sources"), "0", "{original}");

    let brief = run(&[
        "resume",
        "--corpus",
        corpus,
        "--session",
        SESSION,
        "--brief",
    ]);
    assert!(
        brief.contains("The retry budget stays at three attempts."),
        "{brief}"
    );

    let status = run(&["status", "--corpus", corpus]);
    assert_eq!(value(&status, "complete_sources"), "1", "{status}");
    assert_eq!(value(&status, "incomplete_sources"), "0", "{status}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_log_compressed_after_a_sync_keeps_its_source_id_and_references() {
    let root = root("compressed-later");
    let logs = logs(&root);
    let plain = logs.join(log_name());
    std::fs::write(&plain, rollout()).expect("write log");
    let corpus = root.join("corpus");
    let corpus = text(&corpus);
    run(&["sync", "--corpus", corpus, "--source", text(&logs)]);
    let before = first_match(&run(&[
        "search",
        "--corpus",
        corpus,
        "--query",
        "stays at three",
    ]));

    // Codex compresses the thread, and the plain log goes.
    std::fs::write(
        logs.join(format!("{}.zst", log_name())),
        compressed(rollout().as_bytes()),
    )
    .expect("write compressed log");
    std::fs::remove_file(&plain).expect("remove plain log");
    let sync = run(&["sync", "--corpus", corpus]);
    assert_eq!(value(&sync, "disposition"), "complete", "{sync}");
    assert_eq!(value(&sync, "sources_seen"), "1", "{sync}");
    assert!(sync.contains("\"source_rewritten\""), "read again: {sync}");

    let after = first_match(&run(&[
        "search",
        "--corpus",
        corpus,
        "--query",
        "stays at three",
    ]));
    assert_eq!(after[0], before[0], "the source id stays");
    assert!(after[1].ends_with(".jsonl.zst"), "{after:?}");
    assert_eq!(after[2..], before[2..], "the reference stays");
    let raw = context(corpus, &before, &[]);
    assert_eq!(value(&raw, "disposition"), "ready", "{raw}");
    assert!(raw.contains("The retry budget stays"), "{raw}");

    // Unchanged, it is not read again, and nothing is stale.
    let again = run(&["sync", "--corpus", corpus]);
    assert_eq!(value(&again, "sources_reused"), "1", "{again}");
    let said = run(&["said", "--corpus", corpus]);
    assert_eq!(value(&said, "stale_sources"), "0", "{said}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn while_a_log_and_its_compressed_copy_both_exist_the_log_is_read() {
    let root = root("both");
    let logs = logs(&root);
    std::fs::write(logs.join(log_name()), rollout()).expect("write log");
    std::fs::write(
        logs.join(format!("{}.zst", log_name())),
        compressed(rollout().as_bytes()),
    )
    .expect("write compressed copy");
    let corpus = root.join("corpus");
    let corpus = text(&corpus);
    let sync = run(&["sync", "--corpus", corpus, "--source", text(&logs)]);
    assert_eq!(value(&sync, "sources_seen"), "1", "{sync}");
    let found = run(&["search", "--corpus", corpus, "--query", "stays at three"]);
    assert_eq!(value(&found, "total_candidates"), "1", "{found}");
    assert!(first_match(&found)[1].ends_with(".jsonl"), "{found}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_replaced_compressed_log_is_read_again() {
    let root = root("replaced");
    let logs = logs(&root);
    let log = logs.join(format!("{}.zst", log_name()));
    std::fs::write(&log, compressed(rollout().as_bytes())).expect("write compressed log");
    let corpus = root.join("corpus");
    let corpus = text(&corpus);
    run(&["sync", "--corpus", corpus, "--source", text(&logs)]);

    let longer = format!(
        "{}{}\n",
        rollout(),
        r#"{"timestamp":"2026-09-20T02:00:00.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"then make it four"}]}}"#
    );
    // A new file, as a compressor writes one, written at another time.
    std::fs::remove_file(&log).expect("remove old compressed log");
    std::fs::write(&log, compressed(longer.as_bytes())).expect("write new compressed log");
    let file = std::fs::File::options()
        .write(true)
        .open(&log)
        .expect("open");
    file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
        .expect("set modified time");
    drop(file);

    let stale = run(&["said", "--corpus", corpus]);
    assert_eq!(value(&stale, "stale_sources"), "1", "{stale}");
    let sync = run(&["sync", "--corpus", corpus]);
    assert!(sync.contains("\"source_rewritten\""), "{sync}");
    let said = run(&["said", "--corpus", corpus, "--format", "text"]);
    assert!(said.contains("then make it four"), "{said}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_compressed_log_that_breaks_off_keeps_what_was_read_and_does_not_stop_the_sync() {
    let root = root("undecodable");
    let logs = logs(&root);
    let thread = rollout();
    let lines = thread.lines().collect::<Vec<_>>();
    // The first block holds the opening record and the person's message; the second block's
    // header is damaged.
    let split = lines[0].len() + lines[1].len() + 2;
    let mut damaged = compressed_in_blocks(thread.as_bytes(), &[split]);
    let second_header = 9 + 3 + split;
    damaged[second_header..second_header + 3].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    std::fs::write(logs.join(format!("{}.zst", log_name())), &damaged).expect("write damaged");
    let other = "01a0f6c2-0000-7000-8000-00000000beef";
    std::fs::write(
        logs.join(format!("rollout-2026-09-20T03-00-00-{other}.jsonl")),
        thread.replace(SESSION, other).replace("three", "five"),
    )
    .expect("write readable log");
    let corpus = root.join("corpus");
    let corpus = text(&corpus);
    let sync = run(&["sync", "--corpus", corpus, "--source", text(&logs)]);
    assert_eq!(value(&sync, "disposition"), "incomplete", "{sync}");
    assert!(sync.contains("\"source_undecodable\""), "{sync}");

    let said = run(&["said", "--corpus", corpus, "--format", "text"]);
    assert!(
        said.contains("keep the retry budget at three attempts"),
        "{said}"
    );
    assert!(
        said.contains("keep the retry budget at five attempts"),
        "{said}"
    );
    let found = run(&["search", "--corpus", corpus, "--query", "stays at three"]);
    assert_eq!(value(&found, "total_candidates"), "0", "{found}");
    let status = run(&["status", "--corpus", corpus]);
    assert_eq!(value(&status, "incomplete_sources"), "1", "{status}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn import_keeps_a_compressed_log_compressed() {
    let root = root("import");
    let logs = logs(&root);
    let bytes = compressed(rollout().as_bytes());
    std::fs::write(logs.join(format!("{}.zst", log_name())), &bytes).expect("write compressed");
    let corpus = root.join("corpus");
    let corpus = text(&corpus);
    let import = run(&[
        "import",
        "--corpus",
        corpus,
        "--source",
        text(&logs),
        "--provenance",
        "a copy of another computer's sessions",
        "--label",
        "another computer",
    ]);
    assert_eq!(value(&import, "files_copied"), "1", "{import}");
    assert_eq!(
        value(&import, "bytes_copied"),
        bytes.len().to_string(),
        "{import}"
    );
    let sync = run(&["sync", "--corpus", corpus]);
    assert_eq!(value(&sync, "disposition"), "complete", "{sync}");
    let found = run(&["search", "--corpus", corpus, "--query", "stays at three"]);
    assert_eq!(value(&found, "total_candidates"), "1", "{found}");
    let reference = first_match(&found);
    assert!(reference[1].ends_with(".jsonl.zst"), "{found}");
    let raw = context(corpus, &reference, &[]);
    assert_eq!(value(&raw, "disposition"), "ready", "{raw}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn follow_reads_a_compressed_transcript_once_without_waiting() {
    let root = root("follow");
    let logs = logs(&root);
    let log = logs.join(format!("{}.zst", log_name()));
    std::fs::write(&log, compressed(rollout().as_bytes())).expect("write compressed log");
    let corpus = root.join("corpus");
    let corpus = text(&corpus);
    run(&["sync", "--corpus", corpus, "--source", text(&logs)]);

    let started = std::time::Instant::now();
    let received = run(&[
        "follow",
        "--corpus",
        corpus,
        "--source",
        text(&log),
        "--after-byte",
        "0",
        "--sender",
        "human",
        "--seconds",
        "30",
    ]);
    assert_eq!(value(&received, "disposition"), "received", "{received}");
    assert!(received.contains("keep the retry budget"), "{received}");

    let waiting = run(&[
        "follow",
        "--corpus",
        corpus,
        "--session",
        SESSION,
        "--sender",
        "human",
        "--seconds",
        "30",
    ]);
    assert_eq!(value(&waiting, "disposition"), "waiting", "{waiting}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "nothing more is written to a compressed transcript, so follow does not wait for it"
    );
    let _ = std::fs::remove_dir_all(root);
}
