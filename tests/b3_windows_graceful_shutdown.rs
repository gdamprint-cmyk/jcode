// reference: https://learn.microsoft.com/en-us/windows/console/handlerc-routine
//
// B3: on Windows an abrupt termination (herdr closing the pane, console
// window close, logoff) must still restore the terminal modes, exactly like
// the Unix signal path does. The production entry point under test is
// `jcode::cli::terminal::spawn_session_signal_watchers`.
//
// The negative control is behavioural and compiles against the BASE commit:
// the child process installs the production handler, the parent delivers a
// real console control event, and the test asserts on what the handler
// actually restored plus the exit code. On the base `spawn_session_signal_watchers`
// is an empty stub, so the child has NO handler, the default OS action
// terminates it, and the terminal modes are never restored: the assertion on
// the restored VT mouse reset fails.
//
// The Win32 entry points are declared inline instead of taken from
// `windows_sys` on purpose: this file must also compile against the BASE
// commit, where the `Win32_System_Console` feature is not enabled. That is
// what makes the base run fail on an ASSERTION instead of a compile error.

#![cfg(windows)]

use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CTRL_BREAK_EVENT: u32 = 1;
const CHILD_ENV: &str = "JCODE_B3_GRACEFUL_SHUTDOWN_CHILD";
const TEST_NAME: &str = "windows_console_ctrl_event_restores_terminal_modes_before_exit";

unsafe extern "system" {
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
    fn GenerateConsoleCtrlEvent(ctrl_event: u32, process_group_id: u32) -> i32;
    fn GetConsoleWindow() -> *mut core::ffi::c_void;
}

/// The VT mouse-mode reset. jcode arms these modes itself with
/// `sync_windows_vt_mouse_capture(true)`, and crossterm's DisableMouseCapture
/// reports `is_ansi_code_supported() == false` on Windows, so it never writes
/// these bytes. Only jcode's own reset writes them, which makes them the
/// observable proof that the production cleanup ran.
const VT_MOUSE_RESET: &str = "\x1b[?1006l";

/// The exit code a Windows console control handler terminates with
/// (STATUS_CONTROL_C_EXIT).
const EXPECTED_EXIT_CODE: u32 = 0xC000_013A;

/// The child: install the production handler, then wait to be terminated.
///
/// The probe handler is registered AFTER the production handler and returns
/// FALSE, so it observes the event first and then passes it down the handler
/// stack (Windows runs the handlers in reverse registration order). That proves
/// the event was actually DELIVERED in both the base and the fixed run, so a
/// failing assertion is about the restore, not about event delivery.
fn run_child() {
    unsafe extern "system" fn probe_handler(ctrl_type: u32) -> i32 {
        let mut out = std::io::stdout();
        let _ = write!(out, "PROBE_EVENT type={ctrl_type}\n");
        let _ = out.flush();
        // FALSE: let the next handler in the stack run.
        0
    }

    jcode::cli::terminal::spawn_session_signal_watchers();

    unsafe {
        let _ = SetConsoleCtrlHandler(Some(probe_handler), 1);
    }

    let mut out = std::io::stdout();
    let _ = out.write_all(b"READY\n");
    let _ = out.flush();

    // Wait for the console control event; the handler exits the process.
    std::thread::sleep(std::time::Duration::from_secs(30));
    std::process::exit(99);
}

#[test]
fn windows_console_ctrl_event_restores_terminal_modes_before_exit() {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_child();
        return;
    }

    // A console control event needs a real console. Without one the test
    // cannot say anything, and it must not report a false green.
    let console = unsafe { GetConsoleWindow() };
    assert!(
        !console.is_null(),
        "no console attached: a console control event cannot be delivered, so this \
         test has no resolving power on this host"
    );

    let exe = std::env::current_exe().expect("test binary path");
    let mut cmd = Command::new(exe);
    cmd.arg("--exact")
        .arg(TEST_NAME)
        .env(CHILD_ENV, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NEW_PROCESS_GROUP);
    let mut child = cmd.spawn().expect("spawn child");
    let child_pid = child.id();
    let mut stdout = child.stdout.take().expect("child stdout");

    // Wait until the child reports that the handler is installed.
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    while !seen.ends_with(b"READY\n") {
        let n = stdout.read(&mut byte).expect("read child stdout");
        assert_ne!(n, 0, "child exited before installing the handler");
        seen.push(byte[0]);
    }

    // Deliver the event to the child's process group.
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child_pid) };
    assert_ne!(
        sent, 0,
        "GenerateConsoleCtrlEvent failed: the event never reached the child"
    );

    let mut rest = String::new();
    let _ = stdout.read_to_string(&mut rest);
    let status = child.wait().expect("wait child");
    let exit_code = status.code().expect("child exit code");

    // Delivery proof: the probe handler ran in BOTH runs.
    assert!(
        rest.contains("PROBE_EVENT"),
        "the console control event was not delivered to the child: {rest:?}"
    );

    // The behaviour under test: the production cleanup restored the terminal.
    assert!(
        rest.contains(VT_MOUSE_RESET),
        "the production handler did not restore the terminal modes: the child \
         stdout carries no VT mouse reset {VT_MOUSE_RESET:?}: {rest:?}"
    );

    assert_eq!(
        exit_code as u32,
        EXPECTED_EXIT_CODE,
        "unexpected child exit code: {exit_code:#x}"
    );
}
