// reference: no prior art, searched crates/jcode-app-core/src/agent_tests/ and
// crates/jcode-base/src/hooks_tests.rs for an existing test that asserts a
// lifecycle hook fires on the swarm capture path; none exists.
//
// Negative control for the swarm lifecycle-hook fix.
//
// Swarm workers run through `run_once_capture`, which (before the fix) fired
// NO lifecycle hooks. Every external integration that observes turns - Entire.io
// checkpoints, status bars, turn counters - therefore saw nothing for a whole
// swarm.
//
// This test is the SAME test for base and fixed:
//   base  -> the marker file is never written -> assertion fails
//   fixed -> both `turn_start` and `turn_end` are written -> assertion passes
//
// Note on paths: the hook command parser treats a backslash as an escape
// character, so a Windows path would arrive at the child process with its
// separators stripped. Both the script and the marker path therefore use
// forward slashes, which Python accepts on Windows.

use super::*;

struct HookMarkerProvider;

#[async_trait]
impl Provider for HookMarkerProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let (_tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(1);
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn context_window(&self) -> usize {
        1_000
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
}

/// A path in the form the hook command parser can carry: forward slashes.
fn hook_safe_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

const MARKER_SCRIPT: &str = r#"import os, sys
with open(sys.argv[1], "a", encoding="utf-8") as handle:
    handle.write(os.environ.get("JCODE_HOOK_EVENT", "") + "\n")
"#;

#[tokio::test]
async fn swarm_capture_path_fires_turn_lifecycle_hooks() {
    let _guard = crate::storage::lock_test_env();

    let temp = std::env::temp_dir();
    let script = temp.join(format!("jcode-hook-marker-{}.py", std::process::id()));
    let marker = temp.join(format!("jcode-hook-marker-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    std::fs::write(&script, MARKER_SCRIPT).expect("write marker script");

    let command = format!(
        "python {} {}",
        hook_safe_path(&script),
        hook_safe_path(&marker)
    );
    let previous_start = std::env::var_os("JCODE_HOOK_TURN_START");
    let previous_end = std::env::var_os("JCODE_HOOK_TURN_END");
    crate::env::set_var("JCODE_HOOK_TURN_START", &command);
    crate::env::set_var("JCODE_HOOK_TURN_END", &command);
    crate::config::invalidate_config_cache();

    let provider: Arc<dyn Provider> = Arc::new(HookMarkerProvider);
    let registry = Registry::empty();
    let mut agent = Agent::new(provider, registry);

    // The swarm worker path.
    let _ = agent.run_once_capture("swarm worker prompt").await;

    // The hook process is detached, so give it a moment to write.
    let mut observed = String::new();
    for _ in 0..50 {
        if let Ok(text) = std::fs::read_to_string(&marker) {
            observed = text;
            if observed.contains("turn_start") && observed.contains("turn_end") {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    match previous_start {
        Some(value) => crate::env::set_var("JCODE_HOOK_TURN_START", value),
        None => crate::env::remove_var("JCODE_HOOK_TURN_START"),
    }
    match previous_end {
        Some(value) => crate::env::set_var("JCODE_HOOK_TURN_END", value),
        None => crate::env::remove_var("JCODE_HOOK_TURN_END"),
    }
    crate::config::invalidate_config_cache();
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&script);

    assert!(
        observed.contains("turn_start"),
        "swarm capture path did not fire the turn_start hook; observed: {observed:?}"
    );
    assert!(
        observed.contains("turn_end"),
        "swarm capture path did not fire the turn_end hook; observed: {observed:?}"
    );
}
