"""Exercise crew model starts and nonce-bound probes through a stub Herdr CLI."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


HELPER = Path(__file__).resolve().parents[1] / "kits/herdr-crew/files/home/crew/bin/crew_models.py"
SPEC = importlib.util.spec_from_file_location("crew_models", HELPER)
MODELS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODELS)
PINS = "claude=claude-opus-5-5,codex=gpt-6.1-sol,pi=google/gemini-3.8-flash"

# Herdr 0.9.0 native prompt error envelope (stderr, exit 1), distinct from
# Codex's persisted task_complete provider-error record above.
NATIVE_STALLED = {
    "id": "cli:agent:prompt",
    "error": {
        "code": "agent_prompt_stalled",
        "message": "agent prompt produced no observed working or blocked state within 5000 ms; current status is idle",
    },
}

# Minimal records from CLI 2.1.289 / 0.153.4 / 0.85.1 standalone probes.
# Preserve only fields used by the verifier, with no credentials or transcripts.
def records(kind, nonce, model, outcome="match"):
    user = {"role": "user", "content": [{"type": "text", "text": "Reply with exactly: " + nonce}]}
    assistant = {"role": "assistant", "content": [{"type": "text", "text": nonce}], "model": model}
    if kind == "codex":
        user["type"] = assistant["type"] = "message"
        user["content"][0]["type"] = "input_text"
        assistant["content"][0]["type"] = "output_text"
        assistant.pop("model")
        entries = [{"type": "turn_context", "payload": {"model": model}},
                   {"type": "response_item", "payload": user}]
        if outcome == "provider-error":
            entries.append({"type": "event_msg", "payload": {"type": "task_complete", "last_agent_message": None,
                "error": {"message": "The model does not exist or you do not have access to it.", "codex_error_info": "other"}}})
        elif outcome != "no-turn":
            entries.append({"type": "response_item", "payload": assistant})
        return entries
    if kind == "pi":
        provider, assistant["model"] = model.split("/", 1)
        assistant.update(provider=provider, stopReason="stop")
        if outcome == "provider-error":
            assistant.update(stopReason="error", errorMessage="model_not_found")
        entries = [{"type": "message", "message": user}]
        if outcome != "no-turn":
            entries.append({"type": "message", "message": assistant})
        return entries
    entries = [{"type": "user", "message": user}]
    if outcome == "provider-error":
        assistant.update(model="<synthetic>", content=[{"type": "text", "text": "model_not_found"}])
    if outcome != "no-turn":
        entries.append({"type": "assistant", "message": assistant})
    return entries


class CrewModelsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="crew-models-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.crew = self.root / "crew"
        self.crew.mkdir()
        (self.crew / "models").write_text(PINS)
        self.calls = self.root / "calls.jsonl"
        self.config = self.root / "scenario.json"
        self.bin = self.root / "herdr"
        self.bin.write_text('''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
from scripts.test_crew_models import records, NATIVE_STALLED
args = sys.argv[1:]
config = json.loads(Path(os.environ["TEST_CONFIG"]).read_text())
with open(os.environ["TEST_CALLS"], "a") as f:
    f.write(json.dumps(args) + "\\n")
kind = config["kind"]
calls = [json.loads(line) for line in Path(os.environ["TEST_CALLS"]).read_text().splitlines()]
if args[:2] == ["agent", "get"]:
    sequence = config.get("get_sequence", [{}])
    index = sum(c[:2] == ["agent", "get"] for c in calls) - 1
    state = {"agent": kind, "agent_status": config.get("state", "idle"), "terminal_id": "term_fixture"}
    state.update(sequence[min(index, len(sequence) - 1)])
    print(json.dumps({"result": {"agent": state}}))
elif args[:2] == ["agent", "start"]:
    if config.get("start_fail"):
        print(json.dumps({"error": {"code": "agent_not_running"}}))
        sys.exit(7)
    print('{}')
elif args[:2] == ["agent", "prompt"]:
    nonce = args[3].removeprefix("Reply with exactly: ")
    sequence = config.get("prompt_sequence", [{}])
    index = sum(c[:2] == ["agent", "prompt"] for c in calls) - 1
    step = sequence[min(index, len(sequence) - 1)]
    if step.get("no_artifact"):
        print(json.dumps(NATIVE_STALLED), file=sys.stderr)
        sys.exit(1)
    roots = {"claude": ".claude/projects/test", "codex": ".codex/sessions/test", "pi": ".pi/agent/sessions/test"}
    root = Path.home() / roots[kind]
    root.mkdir(parents=True)
    for name, token in [("other.jsonl", "OTHER-NONCE"), ("matched.jsonl", nonce)]:
        model = config["model"] if token == nonce else config["wrong_model"]
        data = records(kind, token, model, config.get("outcome", "match"))
        (root / name).write_text(''.join(json.dumps(d) + "\\n" for d in data))
    if config.get("prompt_fail"):
        print(json.dumps(NATIVE_STALLED), file=sys.stderr)
        sys.exit(1)
    print('{}')
else:
    sys.exit(99)
''')
        self.bin.chmod(0o755)
        self.env = dict(os.environ, HOME=str(self.root), CODEX_HOME=str(self.root / ".codex"),
                        HERDR_BIN=str(self.bin), TEST_CONFIG=str(self.config), TEST_CALLS=str(self.calls),
                        PYTHONPATH=str(HELPER.parents[6]))
        # Explicit repository path, independent of the caller's cwd.
        self.env["PYTHONPATH"] = str(Path(__file__).resolve().parents[1])

    def run_helper(self, command, kind="claude", **scenario):
        pins = MODELS.parse_models((self.crew / "models").read_text()) if (self.crew / "models").exists() else MODELS.parse_models(PINS)
        wrong = "other/model" if kind == "pi" else "other-model"
        config = dict(kind=kind, model=pins[kind], wrong_model=wrong)
        config.update(scenario)
        self.config.write_text(json.dumps(config))
        self.calls.unlink(missing_ok=True)
        args = [sys.executable, str(HELPER), "--crew-dir", str(self.crew), command, "--role", "worker"]
        if command == "start":
            args += ["--kind", kind, "--pane", "w1:p9"]
        result = subprocess.run(args, env=self.env, text=True, capture_output=True, timeout=15)
        self.output = result.stdout + result.stderr
        self.assertNotRegex(self.output, r"sk-[A-Za-z0-9]|AIza[A-Za-z0-9]|Bearer [A-Za-z0-9]")
        self.commands = [json.loads(s) for s in self.calls.read_text().splitlines()] if self.calls.exists() else []
        return result.returncode

    def test_start_all_kinds_with_explicit_pins(self):
        for kind, argv in [("claude", ["--model", "claude-opus-5-5"]),
                           ("codex", ["-m", "gpt-6.1-sol", "-c", "check_for_update_on_startup=false"]),
                           ("pi", ["--provider", "google", "--model", "gemini-3.8-flash"])]:
            with self.subTest(kind=kind):
                self.assertEqual(self.run_helper("start", kind), 0, self.output)
                self.assertEqual(self.commands[-1], ["agent", "start", "worker", "--kind", kind,
                                                    "--pane", "w1:p9", "--timeout", "240000", "--", *argv])

    def test_models_file_override_wins_over_environment(self):
        (self.crew / "models").write_text("claude=other-claude,codex=other-codex,pi=alternate/other-pi")
        self.env["HERDR_CREW_GEMINI_MODEL"] = "ignored-model"
        self.assertEqual(self.run_helper("start", "pi"), 0, self.output)
        self.assertEqual(self.commands[-1][-4:], ["--provider", "alternate", "--model", "other-pi"])

    def test_probe_shapes_and_exact_codes(self):
        for kind in ("claude", "codex", "pi"):
            for outcome, expected in [("match", 0), ("mismatch", 30), ("provider-error", 31), ("no-turn", 32)]:
                with self.subTest(kind=kind, outcome=outcome):
                    # Each probe starts from a fresh artifact tree.
                    import shutil
                    for dirname in (".claude", ".codex", ".pi"):
                        shutil.rmtree(self.root / dirname, ignore_errors=True)
                    config = {"outcome": outcome}
                    if outcome == "mismatch":
                        config["model"] = "other/wrong" if kind == "pi" else "wrong-model"
                    self.assertEqual(self.run_helper("probe", kind, **config), expected, self.output)
                    self.assertEqual(self.commands[-1][:3], ["agent", "prompt", "worker"])
                    self.assertEqual(self.commands[-1][-3:], ["--wait", "--timeout", "180000"])

    def test_missing_models_is_old_kit(self):
        (self.crew / "models").unlink()
        self.assertEqual(self.run_helper("probe"), 33, self.output)
        self.assertEqual(self.commands, [])

    def test_start_preserves_native_failure_exit(self):
        self.assertEqual(self.run_helper("start", start_fail=True), 7, self.output)
        self.assertIn("agent_not_running", self.output)

    def test_native_prompt_failure_cannot_pass_from_artifact(self):
        self.assertEqual(self.run_helper("probe", prompt_fail=True), 34, self.output)
        self.assertIn("agent_prompt_stalled", self.output)

    def test_busy_agent_is_never_prompted(self):
        self.assertEqual(self.run_helper("probe", state="working"), 34, self.output)
        self.assertEqual(self.commands, [["agent", "get", "worker"]])

    def test_real_start_stall_idle_retry_sequence_recovers_once(self):
        self.assertEqual(self.run_helper("start", "codex"), 0, self.output)
        self.assertEqual(self.commands[0][:2], ["agent", "start"])
        self.assertEqual(self.run_helper("probe", "codex", prompt_sequence=[
            {"no_artifact": True}, {},
        ]), 0, self.output)
        self.assertEqual([c[:2] for c in self.commands], [
            ["agent", "get"], ["agent", "prompt"], ["agent", "get"], ["agent", "prompt"],
        ])
        prompts = [c for c in self.commands if c[:2] == ["agent", "prompt"]]
        self.assertEqual(prompts[0][3], prompts[1][3])
        self.assertIn("native_delivery_retry error_class=agent_prompt_stalled attempt=1", self.output)
        self.assertIn("observed=gpt-6.1-sol", self.output)
        self.assertIn("attempts=2", self.output)

    def test_exhausted_native_delivery_is_distinct_from_provider_rejection(self):
        self.assertEqual(self.run_helper("probe", prompt_sequence=[{"no_artifact": True}]), 34, self.output)
        self.assertEqual(sum(c[:2] == ["agent", "prompt"] for c in self.commands), 3)
        self.assertIn("reason=retries_exhausted", self.output)
        self.assertEqual(self.output.count("native_delivery_retry"), 2)

    def test_stall_never_retries_after_terminal_replacement_or_busy_state(self):
        for state in [{"terminal_id": "term_replaced"}, {"agent_status": "working"},
                      {"agent_status": "blocked"}, {"agent_status": "done"}]:
            with self.subTest(state=state):
                self.assertEqual(self.run_helper("probe", prompt_sequence=[{"no_artifact": True}],
                    get_sequence=[{}, state]), 34, self.output)
                self.assertEqual(sum(c[:2] == ["agent", "prompt"] for c in self.commands), 1)

    def test_stall_with_observed_nonce_never_retries_even_without_assistant(self):
        self.assertEqual(self.run_helper("probe", outcome="no-turn", prompt_fail=True), 34, self.output)
        self.assertEqual(sum(c[:2] == ["agent", "prompt"] for c in self.commands), 1)
        self.assertIn("reason=nonce_observed", self.output)

    def test_provider_rejection_is_never_retried(self):
        self.assertEqual(self.run_helper("probe", "codex", outcome="provider-error"), 31, self.output)
        self.assertEqual(sum(c[:2] == ["agent", "prompt"] for c in self.commands), 1)
        self.assertNotIn("native_delivery_retry", self.output)

    def test_missing_and_hostile_values(self):
        for value in ["", "claude=a,codex=b", "claude=a,codex=b,pi=p/m,pi=p/m",
                      "claude=a,codex=b,pi=/m", "claude=a,codex=b,pi=p/",
                      "claude=a b,codex=b,pi=p/m", "claude=$(id),codex=b,pi=p/m",
                      "claude=a;id,codex=b,pi=p/m"]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                MODELS.parse_models(value)

    def test_nonce_must_be_in_user_turn_not_history_or_assistant(self):
        p = self.root / "record.jsonl"
        data = records("claude", "other", "unrelated")
        data[-1]["message"]["content"][0]["text"] = "wanted"
        p.write_text(''.join(json.dumps(x) + "\n" for x in data))
        self.assertIsNone(MODELS.session_observation(p, "claude", "wanted"))

    def test_later_user_turn_does_not_satisfy_missing_probe_response(self):
        p = self.root / "record.jsonl"
        data = records("claude", "wanted", "expected", "no-turn") + records("claude", "other", "expected")
        p.write_text(''.join(json.dumps(x) + "\n" for x in data))
        self.assertEqual(MODELS.session_observation(p, "claude", "wanted"), {"error": "no_assistant_turn"})


if __name__ == "__main__":
    unittest.main()
