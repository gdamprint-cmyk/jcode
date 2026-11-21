#![allow(clippy::await_holding_lock)]

use super::*;
use crate::protocol::ServerEvent;
use crate::provider::{EventStream, Provider};
use crate::session::{Session, SessionStatus};
use crate::tool::Registry;
use async_trait::async_trait;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::time::timeout;

struct NoRequests;
#[async_trait]
impl Provider for NoRequests {
    async fn complete(
        &self,
        _: &[crate::message::Message],
        _: &[crate::message::ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        anyhow::bail!("disconnect tests must not request a provider")
    }
    fn name(&self) -> &str {
        "mock"
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
}

struct Home {
    _dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}
impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", dir.path());
        Self {
            _dir: dir,
            previous,
        }
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

struct Fixture {
    id: String,
    agent: Arc<Mutex<Agent>>,
    sessions: SessionAgents,
    members: Arc<RwLock<HashMap<String, SwarmMember>>>,
    connections: Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    swarms: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    coordinators: Arc<RwLock<HashMap<String, String>>>,
    history: Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    events: mpsc::UnboundedSender<ServerEvent>,
}
impl Fixture {
    async fn new(persisted: bool) -> Self {
        let provider: Arc<dyn Provider> = Arc::new(NoRequests);
        let registry = Registry::new(provider.clone()).await;
        let mut session = Session::create(None, None);
        if persisted {
            session.title = Some("Explicit saved panel".into());
            session.save().unwrap();
        }
        let id = session.id.clone();
        let mut agent = Agent::new_with_session(provider, registry, session, None);
        agent.set_memory_enabled(false);
        let agent = Arc::new(Mutex::new(agent));
        let (events, _) = mpsc::unbounded_channel();
        let members = Arc::new(RwLock::new(HashMap::from([(
            id.clone(),
            SwarmMember {
                session_id: id.clone(),
                event_tx: events.clone(),
                event_txs: HashMap::from([("original".into(), events.clone())]),
                working_dir: None,
                swarm_id: None,
                swarm_enabled: false,
                status: "ready".into(),
                detail: None,
                task_label: None,
                friendly_name: None,
                report_back_to_session_id: None,
                latest_completion_report: None,
                role: "agent".into(),
                joined_at: Instant::now(),
                last_status_change: Instant::now(),
                is_headless: false,
                output_tail: None,
                todo_progress: None,
                todo_items: Vec::new(),
                runtime: Default::default(),
            },
        )])));
        let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
            HashMap::from([(id.clone(), agent.clone())]),
        ));
        let connections = Arc::new(RwLock::new(HashMap::from([(
            "original".into(),
            connection("original", &id),
        )])));
        Self {
            id,
            agent,
            sessions,
            members,
            connections,
            swarms: Arc::new(RwLock::new(HashMap::new())),
            coordinators: Arc::new(RwLock::new(HashMap::new())),
            history: Arc::new(RwLock::new(std::collections::VecDeque::new())),
            events,
        }
    }

    async fn cleanup(&self, processing: bool, grace: Duration) {
        assert!(
            self.cleanup_task(processing, grace, &mut None)
                .await
                .is_none()
        );
    }

    async fn cleanup_task(
        &self,
        processing: bool,
        grace: Duration,
        task: &mut Option<tokio::task::JoinHandle<()>>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        self.cleanup_with_continuation(processing, false, grace, task)
            .await
    }

    async fn cleanup_with_continuation(
        &self,
        processing: bool,
        continue_on_disconnect: bool,
        grace: Duration,
        task: &mut Option<tokio::task::JoinHandle<()>>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let (swarm_events, _) = broadcast::channel(8);
        cleanup_client_connection(
            &self.sessions,
            &self.id,
            processing,
            continue_on_disconnect,
            task,
            tokio::spawn(std::future::pending()),
            &self.members,
            &self.swarms,
            &self.coordinators,
            &Arc::new(RwLock::new(HashMap::new())),
            &FileTouchService::new(),
            &Arc::new(RwLock::new(HashMap::new())),
            &Arc::new(RwLock::new(HashMap::new())),
            &Arc::new(RwLock::new(ClientDebugState::default())),
            "debug-original",
            &self.connections,
            "original",
            &Arc::new(RwLock::new(HashMap::new())),
            &Arc::new(RwLock::new(HashMap::new())),
            &self.history,
            &Arc::new(std::sync::atomic::AtomicU64::new(0)),
            &swarm_events,
            &self.events,
            grace,
        )
        .await
        .unwrap()
    }

