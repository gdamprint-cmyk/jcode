// reference: https://doc.rust-lang.org/std/fs/fn.rename.html (read 2026-11-15) -
// std::fs::rename "Renames a file or directory to a new name, replacing the
// original file if `to` already exists"; on Windows it maps to `MoveFileExW`
// with a `SetFileInformationByHandle` fallback, and "On Windows 10 version
// 1607 and above, the behavior is the same as Unix if the filesystem supports
// FileRenameInfoEx". This is why the journal is written with an atomic
// temp-file + rename and never as a partial write.
// reference: https://man7.org/linux/man-pages/man2/flock.2.html (read
// 2026-11-15) - flock(2): "A shared or exclusive lock can be placed on a file
// regardless of the mode in which the file was opened", LOCK_EX|LOCK_NB
// returns EWOULDBLOCK when the lock is held, and "Locks created by flock()
// are preserved across execve(2)". This is the basis of the single-writer
// landing lock: an advisory exclusive lock released by the kernel when the
// holder dies, so a killed carrier cannot wedge the landing forever.
// reference: crates/jcode-storage/src/lib.rs:564-623 (read 2026-11-15) -
// `write_bytes_inner` writes a temp file and renames it over the target, and
// on Windows "rename fails when the destination exists, so the primary must
// be moved away first; the brief missing window is unavoidable without
// platform-specific replace APIs". The landing journal is therefore written
// through `storage::write_json` (temp + rename), never in place.
// reference: crates/jcode-build-support/src/lib.rs:641-682 (read 2026-11-15) -
// `update_channel_symlink` / `update_current_symlink` /
// `update_shared_server_symlink`: each channel is swapped independently via
// `platform_support::atomic_symlink_swap` and then its marker file is
// written. Two channels therefore cannot be swapped as one filesystem
// transaction, which is exactly why this module journals the transaction
// instead of pretending the swaps are atomic together.
// reference: crates/jcode-build-support/src/platform_support.rs:38-103 (read
// 2026-11-15) - `atomic_symlink_swap` on Windows stages a copy, renames the
// old entry aside and moves the staged file into place, because Windows keeps
// a loaded executable open. On Unix it is a symlink + rename, which is
// atomic for a single channel.
//! The landing transaction: one crash-safe boundary that moves the whole
//! jcode ecosystem (client channels + daemon channel + the running daemon)
//! onto a single build.
//!
//! # The defect this closes
//!
//! MEASURED 2026-11-14 on this host: the client launcher ran
//! `v0.99.684-dev (04265bcce)`, the running daemon ran
//! `v0.99.681-dev (e8d6ba898)`, and the two channels disagreed for 14 hours.
//!
//! Root cause: a BUILD moved the client channel and the launcher
//! (`publish_local_current_build_for_source` -> `update_current_symlink` +
//! `update_launcher_symlink_to_current`), but only a RELOAD moved the daemon
//! channel (`update_shared_server_symlink`). Every build therefore opened a
//! window in which a new client could attach to an older daemon.
//!
//! # The protocol
//!
//! 1. **Stage.** The build installs an immutable versioned binary under
//!    `builds/versions/<label>/` and NOTHING else. No channel moves.
//! 2. **Begin.** The landing writes a journal recording the target version
//!    and the previous channel versions, so a crash mid-transaction is
//!    recoverable.
//! 3. **Prepare.** `shared-server` is pointed at the target. The client
//!    channels are NOT touched yet, so a failed reload leaves clients on the
//!    old version.
//! 4. **Hand off.** The running daemon is asked to exec into the target.
//! 5. **Verify.** The daemon's identity is checked against the target. A
//!    daemon that does not run the target is a failed landing.
//! 6. **Activate.** Only now do `current` and the launcher move to the target.
//! 7. **Commit.** The journal is cleared.
//!
//! If any step fails, the transaction rolls back: `shared-server` is restored
//! to its previous version and the client channels are left untouched (they
//! were never moved). A crash leaves the journal behind, and
//! [`recover_landing`] decides how to finish it: forward if the daemon already
//! runs the target, backward otherwise.

use crate::{
    build_log_path, builds_dir, read_current_version, read_shared_server_version,
    update_current_symlink, update_launcher_symlink_to_current, update_shared_server_symlink,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use jcode_storage as storage;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where the landing journal lives, relative to the builds dir.
const LANDING_JOURNAL_FILE: &str = "landing-transaction.json";

/// The single-writer lock for landing transactions.
const LANDING_LOCK_FILE: &str = "landing-transaction.lock";

/// Where the daemon publishes its own identity, so a landing can verify that
/// the daemon really runs the candidate build.
const DAEMON_IDENTITY_FILE: &str = "daemon-identity.json";

/// How far a landing transaction has progressed.
///
/// The phase is the crash-recovery anchor: it tells recovery whether the
/// daemon channel has already moved (so a rollback must undo it) and whether
/// the client channels have already moved (so a rollback must undo them too).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandingPhase {
    /// The journal exists; nothing has moved yet.
    Begun,
    /// `shared-server` points at the target; the client channels still point
    /// at the previous version.
    SharedServerPrepared,
    /// The daemon was asked to exec into the target; the outcome is not yet
    /// known. The handoff timestamp is recorded so a stale daemon identity
    /// (written before the handoff) cannot be mistaken for the new daemon.
    HandoffRequested,
    /// The daemon has been verified to run the target. The client channels
    /// still point at the previous version.
    DaemonVerified,
    /// `current` and the launcher have been moved to the target. Only the
    /// journal clear is left.
    ClientsActivated,
}

impl LandingPhase {
    /// Whether the daemon channel has already been moved to the target.
    pub fn shared_server_moved(self) -> bool {
        matches!(
            self,
            LandingPhase::SharedServerPrepared
                | LandingPhase::HandoffRequested
                | LandingPhase::DaemonVerified
                | LandingPhase::ClientsActivated
        )
    }

    /// Whether the client channels have already been moved to the target.
    pub fn clients_moved(self) -> bool {
        matches!(self, LandingPhase::ClientsActivated)
    }
}

