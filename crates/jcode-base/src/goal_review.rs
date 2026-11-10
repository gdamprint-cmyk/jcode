// reference: C:/Users/WorkShop1/Documents/Codex/2026-11-08/new-chat/outputs/jcode-hindsight-implementation.md
//   ("First complete feature to ship: reopen a good idea at the right time" -
//   "Use one accepted, deferred goal with a verifiable external condition.
//   Suppose the owner wanted a deliverable, but the only feasible tool
//   required payment. Record the goal key, the budget constraint, the reason
//   the old option failed and the evidence that would justify reconsideration.
//   A sentence such as 'remind me if a free option becomes usable' gives
//   Jcode an observable condition."; and "When a trusted source reports a
//   relevant change, resolve the product or capability to its entity.
//   Retrieve affected open goals, candidate evidence about the change, the
//   original constraint and any later cancellation.").
// reference: https://hindsight.vectorize.io/0.10/developer/api/reflect
//   (the `reflect` endpoint, `response_schema` and `include.facts`).
//! Reopen a deferred goal when a trusted source reports a relevant change.
//!
//! WHAT IT DOES
//! ------------
//! An accepted goal can be deferred because a condition did not hold ("the
//! only feasible tool required payment"). This module finds the goals that
//! carry such a condition, matches a reported change against the condition's
//! watch terms, and asks `reflect` to decide between the four outcomes the
//! plan names: `wait`, `prepare`, `act_within_authority` and `ask_owner`.
//!
//! WHAT IT DELIBERATELY DOES NOT DO
//! ---------------------------------
//! It does not act on the decision. The four values are a PROPOSAL to the
//! agent, not server-native action commands, and the plan says so explicitly.
//! It also does not decide on its own whether a change satisfies the
//! condition: that judgement belongs to `reflect`, which sees the owner's own
//! statements about the goal.

use anyhow::Result;
use chrono::{DateTime, Utc};
use jcode_task_types::{Goal, GoalReviewCondition};

use crate::memory_hindsight::{
    assess_reopen, recent_memories, HindsightConfig, ReopenAssessment, ReopenDecision,
};

/// A goal whose condition bears on a reported change.
#[derive(Debug, Clone)]
pub struct ReopenCandidate {
    pub goal_id: String,
    pub goal_title: String,
    /// The condition as the owner stated it.
    pub condition: String,
    /// Why the goal was deferred.
    pub blocked_because: String,
    /// The evidence that would justify reconsidering.
    pub evidence_to_look_for: String,
    /// Whether preparation was authorized in advance.
    pub preparation_authorized: bool,
    /// The score of the match between the change and the condition.
    pub match_score: f64,
    /// The watch terms the change matched.
    pub matched_terms: Vec<String>,
}

/// A candidate together with the decision `reflect` made about it.
#[derive(Debug, Clone)]
pub struct ReopenOutcome {
    pub candidate: ReopenCandidate,
    pub assessment: ReopenAssessment,
    /// Whether the owner should hear about this.
    ///
    /// A `wait` is silent by design: the plan requires that "an unrelated
    /// event, stale warning or previously declined suggestion" produces
    /// silence, and only new evidence that changes the decision reopens the
    /// matter.
    pub worth_notifying: bool,
}

/// The tokens of a text, lowercased, with the short ones dropped.
///
/// The same rule the memory model selection uses: a token shorter than four
/// characters carries no topic signal.
fn tokens(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| part.len() >= 4)
        .collect()
}