    /// The same cleanup call as [`Self::cleanup_task`], as a spawnable future
    /// carrying the test-only pause, so the test body can claim while cleanup
    /// is parked in the check/removal gap.
    fn cleanup_future_with_race(
        &self,
        processing: bool,
        grace: Duration,
        pause: crate::server::client_disconnect_grace_test_pause::Pause,
    ) -> impl std::future::Future<Output = ()> + use<> {
        crate::server::client_disconnect_grace_test_pause::register(&self.id, pause);
        let sessions = Arc::clone(&self.sessions);
        let id = self.id.clone();
        let members = Arc::clone(&self.members);
        let connections = Arc::clone(&self.connections);
        let events = self.events.clone();
        async move {
            let (swarm_events, _) = broadcast::channel(8);
            let _ = cleanup_client_connection(
                &sessions,
                &id,
                processing,
                false,
                &mut None,
                tokio::spawn(std::future::pending()),
                &members,
                &Arc::new(RwLock::new(HashMap::new())),
                &Arc::new(RwLock::new(HashMap::new())),
                &Arc::new(RwLock::new(HashMap::new())),
                &FileTouchService::new(),
                &Arc::new(RwLock::new(HashMap::new())),
                &Arc::new(RwLock::new(HashMap::new())),
                &Arc::new(RwLock::new(ClientDebugState::default())),
                "debug-original",
                &connections,
                "original",
                &Arc::new(RwLock::new(HashMap::new())),
                &Arc::new(RwLock::new(HashMap::new())),
                &Arc::new(RwLock::new(std::collections::VecDeque::new())),
                &Arc::new(std::sync::atomic::AtomicU64::new(0)),
                &swarm_events,
                &events,
                grace,
            )
            .await;
        }
    }

    /// The same cleanup call as [`Self::cleanup_task`], as a spawnable future
    /// without the test-only pause.
    fn cleanup_future(
        &self,
        processing: bool,
        grace: Duration,
    ) -> impl std::future::Future<Output = ()> + use<> {
        let sessions = Arc::clone(&self.sessions);
        let id = self.id.clone();
        let members = Arc::clone(&self.members);
        let connections = Arc::clone(&self.connections);
        let events = self.events.clone();
        async move {
            let (swarm_events, _) = broadcast::channel(8);
            let _ = cleanup_client_connection(
                &sessions,
                &id,
                processing,
                false,
                &mut None,
                tokio::spawn(std::future::pending()),
                &members,
                &Arc::new(RwLock::new(HashMap::new())),
                &Arc::new(RwLock::new(HashMap::new())),
                &Arc::new(RwLock::new(HashMap::new())),
                &FileTouchService::new(),
                &Arc::new(RwLock::new(HashMap::new())),
                &Arc::new(RwLock::new(HashMap::new())),
                &Arc::new(RwLock::new(ClientDebugState::default())),
                "debug-original",
                &connections,
                "original",
                &Arc::new(RwLock::new(HashMap::new())),
                &Arc::new(RwLock::new(HashMap::new())),
                &Arc::new(RwLock::new(std::collections::VecDeque::new())),
                &Arc::new(std::sync::atomic::AtomicU64::new(0)),
                &swarm_events,
                &events,
                grace,
            )
            .await;
        }
    }