/// A landing transaction journal.
///
/// This is the durable record of an in-flight landing. It exists so that a
/// crash (a killed carrier, a machine reboot) cannot leave the client
/// channels pointing at one build and the daemon at another: recovery reads
/// this file and either finishes the landing or rolls it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingTransaction {
    /// The version the whole ecosystem is being moved onto.
    pub target_version: String,
    /// The phase the transaction reached.
    pub phase: LandingPhase,
    /// The `current` channel version before the transaction, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_current_version: Option<String>,
    /// The `shared-server` channel version before the transaction, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_shared_server_version: Option<String>,
    /// The source fingerprint of the build being landed, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fingerprint: Option<String>,
    /// When the transaction was begun.
    pub started_at: DateTime<Utc>,
    /// When the daemon handoff was requested, if it was.
    ///
    /// A daemon identity published BEFORE this moment describes the old
    /// daemon, so it can never prove that the handoff succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_requested_at: Option<DateTime<Utc>>,
    /// The last recorded step, for diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_step: Option<String>,
}

impl LandingTransaction {
    /// Whether the client channels have already been moved to the target.
    pub fn clients_moved(&self) -> bool {
        self.phase.clients_moved()
    }

    /// Whether the daemon channel has already been moved to the target.
    pub fn shared_server_moved(&self) -> bool {
        self.phase.shared_server_moved()
    }
}

/// The identity a daemon publishes about itself.
///
/// A landing must not trust "the reload command returned 0": the daemon's own
/// reload guard refuses to exec into an older or identical binary, and a
/// refused reload leaves the daemon running the OLD build while the command
/// still reports success. The identity file is what lets the landing prove
/// which build actually runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonIdentity {
    /// The daemon's process id.
    pub pid: u32,
    /// The binary the daemon was started from.
    pub exe: PathBuf,
    /// The git hash the running binary was built from.
    pub git_hash: String,
    /// The version string the running binary reports.
    pub version: String,
    /// When the daemon published this identity.
    pub started_at: DateTime<Utc>,
}

/// Path of the landing journal.
pub fn landing_journal_path() -> Result<PathBuf> {
    Ok(builds_dir()?.join(LANDING_JOURNAL_FILE))
}

/// Path of the landing single-writer lock.
pub fn landing_lock_path() -> Result<PathBuf> {
    Ok(builds_dir()?.join(LANDING_LOCK_FILE))
}

/// Path of the daemon identity file.
///
/// The identity lives next to the landing journal, in the builds directory,
/// because that is the one place the client, the daemon and the carrier all
/// resolve identically: `builds_dir()` honours `JCODE_HOME` first and then
/// `%LOCALAPPDATA%\jcode\builds`, while `~/.jcode` is a different tree on
/// Windows. Splitting the landing state across two trees would let the
/// daemon publish its identity where the landing cannot see it.
pub fn daemon_identity_path() -> Result<PathBuf> {
    Ok(builds_dir()?.join(DAEMON_IDENTITY_FILE))
}

/// Load the in-flight landing journal, if any.
pub fn load_landing_transaction() -> Result<Option<LandingTransaction>> {
    let path = landing_journal_path()?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(storage::read_json(&path)?))
}

/// Persist the landing journal.
pub fn save_landing_transaction(transaction: &LandingTransaction) -> Result<()> {
    let path = landing_journal_path()?;
    storage::write_json(&path, transaction)
}

/// Remove the landing journal (the transaction is over).
pub fn clear_landing_transaction() -> Result<()> {
    let path = landing_journal_path()?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Publish the daemon's identity so a landing can verify which build runs.
pub fn publish_daemon_identity(identity: &DaemonIdentity) -> Result<()> {
    let path = daemon_identity_path()?;
    storage::write_json(&path, identity)
}

/// Read the last published daemon identity, if any.
pub fn read_daemon_identity() -> Result<Option<DaemonIdentity>> {
    let path = daemon_identity_path()?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(storage::read_json(&path)?))
}

/// The commit of a version label.
///
/// A self-dev version label is `<short-hash>` or
/// `<short-hash>-dirty-<fingerprint>`; a release label is a semver. The commit
/// is the part that identifies the BUILD, and it is what a daemon's embedded
/// `git_hash` can be compared against.
pub fn version_commit(version: &str) -> &str {
    version.split('-').next().unwrap_or(version).trim()
}

/// The build time of an installed version, read from its immutable binary.
///
/// MEASURED 2026-11-15 on this host: `builds/current/jcode.exe` and
/// `builds/versions/cec14bfbb-dirty-cc06d2aa916f/jcode.exe` share the mtime
/// 2026-11-14T20:18:56.907044, which is exactly the `build_time` the binary
/// reports in `version --json`. So the mtime of an immutable version binary IS
/// its build time, and two builds are comparable by it.
///
/// reference: crates/jcode-build-support/src/platform_support.rs (read
/// 2026-11-15) - `atomic_symlink_swap` on Windows COPIES the version binary
/// over the channel path, and the copy keeps the source's mtime. That is why
/// the CHANNEL's mtime is also the build's time, and why the build time can be
/// read from either place.
pub fn version_build_time(version: &str) -> Result<Option<std::time::SystemTime>> {
    if !is_valid_version_label(version) {
        anyhow::bail!("Invalid installed version label: {version:?}");
    }
    let path = crate::version_binary_path(version)?;
    let payload = crate::resolve_binary_payload(&path);
    Ok(std::fs::metadata(&payload)
        .ok()
        .and_then(|metadata| metadata.modified().ok()))
}

/// Whether landing `candidate` would move the ecosystem BACKWARD from `live`.
///
/// The comparison is by build time, because the labels themselves are not
/// ordered: a self-dev label is `<commit>-dirty-<fingerprint>`, and two labels
/// can name the same commit with different fingerprints.
///
/// An UNVERIFIABLE build (no binary on disk, no readable mtime) is never
/// treated as a downgrade: the guard must not block a landing it cannot judge.
pub fn would_downgrade(candidate: &str, live: &str) -> Result<bool> {
    if candidate == live {
        return Ok(false);
    }
    let (Some(candidate_time), Some(live_time)) =
        (version_build_time(candidate)?, version_build_time(live)?)
    else {
        return Ok(false);
    };
    Ok(candidate_time < live_time)
}

/// Refuse a landing that would DOWNGRADE the ecosystem.
///
/// This guards the AUTOMATIC target selection. An EXPLICIT version stays a
/// deliberate rollback, because a human who names a version wants that exact
/// build even when it is older.
///
/// The reason the guard exists: `staged-version` is written by every build and
/// is never cleared, and the version GC protects it from being pruned
/// (`protected_versions` in crates/jcode-build-support/src/version_gc.rs reads
/// `read_staged_version()`). So a stale staged label survives indefinitely.
/// Landing it automatically would move the whole ecosystem back to an older
/// build - the exact split the landing transaction exists to prevent, reached
/// by the other direction.
pub fn guard_landing_target_is_not_a_downgrade(candidate: &str) -> Result<()> {
    let mut checked: Vec<String> = Vec::new();
    for live in [crate::read_shared_server_version()?, crate::read_current_version()?] {
        let Some(live) = live else {
            continue;
        };
        if live == candidate || checked.contains(&live) {
            continue;
        }
        checked.push(live.clone());
        if would_downgrade(candidate, &live)? {
            anyhow::bail!(
                "Refusing to land {candidate}: the live build {live} is NEWER, so landing it \
                 would downgrade the whole ecosystem. Pass the version explicitly \
                 (`jcode server land {candidate}`) if a rollback is really intended."
            );
        }
    }
    Ok(())
}

