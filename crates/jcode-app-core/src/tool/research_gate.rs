//! Runtime state for the "research the web / official docs BEFORE implementing"
//! gate.
//!
//! The pure decision lives in [`should_require_research`]. This module holds
//! the per-session state that decision reads: whether a qualifying research
//! lookup (`websearch` / `webfetch` / their aliases) happened in the CURRENT
//! TURN for that session.
//!
//! WHY THIS EXISTS
//! ---------------
//! `~/.jcode/decisions/reference_first.py` already blocks a substantial new
//! source file that carries no `// reference:` declaration. It is
//! PROSE-COMPLIANT ONLY: a declaration can be invented, so it does not prove a
//! lookup happened. This gate is the second half - it refuses the write until a
//! real research tool call has been observed this turn.
//!
//! SHAPE
//! -----
//! Same contract as Hermes' per-turn controller (`tool_guardrails.py`,
//! `before_call`): per-turn state, a decision BEFORE the call, and a reset when
//! a new turn begins. Only the research-lookup dimension is implemented here.
//!
//! The state is keyed by session id, so a lookup in session A never unblocks a
//! write in session B, and the reset at turn start means a lookup in turn N
//! does not carry into turn N+1.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use serde_json::Value;

/// Per-session: did a qualifying research lookup happen in the current turn?
// reference: crates/jcode-app-core/src/tool/plan_declaration.rs - the same
// per-session `static LazyLock<RwLock<HashMap<String, ...>>>` pattern keyed by
// session id, with poison-tolerant read/write.
static SESSION_RESEARCH_LOOKUP: LazyLock<RwLock<HashMap<String, bool>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// The tools that count as "a qualifying research lookup".
///
/// `websearch` / `webfetch` are jcode's own; `web_search` / `web_extract` are
/// the Hermes names, accepted so a gateway or MCP alias that reaches the
/// registry under its own name is still credited.
// reference: C:\Users\WorkShop1\AppData\Local\hermes\hermes-agent\agent\tool_guardrails.py
// - `IDEMPOTENT_TOOL_NAMES` lists `web_search` / `web_extract` as the read-only
// lookup family; this is the jcode-side subset that satisfies "research".
const DEFAULT_RESEARCH_TOOLS: [&str; 4] = ["websearch", "webfetch", "web_search", "web_extract"];

/// The tools that can bring a new implementation into existence. Same set the
/// python gate uses, so the two agree.
// reference: ~/.jcode/decisions/reference_first.py - `WRITE_TOOLS`.
const WRITE_TOOLS: [&str; 6] = [
    "write",
    "edit",
    "apply_patch",
    "replace",
    "create_file",
    "write_file",
];

/// Directory segments that mean "working area", not "product".
///
/// MEASURED 2026-11-16 on this host: this gate blocked a one-off MEASUREMENT
/// probe written by an agent, and it had no exemption at all, while the python
/// `research_required.py` gate DID have one - so the two disagreed about the
/// same file. The rules now live in one module (`decisions/triviality.py`) that
/// both python gates import, and this file mirrors it.
/// reference: C:\Users\WorkShop1\.jcode\decisions\triviality.py
const TRIVIAL_DIR_SEGMENTS: [&str; 13] = [
    "scratch", "scratchpad", "tmp", "temp", "research", "probe", "probes",
    "sandbox", "playground", "out", "output", "artifacts", "artifact",
];

/// Directory segments that mean "this is a test".
/// reference: C:\Users\WorkShop1\.jcode\decisions\triviality.py
const TEST_DIR_SEGMENTS: [&str; 6] =
    ["test", "tests", "testing", "__tests__", "spec", "specs"];

/// Extensions that are SOURCE for this gate. Mirrors the python gate's
/// `SOURCE_EXT`.
// reference: ~/.jcode/decisions/reference_first.py - `SOURCE_EXT`.
const SOURCE_EXTENSIONS: &[&str] = &[
    ".cs", ".py", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".rs", ".go", ".java", ".kt",
    ".kts", ".c", ".h", ".cc", ".cpp", ".hpp", ".cshtml", ".vb", ".swift", ".m", ".mm", ".php", ".rb",
    ".scala", ".dart", ".lua", ".pl",
];

/// Never source for the purposes of this gate. The first group is prose and data;
/// the second is what this host actually writes by hand, every day, and gating it
/// would make the gate an obstacle rather than a control.
///
/// This list mirrors the python gate's `NEVER_SOURCE` exactly; `SOURCE_EXTENSIONS`
/// is the same set plus `.vb`, so the two gates agree on every extension.
// reference: ~/.jcode/decisions/reference_first.py - `NEVER_SOURCE`.
const NEVER_SOURCE_EXTENSIONS: &[&str] = &[
    ".md", ".markdown", ".txt", ".json", ".jsonl", ".toml", ".yaml", ".yml", ".csv", ".log",
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".ico", ".ps1", ".psm1", ".bat", ".cmd",
    ".ini", ".cfg", ".xml", ".lock",
];

