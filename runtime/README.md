# Local Clef worker

`clef_worker.py` is a persistent JSONL subprocess launched with Python 3. It loads
a separately prepared local model snapshot. Tests use fake tokenizer/model
objects. Live Clef inference remains untested.

## Local prerequisites

Use an independently prepared local snapshot of
`Cloudflare/clef-flash` at revision
`17f0b0ad64efb65d273590632833508766b2aae6`, including its backbone safetensors,
joint-head safetensors/config, processor/tokenizer assets, and
`joint_schema_model.py`. Install the official release's compatible PyTorch,
Transformers, safetensors and Hugging Face Hub dependencies separately. The worker
requires these dependencies and artifacts to be installed before launch.

The helper must match SHA256
`0e304cf7c6500e8bb59bef7e2afd2c6373f82596dfb3b57d1aa93c175e2dc3a3`.
Official source:
https://huggingface.co/Cloudflare/clef-flash/blob/17f0b0ad64efb65d273590632833508766b2aae6/joint_schema_model.py

Run `python3 runtime/clef_worker.py`; write one request per line. The worker waits
for the first request on startup. Each response echoes the request ID. Errors
use JSON objects with fixed messages. Dependency logs are suppressed. The worker survives request-level errors.

## Identity before cache lookup

```json
{"id":"identity-1","op":"identity","runtime":{"model_path":"/absolute/local/snapshot","device":"cpu","max_tokens":16384}}
```

Response:

```json
{"id":"identity-1","model_identity":"clef-sha256:..."}
```

Identity hashes all local snapshot files (excluding `.git`, `.cache`, and
`__pycache__`), the worker, Python/dependency versions and execution device. Full
weight hashing may take time on the first request. Subsequent requests check the
file manifest. Keep the snapshot immutable while the worker is running. The
worker rejects detected changes and changes of model path/device. Restart it to
switch snapshots. Obtain the weights from a trusted source; the identity hash
tracks their contents.
Include max_tokens, full question schema, evidence and application versions in
the caller's cache key too.

## Score

```json
{"id":"score-1","op":"score","state":"Complete evidence text","questions":[{"id":"present","type":"noul","question":"Is X explicitly present?"},{"id":"category","type":"choice","question":"Which category fits?","choices":["a","b","unknown"]},{"id":"amount","type":"score","question":"How much?","criteria":["none","some","many"]}],"runtime":{"model_path":"/absolute/local/snapshot","device":"cpu","max_tokens":16384}}
```

The wrapper converts the question array into the official helper's keyed schema:
`question` becomes `instructions`; choice string lists become a criteria map;
score requires an ordered criteria list (a `choices` list is also accepted).
Choice `criteria` can instead be an object mapping labels to descriptions.
Duplicate question IDs and duplicate choice labels are rejected. `noul` has fixed
true/false semantics. Output follows the answer shapes below.

Responses contain `id`, `answers` (official keyed answer dictionary),
`input_tokens`, and `model_identity`. Official answer shapes:

- noul: `{ "type": "noul", "noul": 0.75 }` (probability of true from 0 to 1)
- choice: `type`, `choice`, `confidence`, and `probabilities`
- score: `type`, `score` (expected zero-based ordinal index), `confidence`,
  `legend`, and `probabilities`

Errors contain `id` and `error: {code, message}`. `context_overflow` additionally
has `input_tokens` and `max_tokens` inside `error`. A failed request returns an
error object in place of answers.

## Truncation and offline behavior

The official helper truncates state to fit its maximum length. This worker first
calls its exact `encode_record` with `sys.maxsize` to measure the complete input
including state, prompt, instructions, all choices, and formatting. It rejects an
overflow before inference and checks inference usage against preflight afterward.
The supported limit is 1–16384 tokens. Oversize inputs must be narrowed or split
by the caller.

`HF_HUB_OFFLINE`, `TRANSFORMERS_OFFLINE`, and `HF_DATASETS_OFFLINE` are forced on
before ML imports. The path must already be a directory; the official loader is
called with `local_files_only=True`. Processor loads inherit offline mode. These
configure library-level offline behavior. For network isolation, run inside a
network-disabled sandbox.

CPU uses float32. CUDA uses bfloat16 and must already be available. The worker
loads a single model lazily and reuses it across score requests. Identity requests
hash the snapshot and installed dependency metadata.

## Tests

```sh
python3 -m unittest discover -s runtime -p 'test_*.py' -v
```

These tests verify schema conversion, invalid inputs, local-path requirements,
identity behavior, persistent error handling, and preflight overflow. Live model
inference, quality, calibration and latency require separate evaluation.