/// Whether a daemon identity proves the daemon runs `version`.
///
/// The comparison is by COMMIT, not by the full label: a dirty self-dev
/// label carries a fingerprint of the working tree, and the daemon's embedded
/// hash is the plain commit. Comparing the full labels would reject every
/// legitimate self-dev landing.
///
/// Two label shapes are supported:
///
/// - a BUILD label (`<hash>`, `<hash>-dirty-<fingerprint>`, `main-<hash>`):
///   the hash-shaped part of the label is compared with the daemon's embedded
///   git hash;
/// - a RELEASE label (a semver, e.g. `1.2.3` or `1.0.0-preview.7`): a release
///   label carries no commit, so the semver the daemon REPORTS
///   (`v1.2.3 (abc1234)`) is compared instead. A dev build of the same semver
///   reports `v1.2.3-dev (...)`, which does not match a release label.
///
/// reference: https://docs.rs/semver/latest/semver/ (read 2026-11-15) - the
/// SemVer spec shape is `MAJOR.MINOR.PATCH` with an optional `-prerelease`
/// and `+build`; this function parses the same shape by hand, because the
/// build-support crate deliberately has no semver dependency.
pub fn daemon_identity_matches_version(identity: &DaemonIdentity, version: &str) -> bool {
    let hash = identity.git_hash.trim();
    if !hash.is_empty() && hash != "unknown" {
        for candidate in version_commit_candidates(version) {
            if hash == candidate || candidate.starts_with(hash) || hash.starts_with(&candidate) {
                return true;
            }
        }
    }

    match (
        release_semver_of(version),
        release_semver_of(&identity.version),
    ) {
        (Some(label), Some(reported)) => label == reported,
        _ => false,
    }
}

/// The hash-shaped parts a version label can name.
///
/// A self-dev label is `<short-hash>` or `<short-hash>-dirty-<fingerprint>`, a
/// main-channel label is `main-<short-hash>`, and a release label is a semver.
/// The daemon's identity carries the git hash of the RUNNING binary, so a
/// hash-shaped part of the label is what can be compared with it.
fn version_commit_candidates(version: &str) -> Vec<String> {
    version
        .split('-')
        .filter(|part| part.len() >= 7 && part.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|part| part.to_string())
        .collect()
}

/// The semver a version string names, if it names one.
///
/// `v1.2.3 (abc1234)` -> `1.2.3`, `1.2.3-preview.7` -> `1.2.3-preview.7`,
/// `abc1234-dirty-deadbeef` -> `None`.
fn release_semver_of(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let trimmed = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let core = trimmed
        .split([' ', '(', ')'])
        .next()
        .unwrap_or(trimmed)
        .trim();
    if core.is_empty() || !core.contains('.') {
        return None;
    }
    if !core.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(core.to_string())
}

/// Whether a version label names a RELEASE build (a semver).
///
/// `1.2.3` and `1.0.0-preview.7` are release labels; `abc1234-dirty-deadbeef`,
/// `main-abc1234` and `v1.2.3-dev` are not.
pub fn is_release_version_label(label: &str) -> bool {
    let trimmed = label.trim();
    let trimmed = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let core = trimmed
        .split([' ', '(', ')'])
        .next()
        .unwrap_or(trimmed)
        .trim();
    let Some(major) = core.split('.').next() else {
        return false;
    };
    !major.is_empty() && major.chars().all(|c| c.is_ascii_digit())
}

/// Whether a version label names a MAIN-channel build (`main-<hash>`).
pub fn is_main_channel_label(label: &str) -> bool {
    label.trim().starts_with("main-")
}

/// Whether the daemon channel is deliberately PINNED to a self-dev build.
///
/// A pin is the marker of a build that was promoted onto the `shared-server`
/// channel on purpose (a self-dev build the user wants to keep running). A
/// release or main-channel label is a channel build, not a pin: those are
/// expected to move with every update.
///
/// This is the ONE decision the update path makes about a pin, and it is
/// explicit rather than a warning: an update that would replace a pinned
/// self-dev daemon REFUSES to move any channel, so the pin is preserved and
/// the ecosystem is never split. A local `/rebuild` landing is a different
/// case: the user is asking for that exact local build, so it lands over the
/// pin atomically (the whole ecosystem moves together).
pub fn shared_server_is_pinned_to_selfdev() -> Result<bool> {
    let Some(shared) = crate::read_shared_server_version()? else {
        return Ok(false);
    };
    let shared = shared.trim();
    if shared.is_empty() {
        return Ok(false);
    }
    Ok(!is_release_version_label(shared) && !is_main_channel_label(shared))
}

/// The JSON report the `server land --json` command prints.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LandingReport {
    pub version: String,
    pub landed: bool,
    #[serde(default)]
    pub previous_current_version: Option<String>,
    #[serde(default)]
    pub previous_shared_server_version: Option<String>,
    #[serde(default)]
    pub detail: String,
}

/// Build the command that lands `target_version`.
///
/// The TARGET's own immutable versioned binary runs the landing, not the
/// launcher and not the caller's binary: only the target binary knows the
/// `land` subcommand, and the launcher may still be an older build that
/// predates it.
///
/// The version is passed EXPLICITLY: `jcode server land` without a version
/// lands the STAGED build, which is not necessarily the build the caller
/// installed (the staged marker is never cleared, so it can be stale).
pub fn landing_command(target_version: &str) -> Result<std::process::Command> {
    if !is_valid_version_label(target_version) {
        anyhow::bail!("Invalid installed version label: {target_version:?}");
    }
    let binary = crate::version_binary_path(target_version)?;
    if !binary.exists() {
        anyhow::bail!("Version binary not found at {:?}", binary);
    }
    let mut command = std::process::Command::new(binary);
    command.arg("server").arg("land").arg(target_version).arg("--json");
    Ok(command)
}

