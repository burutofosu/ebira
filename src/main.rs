mod ask;
mod commits;
mod context_text;
mod core;
mod corpus;
mod follow;
mod format;
mod imports;
mod json;
mod jsonl;
mod logfile;
mod output;
mod private_fs;
mod questions;
mod resume;
mod said;
mod search;
mod sessions;
mod time;
mod zstd;

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
    /// The one argument given without an option name, if the command takes one.
    positional: Option<&'static str>,
    flags: &'static [&'static str],
}

const ASK_DETAILS: &[&str] = &[
    "  <question>                   what to find out, in your own words",
    "  --quick                      the cheap model throughout, without the check for later changes:",
    "                               about a tenth of the cost, with more mistakes",
    "  --cwd <text>                 conversations whose working directory contains the text",
    "  --from <time>, --to <time>   a yyyy-mm-dd date or an RFC 3339 time",
    "  --term <text>                a search string; repeat it to name several (otherwise the",
    "                               strong model chooses them)",
    "  --no-api                     search only, and list the records the answer would read",
    "                               (needs --term; nothing is sent or spent)",
    "  --format json|text           the result as one JSON object (default) or as text",
    "",
    "Service and models (saved by `ebira ask-setup`; a flag or an environment variable wins):",
    "  --api <name>                 anthropic (default), openai, gemini, openrouter, or compatible:",
    "                               any service or local server with the OpenAI Chat Completions API",
    "  --api-url <url>              the base URL; compatible needs it (http://localhost:11434/v1)",
    "  --api-key-env <name>         the variable that holds the key, if not the service's own:",
    "                               ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY,",
    "                               OPENROUTER_API_KEY, or EBIRA_ASK_API_KEY (compatible)",
    "  --strong-model <id>          plans the search and writes the answer (default claude-opus-5-5)",
    "  --cheap-model <id>           screens and drafts, and with --quick does everything",
    "                               (default claude-haiku-5-5)",
    "The variables: EBIRA_ASK_API, EBIRA_ASK_API_URL, EBIRA_ASK_API_KEY_ENV, EBIRA_ASK_STRONG_MODEL,",
    "EBIRA_ASK_CHEAP_MODEL. A service other than anthropic needs both models chosen. Prices are",
    "known for Claude models only; other calls are recorded with their tokens and no price.",
    "",
    "One step's model, and breadth:",
    "  --plan-model <id>            chooses search strings (default: the strong model)",
    "  --screen-model <id>          screens the records found (default: the cheap model)",
    "  --draft-model <id>           writes the draft that feeds the second round (the cheap model)",
    "  --answer-model <id>          writes the answer (default: the strong model)",
    "  --effort <level>             low | medium | high | xhigh | max for the answer (default medium)",
    "  --keep <n>                   ranked records to read (default 40; recent messages may add more)",
    "  --person-per-term <n>        the person's messages per search string (default 60)",
    "  --agent-per-term <n>         the agents' replies per search string (default 40; 0 for none)",
    "  --candidates <n>             records screened per round (default 600)",
    "",
    "Also reads up to 8 recent human messages from each of the 4 newest related sessions,",
    "within the date/directory bounds. The result reports the coverage and any unread tail.",
    "A reply cut at max_tokens is retried once with twice the output allowance (up to 64000).",
    "Both billed attempts are recorded, including a failed attempt.",
    "",
    "The question and the text of the records read go to the API you name (Anthropic's unless",
    "--api or `ebira ask-setup` says otherwise) under your key; nothing else leaves the computer.",
    "Every call is written to a ledger that `ebira ask-usage` sums.",
];

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
    "  --query <text>               search: repeat it to find several texts in one pass over the",
    "                               corpus, each with the matches a search for it alone gives",
    "  --raw                        compare one query with the original JSONL records",
    "  --ignore-case                fold ASCII letters",
    "  --cwd <text>                 records whose working directory contains the text",
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
    "--cwd",
    "--role",
    "--kind",
    "--sender",
    "--from",
    "--to",
    "--order",
    "--date-limit",
    "--date-offset",
];

/// `search` takes what `history` takes, and `--format`.
const SEARCH_COMMAND_DETAILS: &[&str] = &[
    "  --query <text>               search: repeat it to find several texts in one pass over the",
    "                               corpus, each with the matches a search for it alone gives",
    "  --raw                        compare one query with the original JSONL records",
    "  --ignore-case                fold ASCII letters",
    "  --cwd <text>                 records whose working directory contains the text",
    "  --date-limit <n>             dates per page of the date map (default 12)",
    "  --date-offset <n>            the first date of that page",
    "  --format json|text           one JSON object (default), or the matches as text, with times",
    "                               in the corpus's zone",
];

