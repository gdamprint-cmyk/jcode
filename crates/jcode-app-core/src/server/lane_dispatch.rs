//! Automatic worker dispatch for worker-owned todo lanes.
//!
// reference: no prior art, searched "cleanup_finished_workers_for_capacity" and
// "member cap recovery" in the jcode fork repo (github.com/1jehuang/jcode, commit 93cb1e471,
// crates/jcode-app-core/src/tool/communicate.rs:278) and upstream 553db9088; upstream never had the
// helper, the fork's copy was dropped by 2f2b78796. Re-implemented server-side here.
//!
//! A root session marks a todo lane `owner: "worker"`. Whenever its todo list
//! is saved (including mirrored saves when a worker finishes a lane item), the
//! server spawns one swarm worker for every worker-owned lane whose
//! dependencies are satisfied and that has no owner yet. There is no launch
//! call and no per-plan concurrency number: lane dependencies decide when a
//! lane starts, and the global `agents.swarm_max_concurrent_agents` cap holds
//! back the rest as queued until capacity frees up.
//!
//! The spawned worker is recorded on the lane's `assigned_to` before the save
//! that follows, so repeated saves, plan edits and server reloads never spawn
//! a duplicate for the same lane.

use super::comm_session::handle_comm_spawn;
use super::comm_session::swarm_stop_target::stop_swarm_member_now;
use super::swarm_mutation_state::SwarmMutationRuntime;
use super::{
    ChannelSubscriptions, ClientConnectionInfo, SessionAgents, SessionInterruptQueues, SwarmEvent,
    SwarmMember, SwarmState,
};
use crate::protocol::ServerEvent;
use crate::provider::Provider;
use crate::todo::TodoItem;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;
use tokio::sync::{OnceCell, RwLock, broadcast, mpsc};

/// Server-owned handles the dispatcher needs to spawn workers. Registered once
/// when the server starts; without it, dispatch is a no-op (tests, embedders).
pub(super) struct LaneDispatchContext {
    pub provider: Arc<dyn Provider>,
    pub sessions: SessionAgents,
    pub global_session_id: Arc<RwLock<String>>,
    pub swarm_state: SwarmState,
    pub channel_subscriptions: ChannelSubscriptions,
    pub channel_subscriptions_by_session: ChannelSubscriptions,
    pub event_history: Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    pub event_counter: Arc<std::sync::atomic::AtomicU64>,
    pub swarm_event_tx: broadcast::Sender<SwarmEvent>,
    pub mcp_pool: Arc<OnceCell<Arc<crate::mcp::SharedMcpPool>>>,
    pub soft_interrupt_queues: SessionInterruptQueues,
    pub swarm_mutation_runtime: SwarmMutationRuntime,
    pub client_connections: Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
}

static CONTEXT: OnceLock<LaneDispatchContext> = OnceLock::new();

pub(super) fn register_context(context: LaneDispatchContext) {
    if CONTEXT.set(context).is_err() {
        crate::logging::warn(
            "lane dispatch context already registered, retaining original context",
        );
    }
}

/// After a server start or reload, re-run dispatch for every root member so
/// lanes that became ready while the server was down (or whose spawn was cut
/// off mid-flight) still get a worker. Lanes already owned are skipped by the
/// persisted `assigned_to`, so this never duplicates a worker.
pub(super) fn spawn_startup_scan(delay: Duration) {
    let Some(context) = CONTEXT.get() else {
        return;
    };
    let members = Arc::clone(&context.swarm_state.members);
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let roots: Vec<String> = members
            .read()
            .await
            .values()
            .filter(|member| member.report_back_to_session_id.is_none())
            .map(|member| member.session_id.clone())
            .collect();
        for session_id in roots {
            if let Ok(todos) = crate::todo::load_todos(&session_id) {
                on_todos_saved(&session_id, &todos);
            }
        }
    });
}

/// Lanes with a spawn in flight, keyed by `(coordinator, lane id)`. The
/// assignment is persisted by the spawn itself; this only covers the window
/// before that save lands.
fn in_flight() -> &'static StdMutex<HashSet<(String, String)>> {
    static IN_FLIGHT: OnceLock<StdMutex<HashSet<(String, String)>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(|| StdMutex::new(HashSet::new()))
}

/// Coordinators with lanes held back by the live-agent cap. Retried on a
/// timer because capacity frees up through events in other sessions.
fn queued_sessions() -> &'static StdMutex<HashSet<String>> {
    static QUEUED: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();
    QUEUED.get_or_init(|| StdMutex::new(HashSet::new()))
}

const QUEUED_RETRY_INTERVAL: Duration = Duration::from_secs(10);

