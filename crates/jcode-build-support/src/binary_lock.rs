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
use std::time::{Duration, SystemTime};
use std::sync::atomic::AtomicU32;

/// A PROCESS-WIDE sequence number for the quarantine destination names.
///
/// The pid alone is not unique per call: the same daemon quarantines the
/// binary more than once, and the pid does not change between two builds.
/// A per-call attempt index would restart at 0, so the second call would
/// target the first call's name again.
/// A PROCESS-WIDE sequence number for the quarantine destination names.
///
/// The pid alone is not unique per call: the same daemon quarantines the
/// binary more than once, and the pid does not change between two builds.
/// A per-call attempt index would restart at 0, so the second call would
/// target the first call's name again.
static QUARANTINE_SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// The exact format of a quarantine file name, produced by
/// `rename_binary_aside`: `{binary_name}.locked-{pid}-{sequence}`.
///
/// Both the pre-fix form `{binary_name}.locked-{pid}` (no sequence) and the
/// post-fix form (with sequence) are produced over time; the reclamation below
/// matches BOTH.
///
/// MEASURED 2026-11-17: a `QUARANTINE_FORMAT` constant used to sit here with no
/// reader anywhere in the tree while `rename_binary_aside` built the name from
/// its own literal, so the two could drift silently. It was removed; the format
/// lives in this doc comment and in the single producing literal.

/// Keep the most recent N quarantine files; reclaim everything older.
pub const KEEP_RECENT_QUARANTINES: usize = 3;

/// Reclaim a quarantine file whose last-modified time is older than this.
/// A zero value disables the age check.
pub const QUARANTINE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Whether a file name is a quarantine artifact of `binary_name`.
///
/// The rule is EXACT: the name must be the binary's own name, then
/// `.locked-`, then a decimal pid, optionally followed by `-` and a decimal
/// sequence number. Both the pre-fix form (`jcode.exe.locked-42572`) and the
/// post-fix form (`jcode.exe.locked-42572-0`) match; nothing else does.
///
/// MEASURED 2026-11-17 on this host: the same directory also holds
/// `jcode.exe.running-20261115-140102`, `jcode.exe.harbor-stale-20261113-024422`,
/// `jcode.exe.source.bak` and `jcode.exe.source.json`. Those are NOT
/// quarantine artifacts of this mechanism - the `.running-` and
/// `.harbor-stale-` names have no creator in this repository or in the Harbor
/// tooling (grep for `running-` and `harbor-stale` over `crates/` and
/// `~/.jcode/harbor/*.py` returns 0 matches) - and a reclamation that matched
/// them would delete artifacts it does not own.
pub fn is_quarantine_name(binary_name: &str, candidate: &str) -> bool {
    let Some(suffix) = candidate.strip_prefix(binary_name) else {
        return false;
    };
    let Some(rest) = suffix.strip_prefix(".locked-") else {
        return false;
    };
    let mut parts = rest.split('-');
    let Some(pid) = parts.next() else {
        return false;
    };
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match parts.next() {
        None => true,
        Some(sequence) => {
            !sequence.is_empty()
                && sequence.bytes().all(|byte| byte.is_ascii_digit())
                && parts.next().is_none()
        }
    }
}

/// Whether a file is old enough to be reclaimed by the age rule.
fn is_older_than(path: &Path, age: Duration) -> bool {
    if age.is_zero() {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|elapsed| elapsed >= age)
        .unwrap_or(false)
}

