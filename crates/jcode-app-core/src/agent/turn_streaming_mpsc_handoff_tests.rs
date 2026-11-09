use super::*;

/// Isolate a timing-sensitive test from the user's global hook configuration.
///
/// The handoff tests assert exact elapsed times on a paused virtual clock. The
/// user's `config.toml` can configure `pre_tool_transform`, `pre_tool`,
/// `post_tool` and `post_tool_feedback` hooks. Each one spawns a subprocess
/// inside the measured path and contributes its own timeout to the measured
/// elapsed time, so the measurement would describe the hook configuration
/// rather than the behaviour under test.
///
/// Measured on this host (`config.toml` with 1 transformer, 8 pre_tool gates
/// and 4 feedback hooks):
///
/// ```text
/// 1 x  500ms (pre_tool_transform)  =  0.5s
/// 8 x 5000ms (pre_tool)            = 40.0s
///      8s    (the tool under test) =  8.0s
/// 4 x 10000ms(post_tool_feedback)  = 40.0s
///                                  = 88.5s   (the asserted value was 8s)
/// ```
///
/// Why the subprocess time lands in the measurement: with a paused clock,
/// tokio auto-advances virtual time to the next timer deadline whenever the
/// runtime has no other work to do. A spawned hook process is not a timer, so
/// the runtime looks idle and the clock jumps straight to the hook's timeout
/// deadline. Each configured hook therefore adds its full timeout to the
/// measured elapsed time, even though the hook itself finishes in ~100ms of
/// real time.
///
/// `JCODE_HOOKS_DISABLED` is the same switch a hook process receives so nested
/// jcode calls do not recurse, so it suppresses every hook event without
/// touching the config cache.
///
/// Reference: tokio 1.50.0 `src/time/clock.rs:354` (`can_auto_advance`) and
/// `src/runtime/time/mod.rs:259` (`park_thread_timeout`) for the auto-advance
/// rule; `docs.rs/tokio/latest/tokio/attr.test.html` for `start_paused`.
struct HookIsolationGuard {
    _env: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
}

impl HookIsolationGuard {
    fn new() -> Self {
        // Hold the shared test-env lock: sibling tests mutate the environment
        // (JCODE_HOME, the hook env vars) under it.
        let env = crate::storage::lock_test_env();
        let previous = std::env::var_os("JCODE_HOOKS_DISABLED");
        crate::env::set_var("JCODE_HOOKS_DISABLED", "1");
        let guard = Self {
            _env: env,
            previous,
        };
        // The guard exists so that no hook can reach the measured path. Check
        // that here, so a broken suppression mechanism fails loudly instead of
        // silently inflating a measurement.
        for event in [
            "pre_tool_transform",
            "pre_tool",
            "post_tool",
            "post_tool_feedback",
        ] {
            assert!(
                !crate::hooks::hook_configured(event),
                "hook isolation failed: '{event}' is still configured"
            );
        }
        guard
    }
}