const SEARCH_COMMAND_VALUES: &[&str] = &[
    "--corpus",
    "--query",
    "--limit",
    "--offset",
    "--session",
    "--source-id",
    "--cwd",
    "--role",
    "--kind",
    "--sender",
    "--from",
    "--to",
    "--order",
    "--date-limit",
    "--date-offset",
    "--format",
];

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "sync",
        synopsis: "[--source <path>]... [--rebuild] [--rebuild-source <path>]... [--tool-output-chars <n>]",
        summary: "Create the corpus, or add what the logs gained since the last sync",
        details: &[
            "  --source <path>              a JSONL file (.jsonl or .jsonl.zst) or a directory of them,",
            "                               kept for later syncs;",
            "                               without it the first sync reads the Claude Code and Codex",
            "                               transcript directories",
            "  --rebuild                    build the corpus again; with --source, from exactly those",
            "  --rebuild-source <path>      read again the logs at a path: one log, or all under a directory",
            "  --tool-output-chars <n>      characters kept from each tool output (default 300)",
        ],
        values: &["--corpus", "--source", "--rebuild-source", "--tool-output-chars"],
        positional: None,
        flags: &["--rebuild"],
    },
    CommandSpec {
        name: "status",
        synopsis: "",
        summary: "Show the corpus, its sources and imports, and what is not yet indexed",
        details: &[],
        values: &["--corpus"],
        positional: None,
        flags: &[],
    },
    CommandSpec {
        name: "sessions",
        synopsis: "[--app claude|codex|other] [--cwd <text>] [--query <text>] [--ignore-case] [--from <time>] [--to <time>] [--order desc|asc] [--all] [--limit <n>] [--offset <n>] [--format json|text]",
        summary: "List the conversations: when each ran, where, how much the person said, and how it began",
        details: &[
            "  --cwd <text>                 sessions with a working directory that contains the text",
            "  --query <text>               sessions in which the person wrote the text",
            "  --from <time>, --to <time>   count only the records in that time; a session with none",
            "                               is left out",
            "  --order desc|asc             by when the session began (default desc, newest first)",
            "  --all                        also the sessions in which the person wrote nothing,",
            "                               such as threads another program started",
            "  --format json|text           one JSON object (default), or text with times in the",
            "                               corpus's zone",
            "",
            "A subagent's transcript and a Codex child thread count in their parent's session. The",
            "person's messages are counted as `said` lists them.",
        ],
        values: &[
            "--corpus",
            "--app",
            "--cwd",
            "--query",
            "--from",
            "--to",
            "--order",
            "--limit",
            "--offset",
            "--format",
        ],
        positional: None,
        flags: &["--ignore-case", "--all"],
    },
    CommandSpec {
        name: "said",
        synopsis: "[--session <id>] [--source-id <id>] [--cwd <text>] [--app claude|codex|other] [--query <text>]... [--ignore-case] [--from <time>] [--to <time>] [--order desc|asc] [--limit <n>] [--offset <n>] [--max-chars <n>] [--format json|text] [--include-imported]",
        summary: "List the person's own messages, newest first",
        details: &[
            "  --query <text>               messages that hold the text; repeat it to look for",
            "                               several texts in one pass, each listed apart",
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
        positional: None,
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
        positional: None,
        flags: &["--brief"],
    },
    CommandSpec {
        name: "search",
        synopsis: "--query <text> [--query <text>]... [--raw] [--ignore-case] [--cwd <text>] [--order asc|desc] [--format json|text] [filters]",
        summary: "Find the records that contain a literal text, oldest first",
        details: SEARCH_COMMAND_DETAILS,
        values: SEARCH_COMMAND_VALUES,
        positional: None,
        flags: &["--raw", "--ignore-case"],
    },
    CommandSpec {
        name: "history",
        synopsis: "--query <text> [--query <text>]... [--raw] [--ignore-case] [--cwd <text>] [--order asc|desc] [filters]",
        summary: "Show on which dates a literal text appears",
        details: SEARCH_DETAILS,
        values: SEARCH_VALUES,
        positional: None,
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
        positional: None,
        flags: &["--rebuild"],
    },
    CommandSpec {
        name: "context",
        synopsis: "--source-id <id> --byte-start <n> --byte-len <n> [--before <n>] [--after <n>] [--format json|text]",
        summary: "Read an original record, and its neighbours, from a source reference",
        details: &["  --format json|text           raw original JSONL (default), or readable records with source references"],
        values: &[
            "--corpus",
            "--source-id",
            "--byte-start",
            "--byte-len",
            "--before",
            "--after",
            "--format",
        ],
        positional: None,
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
        positional: None,
        flags: &[],
    },
    CommandSpec {
        name: "commits",
        synopsis: "--repo <dir> [--from <date>] [--to <date>] [--limit <n>] [--offset <n>]",
        summary: "Find the commit ids mentioned in the records and resolve them with Git",
        details: &[],
        values: &["--corpus", "--repo", "--from", "--to", "--limit", "--offset"],
        positional: None,
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
        positional: None,
        flags: &[],
    },
    CommandSpec {
        name: "ask",
        synopsis: "<question> [--quick] [--api <name>] [--cwd <text>] [--from <time>] [--to <time>] [--term <text>]... [--no-api] [--format json|text]",
        summary: "Answer a question about past work from the records, with quotes (through a model's API)",
        details: ASK_DETAILS,
        values: &[
            "--corpus",
            "--cwd",
            "--from",
            "--to",
            "--term",
            "--format",
            "--plan-model",
            "--screen-model",
            "--draft-model",
            "--answer-model",
            "--effort",
            "--keep",
            "--person-per-term",
            "--agent-per-term",
            "--candidates",
            "--api",
            "--api-url",
            "--api-key-env",
            "--strong-model",
            "--cheap-model",
        ],
        positional: Some("<question>"),
        flags: &["--quick", "--no-api"],
    },
    CommandSpec {
        name: "ask-setup",
        synopsis: "[--no-open]",
        summary: "Open a page on this computer to choose the API, key, and models `ask` uses",
        details: &[
            "  --no-open                    print the page's address without opening a browser",
            "",
            "The page checks a key with the service's model list, which costs nothing, and saves",
            "the service, the key, and the models as Windows user environment variables, from",
            "Windows or from WSL. `ask` reads them there itself, also in a program started before",
            "they were saved; a flag or a variable set in the environment comes first. Elsewhere,",
            "the page shows the variables to set.",
        ],
        values: &[],
        positional: None,
        flags: &["--no-open"],
    },
    CommandSpec {
        name: "ask-usage",
        synopsis: "[--format json|text]",
        summary: "Show what `ask` has spent, from its ledger",
        details: &[
            "  --format json|text           one JSON object (default) or text",
            "",
            "Set EBIRA_ASK_BUDGET_USD to see what is left of a credit.",
        ],
        values: &["--format"],
        positional: None,
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
        text.push_str(&format!("  {:<11}{}\n", spec.name, spec.summary));
    }
    text.push_str(
        "\nThe commands that read a corpus take --corpus <dir> (default: $EBIRA_CORPUS, else the\n\
         platform data directory). Every command writes one JSON object. `ebira help <command>`\n\
         lists its options.",
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
    if spec.values.contains(&"--corpus") {
        text.push_str("\n  --corpus <dir>               the corpus (default: $EBIRA_CORPUS)");
    }
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
    data_dir().map(|path| path.join("corpus")).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            if cfg!(windows) {
                "neither EBIRA_CORPUS nor LOCALAPPDATA names a corpus location"
            } else {
                "neither EBIRA_CORPUS, XDG_DATA_HOME, nor HOME names a corpus location"
            },
        )
    })
}

