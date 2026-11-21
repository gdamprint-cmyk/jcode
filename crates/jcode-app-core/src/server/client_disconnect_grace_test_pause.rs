// reference: no prior art, searched "clippy await_holding_lock" in
// https://rust-lang.github.io/rust-clippy/master/ (the lint's own page documents
// the fix pattern: drop the guard in a scope before `.await`). This file is a
// test-only synchronization primitive; its correctness comes from reading the
// host, not the internet.
//! Test-only synchronization for the disconnect-cleanup ownership race.
//!
//! A unit test cannot cut the window between cleanup's successor check and its
//! destructive removal from outside the function, and adding a parameter would
//! change the production signature for every caller. The pause is registered
//! here by session id instead: cleanup looks up its own session, so production
//! call sites stay unchanged and only `#[cfg(test)]` lookups appear inside the
//! functions under test.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Notify;

#[derive(Clone)]
pub(crate) struct Pause {
    reached: Arc<Notify>,
    release: Arc<Notify>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

impl Pause {
    pub(crate) fn new() -> Self {
        Self {
            reached: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            finished: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Test side: wait until cleanup reports it is in the check/removal gap.
    ///
    /// Bounded so a lost `reached` notification fails the test instead of
    /// hanging the whole suite (measured 2026-11-19: an unbounded wait hung the
    /// 1904-test lib binary while the focused run passed in 1.58 s).
    pub(crate) async fn reached(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(5), self.reached.notified())
            .await
            .expect("cleanup must reach the check/removal gap");
    }

    /// Cleanup side: report arrival and wait for the test to release teardown.
    pub(crate) async fn wait_for_release(&self) {
        self.reached.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), self.release.notified())
            .await
            .expect("test must release the parked cleanup");
    }

    /// Test side: let cleanup proceed into the destructive removal.
    pub(crate) fn release(&self) {
        self.release.notify_one();
    }

    /// Cleanup side: report that teardown finished.
    pub(crate) fn mark_finished(&self) {
        self.finished.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Test side: did cleanup run its teardown?
    pub(crate) fn is_finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn pauses() -> &'static Mutex<HashMap<String, Pause>> {
    static PAUSES: OnceLock<Mutex<HashMap<String, Pause>>> = OnceLock::new();
    PAUSES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, Pause>> {
    pauses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Test side: park cleanup for this session when it reaches the gap.
pub(crate) fn register(session_id: &str, pause: Pause) {
    lock().insert(session_id.to_string(), pause);
}

/// Cleanup side: clone the pause registered for this session, if any.
pub(crate) fn take(session_id: &str) -> Option<Pause> {
    lock().get(session_id).cloned()
}

/// Test side: drop the registration once the assertions are done.
pub(crate) fn forget(session_id: &str) {
    lock().remove(session_id);
}
