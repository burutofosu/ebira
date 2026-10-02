mod commits;
mod core;
mod corpus;
mod follow;
mod format;
mod imports;
mod json;
mod jsonl;
mod output;
mod private_fs;
mod questions;
mod resume;
mod said;
mod search;
mod time;

use search::SearchRequest;
use std::io;
use std::path::PathBuf;

/// One command: its help text and the options it accepts. The same table drives `ebira help`
/// and option validation, so the two cannot disagree.
struct CommandSpec {
    name: &'static str,
    synopsis: &'static str,
    summary: &'static str,
    details: &'static [&'static str],
    values: &'static [&'static str],
    flags: &'static [&'static str],
}

const FILTERS: &[&str] = &[
    "",
    "filters:",
    "  --session <id>               records of one session",
    "  --source-id <id>             records of one source (a complete source_id)",
    "  --sender <sender>            human (the person) | agent | system | summary | assistant",
    "  --role <role>                the role written in the log",
    "  --kind <kind>                user | assistant | command | output | patch | summary",
    "  --from <time>, --to <time>   a yyyy-mm-dd date or an RFC 3339 time",
    "  --limit <n>, --offset <n>    page size and start",
];

const SEARCH_DETAILS: &[&str] = &[
    "  --raw                        compare the query with the original JSONL records",
    "  --ignore-case                fold ASCII letters",
    "  --date-limit <n>             dates per page of the date map (default 12)",
    "  --date-offset <n>            the first date of that page",
];

const SEARCH_VALUES: &[&str] = &[
    "--corpus",
    "--query",
    "--limit",
    "--offset",
    "--session",
    "--source-id",
    "--role",
    "--kind",
    "--sender",
    "--from",
    "--to",
    "--order",
    "--date-limit",
    "--date-offset",
];

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "sync",
        synopsis: "[--source <path>]... [--rebuild] [--rebuild-source <path>]... [--tool-output-chars <n>]",
        summary: "Create the corpus, or add what the logs gained since the last sync",
        details: &[
            "  --source <path>              a JSONL file or a directory of them, kept for later syncs;",
            "                               without it the first sync reads the Claude Code and Codex",
            "                               transcript directories",
            "  --rebuild                    build the corpus again; with --source, from exactly those",
            "  --rebuild-source <path>      read again the logs at a path: one log, or all under a directory",
            "  --tool-output-chars <n>      characters kept from each tool output (default 300)",
        ],
        values: &["--corpus", "--source", "--rebuild-source", "--tool-output-chars"],
        flags: &["--rebuild"],
    },
    CommandSpec {
        name: "status",
        synopsis: "",
        summary: "Show the corpus, its sources and imports, and what is not yet indexed",
        details: &[],
        values: &["--corpus"],
        flags: &[],
    },
    CommandSpec {
        name: "said",
        synopsis: "[--session <id>] [--source-id <id>] [--cwd <text>] [--app claude|codex|other] [--query <text>] [--ignore-case] [--from <time>] [--to <time>] [--order desc|asc] [--limit <n>] [--offset <n>] [--max-chars <n>] [--format json|text] [--include-imported]",
        summary: "List the person's own messages, newest first",
        details: &[
            "  --cwd <text>                 messages whose working directory contains the text",
            "  --limit <n>, --max-chars <n> messages and characters per page (default 30 and 15000)",
            "  --include-imported           also list copies of the person's messages imported",
            "                               with another conversation",
        ],
        values: &[
            "--corpus",
            "--session",
            "--source-id",
            "--cwd",
            "--app",
            "--query",
            "--from",
            "--to",
            "--order",
            "--limit",
            "--offset",
            "--max-chars",
            "--format",
        ],
        flags: &["--ignore-case", "--include-imported"],
    },
    CommandSpec {
        name: "resume",
        synopsis: "[--session <id> | --source-id <id>] [--brief]",
        summary: "Recover the latest state of a session",
        details: &[
            "  --brief                      only the person's latest messages, the latest replies",
            "                               and tool calls, and the compactions",
        ],
        values: &["--corpus", "--source-id", "--session"],
        flags: &["--brief"],
    },
    CommandSpec {
        name: "search",
        synopsis: "--query <text> [--raw] [--ignore-case] [--order asc|desc] [filters]",
        summary: "Find the records that contain a literal text, oldest first",
        details: SEARCH_DETAILS,
        values: SEARCH_VALUES,
        flags: &["--raw", "--ignore-case"],
    },
    CommandSpec {
        name: "history",
        synopsis: "--query <text> [--raw] [--ignore-case] [--order asc|desc] [filters]",
        summary: "Show on which dates a literal text appears",
        details: SEARCH_DETAILS,
        values: SEARCH_VALUES,
        flags: &["--raw", "--ignore-case"],
    },
    CommandSpec {
        name: "timeline",
        synopsis: "[--date <yyyy-mm-dd>] [--order desc|asc] [--rebuild] [filters]",
        summary: "List the dates newest first, or the records of one date",
        details: &["  --rebuild                    rebuild the date map from the corpus"],
        values: &[
            "--corpus",
            "--date",
            "--order",
            "--from",
            "--to",
            "--source-id",
            "--session",
            "--role",
            "--kind",
            "--sender",
            "--limit",
            "--offset",
        ],
        flags: &["--rebuild"],
    },
    CommandSpec {
        name: "context",
        synopsis: "--source-id <id> --byte-start <n> --byte-len <n> [--before <n>] [--after <n>]",
        summary: "Read an original record, and its neighbours, from a source reference",
        details: &[],
        values: &[
            "--corpus",
            "--source-id",
            "--byte-start",
            "--byte-len",
            "--before",
            "--after",
        ],
        flags: &[],
    },
    CommandSpec {
        name: "follow",
        synopsis: "(--session <id> | --source-id <id> | --source <file>) [--after-byte <n>] [--seconds <n>] [--sender <sender>|any] [--prefix <text>] [--limit <n>]",
        summary: "Wait for the next message written to another session's transcript",
        details: &[
            "  --sender <sender>|any        whose messages to return (default assistant)",
            "  --seconds <n>                how long to wait (default 55)",
            "  --after-byte <n>             continue from the after_byte of a previous result",
        ],
        values: &[
            "--corpus",
            "--source-id",
            "--session",
            "--source",
            "--after-byte",
            "--seconds",
            "--sender",
            "--prefix",
            "--limit",
        ],
        flags: &[],
    },
    CommandSpec {
        name: "commits",
        synopsis: "--repo <dir> [--from <date>] [--to <date>] [--limit <n>] [--offset <n>]",
        summary: "Find the commit ids mentioned in the records and resolve them with Git",
        details: &[],
        values: &["--corpus", "--repo", "--from", "--to", "--limit", "--offset"],
        flags: &[],
    },
    CommandSpec {
        name: "import",
        synopsis: "--source <path> --provenance <text> --label <text> [--source-computer <text>]",
        summary: "Copy JSONL logs into the corpus directory so they outlive the originals",
        details: &["Run `ebira sync` afterwards to add them to the corpus."],
        values: &[
            "--corpus",
            "--source",
            "--provenance",
            "--label",
            "--source-computer",
        ],
        flags: &[],
    },
];

