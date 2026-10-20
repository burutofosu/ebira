# Changelog

The notable changes in each release of Ebira. Before 1.0, a minor version may
change what a command writes; a change to the corpus's storage format or reading
rules makes the next `ebira sync` rebuild the corpus from the original logs.

## 0.5.0 - 2026-10-20

Measured on the evaluation in `evals/`, now fifty-four questions, and on the
author's own history ([results](evals/results/2026-10-20-delivery.md)).

### Added

- `said --context` shows each of the person's messages with the end of the
  agent's reply before it and the agent's last reply in the turn it opened, each
  with its `source_ref`, so that a short reply says what it answers; tool calls,
  thinking, and the memory citations Codex appends are not replies, and a message
  or reply Codex wrote twice counts once. On the author's history, 332 of the 335
  messages of twelve characters or fewer get the reply they answered, and a page
  takes as long as without it.
- `ask --evidence-only` stops before the answer and writes what the answer would
  read and what the steps before it screened, for measuring; `evals/run.py
  --evidence` reports from it how far each record a question should cite gets,
  and `--terms-from` makes runs search the same strings. Ways of reading under
  trial, off by default: `--sweep-context`, `--sweep-chunks`, `--turn-replies`,
  `--delivery current|spread`, `--agent-select newest|idf|all`.
- Twelve evaluation cases that each strain one step of `ask`, on logs
  `evals/stress.py` generates: short agreements to an agent's proposal, a long
  pasted spec, hundreds of design remarks with three rules restated, and causes
  an agent found months ago.

## 0.4.0 - 2026-10-20

Measured with the evaluation in `evals/`, now forty-two questions, and on the
author's own history ([results](evals/results/2026-10-19-sweep.md)).

### Changed

- `ask` screens every message the person wrote in the scope (the newest 4,000 at
  most) with the cheap model, not only the records that hold a search string, and
  reads the relevant ones; those beyond what it reads with their context go to the
  answer in one line each. The check for later changes reads every later message
  of the person's instead of searching for them. With `--cwd`, the scope is every
  message of the sessions that worked in that directory. On the forty-two
  questions, `--quick` goes from 90% of the key points to 99% on the original
  thirty, and the default models stay at 100%; a question costs about a fifth
  more. `--no-sweep` turns it off, `--sweep-limit <n>` changes the limit.
- The plan model sees a sample of the person's messages in the scope, spread over
  it, before choosing the search strings; when no record holds any string, it is
  asked once more.
- The result says what was not read: `search.sweep` (messages screened and judged
  relevant), `search.sweep_digest`, `search.coverage` (the sessions in the scope
  the records read come from, and those not read), and for each search string held
  by more records than were taken, `oldest_taken`, the time before which none was
  read. The reversal checks report `swept` and `later_messages`.

### Added

- `ask --export <path>` writes, as text, the records the answer read and every
  message of the person's judged to bear on the question.
- Twelve evaluation cases of four new kinds: a rule the person repeated in
  different words, a problem tried several ways, a rule an agent or a summary
  wrote as the person's, and an answer in words the question does not use.
  `evals/run.py --ask-arg` passes an option to every `ask`.

## 0.3.1 - 2026-10-19

### Fixed

- Turns Claude Desktop starts itself (`turnOrigin: "sdk"`) were read as the
  person's messages: the brief a session another session started opens with,
  and the notices the app writes into a session, such as `<side-sessions-event>`.
  The brief is now an agent's (`via: session_brief`) and a notice is system text
  (`via: notification`). The reading rules are now version 8, so the next sync
  rebuilds the corpus, keeping its source ids.

## 0.3.0 - 2026-10-19

Found by the evaluation in `evals/`.

### Changed

- `ask` also searches each string with a space and with none between ASCII
  letters or digits and kana or kanji, in the same pass: `csv取り込み` finds
  `CSV 取り込み`. The variants are listed with the search strings.
- When the screen judges none of the records found relevant, `ask` reads the
  closest of them (up to 8) with the records around them and answers from them,
  told that they were not judged relevant; the result is
  `disposition: "nothing_relevant"`. It used to return `no_records` with no
  answer, though the record beside one found could bear on the question.
- `ask` results have `not_settled`, `null` unless the records found do not
  settle the question: then it says so, with the strings searched, the
  directory and dates searched, and how many records they found. The text
  format puts it before the answer, and in place of "No record holds the search
  strings" when no record holds any.

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
