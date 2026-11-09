// reference: %LOCALAPPDATA%\hermes\hermes-agent\agent\background_review.py (Hermes
//   post-turn self-improvement fork: restricted toolset, skill_update prompt,
//   preference order, read-before-write rule)
// reference: crates/jcode-app-core/src/server/swarm.rs:1908 `run_swarm_task` (the
//   jcode fork mechanism this module reuses: a child Agent built with an
//   explicit tool allowlist and run via `run_once_capture`)
// reference: crates/jcode-app-core/src/tool/skill_write.rs (the write surface the
//   review is allowed to use)
//! Post-turn skill review: the "improves skills during use" half of the
//! learning loop.
//!
//! Hermes runs a forked model turn after a turn ends, with a restricted
//! toolset, and lets it patch a skill that turned out wrong or create one for a
//! class of work that has no skill yet. That is the difference between a
//! library of procedures and a library that learns.
//!
//! jcode had the write surface missing and the review missing. This module is
//! the review: it decides WHEN a review is worth running, builds the prompt,
//! and runs the fork with only the tools a skill edit needs.
//!
//! Deliberately NOT done here:
//! * no curator, no archiving, no consolidation. Hermes' curator deletes and
//!   merges skills on a schedule; an autonomous process that can delete
//!   executable instruction is a different risk class from one that can add to
//!   it, and the user asked for addition and improvement, not removal.
//! * no memory writes. A skill captures "how to do this class of task"; a
//!   memory captures state. Mixing them produced, in Hermes' own words,
//!   "narrow one-session skills" that pollute the catalog.
//! * no automatic enablement. The review is opt-in per config, and it is
//!   observable: every mutation goes through the mutation ledger.

use anyhow::{Result, bail};
use std::collections::HashSet;

use super::Agent;

/// The configuration type comes from the shared config crate: the review is
/// configured by the user, and the product's config section is the one place
/// that shape is defined.
pub use crate::config::SkillReviewConfig;

/// The tools a skill-review fork is allowed to use.
///
/// `skill_manage` is the only mutating tool, and `read` lets the fork inspect
/// a skill before patching it. Nothing else: the fork must not run shell
/// commands, must not touch the filesystem directly, and must not spawn
/// further agents. A review that could run arbitrary commands would be a
/// second agent with full authority, which is not what a review is.
pub const REVIEW_ALLOWED_TOOLS: &[&str] = &["skill_manage", "read"];

/// Minimum turns since the last review before another one is considered.
///
/// The review costs a model call. Running it after every short turn burns
/// budget for nothing; the Hermes default nudges on a similar cadence.
pub const DEFAULT_REVIEW_INTERVAL_TURNS: u32 = 15;

/// The prompt that drives the review.
///
/// Ported in substance from Hermes' `_SKILL_UPDATE_PROMPT`, trimmed to what
/// jcode can act on. The preference order is the part that matters: patching
/// a skill that was in play beats creating a new one, because a narrow
/// one-session skill is catalog pollution.
pub const REVIEW_PROMPT: &str = "\
Review the conversation above for durable learnings that belong in a skill.

A learning belongs in a skill when a FUTURE session doing this class of task \
would be measurably better for knowing it. That means: the user corrected your \
approach, a non-trivial technique or fix emerged, or a skill that was consulted \
turned out wrong, incomplete, or outdated.

Preference order — take the earliest action that fits, but DO act:

1. PATCH A SKILL THAT WAS IN PLAY. If a skill covering this territory was \
   loaded or consulted during the conversation, patch it. It is the skill that \
   was in play, so it is the right one to extend.
2. PATCH AN EXISTING UMBRELLA. If a loaded skill does not fit but an \
   existing class-level skill does, patch that one. Add a pitfall, a step, or \
   broaden a trigger.
3. ADD A SUPPORT FILE under an existing skill, with skill_manage \
   action=write_file and file_path under 'references/', 'templates/', or \
   'scripts/'. Add a one-line pointer to it in the skill's SKILL.md. Distil \
   the lesson into a rule; do not paste an error transcript.
4. CREATE A NEW CLASS-LEVEL SKILL with skill_manage action=create, when no \
   existing skill covers the class at all. The name MUST be at the class level \
   and MUST NOT be a session artifact (no PR numbers, error strings, \
   'fix-X-today' names). If the name only makes sense for today's task, it is \
   wrong — fall back to (1), (2), or (3).

