// reference: https://github.com/NousResearch/hermes-agent/blob/main/tools/write_approval.py
// Tests for the opt-in memory write gate.
//
// The gate reads `crate::config::config()`, which is process-global, so a test
// that needs the gate ON writes a real config.toml into an isolated
// JCODE_HOME and invalidates the config cache. The config cache checks
// `cfg!(test)` and reloads every call, so no lock is held between assertions.
//
// reference: crates/jcode-app-core/src/tool/research_gate_tests.rs - the
// sibling test module for a per-session tool gate, same wiring via
// `#[path = ...] mod ...` plus `#[cfg(test)]`.

use super::*;
use serde_json::json;

/// A temp JCODE_HOME with a config.toml carrying the given agents section.
///
/// The gate is read from the process-global config, so the file has to exist
/// on disk; an in-memory override would not be seen by `crate::config::config()`.
struct GateHome {
    /// Kept alive so the temp directory outlives the test.
    _home: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

impl GateHome {
    fn new(gate_value: &str) -> Self {
        let previous = std::env::var_os("JCODE_HOME");
        let home = tempfile::tempdir().expect("create temp jcode home");
        let config_path = home.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!("[agents]\nmemory_write_gate = \"{gate_value}\"\n"),
        )
        .expect("write config");
        crate::env::set_var("JCODE_HOME", home.path());
        crate::config::invalidate_config_cache();
        Self {
            _home: home,
            previous,
        }
    }
}

