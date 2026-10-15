//! The skills under `skills/` reach the agents as written, except that Claude Code replaces `$`
//! before a digit or `ARGUMENTS` with the skill's arguments: a price such as `$0.30` turned into
//! the request text. They are checked for both apps, so the two copies stay alike.

use std::fs;
use std::path::{Path, PathBuf};

fn skill_files(directory: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("read the skills directory") {
        let path = entry.expect("read a skills entry").path();
        if path.is_dir() {
            skill_files(&path, found);
        } else if path.file_name().is_some_and(|name| name == "SKILL.md") {
            found.push(path);
        }
    }
}

/// The `$` placeholders a line holds, unless a backslash escapes them.
fn placeholders(line: &str) -> Vec<&str> {
    line.match_indices('$')
        .filter(|(index, _)| !line[..*index].ends_with('\\'))
        .map(|(index, _)| &line[index + 1..])
        .filter(|rest| {
            rest.starts_with(|c: char| c.is_ascii_digit()) || rest.starts_with("ARGUMENTS")
        })
        .collect()
}

#[test]
fn skills_hold_no_argument_placeholders() {
    let mut files = Vec::new();
    skill_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("skills"),
        &mut files,
    );
    assert!(files.len() >= 4, "found {files:?}");
    for file in files {
        let text = fs::read_to_string(&file).expect("read a skill");
        for (number, line) in text.lines().enumerate() {
            assert!(
                placeholders(line).is_empty(),
                "{}:{}: `$` before a digit or ARGUMENTS is replaced by the skill's arguments: {line}",
                file.display(),
                number + 1
            );
        }
    }
}

#[test]
fn finds_placeholders_but_not_escaped_or_other_dollars() {
    assert_eq!(placeholders("costs about $0.30 a question").len(), 1);
    assert_eq!(placeholders("all of $ARGUMENTS").len(), 1);
    assert!(placeholders(r"costs about \$0.30").is_empty());
    assert!(placeholders("ebira said --session \"$CODEX_THREAD_ID\"").is_empty());
    assert!(placeholders("($env:CLAUDE_CODE_SESSION_ID in PowerShell)").is_empty());
}
