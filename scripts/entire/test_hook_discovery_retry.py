#!/usr/bin/env python3
# ---------------------------------------------------------------------------
# Ported into the jcode repository from the Entire external-agent adapter.
# source: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
# source repo: entire-agent-jcode @ 26a19cc
#                ("fix(adapter): correct the stdout-cap refusal rationale with the measured mechanism")
# ---------------------------------------------------------------------------
# reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/hooks_cmd.go
# reference: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
"""Tests for the hook's bounded retry on a dropped external-plugin discovery.

The Entire CLI discovers external plugins under a 5 second budget
(cmd/entire/cli/hooks_cmd.go:47) and runs each plugin's `info` subprocess inside
it. When the budget expires mid-scan the plugin is silently dropped and the CLI
reports:

    unknown agent "jcode" (not found as built-in or external plugin)

The hook must retry that specific failure, because it is a discovery timeout
and not a hook error. It must NOT retry any other failure, and it must not
retry endlessly.

The CLI is replaced by a stub on PATH so the tests are deterministic and do
not need the real Entire install.

Usage:  python test_hook_discovery_retry.py
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
HOOK = os.path.join(HERE, "entire-agent-jcode-hook.py")

CHECKS_RUN = 0
FAILURES: list[str] = []


def check(label: str, condition: bool, detail: str = "") -> None:
    global CHECKS_RUN
    CHECKS_RUN += 1
    if condition:
        print(f"  ok   {label}")
    else:
        FAILURES.append(label)
        print(f"  FAIL {label} {detail}")


def write_stub(directory: str, name: str, behaviour: str) -> str:
    """Write a fake `entire` that answers according to `behaviour`.

    Behaviours:
      drop_then_ok   - first call prints the unknown-agent error, later calls succeed
      always_drop    - every call prints the unknown-agent error
      other_error    - every call fails with a DIFFERENT error
      ok             - every call succeeds
    """
    path = os.path.join(directory, "entire.py")
    counter_file = os.path.join(directory, "calls.txt")
    stub = (
        "import sys, os\n"
        f"counter = r'{counter_file}'\n"
        f"behaviour = '{behaviour}'\n"
        "calls = 0\n"
        "if os.path.exists(counter):\n"
        "    calls = len([l for l in open(counter, encoding='utf-8') if l.strip()])\n"
        "with open(counter, 'a', encoding='utf-8') as h:\n"
        "    h.write('x\\n')\n"
        "calls += 1\n"
        "DROP = 'unknown agent \"jcode\" (not found as built-in or external plugin)'\n"
        "if behaviour == 'drop_then_ok' and calls == 1:\n"
        "    sys.stderr.write(DROP + '\\n')\n"
        "    sys.exit(1)\n"
        "if behaviour == 'always_drop':\n"
        "    sys.stderr.write(DROP + '\\n')\n"
        "    sys.exit(1)\n"
        "if behaviour == 'other_error':\n"
        "    sys.stderr.write('failed to read transcript: something else broke\\n')\n"
        "    sys.exit(1)\n"
        "sys.stdout.write('{\"ok\":true}\\n')\n"
        "sys.exit(0)\n"
    )
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(stub)
    launcher = os.path.join(directory, "entire.cmd")
    with open(launcher, "w", newline="\r\n") as handle:
        handle.write(f'@echo off\r\n"{sys.executable}" "{path}" %*\r\n')
    return launcher


def run_hook(behaviour: str) -> tuple[int, str, int]:
    """Run the hook with a stub `entire` and return (rc, stderr, calls)."""
    with tempfile.TemporaryDirectory() as tmp:
        write_stub(tmp, "entire", behaviour)
        counter_file = os.path.join(tmp, "calls.txt")
        env = dict(os.environ)
        env["PATH"] = tmp + os.pathsep + env["PATH"]
        env["ENTIRE_BIN"] = os.path.join(tmp, "entire.cmd")
        env["ENTIRE_JCODE_REPO_ROOT"] = tmp
        env["ENTIRE_JCODE_DEBUG_LOG"] = os.path.join(tmp, "debug.log")
        payload = json.dumps(
            {"hook_type": "turn_start", "session_id": "test-session", "timestamp": "2026-11-09T00:00:00Z"}
        )
        proc = subprocess.run(
            [sys.executable, HOOK, "turn-start"],
            input=payload,
            text=True,
            capture_output=True,
            timeout=120,
            env=env,
        )
        calls = 0
        if os.path.exists(counter_file):
            with open(counter_file, encoding="utf-8") as handle:
                calls = len([line for line in handle if line.strip()])
        return proc.returncode, proc.stderr, calls


print("=== the retry fires on the dropped-plugin error ===")
rc, stderr, calls = run_hook("drop_then_ok")
check("hook exits 0 (a failing Entire hook must never break the turn)", rc == 0, f"rc={rc}")
check("the CLI was called more than once", calls > 1, f"calls={calls}")
check("the retry succeeded (no unknown-agent error left)", "unknown agent" not in stderr, stderr[:120])

print()
print("=== the retry is BOUNDED (not endless) ===")
rc, stderr, calls = run_hook("always_drop")
check("hook still exits 0", rc == 0, f"rc={rc}")
check("attempts are bounded at 3", calls == 3, f"calls={calls}")
check("the unknown-agent error is reported", "unknown agent" in stderr, stderr[:120])

print()
print("=== OTHER failures are NOT retried (negative control) ===")
rc, stderr, calls = run_hook("other_error")
check("hook exits 0", rc == 0, f"rc={rc}")
check("the CLI was called exactly once", calls == 1, f"calls={calls}")
check("the original error is reported", "something else broke" in stderr, stderr[:120])

print()
print("=== a working CLI is not retried (negative control) ===")
rc, stderr, calls = run_hook("ok")
check("hook exits 0", rc == 0, f"rc={rc}")
check("the CLI was called exactly once", calls == 1, f"calls={calls}")
check("no error is reported", stderr.strip() == "", stderr[:120])

print()
print(f"checks run: {CHECKS_RUN}, failures: {len(FAILURES)}")
if FAILURES:
    for name in FAILURES:
        print(f"  FAILED: {name}")
    sys.exit(1)
print("all checks passed")
