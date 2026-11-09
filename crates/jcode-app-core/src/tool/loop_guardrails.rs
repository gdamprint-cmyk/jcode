//! Per-turn TOOL-LOOP GUARDRAILS: detect and stop the model from repeating
//! the same failing or no-progressing tool call.
//!
//! WHY THIS EXISTS
//! ---------------
//! Hermes (Nous Research) ships a `tool_loop_guardrails` config section and a
//! per-turn controller for it. jcode had a WRITTEN rule in the overlay ("second
//! failure ends approach") and a `pre_tool` gate, but no measured counter, so
//! nothing stopped a model from retrying the same failing call until the
//! iteration budget ran out. This is that counter, in process.
//!
//! WHAT IT DETECTS
//! ---------------
//! Three distinct failure classes, deliberately not one:
//!
//! * `exact_failure` — the SAME call with the SAME arguments failed again.
//!   A replay, not an experiment.
//! * `same_tool_failure` — the same TOOL failed again with DIFFERENT
//!   arguments. For tools whose non-zero exit is ordinary output (a red test
//!   run, an empty grep) this is DIAGNOSIS, so it only warns and never halts.
//! * `idempotent_no_progress` — a read-only call returned the same result
//!   again and again. Nothing is changing.
//!
//! And the "edit then re-run is never a loop" rule: a successful MUTATING call
//! marks progress for every failing call still being counted, so the next
//! identical retry starts a fresh streak instead of accumulating toward a
//! block. Without that rule the guardrail would punish correct iteration.
//!
//! SHAPE
//! -----
//! Per-turn state keyed by session id, a decision made BEFORE the call, and a
//! reset when a new turn begins. The reset is what makes it a per-turn guard
//! rather than a per-session one: a legitimate multi-turn session is never
//! starved, but a single turn that spirals is stopped.
//!
//! reference: C:\Users\WorkShop1\AppData\Local\hermes\hermes-agent\agent\tool_guardrails.py
//!   - the reference implementation (`ToolCallGuardrailController`,
//!   `before_call` / `after_call` / `reset_for_turn`, the `_DECISION_MESSAGES`
//!   table, and the `IDEMPOTENT_TOOL_NAMES` / `MUTATING_TOOL_NAMES` /
//!   `PROGRESS_RESET_TOOL_NAMES` sets).
//! reference: C:\Users\WorkShop1\AppData\Local\hermes\hermes-agent\website\docs\user-guide\configuration.md
//!   - the official configuration contract for the `tool_loop_guardrails`
//!   section (warn_after / hard_stop_after / loop_caps).

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use serde_json::Value;

/// Per-session, per-turn controller state.
// reference: crates/jcode-app-core/src/tool/plan_declaration.rs - the same
// per-session `static LazyLock<RwLock<HashMap<String, ...>>>` pattern keyed by
// session id, with poison-tolerant access.
static SESSION_CONTROLLERS: LazyLock<RwLock<HashMap<String, LoopGuardrailController>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Thresholds and switches. Defaults match Hermes' shipped config.
#[derive(Debug, Clone, PartialEq)]
pub struct LoopGuardrailConfig {
    /// Inject warnings into the tool result so the model self-corrects.
    pub warnings_enabled: bool,
    /// Also BLOCK the call past the hard-stop threshold.
    pub hard_stop_enabled: bool,
    /// Identical failing call repeated N times -> warn.
    pub exact_failure_warn_after: u32,
    /// Identical failing call repeated N times -> block.
    pub exact_failure_block_after: u32,
    /// Same tool failing N times (different args) -> warn.
    pub same_tool_failure_warn_after: u32,
    /// Same tool failing N times (different args) -> block.
    pub same_tool_failure_block_after: u32,
    /// Same result, no progress, N times -> warn.
    pub no_progress_warn_after: u32,
    /// Same result, no progress, N times -> block.
    pub no_progress_block_after: u32,
    /// Per-turn ceiling on web-search calls. 0 disables the cap.
    pub max_web_searches_per_turn: u32,
}

impl Default for LoopGuardrailConfig {
    fn default() -> Self {
        Self {
            warnings_enabled: true,
            hard_stop_enabled: false,
            exact_failure_warn_after: 2,
            exact_failure_block_after: 5,
            same_tool_failure_warn_after: 3,
            same_tool_failure_block_after: 8,
            no_progress_warn_after: 2,
            no_progress_block_after: 5,
            max_web_searches_per_turn: 50,
        }
    }
}

