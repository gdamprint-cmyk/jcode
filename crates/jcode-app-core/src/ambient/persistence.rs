use anyhow::Result;
use chrono::Utc;
use std::path::PathBuf;

use super::paths::{lock_path, state_path};
use super::{AmbientCycleResult, AmbientState, AmbientStatus, CycleStatus, ScheduledItem};
use crate::storage;

#[cfg(test)]
#[path = "queue_transaction_tests.rs"]
mod queue_transaction_tests;

// `total_cycles` values written by `AmbientState::save()` on this thread.
//
// Test-only instrument for the single-writer invariant of the ambient state:
// "exactly one incrementing `state.json` write per completed cycle, and the
// written value never exceeds the number of completed cycles". The file
// contents alone cannot show this, because the previous value is preserved
// as `state.bak` and the intermediate values are gone.
//
// Thread-local, so concurrently running tests cannot pollute the log.
#[cfg(test)]
std::thread_local! {
    static STATE_WRITES: std::cell::RefCell<Vec<StateWrite>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// One recorded `state.json` write.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct StateWrite {
    /// The resolved `state.json` path the write went to.
    pub path: String,
    /// The `total_cycles` value that was written.
    pub total_cycles: u64,
    /// The writer that performed the save.
    pub writer: &'static str,
}

/// All `state.json` writes performed on this thread for a specific path,
/// regardless of which writer performed them.
///
/// The write log is thread-local, and cargo REUSES worker threads across
/// tests, so a write recorded by an earlier test on the same thread would
/// otherwise show up in a later test's assertions. Conversely, `JCODE_HOME` is
/// process-global, so a background task leaked by another test can resolve
/// `state_path()` against the `JCODE_HOME` that is current at ITS save time —
/// which may be this test's temp home.
///
/// Filtering by the target path scopes the instrument to the writes of one
/// state file. There is deliberately NO per-writer variant: a label-filtered
/// view is blind to any writer that uses the plain `AmbientState::save()`
/// (logged as `"unlabelled"`), which is exactly how the pre-fix defect hid
/// from the tests (measured 2026-11-05, report
/// `revalidate-ambient-counter-fix.md` §5, control experiment C4).
#[cfg(test)]
pub(crate) fn state_writes_all_for(path: &std::path::Path) -> Vec<StateWrite> {
    let wanted = path.display().to_string();
    STATE_WRITES.with(|writes| {
        writes
            .borrow()
            .iter()
            .filter(|write| write.path == wanted)
            .cloned()
            .collect()
    })
}

/// Forget the recorded `state.json` writes on this thread.
#[cfg(test)]
pub(crate) fn reset_state_writes() {
    STATE_WRITES.with(|writes| writes.borrow_mut().clear());
}

// ---------------------------------------------------------------------------
// AmbientState persistence
// ---------------------------------------------------------------------------

