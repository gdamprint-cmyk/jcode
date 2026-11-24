#!/usr/bin/env python3
"""Regression tests for adapter-local image externalization in entire-agent-jcode.py.

reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/transcript/imageextract/imageextract.go
reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/checkpoint/persistent.go

An oversized image used to be TRUNCATED: _shrink_etf_line cut the base64 on a
quad boundary and flagged the block "truncated": true. The cut is a valid
base64 prefix, so the line survived the Brain's scanner but the picture was
destroyed and nothing anywhere held the rest.

The adapter now externalizes the image instead: the raw bytes go to the
adapter's asset store and the payload becomes upstream's placeholder.

The tests below are the acceptance criteria:
  - an oversized image is externalized, not truncated;
  - the stored bytes are the ORIGINAL bytes (byte-exact);
  - the placeholder is low-entropy enough to survive upstream's redaction;
  - the manifest records the asset;
  - reinjection restores the original bytes;
  - a line that FITS is untouched (no externalization, byte-for-byte);
  - a NON-image payload is still trimmed (the old behaviour is preserved);
  - NEGATIVE CONTROLS: a small image is not externalized, and the old
    truncate path really does destroy the image.

Run: python test_image_externalization.py
"""

import base64
import hashlib
import importlib.util
import json
import math
import os
import shutil
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ADAPTER = os.path.join(HERE, "entire-agent-jcode.py")

_spec = importlib.util.spec_from_file_location("adapter", ADAPTER)
adapter = importlib.util.module_from_spec(_spec)
sys.argv = ["adapter"]
_spec.loader.exec_module(adapter)

FAILURES = []

# A real PNG header plus filler, so the payload is a genuine image byte string
# and not a run of one repeated character.
PNG_HEADER = bytes.fromhex(
    "89504e470d0a1a0a0000000d4948445200000001000000010806000000"
    "1f15c4890000000a49444154789c6300010000050001"
)


def _image_bytes(size):
    return PNG_HEADER + bytes((i * 7) % 251 for i in range(size))


def check(name, condition, detail=""):
    if condition:
        print(f"PASS {name}")
    else:
        print(f"FAIL {name} {detail}")
        FAILURES.append(name)


class TempHome:
    """A throwaway JCODE_HOME so the real session store is never touched."""

    def __enter__(self):
        self.root = tempfile.mkdtemp(prefix="adapter_home_")
        self._old = os.environ.get("JCODE_HOME")
        os.environ["JCODE_HOME"] = self.root
        return self.root

    def __exit__(self, *exc):
        if self._old is None:
            os.environ.pop("JCODE_HOME", None)
        else:
            os.environ["JCODE_HOME"] = self._old
        shutil.rmtree(self.root, ignore_errors=True)
        return False


def _line_with_image(payload_b64, extra_text=""):
    blocks = []
    if extra_text:
        blocks.append({"type": "text", "text": extra_text})
    blocks.append({"type": "image", "media_type": "image/png", "data": payload_b64})
    return {"v": 1, "type": "user", "agent": "jcode", "content": blocks}


def _shannon_bits_per_char(text):
    if not text:
        return 0.0
    counts = {}
    for ch in text:
        counts[ch] = counts.get(ch, 0) + 1
    n = len(text)
    entropy = 0.0
    for count in counts.values():
        p = count / n
        entropy -= p * math.log2(p)
    return entropy


def test_oversized_image_is_externalized_not_truncated():
    with TempHome() as root:
        raw = _image_bytes(400_000)
        payload = base64.b64encode(raw).decode("ascii")
        session_id = "session_ext_test"
        assets = []
        line = _line_with_image(payload)
        out = adapter._shrink_etf_line(line, 10_000_000, session_id, assets)

        block = out["content"][-1]
        check("oversized image externalized",
              block["data"].startswith(adapter._ASSET_PLACEHOLDER_PREFIX),
              f"data={block['data'][:80]}")
        check("oversized image not flagged truncated",
              block.get("truncated") is not True, f"truncated={block.get('truncated')}")
        check("one asset recorded", len(assets) == 1, f"assets={len(assets)}")
        check("line fits the budget",
              len(adapter._encode_etf_line(out)) <= adapter._MAX_LINE_BYTES,
              f"len={len(adapter._encode_etf_line(out))}")

        # The stored blob must be the ORIGINAL bytes, byte for byte.
        name = assets[0]["name"]
        blob = adapter._assets_dir(session_id) / name
        check("asset blob written", blob.exists(), f"path={blob}")
        if blob.exists():
            stored = blob.read_bytes()
            check("asset bytes are byte-exact", stored == raw,
                  f"stored={len(stored)} original={len(raw)}")
            check("manifest sha256 matches",
                  assets[0]["sha256"] == hashlib.sha256(raw).hexdigest())
            check("manifest size matches", assets[0]["size"] == len(raw))
            check("asset name extension follows media type", name.endswith(".png"), name)