/// The platform data directory: `%LOCALAPPDATA%\ebira` on Windows, else `$XDG_DATA_HOME/ebira`
/// or `~/.local/share/ebira`.
fn data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .map(|path| path.join("ebira"))
    }
    #[cfg(not(windows))]
    {
        if let Some(path) = std::env::var_os("XDG_DATA_HOME").filter(|path| !path.is_empty()) {
            return Some(PathBuf::from(path).join("ebira"));
        }
        std::env::var_os("HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .map(|path| path.join(".local").join("share").join("ebira"))
    }
}

/// Where `ask` keeps its spending ledger (`usage.tsv`) and each request's files while it runs
/// (`tmp`): `ask` in the data directory, apart from any corpus.
fn ask_dir() -> io::Result<PathBuf> {
    data_dir().map(|path| path.join("ask")).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no data directory for the ask ledger: LOCALAPPDATA (Windows) or HOME is not set",
        )
    })
}

/// `EBIRA_ASK_BUDGET_USD`, the credit `ask-usage` counts the spending against, if set.
fn ask_budget() -> io::Result<Option<f64>> {
    let Some(value) = std::env::var("EBIRA_ASK_BUDGET_USD")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    value.trim().parse::<f64>().map(Some).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("EBIRA_ASK_BUDGET_USD must be a number of dollars, got {value}"),
        )
    })
}