/// Ready, unowned, worker-owned lanes of `todos`, in plan order.
///
/// A lane is ready when its plan item has no open blockers. It is unowned when
/// no open member of the lane has `assigned_to` set.
pub(crate) fn dispatchable_lanes(todos: &[TodoItem]) -> Vec<String> {
    let open = |todo: &&TodoItem| !matches!(todo.status.as_str(), "completed" | "cancelled");
    super::swarm_todo_link::plan_items_from_todos(todos)
        .into_iter()
        .filter(|item| item.blocked_by.is_empty())
        .filter(|item| {
            let members: Vec<&TodoItem> = todos
                .iter()
                .filter(open)
                .filter(|todo| crate::todo::stages::lane_id(todo) == item.id)
                .collect();
            !members.is_empty()
                && members
                    .iter()
                    .any(|todo| crate::todo::stages::lane_requests_worker(todo))
                && members.iter().all(|todo| todo.assigned_to.is_none())
        })
        .map(|item| item.id)
        .collect()
}

/// React to a todo save: spawn workers for newly dispatchable lanes.
pub(super) fn on_todos_saved(session_id: &str, todos: &[TodoItem]) {
    if CONTEXT.get().is_none() {
        return;
    }
    let lanes = dispatchable_lanes(todos);
    if lanes.is_empty() {
        queued_sessions()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
        return;
    }
    let session_id = session_id.to_string();
    tokio::spawn(async move { dispatch(session_id, lanes).await });
}

async fn dispatch(session_id: String, lanes: Vec<String>) {
    let Some(context) = CONTEXT.get() else {
        return;
    };
    // Only a root session (no spawner) coordinates worker lanes. Workers get
    // their lane with owners cleared, so this is defense in depth.
    let is_root = context
        .swarm_state
        .members
        .read()
        .await
        .get(&session_id)
        .is_some_and(|member: &SwarmMember| member.report_back_to_session_id.is_none());
    if !is_root {
        return;
    }
    for lane in lanes.clone() {
        let key = (session_id.clone(), lane.clone());
        if !in_flight()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.clone())
        {
            continue;
        }
        // Re-read from disk: the triggering event may predate an assignment
        // persisted by an earlier dispatch of the same lane.
        let still_dispatchable = crate::todo::load_todos(&session_id)
            .map(|todos| dispatchable_lanes(&todos).contains(&lane))
            .unwrap_or(false);
        if !still_dispatchable {
            in_flight()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&key);
            continue;
        }
        let outcome = spawn_lane_worker(context, &session_id, &lane).await;
        in_flight()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
        match outcome {
            Ok(worker) => crate::logging::info(&format!(
                "lane dispatch: {session_id} lane {lane} -> worker {worker}"
            )),
            Err(message) if is_capacity_error(&message) => {
                crate::logging::info(&format!(
                    "lane dispatch: {session_id} lane {lane} queued: {message}"
                ));
                // Re-ported fork recovery: a cap refusal used to retire this
                // coordinator's idle workers before waiting. Without it their
                // slots stay held until an explicit `swarm stop`, and a large
                // plan wedges at the cap.
                // Protect lanes already assigned earlier in this loop whose
                // queued status may not have propagated yet.
                let exclude: Vec<String> = lanes
                    .iter()
                    .flat_map(|pending| {
                        crate::todo::load_todos(&session_id)
                            .ok()
                            .into_iter()
                            .flatten()
                            .filter(|todo| crate::todo::stages::lane_id(todo) == *pending)
                            .filter_map(|todo| todo.assigned_to.clone())
                            .collect::<Vec<_>>()
                    })
                    .collect();
                let stopped = recover_member_capacity(context, &session_id, &exclude).await;
                if stopped > 0 {
                    crate::logging::info(&format!(
                        "lane dispatch: member-cap recovery stopped {stopped} finished worker(s) for {session_id}"
                    ));
                }
                schedule_queued_retry(&session_id);
                // Later lanes would hit the same cap.
                break;
            }
            Err(message) => crate::logging::warn(&format!(
                "lane dispatch: {session_id} lane {lane} failed: {message}"
            )),
        }
    }
}

fn is_capacity_error(message: &str) -> bool {
    message.contains("live-agent limit reached") || message.contains("member limit reached")
}

/// Free swarm member slots by stopping this coordinator's finished workers.
///
/// The spawn path refuses new workers while the live-agent cap is full
/// (`comm_session.rs` cap check). A lane blocked only by the cap used to wait
/// for its own finished workers to be stopped; after the upstream merge that
/// recovery was dropped and the cap error only schedules a retry, so idle
/// workers held their slots until an explicit `swarm stop`. This restores the
/// recovery on the server side: on a cap refusal, retire the coordinator's
/// idle, slot-holding workers and re-queue the lane.
///
/// Returns the number of workers actually stopped.
async fn recover_member_capacity(
    context: &LaneDispatchContext,
    session_id: &str,
    exclude: &[String],
) -> usize {
    let swarm_id = {
        let members = context.swarm_state.members.read().await;
        members
            .get(session_id)
            .and_then(|member| member.swarm_id.clone())
    };
    let Some(swarm_id) = swarm_id else {
        return 0;
    };

    let candidates = {
        let members = context.swarm_state.members.read().await;
        capacity_recovery_candidates(&members, session_id, &swarm_id, exclude)
    };

    let mut stopped = 0usize;
    for target in candidates {
        let removed = stop_swarm_member_now(
            &target,
            session_id,
            &context.sessions,
            &context.swarm_state.members,
            &context.swarm_state.swarms_by_id,
            &context.swarm_state.coordinators,
            &context.swarm_state.plans,
            &context.channel_subscriptions,
            &context.channel_subscriptions_by_session,
            &context.event_history,
            &context.event_counter,
            &context.swarm_event_tx,
            &context.soft_interrupt_queues,
        )
        .await;
        if removed {
            stopped += 1;
        }
    }
    stopped
}

