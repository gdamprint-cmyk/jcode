// reference: crates/jcode-app-core/src/tool/selfdev/mod.rs:783-819 (read
// 2026-11-16) - the self-dev tool's own build lock: the same lock file path
// (`<jcode home>/selfdev-build-locks/<worktree scope>.lock`), acquired with
// `create_new` on Windows and with `flock` on Unix, and removed on drop. This
// file is that lock, moved into the shared build-support crate so the
// DETERMINISTIC build executor (`jcode server build`) contends with the
// session-driven build instead of racing it.
//
// MEASURED 2026-11-16 on this host, three defects in ONE protocol:
//  1. The tool's own acquisition had NO stale break, so a build killed by the
//     reload that a landing performs left its lock file behind and every later
//     build for that scope waited forever. OBSERVED: a lock file aged 136.5
//     minutes with no owning process and a frozen mtime.
//  2. This crate had a stale break, but only by AGE and without recording an
//     owner, so it could not tell an abandoned lock from a live one.
//  3. On Unix the `flock` handle was a local variable, so the flock was
//     released the moment the function returned and the lock excluded
//     NOTHING: the deterministic executor and a session build could compile
//     into the same artifact directory at the same time.
//
// The lock file now records its owner's process id, and on Windows a lock
// whose owner is provably gone is broken instead of wedging the scope. The
// file format is the one the Harbor executor already writes (a bare decimal
// pid), so the two stay interoperable.
//
// reference: ~/.jcode/harbor/harbor_build.py:184-199 (read 2026-11-16) -
// `acquire_lock` creates the same lock file with O_CREAT|O_EXCL and writes
// `str(os.getpid())` into it.
//
// reference: crates/jcode-build-support/src/landing.rs:1341-1362 (read
// 2026-11-16) - the Win32 process-liveness check reused below.
//
// reference: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getexitcodeprocess
// (read 2026-11-16) - "If the process has not terminated and the function
// succeeds, the status returned is STILL_ACTIVE"; the handle must carry
// PROCESS_QUERY_LIMITED_INFORMATION.
//
// reference: https://doc.rust-lang.org/std/fs/struct.File.html (read
// 2026-11-15) - `File::set_modified(&self, time: SystemTime)` changes the
// modification time of the underlying file; used by the stale-lock test to age
// a lock file past the stale window.
use anyhow::Result;
use jcode_storage as storage;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long a build lock may sit before it is considered abandoned when its
/// owner cannot be identified.
///
/// A cold self-dev build of the whole workspace can take a long time, so this
/// is generous on purpose: breaking a LIVE lock would let two cargo builds
/// write the same artifact directory.
const STALE_LOCK_AGE: Duration = Duration::from_secs(6 * 60 * 60);

/// The lock file for a worktree scope.
pub fn build_lock_path(worktree_scope: &str) -> Result<PathBuf> {
    let dir = storage::jcode_dir()?.join("selfdev-build-locks");
    storage::ensure_dir(&dir)?;
    Ok(dir.join(format!("{worktree_scope}.lock")))
}

/// Holds the build lock for one worktree scope. The lock is released when the
/// guard is dropped.
pub struct BuildLockGuard {
    /// The open handle on the lock file.
    ///
    /// It is kept for the guard's lifetime because on Unix it carries the
    /// `flock`, and because on Windows an open file cannot be deleted.
    file: Option<std::fs::File>,
    path: PathBuf,
}

impl BuildLockGuard {
    /// Take the lock for a worktree scope, or return `None` when another build
    /// holds it.
    pub fn try_acquire(worktree_scope: &str) -> Result<Option<Self>> {
        Self::try_acquire_path(&build_lock_path(worktree_scope)?)
    }

    /// Take the lock at an exact path, or return `None` when another build
    /// holds it.
    ///
    /// The path-based form exists so a caller (and a test) can resolve the
    /// lock directory ONCE. The directory depends on `JCODE_HOME`, which
    /// other tests in this crate mutate, so resolving it per call could make
    /// two calls land on different paths.
    pub fn try_acquire_path(path: &Path) -> Result<Option<Self>> {
        match Self::open_exclusive(path) {
            Ok(file) => Ok(Some(Self::guard(path, file))),
            Err(err) => {
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    // Unix: a LIVE owner holds the `flock`. The kernel
                    // releases a `flock` when its owner dies, so a refused
                    // flock never means an abandoned lock and must never be
                    // broken here.
                    return Ok(None);
                }
                if err.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(err.into());
                }
                // Windows: the file's EXISTENCE is the lock, so a leftover
                // file from a killed build would wedge every later build for
                // that scope. The recorded owner decides whether it is
                // abandoned.
                if !break_stale_lock(path) {
                    return Ok(None);
                }
                match Self::open_exclusive(path) {
                    Ok(file) => Ok(Some(Self::guard(path, file))),
                    Err(_) => Ok(None),
                }
            }
        }
    }

    /// Build a guard around an acquired lock file and record the owner.
    fn guard(path: &Path, mut file: std::fs::File) -> Self {
        record_owner(&mut file);
        Self {
            file: Some(file),
            path: path.to_path_buf(),
        }
    }

    #[cfg(unix)]
    fn open_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
        use std::os::fd::AsRawFd;

        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        // SAFETY: `file` owns the descriptor for the duration of the call.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        Ok(file)
    }

    #[cfg(not(unix))]
    fn open_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
    }
}

