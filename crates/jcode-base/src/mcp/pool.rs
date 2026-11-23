//! Shared MCP Server Pool
//!
//! Manages a global pool of MCP server processes that are shared across
//! all jcode sessions. Instead of each session spawning its own set of
//! MCP servers (N sessions × M servers = N×M processes), sessions share
//! a single pool (M processes total).
//!
//! Sessions get lightweight `McpHandle` clones that can send concurrent
//! requests to shared server processes. Request/response correlation by
//! ID ensures no interference between sessions.

// reference: https://docs.claude.com/en/docs/claude-code/mcp (read 2026-11-11) - Claude Code scopes an MCP server to `local`, `project` or `user`, and a project-scoped server is resolved per project directory rather than per daemon. Jcode's `per_project` flag mirrors that project scope.

use super::client::{McpClient, McpHandle};
use super::protocol::{McpConfig, McpServerConfig, McpToolDef};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, RwLock};

const FAILED_CONNECT_RETRY_COOLDOWN: Duration = Duration::from_secs(30);
/// Repeated failures (e.g. `mcp-remote` stuck waiting for OAuth) back off
/// exponentially up to this cap instead of respawning `npx` every 30s.
const MAX_FAILED_CONNECT_RETRY_COOLDOWN: Duration = Duration::from_secs(30 * 60);

#[derive(Clone)]
struct FailedConnectRecord {
    message: String,
    failed_at: Instant,
    failures: u32,
}

impl FailedConnectRecord {
    fn cooldown(&self) -> Duration {
        failed_connect_cooldown(self.failures)
    }
}

fn failed_connect_cooldown(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(10);
    FAILED_CONNECT_RETRY_COOLDOWN
        .saturating_mul(1 << shift)
        .min(MAX_FAILED_CONNECT_RETRY_COOLDOWN)
}

enum ConnectAttempt {
    Connected,
    Leader(Arc<Notify>),
    Wait(Arc<Notify>),
}

/// Global shared pool of MCP server processes.
///
/// Only one pool exists per jcode daemon. It owns the child processes
/// and hands out cheap `McpHandle` clones to sessions.
pub struct SharedMcpPool {
    clients: Mutex<HashMap<PoolKey, McpClient>>,
    handles: RwLock<HashMap<PoolKey, McpHandle>>,
    config: RwLock<McpConfig>,
    /// Directory against which the pool's default config was resolved. Keep it
    /// stable across reloads because the daemon may serve sessions in many dirs.
    config_dir: Option<std::path::PathBuf>,
    ref_counts: Mutex<HashMap<PoolKey, usize>>,
    connecting: Mutex<HashMap<PoolKey, Arc<Notify>>>,
    last_errors: RwLock<HashMap<PoolKey, FailedConnectRecord>>,
}

/// Identifies one pooled server process.
///
/// A plain server name addresses the single daemon-wide process. A server
/// marked `per_project` gets one process per project directory, so the key
/// carries that directory and the process can be started with it as its
/// working directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PoolKey {
    name: String,
    /// `None` for a daemon-wide server, `Some(dir)` for a per-project one.
    project_dir: Option<std::path::PathBuf>,
}

impl PoolKey {
    fn global(name: &str) -> Self {
        Self {
            name: name.to_string(),
            project_dir: None,
        }
    }

    /// The key a server resolves to for a given session directory.
    fn for_dir(name: &str, config: &McpServerConfig, project_dir: Option<&std::path::Path>) -> Self {
        if config.per_project {
            Self {
                name: name.to_string(),
                project_dir: project_dir.map(std::path::Path::to_path_buf),
            }
        } else {
            Self::global(name)
        }
    }

    /// Whether this key addresses the process a session in `project_dir` may use.
    ///
    /// A daemon-wide server has one process that every session shares. A
    /// per-project server only matches the session that runs in the very same
    /// directory, so a session never receives another project's process.
    fn matches_dir(&self, project_dir: Option<&std::path::Path>) -> bool {
        match (&self.project_dir, project_dir) {
            (None, _) => true,
            (Some(mine), Some(theirs)) => mine == theirs,
            (Some(_), None) => false,
        }
    }
}

impl SharedMcpPool {
    /// Create a new shared pool with the given config
    pub fn new(config: McpConfig) -> Self {
        Self::new_for_dir(config, std::env::current_dir().ok())
    }

