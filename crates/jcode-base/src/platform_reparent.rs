// reference: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute
// reference: https://github.com/golang/go/blob/master/src/syscall/exec_windows.go (Go stdlib: same attribute)
// reference: https://github.com/canyonroad/agentsh/blob/main/internal/platform/windows/ntdll_windows.go (CreateProcessAsChild)
// reference: ~/.jcode/research/2026-11-06-herdr-agent-loss/worker-d-win32-inherited-parent.md (prior art, live 5/5)
// reference: ~/.jcode/research/2026-11-06-herdr-agent-loss/exp-parent-process-treatment.md (live treatment proof)
//
//! Windows: spawn a replacement process that the process tree records as a
//! child of *our* parent instead of a child of this process.
//!
//! `PROC_THREAD_ATTRIBUTE_PARENT_PROCESS` (0x0002_0000) is the only documented
//! Win32 mechanism that rewrites `th32ParentProcessID` for a new process.
//!
//! Why: a jcode client that re-execs itself must stay a descendant of the pane
//! shell, otherwise the pane looks idle to herdr (it treats a pane with zero
//! descendants as free). On Unix `exec()` keeps the PID and therefore the
//! parent; on Windows `Command::spawn()` records the dying process as parent.
//!
//! Every failure path here is recoverable by the caller: it falls back to a
//! plain `Command::spawn()`.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{GetConsoleProcessList};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcessId, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess, PROC_THREAD_ATTRIBUTE_PARENT_PROCESS,
    PROCESS_CREATE_PROCESS, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW, STARTUPINFOEXW, STARTUPINFOW, UpdateProcThreadAttribute,
};

/// Console programs that may own a terminal pane. A GUI parent (explorer.exe,
/// a terminal emulator) makes a console child fail to initialize
/// (`STATUS_DLL_INIT_FAILED`), so such parents are rejected.
const CONSOLE_PARENTS: &[&str] = &[
    "cmd",
    "powershell",
    "pwsh",
    "sh",
    "bash",
    "dash",
    "ash",
    "busybox",
    "zsh",
    "fish",
    "ksh",
    "mksh",
    "csh",
    "tcsh",
    "elvish",
    "xonsh",
    "nu",
    "yash",
    "mintty",
    "conhost",
    "openconsole",
];

/// Spawn `cmd` so the new process is recorded as a child of our parent.
///
/// Returns the child PID on success. The caller is expected to treat every
/// error as "fall back to a plain spawn".
pub fn spawn_reparented(cmd: &std::process::Command) -> std::io::Result<u32> {
    let program = cmd.get_program();
    if has_batch_extension(program) {
        return Err(std::io::Error::other(
            "batch files cannot be spawned with a parent-process attribute",
        ));
    }

    let parent_pid = reparent_target_pid().ok_or_else(|| {
        std::io::Error::other(
            "no console program was found among our ancestors, so the replacement \
             cannot be spawned under a console parent",
        )
    })?;
    if parent_pid == 0 || parent_pid == unsafe { GetCurrentProcessId() } {
        return Err(std::io::Error::other("parent process id is not usable"));
    }

    let command_line = build_command_line(cmd);
    let environment = build_environment_block(cmd);
    let current_dir = cmd.get_current_dir().map(|dir| dir.to_path_buf());

    unsafe { create_process(parent_pid, command_line, environment, current_dir) }
}

/// The reparent target: the NEAREST CONSOLE PROGRAM among our ancestors.
///
/// Why not simply "our parent": the recorded parent is the pane shell only when
/// the client was started directly by the pane shell. When jcode spawns a
/// relay or exec intermediary, the recorded parent is that intermediary, which
/// is not a console program, and the reparent falls back to a plain spawn - the
/// replacement then leaves the pane's process tree and herdr drops the agent
/// registration.
///
/// Walking up to the nearest console ancestor fixes that case and leaves the
/// common case untouched: when the recorded parent IS the pane shell, the walk
/// stops immediately and the behaviour is exactly what it was.
///
/// Measured on the live host (2026-11-07): both running jcode clients had the
/// pane shell as their recorded parent, and the shared-server daemon had no
/// console ancestor at all - so the daemon's plain-spawn path is unchanged.
///
/// reference: ~/.jcode/research/2026-11-06-herdr-agent-loss/_val6_walkup_reparent_target.md
fn reparent_target_pid() -> Option<u32> {
    let self_pid = unsafe { GetCurrentProcessId() };
    let parents = process_parent_map();
    nearest_console_ancestor(&parents, self_pid, is_console_parent)
}

