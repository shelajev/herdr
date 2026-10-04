"""Read native session usage locally; emit an allowlisted metadata-only snapshot.

Executed by the host task CLI over SSH. Never emits transcript text or costs.
Input counters are normalized to uncached input; raw counter semantics retained.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROLES = ("orchestrator", "planner", "implementer", "qc")
FIELDS = ("input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens")
IDENTIFIER = re.compile(r"^[A-Za-z0-9_.:/+-]{1,512}$")


def identifier(value):
    return value if isinstance(value, str) and IDENTIFIER.fullmatch(value) else None


def counter(value):
    return value if type(value) is int and value >= 0 else None


def rows(path, issues):
    with path.open(encoding="utf-8") as stream:
        for line in stream:
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                issues.add("invalid_or_incomplete_record")
                continue
            if isinstance(value, dict):
                yield value


def parse_session(path, kind, role):
    issues = set()
    segments = {}
    requests = {}
    session_id = None
    model = provider = None
    previous = None
    native_inputs = {}
    native_keys = ("input_tokens", "output_tokens", "cached_input_tokens", "cache_write_input_tokens")
    for entry in rows(path, issues):
        payload = entry.get("payload", {})
        if not isinstance(payload, dict):
            payload = {}
        message = entry.get("message", {})
        if not isinstance(message, dict):
            message = {}
        if kind == "codex":
            if entry.get("type") == "session_meta":
                session_id = identifier(payload.get("id"))
                provider = identifier(payload.get("model_provider"))
            if entry.get("type") == "turn_context":
                model = identifier(payload.get("model"))
                provider = identifier(payload.get("model_provider")) or provider
            if payload.get("type") != "token_count":
                continue
            info = payload.get("info")
            if not isinstance(info, dict) or not isinstance(info.get("total_token_usage"), dict):
                continue
            total = info["total_token_usage"]
            values = tuple(counter(total.get(k)) for k in native_keys)
            if values == previous:
                continue  # A repeated cumulative event is not another request.
            delta = tuple(v - p if v is not None and p is not None else None
                          for v, p in zip(values, previous or (0, 0, 0, 0)))
            if any(v is not None and v < 0 for v in delta):
                issues.add("cumulative_counter_decreased")
                previous = values
                continue
            previous = values
            raw_input, output, cache_read, cache_write = delta
            # Responses input_tokens_details contains both cache subsets within
            # input_tokens. Make categories exclusive, retaining native totals.
            # Source: openai/codex codex-api/src/sse/responses.rs.
            uncached = raw_input - cache_read - cache_write if all(
                x is not None for x in (raw_input, cache_read, cache_write)) else None
            if uncached is not None and uncached < 0:
                issues.add("cache_exceeds_input")
                uncached = None
            usage = dict(zip(FIELDS, (uncached, output, cache_read, cache_write)))
            key = (provider, model)
            native_inputs.setdefault(key, []).append(raw_input)
            segment = segments.setdefault(key, {f: [] for f in FIELDS})
            for field in FIELDS:
                segment[field].append(usage[field])
        else:
            session_id = identifier(entry.get("sessionId")) or session_id
            if kind == "pi" and entry.get("type") == "session":
                session_id = identifier(entry.get("id"))
            if message.get("role") != "assistant" or not isinstance(message.get("usage"), dict):
                continue
            if message.get("model") == "<synthetic>":
                continue
            model = identifier(message.get("model"))
            provider = identifier(message.get("provider"))
            request_id = identifier(entry.get("requestId")) or identifier(message.get("id"))
            request_id = request_id or identifier(entry.get("id")) or identifier(entry.get("uuid"))
            if request_id is None:
                issues.add("request_identity_missing")
                continue
            u = message["usage"]
            keys = (("input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens")
                    if kind == "claude" else ("input", "output", "cacheRead", "cacheWrite"))
            usage = dict(zip(FIELDS, (counter(u.get(k)) for k in keys)))
            value = (provider, model, usage)
            if request_id in requests and requests[request_id] != value:
                issues.add("request_snapshot_changed")
            requests[request_id] = value  # Latest snapshot per native request, not per content block.
    for provider, model, usage in requests.values():
        segment = segments.setdefault((provider, model), {f: [] for f in FIELDS})
        for field in FIELDS:
            segment[field].append(usage[field])
    session_id_provenance = "native_record" if session_id else "filename"
    if not session_id:
        session_id = identifier(path.stem)
        issues.add("native_session_id_unavailable")
    if not segments:
        issues.add("usage_not_recorded")
        segments[(provider, model)] = {f: [None] for f in FIELDS}
    result = []
    for (provider, model), values in segments.items():
        totals = {f: sum(v) if v and all(x is not None for x in v) else None
                  for f, v in values.items()}
        missing = [f for f in FIELDS if totals[f] is None]
        segment_issues = issues | ({"counter_unavailable"} if missing else set())
        if model is None:
            segment_issues.add("actual_model_unavailable")
        result.append({"role": role, "harness": kind, "provider": provider, "actual_model": model,
                       "session_id": session_id, "source_path": str(path), "tokens": totals,
                       "session_id_provenance": session_id_provenance,
                       "coverage": "partial" if segment_issues else "complete",
                       "issues": sorted(segment_issues),
                       "input_semantics": "exclusive input: excludes cache read and cache write",
                       "native_input_tokens": (sum(native_inputs[(provider, model)]) if kind == "codex" and
                           all(v is not None for v in native_inputs.get((provider, model), [None])) else totals["input_tokens"] if kind != "codex" else None),
                       "native_input_semantics": "includes cache read and cache write" if kind == "codex" else "uncached; caches separate",
                       "request_count": len(next(iter(values.values()))) if kind == "codex" else sum(
                           1 for p, m, _ in requests.values() if (p, m) == (provider, model))})
    return result


def unique_sessions(sessions):
    """One native session/model snapshot even when a resumed log has two paths."""
    result = {}
    for session in sessions:
        key = (session["harness"], session["session_id"] or session["source_path"],
               session["provider"], session["actual_model"])
        previous = result.get(key)
        if previous is not None:
            if session["role"] is None:
                session["role"] = previous["role"]
            elif previous["role"] is not None and previous["role"] != session["role"]:
                session["role"] = None
                session["issues"] = sorted(set(session["issues"]) | {"role_attribution_conflict"})
                session["coverage"] = "partial"
            session["source_paths"] = sorted(set(previous.get("source_paths", [previous["source_path"]]))
                                             | {session["source_path"]})
            if previous["tokens"] != session["tokens"]:
                session["issues"] = sorted(set(session["issues"]) | {"duplicate_session_snapshot_conflict"})
                session["coverage"] = "partial"
        result[key] = session
    return list(result.values())


def content_text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(b.get("text", "") for b in content if isinstance(b, dict)
                         and isinstance(b.get("text"), str))
    return ""


def recover_role(path, kind, assignments):
    """Inspect only controller user headers; no transcripts leave the process."""
    roles = set()
    for entry in rows(path, set()):
        message = entry.get("payload", {}) if kind == "codex" else entry.get("message", {})
        if not isinstance(message, dict) or message.get("role") != "user":
            continue
        text = content_text(message.get("content"))
        if text.startswith("<pasted_content>"):
            text = text[len("<pasted_content>"):].lstrip("\r\n")
        # Match the controller header at the start, never instructions quoted in email/tool content.
        match = re.match(r"\AHANDOFF run=[A-Za-z0-9_-]+[^\n]*\brole=(orchestrator|planner|implementer|qc)\b", text)
        if match and assignments.get(match[1]) == kind:
            roles.add(match[1])
    return next(iter(roles)) if len(roles) == 1 else None


def roots(home):
    return {"codex": Path(os.environ.get("CODEX_HOME", str(home / ".codex"))) / "sessions",
            "claude": home / ".claude/projects", "pi": home / ".pi/agent/sessions"}


def persisted_agents(home):
    """Read native session references when the runtime has already stopped."""
    config = Path(os.environ.get("XDG_CONFIG_HOME", str(home / ".config"))) / "herdr"
    snapshot = config / "session.json"
    if not snapshot.exists():
        return []
    value = json.loads(snapshot.read_text())
    agents = []
    for workspace in value.get("workspaces", []):
        for tab in workspace.get("tabs", []):
            for pane in tab.get("panes", {}).values():
                agents.append({"name": pane.get("agent_name"), "agent": pane.get("managed_agent_kind"),
                               "agent_session": pane.get("agent_session")})
    return agents


def collect(crew, home):
    assignments = dict(part.split("=", 1) for part in (crew / "assignment").read_text().strip().split(","))
    if set(assignments) != set(ROLES) or any(k not in ("codex", "claude", "pi") for k in assignments.values()):
        raise ValueError("invalid role manifest")
    root_paths = roots(home)
    paths = {kind: sorted(root.rglob("*.jsonl")) for kind, root in root_paths.items()}
    attribution = {}
    issues = []
    agents = []
    response = subprocess.run(["herdr", "agent", "list"], capture_output=True, text=True, timeout=20)
    if response.returncode == 0:
        agents = json.loads(response.stdout).get("result", {}).get("agents", [])
        if any(a.get("agent_status") == "working" for a in agents):
            issues.append("active_agent_sessions_snapshot")
    else:
        agents = persisted_agents(home)
        issues.append("live_agent_registry_unavailable")
    for agent in agents:
        role = agent.get("name")
        kind = agent.get("agent")
        ref = agent.get("agent_session") or {}
        if role not in ROLES or assignments.get(role) != kind:
            continue
        for path in paths.get(kind, []):
            sid = identifier(ref.get("value"))
            if (ref.get("kind") == "path" and str(path) == ref.get("value")) or (
                    ref.get("kind") == "id" and sid and (path.stem == sid or path.stem.endswith("-" + sid) or path.stem.endswith("_" + sid))):
                attribution[(kind, path)] = role
    registry_roles = [{"role": a.get("name"), "harness": assignments[a["name"]],
                       "session_reference_kind": ((a.get("agent_session") or {}).get("kind") if
                           (a.get("agent_session") or {}).get("kind") in ("id", "path") else None),
                       "session_reference_present": bool(a.get("agent_session"))}
                      for a in agents if a.get("name") in ROLES]
    # Registry only reports the current session. Headers recover replaced workers and prior goals.
    for kind, candidates in paths.items():
        for path in candidates:
            if (kind, path) not in attribution:
                try:
                    role = recover_role(path, kind, assignments)
                except (OSError, UnicodeError):
                    issues.append("session_unreadable")
                    continue
                if role:
                    attribution[(kind, path)] = role
    sessions = []
    for (kind, path), role in attribution.items():
        try:
            sessions.extend(parse_session(path, kind, role))
        except (OSError, UnicodeError):
            issues.append("session_unreadable")
    for kind, candidates in paths.items():
        for path in candidates:
            if (kind, path) not in attribution:
                try:
                    records = parse_session(path, kind, None)
                    for record in records:
                        record["issues"] = sorted(set(record["issues"]) | {"role_attribution_unavailable"})
                        record["coverage"] = "partial"
                    sessions.extend(records)
                except (OSError, UnicodeError):
                    issues.append("session_unreadable")
    missing = [role for role in ROLES if not any(s["role"] == role for s in sessions)]
    unassigned = sum(1 for kind, candidates in paths.items() for p in candidates if (kind, p) not in attribution)
    if unassigned:
        issues.append("unattributed_native_sessions")
    # Allowlist run metadata: goal text/hash and reports are intentionally excluded.
    run = {}
    metadata = crew / "run.json"
    if metadata.exists():
        value = json.loads(metadata.read_text())
        run = {"run_id": identifier(value.get("run_id"))}
        if isinstance(value.get("base_commit"), str) and re.fullmatch(r"[a-f0-9]{40,64}", value["base_commit"]):
            run["base_commit"] = value["base_commit"]
        workdir = value.get("workspace")
        if isinstance(workdir, str) and workdir.startswith("/") and len(workdir) <= 4096 and not re.search(r"[\x00-\x1f\x7f]", workdir):
            run["workspace"] = workdir
    sessions = unique_sessions(sessions)
    reported_qc = None
    if (crew / "qc.json").exists():
        value = json.loads((crew / "qc.json").read_text())
        reported_qc = {k: identifier(value.get(k)) for k in ("run_id", "commit", "qc", "implementer", "verdict")}
    result = None
    if (crew / "status.md").exists():
        status = (crew / "status.md").read_text().rsplit("==== goal delivered ====", 1)[-1]
        matches = re.findall(r"^RESULT: (DONE|FAILED|BLOCKED)\s*$", status, re.MULTILINE)
        result = matches[-1] if matches else None
    return {"schema_version": 1, "scope": "sandbox_lifetime", "run": run,
            "reported_qc": reported_qc,
            "reported_result": result,
            "registry_roles": registry_roles,
            "assignments": assignments, "sessions": sessions, "missing_roles": missing,
            "unattributed_session_count": unassigned, "issues": sorted(set(issues)),
            "coverage": "complete" if not missing and not issues and all(
                s["coverage"] == "complete" for s in sessions) else "partial"}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--crew-dir", type=Path, default=Path("/home/agent/crew"))
    args = parser.parse_args()
    try:
        report = collect(args.crew_dir, Path.home())
    except (OSError, ValueError, subprocess.TimeoutExpired):
        print("Usage collection failed; preserve the sandbox and retry task usage.", file=sys.stderr)
        return 1
    print(json.dumps(report, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
