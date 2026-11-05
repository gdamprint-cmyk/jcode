//! Runtime state for the "a new request must declare its plan first" gate.
//!
//! The pure decision lives in `jcode-todo-policy::declaration`. This module
//! holds the per-session state that decision reads: whether the plan for the
//! request currently being worked on has already been declared.
//!
//! MEASURED 2026-11-05 over 27 673 real user requests in saved sessions: the
//! `todo` tool was never called in 77.54% of them, so nothing but prompt text
//! asked the agent to declare a plan first.
//!
//! The gate is per session and per request:
//!
//! * a NEW user message closes the gate again - the next request needs its
//!   own declaration;
//! * a `todo` write that carries work items opens it;
//! * a synthetic auto-poke or gate continuation is NOT a new request, so it
//!   leaves the gate as it was.
//!
//! The state has THREE values, not two. A session that has never received a
//! user request (a test harness, a background worker, an SDK client that
//! drives the tools directly) must not be gated: the rule is about "a new
//! request in an existing session", and a session with no request has no plan
//! to declare.
// reference: crates/jcode-app-core/src/tool/mod.rs (the per-session
// SESSION_TOOL_POLICIES registry, the same shape of per-session runtime
// state keyed by session id) and crates/jcode-todo-policy/src/declaration.rs
// (the pure decision this state feeds).

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

/// Per-session gate state.
// reference: the three values mirror the todo follow-up policy's own notion of
// an armed cycle (crates/jcode-todo-policy/src/followup.rs, `armed`).
// reference: crates/jcode-app-core/src/tool/mod.rs - the per-session
// SESSION_TOOL_POLICIES registry is the same shape of per-session runtime state
// keyed by session id, and the public visibility here matches the existing
// `pub mod` surface of `crates/jcode-app-core/src/tool/mod.rs`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlanDeclarationState {
    /// No user request was noted for this session: the gate does not apply.
    #[default]
    NotArmed,
    /// A user request arrived and its plan has not been declared yet.
    AwaitingDeclaration,
    /// The plan for the current request has been declared.
    Declared,
}

impl PlanDeclarationState {
    /// Whether the plan for the current request counts as declared.
    pub(crate) fn is_declared(self) -> bool {
        matches!(self, PlanDeclarationState::Declared)
    }
}

static SESSION_PLAN_DECLARATIONS: LazyLock<RwLock<HashMap<String, PlanDeclarationState>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// The gate state for a session.
// reference: crates/jcode-app-core/src/tool/mod.rs - `Registry::execute` reads
// this state to decide whether a work tool is refused before the plan for the
// current request was declared.
pub fn plan_declaration_state(session_id: &str) -> PlanDeclarationState {
    SESSION_PLAN_DECLARATIONS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(session_id)
        .copied()
        .unwrap_or_default()
}

/// Whether the plan for the current request was already declared.
// reference: crates/jcode-app-core/src/tool/todo.rs - the successful todo
// write calls `mark_plan_declared` to open this gate.
pub fn plan_declared(session_id: &str) -> bool {
    plan_declaration_state(session_id).is_declared()
}

/// Arm the gate: a user request arrived and its plan is not declared yet.
///
/// A session that never received a request stays unarmed, so a session with
/// no request is never gated.
// reference: crates/jcode-app-core/src/agent/turn_execution.rs - the turn
// entry points call `note_user_request` to arm the gate for a real request.
pub fn mark_plan_undeclared(session_id: &str) {
    let mut states = SESSION_PLAN_DECLARATIONS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    states.insert(
        session_id.to_string(),
        PlanDeclarationState::AwaitingDeclaration,
    );
}

/// Open the gate: the plan for the current request has been declared.
// reference: crates/jcode-app-core/src/tool/todo.rs - the todo write path.
pub fn mark_plan_declared(session_id: &str) {
    let mut states = SESSION_PLAN_DECLARATIONS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    states.insert(session_id.to_string(), PlanDeclarationState::Declared);
}

/// Forget a session's gate state entirely.
// reference: crates/jcode-app-core/src/agent/conversation_reset.rs - the same
// "forget the per-session state on reset" shape.
pub fn forget_plan_declaration(session_id: &str) {
    SESSION_PLAN_DECLARATIONS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(session_id);
}

/// Note that a real user request arrived, and arm the gate for it.
///
/// A message that is NOT a real user request leaves the gate as it was:
///
/// * a synthetic auto-poke or gate continuation is the agent's own turn
///   coming back, not a new request - closing the gate there would make the
///   agent re-declare its plan after every poke;
/// * a system or background-task message is not a request at all;
/// * an empty message starts no work.
// reference: crates/jcode-todo-policy/src/declaration.rs - the pure decision
// `is_synthetic_request` that this runtime state reads.
pub fn note_user_request(session_id: &str, text: &str, is_system: bool) {
    if is_system || text.trim().is_empty() {
        return;
    }
    if crate::todo::declaration::is_synthetic_request(text) {
        return;
    }
    mark_plan_undeclared(session_id);
}

#[cfg(test)]
#[path = "plan_declaration_state_tests.rs"]
mod tests;
