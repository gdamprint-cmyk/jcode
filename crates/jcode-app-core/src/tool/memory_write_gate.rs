// reference: https://github.com/NousResearch/hermes-agent/blob/main/tools/write_approval.py
// reference: https://github.com/NousResearch/hermes-agent/blob/main/tools/memory_tool.py
//! Opt-in approve/reject gate for writes into memory.
//!
//! Port of Hermes' working memory write gate onto jcode's EXISTING permission
//! surface (`jcode-base/src/safety.rs`), so no second human-in-the-loop
//! mechanism is introduced.
//!
//! Hermes' `evaluate_gate()` returns exactly one of allow / blocked / stage:
//! the gate off allows the write; the gate on with nobody to answer STAGES the
//! write (delayed, never silently refused); only an explicit user denial
//! BLOCKS it. `stage_write()` persists the payload under
//! `pending/<subsystem>/<id>.json` for replay on approval, and the config key
//! is `<subsystem>.write_approval` (a bool, off when unset).
//!
//! MEASURED 2026-11-17: upstream jcode has NO equivalent - `memory_write_gate`,
//! `write_approval` and `memory_write` have zero hits in `upstream/master`
//! under `crates/`. Hermes is therefore the only prior art.
//!
//! HOW THE PORT DIFFERS: Hermes stages the payload and replays it from
//! `/memory approve`; jcode has no pending-store replay path, so a
//! DETERMINISTIC request id (a hash of session id, scope, category and
//! content) replaces it. The agent's next `remember` call with the same
//! content computes the same id, finds the recorded approval in
//! `~/.jcode/safety/history.json`, and the write is applied. The staged
//! PAYLOAD is the content itself, which the model already holds.
//!
//! The gate is opt-in: `memory_write_gate = "off"` (the default) keeps the
//! pre-gate behaviour exactly - the write is applied immediately.
//!
//! reference: crates/jcode-base/src/safety.rs - the jcode-side permission
//! queue, decisions and `record_permission_via_file` this gate writes into.
//! reference: crates/jcode-app-core/src/tool/research_gate.rs - the sibling
//! per-session tool gate, same "read the config, decide before the call" shape.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::MemoryWriteGate;
use crate::memory::MemoryEntry;

/// The action label the permission surface shows for a memory write.
const MEMORY_WRITE_ACTION: &str = "memory_write";