    async fn wait_for_detach(&self) {
        timeout(Duration::from_secs(1), async {
            while self.connections.read().await.contains_key("original") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup releases attachment registry promptly");
    }

    async fn attach_successor(&self) {
        // Reserve the same live Agent under the same registry lock order used
        // by claim_live_target_agent, then register the real event attachment.
        let mut connections = self.connections.write().await;
        assert!(Arc::ptr_eq(
            self.sessions.read().await.get(&self.id).unwrap(),
            &self.agent
        ));
        connections.insert("successor".into(), connection("successor", &self.id));
        drop(connections);
        let (sender, _) = mpsc::unbounded_channel();
        crate::server::register_session_event_sender(&self.members, &self.id, "successor", sender)
            .await;
    }
}

fn connection(name: &str, id: &str) -> ClientConnectionInfo {
    let (disconnect_tx, _) = mpsc::unbounded_channel();
    ClientConnectionInfo {
        client_id: name.into(),
        session_id: id.into(),
        client_instance_id: None,
        debug_client_id: None,
        connected_at: Instant::now(),
        last_seen: Instant::now(),
        is_processing: false,
        current_tool_name: None,
        terminal_env: vec![],
        disconnect_tx,
    }
}

fn build_test_agent_with_id(session_id: &str) -> Agent {
    let provider: Arc<dyn Provider> = Arc::new(NoRequests);
    let registry = Registry::empty();
    let mut session = crate::session::Session::create_with_id(session_id.to_string(), None, None);
    session.model = Some("mock".to_string());
    Agent::new_with_session(provider, registry, session, None)
}

/// The successor's claim holds this per-session reservation from target
/// selection through registry publication, and publishes its registry record
/// while holding it.
#[tokio::test]
async fn successor_claim_in_flight_blocks_destructive_cleanup() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(true).await;
    let pause = crate::server::client_disconnect_grace_test_pause::Pause::new();

    let claim_gate = fixture.sessions.reserve(&fixture.id).await;
    let cleanup = tokio::spawn(fixture.cleanup_future_with_race(true, Duration::ZERO, pause.clone()));

    // Cleanup has answered its first ownership question (no successor) and is
    // about to take the claim gate. A live resume holds that gate from target
    // selection through registry publication, so the claim is placed now
    // through the real claim path, exactly as the second-window test does.
    pause.reached().await;
    fixture
        .connections
        .write()
        .await
        .insert("successor".into(), connection("successor", &fixture.id));
    let claimed = crate::server::client_session::claim_live_target_agent_for_test(
        &fixture.id,
        "successor",
        Some("instance-successor"),
        &fixture.agent,
        &fixture.sessions,
        &fixture.connections,
    )
    .await
    .expect("live target must be claimable");
    assert!(Arc::ptr_eq(&claimed, &fixture.agent));
    let (sender, _) = mpsc::unbounded_channel();
    crate::server::register_session_event_sender(
        &fixture.members,
        &fixture.id,
        "successor",
        sender,
    )
    .await;

    // Release the claim gate and let cleanup take it. Its second ownership check
    // under the gate must see the successor and skip destructive teardown.
    drop(claim_gate);
    pause.release();
    tokio::time::timeout(Duration::from_secs(5), cleanup)
        .await
        .expect("cleanup must finish")
        .expect("cleanup task panicked");

    assert!(
        fixture.sessions.read().await.contains_key(&fixture.id),
        "cleanup must not remove a session a successor claimed"
    );
    // The companion assertion proves the survivor is the same live owner, not a
    // rebuilt entry.
    assert!(Arc::ptr_eq(
        fixture.sessions.read().await.get(&fixture.id).unwrap(),
        &fixture.agent
    ));
    assert!(fixture.connections.read().await.contains_key("successor"));
    assert!(fixture.members.read().await.contains_key(&fixture.id));
    crate::server::client_disconnect_grace_test_pause::forget(&fixture.id);
}

/// Production-shaped variant of the window-1 claim test: by the time cleanup
/// answers its ownership question, the disconnecting connection's registry
/// record is already gone, because `record_transport_closed` runs before the
/// window in production and inside cleanup. The grace fixture instead keeps
/// `original` present, which is the state production never reaches. This variant
/// removes it before parking, so the re-check is measured against the production
/// registry shape.
#[tokio::test]
async fn successor_claim_in_window_survives_production_registry_shape() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(true).await;

    // The disconnecting connection's record is gone before cleanup starts, as in
    // production.
    assert!(fixture.connections.write().await.remove("original").is_some());

