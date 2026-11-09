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