    fn new_for_dir(config: McpConfig, config_dir: Option<std::path::PathBuf>) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            handles: RwLock::new(HashMap::new()),
            config: RwLock::new(config),
            config_dir,
            ref_counts: Mutex::new(HashMap::new()),
            connecting: Mutex::new(HashMap::new()),
            last_errors: RwLock::new(HashMap::new()),
        }
    }

    /// Create pool loading config from default locations
    pub fn from_default_config() -> Self {
        let config_dir = std::env::current_dir().ok();
        let config = McpConfig::load_for_dir(config_dir.as_deref());
        Self::new_for_dir(config, config_dir)
    }

    /// Connect to all configured servers.
    /// Returns (successes, failures).
    pub async fn connect_all(&self) -> (usize, Vec<(String, String)>) {
        let config = self.config.read().await;
        let mut connect_futures = Vec::new();

        for (name, server_config) in &config.servers {
            // Disabled servers stay configured but are never auto-spawned
            // (issue #436); they can still be connected on demand by name.
            if !server_config.is_enabled() {
                continue;
            }
            // A per-project server needs a project directory to run in, so the
            // daemon-wide pool cannot start it: the session that owns the
            // project connects it on demand (see `connect_server`).
            if server_config.per_project {
                continue;
            }
            // Non-shared servers are owned per-session (with the session cwd)
            // and must never be spawned in the daemon-global pool (issue #557).
            if !server_config.shared {
                continue;
            }
            let name = name.clone();
            let server_config = server_config.clone();
            connect_futures.push(async move {
                let key = PoolKey::global(&name);
                let result = self.ensure_connected(key, server_config).await;
                (name, result)
            });
        }
        drop(config);

        let mut successes = 0;
        let mut failures = Vec::new();

        for (name, result) in futures::future::join_all(connect_futures).await {
            match result {
                Ok(new_connection) => {
                    if new_connection {
                        successes += 1;
                    }
                }
                Err(error_msg) => {
                    crate::logging::error(&format!(
                        "Failed to connect to MCP server '{}': {}",
                        name, error_msg
                    ));
                    failures.push((name, error_msg));
                }
            }
        }

        if successes == 0 {
            successes = self
                .handles
                .read()
                .await
                .values()
                .filter(|handle| !handle.is_closed())
                .count();
        }

        (successes, failures)
    }

    /// Connect to a specific server by name and config.
    ///
    /// A server marked `per_project` is started in `project_dir` and keyed by
    /// that directory, so every session in the same project shares one
    /// process and a session in another project gets its own.
    pub async fn connect_server(
        &self,
        name: &str,
        config: &McpServerConfig,
        project_dir: Option<&std::path::Path>,
    ) -> Result<()> {
        let key = PoolKey::for_dir(name, config, project_dir);
        self.ensure_connected(key, config.clone())
            .await
            .map(|_| ())
            .map_err(|error_msg| anyhow::anyhow!(error_msg))
            .with_context(|| format!("Failed to connect to MCP server '{}'", name))
    }

    /// Disconnect a specific server
    pub async fn disconnect_server(&self, name: &str) {
        {
            let mut handles = self.handles.write().await;
            handles.retain(|key, _| key.name != name);
        }
        {
            let mut clients = self.clients.lock().await;
            let mut doomed: Vec<PoolKey> = Vec::new();
            for (key, _) in clients.iter() {
                if key.name == name {
                    doomed.push(key.clone());
                }
            }
            for key in doomed {
                if let Some(mut client) = clients.remove(&key) {
                    client.shutdown().await;
                }
            }
        }
        {
            let mut refs = self.ref_counts.lock().await;
            refs.retain(|key, _| key.name != name);
        }
        {
            let mut errors = self.last_errors.write().await;
            errors.retain(|key, _| key.name != name);
        }
    }

    /// Disconnect all servers
    pub async fn disconnect_all(&self) {
        {
            let mut handles = self.handles.write().await;
            handles.clear();
        }
        {
            let mut clients = self.clients.lock().await;
            for (_, mut client) in clients.drain() {
                client.shutdown().await;
            }
        }
        {
            let mut refs = self.ref_counts.lock().await;
            refs.clear();
        }
        {
            let mut errors = self.last_errors.write().await;
            errors.clear();
        }
    }

    /// Get handles for all connected servers (for a new session).
    /// Increments reference counts.
    ///
    /// `project_dir` selects which per-project process the session gets; a
    /// session in project A never receives the handle of project B.
    pub async fn acquire_handles(
        &self,
        session_id: &str,
        project_dir: Option<&std::path::Path>,
    ) -> HashMap<String, McpHandle> {
        let mut acquired: Vec<PoolKey> = Vec::new();
        let result: HashMap<String, McpHandle> = {
            let handles = self.handles.read().await;
            let mut result: HashMap<String, McpHandle> = HashMap::new();
            for (key, handle) in handles.iter() {
                if handle.is_closed() || !key.matches_dir(project_dir) {
                    continue;
                }
                acquired.push(key.clone());
                result.insert(key.name.clone(), handle.clone());
            }
            result
        };

        // Count the keys actually handed out: a per-project process is keyed by
        // its project directory, so a global key would misattribute the count.
        let mut refs = self.ref_counts.lock().await;
        for key in acquired {
            *refs.entry(key).or_insert(0) += 1;
        }

        if !result.is_empty() {
            crate::logging::info(&format!(
                "MCP pool: session '{}' acquired {} server handle(s)",
                session_id,
                result.len()
            ));
        }

        result
    }

    /// Release handles when a session disconnects.
    /// Decrements reference counts.
    pub async fn release_handles(
        &self,
        session_id: &str,
        server_names: &[String],
        project_dir: Option<&std::path::Path>,
    ) {
        let mut refs = self.ref_counts.lock().await;
        // Decrement only the keys this session could have acquired: the
        // daemon-wide key and the per-project key for its own directory.
        for (key, count) in refs.iter_mut() {
            if server_names.contains(&key.name) && key.matches_dir(project_dir) {
                *count = count.saturating_sub(1);
            }
        }
        refs.retain(|_, count| *count > 0);

        if !server_names.is_empty() {
            crate::logging::info(&format!(
                "MCP pool: session '{}' released {} server handle(s)",
                session_id,
                server_names.len()
            ));
        }
    }

    /// Get a handle for a specific server
    pub async fn get_handle(&self, name: &str) -> Option<McpHandle> {
        let handles = self.handles.read().await;
        handles
            .get(&PoolKey::global(name))
            .filter(|handle| !handle.is_closed())
            .cloned()
    }

    /// Get a handle for a specific server, resolved for a session directory.
    ///
    /// For a `per_project` server this returns the process that runs in that
    /// directory; for a daemon-wide server the single shared process.
    pub async fn get_handle_for_dir(
        &self,
        name: &str,
        config: &McpServerConfig,
        project_dir: Option<&std::path::Path>,
    ) -> Option<McpHandle> {
        let key = PoolKey::for_dir(name, config, project_dir);
        let handles = self.handles.read().await;
        handles.get(&key).filter(|handle| !handle.is_closed()).cloned()
    }

    /// Get all available tools from all connected servers
    pub async fn all_tools(&self) -> Vec<(String, McpToolDef)> {
        let handles = self.handles.read().await;
        let mut tools = Vec::new();
        for (server_name, handle) in handles.iter() {
            if handle.is_closed() {
                continue;
            }
            for tool in handle.tools() {
                tools.push((server_name.name.clone(), tool));
            }
        }
        tools
    }

    /// Get list of connected server names
    pub async fn connected_servers(&self) -> Vec<String> {
        let handles = self.handles.read().await;
        let mut names: Vec<String> = handles
            .iter()
            .filter(|(_, handle)| !handle.is_closed())
            .map(|(key, _)| key.name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Call a tool on a specific server
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: serde_json::Value,
    ) -> Result<super::protocol::ToolCallResult> {
        let handles = self.handles.read().await;
        let handle = handles
            .get(&PoolKey::global(server))
            .filter(|handle| !handle.is_closed())
            .with_context(|| format!("MCP server '{}' not connected", server))?;
        handle.call_tool(tool, arguments).await
    }

    /// Reload config and reconnect all servers
    pub async fn reload(&self) -> (usize, Vec<(String, String)>) {
        self.disconnect_all().await;
        *self.config.write().await = McpConfig::load_for_dir(self.config_dir.as_deref());
        self.connect_all().await
    }

    /// Get current config
    pub async fn config(&self) -> McpConfig {
        self.config.read().await.clone()
    }

    /// Check if any servers are connected
    pub async fn has_connections(&self) -> bool {
        let handles = self.handles.read().await;
        handles.values().any(|handle| !handle.is_closed())
    }

    /// Get reference counts (for debugging)
    pub async fn ref_counts(&self) -> HashMap<String, usize> {
        self.ref_counts
            .lock()
            .await
            .iter()
            .map(|(key, count)| (key.name.clone(), *count))
            .collect()
    }

    async fn begin_connect(&self, key: &PoolKey) -> ConnectAttempt {
        let mut connecting = self.connecting.lock().await;
        if let Some(notify) = connecting.get(key) {
            return ConnectAttempt::Wait(Arc::clone(notify));
        }

        {
            let mut handles = self.handles.write().await;
            if handles.get(key).is_some_and(|handle| !handle.is_closed()) {
                return ConnectAttempt::Connected;
            }
            handles.remove(key);
        }
        // Preserve reference counts held by existing sessions, but release the
        // dead transport before starting a fresh initialization handshake.
        self.clients.lock().await.remove(key);

        let notify = Arc::new(Notify::new());
        connecting.insert(key.clone(), Arc::clone(&notify));
        ConnectAttempt::Leader(notify)
    }

    async fn finish_connect(
        &self,
        key: &PoolKey,
        notify: Arc<Notify>,
        result: Result<McpClient>,
    ) {
        match result {
            Ok(client) => {
                let handle = client.handle();
                {
                    let mut handles = self.handles.write().await;
                    handles.insert(key.clone(), handle);
                }
                {
                    let mut clients = self.clients.lock().await;
                    clients.insert(key.clone(), client);
                }
                {
                    let mut errors = self.last_errors.write().await;
                    errors.remove(key);
                }
            }
            Err(error) => {
                let mut errors = self.last_errors.write().await;
                let failures = errors.get(key).map_or(0, |r| r.failures) + 1;
                errors.insert(
                    key.clone(),
                    FailedConnectRecord {
                        message: format!("{:#}", error),
                        failed_at: Instant::now(),
                        failures,
                    },
                );
            }
        }

        {
            let mut connecting = self.connecting.lock().await;
            if connecting
                .get(key)
                .map(|current| Arc::ptr_eq(current, &notify))
                .unwrap_or(false)
            {
                connecting.remove(key);
            }
        }

        notify.notify_waiters();
    }

    async fn ensure_connected(
        &self,
        key: PoolKey,
        config: McpServerConfig,
    ) -> std::result::Result<bool, String> {
        if let Some(record) = self.recent_failure(&key).await {
            let retry_after = record
                .cooldown()
                .saturating_sub(record.failed_at.elapsed())
                .as_secs()
                .max(1);
            crate::logging::info(&format!(
                "MCP: Skipping reconnect to '{}' for {}s after recent failure",
                key.name, retry_after
            ));
            return Err(format!(
                "{} (retry suppressed for ~{}s after recent failure)",
                record.message, retry_after
            ));
        }

        match self.begin_connect(&key).await {
            ConnectAttempt::Connected => Ok(false),
            ConnectAttempt::Wait(notify) => {
                self.wait_for_connect(&key, &notify).await;
                if self
                    .handles
                    .read()
                    .await
                    .get(&key)
                    .is_some_and(|handle| !handle.is_closed())
                {
                    Ok(false)
                } else {
                    let error = self
                        .last_errors
                        .read()
                        .await
                        .get(&key)
                        .map(|record| record.message.clone())
                        .unwrap_or_else(|| {
                            "Connection attempt did not produce a handle".to_string()
                        });
                    Err(error)
                }
            }
            ConnectAttempt::Leader(notify) => {
                // A per-project server must start in its project directory so
                // it resolves that project (a git repo, a language-server
                // index) instead of the daemon's working directory.
                let working_dir = key.project_dir.as_deref();
                let result = McpClient::connect_in_dir(key.name.clone(), &config, working_dir)
                    .await;
                let outcome = match &result {
                    Ok(_) => Ok(true),
                    Err(error) => Err(format!("{:#}", error)),
                };
                self.finish_connect(&key, notify, result).await;
                outcome
            }
        }
    }

    async fn wait_for_connect(&self, key: &PoolKey, notify: &Arc<Notify>) {
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        // Registration must precede this check because notify_waiters does
        // not retain a permit when the leader finishes before we start waiting.
        if self
            .connecting
            .lock()
            .await
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, notify))
        {
            notified.await;
        }
    }

    async fn recent_failure(&self, key: &PoolKey) -> Option<FailedConnectRecord> {
        if self
            .handles
            .read()
            .await
            .get(key)
            .is_some_and(|handle| !handle.is_closed())
        {
            return None;
        }

        self.last_errors
            .read()
            .await
            .get(key)
            .filter(|record| record.failed_at.elapsed() < record.cooldown())
            .cloned()
    }
}