fn command_spec(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|spec| spec.name == name)
}

fn usage() -> String {
    let mut text = format!(
        "ebira {}: search and recover Claude Code and Codex conversations from their JSONL logs\n\n\
         usage: ebira <command> [options]\n\ncommands:\n",
        env!("CARGO_PKG_VERSION")
    );
    for spec in COMMANDS {
        text.push_str(&format!("  {:<10}{}\n", spec.name, spec.summary));
    }
    text.push_str(
        "\nEvery command reads --corpus <dir> (default: $EBIRA_CORPUS, else the platform data\n\
         directory) and writes one JSON object. `ebira help <command>` lists its options.",
    );
    text
}

fn command_usage(spec: &CommandSpec) -> String {
    let mut text = format!("ebira {} {}", spec.name, spec.synopsis)
        .trim_end()
        .to_string();
    text.push_str(&format!("\n\n{}.\n", spec.summary));
    let mut lines = spec.details.to_vec();
    if spec.synopsis.contains("[filters]") {
        lines.extend_from_slice(FILTERS);
    }
    if !lines.is_empty() {
        text.push('\n');
        text.push_str(&lines.join("\n"));
        text.push('\n');
    }
    text.push_str("\n  --corpus <dir>               the corpus (default: $EBIRA_CORPUS)");
    text
}

fn unknown_command(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("unknown command: {}; run `ebira help`", name),
    )
}

fn attached_value<'a>(argument: &'a str, name: &str) -> Option<&'a str> {
    let rest = argument.strip_prefix(name)?;
    rest.strip_prefix('=')
}

