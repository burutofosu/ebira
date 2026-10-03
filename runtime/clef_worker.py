#!/usr/bin/env python3
"""Persistent, offline JSONL adapter for the pinned Cloudflare Clef release.

stdout carries JSONL responses. Dependency output on stdout and stderr is
suppressed because it can contain input evidence.
"""
from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import importlib.metadata
import json
import os
from pathlib import Path
import sys

REVISION = "17f0b0ad64efb65d273590632833508766b2aae6"
HELPER_SHA256 = "0e304cf7c6500e8bb59bef7e2afd2c6373f82596dfb3b57d1aa93c175e2dc3a3"
MAX_CONTEXT = 16384

# Set before any ML imports; override inherited values that permit online use.
for key in ("HF_HUB_OFFLINE", "TRANSFORMERS_OFFLINE", "HF_DATASETS_OFFLINE"):
    os.environ[key] = "1"
os.environ["HF_HUB_DISABLE_TELEMETRY"] = "1"
os.environ["TOKENIZERS_PARALLELISM"] = "false"


class WorkerError(Exception):
    def __init__(self, code, message, **details):
        self.payload = {"code": code, "message": message, **details}
        super().__init__(message)


def fail(code, message, **details):
    raise WorkerError(code, message, **details)


def digest_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


@contextlib.contextmanager
def quiet_dependencies():
    """Suppress Python and native-library logging during dependency calls."""
    sys.stdout.flush()
    sys.stderr.flush()
    saved = [os.dup(1), os.dup(2)]
    try:
        with open(os.devnull, "w") as sink:
            os.dup2(sink.fileno(), 1)
            os.dup2(sink.fileno(), 2)
            with contextlib.redirect_stdout(sink), contextlib.redirect_stderr(sink):
                yield
    finally:
        os.dup2(saved[0], 1)
        os.dup2(saved[1], 2)
        for descriptor in saved:
            os.close(descriptor)


def runtime_config(raw):
    if not isinstance(raw, dict):
        fail("invalid_runtime", "runtime must be an object")
    if raw.get("backend", "clef") != "clef":
        fail("invalid_runtime", "Only the local clef backend is supported")
    path_value = raw.get("model_path")
    if not isinstance(path_value, str) or not path_value:
        fail("invalid_runtime", "An existing local model_path is required")
    path = Path(path_value).expanduser().resolve()
    if not path.is_dir():
        fail("model_unavailable", "model_path must be an existing local directory")
    device = raw.get("device", "cpu")
    if not isinstance(device, str) or not (device == "cpu" or device == "cuda" or (device.startswith("cuda:") and device[5:].isdigit())):
        fail("invalid_runtime", "device must be cpu, cuda, or cuda:N")
    maximum = raw.get("max_tokens", MAX_CONTEXT)
    if type(maximum) is not int or not 1 <= maximum <= MAX_CONTEXT:
        fail("invalid_runtime", "max_tokens must be an integer between 1 and 16384")
    return path, device, maximum


def convert_questions(raw):
    if not isinstance(raw, list) or not raw:
        fail("invalid_questions", "questions must be a nonempty array")
    result = {}
    for item in raw:
        if not isinstance(item, dict):
            fail("invalid_questions", "Each question must be an object")
        qid, kind, instruction = item.get("id"), item.get("type"), item.get("question")
        if not isinstance(qid, str) or not qid or qid in result:
            fail("invalid_questions", "Question IDs must be unique nonempty strings")
        if kind not in ("noul", "choice", "score") or not isinstance(instruction, str) or not instruction:
            fail("invalid_questions", "Each question needs a supported type and nonempty question text")
        q = {"type": kind, "instructions": instruction}
        criteria = item.get("criteria", item.get("choices"))
        if kind == "choice":
            if isinstance(criteria, list) and criteria and all(isinstance(c, str) and c for c in criteria) and len(set(criteria)) == len(criteria):
                criteria = {c: c for c in criteria}
            if not isinstance(criteria, dict) or not criteria or not all(isinstance(k, str) and k and (v is None or isinstance(v, str)) for k, v in criteria.items()):
                fail("invalid_questions", "choice requires unique string choices or a criteria object")
            q["criteria"] = criteria
        elif kind == "score":
            if not isinstance(criteria, list) or not criteria or not all(isinstance(c, str) and c for c in criteria):
                fail("invalid_questions", "score requires an ordered list of criteria descriptions")
            q["criteria"] = criteria
        elif criteria is not None:
            fail("invalid_questions", "choices and criteria require question type choice or score")
        result[qid] = q
    return result


