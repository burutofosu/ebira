---
name: ebira-full-survey
description: Research earlier sessions in the Ebira corpus (what the person said, decided, or rejected, what failed, and when) and cite the original JSONL records. For continuing the current task after a compaction, use ebira-recent-recall.
---

# Ebira full survey

Find the relevant records with Ebira, then read the original JSONL around each
one you rely on. Narrow by project, session, date, and literal text; a single
decision does not need the whole corpus.

## Before you start

`ebira` must be on `PATH`; if it is not, say so and stop. Ebira finds its corpus
itself (`EBIRA_CORPUS`, else the platform data directory), and `ebira status`
prints it as `corpus`. Do not test that path from another shell: the WSL build
reports a WSL path (`/home/...`), which PowerShell and Git Bash do not resolve to
the same place.

Run `ebira sync` first when the question includes recent events. If the storage
format or reading rules changed, sync rebuilds the corpus from the logs.

## Ask first

When an Anthropic API key is set (`ANTHROPIC_API_KEY`, or one saved with
`ebira ask-setup`), let Ebira do the first search and reading:

```text
ebira ask "<the question, in the person's words>" [--cwd <project-path-part>] [--from <yyyy-mm-dd>] [--to <yyyy-mm-dd>] --format text
```

It chooses search strings, screens what it finds, reads the records around the
best of them, checks the person's later messages for changes to each decision it
reports, and answers with quotes, each checked against the records and cited by
source reference. It takes about two minutes and costs about 30 US cents a question;
`--quick` costs a tenth and makes more mistakes about what changed later. A quote
marked `(not found in the records)` is not one. The answer is a lead: read the
original of every claim you rely on (below) before you report it.

Without a key, or to look further, search yourself.

## Find

The person's own statements across Claude Code and Codex, newest first:

```text
ebira said --query <text> [--cwd <project-path-part>] [--from <yyyy-mm-dd>] [--to <yyyy-mm-dd>] --format text
```

When a topic came up, and the records that mention it:

```text
ebira history --query <text>
ebira search --query <text> [--sender human] [--cwd <project-path-part>] [--from <yyyy-mm-dd>] [--to <yyyy-mm-dd>] [--order desc]
ebira timeline --date <yyyy-mm-dd>
```

Queries are literal text; `--ignore-case` folds ASCII letters only. Narrow with
`--session`, `--source-id`, `--sender`, and `--kind`; page with `--offset` and
the returned `next_offset`.

Several texts take one pass over the corpus instead of one each: repeat
`--query` with `said` or `search`. Each text gets the list it would get alone,
under `results`; keep `--limit` small so the output stays readable.

For the commits a conversation produced, run `ebira commits --repo <repository>`
and read them with Git.

## Read the original

```text
ebira context --source-id <id> --byte-start <n> --byte-len <n> --before 5 --after 5
```

If it reports `source_changed_since_projection`, run `ebira sync` and search again.

## A history from start to now

For how a project, tool, or decision developed and where it stands:

1. Map it with `ebira history --query <name>` for each spelling of the name: the
   dates and sessions show when it was worked on, and the projects where.
2. Ask, as above. The answer's list of what the records do not settle is the list
   of what to check next.
3. Read every message of the person's about it: one `said` pass with each
   spelling as its own `--query` (the English name, its katakana, lowercase, a
   former name). These are the backbone of the account; an agent's report is a
   claim to check against them.
4. When the work produced code, `ebira commits --repo <repository>` ties the
   commits to the conversations, and Git dates the code itself, uncommitted
   changes included.
5. For where it stands now, search after the last decision with
   `ebira search --query <name> --from <yyyy-mm-dd>`. Later records that only
   look back, such as surveys and summaries, are not new work.
6. Read the original of every record the account rests on, as above.

The records hold what was said to Claude Code and Codex. Work done by hand or in
other tools, and what images show, are not in them; say so where the account
depends on it.

## Before calling something absent

- `no_match_in_projection` covers the compact corpus only; check the original
  records with `ebira search --query <text> --raw`.
- Records without a usable time sit in the `undated` bucket, which date filters
  do not reach.
- Compare `returned` with `total_candidates`; read the remaining pages or say
  what was left unread.
- Mention `stale_sources` or `unscanned_source_bytes` when they affect the answer.

## Who wrote a record

| `sender` | Meaning |
| --- | --- |
| `human` | The person: typed or pasted, typed while the agent worked, slash commands, answers to a question |
| `agent` | Another agent in the user role: a subagent prompt, a Codex child or `codex exec` thread, a relayed agent message |
| `system` | Tool results, notifications, injected context |
| `summary` | A compaction summary |
| `assistant` | The recording agent's replies and tool calls |
| `unknown` | A record in a log format that marks no sender |

`human` means the person sent it; text they quoted or pasted inside it (another
model's opinion, a review, a document) is material they shared, not their
decision. Another agent's words are claims to check, not the person's intent.

## Report

- Separate what the records say, the chronology you derived, and your inference.
- Cite the `source_ref` (source_id, byte_start, byte_len) of each central claim;
  a source_id starting with `claude-` or `codex-` names the app whose log recorded it.
- State the scope you searched and anything left unread.
