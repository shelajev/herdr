"""Durable host archive for the task CLI's metadata-only collector."""
import datetime
from contextlib import contextmanager
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import uuid

ROLES = ("orchestrator", "planner", "implementer", "qc")
TOKEN_FIELDS = ("input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens")


def validate_report(value):
    """Do not archive arbitrary sandbox stdout, even if it is valid JSON."""
    def ident(v):
        if v is None:
            return None
        if not isinstance(v, str) or not re.fullmatch(r"[A-Za-z0-9_.:/+-]{1,512}", v):
            raise ValueError("invalid metadata identifier")
        return v

    def number(v):
        if v is not None and (type(v) is not int or v < 0):
            raise ValueError("invalid counter")
        return v

    def path(v):
        if not isinstance(v, str) or not v.startswith("/") or len(v) > 4096 or re.search(r"[\x00-\x1f\x7f]", v):
            raise ValueError("invalid source path")
        return v

    if value.get("schema_version") != 1 or value.get("scope") != "sandbox_lifetime":
        raise ValueError("unsupported usage schema")
    sessions = []
    for s in value["sessions"]:
        if s["role"] not in (*ROLES, None) or s["harness"] not in ("codex", "claude", "pi"):
            raise ValueError("invalid session role or harness")
        if s["coverage"] not in ("complete", "partial"):
            raise ValueError("invalid session coverage")
        row = {"role": s["role"], "harness": s["harness"], "coverage": s["coverage"],
               "provider": ident(s.get("provider")), "actual_model": ident(s.get("actual_model")),
               "session_id": ident(s.get("session_id")), "source_path": path(s["source_path"]),
               "tokens": {f: number(s["tokens"].get(f)) for f in TOKEN_FIELDS},
               "issues": [ident(i) for i in s["issues"]],
               "native_input_tokens": number(s.get("native_input_tokens")),
               "request_count": number(s.get("request_count")),
               "session_id_provenance": s.get("session_id_provenance") if s.get("session_id_provenance") in ("native_record", "filename") else None,
               "input_semantics": "exclusive input: excludes cache read and cache write",
               "native_input_semantics": "includes cache read and cache write" if s["harness"] == "codex" else "uncached; caches separate"}
        if s.get("source_paths"):
            row["source_paths"] = [path(p) for p in s["source_paths"]]
        sessions.append(row)
    assignments = value.get("assignments", {})
    if any(r not in ROLES or k not in ("codex", "claude", "pi") for r, k in assignments.items()):
        raise ValueError("invalid role assignments")
    run = value.get("run") or {}
    metadata = {"run_id": ident(run.get("run_id"))}
    if run.get("workspace"):
        metadata["workspace"] = path(run["workspace"])
    if run.get("base_commit"):
        if not re.fullmatch(r"[a-f0-9]{40,64}", run["base_commit"]):
            raise ValueError("invalid commit")
        metadata["base_commit"] = run["base_commit"]
    qc = value.get("reported_qc")
    if qc is not None:
        qc = {k: ident(qc.get(k)) for k in ("run_id", "commit", "qc", "implementer", "verdict")}
    coverage = value["coverage"]
    if coverage not in ("complete", "partial"):
        raise ValueError("invalid coverage")
    missing = value["missing_roles"]
    if any(r not in ROLES for r in missing):
        raise ValueError("invalid missing role")
    return {"schema_version": 1, "scope": "sandbox_lifetime", "sessions": sessions,
            "assignments": assignments, "run": metadata, "reported_qc": qc,
            "reported_result": value.get("reported_result") if value.get("reported_result") in ("DONE", "FAILED", "BLOCKED") else None,
            "coverage": coverage, "missing_roles": missing, "issues": [ident(i) for i in value["issues"]],
            "unattributed_session_count": number(value.get("unattributed_session_count"))}


