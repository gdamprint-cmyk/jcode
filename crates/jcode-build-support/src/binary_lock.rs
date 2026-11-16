// reference: no prior art, searched the jcode workspace for an existing
// "locked target binary" handler - there is none. The behaviour below was
// MEASURED on this host on 2026-11-15: a live `jcode.exe self-dev --resume`
// session holds a Windows handle on `target/selfdev/jcode.exe`, so cargo's
// `remove_file` fails with "Отказано в доступе. (os error 5)" (access
// denied) and the build dies with exit 101.
//
// The measured bypass: Windows ALLOWS RENAMING a file that has an open
// handle. Renaming the locked binary aside lets cargo write a fresh one, and
// the running sessions stay alive (their handle follows the renamed file).
// The holder is NEVER killed - it is a real live session.
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU32;

/// A PROCESS-WIDE sequence number for the quarantine destination names.
///
/// The pid alone is not unique per call: the same daemon quarantines the
/// binary more than once, and the pid does not change between two builds.
/// A per-call attempt index would restart at 0, so the second call would
/// target the first call's name again.
static QUARANTINE_SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// Whether the file at `path` cannot be opened for writing because another
/// process holds it.
///
/// On Windows a running executable denies write access, so a failed write-open
/// is the lock signal. On Unix an executable can be written while it runs, so
/// there is nothing to handle.
///
/// MEASURED 2026-11-16 on this host with a REAL running exe (a copy of
/// `cmd.exe`, compiled and run as a Rust program - not inferred):
///
/// ```text
/// write-open FAILED
///   kind(): Uncategorized
///   raw_os_error(): Some(32)
///   display: ... (os error 32)
/// ```
///
/// The raw code is 32 = ERROR_SHARING_VIOLATION, and the Rust `ErrorKind` is
/// `Uncategorized` - NOT `PermissionDenied`. The previous check accepted only
/// `PermissionDenied` or raw code 5, so it reported "not locked" for a binary
/// that a live session was holding, and the quarantine never ran. The
/// `Uncategorized` kind is the reason: Rust's Windows error mapping does not
/// give sharing violations a dedicated kind.
///
/// reference: https://learn.microsoft.com/en-us/windows/win32/debug/system-error-codes--0-499
/// - ERROR_SHARING_VIOLATION (32): "The process cannot access the file
/// because it is being used by another process."
/// reference: https://doc.rust-lang.org/std/io/enum.ErrorKind.html - the
/// error kinds and their Windows mapping.
pub fn is_locked(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => false,
        Err(err) => {
            err.kind() == std::io::ErrorKind::PermissionDenied
                // ERROR_ACCESS_DENIED (5): the file itself is not writable.
                || err.raw_os_error() == Some(5)
                // ERROR_SHARING_VIOLATION (32): another process holds it open
                // without FILE_SHARE_WRITE. This is the code Windows returns
                // for a RUNNING IMAGE, and it is what cargo's `remove_file`
                // hits. MEASURED, see the doc comment above.
                || err.raw_os_error() == Some(32)
        }
    }
}

/// Move a locked binary aside so the compiler can write a fresh one.
///
/// The name keeps the original stem, so a human can tell what it was, plus
/// the pid of the process that moved it, plus a COUNTER that makes the name
/// unique per call.
///
/// MEASURED 2026-11-16 on this host: with only `{name}.locked-{pid}` the
/// quarantine worked EXACTLY ONCE per daemon. The pid is the daemon's, and it
/// does not change between two builds, so the second quarantine targeted a
/// destination that ALREADY EXISTED - and that destination was itself a
/// running image, because the sessions that were running the old binary kept
/// running it. The replacement rename then failed:
///
/// ```text
/// rename ok=False win32=5   (ERROR_ACCESS_DENIED)
/// source still there (quarantine FAILED): True
/// holder still alive: True
/// ```
///
/// `MOVEFILE_REPLACE_EXISTING` cannot save it: the flag "replaces its
/// contents ... provided that security requirements regarding access control
/// lists (ACLs) are met", and a destination that is a RUNNING IMAGE cannot be
/// replaced at all.
///
/// The counter is a PROCESS-WIDE sequence number, not a per-call attempt
/// index: a per-call index would restart at 0 on the second call and would
/// therefore target the first call's name again. The process-wide counter
/// makes every quarantine land on a fresh name, so a daemon can quarantine as
/// many times as it needs.
///
/// reference: https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw
/// - MOVEFILE_REPLACE_EXISTING and the ACL caveat.
/// reference: https://doc.rust-lang.org/std/fs/fn.rename.html - Rust's
/// `fs::rename` on Windows is MoveFileExW.
pub fn rename_binary_aside(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid binary path: {:?}", path))?;
    let mut last_error = None;
    // The counter is PROCESS-WIDE: the pid alone is not unique per call,
    // because the same daemon quarantines more than once.
    for _ in 0..64u32 {
        let sequence = QUARANTINE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let aside = path.with_file_name(format!("{name}.locked-{}-{sequence}", std::process::id()));
        match std::fs::rename(path, &aside) {
            Ok(()) => return Ok(aside),
            Err(err) => {
                // The SOURCE is gone: a concurrent rename already moved it.
                if !path.exists() {
                    return Ok(aside);
                }
                last_error = Some((aside, err));
            }
        }
    }
    let (aside, err) = last_error.expect("at least one attempt");
    Err(anyhow::anyhow!(
        "Failed to move the locked binary {:?} aside to {:?}: {err}",
        path,
        aside
    ))
}

