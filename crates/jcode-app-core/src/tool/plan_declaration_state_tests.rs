// reference: crates/jcode-app-core/src/tool/tests.rs - the sibling runtime
// state tests for this crate, same shape (direct state assertions).
use super::*;

const SESSION: &str = "plan-declaration-state-test-session";

#[test]
fn an_unknown_session_has_no_declared_plan() {
    assert!(!plan_declared("plan-declaration-state-test-unknown"));
}

/// A session that never received a request must not be gated: the rule is
/// about "a new request in an existing session".
#[test]
fn an_unknown_session_is_not_armed() {
    assert_eq!(
        plan_declaration_state("plan-declaration-state-test-unknown"),
        PlanDeclarationState::NotArmed
    );
}

#[test]
fn declaring_opens_the_gate_and_a_new_request_closes_it() {
    assert!(!plan_declared(SESSION));
    mark_plan_undeclared(SESSION);
    assert_eq!(
        plan_declaration_state(SESSION),
        PlanDeclarationState::AwaitingDeclaration
    );
    assert!(!plan_declared(SESSION));
    mark_plan_declared(SESSION);
    assert!(plan_declared(SESSION));
    assert_eq!(
        plan_declaration_state(SESSION),
        PlanDeclarationState::Declared
    );
    mark_plan_undeclared(SESSION);
    assert!(!plan_declared(SESSION));
    forget_plan_declaration(SESSION);
    assert_eq!(
        plan_declaration_state(SESSION),
        PlanDeclarationState::NotArmed
    );
}

#[test]
fn sessions_are_independent() {
    mark_plan_declared("plan-declaration-state-test-a");
    assert!(!plan_declared("plan-declaration-state-test-b"));
    forget_plan_declaration("plan-declaration-state-test-a");
}
