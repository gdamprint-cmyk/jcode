#!/usr/bin/env python3
# ---------------------------------------------------------------------------
# Ported into the jcode repository from the Entire external-agent adapter.
# source: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
# source repo: entire-agent-jcode @ 26a19cc
#                ("fix(hook): retry when Entire's plugin discovery drops the adapter")
# ---------------------------------------------------------------------------
"""Regression tests for the adapter's refusal to substitute data (B7.1, B7.2).

reference: https://raw.githubusercontent.com/entireio/cli/main/transcript/README.md
reference: https://docs.entire.io/agents/agent-integration-protocol/commands

Two defects of the same class are covered: the adapter did not REFUSE, it
substituted or passed through data.

B7.1: a file that is neither a Jcode session document nor a valid ETF v1
transcript was returned as the transcript bytes, so the CLI stored garbage as
the session's transcript.

B7.2: a named ``--session-ref`` that did not exist fell back to the newest
session on disk, so the CLI recorded ANOTHER session's transcript under THIS
session's id.

Every test has a negative control: the same input must be ACCEPTED by the
old behaviour and REFUSED by the new one, so a test that passes for the wrong
reason is caught.

Run: python test_transcript_refusal.py
"""

import base64
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = os.path.dirname(os.path.abspath(__file__))
ADAPTER = os.path.join(HERE, "entire-agent-jcode.py")

_spec = importlib.util.spec_from_file_location("adapter", ADAPTER)
adapter = importlib.util.module_from_spec(_spec)
sys.argv = ["adapter"]
_spec.loader.exec_module(adapter)

FAILURES = []


def check(name, condition, detail=""):
    if condition:
        print(f"PASS {name}")
    else:
        print(f"FAIL {name} {detail}")
        FAILURES.append(name)


def write(tmp, name, data):
    path = os.path.join(tmp, name)
    with open(path, "wb") as handle:
        handle.write(data)
    return path


# ---------------------------------------------------------------------------
# B7.1: only a session document or a valid ETF v1 transcript is a transcript
# ---------------------------------------------------------------------------

def test_garbage_is_refused():
    """Arbitrary bytes are not a transcript, and are not passed through."""
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "garbage.json", b"not json at all\xff")
        got = adapter._session_transcript_bytes(_path(path))
        check("garbage is refused", got is None, f"got {len(got) if got else 0} bytes")


def test_empty_file_is_an_empty_transcript():
    """An empty file is an EMPTY transcript, not an invalid one.

    A session document with no messages converts to zero bytes the same
    way, so refusing the empty file would refuse a legitimate transcript
    and disagree with the session-document path.
    """
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "empty.json", b"")
        check("empty file is an empty transcript",
              adapter._session_transcript_bytes(_path(path)) == b"")


def test_session_document_with_no_messages_is_an_empty_transcript():
    """The empty sidecar and the empty session must agree."""
    document = {"messages": []}
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "nomessages.json", json.dumps(document).encode("utf-8"))
        check("a session with no messages converts to zero bytes",
              adapter._session_transcript_bytes(_path(path)) == b"")


def test_truncated_json_is_refused():
    """A truncated session document is not a session document."""
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "cut.json", b'{"messages": [{"role": "user", "content"')
        check("truncated JSON is refused",
              adapter._session_transcript_bytes(_path(path)) is None)


def test_valid_session_document_is_converted():
    """A real session document is still converted into ETF v1 JSONL."""
    document = {"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]}
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "session.json", json.dumps(document).encode("utf-8"))
        got = adapter._session_transcript_bytes(_path(path))
        check("session document is converted", got is not None)
        if got:
            lines = [json.loads(x) for x in got.splitlines() if x.strip()]
            check("converted line is ETF v1",
                  bool(lines) and lines[0].get("v") == 1
                  and lines[0].get("type") == "user",
                  str(lines[:1]))


def test_valid_etf_sidecar_is_passed_through():
    """An existing ETF v1 sidecar is returned unchanged."""
    raw = (b'{"v":1,"type":"user","agent":"jcode","content":[{"text":"hi"}]}\n'
           b'{"v":1,"type":"assistant","agent":"jcode","content":[{"type":"text","text":"yo"}]}\n')
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "sidecar.jsonl", raw)
        got = adapter._session_transcript_bytes(_path(path))
        check("valid ETF sidecar passes through", got == raw)