@contextmanager
def task_lock(path):
    with path.open("a+b") as lock:
        if os.name == "nt":
            import msvcrt
            lock.seek(0)
            if not lock.read(1):
                lock.write(b"0")
                lock.flush()
            lock.seek(0)
            msvcrt.locking(lock.fileno(), msvcrt.LK_LOCK, 1)
            try:
                yield
            finally:
                lock.seek(0)
                msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
        else:
            import fcntl
            fcntl.flock(lock, fcntl.LOCK_EX)
            yield


def atomic_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.is_symlink():
        raise ValueError("archive file is a symlink")
    fd, name = tempfile.mkstemp(prefix=".usage-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def checked_id(value):
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,199}", value):
        raise ValueError("invalid archive identifier")
    return value


def history_root():
    override = os.environ.get("HERDR_TASK_HISTORY_ROOT")
    if override:
        return Path(override).expanduser().resolve()
    # Central host store, even when called in a nested project checkout.
    for parent in (Path.cwd(), *Path.cwd().parents):
        if (parent / ".beans").is_dir():
            return parent / ".herdr-history"
    return Path.home() / ".local/state/herdr/task-history"


def merge_snapshot(old, new):
    """Upsert whole session/model snapshots, never sum repeated cumulative exports."""
    records = {}
    replaced = {(s["harness"], s["session_id"] or s["source_path"]) for s in new["sessions"]}
    for session in old.get("sessions", []):
        if (session["harness"], session["session_id"] or session["source_path"]) not in replaced:
            key = (session["role"], session["harness"], session["session_id"] or session["source_path"], session["provider"], session["actual_model"])
            records[key] = session
    for session in new["sessions"]:
        key = (session["role"], session["harness"], session["session_id"] or session["source_path"], session["provider"], session["actual_model"])
        records[key] = session
    new["sessions"] = sorted(records.values(), key=lambda s: (
        s["role"] or "", s["harness"], s["source_path"], s["provider"] or "", s["actual_model"] or ""))
    runs = {r["run_id"]: r for r in old.get("goal_runs", []) if r.get("run_id")}
    if new.get("run", {}).get("run_id"):
        runs[new["run"]["run_id"]] = new["run"]
    new["goal_runs"] = list(runs.values())
    # Keep evidence gaps visible across captures, even if a later registry looks healthy.
    new["issues"] = sorted((set(old.get("issues", [])) - {"active_agent_sessions_snapshot"}) | set(new.get("issues", [])))
    new["missing_roles"] = [r for r in ("orchestrator", "planner", "implementer", "qc")
                            if not any(s["role"] == r for s in new["sessions"])]
    if new["issues"] or new["missing_roles"] or any(s["coverage"] != "complete" for s in new["sessions"]):
        new["coverage"] = "partial"
    return new


def summary(report):
    lines = [f"coverage: {report['coverage']} ({report['scope']})",
             "role | provider | model | sessions | uncached input | output | cache read | cache write"]
    groups = {}
    for s in report["sessions"]:
        groups.setdefault((s["role"], s["provider"], s["actual_model"]), []).append(s)
    for (role, provider, model), sessions in sorted(groups.items(), key=lambda x: tuple(v or "" for v in x[0])):
        values = []
        for field in ("input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens"):
            counters = [s["tokens"][field] for s in sessions]
            values.append(str(sum(counters)) if all(v is not None for v in counters) else "unavailable")
        count = str(len({(s["harness"], s["session_id"] or s["source_path"]) for s in sessions})) if all(
            s.get("session_id_provenance") == "native_record" for s in sessions) else "unavailable"
        lines.append(" | ".join([role or "unattributed", provider or "unavailable", model or "unavailable", count, *values]))
    for role in report["missing_roles"]:
        lines.append(role + " | " + " | ".join(["unavailable"] * 7))
    return "\n".join(lines)


