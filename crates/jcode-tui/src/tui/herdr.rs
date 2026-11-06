//! Native agent-state reporting for the herdr terminal multiplexer.
//!
//! When jcode runs inside a herdr pane (`HERDR_ENV=1`), it reports its state
//! (`idle` / `working` / `blocked`), the command that resumes the current
//! session, and releases the pane when the user quits. `blocked` is reported
//! while the client waits for a user decision (a keypress offer such as a
//! provider fallback or a merge prompt); the turn itself has already ended at
//! that point. See <https://herdr.dev/docs/add-herdr-support/>.
//!
//! reference: https://herdr.dev/docs/add-herdr-support/
//! reference: https://github.com/1jehuang/jcode
//! Reports go through `"$HERDR_BIN_PATH" pane report-agent ...` on a single
//! background thread. Only the latest pending report is kept, every command
//! has a short timeout, and failures are ignored so herdr can never slow down
//! or break the TUI. Outside herdr every entry point is a no-op.

use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Stable integration id. Must not start with `herdr:`.
const SOURCE: &str = "jcode";
/// Agent name shown in herdr's sidebar and `herdr agent list`.
const AGENT: &str = "jcode";
/// Upper bound on a single herdr CLI call.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
/// Wait before a failed report is retried. Without it a down herdr would spawn
/// one CLI per frame; with it the pane still recovers within a second.
const REPORT_RETRY_DELAY: Duration = Duration::from_secs(1);
/// How often a live client re-asserts its agent in herdr's registry.
///
/// reference: https://herdr.dev/docs/add-herdr-support/ (herdr clears a pane's
/// agent when the agent process exits without releasing the pane, so a client
/// restarted in the same pane - which is what a server reload does - has to
/// register itself again; the report that recreated the agent can still be
/// undone by the cleanup of the previous process a second later).
const REPORT_HEARTBEAT: Duration = Duration::from_secs(30);
/// Delay of the first re-assert. herdr notices a dead agent within a second
/// or two, so the first re-assert has to happen after that window.
const REPORT_REASSERT_DELAY: Duration = Duration::from_secs(5);
/// herdr's resume argv limits.
const MAX_RESUME_ARGS: usize = 64;
const MAX_RESUME_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    /// The client is waiting for the user to decide something.
    Blocked,
}

impl AgentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }

    /// Short explanation herdr shows next to a blocked agent.
    fn message(self) -> Option<&'static str> {
        match self {
            Self::Blocked => Some("waiting for your decision"),
            _ => None,
        }
    }
}

/// A state report plus, when known, the session that `resume_argv` reopens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateReport {
    pub state: AgentState,
    pub session_id: Option<String>,
    pub resume_argv: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HerdrEnv {
    bin: String,
    pane_id: String,
}

impl HerdrEnv {
    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let non_empty = |key: &str| get(key).filter(|value| !value.trim().is_empty());
        if non_empty("HERDR_ENV").as_deref() != Some("1") {
            return None;
        }
        // The socket must exist for the CLI to reach herdr; require it as the
        // docs do, even though we talk to herdr through its binary.
        non_empty("HERDR_SOCKET_PATH")?;
        Some(Self {
            bin: non_empty("HERDR_BIN_PATH")?,
            pane_id: non_empty("HERDR_PANE_ID")?,
        })
    }
}

enum Job {
    Report(StateReport),
}

#[derive(Default)]
struct Slot {
    pending: Option<Job>,
    released: bool,
    /// Earliest time a pending report may be sent again.
    retry_at: Option<Instant>,
}

// reference: https://herdr.dev/docs/add-herdr-support/ (report-agent protocol)
struct Reporter {
    env: HerdrEnv,
    slot: Arc<(Mutex<Slot>, Condvar)>,
    /// The last report herdr confirmed. Filled only after a successful
    /// delivery, so an undelivered state is retried instead of suppressed.
    last_delivered: Arc<Mutex<Option<StateReport>>>,
}