/// Reclaim the quarantined copies of `binary_name` in `dir`.
///
/// A quarantined file may still be a RUNNING image - that is exactly WHY it was
/// quarantined: the holder is never killed, the binary is renamed aside and the
/// sessions keep running on the renamed image. On Windows a mapped image cannot
/// be deleted, so the delete fails with ERROR_SHARING_VIOLATION (32). A failed
/// delete therefore means "leave it in place", NEVER an error: this function
/// must never fail a build.
///
/// The rules:
/// - only files whose name matches `is_quarantine_name` are considered;
/// - the most recent `KEEP_RECENT_QUARANTINES` files are kept;
/// - a file older than `QUARANTINE_MAX_AGE` is reclaimed even when it is among
///   the most recent ones;
/// - only regular files are considered - a DIRECTORY named like a quarantine
///   file is skipped;
/// - every failure is swallowed: this is best-effort housekeeping.
///
/// reference: https://doc.rust-lang.org/std/fs/fn/read_dir.html (read
/// 2026-11-17) - "The order in which read_dir returns entries can change
/// between calls. If reproducible ordering is required, the entries should be
/// explicitly sorted." The candidates are therefore sorted by modification time
/// before the keep-window rule is applied.
pub fn reclaim_quarantines_in(dir: &Path, binary_name: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    // Collect the candidates with their modification times first, so the
    // "keep the most recent N" rule is decided on the whole set and not on the
    // order the directory happens to hand out.
    let mut candidates: Vec<(PathBuf, SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_quarantine_name(binary_name, name) {
            continue;
        }
        // A directory named like a quarantine file is not a quarantined BINARY.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Some(modified) = meta.modified().ok() else {
            continue;
        };
        candidates.push((path, modified));
    }

    // Newest first.
    candidates.sort_by(|left, right| right.1.cmp(&left.1));

    let mut reclaimed = Vec::new();
    for (index, (path, _)) in candidates.into_iter().enumerate() {
        let beyond_the_keep_window = index >= KEEP_RECENT_QUARANTINES;
        let too_old = is_older_than(&path, QUARANTINE_MAX_AGE);
        if !beyond_the_keep_window && !too_old {
            continue;
        }
        // A delete that fails means the file is still mapped by a running
        // image. That is expected, not an error.
        if std::fs::remove_file(&path).is_ok() {
            reclaimed.push(path);
        }
    }
    reclaimed
}

