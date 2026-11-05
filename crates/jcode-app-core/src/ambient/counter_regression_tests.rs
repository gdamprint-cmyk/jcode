// reference: ~/.jcode/research/2026-11-04-always-on/ambient-counter-fix-reference-implementations.md
//
// Regression test for the ambient `total_cycles` double-count defect.
//
// The defect: before the fix, three independent writers persisted `state.json`
// for a single completed ambient cycle:
//
// 1. `crate::tool::ambient::EndAmbientCycleTool::execute` — loaded the state
//    from disk, incremented `total_cycles`, saved.
// 2. `AmbientManager::record_cycle_result` — loaded the state from disk,
//    incremented `total_cycles`, saved.
// 3. `AmbientRunnerHandle` — incremented its in-memory state and saved.
//
// The result was a transient `D+2` overcount on disk, observable live on
// 2026-11-04 (`~/.jcode/research/2026-11-04-always-on/ambient-counter-live-capture.md`,
// observation 3: `state.bak` held `total_cycles = 13` while the authoritative
// value was 12), plus a crash window in which the double count became
// permanent.
//
// The invariant: the runner is the single writer of the ambient state's cycle
// accounting —
//
// * exactly ONE `state.json` write per completed cycle, by ANY writer,
// * the written `total_cycles` never exceeds the number of completed cycles
//   (no `D+2` on disk, not even transiently),
// * the persisted counter never decreases.
//
// IMPORTANT (v2): the invariant is checked WITHOUT relying on writer labels.
//
// The v1 test filtered the write log by label (`"runner"`, `"tool"`,
// `"manager"`), so any writer that used the plain `AmbientState::save()`
// (logged as `"unlabelled"`) was invisible to it: a hidden writer produced
// `72 passed / 0 failed` while `state.bak` held `D+2` and `state.json` `D+1`
// (measured 2026-11-05, report
// `revalidate-ambient-counter-fix.md` §5, control experiment C4).
//
// Therefore the checks below are label-independent:
//   * the write count is taken over ALL writes for the path
//     (`state_writes_all_for`), and
//   * the authoritative check is made ON DISK: `state.json` must hold the
//     expected `total_cycles` and `state.bak` (the previous version kept by
//     the atomic writer) must hold exactly one less — `bak == json - 1` —
//     which is structurally impossible when a second writer slipped in.

use super::{
    AmbientCycleResult, AmbientManager, AmbientState, CycleStatus, ScheduleRequest, ScheduleTarget,
};
use crate::ambient::runner::AmbientRunnerHandle;
use crate::safety::SafetySystem;
use crate::tool::Tool;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Isolation helpers
// ---------------------------------------------------------------------------

/// Redirect `JCODE_HOME` to a temp dir for the duration of the test, so the
/// test never touches the live `~/.jcode/ambient`.
struct HomeGuard(Option<OsString>);

impl HomeGuard {
    fn set(path: &std::path::Path) -> Self {
        let previous = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", path);
        Self(previous)
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(old) => crate::env::set_var("JCODE_HOME", old),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

fn state_path_in(home: &std::path::Path) -> std::path::PathBuf {
    // Deliberately NOT `home/ambient/state.json`: that is the path a
    // background task resolves through the global `JCODE_HOME`. Using a
    // distinct directory keeps this test's own state file separate from any
    // write a leaked task from another test performs, so the assertions below
    // see only what this test did.
    home.join("isolated").join("state.json")
}

/// Pin this thread's ambient `state.json` to `home`'s own file.
///
/// `JCODE_HOME` is process-global, so a background task leaked by another
/// test can resolve `state_path()` against the `JCODE_HOME` that is current
/// at ITS save time — which may be this test's temp home. The override pins
/// this thread's `state.json` to the test's own file.
fn pin_state_path(home: &std::path::Path) -> super::paths::StatePathGuard {
    super::paths::StatePathGuard::set(state_path_in(home))
}

/// The `.bak` sibling the atomic JSON writer keeps next to `state.json`.
fn bak_path_of(path: &Path) -> std::path::PathBuf {
    path.with_extension("bak")
}

/// The persisted `total_cycles` of a state file, if it exists and parses.
fn read_total_cycles(path: &Path) -> Option<u64> {
    let raw = std::fs::read_to_string(path).ok()?;
    let state: AmbientState = serde_json::from_str(&raw).ok()?;
    Some(state.total_cycles)
}

fn read_disk_total_cycles(home: &std::path::Path) -> u64 {
    let path = state_path_in(home);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("state.json must exist at {}: {e}", path.display()));
    let state: AmbientState =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("state.json must parse: {e}"));
    state.total_cycles
}

/// A completed cycle result for the given summary.
fn completed_cycle(summary: &str) -> AmbientCycleResult {
    let now = chrono::Utc::now();
    AmbientCycleResult {
        summary: summary.to_string(),
        memories_modified: 1,
        compactions: 0,
        proactive_work: None,
        next_schedule: None,
        started_at: now,
        ended_at: now,
        status: CycleStatus::Complete,
        conversation: None,
    }
}

