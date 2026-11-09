#!/usr/bin/env python3
# ---------------------------------------------------------------------------
# Ported into the jcode repository from the Entire external-agent adapter.
# source: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
# source repo: entire-agent-jcode @ 091a3ac
#                ("fix(adapter): correct the stdout-cap refusal rationale with the measured mechanism")
# ---------------------------------------------------------------------------
# reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/agent/external/external.go
# reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/lifecycle.go
"""Tests for the adapter's refusal to exceed the CLI's stdout cap.

The Entire CLI caps an external agent's stdout at 10 MiB
(cmd/entire/cli/agent/external/external.go:530) and aborts with
"output exceeded 10485760 byte limit".

The adapter must NOT truncate. The measured reason is NOT that a truncated
answer would corrupt the stored sidecar: the CLI's limitedWriter flags the
overflow and external.go:550-552 returns an error INSTEAD of the bytes, so
cmd/entire/cli/lifecycle.go:870-875 fails BEFORE the sidecar write at
lifecycle.go:882, and no truncated sidecar can exist. (0 of 417 stored
sidecars on this host sit at or within 2% of the cap.) What the refusal buys
is a SPECIFIC error naming the transcript's actual size, a clear refusal
instead of a partial stdout, and the stored sidecar left at its previous
complete contents.

So, when the CLI is the parent, the adapter REFUSES an oversized transcript:
non-zero exit, no transcript bytes on stdout, and an explanation on stderr.

The cap is a property of the CLI's stdout PIPE, not of the transcript. The
CLI marks every plugin subprocess with ENTIRE_CLI_VERSION
(cmd/entire/cli/agent/external/external.go:511), so the adapter refuses only
under that marker. A direct caller has no cap and gets the whole transcript.

Both directions are checked here, each with its own negative control:
  - under the CLI marker: oversized REFUSED, undersized served in full;
  - without it: oversized served in full.

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

CLI_STDOUT_LIMIT_BYTES = 10 * 1024 * 1024

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


def run_read_transcript(
    session_path: str,
    home: str,
    as_cli: bool,
    extra_args: list[str] | None = None,
):
    """Run `read-transcript --session-ref <path>` with JCODE_HOME at `home`.

    `as_cli` sets ENTIRE_CLI_VERSION, which is how the CLI marks a plugin
    subprocess (cmd/entire/cli/agent/external/external.go:511).

    reference: https://docs.entire.io/agents/agent-integration-protocol/commands
    """
    env = dict(os.environ)
    env["JCODE_HOME"] = home
    env.pop("ENTIRE_CLI_VERSION", None)
    if as_cli:
        env["ENTIRE_CLI_VERSION"] = "dev"
    args = [sys.executable, ADAPTER, "read-transcript", "--session-ref", session_path]
    if extra_args:
        args.extend(extra_args)
    proc = subprocess.run(
        args,
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

    print("=== under the CLI marker: an oversized transcript is REFUSED ===")
    # ~11 MiB of assistant content, above the 10 MiB cap.
    big = make_session(sessions, "session_big", 2700)
    proc = run_read_transcript(big, home, as_cli=True)
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
    print("=== under the CLI marker: a transcript UNDER the limit is served (negative control) ===")
    small = make_session(sessions, "session_small", 10)
    proc = run_read_transcript(small, home, as_cli=True)
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
    print("=== WITHOUT the CLI marker: the oversized transcript is served in full ===")
    # A direct caller has no 10 MiB pipe, so refusing would deny a
    # serviceable full transcript. This is the ETF schema checker's path.
    proc = run_read_transcript(big, home, as_cli=False)
    check(
        "the exit code is zero",
        proc.returncode == 0,
        f"rc={proc.returncode} stderr={(proc.stderr or '')[:160]}",
    )
    check(
        "the FULL transcript is on stdout",
        len(proc.stdout) > CLI_STDOUT_LIMIT_BYTES,
        f"stdout={len(proc.stdout)}",
    )
    check(
        "every line is valid JSON",
        all(_is_json_line(line) for line in proc.stdout.splitlines() if line.strip()),
        "a line failed to parse",
    )

    print()
    print("=== --allow-oversized under the CLI marker: served in full ===")
    proc = run_read_transcript(big, home, as_cli=True, extra_args=["--allow-oversized"])
    check(
        "the exit code is zero",
        proc.returncode == 0,
        f"rc={proc.returncode} stderr={(proc.stderr or '')[:160]}",
    )
    check(
        "the FULL transcript is on stdout",
        len(proc.stdout) > CLI_STDOUT_LIMIT_BYTES,
        f"stdout={len(proc.stdout)}",
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
    proc = run_read_transcript(huge, home, as_cli=True)
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
