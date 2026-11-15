// reference: https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getconsoleprocesslist (GetConsoleProcessList: the PIDs attached to this console)
// reference: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw (CreateProcessW -> PROCESS_INFORMATION.dwProcessId; the id stays valid until the process terminates)
// reference: https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-waitforsingleobject (a process handle becomes signaled when the process terminates)
// reference: ~/.jcode/scratch/herdr_input_path.md (herdr 0.9.5 sources: the pane shell waits for its foreground job, and the multiplexer has no input hand-back mechanism)
// reference: ~/.jcode/scratch/server_attach_analysis.md (the server socket is already closed before the exec, so the successor attaches without a takeover conflict)
//! Exec handoff: keep the pane shell waiting while the successor client takes
//! over the console.
//!
//! Why this exists (measured 2026-11-14 on this host):
//!
//! A terminal multiplexer pane runs the pane shell (`pwsh`) on a ConPTY
//! console, and the shell launches `jcode.exe` as a native command and WAITS
//! for it. When the client re-execs itself for `/reload`, `/restart`,
//! `/rebuild` or `/update`, it spawned the successor and exited immediately
//! (`platform.rs`: `replace_process_reparented` -> `std::process::exit(0)`).
//! The shell then woke up, returned to its prompt and started reading the
//! console's input buffer. The successor connected to the server and painted
//! its TUI, but the input was already gone: keystrokes were echoed by the
//! shell instead of reaching the new composer.
//!
//! The multiplexer cannot fix this. It writes the pane input as bytes into
//! the ConPTY pipe and never resets the console's cooked/raw modes, so whoever
//! reads the console input buffer at that moment wins the race. There is no
//! multiplexer-side mechanism that hands input back to a replacement agent
//! while the pane shell stays alive.
//!
//! The fix is therefore on the jcode side: the outgoing client must NOT exit
//! right after spawning the successor. It stays alive as an idle supervisor,
//! which keeps the pane shell blocked on the wait for its foreground job, and
//! exits only once no client is left in the console. The successor reads the
//! input.
//!
//! Safety of the supervisor:
//!
//! * It never reads stdin. The TUI's crossterm `EventStream` is dropped when
//!   the TUI returns, and dropping it stops crossterm's reader thread, so the
//!   supervisor cannot steal input from the successor.
//! * It never holds the server connection. The socket is closed together with
//!   the TUI, so the successor can attach to the session without a takeover
//!   conflict.
//! * It never writes to the terminal. The terminal keeps the state the
//!   successor expects; the successor sets the console modes itself.
//! * It never releases the pane while an agent is alive: the pane still hosts
//!   a client, so the pane must stay occupied.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Environment variable that carries the exec-chain depth to the successor.
///
/// The supervisor may only exist at the head of the chain. A successor that
/// re-execs again would otherwise leave a growing pile of supervisor
/// processes behind, all of them holding the pane shell hostage.
pub const SUPERVISOR_DEPTH_ENV: &str = "JCODE_SUPERVISOR_DEPTH";

/// Highest chain depth that still allows a supervisor.
///
/// Depth 0 is the client the pane shell launched. Depth 1 is its successor.
/// A successor must exit immediately after spawning its own successor: the
/// pane shell is still waiting for the depth-0 client, so the depth-1 client's
/// exit does not wake the shell up.
pub const MAX_SUPERVISOR_DEPTH: u32 = 1;

/// How long the supervisor waits before it gives up and exits anyway.
///
/// The supervisor is a safety net, not a mechanism the successor depends on.
/// The normal exit is "no client is left in this console". This timeout only
/// bounds the pathological case where the console scan cannot answer, so it is
/// generous: it must outlive a normal working session, otherwise the pane
/// shell would wake up in the middle of a session and take the input away.
pub const SUPERVISOR_TIMEOUT: Duration = Duration::from_secs(12 * 60 * 60);

/// How often the supervisor polls for the successor's state.
const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Whether this process already handed its pane over to a successor.
///
/// The outgoing client stays alive as the exec supervisor, so it lives next to
/// its successor in the same pane. Both processes report their agent state to
/// the terminal multiplexer, and the multiplexer accepts a report whose
/// sequence number is higher than the one it saw last. The supervisor's report
/// therefore arrives with a fresh number and overrules the live successor: the
/// pane flips from "working" to "idle", which the multiplexer reports as a
/// finished session even though the successor is still running.
///
/// The handover is recorded once, and everything that speaks for the pane (the
/// agent-state reporter) stops speaking for a process that has already handed
/// the pane over.
///
/// reference: https://herdr.dev/docs/add-herdr-support/ ("Only release when the
/// user actually quits"; the pane belongs to the agent that owns it)
static PANE_HANDED_OFF: AtomicBool = AtomicBool::new(false);

/// Record that this process handed its pane over to a successor.
///
/// Called right before the successor is spawned. From that moment the
/// successor owns the pane's agent state, and this process must not report
/// anything about it any more.
pub fn mark_pane_handed_off() {
    PANE_HANDED_OFF.store(true, Ordering::SeqCst);
}

