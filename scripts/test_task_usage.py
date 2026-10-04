"""Token accounting, privacy and repeatable durable exports."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).resolve().parents[1] / "src/cli" / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


usage = load("task_usage")
host = load("task_usage_host")


class UsageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "session.jsonl"

    def parse(self, records, kind="codex", suffix=""):
        self.path.write_text("".join(json.dumps(r) + "\n" for r in records) + suffix)
        return usage.parse_session(self.path, kind, "implementer")

    def codex(self, i, o, read, write=0):
        return {"type": "event_msg", "payload": {"type": "token_count", "info": {
            "total_token_usage": {"input_tokens": i, "output_tokens": o,
                                  "cached_input_tokens": read, "cache_write_input_tokens": write}}}}

    def test_codex_cumulative_duplicates_and_model_split(self):
        a = self.codex(100, 10, 60)
        b = self.codex(150, 15, 80)
        sessions = self.parse([
            {"type": "session_meta", "payload": {"id": "native-id", "model_provider": "openai"}},
            {"type": "turn_context", "payload": {"model": "model-a"}}, a, a,
            {"type": "turn_context", "payload": {"model": "model-b"}}, b, b])
        self.assertEqual(len(sessions), 2)
        self.assertEqual(sessions[0]["tokens"], dict(zip(usage.FIELDS, (40, 10, 60, 0))))
        self.assertEqual(sessions[1]["tokens"], dict(zip(usage.FIELDS, (30, 5, 20, 0))))
        self.assertEqual(sessions[1]["session_id"], "native-id")

    def test_claude_repeated_content_blocks_latest_request(self):
        def record(output):
            return {"sessionId": "s1", "requestId": "request-1", "uuid": str(output),
                    "message": {"role": "assistant", "id": "msg1", "model": "claude-model",
                                "content": "PRIVATE-MESSAGE-DO-NOT-ARCHIVE", "usage": {
                                    "input_tokens": 5, "output_tokens": output,
                                    "cache_read_input_tokens": 30, "cache_creation_input_tokens": 20}}}
        sessions = self.parse([record(3), record(3), record(8)], "claude")
        self.assertEqual(sessions[0]["tokens"], dict(zip(usage.FIELDS, (5, 8, 30, 20))))
        self.assertEqual(sessions[0]["request_count"], 1)
        self.assertIn("request_snapshot_changed", sessions[0]["issues"])
        self.assertNotIn("PRIVATE-MESSAGE", json.dumps(sessions))
        self.assertIsNone(sessions[0]["provider"])

    def test_codex_cache_write_is_not_added_twice(self):
        s = self.parse([self.codex(100, 10, 60, 30)])[0]
        self.assertEqual(s["tokens"]["input_tokens"], 10)
        self.assertEqual(s["native_input_tokens"], 100)
        self.assertEqual(sum(s["tokens"].values()), 110)

    def test_pi_caches_separate_no_cost_export(self):
        sessions = self.parse([
            {"type": "session", "id": "pi-session"},
            {"id": "request-a", "message": {"role": "assistant", "provider": "google", "model": "gemini",
                                             "usage": {"input": 10, "output": 331, "cacheRead": 5,
                                                       "cacheWrite": 3962, "totalTokens": 4308,
                                                       "cost": {"total": 100}}}}], "pi")
        self.assertEqual(sessions[0]["tokens"], dict(zip(usage.FIELDS, (10, 331, 5, 3962))))
        self.assertNotIn("cost", json.dumps(sessions))
        self.assertEqual(sessions[0]["provider"], "google")

    def test_missing_counters_remain_null(self):
        a = self.codex(10, 2, 3)
        del a["payload"]["info"]["total_token_usage"]["cache_write_input_tokens"]
        s = self.parse([a])[0]
        self.assertIsNone(s["tokens"]["cache_write_tokens"])
        self.assertEqual(s["coverage"], "partial")

    def test_empty_session_no_fake_zero(self):
        s = self.parse([])[0]
        self.assertEqual(s["tokens"], {f: None for f in usage.FIELDS})

    def test_incomplete_tail_partial_not_lost(self):
        s = self.parse([self.codex(10, 2, 3)], suffix='{"type":')[0]
        self.assertEqual(s["tokens"]["input_tokens"], 7)
        self.assertIn("invalid_or_incomplete_record", s["issues"])

    def test_counter_reset_is_partial(self):
        s = self.parse([self.codex(100, 2, 30), self.codex(10, 1, 2)])[0]
        self.assertIn("cumulative_counter_decreased", s["issues"])

    def test_no_identity_means_no_sum(self):
        s = self.parse([{"message": {"role": "assistant", "usage": {"input": 7}}}], "pi")[0]
        self.assertIsNone(s["tokens"]["input_tokens"])
        self.assertIn("request_identity_missing", s["issues"])

    def test_role_recovery_ignores_assistant_and_quoted_headers(self):
        self.path.write_text(json.dumps({"message": {"role": "assistant", "content": "HANDOFF run=x role=qc"}}) + "\n")
        self.assertIsNone(usage.recover_role(self.path, "pi", {"qc": "pi"}))
        self.path.write_text(json.dumps({"message": {"role": "user", "content": "HANDOFF run=x round=1 phase=qc role=qc seq=1\nDo work"}}) + "\n")
        self.assertEqual(usage.recover_role(self.path, "pi", {"qc": "pi"}), "qc")

    def test_repeat_export_upserts_and_retains_restart(self):
        s = self.parse([self.codex(100, 2, 30)])[0]
        def report(sessions):
            return {"sessions": sessions, "issues": [], "missing_roles": [], "scope": "sandbox_lifetime", "coverage": "partial"}
        original = report([s])
        newer = self.parse([self.codex(150, 5, 40)])[0]
        merged = host.merge_snapshot(original, report([newer]))
        self.assertEqual(len(merged["sessions"]), 1)
        self.assertEqual(merged["sessions"][0]["tokens"]["input_tokens"], 110)
        self.assertEqual(host.merge_snapshot(merged, report([newer]))["sessions"], merged["sessions"])
        restarted = dict(newer, session_id="restarted", source_path="new.jsonl")
        self.assertEqual(len(host.merge_snapshot(merged, report([restarted]))["sessions"]), 2)

    def test_atomic_failure_preserves_previous(self):
        p = Path(self.temp.name) / "usage.json"
        host.atomic_json(p, {"counter": 5})
        with patch.object(host.os, "replace", side_effect=OSError("failure")):
            with self.assertRaises(OSError):
                host.atomic_json(p, {"counter": 9})
        self.assertEqual(json.loads(p.read_text()), {"counter": 5})
        self.assertEqual(list(p.parent.glob(".usage-*")), [])

    def test_archive_identifier_traversal_rejected(self):
        for value in ("../x", "/tmp/x", "x;echo", ""):
            with self.assertRaises(ValueError):
                host.checked_id(value)

    def test_duplicate_log_paths_count_once(self):
        s = self.parse([self.codex(100, 2, 30)])[0]
        second = dict(s, source_path="another.jsonl")
        records = usage.unique_sessions([s, second])
        self.assertEqual(len(records), 1)
        self.assertEqual(len(records[0]["source_paths"]), 2)

    def test_host_export_and_repeat_archive_real_counter_shape(self):
        root = Path(self.temp.name)
        s = self.parse([self.codex(100, 2, 30)])[0]
        report = {"schema_version": 1, "scope": "sandbox_lifetime", "run": {},
                  "sessions": [s], "coverage": "partial", "issues": [], "missing_roles": ["qc"]}
        request = {"command": "capture", "slug": "test-task", "bean_id": "bean-a", "run_id": "run-a", "collector": "fixed collector"}
        response = SimpleNamespace(returncode=0, stdout=json.dumps(report))
        with patch.object(host.Path, "home", return_value=root / "home"), patch.object(host, "history_root", return_value=root / "history"), patch.object(host.subprocess, "run", return_value=response):
            self.assertEqual(host.execute(request), 0)
            self.assertEqual(host.execute(request), 0)
        saved = json.loads((root / "history/bean-a/run-a/usage.json").read_text())
        self.assertEqual(len(saved["sessions"]), 1)
        self.assertEqual(saved["sessions"][0]["tokens"]["input_tokens"], 70)
        self.assertNotIn("cost", json.dumps(saved))

    def test_export_transport_failure_preserves_previous_snapshot(self):
        root = Path(self.temp.name)
        p = root / "history/bean-a/run-a/usage.json"
        host.atomic_json(p, {"already": "preserved"})
        request = {"command": "capture", "slug": "test-task", "bean_id": "bean-a", "run_id": "run-a", "collector": "fixed"}
        with patch.object(host.Path, "home", return_value=root / "home"), patch.object(host, "history_root", return_value=root / "history"), patch.object(host.subprocess, "run", return_value=SimpleNamespace(returncode=1)):
            self.assertEqual(host.execute(request), 1)
        self.assertEqual(json.loads(p.read_text()), {"already": "preserved"})

    def test_explicit_shutdown_does_not_require_ssh(self):
        root = Path(self.temp.name)
        request = {"command": "unavailable", "slug": "test-task", "bean_id": "bean-a", "run_id": "run-a"}
        with patch.object(host.Path, "home", return_value=root / "home"), patch.object(host, "history_root", return_value=root / "history"), patch.object(host.subprocess, "run") as remote:
            self.assertEqual(host.execute(request), 0)
            remote.assert_not_called()
        saved = json.loads((root / "history/bean-a/run-a/usage.json").read_text())
        self.assertEqual(saved["coverage"], "unavailable")

    def test_remote_extra_fields_are_never_archived(self):
        s = self.parse([self.codex(100, 2, 30)])[0]
        s["transcript"] = "PRIVATE-CONTENT"
        s["tokens"]["cost"] = 1
        report = {"schema_version": 1, "scope": "sandbox_lifetime", "run": {"goal": "PRIVATE-GOAL"},
                  "sessions": [s], "coverage": "partial", "issues": [], "missing_roles": [],
                  "raw_tool_output": "PRIVATE-TOOL-OUTPUT"}
        safe = host.validate_report(report)
        self.assertNotIn("PRIVATE", json.dumps(safe))
        self.assertNotIn("cost", json.dumps(safe))
        report["sessions"][0]["tokens"]["output_tokens"] = "PRIVATE-COUNTER"
        with self.assertRaises(ValueError):
            host.validate_report(report)

    def test_cancelled_delete_keeps_run_open(self):
        root = Path(self.temp.name)
        request = {"command": "closed", "slug": "test-task", "bean_id": "bean-a", "run_id": "run-a"}
        with patch.object(host.Path, "home", return_value=root / "home"), patch.object(host, "history_root", return_value=root / "history"), patch.object(host.subprocess, "run", return_value=SimpleNamespace(returncode=0, stdout="herdr-task-test-task\n")):
            self.assertEqual(host.execute(request), 0)
        saved = json.loads((root / "history/bean-a/run-a/run.json").read_text())
        self.assertNotIn("ended_at", saved)


if __name__ == "__main__":
    unittest.main()