/// What the controller decided about a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardrailAction {
    /// Nothing to say.
    Allow,
    /// Say this to the model; the call still runs.
    Warn,
    /// Refuse the call.
    Block,
}

/// The decision, with the message the model should see.
#[derive(Debug, Clone, PartialEq)]
pub struct GuardrailDecision {
    pub action: GuardrailAction,
    /// Stable identifier of the detector that fired.
    pub code: &'static str,
    pub message: String,
    pub tool_name: String,
    /// How many times the pattern occurred.
    pub count: u32,
}

impl GuardrailDecision {
    fn allow() -> Self {
        Self {
            action: GuardrailAction::Allow,
            code: "allow",
            message: String::new(),
            tool_name: String::new(),
            count: 0,
        }
    }

    /// Whether the call may proceed.
    pub fn allows_execution(&self) -> bool {
        !matches!(self.action, GuardrailAction::Block)
    }
}

/// Stable identity of a tool call: the tool name plus a hash of its arguments.
///
/// The hash makes "the same call" checkable without keeping the arguments
/// themselves, so a secret in an argument is never retained here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCallSignature {
    pub tool_name: String,
    pub args_hash: u64,
}

impl ToolCallSignature {
    pub fn from_call(tool_name: &str, args: &Value) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            args_hash: hash_args(args),
        }
    }
}

/// FNV-1a over the canonical (sorted-key) form of the arguments.
///
/// A non-cryptographic hash is the right tool here: the property needed is
/// "are these two argument sets equal", not resistance to an adversary.
// reference: Hermes' `ToolCallSignature.from_call` uses sha256 for the same
// purpose; FNV-1a keeps this module dependency-free.
fn hash_args(args: &Value) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in canonical_args(args).as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The arguments in a canonical form: object keys sorted, compact separators.
fn canonical_args(args: &Value) -> String {
    match args {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .iter()
                .map(|key| format!("{key:?}:{}", canonical_args(&map[*key])))
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical_args).collect();
            format!("[{}]", body.join(","))
        }
        other => other.to_string(),
    }
}

/// Read-only tools: calling them again is expected, so an identical result is
/// the signal, not the call itself.
// reference: Hermes' `IDEMPOTENT_TOOL_NAMES`.
const IDEMPOTENT_TOOLS: &[&str] = &[
    "read",
    "agentgrep",
    "grep",
    "session_search",
    "conversation_search",
    "websearch",
    "webfetch",
    "web_search",
    "web_extract",
    "jcode_docs",
    "integration_tools",
    "skill_manage",
    "list_dir",
    "open",
    "todo",
];

/// Tools whose successful run changes state. A success here is PROGRESS.
// reference: Hermes' `MUTATING_TOOL_NAMES` and `PROGRESS_RESET_TOOL_NAMES`.
const MUTATING_TOOLS: &[&str] = &[
    "write",
    "edit",
    "apply_patch",
    "replace",
    "bash",
    "memory",
    "todo",
    "swarm",
    "schedule",
    "goal",
    "panel",
    "skill_manage",
    "browser",
    "computer",
    "compile_remote",
    "gmail",
    "calendar",
    "open",
];

/// Tools whose non-zero exit is ordinary work output, not a failure to fix.
/// For these, `same_tool_failure` only warns: a run of distinct red commands
/// is diagnosis, not a loop.
// reference: Hermes' `FAILURE_TOLERANT_TOOL_NAMES`.
const FAILURE_TOLERANT_TOOLS: &[&str] = &["bash", "compile_remote", "webfetch", "browser"];

/// Whether the tool is read-only.
pub fn is_idempotent_tool(name: &str) -> bool {
    let lowered = name.trim().to_ascii_lowercase();
    IDEMPOTENT_TOOLS.iter().any(|tool| *tool == lowered)
}

/// Whether the tool changes state.
pub fn is_mutating_tool(name: &str) -> bool {
    let lowered = name.trim().to_ascii_lowercase();
    MUTATING_TOOLS.iter().any(|tool| *tool == lowered)
}

/// Whether the tool is one whose failure is ordinary output.
pub fn is_failure_tolerant_tool(name: &str) -> bool {
    let lowered = name.trim().to_ascii_lowercase();
    FAILURE_TOLERANT_TOOLS.iter().any(|tool| *tool == lowered)
}