    let pause = crate::server::client_disconnect_grace_test_pause::Pause::new();
    let claim_gate = fixture.sessions.reserve(&fixture.id).await;
    let cleanup = tokio::spawn(fixture.cleanup_future_with_race(true, Duration::ZERO, pause.clone()));

    pause.reached().await;
    fixture
        .connections
        .write()
        .await
        .insert("successor".into(), connection("successor", &fixture.id));
    let claimed = crate::server::client_session::claim_live_target_agent_for_test(
        &fixture.id,
        "successor",
        Some("instance-successor"),
        &fixture.agent,
        &fixture.sessions,
        &fixture.connections,
    )
    .await
    .expect("live target must be claimable");
    assert!(Arc::ptr_eq(&claimed, &fixture.agent));

    drop(claim_gate);
    pause.release();
    tokio::time::timeout(Duration::from_secs(5), cleanup)
        .await
        .expect("cleanup must finish")
        .expect("cleanup task panicked");

    assert!(
        fixture.sessions.read().await.contains_key(&fixture.id),
        "with the production registry shape, a claimed session must still survive"
    );
    assert!(Arc::ptr_eq(
        fixture.sessions.read().await.get(&fixture.id).unwrap(),
        &fixture.agent
    ));
    crate::server::client_disconnect_grace_test_pause::forget(&fixture.id);
}

/// Negative control: with no claim in flight, cleanup still performs its
/// destructive removal. Without this, "cleanup never removes" would satisfy
/// the test above for the wrong reason.
#[tokio::test]
async fn cleanup_removes_session_when_no_successor_claim_is_in_flight() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(true).await;
    let pause = crate::server::client_disconnect_grace_test_pause::Pause::new();
    let cleanup = tokio::spawn(fixture.cleanup_future_with_race(true, Duration::ZERO, pause.clone()));

    pause.reached().await;
    pause.release();
    tokio::time::timeout(Duration::from_secs(5), cleanup)
        .await
        .expect("cleanup must finish")
        .expect("cleanup task panicked");

    assert!(pause.is_finished(), "cleanup ran its teardown");
    assert!(!fixture.sessions.read().await.contains_key(&fixture.id));
    assert!(fixture.connections.read().await.is_empty());
    crate::server::client_disconnect_grace_test_pause::forget(&fixture.id);
}

/// Control of the control: every protection test injects its successor through
/// the pause registry. Prove the pause is not what protects the session, by
/// parking cleanup through the very same pause and leaving it there: cleanup must
/// still complete its destructive teardown. If the pause itself were the
/// protection, this test would keep the session and the claim tests would be
/// passing for the wrong reason.
#[tokio::test]
async fn parked_cleanup_removes_the_session_once_released_without_any_claim() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(true).await;
    let pause = crate::server::client_disconnect_grace_test_pause::Pause::new();
    let cleanup = tokio::spawn(fixture.cleanup_future_with_race(true, Duration::ZERO, pause.clone()));

    pause.reached().await;
    pause.release();
    tokio::time::timeout(Duration::from_secs(5), cleanup)
        .await
        .expect("cleanup must finish")
        .expect("cleanup task panicked");

    assert!(
        !fixture.sessions.read().await.contains_key(&fixture.id),
        "a bare pause must not protect the session; only a claim does"
    );
    crate::server::client_disconnect_grace_test_pause::forget(&fixture.id);
}

/// The negative control must prove the claim really happened before cleanup's
/// re-check, not merely that teardown ran. A claim writes its connection record
/// under the same reservation cleanup takes, so a registry write issued while
/// cleanup holds that reservation and after its re-check must still be honoured:
/// the session is already owned again and teardown must not have taken it.
#[tokio::test]
async fn parked_cleanup_finishes_once_the_claim_gate_is_released() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(true).await;
    let pause = crate::server::client_disconnect_grace_test_pause::Pause::new();

    let claim_gate = fixture.sessions.reserve(&fixture.id).await;
    let cleanup = tokio::spawn(fixture.cleanup_future_with_race(true, Duration::ZERO, pause.clone()));

    pause.reached().await;
    // Cleanup has answered its first check and is waiting on the reservation.
    // Its second check has not run yet, so the session must still be intact.
    assert!(
        fixture.sessions.read().await.contains_key(&fixture.id),
        "cleanup must not remove the session before its re-check"
    );
    drop(claim_gate);
    pause.release();
    tokio::time::timeout(Duration::from_secs(5), cleanup)
        .await
        .expect("cleanup must finish once the claim gate is free")
        .expect("cleanup task panicked");
    assert!(pause.is_finished(), "cleanup ran its teardown");
    crate::server::client_disconnect_grace_test_pause::forget(&fixture.id);
}