fn option_values(args: &[String], name: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if let Some(value) = attached_value(&args[index], name) {
            values.push(value.to_string());
            index += 1;
            continue;
        }
        if args[index] == name {
            if let Some(value) = args.get(index + 1) {
                values.push(value.clone());
                index += 2;
                continue;
            }
        }
        index += 1;
    }
    values
}

fn one_option(args: &[String], name: &str) -> io::Result<String> {
    option_values(args, name)
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("{} is required", name)))
}

fn corpus_path(args: &[String]) -> io::Result<PathBuf> {
    option_values(args, "--corpus")
        .into_iter()
        .next()
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(default_corpus_path)
}

fn default_corpus_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("EBIRA_CORPUS").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .map(|path| path.join("ebira").join("corpus"))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "neither EBIRA_CORPUS nor LOCALAPPDATA names a corpus location",
                )
            })
    }
    #[cfg(not(windows))]
    {
        if let Some(path) = std::env::var_os("XDG_DATA_HOME").filter(|path| !path.is_empty()) {
            return Ok(PathBuf::from(path).join("ebira").join("corpus"));
        }
        std::env::var_os("HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .map(|path| {
                path.join(".local")
                    .join("share")
                    .join("ebira")
                    .join("corpus")
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "neither EBIRA_CORPUS, XDG_DATA_HOME, nor HOME names a corpus location",
                )
            })
    }
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| arg == name)
}

fn number_option(args: &[String], name: &str, default: usize) -> io::Result<usize> {
    let Some(value) = option_values(args, name).into_iter().next() else {
        return Ok(default);
    };
    value.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} must be an integer", name),
        )
    })
}

fn optional_number_option(args: &[String], name: &str) -> io::Result<Option<usize>> {
    let Some(value) = option_values(args, name).into_iter().next() else {
        return Ok(None);
    };
    value.parse().map(Some).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} must be an integer", name),
        )
    })
}

fn validate_options(spec: &CommandSpec, args: &[String]) -> io::Result<()> {
    let mut index = 1;
    while index < args.len() {
        let argument = &args[index];
        if spec.flags.contains(&argument.as_str()) {
            index += 1;
            continue;
        }
        if spec
            .values
            .iter()
            .any(|name| attached_value(argument, name).is_some())
        {
            index += 1;
            continue;
        }
        if spec.values.contains(&argument.as_str()) {
            let Some(value) = args.get(index + 1) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} requires a value", argument),
                ));
            };
            if value.starts_with("--") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("use {}={} for option-like values", argument, value),
                ));
            }
            index += 2;
            continue;
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unknown option for {}: {}; run `ebira help {}`",
                spec.name, argument, spec.name
            ),
        ));
    }
    Ok(())
}

