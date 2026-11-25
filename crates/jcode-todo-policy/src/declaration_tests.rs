// reference: crates/jcode-todo-policy/src/followup_tests.rs - the sibling
// policy test module for this crate, same shape (pure decisions over a plan
// snapshot, asserted directly).
// reference: crates/jcode-command-risk/src/lib.rs (assess / RiskLevel::runs_immediately)
// is the primary source for the bash read-only/destructive decision; the same
// classifier is already used by crates/jcode-app-core/src/tool/bash_destructive_gate.rs.
use super::*;
// The three continuation constants live in the crate root, not in the
// `declaration` module this test file is nested in, so `use super::*` does
// not bring them into scope.
use crate::{
    TODO_FINAL_RESPONSE_CONTINUATION_MESSAGE, TODO_INTENT_UNDERSTANDING_CONTINUATION_MESSAGE,
    TODO_LONG_SESSION_REVIEW_MESSAGE,
};

/// MEASURED 2026-11-05 over 27 673 real user requests in saved sessions: the
/// `todo` tool was never called in 77.54% of them, so the "declare the plan
/// first" rule was not enforced by anything but prompt text.
#[test]
fn a_work_tool_is_refused_before_the_plan_is_declared() {
    for tool in [
        "write",
        "edit",
        "apply_patch",
        "compile_remote",
        "browser",
        "gmail",
        "calendar",
        "schedule",
        "selfdev",
        "batch",
        "codemode",
        "mcp",
    ] {
        assert!(!tool_allowed_before_plan_declared(tool, false), "{tool}");
        assert!(is_work_tool(tool), "{tool}");
    }
    assert!(is_work_tool("bash"));
    assert!(!tool_allowed_before_plan_declared("bash", false));
}

/// Reading the code is how the agent forms the plan, so read-only tools must
/// pass the gate. `initiative` is read-only and was being blocked before the
/// declaration, so it is explicitly on the list.
#[test]
fn read_only_tools_pass_before_the_plan_is_declared() {
    for tool in READ_ONLY_TOOLS {
        assert!(tool_allowed_before_plan_declared(tool, false), "{tool}");
        assert!(!is_work_tool(tool), "{tool}");
    }
    assert!(!is_work_tool("initiative"));
    assert!(tool_allowed_before_plan_declared("initiative", false));
}

/// The declaration itself must never be gated, or the agent could not comply.
#[test]
fn the_plan_tool_always_passes() {
    assert!(tool_allowed_before_plan_declared("todo", false));
    assert!(!is_work_tool("todo"));
}

/// A declared plan opens the gate for every tool.
#[test]
fn a_declared_plan_opens_the_gate() {
    for tool in ["bash", "write", "edit", "batch", "mcp"] {
        assert!(tool_allowed_before_plan_declared(tool, true), "{tool}");
    }
}

/// `bash` is both an analysis tool and a work tool: the classifier's outright
/// deny is work, everything else is analysis.
// reference: crates/jcode-command-risk/src/lib.rs (assess / RiskLevel::is_absolute_deny)
// reference: crates/jcode-command-risk/src/paths.rs (expand uses home_dir, which
// `RiskContext::from_env` resolves from USERPROFILE on Windows).
#[test]
fn read_only_bash_commands_pass_and_destructive_ones_do_not() {
    assert!(bash_command_allowed_before_plan_declared("git log --oneline -5", None));
    assert!(bash_command_allowed_before_plan_declared("cargo check -p jcode-todo-policy", None));
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .expect("a home directory is required to test home destruction");
    // Forward slashes: the tokenizer treats `\` as an escape on Windows, so a
    // backslash path would not resolve to the home directory.
    let home = home.to_string_lossy().replace('\\', "/");
    assert!(!bash_command_allowed_before_plan_declared(
        &format!("rm -rf {home}"),
        None
    ));
    assert!(!bash_command_allowed_before_plan_declared("rm -rf /", None));
}

/// An unknown tool is not a reason to skip the declaration.
#[test]
fn an_unknown_tool_is_treated_as_work() {
    assert!(is_work_tool("some_future_tool"));
    assert!(!tool_allowed_before_plan_declared("some_future_tool", false));
}

/// A write that only touches the assessment does not declare work items.
#[test]
fn only_an_assessment_write_does_not_declare_a_plan() {
    assert!(!todo_write_declares_plan(&serde_json::json!({})));
    assert!(!todo_write_declares_plan(
        &serde_json::json!({"goals": [], "plan": {"user_intention": "x"}})
    ));
    assert!(todo_write_declares_plan(
        &serde_json::json!({"todos": [{"name": "a"}]})
    ));
    assert!(todo_write_declares_plan(
        &serde_json::json!({"phases": [{"items": []}]})
    ));
    assert!(todo_write_declares_plan(
        &serde_json::json!({"add": [{"name": "a"}]})
    ));
    assert!(todo_write_declares_plan(
        &serde_json::json!({"update": [{"name": "a", "status": "in_progress"}]})
    ));
}

/// A null value is not a declaration.
#[test]
fn a_null_field_does_not_declare_a_plan() {
    assert!(!todo_write_declares_plan(
        &serde_json::json!({"todos": null, "add": null})
    ));
}

/// The refusal must tell the model what to do, not just refuse.
#[test]
fn the_refusal_names_the_required_action() {
    let message = plan_declaration_required_error();
    assert!(message.contains("todo"));
    assert!(message.contains("declare"));
}

/// The turn reminder is what the model sees BEFORE any work tool: it has to
/// allow the first analysis pass and still ask for the declaration.
#[test]
fn the_reminder_allows_the_first_pass_and_asks_for_the_plan() {
    let message = PLAN_DECLARATION_REMINDER_MESSAGE;
    assert!(message.contains("todo"));
    assert!(message.contains("first pass"));
    assert!(message.contains("before the first work tool"));
    assert!(
        !message.contains("Before doing any work for a new request"),
        "the reminder must not be the old blocking sentence: {message}"
    );
}

/// A synthetic continuation is not a new user request, so it must not re-arm
/// the plan gate. Every synthetic continuation this codebase writes is either
/// prefixed `[auto] ` or is a scheduled-task envelope.
#[test]
fn a_synthetic_continuation_is_not_a_new_request() {
    assert!(is_synthetic_request(TODO_FINAL_RESPONSE_CONTINUATION_MESSAGE));
    assert!(is_synthetic_request(TODO_INTENT_UNDERSTANDING_CONTINUATION_MESSAGE));
    assert!(is_synthetic_request(TODO_LONG_SESSION_REVIEW_MESSAGE));
    assert!(is_synthetic_request(
        "[Scheduled task]\nA scheduled task for this session is now due."
    ));
}

/// A real user request must arm the gate, so it is never classified as
/// synthetic - not even when it happens to mention a todo or a task.
#[test]
fn a_real_request_is_not_synthetic() {
    assert!(!is_synthetic_request("fix the failing test"));
    assert!(!is_synthetic_request("add a todo item for the release"));
    assert!(!is_synthetic_request("update the schedule"));
    assert!(!is_synthetic_request(""));
}