/// Land an already-installed version by running the target's own binary.
///
/// This is the child-process form of the landing: the child takes the
/// single-writer lock, journals the transaction, moves the daemon channel,
/// hands the daemon off, PROVES the daemon runs the target, and only then
/// moves the client channels. A caller that used this never moves a channel
/// itself, so the daemon and the clients can never disagree.
///
/// The returned outcome is the child's own report: a rolled-back landing is
/// reported as `RolledBack` (and as an error), never as a success.
pub fn land_installed_version(target_version: &str) -> Result<LandingOutcome> {
    let mut command = landing_command(target_version)?;
    let output = command.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let report = parse_landing_report(&stdout);

    match report {
        Some(report) if report.landed => Ok(LandingOutcome::Landed {
            target_version: report.version,
            previous_current_version: report.previous_current_version,
            previous_shared_server_version: report.previous_shared_server_version,
        }),
        Some(report) => Ok(LandingOutcome::RolledBack {
            target_version: report.version,
            reason: if report.detail.is_empty() {
                format!("the landing child exited with {}", output.status)
            } else {
                report.detail
            },
        }),
        None => Ok(LandingOutcome::RolledBack {
            target_version: target_version.to_string(),
            reason: format!(
                "the landing child produced no report (status {}): {}{}",
                output.status,
                stdout.trim(),
                stderr.trim()
            ),
        }),
    }
}

/// Parse the landing report out of the child's stdout.
///
/// The child prints the report as JSON, but a warning printed before it (for
/// example a recovery notice) would make the whole stdout unparseable, so the
/// first JSON object in the output is used.
fn parse_landing_report(stdout: &str) -> Option<LandingReport> {
    if let Ok(report) = serde_json::from_str::<LandingReport>(stdout) {
        return Some(report);
    }
    let start = stdout.find('{')?;
    serde_json::from_str::<LandingReport>(&stdout[start..]).ok()
}

/// Whether the daemon identity proves the daemon runs `version` AND is
/// fresher than the handoff request.
///
/// Both facts are required. The commit proves which build runs; the
/// timestamp proves it is the daemon that the handoff produced. Without the
/// timestamp, an identity left behind by the OLD daemon would make a failed
/// handoff look successful.
///
/// Returns `false` when no identity has been published: an unverifiable
/// daemon must never be treated as a verified one.
///
/// MEASURED 2026-11-17: `None` is NOT a weaker proof, it is NO proof at all.
/// A transaction that crashed before `record_handoff_requested` carries
/// `handoff_requested_at: None` (initialised at landing.rs:1041, stamped at
/// landing.rs:1086-1093), and this function used to
/// accept the identity on the git-hash match alone. The git hash cannot
/// distinguish two builds of the SAME commit - the source fingerprint that
/// does distinguish them lives only in the version LABEL, never in the binary
/// and never in the identity - so a stale identity published by an older
/// daemon of the same commit was accepted as proof that the target runs.
///
/// The honest verdict for "no handoff timestamp" is "unprovable", not
/// "probably fine": the landing rolls back in that case, which is the safe
/// direction. A rollback is recoverable; a desynced ecosystem is not.
pub fn daemon_identity_proves_version(
    identity: &DaemonIdentity,
    version: &str,
    handoff_requested_at: Option<DateTime<Utc>>,
) -> bool {
    if !daemon_identity_matches_version(identity, version) {
        return false;
    }
    match handoff_requested_at {
        Some(requested_at) => identity.started_at >= requested_at,
        // No handoff timestamp means no way to tell "the daemon re-exec'd onto
        // the target" from "the old daemon's identity file is still on disk".
        None => false,
    }
}

/// The decision an update makes BEFORE it touches any channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseLandingPlan {
    /// The update may land: the daemon channel is not pinned to a self-dev
    /// build, so the whole ecosystem moves onto the new version atomically.
    Land { version: String },
    /// The daemon channel is PINNED to a self-dev build, so the update is
    /// REFUSED. No channel moves, the installed build stays staged, and the
    /// pin is preserved.
    RefusedPinned {
        version: String,
        pinned_version: String,
    },
}

/// Decide whether an update may land `version`.
///
/// The pin is the ONE explicit decision the update path makes about a
/// deliberately promoted self-dev daemon:
///
/// - a RELEASE update REFUSES to replace a pinned self-dev daemon. The pin
///   exists to keep that build running, and a release update must not silently
///   destroy it. The release stays installed and staged, so
///   `jcode server land <version>` can land it explicitly later.
/// - the MAIN channel (a development channel of the very source tree the user
///   develops) is allowed to land over the pin, because the landing moves the
///   daemon AND the clients together: there is no split either way.
///
/// The caller passes the override explicitly, so the decision is testable and
/// not a hidden side effect.
pub fn plan_release_landing(version: &str) -> Result<ReleaseLandingPlan> {
    if !is_valid_version_label(version) {
        anyhow::bail!("Invalid installed version label: {version:?}");
    }
    if shared_server_is_pinned_to_selfdev()? {
        let pinned = crate::read_shared_server_version()?.unwrap_or_default();
        return Ok(ReleaseLandingPlan::RefusedPinned {
            version: version.to_string(),
            pinned_version: pinned,
        });
    }
    Ok(ReleaseLandingPlan::Land {
        version: version.to_string(),
    })
}

/// Land an already-installed version and move the `stable` channel after it.
///
/// This is the ONLY place an update may move a channel, and it does so only
/// after the landing transaction SUCCEEDED:
///
/// - a REFUSED pin moves nothing at all, and the lander is never called;
/// - a ROLLED-BACK landing moves nothing at all (the transaction restored
///   the daemon channel itself);
/// - a LANDED transaction moved the daemon channel, the running daemon and
///   the client channels as ONE transaction, and only then is `stable`
///   advanced to the same version.
///
/// The lander is injected so the decision is testable without a running
/// daemon, and so the production path can use the target binary's own
/// `server land` child process.
pub fn land_release_version(
    version: &str,
    allow_pin_override: bool,
    land: &mut dyn FnMut(&str) -> Result<LandingOutcome>,
) -> Result<LandingOutcome> {
    let plan = plan_release_landing(version)?;
    if let ReleaseLandingPlan::RefusedPinned {
        version,
        pinned_version,
    } = &plan
        && !allow_pin_override
    {
        anyhow::bail!(
            "Refusing to land {version}: the daemon channel is pinned to the self-dev build \
             {pinned_version}. The update was installed but no channel was moved. Land it \
             explicitly with `jcode server land {version}` if the pin should be replaced."
        );
    }

    let outcome = land(version)?;
    match &outcome {
        LandingOutcome::Landed { .. } => {
            // The ecosystem runs the new version now, so the fallback channel
            // may follow it. Doing this AFTER the landing is what keeps a
            // failed landing from leaving `stable` on a build the daemon does
            // not run.
            crate::update_stable_symlink(version)?;
            Ok(outcome)
        }
        LandingOutcome::RolledBack { reason, .. } => {
            anyhow::bail!("Landing {version} failed and was rolled back: {reason}")
        }
    }
}