fn run(args: &[String]) -> io::Result<()> {
    let Some(command) = args.first().map(String::as_str) else {
        return output::write_line(&usage());
    };
    match command {
        "--version" | "-V" => {
            return output::write_line(&format!("ebira {}", env!("CARGO_PKG_VERSION")));
        }
        "help" | "--help" | "-h" => {
            match args.get(1) {
                None => output::write_line(&usage())?,
                Some(name) => {
                    let spec = command_spec(name).ok_or_else(|| unknown_command(name))?;
                    output::write_line(&command_usage(spec))?;
                }
            }
            return Ok(());
        }
        _ => {}
    }
    let spec = command_spec(command).ok_or_else(|| unknown_command(command))?;
    if args[1..]
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        return output::write_line(&command_usage(spec));
    }
    validate_options(spec, args)?;
    match command {
        "sync" => sync(args),
        "import" => {
            let corpus = corpus_path(args)?;
            let source = PathBuf::from(one_option(args, "--source")?);
            let provenance = one_option(args, "--provenance")?;
            let label = one_option(args, "--label")?;
            let source_computer = option_values(args, "--source-computer")
                .into_iter()
                .next()
                .unwrap_or_default();
            let _lock = corpus::lock_exclusive(&corpus)?;
            let report = imports::import(&corpus, &source, &provenance, &label, &source_computer)?;
            output::write_line(&report.json())
        }
        "search" | "history" => {
            let root = corpus_path(args)?;
            let _lock = corpus::read_lock(&root)?;
            let request = search_request(
                args,
                if command == "history" { "history" } else { "" },
                corpus::corpus_offset_minutes(&root)?,
            )?;
            search::run(&root, request)
        }
        "timeline" => {
            let root = corpus_path(args)?;
            if has_flag(args, "--rebuild") {
                // Checked before the lock too, so that a lock file is not made where no
                // corpus is.
                corpus::require_corpus(&root)?;
                let _lock = corpus::lock_exclusive(&root)?;
                corpus::require_corpus(&root)?;
                return corpus::rebuild_timeline(&root);
            }
            let _lock = corpus::read_lock(&root)?;
            let request = timeline_request(args, corpus::corpus_offset_minutes(&root)?)?;
            search::timeline(&root, request)
        }
        "commits" => {
            let root = corpus_path(args)?;
            let _lock = corpus::read_lock(&root)?;
            let offset_minutes = corpus::corpus_offset_minutes(&root)?;
            let (from, to, range) = range_options(args, offset_minutes)?;
            let request = commits::CommitRequest {
                repo: one_option(args, "--repo")?,
                from,
                to,
                range,
                offset_minutes,
                limit: number_option(args, "--limit", 50)?,
                offset: number_option_u64_optional(args, "--offset")?,
            };
            commits::run(&root, &request)
        }
        "resume" => {
            let root = corpus_path(args)?;
            let _lock = corpus::read_lock(&root)?;
            let source_id = option_values(args, "--source-id").into_iter().next();
            let session = option_values(args, "--session").into_iter().next();
            resume::resume(
                &root,
                source_id.as_deref(),
                session.as_deref(),
                has_flag(args, "--brief"),
            )
        }
        "said" => {
            let root = corpus_path(args)?;
            let _lock = corpus::read_lock(&root)?;
            let offset_minutes = corpus::corpus_offset_minutes(&root)?;
            let (from, to, range) = range_options(args, offset_minutes)?;
            let request = said::SaidRequest {
                session: option_values(args, "--session").into_iter().next(),
                source_id: option_values(args, "--source-id").into_iter().next(),
                cwd: option_values(args, "--cwd").into_iter().next(),
                app: option_values(args, "--app").into_iter().next(),
                query: option_values(args, "--query").into_iter().next(),
                fold_ascii_case: has_flag(args, "--ignore-case"),
                from,
                to,
                range,
                // Defaults keep one page inside a 30,000-character tool output in either format.
                limit: number_option(args, "--limit", 30)?,
                offset: number_option_u64_optional(args, "--offset")?,
                newest_first: order_option(args, true)?,
                max_chars: number_option(args, "--max-chars", 15_000)?,
                text_format: match option_values(args, "--format")
                    .into_iter()
                    .next()
                    .as_deref()
                {
                    None | Some("json") => false,
                    Some("text") => true,
                    Some(value) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("--format must be json or text, got {}", value),
                        ))
                    }
                },
                offset_minutes,
                include_imported: has_flag(args, "--include-imported"),
            };
            if let Some(app) = request.app.as_deref() {
                if !matches!(app, "claude" | "codex" | "other") {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("--app must be claude, codex, or other, got {}", app),
                    ));
                }
            }
            said::run(&root, &request)
        }
        "context" => {
            let root = corpus_path(args)?;
            let _lock = corpus::read_lock(&root)?;
            let source_id = one_option(args, "--source-id")?;
            let byte_start = number_option_u64(args, "--byte-start")?;
            let byte_len = number_option_u64(args, "--byte-len")?;
            search::context(
                &root,
                &source_id,
                byte_start,
                byte_len,
                number_option(args, "--before", 0)?,
                number_option(args, "--after", 0)?,
            )
        }
        "follow" => {
            let root = corpus_path(args)?;
            let after_byte = match option_values(args, "--after-byte").into_iter().next() {
                Some(value) => Some(value.parse::<u64>().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--after-byte must be a whole number",
                    )
                })?),
                None => None,
            };
            let sender = option_values(args, "--sender")
                .into_iter()
                .next()
                .unwrap_or_else(|| "assistant".to_string());
            if sender != "any" {
                check_sender(&sender, &["any"])?;
            }
            let request = follow::FollowRequest {
                source_id: option_values(args, "--source-id").into_iter().next(),
                session: option_values(args, "--session").into_iter().next(),
                path: option_values(args, "--source").into_iter().next(),
                after_byte,
                seconds: number_option(args, "--seconds", 55)? as u64,
                sender,
                prefix: option_values(args, "--prefix").into_iter().next(),
                limit: number_option(args, "--limit", 20)?,
            };
            follow::run(&root, &request)
        }
        "status" => {
            let root = corpus_path(args)?;
            let _lock = corpus::read_lock(&root)?;
            corpus::status(&root)
        }
        _ => Err(unknown_command(command)),
    }
}