/// Note that a qualifying research lookup happened for this session this turn.
///
/// An unrecognised tool name is a no-op, never an error: the caller is the tool
/// dispatch path, and a misnamed tool must not wedge the host.
// reference: crates/jcode-app-core/src/tool/mod.rs - `Registry::execute` calls
// this after a successful research tool call.
pub fn note_research_lookup(session_id: &str, tool_name: &str) {
    if !is_research_tool(tool_name) {
        return;
    }
    let mut states = SESSION_RESEARCH_LOOKUP
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    states.insert(session_id.to_string(), true);
}

/// Whether a qualifying research lookup happened for this session this turn.
// reference: crates/jcode-app-core/src/tool/mod.rs - the gate in `Registry::execute`
// reads this before a source-writing tool runs.
pub fn research_lookup_occurred(session_id: &str) -> bool {
    SESSION_RESEARCH_LOOKUP
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(session_id)
        .copied()
        .unwrap_or(false)
}

/// Forget the current turn's research state for a session.
///
/// Called at the top of every agent turn, so a lookup in turn N does not carry
/// into turn N+1.
// reference: crates/jcode-app-core/src/agent/turn_loops.rs `run_turn` and
// crates/jcode-app-core/src/agent/turn_streaming_mpsc.rs `run_turn_streaming_mpsc`
// - both call this at turn start.
pub fn reset_research_state(session_id: &str) {
    SESSION_RESEARCH_LOOKUP
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(session_id);
}

/// Whether a tool name counts as a research lookup, per the config list.
fn is_research_tool(tool_name: &str) -> bool {
    let configured = &crate::config::config().research_gate.required_tools;
    if configured.is_empty() {
        return DEFAULT_RESEARCH_TOOLS
            .iter()
            .any(|name| name.eq_ignore_ascii_case(tool_name));
    }
    configured
        .iter()
        .any(|name| name.eq_ignore_ascii_case(tool_name))
}