/// Global pool singleton
static SHARED_POOL: tokio::sync::OnceCell<Arc<SharedMcpPool>> = tokio::sync::OnceCell::const_new();

/// Initialize the global shared MCP pool. Call once at daemon startup.
pub async fn init_shared_pool() -> Arc<SharedMcpPool> {
    SHARED_POOL
        .get_or_init(|| async {
            let pool = SharedMcpPool::from_default_config();
            Arc::new(pool)
        })
        .await
        .clone()
}

/// Get the global shared pool, if initialized.
pub fn get_shared_pool() -> Option<Arc<SharedMcpPool>> {
    SHARED_POOL.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::{ConnectAttempt, PoolKey, SharedMcpPool};
    use crate::mcp::protocol::McpConfig;
    use std::sync::Arc;

    #[tokio::test]
    async fn issue_790_reload_reuses_default_config_directory() {
        let _guard = crate::storage::lock_test_env();
        let original_cwd = std::env::current_dir().expect("current cwd");
        let previous_home = std::env::var_os("JCODE_HOME");
        let home = tempfile::tempdir().expect("home tempdir");
        let first_project = tempfile::tempdir().expect("first project tempdir");
        let second_project = tempfile::tempdir().expect("second project tempdir");
        crate::env::set_var("JCODE_HOME", home.path());
        std::fs::write(
            first_project.path().join(".mcp.json"),
            r#"{"mcpServers":{"first":{"command":"first-server","shared":false}}}"#,
        )
        .expect("write first project config");
        std::fs::write(
            second_project.path().join(".mcp.json"),
            r#"{"mcpServers":{"second":{"command":"second-server","shared":false}}}"#,
        )
        .expect("write second project config");

        std::env::set_current_dir(first_project.path()).expect("set first project cwd");
        let pool = SharedMcpPool::from_default_config();
        let initially_loaded_first = pool.config().await.servers.contains_key("first");

        std::fs::write(
            first_project.path().join(".mcp.json"),
            r#"{"mcpServers":{"first-reloaded":{"command":"first-reloaded-server","shared":false}}}"#,
        )
        .expect("update first project config");

        std::env::set_current_dir(second_project.path()).expect("set second project cwd");
        let _ = pool.reload().await;
        let reloaded = pool.config().await;

        std::env::set_current_dir(original_cwd).expect("restore cwd");
        if let Some(previous_home) = previous_home {
            crate::env::set_var("JCODE_HOME", previous_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }

        assert!(initially_loaded_first);
        assert!(!reloaded.servers.contains_key("first"));
        assert!(reloaded.servers.contains_key("first-reloaded"));
        assert!(!reloaded.servers.contains_key("second"));
    }

    #[tokio::test]
    async fn begin_connect_deduplicates_concurrent_attempts() {
        let pool = Arc::new(SharedMcpPool::new(McpConfig::default()));
        let key = PoolKey::global("demo");

        let first = pool.begin_connect(&key).await;
        let second = pool.begin_connect(&key).await;

        let first_notify = match first {
            ConnectAttempt::Leader(notify) => notify,
            _ => panic!("first attempt should lead"),
        };
        let second_notify = match second {
            ConnectAttempt::Wait(notify) => notify,
            _ => panic!("second attempt should wait"),
        };

        assert!(Arc::ptr_eq(&first_notify, &second_notify));
    }

    #[tokio::test]
    async fn reconnect_waiter_observes_leader_completion_before_registration() {
        let pool = SharedMcpPool::new(McpConfig::default());
        let key = PoolKey::global("early");
        let ConnectAttempt::Leader(leader) = pool.begin_connect(&key).await else {
            panic!("leader");
        };
        let ConnectAttempt::Wait(waiter) = pool.begin_connect(&key).await else {
            panic!("waiter");
        };
        pool.finish_connect(&key, leader, Err(anyhow::anyhow!("fixture failure")))
            .await;
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            pool.wait_for_connect(&key, &waiter),
        )
        .await
        .expect("an already-completed attempt must not wait for a lost notification");
    }

    #[tokio::test]
    async fn connect_all_skips_non_shared_servers() {
        // Issue #557: shared:false servers are owned per-session and must not
        // be spawned in the daemon-global pool. If the pool tried to connect
        // this nonexistent command it would show up as a failure.
        let mut config = McpConfig::default();
        config.servers.insert(
            "owned-only".to_string(),
            crate::mcp::protocol::McpServerConfig {
                command: "/nonexistent/jcode-test-mcp-557".to_string(),
                args: vec![],
                env: Default::default(),
                shared: false,
                transport: None,
                url: None,
                headers: std::collections::HashMap::new(),
                enabled: None,
                disabled: None,
                timeout_secs: None,
                per_project: false,
            },
        );
        let pool = SharedMcpPool::new(config);

        let (successes, failures) = pool.connect_all().await;
        assert_eq!(successes, 0);
        assert!(
            failures.is_empty(),
            "non-shared server must be skipped, got failures: {failures:?}"
        );
    }

    fn per_project_config() -> crate::mcp::protocol::McpServerConfig {
        crate::mcp::protocol::McpServerConfig {
            command: "unused-fixture".to_string(),
            args: vec![],
            env: Default::default(),
            shared: true,
            transport: None,
            url: None,
            headers: std::collections::HashMap::new(),
            enabled: None,
            disabled: None,
            timeout_secs: None,
            per_project: true,
        }
    }

    #[test]
    fn per_project_server_keys_one_process_per_directory() {
        // A per-project server must resolve to a different key for each
        // project directory, so the pool starts one process per project
        // instead of one for the whole daemon (wrong project) or one per
        // session (N processes for the same project).
        let config = per_project_config();
        let first = PoolKey::for_dir("scoped", &config, Some(std::path::Path::new("/p/one")));
        let second = PoolKey::for_dir("scoped", &config, Some(std::path::Path::new("/p/two")));
        let first_again = PoolKey::for_dir("scoped", &config, Some(std::path::Path::new("/p/one")));

        assert_ne!(first, second, "different projects need different processes");
        assert_eq!(first, first_again, "the same project reuses one process");
    }

    #[test]
    fn per_project_server_never_leaks_into_another_project() {
        let config = per_project_config();
        let key = PoolKey::for_dir("scoped", &config, Some(std::path::Path::new("/p/one")));

        assert!(key.matches_dir(Some(std::path::Path::new("/p/one"))));
        assert!(!key.matches_dir(Some(std::path::Path::new("/p/two"))));
        assert!(!key.matches_dir(None));
    }

    #[test]
    fn daemon_wide_server_stays_shared_across_projects() {
        // A server without `per_project` keeps one process for the whole
        // daemon, so every session in every project gets the same handle.
        let mut config = per_project_config();
        config.per_project = false;

        let first = PoolKey::for_dir("plain", &config, Some(std::path::Path::new("/p/one")));
        let second = PoolKey::for_dir("plain", &config, Some(std::path::Path::new("/p/two")));

        assert_eq!(first, second);
        assert!(first.matches_dir(Some(std::path::Path::new("/p/one"))));
        assert!(first.matches_dir(Some(std::path::Path::new("/p/two"))));
    }
}

#[cfg(test)]
mod backoff_tests {
    use super::*;

    #[test]
    fn failed_connect_cooldown_grows_and_caps() {
        assert_eq!(failed_connect_cooldown(1), Duration::from_secs(30));
        assert_eq!(failed_connect_cooldown(2), Duration::from_secs(60));
        assert_eq!(failed_connect_cooldown(4), Duration::from_secs(240));
        assert_eq!(
            failed_connect_cooldown(50),
            MAX_FAILED_CONNECT_RETRY_COOLDOWN
        );
    }
}