/// `sync` creates the corpus or brings it up to date. The sources are the registered ones plus
/// any `--source`; a first sync without sources reads the Claude Code and Codex transcript
/// directories. `--rebuild` with `--source` builds from exactly the given sources.
///
/// The logs are the source of truth. A corpus written in another storage format or under other
/// reading rules is rebuilt from them in full; otherwise only what the logs gained is read.
fn sync(args: &[String]) -> io::Result<()> {
    let corpus = corpus_path(args)?;
    let _lock = corpus::lock_exclusive(&corpus)?;
    let given = option_values(args, "--source");
    let requested = has_flag(args, "--rebuild");
    let state = corpus::corpus_state(&corpus);
    let exists = state != corpus::CorpusState::Missing;
    let rebuild_cause = match state {
        _ if requested => Some("requested"),
        corpus::CorpusState::FormatChanged => Some("format_changed"),
        corpus::CorpusState::RulesChanged => Some("rules_changed"),
        _ => None,
    };
    let mut sources = if requested && !given.is_empty() {
        given.clone()
    } else {
        let mut sources = if exists {
            corpus::registered_sources(&corpus)?
        } else {
            Vec::new()
        };
        for source in &given {
            if !sources.contains(source) {
                sources.push(source.clone());
            }
        }
        sources
    };
    let mut detected = Vec::new();
    if sources.is_empty() && !exists {
        detected = corpus::default_sources();
        sources = detected.clone();
    }
    let managed = imports::inventory(&corpus)?;
    for source in managed.source_inputs() {
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    if sources.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no sources: no Claude Code or Codex transcripts were found in their default \
             directories; name the logs with --source <path>",
        ));
    }
    let report = corpus::build_with_preview(
        &sources,
        &corpus,
        rebuild_cause.is_none(),
        optional_number_option(args, "--tool-output-chars")?,
        &option_values(args, "--rebuild-source"),
    )?;
    output::write_line(
        &json::Object::new()
            .name("disposition", report.disposition)
            .name("mode", "compact_literal_corpus")
            .name("counter_scope", "this_command")
            .number("sources_seen", report.sources_seen)
            .number("sources_processed", report.sources_processed)
            .number("sources_appended", report.sources_appended)
            .number("sources_reused", report.sources_reused)
            .number("records", report.records)
            .number("invalid_records", report.invalid_records)
            .number("partial_records", report.partial_records)
            .number("timeline_runs", report.timeline_runs)
            .number("corpus_bytes", report.corpus_bytes)
            .name("corpus_bytes_scope", "current_projection")
            .number("unreadable_source_paths", report.unreadable_source_paths)
            .number("changed_sources_total", report.changed_sources_total)
            .boolean(
                "changed_sources_truncated",
                report.changed_sources_total > report.changed_sources.len() as u64,
            )
            .raw("changed_sources", &changed_sources_json(&report))
            .number("managed_imports", managed.entries.len() as u64)
            .number("unavailable_managed_imports", managed.unavailable_imports)
            .raw(
                "detected_sources",
                &json::strings(detected.iter().map(String::as_str)),
            )
            .optional("rebuild_cause", rebuild_cause)
            .name("corpus", &report.corpus)
            .finish(),
    )
}

fn search_request(
    args: &[String],
    operation: &str,
    offset_minutes: i64,
) -> io::Result<SearchRequest> {
    let (from, to, range) = range_options(args, offset_minutes)?;
    Ok(SearchRequest {
        offset_minutes,
        query: one_option(args, "--query")?,
        limit: number_option(args, "--limit", 50)?,
        offset: number_option_u64_optional(args, "--offset")?,
        operation: operation.to_string(),
        source_id: option_values(args, "--source-id").into_iter().next(),
        session: option_values(args, "--session").into_iter().next(),
        role: option_values(args, "--role").into_iter().next(),
        kind: kind_option(args)?,
        sender: sender_option(args)?,
        from,
        to,
        range,
        date_limit: number_option(args, "--date-limit", 12)?,
        date_offset: number_option_u64_optional(args, "--date-offset")?,
        raw: has_flag(args, "--raw"),
        fold_ascii_case: has_flag(args, "--ignore-case"),
        newest_first: order_option(args, false)?,
    })
}