#[tokio::test]
async fn unrelated_session_claim_is_not_blocked_by_cleanup_teardown() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(true).await;
    let other_id = "session_unrelated".to_string();
    let other_agent = Arc::new(Mutex::new(build_test_agent_with_id(&other_id)));
    fixture
        .sessions
        .register_new(other_id.clone(), Arc::clone(&other_agent))
        .await
        .expect("register unrelated session");
    fixture.connections.write().await.insert(
        "unrelated".into(),
        connection("unrelated", &other_id),
    );

    let pause = crate::server::client_disconnect_grace_test_pause::Pause::new();
    let cleanup = tokio::spawn(fixture.cleanup_future_with_race(true, Duration::ZERO, pause.clone()));

    // Cleanup is parked before it takes its own session's claim gate. An
    // unrelated session's claim must complete here, through the same
    // `claim_live_target_agent` path a resume uses: per-session ownership, not a
    // global teardown guard.
    pause.reached().await;
    let claimed = tokio::time::timeout(
        Duration::from_secs(2),
        crate::server::client_session::claim_live_target_agent_for_test(
            &other_id,
            "unrelated",
            None,
            &other_agent,
            &fixture.sessions,
            &fixture.connections,
        ),
    )
    .await
    .expect("unrelated session claim must not block on this connection's teardown")
    .expect("unrelated session exists");
    assert!(Arc::ptr_eq(&claimed, &other_agent));
    assert_eq!(
        fixture
            .connections
            .read()
            .await
            .get("unrelated")
            .expect("unrelated connection")
            .session_id,
        other_id
    );

    pause.release();
    tokio::time::timeout(Duration::from_secs(5), cleanup)
        .await
        .expect("cleanup must finish")
        .expect("cleanup task panicked");
    assert!(fixture.sessions.read().await.contains_key(&other_id));
    assert!(!fixture.sessions.read().await.contains_key(&fixture.id));
    crate::server::client_disconnect_grace_test_pause::forget(&fixture.id);
}

