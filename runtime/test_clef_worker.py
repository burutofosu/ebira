"""Protocol and token-limit tests using fake tokenizer/model objects."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import types
import unittest
from unittest.mock import patch

import clef_worker as worker


class WorkerTests(unittest.TestCase):
    def test_questions(self):
        result = worker.convert_questions([
            {"id": "yes", "type": "noul", "question": "Is it present?"},
            {"id": "label", "type": "choice", "question": "Which?", "choices": ["a", "b"]},
            {"id": "level", "type": "score", "question": "How much?", "criteria": ["low", "high"]},
        ])
        self.assertEqual(result["label"]["criteria"], {"a": "a", "b": "b"})
        self.assertEqual(result["level"]["criteria"], ["low", "high"])
        self.assertEqual(result["yes"]["instructions"], "Is it present?")

    def test_duplicate_ids_rejected(self):
        q = {"id": "same", "type": "noul", "question": "Test"}
        with self.assertRaises(worker.WorkerError):
            worker.convert_questions([q, q])

    def test_bad_choices_rejected(self):
        for choices in (["x", "x"], [], [1], {"": "bad"}):
            with self.assertRaises(worker.WorkerError):
                worker.convert_questions([{"id": "q", "type": "choice", "question": "Test", "choices": choices}])

    def test_preflight_rejects_overflow(self):
        instance = worker.ClefWorker()
        calls = []
        def encode(*args, **kwargs):
            self.assertEqual(kwargs["max_length"], sys.maxsize)
            return types.SimpleNamespace(input_ids=range(11))
        def infer(*args, **kwargs):
            calls.append(True)
            return {"answers": {"q": {"type": "noul", "noul": 0.75}}, "usage": {"input_tokens": 11}}
        instance.helper = types.SimpleNamespace(encode_record=encode, systemone=infer)
        instance.processor = types.SimpleNamespace(tokenizer=object())
        instance.model = object()
        with tempfile.TemporaryDirectory() as directory:
            request = {"id": "r", "state": "all evidence", "questions": [{"id": "q", "type": "noul", "question": "Present?"}], "runtime": {"model_path": directory, "max_tokens": 10}}
            with patch.object(instance, "identify", return_value="fixture-only"), patch.object(instance, "load"):
                with self.assertRaises(worker.WorkerError) as caught:
                    instance.handle(request)
                self.assertEqual(caught.exception.payload["code"], "context_overflow")
                self.assertEqual(caught.exception.payload["input_tokens"], 11)
                self.assertFalse(calls)
                request["runtime"]["max_tokens"] = 11
                result = instance.handle(request)
                self.assertEqual(result["input_tokens"], 11)
                self.assertEqual(result["answers"]["q"]["noul"], 0.75)
                self.assertEqual(calls, [True])

    def test_identity_hashes_files_and_detects_changes(self):
        instance = worker.ClefWorker()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("config.json", "joint_head_config.json", "joint_head.safetensors", "model.safetensors", "joint_schema_model.py"):
                (root / name).write_text("fixture")
            with patch.object(worker, "HELPER_SHA256", worker.digest_file(root / "joint_schema_model.py")), patch.object(instance, "load", side_effect=AssertionError("unexpected model load")):
                request = {"id": 1, "op": "identity", "runtime": {"model_path": directory}}
                identity = instance.handle(request)["model_identity"]
                self.assertTrue(identity.startswith("clef-sha256:"))
                self.assertEqual(identity, instance.handle(request)["model_identity"])
                (root / "config.json").write_text("changed")
                with self.assertRaises(worker.WorkerError) as caught:
                    instance.handle(request)
                self.assertEqual(caught.exception.payload["code"], "model_changed")

    def test_missing_model_directory_is_rejected(self):
        with self.assertRaises(worker.WorkerError) as caught:
            worker.runtime_config({"model_path": "Cloudflare/nonexistent-model"})
        self.assertEqual(caught.exception.payload["code"], "model_unavailable")

    def test_persistent_protocol_errors_are_sanitized(self):
        sensitive = "secret-state-should-not-appear"
        lines = ["not JSON", json.dumps({"id": "two", "state": sensitive, "runtime": {"model_path": "/nonexistent"}})]
        process = subprocess.run([sys.executable, str(Path(worker.__file__))], input="\n".join(lines) + "\n", text=True, capture_output=True, check=True)
        answers = [json.loads(line) for line in process.stdout.splitlines()]
        self.assertEqual(len(answers), 2)
        self.assertEqual(answers[1]["id"], "two")
        self.assertNotIn(sensitive, process.stdout + process.stderr)
        self.assertEqual(process.stderr, "")


if __name__ == "__main__":
    unittest.main()
