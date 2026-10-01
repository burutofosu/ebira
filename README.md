# Ebira

Ebira searches Claude Code and Codex conversation logs. Use it to find what
you said, recover a session after context compaction, and read the original
record behind a result.

The original JSONL logs are the source of truth. Ebira builds a local
projection called a corpus, which can be rebuilt from them. Nothing is uploaded.

## Install and index

Requires Rust 1.89 or later:

```sh
cargo install --git https://github.com/burutofosu/ebira
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

## Use with an agent

Copy the individual skill directories from [skills/claude](skills/claude) into
`~/.claude/skills`, or from [skills/codex](skills/codex) into `~/.codex/skills`:

- `ebira-recent-recall`: recover the current task after compaction
- `ebira-full-survey`: research earlier sessions using original records

The `ebira` executable must be on `PATH`.

## Data and compatibility

Keep the corpus as private as your conversation logs. Keep the original logs
available for source reads and rebuilds; `ebira help import` explains how to
keep managed copies.

Claude Code and Codex log formats can change and may require an Ebira update.
See [ARCHITECTURE.md](ARCHITECTURE.md) for storage and implementation details.

## License

[Apache-2.0](LICENSE)