/// Whether the live daemon provably runs `version` for this transaction.
pub fn daemon_runs_version(transaction: &LandingTransaction) -> bool {
    match read_daemon_identity() {
        Ok(Some(identity)) => daemon_identity_proves_version(
            &identity,
            &transaction.target_version,
            transaction.handoff_requested_at,
        ),
        _ => false,
    }
}

/// Whether a version label is safe to use as a directory name.
///
/// The label becomes a path component (`builds/versions/<label>/`), so a
/// label carrying a separator or a drive prefix would escape the versions
/// directory. This mirrors the check in `promote_version_to_shared_server`.
pub fn is_valid_version_label(version: &str) -> bool {
    !version.is_empty()
        && version.trim() == version
        && version != "."
        && version != ".."
        && !version.contains(['/', '\\', ':'])
}

/// Stage a build: install the immutable versioned binary WITHOUT touching any
/// channel.
///
/// This is the only thing a BUILD is allowed to do to the published tree. The
/// client channels (`current`, the launcher) and the daemon channel
/// (`shared-server`) are moved exclusively by a landing, so a build can never
/// leave a new client attached to an old daemon.
pub fn stage_build(binary: &Path, version: &str) -> Result<PathBuf> {
    crate::install_binary_at_version(binary, version)
}

/// The outcome of a landing transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandingOutcome {
    /// The whole ecosystem now runs the target.
    Landed {
        target_version: String,
        previous_current_version: Option<String>,
        previous_shared_server_version: Option<String>,
    },
    /// The landing did not happen. The client channels were left on their
    /// previous version and the daemon channel was restored.
    RolledBack {
        target_version: String,
        reason: String,
    },
}

/// How long the landing waits for the daemon to publish an identity that
/// proves it runs the target.
///
/// A handoff is an exec: the old daemon hands its sessions to a freshly
/// exec'd process. That process publishes its identity when it starts, so the
/// landing waits for the file rather than assuming the handoff worked.
const DAEMON_IDENTITY_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// How often the landing re-reads the identity file while waiting.
const DAEMON_IDENTITY_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// Wait until the daemon publishes an identity that proves it runs the
/// target.
///
/// The proof has TWO parts (see [`daemon_identity_proves_version`]): the
/// commit matches the target, and the identity is newer than the handoff
/// request. Waiting for the second part is what distinguishes "the new
/// daemon is running" from "the old daemon's identity file is still there".
pub fn wait_for_daemon_identity(
    target_version: &str,
    handoff_requested_at: Option<DateTime<Utc>>,
    timeout: std::time::Duration,
) -> Result<DaemonIdentity> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(identity)) = read_daemon_identity()
            && daemon_identity_proves_version(&identity, target_version, handoff_requested_at)
        {
            return Ok(identity);
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "the daemon did not report running {target_version} within {}s",
                timeout.as_secs()
            );
        }
        std::thread::sleep(DAEMON_IDENTITY_POLL);
    }
}

/// Activate the client channels once the daemon is PROVEN to run the target.
///
/// This is the last mutating step of the landing, split out for the callers
/// that run their own handoff (the self-dev reload path signals the daemon
/// in-process instead of going through the CLI).
///
/// The proof is required before the channels move. A reload that was
/// acknowledged but did not actually replace the daemon leaves the OLD
/// daemon running; moving the client channels then would point every new
/// client at a daemon running a different build, which is the exact desync
/// this whole mechanism exists to prevent.
pub fn activate_clients_after_verified_daemon(
    target_version: &str,
    handoff_requested_at: Option<DateTime<Utc>>,
    timeout: std::time::Duration,
) -> Result<DaemonIdentity> {
    let identity = wait_for_daemon_identity(target_version, handoff_requested_at, timeout)?;
    update_current_symlink(target_version)?;
    update_launcher_symlink_to_current()?;
    log_landing_step(&format!(
        "client channels activated for {target_version} (daemon pid={})",
        identity.pid
    ));
    Ok(identity)
}

/// A landing transaction in progress.
///
/// The session owns the single-writer lock and the journal, so a caller
/// cannot forget either. The steps are deliberately separate methods: the
/// HANDOFF between `prepare` and `verify` is the one step that needs an
/// async runtime (it talks to the running daemon over the socket), so the
/// caller drives it and the rest stays synchronous.
pub struct LandingSession {
    transaction: LandingTransaction,
    _lock: LandingLock,
}

impl LandingSession {
    /// Begin a landing transaction for `target_version`.
    ///
    /// Takes the single-writer lock and resolves any unfinished transaction
    /// left behind by a killed carrier BEFORE this one writes its own
    /// journal.
    pub fn begin(target_version: &str, source_fingerprint: Option<String>) -> Result<Self> {
        if !is_valid_version_label(target_version) {
            anyhow::bail!("Invalid installed version label: {target_version:?}");
        }
        let target_binary = crate::version_binary_path(target_version)?;
        if !target_binary.exists() {
            anyhow::bail!("Version binary not found at {:?}", target_binary);
        }

        let Some(lock) = LandingLock::try_acquire()? else {
            anyhow::bail!("Another landing is already in progress on this host");
        };

        // A journal left behind by a killed carrier must be resolved BEFORE
        // this landing writes its own, or the two would fight over the
        // channels. The lock is already held here, so the locked variant is
        // used.
        if let Some(recovery) = recover_landing_locked()? {
            log_landing_step(&format!("pre-landing recovery: {recovery:?}"));
        }

        let transaction = begin_landing(target_version, source_fingerprint)?;
        Ok(Self {
            transaction,
            _lock: lock,
        })
    }

    /// The transaction's current state.
    pub fn transaction(&self) -> &LandingTransaction {
        &self.transaction
    }

    /// Point the DAEMON channel at the target.
    ///
    /// The client channels are NOT moved yet, so a failed handoff leaves
    /// them on the old build.
    pub fn prepare(&mut self) -> Result<()> {
        let target = self.transaction.target_version.clone();
        prepare_shared_server(&mut self.transaction, &target)
    }

    /// Record that the handoff was requested.
    ///
    /// The timestamp anchors the identity check: any daemon identity
    /// published before this moment describes the OLD daemon.
    pub fn record_handoff(&mut self) -> Result<()> {
        record_handoff_requested(&mut self.transaction)
    }

