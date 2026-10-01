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

Run `ebira sync` first when the question includes recent events. After an Ebira
update that sync rebuilds the corpus from the logs, which takes about a minute.

## Find

The person's own statements across Claude Code and Codex, newest first:

```text
ebira said --query <text> [--cwd <project-path-part>] [--from <yyyy-mm-dd>] [--to <yyyy-mm-dd>] --format text
```

When a topic came up, and the records that mention it:

```text
ebira history --query <text>
ebira search --query <text> [--sender human] [--from <yyyy-mm-dd>] [--to <yyyy-mm-dd>] [--order desc]
ebira timeline --date <yyyy-mm-dd>
```

Queries are literal text; `--ignore-case` folds ASCII letters only. Narrow with
`--session`, `--source-id`, `--sender`, and `--kind`; page with `--offset` and
the returned `next_offset`.

For the commits a conversation produced, run `ebira commits --repo <repository>`
and read them with Git.

## Read the original

```text
ebira context --source-id <id> --byte-start <n> --byte-len <n> --before 5 --after 5
```

If it reports `source_changed_since_projection`, run `ebira sync` and search again.

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

`human` means the person sent it; text they quoted or pasted inside it (another
model's opinion, a review, a document) is material they shared, not their
decision. Another agent's words are claims to check, not the person's intent.

## Report

- Separate what the records say, the chronology you derived, and your inference.
- Cite the `source_ref` (source_id, byte_start, byte_len) of each central claim;
  a source_id starting with `claude-` or `codex-` names the app whose log recorded it.
- State the scope you searched and anything left unread.
