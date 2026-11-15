// reference: crates/jcode-app-core/src/tool/selfdev/mod.rs:777-818 (read
// 2026-11-15) - the self-dev tool's own build lock: the same lock file path
// (`<jcode home>/selfdev-build-locks/<worktree scope>.lock`), acquired with
// `create_new` on Windows and with `flock` on Unix, and removed on drop. This
// file is that lock, moved into the shared build-support crate so the
// DETERMINISTIC build executor (`jcode server build`) contends with the
// session-driven build instead of racing it.
//
// The one addition over the tool's lock is the STALE break: a session that
// dies mid-build leaves the lock file behind, and the tool's own
// `try_acquire_build_lock` then refuses every later build forever. A lock file
// older than STALE_LOCK_AGE is therefore treated as abandoned.
//
// reference: https://doc.rust-lang.org/std/fs/struct.File.html (read
// 2026-11-15) - `File::set_modified(&self, time: SystemTime)` changes the
// modification time of the underlying file; used by the stale-lock test to age
// a lock file past the stale window.
use anyhow::Result;
use jcode_storage as storage;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// How long a build lock may sit before it is considered abandoned.
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
    pub fn try_acquire_path(path: &std::path::Path) -> Result<Option<Self>> {
        match Self::open_exclusive(path) {
            Ok(()) => Ok(Some(Self {
                path: path.to_path_buf(),
            })),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if !break_stale_lock(path) {
                    return Ok(None);
                }
                match Self::open_exclusive(path) {
                    Ok(()) => Ok(Some(Self {
                        path: path.to_path_buf(),
                    })),
                    Err(_) => Ok(None),
                }
            }
            Err(err) => Err(err.into()),
        }
    }

    #[cfg(unix)]
    fn open_exclusive(path: &std::path::Path) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret == 0 {
            Ok(())
        } else {
            Err(std::io::ErrorKind::AlreadyExists.into())
        }
    }

    #[cfg(not(unix))]
    fn open_exclusive(path: &std::path::Path) -> std::io::Result<()> {
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)?;
        Ok(())
    }
}

impl Drop for BuildLockGuard {
    fn drop(&mut self) {
        // Windows does not allow deleting an open lock file, so the handle is
        // closed before unlinking.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Remove a lock file that has been sitting for longer than STALE_LOCK_AGE.
///
/// Returns whether the lock was broken.
fn break_stale_lock(path: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    if SystemTime::now()
        .duration_since(modified)
        .map(|age| age < STALE_LOCK_AGE)
        .unwrap_or(false)
    {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn set_file_mtime(path: &std::path::Path, time: SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open lock");
        file.set_modified(time).expect("set mtime");
    }
}