/// How strongly a reported change matches a goal's review condition.
///
/// The match is over the condition's watch terms AND over the terms of the
/// condition sentence itself, so a goal that names the entity in prose still
/// matches even when the watch list is incomplete.
///
/// The score is the fraction of the goal's watch terms that appear in the
/// change, weighted up by whether the condition's own terms appear too. A
/// change that shares NOTHING scores zero and never reaches `reflect`.
pub fn match_score(condition: &GoalReviewCondition, change: &str) -> (f64, Vec<String>) {
    let change_tokens: std::collections::HashSet<String> = tokens(change).into_iter().collect();
    if change_tokens.is_empty() {
        return (0.0, Vec::new());
    }

    let watch_terms: Vec<String> = condition
        .watch_terms
        .iter()
        .map(|term| term.trim().to_ascii_lowercase())
        .filter(|term| !term.is_empty())
        .collect();

    let mut matched: Vec<String> = Vec::new();
    let mut watch_hits = 0usize;
    for term in &watch_terms {
        let hit = change_tokens.contains(term)
            || tokens(change).iter().any(|token| token.contains(term.as_str()));
        if hit {
            watch_hits += 1;
            matched.push(term.clone());
        }
    }

    // The condition sentence contributes its own terms, so a goal that
    // describes the blocker in prose still matches.
    let condition_tokens: std::collections::HashSet<String> = tokens(&condition.condition).into_iter().collect();
    let shared_condition_terms = condition_tokens
        .intersection(&change_tokens)
        .count();

    let watch_ratio = if watch_terms.is_empty() {
        0.0
    } else {
        watch_hits as f64 / watch_terms.len() as f64
    };
    let condition_ratio = if condition_tokens.is_empty() {
        0.0
    } else {
        shared_condition_terms as f64 / condition_tokens.len() as f64
    };

    // The watch terms carry the identity of the entity, so they dominate;
    // the condition sentence only lifts an otherwise positive match.
    let score = if watch_terms.is_empty() {
        condition_ratio
    } else {
        watch_ratio + condition_ratio * 0.25
    };

    (score, matched)
}

/// Whether a change is relevant enough to ask `reflect` about it.
///
/// A change that shares no term with the condition is unrelated, and the
/// plan requires silence for it. Asking `reflect` about every change would
/// cost an LLM call per change and drown the real signal.
pub fn is_relevant(candidate_score: f64) -> bool {
    candidate_score > 0.0
}

/// The goals that carry a usable review condition.
pub fn goals_with_review_conditions(goals: &[Goal]) -> Vec<(Goal, GoalReviewCondition)> {
    goals
        .iter()
        .filter_map(|goal| {
            let condition = goal.review_condition.clone()?;
            if !condition.is_usable() {
                return None;
            }
            Some((goal.clone(), condition))
        })
        .collect()
}

/// Build the query `reflect` answers for one candidate.
///
/// The query carries the goal's own words - the condition, the reason it was
/// deferred, the evidence to look for - plus the reported change. Everything
/// `reflect` needs to judge is in the query; the bank supplies the evidence.
pub fn build_assessment_query(candidate: &ReopenCandidate, change: &str) -> String {
    let mut query = format!(
        "A deferred goal may now be reconsidered.\n\n\
         Goal id: {}\n\
         Goal title: {}\n\
         Condition to satisfy: {}\n\
         Why it was deferred: {}\n\
         Evidence that would justify reconsidering: {}\n\
         Preparation was authorized in advance: {}\n\n\
         A trusted source reported this change:\n{}\n\n\
         Decide what to do with this deferred goal now. Answer with exactly one \
         of: wait, prepare, act_within_authority, ask_owner.\n\
         - wait: the change does not really satisfy the condition, or nothing \
         material changed; stay silent.\n\
         - prepare: the condition now holds and preparation was authorized; \
         produce the useful artifact or verification.\n\
         - act_within_authority: the condition holds and the authorized step \
         can be taken directly.\n\
         - ask_owner: the condition may hold, but proceeding requires a new \
         commitment from the owner.\n\
         Ground the answer in the memories about this goal. If the owner \
         cancelled the goal meanwhile, say wait.",
        candidate.goal_id,
        candidate.goal_title,
        candidate.condition,
        candidate.blocked_because,
        candidate.evidence_to_look_for,
        candidate.preparation_authorized,
        change.trim(),
    );
    if !candidate.matched_terms.is_empty() {
        query.push_str(&format!(
            "\n\nThe change matched these watch terms: {}.",
            candidate.matched_terms.join(", ")
        ));
    }
    query
}

