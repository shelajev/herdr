"""Native phase recovery scenarios using real git repos and a scripted CLI."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from scripts.test_crew_models import NATIVE_STALLED

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "kits/herdr-crew/files/home/crew/bin/crew_phase.py"
STUB = r'''#!/usr/bin/env python3
import fcntl, json, os, sys, time
from pathlib import Path
root = Path(os.environ['SCENARIO_DIR'])
with (root / 'stub.lock').open('a') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    script = json.loads((root / 'scenario.json').read_text())
    call = sys.argv[1:]
    with (root / 'calls.jsonl').open('a') as f:
        f.write(json.dumps(call) + '\n')
    if not script:
        print(json.dumps({'error': {'code': 'unexpected_call'}}), file=sys.stderr)
        sys.exit(1)
    step = script.pop(0)
    (root / 'scenario.json').write_text(json.dumps(script))
    if call[:2] != ['agent', step['command']]:
        print(json.dumps({'error': {'code': 'wrong_call'}}), file=sys.stderr)
        sys.exit(1)
    for name, content in step.get('files', {}).items():
        (root / 'crew' / name).write_text(content)
    time.sleep(step.get('delay', 0))
    print(json.dumps(step['response']), file=sys.stderr if step.get('exit', 0) else sys.stdout)
    sys.exit(step.get('exit', 0))
'''


def state(status="idle", seq=1, terminal="t1"):
    return {"command": "get", "response": {"result": {"agent": {
        "agent_status": status, "state_change_seq": seq, "terminal_id": terminal}}}}


def native(command="prompt", error=None, **extra):
    response = {"result": {}}
    if error:
        response = NATIVE_STALLED if error == "agent_prompt_stalled" else {"error": {"code": error}}
    return {"command": command, "response": response, "exit": int(bool(error)), **extra}


class PhaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.crew = self.root / "crew"
        self.repo = self.root / "repo"
        self.crew.mkdir()
        self.repo.mkdir()
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.test")
        self.git("commit", "--allow-empty", "-qm", "base")
        self.head = self.git("rev-parse", "HEAD").strip()
        self.run = {"version": 2, "run_id": "run-current", "workspace": str(self.repo),
                    "goal_digest": "fixture", "scope": "change", "base_commit": self.head}
        self.write_run()
        self.stub = self.root / "herdr"
        self.stub.write_text(STUB)
        self.stub.chmod(0o755)
        self.text = self.root / "prompt.txt"
        self.text.write_text("Do the fixture phase.")
        self.env = {**os.environ, "HERDR_BIN": str(self.stub), "SCENARIO_DIR": str(self.root)}

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.repo), *args], text=True, stderr=subprocess.DEVNULL)

    def write_run(self):
        (self.crew / "run.json").write_text(json.dumps(self.run))

    def script(self, *steps):
        (self.root / "scenario.json").write_text(json.dumps(steps))

    def argv(self, command="deliver", phase="implement", round_number=1):
        args = [sys.executable, str(HELPER), "--crew-dir", str(self.crew), command,
                "--phase", phase, "--round", str(round_number)]
        if command != "record":
            args += ["--role", "worker"]
        if command == "deliver":
            args += ["--text-file", str(self.text), "--timeout", "10000"]
        return args

    def call(self, command="deliver", phase="implement", round_number=1, code=0):
        result = subprocess.run(self.argv(command, phase, round_number), env=self.env,
                                capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, code, result.stdout + result.stderr)
        return json.loads(result.stdout)

    def calls(self, command):
        path = self.root / "calls.jsonl"
        return [call for line in path.read_text().splitlines()
                if (call := json.loads(line))[1] == command] if path.exists() else []

    def ledger(self):
        return json.loads((self.crew / "handoff.json").read_text())

    def entry(self, phase="implement", round_number=1, status="sent", **extra):
        return {"round": round_number, "phase": phase, "role": "worker", "seq": 1,
                "state": status, "terminal_id": "t1", "pre": {"agent_status": "idle", "state_change_seq": 1, "head": self.head},
                "ack": None, "result": None, "ts": 0, **extra}

    def seed(self, *entries, run_id=None):
        (self.crew / "handoff.json").write_text(json.dumps({"run_id": run_id or self.run["run_id"], "entries": entries}))

    def success(self, **kwargs):
        self.script(state(), native(**kwargs), state("done", 3))
        return self.call()

    def test_success_and_duplicate_send_nothing(self):
        self.success()
        self.assertEqual(self.ledger()["entries"][0]["state"], "settled")
        self.assertEqual(len(self.calls("prompt")), 1)
        self.assertIn("HANDOFF run=run-current round=1 phase=implement role=worker seq=1\n", self.calls("prompt")[0][3])
        self.assertEqual(self.call()["decision"], "already_complete")
        self.assertEqual(len(self.calls("prompt")), 1)

    def test_real_stalled_error_then_retry(self):
        self.script(state(), native(error="agent_prompt_stalled"))
        self.call(code=20)
        self.assertEqual(self.ledger()["entries"][0]["state"], "unacked")
        self.script(state(), native(), state("done", 4))
        self.call()
        entries = self.ledger()["entries"]
        self.assertEqual([e["state"] for e in entries], ["superseded", "settled"])
        self.assertEqual(entries[-1]["seq"], 2)
        self.assertIn("RETRY of seq 1: inspect git log/status", self.calls("prompt")[-1][3])

    def test_working_recovery_waits_without_prompting(self):
        self.seed(self.entry())
        self.script(state("working", 2), native("wait"), state("done", 3))
        self.call()
        self.assertEqual(self.calls("prompt"), [])
        self.assertEqual(len(self.calls("wait")), 1)

    def test_changed_terminal_supersedes_and_resends(self):
        self.seed(self.entry())
        self.script(state(terminal="t2"), native(), state("done", 3, "t2"))
        self.call()
        self.assertEqual([e["state"] for e in self.ledger()["entries"]], ["superseded", "settled"])
        self.assertIn("RETRY", self.calls("prompt")[0][3])

    def test_new_run_archives_ledger(self):
        self.seed(self.entry(), run_id="old-run")
        self.success()
        self.assertEqual(json.loads((self.crew / "handoff.old-run.json").read_text())["run_id"], "old-run")
        self.assertEqual(self.ledger()["run_id"], "run-current")

    def test_rewound_history_is_not_already_complete(self):
        self.git("commit", "--allow-empty", "-qm", "later")
        later = self.git("rev-parse", "HEAD").strip()
        self.git("reset", "--hard", self.head)
        self.seed(self.entry(status="settled", result={"commit": later}))
        self.success()
        self.assertEqual(len(self.calls("prompt")), 1)
        self.assertEqual(self.ledger()["entries"][-1]["seq"], 2)

    def test_only_qc_reuse_requires_exact_clean_head(self):
        self.git("commit", "--allow-empty", "-qm", "after review")
        # Historical implementation and plan work remains completed.
        for phase in ["plan", "implement", "revise"]:
            self.seed(self.entry(phase=phase, status="settled", result={"commit": self.head}))
            self.script()
            self.assertEqual(self.call(phase=phase)["decision"], "already_complete")
        # A later descendant invalidates QC. A new native QC prompt is sent;
        # without fresh report files it cannot settle or claim acceptance.
        self.seed(self.entry(phase="qc", status="settled", result={"commit": self.head, "verdict": "PASS"}))
        self.script(state(), native(), state("done", 3))
        self.call(phase="qc", code=26)
        self.assertEqual(len(self.calls("prompt")), 1)
        self.assertEqual(self.ledger()["entries"][-1]["state"], "acknowledged")
        self.git("reset", "--hard", self.head)
        dirty = self.repo / "untracked"
        dirty.write_text("dirty")
        self.seed(self.entry(phase="qc", status="settled", result={"commit": self.head, "verdict": "PASS"}))
        self.script(state(), native(), state("done", 3))
        self.call(phase="qc", code=26)
        self.assertEqual(len(self.calls("prompt")), 2)
        dirty.unlink()
        self.seed(self.entry(phase="qc", status="settled", result={"commit": self.head, "verdict": "PASS"}))
        self.script()
        self.assertEqual(self.call(phase="qc")["decision"], "already_complete")
        self.assertEqual(len(self.calls("prompt")), 2)

    def test_committed_phase_survives_restart_with_next_phase_open(self):
        self.seed(self.entry(status="settled", result={"commit": self.head}),
                  self.entry(phase="qc", status="sent", seq=2))
        self.script()
        self.assertEqual(self.call()["decision"], "already_complete")
        self.assertEqual(self.calls("prompt"), [])
        self.assertEqual(self.ledger()["entries"][1]["state"], "sent")

    def test_busy_blocked_and_round_conflict(self):
        for status, code in [("working", 24), ("blocked", 21)]:
            with self.subTest(status=status):
                self.script(state(status))
                self.call(code=code)
        self.seed(self.entry(phase="plan"))
        self.script()
        self.call(code=25)
        self.assertEqual(self.calls("prompt"), [])

    def test_native_error_mapping_and_timeout_wait_recovery(self):
        for error, code in [("agent_blocked", 21), ("agent_not_running", 23), ("timeout", 22)]:
            with self.subTest(error=error):
                self.seed()
                self.script(state(), native(error=error))
                self.call(code=code)
                self.assertNotEqual(self.ledger()["entries"][-1]["state"], "settled")
        count = len(self.calls("prompt"))
        self.script(state("working", 2), native("wait"), state("done", 3))
        self.call()
        self.assertEqual(len(self.calls("prompt")), count)
        self.assertEqual(len(self.calls("wait")), 1)

    def test_stale_sequence_or_changed_terminal_never_acknowledges(self):
        for after in [state("working", 1), state("done", 3, "t2")]:
            self.seed()
            self.script(state(), native(), after)
            self.call(code=20)
            self.assertEqual(self.ledger()["entries"][-1]["state"], "unacked")

    def test_concurrent_delivers_serialize_and_prompt_once(self):
        self.script(state(), native(delay=0.2), state("done", 3))
        processes = [subprocess.Popen(self.argv(), env=self.env, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True) for _ in range(2)]
        for process in processes:
            stdout, stderr = process.communicate(timeout=15)
            self.assertEqual(process.returncode, 0, stdout + stderr)
        self.assertEqual(len(self.calls("prompt")), 1)
        self.assertEqual(len(self.ledger()["entries"]), 1)

    def test_plan_header_and_acknowledged_record_recovery(self):
        self.script(state(), native(), state("done", 3))
        self.call(phase="plan", code=26)
        self.assertEqual(self.ledger()["entries"][-1]["state"], "acknowledged")
        (self.crew / "plan.md").write_text("PLAN run=run-current round=1\nPlan details\n")
        self.script(state("done", 3))
        self.call(phase="plan")
        self.assertEqual(len(self.calls("prompt")), 1)

    def test_record_rejects_stale_qc_verdict_and_checks_v2_report(self):
        self.seed(self.entry(phase="qc", round_number=2, status="acknowledged"))
        path = self.crew / "qc-log.md"
        path.write_text(f"QC run=run-current round=1 commit={self.head}\nVERDICT: PASS\n")
        self.script(state("done", 3))
        self.call("record", "qc", 2, code=26)
        path.write_text(f"QC run=run-current round=2 commit={self.head}\nVERDICT: PASS\n")
        qc = {**self.run, "round": 1, "commit": self.head, "verdict": "PASS",
              "review": {"started_commit": self.head, "finished_commit": self.head}}
        (self.crew / "qc.json").write_text(json.dumps(qc))
        self.script(state("done", 3))
        self.call("record", "qc", 2, code=26)
        qc["round"] = 2
        (self.crew / "qc.json").write_text(json.dumps(qc))
        self.script(state("done", 3))
        self.assertEqual(self.call("record", "qc", 2)["result"]["verdict"], "PASS")

    def test_planning_and_failed_qc_can_record_without_claiming_clean_acceptance(self):
        # Starting dirty is allowed: planning is not exact-commit QC acceptance.
        (self.repo / "untracked").write_text("work to fix")
        (self.crew / "plan.md").write_text("PLAN run=run-current round=1\nPlan details\n")
        self.seed(self.entry(phase="plan", status="acknowledged"))
        self.script(state("done", 3))
        self.call("record", "plan")
        # A failed review must release the phase so revision can fix the tree.
        (self.crew / "qc-log.md").write_text(f"QC run=run-current round=1 commit={self.head}\nVERDICT: FAIL\n")
        self.seed(self.entry(phase="qc", status="acknowledged"))
        self.script(state("done", 3))
        self.assertEqual(self.call("record", "qc")["result"]["verdict"], "FAIL")
        self.script(state())
        self.assertEqual(self.call("status", "revise", 2)["decision"], "send")
        # The same dirty review cannot be presented as successful QC.
        (self.crew / "qc-log.md").write_text(f"QC run=run-current round=1 commit={self.head}\nVERDICT: PASS\n")
        self.seed(self.entry(phase="qc", status="acknowledged"))
        self.script(state("done", 3))
        self.call("record", "qc", code=26)

    def test_record_refuses_dirty_tree_or_unacked_state(self):
        self.seed(self.entry(status="unacked"))
        self.script(state("done", 3))
        self.call("record", code=20)
        self.seed(self.entry(status="acknowledged"))
        (self.repo / "untracked").write_text("dirty")
        self.script(state("done", 3))
        self.call("record", code=26)

    def test_status_has_no_side_effects_even_for_stale_run(self):
        self.seed(self.entry(), run_id="old-run")
        before = {p.name: p.read_bytes() for p in self.crew.iterdir()}
        self.assertEqual(self.call("status")["decision"], "archive")
        self.assertEqual(before, {p.name: p.read_bytes() for p in self.crew.iterdir()})
        self.seed()
        self.script(state("idle"))
        before = {p.name: p.read_bytes() for p in self.crew.iterdir()}
        self.assertEqual(self.call("status")["decision"], "send")
        self.assertEqual(before, {p.name: p.read_bytes() for p in self.crew.iterdir()})


if __name__ == "__main__":
    unittest.main()
