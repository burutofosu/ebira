# Architecture

Ebira indexes JSONL records into a compact corpus and retains byte locations for
source retrieval. All commands run locally.

## Modules

| Module | Responsibility |
| --- | --- |
| `main.rs` | The command table (help and accepted options), defaults, and dispatch |
| `jsonl.rs` | JSON parsing and scalar field extraction |
| `core.rs` | Event classification, field selection, and turn state |
| `format.rs` | Corpus record encoding and decoding |
| `time.rs` | Timestamps, dates, offsets, and `--from`/`--to` ranges |
| `corpus.rs` | Source discovery, indexing, checkpoints, and generated files |
| `imports.rs` | Managed JSONL imports and provenance records |
| `search.rs` | Literal search, timeline reads, raw scans, and context reads |
| `resume.rs` | Current-turn recovery and the brief recovery view |
| `said.rs` | The person's own messages, by session, project, agent, or topic |
| `follow.rs` | Waiting for the next message in another agent's transcript, by sender |
| `commits.rs` | Commit-name extraction and Git resolution |

The core module receives parsed values and returns state. Filesystem access,
process execution, JSON parsing, and output formatting remain in the surrounding
modules.

## Storage

Everything a corpus needs lives in its directory, the `--corpus` path:

- `ebira.lock`: the lock that orders writers and readers
- `sources.tsv`, `source-inputs.tsv`, `source-availability.tsv`, `timeline.tsv`:
  the catalog, the registered sources, their availability, and the date map
- `segments/*.corpus`: the projected events
- `managed-imports.tsv` and `managed-imports/<import-id>/{files.tsv,jsonl/**}`:
  copies of imported logs and their provenance

The managed imports are durable: they are the only copy of logs whose originals
are gone. Everything else is generated from the logs and the imports, carries
the storage format version, and is rebuilt by `ebira sync` when the format or
the reading rules change. Paths to managed files are relative to the corpus
directory, so the directory can be moved as a whole.

### Events

A segment holds events, each an `@ebira` header line of tab-separated
`key=value` fields followed by a body of `body_len` bytes and a newline. The
header names the source reference, session, turn, kind, sender, channel, and
whether the projection cut a value (`cut=1`). The body is a list of fields, each
`path\tlen\tvalue\n` with `len` the byte length of the value, so any value,
newlines included, reads back exactly (`format::push_field`, `format::body_fields`).

## Indexing

`ebira sync` reads the registered sources plus any `--source`. When no corpus
exists and no source is named, it registers the Claude Code and Codex transcript
directories that exist (`$CLAUDE_CONFIG_DIR/projects` or `~/.claude/projects`,
`$CODEX_HOME/sessions` and `archived_sessions` or the same under `~/.codex`).
`--rebuild` discards the projection and builds it again; with `--source` the
given sources replace the registered ones.

The logs are the source of truth. The catalog header records the storage format
(`v=`), the date offset (`tz=`), and the reading rules (`rules=`) the corpus was
made under. A sync that finds another format or other rules rebuilds the corpus
in full and names the `rebuild_cause`; the rules version changes with any change
to what a sync writes for the same logs, and a test of `core.rs` fingerprints the
classification and the dates of representative records so that such a change
cannot go unversioned. An incremental sync keeps the corpus's date offset.

Each source file goes through these operations:

```text
source path
  -> source kind (a `.claude` or `.codex` directory, else the first records)
  -> source origin (path, and the opening session_meta record for Codex)
  -> JSONL record reader
  -> scalar field extraction
  -> event kind and sender
  -> event and turn state
  -> corpus segment, catalog, and timeline entries
```

The source catalog stores a checkpoint for each JSONL file. Append-only updates
resume at the last complete record. An incomplete final line remains pending for
the next sync. The checkpoint includes the working directory in effect, so an
appended record without its own `cwd` inherits the thread's, as Codex records
after `session_meta` and `turn_context` do. A file that changed other than by
appending is projected again. A file that is gone is dropped; one under a
directory that could not be listed keeps its projection and is reported as
unreadable.

