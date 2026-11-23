//! Client event deduplication, compaction, and session control handles.
// reference: https://github.com/1jehuang/jcode/blob/master/crates/jcode-app-core/src/server/client_event_control.rs - upstream extraction webfetched this session; fork fix e18cdb615 5s bounded busy-lock wait (git HEAD) restored into it

use super::*;

/// Session and payload of a todo snapshot, for skipping identical resends.
pub(super) fn todos_event_key(event: &ServerEvent) -> Option<(String, String)> {
    match event {
        ServerEvent::Todos {
            session_id,
            payload,
            ..
        } => Some((session_id.clone(), payload.clone())),
        _ => None,
    }
}

pub(super) fn server_reload_starting() -> bool {
    matches!(
        crate::server::recent_reload_state(RELOAD_STARTING_GUARD_MAX_AGE),
        Some(state) if state.phase == crate::server::ReloadPhase::Starting
    )
}

pub(super) fn compaction_server_event(event: crate::compaction::CompactionEvent) -> ServerEvent {
    ServerEvent::Compaction {
        trigger: event.trigger,
        pre_tokens: event.pre_tokens,
        post_tokens: event.post_tokens,
        tokens_saved: event.tokens_saved,
        duration_ms: event.duration_ms,
        messages_dropped: event.messages_dropped,
        messages_compacted: event.messages_compacted,
        summary_chars: event.summary_chars,
        active_messages: event.active_messages,
    }
}

pub(super) async fn poll_agent_compaction_completion(
    agent: Arc<Mutex<Agent>>,
) -> Option<ServerEvent> {
    let mut agent_guard = agent.lock().await;
    agent_guard
        .poll_compaction_completion_event()
        .map(compaction_server_event)
}

pub(super) async fn refresh_session_control_handle(
    session_id: &str,
    agent: &Arc<Mutex<Agent>>,
    shutdown_signals: &Arc<RwLock<HashMap<String, InterruptSignal>>>,
    soft_interrupt_queues: &SessionInterruptQueues,
) -> SessionControlHandle {
    let started = Instant::now();
    let agent_guard = match agent.try_lock() {
        Ok(agent_guard) => agent_guard,
        Err(_) => {
            crate::logging::warn(&format!(
                "refresh_session_control_handle: waiting for busy agent lock for session {}; cancel/control requests on this connection may be delayed",
                session_id
            ));
            let fallback_stop_signal = shutdown_signals.read().await.get(session_id).cloned();
            let fallback_soft_interrupt_queue =
                soft_interrupt_queues.read().await.get(session_id).cloned();
            if let Some(soft_interrupt_queue) = fallback_soft_interrupt_queue {
                // A missing shutdown-signal registration (e.g. a session created
                // through the headless spawn path) must not force this connection
                // to block on the busy agent mutex: cancels fired through the
                // handle fan out to the running turn's own signal via the
                // turn-cancel registry (issue #428), so a detached signal is a
                // safe stand-in.
                let stop_signal = fallback_stop_signal.unwrap_or_else(|| {
                    crate::logging::warn(&format!(
                        "refresh_session_control_handle: no registered shutdown signal for busy session {}; using detached signal (cancels reach the running turn via the turn cancel registry)",
                        session_id
                    ));
                    InterruptSignal::new()
                });
                crate::logging::warn(&format!(
                    "refresh_session_control_handle: using lock-free cancel-only control handle for busy session {} after {}ms",
                    session_id,
                    started.elapsed().as_millis()
                ));
                return SessionControlHandle::cancel_only(
                    session_id,
                    soft_interrupt_queue,
                    stop_signal,
                );
            }
            // The agent is busy and no soft-interrupt queue is registered for
            // this session. Waiting for the mutex would stall the connection
            // actor and leave interrupts unread, so bound the wait: after the
            // timeout the handle degrades to cancel-only (Escape still works).
            match tokio::time::timeout(Duration::from_millis(5000), agent.lock()).await {
                Ok(agent_guard) => {
                    crate::logging::warn(&format!(
                        "refresh_session_control_handle: acquired agent lock for session {} after {}ms",
                        session_id,
                        started.elapsed().as_millis()
                    ));
                    agent_guard
                }
                Err(_) => {
                    crate::logging::warn(&format!(
                        "refresh_session_control_handle: gave up waiting for the busy agent lock of session {} after {}ms; using a cancel-only handle",
                        session_id,
                        started.elapsed().as_millis()
                    ));
                    let stop_signal = fallback_stop_signal.unwrap_or_else(InterruptSignal::new);
                    // Keep a real queue. A fresh empty queue would strand the
                    // interrupts already queued for the running turn: the
                    // Agent owns its queue, but the Agent lock is exactly what
                    // we could not get. The session-scoped registry holds the
                    // same Arc for a live session, so prefer it.
                    let soft_interrupt_queue = soft_interrupt_queues
                        .read()
                        .await
                        .get(session_id)
                        .cloned()
                        .unwrap_or_else(|| Arc::new(std::sync::Mutex::new(Vec::new())));
                    return SessionControlHandle::cancel_only(
                        session_id,
                        soft_interrupt_queue,
                        stop_signal,
                    );
                }
            }
        }
    };
    SessionControlHandle::new(
        session_id,
        agent_guard.soft_interrupt_queue(),
        agent_guard.background_tool_signal(),
        agent_guard.graceful_shutdown_signal(),
    )
    .with_provider(agent_guard.provider_handle())
}
