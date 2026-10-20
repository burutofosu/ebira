# Ebira

Ebira searches Claude Code and Codex conversation logs. Use it to find what
you said, recover a session after context compaction, and read the original
record behind a result.

The original JSONL logs are the source of truth. Ebira builds a local
projection called a corpus, which can be rebuilt from them. Nothing is uploaded,
except by `ebira ask`, which sends your question and the text of the records it
reads to the model API you choose (Anthropic's by default) under your own key.

## Install and index

Prebuilt Linux x86-64 and Windows x86-64 binaries are available on
[GitHub Releases](https://github.com/burutofosu/ebira/releases/latest), together
with `SHA256SUMS.txt`. Extract the archive and put `ebira` or `ebira.exe` on
`PATH`. Linux (WSL included) and Windows are tested in CI; macOS has not been
verified. The binaries are unsigned, so Windows Smart App Control may block them;
building from source avoids that.

To build from source, install Rust 1.89 or later:

```sh
cargo install --locked --git https://github.com/burutofosu/ebira --tag v0.5.0
ebira sync
```

To update, install a newer release the same way, adding `--force`. The changes
are listed in [CHANGELOG.md](CHANGELOG.md).

The first sync finds transcripts in `~/.claude/projects`, `~/.codex/sessions`,
and `~/.codex/archived_sessions`, respecting `CLAUDE_CONFIG_DIR` and `CODEX_HOME`.
Run `ebira sync` again to pick up recent work. Changes to Ebira's storage
format or reading rules trigger a rebuild from the logs; a rebuild under new
reading rules keeps each log's source id, so references made before it still
read.

Codex compresses the logs of older threads with Zstandard (`.jsonl.zst`). Ebira
reads them as the JSONL they hold, under the same source ids and byte offsets, so
a reference made before a log was compressed still reads its record. A log an app
moves to another folder keeps its source id the same way, once Ebira has checked
that it holds the records it held before.

## Use

```sh
# Your messages, newest first
ebira said --format text

# Each with the end of the agent's reply before it and the agent's last reply after it
ebira said --context --format text

# The conversations in a project: when each ran, how much you said, how it began
ebira sessions --cwd <project-path-part> --format text

# Recover a session's latest messages, replies, and tool calls
ebira resume --session <session-id> --brief

# Find a phrase in messages you sent
ebira search --query "retry budget" --sender human

# Search all projected records
ebira search --query "retry budget"

# Several phrases in one pass over the corpus, each listed apart
ebira said --query "retry budget" --query "backoff" --format text

# Several date maps in one scan
ebira history --query "terminal disappeared" --query "terminal not found"
```

Copy a session ID from `said` or a search result. Inside a running session,
it is also available as `CLAUDE_CODE_SESSION_ID` (Claude Code) or
`CODEX_THREAD_ID` (Codex).

`said` separates your messages from agent prompts, notifications, tool results,
and summaries. Filter it with `--session <id>`, `--cwd <project-path-part>`,
or `--query <text>`. Use `--limit` and `--offset` to page through results.
A short reply means nothing by itself: "go with that" agreed to something the
agent had just proposed. `--context` shows under each message the last 200
characters of the agent's reply before it and the agent's last reply in the turn
it opened, cut to 240 characters around its middle, with how many replies the
turn held and a `source_ref` for reading each whole; tool calls and thinking are
not replies. Their characters count toward the page's `--max-chars`.

Search matches literal text. Add `--ignore-case` for ASCII case-insensitive
matching or `--from` and `--to` for dates. The corpus keeps selected fields
and shortened tool output; add `--raw` to search the original records.

To read a result's original record and its neighbours, copy the values from
its `source_ref`:

```sh
ebira context --source-id <source_id> --byte-start <byte_start> --byte-len <byte_len> --before 3 --after 3 --format text
```

The text view keeps the sender, timestamp and original byte reference for each
record, omits reasoning and non-message metadata, and reads full tool output from
the original rather than its shortened corpus preview. It needs complete indexed
records. Omit `--format text` (or use `--format json`) for the original raw JSONL,
including metadata or an arbitrary byte range.

If the log has been replaced or rewritten, sync and search again. A log rewritten
in place where the record was is named in the result, to read again with
`ebira sync --rebuild-source <path>`.
Results are JSON by default. Multiple queries work with `said`, `search`, and
`history`; `--raw` takes one query.

`said`, `search`, and `context` also take `--format text`, which gives times in
the corpus's zone and names it: the offset in `EBIRA_TZ_OFFSET` when the corpus
was built, else UTC. JSON results keep each record's `timestamp` as its log wrote
it, and the lists of records add `local_time` in the corpus's zone.

Run `ebira help` for all commands and `ebira help <command>` for options.

## Ask a question

`ebira ask` answers a question about past work from the records, with quotes:

```sh
ebira ask "What did I decide about retries, and did I change it later?" --format text
```

Two models, Claude Opus 5.5 and Claude Haiku 5.5 unless others are chosen, pick
the search strings, screen what Ebira finds, and read the records around the best
of it. A draft answer feeds a second round of searching
and a check of your later messages for changes to each decision it reports.
It also reads the last eight human messages in up to four of the newest related
sessions, without requiring those messages to repeat a search word. Date and
directory bounds still apply. This adds at most 32 records to the ranked search
results; `search.recent_messages` reports how many were read, the time range and
stale sources, and `recent_sessions_omitted` names the remaining count.
This is bounded evidence, not proof that every later decision was found.

Your own messages are few next to the agents' records (a few thousand in a
history of a million records), so `ask` does not leave them to the search
strings: by default it screens every message you wrote in the scope, the newest
4,000 at most, with the cheap model, in the order written, and the ones judged to
bear on the question join the records read. Each message is screened with the end
of the agent's reply before it and the agent's last reply in its turn, so that a
short reply ("go with that") is judged by what it answered; a long message is
screened in overlapping pieces, and the answer is shown the piece judged relevant.
Those beyond what the answer reads with their context are given to it in one line
each (`search.sweep_digest`): the most relevant first, one session at a time,
identical messages once with how many times they were sent, within a character
budget. `--no-sweep-context`, `--no-sweep-chunks`, and `--delivery current` read
as 0.5.0 did. The check for later changes then reads every later message of yours
instead of searching for them. With `--cwd`, the scope is every message of the sessions that
worked in that directory, also those written while a session was elsewhere.
`search.sweep` reports how many messages were screened and judged relevant, and
`search.coverage` which sessions in the scope the records read come from.
`--no-sweep` turns this off, and `--sweep-limit <n>` changes the limit. The
agents' records are still found by the search strings: when no record holds any
of them, `ask` chooses others once, and a string held by more records than were
taken is reported with the time before which none was read.

`--export <path>` writes, as text, the records the answer read and every message
of yours judged to bear on the question, whole, for reading elsewhere.

Each search string is also searched, in the same pass, with a space and with none
between Latin letters or digits and Japanese (`CSV 取り込み` and `CSV取り込み`):
Japanese is written both ways, and the search matches literally. When the screen
judges none of the records found relevant, the result says first that they do not
settle the question, with the strings searched and how many records they found
(`not_settled`), and the answer reads the closest of them with the records around
them (`disposition: "nothing_relevant"`); after the sweep, those closest are your
own messages. When no record holds any search string and none of your messages was
judged to bear on the question, the result says so first. With `--no-sweep`, no
record found means no further call (`no_records`).

Every quote is checked against the records before it is shown, and each finding
cites its `source_ref`. A verified quote confirms the quoted words, not the
answer's interpretation or completeness. Check important conclusions against the
original records. [evals](evals/README.md) holds fifty-four questions about made-up logs,
with the answers known, to measure what a change to `ask` does.

It needs an API key. Run `ebira ask-setup` to choose the service (Anthropic by
default), check its key with the service's model list, which costs nothing, and
choose the models, on a page on this computer. On Windows and in WSL they are
saved as Windows user environment variables, and `ebira ask` reads them there
itself, so an agent started before they were saved need not pass them; elsewhere,
the page shows the variables to set. For the defaults, setting
`ANTHROPIC_API_KEY` is enough.

Answers give times in the corpus's zone. The question and the text of the records
read, not images, go to the chosen API; nothing else leaves the computer. With the
defaults, Claude Opus 5.5 and Claude Haiku 5.5, a question costs about 35 to 45 US
cents and takes one to three minutes. `--quick` uses the cheap model throughout
for about a tenth of that, with more mistakes about what changed later.

A reply cut at `max_tokens` is retried once with twice the output allowance, up to
64,000 tokens, using the same model and evidence. Both billed attempts are
recorded, including failures; an invalid response or refusal is not retried by
this output-limit handling. `ebira ask-usage` sums what has been spent, from a
ledger in the data directory, per question and apart from questions (lines
written before questions were recorded, or by another program). `--no-api` with
`--term` searches only and lists the records an answer would read.

### Other services

`ebira ask-setup` also offers OpenAI, Google Gemini, OpenRouter, and any other
service or local server that speaks the OpenAI Chat Completions API (Ollama,
LM Studio, vLLM, a proxy). Each keeps its key in its own variable
(`OPENAI_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, or `EBIRA_ASK_API_KEY`
for another service; a local server may need none) and needs two models chosen
from its list: a strong one that chooses the search strings and writes the
answer, and a cheap one that screens and drafts, and with `--quick` does
everything.

Flags, or variables set in the environment, come before what is saved:

```sh
ebira ask "What did I decide about retries?" --api openai --strong-model <id> --cheap-model <id>
ebira ask "What did I decide about retries?" --api compatible --api-url http://localhost:11434/v1 --cheap-model <id> --quick
```

The variables are `EBIRA_ASK_API`, `EBIRA_ASK_API_URL`, `EBIRA_ASK_API_KEY_ENV`
(another variable to read the key from), `EBIRA_ASK_STRONG_MODEL`, and
`EBIRA_ASK_CHEAP_MODEL`. The reply's shape is asked for with a strict JSON schema,
which is also written into the instructions for a service that ignores it; a
service that refuses part of a request (the schema, the effort, the name of the
output limit) is asked again without that part. Prices are known only for Claude
models: other calls are recorded with their tokens and counted as unpriced. The
instructions were tuned with Claude models; whichever model answers, its quotes
are checked against the records.

## Logs in other locations

Add a JSONL file (`.jsonl`, or `.jsonl.zst` compressed) or a directory. It is
remembered for later syncs:

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

A compressed log is read from its start, so reading a record near the end of a
large one takes longer: a few seconds for a log that holds a gigabyte of JSONL.

Claude Code and Codex log formats can change and may require an Ebira update.
See [ARCHITECTURE.md](ARCHITECTURE.md) for storage and implementation details.

## License

[Apache-2.0](LICENSE)
