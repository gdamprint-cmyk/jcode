# reference: PNG spec (RFC 2083) - chunk layout and CRC-32
# reference: zlib crc32 (zlib.crc32) for PNG chunk checksums
# reference: Pillow (PIL.Image) - PNG decode to pixels
# reference: scripts/entire/png_lossless.py - the lossless PNG re-encoder that
#   produced the backfilled payloads. This checker is an INDEPENDENT
#   cross-check written against the PNG spec, not a copy of that encoder:
#   it reads blobs straight from git objects, validates the CRC-32 of EVERY
#   chunk (the encoder's crc_ok stops at IEND), and compares pixels for EVERY
#   unique image rather than one.
# reference: upstream entire-brain internal/cli/history.go (historyMaxLineBytes = 1024*1024)
# reference: scripts/entire/test_shrink_etf_line.py - the existing ETF line
#   budget test in this repo, which this checker complements.
"""
Acceptance check for the backfilled image payloads in Entire checkpoint refs.

WHAT IT CHECKS, per checkpoint ref:
  1. the ref resolves, and its newest commit's PARENT is the expected
     pre-backfill commit (the backfill appended; it did not rewrite history);
  2. every image-payload field (`data`) is non-empty - the SAME check is run
     against the parent commit as a NEGATIVE CONTROL, and it must report
     empty payloads there, otherwise the check cannot discriminate;
  3. every PNG payload is valid: signature, IHDR present, IEND last, and the
     CRC-32 of EVERY chunk recomputed and compared;
  4. no compact-transcript line exceeds the Brain's 1 MiB line cap;
  5. every unique image is PIXEL-EQUAL to an original taken from the parent's
     blobs, and every image is fully opaque (alpha min == 255), so that a
     lossy-looking RGBA->RGB step was in fact lossless.

Run:  python scripts/entire/test_backfill_payloads.py [--repo PATH]
Exit: 0 = PASS, 1 = FAIL.

The refs and their expected parents are declared in REFS below. Edit them
together with any future backfill; the check is meaningless if the expected
parents are not the ones the backfill actually used.
"""
import argparse
import base64
import binascii
import hashlib
import io
import json
import os
import re
import struct
import subprocess
import sys
import zlib

BRAIN_LINE_CAP = 1048576
PNG_SIG = b"\x89PNG\r\n\x1a\n"
_B64_RE = re.compile(r"[A-Za-z0-9+/]+={0,2}")

# tag -> (refname, expected parent commit prefix)
REFS = {
    "48": ("refs/entire/checkpoints/48/01M6XKDWSZJV8A2RYH3HPTF148", "cccb8db0f"),
    "Q8": ("refs/entire/checkpoints/Q8/01M6XNE5KHJ3Q1K4HZMYGXXMQ8", "37c283d09"),
    "WM": ("refs/entire/checkpoints/WM/01M6XSY3957WTVEAC3AAQ5HHWM", "0805bb039"),
    "JB": ("refs/entire/checkpoints/JB/01M6RG8ETEZYR68JGVS6Z9BKJB", "3a1373c32"),
    "KY": ("refs/entire/checkpoints/KY/01M6RNAJKMY06EECE6VY8FFVKY", "f54acc831"),
    "XT": ("refs/entire/checkpoints/XT/01M6RWSQW4MWDA3VGEMC3B02XT", "24d661f91"),
    "ZA": ("refs/entire/checkpoints/ZA/01M6RJ08BGSA3P8CFYCA5JGQZA", "174b2fbfb"),
}


class Git:
    def __init__(self, repo):
        self.repo = repo

    def run(self, *args):
        return subprocess.run(("git",) + args, cwd=self.repo, capture_output=True)

    def bytes(self, *args):
        p = self.run(*args)
        if p.returncode != 0:
            raise RuntimeError("git %s failed: %s"
                               % (args, p.stderr.decode("utf-8", "replace")))
        return p.stdout

    def rev(self, rev):
        p = self.run("rev-parse", rev)
        return None if p.returncode != 0 else p.stdout.decode().strip()

    def tree(self, rev):
        out = {}
        for line in self.bytes("ls-tree", "-r", rev).decode("utf-8", "replace").splitlines():
            meta, path = line.split("\t", 1)
            mode, typ, sha = meta.split()
            out[path] = sha
        return out

    def blob(self, sha):
        return self.bytes("cat-file", "blob", sha)


def strict_b64(s):
    return bool(s) and len(s) % 4 == 0 and bool(_B64_RE.fullmatch(s))


def decode_b64(s):
    try:
        return base64.b64decode(s, validate=True)
    except (binascii.Error, ValueError):
        return None