## Concurrency

`sync`, `import`, and `timeline --rebuild` hold the lock file `ebira.lock` in the
corpus directory exclusively; the commands that read the corpus hold it shared.
Two writers never interleave their catalog, registry, or segment updates, and a
reader never sees a sync half done. The lock is released when the process ends,
also abnormally. `follow` reads the transcript itself and takes no lock while it
waits. It reads with the reader a sync uses (`corpus::LogReader`), starting from
the checkpoint the catalog holds for that transcript, so its messages carry the
sender, channel, timestamp, and byte range the corpus gives the same records.

## Senders

The origin of a whole source is decided first: a Claude Code transcript, a
Claude Code subagent transcript (`subagents/`), a Codex thread, a Codex child
thread (`parent_thread_id` or a `subagent` source), or a Codex thread started
by `codex exec`. Each record then gets a sender and a channel (`via`) from its
own markers:

- Claude Code user records: `isCompactSummary` is a summary, a `tool_result`
  is a tool result, `isMeta` is injected, subagent transcripts carry the
  parent's prompts. The remaining text, with `<system-reminder>` blocks
  removed, is the person's unless it opens with a tag the harness writes
  (`task-notification`, `agent-message`, `local-command-stdout`, and similar).
  `queued_command` attachments are the person's prompts typed while the agent
  was working, unless they are notifications.
- Codex user messages: child and `codex exec` threads carry another agent's
  prompts. In other threads, context blocks the app injects
  (`environment_context`, `codex_internal_context`, `heartbeat`, and similar)
  are system text, relayed agent messages are agent text, and the rest, with
  `<in-app-browser-context>` removed, is the person's. Messages in turns named
  `external-import-turn-*` are copies of another agent's conversation.
- A message that opens with Claude Code's compaction sentence is a summary in
  either format. Codex's `compacted` record is a summary when its `message`
  holds text, and otherwise the boundary, like Claude Code's `compact_boundary`.
  These two record types are the compactions `resume` counts (`core::is_compaction`).
- A prompt typed while the agent was working (`queued_command`) is a user-role
  record whoever wrote it, as a typed one is; the sender tells them apart.
- A record's type comes from fixed paths (`/payload/type`, an envelope's
  `type`, then the top-level `/type`), never from a `type` nested deeper.
- Which agent wrote a log (`claude`, `codex`, `other`) is decided by the nearest
  `.claude` or `.codex` directory above it, else by its first records, and only
  once it holds a complete record: the agent is part of the source id, which
  must not change. Model reasoning (thinking blocks, Codex reasoning records)
  is not projected.

A person's message is projected as its text only, length-delimited so that
multi-line text reads back exactly, with images counted rather than stored. The
text is kept as sent; where an injected block opened or closed it, the whitespace
around the cut goes with the block. A message of images alone is the person's in
either format.
Only a person's message (or a record in a format without sender markers) opens
a turn, so notifications, injected context, and summaries stay inside the
person's turn.

Source replacement, truncation, and changes observed during a sync receive
separate dispositions. `--rebuild-source` rebuilds one source projection.

The source catalog is written before processing starts so interrupted builds can
resume pending paths. Partial corpus segments are removed during recovery.

## Managed imports

`ebira import` copies `.jsonl` files into the managed store and writes two
registries:

- The top-level registry records the import ID, provenance, label, source
  computer, original path, import time, and managed directory.
- The per-import registry records each source path, relative managed path, size,
  and modification time.

Every sync includes all registered managed imports, and `ebira status` lists
them. Missing or unreadable managed files remain visible through command
dispositions and status output.

## Source references

Each projected event contains a `SourceRef` with a source ID, byte offset, and
byte length. `ebira context` uses this location to read the corresponding JSONL
record. Managed imports resolve to the copied JSONL.

A context read verifies the source state recorded during indexing. Changed
sources return `source_changed_since_projection`.

## Scope and freshness

A read covers one scope, resolved by `corpus::scope`: the source named by
`--source-id`, the sources of a `--session`, or all of them. A command reads the
segments of its scope and reports the coverage of the same sources.