/// Per-session: the request id of a memory write that is already approved.
///
/// The approval itself lives in `~/.jcode/safety/history.json` (durable, and
/// the same place every other permission decision lands). This map is the
/// in-process cache of "which request id did I already approve", so a repeat
/// write does not have to re-read the history file on every call.
static APPROVED_MEMORY_WRITES: LazyLock<RwLock<HashMap<String, String>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// The deterministic request id for a memory write.
///
/// Deterministic so the SAME write produces the SAME id: the agent can retry
/// the same `remember` call after the user approved it and the gate lets it
/// through. Keyed by the session id so one session's approval never unblocks
/// another session's write.
pub fn memory_write_request_id(
    session_id: &str,
    scope: &str,
    category: &str,
    content: &str,
) -> String {
    let mut hasher = Sha256::new();
    for part in [session_id, scope, category, content.trim()] {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    format!(
        "req_memory_{}",
        digest
            .iter()
            .take(12)
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// The first 12 bytes of a sha256 digest as hex (96 bits of request id).
fn id_prefix(hasher: sha2::Sha256) -> String {
    let digest = hasher.finalize();
    digest
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// Clip a content preview for the permission request description.
fn truncate(content: &str, max_chars: usize) -> String {
    let trimmed = content.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    let clipped: String = trimmed.chars().take(max_chars - 1).collect();
    format!("{clipped}…")
}

/// Whether the config asks for an approval before a memory write.
pub fn gate_enabled() -> bool {
    crate::config::config()
        .agents
        .memory_write_gate
        .requires_approval()
}

/// The configured gate, for callers that want the raw value.
pub fn configured_gate() -> MemoryWriteGate {
    crate::config::config().agents.memory_write_gate
}

/// Mark a memory-write request id as approved in this process.
fn remember_approval(session_id: &str, request_id: &str) {
    APPROVED_MEMORY_WRITES
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(session_id.to_string(), request_id.to_string());
}

/// Whether this session already has an approval for the given request id.
fn approval_cached(session_id: &str, request_id: &str) -> bool {
    APPROVED_MEMORY_WRITES
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(session_id)
        .is_some_and(|approved| approved == request_id)
}

/// Outcome of the gate decision for one memory write.
///
/// Mirrors Hermes' `GateDecision`: `allow` proceeds, `blocked` is an explicit
/// refusal, and the third state is the staged one - a write that is waiting
/// for a decision and is NOT lost, only delayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// The gate is off (or approval is already recorded): write it.
    Allowed,
    /// The write is waiting for the user's decision.
    Pending {
        /// The permission request id to approve.
        request_id: String,
        /// The reason shown to the user.
        reason: String,
    },
    /// The user refused this write. Do not write it.
    Rejected { reason: String },
}

/// Decide whether a memory write may be applied.
///
/// `session_id`, `scope`, `category` and `content` identify the write. The
/// decision is durable: an approval recorded by ANY permission surface
/// (`jcode permissions`, Telegram, email) unblocks the same write on the next
/// attempt.
pub fn evaluate_memory_write(
    session_id: &str,
    scope: &str,
    category: &str,
    content: &str,
) -> GateOutcome {
    if !gate_enabled() {
        return GateOutcome::Allowed;
    }

    let request_id = memory_write_request_id(session_id, scope, category, content);

    if approval_cached(session_id, &request_id) {
        return GateOutcome::Allowed;
    }

    // The durable decision path: the permission surfaces write the decision
    // into `history.json`, so read it before deciding the write is still
    // pending.
    match crate::safety::permission_decision_via_file(&request_id) {
        Ok(Some(decision)) => {
            if decision.approved {
                remember_approval(session_id, &request_id);
                return GateOutcome::Allowed;
            }
            return GateOutcome::Rejected {
                reason: decision
                    .message
                    .unwrap_or_else(|| "the user rejected this memory write".to_string()),
            };
        }
        Ok(None) => {}
        Err(error) => {
            // A broken history file must not wedge the agent: the write stays
            // pending (fail closed on the WRITE, not on the host) and the
            // reason is logged.
            crate::logging::warn(&format!(
                "MEMORY_WRITE_GATE could not read the permission history: {error}"
            ));
        }
    }

    // Still pending: make sure the request is in the queue the permission
    // surfaces read, and report the id back to the agent.
    ensure_request_queued(session_id, &request_id, scope, category, content);

    GateOutcome::Pending {
        request_id: request_id.clone(),
        reason: format!(
            "MEMORY-WRITE GATE: writing to memory needs your approval. \
             Approve it with `jcode permissions` (request id {request_id}) or the \
             Telegram/email reply, then repeat the same memory write."
        ),
    }
}

/// Put the request into the permission queue if it is not there yet.
///
/// Re-queuing is harmless: the queue is keyed by request id, and the id is
/// deterministic, so a second call replaces the previous entry rather than
/// piling up duplicates.
fn ensure_request_queued(
    session_id: &str,
    request_id: &str,
    scope: &str,
    category: &str,
    content: &str,
) {
    let system = crate::safety::SafetySystem::new();
    if system
        .pending_requests()
        .iter()
        .any(|req| req.id == request_id)
    {
        return;
    }

    let request = crate::safety::PermissionRequest {
        id: request_id.to_string(),
        action: MEMORY_WRITE_ACTION.to_string(),
        description: format!("Write to memory ({scope}/{category}): {content}"),
        rationale:
            "The memory write gate is enabled, so this fact is not stored until you approve it."
                .to_string(),
        urgency: crate::safety::Urgency::Normal,
        wait: false,
        created_at: chrono::Utc::now(),
        context: Some(json!({
            "session_id": session_id,
            "review": {
                "summary": format!("Memory write ({scope}/{category}): {content}"),
                "why_permission_needed":
                    "memory_write_gate = \"approve\" is set, so a fact is only stored after you approve it.",
                "files": Vec::<String>::new(),
                "commands": Vec::<String>::new(),
            },
        })),
    };

    let result = system.request_permission(request);
    if let crate::safety::PermissionResult::Queued { request_id } = result {
        crate::logging::info(&format!(
            "MEMORY_WRITE_GATE queued a memory write for approval: {request_id}"
        ));
    }
}

/// Decide whether a whole batch of memory writes may be applied.
///
/// This is the CLI import path: `jcode memory import` writes many entries in
/// one command, so gating them one at a time would queue a request per entry.
/// Instead the batch is evaluated as a unit and ONE permission request covers
/// it, under the same contract as [`evaluate_memory_write`]:
///
/// - gate off, or the batch already approved -> `Allowed`
/// - the batch explicitly refused -> `Rejected` (fail closed, nothing is written)
/// - otherwise -> `Pending { request_id, reason }`, with the request queued for
///   the existing permission surfaces and the batch left unwritten.
///
/// The batch id is deterministic over the same inputs as the per-entry ids, so
/// approving the batch request and re-running the identical import goes
/// through, and a different import is a different request.
// reference: https://github.com/NousResearch/hermes-agent/blob/main/tools/write_approval.py
//   (per-subsystem write_approval gate, allow/blocked/stage; this batch entry
//   point applies the same contract to a multi-entry import)
pub fn evaluate_memory_write_batch(
    scope: &str,
    entries: &[MemoryEntry],
) -> GateOutcome {
    if !gate_enabled() {
        return GateOutcome::Allowed;
    }

    let batch_id = memory_write_batch_id(scope, entries);

    // A CLI import is a fresh process every time, so the approval cache (which
    // lives in the process) is always cold here. The durable decision is the
    // one that matters, and it is read below.
    match crate::safety::permission_decision_via_file(&batch_id) {
        Ok(Some(decision)) if decision.approved => {
            return GateOutcome::Allowed;
        }
        Ok(Some(decision)) => {
            return GateOutcome::Rejected {
                reason: decision
                    .message
                    .unwrap_or_else(|| "the user rejected this memory import".to_string()),
            };
        }
        Ok(None) => {}
        Err(error) => {
            crate::logging::warn(&format!(
                "MEMORY_WRITE_GATE could not read the permission history: {error}"
            ));
        }
    }

    ensure_batch_request_queued(&batch_id, scope, entries);

    let count = entries.len();
    GateOutcome::Pending {
        request_id: batch_id.clone(),
        reason: format!(
            "MEMORY-WRITE GATE: importing {count} memories into {scope} memory needs your \
             approval. Approve it with `jcode permissions` (request id {batch_id}) or the \
             Telegram/email reply, then repeat the same import."
        ),
    }
}

/// The deterministic request id for a batch memory write.
///
/// Same construction as [`memory_write_request_id`] (fields separated by a
/// zero byte, sha256, first 12 bytes hex), but the content component is the
/// concatenation of the entries' category/content so the id identifies the
/// exact batch.
///
/// Deliberately NOT keyed by session id, unlike the single-write id: an import
/// runs in its own process and the approval is made by ANOTHER process
/// (`jcode permissions`, Telegram, email). Keying by session would mint a new
/// id per invocation, so the approval could never be found and the import would
/// be held forever.
// reference: https://github.com/NousResearch/hermes-agent/blob/main/tools/write_approval.py
//   (an approved write is replayed from the staged record; the identity of a
//   pending write must therefore be recoverable without the requester's process)
pub fn memory_write_batch_id(scope: &str, entries: &[MemoryEntry]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(scope.as_bytes());
    hasher.update([0u8]);
    // Keyed by CONTENT, not by the entry id: an import's ids are whatever the
    // source file carried, and re-exporting the same memories can reorder or
    // re-id them. The same batch of facts must produce the same request id so
    // the approval is reusable. A deterministic sort makes the id independent
    // of the order the file happened to list them in.
    let mut fingerprint = entries
        .iter()
        .map(|entry| {
            format!(
                "{}\u{0}{}\u{0}",
                entry.category,
                entry.content.trim()
            )
        })
        .collect::<Vec<_>>();
    fingerprint.sort();
    for part in fingerprint {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    format!("req_memory_{}", id_prefix(hasher))
}

/// Put the batch request into the permission queue if it is not there yet.
fn ensure_batch_request_queued(
    request_id: &str,
    scope: &str,
    entries: &[MemoryEntry],
) {
    let system = crate::safety::SafetySystem::new();
    if system
        .pending_requests()
        .iter()
        .any(|req| req.id == request_id)
    {
        return;
    }

    let summary = entries
        .iter()
        .take(3)
        .map(|entry| format!("{}: {}", entry.category, truncate(&entry.content, 60)))
        .collect::<Vec<_>>()
        .join("; ");
    let more = entries.len().saturating_sub(3);
    let summary = if more > 0 {
        format!("{summary}; and {more} more")
    } else {
        summary
    };

    let request = crate::safety::PermissionRequest {
        id: request_id.to_string(),
        action: MEMORY_WRITE_ACTION.to_string(),
        description: format!(
            "Import {} memories into {scope} memory: {summary}",
            entries.len()
        ),
        rationale:
            "The memory write gate is enabled, so an import is not stored until you approve it."
                .to_string(),
        urgency: crate::safety::Urgency::Normal,
        wait: false,
        created_at: chrono::Utc::now(),
        context: Some(json!({
            // The batch id is content-keyed, so it is the only stable handle on
            // this write; there is no session for a one-shot CLI import.
            "memory_write_request_id": request_id,
            "review": {
                "summary": format!("Import {} memories into {scope} memory", entries.len()),
                "why_permission_needed":
                    "memory_write_gate = \"approve\" is set, so an import is only stored after you approve it.",
                "files": Vec::<String>::new(),
                "commands": Vec::<String>::new(),
            },
        })),
    };

    let result = system.request_permission(request);
    if let crate::safety::PermissionResult::Queued { request_id } = result {
        crate::logging::info(&format!(
            "MEMORY_WRITE_GATE queued a memory import for approval: {request_id}"
        ));
    }
}

/// Drop the cached approval for a session (used by tests and by a config
/// reload that turns the gate back on).
pub fn reset_session(session_id: &str) {
    APPROVED_MEMORY_WRITES
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(session_id);
}