static REPORTER: OnceLock<Option<Reporter>> = OnceLock::new();
static LAST_SEQ: Mutex<u64> = Mutex::new(0);
/// Set once herdr rejects a report carrying a resume command. herdr older
/// than 0.9.2 fails the whole report with a usage error on the `--` resume
/// separator, so after that we keep reporting state without it.
static RESUME_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

fn reporter() -> Option<&'static Reporter> {
    REPORTER
        .get_or_init(|| {
            let env = HerdrEnv::from_lookup(|key| std::env::var(key).ok())?;
            crate::logging::info(&format!(
                "herdr: reporting agent state for pane {}",
                env.pane_id
            ));
            let slot = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
            let worker_slot = Arc::clone(&slot);
            let worker_env = env.clone();
            let last_delivered = Arc::new(Mutex::new(None));
            let worker_delivered = Arc::clone(&last_delivered);
            let spawned = std::thread::Builder::new()
                .name("herdr-reporter".into())
                .spawn(move || {
                    worker_loop(
                        worker_env,
                        worker_slot,
                        worker_delivered,
                        Timing::production(),
                        Arc::new(|env: &HerdrEnv, report: &StateReport| {
                            send_report(env, report.clone())
                        }),
                    )
                });
            if let Err(error) = spawned {
                crate::logging::warn(&format!("herdr: failed to start reporter: {error}"));
                return None;
            }
            Some(Reporter {
                env,
                slot,
                last_delivered,
            })
        })
        .as_ref()
}

/// Report the current state and session. Cheap enough for every UI tick:
/// outside herdr it returns immediately, and unchanged state is dropped
/// without allocating. A newer report replaces one that has not been sent.
pub fn sync(state: AgentState, session_id: Option<&str>) {
    let Some(reporter) = reporter() else {
        return;
    };
    let report = {
        let last = lock(&reporter.last_delivered);
        if last
            .as_ref()
            .is_some_and(|last| last.state == state && last.session_id.as_deref() == session_id)
        {
            return;
        }
        let report = StateReport {
            state,
            session_id: session_id.map(str::to_owned),
            resume_argv: session_id
                .and_then(|id| resume_argv_for_session(id, |key| std::env::var(key).ok())),
        };
        report
    };
    let (slot, wake) = &*reporter.slot;
    let mut slot = lock(slot);
    if slot.released {
        return;
    }
    slot.pending = Some(Job::Report(report));
    // A freshly queued report is current content, so it does not wait for the
    // retry delay of an earlier failed delivery.
    slot.retry_at = None;
    wake.notify_one();
}

/// Release the pane because the user is quitting jcode. Blocks for at most
/// [`COMMAND_TIMEOUT`]. Do not call this when jcode is about to exec itself
/// (reload/update) since the same agent keeps running in the pane.
pub fn release() {
    let Some(reporter) = reporter() else {
        return;
    };
    {
        let mut slot = lock(&reporter.slot.0);
        if slot.released {
            return;
        }
        slot.released = true;
        slot.pending = None;
    }
    let seq = next_seq();
    run_herdr(&reporter.env.bin, &release_args(&reporter.env.pane_id, seq));
}

