#!/usr/bin/env python3
# ---------------------------------------------------------------------------
# Ported into the jcode repository from the Entire external-agent adapter.
# source: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
# source repo: entire-agent-jcode @ fb54426
#                ("fix(adapter): refuse an oversized transcript instead of letting the CLI truncate it")
# ---------------------------------------------------------------------------
# reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/agent/external/external.go
# reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/lifecycle.go
"""Tests for the adapter's refusal to exceed the CLI's stdout cap.

The Entire CLI caps an external agent's stdout at 10 MiB
(cmd/entire/cli/agent/external/external.go:530) and aborts with
"output exceeded 10485760 byte limit".

The adapter must NOT truncate. Truncating would be worse than the error:
the CLI writes the stored sidecar AFTER the read
(cmd/entire/cli/lifecycle.go:882), so a truncated answer overwrites a complete
stored copy with a partial one and moves the growth baseline into the
truncated coordinate.

So the adapter REFUSES an oversized transcript: non-zero exit, no transcript
bytes on stdout, and an explanation on stderr.

The negative control is the important half: a transcript UNDER the limit must
still be served in full, and the refusal must not fire for it.

Usage:  python test_stdout_cap_refusal.py
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ADAPTER = os.path.join(HERE, "entire-agent-jcode.py")

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


def make_session(directory: str, name: str, messages: int) -> str:
    """Write a Jcode session document with `messages` assistant messages."""
    payload = {
        "messages": [
            {"role": "user", "content": "hello"},
            *[
                {
                    "role": "assistant",
                    "content": "x" * 4096,
                }
                for _ in range(messages)
            ],
        ]
    }
    path = os.path.join(directory, f"{name}.json")
    with open(path, "w", encoding="utf-8") as handle:
        json.dump(payload, handle)
    return path


def _is_json_line(line: str) -> bool:
    try:
        json.loads(line)
    except Exception:
        return False
    return True


def run_read_transcript(session_path: str, home: str):
    """Run `read-transcript --session-ref <path>` with JCODE_HOME at `home`.

    reference: https://docs.entire.io/agents/agent-integration-protocol/commands
    """
    env = dict(os.environ)
    env["JCODE_HOME"] = home
    proc = subprocess.run(
        [sys.executable, ADAPTER, "read-transcript", "--session-ref", session_path],
        capture_output=True,
        text=True,
        timeout=300,
        env=env,
    )
    return proc


with tempfile.TemporaryDirectory() as tmp:
    home = os.path.join(tmp, "jcode-home")
    sessions = os.path.join(home, "sessions")
    os.makedirs(sessions, exist_ok=True)

    print("=== an oversized transcript is REFUSED, not truncated ===")
    # ~11 MiB of assistant content, above the 10 MiB cap.
    big = make_session(sessions, "session_big", 2700)
    proc = run_read_transcript(big, home)
    stderr = proc.stderr or ""
    check(
        "the exit code is non-zero",
        proc.returncode != 0,
        f"rc={proc.returncode}",
    )
    check(
        "NO transcript bytes reach stdout",
        len(proc.stdout) == 0,
        f"stdout={len(proc.stdout)} bytes",
    )
    check(
        "stderr explains the refusal and names the limit",
        "refusing rather than truncating" in stderr and "10485760" in stderr,
        stderr[:200],
    )
    check(
        "stderr names the actual size",
        "bytes, above" in stderr,
        stderr[:200],
    )

    print()
    print("=== a transcript UNDER the limit is served in full (negative control) ===")
    small = make_session(sessions, "session_small", 10)
    proc = run_read_transcript(small, home)
    small_stderr = proc.stderr or ""
    check("the exit code is zero", proc.returncode == 0, f"rc={proc.returncode}")
    check("stdout carries the transcript", len(proc.stdout) > 0, f"{len(proc.stdout)} bytes")
    check(
        "stdout is well under the cap",
        len(proc.stdout) < 10 * 1024 * 1024,
        f"stdout={len(proc.stdout)}",
    )
    check("stderr is empty", small_stderr.strip() == "", small_stderr[:120])

    print()
    print("=== the small transcript is COMPLETE, not a prefix ===")
    lines = [line for line in proc.stdout.splitlines() if line.strip()]
    check("it has ETF lines", len(lines) >= 2, f"{len(lines)} lines")
    parsed = []
    for line in lines:
        try:
            parsed.append(json.loads(line))
        except Exception:
            pass
    check("every line is valid JSON", len(parsed) == len(lines), f"{len(parsed)}/{len(lines)}")
    versions = {entry.get("v") for entry in parsed}
    check(
        "every line declares ETF v1",
        versions == {1},
        f"versions={versions}",
    )

    print()
    print("=== the refusal applies to the FINAL transcript size, not the raw one ===")
    # ONE huge message is trimmed by the adapter's per-line budget
    # (_MAX_LINE_BYTES), so the resulting transcript is small and is served
    # normally. The refusal is about the size of what would go on stdout, not
    # about the size of the session file.
    huge = os.path.join(sessions, "session_huge.json")
    with open(huge, "w", encoding="utf-8") as handle:
        json.dump(
            {"messages": [{"role": "assistant", "content": "y" * (11 * 1024 * 1024)}]},
            handle,
        )
    proc = run_read_transcript(huge, home)
    check(
        "a session with one huge message is served (its line was trimmed)",
        proc.returncode == 0,
        f"rc={proc.returncode} stderr={(proc.stderr or '')[:120]}",
    )
    check(
        "and the trimmed transcript is under the cap",
        len(proc.stdout) < 10 * 1024 * 1024,
        f"stdout={len(proc.stdout)}",
    )
    check(
        "and its lines are still valid JSON",
        all(
            _is_json_line(line)
            for line in proc.stdout.splitlines()
            if line.strip()
        ),
        "a line failed to parse",
    )

print()
print(f"checks run: {CHECKS_RUN}, failures: {len(FAILURES)}")
if FAILURES:
    for name in FAILURES:
        print(f"  FAILED: {name}")
    sys.exit(1)
print("all checks passed")