/// The nearest console ancestor of `self_pid` in the recorded-parent map.
///
/// The walk is pure so that a test can feed a synthetic ancestry: on the live
/// host the recorded parent is often already attached to the console, which
/// makes the walk indistinguishable from "just use the recorded parent".
fn nearest_console_ancestor(
    parents: &std::collections::HashMap<u32, u32>,
    self_pid: u32,
    is_console: impl Fn(u32) -> bool,
) -> Option<u32> {
    /// How far up the chain the walk may go before giving up.
    const MAX_DEPTH: usize = 16;

    let mut candidate = parents.get(&self_pid).copied()?;
    let mut seen = std::collections::HashSet::new();

    for _ in 0..MAX_DEPTH {
        if candidate == 0 || candidate == self_pid {
            return None;
        }
        if !seen.insert(candidate) {
            // A cycle: the chain is broken, so there is no ancestor to find.
            return None;
        }
        if is_console(candidate) {
            return Some(candidate);
        }
        match parents.get(&candidate) {
            Some(next) => candidate = *next,
            None => return None,
        }
    }
    None
}

/// The recorded parent of every process (Toolhelp `th32ParentProcessID`).
///
/// One snapshot for the whole walk: taking a snapshot per level would make the
/// walk cost a full process-table scan for every ancestor.
fn process_parent_map() -> std::collections::HashMap<u32, u32> {
    let mut map = std::collections::HashMap::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return map;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                map.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    map
}

/// A console parent either shares our console or is a known console program.
fn is_console_parent(pid: u32) -> bool {
    if let Some(processes) = console_process_list()
        && processes.contains(&pid)
    {
        return true;
    }
    match process_image_name(pid) {
        Some(name) => CONSOLE_PARENTS.contains(&name.as_str()),
        None => false,
    }
}

/// PIDs attached to the console of this process.
///
/// `GetConsoleProcessList` returns the number of processes attached to the
/// console, NOT the number of PIDs it stored. A buffer that is too small is
/// therefore not an error: the call reports the real total, so the buffer has
/// to grow until the whole list fits. Clamping to the buffer length instead
/// would silently drop PIDs, including the parent this module looks for.
fn console_process_list() -> Option<Vec<u32>> {
    // A console with more than this many processes is not a real terminal
    // pane; the cap keeps a bogus return value from looping forever.
    const MAX_CONSOLE_PROCESSES: usize = 4096;

    let mut capacity = 64usize;
    loop {
        let mut buffer = vec![0u32; capacity];
        let count = unsafe { GetConsoleProcessList(buffer.as_mut_ptr(), buffer.len() as u32) };
        if count == 0 {
            return None;
        }
        let count = count as usize;
        if count <= buffer.len() {
            buffer.truncate(count);
            return Some(buffer);
        }
        if count > MAX_CONSOLE_PROCESSES {
            buffer.truncate(MAX_CONSOLE_PROCESSES);
            return Some(buffer);
        }
        capacity = count.next_power_of_two();
    }
}

/// Lowercase executable name of a process, without the directory part.
fn process_image_name(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size);
        let _ = CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buffer[..size as usize]);
        let name = path
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or_default()
            .trim_end_matches(".exe")
            .to_ascii_lowercase();
        if name.is_empty() { None } else { Some(name) }
    }
}

fn has_batch_extension(program: &OsStr) -> bool {
    let name = program.to_string_lossy().to_ascii_lowercase();
    name.ends_with(".bat") || name.ends_with(".cmd")
}

/// Build the Windows command line the same way `std` does: the program is
/// always quoted, arguments are quoted when they contain a space or a tab.
fn build_command_line(cmd: &std::process::Command) -> Vec<u16> {
    let args: Vec<OsString> = cmd.get_args().map(|arg| arg.to_os_string()).collect();
    command_line_for(cmd.get_program(), &args)
}

