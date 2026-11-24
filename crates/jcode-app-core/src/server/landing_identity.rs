// reference: https://doc.rust-lang.org/std/env/fn.current_exe.html (read
// 2026-11-15) - "Returns the full filesystem path of the current running
// executable"; "If the executable was renamed while it is running, platforms
// may return the path at the time it was loaded instead of the new path".
// This is why the identity records the exe path as INFORMATION only: on
// Windows the daemon runs the shared-server channel FILE, and the channel is
// replaced in place, so the path does not identify the build. The git hash
// does.
// reference: https://doc.rust-lang.org/std/process/fn.id.html (read
// 2026-11-15) - "Returns the OS-assigned process identifier associated with
// this process". The pid lets a landing (and an operator) tell which daemon
// published the identity.
//! The daemon's published identity, and the landing recovery hook.
//!
//! A landing must be able to PROVE that the running daemon really runs the
//! build it claims to have landed. It cannot infer that from the exit status
//! of a reload command: the daemon's own reload guard refuses to exec into an
//! older or identical binary, and a refused reload leaves the daemon running
//! the OLD build while the command still reports success.
//!
//! So the daemon publishes who it is, and the landing reads that file. The
//! identity carries the git hash of the running binary, which is the same
//! commit that names the version label (`<short-hash>` or
//! `<short-hash>-dirty-<fingerprint>`), so the comparison is exact.
//!
//! The same module owns the startup recovery hook: a daemon that starts while
//! a landing transaction is unfinished resolves it (forward if this daemon
//! already runs the target, backward otherwise) instead of leaving the
//! ecosystem split.

use jcode_build_support::{
    DaemonIdentity, LandingRecovery, publish_daemon_identity, recover_landing,
};
use std::path::PathBuf;

/// Publish this daemon's identity so a landing can verify which build runs.
///
/// The `started_at` timestamp is the moment the IDENTITY was published, not
/// the moment the process started. That distinction matters: the landing
/// compares the identity's timestamp against the handoff request, and the
/// whole point is to tell "the daemon that the handoff produced" from "the
/// old daemon's identity file that happens to still be on disk". A daemon
/// that re-execs publishes a fresh identity, so the timestamp advances
/// exactly when the build changes.
pub fn publish_current_daemon_identity() -> anyhow::Result<()> {
    let identity = DaemonIdentity {
        pid: std::process::id(),
        exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("unknown")),
        git_hash: jcode_build_meta::git_hash().to_string(),
        version: jcode_build_meta::version().to_string(),
        started_at: chrono::Utc::now(),
    };
    publish_daemon_identity(&identity)
}

/// Resolve an unfinished landing transaction, if there is one.
///
/// This is the crash-recovery hook. It is called by the daemon at startup, so
/// a machine that lost power in the middle of a landing comes back with the
/// channels either finished (the daemon runs the target) or restored (it
/// does not) - never split.
pub fn recover_unfinished_landing() -> Option<LandingRecovery> {
    match recover_landing() {
        Ok(recovery) => recovery,
        Err(error) => {
            crate::logging::warn(&format!(
                "landing recovery failed at startup: {error:#}"
            ));
            None
        }
    }
}

/// Publish the identity and resolve any unfinished landing.
///
/// This is what a daemon calls once it is up: it announces who it is, then
/// it resolves whatever a previous landing left behind.
///
/// The ORDER matters. The identity is published FIRST, so a recovery that
/// finds an unfinished transaction sees THIS daemon's identity and can decide
/// correctly. If the recovery ran first, it would read the previous
/// daemon's identity file and could conclude, wrongly, that the handoff never
/// happened.
pub fn publish_identity_and_recover() {
    if let Err(error) = publish_current_daemon_identity() {
        crate::logging::warn(&format!("failed to publish daemon identity: {error:#}"));
    }
    if let Some(recovery) = recover_unfinished_landing() {
        crate::logging::info(&format!("landing recovery at startup: {recovery:?}"));
    }
}