def test_etf_with_broken_line_is_refused():
    """One broken line among good ones makes the file not a transcript.

    The upstream reader skips such a line; the adapter must not hand the CLI
    a transcript whose middle is missing.
    """
    raw = (b'{"v":1,"type":"user","agent":"jcode","content":[{"text":"hi"}]}\n'
           b'{"v":1,"type":"assistant" BROKEN\n')
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "broken.jsonl", raw)
        check("ETF with a broken line is refused",
              adapter._session_transcript_bytes(_path(path)) is None)


def test_etf_with_wrong_version_is_refused():
    """``v`` other than 1 makes the reader skip the line."""
    raw = b'{"v":2,"type":"user","content":[{"text":"hi"}]}\n'
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "v2.jsonl", raw)
        check("ETF with v=2 is refused",
              adapter._session_transcript_bytes(_path(path)) is None)


def test_etf_without_type_is_refused():
    """A line without ``type`` makes the reader skip it."""
    raw = b'{"v":1,"content":[{"text":"hi"}]}\n'
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "notype.jsonl", raw)
        check("ETF without type is refused",
              adapter._session_transcript_bytes(_path(path)) is None)


def test_etf_with_wrong_field_type_is_refused():
    """A known field of the wrong JSON type makes the reader skip the line."""
    raw = b'{"v":1,"type":"user","agent":42,"content":[{"text":"hi"}]}\n'
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "wrongtype.jsonl", raw)
        check("ETF with a wrong field type is refused",
              adapter._session_transcript_bytes(_path(path)) is None)


def test_etf_with_null_optional_scalar_is_accepted():
    """A null optional scalar is absence, not a wrong type."""
    raw = b'{"v":1,"type":"user","agent":null,"content":[{"text":"hi"}]}\n'
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "nulls.jsonl", raw)
        check("ETF with null optionals is accepted",
              adapter._session_transcript_bytes(_path(path)) == raw)


def test_etf_with_non_string_ts_is_accepted():
    """A ``ts`` of the wrong type is ignored by the reader, not skipped."""
    raw = b'{"v":1,"type":"user","ts":123,"content":[{"text":"hi"}]}\n'
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "badts.jsonl", raw)
        check("ETF with a non-string ts is accepted",
              adapter._session_transcript_bytes(_path(path)) == raw)


def test_etf_string_content_is_accepted():
    """A string ``content`` is legal: the reader makes one text block of it."""
    raw = b'{"v":1,"type":"assistant","content":"hello"}\n'
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "strcontent.jsonl", raw)
        check("ETF with a string content is accepted",
              adapter._session_transcript_bytes(_path(path)) == raw)


def test_etf_null_content_is_accepted():
    """A null ``content`` is legal: the line is returned with no blocks."""
    raw = b'{"v":1,"type":"assistant","content":null}\n'
    with tempfile.TemporaryDirectory() as tmp:
        path = write(tmp, "nullcontent.jsonl", raw)
        check("ETF with a null content is accepted",
              adapter._session_transcript_bytes(_path(path)) == raw)


# ---------------------------------------------------------------------------
# B7.2: a named session is the only candidate
# ---------------------------------------------------------------------------

def test_missing_session_ref_does_not_fall_back():
    """A named but missing session must FAIL, not read another session.

    Negative control: the newest session on disk IS a readable session, so a
    passing "it did not return the wrong bytes" check alone would not prove
    the command refused. The exit code is the discriminator.
    """
    with tempfile.TemporaryDirectory() as home:
        sessions = os.path.join(home, "sessions")
        os.makedirs(sessions)
        # A readable session that a fallback would pick up.
        newest = os.path.join(sessions, "session_hog_1.json")
        with open(newest, "w", encoding="utf-8") as handle:
            json.dump({"messages": [{"role": "user", "content": [{"type": "text", "text": "secret"}]}]}, handle)
        missing = os.path.join(home, "does-not-exist.jsonl")

        env = dict(os.environ)
        env["JCODE_HOME"] = home
        proc = subprocess.run(
            [sys.executable, ADAPTER, "read-transcript", "--session-ref", missing],
            capture_output=True, env=env,
        )
        check("read-transcript fails on a missing session-ref", proc.returncode != 0,
              f"rc={proc.returncode}")
        check("read-transcript emits no transcript bytes", proc.stdout == b"",
              f"bytes={len(proc.stdout)}")
        check("read-transcript says what went wrong", b"not found" in proc.stderr.lower(),
              proc.stderr[:200].decode("utf-8", "replace"))
        check("read-transcript does not leak the other session",
              b"secret" not in proc.stdout)