class ClefWorker:
    def __init__(self):
        self.key = None
        self.identity = None
        self.manifest = None
        self.helper = self.model = self.processor = None

    def identify(self, path, device):
        key = (str(path), device)
        # Bind the worker to one snapshot and device.
        if self.key is not None and self.key != key:
            fail("runtime_changed", "Restart the worker to change the model or device")
        files = sorted(p for p in path.rglob("*") if p.is_file() and "__pycache__" not in p.parts and ".cache" not in p.parts and ".git" not in p.parts)
        manifest = [(str(p.relative_to(path)), p.stat().st_size, p.stat().st_mtime_ns, p.stat().st_ctime_ns) for p in files]
        if self.identity is not None:
            if manifest != self.manifest:
                fail("model_changed", "Local model files changed; restart the worker")
            return self.identity
        helper_path = path / "joint_schema_model.py"
        if not helper_path.is_file() or digest_file(helper_path) != HELPER_SHA256:
            fail("helper_mismatch", "The local helper must match the pinned official Clef release")
        for name in ("config.json", "joint_head_config.json", "joint_head.safetensors"):
            if not (path / name).is_file():
                fail("model_incomplete", "Required local model artifacts are missing")
        if not any(p.name.endswith(".safetensors") and p.name != "joint_head.safetensors" for p in files):
            fail("model_incomplete", "Local backbone safetensors are missing")
        digest = hashlib.sha256()
        digest.update(b"ebira-clef-worker-v1\0")
        digest.update(device.encode())
        digest.update(sys.version.encode())
        for package in ("torch", "transformers", "safetensors", "huggingface-hub", "tokenizers"):
            try:
                version = importlib.metadata.version(package)
            except importlib.metadata.PackageNotFoundError:
                version = "not-installed"
            digest.update((package + "=" + version + "\0").encode())
        digest.update(digest_file(Path(__file__)).encode())
        for p in files:
            digest.update(str(p.relative_to(path)).encode())
            digest.update(b"\0")
            digest.update(digest_file(p).encode())
            digest.update(b"\0")
        self.identity = "clef-sha256:" + digest.hexdigest()
        self.key, self.manifest = key, manifest
        return self.identity

    def load(self, path, device):
        if self.model is not None:
            return
        with quiet_dependencies():
            import torch
            if device.startswith("cuda") and not torch.cuda.is_available():
                fail("device_unavailable", "CUDA device unavailable")
            spec = importlib.util.spec_from_file_location("ebira_pinned_joint_schema_model", path / "joint_schema_model.py")
            helper = importlib.util.module_from_spec(spec)
            sys.modules[spec.name] = helper
            spec.loader.exec_module(helper)
            dtype = torch.float32 if device == "cpu" else torch.bfloat16
            model, processor = helper.load_release_model(path, device=device, dtype=dtype, local_files_only=True)
        self.helper, self.model, self.processor = helper, model, processor

    def handle(self, request):
        if not isinstance(request, dict):
            fail("invalid_request", "Request must be an object")
        if not isinstance(request.get("id"), (str, int)) or isinstance(request.get("id"), bool):
            fail("invalid_request", "A string or integer request id is required")
        op = request.get("op", "score")
        if op not in ("identity", "score"):
            fail("invalid_request", "op must be identity or score")
        path, device, maximum = runtime_config(request.get("runtime"))
        official = None
        if op == "score":
            if not isinstance(request.get("state"), str):
                fail("invalid_request", "state must be a string")
            official = {"model": "Cloudflare/clef-flash", "state": request["state"], "questions": convert_questions(request.get("questions"))}
        identity = self.identify(path, device)
        if op == "identity":
            return {"id": request["id"], "model_identity": identity}
        self.load(path, device)
        with quiet_dependencies():
            # Measure the complete serialized input before applying the token limit.
            encoded = self.helper.encode_record(self.processor.tokenizer, official, max_length=sys.maxsize, processor=self.processor)
            count = len(encoded.input_ids)
            if count > maximum:
                fail("context_overflow", "Full evidence and schema exceed the context limit", input_tokens=count, max_tokens=maximum)
            result = self.helper.systemone(self.model, self.processor, official, max_length=maximum)
        if result.get("usage", {}).get("input_tokens") != count:
            fail("token_count_mismatch", "Inference encoding differed from preflight")
        return {"id": request["id"], "answers": result["answers"], "input_tokens": count, "model_identity": identity}


def main():
    worker = ClefWorker()
    for line in sys.stdin:
        request = None
        try:
            request = json.loads(line)
            response = worker.handle(request)
        except WorkerError as error:
            response = {"id": request.get("id") if isinstance(request, dict) else None, "error": error.payload}
        except (json.JSONDecodeError, UnicodeError):
            response = {"id": None, "error": {"code": "invalid_json", "message": "Request must be one valid JSON object per line"}}
        except Exception:
            # Exception messages from ML libraries can embed state or local paths.
            response = {"id": request.get("id") if isinstance(request, dict) else None, "error": {"code": "runtime_failure", "message": "Local Clef runtime failed; verify dependencies and complete local model artifacts"}}
        print(json.dumps(response, ensure_ascii=False, separators=(",", ":"), allow_nan=False), flush=True)


if __name__ == "__main__":
    main()