impl AmbientState {
    pub fn load() -> Result<Self> {
        let path = state_path()?;
        if path.exists() {
            storage::read_json(&path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = state_path()?;
        #[cfg(test)]
        STATE_WRITES.with(|writes| {
            writes.borrow_mut().push(StateWrite {
                path: path.display().to_string(),
                total_cycles: self.total_cycles,
                writer: "unlabelled",
            })
        });
        storage::write_json(&path, self)
    }

    /// Persist the state, recording `writer` as the author of the write.
    ///
    /// The label is test-only instrumentation: it makes the write log
    /// attributable to a specific writer, so an assertion about one writer
    /// cannot be confused with a concurrent write from another.
    #[cfg(test)]
    pub(crate) fn save_as(&self, writer: &'static str) -> Result<()> {
        let path = state_path()?;
        STATE_WRITES.with(|writes| {
            writes.borrow_mut().push(StateWrite {
                path: path.display().to_string(),
                total_cycles: self.total_cycles,
                writer,
            })
        });
        storage::write_json(&path, self)
    }

    pub fn record_cycle(&mut self, result: &AmbientCycleResult) {
        self.last_run = Some(result.ended_at);
        self.last_summary = Some(result.summary.clone());
        self.last_compactions = Some(result.compactions);
        self.last_memories_modified = Some(result.memories_modified);
        self.total_cycles += 1;

        match result.status {
            CycleStatus::Complete => {
                if let Some(ref req) = result.next_schedule {
                    let next = req.wake_at.unwrap_or_else(|| {
                        Utc::now()
                            + chrono::Duration::minutes(req.wake_in_minutes.unwrap_or(30) as i64)
                    });
                    self.status = AmbientStatus::Scheduled { next_wake: next };
                } else {
                    self.status = AmbientStatus::Idle;
                }
            }
            CycleStatus::Interrupted | CycleStatus::Incomplete => {
                self.status = AmbientStatus::Idle;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ScheduledQueue
// ---------------------------------------------------------------------------

pub struct ScheduledQueue {
    items: Vec<ScheduledItem>,
    path: PathBuf,
}

impl ScheduledQueue {
    pub fn load(path: PathBuf) -> Self {
        let items: Vec<ScheduledItem> = if path.exists() {
            storage::read_json(&path).unwrap_or_default()
        } else {
            Vec::new()
        };
        Self { items, path }
    }

    /// Lock a separate stable inode: atomic JSON replacement must not replace
    /// the inode carrying the lock. Reload under lock, and publish only after
    /// the entire mutation was durably saved. Never write a cached snapshot.
    pub(super) fn transact<T>(
        &mut self,
        update: impl FnOnce(&mut Vec<ScheduledItem>) -> Result<T>,
    ) -> Result<T> {
        if let Some(parent) = self.path.parent() {
            storage::ensure_dir(parent)?;
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.path.with_extension("json.lock"))?;
        lock.lock()?;
        let mut latest = if self.path.exists() {
            storage::read_json::<Vec<ScheduledItem>>(&self.path)?
        } else {
            Vec::new()
        };
        let before = latest.clone();
        let result = update(&mut latest)?;
        if latest != before {
            storage::write_json(&self.path, &latest)?;
        }
        self.items = latest;
        Ok(result)
    }

    pub fn try_push(&mut self, item: ScheduledItem) -> Result<()> {
        self.transact(|items| {
            items.push(item);
            Ok(())
        })
    }

    pub fn push(&mut self, item: ScheduledItem) {
        if let Err(error) = self.try_push(item) {
            crate::logging::warn(&format!("Failed to persist scheduled item: {error:#}"));
        }
    }

    pub fn remove_by_id(&mut self, id: &str) -> Result<Option<ScheduledItem>> {
        self.transact(|items| {
            Ok(items
                .iter()
                .position(|item| item.id == id)
                .map(|index| items.remove(index)))
        })
    }

    fn take_ready(&mut self, direct_only: bool) -> Result<Vec<ScheduledItem>> {
        self.transact(|items| {
            let now = Utc::now();
            let (mut ready, remaining): (Vec<_>, Vec<_>) =
                std::mem::take(items).into_iter().partition(|item| {
                    item.scheduled_for <= now && (!direct_only || item.target.is_direct_delivery())
                });
            *items = remaining;
            ready.sort_by(|a, b| {
                b.priority
                    .cmp(&a.priority)
                    .then_with(|| a.scheduled_for.cmp(&b.scheduled_for))
            });
            Ok(ready)
        })
    }

    pub fn pop_ready(&mut self) -> Vec<ScheduledItem> {
        self.take_ready(false).unwrap_or_else(|error| {
            crate::logging::warn(&format!("Failed to claim scheduled items: {error:#}"));
            Vec::new()
        })
    }

    pub fn take_ready_direct_items(&mut self) -> Vec<ScheduledItem> {
        self.take_ready(true).unwrap_or_else(|error| {
            crate::logging::warn(&format!(
                "Failed to claim direct scheduled items: {error:#}"
            ));
            Vec::new()
        })
    }

    pub fn peek_next(&self) -> Option<&ScheduledItem> {
        self.items.iter().min_by_key(|i| i.scheduled_for)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn items(&self) -> &[ScheduledItem] {
        &self.items
    }
}

// ---------------------------------------------------------------------------
// AmbientLock  (single-instance guard)
// ---------------------------------------------------------------------------

pub struct AmbientLock {
    pub(crate) lock_path: PathBuf,
}

impl AmbientLock {
    /// Read-only probe: which live process, if any, still holds the lock?
    ///
    /// The lock file records the pid of the process that acquired it, so a
    /// dead pid means the holder is gone and the lock file is a leftover.
    /// Callers use this to tell a genuinely in-flight cycle from a status
    /// persisted by a process that has since exited.
    ///
    /// Returns `None` when the lock file is missing, unreadable, malformed, or
    /// names a pid that is no longer running.
    pub fn live_holder_pid() -> Option<u32> {
        let path = lock_path().ok()?;
        let contents = std::fs::read_to_string(&path).ok()?;
        let pid = contents.trim().parse::<u32>().ok()?;
        is_pid_alive(pid).then_some(pid)
    }

    /// Try to acquire the ambient lock.
    /// Returns `Ok(Some(lock))` if acquired, `Ok(None)` if another instance
    /// already holds it, or `Err` on I/O failure.
    pub fn try_acquire() -> Result<Option<Self>> {
        let path = lock_path()?;

        // Check existing lock
        if path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&path)
                && let Ok(pid) = contents.trim().parse::<u32>()
                && is_pid_alive(pid)
            {
                return Ok(None); // Another instance is running
            }
            let _ = std::fs::remove_file(&path);
        }

        // Write our PID
        let pid = std::process::id();
        if let Some(parent) = path.parent() {
            storage::ensure_dir(parent)?;
        }
        std::fs::write(&path, pid.to_string())?;

        Ok(Some(Self { lock_path: path }))
    }

    pub fn release(self) -> Result<()> {
        let _ = std::fs::remove_file(&self.lock_path);
        // Drop runs, but we already cleaned up
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for AmbientLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

fn is_pid_alive(pid: u32) -> bool {
    crate::platform::is_process_running(pid)
}