fn worker_loop(
    env: HerdrEnv,
    slot: Arc<(Mutex<Slot>, Condvar)>,
    delivered: Arc<Mutex<Option<StateReport>>>,
    timing: Timing,
    send: Arc<dyn Fn(&HerdrEnv, &StateReport) -> bool + Send + Sync>,
) {
    let (lock_slot, wake) = &*slot;
    let mut next_heartbeat = Instant::now() + timing.reassert_delay;
    let mut sent_any = false;
    loop {
        let job = {
            let mut guard = lock(lock_slot);
            loop {
                if guard.released {
                    return;
                }
                if let Some(retry_at) = guard.retry_at {
                    let now = Instant::now();
                    if retry_at > now {
                        guard = wake
                            .wait_timeout(guard, retry_at - now)
                            .unwrap_or_else(|e| e.into_inner())
                            .0;
                        continue;
                    }
                    guard.retry_at = None;
                }
                if let Some(job) = guard.pending.take() {
                    break job;
                }
                // herdr drops the agent of a pane whose agent died without
                // releasing it, so keep re-asserting the last delivered
                // report even when nothing changed.
                let now = Instant::now();
                if now >= next_heartbeat {
                    if let Some(report) = lock(&delivered).clone() {
                        break Job::Report(report);
                    }
                    next_heartbeat = now + timing.heartbeat;
                    continue;
                }
                guard = wake
                    .wait_timeout(guard, next_heartbeat - now)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        };
        match job {
            Job::Report(report) => {
                if send(&env, &report) {
                    *lock(&delivered) = Some(report);
                } else {
                    // The report never reached herdr: keep it pending so it is
                    // retried instead of leaving the pane on a stale frame. A
                    // newer report already queued by sync() wins over it.
                    let mut guard = lock(lock_slot);
                    if !guard.released && guard.pending.is_none() {
                        guard.pending = Some(Job::Report(report));
                        guard.retry_at = Some(Instant::now() + REPORT_RETRY_DELAY);
                    }
                }
            }
        }
        next_heartbeat = Instant::now()
            + if sent_any {
                timing.heartbeat
            } else {
                timing.reassert_delay
            };
        sent_any = true;
    }
}

/// Timing of the reporter's re-assert loop. Production uses the constants
/// above; tests use short delays.
#[derive(Clone, Copy)]
struct Timing {
    heartbeat: Duration,
    reassert_delay: Duration,
}

impl Timing {
    fn production() -> Self {
        Self {
            heartbeat: REPORT_HEARTBEAT,
            reassert_delay: REPORT_REASSERT_DELAY,
        }
    }
}

/// Send one report. Returns whether herdr confirmed it.
fn send_report(env: &HerdrEnv, mut report: StateReport) -> bool {
    if RESUME_UNSUPPORTED.load(Ordering::Relaxed) {
        report.resume_argv = None;
    }
    let status = run_herdr(&env.bin, &report_args(&env.pane_id, &report, next_seq()));
    // Only a usage error on a report that carried a resume command means
    // "this herdr predates resume". Timeouts and other failures are retried
    // by the worker and must not disable resume.
    if !(is_usage_error(status) && resume_command(&report).is_some()) {
        return status.is_some_and(|status| status.success());
    }
    report.resume_argv = None;
    let retry = run_herdr(&env.bin, &report_args(&env.pane_id, &report, next_seq()));
    let delivered = retry.is_some_and(|status| status.success());
    if delivered {
        RESUME_UNSUPPORTED.store(true, Ordering::Relaxed);
        crate::logging::info(
            "herdr: this herdr rejects resume commands (needs 0.9.2+), reporting state only",
        );
    }
    delivered
}

/// Exit code clap-based CLIs such as herdr use for argument/usage errors.
const USAGE_ERROR_EXIT_CODE: i32 = 2;

fn is_usage_error(status: Option<ExitStatus>) -> bool {
    status.and_then(|status| status.code()) == Some(USAGE_ERROR_EXIT_CODE)
}

/// The resume command [`report_args`] sends after `--`, if any. herdr only
/// accepts one alongside a session id, and only when it passes its validation.
fn resume_command(report: &StateReport) -> Option<&Vec<String>> {
    report.session_id.as_ref()?;
    report
        .resume_argv
        .as_ref()
        .filter(|argv| valid_resume_argv(argv))
}

/// Strictly increasing across reports, sessions, and exec reloads: wall-clock
/// microseconds, bumped past the previous value if the clock stalls.
fn next_seq() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);
    let mut last = lock(&LAST_SEQ);
    *last = now.max(last.saturating_add(1));
    *last
}

