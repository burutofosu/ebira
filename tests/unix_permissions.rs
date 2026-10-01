#![cfg(unix)]

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

const EBIRA: &str = env!("CARGO_BIN_EXE_ebira");
const RECORD: &str = concat!(
    r#"{"event_msg":{"type":"user_message","message":"private transcript marker"},"role":"user","session_id":"private-session","turn_id":"t1"}"#,
    "\n"
);
static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ebira-permissions-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("create isolated fixture");
        Self(path)
    }

    fn run(&self, mask: &str, args: &[&str], success: bool) -> String {
        // Set umask only in the child: Rust's test runner executes tests concurrently.
        // Arguments are positional parameters, never interpolated into shell code.
        let output = Command::new("sh")
            .args([
                "-c",
                "umask \"$1\"; shift; exec \"$@\"",
                "ebira-permissions",
                mask,
                EBIRA,
            ])
            .args(args)
            .env("HOME", self.0.join("empty-home"))
            .env("USERPROFILE", self.0.join("empty-home"))
            .env_remove("CODEX_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("EBIRA_CORPUS")
            .env_remove("EBIRA_TZ_OFFSET")
            .output()
            .expect("run ebira with a child-local umask");
        assert_eq!(
            output.status.success(),
            success,
            "ebira {args:?} with umask {mask}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("UTF-8 output")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn text(path: &Path) -> &str {
    path.to_str().expect("UTF-8 test path")
}

fn assert_mode(path: &Path, expected: u32) {
    let actual = fs::symlink_metadata(path)
        .expect("read permissions")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(actual, expected, "permissions on {}", path.display());
}

fn assert_private_tree(path: &Path) {
    let metadata = fs::symlink_metadata(path).expect("read corpus metadata");
    if metadata.is_dir() {
        assert_mode(path, 0o700);
        for entry in fs::read_dir(path).expect("read corpus directory") {
            assert_private_tree(&entry.expect("read corpus entry").path());
        }
    } else {
        assert!(
            metadata.is_file(),
            "unexpected corpus entry: {}",
            path.display()
        );
        assert_mode(path, 0o600);
    }
}

#[test]
fn sync_append_and_rebuild_create_private_corpus_files() {
    for mask in ["077", "022", "000"] {
        let fixture = Fixture::new();
        let source = fixture.0.join("source.jsonl");
        fs::write(&source, RECORD).expect("write source");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).expect("set source mode");
        let parent = fixture.0.join("existing-parent");
        fs::create_dir(&parent).expect("create existing parent");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).expect("set parent mode");
        let created = parent.join("new-parent");
        let corpus = created.join("corpus");

        fixture.run(
            mask,
            &["sync", "--corpus", text(&corpus), "--source", text(&source)],
            true,
        );
        assert_private_tree(&created);
        assert!(corpus.join("segments").is_dir());
        for name in [
            "ebira.lock",
            "sources.tsv",
            "source-inputs.tsv",
            "source-availability.tsv",
            "timeline.tsv",
        ] {
            assert!(corpus.join(name).is_file(), "missing {name}");
        }

        OpenOptions::new()
            .append(true)
            .open(&source)
            .expect("open source")
            .write_all(RECORD.as_bytes())
            .expect("append source");
        let appended = fixture.run(mask, &["sync", "--corpus", text(&corpus)], true);
        assert!(appended.contains("\"sources_appended\":1"), "{appended}");
        assert_private_tree(&created);
        fixture.run(
            mask,
            &["sync", "--corpus", text(&corpus), "--rebuild"],
            true,
        );
        assert_private_tree(&created);
        fixture.run(
            mask,
            &["timeline", "--corpus", text(&corpus), "--rebuild"],
            true,
        );
        assert_private_tree(&created);
        let found = fixture.run(
            mask,
            &[
                "search",
                "--corpus",
                text(&corpus),
                "--query",
                "private transcript marker",
            ],
            true,
        );
        assert!(found.contains("\"returned\":2"), "{found}");
        assert_mode(&source, 0o644);
        assert_mode(&parent, 0o755);
        assert_eq!(
            fs::read_to_string(&source).expect("read source"),
            RECORD.repeat(2)
        );
    }
}

#[test]
fn imports_do_not_inherit_public_source_permissions() {
    for mask in ["077", "022", "000"] {
        let fixture = Fixture::new();
        let source = fixture.0.join("archive").join("nested");
        fs::create_dir_all(&source).expect("create source directory");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).expect("set source mode");
        for (name, mode) in [("readable.jsonl", 0o644), ("writable.jsonl", 0o666)] {
            fs::write(source.join(name), RECORD).expect("write archived source");
            fs::set_permissions(source.join(name), fs::Permissions::from_mode(mode))
                .expect("set source mode");
        }
        let corpus = fixture.0.join("corpus");
        fixture.run(
            mask,
            &[
                "import",
                "--corpus",
                text(&corpus),
                "--source",
                text(&fixture.0.join("archive")),
                "--provenance",
                "backup",
                "--label",
                "private import",
            ],
            true,
        );
        assert_private_tree(&corpus);
        let managed = fs::read_dir(corpus.join("managed-imports"))
            .expect("read imports")
            .next()
            .expect("one import")
            .expect("read import entry")
            .path();
        for name in ["readable.jsonl", "writable.jsonl"] {
            assert_eq!(
                fs::read_to_string(managed.join("jsonl/nested").join(name)).expect("read copy"),
                RECORD
            );
        }
        assert!(managed.join("files.tsv").is_file());
        assert!(corpus.join("managed-imports.tsv").is_file());
        fixture.run(mask, &["sync", "--corpus", text(&corpus)], true);
        assert_private_tree(&corpus);
        assert_mode(&source, 0o755);
        assert_mode(&source.join("readable.jsonl"), 0o644);
        assert_mode(&source.join("writable.jsonl"), 0o666);
    }
}

#[test]
fn failed_sync_leaves_private_partial_files() {
    let fixture = Fixture::new();
    let source = fixture.0.join("source.jsonl");
    fs::write(&source, RECORD).expect("write source");
    let corpus = fixture.0.join("corpus");
    // A directory at the final timeline path prevents promoting its completed partial.
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(corpus.join("timeline.tsv"))
        .expect("block timeline promotion");
    fixture.run(
        "000",
        &["sync", "--corpus", text(&corpus), "--source", text(&source)],
        false,
    );
    assert!(corpus.join("timeline.tsv.partial").is_file());
    assert!(corpus.join("source-inputs.tsv.partial").is_file());
    assert_private_tree(&corpus);
}
