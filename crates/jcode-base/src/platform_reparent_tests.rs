// reference: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute
// reference: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw
// reference: ~/.jcode/research/2026-11-06-herdr-agent-loss/exp-parent-process-treatment.md (live treatment proof)
// reference: ~/.jcode/research/2026-11-06-herdr-agent-loss/_worker_console_rootcause.md (measured: the child's inherited handles resolve in the recorded parent's table)
//
//! Tests for the parent-process reparenting helper.

use super::*;
use windows_sys::Win32::Storage::FileSystem::GetFileType;
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

/// Kill a process by PID.
///
/// The reparented child outlives its creator by design, so a test that fails
/// to clean it up leaves a stray process behind.
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Recorded parent of a process (Toolhelp `th32ParentProcessID`).
fn recorded_parent(pid: u32) -> Option<u32> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut parent = None;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == pid {
                    parent = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        parent
    }
}

#[test]
fn reparented_child_is_recorded_under_the_requested_parent() {
    // A console helper that outlives the child we create: the child's recorded
    // parent must be the helper, not this test process.
    let mut helper = std::process::Command::new("cmd.exe")
        .args(["/c", "ping -n 6 127.0.0.1 > nul"])
        .spawn()
        .expect("spawn console helper");
    let helper_pid = helper.id();

    let mut command_line: Vec<u16> = "cmd.exe /c ping -n 2 127.0.0.1 > nul"
        .encode_utf16()
        .collect();
    command_line.push(0);

    let child_pid = unsafe { create_process(helper_pid, command_line, None, None) }
        .expect("create reparented child");

    let recorded = recorded_parent(child_pid).expect("read recorded parent");
    assert_eq!(
        recorded, helper_pid,
        "the child must be recorded under the requested parent"
    );
    assert_ne!(
        recorded,
        std::process::id(),
        "the child must not be recorded under the creating process"
    );

    let _ = helper.kill();
    let _ = helper.wait();
}

#[test]
fn batch_programs_are_rejected_before_any_process_is_created() {
    let mut command = std::process::Command::new("some.bat");
    assert!(has_batch_extension(command.get_program()));
    assert!(spawn_reparented(&command).is_err());
}

/// The reparent target must be the nearest CONSOLE ancestor, not the recorded
/// parent.
///
/// Regression: the target was the recorded parent. When the client was started
/// through a jcode relay, the recorded parent is the relay (not a console
/// program), the reparent was rejected, and the replacement fell back to a
/// plain spawn - which drops the client out of the pane's process tree and
/// makes herdr drop the agent registration.
///
/// The live ancestry CANNOT discriminate this on its own: on this host the
/// recorded parent of the test binary is `cargo`, which IS attached to the
/// console, so "the recorded parent" and "the nearest console ancestor" name
/// the same PID and both rules pass. The discriminating shape is therefore
/// built synthetically: a NON-console intermediary between the process and the
/// console ancestor.
///
/// reference: ~/.jcode/research/2026-11-06-herdr-agent-loss/_val6_walkup_reparent_target.md
#[test]
fn reparent_target_is_the_nearest_console_ancestor() {
    /// The process under test.
    const SELF_PID: u32 = 1000;
    /// A NON-console intermediary (the jcode relay in the failing shape).
    const INTERMEDIARY_PID: u32 = 2000;
    /// The console ancestor above the intermediary (the pane shell).
    const CONSOLE_PID: u32 = 3000;

    let mut parents = std::collections::HashMap::new();
    parents.insert(SELF_PID, INTERMEDIARY_PID);
    parents.insert(INTERMEDIARY_PID, CONSOLE_PID);

    // Only the pane shell is a console program.
    let is_console = |pid: u32| pid == CONSOLE_PID;

    let target = nearest_console_ancestor(&parents, SELF_PID, is_console)
        .expect("the walk must find a console ancestor");
    assert_eq!(
        target, CONSOLE_PID,
        "the walk must skip the non-console intermediary and land on the console ancestor"
    );
    assert_ne!(
        target, INTERMEDIARY_PID,
        "the walk must not stop at the recorded parent"
    );

    // The old rule (the recorded parent) must NOT produce the console
    // ancestor: that is exactly the defect.
    let old_rule = parents.get(&SELF_PID).copied().expect("a recorded parent");
    assert_ne!(
        old_rule, target,
        "the recorded parent and the console ancestor must differ, otherwise \
         this test cannot discriminate the two rules"
    );
}

