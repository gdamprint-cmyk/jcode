// reference: crates/jcode-app-core/src/tool/tests.rs - the sibling runtime tests
// for this crate (direct registry execution with a ToolContext).
//
// These tests exercise the gate as the product uses it: a real registry, a
// real tool call, and the refusal the model reads.
//
// MEASURED 2026-11-05 over 27 673 real user requests in saved sessions: the
// `todo` tool was never called in 77.54% of them. The gate is the code that
// makes the rule real, so the tests below assert the behaviour end to end:
// a work tool is refused before the plan is declared and allowed after.

use super::*;
use crate::message::Role;
use crate::tool::{Registry, Tool, ToolContext, ToolExecutionMode, ToolOutput};
use serde_json::Value;
use std::sync::Arc;

/// A work tool that records that it ran. It exists so the test can tell
/// "refused by the gate" from "ran and produced output".
struct RecordingWorkTool {
    name: &'static str,
    ran: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for RecordingWorkTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "test work tool"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        self.ran
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolOutput::new("ran"))
    }
}

fn gate_context(session_id: &str) -> ToolContext {
    ToolContext {
        session_id: session_id.to_string(),
        message_id: "message".to_string(),
        tool_call_id: "tool-call".to_string(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::AgentTurn,
    }
}

fn registry_with_work_tool(
    name: &'static str,
    ran: Arc<std::sync::atomic::AtomicUsize>,
) -> Registry {
    let mut tools = std::collections::HashMap::new();
    tools.insert(
        name.to_string(),
        Arc::new(RecordingWorkTool { name, ran }) as Arc<dyn Tool>,
    );
    Registry {
        tools: Arc::new(tokio::sync::RwLock::new(tools)),
        mcp_policy: Arc::default(),
        skills: Arc::new(tokio::sync::RwLock::new(crate::skill::SkillRegistry::default())),
        compaction: Arc::new(tokio::sync::RwLock::new(crate::compaction::CompactionManager::new())),
    }
}

fn ran_count(ran: &Arc<std::sync::atomic::AtomicUsize>) -> usize {
    ran.load(std::sync::atomic::Ordering::SeqCst)
}

/// A real user request arms the gate, so a work tool is refused.
#[tokio::test]
async fn a_real_request_refuses_a_work_tool_before_the_plan_is_declared() {
    let _env_lock = crate::storage::lock_test_env();
    let session = "plan-declaration-gate-refuses";
    plan_declaration::forget_plan_declaration(session);
    let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let registry = registry_with_work_tool("bash", ran.clone());

    plan_declaration::note_user_request(session, "fix the failing test", false);

    let error = registry
        .execute("bash", serde_json::json!({}), gate_context(session))
        .await
        .expect_err("a work tool must be refused before the plan is declared");
    assert!(
        error.to_string().contains("todo"),
        "the refusal must tell the model to declare the plan: {error}"
    );
    assert_eq!(ran_count(&ran), 0, "the work tool must not have run");
    plan_declaration::forget_plan_declaration(session);
}

/// Declaring the plan opens the gate, so the same work tool runs.
#[tokio::test]
async fn declaring_the_plan_opens_the_gate_for_the_same_request() {
    let _env_lock = crate::storage::lock_test_env();
    let session = "plan-declaration-gate-opens";
    plan_declaration::forget_plan_declaration(session);
    let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let registry = registry_with_work_tool("bash", ran.clone());

    plan_declaration::note_user_request(session, "fix the failing test", false);
    plan_declaration::mark_plan_declared(session);

    registry
        .execute("bash", serde_json::json!({}), gate_context(session))
        .await
        .expect("a work tool must run once the plan is declared");
    assert_eq!(ran_count(&ran), 1);
    plan_declaration::forget_plan_declaration(session);
}

/// The next real request closes the gate again: every request needs its own
/// declaration.
#[tokio::test]
async fn the_next_real_request_closes_the_gate_again() {
    let _env_lock = crate::storage::lock_test_env();
    let session = "plan-declaration-gate-rearms";
    plan_declaration::forget_plan_declaration(session);
    let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let registry = registry_with_work_tool("bash", ran.clone());

    plan_declaration::note_user_request(session, "first request", false);
    plan_declaration::mark_plan_declared(session);
    plan_declaration::note_user_request(session, "second request", false);

    let error = registry
        .execute("bash", serde_json::json!({}), gate_context(session))
        .await
        .expect_err("a new request must declare its own plan");
    assert!(error.to_string().contains("todo"), "{error}");
    assert_eq!(ran_count(&ran), 0);
    plan_declaration::forget_plan_declaration(session);
}

/// A synthetic continuation is not a new request: it neither re-arms the gate
/// nor closes an open one.
#[tokio::test]
async fn a_synthetic_continuation_neither_re_arms_nor_closes_the_gate() {
    let _env_lock = crate::storage::lock_test_env();
    let session = "plan-declaration-gate-synthetic";
    plan_declaration::forget_plan_declaration(session);

    // A synthetic continuation while the gate is closed must not arm it.
    plan_declaration::note_user_request(
        session,
        crate::todo::TODO_FINAL_RESPONSE_CONTINUATION_MESSAGE,
        false,
    );
    assert_eq!(
        plan_declaration::plan_declaration_state(session),
        plan_declaration::PlanDeclarationState::NotArmed
    );

    // A synthetic continuation while the gate is open must not close it.
    plan_declaration::note_user_request(session, "fix the failing test", false);
    plan_declaration::mark_plan_declared(session);
    plan_declaration::note_user_request(
        session,
        crate::todo::TODO_FINAL_RESPONSE_CONTINUATION_MESSAGE,
        false,
    );
    assert_eq!(
        plan_declaration::plan_declaration_state(session),
        plan_declaration::PlanDeclarationState::Declared
    );
    plan_declaration::forget_plan_declaration(session);
}

/// A session that never received a request is not gated: a test harness, a
/// background worker and an SDK client driving the tools directly all keep
/// working.
#[tokio::test]
async fn a_session_without_a_request_is_not_gated() {
    let _env_lock = crate::storage::lock_test_env();
    let session = "plan-declaration-gate-unarmed";
    plan_declaration::forget_plan_declaration(session);
    let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let registry = registry_with_work_tool("bash", ran.clone());

    registry
        .execute("bash", serde_json::json!({}), gate_context(session))
        .await
        .expect("a session with no request must not be gated");
    assert_eq!(ran_count(&ran), 1);
    plan_declaration::forget_plan_declaration(session);
}

/// Read-only tools stay allowed while the gate is closed: the agent has to be
/// able to look at the code before it can name the plan items.
#[tokio::test]
async fn read_only_tools_stay_allowed_before_the_plan_is_declared() {
    let _env_lock = crate::storage::lock_test_env();
    let session = "plan-declaration-gate-read-only";
    plan_declaration::forget_plan_declaration(session);

    plan_declaration::note_user_request(session, "fix the failing test", false);
    for tool in ["read", "todo", "goal"] {
        assert!(
            crate::todo::declaration::tool_allowed_before_plan_declared(tool, false),
            "{tool} must be allowed before the plan is declared"
        );
    }
    assert!(
        !crate::todo::declaration::tool_allowed_before_plan_declared("bash", false),
        "bash must be refused before the plan is declared"
    );
    plan_declaration::forget_plan_declaration(session);
}

/// The refusal message is the instruction the model reads, so it has to name
/// the todo tool and the action.
#[test]
fn the_refusal_message_names_the_todo_tool_and_the_action() {
    let message = crate::todo::declaration::plan_declaration_required_error();
    assert!(message.contains("todo"), "{message}");
    assert!(message.contains("declare"), "{message}");
}

/// The gate lives in the shared runtime, so the TUI crate reaches it through
/// the same module the server does.
#[test]
fn the_gate_module_is_reachable_from_the_app_core_surface() {
    let _ = Role::User;
    assert_eq!(
        plan_declaration::plan_declaration_state("plan-declaration-gate-unreachable"),
        plan_declaration::PlanDeclarationState::NotArmed
    );
}
