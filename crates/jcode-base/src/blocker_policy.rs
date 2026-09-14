//! Policy primitives for autonomously unblocking a task without bypassing safety.
//!
//! This module is deliberately a planning layer. It does not execute actions,
//! grant permissions, or replace the command-risk and safety systems. Callers
//! use it to choose the next bounded step, while the existing execution gates
//! remain authoritative at the point of action.

use serde::{Deserialize, Serialize};

/// Broad cause of a blocker. Keeping the cause explicit prevents a transient
/// transport failure from being treated like a permissions or safety refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerKind {
    Access,
    Authentication,
    Network,
    Toolchain,
    Dependency,
    Source,
    Configuration,
    Context,
    Evidence,
    ExternalPolicy,
    Safety,
    Unknown,
}

/// Lifecycle of a blocker tracked against a todo item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerStatus {
    Open,
    Resolved,
    Escalated,
}

/// What the agent is considering doing next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Diagnose,
    Research,
    AlternateSource,
    Retry,
    ReversibleChange,
    ExternalSideEffect,
    CredentialChange,
    DestructiveChange,
    AskUser,
}

/// Planning-time risk class. This is intentionally coarser than the execution
/// classifiers. The real tool/command safety gate must still inspect the actual
/// call and can require a stricter outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionRisk {
    ReadOnly,
    ReversibleLocal,
    ExternalSideEffect,
    CredentialChange,
    Destructive,
    Protected,
}

/// Strength of evidence produced by an attempted step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceLevel {
    None,
    Indirect,
    Partial,
    Representative,
    AcceptanceAligned,
}

impl EvidenceLevel {
    /// Whether this evidence directly exercises the requested acceptance path.
    pub fn is_acceptance_aligned(self) -> bool {
        self >= Self::AcceptanceAligned
    }
}

/// A candidate step considered while unblocking a blocker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateAction {
    pub kind: ActionKind,
    pub risk: ActionRisk,
    pub summary: String,
    /// False means the action would exceed the user's stated scope and must not
    /// be silently inferred as authorized.
    pub within_user_scope: bool,
    /// A retry is meaningful only when it changes a tested variable or follows
    /// new evidence. This prevents retry loops that merely burn time or quota.
    pub changes_tested_variable: bool,
    /// A reversible change needs a concrete check that can report whether it
    /// improved the requested outcome.
    pub has_acceptance_check: bool,
}

/// One bounded attempt made against a blocker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerAttempt {
    pub kind: ActionKind,
    pub summary: String,
    pub succeeded: bool,
    pub evidence: EvidenceLevel,
    pub changed_tested_variable: bool,
}

/// Durable, user-visible blocker record. The record contains a concise audit
/// trail, not hidden chain-of-thought: symptom, attempted alternatives, result,
/// and the next safe step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_id: Option<String>,
    pub kind: BlockerKind,
    pub symptom: String,
    pub status: BlockerStatus,
    pub attempts: Vec<BlockerAttempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

impl BlockerRecord {
    pub fn new(id: impl Into<String>, kind: BlockerKind, symptom: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            todo_id: None,
            kind,
            symptom: symptom.into(),
            status: BlockerStatus::Open,
            attempts: Vec::new(),
            next_step: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.status == BlockerStatus::Open
    }

    pub fn record_attempt(&mut self, attempt: BlockerAttempt) {
        self.attempts.push(attempt);
    }

    /// Mark the blocker resolved only after the requested acceptance check has
    /// passed. A successful diagnosis or alternate-source fetch is evidence
    /// about the blocker, not proof that the original objective is fixed.
    pub fn resolve(&mut self) {
        self.status = BlockerStatus::Resolved;
        self.next_step = None;
    }

    pub fn escalate(&mut self, next_step: impl Into<String>) {
        self.status = BlockerStatus::Escalated;
        self.next_step = Some(next_step.into());
    }
}

/// Bounds for autonomous recovery. These are intentionally finite and small:
/// the policy favors a new evidence-producing route over an unbounded retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnblockingPolicy {
    pub max_attempts: usize,
    pub max_same_route_retries: usize,
}

impl Default for UnblockingPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 6,
            max_same_route_retries: 1,
        }
    }
}