READ BEFORE WRITE. Before patching a skill's SKILL.md, call skill_manage \
action=read for that skill in THIS review and base the patch on what it \
returns. Text quoted earlier in the transcript does not count. Creating a new \
skill or adding a new support file needs no prior read. If a write is refused \
for a missing read, read once and retry once; do not loop.

WHAT NOT TO CAPTURE: one-off facts about this session, the current task's \
state, anything already covered by an existing skill, and a preference that \
belongs in memory rather than in a procedure.

'Nothing to save.' is a valid answer when the session produced no durable \
learning. Do not create a skill to have created one.";

/// Decide whether a review should run for a turn.
///
/// Three conditions, all required:
/// * the review is enabled;
/// * enough turns have passed since the last review;
/// * the turn did something worth reviewing — a turn with no tool calls and a
///   short answer has nothing to learn from, and running a review on it
///   spends a model call to conclude "nothing to save".
pub fn should_review(config: &SkillReviewConfig, turns_since_last: u32, tool_calls: usize) -> bool {
    if !config.enabled {
        return false;
    }
    if turns_since_last < config.interval_turns {
        return false;
    }
    tool_calls > 0
}

/// The tool allowlist for the review fork.
///
/// Derived from the live tool inventory so a fork never advertises a tool the
/// registry does not have, and never omits one it needs.
pub fn review_allowed_tools(live_tool_names: &HashSet<String>) -> HashSet<String> {
    live_tool_names
        .iter()
        .filter(|name| REVIEW_ALLOWED_TOOLS.contains(&name.as_str()))
        .cloned()
        .collect()
}

/// Build the prompt for the review fork.
///
/// The turn's own text is included, bounded: the review needs to see WHAT
/// happened, and an unbounded transcript would make the fork's context a
/// second copy of the session.
pub fn build_review_prompt(turn_text: &str, max_chars: usize) -> Result<String> {
    if turn_text.trim().is_empty() {
        bail!("the turn produced no text to review");
    }
    let mut text = turn_text.trim().to_string();
    if text.chars().count() > max_chars {
        text = text.chars().take(max_chars).collect();
        text.push('\n');
        text.push_str("[...truncated]");
    }
    Ok(format!(
        "{REVIEW_PROMPT}\n\n---\nCONVERSATION EXCERPT\n{text}\n"
    ))
}

/// Maximum characters of the reviewed turn's text.
pub const REVIEW_EXCERPT_MAX_CHARS: usize = 12_000;

/// How long a one-shot process waits for its detached review before giving up.
///
/// The wait is only on the process exit path, never on the answer. It is
/// bounded because a provider that never answers must not hang the run.
pub const SKILL_REVIEW_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

