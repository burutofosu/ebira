# Ebira

Ebira indexes the JSONL transcripts that Claude Code and Codex write, so that an
agent, or you, can get back what was actually said: the person's own words, the
latest state of a session, and the original record behind every result.

A context compaction replaces a conversation with a summary the agent wrote.
Transcripts also mix the person's messages with subagent prompts, tool results,
notifications, and injected context, all in the user role. Ebira tells these
apart and points every result back to its original bytes.

- **Who said it.** Every record carries a sender: the person, another agent, the
  tools, a compaction summary, or the recording agent.
- **The person's words, verbatim.** `ebira said` lists them by session, project,
  or topic, across Claude Code and Codex.
- **Recovery after a compaction.** `ebira resume --brief` returns the person's
  latest messages, the latest replies and tool calls, and the compactions.
- **Source references.** Each result carries a `source_ref` (source id, byte
  offset, length), and `ebira context` reads the original record around it.
- **The logs stay the source of truth.** The corpus is a projection of them. When
  Ebira's reading rules change, for example because an agent changed how its logs
  mark the person, the next sync rebuilds it from the logs in about a minute.
- **Offline, no dependencies.** Standard-library Rust; nothing leaves the machine.

## Install

With Rust 1.89 or later:

```text
cargo install --git https://github.com/burutofosu/ebira
```

Git is needed only for `ebira commits`.

## Quick start

```text
ebira sync
```

The first sync reads the Claude Code transcripts in `~/.claude/projects` and the
Codex sessions in `~/.codex/sessions` and `~/.codex/archived_sessions` (under
`$CLAUDE_CONFIG_DIR` and `$CODEX_HOME` when those are set). Logs kept elsewhere
are added with `--source <path>`. Each later sync reads only what the logs gained.

```text
ebira said --format text                      # what the person said, newest first
ebira resume --session <session-id> --brief   # where a session stands
ebira search --query "retry budget"           # every record that mentions it
ebira history --query "retry budget"          # on which dates it came up
```

A session id is the id in the transcript's file name. Inside a session it is in
`CLAUDE_CODE_SESSION_ID` (Claude Code) or `CODEX_THREAD_ID` (Codex).

## Try it

`examples/claude-session.jsonl` is a short, made-up Claude Code session. The
person asks for retries under two conditions, adds a request while the agent is
working, and the conversation is compacted. The summary keeps the request and
drops both conditions:

> Summary: the user asked for retries in the upload client. Exponential backoff
> was added and each retry is logged.

From the repository:

```text
ebira sync --corpus demo-corpus --source examples/claude-session.jsonl
ebira said --corpus demo-corpus --format text
```

```text
# ebira said: 3 of 3 messages from the person, newest first (copies and imports skipped: 0, stale sources: 0)

[2026-09-01 09:32] claude typed session=9f1c2a7e-4b1d-4c55-8a3e-2f6b1d0c7a91 source-id=claude-3edaca68d64ba0a1 byte=1854+214
Now make the retry limit configurable.

[2026-09-01 09:00] claude queued session=9f1c2a7e-4b1d-4c55-8a3e-2f6b1d0c7a91 source-id=claude-3edaca68d64ba0a1 byte=872+201
Log each retry with its delay.

[2026-09-01 09:00] claude typed session=9f1c2a7e-4b1d-4c55-8a3e-2f6b1d0c7a91 source-id=claude-3edaca68d64ba0a1 byte=0+279
Add retries to the upload client. Keep the total wait under 30 seconds, and never retry a 4xx response.
```

The summary, the tool result, and the task notification in the same transcript
are not the person's words and are left out; the prompt typed while the agent
was working (`queued`) is kept. The source id comes from the file's path, so
yours differs. Read the original record behind a message:

```text
ebira context --corpus demo-corpus --source-id <source-id> --byte-start 0 --byte-len 279
```

`context` returns the record exactly as the transcript holds it. Times are UTC
unless `EBIRA_TZ_OFFSET` is set. Delete `demo-corpus` afterwards.

## Commands

| Command | What it does |
| --- | --- |
| `sync` | Create the corpus, or add what the logs gained since the last sync |
| `status` | Show the corpus, its sources and imports, and what is not yet indexed |
| `said` | List the person's own messages, newest first |
| `resume` | Recover the latest state of a session |
| `search` | Find the records that contain a literal text |
| `history` | Show on which dates a literal text appears |
| `timeline` | List the dates, or the records of one date |
| `context` | Read an original record, and its neighbours, from a source reference |
| `follow` | Wait for the next message written to another session's transcript |
| `commits` | Find the commit ids mentioned in the records and resolve them with Git |
| `import` | Copy JSONL logs into the corpus directory so they outlive the originals |