/// The live walk must land on a real console ancestor of this process.
///
/// This complements the synthetic test above: it checks the walk on the real
/// process table, but it cannot discriminate the old rule (see above).
#[test]
fn live_reparent_target_is_a_console_ancestor() {
    let target = reparent_target_pid().expect("the walk must find a console ancestor");
    assert!(
        is_console_parent(target),
        "the reparent target must be a console program (pid {target})"
    );
    assert_ne!(
        target, std::process::id(),
        "the reparent target must not be this process"
    );

    // The walk must land on an ANCESTOR of this process.
    let mut pid = std::process::id();
    let mut hops = 0;
    loop {
        pid = recorded_parent(pid).expect("walk up");
        hops += 1;
        if pid == target {
            break;
        }
        assert!(hops < 32, "the target must be reachable by walking up");
    }
}

/// The reparented child's standard handles must be REAL handles, not dangling
/// values that only existed in the creator's handle table.
///
/// Regression: the implementation used to pass `STARTF_USESTDHANDLES` with
/// handle values produced by `DuplicateHandle` inside the creator. With
/// `PROC_THREAD_ATTRIBUTE_PARENT_PROCESS` the child's inherited handles are
/// resolved in the RECORDED parent's handle table, so those values are
/// dangling there: `GetFileType` reports `FILE_TYPE_UNKNOWN`,
/// `GetConsoleMode` fails with `ERROR_INVALID_HANDLE (6)`, and
/// `std::io::IsTerminal` returns `false`. The replacement jcode client then
/// died with "jcode TUI requires an interactive terminal (stdin/stdout must
/// be a TTY)".
///
/// The child reports the `GetFileType` of its own standard handles into a
/// file, because the point of the test is what the CHILD sees.
///
/// reference: ~/.jcode/research/2026-11-06-herdr-agent-loss/_worker_console_rootcause.md
#[test]
fn reparented_child_gets_valid_standard_handles() {
    const REPORT_ENV: &str = "JCODE_HERDR_REPARENT_STDIO_REPORT";
    const CHILD_ENV: &str = "JCODE_HERDR_REPARENT_STDIO_CHILD";
    /// The full `libtest` path of this test, as `--exact` needs it.
    const CONCRETE_TEST_NAME: &str =
        "platform_reparent::platform_reparent_tests::reparented_child_gets_valid_standard_handles";

    // The child mode: report what this process sees on its standard handles.
    if std::env::var_os(CHILD_ENV).is_some() {
        let mut report = String::new();
        for (name, which) in [
            ("stdin", STD_INPUT_HANDLE),
            ("stdout", STD_OUTPUT_HANDLE),
            ("stderr", STD_ERROR_HANDLE),
        ] {
            let handle = unsafe { GetStdHandle(which) };
            if handle.is_null() {
                report.push_str(&format!("{name}: handle_is_null=true\n"));
                continue;
            }
            let file_type = unsafe { GetFileType(handle) };
            report.push_str(&format!("{name}: handle_is_null=false file_type={file_type}\n"));
        }
        if let Ok(path) = std::env::var(REPORT_ENV) {
            let _ = std::fs::write(path, report);
        }
        return;
    }

    // The recorded parent must hold DIFFERENT standard handles than this
    // process, otherwise the defect cannot be observed.
    //
    // `cmd.exe` spawned with the default stdio inherits OUR standard handles,
    // and their numeric values are then present in the recorded parent's
    // handle table. The broken implementation would hand the child those very
    // values, and the child would resolve them successfully - the defect would
    // be invisible. Redirecting the helper's stdio to the null device gives it
    // its own handles, so a creator-minted value is dangling in the recorded
    // parent and the child observes `FILE_TYPE_UNKNOWN`.
    let mut helper = std::process::Command::new("cmd.exe")
        .args(["/c", "ping -n 8 127.0.0.1 > nul"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn console helper");
    let helper_pid = helper.id();

    let report_path = std::env::temp_dir().join(format!(
        "jcode-herdr-stdio-{}-{}.txt",
        std::process::id(),
        std::time::Instant::now().elapsed().as_nanos()
    ));
    let _ = std::fs::remove_file(&report_path);

    // The child is this very test binary in its child mode.
    //
    // `libtest --exact` needs the FULL test path, not the bare name: with the
    // bare name it selects nothing, the binary exits without running the test,
    // and no report is written.
    let binary = std::env::current_exe().expect("current_exe");
    let mut command_line: Vec<u16> = format!(
        "\"{}\" --exact {} --nocapture",
        binary.display(),
        CONCRETE_TEST_NAME
    )
    .encode_utf16()
    .collect();
    command_line.push(0);

    let child_pid = unsafe {
        create_process(
            helper_pid,
            command_line,
            Some(child_environment(&report_path)),
            None,
        )
    }
    .expect("create reparented child");

    // The child writes its report and exits; give it time to do so.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !report_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let report = std::fs::read_to_string(&report_path).unwrap_or_default();
    let _ = std::fs::remove_file(&report_path);
    let _ = helper.kill();
    let _ = helper.wait();
    kill_pid(child_pid);

    assert!(
        !report.is_empty(),
        "the reparented child wrote no stdio report (child_pid={child_pid})"
    );
    for line in report.lines() {
        if line.contains("handle_is_null=true") {
            // A redirected-away standard handle is not the defect under test.
            continue;
        }
        assert!(
            !line.contains("file_type=0"),
            "the reparented child received a dangling standard handle \
             (file_type=FILE_TYPE_UNKNOWN means the handle value does not exist \
             in the recorded parent's handle table): {line}\n\
             full report:\n{report}"
        );
    }
}

/// The environment block for the child: this process' environment plus the
/// two variables that switch the child into its reporting mode.
fn child_environment(report_path: &std::path::Path) -> Vec<u16> {
    let explicit = vec![
        (
            OsString::from("JCODE_HERDR_REPARENT_STDIO_CHILD"),
            Some(OsString::from("1")),
        ),
        (
            OsString::from("JCODE_HERDR_REPARENT_STDIO_REPORT"),
            Some(OsString::from(report_path.to_string_lossy().to_string())),
        ),
    ];
    let inherited: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    environment_block(&explicit, &inherited)
}

#[test]
/// The console process list must not panic when the console holds more
/// processes than the first buffer.
///
/// Regression: `GetConsoleProcessList` returns the number of processes
/// attached to the console, not the number of PIDs it stored. The first
/// implementation sliced the buffer with that number and panicked with
/// "range end index 271 out of range for slice of length 64" on a console with
/// 271 processes.
fn console_process_list_never_panics_and_contains_this_process() {
    let list = console_process_list();
    if let Some(list) = list {
        assert!(
            list.contains(&std::process::id()),
            "the console list must contain this process ({}), got {} entries",
            std::process::id(),
            list.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Environment block
//
// reference: https://github.com/rust-lang/rust/blob/master/library/std/src/sys/process/windows.rs (make_envp)
// reference: https://github.com/rust-lang/rust/blob/master/library/std/src/sys/process/env.rs (CommandEnv)
// ---------------------------------------------------------------------------

/// Decode an environment block into its "KEY=VALUE" entries.
fn decode_environment_block(block: &[u16]) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current = String::new();
    for unit in block {
        if *unit == 0 {
            if current.is_empty() {
                break;
            }
            entries.push(std::mem::take(&mut current));
            continue;
        }
        current.push(char::from(*unit as u8));
    }
    entries
}

/// A reference implementation of the environment block, written from the
/// documented `std` rule: the keys are case insensitive, the explicit mapping
/// wins, and a removed key disappears. The spelling of the winning key is
/// kept.
fn reference_environment_block(
    explicit: &[(OsString, Option<OsString>)],
    inherited: &[(OsString, OsString)],
) -> Vec<String> {
    // (lookup key, spelling, value)
    let mut entries: Vec<(String, String, String)> = Vec::new();
    for (key, value) in inherited {
        entries.push((
            key.to_string_lossy().to_uppercase(),
            key.to_string_lossy().to_string(),
            value.to_string_lossy().to_string(),
        ));
    }
    for (key, value) in explicit {
        let lookup = key.to_string_lossy().to_uppercase();
        entries.retain(|(existing, _, _)| existing != &lookup);
        if let Some(value) = value {
            entries.push((
                lookup,
                key.to_string_lossy().to_string(),
                value.to_string_lossy().to_string(),
            ));
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
        .into_iter()
        .map(|(_, spelling, value)| format!("{spelling}={value}"))
        .collect()
}

#[test]
fn environment_block_replaces_an_inherited_key_case_insensitively() {
    let inherited = vec![
        (OsString::from("Path"), OsString::from("C:\\old")),
        (OsString::from("HOME"), OsString::from("C:\\home")),
    ];
    let explicit = vec![(OsString::from("PATH"), Some(OsString::from("C:\\new")))];

    let block = environment_block(&explicit, &inherited);
    let entries = decode_environment_block(&block);

    assert_eq!(
        entries,
        vec!["HOME=C:\\home".to_string(), "PATH=C:\\new".to_string()],
        "the explicit PATH must replace Path, not join it"
    );
    assert!(
        !entries.iter().any(|entry| entry.starts_with("Path=")),
        "the inherited spelling must not survive as a second entry"
    );
}

#[test]
fn environment_block_removes_an_inherited_key_case_insensitively() {
    let inherited = vec![
        (OsString::from("SECRET_TOKEN"), OsString::from("value")),
        (OsString::from("KEEP"), OsString::from("yes")),
    ];
    let explicit = vec![(OsString::from("secret_token"), None)];

    let block = environment_block(&explicit, &inherited);
    let entries = decode_environment_block(&block);

    assert_eq!(entries, vec!["KEEP=yes".to_string()]);
}

#[test]
fn environment_block_has_no_duplicate_case_insensitive_keys() {
    let inherited = vec![
        (OsString::from("Path"), OsString::from("C:\\old")),
        (OsString::from("path"), OsString::from("C:\\older")),
        (OsString::from("Other"), OsString::from("x")),
    ];
    let explicit = vec![(OsString::from("PATH"), Some(OsString::from("C:\\new")))];

    let block = environment_block(&explicit, &inherited);
    let entries = decode_environment_block(&block);

    let mut seen: Vec<String> = entries
        .iter()
        .map(|entry| entry.split('=').next().unwrap_or_default().to_uppercase())
        .collect();
    seen.sort();
    let count = seen.len();
    seen.dedup();
    assert_eq!(
        seen.len(),
        count,
        "duplicate case-insensitive keys: {entries:?}"
    );
}

#[test]
fn environment_block_matches_the_reference_implementation() {
    let inherited = vec![
        (OsString::from("Path"), OsString::from("C:\\old")),
        (OsString::from("HOME"), OsString::from("C:\\home")),
        (OsString::from("TEMP"), OsString::from("C:\\temp")),
    ];
    let explicit = vec![
        (OsString::from("PATH"), Some(OsString::from("C:\\new"))),
        (OsString::from("TEMP"), None),
        (OsString::from("EXTRA"), Some(OsString::from("1"))),
    ];

    let block = environment_block(&explicit, &inherited);
    let entries = decode_environment_block(&block);
    assert_eq!(
        entries,
        reference_environment_block(&explicit, &inherited),
        "the block must match the reference implementation"
    );
}

// ---------------------------------------------------------------------------
// Command line
//
// reference: https://github.com/rust-lang/rust/blob/master/library/std/src/sys/args/windows.rs (append_arg)
// ---------------------------------------------------------------------------

/// A reference implementation of the Windows argument quoting rule.
///
/// `std` quotes an argument when it contains a space or a tab (an empty
/// argument is always quoted). Inside the quotes, `n` backslashes before a
/// quote become `2n+1` and `n` backslashes before the closing quote become
/// `2n`.
fn reference_command_line(program: &str, args: &[&str]) -> String {
    let mut line = format!("\"{program}\"");
    for arg in args {
        line.push(' ');
        let needs_quotes = arg.is_empty() || arg.contains(' ') || arg.contains('\t');
        if !needs_quotes {
            line.push_str(arg);
            continue;
        }
        line.push('"');
        let mut backslashes = 0usize;
        for character in arg.chars() {
            if character == '\\' {
                backslashes += 1;
                continue;
            }
            if character == '"' {
                // `n` backslashes before an embedded quote become `2n+1`.
                line.push_str(&"\\".repeat(backslashes * 2 + 1));
            } else {
                // Backslashes that are not followed by a quote stay literal.
                line.push_str(&"\\".repeat(backslashes));
            }
            backslashes = 0;
            line.push(character);
        }
        // `n` backslashes before the closing quote become `2n`.
        line.push_str(&"\\".repeat(backslashes * 2));
        line.push('"');
    }
    line
}

fn decode_command_line(line: &[u16]) -> String {
    let units: Vec<u16> = line.iter().copied().filter(|unit| *unit != 0).collect();
    String::from_utf16_lossy(&units)
}

#[test]
fn command_line_quotes_an_empty_argument() {
    let args = vec![OsString::from("")];
    let line = command_line_for(OsStr::new("tool.exe"), &args);
    assert_eq!(
        decode_command_line(&line),
        "\"tool.exe\" \"\"",
        "an empty argument must be quoted"
    );
    assert_eq!(
        decode_command_line(&line),
        reference_command_line("tool.exe", &[""])
    );
}

#[test]
fn command_line_quotes_an_argument_with_a_space() {
    let args = vec![OsString::from("two words")];
    let line = command_line_for(OsStr::new("tool.exe"), &args);
    assert_eq!(decode_command_line(&line), "\"tool.exe\" \"two words\"");
    assert_eq!(
        decode_command_line(&line),
        reference_command_line("tool.exe", &["two words"])
    );
}

#[test]
fn command_line_escapes_an_embedded_quote() {
    let args = vec![OsString::from("say \"hi\"")];
    let line = command_line_for(OsStr::new("tool.exe"), &args);
    assert_eq!(
        decode_command_line(&line),
        "\"tool.exe\" \"say \\\"hi\\\"\"",
        "an embedded quote must be escaped with a backslash"
    );
    assert_eq!(
        decode_command_line(&line),
        reference_command_line("tool.exe", &["say \"hi\""])
    );
}

#[test]
fn command_line_doubles_trailing_backslashes_before_the_closing_quote() {
    // A quoted argument: the trailing backslashes are doubled so they cannot
    // escape the closing quote.
    let args = vec![OsString::from("C:\\Program Files\\dir\\")];
    let line = command_line_for(OsStr::new("tool.exe"), &args);
    assert_eq!(
        decode_command_line(&line),
        "\"tool.exe\" \"C:\\Program Files\\dir\\\\\"",
        "trailing backslashes must be doubled before the closing quote"
    );
    assert_eq!(
        decode_command_line(&line),
        reference_command_line("tool.exe", &["C:\\Program Files\\dir\\"])
    );
}

#[test]
fn command_line_keeps_an_unquoted_trailing_backslash() {
    // An argument without a space or a tab is not quoted, so its trailing
    // backslash stays literal and never escapes anything.
    let args = vec![OsString::from("C:\\dir\\")];
    let line = command_line_for(OsStr::new("tool.exe"), &args);
    assert_eq!(
        decode_command_line(&line),
        "\"tool.exe\" C:\\dir\\",
        "an unquoted argument must not be quoted or escaped"
    );
    assert_eq!(
        decode_command_line(&line),
        reference_command_line("tool.exe", &["C:\\dir\\"])
    );
}

#[test]
fn command_line_leaves_a_plain_argument_unquoted() {
    let args = vec![OsString::from("plain")];
    let line = command_line_for(OsStr::new("tool.exe"), &args);
    assert_eq!(decode_command_line(&line), "\"tool.exe\" plain");
    assert_eq!(
        decode_command_line(&line),
        reference_command_line("tool.exe", &["plain"])
    );
}
