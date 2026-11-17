// reference: https://socket.io/docs/v4/connection-state-recovery
//! A reconnecting client must reattach to its session, not mint a new one.
//!
//! MEASURED 2026-11-17: a client whose `SessionId` frame was never dispatched
//! re-subscribed with `target_session_id: None` and the server minted a new
//! session on every reconnect (~20/s during the storm). Each new session
//! registers its own MCP child set, so the reconnect churn became hundreds of
//! `jcode.exe` processes.
//!
//! The client instance id is stable across reconnects, so the server resolves
//! the missing target from its own `client_instance_id` -> session binding.
//! This test drives the server over a real socket pair: the client is NOT
//! given the session id on the second Subscribe, exactly like the failing
//! client.

use super::*;
use super::tests::IsolatedReloadRecoveryEnv;
use crate::message::{Message, ToolDefinition};
use crate::provider::EventStream;
use crate::server::client_instance_sessions::ClientInstanceSessions;
use async_trait::async_trait;

/// A provider that never runs: these tests only exercise Subscribe.
struct NoRequests;
#[async_trait]
impl Provider for NoRequests {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        anyhow::bail!("a subscribe test must not invoke a provider")
    }
    fn name(&self) -> &str {
        "mock"
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
}

struct ReconnectHarness {
    sessions: SessionAgents,
    client_instance_sessions: ClientInstanceSessions,
    provider: Arc<dyn Provider>,
    global_event_tx: broadcast::Sender<ServerEvent>,
    global_is_processing: Arc<RwLock<bool>>,
    global_session_id: Arc<RwLock<String>>,
    client_count: Arc<RwLock<usize>>,
    client_connections: Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    shared_context: Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
    swarm_plans: Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: Arc<RwLock<HashMap<String, String>>>,
    file_touch: FileTouchService,
    channel_subscriptions: Arc<RwLock<HashMap<String, HashMap<String, HashSet<String>>>>>,
    channel_subscriptions_by_session: Arc<RwLock<HashMap<String, HashMap<String, HashSet<String>>>>>,
    client_debug_state: Arc<RwLock<ClientDebugState>>,
    debug_response_tx: broadcast::Sender<(u64, String)>,
    event_history: Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: broadcast::Sender<SwarmEvent>,
    shutdown_signals: Arc<RwLock<HashMap<String, InterruptSignal>>>,
    soft_interrupt_queues: SessionInterruptQueues,
    mcp_pool: Arc<crate::mcp::SharedMcpPool>,
}

impl ReconnectHarness {
    fn new(provider: Arc<dyn Provider>) -> Self {
        let (global_event_tx, _) = broadcast::channel(16);
        let (debug_response_tx, _) = broadcast::channel(8);
        let (swarm_event_tx, _) = broadcast::channel(16);
        Self {
            sessions: Arc::new(crate::server::SessionManager::from_agents(HashMap::new())),
            client_instance_sessions:
                crate::server::client_instance_sessions::new_client_instance_sessions(),
            provider,
            global_event_tx,
            global_is_processing: Arc::new(RwLock::new(false)),
            global_session_id: Arc::new(RwLock::new(String::new())),
            client_count: Arc::new(RwLock::new(0usize)),
            client_connections: Arc::new(RwLock::new(HashMap::new())),
            swarm_members: Arc::new(RwLock::new(HashMap::new())),
            swarms_by_id: Arc::new(RwLock::new(HashMap::new())),
            shared_context: Arc::new(RwLock::new(HashMap::new())),
            swarm_plans: Arc::new(RwLock::new(HashMap::new())),
            swarm_coordinators: Arc::new(RwLock::new(HashMap::new())),
            file_touch: FileTouchService::new(),
            channel_subscriptions: Arc::new(RwLock::new(HashMap::new())),
            channel_subscriptions_by_session: Arc::new(RwLock::new(HashMap::new())),
            client_debug_state: Arc::new(RwLock::new(ClientDebugState::default())),
            debug_response_tx,
            event_history: Arc::new(RwLock::new(std::collections::VecDeque::new())),
            event_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            swarm_event_tx,
            shutdown_signals: Arc::new(RwLock::new(HashMap::new())),
            soft_interrupt_queues: Arc::new(RwLock::new(HashMap::new())),
            mcp_pool: Arc::new(crate::mcp::SharedMcpPool::from_default_config()),
        }
    }