    /// PROVE the daemon runs the target, then move the client channels.
    ///
    /// This is the last mutating step. The client channels follow the daemon,
    /// never the other way round.
    pub fn verify_and_activate(&mut self, timeout: std::time::Duration) -> Result<LandingOutcome> {
        let target = self.transaction.target_version.clone();
        let identity = wait_for_daemon_identity(
            &target,
            self.transaction.handoff_requested_at,
            timeout,
        )?;
        log_landing_step(&format!(
            "daemon verified: pid={} version={} git_hash={}",
            identity.pid, identity.version, identity.git_hash
        ));
        advance_landing(
            &mut self.transaction,
            LandingPhase::DaemonVerified,
            "daemon verified",
        )?;

        activate_client_channels(&mut self.transaction, &target)?;
        clear_landing_transaction()?;
        log_landing_step(&format!("landed {target}"));

        Ok(LandingOutcome::Landed {
            target_version: target,
            previous_current_version: self.transaction.previous_current_version.clone(),
            previous_shared_server_version: self.transaction.previous_shared_server_version.clone(),
        })
    }

    /// Move the client channels WITHOUT a daemon handoff.
    ///
    /// This is the landing's answer to "no daemon is running". There is nothing
    /// to hand off, so there is nothing to prove: the next daemon starts from
    /// the `shared-server` channel that `prepare` already pointed at the
    /// target, and the client channels may follow it.
    ///
    /// Waiting for a daemon identity here would be wrong: with no daemon
    /// running, no identity can ever appear, so the landing would wait for its
    /// whole timeout and then roll back a transaction that had nothing to
    /// prove.
    pub fn activate_clients_without_daemon(&mut self) -> Result<LandingOutcome> {
        let target = self.transaction.target_version.clone();
        activate_client_channels(&mut self.transaction, &target)?;
        clear_landing_transaction()?;
        log_landing_step(&format!(
            "landed {target} (no daemon was running; the next daemon starts from the shared-server channel)"
        ));
        Ok(LandingOutcome::Landed {
            target_version: target,
            previous_current_version: self.transaction.previous_current_version.clone(),
            previous_shared_server_version: self.transaction.previous_shared_server_version.clone(),
        })
    }

    /// Roll the transaction back.
    ///
    /// The client channels are restored only when they had already been
    /// moved; the daemon channel only when it had been moved. This is what
    /// makes "a failed landing leaves the client on the old version" true.
    pub fn rollback(&mut self) -> Result<()> {
        rollback_landing(&self.transaction)
    }
}

/// Land `target_version` onto the whole ecosystem as ONE transaction.
///
/// This is the SYNCHRONOUS convenience form, for callers whose handoff is
/// not async (the tests). The order of the steps is the whole point:
///
/// 1. Take the single-writer lock: two landings would publish two different
///    builds and split the ecosystem again.
/// 2. Recover any unfinished transaction first, so a stale journal never
///    decides the outcome of a new landing.
/// 3. Point the DAEMON channel at the target. The client channels still point
///    at the old build.
/// 4. Hand the daemon off to the target.
/// 5. PROVE the daemon runs the target. A handoff that did not take effect is
///    a failed landing, not a success to be papered over.
/// 6. Only now move the client channels onto the target.
/// 7. Commit (clear the journal).
///
/// Any failure before step 6 rolls the daemon channel back and leaves the
/// client channels exactly where they were: a failed landing leaves clients
/// on the old build, which is the whole point of the transaction.
pub fn land_version(
    target_version: &str,
    source_fingerprint: Option<String>,
    handoff: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<LandingOutcome> {
    land_version_with_timeout(
        target_version,
        source_fingerprint,
        handoff,
        DAEMON_IDENTITY_WAIT,
    )
}

/// Land `target_version` with an explicit identity-wait timeout.
///
/// The timeout exists for two callers: the production path uses
/// [`DAEMON_IDENTITY_WAIT`], and the tests need a short bound so a failing
/// handoff is exercised in seconds rather than minutes.
pub fn land_version_with_timeout(
    target_version: &str,
    source_fingerprint: Option<String>,
    handoff: &mut dyn FnMut(&str) -> Result<()>,
    identity_timeout: std::time::Duration,
) -> Result<LandingOutcome> {
    let mut session = LandingSession::begin(target_version, source_fingerprint)?;
    let outcome = (|| -> Result<LandingOutcome> {
        session.prepare()?;
        session.record_handoff()?;
        handoff(target_version)?;
        session.verify_and_activate(identity_timeout)
    })();

    match outcome {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            let reason = format!("{error:#}");
            // Roll back what this transaction moved. The client channels are
            // untouched at this point unless the failure happened after the
            // activation, and `rollback_landing` only restores what it moved.
            if let Err(rollback_error) = session.rollback() {
                log_landing_step(&format!(
                    "rollback FAILED for {target_version}: {rollback_error:#}"
                ));
            }
            Ok(LandingOutcome::RolledBack {
                target_version: target_version.to_string(),
                reason,
            })
        }
    }
}

/// Begin a landing transaction for `target_version`.
///
/// Records the current channel versions so a rollback can restore them. Any
/// pre-existing journal is replaced: a new landing supersedes an unfinished
/// one, and the caller is responsible for recovering that one first.
pub fn begin_landing(
    target_version: &str,
    source_fingerprint: Option<String>,
) -> Result<LandingTransaction> {
    let transaction = LandingTransaction {
        target_version: target_version.to_string(),
        phase: LandingPhase::Begun,
        previous_current_version: read_current_version()?,
        previous_shared_server_version: read_shared_server_version()?,
        source_fingerprint,
        started_at: Utc::now(),
        handoff_requested_at: None,
        last_step: Some("begin".to_string()),
    };
    save_landing_transaction(&transaction)?;
    log_landing_step(&format!(
        "begin target={} previous_current={:?} previous_shared={:?}",
        target_version,
        transaction.previous_current_version,
        transaction.previous_shared_server_version
    ));
    Ok(transaction)
}

/// Advance the journal to `phase` and record the step that reached it.
pub fn advance_landing(
    transaction: &mut LandingTransaction,
    phase: LandingPhase,
    step: &str,
) -> Result<()> {
    transaction.phase = phase;
    transaction.last_step = Some(step.to_string());
    save_landing_transaction(transaction)
}

