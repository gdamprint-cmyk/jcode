#!/usr/bin/env python3
# ---------------------------------------------------------------------------
# Ported into the jcode repository from the Entire external-agent adapter.
# source: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
# source repo: entire-agent-jcode @ f4573ad
#                ("fix(adapter): refuse to substitute data for a transcript")
# ---------------------------------------------------------------------------
"""Negative control: the NEW tests against the OLD adapter (fc457a7).

reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go

The new tests assert that a trimmed base64 payload stays DECODABLE. The old
adapter appended a text marker to the payload, which makes the field
undecodable. This control must FAIL on the old code and PASS on the new one.

The old adapter is vendored next to this file as ``old_adapter_snapshot.py`` so
the control runs without the external adapter repository's git history.

Run: python neg_control_old_adapter.py
"""

import importlib.util
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ADAPTER = os.path.join(HERE, "entire-agent-jcode.py")
SNAPSHOT = os.path.join(HERE, "old_adapter_snapshot.py")
TEST = os.path.join(HERE, "test_shrink_etf_line.py")

if not os.path.exists(SNAPSHOT):
    sys.stderr.write(f"neg_control_old_adapter: missing vendored snapshot {SNAPSHOT}\n")
    sys.exit(2)

with tempfile.TemporaryDirectory() as tmp:
    old_path = os.path.join(tmp, "entire-agent-jcode.py")
    with open(old_path, "w", encoding="utf-8", newline="\n") as fh:
        with open(SNAPSHOT, encoding="utf-8", newline="") as src:
            fh.write(src.read())
    # The test loads the adapter from its own directory, so the old adapter
    # and the new test have to sit side by side.
    with open(os.path.join(tmp, "test_shrink_etf_line.py"), "w",
              encoding="utf-8", newline="\n") as fh:
        with open(TEST, encoding="utf-8") as src:
            fh.write(src.read())
    proc = subprocess.run([sys.executable, "test_shrink_etf_line.py"],
                          cwd=tmp, capture_output=True, text=True)
    print(proc.stdout)
    print(proc.stderr)
    print(f"exit code = {proc.returncode}")
    fails = [l for l in proc.stdout.splitlines() if l.startswith("FAIL")]
    print(f"FAIL count = {len(fails)}")
    for f in fails:
        print("  " + f)
    # The control is meaningful only if the OLD code actually fails the new
    # checks.
    if proc.returncode == 0:
        print("NEGATIVE CONTROL INVALID: the old adapter passed the new tests")
        sys.exit(2)
    print("NEGATIVE CONTROL OK: the old adapter fails the new checks")