/// Reclaim the quarantined copies of the binary at `path`.
///
/// Called from the quarantine path, so it runs on every build that has to move
/// a locked binary aside.
pub fn reclaim_quarantines_for(path: &Path) -> Vec<PathBuf> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let Some(dir) = path.parent() else {
        return Vec::new();
    };
    reclaim_quarantines_in(dir, name)
}

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
    // Reclaim the DEAD quarantined copies BEFORE creating a new one. That way
    // the directory contains no current quarantine file (this one has not been
    // created yet), so this pass can never sweep the very file it is about to
    // create. `prepare_binary_for_overwrite` already does this, but
    // `rename_binary_aside` is also called directly by the deterministic build
    // (crates/jcode-build-support/src/deterministic_build.rs), so the
    // reclamation belongs here too.
    let _reclaimed = reclaim_quarantines_for(path);

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
    // Reclaim the DEAD quarantined copies on EVERY call, whether or not the
    // binary is locked right now. The quarantine is the only thing that ever
    // produces `{name}.locked-*` files, and nothing else removed them, so every
    // self-dev build used to add ~140 MB that stayed on disk forever.
    // MEASURED 2026-11-17 on this host: 7 files, 1027501568 bytes (979.9 MB),
    // and disk C: had 9.3 GB free.
    //
    // The reclaim pass never fails the build: a missing dir, a non-regular
    // entry, or a delete that hits a still-running image is all best-effort.
    let _reclaimed = reclaim_quarantines_for(path);
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

    /// The REAL second-quarantine failure: the destination is already a
    /// RUNNING IMAGE.
    ///
    /// MEASURED 2026-11-16 on this host, the real failure was:
    ///
    /// ```text
    /// rename ok=False win32=5   (ERROR_ACCESS_DENIED)
    /// source still there (quarantine FAILED): True
    /// holder still alive: True
    /// ```
    ///
    /// The destination of the second rename was the file the FIRST quarantine
    /// had moved aside - and that file was a running image, because the
    /// sessions that ran the old binary kept running it. Renaming ONTO a
    /// running image fails with ERROR_ACCESS_DENIED, and
    /// `MOVEFILE_REPLACE_EXISTING` cannot help: a running image cannot be
    /// replaced at all.
    ///
    /// The test below reproduces that EXACT state: a real running exe sits at
    /// the destination the second rename would choose. It fails on the OLD
    /// code (the destination collides) and passes on the new one (the
    /// destination name carries a process-wide sequence).
    ///
    /// The fixture is a REAL running exe: a copy of the system shell, never
    /// the real jcode binary.
    #[cfg(windows)]
    #[test]
    fn a_second_quarantine_survives_a_running_image_at_the_destination() {
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
        std::thread::sleep(std::time::Duration::from_millis(1500));

        // The FIRST quarantine moves the running image aside. The renamed
        // file KEEPS the running image: the handle follows the file.
        let first = rename_binary_aside(&binary).expect("first quarantine");
        assert!(!binary.exists(), "the original path must be free for cargo");
        assert!(first.exists(), "the first quarantine must leave the binary behind");
        assert!(
            holder.try_wait().expect("holder status").is_none(),
            "the process holding the binary must still be running: renaming, never killing"
        );

        // A new build put a fresh binary at the original path.
        std::fs::write(&binary, b"second build").expect("write");

        // The SECOND quarantine must NOT land on the first destination: that
        // destination is a running image and cannot be replaced.
        let second = rename_binary_aside(&binary).expect("second quarantine");
        assert!(
            second != first,
            "the second quarantine must land on a DIFFERENT name, otherwise it collides \
             with the running image the first quarantine left behind"
        );
        assert!(!binary.exists(), "the original path must be free for cargo");
        assert!(second.exists(), "the second quarantine must leave the binary behind");
        assert_eq!(
            std::fs::read(&second).expect("read second"),
            b"second build",
            "the second quarantine must move the NEW binary, not the old one"
        );
        assert!(
            holder.try_wait().expect("holder status").is_none(),
            "the holder must still be running after BOTH quarantines"
        );

        let _ = holder.kill();
        let _ = holder.wait();
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

    // ---- the quarantine reclamation -------------------------------------------------

    /// Write a file with a specific modification time, so the age rule can be
    /// exercised without sleeping.
    ///
    /// reference: https://doc.rust-lang.org/std/fs/struct.File.html#method.set_modified
    /// (read 2026-11-17) - `set_modified` changes the modification time of the
    /// underlying file, and since Rust 1.75.
    fn write_with_mtime(path: &Path, contents: &[u8], age: Duration) {
        std::fs::write(path, contents).expect("write");
        let when = SystemTime::now() - age;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for the mtime change");
        file.set_modified(when).expect("set the modification time");
    }

    #[test]
    fn the_quarantine_pattern_matches_both_formats_and_nothing_else() {
        assert!(is_quarantine_name("jcode.exe", "jcode.exe.locked-42572"));
        assert!(is_quarantine_name("jcode.exe", "jcode.exe.locked-42572-0"));
        assert!(is_quarantine_name("jcode.exe", "jcode.exe.locked-42572-123"));

        // The NEGATIVE CONTROL: the artifacts that share the directory must
        // never match. MEASURED 2026-11-17 on this host.
        for name in [
            "jcode.exe.running-20261115-140102",
            "jcode.exe.harbor-stale-20261113-024422",
            "jcode.exe.source.bak",
            "jcode.exe.source.json",
            "jcode.exe",
            "jcode.exe.locked-",
            "jcode.exe.locked-abc",
            "jcode.exe.locked-42572-",
            "jcode.exe.locked-42572-0-1",
            "other.exe.locked-42572",
        ] {
            assert!(
                !is_quarantine_name("jcode.exe", name),
                "{name} must not be treated as a quarantine artifact"
            );
        }
    }

    #[test]
    fn the_most_recent_quarantines_are_kept_and_the_older_ones_are_reclaimed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();

        // Five quarantine files, newest last.
        for index in 0..5u32 {
            let path = dir.join(format!("jcode.exe.locked-42572-{index}"));
            let age = Duration::from_secs(60 * 60 * (5 - index) as u64);
            write_with_mtime(&path, b"binary", age);
        }
        // A file that is NOT a quarantine artifact and must survive.
        let other = dir.join("jcode.exe.running-20261115-140102");
        std::fs::write(&other, b"other").expect("write");

        let reclaimed = reclaim_quarantines_in(dir, "jcode.exe");

        assert_eq!(
            reclaimed.len(),
            2,
            "only the two OLDEST quarantine files are beyond the keep window; got {reclaimed:?}"
        );
        assert!(
            !dir.join("jcode.exe.locked-42572-0").exists(),
            "the oldest quarantine file must be reclaimed"
        );
        assert!(
            !dir.join("jcode.exe.locked-42572-1").exists(),
            "the second oldest quarantine file must be reclaimed"
        );
        for index in 2..5u32 {
            let path = dir.join(format!("jcode.exe.locked-42572-{index}"));
            assert!(
                path.exists(),
                "the most recent {} quarantine files must be kept",
                KEEP_RECENT_QUARANTINES
            );
        }
        assert!(
            other.exists(),
            "a file that is not a quarantine artifact must NEVER be touched"
        );
    }

    #[test]
    fn a_quarantine_file_older_than_the_age_limit_is_reclaimed_even_when_it_is_recent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();

        let old = dir.join("jcode.exe.locked-42572-0");
        write_with_mtime(&old, b"binary", QUARANTINE_MAX_AGE + Duration::from_secs(3600));

        let reclaimed = reclaim_quarantines_in(dir, "jcode.exe");

        assert_eq!(reclaimed.len(), 1, "the aged file must be reclaimed");
        assert!(!old.exists(), "the aged quarantine file must be gone");
    }

    #[test]
    fn the_pre_fix_quarantine_format_is_reclaimed_too() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();

        // The pre-fix format has no sequence number.
        let legacy = dir.join("jcode.exe.locked-42572");
        write_with_mtime(
            &legacy,
            b"binary",
            QUARANTINE_MAX_AGE + Duration::from_secs(3600),
        );

        let reclaimed = reclaim_quarantines_in(dir, "jcode.exe");

        assert_eq!(reclaimed.len(), 1, "the pre-fix format must be reclaimed too");
        assert!(!legacy.exists());
    }

    #[test]
    fn a_directory_named_like_a_quarantine_file_is_skipped() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();

        let as_dir = dir.join("jcode.exe.locked-999");
        std::fs::create_dir(&as_dir).expect("create dir");

        let reclaimed = reclaim_quarantines_in(dir, "jcode.exe");

        assert!(reclaimed.is_empty(), "a directory must not be reclaimed");
        assert!(
            as_dir.is_dir(),
            "the directory must survive: it is not a quarantined binary"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_running_image_is_left_in_place_and_is_not_an_error() {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        let binary = dir.join("jcode.exe");
        let system_root =
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let shell = std::path::Path::new(&system_root).join("System32").join("cmd.exe");
        std::fs::copy(&shell, &binary).expect("copy the shell as the fixture binary");

        let mut holder = std::process::Command::new(&binary)
            .args(["/c", "ping -n 30 127.0.0.1 > nul"])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn the holder");
        std::thread::sleep(std::time::Duration::from_millis(1500));

        // Quarantine the running image so it lands in the quarantine PATTERN.
        let aside = rename_binary_aside(&binary).expect("quarantine");
        assert!(aside.exists());

        // Fill the keep window with newer plain files, so the running image is
        // the OLDEST candidate and therefore beyond the keep window. The
        // reclamation will try to delete it - and the delete must fail quietly.
        for index in 0..KEEP_RECENT_QUARANTINES {
            let path = dir.join(format!("jcode.exe.locked-1-{index}"));
            std::fs::write(&path, b"binary").expect("write");
        }

        let reclaimed = reclaim_quarantines_in(dir, "jcode.exe");

        assert!(
            reclaimed.is_empty(),
            "a mapped running image cannot be deleted, so nothing is reclaimed: {reclaimed:?}"
        );
        assert!(
            aside.exists(),
            "the running image must be left in place, not force-deleted"
        );
        assert!(
            holder.try_wait().expect("holder status").is_none(),
            "the holder must still be running"
        );

        let _ = holder.kill();
        let _ = holder.wait();
    }

    /// The quarantine path itself must RECLAIM the dead copies, not only the
    /// helper. Without this the reclamation would never run in production.
    #[test]
    fn the_quarantine_path_reclaims_the_dead_copies() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        let binary = dir.join("jcode.exe");

        // Four dead quarantine copies, older than the keep window.
        for index in 0..(KEEP_RECENT_QUARANTINES + 1) {
            let path = dir.join(format!("jcode.exe.locked-42572-{index}"));
            write_with_mtime(
                &path,
                b"binary",
                QUARANTINE_MAX_AGE + Duration::from_secs(3600),
            );
        }

        // A binary that is NOT locked: the quarantine does not run, but the
        // reclamation still does, because it is part of the same path.
        std::fs::write(&binary, b"binary").expect("write");
        let _ = prepare_binary_for_overwrite(&binary).expect("prepare");

        let left = std::fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .filter(|entry| is_quarantine_name("jcode.exe", &entry.file_name().to_string_lossy()))
            .count();
        assert_eq!(
            left, 0,
            "the aged quarantine copies must be reclaimed by the quarantine path"
        );
    }
}