/// The command line for a program and its arguments.
///
/// `std` always quotes the program name so `CreateProcess` cannot be confused
/// about which token is the program, and quotes an argument when it contains
/// a space or a tab (an empty argument is always quoted). Inside the quotes,
/// `n` backslashes before a quote become `2n+1`, and `n` backslashes before
/// the closing quote become `2n`.
fn command_line_for(program: &OsStr, args: &[OsString]) -> Vec<u16> {
    let mut line: Vec<u16> = Vec::new();
    line.push(b'"' as u16);
    line.extend(program.encode_wide());
    line.push(b'"' as u16);
    for arg in args {
        line.push(b' ' as u16);
        push_quoted_arg(&mut line, arg);
    }
    line.push(0);
    line
}

fn push_quoted_arg(line: &mut Vec<u16>, arg: &OsStr) {
    let wide: Vec<u16> = arg.encode_wide().collect();
    let needs_quotes = wide.is_empty()
        || wide
            .iter()
            .any(|unit| *unit == b' ' as u16 || *unit == b'\t' as u16);
    if !needs_quotes {
        line.extend(wide);
        return;
    }
    line.push(b'"' as u16);
    let mut backslashes = 0usize;
    for unit in wide {
        if unit == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        if unit == b'"' as u16 {
            // `n` backslashes before an embedded quote become `2n+1`.
            line.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
        } else {
            // Backslashes that are not followed by a quote stay literal.
            line.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
        }
        backslashes = 0;
        line.push(unit);
    }
    // `n` backslashes before the closing quote become `2n`.
    line.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    line.push(b'"' as u16);
}

/// Build the environment block, or `None` to let the child inherit ours.
///
/// `std` passes a null environment when the command did not change it, and a
/// full block otherwise. The explicit mappings are read back from the command
/// because `Command` does not expose the resolved environment.
fn build_environment_block(cmd: &std::process::Command) -> Option<Vec<u16>> {
    let explicit: Vec<(OsString, Option<OsString>)> = cmd
        .get_envs()
        .map(|(key, value)| (key.to_os_string(), value.map(|value| value.to_os_string())))
        .collect();
    // shortcut: `Command::env_clear()` is not observable from stable std
    // (`get_env_clear` is unstable), so a cleared environment would still
    // inherit ours. No jcode exec path uses `env_clear`; upgrade when std
    // stabilizes the getter.
    if explicit.is_empty() {
        return None;
    }

    let inherited: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    Some(environment_block(&explicit, &inherited))
}

