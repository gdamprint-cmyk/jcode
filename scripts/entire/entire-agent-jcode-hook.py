#!/usr/bin/env python3
# ---------------------------------------------------------------------------
# Ported into the jcode repository from the Entire external-agent adapter.
# source: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
# source repo: entire-agent-jcode @ addf2a9
#                ("fix(adapter): refuse to substitute data for a transcript")
# ---------------------------------------------------------------------------
# reference: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
# reference: https://docs.entire.io/agents/agent-integration-protocol/lifecycle
"""entire-agent-jcode-hook — Jcode lifecycle hook -> Entire bridge.

Jcode fires its `[hooks]` observers with the event described in environment
variables (``JCODE_HOOK_EVENT``, ``JCODE_HOOK_SESSION_ID``,
``JCODE_HOOK_PAYLOAD``, ...). This script forwards each event to the Entire
CLI as ``entire hooks jcode <hook-name>``, handing the Jcode payload to
Entire's ``parse-hook`` stage.

Usage:  python entire-agent-jcode-hook.py <hook-name>
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from datetime import datetime

AGENT_NAME = "jcode"

# Windows: CREATE_NO_WINDOW (0x08000000).
#
# Jcode spawns THIS script with CREATE_NO_WINDOW (crates/jcode-base/src/hooks.rs:457),
# so this process owns NO console. Spawning a CONSOLE-subsystem executable from a
# process without a console makes Windows ALLOCATE A FRESH CONSOLE WINDOW - one
# visible terminal per hook event. The `entire` shim is a console program
# (C:\Users\WorkShop1\scoop\shims\entire.exe, PE subsystem 3), so every hook fired a
# new window. Measured on 2026-11-07: 178 session-start events in 10 minutes.
#
# Passing the same flag to the child keeps it invisible without changing behaviour.
CREATE_NO_WINDOW = 0x08000000
_SUBPROCESS_FLAGS = {"creationflags": CREATE_NO_WINDOW} if os.name == "nt" else {}

# The Entire CLI resolves an external agent PER REPOSITORY: `entire agent list`
# says "Manage agent integrations in this repository" and fails with "not a
# git repository" outside one. Measured on this host:
#   cwd = C:\Users\WorkShop1\proj\jcode-src -> "jcode" is listed and found
#   cwd = C:\Users\WorkShop1                  -> "unknown agent \"jcode\"
# (not found as built-in or external plugin)"
#
# Jcode fires these hooks with the SESSION's cwd, and swarm workers usually run
# in C:\Users\WorkShop1 or in a worktree that is not itself Entire-enabled. So
# the `entire` subprocess is started in a known Entire-enabled repository
# instead of inheriting the session cwd. The payload still carries the real
# session cwd, so Entire records where the work happened.
#
# ENTIRE_JCODE_REPO_ROOT overrides the default.
DEFAULT_REPO_ROOT = r"C:\Users\WorkShop1\proj\jcode-src"

# The hook verb is passed as argv[1] by the adapter's install-hooks command
# (session-start / turn-start / turn-end / session-end).


def _payload() -> dict:
    """Build the HookInput JSON Entire expects from Jcode's env contract."""
    raw = os.environ.get("JCODE_HOOK_PAYLOAD", "")
    payload = {}
    if raw:
        try:
            parsed = json.loads(raw)
            if isinstance(parsed, dict):
                payload = parsed
        except json.JSONDecodeError:
            payload = {}

    event = (os.environ.get("JCODE_HOOK_EVENT", "") or payload.get("event", "")).strip()
    session_id = (
        os.environ.get("JCODE_HOOK_SESSION_ID", "") or payload.get("session_id", "")
    ).strip()
    cwd = (os.environ.get("JCODE_HOOK_CWD", "") or payload.get("cwd", "")).strip()

    hook_input = {
        "hook_type": event,
        "session_id": session_id,
        "session_ref": payload.get("session_ref", ""),
        "timestamp": payload.get("timestamp", ""),
    }
    prompt = payload.get("last_assistant_text") or payload.get("user_prompt")
    if prompt:
        hook_input["user_prompt"] = prompt
    model = os.environ.get("JCODE_HOOK_MODEL") or payload.get("model")
    if model:
        hook_input["model"] = model
    if cwd:
        hook_input["raw_data"] = {"cwd": cwd, "jcode_event": event}
    return hook_input


def main() -> int:
    if len(sys.argv) < 2:
        sys.stderr.write("entire-agent-jcode-hook: missing hook name\n")
        return 2
    hook_name = sys.argv[1]

    entire = os.environ.get("ENTIRE_BIN", "entire")
    hook_input = json.dumps(_payload())

    # Start `entire` in an Entire-enabled repository so the external agent is
    # resolvable regardless of the session's cwd. See DEFAULT_REPO_ROOT.
    repo_root = os.environ.get("ENTIRE_JCODE_REPO_ROOT", DEFAULT_REPO_ROOT)
    run_cwd = repo_root if os.path.isdir(repo_root) else None

    try:
        completed = subprocess.run(
            [entire, "hooks", AGENT_NAME, hook_name],
            input=hook_input,
            text=True,
            capture_output=True,
            timeout=30,
            cwd=run_cwd,
            **_SUBPROCESS_FLAGS,
        )
    except FileNotFoundError:
        sys.stderr.write("[entire-agent-jcode-hook] entire CLI not on PATH; skipping\n")
        return 0
    except subprocess.TimeoutExpired:
        sys.stderr.write("[entire-agent-jcode-hook] entire CLI timed out; skipping\n")
        return 0

    if completed.returncode != 0:
        # A failing Entire hook must never break the agent turn.
        sys.stderr.write(
            f"[entire-agent-jcode-hook] entire hooks {AGENT_NAME} {hook_name} failed "
            f"({completed.returncode}): {completed.stderr.strip()[:500]}\n"
        )
    _debug_log(hook_name, completed.returncode, completed.stdout, completed.stderr, hook_input)
    return 0


def _debug_log(hook_name, rc, stdout, stderr, hook_input) -> None:
    """Append a debug line per hook invocation.

    Jcode dispatches observer hooks with stdout/stderr set to null, so a
    failure inside `entire` is otherwise invisible. Keep the last 200 lines.
    """
    try:
        path = os.path.join(
            os.environ.get("ENTIRE_JCODE_DEBUG_LOG")
            or os.path.join(os.path.expanduser("~"), ".entire", "jcode-hook-debug.log")
        )
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(
                json.dumps(
                    {
                        "ts": datetime.now().isoformat(timespec="seconds"),
                        "hook": hook_name,
                        "rc": rc,
                        "stdout": (stdout or "")[:1000],
                        "stderr": (stderr or "")[:1000],
                        "input": hook_input[:600],
                        "cwd": os.getcwd(),
                    },
                    ensure_ascii=False,
                )
                + "\n"
            )
        lines = open(path, encoding="utf-8").read().splitlines()[-200:]
        with open(path, "w", encoding="utf-8") as handle:
            handle.write("\n".join(lines) + "\n")
    except Exception:  # debug logging must never break a turn
        pass


if __name__ == "__main__":
    sys.exit(main())