#[tokio::test]
async fn teardown_does_not_hold_the_registry_guard_while_waiting_for_the_agent() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(false).await;

    // Holding the agent mutex forces the reconnect-grace eligibility check to
    // fail, so cleanup goes straight to teardown and then parks on this lock.
    let agent_guard = fixture.agent.lock().await;
    let cleanup = tokio::spawn(fixture.cleanup_future(false, Duration::ZERO));

    timeout(Duration::from_secs(5), async {
        while fixture.sessions.read().await.contains_key(&fixture.id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cleanup must remove the session entry");

    // Teardown is now waiting for the agent mutex. The registry guard must
    // already be free for another connection.
    let probe = tokio::spawn({
        let connections = Arc::clone(&fixture.connections);
        async move { connections.write().await.contains_key("original") }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        probe.is_finished(),
        "teardown must not hold the connection registry guard while waiting for the agent lock"
    );
    probe.await.expect("registry probe must not panic");

    drop(agent_guard);
    timeout(Duration::from_secs(5), cleanup)
        .await
        .expect("cleanup must finish")
        .expect("cleanup task panicked");
}

#[tokio::test]
async fn continued_idle_coordinator_survives_disconnect_without_reconnect_grace() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    for persisted in [false, true] {
        let fixture = Fixture::new(persisted).await;
        fixture
            .swarms
            .write()
            .await
            .insert("remote-swarm".into(), HashSet::from([fixture.id.clone()]));
        fixture
            .coordinators
            .write()
            .await
            .insert("remote-swarm".into(), fixture.id.clone());
        {
            let mut members = fixture.members.write().await;
            let member = members.get_mut(&fixture.id).unwrap();
            member.role = "coordinator".into();
            member.swarm_id = Some("remote-swarm".into());
            member.swarm_enabled = true;
        }
        assert!(
            timeout(
                Duration::from_secs(1),
                fixture.cleanup_with_continuation(false, true, Duration::from_secs(30), &mut None),
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(fixture.connections.read().await.is_empty());
        assert!(
            fixture.history.read().await.is_empty(),
            "no stopped/member-left event"
        );
        assert!(fixture.swarms.read().await["remote-swarm"].contains(&fixture.id));
        assert_eq!(
            fixture.coordinators.read().await["remote-swarm"],
            fixture.id
        );
        assert!(Arc::ptr_eq(
            fixture.sessions.read().await.get(&fixture.id).unwrap(),
            &fixture.agent
        ));
        let members = fixture.members.read().await;
        let member = members
            .get(&fixture.id)
            .expect("worker reports still resolve coordinator");
        assert_eq!(member.status, "ready");
        assert_eq!(member.swarm_id.as_deref(), Some("remote-swarm"));
        assert!(member.event_txs.is_empty(), "transport sender was detached");
        drop(members);
        fixture.attach_successor().await;
    }
}

#[tokio::test]
async fn continued_turn_remains_live_after_owner_finishes() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(true).await;
    let (finish, wait) = tokio::sync::oneshot::channel();
    let mut task = Some(tokio::spawn(async move { wait.await.unwrap() }));
    let writer = fixture
        .cleanup_with_continuation(true, true, Duration::ZERO, &mut task)
        .await
        .expect("lifecycle retains completion ownership");
    finish.send(()).unwrap();
    task.take()
        .unwrap()
        .await
        .expect("disconnect must not cancel turn");
    writer.abort();
    // Mirrors the lifecycle's final cleanup after recording completion. The
    // continuation policy must still apply when there is no processing task.
    assert!(
        fixture
            .cleanup_with_continuation(false, true, Duration::ZERO, &mut task)
            .await
            .is_none()
    );
    assert!(fixture.sessions.read().await.contains_key(&fixture.id));
    assert_eq!(fixture.members.read().await[&fixture.id].status, "ready");
    assert!(!matches!(
        fixture.agent.lock().await.session_for_split().status,
        SessionStatus::Closed | SessionStatus::Crashed { .. }
    ));
}

#[tokio::test]
async fn unsaved_idle_session_retains_same_agent_for_reattachment() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(false).await;
    let ((), ()) = tokio::join!(fixture.cleanup(false, Duration::from_secs(2)), async {
        fixture.wait_for_detach().await;
        assert!(!crate::session::session_exists(&fixture.id));
        // These locks must remain available during the reconnect grace.
        let _agent = fixture
            .agent
            .try_lock()
            .expect("grace cannot hold agent lock");
        drop(_agent);
        fixture.attach_successor().await;
    });
    assert!(Arc::ptr_eq(
        fixture.sessions.read().await.get(&fixture.id).unwrap(),
        &fixture.agent
    ));
    assert!(fixture.connections.read().await.contains_key("successor"));
    assert!(fixture.members.read().await.contains_key(&fixture.id));
    assert!(!crate::session::session_exists(&fixture.id));
}

#[tokio::test]
async fn unsaved_idle_session_expires_without_persisting_or_leaking() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(false).await;
    let grace = Duration::from_millis(60);
    let start = Instant::now();
    timeout(Duration::from_secs(1), fixture.cleanup(false, grace))
        .await
        .unwrap();
    assert!(start.elapsed() >= grace);
    assert!(!fixture.sessions.read().await.contains_key(&fixture.id));
    assert!(!fixture.members.read().await.contains_key(&fixture.id));
    assert!(fixture.connections.read().await.is_empty());
    assert!(!crate::session::session_exists(&fixture.id));
}

#[tokio::test]
async fn unsaved_idle_session_without_swarm_member_still_expires() {
    // A client that disconnects before subscribe registers a swarm member
    // (window closed mid-subscribe) must not be mistaken for a replaced
    // attachment. Previously this leaked the Agent for the server lifetime.
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(false).await;
    fixture.members.write().await.clear();
    timeout(
        Duration::from_secs(1),
        fixture.cleanup(false, Duration::from_millis(60)),
    )
    .await
    .unwrap();
    assert!(!fixture.sessions.read().await.contains_key(&fixture.id));
    assert!(fixture.connections.read().await.is_empty());
}

#[tokio::test]
async fn old_grace_cannot_remove_successor_that_already_detached_again() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(false).await;
    tokio::join!(fixture.cleanup(false, Duration::from_millis(100)), async {
        fixture.wait_for_detach().await;
        fixture.attach_successor().await;
        detach_client_attachment(
            &fixture.id,
            "successor",
            "successor-debug",
            &fixture.connections,
            &Arc::new(RwLock::new(ClientDebugState::default())),
            &fixture.members,
        )
        .await;
    });
    assert!(fixture.connections.read().await.is_empty());
    assert!(
        fixture.sessions.read().await.contains_key(&fixture.id),
        "successor owns its own grace/cleanup"
    );
    assert!(fixture.members.read().await.contains_key(&fixture.id));
}

