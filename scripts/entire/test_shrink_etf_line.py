#!/usr/bin/env python3
# ---------------------------------------------------------------------------
# Ported into the jcode repository from the Entire external-agent adapter.
# source: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
# source repo: entire-agent-jcode @ addf2a9
#                ("fix(adapter): refuse to substitute data for a transcript")
# ---------------------------------------------------------------------------
"""Regression tests for the ETF line trimming in entire-agent-jcode.py.

reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go

The Brain's history scanner rejects a transcript line that is over its line
budget, so the adapter has to trim every oversized line. The trimming must
never lose a payload entirely: a cut payload is still readable, an empty one
is data loss.

Run: python test_shrink_etf_line.py
"""

import base64
import importlib.util
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ADAPTER = os.path.join(HERE, "entire-agent-jcode.py")

_spec = importlib.util.spec_from_file_location("adapter", ADAPTER)
adapter = importlib.util.module_from_spec(_spec)
sys.argv = ["adapter"]
_spec.loader.exec_module(adapter)

MAX_LINE = adapter._MAX_LINE_BYTES
MARKER = adapter._TRUNCATION_MARKER

FAILURES = []


def check(name, condition, detail=""):
    if condition:
        print(f"PASS {name}")
    else:
        print(f"FAIL {name} {detail}")
        FAILURES.append(name)


def encoded_len(line):
    return len(adapter._encode_etf_line(line))


def shrink(line):
    return adapter._shrink_etf_line(line, encoded_len(line))


def test_direct_base64_is_trimmed_not_dropped():
    payload = "A" * 1_100_000
    line = {"v": 1, "type": "assistant", "agent": "jcode",
            "content": [{"type": "tool_result", "data": payload}]}
    out = shrink(line)
    got = out["content"][0].get("data")
    check("direct base64 survives", len(got) > 0, f"len={len(got)}")
    check("direct base64 line fits", encoded_len(out) <= MAX_LINE,
          f"len={encoded_len(out)}")
    check("direct base64 marked truncated", out["content"][0].get("truncated") is True)
    # A text marker inside the base64 makes the field undecodable: the length
    # stops being a multiple of 4 and the padding breaks. The truncation is
    # reported through the "truncated" flag, not through the payload.
    # reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go
    check("direct base64 carries no text marker", MARKER not in got)
    check("direct base64 stays decodable", _decodes(got))


def _decodes(value):
    """True when the base64 payload decodes strictly."""
    try:
        base64.b64decode(value, validate=True)
        return True
    except Exception:
        return False


def test_nested_source_data_is_trimmed():
    payload = "B" * 1_100_000
    line = {"v": 1, "type": "assistant", "agent": "jcode",
            "content": [{"type": "tool_result",
                         "source": {"type": "base64", "media_type": "image/png",
                                    "data": payload}}]}
    out = shrink(line)
    got = out["content"][0]["source"]["data"]
    check("nested base64 survives", len(got) > 0, f"len={len(got)}")
    check("nested base64 line fits", encoded_len(out) <= MAX_LINE,
          f"len={encoded_len(out)}")
    check("nested base64 marked truncated", out["content"][0].get("truncated") is True)
    check("nested base64 carries no text marker", MARKER not in got)
    check("nested base64 stays decodable", _decodes(got))


def test_truncated_payload_is_a_prefix_of_the_original():
    # A valid base64 string: no padding in the middle, so the repeated block
    # stays decodable after the cut.
    payload = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5" * 40_000
    line = {"v": 1, "type": "assistant", "agent": "jcode",
            "content": [{"type": "tool_result", "data": payload}]}
    out = shrink(line)
    got = out["content"][0]["data"]
    check("payload decodes", _decodes(got))
    if _decodes(got):
        original = base64.b64decode(payload)
        kept = base64.b64decode(got)
        check("kept bytes are a prefix of the original",
              original.startswith(kept), f"kept={len(kept)} original={len(original)}")


def test_truncate_base64_always_lands_on_a_multiple_of_four():
    # Every cut must land on a 4-character boundary, and no padding may be
    # appended: "=" would make the length a non-multiple of 4 again.
    # reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go
    source = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5" * 1000
    bad = []
    for limit in range(1, 200):
        got = adapter._truncate_base64(source, limit)
        if len(got) % 4 != 0:
            bad.append((limit, len(got)))
    check("every cut length is a multiple of 4", not bad, f"offenders={bad[:5]}")
    padded = adapter._truncate_base64(source, 101)
    check("cut carries no padding", "=" not in padded)
    check("cut decodes", _decodes(padded))
    if _decodes(padded):
        check("cut is a byte prefix of the source",
              base64.b64decode(source).startswith(base64.b64decode(padded)))


