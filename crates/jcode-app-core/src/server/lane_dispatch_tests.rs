use super::{capacity_recovery_candidates, dispatchable_lanes};
use crate::protocol::{ServerEvent, SwarmMemberRuntime};
use crate::server::SwarmMember;
use crate::todo::TodoItem;
use std::collections::HashMap;
use tokio::sync::mpsc;
use std::time::Instant;

fn member(session_id: &str, swarm_id: &str, status: &str, owner: Option<&str>) -> SwarmMember {
    let (event_tx, _event_rx) = mpsc::unbounded_channel::<ServerEvent>();
    SwarmMember {
        session_id: session_id.to_string(),
        event_tx,
        event_txs: HashMap::new(),
        working_dir: None,
        swarm_id: Some(swarm_id.to_string()),
        swarm_enabled: true,
        status: status.to_string(),
        detail: None,
        task_label: None,
        friendly_name: Some(session_id.to_string()),
        report_back_to_session_id: owner.map(str::to_string),
        latest_completion_report: None,
        role: "agent".to_string(),
        joined_at: Instant::now(),
        last_status_change: Instant::now(),
        is_headless: true,
        output_tail: None,
        todo_progress: None,
        todo_items: Vec::new(),
        runtime: SwarmMemberRuntime::default(),
    }
}

fn members(map: Vec<SwarmMember>) -> HashMap<String, SwarmMember> {
    map.into_iter().map(|m| (m.session_id.clone(), m)).collect()
}

fn item(id: &str, stage: u32, lane: &str, owner: Option<&str>) -> TodoItem {
    TodoItem {
        id: id.to_string(),
        content: format!("task {id}"),
        status: "pending".to_string(),
        priority: "high".to_string(),
        stage: Some(stage),
        group: Some(lane.to_string()),
        lane_owner: owner.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn self_owned_lanes_never_dispatch() {
    let todos = vec![item("a", 1, "api", None), item("b", 1, "docs", None)];
    assert!(dispatchable_lanes(&todos).is_empty());
}

#[test]
fn ready_worker_lanes_in_one_phase_start_together() {
    let todos = vec![
        item("a", 1, "api", Some("worker")),
        item("b", 1, "docs", Some("worker")),
        item("c", 1, "mine", None),
    ];
    let lanes = dispatchable_lanes(&todos);
    assert_eq!(lanes.len(), 2, "{lanes:?}");
}

#[test]
fn later_phase_waits_then_unlocks_on_completion() {
    let mut todos = vec![
        item("a", 1, "api", None),
        item("b", 2, "client", Some("worker")),
    ];
    assert!(dispatchable_lanes(&todos).is_empty());
    todos[0].status = "completed".to_string();
    assert_eq!(dispatchable_lanes(&todos).len(), 1);
}

#[test]
fn explicit_dependency_unlocks_lane_early() {
    let mut consumer = item("c", 2, "client", Some("worker"));
    consumer.lane_after = Some(vec!["a".into()]);
    let mut todos = vec![
        item("a", 1, "api", None),
        item("b", 1, "api", None),
        consumer,
    ];
    assert!(dispatchable_lanes(&todos).is_empty());
    todos[0].status = "completed".to_string();
    assert_eq!(dispatchable_lanes(&todos).len(), 1);
}

#[test]
fn owned_lane_is_not_dispatched_again() {
    let mut todos = vec![
        item("a", 1, "api", Some("worker")),
        item("b", 1, "api", Some("worker")),
    ];
    assert_eq!(dispatchable_lanes(&todos).len(), 1);
    todos[0].assigned_to = Some("worker-1".into());
    todos[1].assigned_to = Some("worker-1".into());
    // Plan edits and reloads re-evaluate the saved list; the persisted owner
    // keeps the lane from getting a second worker.
    todos.push({
        let mut extra = item("x", 1, "api", Some("worker"));
        extra.assigned_to = Some("worker-1".into());
        extra
    });
    assert!(dispatchable_lanes(&todos).is_empty());
}

#[test]
fn completed_lane_is_not_dispatched() {
    let mut done = item("a", 1, "api", Some("worker"));
    done.status = "completed".into();
    assert!(dispatchable_lanes(&[done]).is_empty());
}

#[test]
fn normalize_makes_owner_lane_wide() {
    let mut todos = vec![
        item("a", 1, "api", Some("worker")),
        item("b", 1, "api", None),
        item("c", 1, "docs", Some("self")),
    ];
    crate::todo::stages::normalize_lane_owners(&mut todos);
    assert_eq!(todos[1].lane_owner.as_deref(), Some("worker"));
    assert_eq!(todos[2].lane_owner, None);
}

#[test]
fn cap_recovery_retires_idle_owned_slot_holders_only() {
    let map = members(vec![
        member("coord", "swarm-1", "ready", None),
        // Idle, owned, slot-holding: recoverable.
        member("idle-worker", "swarm-1", "ready", Some("coord")),
        member("legacy-idle", "swarm-1", "", Some("coord")),
        // In-flight: must survive.
        member("busy-worker", "swarm-1", "running", Some("coord")),
        // Terminal: does not consume capacity, so stopping it frees nothing.
        member("done-worker", "swarm-1", "completed", Some("coord")),
        member("terminal", "swarm-1", "failed", Some("coord")),
        // Another coordinator's agent: not ours to stop.
        member("foreign", "swarm-1", "ready", Some("other")),
        // Excluded by the caller (assigned earlier in this dispatch loop).
        member("excluded", "swarm-1", "ready", Some("coord")),
        // Different swarm: out of scope.
        member("other-swarm", "swarm-2", "ready", Some("coord")),
    ]);

    let candidates = capacity_recovery_candidates(&map, "coord", "swarm-1", &["excluded".to_string()]);
    assert_eq!(candidates, vec!["idle-worker".to_string(), "legacy-idle".to_string()]);

    // Negative control: the status filter is what carries the fix. Drop it and
    // the completed worker becomes a candidate, i.e. the rule would stop a
    // worker that frees no slot.
    let without_status_filter: Vec<String> = {
        let mut ids: Vec<String> = map
            .values()
            .filter(|member| member.session_id != "coord")
            .filter(|member| member.swarm_id.as_deref() == Some("swarm-1"))
            // No capacity and no status filter: this is the pre-fix shape.
            .filter(|member| member.report_back_to_session_id.as_deref() == Some("coord"))
            .filter(|member| member.session_id != "excluded")
            .map(|member| member.session_id.clone())
            .collect();
        ids.sort();
        ids
    };
    assert!(
        without_status_filter.iter().any(|id| id == "done-worker"),
        "the status filter must be what keeps terminal workers out"
    );
}

#[test]
fn cap_recovery_excludes_the_coordinator_and_unknown_members() {
    let map = members(vec![
        member("coord", "swarm-1", "completed", None),
        member("orphan", "swarm-1", "completed", None),
    ]);
    assert!(capacity_recovery_candidates(&map, "coord", "swarm-1", &[]).is_empty());
}