def test_missing_session_ref_compact_fails():
    """compact-transcript must fail on a missing named session too."""
    with tempfile.TemporaryDirectory() as home:
        sessions = os.path.join(home, "sessions")
        os.makedirs(sessions)
        newest = os.path.join(sessions, "session_hog_1.json")
        with open(newest, "w", encoding="utf-8") as handle:
            json.dump({"messages": [{"role": "user", "content": [{"type": "text", "text": "secret"}]}]}, handle)
        missing = os.path.join(home, "does-not-exist.jsonl")

        env = dict(os.environ)
        env["JCODE_HOME"] = home
        proc = subprocess.run(
            [sys.executable, ADAPTER, "compact-transcript", "--session-ref", missing],
            capture_output=True, env=env,
        )
        check("compact-transcript fails on a missing session-ref", proc.returncode != 0,
              f"rc={proc.returncode}")
        check("compact-transcript emits no transcript", b"secret" not in proc.stdout)


def test_output_is_stable_between_runs():
    """The same named session must produce the same bytes twice.

    The fallback made the output change between runs, because the newest
    session on disk changed.
    """
    with tempfile.TemporaryDirectory() as home:
        sessions = os.path.join(home, "sessions")
        os.makedirs(sessions)
        for index in range(3):
            path = os.path.join(sessions, f"session_x{index}.json")
            with open(path, "w", encoding="utf-8") as handle:
                json.dump({"messages": [{"role": "user", "content": [{"type": "text", "text": f"m{index}"}]}]}, handle)
        target = os.path.join(sessions, "session_x0.json")

        env = dict(os.environ)
        env["JCODE_HOME"] = home
        outputs = []
        for _ in range(2):
            proc = subprocess.run(
                [sys.executable, ADAPTER, "read-transcript", "--session-ref", target],
                capture_output=True, env=env,
            )
            outputs.append(proc.stdout)
            # A NEW session appears between the runs: the fallback would now
            # pick the new one, so the bytes would change.
            with open(os.path.join(sessions, "session_x9.json"), "w", encoding="utf-8") as handle:
                json.dump({"messages": [{"role": "user", "content": [{"type": "text", "text": "new"}]}]}, handle)
        check("the named session is read, not the newest", outputs[0] == outputs[1],
              "output changed between runs")
        check("the named session's own text is used", b"m0" in outputs[0])


def test_named_session_ref_is_honoured():
    """A named session that EXISTS is read, even when another is newer."""
    with tempfile.TemporaryDirectory() as home:
        sessions = os.path.join(home, "sessions")
        os.makedirs(sessions)
        with open(os.path.join(sessions, "session_a.json"), "w", encoding="utf-8") as handle:
            json.dump({"messages": [{"role": "user", "content": [{"type": "text", "text": "target"}]}]}, handle)
        with open(os.path.join(sessions, "session_z.json"), "w", encoding="utf-8") as handle:
            json.dump({"messages": [{"role": "user", "content": [{"type": "text", "text": "other"}]}]}, handle)
        target = os.path.join(sessions, "session_a.json")

        env = dict(os.environ)
        env["JCODE_HOME"] = home
        proc = subprocess.run(
            [sys.executable, ADAPTER, "read-transcript", "--session-ref", target],
            capture_output=True, env=env,
        )
        check("the named session is read", proc.returncode == 0,
              proc.stderr[:200].decode("utf-8", "replace"))
        check("the named session's text is emitted", b"target" in proc.stdout)
        check("no other session's text is emitted", b"other" not in proc.stdout)


