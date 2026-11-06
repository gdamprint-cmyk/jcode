// reference: https://github.com/1jehuang/jcode/commit/8e4c36d89415b68025fe82e73afd39edb496b183 (crates/jcode-app-core/src/server/client_lifecycle_socket_tests_idle_soft_interrupt.rs)

use super::*;

/// A soft interrupt submitted without a `client_id` (the TUI path) on an idle
/// session is picked up as the next turn. The pickup must be published to the
/// caller, otherwise the TUI keeps its queued preview forever and the prompt
/// never appears to be delivered.
#[tokio::test]
async fn queued_message_stuck_idle_soft_interrupt_without_client_id_is_acknowledged() {
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let terminal_provider = TerminalSocketProvider::default();
    let provider_template: Arc<dyn Provider> = Arc::new(terminal_provider.clone());

    let sessions: SessionAgents =
        Arc::new(crate::server::SessionManager::from_agents(HashMap::new()));
    let global_session_id = Arc::new(RwLock::new(String::new()));
    let client_count = Arc::new(RwLock::new(0usize));
    let client_connections = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::new()));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
    let shared_context = Arc::new(RwLock::new(HashMap::new()));
    let swarm_plans = Arc::new(RwLock::new(HashMap::new()));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::new()));
    let file_touch = FileTouchService::new();
    let channel_subscriptions = Arc::new(RwLock::new(HashMap::new()));
    let channel_subscriptions_by_session = Arc::new(RwLock::new(HashMap::new()));
    let client_debug_state = Arc::new(RwLock::new(ClientDebugState::default()));
    let (_debug_response_tx, _) = broadcast::channel(8);
    let event_history = Arc::new(RwLock::new(std::collections::VecDeque::new()));
    let event_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (swarm_event_tx, _) = broadcast::channel(8);
    let (_global_event_tx, _) = broadcast::channel(8);
    let global_is_processing = Arc::new(RwLock::new(false));
    let shutdown_signals = Arc::new(RwLock::new(HashMap::new()));
    let soft_interrupt_queues: SessionInterruptQueues = Arc::new(RwLock::new(HashMap::new()));
    let mcp_pool = Arc::new(crate::mcp::SharedMcpPool::from_default_config());

    let (server_stream, client_stream) = crate::transport::Stream::pair().expect("socket pair");
    let server_task = tokio::spawn(handle_client(
        server_stream,
        Arc::clone(&sessions),
        _global_event_tx.clone(),
        provider_template.clone(),
        global_is_processing.clone(),
        global_session_id.clone(),
        client_count.clone(),
        Arc::clone(&client_connections),
        swarm_members.clone(),
        swarms_by_id.clone(),
        shared_context.clone(),
        swarm_plans.clone(),
        swarm_coordinators.clone(),
        file_touch.clone(),
        channel_subscriptions.clone(),
        channel_subscriptions_by_session.clone(),
        client_debug_state.clone(),
        _debug_response_tx.clone(),
        event_history.clone(),
        event_counter.clone(),
        swarm_event_tx.clone(),
        "jcode-test".to_string(),
        "🧪".to_string(),
        mcp_pool.clone(),
        shutdown_signals.clone(),
        soft_interrupt_queues.clone(),
        AwaitMembersRuntime::default(),
        SwarmMutationRuntime::default(),
    ));
    let (client_reader, mut client_writer) = client_stream.into_split();
    let mut client_reader = BufReader::new(client_reader);

    async fn send(writer: &mut crate::transport::WriteHalf, value: serde_json::Value) {
        writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn until(
        reader: &mut BufReader<crate::transport::ReadHalf>,
        predicate: impl Fn(&ServerEvent) -> bool,
    ) -> ServerEvent {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                let event: ServerEvent = serde_json::from_str(&line).unwrap();
                if predicate(&event) {
                    return event;
                }
            }
        })
        .await
        .expect("daemon event timeout")
    }

    use serde_json::json;
    send(
        &mut client_writer,
        json!({"type":"subscribe","id":1,"working_dir":home._home.path()}),
    )
    .await;
    until(&mut client_reader, |e| {
        matches!(e, ServerEvent::Done { id: 1 })
    })
    .await;

    // No `client_id`: this is exactly what the TUI sends.
    send(
        &mut client_writer,
        json!({"type":"soft_interrupt","id":2,"content":"one shared turn"}),
    )
    .await;

    // The idle session must pick the interrupt up as the next turn and say so.
    let injected = until(&mut client_reader, |event| {
        matches!(event, ServerEvent::SoftInterruptInjected { .. })
    })
    .await;
    let ServerEvent::SoftInterruptInjected {
        content,
        point,
        client_ids,
        ..
    } = injected
    else {
        panic!("expected a soft interrupt pickup event")
    };
    assert_eq!(content, "one shared turn");
    assert_eq!(point, "turn_start");
    assert!(
        client_ids.is_empty(),
        "a caller without a client_id must still be told about the pickup: {client_ids:?}"
    );

    // Let the picked-up turn finish so the daemon can shut down cleanly.
    terminal_provider.finish_first.notify_one();
    until(&mut client_reader, |e| {
        matches!(e, ServerEvent::Done { id: 2 })
    })
    .await;

    sessions.clear_for_test().await;
    drop(client_writer);
    drop(client_reader);
    tokio::time::timeout(Duration::from_secs(5), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
