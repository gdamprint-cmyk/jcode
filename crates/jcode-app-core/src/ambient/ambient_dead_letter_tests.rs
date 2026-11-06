// reference: crates/jcode-app-core/src/ambient/queue_transaction_tests.rs (same crate: the JCODE_HOME-isolated ambient queue test harness this file follows)
use super::*;
use crate::ambient::runner::AmbientRunnerHandle;
use chrono::{Duration, Utc};
use std::path::PathBuf;
use std::sync::Arc;

/// Restores `JCODE_HOME` after the test, so a concurrent test does not observe it.
struct HomeGuard(Option<std::ffi::OsString>);
impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(old) => crate::env::set_var("JCODE_HOME", old),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

fn ambient_item(id: &str, context: String, when: chrono::DateTime<Utc>) -> ScheduledItem {
    ScheduledItem {
        id: id.into(),
        scheduled_for: when,
        context,
        priority: Priority::High,
        target: ScheduleTarget::Ambient,
        created_by_session: "ambient".into(),
        created_at: Utc::now() - Duration::hours(1),
        working_dir: None,
        task_description: None,
        relevant_files: vec![],
        git_branch: None,
        additional_context: None,
    }
}

fn direct_item(id: &str, context: String, when: chrono::DateTime<Utc>) -> ScheduledItem {
    ScheduledItem {
        id: id.into(),
        scheduled_for: when,
        context,
        priority: Priority::High,
        target: ScheduleTarget::Session {
            session_id: "session_direct".into(),
        },
        created_by_session: "session_direct".into(),
        created_at: Utc::now() - Duration::hours(1),
        working_dir: None,
        task_description: None,
        relevant_files: vec![],
        git_branch: None,
        additional_context: None,
    }
}

fn ambient_request(context: String, wake_at: chrono::DateTime<Utc>) -> ScheduleRequest {
    ScheduleRequest {
        wake_in_minutes: None,
        wake_at: Some(wake_at),
        context,
        priority: Priority::Normal,
        target: ScheduleTarget::Ambient,
        created_by_session: "ambient".into(),
        working_dir: None,
        task_description: None,
        relevant_files: vec![],
        git_branch: None,
        additional_context: None,
    }
}

/// A queue full of never-delivered ambient entries must NOT be rendered as one
/// full block per entry. The direct-delivery entry must still be rendered.
#[test]
fn ambient_prompt_collapses_never_delivered_ambient_entries() {
    let now = Utc::now();
    let mut queue: Vec<ScheduledItem> = Vec::new();
    for i in 0..40 {
        queue.push(ambient_item(
            &format!("sched_ambient_{i:03}"),
            format!("AMBIENTCONTEXT{i:03} {}", "x".repeat(400)),
            now - Duration::minutes(60 - i as i64),
        ));
    }
    queue.push(direct_item(
        "sched_direct_000",
        "DIRECTCONTEXTMARKER".to_string(),
        now - Duration::minutes(5),
    ));

    let prompt = build_ambient_system_prompt(
        &AmbientState::default(),
        &queue,
        &MemoryGraphHealth::default(),
        &[],
        &[],
        &ResourceBudget::default(),
        0,
    );

    assert!(
        prompt.contains("DIRECTCONTEXTMARKER"),
        "a direct-delivery entry must still be rendered in full"
    );

    let rendered = queue
        .iter()
        .filter(|item| !item.target.is_direct_delivery())
        .filter(|item| prompt.contains(&item.context))
        .count();
    assert!(
        rendered < 40,
        "expected the ambient entries to be collapsed, but {rendered} of 40 full contexts were rendered"
    );
}

/// `overdue_queue_count` must describe actionable work only. Ambient entries
/// that are never delivered are dead letters, not overdue work.
#[tokio::test]
async fn status_overdue_queue_count_ignores_ambient_dead_letters() {
    let _guard = crate::storage::lock_test_env();
    let dir = tempfile::tempdir().unwrap();
    let _home = HomeGuard(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", dir.path());

    let mut manager = AmbientManager::new().unwrap();
    for i in 0..5 {
        manager
            .schedule(ambient_request(
                format!("DEADLETTER{i:03}"),
                Utc::now() - Duration::minutes(i as i64 + 1),
            ))
            .unwrap();
    }
    manager
        .schedule(ScheduleRequest {
            wake_in_minutes: None,
            wake_at: Some(Utc::now() - Duration::minutes(1)),
            context: "DIRECTOVERDUE".to_string(),
            priority: Priority::High,
            target: ScheduleTarget::Session {
                session_id: "session_direct".into(),
            },
            created_by_session: "session_direct".into(),
            working_dir: None,
            task_description: None,
            relevant_files: vec![],
            git_branch: None,
            additional_context: None,
        })
        .unwrap();

    let runner = AmbientRunnerHandle::new(Arc::new(crate::safety::SafetySystem::new()));
    let status: serde_json::Value = serde_json::from_str(&runner.status_json().await).unwrap();
    assert_eq!(
        status["overdue_queue_count"], serde_json::json!(1),
        "overdue_queue_count must count only direct-delivery entries, got {status}"
    );
}

/// Writing to the queue must bound it. The overflow is archived, not dropped:
/// the carry-forward memory stays readable on disk.
#[test]
fn ambient_queue_retention_archives_overflow_instead_of_growing_forever() {
    let _guard = crate::storage::lock_test_env();
    let dir = tempfile::tempdir().unwrap();
    let _home = HomeGuard(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", dir.path());

    let mut manager = AmbientManager::new().unwrap();
    for i in 0..60 {
        manager
            .schedule(ambient_request(
                format!("RETAINCONTEXT{i:03}"),
                Utc::now() + Duration::minutes(i as i64 + 1),
            ))
            .unwrap();
    }

    let queue = AmbientManager::new().unwrap();
    let len = queue.queue().len();
    assert!(
        len <= 50,
        "the live queue must stay bounded, got {len} entries"
    );

    let archive: PathBuf = dir.path().join("ambient").join("queue_archive.jsonl");
    let archived = std::fs::read_to_string(&archive).unwrap_or_default();
    assert!(
        archived.contains("RETAINCONTEXT000"),
        "the overflow entry must be archived, not lost"
    );
}