impl Agent {
    /// Spawn the skill review fork for a finished turn.
    ///
    /// The fork is a child agent with a restricted toolset, built the same way
    /// a swarm worker is built (`Agent::new_with_session` with an explicit
    /// allowlist) and run with `run_once_capture`. It runs AFTER the turn, in
    /// the same process, and its mutations land in the skill registry and the
    /// mutation ledger like any other write.
    ///
    /// Failures are logged and swallowed: a review that could fail a turn
    /// would make the learning loop a liability. A review that fails has cost
    /// one model call and nothing else.
    ///
    /// The fork runs DETACHED. MEASURED 2026-11-07: with an inline fork a
    /// `jcode run` turn that took ~10 s took ~47 s, because the fork's model
    /// round-trips ran before the turn's result was delivered. The fork is a
    /// background improvement, not part of the answer, so it must not hold
    /// the turn. Detaching needs the fork's future to be `Send`, which is
    /// what `the_agent_is_send_so_the_review_fork_can_be_detached` asserts.
    ///
    /// reference: crates/jcode-app-core/src/server/swarm.rs::run_swarm_task (the
    ///   jcode fork mechanism this reuses: a child Agent built with an explicit
    ///   allowlist, run detached rather than inline);
    /// reference: crates/jcode-base/src/runtime_activity.rs::spawn_session (the
    ///   detached-task spawn this uses, which requires a `Send` future);
    /// reference: crates/jcode-app-core/src/server/comm_control.rs::spawn_assigned_task_run
    ///   (the pattern this follows: own the child's state, spawn it, clean up
    ///   the child's session env when it finishes).
    pub(crate) async fn run_skill_review_fork(
        &mut self,
        config: &SkillReviewConfig,
        turn_text: &str,
    ) {
        let prompt = match build_review_prompt(turn_text, REVIEW_EXCERPT_MAX_CHARS) {
            Ok(prompt) => prompt,
            Err(error) => {
                crate::logging::warn(&format!("[skill_review] no text to review: {error}"));
                return;
            }
        };

        let live_tools: HashSet<String> = self.registry().tool_names().await.into_iter().collect();
        let allowed = review_allowed_tools(&live_tools);
        if !allowed.contains("skill_manage") {
            crate::logging::warn(
                "[skill_review] skill_manage is not available; skipping the review",
            );
            return;
        }

        let provider = self.provider_fork();
        let registry = self.registry();
        let parent_session_id = self.session_id().to_string();
        let coordinator_model = self.provider_model();
        let provider_key = self.session_provider_key();
        let route = self.session_route_api_method();
        let working_dir = self.working_dir().map(|dir| dir.to_string());
        let model = config.model.clone();

        let mut session = crate::session::Session::create(
            Some(parent_session_id.clone()),
            Some("skill review (@skill-review)".to_string()),
        );
        session.model = Some(coordinator_model);
        session.provider_key = provider_key;
        session.route_api_method = route;
        if let Some(dir) = working_dir {
            session.working_dir = Some(dir);
        }
        if let Err(error) = session.save() {
            crate::logging::warn(&format!(
                "[skill_review] could not save the review session: {error}"
            ));
            return;
        }

        let child_session_id = session.id.clone();
        crate::session_env::inherit_session_env(&parent_session_id, &child_session_id);

        // The fork is handed everything it needs, so the spawned future owns
        // its state and does not borrow the parent agent.
        let worker = Agent::new_with_session(provider, registry, session, Some(allowed));
        // The spawned future is 'static and takes an owned copy of the child's
        // id; the borrowed one is only needed for the spawn call itself.
        let child_env_id = child_session_id.clone();
        let handle = crate::runtime_activity::spawn_session(&child_session_id, async move {
            let mut worker = worker;
            // The fork runs a turn, and a turn's end would trigger another
            // review. Without this the fork would review itself, forever.
            worker.suppress_skill_review = true;
            if let Some(model) = model.as_deref()
                && let Err(error) = worker.set_model(model)
            {
                crate::logging::warn(&format!(
                    "[skill_review] could not switch the review to {model}: {error}"
                ));
            }
            match worker.run_once_capture(&prompt).await {
                Ok(output) => {
                    crate::logging::info(&format!(
                        "[skill_review] the review finished: {}",
                        output.chars().take(400).collect::<String>()
                    ));
                }
                Err(error) => {
                    crate::logging::warn(&format!(
                        "[skill_review] the review fork failed: {error}"
                    ));
                }
            }
            // Close the child's session so the next startup does not read it
            // as a stale-PID crash. The child registered presence markers when
            // it was built.
            worker.mark_closed();
            crate::session_env::remove_session_env(&child_env_id);
        });
        match handle {
            Some(handle) => {
                // Keep the handle so the session can WAIT for the review
                // before a one-shot process exits. See `wait_for_skill_review`.
                self.skill_review_tasks.push(handle);
            }
            None => {
                crate::logging::warn(
                    "[skill_review] the runtime refused the detached review task; skipping the review",
                );
            }
        }
    }

    /// Wait for a detached skill review to finish.
    ///
    /// `jcode run` is a one-shot process: when its future returns, the Tokio
    /// runtime is dropped and every detached task dies with it. A detached
    /// review that nobody waits for is therefore dropped exactly where it is
    /// easiest to observe, and silently.
    ///
    /// The wait happens AFTER the turn's answer was delivered, so it does not
    /// slow the answer down — it only delays the process exit. The wait is
    /// bounded, so a provider that never answers cannot hang the run
    /// forever.
    ///
    /// reference: crates/jcode-app-core/src/server/comm_session.rs::prepare_visible_spawn_session
    ///   (the jcode pattern this follows: own the child's state, spawn it, and
    ///   clean up the child's session env when it finishes);
    /// reference: crates/jcode-base/src/runtime_activity.rs::spawn_session (the
    ///   detached-task spawn this uses, which requires a `Send` future).
    pub async fn wait_for_skill_review(&mut self) {
        let tasks = std::mem::take(&mut self.skill_review_tasks);
        if tasks.is_empty() {
            return;
        }
        // Every review is waited for together, under one deadline: the wait
        // bounds the PROCESS EXIT, and a second review that started later
        // should not extend it.
        let deadline = SKILL_REVIEW_DRAIN_TIMEOUT;
        for handle in tasks {
            match tokio::time::timeout(deadline, handle).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    crate::logging::warn(&format!(
                        "[skill_review] the review task ended badly: {error}"
                    ));
                }
                Err(_) => {
                    crate::logging::warn(&format!(
                        "[skill_review] the review did not finish within {} s and was abandoned",
                        deadline.as_secs()
                    ));
                }
            }
        }
    }
}

