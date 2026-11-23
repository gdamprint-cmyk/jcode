#!/usr/bin/env python3
# ---------------------------------------------------------------------------
# Ported into the jcode repository from the Entire external-agent adapter.
# source: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
# source repo: entire-agent-jcode @ 88fd2c7
#                ("fix(adapter): the stdout cap is 10 MiB for read-transcript, 67.7 MiB for compact-transcript")
# ---------------------------------------------------------------------------
# reference: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
# reference: https://docs.entire.io/agents/agent-integration-protocol/commands
# reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/agent/external/types.go
# reference: https://raw.githubusercontent.com/entireio/cli/main/transcript/README.md
"""entire-agent-jcode — External Agent Protocol adapter for Jcode.

Implements the Entire Agent Integration Protocol so that Jcode sessions are
captured as Entire sessions + checkpoints. Drop this binary (named
``entire-agent-jcode``) on PATH and run ``entire agent add jcode``.

Structure mirrors the working ``entire-agent-hermes`` adapter (same
subcommands, same JSON shapes, same argparse layout); the data source differs:

* Hermes reads a SQLite ``state.db`` under ``HERMES_HOME``.
* Jcode stores one JSON document per session under
  ``<JCODE_HOME>/sessions/session_<name>_<ts>_<hash>.json``, with the message
  list in ``messages`` and per-message ``token_usage``.

Stdlib only — no external dependencies.

Subcommands (per the Agent Integration Protocol):
  info, detect, get-session-id, get-session-dir, resolve-session-file,
  read-session, write-session, read-transcript, chunk-transcript,
  reassemble-transcript, format-resume-command,
  install-hooks, uninstall-hooks, are-hooks-installed, parse-hook,
  get-transcript-position, extract-modified-files, extract-prompts,
  extract-summary, calculate-tokens, prepare-transcript.
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import sys
from pathlib import Path

AGENT_NAME = "jcode"

# The Entire CLI caps an external agent's stdout at 10 MiB and aborts with
# "output exceeded 10485760 byte limit" when a command exceeds it
# (cmd/entire/cli/agent/external/external.go:530). The cap is hardcoded: no
# environment variable and no CLI flag overrides it.
#
# ONE EXCEPTION: compact-transcript gets a much higher cap -
# base64.StdEncoding.EncodedLen(agent.MaxChunkSize) + 1 MiB = 70 953 644 bytes
# (67.7 MiB), enough for a 50 MiB raw transcript as base64
# (external.go:531-534). This guard therefore applies to read-transcript ONLY;
# compact-transcript must not be refused, and it is not. Measured on this host:
# a 28.5 MB session yields 38 022 402 bytes of compact-transcript output,
# which is over the 10 MiB cap and under the 67.7 MiB one.
#
# The adapter uses this constant to REFUSE rather than truncate a transcript
# that would exceed the cap.
#
# WHY THE REFUSAL EXISTS, MEASURED (not assumed):
#   The CLI's limitedWriter keeps the first `limit` bytes and sets its
#   `truncated` flag; external.go:550-552 then returns
#   "output exceeded %d byte limit" INSTEAD of the bytes. So the CLI never
#   stores a truncated transcript: cmd/entire/cli/lifecycle.go:870-875 gets an
#   error and returns before the sidecar write at lifecycle.go:882. A
#   truncated sidecar is therefore NOT possible, and the earlier rationale
#   ("a truncated answer would overwrite a complete stored copy") was wrong.
#
#   What the refusal actually buys:
#     1. A SPECIFIC error instead of a generic one. The CLI's own message
#        names only the limit; the adapter's names the transcript's ACTUAL
#        size, so the cause is visible without re-measuring.
#     2. A clear refusal instead of a partial stdout the caller might read.
#        The CLI discards the bytes either way, but a direct consumer of the
#        adapter's stdout that did NOT set the CLI marker still gets the
#        complete transcript (see _called_by_cli).
#     3. The stored sidecar stays at its previous COMPLETE contents, so the
#        growth baseline (SessionState.CheckpointTranscriptSize, compared in
#        the sanitized coordinate) keeps measuring a complete transcript.
#        Measured on this host: 0 of 417 stored sidecars sit at or within 2%
#        of the cap (max 9 923 467 B), which is consistent with "the read
#        either succeeds whole or fails whole".
#
# reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/agent/external/external.go
# reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/lifecycle.go
_CLI_STDOUT_LIMIT_BYTES = 10 * 1024 * 1024

# The CLI sets ENTIRE_CLI_VERSION on every plugin subprocess it runs
# (cmd/entire/cli/agent/external/external.go:511). That is how the adapter
# tells "I am answering the CLI's capped pipe" apart from "I am being run
# directly by a tool that has no such cap".
#
# The distinction matters: the 10 MiB limit is a property of the CLI's
# stdout pipe, not of the transcript. A direct caller (the ETF schema
# checker, a human running the adapter by hand) reads the whole stdout with
# no cap, so refusing there would deny a perfectly serviceable full
# transcript. Refusing only under the CLI keeps the cap's real constraint
# (never truncate) without denying direct callers.
_CLI_VERSION_ENV = "ENTIRE_CLI_VERSION"


def _called_by_cli() -> bool:
    """True when the parent process is the Entire CLI's capped stdout pipe."""
    # Presence, not truthiness: versioninfo.Version is "dev" for a local
    # build, which is a non-empty value and still means "the CLI is the
    # parent".
    return os.environ.get(_CLI_VERSION_ENV) is not None

# Version reported on the ``cli_version`` field of every emitted transcript
# line. It identifies the producer of the line, not the agent.
CLI_VERSION = "0.13.1"

# Jcode lifecycle hook names this adapter handles. Entire calls
# ``entire hooks jcode <name>``, which forwards the raw payload to
# ``parse-hook --hook <name>``.
HOOK_NAMES = ["session-start", "turn-start", "turn-end", "session-end"]

# Jcode hook event -> Entire Event type.
#   1 SessionStart, 2 TurnStart, 3 TurnEnd, 5 SessionEnd
_HOOK_TYPE_MAP = {
    "session-start": 1,
    "session_start": 1,
    "turn-start": 2,
    "turn_start": 2,
    "user_prompt_submit": 2,
    "turn-end": 3,
    "turn_end": 3,
    "stop": 3,
    "session-end": 5,
    "session_end": 5,
}

# Tools whose invocation names a file Jcode may have modified.
_FILE_TOOLS = {
    "write",
    "edit",
    "multiedit",
    "apply_patch",
    "notebookedit",
    "notebook_edit",
    "create_file",
    "str_replace_editor",
}


# ---------------------------------------------------------------------------
# Configuration helpers
# ---------------------------------------------------------------------------

def _jcode_home() -> Path:
    """Root of the Jcode state directory (~/.jcode by default)."""
    home = os.environ.get("JCODE_HOME")
    if home:
        return Path(home)
    user = os.environ.get("USERPROFILE") or os.environ.get("HOME") or ""
    return Path(user) / ".jcode"


def _sessions_dir() -> Path:
    return _jcode_home() / "sessions"


def _session_file(session_id: str) -> Path:
    return _sessions_dir() / f"{session_id}.json"


def _stdin_bytes() -> bytes:
    """Raw stdin bytes, or b"" when stdin is an interactive terminal.

    The protocol always pipes HookInput in, so a pipe is read to EOF. An
    interactive terminal would block forever, so it is never read.
    """
    try:
        if sys.stdin is None or sys.stdin.isatty():
            return b""
    except Exception:
        return b""
    try:
        return sys.stdin.buffer.read()
    except Exception:
        return b""


def _read_stdin() -> str:
    try:
        return _stdin_bytes().decode("utf-8", errors="replace")
    except Exception:
        return ""


def _read_stdin_bytes() -> bytes:
    return _stdin_bytes()


def _load_json(path: Path):
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


def _session_ids() -> list[str]:
    directory = _sessions_dir()
    if not directory.is_dir():
        return []
    return [entry.stem for entry in directory.glob("session_*.json")]


def _session_mtime(session_id: str) -> float:
    try:
        return _session_file(session_id).stat().st_mtime
    except OSError:
        return 0.0


def _metadata_db() -> Path:
    return _jcode_home() / "session-metadata-v1.sqlite3"


