# Experimental local extraction API (v1)

`ebira extract --corpus PATH --request request.json` accepts one JSON request;
`--request -` reads it from stdin (maximum 4 MiB). stdout is JSONL: `progress`,
`item`, then `done`. The API runs as a local CLI subprocess.

## Run the mock example

From the repository root, with Rust 1.89+ and Python 3 available:

```sh
cargo build --locked
./target/debug/ebira sync --corpus /tmp/ebira-extract-demo --source examples/claude-session.jsonl
./target/debug/ebira extract --corpus /tmp/ebira-extract-demo --request examples/extract-mock.json
```

The example uses `backend: "mock"`, which assigns the configured answers to every
event. It exercises request handling, selection and caching. Change a selection
threshold and run again to reuse cached answers and recompute selection. Corpus
and score-cache data are stored locally.

## Use local Clef

Prepare a complete pinned local model snapshot and compatible Python environment
as described in [runtime/README.md](../runtime/README.md). Replace the entire
`runtime` object in the example with:

```json
{
  "backend": "clef",
  "worker_path": "/absolute/path/to/ebira/runtime/clef_worker.py",
  "python": "/absolute/path/to/venv/bin/python",
  "model_path": "/absolute/path/to/clef-flash-snapshot",
  "device": "cuda",
  "max_tokens": 16384,
  "timeout_seconds": 300
}
```

The worker loads once and processes all candidates on stdin/stdout. Identity is
verified before using cached scores. Each response has a timeout; the process is
terminated on timeout/exit, and affected events are unscored. Startup/identity
failure is a command error. The worker/python paths are trusted local executable
configuration. Restrict requests to trusted callers and use trusted executables.
For network isolation, disable network access at the OS/container boundary.

## Request contract

- `version`: exactly `1`.
- `scope`: require nonempty `sessions`, nonempty `source_ids`, or explicit
  `all: true`. When both sessions and source IDs are supplied they intersect.
  `include_children` defaults false; true admits child sources already within
  the selected scope. Session IDs remain as specified.
  Optional `event_ids` narrows to stable `source_id:event_index` candidates from
  an earlier call. Unknown IDs are errors. IDs must be reselected after rewrites.
- `targets`: optional `kinds` and/or `senders` arrays. Omitted or empty arrays mean
  all values. Kinds/senders follow the existing corpus definitions.
- `context`: `{ "mode": "event" }` (default) or `{ "mode": "paired" }`.
  Paired mode joins an unambiguous same-call request/result in the scoped source.
  Missing/ambiguous pairs become unscored. Ordinary messages remain individual
  events. The supported context modes are `event` and `paired`.
- `questions`: 1–64 `{id, type, question, choices?}` objects. IDs are unique.
  `noul` returns the probability of true as a number from 0 to 1. `choice` requires
  unique string `choices`. `score` uses an ordered list of string `choices`; its result is an
  expected ordinal index from 0 through `choices.length - 1`.
- `select`: optional array of AND conditions `{question, op, value}`. `gte` and
  `lte` compare numeric answers; `eq` compares numbers or choice labels. Choice
  questions require `eq` with a declared label. Relevance and exclusion questions
  are independent and scored together. An omitted or empty `select` selects every
  scored item. All scored items are emitted, including `selected:false`.
- `runtime`: explicit `backend: "clef"` or `"mock"`. Mock requires a complete
  `answers` object matching the question types. Clef configuration shown above.
- `cache`: optional boolean (default true). False skips cache reads and writes.

Unknown request keys are rejected. Use scope, target kinds/senders and explicit
event IDs to narrow the input. Answers follow the three question types above.

## Evidence, completeness and errors

The reader consumes complete length-delimited logical events, including multiline
bodies. A cut tool-output projection is hydrated only from its selected raw field
paths after source freshness checks. Transcript text is passed to the model as
evidence. The command emits classification results.

Each item retains event ID, source byte reference, kind/sender/session, hydration
and source freshness, plus same-call references where used. Missing/rewritten or
unsynced sources, invalid records, ambiguous context and failed inference are
reported as unscored. Sync first after appends or rewrites. The worker encodes
the full schema and evidence before scoring and rejects token overflow.

`done.coverage` reports candidate, scored, unscored, selected and cached counts;
`complete:false` means at least one candidate is unscored. Inspect item errors even
when the process exit code is zero. Structural corpus corruption and invalid
requests fail the command (nonzero exit, diagnostic on stderr). Coverage is for
the selected candidates in the scoped corpus.

The command gathers candidates in memory; use narrow scopes for large
archives. Per-event input is bounded at 16 MiB. Each invocation starts at the
first candidate and scores events sequentially. Output streams as scoring
progresses.

## Cache

`CORPUS/extract-cache-v1/` stores scores and token metadata. SHA256 keys cover
hydrated input and context, question schema,
input metadata, runtime/model identity, and API/input-reader code. Changing only
`select` reuses scores. Source rewrite, changed paired input, schema, model or
code invalidates them. Model identity hashes full local weights once per worker,
so the first lookup can take time. Keep model snapshots immutable during a run.
Cache corruption is treated as a miss. Newly created cache files/directories use
owner-only permissions on Unix. Existing files and directories retain their
permissions.

## Verification

```sh
cargo test --locked
cargo fmt --check
cargo clippy --all-targets -- -D warnings
python3 -m unittest discover -s runtime -p 'test_*.py' -v
```

Rust fixtures cover request validation, input hydration, selection, caching and
worker errors. Python tests use fake tokenizer/model objects to cover the worker
protocol and token-limit checks.

Live Clef inference, calibration, quality and latency remain untested. Model
evaluation requires a local snapshot and sufficient CPU memory or GPU capacity.
