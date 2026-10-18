# Changelog

The notable changes in each release of Ebira. Before 1.0, a minor version may
change what a command writes; a change to the corpus's storage format or reading
rules makes the next `ebira sync` rebuild the corpus from the original logs.

## 0.2.1 - 2026-10-18

Found in an outside review of 0.2.0.

### Fixed

- `context` checks that the record a reference names is still the one the corpus
  read when the log grew since the last sync. A log rewritten in place where the
  record was is reported as changed, with the sync that reads it again
  (`--rebuild-source`), instead of returning the other record. A sync reads a log
  again whole when the records it read at either end of it are gone.
- `ask` no longer takes a record left past one round's limit of candidates for
  screened, so the second round's search strings can bring it back.
- `ask` looks for later changes to a decision only in messages written at or after
  it, not in earlier messages of the same day.
- A reply from another service that lacks a field the request asks for, such as
  `{}`, is an error rather than an empty answer.

## 0.2.0 - 2026-10-18

### Added

- `ebira ask` answers a question about past work from the records, with quotes.
  It chooses search strings, screens what it finds, reads the records around the
  best of it, searches again from a draft answer, checks your later messages for
  changes to each decision it reports, and checks every quote against the
  records. `--quick` uses the cheap model throughout for about a tenth of the
  cost. `--no-api` with `--term` searches only.
- `ebira ask` uses Anthropic's API by default, or OpenAI, Google Gemini,
  OpenRouter, or any service or local server that speaks the OpenAI Chat
  Completions API (`--api`, `--api-url`, `--strong-model`, `--cheap-model`).
- `ebira ask-setup` opens a page on this computer (127.0.0.1 only) to choose the
  service, check its key with the service's model list, and choose the models.
  On Windows and in WSL they are saved as Windows user environment variables,
  which `ebira ask` reads by itself.
- `ebira ask-usage` sums what `ask` has spent, per question and apart from
  questions.
- `ebira sessions` lists the conversations in a scope: when each ran, where, how
  much the person said, and its first and last messages.
- `said` and `search` take `--query` more than once and search for every text in
  one pass over the corpus. `search` and `history` take `--cwd`.
- `search --format text`, and `local_time` in the lists of records.

### Changed

- Text views give times in the corpus's zone (`EBIRA_TZ_OFFSET` when the corpus
  was built, else UTC) and name it.
- Claude Code's interrupt marks ("[Request interrupted by user]") are system text,
  not the person's words.
- Of a message Codex wraps with attachments, browser tabs, or annotations, only
  the person's own words count as theirs; image tags and the apps page's state are
  left out.

### Fixed

- Codex logs compressed with Zstandard (`.jsonl.zst`) are read, under the source
  ids and byte offsets they had as `.jsonl`; a sync used to drop them as removed.
- A log an app moves to another folder keeps its source id, so references made
  before the move still read.
- A Codex thread's imported copy of another conversation no longer reads as the
  thread's own records.
- Answers to an agent's own questions count as the person's messages.

## 0.1.0 - 2026-10-01

The first release: `sync`, `status`, `said`, `resume`, `search`, `history`,
`timeline`, `context`, `follow`, `commits`, and `import`, with agent skills for
Claude Code and Codex.