def test_tiny_payload_next_to_huge_text_is_not_emptied():
    # A budget smaller than one base64 quad used to return "" and lose the
    # image entirely: the proportional share rounded down to zero when the
    # other parts of the line were huge. The payload must keep its first quad.
    # reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go
    payload = base64.b64encode(b"x" * 3).decode()
    line = {"v": 1, "type": "assistant", "agent": "jcode",
            "content": [{"type": "text", "text": "T" * 900_000},
                        {"type": "tool_result", "data": payload}]}
    out = shrink(line)
    got = out["content"][1].get("data")
    check("tiny payload survives a huge neighbour", bool(got), f"len={len(got) if got else 0}")
    check("tiny payload still decodes", _decodes(got) if got else False)
    check("line still fits the budget", encoded_len(out) <= MAX_LINE,
          f"len={encoded_len(out)}")


def test_many_tiny_payloads_next_to_a_huge_text():
    payload = base64.b64encode(b"x" * 3).decode()
    blocks = [{"type": "text", "text": "T" * 3_000_000}]
    for _ in range(20):
        blocks.append({"type": "tool_result", "data": payload})
    line = {"v": 1, "type": "assistant", "agent": "jcode", "content": blocks}
    out = shrink(line)
    empties = sum(1 for b in out["content"]
                  if isinstance(b.get("data"), str) and b["data"] == "")
    check("no tiny payload is emptied", empties == 0, f"emptied={empties}")
    check("line still fits the budget", encoded_len(out) <= MAX_LINE,
          f"len={encoded_len(out)}")


def test_small_payload_is_untouched():
    line = {"v": 1, "type": "assistant", "agent": "jcode",
            "content": [{"type": "tool_result", "data": "small"}]}
    out = shrink(line)
    check("small payload unchanged", out["content"][0].get("data") == "small")
    check("small payload not flagged", out["content"][0].get("truncated") is None)
    check("small payload has no marker", MARKER not in json.dumps(out))


def test_mixed_line_fits_and_keeps_every_part():
    line = {"v": 1, "type": "assistant", "agent": "jcode",
            "content": [
                {"type": "text", "text": "T" * 200_000},
                {"type": "tool_result", "result": {"output": "O" * 400_000}},
                {"type": "tool_result", "data": "D" * 600_000},
                {"type": "tool_result",
                 "source": {"type": "base64", "data": "N" * 600_000}},
            ]}
    out = shrink(line)
    check("mixed line fits", encoded_len(out) <= MAX_LINE, f"len={encoded_len(out)}")
    blocks = out["content"]
    check("mixed text kept", len(blocks[0].get("text", "")) > 0)
    check("mixed output kept", len(blocks[1]["result"]["output"]) > 0)
    check("mixed direct payload kept", len(blocks[2].get("data", "")) > 0)
    check("mixed nested payload kept", len(blocks[3]["source"]["data"]) > 0)
    output_len = len(blocks[1]["result"]["output"])
    check("tool output respects its own cap",
          output_len <= adapter._MAX_TOOL_OUTPUT_BYTES + len(MARKER),
          f"len={output_len}")


def test_no_marker_when_nothing_was_cut():
    line = {"v": 1, "type": "assistant", "agent": "jcode",
            "content": [{"type": "tool_result", "result": {"output": "tiny"},
                         "text": "hello"}]}
    out = shrink(line)
    check("no marker without a cut", MARKER not in json.dumps(out))


def test_line_without_content_is_unchanged():
    line = {"v": 1, "type": "assistant", "agent": "jcode"}
    out = shrink(line)
    check("line without content unchanged", out == line)


def main():
    test_direct_base64_is_trimmed_not_dropped()
    test_nested_source_data_is_trimmed()
    test_truncated_payload_is_a_prefix_of_the_original()
    test_truncate_base64_always_lands_on_a_multiple_of_four()
    test_tiny_payload_next_to_huge_text_is_not_emptied()
    test_many_tiny_payloads_next_to_a_huge_text()
    test_small_payload_is_untouched()
    test_mixed_line_fits_and_keeps_every_part()
    test_no_marker_when_nothing_was_cut()
    test_line_without_content_is_unchanged()
    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED: {', '.join(FAILURES)}")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
