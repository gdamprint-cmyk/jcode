//! Interrupting a coordinator does NOT kill the workers it spawned.
//!
//! MEASURED 2026-11-16: the previous contract fired the hard turn-cancel
//! signal of every descendant and set its member status to `stopped`. 11 such
//! cascade events in one day killed 1-7 running workers each. The new contract
//! is: the interrupt stops the coordinator's own turn, and the workers get a
//! soft wrap-up notice so their in-flight work survives.

use super::{cancel_active_turns, notify_spawned_descendants_of_interrupt, swarm_descendants};

/// Field reports: files kept appearing in a stopped worker's directory minutes
/// after `swarm stop`, because stop dropped the handles without cancelling.
#[test]
fn stop_cancels_only_the_target_sessions_running_turns() {
    let target = format!("stop-target-{}", std::process::id());
    let other = format!("stop-other-{}", std::process::id());
    let target_signal = InterruptSignal::new();
    let other_signal = InterruptSignal::new();
    let _target_turn =
        crate::turn_cancel_registry::register_active_turn(&target, target_signal.clone());
    let _other_turn =
        crate::turn_cancel_registry::register_active_turn(&other, other_signal.clone());

    assert_eq!(cancel_active_turns(&target), 1);
    assert!(target_signal.is_set());
    assert!(!other_signal.is_set());
    assert_eq!(cancel_active_turns("stop-idle-session"), 0);
}
use crate::protocol::ServerEvent;
use crate::server::SwarmMember;
use jcode_agent_runtime::InterruptSignal;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{RwLock, mpsc};

fn member(
    session_id: &str,
    parent: Option<&str>,
) -> (SwarmMember, mpsc::UnboundedReceiver<ServerEvent>) {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    (
        SwarmMember {
            session_id: session_id.to_string(),
            event_tx,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: Some("swarm-1".to_string()),
            swarm_enabled: true,
            status: "running".to_string(),
            detail: None,
            task_label: None,
            friendly_name: Some(session_id.to_string()),
            report_back_to_session_id: parent.map(str::to_string),
            latest_completion_report: None,
            role: "agent".to_string(),
            joined_at: Instant::now(),
            last_status_change: Instant::now(),
            is_headless: true,
            output_tail: None,
            todo_progress: None,
            todo_items: Vec::new(),
            runtime: crate::protocol::SwarmMemberRuntime::default(),
        },
        event_rx,
    )
}

#[test]
fn swarm_descendants_follow_the_spawn_tree_only() {
    let mut members: HashMap<String, SwarmMember> = HashMap::new();
    for (id, parent) in [
        ("root", None),
        ("a", Some("root")),
        ("b", Some("a")),
        ("c", Some("root")),
        ("other-root", None),
        ("other-child", Some("other-root")),
    ] {
        members.insert(id.to_string(), member(id, parent).0);
    }
    assert_eq!(swarm_descendants(&members, "root"), vec!["a", "b", "c"]);
    assert_eq!(swarm_descendants(&members, "a"), vec!["b"]);
    assert!(swarm_descendants(&members, "b").is_empty());
    // A cycle must not loop or count the session as its own descendant.
    members.insert("x".to_string(), member("x", Some("y")).0);
    members.insert("y".to_string(), member("y", Some("x")).0);
    assert_eq!(swarm_descendants(&members, "x"), vec!["y"]);
}

/// Interrupting a coordinator must NOT cancel the running turns of the workers
/// it spawned (and theirs). They get a soft wrap-up notice instead, so their
/// in-flight work survives and they report back when they finish.
#[tokio::test]
async fn interrupting_a_coordinator_notifies_workers_without_cancelling_them() {
    let tag = format!("cascade-{}", std::process::id());
    let id = |name: &str| format!("{tag}-{name}");
    let mut members: HashMap<String, SwarmMember> = HashMap::new();
    let mut worker_rx = None;
    for (name, parent) in [
        ("coord", None),
        ("worker", Some("coord")),
        ("grandchild", Some("worker")),
        ("idle", Some("coord")),
        ("stranger", None),
    ] {
        let parent = parent.map(id);
        let (member, rx) = member(&id(name), parent.as_deref());
        if name == "worker" {
            worker_rx = Some(rx);
        }
        members.insert(id(name), member);
    }
    let swarm_members = Arc::new(RwLock::new(members));

    let worker_signal = InterruptSignal::new();
    let grandchild_signal = InterruptSignal::new();
    let stranger_signal = InterruptSignal::new();
    let _worker_turn =
        crate::turn_cancel_registry::register_active_turn(&id("worker"), worker_signal.clone());
    let _grandchild_turn = crate::turn_cancel_registry::register_active_turn(
        &id("grandchild"),
        grandchild_signal.clone(),
    );
    let _stranger_turn =
        crate::turn_cancel_registry::register_active_turn(&id("stranger"), stranger_signal.clone());

    // The workers have live turns but no live agent in this registry, so the
    // notice is persisted as a pending soft interrupt instead of queued on an
    // in-memory queue. A live worker always has its session file on disk;
    // that is what makes the persisted fallback reachable.
    let _env = crate::storage::lock_test_env();
    let home = tempfile::TempDir::new().expect("home");
    let previous_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", home.path());
    let sessions_dir = home.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    for name in ["worker", "grandchild"] {
        std::fs::write(
            sessions_dir.join(format!("{}.json", id(name))),
            format!(r#"{{"id":"{}","messages":[]}}"#, id(name)),
        )
        .expect("session file");
    }

    let sessions: super::super::SessionAgents =
        Arc::new(crate::server::session_manager::SessionManager::from_agents(HashMap::new()));
    let soft_interrupt_queues: super::super::SessionInterruptQueues =
        Arc::new(RwLock::new(HashMap::new()));

    let notified = notify_spawned_descendants_of_interrupt(
        &id("coord"),
        &swarm_members,
        &sessions,
        &soft_interrupt_queues,
    )
    .await;

    // The workers with a live turn were notified.
    assert_eq!(notified, vec![id("grandchild"), id("worker")]);
    // Their turns were NOT cancelled: this is the whole point of the fix.
    assert!(
        !worker_signal.is_set(),
        "a spawned worker turn must survive the coordinator's interrupt"
    );
    assert!(
        !grandchild_signal.is_set(),
        "a nested worker turn must survive the coordinator's interrupt"
    );
    assert!(!stranger_signal.is_set(), "unrelated session untouched");
    let members = swarm_members.read().await;
    assert_eq!(
        members[&id("worker")].status, "running",
        "a notified worker keeps running"
    );
    assert_eq!(members[&id("grandchild")].status, "running");
    assert_eq!(members[&id("idle")].status, "running", "no turn, no change");
    assert_eq!(members[&id("stranger")].status, "running");
    drop(members);
    let mut worker_rx = worker_rx.expect("worker receiver");
    let mut events = Vec::new();
    while let Ok(event) = worker_rx.try_recv() {
        events.push(event);
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            ServerEvent::Notification {
                notification_type: crate::protocol::NotificationType::Message { .. },
                ..
            }
        )),
        "the worker is told its coordinator was interrupted: {events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            ServerEvent::TurnStopped {
                reason: crate::protocol::TurnStopReason::Interrupted,
                ..
            }
        )),
        "the worker turn must not be reported as stopped: {events:?}"
    );

    if let Some(value) = previous_home {
        crate::env::set_var("JCODE_HOME", value);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}