def check_png(raw):
    """Validate EVERY chunk CRC-32. Returns (ok, error, chunk_types)."""
    if not raw.startswith(PNG_SIG):
        return False, "bad signature", []
    off = 8
    types = []
    saw_ihdr = False
    saw_iend = False
    while off + 8 <= len(raw):
        (ln,) = struct.unpack(">I", raw[off:off + 4])
        typ = raw[off + 4:off + 8]
        if off + 12 + ln > len(raw):
            return False, "truncated chunk %r at %d" % (typ, off), types
        data = raw[off + 8:off + 8 + ln]
        (crc,) = struct.unpack(">I", raw[off + 8 + ln:off + 12 + ln])
        want = zlib.crc32(typ + data) & 0xFFFFFFFF
        if crc != want:
            return False, ("CRC mismatch in %r at %d (got %08x want %08x)"
                           % (typ.decode("latin1"), off, crc, want)), types
        types.append(typ.decode("latin1"))
        if typ == b"IHDR":
            saw_ihdr = True
        if typ == b"IEND":
            saw_iend = True
            off += 12 + ln
            break
        off += 12 + ln
    if not saw_ihdr:
        return False, "no IHDR chunk (types=%s)" % types, types
    if not saw_iend:
        return False, "no IEND chunk (types=%s)" % types, types
    return True, "ok", types


def pixels(raw):
    from PIL import Image
    im = Image.open(io.BytesIO(raw))
    im.load()
    if im.mode != "RGBA":
        im = im.convert("RGBA")
    return im.size, list(im.getdata())


def iter_strings(data):
    """Yield (line_idx, json_path, value) for every string value in the JSONL."""
    for idx, raw in enumerate(data.split(b"\n")):
        if not raw.strip():
            continue
        try:
            obj = json.loads(raw.decode("utf-8"))
        except Exception:
            continue
        stack = [("", obj)]
        while stack:
            path, val = stack.pop()
            if isinstance(val, dict):
                for k, v in val.items():
                    stack.append((path + "/" + str(k), v))
            elif isinstance(val, list):
                for i, v in enumerate(val):
                    stack.append((path + "/%d" % i, v))
            elif isinstance(val, str):
                yield (idx, path, val)


def find_pngs(data):
    """Every strict-base64 string in the blob that decodes to a PNG."""
    out = []
    for idx, path, s in iter_strings(data):
        if len(s) < 64 or not strict_b64(s):
            continue
        rb = decode_b64(s)
        if rb is not None and rb.startswith(PNG_SIG):
            out.append((idx, path, s, rb))
    return out


