use std::path::{Path, PathBuf};
use std::process::Command;

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");

/// Runs ebira with a home that holds no transcripts, so nothing outside the test is read.
fn ebira() -> Command {
    let home = std::env::temp_dir().join("ebira-test-empty-home");
    let mut command = Command::new(EBIRA);
    command
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("EBIRA_CORPUS");
    command
}

fn run(args: &[&str]) -> String {
    let output = ebira().args(args).output().expect("ebira runs");
    assert!(
        output.status.success(),
        "ebira {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("ebira writes UTF-8")
}

fn run_failure(args: &[&str]) -> String {
    let output = ebira().args(args).output().expect("ebira runs");
    assert!(
        !output.status.success(),
        "ebira {:?} unexpectedly succeeded: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("ebira writes UTF-8")
}

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "ebira-managed-import-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create test root");
    root
}

fn text(path: &Path) -> &str {
    path.to_str().expect("test path is UTF-8")
}

fn imported_directory(home: &Path) -> PathBuf {
    std::fs::read_dir(home.join("managed-imports"))
        .expect("read managed imports")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_dir() && !path.to_string_lossy().ends_with(".partial"))
        .expect("completed managed import")
}

#[test]
fn import_preserves_records_provenance_and_portability() {
    let root = root("portable");
    let source = root.join("old-machine").join("backup-2026");
    std::fs::create_dir_all(source.join("02").join("04")).expect("create source tree");
    let record = concat!(
        r#"{"event_msg":{"type":"user_message","message":"managed portable marker"},"role":"user","session_id":"portable","turn_id":"t1"}"#,
        "\n"
    );
    std::fs::write(source.join("02").join("04").join("rollout.jsonl"), record)
        .expect("write archived JSONL");
    std::fs::write(source.join("ignored.txt"), "not a JSONL source").expect("write ignored file");

    let home = root.join("ebira-home");
    let corpus = home.join("corpus");
    let imported = run(&[
        "import",
        "--source",
        text(&source),
        "--provenance",
        "backup",
        "--label",
        "codex-2026",
        "--source-computer",
        "OLD-PC",
        "--corpus",
        text(&corpus),
    ]);
    assert!(
        imported.contains("\"disposition\":\"imported\"")
            && imported.contains("\"mode\":\"managed_jsonl_import\"")
            && imported.contains("\"provenance\":\"backup\"")
            && imported.contains("\"label\":\"codex-2026\"")
            && imported.contains("\"source_computer\":\"OLD-PC\"")
            && imported.contains("\"files_copied\":1")
            && imported.contains("\"projection_state\":\"not_built\""),
        "import reports the copy and its provenance: {imported}"
    );

    let managed = imported_directory(&home);
    let copied = managed
        .join("jsonl")
        .join("02")
        .join("04")
        .join("rollout.jsonl");
    assert_eq!(
        std::fs::read(&copied).expect("read managed copy"),
        std::fs::read(source.join("02").join("04").join("rollout.jsonl")).expect("read original"),
        "the managed JSONL is a byte-for-byte copy"
    );
    let files = std::fs::read_to_string(managed.join("files.tsv")).expect("read file provenance");
    let relative_directory = Path::new("02").join("04");
    assert!(
        files.contains("rollout.jsonl")
            && files.contains(relative_directory.to_string_lossy().as_ref()),
        "the per-file route back to the imported layout is retained: {files}"
    );

    std::fs::remove_dir_all(root.join("old-machine")).expect("remove original placement");
    let moved_home = root.join("moved-ebira-home");
    std::fs::rename(&home, &moved_home).expect("move complete Ebira home");
    let moved_corpus = moved_home.join("corpus");
    let build = run(&["sync", "--corpus", text(&moved_corpus)]);
    assert!(
        build.contains("\"managed_imports\":1")
            && build.contains("\"unavailable_managed_imports\":0"),
        "a fresh corpus discovers the moved managed import: {build}"
    );
    let found = run(&[
        "search",
        "--corpus",
        text(&moved_corpus),
        "--query",
        "managed portable marker",
        "--limit",
        "10",
    ]);
    assert!(
        found.contains("managed portable marker"),
        "the copied JSONL remains searchable without its old path: {found}"
    );
    let status = run(&["status", "--corpus", text(&moved_corpus)]);
    assert!(
        status.contains("\"disposition\":\"observed\"")
            && status.contains("\"imports\":[{\"import_id\"")
            && status.contains("\"original_path\"")
            && status.contains("backup-2026")
            && status.contains("\"disposition\":\"present\""),
        "moved import lost its provenance: {status}"
    );

    std::fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn corpus_rebuild_includes_managed_imports() {
    let root = root("rebuild");
    let archive = root.join("archive");
    std::fs::create_dir_all(&archive).expect("create archive");
    std::fs::write(
        archive.join("archive.jsonl"),
        concat!(
            r#"{"event_msg":{"type":"user_message","message":"managed rebuild marker"},"role":"user","session_id":"archive","turn_id":"a"}"#,
            "\n"
        ),
    )
    .expect("write archive");
    let live = root.join("live.jsonl");
    std::fs::write(
        &live,
        concat!(
            r#"{"event_msg":{"type":"user_message","message":"live marker"},"role":"user","session_id":"live","turn_id":"l"}"#,
            "\n"
        ),
    )
    .expect("write live source");
    let corpus = root.join("home").join("corpus");

    run(&[
        "import",
        "--source",
        text(&archive),
        "--provenance",
        "backup",
        "--label",
        "archive",
        "--corpus",
        text(&corpus),
    ]);
    run(&["sync", "--source", text(&live), "--corpus", text(&corpus)]);
    let rebuilt = run(&[
        "sync",
        "--rebuild",
        "--source",
        text(&live),
        "--corpus",
        text(&corpus),
    ]);
    assert!(
        rebuilt.contains("\"managed_imports\":1") && rebuilt.contains("\"sources_seen\":2"),
        "corpus rebuild omitted the managed archive: {rebuilt}"
    );
    let status = run(&["status", "--corpus", text(&corpus)]);
    assert!(
        status.contains("\"sources\":2")
            && status.contains("\"registered_source_inputs\":2")
            && status.contains("\"managed_imports\":1"),
        "the rebuilt corpus retains both authorities: {status}"
    );
    let found = run(&[
        "search",
        "--corpus",
        text(&corpus),
        "--query",
        "managed rebuild marker",
        "--limit",
        "10",
    ]);
    assert!(
        found.contains("managed rebuild marker"),
        "the archive remains in the rebuilt projection: {found}"
    );

    std::fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn missing_managed_copy_remains_registered() {
    let root = root("missing");
    let archive = root.join("archive");
    std::fs::create_dir_all(&archive).expect("create archive");
    std::fs::write(archive.join("archive.jsonl"), "{}\n").expect("write archive");
    let live = root.join("live.jsonl");
    std::fs::write(&live, "{}\n").expect("write live source");
    let home = root.join("home");
    let corpus = home.join("corpus");

    run(&[
        "import",
        "--source",
        text(&archive),
        "--provenance",
        "other-pc",
        "--label",
        "old-machine",
        "--corpus",
        text(&corpus),
    ]);
    run(&["sync", "--source", text(&live), "--corpus", text(&corpus)]);
    std::fs::remove_dir_all(imported_directory(&home)).expect("remove managed copy");
    let immediate_status = run(&["status", "--corpus", text(&corpus)]);
    assert!(
        immediate_status.contains("\"disposition\":\"managed_imports_unavailable\"")
            && immediate_status.contains("\"unavailable_managed_imports\":1"),
        "status missed the removed managed copy: {immediate_status}"
    );
    let build = run(&["sync", "--corpus", text(&corpus)]);
    assert!(
        build.contains("\"unavailable_managed_imports\":1")
            && build.contains("\"unreadable_source_paths\":1"),
        "build missed the unavailable managed copy: {build}"
    );
    let status = run(&["status", "--corpus", text(&corpus)]);
    assert!(
        status.contains("\"unavailable_managed_imports\":1")
            && status.contains("\"label\":\"old-machine\"")
            && status.contains("\"disposition\":\"missing\""),
        "status keeps the missing managed import visible: {status}"
    );

    std::fs::remove_dir_all(root).expect("remove test root");
}

/// One directory can be written two ways: through a symbolic link here, with an 8.3 short
/// name or a `\\?\` prefix on Windows. A removed import is still one missing source.
#[cfg(unix)]
#[test]
fn removed_import_is_one_missing_source_under_any_spelling() {
    let root = root("spelling");
    let real = root.join("real");
    std::fs::create_dir_all(&real).expect("create the real directory");
    let link = root.join("link");
    std::os::unix::fs::symlink(&real, &link).expect("link the directory");
    let archive = root.join("archive");
    std::fs::create_dir_all(&archive).expect("create archive");
    std::fs::write(archive.join("archive.jsonl"), "{}\n").expect("write archive");
    let live = root.join("live.jsonl");
    std::fs::write(&live, "{}\n").expect("write live source");
    let corpus = link.join("home").join("corpus");

    run(&[
        "import",
        "--source",
        text(&archive),
        "--provenance",
        "other-pc",
        "--label",
        "old-machine",
        "--corpus",
        text(&corpus),
    ]);
    run(&["sync", "--source", text(&live), "--corpus", text(&corpus)]);
    std::fs::remove_dir_all(imported_directory(&real.join("home"))).expect("remove managed copy");
    let build = run(&["sync", "--corpus", text(&corpus)]);
    assert!(
        build.contains("\"unavailable_managed_imports\":1")
            && build.contains("\"unreadable_source_paths\":1"),
        "the removed import is one missing source: {build}"
    );

    std::fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn invalid_registry_returns_an_error() {
    let root = root("registry-not-file");
    let home = root.join("home");
    std::fs::create_dir_all(home.join("managed-imports.tsv"))
        .expect("create a non-file registry path");
    let corpus = home.join("corpus");

    let observed = run_failure(&["sync", "--corpus", text(&corpus)]);
    assert!(
        observed.contains("\"disposition\":\"error\"")
            && observed.contains("registry path is not a file")
            && !observed.contains("\"managed_imports\":0"),
        "invalid registry was reported as empty: {observed}"
    );

    std::fs::remove_dir_all(root).expect("remove test root");
}