What the logs of a scope hold that the corpus has not read is judged by one test,
`corpus::freshness`, the one a sync makes before it reads a log again. A log is
`current` when it is the file the corpus read, unchanged; `behind` when it has
grown, by the bytes after what was read; `rewritten` when it was replaced,
shortened, or changed in place, in which case all of it is read again; and
`unreachable` when it cannot be read. `search`, `history`, `timeline`, and `said`
report the stale sources and unscanned bytes of their scope; `resume` reports its
transcript's freshness and whether to sync first. A transcript still being
written is `behind` until the next sync.

## Search

Projection search compares a literal query with the values of the selected
corpus fields, never with the paths and lengths that frame them in a body, and a
match names its field and quotes that value around it. `commits` reads names
from the same values, and timeline listings quote the start of them. Tool output
is shortened to `sync --tool-output-chars` characters (default 300).

Raw search uses corpus entries to select records, then compares the query with
the original JSONL bytes. The result reports unavailable sources, unreadable
records, source growth, and the observed time boundary.

Matches are chronological, or newest first with `--order desc`. Filters are
applied to source, session, role, event kind, sender, and time range. ASCII case
folding is optional.

The person's messages have one definition, in `core.rs`, which `said`,
`resume`, and `follow` share: records whose sender is the person, listed once
when the same text was stored twice with the same timestamp
(`core::SeenMessages`). Imported copies (`via: imported`) are counted but listed
only when requested. A message is read whole, however long. `said` reads only
such events; a session scope reads the segments of the session's sources, and
the other filters apply to event headers.

A `--session` is resolved once, by `corpus::session_sources`: the session's
sources are those whose records declare it, which takes in the transcripts of the
agents it started (Claude Code subagents and Codex child threads record their
parent's session), and its main transcript is the one source outside
`subagents/` whose file name carries the session id. `said` reads all of them;
`resume` and `follow` read the main transcript. `follow` looks for a file named by
the session under the registered source roots only when the corpus has not read
the session yet.

## Timeline and dates

`timeline.tsv` stores date runs, event counts, session counts, kind counts, and
record locations. The corpus header records a fixed timezone offset. A build from
scratch uses `EBIRA_TZ_OFFSET`; incremental syncs and reads use the offset stored
in the corpus.

Time has one reading, in `time.rs`, used for dates, ordering, and ranges alike. A
timestamp names an instant; one written without a zone is local time at the corpus
offset. A record whose timestamp cannot be read is `undated`: it is outside every
date and every bounded range, and it sorts after the dated records. A `--from` or
`--to` written as a date is that whole local day; one written as a time is that
instant. A bound that cannot be read is an error, and so is an `EBIRA_TZ_OFFSET`
that is not an offset.

## Turn recovery

Turn state is tracked per session because records from multiple sessions can be
interleaved. Inferred turn identifiers use the byte offset of the record that
opened the turn, which keeps them stable across rebuilds.

Resume output reports the observed source boundary, current turn state,
retention counts, truncation flags, and the next read position. A session id
resumes its main transcript; without one, a single source of the session, and
otherwise the candidates are listed. Every segment of the source is read, whether it was named by session or
by source id, so both give the same result. The scan also keeps the person's
latest messages, the latest replies and tool calls across turns, and the
compaction count; `--brief` prints only those, reading reply text and tool
inputs from the original records.

## Commit lookup

The `commits` command extracts hexadecimal names from projected records and asks
Git which names resolve to commits in the selected repository. It returns the
resolved object IDs and the earliest source references for each commit.

## Output

Commands write one JSON object to standard output; `help`, `--version`, and
`said --format text` write plain text. `disposition` identifies the command
result. Errors use `disposition: "error"` with a `reason` field and exit with
status 1. A `next_actions` entry names a command and its options without the
leading dashes, for example `{"action":"sync","request":{"corpus":...}}`.

Counts and limits describe the current command. Corpus byte totals describe the
current generated projection.