    fn connect(&self) -> (tokio::task::JoinHandle<Result<()>>, crate::transport::Stream) {
        let (server_stream, client_stream) = crate::transport::Stream::pair().expect("socket pair");
        let task = tokio::spawn(handle_client(
            server_stream,
            Arc::clone(&self.sessions),
            self.global_event_tx.clone(),
            Arc::clone(&self.provider),
            Arc::clone(&self.global_is_processing),
            Arc::clone(&self.global_session_id),
            Arc::clone(&self.client_count),
            Arc::clone(&self.client_connections),
            Arc::clone(&self.swarm_members),
            Arc::clone(&self.swarms_by_id),
            Arc::clone(&self.shared_context),
            Arc::clone(&self.swarm_plans),
            Arc::clone(&self.swarm_coordinators),
            self.file_touch.clone(),
            Arc::clone(&self.channel_subscriptions),
            Arc::clone(&self.channel_subscriptions_by_session),
            Arc::clone(&self.client_debug_state),
            self.debug_response_tx.clone(),
            Arc::clone(&self.event_history),
            Arc::clone(&self.event_counter),
            self.swarm_event_tx.clone(),
            "jcode-test".to_string(),
            "ЁЯзк".to_string(),
            Arc::clone(&self.mcp_pool),
            Arc::clone(&self.shutdown_signals),
            Arc::clone(&self.soft_interrupt_queues),
            AwaitMembersRuntime::default(),
            SwarmMutationRuntime::default(),
            Arc::clone(&self.client_instance_sessions),
        ));
        (task, client_stream)
    }
}

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

/// A reconnect that carries the same `client_instance_id` but NO
/// `target_session_id` must reattach to the session that client already owns.
///
/// Before the fix this minted a brand-new session per reconnect, and each new
/// session registered its own MCP child set.
#[tokio::test]
async fn reconnect_without_target_reattaches_to_the_same_session() {
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let harness = ReconnectHarness::new(Arc::new(NoRequests));
    let client_instance_id = "client_reconnect_test";

    // First connection: a plain Subscribe creates the session.
    let (first_task, first_stream) = harness.connect();
    let (first_reader, mut first_writer) = first_stream.into_split();
    let mut first_reader = BufReader::new(first_reader);
    send(
        &mut first_writer,
        serde_json::to_value(Request::Subscribe {
            system_prompt: None,
            supports_pdf_panels: false,
            supports_user_response: false,
            supports_server_failover: false,
            supports_turn_identity: true,
            id: 1,
            working_dir: Some(home._home.path().to_string_lossy().into_owned()),
            selfdev: None,
            target_session_id: None,
            client_instance_id: Some(client_instance_id.to_string()),
            client_has_local_history: false,
            history_base: None,
            allow_session_takeover: false,
            crash_on_disconnect: false,
            continue_on_disconnect: false,
            terminal_env: Vec::new(),
        })
        .unwrap(),
    )
    .await;
    let ServerEvent::SessionId {
        session_id: first_session_id,
    } = until(&mut first_reader, |event| {
        matches!(event, ServerEvent::SessionId { .. })
    })
    .await
    else {
        unreachable!("expected a SessionId event")
    };

    // The connection dies without the client ever learning that id (the
    // measured failure: the frame sits unread in the socket).
    drop(first_writer);
    drop(first_reader);
    let _ = tokio::time::timeout(Duration::from_secs(5), first_task).await;

    // Second connection: SAME client instance, NO target session id.
    let (second_task, second_stream) = harness.connect();
    let (second_reader, mut second_writer) = second_stream.into_split();
    let mut second_reader = BufReader::new(second_reader);
    send(
        &mut second_writer,
        serde_json::to_value(Request::Subscribe {
            system_prompt: None,
            supports_pdf_panels: false,
            supports_user_response: false,
            supports_server_failover: false,
            supports_turn_identity: true,
            id: 2,
            working_dir: Some(home._home.path().to_string_lossy().into_owned()),
            selfdev: None,
            target_session_id: None,
            client_instance_id: Some(client_instance_id.to_string()),
            client_has_local_history: false,
            history_base: None,
            allow_session_takeover: false,
            crash_on_disconnect: false,
            continue_on_disconnect: false,
            terminal_env: Vec::new(),
        })
        .unwrap(),
    )
    .await;
    let ServerEvent::SessionId {
        session_id: second_session_id,
    } = until(&mut second_reader, |event| {
        matches!(event, ServerEvent::SessionId { .. })
    })
    .await
    else {
        unreachable!("expected a SessionId event")
    };

    assert_eq!(
        second_session_id, first_session_id,
        "a reconnect must reattach to the session it already owns, not mint a new one"
    );

    drop(second_writer);
    drop(second_reader);
    let _ = tokio::time::timeout(Duration::from_secs(5), second_task).await;
}