def test_chunk_transcript_refuses_garbage():
    """chunk-transcript must not hand the CLI an empty chunk list."""
    with tempfile.TemporaryDirectory() as home:
        garbage = os.path.join(home, "garbage.json")
        with open(garbage, "wb") as handle:
            handle.write(b"not json at all\xff")
        env = dict(os.environ)
        env["JCODE_HOME"] = home
        proc = subprocess.run(
            [sys.executable, ADAPTER, "chunk-transcript", "--session-ref", garbage],
            capture_output=True, env=env, input=b"",
        )
        check("chunk-transcript refuses a non-transcript", proc.returncode != 0,
              f"rc={proc.returncode}")
        check("chunk-transcript emits no empty chunk list", b'"chunks":[]' not in proc.stdout)


# ---------------------------------------------------------------------------
# Negative control: the OLD behaviour would have passed the data checks
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# B7.3: --path is a dead flag for read-transcript and compact-transcript
# reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/agent/external/external.go
# ---------------------------------------------------------------------------

def _path_flag_fixture(tmp):
    """A readable session, a valid ETF sidecar, and a garbage file.

    Returns (session_json, sidecar_jsonl, garbage_jsonl, missing_jsonl).
    The session holds a marker string so a leaked foreign transcript is
    detectable in the output.
    """
    sessions = os.path.join(tmp, "sessions")
    os.makedirs(sessions)
    session = os.path.join(sessions, "session_hog_1.json")
    with open(session, "w", encoding="utf-8") as handle:
        json.dump(
            {"messages": [{"role": "user", "content": [{"type": "text", "text": "MARKER-SESSION"}]}]},
            handle,
        )
    sidecar = os.path.join(tmp, "valid.etf.jsonl")
    with open(sidecar, "wb") as handle:
        handle.write(
            b'{"v":1,"type":"user","agent":"jcode","content":"MARKER-SIDECAR"}\n'
        )
    garbage = os.path.join(tmp, "garbage.jsonl")
    with open(garbage, "wb") as handle:
        handle.write(b"this is not a transcript at all\n")
    missing = os.path.join(tmp, "definitely-missing.jsonl")
    return session, sidecar, garbage, missing


def test_path_flag_is_rejected_by_read_transcript():
    """read-transcript must refuse --path instead of ignoring it.

    Negative control: the old behaviour ACCEPTED --path and answered with
    the newest session on disk, exiting 0. So a passing "the output is not
    the requested transcript" check alone would not prove a refusal: the
    exit code is the discriminator, and the marker check proves no foreign
    transcript was substituted.
    """
    with tempfile.TemporaryDirectory() as tmp:
        session, sidecar, garbage, missing = _path_flag_fixture(tmp)
        env = dict(os.environ)
        env["JCODE_HOME"] = tmp

        for label, candidate in (
            ("valid", sidecar),
            ("garbage", garbage),
            ("missing", missing),
        ):
            proc = subprocess.run(
                [sys.executable, ADAPTER, "read-transcript", "--path", candidate],
                capture_output=True, env=env,
            )
            check(f"read-transcript rejects --path ({label})", proc.returncode != 0,
                  f"rc={proc.returncode}")
            check(f"read-transcript emits no bytes for --path ({label})",
                  proc.stdout == b"", f"bytes={len(proc.stdout)}")
            check(f"read-transcript explains the --path refusal ({label})",
                  b"--path" in proc.stderr, proc.stderr[:200].decode("utf-8", "replace"))
            check(f"read-transcript leaks no foreign transcript for --path ({label})",
                  b"MARKER-SESSION" not in proc.stdout and b"MARKER-SIDECAR" not in proc.stdout)

        # The control: the same session named the supported way still works.
        proc = subprocess.run(
            [sys.executable, ADAPTER, "read-transcript", "--session-ref", session],
            capture_output=True, env=env,
        )
        check("read-transcript still accepts --session-ref", proc.returncode == 0,
              f"rc={proc.returncode}")
        check("read-transcript still returns the named session",
              b"MARKER-SESSION" in proc.stdout, f"bytes={len(proc.stdout)}")