#[tokio::test]
async fn persisted_idle_session_does_not_wait_for_reconnect_grace() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(true).await;
    timeout(
        Duration::from_secs(1),
        fixture.cleanup(false, Duration::from_secs(30)),
    )
    .await
    .unwrap();
    assert!(fixture.sessions.read().await.is_empty());
    assert!(crate::session::session_exists(&fixture.id));
}

#[tokio::test]
async fn interrupted_session_does_not_wait_for_reconnect_grace() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    crate::server::clear_reload_marker();
    let fixture = Fixture::new(false).await;
    timeout(
        Duration::from_secs(1),
        fixture.cleanup(true, Duration::from_secs(30)),
    )
    .await
    .unwrap();
    assert!(fixture.sessions.read().await.is_empty());
    assert!(matches!(
        fixture.agent.lock().await.session_for_split().status,
        SessionStatus::Crashed { .. }
    ));
}

#[tokio::test]
async fn busy_successor_retains_owner_task_and_completion_receiver() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let fixture = Fixture::new(true).await;
    fixture.attach_successor().await;
    let (finish, wait) = tokio::sync::oneshot::channel();
    let (done, mut completions) = mpsc::unbounded_channel();
    let mut task = Some(tokio::spawn(async move {
        wait.await.unwrap();
        done.send("completed").unwrap();
    }));
    let writer = fixture
        .cleanup_task(true, Duration::ZERO, &mut task)
        .await
        .expect("successor returns ownership to lifecycle continuation");
    assert!(!task.as_ref().unwrap().is_finished());
    assert!(fixture.sessions.read().await.contains_key(&fixture.id));
    assert!(fixture.connections.read().await.contains_key("successor"));
    assert!(!fixture.connections.read().await.contains_key("original"));
    writer.abort();
    finish.send(()).unwrap();
    task.take().unwrap().await.expect("owner was not cancelled");
    assert_eq!(completions.recv().await, Some("completed"));
    fixture.cleanup(false, Duration::ZERO).await;
    assert!(fixture.sessions.read().await.contains_key(&fixture.id));
}

#[tokio::test]
async fn busy_without_successor_aborts_owner_and_marks_crashed() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    crate::server::clear_reload_marker();
    let fixture = Fixture::new(true).await;
    let (dropped, observed_drop) = tokio::sync::oneshot::channel::<()>();
    let mut task = Some(tokio::spawn(async move {
        let _guard = dropped;
        std::future::pending::<()>().await;
    }));
    assert!(
        fixture
            .cleanup_task(true, Duration::ZERO, &mut task)
            .await
            .is_none()
    );
    assert!(task.is_none());
    assert!(
        timeout(Duration::from_secs(1), observed_drop)
            .await
            .unwrap()
            .is_err()
    );
    assert!(fixture.sessions.read().await.is_empty());
    assert!(matches!(
        fixture.agent.lock().await.session_for_split().status,
        SessionStatus::Crashed { .. }
    ));
}
