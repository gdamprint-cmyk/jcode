#!/usr/bin/env python3
# ---------------------------------------------------------------------------
# Ported into the jcode repository from the Entire external-agent adapter.
# source: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
# source repo: entire-agent-jcode @ f4573ad
#                ("fix(adapter): refuse to substitute data for a transcript")
# ---------------------------------------------------------------------------
"""Negative control #2: the adapter version that EMPTIES a tiny payload.

reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go

The main negative control stops at the first missing attribute, so it never
reaches the new "tiny payload next to a huge text" check. This control runs
that one case against 05cddf4 - the version that introduced the emptying -
which must empty the payload. That is the defect the fix closes.

The defective adapter is vendored next to this file as
``tiny_payload_snapshot.py`` so the control runs without the external adapter
repository's git history.

Run: python neg_control_tiny_payload.py
"""

import importlib.util
import os
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
SNAPSHOT = os.path.join(HERE, "tiny_payload_snapshot.py")
DEFECTIVE_REF = "05cddf4"

if not os.path.exists(SNAPSHOT):
    sys.stderr.write(f"neg_control_tiny_payload: missing vendored snapshot {SNAPSHOT}\n")
    sys.exit(2)

with tempfile.TemporaryDirectory() as tmp:
    old_path = os.path.join(tmp, "entire-agent-jcode.py")
    with open(old_path, "w", encoding="utf-8", newline="\n") as fh:
        with open(SNAPSHOT, encoding="utf-8", newline="") as src:
            fh.write(src.read())
    spec = importlib.util.spec_from_file_location("old_adapter", old_path)
    old = importlib.util.module_from_spec(spec)
    sys.argv = ["old_adapter"]
    spec.loader.exec_module(old)

    payload = "eHh4"  # base64 of 3 bytes: valid, length 4
    line = {"v": 1, "type": "assistant", "agent": "jcode",
            "content": [{"type": "text", "text": "T" * 900_000},
                        {"type": "tool_result", "data": payload}]}
    encoded = len(old._encode_etf_line(line))
    out = old._shrink_etf_line(line, encoded)
    got = out["content"][1].get("data")
    print(f"adapter {DEFECTIVE_REF}: payload in = {len(payload)} chars")
    print(f"adapter {DEFECTIVE_REF}: payload out = {len(got) if got is not None else 'MISSING'} chars")
    print(f"adapter {DEFECTIVE_REF}: payload EMPTIED = {got == ''}")
    print(f"adapter {DEFECTIVE_REF}: line size = {len(old._encode_etf_line(out))} "
          f"(max {old._MAX_LINE_BYTES})")

    if got != "":
        print("NEGATIVE CONTROL INVALID: the defective adapter did not empty the payload")
        sys.exit(2)
    print("NEGATIVE CONTROL OK: the defective adapter empties the payload, "
          "the new one keeps its first quad")