/// Make `path` writable by the compiler, renaming it aside when it is locked.
///
/// Returns the path the binary was moved to, or `None` when nothing had to be
/// moved. The holder is never killed: renaming is the only operation used.
pub fn prepare_binary_for_overwrite(path: &Path) -> Result<Option<PathBuf>> {
    if !is_locked(path) {
        return Ok(None);
    }
    rename_binary_aside(path).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlocked_binary_is_left_alone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("jcode.exe");
        std::fs::write(&binary, b"binary").expect("write");

        assert!(!is_locked(&binary), "a plain file is not locked");
        assert_eq!(
            prepare_binary_for_overwrite(&binary).expect("prepare"),
            None,
            "an unlocked binary must not be renamed"
        );
        assert!(binary.exists());
    }

    #[test]
    fn a_locked_binary_is_renamed_aside_not_removed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("jcode.exe");
        std::fs::write(&binary, b"binary").expect("write");

        let aside = rename_binary_aside(&binary).expect("rename");
        assert!(!binary.exists(), "the original path must be free for cargo");
        assert!(aside.exists(), "the binary must survive, renamed aside");
        assert_eq!(
            std::fs::read(&aside).expect("read aside"),
            b"binary",
            "renaming must not change the bytes"
        );
    }

    /// A RUNNING image must be detected as locked.
    ///
    /// MEASURED 2026-11-16 on this host: a write-open on a running image
    /// fails with raw os error 32 (ERROR_SHARING_VIOLATION) and the Rust
    /// `ErrorKind` is `Uncategorized` - NOT `PermissionDenied`. The detector
    /// used to accept only `PermissionDenied` or raw code 5, so it reported
    /// "not locked" for a binary a live session was holding, and the quarantine
    /// never ran. That is why `cargo` could not replace the binary.
    ///
    /// The fixture is a REAL running exe: a copy of the system shell, never
    /// the real jcode binary.
    ///
    /// reference: ~/.jcode/harbor/_probe_running_exe.py - the measured probe
    /// that established that a RUNNING exe cannot be opened for write but CAN
    /// be renamed.
    /// reference: https://learn.microsoft.com/en-us/windows/win32/debug/system-error-codes--0-499
    /// - ERROR_SHARING_VIOLATION (32).
    #[cfg(windows)]
    #[test]
    fn a_running_image_is_detected_as_locked() {
        use std::os::windows::process::CommandExt;

        // CREATE_NO_WINDOW so the fixture does not flash a console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("jcode.exe");
        let system_root =
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let shell = std::path::Path::new(&system_root).join("System32").join("cmd.exe");
        std::fs::copy(&shell, &binary).expect("copy the shell as the fixture binary");

        let mut holder = std::process::Command::new(&binary)
            .args(["/c", "ping -n 30 127.0.0.1 > nul"])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn the holder");
        // Give the holder time to map its image.
        std::thread::sleep(std::time::Duration::from_millis(1500));

        assert!(
            is_locked(&binary),
            "a RUNNING image must be detected as locked, otherwise the quarantine never runs"
        );
        let aside = prepare_binary_for_overwrite(&binary)
            .expect("prepare")
            .expect("a locked binary must be moved aside");
        assert!(!binary.exists(), "the original path must be free for cargo");
        assert!(aside.exists(), "the binary must survive, renamed aside");
        assert!(
            holder.try_wait().expect("holder status").is_none(),
            "the process holding the binary must still be running: renaming, never killing"
        );

        let _ = holder.kill();
        let _ = holder.wait();
    }

    /// The SECOND quarantine must not collide with the first one.
    ///
    /// MEASURED 2026-11-16: with the destination named `{name}.locked-{pid}`
    /// the quarantine worked EXACTLY ONCE per daemon, because the pid does not
    /// change between two builds. The second rename targeted a destination
    /// that already existed - and, in the real case, was itself a running
    /// image - and failed with ERROR_ACCESS_DENIED (win32 5), so every later
    /// build died with the os error 5 again.
    ///
    /// This test reproduces the collision WITHOUT a running image: the first
    /// quarantine leaves a file behind at the destination, so the second one
    /// would target an occupied name.
    #[test]
    fn a_second_quarantine_does_not_collide_with_the_first() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("jcode.exe");

        std::fs::write(&binary, b"first").expect("write");
        let first = rename_binary_aside(&binary).expect("first quarantine");
        assert!(first.exists(), "the first quarantine must leave the binary behind");

        // A new build put a fresh binary at the original path.
        std::fs::write(&binary, b"second").expect("write");
        let second = rename_binary_aside(&binary).expect("second quarantine");
        assert!(
            second != first,
            "the second quarantine must land on a DIFFERENT name, otherwise it collides"
        );
        assert!(second.exists(), "the second quarantine must leave the binary behind");
        assert_eq!(
            std::fs::read(&second).expect("read second"),
            b"second",
            "the second quarantine must move the NEW binary, not the old one"
        );
        assert_eq!(
            std::fs::read(&first).expect("read first"),
            b"first",
            "the first quarantined binary must survive untouched"
        );
    }

    /// The NEGATIVE CONTROL for the test above: an UNLOCKED binary must NOT
    /// be reported as locked. Without it, a detector that always returns true
    /// would make the locked test pass vacuously.
    #[cfg(windows)]
    #[test]
    fn an_unlocked_binary_is_not_reported_as_locked() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("jcode.exe");
        std::fs::write(&binary, b"binary").expect("write");
        assert!(
            !is_locked(&binary),
            "a plain file must not be reported as locked"
        );
    }

    #[test]
    fn a_missing_binary_is_not_a_lock() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("jcode.exe");
        assert!(!is_locked(&binary));
        assert_eq!(prepare_binary_for_overwrite(&binary).expect("prepare"), None);
    }
}
