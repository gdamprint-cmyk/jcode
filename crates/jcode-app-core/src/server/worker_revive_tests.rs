// reference: no prior art, searched upstream jcode repository (github.com/1jehuang/jcode,
// crates/jcode-app-core/src/server/worker_revive.rs -> 404) and web search for
// "revive preserve persisted session flag" / tokio test isolation with JCODE_HOME;
// none found. The behaviour under test is this repository's own: a headless worker is
// created with is_debug = true (server::headless::create_headless_session) and the
// session picker hides debug sessions
// (crates/jcode-tui/src/tui/session_picker/filter.rs). Test conventions follow
// crates/jcode-app-core/src/server/client_target_attach_tests.rs.
#![allow(clippy::await_holding_lock)]

use super::*;
use crate::message::{Message, ToolDefinition};
use crate::provider::EventStream;
use crate::tool::Registry;
use async_trait::async_trait;

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
        anyhow::bail!("reviving a worker must not invoke a provider")
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(NoRequests)
    }
}

struct Home {
    _temp: tempfile::TempDir,
    old: Option<std::ffi::OsString>,
}

impl Home {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let old = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());
        Self { _temp: temp, old }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        match self.old.take() {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

async fn revived_agent_for(session: crate::session::Session) -> Agent {
    let provider: Arc<dyn Provider> = Arc::new(NoRequests);
    let registry = Registry::new(provider.clone()).await;
    build_revived_agent(provider, registry, session)
}

#[tokio::test]
async fn revive_keeps_a_user_session_visible_in_the_session_picker() {
    // Regression: reviving a worker used to force `is_debug = true`. A user
    // session whose TUI client died is registered as a headless swarm member,
    // so the revive path reclassified it as a test session and it silently
    // vanished from the session list.
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let mut session = crate::session::Session::create(None, Some("user session".into()));
    session.is_debug = false;

    let agent = revived_agent_for(session).await;

    assert!(
        !agent.is_debug(),
        "revive must not reclassify a user session as a debug session"
    );
}

#[tokio::test]
async fn revive_keeps_a_headless_worker_hidden_from_the_session_picker() {
    // A real headless worker is created with `is_debug = true`
    // (server::headless::create_headless_session). The revive path must keep it
    // hidden instead of promoting it into the user's session list.
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let mut session = crate::session::Session::create(None, Some("headless worker".into()));
    session.is_debug = true;

    let agent = revived_agent_for(session).await;

    assert!(
        agent.is_debug(),
        "revive must keep a headless worker classified as a debug session"
    );
}