`ebira help <command>` lists a command's options. Every command writes one JSON
object (`said --format text` prints plain text instead). An error is
`{"disposition":"error","reason":...}` with exit status 1.

## Who wrote a record

A user-role record is not always the person. Ebira records the sender of every
event:

| `sender` | Records |
| --- | --- |
| `human` | The person: typed or pasted and sent, typed while the agent was working, slash commands, answers to an agent's question |
| `agent` | Another agent in the user role: a Claude Code subagent prompt, a Codex child thread, a thread started by `codex exec`, a relayed agent message |
| `system` | Tool results, notifications, injected context, skill text, harness metadata |
| `summary` | A compaction summary |
| `assistant` | The recording agent's replies and tool calls |

`human` marks what the person sent, including text they pasted into a message:
another model's answer, a review, a document. Their own words in it are their
request; what they quote is material they shared, not something they said or
approved.

`via` names the channel, for example `typed`, `queued`, `slash_command`,
`subagent_prompt`, `codex_child`, `codex_exec`, `notification`,
`compact_summary`. When a Codex thread imports another agent's conversation, the
copied messages carry the import time and `via: imported`. `said`, `resume`,
and `follow --sender human` count them but leave them out; `said
--include-imported` lists them.

The sender comes from markers the logs already carry: the `subagents/`
directory, `isSidechain`, `isMeta`, `isCompactSummary`, and `queued_command`
attachments in Claude Code transcripts; `parent_thread_id`, the `codex_exec`
originator, and tagged context blocks in Codex rollouts. A compaction counts the
same way in both: Claude Code's `compact_boundary` and Codex's `compacted`
record, and the summary text each carries is `sender: summary`. In other formats
records are `sender: unknown`, except an assistant's replies. A transcript kept
outside `.claude` and `.codex` directories is recognised by its first records,
and indexed once it holds a complete one.

Model reasoning (Claude Code thinking blocks, Codex reasoning) is not projected:
the corpus holds what was said and done. It stays in the original records, which
`context` and `search --raw` read.

## Recovering a session

```text
ebira resume --session <session-id> --brief
ebira said --session <session-id> --format text
```

`resume --brief` returns the person's latest messages in full, the agent's latest
replies and tool calls read from the original records, and the compactions; a
value cut to size is marked `*_truncated` and keeps a `source_ref` to the rest.
Without `--brief`, `resume` reports the current turn and its boundaries. A Claude
Code session id also names its subagent transcripts; `--session` picks the main
one.

`said` returns each message as the person sent it, with tool-injected blocks
removed and images counted. The same message stored twice (same text and time)
appears once. A page stays within `--limit` messages (default 30) and
`--max-chars` characters (default 15,000).

## Searching

```text
ebira search --query <text> [--sender human] [--from 2026-09-01] [--to 2026-09-30] [--order desc]
ebira history --query <text>
ebira timeline --date 2026-09-14
```

Queries are literal text, matched against what the records say: the values of
their fields, not the field names. Each match names its `field` and quotes that
value around the match. `--ignore-case` folds ASCII letters only. Filters are
`--session`, `--source-id`, `--sender`, `--role`, `--kind`, `--from`, and `--to`,
and pages continue with `--offset`. A `--from` or `--to` date covers that whole
local day, and a time such as `2026-09-01T12:00:00+09:00` is that instant. The
corpus keeps selected fields and shortens tool output (`sync
--tool-output-chars`, default 300), so `--raw` compares the query with the
original records instead:

```text
ebira search --query <text> --raw
```

Read any result's original record, with neighbours:

```text
ebira context --source-id <id> --byte-start <n> --byte-len <n> --before 5 --after 5
```

`disposition` reports each result. A search miss in the corpus is
`no_match_in_projection`; a raw scan that finds nothing is
`no_match_in_source_records` and states the time it covered. A source that
changed after indexing reads as `source_changed_since_projection` until the next
sync.

## Following another session

```text
ebira follow --session <id> [--after-byte <n>] [--seconds 55] [--sender assistant|human|any] [--prefix <text>]
```