/// `--format json` (the default) or `--format text`: whether the result is text.
fn text_format(args: &[String]) -> io::Result<bool> {
    match option_values(args, "--format")
        .into_iter()
        .next()
        .as_deref()
    {
        None | Some("json") => Ok(false),
        Some("text") => Ok(true),
        Some(value) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--format must be json or text, got {}", value),
        )),
    }
}

/// A number option bounded to `low..=high`.
fn bounded_number(
    args: &[String],
    name: &str,
    default: usize,
    low: usize,
    high: usize,
) -> io::Result<usize> {
    let value = number_option(args, name, default)?;
    if (low..=high).contains(&value) {
        Ok(value)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} must be from {} to {}, got {}", name, low, high, value),
        ))
    }
}

/// The service `ask` sends its requests to, and its models. Each comes from its flag, else from
/// its variable in this process (set even to nothing, it wins: the default then applies), else
/// from what `ebira ask-setup` saved. A step's own model flag comes after all of them.
fn ask_service(args: &[String], options: &mut ask::Options) -> io::Result<()> {
    let setting = |flag: &str, variable: &str| {
        option_values(args, flag)
            .into_iter()
            .next()
            .or_else(|| match std::env::var_os(variable) {
                Some(value) => Some(value.to_string_lossy().into_owned()),
                None => ask::saved_setting(variable),
            })
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    options.endpoint = ask::Endpoint::named(
        setting("--api", "EBIRA_ASK_API")
            .as_deref()
            .unwrap_or("anthropic"),
        setting("--api-url", "EBIRA_ASK_API_URL").as_deref(),
        setting("--api-key-env", "EBIRA_ASK_API_KEY_ENV").as_deref(),
    )
    .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    // The strong model plans and answers, the cheap one screens and drafts; --quick has the
    // cheap one do everything.
    let quick = has_flag(args, "--quick");
    let strong = setting("--strong-model", "EBIRA_ASK_STRONG_MODEL");
    let cheap = setting("--cheap-model", "EBIRA_ASK_CHEAP_MODEL");
    let planner = if quick { &cheap } else { &strong };
    if let Some(model) = planner {
        options.plan_model = model.clone();
        options.answer_model = model.clone();
    }
    if let Some(model) = &cheap {
        options.screen_model = model.clone();
        if options.draft_model.is_some() {
            options.draft_model = Some(model.clone());
        }
    }
    if options.endpoint.name == "anthropic" {
        return Ok(());
    }
    // The defaults are Claude models: another API needs its own named, before anything is sent.
    let named = |flag: &str| !option_values(args, flag).is_empty();
    let unnamed: Vec<&str> = [
        ("--plan-model", planner.is_some()),
        ("--answer-model", planner.is_some()),
        ("--screen-model", cheap.is_some()),
        ("--draft-model", quick || cheap.is_some()),
    ]
    .into_iter()
    .filter(|(flag, covered)| !covered && !named(flag))
    .map(|(flag, _)| flag)
    .collect();
    if unnamed.is_empty() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "--api {} needs its models named: {} (or each step's: {}), or choose them once with \
             `ebira ask-setup`",
            options.endpoint.name,
            if quick {
                "--cheap-model <id>"
            } else {
                "--strong-model <id> and --cheap-model <id>"
            },
            unnamed.join(", ")
        ),
    ))
}

fn ask_options(args: &[String], question: String) -> io::Result<ask::Options> {
    let mut options = if has_flag(args, "--quick") {
        ask::Options::quick(question)
    } else {
        ask::Options::thorough(question)
    };
    let from = option_values(args, "--from").into_iter().next();
    let to = option_values(args, "--to").into_iter().next();
    // Read now, so that a bad date fails before anything is spent.
    time::Range::parse(from.as_deref(), to.as_deref(), 0)?;
    options.filters = ask::Filters {
        cwd: option_values(args, "--cwd").into_iter().next(),
        from,
        to,
    };
    options.terms = option_values(args, "--term");
    options.no_api = has_flag(args, "--no-api");
    options.text_format = text_format(args)?;
    if !options.no_api {
        ask_service(args, &mut options)?;
    }
    for (name, model) in [
        ("--plan-model", &mut options.plan_model),
        ("--screen-model", &mut options.screen_model),
        ("--answer-model", &mut options.answer_model),
    ] {
        if let Some(value) = option_values(args, name).into_iter().next() {
            *model = value;
        }
    }
    if let Some(model) = option_values(args, "--draft-model").into_iter().next() {
        options.draft_model = Some(model);
    }
    if let Some(effort) = option_values(args, "--effort").into_iter().next() {
        check_name(
            "--effort",
            &effort,
            &["low", "medium", "high", "xhigh", "max"],
        )?;
        options.effort = effort;
    }
    options.keep = bounded_number(args, "--keep", options.keep, 1, 120)?;
    options.person_per_term =
        bounded_number(args, "--person-per-term", options.person_per_term, 1, 400)?;
    options.agent_per_term =
        bounded_number(args, "--agent-per-term", options.agent_per_term, 0, 400)?;
    options.candidates = bounded_number(args, "--candidates", options.candidates, 1, 2000)?;
    if options.no_api && options.terms.iter().all(|term| term.trim().is_empty()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--no-api needs --term: choosing search strings takes the API",
        ));
    }
    Ok(options)
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