def execute(request):
    slug = checked_id(request["slug"])
    registry_root = Path.home() / ".local/state/herdr/task-usage"
    registry_root.mkdir(parents=True, exist_ok=True)
    # Serialize archive snapshots and registry changes on this task only.
    with task_lock(registry_root / (slug + ".lock")):
        registry_path = registry_root / (slug + ".json")
        existing = json.loads(registry_path.read_text()) if registry_path.exists() else None
        command = request["command"]
        if command == "init" or existing is None:
            now = datetime.datetime.now(datetime.timezone.utc)
            existing = {"slug": slug, "bean_id": checked_id(request.get("bean_id") or slug),
                        "run_id": checked_id(request.get("run_id") or (slug + "-" + now.strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:8])),
                        "started_at": now.isoformat(), "history_root": str(history_root()),
                        "start_time_provenance": "task_creation" if command == "init" else "legacy_adoption",
                        "workspace": request.get("workspace")}
        elif request.get("bean_id") or request.get("run_id"):
            # Relinking an existing archive must be explicit and non-destructive.
            existing["bean_id"] = checked_id(request.get("bean_id") or existing["bean_id"])
            existing["run_id"] = checked_id(request.get("run_id") or existing["run_id"])
        archive = Path(existing["history_root"]) / existing["bean_id"] / existing["run_id"]
        if archive.is_symlink() or archive.parent.is_symlink():
            raise ValueError("archive directory is a symlink")
        atomic_json(registry_path, existing)
        metadata_path = archive / "run.json"
        previous_metadata = json.loads(metadata_path.read_text()) if metadata_path.exists() else {}
        atomic_json(metadata_path, {**previous_metadata, **existing})
        if command == "init":
            print(f"task history: {archive}")
            return 0
        if command == "closed":
            inventory = subprocess.run(["sbx", "ls", "--quiet"], capture_output=True, text=True, timeout=30)
            if inventory.returncode != 0:
                raise ValueError("sandbox deletion could not be verified")
            if "herdr-task-" + slug in inventory.stdout.splitlines():
                return 0  # The operator declined sbx's confirmation; task remains live.
            existing["ended_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            existing["sandbox_deleted"] = True
            atomic_json(registry_path, existing)
            atomic_json(metadata_path, {**previous_metadata, **existing})
            return 0
        if command == "unavailable":
            target = archive / "usage.json"
            report = json.loads(target.read_text()) if target.exists() else {
                "schema_version": 1, "scope": "sandbox_lifetime", "sessions": [],
                "missing_roles": ["orchestrator", "planner", "implementer", "qc"], "issues": []}
            report.update(bean_id=existing["bean_id"], host_run_id=existing["run_id"], slug=slug,
                          coverage="partial" if report["sessions"] else "unavailable")
            report["issues"] = sorted(set(report["issues"]) | {"explicit_shutdown_without_final_capture"})
            atomic_json(target, report)
            print(f"usage archive (final counters unavailable): {target}")
            return 0
        remote_command = "python3 -c " + shlex.quote(request["collector"])
        response = subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10",
                                   "herdr-task-" + slug + ".sbx", remote_command],
                                  capture_output=True, text=True, timeout=120)
        if response.returncode:
            print("Usage export failed; preserve the sandbox and retry `herdr task usage " + slug + "`.", file=sys.stderr)
            return 1
        report = validate_report(json.loads(response.stdout))
        report["bean_id"] = existing["bean_id"]
        report["host_run_id"] = existing["run_id"]
        report["slug"] = slug
        report["exported_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        report["goal_runs"] = [report["run"]] if report.get("run", {}).get("run_id") else []
        target = archive / "usage.json"
        if target.exists():
            previous = json.loads(target.read_text())
            if previous.get("schema_version") != report["schema_version"]:
                raise ValueError("existing usage schema differs; choose a new run ID")
            report = merge_snapshot(previous, report)
        atomic_json(target, report)
        if command != "capture-quiet":
            print(f"usage archive: {target}")
            print(summary(report))
        return 0


def main():
    try:
        return execute(json.load(sys.stdin))
    except (OSError, ValueError, KeyError, subprocess.TimeoutExpired):
        print("Cannot archive task usage; check history directory permissions, identifiers and SSH, then retry. Sandbox preserved.", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