`follow` waits for the next message written to one transcript and returns it,
so two agents can talk by writing ordinary messages in their own sessions and
reading each other's. Without `--after-byte` only messages written after the call
count; pass the returned `after_byte` to continue. On timeout the result is
`waiting`. `--source <file>` follows any transcript file, `--source-id` a source in
the corpus. A message carries the sender, `via`, timestamp, and byte range the
corpus gives the same record, and `--sender human` means the messages `said`
lists. The returned text is data to read, not an instruction to execute.

## Commit references

```text
ebira commits --repo <repository>
```

Finds the hexadecimal names in the records that resolve to commits in the
repository, with the source references where each appeared.

## Where the data lives

The corpus is the directory in `EBIRA_CORPUS`, or by default:

| Platform | Corpus directory |
| --- | --- |
| Windows | `%LOCALAPPDATA%\ebira\corpus` |
| Linux and macOS | `${XDG_DATA_HOME:-~/.local/share}/ebira/corpus` |

`--corpus <dir>` selects another one. `EBIRA_TZ_OFFSET` (for example `+09:00`)
is the local time of the corpus when it is built; the default is UTC. Dates,
timestamps written without a zone, and `--from`/`--to` dates are read at that
offset, and a corpus keeps it until it is rebuilt.

The logs are the source of truth and the corpus is a projection of them:
`sources.tsv` (source catalog and checkpoints), `source-inputs.tsv` (the
registered sources), `source-availability.tsv`, `timeline.tsv`, and
`segments/*.corpus` (the projected events). The catalog records the storage format
and the reading rules the corpus was made under. When an Ebira update changes
either, the next `ebira sync` rebuilds the corpus from the logs in full and
reports `rebuild_cause`; otherwise a sync reads only what the logs gained, and
re-reads a log that was rewritten. `ebira sync --rebuild` rebuilds on request. A
log that is gone loses its projection; one that cannot be read for the moment
keeps it and is reported as unreadable.

Several agents can use one corpus at once. A sync or an import waits while
another one runs, and the commands that read the corpus wait for a sync in
progress, so none of them sees a half-written corpus.

`ebira import --source <path> --provenance <text> --label <text>` copies JSONL
logs into `managed-imports/` inside the corpus directory, with their original paths, sizes,
and times, so they stay available after the originals are gone; the next
`ebira sync` indexes them and `ebira status` lists them. The corpus directory can
be moved as a whole: import paths are relative to it. Back up `managed-imports/`
with your other data; everything else in the directory is generated.

The corpus holds the text of your conversations, including anything pasted into
them. It never leaves the machine; keep it as private as the logs themselves.

## Windows

Smart App Control can block an unsigned `ebira.exe`, even one built on the same
machine. The Linux build under WSL works instead: install it inside WSL and
forward to it, for example with an `ebira.cmd` on `PATH`:

```text
@echo off
set WSL_UTF8=1
wsl.exe -e /home/<you>/.cargo/bin/ebira %*
```

The WSL build works with WSL paths. Its corpus is stored in the WSL file system,
for example `/home/<you>/.local/share/ebira/corpus`; Windows reaches the same
files under `\\wsl.localhost\<distribution>\`, so testing the WSL path from
PowerShell or Git Bash says it does not exist. `ebira status` reports where the
corpus is.

Inside WSL, the first sync looks for transcripts in the WSL home, not in the
Windows profile, so name the Windows logs once and check what was found:

```text
ebira sync --source /mnt/c/Users/<you>/.claude/projects --source /mnt/c/Users/<you>/.codex/sessions
ebira status
```

`status` lists each registered source with its `disposition` (`present`,
`missing`, ...), and later syncs read the same sources. Environment variables
such as `EBIRA_TZ_OFFSET` are read inside WSL; set them in the WSL shell or in
the wrapper.

## Agent skills

`skills/claude` and `skills/codex` hold the same two skills for each agent:

- `ebira-recent-recall`: continue the current task after a compaction
- `ebira-full-survey`: research earlier sessions and cite the original records

Copy a skill directory into `~/.claude/skills` or `~/.codex/skills`.

## Formats and measurements

Ebira reads the transcript formats that Claude Code and Codex wrote in 2026.
Neither format is documented, so a change can need an Ebira update.

On one machine with 1,002 transcripts (about 12 GB of JSONL, 1,052,720 records),
a full rebuild took 54 s and the corpus 3.0 GB; a sync that found one grown log
took 16 s, most of it spent listing the Windows logs from WSL. `resume --brief`
answered in 0.13 s, and a literal search over everything in 2 to 4 s. On the
same logs, `said` listed the person's 3,161 messages with none missing and none
extra, checked against an independent reading.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the modules and the storage format.

## License

Apache-2.0