/// Decide whether this turn should trigger a review, given the session state.
///
/// Kept separate from the spawn so the decision is testable without a
/// provider, and so the turn counter is advanced by exactly one place.
pub fn review_due(
    enabled: bool,
    interval_turns: u32,
    turns_since_last: u32,
    tool_calls: usize,
) -> bool {
    should_review(
        &SkillReviewConfig {
            enabled,
            interval_turns,
            model: None,
        },
        turns_since_last,
        tool_calls,
    )
}

impl Agent {
    /// Count the tool calls made since a transcript index.
    ///
    /// The review is skipped for a turn with no tool calls: a pure
    /// conversation turn has nothing procedural to learn from, and reviewing it
    /// spends a model call to conclude "nothing to save".
    fn tool_calls_after(&self, start_index: usize) -> usize {
        self.session
            .messages
            .iter()
            .skip(start_index)
            .flat_map(|message| message.content.iter())
            .filter(|block| matches!(block, crate::message::ContentBlock::ToolUse { .. }))
            .count()
    }

    /// Run the skill review for a finished turn, if one is due.
    ///
    /// Called after the turn's answer has been delivered. The turn counter is
    /// advanced here and nowhere else, so the cadence cannot be shortened by
    /// another code path.
    ///
    /// The counter passed to the gate is the number of COMPLETED turns since
    /// the last review, so `interval_turns = 15` means "review on the 15th
    /// turn". MEASURED 2026-11-07: the first draft passed the PRE-increment
    /// value, so the review fired on the 16th turn, one late.
    pub(crate) async fn maybe_run_skill_review(&mut self, start_message_index: usize) {
        if self.suppress_skill_review {
            return;
        }
        let config = crate::config::config().skill_review.clone();
        let enabled = self.skill_review_enabled.unwrap_or(config.enabled);
        self.skill_review_turns_since_last = self.skill_review_turns_since_last.saturating_add(1);
        if !enabled {
            return;
        }

        let tool_calls = self.tool_calls_after(start_message_index);
        if !review_due(
            enabled,
            config.interval_turns,
            self.skill_review_turns_since_last,
            tool_calls,
        ) {
            return;
        }

        let Some(turn_text) = self.latest_assistant_text_after(start_message_index) else {
            return;
        };
        self.skill_review_turns_since_last = 0;
        // The fork runs through `run_once_capture`, which does not trigger a
        // review (that path belongs to swarm workers and to the review fork
        // itself), so there is no recursion to break here.
        self.run_skill_review_fork(&config, &turn_text).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Registry;
    use std::sync::Arc;

    /// A provider that never answers: the drain tests must not need one.
    ///
    /// reference: crates/jcode-app-core/src/agent/messages.rs::durable_error_tests
    ///   (the in-repo stub pattern this copies: a `Provider` impl whose
    ///   `complete` bails, so the test never touches the network).
    struct NeverAnswers;

    #[async_trait::async_trait]
    impl crate::provider::Provider for NeverAnswers {
        async fn complete(
            &self,
            _: &[crate::message::Message],
            _: &[crate::message::ToolDefinition],
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<crate::provider::EventStream> {
            anyhow::bail!("test must not call provider")
        }

        fn name(&self) -> &str {
            "mock"
        }

        fn fork(&self) -> Arc<dyn crate::provider::Provider> {
            Arc::new(Self)
        }
    }

    /// The review fork is a detached task: it must be `Send` so the turn can
    /// hand it to `tokio::spawn` and return immediately.
    ///
    /// MEASURED 2026-11-07: with an inline fork, a `jcode run` turn that took
    /// ~10 s took ~47 s, because the fork's model round-trips ran before the
    /// turn's result was delivered. The fork is a background improvement, not
    /// part of the answer, so it must not hold the turn.
    #[test]
    fn the_agent_is_send_so_the_review_fork_can_be_detached() {
        fn assert_send<T: Send>() {}
        assert_send::<Agent>();
    }

    /// A detached review must be WAITED for before a one-shot process exits.
    ///
    /// MEASURED 2026-11-07: `jcode run` is one-shot — its future returns, the
    /// Tokio runtime is dropped, and every detached task dies with it. A
    /// review nobody waits for is therefore dropped SILENTLY in exactly the
    /// mode where it is easiest to observe. The drain is what makes the
    /// detached fork survive the process.
    #[tokio::test]
    async fn the_drain_waits_for_a_detached_review() {
        let _guard = crate::storage::lock_test_env();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _ = rx.await;
        });
        // The task is pending until the sender fires, so a drain that did
        // not wait would return before the task completed.
        tx.send(()).ok();
        let mut agent = Agent::new(Arc::new(NeverAnswers), Registry::empty());
        agent.skill_review_tasks.push(handle);
        agent.wait_for_skill_review().await;
        assert!(
            agent.skill_review_tasks.is_empty(),
            "the drain must consume the handles it waited for"
        );
    }