/// Build the environment block from the inherited and the explicit mappings.
///
/// Environment keys are case insensitive on Windows, so an explicit "PATH"
/// replaces the inherited "Path" instead of adding a second entry with a
/// different spelling. The winning key keeps its original spelling and the
/// entries are sorted case insensitively, like `std` does.
fn environment_block(
    explicit: &[(OsString, Option<OsString>)],
    inherited: &[(OsString, OsString)],
) -> Vec<u16> {
    // (case insensitive key, original spelling, value)
    let mut entries: Vec<(String, OsString, OsString)> = Vec::new();
    for (key, value) in inherited {
        entries.push((case_insensitive_key(key), key.clone(), value.clone()));
    }
    for (key, value) in explicit {
        let lookup = case_insensitive_key(key);
        entries.retain(|(existing, _, _)| existing != &lookup);
        if let Some(value) = value {
            entries.push((lookup, key.clone(), value.clone()));
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let mut block: Vec<u16> = Vec::new();
    for (_, key, value) in entries {
        if key.as_encoded_bytes().contains(&b'=')
            || key.as_encoded_bytes().contains(&0)
            || value.as_encoded_bytes().contains(&0)
        {
            continue;
        }
        block.extend(key.encode_wide());
        block.push(b'=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// The comparison key for an environment variable name.
///
/// `std` compares with `CompareStringOrdinal(..., bIgnoreCase=TRUE)`, an
/// ordinal comparison rather than a Unicode case fold. Uppercasing the UTF-8
/// bytes matches that for ASCII names, which is what environment variable
/// names are in practice.
fn case_insensitive_key(key: &OsStr) -> String {
    key.to_string_lossy().to_uppercase()
}

/// Create the child process with the requested recorded parent.
///
/// `unsafe` because it calls Win32 process-creation APIs. The safety argument:
/// every pointer handed to the OS points into a live local allocation that
/// outlives the call, and every handle the OS returns is closed on both the
/// success and the failure path.
///
/// reference: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw
/// reference: https://github.com/golang/go/blob/master/src/syscall/exec_windows.go (Go stdlib: same attribute, same unsafe block shape)
unsafe fn create_process(
    parent_pid: u32,
    mut command_line: Vec<u16>,
    environment: Option<Vec<u16>>,
    current_dir: Option<std::path::PathBuf>,
) -> std::io::Result<u32> {
    unsafe {
        let parent = OpenProcess(PROCESS_CREATE_PROCESS, 0, parent_pid);
        if parent.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        let mut size: usize = 0;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
        let mut attribute_buffer = vec![0u8; size];
        let attributes = attribute_buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(attributes, 1, 0, &mut size) == 0 {
            let _ = CloseHandle(parent);
            return Err(std::io::Error::last_os_error());
        }
        // The attribute value is a POINTER to the handle variable: CreateProcessW
        // dereferences it. Passing the raw handle value crashes the child.
        let parent_handle = parent;
        let updated = UpdateProcThreadAttribute(
            attributes,
            0,
            PROC_THREAD_ATTRIBUTE_PARENT_PROCESS as usize,
            &parent_handle as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<HANDLE>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if updated == 0 {
            let error = std::io::Error::last_os_error();
            DeleteProcThreadAttributeList(attributes);
            let _ = CloseHandle(parent);
            return Err(error);
        }

        let mut startup: STARTUPINFOEXW = std::mem::zeroed();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = attributes;
        // The standard handles are deliberately NOT set here.
        //
        // `PROC_THREAD_ATTRIBUTE_PARENT_PROCESS` does not only change the
        // recorded parent: the child's inherited handles are resolved in the
        // RECORDED parent's handle table, not in the creator's. A handle value
        // minted in the creator (a `DuplicateHandle` result) is therefore
        // dangling in the recorded parent: `GetFileType` reports
        // `FILE_TYPE_UNKNOWN`, `GetConsoleMode` fails with
        // `ERROR_INVALID_HANDLE (6)`, and `IsTerminal` returns `false`. The
        // replacement client then dies with "jcode TUI requires an interactive
        // terminal".
        //
        // Leaving the standard handles unset makes the child inherit the
        // recorded parent's standard handles, which for a pane shell are the
        // very console handles the creator had. That is also what `exec()` does
        // on Unix.
        //
        // Measured in a live herdr pane (reparent + raw handles, and reparent
        // without `STARTF_USESTDHANDLES`): `console(0x1f7)` on stdin,
        // `console(0x7)` on stdout, `is_terminal=true`. The broken combination
        // (reparent + creator-minted duplicate) reports
        // `no-console-mode(err=6)`.
        //
        // reference: ~/.jcode/research/2026-11-06-herdr-agent-loss/_worker_console_rootcause.md

        let mut current_dir_wide: Vec<u16> = Vec::new();
        if let Some(dir) = current_dir.as_ref() {
            current_dir_wide.extend(dir.as_os_str().encode_wide());
            current_dir_wide.push(0);
        }
        let current_dir_ptr = if current_dir_wide.is_empty() {
            std::ptr::null()
        } else {
            current_dir_wide.as_ptr()
        };
        let environment_ptr = match environment.as_ref() {
            Some(block) => block.as_ptr() as *const std::ffi::c_void,
            None => std::ptr::null(),
        };

        let mut info: PROCESS_INFORMATION = std::mem::zeroed();
        let created = CreateProcessW(
            std::ptr::null(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            environment_ptr,
            current_dir_ptr,
            &startup.StartupInfo as *const STARTUPINFOW,
            &mut info,
        );

        let error = if created == 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };

        DeleteProcThreadAttributeList(attributes);
        let _ = CloseHandle(parent);
        if created == 0 {
            return Err(error.unwrap_or_else(|| std::io::Error::other("CreateProcessW failed")));
        }
        let pid = info.dwProcessId;
        let _ = CloseHandle(info.hThread);
        let _ = CloseHandle(info.hProcess);
        Ok(pid)
    }
}

#[cfg(test)]
#[path = "platform_reparent_tests.rs"]
mod platform_reparent_tests;