def test_manifest_is_written():
    with TempHome() as root:
        raw = _image_bytes(400_000)
        payload = base64.b64encode(raw).decode("ascii")
        session_id = "session_manifest_test"
        document = {"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "x" * 600_000},
                {"type": "image", "media_type": "image/png", "data": payload},
            ]},
        ]}
        data = adapter._etf_jsonl(document, session_id)
        manifest_path = adapter._asset_manifest_path(session_id)
        check("manifest written", manifest_path.exists(), f"path={manifest_path}")
        if manifest_path.exists():
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            check("manifest version is 1", manifest.get("version") == 1)
            check("manifest lists one asset", len(manifest.get("assets", [])) == 1)
            entry = manifest["assets"][0]
            for key in ("name", "media_type", "size", "sha256"):
                check(f"manifest entry has {key}", key in entry)
        # The transcript must carry the placeholder, not the base64.
        check("transcript carries the placeholder",
              adapter._ASSET_PLACEHOLDER_PREFIX.encode() in data)
        check("transcript does not carry the raw base64",
              payload.encode() not in data)


def test_reinjection_restores_the_original_bytes():
    with TempHome() as root:
        raw = _image_bytes(400_000)
        payload = base64.b64encode(raw).decode("ascii")
        session_id = "session_reinject_test"
        assets = []
        line = _line_with_image(payload)
        out = adapter._shrink_etf_line(line, 10_000_000, session_id, assets)
        stored = adapter._encode_etf_line(out)

        restored = adapter._reinject_assets(session_id, stored)
        check("reinjection removes the placeholder",
              adapter._ASSET_PLACEHOLDER_PREFIX.encode() not in restored)
        check("reinjection restores the original base64",
              payload.encode() in restored,
              f"len(restored)={len(restored)}")
        check("reinjected bytes decode to the original",
              base64.b64decode(payload) in base64.b64decode(
                  restored.split(b'"data":"')[1].split(b'"')[0]))


def test_placeholder_is_low_entropy():
    """Upstream's redaction flags any [A-Za-z0-9+_=-]{10,} run at >= 4.5 bits/char."""
    with TempHome() as root:
        raw = _image_bytes(400_000)
        payload = base64.b64encode(raw).decode("ascii")
        assets = []
        out = adapter._shrink_etf_line(_line_with_image(payload), 10_000_000,
                                       "session_entropy_test", assets)
        placeholder = out["content"][-1]["data"]
        runs = []
        current = ""
        for ch in placeholder:
            if ch.isalnum() or ch in "+_=-":
                current += ch
            else:
                if len(current) >= 10:
                    runs.append(current)
                current = ""
        if len(current) >= 10:
            runs.append(current)
        check("placeholder has detector-sized runs", len(runs) > 0, placeholder)
        for run in runs:
            entropy = _shannon_bits_per_char(run)
            check(f"run {run[:20]!r} below the 4.5 bits/char threshold",
                  entropy < 4.5, f"entropy={entropy:.2f}")


def test_fitting_line_is_untouched():
    """A line that fits today must be byte-for-byte unchanged."""
    with TempHome() as root:
        raw = _image_bytes(1_000)
        payload = base64.b64encode(raw).decode("ascii")
        session_id = "session_small_test"
        document = {"messages": [
            {"role": "user", "content": [
                {"type": "image", "media_type": "image/png", "data": payload},
            ]},
        ]}
        data = adapter._etf_jsonl(document, session_id)
        check("small image stays inline", payload.encode() in data)
        check("small image creates no manifest",
              not adapter._asset_manifest_path(session_id).exists())


def test_non_image_payload_is_still_trimmed():
    """The old behaviour is preserved for payloads that are not images."""
    with TempHome() as root:
        payload = "A" * 1_100_000
        session_id = "session_nonimage_test"
        assets = []
        line = {"v": 1, "type": "assistant", "agent": "jcode",
                "content": [{"type": "tool_result", "data": payload}]}
        out = adapter._shrink_etf_line(line, 10_000_000, session_id, assets)
        got = out["content"][0].get("data")
        check("non-image payload is not externalized", len(assets) == 0)
        check("non-image payload is trimmed", len(got) < len(payload))
        check("non-image payload marked truncated",
              out["content"][0].get("truncated") is True)
        check("non-image payload line fits",
              len(adapter._encode_etf_line(out)) <= adapter._MAX_LINE_BYTES)