/// Finished, capacity-consuming workers of `session_id` in `swarm_id` that a
/// cap-blocked dispatch may retire. Pure so the selection rule is testable.
///
/// A candidate must be *alive and idle*: `member_consumes_swarm_capacity` is
/// false for every terminal status (`completed`/`failed`/`stopped`/...), so a
/// completed worker holds no slot and stopping it would free nothing. The
/// statuses that do hold a slot and are safe to stop are the idle ones
/// (`ready` and the empty legacy status). Excludes the coordinator itself,
/// foreign owners, in-flight workers and the `exclude` set (lanes assigned
/// earlier in this dispatch loop).
fn capacity_recovery_candidates(
    members: &HashMap<String, SwarmMember>,
    session_id: &str,
    swarm_id: &str,
    exclude: &[String],
) -> Vec<String> {
    const IDLE_CANDIDATE_STATUSES: [&str; 2] = ["ready", ""];
    let mut ids: Vec<String> = members
        .values()
        .filter(|member| member.session_id != session_id)
        .filter(|member| member.swarm_id.as_deref() == Some(swarm_id))
        .filter(|member| super::member_consumes_swarm_capacity(member))
        .filter(|member| IDLE_CANDIDATE_STATUSES.contains(&member.status.as_str()))
        // Only this coordinator's own workers: a sibling coordinator's agents
        // are not ours to retire.
        .filter(|member| member.report_back_to_session_id.as_deref() == Some(session_id))
        .filter(|member| !exclude.iter().any(|assigned| assigned == &member.session_id))
        .map(|member| member.session_id.clone())
        .collect();
    ids.sort();
    ids
}

fn schedule_queued_retry(session_id: &str) {
    let newly_queued = queued_sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(session_id.to_string());
    if !newly_queued {
        return;
    }
    let session_id = session_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(QUEUED_RETRY_INTERVAL).await;
        let still_queued = queued_sessions()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session_id);
        if !still_queued {
            return;
        }
        let Ok(todos) = crate::todo::load_todos(&session_id) else {
            return;
        };
        let lanes = dispatchable_lanes(&todos);
        if lanes.is_empty() {
            return;
        }
        // Dispatch re-inserts the session when the cap still holds. That
        // insert schedules a fresh delayed retry, so this task ends here.
        dispatch(session_id, lanes).await;
    });
}

async fn spawn_lane_worker(
    context: &LaneDispatchContext,
    session_id: &str,
    lane: &str,
) -> Result<String, String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerEvent>();
    let mcp_pool = super::get_shared_mcp_pool(&context.mcp_pool).await;
    handle_comm_spawn(
        0,
        session_id.to_string(),
        Some(lane.to_string()),
        None,
        None,
        // A fresh nonce per attempt: a lane that was reopened after its
        // worker ended must get a new worker, not a replayed old response.
        // Duplicate dispatch is prevented by `in_flight` and `assigned_to`.
        Some(format!(
            "lane-dispatch:{lane}:{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_else(|error| {
                    crate::logging::warn(&format!(
                        "lane dispatch clock is before Unix epoch: {error}"
                    ));
                    0
                })
        )),
        None,
        None,
        None,
        None,
        &tx,
        &context.sessions,
        &context.global_session_id,
        &context.provider,
        &context.swarm_state.members,
        &context.swarm_state.swarms_by_id,
        &context.swarm_state.coordinators,
        &context.swarm_state.plans,
        &context.channel_subscriptions,
        &context.channel_subscriptions_by_session,
        &context.event_history,
        &context.event_counter,
        &context.swarm_event_tx,
        &mcp_pool,
        &context.soft_interrupt_queues,
        &context.swarm_mutation_runtime,
        &context.client_connections,
    )
    .await;
    drop(tx);
    match rx.recv().await {
        Some(ServerEvent::CommSpawnResponse { new_session_id, .. })
            if !new_session_id.is_empty() =>
        {
            Ok(new_session_id)
        }
        Some(ServerEvent::Error { message, .. }) => Err(message),
        Some(other) => Err(format!("unexpected spawn response: {other:?}")),
        None => Err("spawn produced no response".to_string()),
    }
}

#[cfg(test)]
#[path = "lane_dispatch_tests.rs"]
mod tests;