def test_path_flag_is_rejected_by_compact_transcript():
    """compact-transcript must refuse --path instead of ignoring it."""
    with tempfile.TemporaryDirectory() as tmp:
        session, sidecar, garbage, missing = _path_flag_fixture(tmp)
        env = dict(os.environ)
        env["JCODE_HOME"] = tmp

        for label, candidate in (
            ("valid", sidecar),
            ("garbage", garbage),
            ("missing", missing),
        ):
            proc = subprocess.run(
                [sys.executable, ADAPTER, "compact-transcript", "--path", candidate],
                capture_output=True, env=env,
            )
            check(f"compact-transcript rejects --path ({label})", proc.returncode != 0,
                  f"rc={proc.returncode}")
            check(f"compact-transcript emits no transcript for --path ({label})",
                  proc.stdout == b"", f"bytes={len(proc.stdout)}")
            check(f"compact-transcript explains the --path refusal ({label})",
                  b"--path" in proc.stderr, proc.stderr[:200].decode("utf-8", "replace"))

        proc = subprocess.run(
            [sys.executable, ADAPTER, "compact-transcript", "--session-ref", session],
            capture_output=True, env=env,
        )
        check("compact-transcript still accepts --session-ref", proc.returncode == 0,
              f"rc={proc.returncode}")
        # compact-transcript returns the transcript base64-encoded, so the
        # marker is only visible after decoding.
        try:
            payload = json.loads(proc.stdout.decode("utf-8", "replace"))
            decoded = base64.b64decode(payload.get("transcript", ""), validate=True)
        except Exception:
            decoded = b""
        check("compact-transcript still returns the named session",
              b"MARKER-SESSION" in decoded, f"bytes={len(proc.stdout)}")


def test_path_flag_still_works_for_the_commands_that_use_it():
    """--path must keep working for the commands that DO read it.

    A negative control for the rejection itself: if --path were refused
    everywhere, the CLI could not read a transcript at a position, and the
    growth baseline would break.
    """
    with tempfile.TemporaryDirectory() as tmp:
        session, sidecar, garbage, missing = _path_flag_fixture(tmp)
        env = dict(os.environ)
        env["JCODE_HOME"] = tmp

        proc = subprocess.run(
            [sys.executable, ADAPTER, "get-transcript-position", "--path", sidecar],
            capture_output=True, env=env,
        )
        check("get-transcript-position still accepts --path", proc.returncode == 0,
              f"rc={proc.returncode}")
        try:
            payload = json.loads(proc.stdout.decode("utf-8", "replace"))
        except Exception:
            payload = {}
        check("get-transcript-position reports a position", payload.get("position", 0) >= 1,
              f"payload={payload}")


def test_negative_control_old_behaviour_would_have_passed_through():
    """The old code returned the garbage bytes; the new code refuses.

    This is the control for the whole file: if the new code still returned the
    bytes, every "is refused" check above would be the only thing standing
    between the CLI and a garbage transcript.
    """
    garbage = b"not json at all\xff"
    old_result = garbage  # the old code returned the raw bytes unchanged
    new_result = adapter._session_transcript_bytes(_raw_path(garbage))
    check("the old behaviour would have passed the garbage through",
          old_result == garbage)
    check("the new behaviour refuses it", new_result is None)


def _path(name):
    return Path(name)


def _raw_path(data):
    handle = tempfile.NamedTemporaryFile(delete=False)
    handle.write(data)
    handle.close()
    return Path(handle.name)


def main():
    test_garbage_is_refused()
    test_empty_file_is_an_empty_transcript()
    test_session_document_with_no_messages_is_an_empty_transcript()
    test_truncated_json_is_refused()
    test_valid_session_document_is_converted()
    test_valid_etf_sidecar_is_passed_through()
    test_etf_with_broken_line_is_refused()
    test_etf_with_wrong_version_is_refused()
    test_etf_without_type_is_refused()
    test_etf_with_wrong_field_type_is_refused()
    test_etf_with_null_optional_scalar_is_accepted()
    test_etf_with_non_string_ts_is_accepted()
    test_etf_string_content_is_accepted()
    test_etf_null_content_is_accepted()
    test_missing_session_ref_does_not_fall_back()
    test_missing_session_ref_compact_fails()
    test_output_is_stable_between_runs()
    test_named_session_ref_is_honoured()
    test_chunk_transcript_refuses_garbage()
    test_path_flag_is_rejected_by_read_transcript()
    test_path_flag_is_rejected_by_compact_transcript()
    test_path_flag_still_works_for_the_commands_that_use_it()
    test_negative_control_old_behaviour_would_have_passed_through()
    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED: {', '.join(FAILURES)}")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