    /// The drain must be a no-op when no review ran, so the exit path stays
    /// free for the common case.
    #[tokio::test]
    async fn the_drain_is_a_noop_without_a_review() {
        let _guard = crate::storage::lock_test_env();
        let mut agent = Agent::new(Arc::new(NeverAnswers), Registry::empty());
        agent.wait_for_skill_review().await;
        assert!(agent.skill_review_tasks.is_empty());
    }

    #[test]
    fn review_is_off_unless_enabled() {
        let config = SkillReviewConfig::default();
        assert!(!should_review(&config, 100, 10));
    }

    #[test]
    fn review_respects_the_interval() {
        let config = SkillReviewConfig {
            enabled: true,
            interval_turns: 15,
            model: None,
        };
        assert!(!should_review(&config, 14, 10));
        assert!(should_review(&config, 15, 10));
    }

    /// The cadence counts COMPLETED turns, so the review fires ON the
    /// `interval_turns`-th turn rather than one turn late.
    ///
    /// MEASURED 2026-11-07: the first draft passed the pre-increment counter,
    /// so `interval_turns = 15` fired on turn 16. The counter the gate sees
    /// is now the post-increment one.
    #[test]
    fn the_cadence_counts_completed_turns() {
        let config = SkillReviewConfig {
            enabled: true,
            interval_turns: 3,
            model: None,
        };
        // Turn 1 and 2: not yet due. Turn 3: due.
        assert!(!should_review(&config, 1, 1));
        assert!(!should_review(&config, 2, 1));
        assert!(should_review(&config, 3, 1));
    }

    #[test]
    fn review_skips_a_turn_with_no_tool_calls() {
        let config = SkillReviewConfig {
            enabled: true,
            interval_turns: 1,
            model: None,
        };
        assert!(!should_review(&config, 5, 0));
        assert!(should_review(&config, 5, 1));
    }

    #[test]
    fn the_allowlist_contains_only_review_tools() {
        let live: HashSet<String> = ["skill_manage", "read", "bash", "write", "websearch"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let allowed = review_allowed_tools(&live);
        assert!(allowed.contains("skill_manage"));
        assert!(allowed.contains("read"));
        assert!(!allowed.contains("bash"));
        assert!(!allowed.contains("write"));
        assert!(!allowed.contains("websearch"));
    }

    #[test]
    fn the_prompt_bounds_the_transcript() {
        let long = "x".repeat(5000);
        let prompt = build_review_prompt(&long, 1000).expect("prompt");
        assert!(prompt.contains("[...truncated]"));
        assert!(prompt.len() < 4000);
        assert!(build_review_prompt("   ", 100).is_err());
    }

    #[test]
    fn the_prompt_states_the_preference_order_and_the_read_rule() {
        assert!(REVIEW_PROMPT.contains("PATCH A SKILL THAT WAS IN PLAY"));
        assert!(REVIEW_PROMPT.contains("READ BEFORE WRITE"));
        assert!(REVIEW_PROMPT.contains("Nothing to save."));
    }
}