// ---------------------------------------------------------------------------
// The invariant, in one place
// ---------------------------------------------------------------------------

/// Check the single-writer invariant for one completed cycle.
///
/// Returns the list of violations; an empty list means the invariant holds.
/// Deliberately label-independent: it never looks at WHICH writer performed
/// a write, only at HOW MANY writes happened and what ended up on disk.
fn single_write_violations(
    path: &Path,
    expected_total_cycles: u64,
    writes: &[super::StateWrite],
) -> Vec<String> {
    let mut violations = Vec::new();

    if writes.len() != 1 {
        violations.push(format!(
            "exactly one state.json write per completed cycle, got {}: {writes:?}",
            writes.len()
        ));
    }
    for write in writes {
        if write.total_cycles != expected_total_cycles {
            violations.push(format!(
                "a write persisted total_cycles={} but {expected_total_cycles} was expected",
                write.total_cycles
            ));
        }
    }

    // The on-disk check does not depend on the write log at all: it catches a
    // writer the log cannot attribute, including one on another thread.
    let json = read_total_cycles(path);
    let bak = read_total_cycles(&bak_path_of(path));
    match json {
        None => violations.push("state.json is missing on disk".to_string()),
        Some(value) if value != expected_total_cycles => violations.push(format!(
            "state.json holds total_cycles={value} but {expected_total_cycles} was expected"
        )),
        Some(_) => {}
    }
    match (json, bak) {
        (Some(current), Some(previous)) if previous.saturating_add(1) != current => {
            violations.push(format!(
                "state.bak holds total_cycles={previous} but must hold state.json's value minus one ({}): a second writer touched the state",
                current.saturating_sub(1)
            ));
        }
        (Some(current), None) if current != 1 => violations.push(format!(
            "state.bak is missing although state.json holds total_cycles={current}"
        )),
        _ => {}
    }

    violations
}

// ---------------------------------------------------------------------------
// The regression test
// ---------------------------------------------------------------------------

/// One completed cycle must produce exactly ONE `state.json` write — by ANY
/// writer — whose `total_cycles` equals the number of completed cycles.
///
/// Checked twice, so that neither instrument can pass for the wrong reason:
/// over the complete write log (all writers) and over the on-disk pair
/// `state.json` / `state.bak`.
#[tokio::test]
async fn a_completed_cycle_writes_state_json_exactly_once() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let _home = HomeGuard::set(temp.path());
    let _state = pin_state_path(temp.path());

    let runner = AmbientRunnerHandle::new(Arc::new(SafetySystem::new()));
    let path = state_path_in(temp.path());

    for cycle in 1..=3u64 {
        super::reset_state_writes();

        runner
            .apply_completed_cycle(&completed_cycle(&format!("cycle {cycle}")))
            .await;

        let writes = super::state_writes_all_for(&path);
        let violations = single_write_violations(&path, cycle, &writes);
        assert!(
            violations.is_empty(),
            "cycle {cycle} broke the single-writer invariant: {violations:#?}"
        );
        assert_eq!(
            read_disk_total_cycles(temp.path()),
            cycle,
            "the persisted total_cycles must equal the number of completed cycles"
        );
    }
}

/// The ambient TOOL must not persist the cycle. It only hands the result to
/// the runner, which is the single writer.
#[tokio::test]
async fn the_ambient_tool_does_not_write_state_json() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let _home = HomeGuard::set(temp.path());
    let _state = pin_state_path(temp.path());

    let tool = crate::tool::ambient::EndAmbientCycleTool::new();
    let ctx = crate::tool::ToolContext {
        session_id: "ambient_regression_test".to_string(),
        message_id: "msg_1".to_string(),
        tool_call_id: "call_1".to_string(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::Direct,
    };

    super::reset_state_writes();

    tool.execute(
        serde_json::json!({
            "summary": "tool wrote the cycle",
            "memories_modified": 2,
            "compactions": 0,
        }),
        ctx,
    )
    .await
    .expect("end_ambient_cycle must succeed");

    // ALL writers, not just the labelled ones.
    let writes = super::state_writes_all_for(&state_path_in(temp.path()));
    assert!(
        writes.is_empty(),
        "the ambient tool must not write state.json: {writes:?}"
    );
    assert!(
        !state_path_in(temp.path()).exists(),
        "the ambient tool must not create state.json"
    );
}