/// `--from` and `--to` as typed, for echoing back in next actions, and the range they name.
fn range_options(
    args: &[String],
    offset_minutes: i64,
) -> io::Result<(Option<String>, Option<String>, time::Range)> {
    let from = option_values(args, "--from").into_iter().next();
    let to = option_values(args, "--to").into_iter().next();
    let range = time::Range::parse(from.as_deref(), to.as_deref(), offset_minutes)?;
    Ok((from, to, range))
}

/// `--order desc` puts the newest first; `default_newest` applies when --order is absent.
fn order_option(args: &[String], default_newest: bool) -> io::Result<bool> {
    match option_values(args, "--order").into_iter().next().as_deref() {
        None => Ok(default_newest),
        Some("desc") => Ok(true),
        Some("asc") => Ok(false),
        Some(value) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--order must be asc or desc, got {}", value),
        )),
    }
}

fn sender_option(args: &[String]) -> io::Result<Option<String>> {
    let Some(value) = option_values(args, "--sender").into_iter().next() else {
        return Ok(None);
    };
    check_sender(&value, &[])?;
    Ok(Some(value))
}

fn check_sender(value: &str, extra: &[&str]) -> io::Result<()> {
    let names = core::Sender::ALL
        .iter()
        .map(|sender| sender.as_str())
        .chain(extra.iter().copied())
        .collect::<Vec<_>>();
    check_name("--sender", value, &names)
}

/// A value that must be one of `names`, all of which the error lists.
fn check_name(option: &str, value: &str, names: &[&str]) -> io::Result<()> {
    if names.contains(&value) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{} must be {}, got {}", option, names.join("|"), value),
    ))
}

fn kind_option(args: &[String]) -> io::Result<Option<String>> {
    let Some(value) = option_values(args, "--kind").into_iter().next() else {
        return Ok(None);
    };
    let names = core::EventKind::ALL.map(core::EventKind::as_str);
    check_name("--kind", &value, &names)?;
    Ok(Some(value))
}

fn changed_sources_json(report: &corpus::BuildReport) -> String {
    json::array(report.changed_sources.iter().map(|source| {
        json::Object::new()
            .name("source_id", &source.source_id)
            .name("disposition", &source.disposition)
            .number("previous_byte_end", source.previous_byte_end)
            .number("committed_byte_end", source.committed_byte_end)
            .number("observed_size", source.observed_size)
            .number("records_added", source.records_added)
            .finish()
    }))
}

fn timeline_request(args: &[String], offset_minutes: i64) -> io::Result<search::TimelineRequest> {
    let date = option_values(args, "--date").into_iter().next();
    let order = match option_values(args, "--order").into_iter().next().as_deref() {
        None | Some("desc") => search::TimelineOrder::Desc,
        Some("asc") => search::TimelineOrder::Asc,
        Some(value) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--order must be asc or desc, got {}", value),
            ))
        }
    };
    let (from, to, range) = range_options(args, offset_minutes)?;
    Ok(search::TimelineRequest {
        offset_minutes,
        date: date.clone(),
        from,
        to,
        range,
        source_id: option_values(args, "--source-id").into_iter().next(),
        session: option_values(args, "--session").into_iter().next(),
        role: option_values(args, "--role").into_iter().next(),
        kind: kind_option(args)?,
        sender: sender_option(args)?,
        limit: number_option(args, "--limit", if date.is_some() { 50 } else { 200 })?,
        offset: number_option_u64_optional(args, "--offset")?,
        order,
    })
}

fn number_option_u64_optional(args: &[String], name: &str) -> io::Result<u64> {
    let Some(value) = option_values(args, name).into_iter().next() else {
        return Ok(0);
    };
    value.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} must be an integer", name),
        )
    })
}

fn number_option_u64(args: &[String], name: &str) -> io::Result<u64> {
    let value = one_option(args, name)?;
    value.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} must be an integer", name),
        )
    })
}

fn main() {
    if let Err(error) = run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        if error.kind() == io::ErrorKind::BrokenPipe {
            return;
        }
        // Preserve the command failure even if its error output has no reader.
        let _ = output::write_line(
            &json::Object::new()
                .name("disposition", "error")
                .text("reason", &error.to_string())
                .finish(),
        );
        output::diagnostic(format_args!("ebira: {}", error));
        std::process::exit(1);
    }
}
