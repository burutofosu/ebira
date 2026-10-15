# Ebira

Ebira searches Claude Code and Codex conversation logs. Use it to find what
you said, recover a session after context compaction, and read the original
record behind a result.

The original JSONL logs are the source of truth. Ebira builds a local
projection called a corpus, which can be rebuilt from them. Nothing is uploaded,
except by `ebira ask`, which sends your question and the text of the records it
reads to the Anthropic API under your own key.

## Install and index

Prebuilt Linux x86-64 and Windows x86-64 binaries are available on
[GitHub Releases](https://github.com/burutofosu/ebira/releases/latest), together
with `SHA256SUMS.txt`. Extract the archive and put `ebira` or `ebira.exe` on
`PATH`. Linux and Windows are tested in CI; macOS has not been verified.

To build from source, install Rust 1.89 or later:

```sh
cargo install --locked --git https://github.com/burutofosu/ebira --tag v0.1.0
ebira sync
```

The first sync finds transcripts in `~/.claude/projects`, `~/.codex/sessions`,
and `~/.codex/archived_sessions`, respecting `CLAUDE_CONFIG_DIR` and `CODEX_HOME`.
Run `ebira sync` again to pick up recent work. Changes to Ebira's storage
format or reading rules trigger a rebuild from the logs.

## Use

```sh
# Your messages, newest first
ebira said --format text

# Recover a session's latest messages, replies, and tool calls
ebira resume --session <session-id> --brief

# Find a phrase in messages you sent
ebira search --query "retry budget" --sender human

# Search all projected records
ebira search --query "retry budget"

# Several phrases in one pass over the corpus, each listed apart
ebira said --query "retry budget" --query "backoff" --format text
```

Copy a session ID from `said` or a search result. Inside a running session,
it is also available as `CLAUDE_CODE_SESSION_ID` (Claude Code) or
`CODEX_THREAD_ID` (Codex).

`said` separates your messages from agent prompts, notifications, tool results,
and summaries. Filter it with `--session <id>`, `--cwd <project-path-part>`,
or `--query <text>`. Use `--limit` and `--offset` to page through results.

Search matches literal text. Add `--ignore-case` for ASCII case-insensitive
matching or `--from` and `--to` for dates. The corpus keeps selected fields
and shortened tool output; add `--raw` to search the original records.

To read a result's original record and its neighbours, copy the values from
its `source_ref`:

```sh
ebira context --source-id <source_id> --byte-start <byte_start> --byte-len <byte_len> --before 3 --after 3
```

If the log has been replaced or rewritten, sync and search again.
Results are JSON by default; `said --format text` gives a plain-text view.

Run `ebira help` for all commands and `ebira help <command>` for options.

## Ask a question

`ebira ask` answers a question about past work from the records, with quotes:

```sh
ebira ask "What did I decide about retries, and did I change it later?" --format text
```

Claude models choose search strings, screen what Ebira finds, and read the
records around the best of it. A draft answer feeds a second round of searching
and a check of your later messages for changes to each decision it reports.
Every quote is checked against the records before it is shown, and each finding
cites its `source_ref`.

It needs an Anthropic API key: set `ANTHROPIC_API_KEY`, or run `ebira ask-setup`
to check and save one from a page on this computer (on Windows and in WSL, it is
saved as a Windows user environment variable). The question and the text of the
records read, not images, go to the Anthropic API; nothing else leaves the
computer. With the defaults, Claude Opus 5.5 and Claude Haiku 5.5, a question
costs about $0.30 and takes about two minutes. `--quick` uses Claude Haiku 5.5
throughout for about a tenth of that, with more mistakes about what changed
later. `ebira ask-usage` sums what has been spent, from a ledger in the data
directory. `--no-api` with `--term` searches only and lists the records an
answer would read.

## Logs in other locations

Add a JSONL file or directory. It is remembered for later syncs:

```sh
ebira sync --source /path/to/transcripts
ebira status
```

`status` shows the corpus location and its sources. Use `--corpus <directory>`
on any command, or set `EBIRA_CORPUS`, to select a different corpus.

## Try the example

From a checkout of this repository:

```sh
ebira sync --corpus demo-corpus --source examples/claude-session.jsonl
ebira said --corpus demo-corpus --format text
```

This lists three messages from a made-up session, including retry conditions
that its compaction summary left out.

Excerpt (message text only, newest first):

```text
Now make the retry limit configurable.
Log each retry with its delay.
Add retries to the upload client. Keep the total wait under 30 seconds, and never retry a 4xx response.
```

## Use with an agent

Copy the individual skill directories from [skills/claude](skills/claude) into
`~/.claude/skills`, or from [skills/codex](skills/codex) into `~/.agents/skills`:

- `ebira-recent-recall`: recover the current task after compaction
- `ebira-full-survey`: research earlier sessions using original records,
  starting with `ebira ask` when an API key is set

These user-level install locations are documented in the
[Claude Code skills guide](https://code.claude.com/docs/en/skills) and the
[Codex skills guide](https://learn.chatgpt.com/docs/build-skills).

The `ebira` executable must be on `PATH`.

## Data and compatibility

Keep the corpus as private as your conversation logs. Keep the original logs
available for source reads and rebuilds; `ebira help import` explains how to
keep managed copies.

Claude Code and Codex log formats can change and may require an Ebira update.
See [ARCHITECTURE.md](ARCHITECTURE.md) for storage and implementation details.

## License

[Apache-2.0](LICENSE)