/// `AmbientManager::record_cycle_result` must not persist the cycle either.
/// It only enqueues a produced schedule request.
#[tokio::test]
async fn the_manager_does_not_write_state_json() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let _home = HomeGuard::set(temp.path());
    let _state = pin_state_path(temp.path());

    let mut manager = AmbientManager::new().expect("ambient manager");

    super::reset_state_writes();

    let mut result = completed_cycle("manager wrote the cycle");
    result.next_schedule = Some(ScheduleRequest {
        wake_in_minutes: Some(15),
        wake_at: None,
        context: "check CI".to_string(),
        priority: crate::ambient::Priority::Normal,
        target: ScheduleTarget::Ambient,
        created_by_session: "ambient_regression_test".to_string(),
        working_dir: None,
        task_description: None,
        relevant_files: Vec::new(),
        git_branch: None,
        additional_context: None,
    });

    manager
        .record_cycle_result(result)
        .expect("record_cycle_result must succeed");

    // ALL writers, not just the labelled ones.
    let writes = super::state_writes_all_for(&state_path_in(temp.path()));
    assert!(
        writes.is_empty(),
        "AmbientManager must not write state.json: {writes:?}"
    );
    assert!(
        !state_path_in(temp.path()).exists(),
        "AmbientManager must not create state.json"
    );
    assert_eq!(
        manager.queue().len(),
        1,
        "the schedule request must still be enqueued"
    );
}

// ---------------------------------------------------------------------------
// Negative controls
// ---------------------------------------------------------------------------

/// Negative control: reproduce the pre-fix defect with the REAL writer path
/// and assert that the invariant checker REPORTS it.
///
/// The two removed writers used the production `AmbientState::save()` — not a
/// test-only helper — so the control exercises the real code path a future
/// writer would take. The v1 version of this test called the test-only
/// `save_as("tool")` / `save_as("manager")` itself, so it only proved that
/// the test could count its own writes, not that the invariant checker sees
/// a real second writer.
#[tokio::test]
async fn regression_pre_fix_writers_are_detected() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let _home = HomeGuard::set(temp.path());
    let _state = pin_state_path(temp.path());

    let runner = AmbientRunnerHandle::new(Arc::new(SafetySystem::new()));
    let cycle = completed_cycle("negative control");
    let path = state_path_in(temp.path());

    super::reset_state_writes();

    // Pre-fix writer #1 (ambient tool): load from disk, increment, save.
    // Exactly the production calls the removed code made.
    let mut tool_state = AmbientState::load().expect("load state");
    tool_state.record_cycle(&cycle);
    tool_state.save().expect("tool save");

    // Pre-fix writer #2 (manager): load from disk, increment, save.
    let mut manager = AmbientManager::new().expect("ambient manager");
    let mut manager_state = AmbientState::load().expect("load state");
    manager_state.record_cycle(&cycle);
    manager_state.save().expect("manager save");
    let _ = manager.record_cycle_result(cycle.clone());

    // Pre-fix writer #3 (runner): the real runner path.
    runner.apply_completed_cycle(&cycle).await;

    let writes = super::state_writes_all_for(&path);
    assert!(
        writes.len() >= 3,
        "the pre-fix code wrote the cycle three times, got {writes:?}"
    );
    assert!(
        writes.iter().any(|write| write.total_cycles > 1),
        "the pre-fix code must produce a transient overcount on disk, got {writes:?}"
    );
    assert!(
        writes
            .windows(2)
            .any(|pair| pair[0].total_cycles > pair[1].total_cycles),
        "the pre-fix code must produce a decreasing persisted value, got {writes:?}"
    );

    // The invariant checker must see the defect — that is what makes its
    // passing on the fixed code meaningful.
    let violations = single_write_violations(&path, 1, &writes);
    assert!(
        !violations.is_empty(),
        "the invariant checker must report the pre-fix writers, got {writes:?}"
    );
}

/// Negative control for the label-blindness itself: a SINGLE hidden writer
/// that uses the plain `AmbientState::save()` (logged as `"unlabelled"`).
///
/// This is control experiment C4 from the 2026-11-05 revalidation report:
/// the v1 tests were blind to it (72 passed / 0 failed while `state.bak` held
/// `D+2` and `state.json` `D+1`). The v1 checks must fail here.
#[tokio::test]
async fn regression_hidden_unlabelled_writer_is_detected() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let _home = HomeGuard::set(temp.path());
    let _state = pin_state_path(temp.path());

    let runner = AmbientRunnerHandle::new(Arc::new(SafetySystem::new()));
    let cycle = completed_cycle("hidden writer");
    let path = state_path_in(temp.path());

    super::reset_state_writes();

    // A hidden writer: the plain production `save()`, no label.
    let mut hidden = AmbientState::load().expect("load state");
    hidden.record_cycle(&cycle);
    hidden.save().expect("hidden save");

    runner.apply_completed_cycle(&cycle).await;

    let writes = super::state_writes_all_for(&path);
    assert!(
        writes.len() > 1,
        "the hidden writer must be visible in the write log, got {writes:?}"
    );
    assert!(
        writes.iter().any(|write| write.writer == "unlabelled"),
        "the control must exercise the plain save() writer, got {writes:?}"
    );

    let violations = single_write_violations(&path, 1, &writes);
    assert!(
        !violations.is_empty(),
        "the invariant checker must report the hidden writer, got {writes:?}"
    );
}
