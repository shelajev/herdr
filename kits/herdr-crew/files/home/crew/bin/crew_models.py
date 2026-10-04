"""Start with explicit pins and verify the response to a fresh native prompt.

Only model identifiers and error classes leave session artifacts. No transcript,
provider error body, credential, or fallback model is printed.
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import time
import uuid


TOKEN = re.compile(r"[A-Za-z0-9._:/-]+\Z")


def model_value(value):
    if not TOKEN.fullmatch(value):
        raise ValueError("invalid_model_value")
    return value


def parse_models(text):
    pins = {}
    for pair in text.rstrip("\r\n").split(","):
        kind, sep, value = pair.partition("=")
        if not sep or kind not in ("claude", "codex", "pi") or kind in pins:
            raise ValueError("invalid_models_file")
        pins[kind] = model_value(value)
    if set(pins) != {"claude", "codex", "pi"}:
        raise ValueError("missing_model_kind")
    provider, sep, model = pins["pi"].partition("/")
    if not sep:
        raise ValueError("missing_pi_provider")
    model_value(provider)
    model_value(model)
    return pins


def start_args(kind, pins):
    if kind == "claude":
        return ["--model", pins[kind]]
    if kind == "codex":
        return ["-m", pins[kind], "-c", "check_for_update_on_startup=false"]
    if kind == "pi":
        provider, model = pins[kind].split("/", 1)
        return ["--provider", provider, "--model", model]
    raise ValueError("unsupported_agent_kind")


def safe_id(value):
    """Never reflect arbitrary session/error text as an identifier."""
    if not isinstance(value, str) or len(value) > 160 or not TOKEN.fullmatch(value):
        return "not_reported"
    if value.startswith(("sk-", "oai-", "AIza", "ghp_", "gho_")):
        return "redacted"
    return value


def error_class(value):
    # The provider's prose may contain a credential. Classify it without echoing it.
    text = str(value).lower()
    for name in ("model_not_found", "unsupported_model", "invalid_model",
                 "authentication_error", "rate_limit_error", "permission_denied",
                 "invalid_request_error", "insufficient_quota",
                 "agent_prompt_stalled", "agent_blocked", "agent_not_running", "timeout"):
        if name in text:
            return name
    if "model" in text and any(word in text for word in
                               ("not found", "not supported", "unavailable", "unknown", "does not exist")):
        return "model_unavailable"
    return "provider_error"


def content_text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(block.get("text", "") for block in content
                       if isinstance(block, dict) and block.get("type") in ("text", "input_text", "output_text"))
    return ""


def session_observation(path, kind, nonce):
    """Bind an assistant turn to a nonce-bearing user message, never to recency."""
    matched = False
    context_model = None
    with path.open() as stream:
        for line in stream:
            try:
                entry = json.loads(line)
            except ValueError:
                continue  # A concurrently appended final line may be incomplete.
            if not isinstance(entry, dict):
                continue
            if kind == "codex":
                payload = entry.get("payload", {})
                if not isinstance(payload, dict):
                    continue
                if entry.get("type") == "turn_context":
                    context_model = payload.get("model")
                message = payload if entry.get("type") == "response_item" else {}
                if matched and entry.get("type") == "event_msg":
                    if payload.get("type") == "error":
                        return {"error": error_class(payload)}
                    # Codex 0.153.4 persists provider failures on task_complete,
                    # without an assistant response_item or a separate error event.
                    if payload.get("type") == "task_complete" and payload.get("error"):
                        return {"error": error_class(payload["error"])}
            else:
                message = entry.get("message", {})
            if not isinstance(message, dict):
                continue
            role = message.get("role")
            if role == "user":
                if matched:
                    return {"error": "no_assistant_turn"}
                matched = nonce in content_text(message.get("content"))
                continue
            if not matched or role != "assistant":
                continue
            if (entry.get("isApiErrorMessage") or message.get("stopReason") in ("error", "aborted")
                    or message.get("model") == "<synthetic>"):
                return {"error": error_class(message.get("errorMessage", content_text(message.get("content"))))}
            model = message.get("model") if kind != "codex" else context_model
            if not model:
                return {"error": "model_not_reported"}
            # pi's provider is part of the pin, even when two providers share an id.
            if kind == "pi":
                provider = message.get("provider")
                if not provider:
                    return {"error": "model_not_reported"}
                model = provider + "/" + model
            return {"model": model}
    return {"error": "no_assistant_turn"} if matched else None


def observe(kind, nonce):
    home = Path.home()
    root = {"claude": home / ".claude/projects",
            "codex": Path(os.environ.get("CODEX_HOME", str(home / ".codex"))) / "sessions",
            "pi": home / ".pi/agent/sessions"}[kind]
    matches = []
    for path in root.rglob("*.jsonl"):
        try:
            result = session_observation(path, kind, nonce)
        except (OSError, UnicodeError):
            continue
        if result is not None:
            matches.append(dict(result, session=path.name))
    if len(matches) != 1:
        return {"error": "no_artifact" if not matches else "ambiguous_artifact"}
    return matches[0]


def herdr(*args, timeout=195):
    return subprocess.run([os.environ.get("HERDR_BIN", "herdr"), *args],
                          capture_output=True, text=True, timeout=timeout)


def agent_info(role):
    response = herdr("agent", "get", role)
    try:
        agent = json.loads(response.stdout)["result"]["agent"]
        if response.returncode or not isinstance(agent, dict):
            return None
        return agent
    except (ValueError, KeyError, TypeError):
        return None


def native_error(response):
    # Only a structured Herdr error permits delivery recovery. A provider's
    # prose mentioning a stalled prompt must never trigger another request.
    for stream in (response.stderr, response.stdout):
        try:
            code = json.loads(stream)["error"]["code"]
        except (ValueError, KeyError, TypeError):
            continue
        return safe_id(code)
    return "native_command_failed"


def flushed_observation(kind, nonce):
    result = observe(kind, nonce)
    deadline = time.monotonic() + 2
    while result.get("error") in ("no_artifact", "no_assistant_turn") and time.monotonic() < deadline:
        time.sleep(0.1)
        result = observe(kind, nonce)
    return result


def probe(role, pins):
    agent = agent_info(role)
    if agent is None or agent.get("agent") not in pins:
        print("native_delivery_error=agent_not_running")
        return 34
    kind = agent["agent"]
    if agent.get("agent_status") not in ("idle", "done"):
        print("native_delivery_error=agent_not_ready")
        return 34
    terminal = agent.get("terminal_id")
    nonce = "CREW-MODEL-PROBE-" + uuid.uuid4().hex
    expected = pins[kind]
    provider = expected.split("/", 1)[0] if kind == "pi" else kind
    for attempt in range(1, 4):  # At most two retries of an unobserved delivery.
        response = herdr("agent", "prompt", role, "Reply with exactly: " + nonce,
                         "--wait", "--timeout", "180000")
        result = flushed_observation(kind, nonce)
        if "model" in result and result["model"] != expected:
            print(f"model_mismatch expected={safe_id(expected)} observed={safe_id(result['model'])}")
            return 30
        error = result.get("error")
        if error and error not in ("no_artifact", "no_assistant_turn", "model_not_reported", "ambiguous_artifact"):
            print(f"provider={safe_id(provider)} error_class={error}")
            return 31
        if response.returncode:
            code = native_error(response)
            reason = "native_command_failed"
            if code == "agent_prompt_stalled" and error == "no_artifact":
                current = agent_info(role)
                if not terminal or current is None or current.get("terminal_id") != terminal:
                    reason = "terminal_changed_or_unavailable"
                elif current.get("agent") != kind or current.get("agent_status") != "idle":
                    reason = "target_not_idle"
                elif observe(kind, nonce).get("error") != "no_artifact":
                    reason = "nonce_observed"
                elif attempt == 3:
                    reason = "retries_exhausted"
                else:
                    print(f"native_delivery_retry error_class={code} attempt={attempt} nonce_observed=false", flush=True)
                    continue
            elif code == "agent_prompt_stalled":
                reason = "nonce_observed"
            print(f"native_delivery_error={code} attempts={attempt} reason={reason}")
            return 34
        if error:
            print(f"provider={safe_id(provider)} error_class={error}")
            return 32
        print(f"kind={kind} requested={safe_id(expected)} observed={safe_id(result['model'])} session={safe_id(result['session'])} attempts={attempt}")
        return 0
    raise AssertionError("bounded probe loop must return")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--crew-dir", type=Path, default=Path("/home/agent/crew"))
    commands = parser.add_subparsers(dest="command", required=True)
    start = commands.add_parser("start")
    start.add_argument("--role", required=True)
    start.add_argument("--kind", choices=("claude", "codex", "pi"), required=True)
    start.add_argument("--pane", required=True)
    check = commands.add_parser("probe")
    check.add_argument("--role", required=True)
    args = parser.parse_args()
    try:
        pins = parse_models((args.crew_dir / "models").read_text())
    except FileNotFoundError:
        print("models file missing: sandbox predates model pins; recreate the task")
        return 33
    except (OSError, ValueError, UnicodeError):
        print("error_class=invalid_models_file")
        return 31
    try:
        if args.command == "probe":
            return probe(args.role, pins)
        result = herdr("agent", "start", args.role, "--kind", args.kind, "--pane", args.pane,
                       "--timeout", "240000", "--", *start_args(args.kind, pins), timeout=255)
        if result.returncode:
            print(f"herdr agent start failed: error_class={error_class(result.stdout + result.stderr)}")
            return result.returncode if result.returncode > 0 else 31
        print(f"started kind={args.kind} requested={safe_id(pins[args.kind])}")
        return 0
    except subprocess.TimeoutExpired:
        print("native_delivery_error=timeout")
        return 34
    except OSError:
        print("native_delivery_error=herdr_unavailable")
        return 34


if __name__ == "__main__":
    raise SystemExit(main())