impl Drop for BuildLockGuard {
    fn drop(&mut self) {
        // Windows does not allow deleting an open lock file, so the handle is
        // closed before unlinking.
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Record the owner's process id in the lock file.
///
/// The id makes the lock's owner identifiable, so a lock left behind by a
/// killed build can be told apart from a live one. The format is the one the
/// Harbor executor already writes, so the two stay interoperable.
///
/// reference: https://doc.rust-lang.org/std/fs/struct.File.html#method.set_len
/// (read 2026-11-16) - `set_len` truncates the file to the given size, so a
/// leftover file with a LONGER previous owner id is cut to this one.
fn record_owner(file: &mut std::fs::File) {
    use std::io::Write;

    let pid = std::process::id().to_string();
    if file.write_all(pid.as_bytes()).is_ok() {
        let _ = file.set_len(pid.len() as u64);
    }
}

/// The process id recorded in a lock file, when there is a readable one.
#[cfg(not(unix))]
fn lock_owner(path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(path).ok()?;
    content.trim().parse::<u32>().ok()
}

/// Remove a lock file whose owner is provably gone.
///
/// Returns whether the lock was broken.
///
/// On Windows the file's existence IS the lock, so this is the only thing
/// that keeps a killed build from wedging its scope forever.
///
/// A LIVE owner's lock is NEVER broken, however old it is. MEASURED 2026-11-16
/// (independent review): the age valve that used to apply to a live owner
/// let the Rust side break a lock the Harbor executor keeps, and breaking a
/// live owner's lock starts a SECOND cargo build into the same artifact
/// directory - the exact harm the lock exists to prevent. The dead-owner check
/// is what closes the real wedge: a build killed by a landing reload has a
/// dead pid, and its lock is broken at once.
///
/// The age rule therefore applies only to a lock with NO readable owner (an
/// empty file, or a lock written before the pid was recorded), where there is
/// no liveness to consult.
///
/// reference: crates/jcode-build-support/src/landing.rs:1341-1362 (read
/// 2026-11-16) - the same Win32 liveness check.
#[cfg(not(unix))]
fn break_stale_lock(path: &Path) -> bool {
    if let Some(pid) = lock_owner(path) {
        if pid == std::process::id() {
            // Our own lock: never break it.
            return false;
        }
        if process_is_alive(pid) {
            // The owner is still running: never break its lock, not even when
            // it has been sitting there for longer than the stale window.
            return false;
        }
        // The owner is provably gone: the lock is abandoned.
        return std::fs::remove_file(path).is_ok();
    }
    // No readable owner (an empty file, or a lock written by an older
    // build): fall back to the age rule.
    if !lock_is_older_than(path, STALE_LOCK_AGE) {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}

/// Whether a lock file has been sitting for at least `age`.
#[cfg(not(unix))]
fn lock_is_older_than(path: &Path, age: Duration) -> bool {
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

/// Whether a process id is still alive.
///
/// On Windows the check uses the Win32 process handle: a handle is opened with
/// PROCESS_QUERY_LIMITED_INFORMATION and queried for the exit code, which is
/// the documented way to tell a live process from a reaped one without racing
/// the PID reuse.
///
/// reference: crates/jcode-build-support/src/landing.rs:1341-1362 (read
/// 2026-11-16) - the same check, for the landing lock.
///
/// reference: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getexitcodeprocess
/// (read 2026-11-16) - "If the process has not terminated and the function
/// succeeds, the status returned is STILL_ACTIVE".
#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: every Win32 call below uses a handle it owns and closes it
    // before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut exit_code) != 0;
        let _ = CloseHandle(handle);
        // `STILL_ACTIVE` is an `NTSTATUS` (i32) in windows-sys, while
        // `GetExitCodeProcess` writes a DWORD. The documented value of a
        // running process is 259 (0x103), so the comparison is done in u32.
        ok && exit_code == STILL_ACTIVE as u32
    }
}

/// Unknown platform: a recorded owner cannot be checked, so the owner is
/// treated as alive and such a lock is only broken by the age rule.
#[cfg(not(windows))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process id the operating system will never assign, so the liveness
    /// check must report it as dead.
    ///
    /// The value exceeds the SIGNED 32-bit range that the Win32 process-id
    /// space uses, so `OpenProcess` on it cannot succeed: no process can carry
    /// this id, and the liveness check reports it as dead.
    #[cfg(windows)]
    const DEAD_OWNER_PID: u32 = u32::MAX - 1;

    /// A live process id that is NOT this process.
    ///
    /// MEASURED 2026-11-16: the "live owner" tests used to write THIS
    /// process's own pid into the lock file. The self-pid guard in
    /// `break_stale_lock` then returned false BEFORE the liveness check was
    /// ever consulted, so those tests passed even when the age valve for a
    /// live owner was restored - they were vacuous for the rule they claimed
    /// to pin. A foreign live pid is what actually exercises the liveness
    /// branch.
    #[cfg(windows)]
    fn spawn_live_foreign_process() -> std::process::Child {
        std::process::Command::new("cmd")
            .args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a live foreign process")
    }

    #[test]
    fn a_lock_is_exclusive_and_released_on_drop() {
        // The lock path is resolved ONCE: the lock directory depends on
        // JCODE_HOME, which other tests in this crate mutate.
        let dir = tempfile::tempdir().expect("temp lock dir");
        let path = dir.path().join("scope.lock");
        let guard = BuildLockGuard::try_acquire_path(&path)
            .expect("acquire")
            .expect("first acquire must win");
        assert!(
            BuildLockGuard::try_acquire_path(&path)
                .expect("acquire")
                .is_none(),
            "a second acquire must not win while the first holds the lock"
        );
        drop(guard);
        assert!(
            BuildLockGuard::try_acquire_path(&path)
                .expect("acquire")
                .is_some(),
            "the lock must be reusable after it is dropped"
        );
    }

    #[test]
    fn an_abandoned_lock_is_broken() {
        let dir = tempfile::tempdir().expect("temp lock dir");
        let path = dir.path().join("scope.lock");
        std::fs::write(&path, b"stale").expect("write stale lock");
        // Age the lock past the stale window.
        let old = SystemTime::now() - STALE_LOCK_AGE - Duration::from_secs(60);
        set_file_mtime(&path, old);
        assert!(
            BuildLockGuard::try_acquire_path(&path)
                .expect("acquire")
                .is_some(),
            "an abandoned lock must not wedge the build forever"
        );
    }

    /// A lock left behind by a killed build carries a dead owner's id. On
    /// Windows the file's existence is the lock, so without the owner check
    /// that leftover file wedges the scope until the age rule expires.
    #[cfg(windows)]
    #[test]
    fn a_lock_whose_owner_is_dead_is_broken() {
        let dir = tempfile::tempdir().expect("temp lock dir");
        let path = dir.path().join("scope.lock");
        std::fs::write(&path, DEAD_OWNER_PID.to_string()).expect("write stale lock");
        assert!(
            BuildLockGuard::try_acquire_path(&path)
                .expect("acquire")
                .is_some(),
            "a lock whose owner is gone must not wedge the build forever"
        );
    }

    /// The negative control for the check above: a lock whose owner is alive
    /// is NEVER broken, however long it has been sitting.
    ///
    /// The owner is a FOREIGN live process, so the self-pid guard cannot be
    /// what keeps the lock.
    #[cfg(windows)]
    #[test]
    fn a_lock_whose_owner_is_alive_is_not_broken() {
        let dir = tempfile::tempdir().expect("temp lock dir");
        let path = dir.path().join("scope.lock");
        let mut owner = spawn_live_foreign_process();
        let owner_pid = owner.id();
        assert!(
            process_is_alive(owner_pid),
            "the spawned owner must really be alive, or the test proves nothing"
        );
        std::fs::write(&path, owner_pid.to_string()).expect("write live lock");
        assert!(
            BuildLockGuard::try_acquire_path(&path)
                .expect("acquire")
                .is_none(),
            "a lock held by a live owner must never be broken"
        );
        let _ = owner.kill();
    }

    /// The negative control for the age rule: a live owner's lock that is
    /// YET younger than the stale window is never broken either.
    ///
    /// MEASURED 2026-11-16 (independent review): this test and
    /// `a_lock_whose_owner_is_alive_is_not_broken` were IDENTICAL - both wrote
    /// a fresh lock with the current pid - so neither distinguished the age
    /// rule from the self-pid guard, and BOTH passed even with the age valve
    /// restored. They also proved nothing about a live owner other than
    /// ourselves. The owner here is a FOREIGN live process, so the liveness
    /// branch is the one under test.
    #[cfg(windows)]
    #[test]
    fn a_young_lock_held_by_a_live_owner_is_not_broken() {
        let dir = tempfile::tempdir().expect("temp lock dir");
        let path = dir.path().join("scope.lock");
        let mut owner = spawn_live_foreign_process();
        let owner_pid = owner.id();
        assert!(
            process_is_alive(owner_pid),
            "the spawned owner must really be alive, or the test proves nothing"
        );
        std::fs::write(&path, owner_pid.to_string()).expect("write live lock");
        assert!(
            BuildLockGuard::try_acquire_path(&path)
                .expect("acquire")
                .is_none(),
            "a fresh lock held by a live owner must never be broken"
        );
        let _ = owner.kill();
    }

    /// The control that the two tests above could not express: a live owner's
    /// lock that is OLDER than the stale window is still never broken.
    ///
    /// MEASURED 2026-11-16 (independent review): the age rule used to apply
    /// to a live owner, so this file WAS broken after 6h - while the Harbor
    /// executor kept it. The two sides disagreed, and breaking a live owner's
    /// lock starts a second cargo build into the same artifact directory.
    /// This test is the control that pins the agreed rule: a live owner keeps
    /// its lock, at any age.
    #[cfg(windows)]
    #[test]
    fn an_aged_lock_held_by_a_live_owner_is_not_broken() {
        let dir = tempfile::tempdir().expect("temp lock dir");
        let path = dir.path().join("scope.lock");
        let mut owner = spawn_live_foreign_process();
        let owner_pid = owner.id();
        assert!(
            process_is_alive(owner_pid),
            "the spawned owner must really be alive, or the test proves nothing"
        );
        std::fs::write(&path, owner_pid.to_string()).expect("write live lock");
        // Age the lock well past the stale window.
        let old = SystemTime::now() - STALE_LOCK_AGE - Duration::from_secs(60);
        set_file_mtime(&path, old);
        assert!(
            BuildLockGuard::try_acquire_path(&path)
                .expect("acquire")
                .is_none(),
            "a lock held by a live owner must never be broken, however old it is"
        );
        let _ = owner.kill();
    }

    /// Our OWN lock is never broken, even when it is older than the stale
    /// window.
    ///
    /// MEASURED 2026-11-16: the self-pid guard is what makes this true. With
    /// the age valve in front of the liveness check, an old lock of our own
    /// was broken, and the process that owns it does not know it lost the
    /// lock.
    #[cfg(windows)]
    #[test]
    fn our_own_aged_lock_is_not_broken() {
        let dir = tempfile::tempdir().expect("temp lock dir");
        let path = dir.path().join("scope.lock");
        std::fs::write(&path, std::process::id().to_string()).expect("write own lock");
        let old = SystemTime::now() - STALE_LOCK_AGE - Duration::from_secs(60);
        set_file_mtime(&path, old);
        assert!(
            BuildLockGuard::try_acquire_path(&path)
                .expect("acquire")
                .is_none(),
            "our own lock must never be broken, however old it is"
        );
    }

    /// The lock file records its owner, so an abandoned lock can be told
    /// apart from a live one. The format is the one the Harbor executor
    /// writes, so the two stay interoperable.
    #[test]
    fn the_lock_records_its_owner() {
        let dir = tempfile::tempdir().expect("temp lock dir");
        let path = dir.path().join("scope.lock");
        let guard = BuildLockGuard::try_acquire_path(&path)
            .expect("acquire")
            .expect("first acquire must win");
        let recorded = std::fs::read_to_string(&path).expect("read the lock file");
        assert_eq!(
            recorded.trim(),
            std::process::id().to_string(),
            "the lock file must name its owner"
        );
        drop(guard);
    }

    /// A lock whose owner is gone is broken IMMEDIATELY, without waiting for
    /// the stale window. MEASURED 2026-11-16: the real wedge was a lock file
    /// aged 136.5 minutes whose owner was provably dead; the age rule alone
    /// would have kept it for 6 hours.
    #[cfg(windows)]
    #[test]
    fn a_dead_owners_lock_is_broken_before_the_stale_window() {
        let dir = tempfile::tempdir().expect("temp lock dir");
        let path = dir.path().join("scope.lock");
        std::fs::write(&path, DEAD_OWNER_PID.to_string()).expect("write dead lock");
        // The lock is FRESH: only the dead owner can justify breaking it.
        assert!(
            BuildLockGuard::try_acquire_path(&path)
                .expect("acquire")
                .is_some(),
            "a fresh lock whose owner is gone must still be broken"
        );
    }

    fn set_file_mtime(path: &std::path::Path, time: SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open lock");
        file.set_modified(time).expect("set mtime");
    }
}