/// The per-turn controller.
#[derive(Debug, Clone)]
pub struct LoopGuardrailController {
    config: LoopGuardrailConfig,
    /// signature -> how many times that exact call failed.
    exact_failures: HashMap<ToolCallSignature, u32>,
    /// tool name -> how many times it failed.
    tool_failures: HashMap<String, u32>,
    /// signature -> (result hash, how many times in a row it was identical).
    no_progress: HashMap<ToolCallSignature, (u64, u32)>,
    /// Web searches made in this turn.
    web_searches: u32,
}

impl LoopGuardrailController {
    pub fn new(config: LoopGuardrailConfig) -> Self {
        Self {
            config,
            exact_failures: HashMap::new(),
            tool_failures: HashMap::new(),
            no_progress: HashMap::new(),
            web_searches: 0,
        }
    }

    /// Clear every counter. Called at the start of each turn.
    pub fn reset_for_turn(&mut self) {
        self.exact_failures.clear();
        self.tool_failures.clear();
        self.no_progress.clear();
        self.web_searches = 0;
    }

    /// The decision for a call that is ABOUT to run.
    pub fn before_call(&mut self, tool_name: &str, args: &Value) -> GuardrailDecision {
        let name = tool_name.trim().to_ascii_lowercase();
        let signature = ToolCallSignature::from_call(&name, args);

        // The per-turn cap fires regardless of the hard-stop switch: dozens of
        // web searches in ONE turn is pathological even when every one of them
        // succeeded.
        if is_web_search(&name) && self.config.max_web_searches_per_turn > 0 {
            if self.web_searches >= self.config.max_web_searches_per_turn {
                let count = self.web_searches;
                return GuardrailDecision {
                    action: GuardrailAction::Block,
                    code: "loop_web_search_cap",
                    message: format!(
                        "Blocked {name}: this turn has already made {count} web searches, the \
                         per-turn limit. This looks like a runaway search loop. Work with the \
                         results you already have and answer the user."
                    ),
                    tool_name: name,
                    count,
                };
            }
        }

        if !self.config.hard_stop_enabled {
            return GuardrailDecision::allow();
        }

        if let Some(&count) = self.exact_failures.get(&signature)
            && count >= self.config.exact_failure_block_after
        {
            return GuardrailDecision {
                action: GuardrailAction::Block,
                code: "repeated_exact_failure_block",
                message: format!(
                    "Blocked {name}: the same call failed {count} times with identical arguments. \
                     Stop retrying it unchanged; change the approach, or search the exact error \
                     text for a known fix."
                ),
                tool_name: name,
                count,
            };
        }

        if is_idempotent_tool(&name)
            && let Some(&(_, count)) = self.no_progress.get(&signature)
            && count >= self.config.no_progress_block_after
        {
            return GuardrailDecision {
                action: GuardrailAction::Block,
                code: "idempotent_no_progress_block",
                message: format!(
                    "Blocked {name}: this read-only call returned the same result {count} times. \
                     Use the result already provided, or ask a different question."
                ),
                tool_name: name,
                count,
            };
        }

        GuardrailDecision::allow()
    }