def _session_ids_by_workdir(workdir: str) -> list[str]:
    """Session ids for a working directory, newest first, from the metadata index.

    Jcode keeps a SQLite index of recent sessions; scanning every session JSON
    on disk is far too slow for a hook that runs on every turn.
    """
    db = _metadata_db()
    if not db.exists():
        return []
    try:
        import sqlite3

        conn = sqlite3.connect(f"file:{db.as_posix()}?mode=ro", uri=True, timeout=2.0)
        try:
            rows = conn.execute(
                "SELECT session_id, updated_at_ms FROM recent_sessions "
                "WHERE LOWER(working_dir) = LOWER(?) ORDER BY updated_at_ms DESC",
                (workdir,),
            ).fetchall()
        finally:
            conn.close()
    except Exception:
        return []
    return [row[0] for row in rows]


def _find_session(session_id: str | None = None, workdir: str | None = None) -> str | None:
    """Best-effort session lookup: explicit id first, then newest for a workdir."""
    if session_id:
        return session_id if _session_file(session_id).exists() else None
    if workdir:
        for sid in _session_ids_by_workdir(workdir):
            if _session_file(sid).exists():
                return sid
        return None
    try:
        candidates = _session_ids()
    except Exception:
        return None
    if not candidates:
        return None
    candidates.sort(key=_session_mtime, reverse=True)
    return candidates[0]


def _hook_input() -> dict:
    raw = _read_stdin().strip()
    if not raw:
        return {}
    try:
        value = json.loads(raw)
    except json.JSONDecodeError:
        return {}
    return value if isinstance(value, dict) else {}


def _text_of(content) -> str:
    """Flatten Jcode message content blocks into plain text."""
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    parts = []
    for block in content:
        if not isinstance(block, dict):
            continue
        if block.get("type") == "text" and isinstance(block.get("text"), str):
            parts.append(block["text"])
    return "\n".join(parts)


# ---------------------------------------------------------------------------
# Entire Transcript Format v1
# ---------------------------------------------------------------------------
# reference: https://raw.githubusercontent.com/entireio/cli/main/transcript/README.md
#
# Jcode stores a session as ONE JSON document whose ``messages`` array holds
# every turn. Entire and the Brain read transcripts line by line, so a session
# document is not a transcript: emitting it verbatim produces a single line of
# several megabytes, which the readers reject outright ("bufio.Scanner: token
# too long", a 1 MiB line cap in the Brain history scanner).
#
# The fix is to emit the Entire Transcript Format v1: one JSON object per line,
# each line terminated by "\n". The format is specified in ``transcript/README.md``
# of the entireio/cli repository:
#
#   v            int, required, always 1
#   type         string, required, "user" or "assistant"
#   agent        string, optional, the producing agent
#   cli_version  string, optional, the producer version
#   ts           string, optional, RFC 3339
#   id           string, optional, the agent's own message id
#   content      array of blocks, string or null, required
#
# Blocks: user text is ``{"text": ...}`` with NO ``type`` field (a v1 quirk);
# assistant text is ``{"type": "text", "text": ...}``; a tool call is
# ``{"type": "tool_use", "id": ..., "name": ..., "input": ...}`` with the
# result inlined as ``result: {"output": ..., "status": "success"|"error"}``;
# thinking ("reasoning") blocks are dropped; any other block type passes through.
#
# A reader decodes such a line as-is: "input whose first non-blank line is a
# JSON object with an integer v and a non-empty string type is decoded as is
# and the options are ignored".

# Jcode block type names this converter recognises.
_JCODE_TEXT = "text"
_JCODE_REASONING = "reasoning"
_JCODE_TOOL_USE = "tool_use"
_JCODE_TOOL_RESULT = "tool_result"


# The Brain indexes a transcript line by line and drops the WHOLE session when
# a single line exceeds its scanner buffer:
#   internal/cli/history.go:33      historyMaxLineBytes = 1024 * 1024
#   internal/cli/history.go:1843    scanner.Buffer(..., historyMaxLineBytes)
#   internal/cli/conversation.go:210 scanner.Buffer(..., historyMaxLineBytes)
# A tool result is inlined into the tool_use block, so one long command output
# produced one oversized line and silently removed the entire session from the
# history and conversation indexes (measured 2026-11-08: 11 lines over the cap
# across the backfilled checkpoints).
# reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go
_MAX_LINE_BYTES = 512 * 1024

# How much of a tool result is kept. The cap is well below the Brain line
# limit so that the line survives, and a truncated result is still useful
# context: the head of a command output carries the signal.
_MAX_TOOL_OUTPUT_BYTES = 256 * 1024

_TRUNCATION_MARKER = "\n[... truncated by entire-agent-jcode ...]"

# The truncation marker for a base64 payload. It is deliberately NOT the text
# marker: a marker appended to a base64 string makes the field undecodable
# (the base64 length stops being a multiple of 4 and the padding breaks), so
# the marker for a payload lives in its own field instead.
#
# reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go
_TRUNCATION_MARKER_B64 = "[... truncated by entire-agent-jcode ...]"


def _truncate_bytes(text: str, limit: int) -> str:
    """Cut text to at most limit bytes without splitting a UTF-8 character."""
    if len(text.encode("utf-8")) <= limit:
        return text
    encoded = text.encode("utf-8")[:limit]
    return encoded.decode("utf-8", errors="ignore")


def _truncate_base64(data: str, limit: int) -> str:
    """Cut a base64 payload to at most limit bytes, still DECODABLE.

    A base64 string encodes 3 bytes into 4 characters, so a payload cut at an
    arbitrary offset does not decode: its length is no longer a multiple of 4
    and the decoder rejects it with "Incorrect padding". The cut is therefore
    made on a 4-character boundary.

    The result is a valid base64 PREFIX of the original, never a corrupted
    copy of it. No padding is appended: the cut already lands on a boundary
    of 4 characters, and adding "=" would make the length a NON-multiple of
    4 again (4k + 1, 4k + 2 or 4k + 3), which is undecodable in exactly the
    same way.

    A budget too small for even one base64 quad (4 characters) still keeps
    the FIRST QUAD rather than emptying the field. Returning "" here would
    lose the image entirely, which is the data loss this function exists to
    prevent; measured 2026-11-08: a valid 4-character payload next to a
    900 KB text was emptied by the proportional share rounding down to zero.

    reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go
    """
    if len(data.encode("utf-8")) <= limit:
        return data
    # The base64 alphabet is ASCII, so one character is one byte.
    chars = limit // 4 * 4
    if chars <= 0:
        # Below one quad there is no decodable cut. Keep the first quad so
        # the field stays readable instead of becoming empty.
        return data[:4]
    return data[:chars]


def _etf_tool_result(block: dict) -> dict:
    """The ETF ``result`` object for a Jcode tool_result block."""
    content = block.get("content")
    if isinstance(content, str):
        output = content
    elif isinstance(content, list):
        parts = []
        for part in content:
            if isinstance(part, dict):
                text = part.get("text")
                if isinstance(text, str):
                    parts.append(text)
            elif isinstance(part, str):
                parts.append(part)
        output = "\n".join(parts)
    elif content is None:
        output = ""
    else:
        output = json.dumps(content, ensure_ascii=False)
    truncated = _truncate_bytes(output, _MAX_TOOL_OUTPUT_BYTES)
    if truncated != output:
        output = truncated + _TRUNCATION_MARKER
    return {"output": output, "status": "error" if block.get("is_error") else "success"}