/// A DIFFERENT client instance must NOT inherit the previous client's session.
#[tokio::test]
async fn a_different_client_instance_does_not_inherit_the_session() {
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let harness = ReconnectHarness::new(Arc::new(NoRequests));

    let (first_task, first_stream) = harness.connect();
    let (first_reader, mut first_writer) = first_stream.into_split();
    let mut first_reader = BufReader::new(first_reader);
    send(
        &mut first_writer,
        serde_json::to_value(Request::Subscribe {
            system_prompt: None,
            supports_pdf_panels: false,
            supports_user_response: false,
            supports_server_failover: false,
            supports_turn_identity: true,
            id: 1,
            working_dir: Some(home._home.path().to_string_lossy().into_owned()),
            selfdev: None,
            target_session_id: None,
            client_instance_id: Some("client-a".to_string()),
            client_has_local_history: false,
            history_base: None,
            allow_session_takeover: false,
            crash_on_disconnect: false,
            continue_on_disconnect: false,
            terminal_env: Vec::new(),
        })
        .unwrap(),
    )
    .await;
    let ServerEvent::SessionId {
        session_id: first_session_id,
    } = until(&mut first_reader, |event| {
        matches!(event, ServerEvent::SessionId { .. })
    })
    .await
    else {
        unreachable!("expected a SessionId event")
    };

    let (second_task, second_stream) = harness.connect();
    let (second_reader, mut second_writer) = second_stream.into_split();
    let mut second_reader = BufReader::new(second_reader);
    send(
        &mut second_writer,
        serde_json::to_value(Request::Subscribe {
            system_prompt: None,
            supports_pdf_panels: false,
            supports_user_response: false,
            supports_server_failover: false,
            supports_turn_identity: true,
            id: 2,
            working_dir: Some(home._home.path().to_string_lossy().into_owned()),
            selfdev: None,
            target_session_id: None,
            client_instance_id: Some("client-b".to_string()),
            client_has_local_history: false,
            history_base: None,
            allow_session_takeover: false,
            crash_on_disconnect: false,
            continue_on_disconnect: false,
            terminal_env: Vec::new(),
        })
        .unwrap(),
    )
    .await;
    let ServerEvent::SessionId {
        session_id: second_session_id,
    } = until(&mut second_reader, |event| {
        matches!(event, ServerEvent::SessionId { .. })
    })
    .await
    else {
        unreachable!("expected a SessionId event")
    };

    assert_ne!(
        second_session_id, first_session_id,
        "a distinct client instance must get its own session"
    );

    drop(first_writer);
    drop(first_reader);
    drop(second_writer);
    drop(second_reader);
    let _ = tokio::time::timeout(Duration::from_secs(5), first_task).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), second_task).await;
}