    /// Update the counters after a call, and return any warning.
    pub fn after_call(
        &mut self,
        tool_name: &str,
        args: &Value,
        result: &str,
        failed: bool,
    ) -> GuardrailDecision {
        let name = tool_name.trim().to_ascii_lowercase();
        let signature = ToolCallSignature::from_call(&name, args);

        if is_web_search(&name) {
            self.web_searches = self.web_searches.saturating_add(1);
        }

        if failed {
            let exact = self.exact_failures.entry(signature.clone()).or_insert(0);
            *exact = exact.saturating_add(1);
            let exact_count = *exact;
            let tool_count = {
                let entry = self.tool_failures.entry(name.clone()).or_insert(0);
                *entry = entry.saturating_add(1);
                *entry
            };
            self.no_progress.remove(&signature);

            if self.config.hard_stop_enabled
                && !is_failure_tolerant_tool(&name)
                && tool_count >= self.config.same_tool_failure_block_after
            {
                return GuardrailDecision {
                    action: GuardrailAction::Block,
                    code: "same_tool_failure_block",
                    message: format!(
                        "Blocked {name}: it failed {tool_count} times this turn. Stop retrying the \
                         same failing tool path and choose a different approach."
                    ),
                    tool_name: name,
                    count: tool_count,
                };
            }

            if self.config.warnings_enabled && exact_count >= self.config.exact_failure_warn_after {
                return GuardrailDecision {
                    action: GuardrailAction::Warn,
                    code: "repeated_exact_failure_warning",
                    message: format!(
                        "{name} has failed {exact_count} times with identical arguments. This looks \
                         like a loop: inspect the error and change strategy instead of retrying it \
                         unchanged."
                    ),
                    tool_name: name,
                    count: exact_count,
                };
            }

            if self.config.warnings_enabled && tool_count >= self.config.same_tool_failure_warn_after {
                return GuardrailDecision {
                    action: GuardrailAction::Warn,
                    code: "same_tool_failure_warning",
                    message: format!(
                        "{name} has failed {tool_count} times this turn. A run of distinct red \
                         commands is diagnosis, not a loop - but if the arguments are repeating, \
                         change the approach."
                    ),
                    tool_name: name,
                    count: tool_count,
                };
            }

            return GuardrailDecision {
                action: GuardrailAction::Allow,
                code: "allow",
                message: String::new(),
                tool_name: name,
                count: exact_count,
            };
        }

        // A successful call clears the failure streaks for its own signature
        // and for the tool.
        self.exact_failures.remove(&signature);
        self.tool_failures.remove(&name);

        // A successful MUTATING call is progress for every failing call still
        // being counted: "edit, then re-run the red test" is not a loop.
        if is_mutating_tool(&name) {
            self.exact_failures.clear();
            self.tool_failures.clear();
        }

        if !is_idempotent_tool(&name) {
            self.no_progress.remove(&signature);
            return GuardrailDecision {
                action: GuardrailAction::Allow,
                code: "allow",
                message: String::new(),
                tool_name: name,
                count: 0,
            };
        }

        let result_hash = hash_result(result);
        let entry = self.no_progress.entry(signature).or_insert((result_hash, 0));
        if entry.0 == result_hash {
            entry.1 = entry.1.saturating_add(1);
        } else {
            *entry = (result_hash, 1);
        }
        let count = entry.1;

        if self.config.warnings_enabled && count >= self.config.no_progress_warn_after {
            return GuardrailDecision {
                action: GuardrailAction::Warn,
                code: "idempotent_no_progress_warning",
                message: format!(
                    "{name} returned the same result {count} times. Use the result already provided \
                     or change the query instead of repeating it unchanged."
                ),
                tool_name: name,
                count,
            };
        }

        GuardrailDecision {
            action: GuardrailAction::Allow,
            code: "allow",
            message: String::new(),
            tool_name: name,
            count,
        }
    }
}

/// Whether the tool is one of the web-search family.
fn is_web_search(name: &str) -> bool {
    matches!(name, "websearch" | "web_search")
}