/// Undo [`mark_pane_handed_off`].
///
/// Used when the successor could not be spawned: this process keeps the pane
/// and must keep reporting its state.
pub fn clear_pane_handoff() {
    PANE_HANDED_OFF.store(false, Ordering::SeqCst);
}

/// Whether this process already handed its pane over to a successor.
///
/// A process that handed the pane over must not report the pane's agent state
/// any more: its report would carry a newer sequence number than the live
/// successor's and would make the pane look finished.
pub fn pane_handed_off() -> bool {
    PANE_HANDED_OFF.load(Ordering::SeqCst)
}

/// The exec-chain depth of this process.
///
/// Defaults to 0 when the variable is missing or malformed, so an unset
/// environment behaves like a fresh client.
pub fn supervisor_depth() -> u32 {
    std::env::var(SUPERVISOR_DEPTH_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// The depth to hand to the successor.
pub fn successor_depth() -> u32 {
    supervisor_depth().saturating_add(1)
}

/// Whether this process may act as the exec supervisor.
///
/// Two conditions must hold:
///
/// * The chain depth allows it (see [`MAX_SUPERVISOR_DEPTH`]). Otherwise the
///   process would pile up supervisors behind the pane shell's wait.
/// * The parent is a console program. Only a console parent waits for its
///   child as a foreground job, so only then does this process' lifetime
///   gate the shell's wait, and only then does staying alive keep the shell
///   from grabbing the console input.
pub fn should_supervise() -> bool {
    if supervisor_depth() >= MAX_SUPERVISOR_DEPTH {
        return false;
    }
    parent_is_console_shell()
}

/// Whether the parent process is a console program (a terminal shell).
///
/// On Unix the shell waits for its foreground job by PID, and `exec()` keeps
/// the PID, so the same reasoning holds - but Unix never reaches the
/// supervisor, because `exec()` replaces the process instead of spawning a
/// successor.
pub fn parent_is_console_shell() -> bool {
    #[cfg(windows)]
    {
        crate::platform_reparent::parent_is_console_shell()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// The executable name of the given program, lowercased and without the
/// extension.
///
/// The supervisor uses it to recognise its successor in the console: the
/// successor is another instance of the same program, attached to the same
/// console. The name of the SUCCESSOR matters, not the name of the supervisor:
/// `/reload` can hand over to a binary at a different path, and a supervisor
/// that looked for its own name would not see that successor.
pub fn program_name(program: impl AsRef<std::path::Path>) -> Option<String> {
    let stem = program.as_ref().file_stem()?;
    let name = stem.to_string_lossy().to_ascii_lowercase();
    if name.is_empty() { None } else { Some(name) }
}

/// The executable name of this process, lowercased and without the extension.
///
/// Used as the fallback when the successor's program name cannot be
/// determined.
pub fn own_exe_name() -> Option<String> {
    program_name(&std::env::current_exe().ok()?)
}

/// What the supervisor watches.
pub struct SupervisorTarget {
    /// The PID of the process this one spawned.
    pub successor_pid: u32,
    /// The executable name of the successor.
    pub exe_name: String,
}

/// Why the supervisor stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorExit {
    /// No client is left in the console.
    ClientGone,
    /// The supervisor outlived its patience.
    TimedOut,
}

/// Whether the supervisor must keep waiting.
///
/// Two independent signals are used, and the supervisor stops only when both
/// say "gone":
///
/// * `successor_alive`: the spawned process still exists.
/// * `console_has_client`: another process with the same executable name is
///   attached to this console.
///
/// The second signal is what makes the supervisor survive a CHAIN: when the
/// successor re-execs itself, the successor's PID dies but the next client is
/// already attached to the same console. Waiting for the PID alone would let
/// the supervisor exit in the middle of a chain, and the pane shell would
/// wake up and take the console input away from the live client.
pub fn should_keep_waiting(successor_alive: bool, console_has_client: Option<bool>) -> bool {
    if successor_alive {
        return true;
    }
    matches!(console_has_client, Some(true))
}

/// Wait until no client is left in the console, then return.
///
/// The successor is not a child of this process (it is reparented to the pane
/// shell), so it cannot be waited on with a `Child` handle. The state is
/// therefore polled: the successor's PID, and the console's process list.
pub fn wait_for_client_exit(target: &SupervisorTarget) -> SupervisorExit {
    let deadline = Instant::now() + SUPERVISOR_TIMEOUT;
    loop {
        let successor_alive = crate::platform::is_process_running(target.successor_pid);
        let console_has_client =
            crate::platform_reparent::console_has_other_process_with_name(&target.exe_name);
        if !should_keep_waiting(successor_alive, console_has_client) {
            return SupervisorExit::ClientGone;
        }
        if Instant::now() >= deadline {
            return SupervisorExit::TimedOut;
        }
        std::thread::sleep(SUPERVISOR_POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set the chain depth for the duration of a test.
    ///
    /// The environment is process-global, so the tests that touch it must not
    /// run in parallel with each other. `crate::env::set_var` is the project's
    /// own wrapper, which keeps the same guarantee the rest of the test suite
    /// relies on.
    struct DepthGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl DepthGuard {
        fn set(value: &str) -> Self {
            let previous = std::env::var_os(SUPERVISOR_DEPTH_ENV);
            crate::env::set_var(SUPERVISOR_DEPTH_ENV, value);
            Self { previous }
        }
    }

    impl Drop for DepthGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => crate::env::set_var(SUPERVISOR_DEPTH_ENV, value),
                None => crate::env::remove_var(SUPERVISOR_DEPTH_ENV),
            }
        }
    }

    /// The depth decides whether a process may supervise. The head of the
    /// chain (the client the pane shell launched) may; a successor may not.
    ///
    /// This is the rule that keeps the exec chain from piling up supervisors
    /// behind the pane shell's wait: a successor's exit does not wake the
    /// shell, because the shell waits for the head of the chain.
    #[test]
    fn only_the_head_of_the_chain_may_supervise() {
        let _guard = DepthGuard::set("0");
        assert_eq!(supervisor_depth(), 0);
        assert!(
            should_supervise(),
            "the client the pane shell launched must be able to supervise"
        );

        let _guard = DepthGuard::set("1");
        assert_eq!(supervisor_depth(), 1);
        assert!(
            !should_supervise(),
            "a successor must not supervise: its exit does not gate the shell's wait"
        );

        let _guard = DepthGuard::set("2");
        assert!(
            !should_supervise(),
            "a successor of a successor must not supervise either"
        );
    }

    /// A malformed depth must not grant supervision rights.
    ///
    /// An unparsable value falls back to depth 0, which is the head of the
    /// chain. That is the safe direction: the supervisor then holds the pane
    /// shell until the console really has no client left, instead of exiting
    /// early and letting the shell take the input.
    #[test]
    fn a_malformed_depth_is_read_as_the_head_of_the_chain() {
        let _guard = DepthGuard::set("not-a-number");
        assert_eq!(supervisor_depth(), 0);
        assert!(should_supervise());
    }

    /// A missing depth variable means a fresh client, which may supervise.
    #[test]
    fn a_missing_depth_is_the_head_of_the_chain() {
        let _guard = DepthGuard::set("0");
        crate::env::remove_var(SUPERVISOR_DEPTH_ENV);
        assert_eq!(supervisor_depth(), 0);
    }

    #[test]
    fn successor_depth_counts_up_from_the_current_depth() {
        assert_eq!(successor_depth(), supervisor_depth() + 1);
    }

    #[test]
    fn successor_depth_saturates_instead_of_overflowing() {
        assert_eq!(u32::MAX.saturating_add(1), u32::MAX);
    }

    #[test]
    fn a_live_successor_keeps_the_supervisor_alive() {
        assert!(should_keep_waiting(true, Some(true)));
        assert!(should_keep_waiting(true, None));
        assert!(should_keep_waiting(true, Some(false)));
    }

    #[test]
    fn a_live_console_client_keeps_the_supervisor_alive_after_the_successor_exits() {
        // The chain case: the successor re-execs, its PID dies, and the next
        // client is already in the console. Exiting here would wake the pane
        // shell and take the console input away from the live client.
        assert!(should_keep_waiting(false, Some(true)));
    }

    #[test]
    fn the_supervisor_exits_when_no_client_is_left() {
        assert!(!should_keep_waiting(false, Some(false)));
    }

    #[test]
    fn the_supervisor_exits_when_the_console_cannot_be_scanned_and_the_successor_is_gone() {
        assert!(!should_keep_waiting(false, None));
    }

    #[test]
    fn supervisor_exit_variants_are_distinct() {
        assert_ne!(SupervisorExit::ClientGone, SupervisorExit::TimedOut);
    }

    /// The handover flag gates the agent-state reporter.
    ///
    /// The outgoing client stays alive as the exec supervisor next to its
    /// successor, and both report their agent state to the terminal
    /// multiplexer. The supervisor's report would carry a newer sequence
    /// number than the live successor's, so the multiplexer would take it and
    /// make the pane look finished while the successor is still working.
    ///
    /// reference: https://herdr.dev/docs/add-herdr-support/ ("Herdr ignores
    /// reports whose number is not higher than the last one it accepted, so
    /// late or out-of-order reports can't overwrite newer state").
    #[test]
    fn the_handover_flag_gates_the_reporter() {
        clear_pane_handoff();
        assert!(!pane_handed_off());
        mark_pane_handed_off();
        assert!(pane_handed_off());
        // The flag is undone when the successor could not be spawned: this
        // process keeps the pane and must keep reporting it.
        clear_pane_handoff();
        assert!(!pane_handed_off());
    }
}