def _etf_lines(data: dict) -> list[dict]:
    """Convert a Jcode session document into Entire Transcript Format v1 lines.

    Returns the line objects in message order. A line is one JSON object; the
    caller serialises them one per line.

    Jcode keeps a tool call and its result in SEPARATE messages (the call in
    an assistant message, the result in a later user message), while ETF
    inlines the result into the tool_use block that requested it. So the
    results are collected first and attached to the matching call by id.
    """
    messages = data.get("messages")
    if not isinstance(messages, list):
        return []

    # First pass: collect every tool result, keyed by the call it answers.
    results: dict[str, dict] = {}
    for message in messages:
        if not isinstance(message, dict):
            continue
        content = message.get("content")
        if not isinstance(content, list):
            continue
        for block in content:
            if not isinstance(block, dict) or block.get("type") != _JCODE_TOOL_RESULT:
                continue
            call_id = block.get("tool_use_id")
            if isinstance(call_id, str) and call_id:
                results[call_id] = _etf_tool_result(block)

    # Second pass: build the lines.
    lines: list[dict] = []
    for message in messages:
        if not isinstance(message, dict):
            continue
        role = message.get("role")
        if role not in ("user", "assistant"):
            continue
        content = message.get("content")
        if isinstance(content, str):
            blocks: list[dict] = [
                {"text": content} if role == "user" else {"type": _JCODE_TEXT, "text": content}
            ]
        elif isinstance(content, list):
            blocks = []
            for block in content:
                if not isinstance(block, dict):
                    continue
                kind = block.get("type")
                if kind == _JCODE_REASONING:
                    # Thinking blocks are dropped by the ETF converters.
                    continue
                if kind == _JCODE_TOOL_RESULT:
                    # Already inlined into the matching tool_use block.
                    continue
                if kind == _JCODE_TEXT:
                    text = block.get("text")
                    if not isinstance(text, str):
                        continue
                    blocks.append({"text": text} if role == "user" else {"type": _JCODE_TEXT, "text": text})
                    continue
                if kind == _JCODE_TOOL_USE:
                    tool_use = {
                        "type": _JCODE_TOOL_USE,
                        "name": block.get("name"),
                        "input": block.get("input"),
                    }
                    call_id = block.get("id")
                    if isinstance(call_id, str):
                        tool_use["id"] = call_id
                    result = results.get(call_id) if isinstance(call_id, str) else None
                    if result is not None:
                        tool_use["result"] = result
                    blocks.append(tool_use)
                    continue
                # Any other block type passes through verbatim.
                blocks.append(block)
        else:
            blocks = []

        if not blocks:
            # A user message that carried only tool results produces nothing
            # of its own; its content already rides on the tool_use blocks.
            continue

        line = {
            "v": 1,
            "type": role,
            "agent": AGENT_NAME,
            "cli_version": CLI_VERSION,
            "content": blocks,
        }
        message_id = message.get("id")
        if isinstance(message_id, str) and message_id:
            line["id"] = message_id
        timestamp = message.get("timestamp")
        if isinstance(timestamp, str) and timestamp:
            line["ts"] = timestamp
        if role == "assistant":
            usage = message.get("token_usage")
            if isinstance(usage, dict):
                for source, target in (
                    ("input_tokens", "input_tokens"),
                    ("output_tokens", "output_tokens"),
                    ("cache_read_input_tokens", "cache_read_tokens"),
                    ("cache_creation_input_tokens", "cache_write_tokens"),
                ):
                    value = usage.get(source)
                    if isinstance(value, int) and value:
                        line[target] = value
        lines.append(line)
    return lines


def _etf_jsonl(data: dict) -> bytes:
    """The ETF v1 transcript of a Jcode session document, as JSONL bytes.

    Every line is kept under ``_MAX_LINE_BYTES`` so that the Brain's line
    scanner never rejects it. A line that is still too long after the tool
    result was truncated has its text blocks trimmed.
    """
    out = bytearray()
    for line in _etf_lines(data):
        encoded = _encode_etf_line(line)
        if len(encoded) > _MAX_LINE_BYTES:
            line = _shrink_etf_line(line, len(encoded))
            encoded = _encode_etf_line(line)
        out += encoded
        out += b"\n"
    return bytes(out)


# reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go
def _encode_etf_line(line: dict) -> bytes:
    """One ETF line as the JSON bytes that go on the wire."""
    return json.dumps(line, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


# reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go
def _shrink_etf_line(line: dict, encoded_len: int) -> dict:
    """Trim an ETF line that is over the Brain line budget.

    # reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go
    The trimmable parts of a line are the block text, the tool result output
    and the base64 payloads (both the direct "data" field and the nested
    "source.data"). Every one of them is a candidate for trimming.

    The line is measured after every round with the real encoder, and the
    biggest part is cut by the overshoot, until the encoded line fits. The
    measurement is what decides, not a hand-computed budget: a hand-computed
    budget is wrong whenever a part carries bytes that were not accounted
    for, and the result is a line that is still over the limit (the Brain
    then drops the whole session) or a payload that was cut to nothing.

    reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go
    """
    blocks = line.get("content")
    if not isinstance(blocks, list):
        return line
    work = json.loads(json.dumps(blocks))
    targets: list[dict] = []
    for block in work:
        if not isinstance(block, dict):
            continue
        if isinstance(block.get("text"), str):
            targets.append({"block": block, "path": ("text",), "size": len(block["text"].encode("utf-8")), "payload": False})
        result = block.get("result")
        if isinstance(result, dict) and isinstance(result.get("output"), str):
            # The tool output has its own cap, independent of the line budget.
            targets.append({"block": block, "path": ("result", "output"), "size": len(result["output"].encode("utf-8")), "payload": False, "cap": _MAX_TOOL_OUTPUT_BYTES})
        if isinstance(block.get("data"), str):
            targets.append({"block": block, "path": ("data",), "size": len(block["data"].encode("utf-8")), "payload": True})
        source = block.get("source")
        if isinstance(source, dict) and isinstance(source.get("data"), str):
            targets.append({"block": block, "path": ("source", "data"), "size": len(source["data"].encode("utf-8")), "payload": True})
    if not targets:
        return line

    def _read(target: dict) -> str:
        value = target["block"]
        for key in target["path"]:
            value = value[key]
        return value

    def _write(target: dict, value: str) -> None:
        holder = target["block"]
        for key in target["path"][:-1]:
            holder = holder[key]
        holder[target["path"][-1]] = value

    marker_len = len(_TRUNCATION_MARKER.encode("utf-8"))
    b64_marker_len = len(_TRUNCATION_MARKER_B64.encode("utf-8"))
    out = dict(line)
    out["content"] = work
    # A part with its own cap is trimmed down to that cap first, so it never
    # grows back over it while the line budget is being enforced.
    for target in targets:
        cap = target.get("cap")
        if cap is not None and target["size"] > cap:
            if target["payload"]:
                trimmed = _truncate_base64(_read(target), max(0, cap - b64_marker_len))
            else:
                trimmed = _truncate_bytes(_read(target), max(0, cap - marker_len))
            target["size"] = len(trimmed.encode("utf-8"))
            _write(target, trimmed)
            if target["payload"]:
                if target["block"].get("truncated") is not True:
                    target["block"]["truncated"] = True
            else:
                _write(target, trimmed + _TRUNCATION_MARKER)
    for _ in range(256):
        encoded = _encode_etf_line(out)
        if len(encoded) <= _MAX_LINE_BYTES:
            return out
        overshoot = len(encoded) - _MAX_LINE_BYTES
        candidates = [t for t in targets if t["size"] > 0]
        if not candidates:
            break
        # The cut is shared PROPORTIONALLY between the parts, not taken from
        # the biggest one alone. Taking it from the biggest one alone would
        # leave the other parts whole and cut one payload down to nothing,
        # which is the data loss this whole function exists to prevent.
        total = sum(t["size"] for t in candidates)
        need = overshoot + marker_len
        for target in candidates:
            share = -(-target["size"] * need // total)  # ceil division
            if target["payload"]:
                # A base64 payload carries NO text marker. Appending one makes
                # the field undecodable: the base64 length stops being a
                # multiple of 4 and the decoder rejects it with "Incorrect
                # padding". The truncation is reported in the block's own
                # "truncated" flag instead, and the payload stays a valid,
                # decodable prefix of the original.
                # reference: https://github.com/entireio/entire-brain/blob/main/internal/cli/history.go
                allowance = max(0, target["size"] - share)
                current = _read(target)
                trimmed = _truncate_base64(current, allowance)
                target["size"] = len(trimmed.encode("utf-8"))
                if trimmed != current:
                    _write(target, trimmed)
                    if target["block"].get("truncated") is not True:
                        target["block"]["truncated"] = True
                else:
                    _write(target, trimmed)
            else:
                allowance = max(0, target["size"] - share - marker_len)
                current = _read(target)
                trimmed = _truncate_bytes(current, allowance)
                target["size"] = len(trimmed.encode("utf-8"))
                if trimmed != current:
                    _write(target, trimmed + _TRUNCATION_MARKER)
                else:
                    _write(target, trimmed)
    return out


# ---------------------------------------------------------------------------
# Materialised transcript sidecar
# ---------------------------------------------------------------------------
# reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/strategy/manual_commit_hooks.go
# reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/strategy/manual_commit_condensation.go
#
# The Entire CLI does not read a Jcode session through ``read-transcript`` when
# it writes a checkpoint: it reads ``state.TranscriptPath`` straight off disk
# (agent.ReadTranscriptFile). That path is whatever the hook event reported as
# ``session_ref``, so the ETF transcript has to live in a FILE the adapter
# points at, not only in a command's stdout.
#
# The sidecar is written next to the session store, inside it, so the CLI's
# session-store containment check (the reference must resolve inside the
# agent's session directory) accepts it.

_SIDECAR_DIR = ".entire"


# reference: https://raw.githubusercontent.com/entireio/cli/main/transcript/README.md
def _session_transcript_bytes(path: Path) -> bytes | None:
    """The transcript bytes for a session path.

    A Jcode session is one JSON document, so it is converted into ETF v1
    JSONL. A file that already holds a valid ETF v1 transcript is returned
    unchanged. Anything else is NOT a transcript, and None is returned: the
    caller must refuse rather than hand the CLI bytes that only look like a
    transcript. A file that is neither a session document nor a valid ETF v1
    JSONL used to pass through unchanged and was written into the checkpoint
    as the session's transcript.

    Returns None for a file that cannot be read as well.
    """
    try:
        raw = path.read_bytes()
    except OSError:
        return None
    document = _read_session_document_bytes(raw)
    if document is not None:
        return _etf_jsonl(document)
    if _is_etf_v1_jsonl(raw):
        return raw
    return None


# reference: https://raw.githubusercontent.com/entireio/cli/main/transcript/README.md
def _is_etf_v1_jsonl(raw: bytes) -> bool:
    """True when raw is a valid Entire Transcript Format v1 transcript.

    The format is specified in ``transcript/README.md`` of the entireio/cli
    repository: one JSON object per line, ``v`` is the integer 1, ``type`` is
    a string, and a known field of the wrong JSON type makes the reader skip
    the line. A transcript is valid only when EVERY line is valid: a file
    that holds one broken line among good ones is not a transcript either.

    An empty file is an EMPTY transcript, not an invalid one: a session
    document with no messages converts to zero bytes the same way, so
    refusing the empty file would refuse a legitimate transcript and
    disagree with the session-document path. The upstream reader returns
    zero lines for it and keeps the line.
    """
    for raw_line in raw.split(b"\n"):
        line = raw_line.strip()
        if not line:
            # Blank lines are ignored by the reader, but they are counted in
            # the line numbers it reports.
            continue
        try:
            value = json.loads(line.decode("utf-8"))
        except Exception:
            return False
        if not isinstance(value, dict):
            return False
        version = value.get("v")
        if isinstance(version, bool) or not isinstance(version, int) or version != 1:
            return False
        if not isinstance(value.get("type"), str):
            return False
        # A known field of the wrong JSON type makes the reader skip the line.
        # A null optional scalar is treated as absence, not as a wrong type.
        for key in ("agent", "cli_version", "id", "model"):
            field = value.get(key)
            if field is not None and not isinstance(field, str):
                return False
        for key in (
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "cache_write_5m_tokens",
            "cache_write_1h_tokens",
            "reasoning_tokens",
        ):
            field = value.get(key)
            if field is None:
                continue
            if isinstance(field, bool) or not isinstance(field, int):
                return False
        # ``ts`` is deliberately not checked: the reader ignores a ``ts`` of
        # the wrong type instead of skipping the line.
    return True


def _read_session_document_bytes(raw: bytes) -> dict | None:
    """The session document in raw, or None when raw is not one.

    A session document is ONE JSON object whose ``messages`` array holds the
    turns. An ETF v1 sidecar is JSONL: its first line is also a JSON object,
    so the whole-payload parse is what distinguishes the two.
    """
    stripped = raw.lstrip()
    if not stripped.startswith(b"{"):
        return None
    try:
        value = json.loads(stripped.decode("utf-8", errors="replace"))
    except Exception:
        return None
    if isinstance(value, dict) and isinstance(value.get("messages"), list):
        return value
    return None


def _sidecar_path(session_id: str) -> Path:
    """Where the ETF transcript of a session lives."""
    return _sessions_dir() / _SIDECAR_DIR / f"{session_id}.jsonl"


def _sidecar_meta_path(session_id: str) -> Path:
    """Where the sidecar's provenance (source mtime and size) is recorded."""
    return _sessions_dir() / _SIDECAR_DIR / f"{session_id}.jsonl.meta"


def _materialize_transcript(session_id: str) -> Path | None:
    """Write the ETF v1 transcript of a session to its sidecar.

    The sidecar is rewritten only when the session document changed, so a hook
    that fires on every turn does not re-serialize a large session each time.

    Returns the sidecar path, or None when the transcript could not be
    materialized (the caller then keeps using the session document).
    """
    source = _session_file(session_id)
    if not source.exists():
        return None
    try:
        source_stat = source.stat()
    except OSError:
        return None

    target = _sidecar_path(session_id)
    meta_path = _sidecar_meta_path(session_id)
    try:
        if target.exists() and meta_path.exists():
            meta = json.loads(meta_path.read_text(encoding="utf-8"))
            fresh = (
                meta.get("source_mtime") == source_stat.st_mtime
                and meta.get("source_size") == source_stat.st_size
            )
            if fresh:
                return target
    except Exception:
        pass

    data = _session_transcript_bytes(source)
    if data is None:
        return None
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        temp = target.with_suffix(".jsonl.tmp")
        temp.write_bytes(data)
        temp.replace(target)
        meta_path.write_text(
            json.dumps({"source_mtime": source_stat.st_mtime, "source_size": source_stat.st_size}),
            encoding="utf-8",
        )
    except Exception as exc:  # noqa: BLE001 - report, do not crash the hook
        sys.stderr.write(f"[entire-agent-jcode] materialize transcript: {exc}\n")
        return None
    return target


def _transcript_lines(path: Path) -> list[dict]:
    """The ETF lines of the transcript at path.

    Accepts both the sidecar (ETF v1 JSONL) and a raw session document, so the
    readers work whichever form they are handed.
    """
    try:
        data = path.read_bytes()
    except OSError:
        return []
    document = _read_session_document_bytes(data)
    if document is not None:
        return _etf_lines(document)
    lines: list[dict] = []
    for raw in data.split(b"\n"):
        raw = raw.strip()
        if not raw:
            continue
        try:
            value = json.loads(raw.decode("utf-8", errors="replace"))
        except Exception:
            continue
        if isinstance(value, dict):
            lines.append(value)
    return lines


def _line_count(path: Path) -> int:
    """The number of lines in the transcript at path.

    The offset metric the Entire CLI uses for a JSONL agent is LINES, not
    bytes: CheckpointTranscriptStart indexes the stored transcript's lines
    (see countTranscriptItems and leadingLines in the CLI). Counting bytes
    here would make every incremental checkpoint slice the wrong range.
    """
    try:
        data = path.read_bytes()
    except OSError:
        return 0
    if not data:
        return 0
    count = data.count(b"\n")
    if not data.endswith(b"\n"):
        count += 1
    return count


def _etf_line_text(line: dict) -> str:
    """The text of an ETF line's content blocks."""
    content = line.get("content")
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    parts = []
    for block in content:
        if not isinstance(block, dict):
            continue
        text = block.get("text")
        if isinstance(text, str):
            parts.append(text)
    return "\n".join(parts)


def _etf_line_blocks(line: dict) -> list[dict]:
    """The content blocks of an ETF line."""
    content = line.get("content")
    if not isinstance(content, list):
        return []
    return [block for block in content if isinstance(block, dict)]


def _is_system_reminder(text: str) -> bool:
    stripped = text.strip()
    return stripped.startswith("<system-reminder>") and stripped.endswith("</system-reminder>")


def _hook_marker(repo_path: str | None = None) -> Path:
    """Repo-scoped marker for the installed-hook state."""
    if repo_path:
        candidate = Path(repo_path) / ".entire" / "jcode-hooks.json"
        if candidate.parent.parent.is_dir():
            return candidate
    return _jcode_home() / "entire" / "jcode-hooks.json"


# ---------------------------------------------------------------------------
# Protocol subcommands
# ---------------------------------------------------------------------------

def cmd_info(args) -> int:
    info = {
        "protocol_version": 1,
        "name": AGENT_NAME,
        "type": "Jcode",
        "description": "Jcode - AI coding agent (JSON session documents under ~/.jcode/sessions)",
        "protected_dirs": [".jcode"],
        "hook_names": HOOK_NAMES,
        "capabilities": {
            "hooks": True,
            "transcript_analyzer": True,
            "transcript_preparer": False,
            "compact_transcript": True,
            "token_calculator": True,
            "text_generator": False,
            "hook_response_writer": False,
            "subagent_aware_extractor": False,
        },
    }
    print(json.dumps(info))
    return 0


def cmd_detect(args) -> int:
    present = _sessions_dir().is_dir()
    print(json.dumps({"present": bool(present)}))
    return 0


def cmd_get_session_id(args) -> int:
    payload = _hook_input()
    sid = payload.get("session_id")
    if not sid:
        repo_path = getattr(args, "repo_path", None) or getattr(args, "dir", None)
        sid = _find_session(workdir=repo_path)
    print(json.dumps({"session_id": sid or ""}))
    return 0 if sid else 1


def cmd_get_session_dir(args) -> int:
    print(json.dumps({"session_dir": str(_sessions_dir())}))
    return 0


def cmd_resolve_session_file(args) -> int:
    session_id = getattr(args, "session_id", None) or getattr(args, "session", None)
    session_dir = getattr(args, "session_dir", None)
    if session_id:
        sidecar = _materialize_transcript(session_id)
        if sidecar is not None:
            print(json.dumps({"session_file": str(sidecar)}))
            return 0
        candidate = Path(session_dir) / f"{session_id}.json" if session_dir else _session_file(session_id)
        if candidate.exists():
            print(json.dumps({"session_file": str(candidate)}))
            return 0
    print(json.dumps({"session_file": str(_session_file(session_id or ""))}))
    return 0


def cmd_read_session(args) -> int:
    payload = _hook_input()
    session_id = (
        getattr(args, "session", None)
        or getattr(args, "session_id", None)
        or payload.get("session_id")
    )
    repo_path = getattr(args, "repo_path", None) or getattr(args, "dir", None)
    if not session_id:
        session_id = _find_session(workdir=repo_path)
    if not session_id:
        return 1
    path = _session_file(session_id)
    try:
        data = _load_json(path)
    except Exception:
        return 1
    sidecar = _materialize_transcript(session_id)
    data_out = {
        "session_id": data.get("id") or session_id,
        "agent_name": AGENT_NAME,
        "repo_path": data.get("working_dir") or repo_path or os.getcwd(),
        "session_ref": str(sidecar) if sidecar is not None else str(path),
        "start_time": data.get("created_at") or "",
        "native_data": None,
        "modified_files": _modified_files(data),
        "new_files": [],
        "deleted_files": [],
    }
    print(json.dumps(data_out))
    return 0


def cmd_write_session(args) -> int:
    raw = _read_stdin()
    try:
        data = json.loads(raw) if raw.strip() else {}
    except json.JSONDecodeError:
        data = {}
    session_id = data.get("session_id") or "unknown"
    # Persist a sidecar next to the Jcode session store. The session_ref itself
    # stays inside the agent session directory, which is what Entire's
    # write-session containment check requires.
    try:
        sidecar_dir = _sessions_dir() / ".entire"
        sidecar_dir.mkdir(parents=True, exist_ok=True)
        (sidecar_dir / f"{session_id}.json").write_text(json.dumps(data), encoding="utf-8")
    except Exception as exc:  # noqa: BLE001 - report, do not crash the hook
        sys.stderr.write(f"[entire-agent-jcode] write-session: {exc}\n")
    print(json.dumps({"accepted": True, "session_id": session_id}))
    return 0


# reference: https://docs.entire.io/agents/agent-integration-protocol/commands
def cmd_read_transcript(args) -> int:
    error = _reject_dead_path_flag(args, "read-transcript")
    if error is not None:
        sys.stderr.write(error)
        return 1
    path, error = _resolve_transcript_path(args)
    if error is not None:
        sys.stderr.write(error)
        return 1
    if path is None:
        return 1
    # A Jcode session is one JSON document; the readers need one line per
    # message, so it is converted into Entire Transcript Format v1.
    data = _session_transcript_bytes(path)
    if data is None:
        sys.stderr.write("[entire-agent-jcode] read-transcript: not a transcript\n")
        return 1
    # The CLI caps an external agent's stdout at 10 MiB
    # (cmd/entire/cli/agent/external/external.go:530). A transcript above that
    # limit makes the CLI abort with "output exceeded 10485760 byte limit".
    #
    # REFUSING here rather than truncating is deliberate, but NOT for the
    # reason previously recorded. The CLI does NOT store a truncated
    # transcript: its limitedWriter flags the overflow and external.go:550-552
    # returns an error INSTEAD of the bytes, so lifecycle.go:870-875 fails
    # before the sidecar write at lifecycle.go:882. The earlier comment here
    # claimed a truncated answer would overwrite a complete stored copy with a
    # partial one; that was wrong, and the sidecar measurement agrees
    # (0 of 417 stored sidecars sit at or within 2% of the cap).
    #
    # What the refusal actually buys:
    #   - a SPECIFIC error: the adapter names the transcript's ACTUAL size,
    #     where the CLI's own message names only the limit;
    #   - a clear refusal rather than a partial stdout, for any consumer that
    #     reads the adapter directly without the CLI marker;
    #   - the stored sidecar stays at its previous COMPLETE contents, so the
    #     growth baseline keeps measuring a complete transcript.
    #
    # The refusal is scoped to the CLI's capped pipe. A direct caller has no
    # such cap, so it gets the complete transcript.
    #
    # reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/agent/external/external.go
    # reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/lifecycle.go
    if _called_by_cli() and not args.allow_oversized and len(data) > _CLI_STDOUT_LIMIT_BYTES:
        sys.stderr.write(
            "[entire-agent-jcode] read-transcript: transcript is "
            f"{len(data)} bytes, above the Entire CLI's stdout limit of "
            f"{_CLI_STDOUT_LIMIT_BYTES} bytes; refusing rather than truncating "
            "(the CLI discards an oversized reply and would report only the "
            "limit, not this transcript's actual size)\n"
        )
        return 1
    sys.stdout.buffer.write(data)
    if data and not data.endswith(b"\n"):
        sys.stdout.buffer.write(b"\n")
    return 0


# reference: https://docs.entire.io/agents/agent-integration-protocol/commands
def cmd_compact_transcript(args) -> int:
    error = _reject_dead_path_flag(args, "compact-transcript")
    if error is not None:
        sys.stderr.write(error)
        return 1
    path, error = _resolve_transcript_path(args)
    if error is not None:
        sys.stderr.write(error)
        return 1
    if path is None:
        sys.stderr.write("[entire-agent-jcode] compact-transcript: session not found\n")
        return 1
    # The CLI runs this with a 5 second budget on every checkpoint write, and
    # the input is a temp file of the already-redacted transcript, so the
    # answer must be produced from that file. A Jcode session document is
    # converted; an already-converted transcript is passed through.
    data = _session_transcript_bytes(path)
    if data is None:
        sys.stderr.write("[entire-agent-jcode] compact-transcript: session not readable\n")
        return 1
    print(json.dumps({"transcript": base64.b64encode(data).decode("ascii"), "assets": []}))
    return 0


# reference: https://docs.entire.io/agents/agent-integration-protocol/commands
def _reject_dead_path_flag(args, command: str) -> str | None:
    """An error message when ``--path`` was given to a command that has no
    use for it, and None otherwise.

    ``--path`` names a transcript file for the commands that read a
    transcript AT A POSITION: get-transcript-position, extract-modified-files
    and extract-prompts. The CLI never passes it to read-transcript or
    compact-transcript, which are named with ``--session-ref``.

    The flag used to be accepted and ignored by these two commands, so a
    caller that passed it got the transcript of a DIFFERENT session (the
    newest one on disk) and a zero exit code. The measured difference on
    this host was 60444 bytes of a foreign session instead of 793526 bytes
    of the requested one. A dead flag that silently answers with another
    session's data is worse than a refusal, so it is refused.
    """
    path = getattr(args, "path", None)
    if not path:
        return None
    return (
        f"[entire-agent-jcode] {command}: --path is not supported; "
        f"this command reads the transcript named by --session-ref\n"
    )


# reference: https://docs.entire.io/agents/agent-integration-protocol/commands
def _resolve_transcript_path(args) -> tuple[Path | None, str | None]:
    """The transcript file the command must read, or an error message.

    The CLI names the transcript it wants with ``--session-ref``. When it
    does, that name is the ONLY candidate: the command fails when that file
    is gone. Falling back to the newest session on disk made the CLI record
    ANOTHER session's transcript under THIS session's id, and made the output
    of the same command change between two runs.

    The fallback to the newest session is kept for the case where the CLI
    named no session at all: the transcript of the session the hook reported
    is then used, and only then the newest one on disk.

    Returns (path, None) on success, (None, message) on a named-session
    failure, and (None, None) when nothing was named and nothing was found.
    """
    session_ref = getattr(args, "session_ref", None) or getattr(args, "session", None)
    if session_ref:
        path = Path(session_ref)
        if not path.exists():
            return None, f"[entire-agent-jcode] session not found: {session_ref}\n"
        return path, None
    session_id = getattr(args, "session_id", None) or getattr(args, "session", None)
    if not session_id:
        session_id = _hook_input().get("session_id")
    if not session_id:
        session_id = _find_session()
    if not session_id:
        return None, None
    path = _session_file(session_id)
    if not path.exists():
        return None, f"[entire-agent-jcode] session not found: {session_id}\n"
    return path, None


# reference: https://docs.entire.io/agents/agent-integration-protocol/commands
def cmd_chunk_transcript(args) -> int:
    max_size = (
        getattr(args, "max_size", None)
        or getattr(args, "max_chunk_tokens", None)
        or getattr(args, "max_chunk_size", None)
        or 4000
    )
    raw = _read_stdin_bytes()
    if not raw:
        session = getattr(args, "session", None) or getattr(args, "session_id", None)
        session_ref = getattr(args, "session_ref", None)
        candidate = Path(session_ref) if session_ref else None
        if candidate is None and session:
            candidate = _session_file(session)
        if candidate is not None:
            if not candidate.exists():
                sys.stderr.write(f"[entire-agent-jcode] chunk-transcript: not found: {candidate}\n")
                return 1
            # The chunked path must chunk the transcript the readers see, not
            # the raw session document. A file that is neither a session
            # document nor a valid ETF v1 transcript is refused: chunking it
            # would hand the CLI an empty chunk list, which it stores as an
            # empty transcript.
            raw = _session_transcript_bytes(candidate)
            if raw is None:
                sys.stderr.write(
                    "[entire-agent-jcode] chunk-transcript: not a transcript\n"
                )
                return 1
    max_size = int(max_size)
    if max_size <= 0:
        max_size = 4000
    chunks = [
        base64.b64encode(raw[i : i + max_size]).decode("ascii")
        for i in range(0, len(raw), max_size)
    ]
    print(json.dumps({"chunks": chunks}))
    return 0


def cmd_reassemble_transcript(args) -> int:
    raw = _read_stdin().strip()
    chunks = []
    try:
        data = json.loads(raw) if raw else {}
        if isinstance(data, dict) and "chunks" in data:
            chunks = data["chunks"]
        elif isinstance(data, list):
            chunks = data
    except json.JSONDecodeError:
        for line in raw.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                chunks.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    if chunks and isinstance(chunks[0], str):
        out = bytearray()
        for chunk in chunks:
            out += base64.b64decode(chunk)
        sys.stdout.buffer.write(bytes(out))
        return 0
    messages = []
    for chunk in chunks:
        if isinstance(chunk, list):
            messages.extend(chunk)
        elif isinstance(chunk, dict) and "messages" in chunk:
            messages.extend(chunk["messages"])
    sys.stdout.write(json.dumps(messages))
    return 0


def cmd_format_resume_command(args) -> int:
    session_id = getattr(args, "session_id", None) or getattr(args, "session", None)
    print(json.dumps({"command": f"jcode --resume {session_id}"}))
    return 0


# ---------------------------------------------------------------------------
# hooks capability
# ---------------------------------------------------------------------------

def _hook_command_line(event: str) -> str:
    """Command Jcode runs for a lifecycle event (its [hooks] table).

    TOML literal string (single quotes): a Windows path carries backslashes
    that a basic TOML string would treat as escapes.
    """
    script = Path(__file__).resolve().parent / "entire-agent-jcode-hook.py"
    return f"python '{script}' {event}"


# Jcode's [hooks] table keys. These are the exact names jcode reads
# (crates/jcode-base/src/hooks.rs); hyphens would be ignored by jcode.
_HOOK_CONFIG_KEYS = {
    "session-start": "session_start",
    "turn-start": "turn_start",
    "turn-end": "turn_end",
    "session-end": "session_end",
}


def _config_path() -> Path:
    return _jcode_home() / "config.toml"


# reference: https://docs.python.org/3/library/tomllib.html
def _toml_quote(value: str) -> str:
    """TOML basic string with the escapes the value needs.

    The hook command contains single quotes (the shell quoting of the script
    path), and a TOML literal string cannot hold those. A basic string can,
    provided the backslashes of a Windows path are escaped.
    """
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def _toml_value(raw: str) -> str | None:
    """The string value of a TOML scalar/array element, or None if not a string."""
    raw = raw.strip()
    if len(raw) >= 2 and raw[0] == "'" and raw.endswith("'"):
        return raw[1:-1]
    if len(raw) >= 2 and raw[0] == '"' and raw.endswith('"'):
        try:
            return raw[1:-1].encode("utf-8").decode("unicode_escape")
        except Exception:
            return raw[1:-1]
    return None


def _split_array(raw: str) -> list[str]:
    """Split a TOML array body into its string elements."""
    inner = raw.strip()
    if inner.startswith("["):
        inner = inner[1:]
    if inner.endswith("]"):
        inner = inner[:-1]
    elements = []
    current = ""
    quote = ""
    for ch in inner:
        if quote:
            current += ch
            if ch == quote:
                quote = ""
            continue
        if ch in "\"'":
            quote = ch
            current += ch
            continue
        if ch == ",":
            elements.append(current)
            current = ""
            continue
        current += ch
    if current.strip():
        elements.append(current)
    out = []
    for element in elements:
        value = _toml_value(element)
        if value is not None:
            out.append(value)
    return out


def _patch_jcode_config(commands: dict[str, str], remove: bool = False) -> tuple[bool, str]:
    """Add (or remove) Jcode hook entries in ~/.jcode/config.toml.

    Text-level edit: the file is hand-editable TOML and the [hooks] table may
    hold scalars or arrays. Existing entries are preserved; a scalar that must
    share a key with ours is widened into an array. The result is parsed back
    before it is written, so a malformed edit never reaches the user's config.
    """
    path = _config_path()
    if not path.exists():
        return False, f"no Jcode config at {path}"
    original = path.read_text(encoding="utf-8")
    lines = original.splitlines()

    try:
        start = next(i for i, line in enumerate(lines) if line.strip() == "[hooks]")
    except StopIteration:
        if remove:
            return False, "no [hooks] table"
        lines.append("")
        lines.append("[hooks]")
        start = len(lines) - 1

    end = len(lines)
    for i in range(start + 1, len(lines)):
        if lines[i].strip().startswith("["):
            end = i
            break
    body: list[str | None] = lines[start + 1 : end]

    changed = False
    for event, command in commands.items():
        key = _HOOK_CONFIG_KEYS.get(event, event)
        idx = None
        for i, line in enumerate(body):
            if line is None or "=" not in line:
                continue
            head = line.split("=", 1)[0].strip()
            if head == key:
                idx = i
                break

        if idx is None:
            if remove:
                continue
            body.insert(0, f"{key} = {_toml_quote(command)}")
            changed = True
            continue

        line = body[idx] or ""
        value = line.split("=", 1)[1].strip()
        if value.startswith("["):
            existing = _split_array(value)
        else:
            scalar = _toml_value(value)
            existing = [scalar] if scalar is not None else []

        if remove:
            # Remove ONLY our own command. The user's own hook on the same key
            # must survive an uninstall of this adapter.
            remaining = [v for v in existing if v != command]
            if len(remaining) == len(existing):
                continue
            changed = True
            if not remaining:
                body[idx] = None
            else:
                body[idx] = f"{key} = [" + ", ".join(_toml_quote(v) for v in remaining) + "]"
            continue

        if command in existing:
            continue
        existing.append(command)
        body[idx] = f"{key} = [" + ", ".join(_toml_quote(v) for v in existing) + "]"
        changed = True

    if not changed:
        return False, "no change"

    body = [line for line in body if line is not None]
    new_lines = lines[: start + 1] + body + lines[end:]
    new_text = "\n".join(new_lines) + "\n"

    # Never write a config jcode cannot parse.
    try:
        import tomllib

        tomllib.loads(new_text)
    except ImportError:
        pass
    except Exception as exc:
        return False, f"refusing to write invalid TOML: {exc}"

    backup = path.with_suffix(path.suffix + ".bak-entire-jcode")
    backup.write_text(original, encoding="utf-8")
    path.write_text(new_text, encoding="utf-8")
    return True, str(backup)


def cmd_install_hooks(args) -> int:
    force = bool(getattr(args, "force", False))
    global_scope = bool(getattr(args, "global_scope", False))
    repo_path = getattr(args, "repo_path", None) or getattr(args, "dir", None) or os.getcwd()
    marker = _hook_marker(repo_path)
    marker.parent.mkdir(parents=True, exist_ok=True)

    commands = {event: _hook_command_line(event) for event in HOOK_NAMES}
    state = {
        "installed": True,
        "local_dev": bool(getattr(args, "local_dev", False)),
        "force": force,
        "agent": AGENT_NAME,
        "hook_names": HOOK_NAMES,
        "commands": commands,
        "jcode_config": str(_config_path()),
        "global_scope": global_scope,
    }
    marker.write_text(json.dumps(state, indent=2), encoding="utf-8")

    installed = 0
    if global_scope:
        ok, detail = _patch_jcode_config(commands)
        if ok:
            installed = len(commands)
            sys.stderr.write(
                f"[entire-agent-jcode] installed {installed} Jcode hooks in {_config_path()} "
                f"(backup: {detail})\n"
            )
        else:
            sys.stderr.write(f"[entire-agent-jcode] Jcode config not patched: {detail}\n")
    else:
        sys.stderr.write(
            "[entire-agent-jcode] repo-scoped install. Add these to ~/.jcode/config.toml "
            "under [hooks] to wire Jcode lifecycle events into Entire:\n"
        )
        for event, command in commands.items():
            sys.stderr.write(f'  {_HOOK_CONFIG_KEYS.get(event, event)} = "{command}"\n')
        sys.stderr.write(
            "  (or re-run with --global to let the adapter patch the file for you)\n"
        )

    print(json.dumps({"hooks_installed": installed if installed else len(commands)}))
    return 0


def cmd_uninstall_hooks(args) -> int:
    repo_path = getattr(args, "repo_path", None) or getattr(args, "dir", None) or os.getcwd()
    marker = _hook_marker(repo_path)
    try:
        marker.unlink()
    except FileNotFoundError:
        pass
    commands = {event: _hook_command_line(event) for event in HOOK_NAMES}
    ok, detail = _patch_jcode_config(commands, remove=True)
    if ok:
        sys.stderr.write(
            f"[entire-agent-jcode] removed Jcode hooks from {_config_path()} (backup: {detail})\n"
        )
    else:
        sys.stderr.write(f"[entire-agent-jcode] Jcode config not patched: {detail}\n")
    return 0


def cmd_are_hooks_installed(args) -> int:
    repo_path = getattr(args, "repo_path", None) or getattr(args, "dir", None) or os.getcwd()
    installed = _hook_marker(repo_path).exists()
    print(json.dumps({"installed": bool(installed)}))
    return 0


def cmd_parse_hook(args) -> int:
    hook_name = getattr(args, "hook", None) or "turn-end"
    payload = _hook_input()

    # Jcode's own hook payload uses "event"; the protocol uses "hook_type".
    event_key = payload.get("hook_type") or payload.get("event") or hook_name
    etype = _HOOK_TYPE_MAP.get(event_key) or _HOOK_TYPE_MAP.get(hook_name)
    if etype is None:
        print("null")
        return 0

    session_id = (payload.get("session_id") or "").strip()
    if not session_id:
        session_id = _find_session(workdir=payload.get("cwd"))
    if not session_id:
        print("null")
        return 0

    session_ref = payload.get("session_ref")
    if not session_ref:
        candidate = _session_file(session_id)
        session_ref = str(candidate) if candidate.exists() else None
    if session_ref:
        # The CLI reads the transcript at this path straight off disk when it
        # writes the checkpoint, so it must point at the per-line ETF file, not
        # at the one-document session file.
        sidecar = _materialize_transcript(session_id)
        if sidecar is not None:
            session_ref = str(sidecar)

    event = {"type": etype, "session_id": session_id}
    if session_ref:
        event["session_ref"] = session_ref
    prompt = payload.get("user_prompt") or payload.get("prompt")
    if not prompt and etype == 2:
        prompt = _last_user_prompt(session_id)
    if prompt:
        event["prompt"] = prompt
    model = payload.get("model")
    if model:
        event["model"] = model
    timestamp = payload.get("timestamp")
    if timestamp:
        event["timestamp"] = timestamp
    print(json.dumps(event))
    return 0


def cmd_hooks(args) -> int:
    sub = getattr(args, "hook_sub", None)
    if sub == "install-hooks":
        return cmd_install_hooks(args)
    if sub == "uninstall-hooks":
        return cmd_uninstall_hooks(args)
    if sub == "are-hooks-installed":
        return cmd_are_hooks_installed(args)
    if sub == "parse-hook":
        return cmd_parse_hook(args)
    raw = _read_stdin()
    for line in raw.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        sys.stderr.write(f"[entire-agent-jcode] hook event: {event.get('event', '')}\n")
    sys.stderr.flush()
    return 0


# ---------------------------------------------------------------------------
# transcript_analyzer capability
# ---------------------------------------------------------------------------

def _session_document(path_or_id) -> dict | None:
    try:
        path = Path(path_or_id)
        if not path.exists():
            path = _session_file(str(path_or_id))
        if not path.exists():
            return None
        return _load_json(path)
    except Exception:
        return None


def _modified_files(document: dict) -> list[str]:
    """Files named by write-like tool calls in the session document."""
    files: list[str] = []
    seen = set()
    for message in document.get("messages") or []:
        content = message.get("content")
        if not isinstance(content, list):
            continue
        for block in content:
            if not isinstance(block, dict) or block.get("type") != "tool_use":
                continue
            name = (block.get("name") or "").lower()
            if name not in _FILE_TOOLS:
                continue
            tool_input = block.get("input")
            if not isinstance(tool_input, dict):
                continue
            candidates = []
            for key in ("file_path", "path", "notebook_path", "filePath"):
                value = tool_input.get(key)
                if isinstance(value, str):
                    candidates.append(value)
            for key in ("paths", "files"):
                value = tool_input.get(key)
                if isinstance(value, list):
                    candidates.extend(v for v in value if isinstance(v, str))
            for candidate in candidates:
                if candidate and candidate not in seen:
                    seen.add(candidate)
                    files.append(candidate)
    return files


def _last_user_prompt(session_id: str) -> str:
    document = _session_document(session_id)
    if not document:
        return ""
    for message in reversed(document.get("messages") or []):
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if not isinstance(content, list):
            continue
        if any(isinstance(b, dict) and b.get("type") == "tool_result" for b in content):
            continue
        text = _text_of(content).strip()
        if text and not _is_system_reminder(text):
            return text
    return ""


def cmd_get_transcript_position(args) -> int:
    path = getattr(args, "path", None) or getattr(args, "session_ref", None)
    position = 0
    if path:
        candidate = Path(path)
        if not candidate.exists():
            candidate = _session_file(path)
        position = _line_count(candidate)
    print(json.dumps({"position": position}))
    return 0


def _modified_files_from_lines(lines: list[dict], offset: int = 0) -> list[str]:
    """Files named by the write-like tool calls in ETF lines."""
    files: list[str] = []
    seen = set()
    for index, line in enumerate(lines):
        if index < offset:
            continue
        for block in _etf_line_blocks(line):
            if block.get("type") != "tool_use":
                continue
            name = (block.get("name") or "").lower()
            if name not in _FILE_TOOLS:
                continue
            tool_input = block.get("input")
            if not isinstance(tool_input, dict):
                continue
            candidates = []
            for key in ("file_path", "path", "notebook_path", "filePath"):
                value = tool_input.get(key)
                if isinstance(value, str):
                    candidates.append(value)
            for key in ("paths", "files"):
                value = tool_input.get(key)
                if isinstance(value, list):
                    candidates.extend(v for v in value if isinstance(v, str))
            for candidate in candidates:
                if candidate and candidate not in seen:
                    seen.add(candidate)
                    files.append(candidate)
    return files


def cmd_extract_modified_files(args) -> int:
    path = getattr(args, "path", None) or getattr(args, "session_ref", None)
    offset = int(getattr(args, "offset", 0) or 0)
    files: list[str] = []
    current = 0
    if path:
        candidate = Path(path)
        if not candidate.exists():
            candidate = _session_file(path)
        lines = _transcript_lines(candidate)
        files = _modified_files_from_lines(lines, offset)
        current = _line_count(candidate)
    print(json.dumps({"files": files, "current_position": current}))
    return 0


def cmd_extract_prompts(args) -> int:
    path = getattr(args, "session_ref", None) or getattr(args, "path", None)
    prompts: list[str] = []
    if path:
        candidate = Path(path)
        if not candidate.exists():
            candidate = _session_file(path)
        offset = int(getattr(args, "offset", 0) or 0)
        for index, line in enumerate(_transcript_lines(candidate)):
            if index < offset:
                continue
            if line.get("type") != "user":
                continue
            text = _etf_line_text(line).strip()
            if text and not _is_system_reminder(text):
                prompts.append(text)
    print(json.dumps({"prompts": prompts}))
    return 0


def cmd_extract_summary(args) -> int:
    print(json.dumps({"summary": "", "has_summary": False}))
    return 0


def cmd_calculate_tokens(args) -> int:
    raw = _read_stdin_bytes()
    input_tokens = 0
    output_tokens = 0
    cache_read = 0
    cache_creation = 0
    api_call_count = 0
    lines: list[dict] = []
    if raw:
        text = raw.decode("utf-8", errors="replace")
        stripped = text.lstrip()
        if stripped.startswith("{"):
            try:
                document = json.loads(text)
            except Exception:
                document = None
            if isinstance(document, dict) and isinstance(document.get("messages"), list):
                lines = _etf_lines(document)
            elif isinstance(document, list):
                lines = [item for item in document if isinstance(item, dict)]
        else:
            for item in text.split("\n"):
                item = item.strip()
                if not item:
                    continue
                try:
                    value = json.loads(item)
                except Exception:
                    continue
                if isinstance(value, dict):
                    lines.append(value)
    for line in lines:
        if line.get("type") != "assistant":
            continue
        api_call_count += 1
        output_tokens += int(line.get("output_tokens") or 0)
        input_tokens += int(line.get("input_tokens") or 0)
        cache_read += int(line.get("cache_read_tokens") or 0)
        cache_creation += int(line.get("cache_write_tokens") or 0)
    print(
        json.dumps(
            {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cache_creation_tokens": cache_creation,
                "cache_read_tokens": cache_read,
                "api_call_count": api_call_count,
            }
        )
    )
    return 0


def cmd_prepare_transcript(args) -> int:
    return 0


# ---------------------------------------------------------------------------
# Argparse
# ---------------------------------------------------------------------------

def _add_common(sp):
    sp.add_argument("--dir", default=None)
    sp.add_argument("--repo-path", default=None, dest="repo_path")
    sp.add_argument("--session", default=None)
    sp.add_argument("--session-id", default=None, dest="session_id")
    sp.add_argument("--session-dir", default=None, dest="session_dir")
    sp.add_argument("--session-ref", default=None, dest="session_ref")
    sp.add_argument("--path", default=None)
    sp.add_argument("--offset", type=int, default=0)
    sp.add_argument("--max-size", default=None, dest="max_size")
    sp.add_argument("--max-chunk-tokens", type=int, default=None, dest="max_chunk_tokens")
    sp.add_argument("--max-chunk-size", default=None, dest="max_chunk_size")
    sp.add_argument("--hook", default=None)
    sp.add_argument("--local-dev", action="store_true", dest="local_dev")
    sp.add_argument("--force", action="store_true")
    sp.add_argument("--global", action="store_true", dest="global_scope")
    sp.add_argument("--model", default=None)
    sp.add_argument("--message", default=None)


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="entire-agent-jcode", description="Entire external agent adapter for Jcode"
    )
    sub = p.add_subparsers(dest="command", required=True)

    sp = sub.add_parser("info")
    sp.set_defaults(func=cmd_info)

    sp = sub.add_parser("detect")
    _add_common(sp)
    sp.set_defaults(func=cmd_detect)

    sp = sub.add_parser("get-session-id")
    _add_common(sp)
    sp.set_defaults(func=cmd_get_session_id)

    sp = sub.add_parser("get-session-dir")
    _add_common(sp)
    sp.set_defaults(func=cmd_get_session_dir)

    sp = sub.add_parser("resolve-session-file")
    _add_common(sp)
    sp.set_defaults(func=cmd_resolve_session_file)

    sp = sub.add_parser("read-session")
    _add_common(sp)
    sp.set_defaults(func=cmd_read_session)

    sp = sub.add_parser("write-session")
    _add_common(sp)
    sp.set_defaults(func=cmd_write_session)

    sp = sub.add_parser("read-transcript")
    _add_common(sp)
    # Escape hatch for a direct caller that knows its own stdout is not the
    # CLI's capped pipe. Never set this when the CLI is the parent: the cap
    # is then real and the refusal is the correct answer.
    sp.add_argument("--allow-oversized", action="store_true", dest="allow_oversized")
    sp.set_defaults(func=cmd_read_transcript)

    sp = sub.add_parser("compact-transcript")
    _add_common(sp)
    sp.set_defaults(func=cmd_compact_transcript)

    sp = sub.add_parser("chunk-transcript")
    _add_common(sp)
    sp.set_defaults(func=cmd_chunk_transcript)

    sp = sub.add_parser("reassemble-transcript")
    _add_common(sp)
    sp.set_defaults(func=cmd_reassemble_transcript)

    sp = sub.add_parser("format-resume-command")
    _add_common(sp)
    sp.set_defaults(func=cmd_format_resume_command)

    # hooks capability: top-level subcommands
    sp = sub.add_parser("install-hooks")
    _add_common(sp)
    sp.set_defaults(func=cmd_install_hooks)

    sp = sub.add_parser("uninstall-hooks")
    _add_common(sp)
    sp.set_defaults(func=cmd_uninstall_hooks)

    sp = sub.add_parser("are-hooks-installed")
    _add_common(sp)
    sp.set_defaults(func=cmd_are_hooks_installed)

    sp = sub.add_parser("parse-hook")
    _add_common(sp)
    sp.set_defaults(func=cmd_parse_hook)

    # hooks container with subcommands
    sp = sub.add_parser("hooks")
    hook_sub = sp.add_subparsers(dest="hook_sub")
    for name, func in (
        ("install-hooks", cmd_install_hooks),
        ("uninstall-hooks", cmd_uninstall_hooks),
        ("are-hooks-installed", cmd_are_hooks_installed),
        ("parse-hook", cmd_parse_hook),
    ):
        hsp = hook_sub.add_parser(name)
        _add_common(hsp)
        hsp.set_defaults(func=func)
    sp.set_defaults(func=cmd_hooks)

    # transcript_analyzer
    sp = sub.add_parser("get-transcript-position")
    _add_common(sp)
    sp.set_defaults(func=cmd_get_transcript_position)

    sp = sub.add_parser("extract-modified-files")
    _add_common(sp)
    sp.set_defaults(func=cmd_extract_modified_files)

    sp = sub.add_parser("extract-prompts")
    _add_common(sp)
    sp.set_defaults(func=cmd_extract_prompts)

    sp = sub.add_parser("extract-summary")
    _add_common(sp)
    sp.set_defaults(func=cmd_extract_summary)

    sp = sub.add_parser("calculate-tokens")
    _add_common(sp)
    sp.set_defaults(func=cmd_calculate_tokens)

    sp = sub.add_parser("prepare-transcript")
    _add_common(sp)
    sp.set_defaults(func=cmd_prepare_transcript)

    return p


def main() -> int:
    p = build_parser()
    args = p.parse_args()
    try:
        return args.func(args)
    except KeyboardInterrupt:
        return 130
    except Exception as exc:  # noqa: BLE001 - the CLI treats stderr+nonzero as failure
        sys.stderr.write(f"entire-agent-jcode: error: {exc}\n")
        return 1


if __name__ == "__main__":
    sys.exit(main())
