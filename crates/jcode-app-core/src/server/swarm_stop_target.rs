//! Ownership and unique target resolution for swarm stop requests.

use super::*;

pub(super) fn swarm_stop_allowed_by_owner(
    req_session_id: &str,
    target_member: &SwarmMember,
    force: bool,
) -> bool {
    force || target_member.report_back_to_session_id.as_deref() == Some(req_session_id)
}

pub(super) async fn resolve_stop_target_session(
    swarm_id: &str,
    target: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> std::result::Result<String, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("target_session is required.".to_string());
    }

    let members = swarm_members.read().await;
    if members
        .get(target)
        .is_some_and(|member| member.swarm_id.as_deref() == Some(swarm_id))
    {
        return Ok(target.to_string());
    }

    let mut matches = members
        .iter()
        .filter(|(_, member)| member.swarm_id.as_deref() == Some(swarm_id))
        .filter(|(session_id, member)| {
            member.friendly_name.as_deref() == Some(target)
                || session_id.starts_with(target)
                || session_id.ends_with(target)
        })
        .map(|(session_id, member)| {
            (
                session_id.clone(),
                member
                    .friendly_name
                    .as_deref()
                    .unwrap_or(session_id)
                    .to_string(),
            )
        })
        .collect::<Vec<_>>();
    matches.sort_by(|a, b| a.0.cmp(&b.0));

    match matches.len() {
        0 => Err(format!(
            "Unknown swarm session '{target}'. Use an exact session ID, unique friendly name, or unique session ID prefix/suffix."
        )),
        1 => Ok(matches.remove(0).0),
        _ => Err(format!(
            "Ambiguous swarm session '{target}' matched: {}. Use an exact session ID.",
            matches
                .iter()
                .map(|(session_id, friendly)| format!("{friendly} [{session_id}]"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Tear one member session down: cancel its turn, drop the server session and
/// remove it from the swarm. Returns whether anything was actually removed.
///
/// Shared by the explicit `stop` request (`handle_comm_stop`, which keeps its
/// mutation replay and permission check) and the automatic member-cap recovery
/// in lane dispatch, so both paths retire a worker identically. Callers must
/// have authorised the stop for `req_session_id`.
pub(crate) async fn stop_swarm_member_now(
    target_session: &str,
    req_session_id: &str,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    channel_subscriptions: &ChannelSubscriptions,
    channel_subscriptions_by_session: &ChannelSubscriptions,
    event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
    soft_interrupt_queues: &SessionInterruptQueues,
) -> bool {
    let _ = fanout_session_event(
        swarm_members,
        target_session,
        ServerEvent::SessionCloseRequested {
            reason: format!("Stopped by coordinator {req_session_id}"),
        },
    )
    .await;

    // Cancel the worker's in-flight turn before its handles are removed, or it
    // keeps generating and editing files with nothing left to stop it by.
    super::super::swarm::cancel_active_turns(target_session);

    let removed_agent = super::super::remove_session_entry(sessions, target_session).await;
    let removed_live_agent = removed_agent.is_some();
    if let Some(agent_arc) = removed_agent {
        remove_session_interrupt_queue(soft_interrupt_queues, target_session).await;
        remove_background_tool_signal(target_session);
        if let Ok(mut agent) = agent_arc.try_lock() {
            agent.mark_closed();
            let memory_enabled = agent.memory_enabled();
            let transcript = if memory_enabled {
                Some(agent.build_transcript_for_extraction())
            } else {
                None
            };
            let sid = target_session.to_string();
            let working_dir = agent.working_dir().map(|dir| dir.to_string());
            drop(agent);
            if let Some(transcript) = transcript {
                crate::memory_agent::trigger_final_extraction_with_dir(
                    transcript,
                    sid,
                    working_dir,
                );
            }
        } else {
            // The agent was mid-turn so its lock was busy; still record that
            // the session is closed, or it looks alive on disk forever (and
            // reload keeps re-announcing awaits on it).
            if let Ok(mut session) = crate::session::Session::load(target_session) {
                session.mark_closed();
                // save_prepared: a worker stopped before it wrote any message
                // must still persist Closed (plain save skips empty sessions).
                if let Err(error) = session.save_prepared() {
                    crate::logging::warn(&format!(
                        "stop {target_session}: failed to persist closed status: {error}"
                    ));
                }
            }
        }
    }

    let (removed_swarm_id, removed_name) = {
        let mut members = swarm_members.write().await;
        if let Some(member) = members.remove(target_session) {
            (member.swarm_id, member.friendly_name)
        } else {
            (None, None)
        }
    };
    if let Some(ref swarm_id) = removed_swarm_id {
        record_swarm_event(
            event_history,
            event_counter,
            swarm_event_tx,
            target_session.to_string(),
            removed_name.clone(),
            Some(swarm_id.clone()),
            SwarmEventType::MemberChange {
                action: "left".to_string(),
            },
        )
        .await;
        remove_session_from_swarm(
            target_session,
            swarm_id,
            swarm_members,
            swarms_by_id,
            swarm_coordinators,
            swarm_plans,
        )
        .await;
    }
    remove_session_channel_subscriptions(
        target_session,
        channel_subscriptions,
        channel_subscriptions_by_session,
    )
    .await;

    removed_live_agent || removed_swarm_id.is_some()
}