/// Hash of a tool result, for the no-progress detector.
fn hash_result(result: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in result.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Build the controller config from the user-facing config section.
///
/// The conversion lives HERE, not in the config type: the config type lives in
/// `jcode-config-types`, which cannot depend on this crate (the dependency
/// runs the other way), so a `to_config` method there could not name this
/// type.
// reference: crates/jcode-config-types/src/lib.rs - `ToolLoopGuardrailsConfig`,
// the user-facing section this converts.
pub fn config_from_section(section: &crate::config::ToolLoopGuardrailsConfig) -> LoopGuardrailConfig {
    LoopGuardrailConfig {
        warnings_enabled: section.warnings_enabled,
        hard_stop_enabled: section.hard_stop_enabled,
        exact_failure_warn_after: section.exact_failure_warn_after,
        exact_failure_block_after: section.exact_failure_block_after,
        same_tool_failure_warn_after: section.same_tool_failure_warn_after,
        same_tool_failure_block_after: section.same_tool_failure_block_after,
        no_progress_warn_after: section.no_progress_warn_after,
        no_progress_block_after: section.no_progress_block_after,
        max_web_searches_per_turn: section.max_web_searches_per_turn,
    }
}

/// Read the controller for a session, creating it if needed.
fn with_controller<R>(
    session_id: &str,
    mut action: impl FnMut(&mut LoopGuardrailController) -> R,
) -> Option<R> {
    let mut controllers = SESSION_CONTROLLERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let controller = controllers
        .entry(session_id.to_string())
        .or_insert_with(|| LoopGuardrailController::new(LoopGuardrailConfig::default()));
    Some(action(controller))
}

/// The decision for a call that is about to run, for this session.
pub fn before_call(
    session_id: &str,
    tool_name: &str,
    args: &Value,
    config: &LoopGuardrailConfig,
) -> GuardrailDecision {
    with_controller(session_id, |controller| {
        controller.config = config.clone();
        controller.before_call(tool_name, args)
    })
    .unwrap_or_else(|| GuardrailDecision {
        action: GuardrailAction::Allow,
        code: "allow",
        message: String::new(),
        tool_name: tool_name.to_string(),
        count: 0,
    })
}

/// Update the counters after a call, for this session.
pub fn after_call(
    session_id: &str,
    tool_name: &str,
    args: &Value,
    result: &str,
    failed: bool,
    config: &LoopGuardrailConfig,
) -> GuardrailDecision {
    with_controller(session_id, |controller| {
        controller.config = config.clone();
        controller.after_call(tool_name, args, result, failed)
    })
    .unwrap_or_else(|| GuardrailDecision {
        action: GuardrailAction::Allow,
        code: "allow",
        message: String::new(),
        tool_name: tool_name.to_string(),
        count: 0,
    })
}

/// Clear this session's counters. Called at the start of each turn.
pub fn reset_for_turn(session_id: &str) {
    let mut controllers = SESSION_CONTROLLERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(controller) = controllers.get_mut(session_id) {
        controller.reset_for_turn();
    }
}

/// Drop this session's controller entirely (used when a session ends).
pub fn forget_session(session_id: &str) {
    let mut controllers = SESSION_CONTROLLERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    controllers.remove(session_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(hard_stop: bool) -> LoopGuardrailConfig {
        LoopGuardrailConfig {
            hard_stop_enabled: hard_stop,
            ..LoopGuardrailConfig::default()
        }
    }

    #[test]
    fn identical_failing_call_warns_at_two_and_blocks_at_five() {
        let mut controller = LoopGuardrailController::new(config(true));
        let args = json!({"command": "cargo test"});

        let first = controller.after_call("bash", &args, "error: 3 failed", true);
        assert_eq!(first.action, GuardrailAction::Allow);

        let second = controller.after_call("bash", &args, "error: 3 failed", true);
        assert_eq!(second.action, GuardrailAction::Warn);
        assert_eq!(second.code, "repeated_exact_failure_warning");

        for _ in 0..3 {
            controller.after_call("bash", &args, "error: 3 failed", true);
        }
        // Five identical failures have now been counted.
        let blocked = controller.before_call("bash", &args);
        assert_eq!(blocked.action, GuardrailAction::Block);
        assert_eq!(blocked.code, "repeated_exact_failure_block");
        assert_eq!(blocked.count, 5);
    }

    #[test]
    fn a_successful_mutating_call_resets_the_exact_failure_streak() {
        let mut controller = LoopGuardrailController::new(config(true));
        let failing = json!({"command": "cargo test"});

        controller.after_call("bash", &failing, "error", true);
        controller.after_call("bash", &failing, "error", true);
        assert_eq!(controller.exact_failures.len(), 1);

        // The fix landed: this is progress.
        controller.after_call("edit", &json!({"file_path": "a.rs"}), "ok", false);

        assert!(controller.exact_failures.is_empty());
        assert!(controller.tool_failures.is_empty());
        assert_eq!(
            controller.before_call("bash", &failing).action,
            GuardrailAction::Allow
        );
    }

    #[test]
    fn same_tool_failing_with_different_args_hits_the_tool_streak_not_the_exact_streak() {
        let mut controller = LoopGuardrailController::new(config(true));

        controller.after_call("bash", &json!({"command": "a"}), "error", true);
        controller.after_call("bash", &json!({"command": "b"}), "error", true);
        controller.after_call("bash", &json!({"command": "c"}), "error", true);

        // Counters are per signature, so each distinct argument set has its
        // own entry - but no signature has an exact-replay streak.
        assert!(
            controller.exact_failures.values().all(|count| *count == 1),
            "different arguments are not an exact replay: {:?}",
            controller.exact_failures
        );
        assert_eq!(controller.tool_failures.get("bash"), Some(&3));
        let warned = controller.after_call("bash", &json!({"command": "d"}), "error", true);
        assert_eq!(warned.action, GuardrailAction::Warn);
        assert_eq!(warned.code, "same_tool_failure_warning");
    }

    #[test]
    fn a_failure_tolerant_tool_only_warns_never_blocks() {
        let mut controller = LoopGuardrailController::new(config(true));
        for index in 0..20 {
            controller.after_call(
                "bash",
                &json!({"command": format!("test {index}")}),
                "error",
                true,
            );
        }
        let decision = controller.after_call("bash", &json!({"command": "last"}), "error", true);
        assert_ne!(decision.action, GuardrailAction::Block);
        assert_eq!(decision.code, "same_tool_failure_warning");
    }

    #[test]
    fn an_idempotent_tool_returning_the_same_result_warns_then_blocks() {
        let mut controller = LoopGuardrailController::new(config(true));
        let args = json!({"query": "needle"});

        controller.after_call("agentgrep", &args, "same output", false);
        let second = controller.after_call("agentgrep", &args, "same output", false);
        assert_eq!(second.action, GuardrailAction::Warn);
        assert_eq!(second.code, "idempotent_no_progress_warning");

        controller.after_call("agentgrep", &args, "same output", false);
        controller.after_call("agentgrep", &args, "same output", false);
        controller.after_call("agentgrep", &args, "same output", false);
        let blocked = controller.before_call("agentgrep", &args);
        assert_eq!(blocked.action, GuardrailAction::Block);
        assert_eq!(blocked.code, "idempotent_no_progress_block");
    }

    #[test]
    fn a_different_tool_does_not_trip_the_no_progress_detector() {
        let mut controller = LoopGuardrailController::new(config(true));
        controller.after_call("agentgrep", &json!({"query": "a"}), "same", false);
        let other = controller.after_call("read", &json!({"path": "b"}), "same", false);
        assert_eq!(other.action, GuardrailAction::Allow);
        assert_eq!(other.code, "allow");
        // Two signatures, each at one occurrence: the second tool did not
        // extend the first tool's no-progress streak.
        assert_eq!(controller.no_progress.len(), 2);
        assert!(controller.no_progress.values().all(|(_, count)| *count == 1));
    }

    #[test]
    fn the_web_search_cap_blocks_at_the_cap() {
        let mut controller = LoopGuardrailController::new(LoopGuardrailConfig {
            max_web_searches_per_turn: 3,
            ..config(true)
        });
        for index in 0..3 {
            // Distinct results, so the idempotent no-progress detector stays
            // quiet and only the web-search cap is under test.
            let decision = controller.after_call(
                "websearch",
                &json!({"query": "x"}),
                &format!("result {index}"),
                false,
            );
            assert_eq!(decision.action, GuardrailAction::Allow);
        }
        let blocked = controller.before_call("websearch", &json!({"query": "x"}));
        assert_eq!(blocked.action, GuardrailAction::Block);
        assert_eq!(blocked.code, "loop_web_search_cap");
    }

    #[test]
    fn reset_for_turn_clears_every_counter() {
        let mut controller = LoopGuardrailController::new(config(true));
        controller.after_call("bash", &json!({"command": "a"}), "error", true);
        controller.after_call("websearch", &json!({"query": "x"}), "ok", false);
        assert!(!controller.exact_failures.is_empty());
        assert_eq!(controller.web_searches, 1);

        controller.reset_for_turn();

        assert!(controller.exact_failures.is_empty());
        assert!(controller.tool_failures.is_empty());
        assert!(controller.no_progress.is_empty());
        assert_eq!(controller.web_searches, 0);
    }

    #[test]
    fn warnings_are_off_when_the_switch_is_off() {
        let mut controller = LoopGuardrailController::new(LoopGuardrailConfig {
            warnings_enabled: false,
            hard_stop_enabled: false,
            ..LoopGuardrailConfig::default()
        });
        let decision = controller.after_call("bash", &json!({"command": "a"}), "error", true);
        assert_eq!(decision.action, GuardrailAction::Allow);
        assert_eq!(
            controller.before_call("bash", &json!({"command": "a"})).action,
            GuardrailAction::Allow
        );
    }

    #[test]
    fn argument_order_does_not_change_the_signature() {
        let left = ToolCallSignature::from_call("read", &json!({"a": 1, "b": 2}));
        let right = ToolCallSignature::from_call("read", &json!({"b": 2, "a": 1}));
        assert_eq!(left, right);
        let different = ToolCallSignature::from_call("read", &json!({"a": 1, "b": 3}));
        assert_ne!(left, different);
    }

    #[test]
    fn the_session_level_helpers_keep_state_per_session() {
        let config = LoopGuardrailConfig::default();
        let args = json!({"command": "cargo test"});

        after_call("guardrails-session-a", "bash", &args, "error", true, &config);
        after_call("guardrails-session-a", "bash", &args, "error", true, &config);
        let warned = after_call("guardrails-session-a", "bash", &args, "error", true, &config);
        assert_eq!(warned.action, GuardrailAction::Warn);

        // A different session has its own counters.
        let other = after_call("guardrails-session-b", "bash", &args, "error", true, &config);
        assert_eq!(other.action, GuardrailAction::Allow);

        forget_session("guardrails-session-a");
        forget_session("guardrails-session-b");
    }
}
