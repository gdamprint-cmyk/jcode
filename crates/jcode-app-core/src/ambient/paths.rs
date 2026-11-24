use anyhow::Result;
use std::path::PathBuf;

use crate::storage;

// ---------------------------------------------------------------------------
// Storage paths
// ---------------------------------------------------------------------------

pub(super) fn ambient_dir() -> Result<PathBuf> {
    let dir = storage::jcode_dir()?.join("ambient");
    storage::ensure_dir(&dir)?;
    Ok(dir)
}

pub(super) fn state_path() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(path) = test_state_path_override() {
        if let Some(parent) = path.parent() {
            storage::ensure_dir(parent)?;
        }
        return Ok(path);
    }
    Ok(ambient_dir()?.join("state.json"))
}

// ---------------------------------------------------------------------------
// Test-only state path override
// ---------------------------------------------------------------------------

/// A thread-local override of the `state.json` path.
///
/// `JCODE_HOME` is process-global, so tests that redirect it cannot isolate
/// themselves from each other: a background task spawned by one test still
/// resolves `state_path()` against whatever `JCODE_HOME` is current when it
/// saves. A thread-local override lets a test own its state file regardless of
/// what other threads do to the environment.
#[cfg(test)]
std::thread_local! {
    static STATE_PATH_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// The thread-local `state.json` path override, if any.
#[cfg(test)]
pub(super) fn test_state_path_override() -> Option<PathBuf> {
    STATE_PATH_OVERRIDE.with(|slot| slot.borrow().clone())
}

/// Redirect this thread's `state.json` to `path` until the guard is dropped.
#[cfg(test)]
pub(super) struct StatePathGuard(Option<PathBuf>);

#[cfg(test)]
impl StatePathGuard {
    pub(super) fn set(path: PathBuf) -> Self {
        STATE_PATH_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(path.clone()));
        Self(Some(path))
    }
}

#[cfg(test)]
impl Drop for StatePathGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        STATE_PATH_OVERRIDE.with(|slot| *slot.borrow_mut() = previous);
    }
}

pub(super) fn queue_path() -> Result<PathBuf> {
    Ok(ambient_dir()?.join("queue.json"))
}

pub(super) fn lock_path() -> Result<PathBuf> {
    Ok(ambient_dir()?.join("ambient.lock"))
}

pub(super) fn transcripts_dir() -> Result<PathBuf> {
    let dir = ambient_dir()?.join("transcripts");
    storage::ensure_dir(&dir)?;
    Ok(dir)
}