/// Point the daemon channel at the target.
///
/// The client channels are deliberately NOT moved here: until the daemon has
/// been verified to run the target, a new client must keep using the old
/// build, so a failed landing leaves no client on an unverified daemon.
pub fn prepare_shared_server(
    transaction: &mut LandingTransaction,
    target_version: &str,
) -> Result<()> {
    update_shared_server_symlink(target_version)?;
    advance_landing(
        transaction,
        LandingPhase::SharedServerPrepared,
        "shared-server prepared",
    )
}

/// Record that the daemon handoff was requested.
///
/// The timestamp anchors the identity check: any daemon identity published
/// before this moment describes the OLD daemon.
pub fn record_handoff_requested(transaction: &mut LandingTransaction) -> Result<()> {
    transaction.handoff_requested_at = Some(Utc::now());
    advance_landing(
        transaction,
        LandingPhase::HandoffRequested,
        "handoff requested",
    )
}

/// Move the client channels (`current` and the launcher) to the target.
///
/// This is the last mutating step. It runs only after the daemon has been
/// verified to run the target, so the client channels and the daemon always
/// name the same build.
pub fn activate_client_channels(
    transaction: &mut LandingTransaction,
    target_version: &str,
) -> Result<()> {
    update_current_symlink(target_version)?;
    update_launcher_symlink_to_current()?;
    advance_landing(
        transaction,
        LandingPhase::ClientsActivated,
        "client channels activated",
    )
}

/// Roll the transaction back.
///
/// The client channels are restored only when they had already been moved;
/// when the failure happened before that, they are left exactly as they were.
/// The daemon channel is restored only when it had been moved. This is what
/// makes "a failed landing leaves the client on the old version" true.
pub fn rollback_landing(transaction: &LandingTransaction) -> Result<()> {
    if transaction.clients_moved()
        && let Some(previous) = transaction.previous_current_version.as_deref()
    {
        update_current_symlink(previous)?;
        update_launcher_symlink_to_current()?;
    }
    if transaction.shared_server_moved()
        && let Some(previous) = transaction.previous_shared_server_version.as_deref()
    {
        update_shared_server_symlink(previous)?;
    }
    clear_landing_transaction()?;
    log_landing_step(&format!(
        "rollback target={} clients_moved={} shared_moved={}",
        transaction.target_version,
        transaction.clients_moved(),
        transaction.shared_server_moved()
    ));
    Ok(())
}

/// What recovery should do with an unfinished landing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandingRecovery {
    /// Nothing to recover: no journal was found.
    Nothing,
    /// The daemon already runs the target, so the landing can be completed
    /// forward: move the client channels onto it and commit.
    CompleteForward { target_version: String },
    /// The daemon does not run the target, so the transaction is rolled back:
    /// the client channels keep the previous version and the daemon channel is
    /// restored.
    RollBack {
        target_version: String,
        reason: String,
    },
}

/// Decide how to finish an unfinished landing.
///
/// The decision is made from ONE fact: does the running daemon already run
/// the target? If yes, the landing had already reached the handoff and the
/// only remaining work is to move the client channels and commit. If no, the
/// landing never took effect on the daemon, so it is rolled back and the
/// client channels keep their previous version.
///
/// A daemon whose identity cannot be read is treated as "does not run the
/// target": an unverifiable daemon must never be allowed to drag the client
/// channels forward.
pub fn decide_landing_recovery(
    transaction: &LandingTransaction,
    daemon_runs_target: bool,
) -> LandingRecovery {
    if daemon_runs_target {
        return LandingRecovery::CompleteForward {
            target_version: transaction.target_version.clone(),
        };
    }
    LandingRecovery::RollBack {
        target_version: transaction.target_version.clone(),
        reason: "the running daemon does not run the landing target".to_string(),
    }
}

/// Recover an unfinished landing, if there is one.
///
/// This is the crash-recovery entry point. It is safe to call at any time: it
/// does nothing when there is no journal.
///
/// The single-writer lock is taken first. If a landing is IN PROGRESS, the
/// recovery does nothing: that landing owns the channels and will finish or
/// roll them back itself. Without this, a daemon that restarts for an
/// unrelated reason mid-landing would roll back a transaction whose client
/// channels had already been activated, re-creating the very desync this
/// module exists to prevent.
pub fn recover_landing() -> Result<Option<LandingRecovery>> {
    let Some(_lock) = LandingLock::try_acquire()? else {
        // A landing owns the channels right now. Leave them to it.
        return Ok(None);
    };
    recover_landing_locked()
}

/// Recover an unfinished landing while the caller already holds the lock.
pub fn recover_landing_locked() -> Result<Option<LandingRecovery>> {
    let Some(transaction) = load_landing_transaction()? else {
        return Ok(None);
    };
    let daemon_runs_target = daemon_runs_version(&transaction);
    match decide_landing_recovery(&transaction, daemon_runs_target) {
        LandingRecovery::CompleteForward { target_version } => {
            let mut transaction = transaction;
            // The client channels may already be on the target (the crash
            // happened between activating them and clearing the journal), so
            // the activation is idempotent.
            activate_client_channels(&mut transaction, &target_version)?;
            clear_landing_transaction()?;
            log_landing_step(&format!("recovered forward target={target_version}"));
            Ok(Some(LandingRecovery::CompleteForward { target_version }))
        }
        LandingRecovery::RollBack {
            target_version,
            reason,
        } => {
            rollback_landing(&transaction)?;
            Ok(Some(LandingRecovery::RollBack {
                target_version,
                reason,
            }))
        }
        LandingRecovery::Nothing => Ok(None),
    }
}

/// A single-writer lock over landing transactions.
///
/// Two landings running at once would publish two different builds and leave
/// the ecosystem split, which is exactly the desync this module exists to
/// prevent. The lock is advisory and process-scoped: on unix it is a
/// `flock` (released by the kernel when the holder dies), on Windows it is an
/// exclusively created file carrying the owner's pid, broken when that pid is
/// gone.
pub struct LandingLock {
    #[cfg(unix)]
    file: std::fs::File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl LandingLock {
    /// Try to take the lock without blocking.
    ///
    /// Returns `None` when another landing holds it: the caller is expected to
    /// do nothing, because the holder is already performing exactly this work.
    pub fn try_acquire() -> Result<Option<Self>> {
        let path = landing_lock_path()?;
        if let Some(parent) = path.parent() {
            storage::ensure_dir(parent)?;
        }

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)?;
            // SAFETY: `file` owns a live descriptor for the duration of the
            // call; flock(2) only inspects the descriptor.
            let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if ret == 0 {
                Ok(Some(Self { file }))
            } else {
                Ok(None)
            }
        }