impl Drop for GateHome {
    fn drop(&mut self) {
        crate::config::invalidate_config_cache();
        match self.previous.take() {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
        crate::config::invalidate_config_cache();
    }
}

/// The `[agents] memory_write_gate` value parses.
#[test]
fn memory_write_gate_parses_from_toml() {
    let config: crate::config::Config =
        toml::from_str("[agents]\nmemory_write_gate = \"approve\"\n").expect("section parses");
    assert_eq!(
        config.agents.memory_write_gate,
        crate::config::MemoryWriteGate::Approve
    );
    assert!(config.agents.memory_write_gate.requires_approval());
}

/// A config with no `[agents]` section keeps the gate OFF, so an existing
/// config.toml behaves exactly as before this setting existed.
#[test]
fn memory_write_gate_defaults_off_when_absent() {
    let config: crate::config::Config = toml::from_str("").expect("empty config parses");
    assert_eq!(
        config.agents.memory_write_gate,
        crate::config::MemoryWriteGate::Off
    );
    assert!(!config.agents.memory_write_gate.requires_approval());
}

/// The gate off (the default) lets a memory write through unchanged.
#[tokio::test]
async fn gate_off_writes_the_memory() {
    let _guard = crate::storage::lock_test_env();
    let _home = GateHome::new("off");
    let project = tempfile::tempdir().unwrap();
    let tool = MemoryTool::new();
    let ctx = test_ctx(Some(project.path().to_path_buf()));

    let output = tool
        .execute(
            json!({"action":"remember", "content":"gate-off-probe", "scope":"project"}),
            ctx,
        )
        .await
        .expect("a write must succeed when the gate is off");
    assert!(
        output.output.contains("Remembered"),
        "the gate off must keep the normal write path: {}",
        output.output
    );
}

/// The gate on refuses the write and queues it for approval instead.
#[tokio::test]
async fn gate_on_refuses_the_write_until_approved() {
    let _guard = crate::storage::lock_test_env();
    let _home = GateHome::new("approve");
    let project = tempfile::tempdir().unwrap();
    let tool = MemoryTool::new();
    let ctx = test_ctx(Some(project.path().to_path_buf()));

    let output = tool
        .execute(
            json!({"action":"remember", "content":"gate-on-probe", "scope":"project"}),
            ctx,
        )
        .await
        .expect("a gated write is held, not failed");
    assert!(
        output.output.contains("pending approval"),
        "the gated write must say it is pending: {}",
        output.output
    );
    assert!(
        !output.output.contains("Remembered"),
        "the gated write must NOT be applied: {}",
        output.output
    );

    // The fact must not be in the store.
    let recall = tool
        .execute(
            json!({"action":"recall", "scope":"project", "limit":50}),
            test_ctx(Some(project.path().to_path_buf())),
        )
        .await
        .expect("recall works");
    assert!(
        !recall.output.contains("gate-on-probe"),
        "the fact must not be stored before approval: {}",
        recall.output
    );

    // The request must be visible to the existing permission surface.
    let pending = crate::safety::SafetySystem::new().pending_requests();
    assert!(
        pending
            .iter()
            .any(|req| req.action == "memory_write" && req.description.contains("gate-on-probe")),
        "the gated write must be queued for approval: {pending:?}"
    );
}

/// After the user approves, the SAME write goes through.
///
/// The request id is deterministic, so the retry finds the recorded approval.
#[tokio::test]
async fn gate_on_lets_the_write_through_after_approval() {
    let _guard = crate::storage::lock_test_env();
    let _home = GateHome::new("approve");
    let project = tempfile::tempdir().unwrap();
    let tool = MemoryTool::new();
    let ctx = || test_ctx(Some(project.path().to_path_buf()));

    let content = "gate-approved-probe";
    let first = tool
        .execute(
            json!({"action":"remember", "content":content, "scope":"project"}),
            ctx(),
        )
        .await
        .expect("a gated write is held, not failed");
    assert!(first.output.contains("pending approval"), "{}", first.output);

    let request_id = crate::tool::memory_write_gate::memory_write_request_id(
        "test-session",
        "project",
        "fact",
        content,
    );
    crate::safety::record_permission_via_file(&request_id, true, "test", None)
        .expect("record the approval");

    let second = tool
        .execute(
            json!({"action":"remember", "content":content, "scope":"project"}),
            ctx(),
        )
        .await
        .expect("an approved write must succeed");
    assert!(
        second.output.contains("Remembered"),
        "the approved write must be applied: {}",
        second.output
    );
}

/// A refusal is final: the same write is rejected, not held.
#[tokio::test]
async fn gate_on_rejects_the_write_after_a_refusal() {
    let _guard = crate::storage::lock_test_env();
    let _home = GateHome::new("approve");
    let project = tempfile::tempdir().unwrap();
    let tool = MemoryTool::new();
    let ctx = || test_ctx(Some(project.path().to_path_buf()));

    let content = "gate-refused-probe";
    tool.execute(
        json!({"action":"remember", "content":content, "scope":"project"}),
        ctx(),
    )
    .await
    .expect("a gated write is held, not failed");

    let request_id = crate::tool::memory_write_gate::memory_write_request_id(
        "test-session",
        "project",
        "fact",
        content,
    );
    crate::safety::record_permission_via_file(
        &request_id,
        false,
        "test",
        Some("not useful".to_string()),
    )
    .expect("record the refusal");

    let error = tool
        .execute(
            json!({"action":"remember", "content":content, "scope":"project"}),
            ctx(),
        )
        .await
        .expect_err("a refused write must fail");
    assert!(
        error.to_string().contains("rejected"),
        "the refusal must be reported: {error}"
    );
}

/// The request id is deterministic and scoped to the session.
#[test]
fn request_id_is_deterministic_and_session_scoped() {
    let first = crate::tool::memory_write_gate::memory_write_request_id(
        "session-a",
        "project",
        "fact",
        "same content",
    );
    let again = crate::tool::memory_write_gate::memory_write_request_id(
        "session-a",
        "project",
        "fact",
        "same content",
    );
    let other_session = crate::tool::memory_write_gate::memory_write_request_id(
        "session-b",
        "project",
        "fact",
        "same content",
    );
    let other_scope = crate::tool::memory_write_gate::memory_write_request_id(
        "session-a",
        "global",
        "fact",
        "same content",
    );

    assert_eq!(first, again, "the same write must produce the same id");
    assert_ne!(first, other_session, "a session must not share an approval");
    assert_ne!(first, other_scope, "a scope must not share an approval");
    assert!(first.starts_with("req_memory_"), "unexpected id: {first}");
}

/// The batch id is deterministic and identifies the exact batch.
#[test]
fn batch_id_is_deterministic_and_batch_scoped() {
    let entry = |content: &str| {
        crate::memory::MemoryEntry::new(crate::memory::MemoryCategory::Fact, content.to_string())
    };

    let first = crate::tool::memory_write_gate::memory_write_batch_id(
        "project",
        &[entry("one"), entry("two")],
    );
    let again = crate::tool::memory_write_gate::memory_write_batch_id(
        "project",
        &[entry("one"), entry("two")],
    );
    let reordered = crate::tool::memory_write_gate::memory_write_batch_id(
        "project",
        &[entry("two"), entry("one")],
    );
    let different = crate::tool::memory_write_gate::memory_write_batch_id(
        "project",
        &[entry("one"), entry("three")],
    );
    let other_scope = crate::tool::memory_write_gate::memory_write_batch_id(
        "global",
        &[entry("one"), entry("two")],
    );

    assert_eq!(first, again, "the same batch must produce the same id");
    assert_eq!(
        first, reordered,
        "the id must not depend on the order the file listed the entries"
    );
    assert_ne!(first, different, "a different batch must produce a different id");
    assert_ne!(
        first, other_scope,
        "the same content into a different scope is a different write"
    );
    assert!(first.starts_with("req_memory_"), "unexpected id: {first}");
}

/// The gate off lets an import batch through.
#[tokio::test]
async fn batch_gate_off_allows_the_import() {
    let _guard = crate::storage::lock_test_env();
    let _home = GateHome::new("off");
    let entries = vec![crate::memory::MemoryEntry::new(
        crate::memory::MemoryCategory::Fact,
        "batch-off-probe".to_string(),
    )];

    let outcome = crate::tool::memory_write_gate::evaluate_memory_write_batch(
        "project",
        &entries,
    );
    assert_eq!(outcome, crate::tool::memory_write_gate::GateOutcome::Allowed);
}

/// The gate on holds the whole batch and queues ONE request for it.
#[tokio::test]
async fn batch_gate_on_holds_the_batch_and_queues_one_request() {
    let _guard = crate::storage::lock_test_env();
    let _home = GateHome::new("approve");
    let entries = vec![
        crate::memory::MemoryEntry::new(
            crate::memory::MemoryCategory::Fact,
            "batch-probe-one".to_string(),
        ),
        crate::memory::MemoryEntry::new(
            crate::memory::MemoryCategory::Fact,
            "batch-probe-two".to_string(),
        ),
    ];

    let outcome = crate::tool::memory_write_gate::evaluate_memory_write_batch(
        "project",
        &entries,
    );

    let crate::tool::memory_write_gate::GateOutcome::Pending { reason, .. } = &outcome else {
        panic!("a gated batch must be held: {outcome:?}");
    };
    assert!(
        reason.contains("needs your approval"),
        "the reason must say the import is held: {reason}"
    );

    let pending = crate::safety::SafetySystem::new().pending_requests();
    assert_eq!(
        pending
            .iter()
            .filter(|req| req.action == "memory_write")
            .count(),
        1,
        "one import must queue exactly one request: {pending:?}"
    );
    assert!(
        pending.iter().any(|req| req.description.contains("2 memories")),
        "the request must describe the batch: {pending:?}"
    );
}

/// After the user approves the batch, the same import goes through.
#[tokio::test]
async fn batch_gate_lets_the_import_through_after_approval() {
    let _guard = crate::storage::lock_test_env();
    let _home = GateHome::new("approve");
    let entries = vec![crate::memory::MemoryEntry::new(
        crate::memory::MemoryCategory::Fact,
        "batch-approved-probe".to_string(),
    )];

    let first = crate::tool::memory_write_gate::evaluate_memory_write_batch(
        "project",
        &entries,
    );
    assert!(
        matches!(first, crate::tool::memory_write_gate::GateOutcome::Pending { .. }),
        "{first:?}"
    );

    let batch_id = crate::tool::memory_write_gate::memory_write_batch_id("project", &entries);
    crate::safety::record_permission_via_file(&batch_id, true, "test", None)
        .expect("record the approval");

    let second = crate::tool::memory_write_gate::evaluate_memory_write_batch(
        "project",
        &entries,
    );
    assert_eq!(
        second, crate::tool::memory_write_gate::GateOutcome::Allowed,
        "the approved batch must go through"
    );
}
