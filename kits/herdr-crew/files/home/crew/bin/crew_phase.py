"""Run-scoped native phase delivery. No terminal scraping or fallback transport.

The lock covers decision, delivery and recording. Atomic ledger replacement
lets status inspect recovery decisions without creating or changing any file.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time

OPEN = {"sent", "acknowledged", "unacked"}
PHASES = ("plan", "implement", "revise", "qc")
ERRORS = {"agent_prompt_stalled": 20, "agent_blocked": 21,
          "timeout": 22, "agent_not_running": 23}


class Refusal(Exception):
    def __init__(self, code, reason):
        self.code, self.reason = code, reason


def read_json(path):
    return json.loads(path.read_text())


def atomic_json(path, value):
    fd, name = tempfile.mkstemp(prefix=".handoff-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream, indent=2)
            stream.write("\n")
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def full_commit(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}", value) is not None


class Phases:
    def __init__(self, crew):
        self.crew = crew
        self.path = crew / "handoff.json"
        self.run = read_json(crew / "run.json")
        if (self.run.get("version") != 2
                or not re.fullmatch(r"[A-Za-z0-9_-]+", self.run.get("run_id", ""))):
            raise Refusal(26, "invalid_run_metadata")
        self.workspace = self.run["workspace"]
        self.ledger = read_json(self.path) if self.path.exists() else {
            "run_id": self.run["run_id"], "entries": []}
        if not isinstance(self.ledger.get("entries"), list):
            raise Refusal(26, "invalid_ledger")

    def save(self):
        atomic_json(self.path, self.ledger)

    def git(self, *args):
        return subprocess.run(["git", "-C", self.workspace, *args],
                              capture_output=True, text=True)

    def head(self):
        result = self.git("rev-parse", "HEAD")
        if result.returncode or not full_commit(result.stdout.strip()):
            raise Refusal(26, "unreadable_head")
        return result.stdout.strip()

    def ancestor(self, commit, head):
        if not full_commit(commit):
            return False
        result = self.git("merge-base", "--is-ancestor", commit, head)
        if result.returncode not in (0, 1):
            raise Refusal(26, "unreadable_ancestry")
        return result.returncode == 0

    def native(self, *args):
        timeout = int(args[args.index("--timeout") + 1]) / 1000 + 10 if "--timeout" in args else 15
        try:
            result = subprocess.run([os.environ.get("HERDR_BIN", "herdr"), "agent", *args],
                                    capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            raise Refusal(22, "timeout") from None
        value = None
        for stream in (result.stderr, result.stdout):
            try:
                candidate = json.loads(stream)
            except ValueError:
                continue
            if isinstance(candidate, dict):
                value = candidate
                if value.get("error"):
                    break
        if value is None:
            raise Refusal(26, "invalid_native_response")
        if value.get("error") or result.returncode:
            error = value.get("error") or {}
            code = error.get("code") if isinstance(error, dict) else None
            # Never print provider prose, terminal text, or credentials.
            raise Refusal(ERRORS.get(code, 26), code if code in ERRORS else "native_error")
        return value.get("result", {})

    def agent(self, role):
        value = self.native("get", role).get("agent", {})
        # Herdr 0.9.0 exposes AgentInfo at result.agent.
        if (not value.get("terminal_id") or not isinstance(value.get("state_change_seq"), int)
                or value.get("agent_status") not in ("idle", "done", "working", "blocked", "unknown")):
            raise Refusal(26, "invalid_agent_state")
        return value

    def latest(self, round_number, phase):
        return next((entry for entry in reversed(self.ledger["entries"])
                     if entry["round"] == round_number and entry["phase"] == phase), None)

    def decision(self, role, round_number, phase):
        if self.ledger["run_id"] != self.run["run_id"]:
            return "archive", None, None
        entry = self.latest(round_number, phase)
        head = self.head()
        if entry and entry["role"] != role:
            raise Refusal(25, "round_conflict")
        if entry and entry["state"] == "settled":
            commit = (entry.get("result") or {}).get("commit")
            if phase == "qc":
                # A QC review is valid only at its exact clean commit. Earlier
                # implementation/plan phases can still be reused after commits.
                porcelain = self.git("status", "--porcelain")
                if porcelain.returncode:
                    raise Refusal(26, "unreadable_worktree")
                complete = commit == head and not porcelain.stdout.strip()
            else:
                complete = self.ancestor(commit, head)
            if complete:
                return "already_complete", entry, None
        if any(e["state"] in OPEN and (e["round"], e["phase"]) != (round_number, phase)
               for e in self.ledger["entries"]):
            raise Refusal(25, "round_conflict")
        agent = self.agent(role)
        if entry and entry["state"] in OPEN:
            if agent["terminal_id"] != entry["terminal_id"]:
                return "resend", entry, agent
            if agent["agent_status"] == "working":
                return "wait", entry, agent
            if agent["agent_status"] == "blocked":
                raise Refusal(21, "agent_blocked")
            if entry["state"] == "acknowledged" and self.acknowledged(entry, agent):
                try:
                    self.evidence(round_number, phase)
                    return "record", entry, agent
                except (Refusal, OSError, ValueError):
                    pass  # No completion evidence yet; use the documented retry.
            return "resend", entry, agent
        if agent["agent_status"] == "working":
            raise Refusal(24, "busy")
        if agent["agent_status"] == "blocked":
            raise Refusal(21, "agent_blocked")
        return ("resend" if entry else "send"), entry, agent

    def archive(self):
        old = self.ledger["run_id"]
        if not isinstance(old, str) or not re.fullmatch(r"[A-Za-z0-9_-]+", old):
            raise Refusal(26, "invalid_ledger_run_id")
        target = self.crew / f"handoff.{old}.json"
        if target.exists():
            raise Refusal(26, "archive_exists")
        os.replace(self.path, target)
        self.ledger = {"run_id": self.run["run_id"], "entries": []}
        self.save()

    def acknowledged(self, entry, agent):
        return (agent["terminal_id"] == entry["terminal_id"]
                and agent["state_change_seq"] > entry["pre"]["state_change_seq"])

    def evidence(self, round_number, phase):
        head = self.head()
        status = self.git("status", "--porcelain")
        if status.returncode or (phase in ("implement", "revise") and status.stdout.strip()):
            raise Refusal(26, "dirty_or_unreadable_worktree")
        if phase == "plan":
            lines = (self.crew / "plan.md").read_text().splitlines()
            if not lines or lines[0] != f"PLAN run={self.run['run_id']} round={round_number}":
                raise Refusal(26, "plan_evidence_mismatch")
        elif phase == "qc":
            text = (self.crew / "qc-log.md").read_text()
            # Only the last entry can settle this round. Never search backwards
            # for a convenient PASS belonging to a previous review.
            headers = list(re.finditer(r"^QC run=(\S+) round=(\d+) commit=(\S+)$", text, re.M))
            if not headers:
                raise Refusal(26, "missing_qc_header")
            last = headers[-1]
            if last.groups() != (self.run["run_id"], str(round_number), head):
                raise Refusal(26, "qc_evidence_mismatch")
            verdicts = re.findall(r"^VERDICT: (PASS|FAIL)$", text[last.end():], re.M)
            if len(verdicts) != 1:
                raise Refusal(26, "missing_or_ambiguous_verdict")
            verdict = verdicts[0]
            if verdict == "PASS":
                if status.stdout.strip():
                    raise Refusal(26, "dirty_worktree")
                qc = read_json(self.crew / "qc.json")
                expected = {key: self.run[key] for key in
                            ("run_id", "goal_digest", "workspace", "scope", "base_commit")}
                expected.update(version=2, round=round_number, commit=head, verdict="PASS",
                                review={"started_commit": head, "finished_commit": head})
                if any(qc.get(key) != value for key, value in expected.items()):
                    raise Refusal(26, "qc_json_mismatch")
            return {"commit": head, "verdict": verdict}
        return {"commit": head}

    def record(self, round_number, phase, agent=None):
        if self.ledger["run_id"] != self.run["run_id"]:
            raise Refusal(26, "stale_run")
        entry = self.latest(round_number, phase)
        if not entry or entry["state"] not in OPEN | {"settled"}:
            raise Refusal(26, "no_phase_to_record")
        agent = agent or self.agent(entry["role"])
        if agent["agent_status"] == "blocked":
            raise Refusal(21, "agent_blocked")
        if agent["agent_status"] not in ("idle", "done") or not self.acknowledged(entry, agent):
            raise Refusal(20, "phase_not_settled")
        # An unacked delivery cannot be blessed just because a later state moved.
        if entry["state"] not in ("acknowledged", "settled"):
            raise Refusal(20, "phase_not_acknowledged")
        result = self.evidence(round_number, phase)
        status = self.git("status", "--porcelain")
        requires_clean = phase in ("implement", "revise") or result.get("verdict") == "PASS"
        if (self.head() != result["commit"] or status.returncode
                or (requires_clean and status.stdout.strip())):
            raise Refusal(26, "worktree_changed_during_record")
        entry.update(state="settled", result=result, ts=time.time())
        self.save()
        return {"decision": "settled", "seq": entry["seq"], "result": result}

    def deliver(self, role, round_number, phase, text, timeout):
        decision, entry, agent = self.decision(role, round_number, phase)
        if decision == "archive":
            self.archive()
            decision, entry, agent = self.decision(role, round_number, phase)
        if decision == "already_complete":
            return {"decision": decision, "seq": entry["seq"], "result": entry["result"]}
        if decision == "record":
            return self.record(round_number, phase, agent)
        if decision != "wait":
            previous = entry
            if previous:
                previous.update(state="superseded", ts=time.time())
            seq = max((e["seq"] for e in self.ledger["entries"]), default=0) + 1
            entry = {"round": round_number, "phase": phase, "role": role, "seq": seq,
                     "state": "sent", "terminal_id": agent["terminal_id"],
                     "pre": {"agent_status": agent["agent_status"],
                             "state_change_seq": agent["state_change_seq"], "head": self.head()},
                     "ack": None, "result": None, "ts": time.time()}
            self.ledger["entries"].append(entry)
            self.save()  # Persist before native delivery, including if the caller dies.
            header = f"HANDOFF run={self.run['run_id']} round={round_number} phase={phase} role={role} seq={seq}\n"
            if previous:
                header += (f"RETRY of seq {previous['seq']}: inspect git log/status and crew files first; "
                           "do not redo work already committed; report what exists\n")
            args = ("prompt", role, header + text, "--wait", "--timeout", str(timeout))
        else:
            args = ("wait", role, "--timeout", str(timeout))
        try:
            self.native(*args)
            after = self.agent(role)
            if not self.acknowledged(entry, after):
                raise Refusal(20, "missed_acknowledgment")
            entry.update(state="acknowledged", ack={"agent_status": after["agent_status"],
                         "state_change_seq": after["state_change_seq"]}, ts=time.time())
            self.save()
        except Refusal as error:
            if error.code == 20:
                entry.update(state="unacked", ts=time.time())
                self.save()
            raise
        return self.record(round_number, phase, after)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--crew-dir", type=Path, default=Path("/home/agent/crew"))
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("deliver", "status", "record"):
        command = sub.add_parser(name)
        command.add_argument("--round", type=int, required=True)
        command.add_argument("--phase", choices=PHASES, required=True)
        if name != "record":
            command.add_argument("--role", required=True)
        if name == "deliver":
            command.add_argument("--text-file", type=Path, required=True)
            command.add_argument("--timeout", type=int, default=1800000)
    args = parser.parse_args()
    try:
        if args.round < 1 or (args.command == "deliver" and args.timeout <= 0):
            raise Refusal(26, "invalid_round_or_timeout")
        if args.command == "status":
            decision, entry, _ = Phases(args.crew_dir).decision(args.role, args.round, args.phase)
            print(json.dumps({"decision": decision, "seq": entry["seq"] if entry else None}))
            return 0
        with (args.crew_dir / "handoff.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            phases = Phases(args.crew_dir)
            if args.command == "record":
                result = phases.record(args.round, args.phase)
            else:
                result = phases.deliver(args.role, args.round, args.phase,
                                        args.text_file.read_text(), args.timeout)
            print(json.dumps(result))
        return 0
    except Refusal as error:
        print(json.dumps({"error": error.reason, "exit_code": error.code}))
        return error.code
    except (OSError, ValueError, KeyError, TypeError):
        print(json.dumps({"error": "invalid_or_unreadable_phase_evidence", "exit_code": 26}))
        return 26


if __name__ == "__main__":
    raise SystemExit(main())