def count_empty_payload_fields(git, tree):
    """Count EMPTY image-payload fields across a whole tree.

    The binary image payload lives in the `data` field. The sibling `content`
    field is the message TEXT and is legitimately empty for tool-call blocks,
    so it is NOT counted here - counting it produced a false failure once.
    """
    empties = 0
    total = 0
    for path, sha in tree.items():
        if "transcript" not in path and "full.jsonl" not in path:
            continue
        data = git.blob(sha)
        for _idx, raw in enumerate(data.split(b"\n")):
            if not raw.strip():
                continue
            try:
                obj = json.loads(raw.decode("utf-8"))
            except Exception:
                continue
            stack = [obj]
            while stack:
                val = stack.pop()
                if isinstance(val, dict):
                    for k, v in val.items():
                        if k == "data" and isinstance(v, str):
                            total += 1
                            if v == "":
                                empties += 1
                        stack.append(v)
                elif isinstance(val, list):
                    for v in val:
                        stack.append(v)
    return total, empties


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", default=os.path.dirname(os.path.dirname(
        os.path.dirname(os.path.abspath(__file__)))))
    args = ap.parse_args()
    git = Git(args.repo)

    print("REPO:", args.repo)
    print("=" * 78)
    failures = []
    control_fired = 0
    unique = {}
    all_originals = {}
    all_resolve = True

    for tag, (refname, old_sha) in sorted(REFS.items()):
        new_sha = git.rev(refname)
        if new_sha is None:
            failures.append("%s: ref %s does not resolve" % (tag, refname))
            all_resolve = False
            continue
        parent = git.rev(new_sha + "^")
        if parent is None:
            failures.append("%s: no parent commit" % tag)
            all_resolve = False
            continue
        parent_ok = parent.startswith(old_sha)

        p_tree = git.tree(parent)
        c_tree = git.tree(new_sha)
        p_total, p_empty = count_empty_payload_fields(git, p_tree)
        c_total, c_empty = count_empty_payload_fields(git, c_tree)

        ctrl = p_empty > 0
        if ctrl:
            control_fired += 1
        else:
            failures.append("%s: NEGATIVE CONTROL FAILED - parent shows %d empty payloads"
                            % (tag, p_empty))

        max_line = 0
        n_lines = 0
        cap_ok = True
        png_bad = 0
        png_total = 0
        for p in sorted(c_tree):
            if "transcript" not in p:
                continue
            data = git.blob(c_tree[p])
            for raw in data.split(b"\n"):
                if not raw.strip():
                    continue
                n_lines += 1
                if len(raw) > max_line:
                    max_line = len(raw)
                if len(raw) > BRAIN_LINE_CAP:
                    cap_ok = False
            for _idx, _path, _s, rb in find_pngs(data):
                png_total += 1
                ok, err, _t = check_png(rb)
                if not ok:
                    png_bad += 1
                    failures.append("%s: invalid PNG in %s: %s" % (tag, p, err))
                unique[hashlib.sha256(rb).hexdigest()] = rb

        # ORIGINALS: the pre-backfill PNGs, taken from the PARENT commit's blobs.
        originals = {}
        for p in sorted(p_tree):
            if "transcript" not in p and "full.jsonl" not in p:
                continue
            data = git.blob(p_tree[p])
            for _idx, _path, _s, rb in find_pngs(data):
                originals.setdefault(hashlib.sha256(rb).hexdigest(), rb)
        all_originals.update(originals)

        print("[%s] %s" % (tag, new_sha))
        print("   parent=%s  parent_is_expected=%s" % (parent, parent_ok))
        print("   payload fields: PARENT total=%d empty=%d | CHILD total=%d empty=%d"
              % (p_total, p_empty, c_total, c_empty))
        print("   NEGATIVE CONTROL (parent must have empties>0): %s"
              % ("PASS" if ctrl else "FAIL"))
        print("   lines=%d  max_line=%d  cap=%d  cap_ok=%s"
              % (n_lines, max_line, BRAIN_LINE_CAP, cap_ok))
        print("   PNG payloads=%d invalid=%d   originals_from_parent=%d"
              % (png_total, png_bad, len(originals)))
        if not parent_ok:
            failures.append("%s: parent %s does not match the expected old sha %s"
                            % (tag, parent, old_sha))
        if not cap_ok:
            failures.append("%s: a compact-transcript line exceeds the 1 MiB cap" % tag)
        if c_empty != 0:
            failures.append("%s: child still has %d empty payload fields" % (tag, c_empty))
        if png_bad:
            failures.append("%s: %d invalid PNG payloads" % (tag, png_bad))
        print("-" * 78)

    # --- pixel equality + alpha over ALL unique images ---
    print("UNIQUE IMAGES: %d" % len(unique))
    print("UNIQUE ORIGINALS (parent): %d" % len(all_originals))
    mismatch = []
    alpha_bad = []
    matched_exact = 0
    matched_pixel = 0
    unmatched = []
    for h, rb in sorted(unique.items()):
        try:
            size, px = pixels(rb)
        except Exception as e:
            failures.append("unique %s: cannot decode: %s" % (h[:12], e))
            continue
        min_alpha = min(p[3] for p in px)
        if min_alpha != 255:
            alpha_bad.append((h, min_alpha))
        orig = all_originals.get(h)
        if orig is not None:
            matched_exact += 1
            try:
                osize, opx = pixels(orig)
                if (osize, opx) != (size, px):
                    mismatch.append(h)
            except Exception:
                mismatch.append(h)
            continue
        # bytes differ (re-encoded) -> find a pixel-equal original
        found = False
        for oh, orb in all_originals.items():
            try:
                osize, opx = pixels(orb)
            except Exception:
                continue
            if (osize, opx) == (size, px):
                found = True
                matched_pixel += 1
                break
        if not found:
            unmatched.append(h)

    print("images byte-identical to a parent original : %d" % matched_exact)
    print("images re-encoded but pixel-equal            : %d" % matched_pixel)
    print("images with NO pixel-equal original          : %d" % len(unmatched))
    for h in unmatched:
        print("   NO ORIGINAL %s" % h[:16])
    print("pixel MISMATCHES (original exists, differs)  : %d" % len(mismatch))
    for h in mismatch:
        print("   MISMATCH %s" % h[:16])
    print("alpha not fully opaque (min<255)             : %d" % len(alpha_bad))
    for h, a in alpha_bad:
        print("   ALPHA %s min=%d" % (h[:16], a))

    if mismatch:
        failures.append("%d images differ in pixels from their original" % len(mismatch))
    if unmatched:
        failures.append("%d images have no pixel-equal original in the parent" % len(unmatched))
    if alpha_bad:
        failures.append("%d images are not fully opaque" % len(alpha_bad))
    if control_fired == 0:
        failures.append("negative control never fired on any ref")

    print("=" * 78)
    print("all refs resolve        : %s" % all_resolve)
    print("negative controls fired : %d/%d" % (control_fired, len(REFS)))
    if failures:
        print("FAILURES (%d):" % len(failures))
        for f in failures:
            print("  - %s" % f)
        print("RESULT: FAIL")
        return 1
    print("RESULT: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
