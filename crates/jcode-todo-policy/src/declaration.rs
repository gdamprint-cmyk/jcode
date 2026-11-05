//! Policy for the "a new request must declare its plan first" gate.
//!
//! The user's rule (2026-11-05): when a new request arrives in an existing
//! session, the agent must first update the todo plan, add the new items and
//! declare them, and only then start the work.
//!
//! MEASURED 2026-11-05 over 27 673 real user requests in saved sessions:
//! the plan received new items in only 5 418 of them (19.58%), and in 21 457
//! (77.54%) the `todo` tool was never called at all - the agent answered
//! straight away. The rule existed only as prompt-overlay text, so nothing
//! enforced it.
//!
//! This module holds the pure decision: given the tool the model wants to call
//! and whether the plan for the current request was already declared, decide
//! whether the call is allowed or must be refused with an instruction to
//! declare the plan. Runtime state lives in the caller.

// reference: crates/jcode-app-core/src/tool/mod.rs (Registry::execute runs the
// configured `pre_tool` gate and refuses the call with a reason the model
// reads) and crates/jcode-todo-policy/src/followup.rs (a pure decision over a
// plan snapshot, returning a message for the model).
// reference: no prior art for this exact gate; searched the jcode repo for
// "declaration", "gate" and "before plan" and found only prompt-overlay text.

/// The message returned to the model when a work tool runs before the plan
/// for the current request was declared.
pub const PLAN_DECLARATION_REQUIRED_MESSAGE: &str = "Before doing any work for a new request, update the todo plan: add the new items for this request and declare them. Then continue with the work.";

/// Tools that are part of declaring or reading the plan. They are always
/// allowed: a plan declaration must not be blocked by the plan gate.
pub const PLAN_TOOLS: &[&str] = &["todo"];

/// Tools that only look at the world. They do not change anything, so they
/// are allowed before the plan is declared: reading is how the agent forms
/// the plan in the first place.
///
/// `memory` is NOT here: it has mutating actions (`remember`, `forget`,
/// `tag`, `link`), so it is a work tool.
pub const READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "agentgrep",
    "graft",
    "serena",
    "jcode_docs",
    "session_search",
    "conversation_search",
    "websearch",
    "webfetch",
    "open",
    "panel",
    "side_panel",
    "todo",
    "goal",
];

/// Whether a tool call is allowed before the plan for the current request has
/// been declared.
///
/// `plan_declared` is true once the model has written the plan for the request
/// that is currently being worked on. `tool` is the resolved tool name.
///
/// The gate is deliberately narrow: it blocks tools that DO work, and lets
/// read-only tools through, because the agent has to be able to look at the
/// code before it can name the plan items.
pub fn tool_allowed_before_plan_declared(tool: &str, plan_declared: bool) -> bool {
    if plan_declared {
        return true;
    }
    if PLAN_TOOLS.contains(&tool) || READ_ONLY_TOOLS.contains(&tool) {
        return true;
    }
    !is_work_tool(tool)
}

/// Whether the tool is one that performs work, i.e. one the plan gate guards.
///
/// A tool that is not on either list is treated as a work tool: an unknown
/// tool is not a reason to skip the declaration.
pub fn is_work_tool(tool: &str) -> bool {
    !PLAN_TOOLS.contains(&tool) && !READ_ONLY_TOOLS.contains(&tool)
}

/// Whether a todo write declares a plan for the request.
///
/// A write that only changes the assessment (goals/plan fields) does not
/// declare work items, so it does not open the gate.
pub fn todo_write_declares_plan(input: &serde_json::Value) -> bool {
    let has = |key: &str| input.get(key).is_some_and(|value| !value.is_null());
    has("todos") || has("phases") || has("update") || has("add")
}

/// The error a refused work tool returns to the model.
pub fn plan_declaration_required_error() -> String {
    PLAN_DECLARATION_REQUIRED_MESSAGE.to_string()
}

/// Whether a message is a synthetic continuation rather than a real user
/// request.
///
/// A synthetic continuation is the agent's own turn coming back: an auto-poke
/// for incomplete todos, a retry reminder, a reload recovery, a scheduled
/// task. Those are persisted as user turns so the model treats them as
/// continuations, but they are NOT a new request from the user, so they must
/// not re-arm the plan-declaration gate.
///
/// MEASURED 2026-11-05: every synthetic continuation this codebase writes is
/// prefixed `[auto] ` (see `jcode-todo-policy/src/lib.rs`), except the
/// scheduled-task envelope, which uses `[Scheduled task]`.
pub fn is_synthetic_request(message: &str) -> bool {
    let trimmed = message.trim();
    trimmed.starts_with("[auto] ")
        || trimmed.starts_with("[Scheduled task]\n")
        || crate::is_auto_poke_message(trimmed)
}

#[cfg(test)]
#[path = "declaration_tests.rs"]
mod tests;
