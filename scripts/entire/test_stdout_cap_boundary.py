#!/usr/bin/env python3
# ---------------------------------------------------------------------------
# Ported into the jcode repository from the Entire external-agent adapter.
# source: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
# source repo: entire-agent-jcode @ 88fd2c7
#                ("fix(adapter): the stdout cap is 10 MiB for read-transcript, 67.7 MiB for compact-transcript")
# ---------------------------------------------------------------------------
# reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/agent/external/external.go
# reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/lifecycle.go
"""Boundary check for the adapter's stdout-cap refusal.

The adapter refuses when len(data) > 10485760, but it writes a TRAILING NEWLINE
when the transcript does not end with one (entire-agent-jcode.py:1078-1079). So
a transcript of exactly 10485760 bytes produces 10485761 bytes on stdout, which
is OVER the CLI's cap: the CLI's limitedWriter truncates it and
external.go:550-552 then reports "output exceeded 10485760 byte limit".

The guard must therefore measure what goes on stdout, not the transcript alone.
This script measures the actual stdout byte count at the boundary.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ADAPTER = os.path.join(HERE, "entire-agent-jcode.py")
LIMIT = 10 * 1024 * 1024

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


def run(path: str, home: str, as_cli: bool):
    """Run `read-transcript`, whose CLI stdout cap is 10 MiB.

    reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/agent/external/external.go
    """
    env = dict(os.environ)
    env["JCODE_HOME"] = home
    env.pop("ENTIRE_CLI_VERSION", None)
    if as_cli:
        env["ENTIRE_CLI_VERSION"] = "dev"
    return subprocess.run(
        [sys.executable, ADAPTER, "read-transcript", "--session-ref", path],
        capture_output=True,
        timeout=300,
        env=env,
    )


def run_compact(path: str, home: str, as_cli: bool):
    """Run `compact-transcript`, whose CLI stdout cap is much higher.

    reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/agent/external/external.go
    """
    env = dict(os.environ)
    env["JCODE_HOME"] = home
    env.pop("ENTIRE_CLI_VERSION", None)
    if as_cli:
        env["ENTIRE_CLI_VERSION"] = "dev"
    return subprocess.run(
        [sys.executable, ADAPTER, "compact-transcript", "--session-ref", path],
        capture_output=True,
        timeout=300,
        env=env,
    )


def build_session(path: str, home: str, target_bytes: int) -> int:
    """Write a session whose ETF transcript is close to `target_bytes`.

    Returns the size of the produced ETF transcript, measured by running the
    adapter once as a DIRECT caller (no cap).
    """
    count = max(1, target_bytes // 4300)
    size = 0
    for _ in range(12):
        payload = {
            "messages": [
                {"role": "user", "content": "hello"},
                *[
                    {"role": "assistant", "content": "x" * 4096}
                    for _ in range(count)
                ],
            ]
        }
        with open(path, "w", encoding="utf-8") as fh:
            json.dump(payload, fh)
        proc = run(path, home, as_cli=False)
        size = len(proc.stdout)
        if size == target_bytes:
            break
        per = max(1, size // max(1, count))
        if size < target_bytes:
            count += max(1, (target_bytes - size) // per)
        else:
            count = max(1, count - max(1, (size - target_bytes) // per))
    return size


with tempfile.TemporaryDirectory() as tmp:
    home = os.path.join(tmp, "jcode-home")
    sessions = os.path.join(home, "sessions")
    os.makedirs(sessions, exist_ok=True)

    print("=== build a transcript of EXACTLY the cap ===")
    path = os.path.join(sessions, "session_boundary.json")
    size = build_session(path, home, LIMIT)
    print(f"  produced ETF transcript: {size} bytes (cap {LIMIT}, delta {size - LIMIT})")

    proc = run(path, home, as_cli=True)
    stdout_bytes = len(proc.stdout)
    print(f"  under the CLI marker: rc={proc.returncode}, stdout={stdout_bytes} bytes")
    print(f"  stderr: {(proc.stderr or '').strip()[:200]}")

    print()
    print("=== the guard must keep the CLI's stdout UNDER the cap ===")
    # The CLI's limitedWriter keeps the first `limit` bytes and flags
    # truncation, so external.go:551 reports the error. The adapter must not
    # let stdout reach the cap: it must refuse while there is still room.
    check(
        "stdout stays strictly under the cap (the CLI errors at the cap)",
        stdout_bytes < LIMIT,
        f"stdout={stdout_bytes} cap={LIMIT}",
    )

    print()
    print("=== a transcript comfortably under the cap is still served ===")
    small = os.path.join(sessions, "session_small.json")
    with open(small, "w", encoding="utf-8") as fh:
        json.dump({"messages": [{"role": "user", "content": "hi"}]}, fh)
    proc = run(small, home, as_cli=True)
    check("rc == 0", proc.returncode == 0, f"rc={proc.returncode}")
    check("stdout is non-empty", len(proc.stdout) > 0, f"{len(proc.stdout)} bytes")

    print()
    print("=== compact-transcript has its OWN, much higher cap ===")
    # reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/agent/external/external.go
    # external.go:531-534: compact-transcript is the ONE subcommand that does
    # NOT get the 10 MiB cap. It gets
    # base64.StdEncoding.EncodedLen(agent.MaxChunkSize) + 1 MiB = 70 953 644.
    # So the refusal guard must apply to read-transcript ONLY: refusing
    # compact-transcript would deny a transcript the CLI would have accepted.
    COMPACT_CAP = 70953644
    proc = run_compact(path, home, as_cli=True)
    compact_out = len(proc.stdout)
    print(f"  compact-transcript stdout: {compact_out} bytes (cap {COMPACT_CAP})")
    print(f"  stderr: {(proc.stderr or b'').strip()[:200]}")
    check(
        "compact-transcript is NOT refused under the CLI marker",
        proc.returncode == 0,
        f"rc={proc.returncode} stderr={(proc.stderr or b'')[:160]}",
    )
    check(
        "compact-transcript output is non-empty",
        compact_out > 0,
        f"{compact_out} bytes",
    )
    check(
        "compact-transcript output fits ITS cap (not the 10 MiB one)",
        compact_out <= COMPACT_CAP,
        f"stdout={compact_out} cap={COMPACT_CAP}",
    )

    print()
    print("=== the same OVER-CAP transcript: read-transcript refuses, compact-transcript does not ===")
    # The boundary session above is just UNDER the 10 MiB cap, so read-transcript
    # correctly serves it. For the two caps to differ, the session has to be
    # OVER 10 MiB. compact-transcript's own output for such a session stays
    # under its 67.7 MiB cap, so the CLI would have accepted it.
    over = os.path.join(sessions, "session_over_cap.json")
    over_size = build_session(over, home, LIMIT + 400000)
    print(f"  over-cap ETF transcript: {over_size} bytes (cap {LIMIT})")
    proc = run(over, home, as_cli=True)
    check(
        "read-transcript REFUSES a transcript over the 10 MiB cap",
        proc.returncode != 0,
        f"rc={proc.returncode} stdout={len(proc.stdout)}",
    )
    check(
        "and emits no transcript bytes",
        len(proc.stdout) == 0,
        f"stdout={len(proc.stdout)}",
    )
    proc = run_compact(over, home, as_cli=True)
    check(
        "compact-transcript SERVES the same over-cap transcript",
        proc.returncode == 0,
        f"rc={proc.returncode} stderr={(proc.stderr or b'')[:160]}",
    )
    check(
        "and its output still fits the 67.7 MiB cap",
        0 < len(proc.stdout) <= COMPACT_CAP,
        f"stdout={len(proc.stdout)} cap={COMPACT_CAP}",
    )

print()
print(f"checks run: {CHECKS_RUN}, failures: {len(FAILURES)}")
if FAILURES:
    for name in FAILURES:
        print(f"  FAILED: {name}")
    sys.exit(1)
print("all checks passed")
