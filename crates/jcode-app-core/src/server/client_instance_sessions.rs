// reference: https://socket.io/docs/v4/connection-state-recovery
//! The server's durable mapping from a client instance to its session.
//!
//! MEASURED 2026-11-17: a reconnecting client whose `SessionId` frame was
//! never dispatched re-subscribed with `target_session_id: None`. The server
//! then minted a brand-new session per reconnect (~20/s during the storm), and
//! every new session registers its own MCP child set
//! (`client_session.rs` -> `register_mcp_tools_for_dir`), which is what turned
//! a dropped connection into hundreds of `jcode.exe` processes.
//!
//! The client instance id is stable across reconnects of the same client
//! (`RemoteConnection::client_instance_id`), so the server can resolve a
//! missing target to the session that client already owns.
//!
//! The pattern follows Socket.IO's connection state recovery: the SERVER keeps
//! the per-client session binding and restores it on reconnect, instead of
//! letting the client mint a fresh session. Socket.IO documents that recovery
//! is best-effort ("it will not always be successful"), so this mapping is a
//! shortcut for a live daemon, not a second source of truth about sessions
//! (that stays on disk).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

/// `client_instance_id` -> the session id that instance is bound to.
pub(super) type ClientInstanceSessions = Arc<RwLock<HashMap<String, String>>>;

pub(super) fn new_client_instance_sessions() -> ClientInstanceSessions {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Bind a client instance to the session it is now using.
///
/// A client instance owns exactly one session at a time: `Clear` replaces the
/// session in place, so the mapping is overwritten rather than accumulated.
pub(super) async fn bind_client_instance(
    map: &ClientInstanceSessions,
    client_instance_id: Option<&str>,
    session_id: &str,
) {
    let Some(client_instance_id) = client_instance_id else {
        return;
    };
    map.write()
        .await
        .insert(client_instance_id.to_string(), session_id.to_string());
}

/// The session this client instance is bound to, if any.
///
/// The caller decides whether that session is still usable: a session whose
/// runtime was reclaimed must not be resurrected by a reconnect, because the
/// client would then attach to a transcript that no longer matches its own
/// view. When the caller rejects the session it calls
/// [`forget_client_instance`] so the stale binding does not linger.
pub(super) async fn lookup_client_instance_session(
    map: &ClientInstanceSessions,
    client_instance_id: Option<&str>,
) -> Option<String> {
    let client_instance_id = client_instance_id?;
    map.read().await.get(client_instance_id).cloned()
}

/// Forget a binding. Used when a client's session is destroyed for good.
pub(super) async fn forget_client_instance(
    map: &ClientInstanceSessions,
    client_instance_id: Option<&str>,
    session_id: &str,
) {
    let Some(client_instance_id) = client_instance_id else {
        return;
    };
    let mut map = map.write().await;
    if map
        .get(client_instance_id)
        .is_some_and(|bound| bound == session_id)
    {
        map.remove(client_instance_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lookup_returns_the_bound_session() {
        let map = new_client_instance_sessions();
        bind_client_instance(&map, Some("client-1"), "session-a").await;
        assert_eq!(
            lookup_client_instance_session(&map, Some("client-1")).await,
            Some("session-a".to_string())
        );
    }

    #[tokio::test]
    async fn rebinding_replaces_the_session() {
        let map = new_client_instance_sessions();
        bind_client_instance(&map, Some("client-1"), "session-a").await;
        bind_client_instance(&map, Some("client-1"), "session-b").await;
        assert_eq!(
            lookup_client_instance_session(&map, Some("client-1")).await,
            Some("session-b".to_string())
        );
    }

    #[tokio::test]
    async fn missing_instance_resolves_to_nothing() {
        let map = new_client_instance_sessions();
        assert_eq!(
            lookup_client_instance_session(&map, None).await,
            None
        );
        assert_eq!(
            lookup_client_instance_session(&map, Some("unknown")).await,
            None
        );
    }

    #[tokio::test]
    async fn forget_only_drops_the_matching_session() {
        let map = new_client_instance_sessions();
        bind_client_instance(&map, Some("client-1"), "session-a").await;
        forget_client_instance(&map, Some("client-1"), "session-other").await;
        assert_eq!(
            lookup_client_instance_session(&map, Some("client-1")).await,
            Some("session-a".to_string())
        );
        forget_client_instance(&map, Some("client-1"), "session-a").await;
        assert_eq!(
            lookup_client_instance_session(&map, Some("client-1")).await,
            None
        );
    }

    #[test]
    fn map_type_is_shared() {
        let map: ClientInstanceSessions = Arc::new(RwLock::new(HashMap::new()));
        let clone = Arc::clone(&map);
        assert!(Arc::ptr_eq(&map, &clone));
    }
}