        #[cfg(not(unix))]
        {
            match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    let _ = write!(file, "{}", std::process::id());
                    Ok(Some(Self { path }))
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    // A lock whose owner is gone is stale: break it, otherwise
                    // a killed carrier would wedge the landing forever.
                    if break_stale_landing_lock(&path) {
                        match std::fs::OpenOptions::new()
                            .create_new(true)
                            .write(true)
                            .open(&path)
                        {
                            Ok(mut file) => {
                                use std::io::Write;
                                let _ = write!(file, "{}", std::process::id());
                                Ok(Some(Self { path }))
                            }
                            Err(_) => Ok(None),
                        }
                    } else {
                        Ok(None)
                    }
                }
                Err(error) => Err(error.into()),
            }
        }
    }
}

impl Drop for LandingLock {
    fn drop(&mut self) {
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Remove a landing lock whose owner process is gone.
///
/// Returns true when the lock was broken (or was already gone).
#[cfg(not(unix))]
fn break_stale_landing_lock(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        // Unreadable lock: treat it as stale so a corrupt lock cannot wedge
        // the landing forever.
        let _ = std::fs::remove_file(path);
        return true;
    };
    let Ok(owner) = content.trim().parse::<u32>() else {
        let _ = std::fs::remove_file(path);
        return true;
    };
    if owner == std::process::id() {
        return false;
    }
    if process_is_alive(owner) {
        return false;
    }
    let _ = std::fs::remove_file(path);
    true
}

/// Whether a process id is still alive.
///
/// On Windows the check uses the Win32 process handle: a handle is opened with
/// SYNCHRONIZE and queried for the exit code, which is the documented way to
/// tell a live process from a reaped one without racing the PID reuse.
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

#[cfg(all(not(windows), not(unix)))]
fn process_is_alive(_pid: u32) -> bool {
    // Unknown platform: assume alive so a live landing is never displaced.
    true
}

/// Log a landing step to the build log, so a landing is traceable after the
/// fact even when the journal is gone.
pub fn log_landing_step(step: &str) {
    let Ok(path) = build_log_path() else {
        return;
    };
    let line = format!("[{}] landing: {}\n", Utc::now().to_rfc3339(), step);
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transaction(phase: LandingPhase) -> LandingTransaction {
        LandingTransaction {
            target_version: "abc1234-dirty-deadbeef".to_string(),
            phase,
            previous_current_version: Some("old-current".to_string()),
            previous_shared_server_version: Some("old-shared".to_string()),
            source_fingerprint: None,
            started_at: Utc::now(),
            handoff_requested_at: None,
            last_step: None,
        }
    }

    fn identity(git_hash: &str, started_at: DateTime<Utc>) -> DaemonIdentity {
        DaemonIdentity {
            pid: 1,
            exe: PathBuf::from("/x/jcode"),
            git_hash: git_hash.to_string(),
            version: format!("v0.99.1-dev ({git_hash}, dirty)"),
            started_at,
        }
    }

    #[test]
    fn commit_is_the_hash_part_of_a_label() {
        assert_eq!(version_commit("abc1234"), "abc1234");
        assert_eq!(version_commit("abc1234-dirty-deadbeef"), "abc1234");
        assert_eq!(version_commit("0.15.0"), "0.15.0");
    }

    #[test]
    fn daemon_identity_matches_the_commit_of_a_dirty_label() {
        let identity = identity("abc1234", Utc::now());
        assert!(daemon_identity_matches_version(
            &identity,
            "abc1234-dirty-deadbeef"
        ));
        assert!(!daemon_identity_matches_version(&identity, "9999999"));
    }

    #[test]
    fn unknown_daemon_hash_never_matches() {
        let identity = identity("unknown", Utc::now());
        assert!(!daemon_identity_matches_version(&identity, "abc1234"));
    }

    #[test]
    fn an_identity_from_before_the_handoff_never_proves_the_target() {
        // The OLD daemon published its identity before the handoff was even
        // requested. It runs the target's commit only by coincidence; the
        // timestamp proves it is the wrong daemon.
        let handoff = Utc::now();
        let old_identity = identity("abc1234", handoff - chrono::Duration::seconds(30));
        assert!(!daemon_identity_proves_version(
            &old_identity,
            "abc1234-dirty-deadbeef",
            Some(handoff)
        ));
    }

    #[test]
    fn an_identity_from_after_the_handoff_proves_the_target() {
        let handoff = Utc::now();
        let new_identity = identity("abc1234", handoff + chrono::Duration::seconds(2));
        assert!(daemon_identity_proves_version(
            &new_identity,
            "abc1234-dirty-deadbeef",
            Some(handoff)
        ));
    }

    /// A transaction that crashed BEFORE the handoff was recorded carries no
    /// timestamp, so there is no way to tell the new daemon's identity from
    /// the old daemon's leftover file. The proof must REFUSE, not assume.
    ///
    /// MEASURED 2026-11-17: the git hash cannot make the distinction either.
    /// The source fingerprint that separates two builds of one commit lives
    /// only in the version label, never in the binary and never in the
    /// identity, so a commit match is NOT evidence that the target runs.
    #[test]
    fn an_identity_without_a_handoff_timestamp_never_proves_the_target() {
        let identity = identity("abc1234", Utc::now());
        assert!(
            !daemon_identity_proves_version(&identity, "abc1234-dirty-deadbeef", None),
            "without a handoff timestamp the identity proves nothing"
        );
    }

    #[test]
    fn recovery_completes_forward_when_the_daemon_runs_the_target() {
        let transaction = transaction(LandingPhase::SharedServerPrepared);
        let recovery = decide_landing_recovery(&transaction, true);
        assert!(matches!(
            recovery,
            LandingRecovery::CompleteForward { target_version } if target_version == "abc1234-dirty-deadbeef"
        ));
    }

    #[test]
    fn recovery_rolls_back_when_the_daemon_does_not_run_the_target() {
        let transaction = transaction(LandingPhase::SharedServerPrepared);
        let recovery = decide_landing_recovery(&transaction, false);
        assert!(matches!(recovery, LandingRecovery::RollBack { .. }));
    }

    #[test]
    fn a_begun_transaction_has_moved_nothing() {
        let transaction = transaction(LandingPhase::Begun);
        assert!(!transaction.clients_moved());
        assert!(!transaction.shared_server_moved());
    }

    #[test]
    fn a_prepared_transaction_moved_only_the_daemon_channel() {
        let transaction = transaction(LandingPhase::SharedServerPrepared);
        assert!(!transaction.clients_moved());
        assert!(transaction.shared_server_moved());
    }
}