/// The argument given without an option name, for a command that takes one.
fn positional_value(spec: &CommandSpec, args: &[String]) -> Option<String> {
    spec.positional?;
    let mut index = 1;
    while index < args.len() {
        let argument = &args[index];
        if spec.values.contains(&argument.as_str()) {
            index += 2;
            continue;
        }
        if !argument.starts_with("--") {
            return Some(argument.clone());
        }
        index += 1;
    }
    None
}

fn validate_options(spec: &CommandSpec, args: &[String]) -> io::Result<()> {
    let mut index = 1;
    let mut positional_seen = false;
    while index < args.len() {
        let argument = &args[index];
        if spec.flags.contains(&argument.as_str()) {
            index += 1;
            continue;
        }
        if spec.positional.is_some() && !positional_seen && !argument.starts_with("--") {
            positional_seen = true;
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
            let queries = option_values(args, "--query");
            if queries.len() > 1 && has_flag(args, "--raw") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--raw takes one --query; several work with search and history of the corpus",
                ));
            }
            let _lock = corpus::read_lock(&root)?;
            let request = search_request(
                args,
                if command == "history" { "history" } else { "" },
                corpus::corpus_offset_minutes(&root)?,
            )?;
            if queries.len() > 1 {
                return search::run_many(&root, request, &queries);
            }
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
        "sessions" => {
            let root = corpus_path(args)?;
            let _lock = corpus::read_lock(&root)?;
            let offset_minutes = corpus::corpus_offset_minutes(&root)?;
            let (from, to, range) = range_options(args, offset_minutes)?;
            let app = option_values(args, "--app").into_iter().next();
            if let Some(app) = app.as_deref() {
                if !matches!(app, "claude" | "codex" | "other") {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("--app must be claude, codex, or other, got {}", app),
                    ));
                }
            }
            let request = sessions::SessionsRequest {
                app,
                cwd: option_values(args, "--cwd").into_iter().next(),
                query: option_values(args, "--query").into_iter().next(),
                fold_ascii_case: has_flag(args, "--ignore-case"),
                from,
                to,
                range,
                newest_first: order_option(args, true)?,
                all: has_flag(args, "--all"),
                limit: number_option(args, "--limit", 30)?,
                offset: number_option_u64_optional(args, "--offset")?,
                text_format: text_format(args)?,
                offset_minutes,
            };
            sessions::run(&root, &request)
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
                queries: option_values(args, "--query"),
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
                text_format(args)?,
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
        "ask" => {
            let root = corpus_path(args)?;
            let question = positional_value(spec, args)
                .map(|question| question.trim().to_string())
                .filter(|question| !question.is_empty())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "ask needs a question: ebira ask \"<question>\"",
                    )
                })?;
            let options = ask_options(args, question)?;
            let directory = ask_dir()?;
            let ledger = ask::usage::Ledger::new(directory.join("usage.tsv"));
            ask::run(&root, &options, &ledger, directory.join("tmp"))
        }
        "ask-setup" => {
            let directory = ask_dir()?;
            let ledger = ask::usage::Ledger::new(directory.join("usage.tsv"));
            ask::setup::run(
                directory.join("tmp"),
                &ledger,
                ask_budget()?,
                !has_flag(args, "--no-open"),
            )
        }
        "ask-usage" => {
            let ledger = ask::usage::Ledger::new(ask_dir()?.join("usage.tsv"));
            let spending = ledger.spending();
            if text_format(args)? {
                output::write(&spending.text(ledger.path(), ask_budget()?))
            } else {
                output::write_line(&spending.json(ledger.path(), ask_budget()?))
            }
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
        cwd: option_values(args, "--cwd").into_iter().next(),
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
        // `search` lists every record that holds the text, imported copies included.
        skip_imported: false,
        text_format: text_format(args)?,
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