/// The extension of a path, lowercased, with the leading dot. Empty when the
/// path has no extension.
fn extension_of(path: &str) -> String {
    let trimmed = path.trim().trim_matches('"').trim_matches('\'');
    let base = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    match base.rfind('.') {
        // A dot at position 0 is a dotfile, not an extension.
        Some(dot) if dot > 0 => base[dot..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Whether the target path is a source file this gate applies to.
fn is_source_path(path: &str) -> bool {
    let ext = extension_of(path);
    if ext.is_empty() || NEVER_SOURCE_EXTENSIONS.contains(&ext.as_str()) {
        return false;
    }
    SOURCE_EXTENSIONS.contains(&ext.as_str())
}

/// The target path a tool call writes to, if the payload names one.
fn target_path(payload: &Value) -> &str {
    for key in ["file_path", "path", "file", "target", "filePath"] {
        if let Some(value) = payload.get(key).and_then(Value::as_str)
            && !value.trim().is_empty()
        {
            return value;
        }
    }
    ""
}

/// The path segments of a path, split on both separators.
fn path_segments(path: &str) -> Vec<String> {
    path.split(['/', '\\'])
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .map(|segment| segment.to_ascii_lowercase())
        .collect()
}

/// Whether the file is a TEST file, by name or by directory.
///
/// A test asserts about code that already exists, so its provenance IS the
/// module under test: there is nothing to look up.
/// reference: C:\Users\WorkShop1\.jcode\decisions\triviality.py - `is_test_path`.
fn is_test_path(path: &str) -> bool {
    let segments = path_segments(path);
    let Some(name) = segments.last() else {
        return false;
    };
    let stem = name.split('.').next().unwrap_or(name);
    // The Python rule this mirrors is:
    //   ^test_ | _test$ | ^tests?$ | _tests?$ | \.test\. | _spec\. | ^spec_ | ^conftest
    // So the shape is a PREFIX or a SUFFIX, never a free token: "mod_tests" is
    // a test file, "test_thing" is a test file, "thing_spec" is a test file,
    // and "specs" or "mytest" are NOT.
    //
    // MEASURED 2026-11-16: the first version compared the whole stem, so
    // "mod_tests.rs" was NOT recognised as a test. The isolated compile check
    // caught it. A later version split on `_` and over-matched: "specs.rs"
    // became a test file in Rust but not in Python.
    let is_test = |token: &str| {
        token == "test"
            || token == "tests"
            || token == "testing"
            || token == "conftest"
    };
    if is_test(stem) {
        return true;
    }
    if stem.starts_with("test_")
        || stem.starts_with("spec_")
        || stem.starts_with("conftest")
    {
        return true;
    }
    for suffix in ["_test", "_tests", "_spec"] {
        if stem.ends_with(suffix) {
            return true;
        }
    }
    // ".test." anywhere in the name.
    if name.contains(".test.") {
        return true;
    }
    segments[..segments.len().saturating_sub(1)]
        .iter()
        .any(|segment| TEST_DIR_SEGMENTS.contains(&segment.as_str()))
}

/// Whether the path says "working area" rather than "product tree".
/// reference: C:\Users\WorkShop1\.jcode\decisions\triviality.py - `is_trivial_path`.
fn is_trivial_path(path: &str) -> bool {
    let segments = path_segments(path);
    if segments.is_empty() {
        return false;
    }
    if segments[..segments.len() - 1]
        .iter()
        .any(|segment| TRIVIAL_DIR_SEGMENTS.contains(&segment.as_str()))
    {
        return true;
    }
    let name = segments.last().cloned().unwrap_or_default();
    let stem = name.split('.').next().unwrap_or(&name).to_string();
    // The python rule matches the probe words as SEPARATE tokens, separated by
    // `.`, `_` or `-`. Mirror that: a whole token must be one of the words.
    const PROBE_WORDS: [&str; 24] = [
        "probe", "probes", "measure", "measurement", "check", "checker", "audit",
        "verify", "validation", "scratch", "tmp", "temp", "scratchpad", "dump",
        "diag", "diagnostic", "inspect", "inspector", "sniff", "count", "scan",
        "sampler", "bench", "benchmark",
    ];
    stem
        .split(['.', '_', '-'])
        .any(|token| PROBE_WORDS.contains(&token))
}

/// Whether the content has the SHAPE of a measurement rather than of a module.
///
/// The python rule: no module docstring, no typed signatures, no typed
/// exports, and at least half of the body lines are calls.
/// reference: C:\Users\WorkShop1\.jcode\decisions\triviality.py - `is_trivial_shape`.
fn is_trivial_shape(text: &str) -> bool {
    let stripped = text.trim();
    if stripped.is_empty() {
        return false;
    }
    let lines: Vec<&str> = stripped
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return false;
    }
    // A module docstring marks a real module.
    let first = lines[0].trim_start();
    if first.starts_with("//!")
        || first.starts_with("///")
        || first.starts_with("/**")
        || first.starts_with("/*")
        || (first.starts_with('#') && !first.starts_with("#!"))
        || (first.starts_with('"') && first.len() > 2)
        || (first.starts_with('\'') && first.len() > 2)
    {
        return false;
    }
    let mut typed_signature = false;
    let mut typed_export = false;
    for line in &lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("fn ")
            || trimmed.starts_with("fn(")
            || trimmed.starts_with("pub fn ")
            || trimmed.starts_with("func ")
            || trimmed.starts_with("function ")
            || trimmed.starts_with("pub ")
            || trimmed.starts_with("impl ")
            || trimmed.starts_with("export ")
            || trimmed.starts_with("type ")
            || trimmed.starts_with("interface ")
            || trimmed.starts_with("class ")
        {
            typed_signature = true;
        }
        if trimmed.starts_with("export ")
            || trimmed.starts_with("pub ")
            || trimmed.starts_with("impl ")
        {
            typed_export = true;
        }
    }
    if typed_signature || typed_export {
        return false;
    }
    // At least half of the non-definition, non-import body lines are calls.
    let mut body = 0usize;
    let mut calls = 0usize;
    for line in &lines {
        let trimmed = line.trim_start();
        let is_definition = trimmed.starts_with("def ")
            || trimmed.starts_with("async def ")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("function ")
            || trimmed.starts_with("fn ")
            || trimmed.starts_with("func ");
        let is_import = trimmed.starts_with("import ") || trimmed.starts_with("from ");
        if is_definition || is_import {
            continue;
        }
        body += 1;
        if trimmed.contains('(') {
            calls += 1;
        }
    }
    body > 0 && calls * 2 >= body
}

/// Whether this write is exempt from the research requirement.
///
/// MEASURED 2026-11-16: without this, the gate refused a one-off measurement
/// probe - a script whose correctness comes from reading the host, not from the
/// internet - and it had no way to exempt a test file either. The python gates
/// already exempted both; this file now agrees with them.
/// reference: C:\Users\WorkShop1\.jcode\decisions\triviality.py
fn is_trivial(path: &str, text: &str) -> bool {
    is_test_path(path) || is_trivial_path(path) || is_trivial_shape(text)
}

/// The text this tool call would introduce. Only the NEW side of an edit.
// reference: ~/.jcode/decisions/reference_first.py - `_payload_text`.
fn payload_text(payload: &Value) -> String {
    for key in [
        "content",
        "new_string",
        "replacement",
        "patch_text",
        "patch",
        "new_text",
        "replacement_text",
        "body",
    ] {
        if let Some(value) = payload.get(key).and_then(Value::as_str)
            && !value.trim().is_empty()
        {
            return value.to_string();
        }
    }
    // `edit` may carry a list of edits instead of a single `new_string`.
    if let Some(edits) = payload.get("edits").and_then(Value::as_array) {
        let mut parts = Vec::new();
        for item in edits {
            for key in ["new_string", "replacement", "content"] {
                if let Some(value) = item.get(key).and_then(Value::as_str) {
                    parts.push(value.to_string());
                }
            }
        }
        return parts.join("\n");
    }
    String::new()
}

/// Whether new content is substantial enough to be a synthesised technique.
///
/// Mirrors the python gate's floor exactly: 2000 bytes OR 60 non-empty lines,
/// measured on the new content only.
fn is_substantial(text: &str, min_bytes: usize, min_lines: usize) -> bool {
    let stripped = text.trim();
    if stripped.len() >= min_bytes {
        return true;
    }
    stripped.lines().filter(|line| !line.trim().is_empty()).count() >= min_lines
}

/// The block reason for a refused call. Escape FIRST, because the tail is
/// truncated by the harness.
fn block_reason(path: &str, line_count: usize, tool: &str) -> String {
    format!(
        "RESEARCH-FIRST GATE: `{tool}` on `{path}` was refused.\n\
         {line_count} lines of new source are about to be written, and no research \
         lookup happened this turn. A declaration line is not evidence, so this \
         gate does not accept one.\n\n\
         DO THIS, THEN RE-SEND THE SAME CALL:\n\
         1. Run `websearch` with the EXACT symptom, error text or capability you \
         are about to implement, not a paraphrase.\n\
         2. `webfetch` the official documentation of the thing you intend to use.\n\
         3. Re-send the `{tool}` call. The block clears automatically once a \
         research lookup is observed this turn.\n\n\
         The point is not the search log. It is that the implementation is \
         decided by a real source instead of a guess.\n\n\
         NOT gated: files under the size floor, prose/data (.md .txt .json .toml \
         .yaml .csv .log, images), shell/batch (.ps1 .psm1 .bat .cmd), any \
         non-source extension, and any tool that is not a source writer.\n\n\
         Disable: `[research_gate]` `enabled = false` in config.toml."
    )
}

/// Decide whether a tool call must be refused because no research lookup
/// happened this turn. Returns the block reason, or `None` to allow.
///
/// FAIL OPEN: a malformed payload, an unreadable path, or any internal error
/// yields `None`. A broken policy must never wedge the host.
// reference: crates/jcode-app-core/src/tool/mod.rs - `Registry::execute` calls
// this before `tool.execute`, after the plan-declaration gate.
pub fn should_require_research(
    tool: &str,
    payload: &Value,
    session_id: &str,
) -> Option<String> {
    let config = &crate::config::config().research_gate;
    if !config.enabled {
        return None;
    }
    if !payload.is_object() {
        return None;
    }
    let name = tool.trim().to_ascii_lowercase();
    if !WRITE_TOOLS.contains(&name.as_str()) {
        return None;
    }
    let path = target_path(payload);
    if !is_source_path(path) {
        return None;
    }
    let text = payload_text(payload);
    if text.trim().is_empty() {
        return None;
    }
    if !is_substantial(&text, config.min_bytes, config.min_lines) {
        return None;
    }
    // A probe, a measurement, a test, or a one-off helper is exempt: its
    // correctness comes from reading the host, not from the internet.
    // reference: C:\Users\WorkShop1\.jcode\decisions\triviality.py
    if is_trivial(&path, &text) {
        return None;
    }
    if research_lookup_occurred(session_id) {
        return None;
    }
    let line_count = text.lines().filter(|line| !line.trim().is_empty()).count();
    Some(block_reason(path, line_count, &name))
}

// The unit tests live in `research_gate_tests.rs` and are wired ONCE, from
// `tool/mod.rs`, so the module path they see is `crate::tool` and
// `research_gate::…` resolves there. A second wiring here would compile the
// same file twice under a different `super`, and every `research_gate::…`
// path in it would resolve against the wrong parent.
// reference: crates/jcode-app-core/src/tool/mod.rs - the single wiring site.