/// The goals a reported change bears on, best match first.
pub fn candidates_for_change(
    goals: &[Goal],
    change: &str,
) -> Vec<ReopenCandidate> {
    let mut candidates: Vec<ReopenCandidate> = goals_with_review_conditions(goals)
        .into_iter()
        .filter_map(|(goal, condition)| {
            let (score, matched) = match_score(&condition, change);
            if !is_relevant(score) {
                return None;
            }
            Some(ReopenCandidate {
                goal_id: goal.id.clone(),
                goal_title: goal.title.clone(),
                condition: condition.condition.clone(),
                blocked_because: condition.blocked_because.clone(),
                evidence_to_look_for: condition.evidence_to_look_for.clone(),
                preparation_authorized: condition.preparation_authorized,
                match_score: score,
                matched_terms: matched,
            })
        })
        .collect();
    candidates.sort_by(|left, right| {
        right
            .match_score
            .partial_cmp(&left.match_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates
}

/// Whether an assessment is worth telling the owner about.
///
/// reference: C:/Users/WorkShop1/Documents/Codex/2026-11-08/new-chat/outputs/jcode-hindsight-implementation.md
///   ("If the free tier only changed its name, do nothing."; "If it satisfies
///   the real blocker and the owner authorized preparation, produce the first
///   useful artifact or verification."; "If proceeding requires a new
///   commitment, present the choice with the evidence and prepared next
///   step."; "If the owner cancelled meanwhile, discard even an otherwise
///   excellent match.").
pub fn worth_notifying(assessment: &ReopenAssessment) -> bool {
    match assessment.decision {
        Some(ReopenDecision::Wait) => false,
        Some(ReopenDecision::Prepare)
        | Some(ReopenDecision::ActWithinAuthority)
        | Some(ReopenDecision::AskOwner) => true,
        // No decision at all is inconclusive, not an invitation. The plan
        // treats a failed decision as "stay quiet".
        None => false,
    }
}

/// Assess one candidate against a reported change.
///
/// The decision comes from `reflect`; this function only wires the query, the
/// call and the notification rule together.
pub fn assess_candidate(
    config: &HindsightConfig,
    candidate: &ReopenCandidate,
    change: &str,
) -> Result<ReopenOutcome> {
    let query = build_assessment_query(candidate, change);
    let assessment = assess_reopen(config, &query)?;
    let worth_notifying = worth_notifying(&assessment);
    Ok(ReopenOutcome {
        candidate: candidate.clone(),
        assessment,
        worth_notifying,
    })
}

/// Record the outcome of an assessment back onto the goal's condition.
///
/// The watermark and the last decision are what stop the same change from
/// being proposed twice: the plan requires "evidence of a material change
/// before repeating a suggestion".
pub fn record_assessment(condition: &mut GoalReviewCondition, assessment: &ReopenAssessment) {
    condition.last_assessed_at = Some(Utc::now());
    if let Some(decision) = assessment.decision {
        condition.last_decision = Some(decision.as_str().to_string());
    }
    condition.last_evidence_at = Some(Utc::now());
}

/// Whether a goal was cancelled after its condition was recorded.
///
/// reference: C:/Users/WorkShop1/Documents/Codex/2026-11-08/new-chat/outputs/jcode-hindsight-implementation.md
///   ("If the owner cancelled meanwhile, discard even an otherwise excellent
///   match.").
pub fn was_cancelled(goal: &Goal, condition: &GoalReviewCondition) -> bool {
    let cancelled = matches!(
        goal.status,
        jcode_task_types::GoalStatus::Abandoned | jcode_task_types::GoalStatus::Archived
    );
    if !cancelled {
        return false;
    }
    // The cancellation must come AFTER the condition was recorded, otherwise
    // the goal was already closed when the condition was set.
    match condition.last_assessed_at {
        Some(assessed) => goal.updated_at > assessed,
        None => true,
    }
}

/// The state a goal's review condition is in, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewState {
    /// No condition recorded: the goal is not a reopen candidate.
    NotApplicable,
    /// The condition is recorded but incomplete.
    Incomplete,
    /// The condition is armed and waiting for a matching change.
    Waiting,
    /// The condition was assessed at this time with this decision.
    Assessed {
        at: DateTime<Utc>,
        decision: Option<String>,
    },
}

/// The review state of a goal.
pub fn review_state(goal: &Goal) -> ReviewState {
    let Some(condition) = goal.review_condition.as_ref() else {
        return ReviewState::NotApplicable;
    };
    if !condition.is_usable() {
        return ReviewState::Incomplete;
    }
    match (condition.last_assessed_at, condition.last_decision.as_deref()) {
        (Some(at), Some(decision)) => ReviewState::Assessed {
            at,
            decision: Some(decision.to_string()),
        },
        (Some(at), None) => ReviewState::Assessed { at, decision: None },
        (None, _) => ReviewState::Waiting,
    }
}

/// The outcome of a whole sweep: every armed goal against every new event.
#[derive(Debug, Clone, Default)]
pub struct ReopenSweep {
    /// The outcomes that deserve the owner's attention.
    pub notable: Vec<ReopenOutcome>,
    /// How many goals carried an armed condition.
    pub armed_goals: usize,
    /// How many events were examined.
    pub events_examined: usize,
}

/// The goals that carry an ARMED condition: usable, and not yet cancelled.
pub fn armed_goals(goals: &[Goal]) -> Vec<(Goal, GoalReviewCondition)> {
    goals_with_review_conditions(goals)
        .into_iter()
        .filter(|(goal, condition)| !was_cancelled(goal, condition))
        .collect()
}

/// Examine new bank events against the armed goals.
///
/// This is the trigger half of the plan's feature: the bank's own records of
/// what happened are the "trusted source reports a relevant change" events.
/// A goal is only assessed when an event actually bears on its condition, so
/// an unrelated event costs nothing.
///
/// The events are read by POLLING the bank's memories list rather than by a
/// webhook. Upstream offers both: `retain.completed` and
/// `consolidation.completed` webhooks ("Hindsight can notify your
/// application in real-time when memory events occur by sending HTTP POST
/// requests to a URL you configure"), and the plan names them as the event
/// source. A webhook needs a running receiver endpoint, which the TUI does
/// not have; polling needs nothing and reuses the same connection the
/// recall already uses. The webhook route remains the right one for a
/// long-running daemon that wants the push.
///
/// reference: C:/Users/WorkShop1/Documents/Codex/2026-11-08/new-chat/outputs/jcode-hindsight-implementation.md
///   ("When a trusted source reports a relevant change, resolve the product
///   or capability to its entity. Retrieve affected open goals, candidate
///   evidence about the change, the original constraint and any later
///   cancellation."; and "Require evidence of a material change before
///   repeating a suggestion.").
/// reference: https://hindsight.vectorize.io/0.10/developer/api/webhooks
///   (the event types and their at-least-once delivery, which is why the
///   polling route also needs a watermark rather than trusting each event).
pub fn sweep_for_reopens(
    config: &HindsightConfig,
    goals: &[Goal],
    since: DateTime<Utc>,
    limit: usize,
) -> Result<ReopenSweep> {
    let armed = armed_goals(goals);
    let mut sweep = ReopenSweep {
        armed_goals: armed.len(),
        ..Default::default()
    };
    if armed.is_empty() {
        return Ok(sweep);
    }

    let events = recent_memories(config, since, limit)?;
    sweep.events_examined = events.len();

    for event in &events {
        for (goal, condition) in &armed {
            let (score, matched) = match_score(condition, &event.text);
            if !is_relevant(score) {
                continue;
            }
            let candidate = ReopenCandidate {
                goal_id: goal.id.clone(),
                goal_title: goal.title.clone(),
                condition: condition.condition.clone(),
                blocked_because: condition.blocked_because.clone(),
                evidence_to_look_for: condition.evidence_to_look_for.clone(),
                preparation_authorized: condition.preparation_authorized,
                match_score: score,
                matched_terms: matched,
            };
            let outcome = assess_candidate(config, &candidate, &event.text)?;
            if outcome.worth_notifying {
                sweep.notable.push(outcome);
            }
        }
    }

    Ok(sweep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_task_types::{GoalScope, GoalStatus};

    fn condition(watch: &[&str], text: &str) -> GoalReviewCondition {
        GoalReviewCondition {
            condition: text.to_string(),
            blocked_because: "the only feasible tool required payment".to_string(),
            evidence_to_look_for: "a usable free tier".to_string(),
            watch_terms: watch.iter().map(|term| term.to_string()).collect(),
            preparation_authorized: true,
            ..Default::default()
        }
    }

    fn goal_with(condition: GoalReviewCondition) -> Goal {
        let mut goal = Goal::new("Produce the deliverable", GoalScope::Global);
        goal.review_condition = Some(condition);
        goal
    }

    /// The plan's own example: a free tier becoming usable must match.
    #[test]
    fn a_reported_free_tier_matches_the_deferred_goal() {
        let goal = goal_with(condition(&["free", "tier"], "a free tier becomes usable"));
        let candidates = candidates_for_change(&[goal], "Provider now offers a free tier with a usable quota");
        assert_eq!(candidates.len(), 1, "the change must reach the goal");
        let candidate = &candidates[0];
        assert_eq!(candidate.goal_id, "produce-the-deliverable");
        assert!(
            candidate.matched_terms.iter().any(|term| term == "free"),
            "the watch term must be reported as matched: {:?}",
            candidate.matched_terms
        );
    }

    /// An unrelated change must NOT reach the goal: the plan requires silence
    /// for it, and asking `reflect` would cost an LLM call per change.
    #[test]
    fn an_unrelated_change_reaches_no_goal() {
        let goal = goal_with(condition(&["free", "tier"], "a free tier becomes usable"));
        let candidates = candidates_for_change(&[goal], "The office coffee machine was replaced on Tuesday");
        assert!(
            candidates.is_empty(),
            "an unrelated change must not produce a candidate: {candidates:?}"
        );
    }

    /// A goal without a condition is not a reopen candidate at all.
    #[test]
    fn a_goal_without_a_condition_is_never_a_candidate() {
        let mut goal = Goal::new("Plain goal", GoalScope::Global);
        goal.review_condition = None;
        let candidates = candidates_for_change(&[goal], "anything at all changed");
        assert!(candidates.is_empty());
    }

    /// A condition without watch terms cannot be matched without asking the
    /// owner, so it is not usable.
    #[test]
    fn a_condition_without_watch_terms_is_incomplete() {
        let mut condition = condition(&[], "a free tier becomes usable");
        assert!(!condition.is_usable());
        condition.watch_terms = vec!["free".to_string()];
        assert!(condition.is_usable());
    }

    /// The four decisions: only `wait` is silent.
    #[test]
    fn only_wait_is_silent() {
        let mut assessment = ReopenAssessment::default();
        assessment.decision = Some(ReopenDecision::Wait);
        assert!(!worth_notifying(&assessment));

        for decision in [
            ReopenDecision::Prepare,
            ReopenDecision::ActWithinAuthority,
            ReopenDecision::AskOwner,
        ] {
            assessment.decision = Some(decision);
            assert!(
                worth_notifying(&assessment),
                "{decision:?} must reach the owner"
            );
        }
    }

    /// No decision at all is inconclusive, and inconclusive means silence.
    #[test]
    fn an_inconclusive_assessment_is_silent() {
        let assessment = ReopenAssessment::default();
        assert!(!worth_notifying(&assessment));
    }

    /// A cancelled goal must not be reopened, even by a perfect match.
    #[test]
    fn a_cancelled_goal_is_discarded() {
        let mut condition = condition(&["free", "tier"], "a free tier becomes usable");
        let mut goal = goal_with(condition.clone());
        goal.status = GoalStatus::Abandoned;
        goal.updated_at = Utc::now();

        // Cancelled BEFORE the last assessment: the cancellation is newer
        // than the condition, so the goal is discarded.
        condition.last_assessed_at = Some(Utc::now() - chrono::Duration::hours(1));
        assert!(was_cancelled(&goal, &condition));

        // Cancelled BEFORE any assessment: also discarded.
        let mut fresh = condition.clone();
        fresh.last_assessed_at = None;
        assert!(was_cancelled(&goal, &fresh));
    }

    /// A live goal is never discarded.
    #[test]
    fn a_live_goal_is_not_discarded() {
        let condition = condition(&["free", "tier"], "a free tier becomes usable");
        let goal = goal_with(condition.clone());
        assert!(!was_cancelled(&goal, &condition));
    }

    /// The assessment query must carry the goal's own words, otherwise
    /// `reflect` cannot judge whether the condition holds.
    #[test]
    fn the_assessment_query_carries_the_goal_and_the_change() {
        let goal = goal_with(condition(&["free", "tier"], "a free tier becomes usable"));
        let candidates = candidates_for_change(&[goal], "Provider now offers a free tier");
        let candidate = &candidates[0];
        let query = build_assessment_query(candidate, "Provider now offers a free tier");

        assert!(query.contains("produce-the-deliverable"), "{query}");
        assert!(query.contains("a free tier becomes usable"), "{query}");
        assert!(query.contains("required payment"), "{query}");
        assert!(query.contains("Provider now offers a free tier"), "{query}");
        for decision in ["wait", "prepare", "act_within_authority", "ask_owner"] {
            assert!(query.contains(decision), "the query must name {decision}: {query}");
        }
    }

    /// A cancelled goal must not be armed, even with a usable condition.
    #[test]
    fn a_cancelled_goal_is_not_armed() {
        let mut condition = condition(&["free", "tier"], "a free tier becomes usable");
        let mut goal = goal_with(condition.clone());
        condition.last_assessed_at = Some(Utc::now() - chrono::Duration::hours(1));
        goal.review_condition = Some(condition.clone());
        goal.status = GoalStatus::Abandoned;
        goal.updated_at = Utc::now();

        let armed = armed_goals(std::slice::from_ref(&goal));
        assert!(
            armed.is_empty(),
            "a cancelled goal must not be armed: {armed:?}"
        );
    }

    /// A live goal with a usable condition IS armed.
    #[test]
    fn a_live_goal_with_a_condition_is_armed() {
        let goal = goal_with(condition(&["free", "tier"], "a free tier becomes usable"));
        let armed = armed_goals(std::slice::from_ref(&goal));
        assert_eq!(armed.len(), 1, "the goal must be armed");
    }

    /// The review state tells the owner whether a goal is armed, waiting or
    /// already assessed.
    #[test]
    fn the_review_state_reflects_the_recorded_condition() {
        let mut goal = Goal::new("Plain", GoalScope::Global);
        assert_eq!(review_state(&goal), ReviewState::NotApplicable);

        goal.review_condition = Some(condition(&[], "text"));
        assert_eq!(review_state(&goal), ReviewState::Incomplete);

        goal.review_condition = Some(condition(&["free"], "text"));
        assert_eq!(review_state(&goal), ReviewState::Waiting);

        let mut assessed = condition(&["free"], "text");
        assessed.last_assessed_at = Some(Utc::now());
        assessed.last_decision = Some("wait".to_string());
        goal.review_condition = Some(assessed);
        assert!(matches!(
            review_state(&goal),
            ReviewState::Assessed { .. }
        ));
    }
}