/// Result of the planning policy. `Continue` does not itself authorize a tool
/// call. It only says that the candidate is eligible for the next bounded step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnblockingDecision {
    Continue { reason: String },
    RequestPermission { reason: String },
    Escalate { reason: String },
    Reject { reason: String },
}

impl UnblockingPolicy {
    /// Evaluate one candidate step against the current blocker state.
    pub fn evaluate(
        &self,
        blocker: &BlockerRecord,
        candidate: &CandidateAction,
    ) -> UnblockingDecision {
        if !blocker.is_open() {
            return UnblockingDecision::Reject {
                reason: "the blocker is no longer open".to_string(),
            };
        }

        if candidate.risk == ActionRisk::Protected {
            return UnblockingDecision::Escalate {
                reason: "the candidate targets a protected or catastrophic scope; do not bypass the existing safety boundary".to_string(),
            };
        }

        if !candidate.within_user_scope {
            return UnblockingDecision::RequestPermission {
                reason:
                    "the candidate exceeds the user's stated scope, so scope cannot be inferred"
                        .to_string(),
            };
        }

        if matches!(
            candidate.risk,
            ActionRisk::ExternalSideEffect | ActionRisk::CredentialChange | ActionRisk::Destructive
        ) || matches!(
            candidate.kind,
            ActionKind::ExternalSideEffect
                | ActionKind::CredentialChange
                | ActionKind::DestructiveChange
                | ActionKind::AskUser
        ) {
            return UnblockingDecision::RequestPermission {
                reason: "the candidate can affect external state, credentials, or non-trivial data; use the existing confirmation/permission flow".to_string(),
            };
        }

        if blocker.attempts.len() >= self.max_attempts {
            return UnblockingDecision::Escalate {
                reason: format!(
                    "the bounded recovery budget of {} attempts is exhausted; preserve the evidence and escalate instead of looping",
                    self.max_attempts
                ),
            };
        }

        if candidate.kind == ActionKind::Retry && !candidate.changes_tested_variable {
            return UnblockingDecision::Reject {
                reason: "blind retries are not an unblocking strategy; change a tested variable or gather new evidence first".to_string(),
            };
        }

        if candidate.kind == ActionKind::Retry {
            let same_route_retries = blocker
                .attempts
                .iter()
                .filter(|attempt| attempt.kind == ActionKind::Retry)
                .count();
            if same_route_retries >= self.max_same_route_retries {
                return UnblockingDecision::Escalate {
                    reason: format!(
                        "the same recovery route already used its {} retry allowance; choose a different evidence-producing route",
                        self.max_same_route_retries
                    ),
                };
            }
        }

        if candidate.kind == ActionKind::ReversibleChange && !candidate.has_acceptance_check {
            return UnblockingDecision::Reject {
                reason:
                    "a reversible change needs a concrete acceptance check before it is applied"
                        .to_string(),
            };
        }

        UnblockingDecision::Continue {
            reason: match candidate.kind {
                ActionKind::Diagnose => {
                    "run a bounded read-only diagnosis and classify the blocker".to_string()
                }
                ActionKind::Research | ActionKind::AlternateSource => {
                    "research an alternative channel or stronger source, then record what it proves"
                        .to_string()
                }
                ActionKind::Retry => {
                    "run one bounded retry after changing a tested variable".to_string()
                }
                ActionKind::ReversibleChange => {
                    "apply one reversible in-scope change, then run its acceptance check"
                        .to_string()
                }
                _ => "continue with the bounded in-scope step".to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(kind: ActionKind, risk: ActionRisk) -> CandidateAction {
        CandidateAction {
            kind,
            risk,
            summary: "test candidate".to_string(),
            within_user_scope: true,
            changes_tested_variable: true,
            has_acceptance_check: true,
        }
    }

    #[test]
    fn allows_read_only_diagnosis() {
        let blocker = BlockerRecord::new("b1", BlockerKind::Network, "timeout");
        let decision = UnblockingPolicy::default().evaluate(
            &blocker,
            &action(ActionKind::Diagnose, ActionRisk::ReadOnly),
        );
        assert!(matches!(decision, UnblockingDecision::Continue { .. }));
    }

    #[test]
    fn allows_research_and_alternate_source() {
        let blocker = BlockerRecord::new("b1", BlockerKind::ExternalPolicy, "403");
        for kind in [ActionKind::Research, ActionKind::AlternateSource] {
            let decision =
                UnblockingPolicy::default().evaluate(&blocker, &action(kind, ActionRisk::ReadOnly));
            assert!(matches!(decision, UnblockingDecision::Continue { .. }));
        }
    }

    #[test]
    fn rejects_blind_retry() {
        let blocker = BlockerRecord::new("b1", BlockerKind::Network, "timeout");
        let mut candidate = action(ActionKind::Retry, ActionRisk::ReadOnly);
        candidate.changes_tested_variable = false;
        let decision = UnblockingPolicy::default().evaluate(&blocker, &candidate);
        assert!(matches!(decision, UnblockingDecision::Reject { .. }));
    }

    #[test]
    fn bounds_same_route_retry() {
        let mut blocker = BlockerRecord::new("b1", BlockerKind::Network, "timeout");
        blocker.record_attempt(BlockerAttempt {
            kind: ActionKind::Retry,
            summary: "first retry".to_string(),
            succeeded: false,
            evidence: EvidenceLevel::Partial,
            changed_tested_variable: true,
        });
        let decision = UnblockingPolicy::default()
            .evaluate(&blocker, &action(ActionKind::Retry, ActionRisk::ReadOnly));
        assert!(matches!(decision, UnblockingDecision::Escalate { .. }));
    }

    #[test]
    fn successful_attempt_does_not_close_blocker_without_acceptance() {
        let mut blocker = BlockerRecord::new("b1", BlockerKind::Evidence, "source unavailable");
        blocker.record_attempt(BlockerAttempt {
            kind: ActionKind::AlternateSource,
            summary: "alternate source was reachable".to_string(),
            succeeded: true,
            evidence: EvidenceLevel::Representative,
            changed_tested_variable: true,
        });
        assert!(blocker.is_open());

        blocker.resolve();
        assert_eq!(blocker.status, BlockerStatus::Resolved);
    }

    #[test]
    fn requests_permission_for_external_or_credential_actions() {
        let blocker = BlockerRecord::new("b1", BlockerKind::Access, "missing access");
        for (kind, risk) in [
            (
                ActionKind::ExternalSideEffect,
                ActionRisk::ExternalSideEffect,
            ),
            (ActionKind::CredentialChange, ActionRisk::CredentialChange),
            (ActionKind::DestructiveChange, ActionRisk::Destructive),
        ] {
            let decision = UnblockingPolicy::default().evaluate(&blocker, &action(kind, risk));
            assert!(matches!(
                decision,
                UnblockingDecision::RequestPermission { .. }
            ));
        }
    }

    #[test]
    fn protected_targets_escalate_without_a_bypass() {
        let blocker = BlockerRecord::new("b1", BlockerKind::Safety, "protected path");
        let decision = UnblockingPolicy::default().evaluate(
            &blocker,
            &action(ActionKind::ReversibleChange, ActionRisk::Protected),
        );
        assert!(matches!(decision, UnblockingDecision::Escalate { .. }));
    }

    #[test]
    fn reversible_changes_require_acceptance_check() {
        let blocker = BlockerRecord::new("b1", BlockerKind::Configuration, "bad setting");
        let mut candidate = action(ActionKind::ReversibleChange, ActionRisk::ReversibleLocal);
        candidate.has_acceptance_check = false;
        let decision = UnblockingPolicy::default().evaluate(&blocker, &candidate);
        assert!(matches!(decision, UnblockingDecision::Reject { .. }));
    }

    #[test]
    fn exhausted_budget_escalates() {
        let mut blocker = BlockerRecord::new("b1", BlockerKind::Unknown, "stuck");
        for _ in 0..2 {
            blocker.record_attempt(BlockerAttempt {
                kind: ActionKind::Diagnose,
                summary: "diagnosis".to_string(),
                succeeded: false,
                evidence: EvidenceLevel::Indirect,
                changed_tested_variable: false,
            });
        }
        let policy = UnblockingPolicy {
            max_attempts: 2,
            max_same_route_retries: 1,
        };
        let decision = policy.evaluate(
            &blocker,
            &action(ActionKind::Research, ActionRisk::ReadOnly),
        );
        assert!(matches!(decision, UnblockingDecision::Escalate { .. }));
    }
}