def test_negative_control_the_old_path_destroyed_the_image():
    """NEGATIVE CONTROL.

    The old behaviour (trim, no externalization) really did destroy the image:
    the surviving base64 is a PREFIX of the original, so the decoded bytes are
    shorter than the original. If this control does not fire, the test above
    proves nothing.
    """
    raw = _image_bytes(400_000)
    payload = base64.b64encode(raw).decode("ascii")
    trimmed = adapter._truncate_base64(payload, 100_000)
    decoded = base64.b64decode(trimmed)
    check("control: the trimmed payload is a strict prefix",
          len(decoded) < len(raw), f"decoded={len(decoded)} original={len(raw)}")
    check("control: the trimmed bytes are NOT the original image",
          decoded != raw[:len(decoded)] or len(decoded) != len(raw))

    with TempHome() as root:
        assets = []
        line = _line_with_image(payload)
        out = adapter._shrink_etf_line(line, 10_000_000, None, assets)
        got = out["content"][-1]["data"]
        check("control: without externalization the image is cut",
              got != payload, f"len={len(got)}")
        check("control: the cut is flagged truncated",
              out["content"][-1].get("truncated") is True)


def test_small_image_is_not_externalized():
    """NEGATIVE CONTROL: below the threshold, nothing is externalized."""
    with TempHome() as root:
        payload = base64.b64encode(b"x" * 3).decode("ascii")
        assets = []
        line = _line_with_image(payload)
        out = adapter._shrink_etf_line(line, 10_000_000, "session_tiny_test", assets)
        check("tiny payload not externalized", len(assets) == 0)
        check("tiny payload left inline",
              out["content"][-1]["data"] == payload)


def test_undecodable_payload_is_left_inline():
    """A payload that is not decodable base64 must not be stored as an asset."""
    with TempHome() as root:
        payload = "not valid base64!!!" * 100
        assets = []
        line = _line_with_image(payload)
        out = adapter._shrink_etf_line(line, 10_000_000, "session_bad_test", assets)
        check("undecodable payload not externalized", len(assets) == 0)
        check("undecodable payload left inline",
              out["content"][-1]["data"] == payload)


def test_repeated_conversion_is_idempotent_and_leaves_no_orphan():
    """A second conversion of the same session must not grow the asset store.

    MEASURED 2026-11-24 (scratch/repro_manifest_loss.py): the name used to be
    fresh random per call, so each materialization wrote a SECOND blob for the
    SAME bytes and referenced only that one; the earlier blob stayed on disk
    with no manifest entry. The store grew by N files per pass and the manifest
    misreported it.
    """
    with TempHome() as root:
        raw = _image_bytes(400_000)
        payload = base64.b64encode(raw).decode("ascii")
        session_id = "session_repeat_test"
        document = {"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "x" * 600_000},
                {"type": "image", "media_type": "image/png", "data": payload},
            ]},
        ]}
        first = adapter._etf_jsonl(document, session_id)
        assets_dir = adapter._assets_dir(session_id)
        after_first = sorted(p.name for p in assets_dir.iterdir() if p.name != adapter._ASSET_MANIFEST_NAME)
        first_manifest = json.loads(adapter._asset_manifest_path(session_id).read_text(encoding="utf-8"))

        second = adapter._etf_jsonl(document, session_id)
        after_second = sorted(p.name for p in assets_dir.iterdir() if p.name != adapter._ASSET_MANIFEST_NAME)
        second_manifest = json.loads(adapter._asset_manifest_path(session_id).read_text(encoding="utf-8"))

        check("same placeholder across conversions",
              first.split(b'"data":"')[1] == second.split(b'"data":"')[1])
        check("asset store does not grow on re-conversion",
              after_first == after_second,
              f"{after_first} -> {after_second}")
        check("manifest keeps every stored asset",
              {a["name"] for a in first_manifest["assets"]}
              <= {a["name"] for a in second_manifest["assets"]},
              f"lost={sorted({a['name'] for a in first_manifest['assets']} - {a['name'] for a in second_manifest['assets']})}")
        referenced = {a["name"] for a in second_manifest["assets"]}
        on_disk = set(after_second)
        check("no blob is unreferenced (no orphan)",
              on_disk <= referenced, f"orphans={sorted(on_disk - referenced)}")
        check("no reference is missing its blob (no dangling)",
              referenced <= on_disk, f"dangling={sorted(referenced - on_disk)}")


def main():
    test_oversized_image_is_externalized_not_truncated()
    test_manifest_is_written()
    test_reinjection_restores_the_original_bytes()
    test_placeholder_is_low_entropy()
    test_fitting_line_is_untouched()
    test_non_image_payload_is_still_trimmed()
    test_negative_control_the_old_path_destroyed_the_image()
    test_small_image_is_not_externalized()
    test_undecodable_payload_is_left_inline()
    test_repeated_conversion_is_idempotent_and_leaves_no_orphan()
    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED: {', '.join(FAILURES)}")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