fn report_args(pane_id: &str, report: &StateReport, seq: u64) -> Vec<String> {
    let mut args: Vec<String> = [
        "pane",
        "report-agent",
        pane_id,
        "--source",
        SOURCE,
        "--agent",
        AGENT,
        "--state",
        report.state.as_str(),
        "--seq",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(seq.to_string());
    if let Some(message) = report.state.message() {
        args.push("--message".into());
        args.push(message.to_string());
    }
    if let Some(session_id) = report.session_id.as_deref() {
        args.push("--agent-session-id".into());
        args.push(session_id.to_string());
        if let Some(argv) = resume_command(report) {
            args.push("--".into());
            args.extend(argv.iter().cloned());
        }
    }
    args
}

fn release_args(pane_id: &str, seq: u64) -> Vec<String> {
    vec![
        "pane".into(),
        "release-agent".into(),
        pane_id.into(),
        "--source".into(),
        SOURCE.into(),
        "--agent".into(),
        AGENT.into(),
        "--seq".into(),
        seq.to_string(),
    ]
}

/// herdr rejects resume commands whose first word is a path, that contain an
/// apostrophe or control character, or that exceed 64 args / 8 KiB.
pub fn valid_resume_argv(argv: &[String]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    if program.is_empty() || program.contains('/') || program.contains('\\') {
        return false;
    }
    if argv.len() > MAX_RESUME_ARGS {
        return false;
    }
    let total: usize = argv.iter().map(|arg| arg.len()).sum::<usize>() + argv.len();
    if total > MAX_RESUME_BYTES {
        return false;
    }
    argv.iter()
        .all(|arg| !arg.contains('\'') && !arg.chars().any(char::is_control))
}

/// Command that reopens `session_id` from the pane's working directory,
/// keeping SSH attach targets and self-dev mode so the resumed client
/// behaves like this one. The model and provider live with the session.
fn resume_argv_for_session(
    session_id: &str,
    get: impl Fn(&str) -> Option<String>,
) -> Option<Vec<String>> {
    let mut argv = vec!["jcode".to_string()];
    if let Some(host) = get("JCODE_SSH_REMOTE").filter(|h| !h.trim().is_empty()) {
        argv.extend(["--ssh".to_string(), host]);
        for (flag, variable) in [
            ("--ssh-binary", "JCODE_SSH_BINARY"),
            ("--ssh-server-socket", "JCODE_SSH_SERVER_SOCKET"),
            ("--remote-working-dir", "JCODE_SSH_WORKING_DIR"),
        ] {
            if let Some(value) = get(variable) {
                argv.extend([flag.to_string(), value]);
            }
        }
    }
    argv.extend(["--resume".to_string(), session_id.to_string()]);
    if get(jcode_selfdev_types::CLIENT_SELFDEV_ENV).is_some() {
        argv.push("self-dev".to_string());
    }
    valid_resume_argv(&argv).then_some(argv)
}

/// Run one herdr CLI call with a timeout. Returns its exit status, or `None`
/// when it could not be started or timed out.
fn run_herdr(bin: &str, args: &[String]) -> Option<ExitStatus> {
    let child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("JCODE_HOOKS_DISABLED", "1")
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            crate::logging::warn(&format!("herdr: failed to run {bin}: {error}"));
            return None;
        }
    };
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    crate::logging::info(&format!(
                        "herdr: `{} {}` exited with {status}",
                        bin,
                        args.get(1).map(String::as_str).unwrap_or("")
                    ));
                }
                return Some(status);
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                crate::logging::warn("herdr: report timed out");
                return None;
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Option<HerdrEnv> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        HerdrEnv::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn activates_only_inside_herdr_with_all_vars() {
        let full = [
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "p1"),
            ("HERDR_BIN_PATH", "/usr/bin/herdr"),
            ("HERDR_SOCKET_PATH", "/tmp/herdr.sock"),
        ];
        assert_eq!(
            env(&full),
            Some(HerdrEnv {
                bin: "/usr/bin/herdr".into(),
                pane_id: "p1".into()
            })
        );
        assert_eq!(env(&[]), None);
        let mut not_one = full;
        not_one[0] = ("HERDR_ENV", "0");
        assert_eq!(env(&not_one), None);
        for skip in 1..full.len() {
            let partial: Vec<_> = full
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .map(|(_, kv)| *kv)
                .collect();
            assert_eq!(env(&partial), None, "missing {}", full[skip].0);
        }
    }

    #[test]
    fn report_args_include_resume_command_after_separator() {
        let report = StateReport {
            state: AgentState::Idle,
            session_id: Some("session_fox_123".into()),
            resume_argv: resume_argv_for_session("session_fox_123", |_| None),
        };
        assert_eq!(
            report_args("p1", &report, 42),
            [
                "pane",
                "report-agent",
                "p1",
                "--source",
                "jcode",
                "--agent",
                "jcode",
                "--state",
                "idle",
                "--seq",
                "42",
                "--agent-session-id",
                "session_fox_123",
                "--",
                "jcode",
                "--resume",
                "session_fox_123"
            ]
        );
    }

    #[test]
    fn report_args_without_session_omit_resume() {
        let report = StateReport {
            state: AgentState::Working,
            session_id: None,
            resume_argv: None,
        };
        let args = report_args("p1", &report, 7);
        assert_eq!(&args[7..], ["--state", "working", "--seq", "7"]);
    }

    #[test]
    fn release_args_match_herdr_cli() {
        assert_eq!(
            release_args("p1", 9),
            [
                "pane",
                "release-agent",
                "p1",
                "--source",
                "jcode",
                "--agent",
                "jcode",
                "--seq",
                "9"
            ]
        );
    }

    #[test]
    fn resume_argv_validation_follows_herdr_rules() {
        let ok =
            |v: &[&str]| valid_resume_argv(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(ok(&["jcode", "--resume", "session_a"]));
        assert!(!ok(&[]));
        assert!(!ok(&["/usr/bin/jcode"]));
        assert!(!ok(&["jcode", "it's"]));
        assert!(!ok(&["jcode", "a\nb"]));
        assert!(!ok(&vec!["x"; 65]));
        assert!(!ok(&["jcode", &"x".repeat(9000)]));
        assert_eq!(resume_argv_for_session("bad'id", |_| None), None);
    }

    #[test]
    fn resume_argv_keeps_ssh_target_and_selfdev() {
        let vars: std::collections::HashMap<&str, &str> = [
            ("JCODE_SSH_REMOTE", "devbox"),
            ("JCODE_SSH_WORKING_DIR", "/srv/repo"),
            (jcode_selfdev_types::CLIENT_SELFDEV_ENV, "1"),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            resume_argv_for_session("session_a", |k| vars.get(k).map(|v| v.to_string())).unwrap(),
            [
                "jcode",
                "--ssh",
                "devbox",
                "--remote-working-dir",
                "/srv/repo",
                "--resume",
                "session_a",
                "self-dev"
            ]
        );
    }

    #[test]
    /// The reporter must keep re-asserting its report even when the agent state
    /// never changes: herdr drops a pane's agent when the agent process dies
    /// without releasing the pane, which is what a server reload causes. A new
    /// client in the same pane therefore has to re-register itself instead of
    /// waiting for the next state change.
    ///
    /// reference: https://herdr.dev/docs/add-herdr-support/ (herdr's safety net
    /// clears the agent and its resume command once the pane is back at its
    /// idle shell prompt, a second or two after the agent died).
    fn unchanged_report_is_reasserted_after_the_heartbeat() {
        use std::sync::atomic::AtomicUsize;

        let env = HerdrEnv {
            bin: "herdr".into(),
            pane_id: "p1".into(),
        };
        let slot = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
        let delivered = Arc::new(Mutex::new(None));
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_worker = Arc::clone(&calls);
        let timing = Timing {
            heartbeat: Duration::from_millis(20),
            reassert_delay: Duration::from_millis(20),
        };
        let worker_env = env.clone();
        let worker_slot = Arc::clone(&slot);
        let worker_delivered = Arc::clone(&delivered);
        let worker = std::thread::spawn(move || {
            worker_loop(
                worker_env,
                worker_slot,
                worker_delivered,
                timing,
                Arc::new(move |_: &HerdrEnv, _: &StateReport| {
                    calls_for_worker.fetch_add(1, Ordering::Relaxed) >= 0
                }),
            )
        });

        // First report: the client registers itself.
        {
            let mut guard = lock(&slot.0);
            guard.pending = Some(Job::Report(StateReport {
                state: AgentState::Idle,
                session_id: Some("session_a".into()),
                resume_argv: Some(vec![
                    "jcode".into(),
                    "--resume".into(),
                    "session_a".into(),
                ]),
            }));
            guard.retry_at = None;
        }

        // The state never changes again, but the agent must keep being reported.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::Relaxed) < 3 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let sent = calls.load(Ordering::Relaxed);
        lock(&slot.0).released = true;
        let _ = worker.join();

        assert!(
            sent >= 3,
            "unchanged report must be re-sent to re-register the agent, got {sent} sends"
        );
    }

    #[test]
    fn seq_is_strictly_increasing() {
        let a = next_seq();
        let b = next_seq();
        let c = next_seq();
        assert!(a < b && b < c);
    }

    /// Fake herdr for `send_report` tests. Each call is logged; any call
    /// carrying the `--` resume separator exits with `resume_exit_code`.
    /// Holds the test lock and resets [`RESUME_UNSUPPORTED`] on drop, so a
    /// failing assertion cannot leak the global flag into other tests.
    #[cfg(unix)]
    struct FakeHerdr {
        dir: std::path::PathBuf,
        env: HerdrEnv,
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    #[cfg(unix)]
    impl FakeHerdr {
        fn new(resume_exit_code: i32) -> Self {
            use std::os::unix::fs::PermissionsExt;
            static SERIAL: Mutex<()> = Mutex::new(());
            let serial = lock(&SERIAL);
            RESUME_UNSUPPORTED.store(false, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "jcode-herdr-fake-{}-{}",
                std::process::id(),
                next_seq()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let bin = dir.join("herdr");
            // The script logs next to itself via `$0` rather than embedding
            // the temp path, so any `TMPDIR` (spaces, quotes) works.
            std::fs::write(
                &bin,
                format!(
                    "#!/bin/sh\necho \"$*\" >> \"$(dirname \"$0\")/calls.log\"\nfor a in \"$@\"; do [ \"$a\" = -- ] && exit {resume_exit_code}; done\nexit 0\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            let env = HerdrEnv {
                bin: bin.display().to_string(),
                pane_id: "p1".into(),
            };
            Self {
                dir,
                env,
                _serial: serial,
            }
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("calls.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    #[cfg(unix)]
    impl Drop for FakeHerdr {
        fn drop(&mut self) {
            RESUME_UNSUPPORTED.store(false, Ordering::Relaxed);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    fn idle_report_with_resume() -> StateReport {
        StateReport {
            state: AgentState::Idle,
            session_id: Some("session_fox_1".into()),
            resume_argv: resume_argv_for_session("session_fox_1", |_| None),
        }
    }

    #[cfg(unix)]
    #[test]
    fn report_falls_back_to_state_only_when_herdr_rejects_resume() {
        // herdr < 0.9.2 treats the `--` resume separator as a usage error.
        let herdr = FakeHerdr::new(USAGE_ERROR_EXIT_CODE);
        send_report(&herdr.env, idle_report_with_resume());
        assert!(RESUME_UNSUPPORTED.load(Ordering::Relaxed));
        send_report(&herdr.env, idle_report_with_resume());

        let calls = herdr.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert!(calls[0].contains(" -- jcode --resume session_fox_1"));
        assert!(!calls[1].contains(" -- "));
        assert!(calls[1].contains("--agent-session-id session_fox_1"));
        // Remembered: later reports skip the doomed first attempt.
        assert!(!calls[2].contains(" -- "));
    }

    #[cfg(unix)]
    #[test]
    fn report_keeps_resume_after_non_usage_failure() {
        // A transient failure (not a usage error) must not disable resume.
        let herdr = FakeHerdr::new(1);
        send_report(&herdr.env, idle_report_with_resume());
        assert!(!RESUME_UNSUPPORTED.load(Ordering::Relaxed));

        let calls = herdr.calls();
        assert_eq!(calls.len(), 1, "no state-only retry expected: {calls:?}");
        assert!(calls[0].contains(" -- jcode --resume session_fox_1"));
    }
}