impl Drop for HookIsolationGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => crate::env::set_var("JCODE_HOOKS_DISABLED", value),
            None => crate::env::remove_var("JCODE_HOOKS_DISABLED"),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn configured_streaming_budget_and_disabled_handoff() {
    for budget in [Duration::from_millis(25), Duration::from_secs(12)] {
        let bg = InterruptSignal::new();
        let shutdown = InterruptSignal::new();
        let start = tokio::time::Instant::now();
        assert!(wait_for_tool_handoff(&bg, &shutdown, true, budget).await);
        assert_eq!(start.elapsed(), budget);
    }
    for shutdown_requested in [false, true] {
        let bg = InterruptSignal::new();
        let shutdown = InterruptSignal::new();
        tokio::select! {
            biased;
            _ = wait_for_tool_handoff(&bg, &shutdown, true, Duration::ZERO) => {
                panic!("zero budget should disable automatic handoff");
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        if shutdown_requested {
            shutdown.fire();
        } else {
            bg.fire();
        }
        assert!(!wait_for_tool_handoff(&bg, &shutdown, true, Duration::ZERO).await);
    }
}

#[tokio::test(start_paused = true)]
async fn callback_paths_honor_custom_and_disabled_budgets() {
    // SDK and nonstreaming calls share execute_with_background_budget.
    let _isolation = HookIsolationGuard::new();
    for (budget, action, backgrounded) in [
        (Duration::from_millis(1250), "status", true),
        (Duration::from_secs(12), "status", false),
        (Duration::ZERO, "status", false),
        (Duration::from_millis(10), "wait", false),
    ] {
        let registry = Registry::empty();
        registry.register("bg".into(), Arc::new(WaitProbe)).await;
        let ctx = ToolContext {
            session_id: format!("configured-handoff-{action}-{}", budget.as_millis()),
            message_id: "message".into(),
            tool_call_id: "configured-probe".into(),
            working_dir: None,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        };
        let start = tokio::time::Instant::now();
        let result = execute_with_background_budget(
            registry,
            "bg".into(),
            serde_json::json!({"action": action}),
            ctx,
            budget,
        )
        .await
        .unwrap();
        if backgrounded {
            assert_eq!(start.elapsed(), budget);
            assert!(result.output.contains("automatically after 1.25 seconds"));
            assert!(result.output.contains("task_id:"));
            let task_id = result
                .output
                .split("task_id: ")
                .nth(1)
                .unwrap()
                .split(')')
                .next()
                .unwrap();
            let finished = crate::background::global()
                .wait(task_id, Duration::from_secs(30), false)
                .await
                .unwrap();
            assert_eq!(
                finished.task.status,
                crate::bus::BackgroundTaskStatus::Completed
            );
            assert_eq!(
                crate::background::global().output(task_id).await.unwrap(),
                "wait finished"
            );
        } else {
            assert_eq!(start.elapsed(), Duration::from_secs(8));
            assert_eq!(result.output, "wait finished");
        }
    }
}

struct WaitProbe;

#[tokio::test(start_paused = true)]
async fn ask_user_waits_past_budget_and_returns_its_answer() {
    let _isolation = HookIsolationGuard::new();
    let registry = Registry::empty();
    registry
        .register("ask_user".into(), Arc::new(WaitProbe))
        .await;
    let ctx = ToolContext {
        session_id: "foreground-ask-user-probe".into(),
        message_id: "message".into(),
        tool_call_id: "ask-user-probe".into(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    };
    let start = tokio::time::Instant::now();
    let result = execute_with_background_budget(
        registry,
        "ask_user".into(),
        serde_json::json!({"question": "Continue?"}),
        ctx,
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert_eq!(start.elapsed(), Duration::from_secs(8));
    assert_eq!(result.output, "wait finished");
}

#[tokio::test(start_paused = true)]
async fn streaming_ask_user_wait_is_not_automatically_backgrounded() {
    for input in [
        serde_json::json!({}),
        serde_json::json!({"action": "choose"}),
    ] {
        assert!(!auto_background_allowed("ask_user", &input));
        let bg = InterruptSignal::new();
        let shutdown = InterruptSignal::new();
        tokio::select! {
            biased;
            _ = wait_for_tool_handoff(&bg, &shutdown, auto_background_allowed("ask_user", &input), Duration::from_secs(3)) => {
                panic!("human decision was automatically backgrounded");
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        shutdown.fire();
        assert!(!wait_for_tool_handoff(&bg, &shutdown, false, Duration::from_secs(3)).await);
    }
}

#[async_trait::async_trait]
impl crate::tool::Tool for WaitProbe {
    fn name(&self) -> &str {
        "bg"
    }
    fn description(&self) -> &str {
        "Wait probe"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(
        &self,
        _: serde_json::Value,
        _: ToolContext,
    ) -> Result<crate::tool::ToolOutput> {
        tokio::time::sleep(Duration::from_secs(8)).await;
        Ok(crate::tool::ToolOutput::new("wait finished"))
    }
}

#[tokio::test(start_paused = true)]
async fn nonstreaming_background_wait_returns_original_result_after_budget() {
    let _isolation = HookIsolationGuard::new();
    let registry = Registry::empty();
    registry.register("bg".into(), Arc::new(WaitProbe)).await;
    let ctx = ToolContext {
        session_id: "foreground-wait-probe".into(),
        message_id: "message".into(),
        tool_call_id: "wait-probe".into(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    };
    let start = tokio::time::Instant::now();
    let result = execute_with_background(
        registry,
        "bg".into(),
        serde_json::json!({"action":"wait"}),
        ctx,
    )
    .await
    .unwrap();
    assert_eq!(result.output, "wait finished");
    assert_eq!(start.elapsed(), Duration::from_secs(8));
}

#[tokio::test(start_paused = true)]
async fn background_wait_is_not_automatically_backgrounded() {
    let input = serde_json::json!({"action": "wait", "task_id": "task"});
    assert!(!auto_background_allowed("bg", &input));
    assert!(auto_background_allowed(
        "bg",
        &serde_json::json!({"action": "status"})
    ));
    assert!(auto_background_allowed("bash", &input));
    let bg = InterruptSignal::new();
    let shutdown = InterruptSignal::new();
    tokio::select! {
        biased;
        _ = wait_for_tool_handoff(&bg, &shutdown, auto_background_allowed("bg", &input), Duration::from_secs(3)) => {
            panic!("bg wait was backgrounded while waiting for its dependency");
        }
        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
    }
    // Reloads still interrupt a wait safely, without waiting for its deadline.
    shutdown.fire();
    assert!(!wait_for_tool_handoff(&bg, &shutdown, false, Duration::from_secs(3)).await);
}

#[tokio::test(start_paused = true)]
async fn slow_tool_handoff_occurs_after_three_seconds_without_cancelling_work() {
    let bg = InterruptSignal::new();
    let shutdown = InterruptSignal::new();
    let mut work = tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        42
    });
    let start = tokio::time::Instant::now();
    tokio::select! {
        biased;
        _ = &mut work => panic!("slow work completed before handoff"),
        automatic = wait_for_tool_handoff(&bg, &shutdown, true, Duration::from_secs(3)) => assert!(automatic),
    }
    assert_eq!(start.elapsed(), Duration::from_secs(3));
    assert!(!work.is_finished());
    assert_eq!(work.await.unwrap(), 42);
}

#[tokio::test(start_paused = true)]
async fn fast_tool_finishes_in_foreground() {
    let bg = InterruptSignal::new();
    let shutdown = InterruptSignal::new();
    tokio::select! {
        biased;
        _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        _ = wait_for_tool_handoff(&bg, &shutdown, true, Duration::from_secs(3)) => panic!("fast tool backgrounded"),
    }
}

#[tokio::test(start_paused = true)]
async fn manual_background_and_shutdown_do_not_wait_for_budget() {
    for shutdown_requested in [false, true] {
        let bg = InterruptSignal::new();
        let shutdown = InterruptSignal::new();
        if shutdown_requested {
            shutdown.fire();
        } else {
            bg.fire();
        }
        let start = tokio::time::Instant::now();
        assert!(!wait_for_tool_handoff(&bg, &shutdown, true, Duration::from_secs(3)).await);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }
}
