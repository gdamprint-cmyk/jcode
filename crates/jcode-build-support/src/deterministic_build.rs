// reference: `src/cli/selfdev.rs:127-141` (read 2026-11-15) - the TUI build
// path: `current_source_state` -> `run_selfdev_build` ->
// `ensure_source_state_matches` -> `stage_local_current_build_for_source`. This
// module is the SAME build, driven deterministically by a carrier instead of by
// an LLM turn, and it adds the one thing the LLM used to own: the DISTINCTION
// between "the tree moved, build again" (SUPERSEDED) and "the code does not
// compile" (FAILED).
//
// reference: crates/jcode-app-core/src/tool/selfdev/build_queue.rs:374-430
// (read 2026-11-15) - the session-driven build path: it compiles, re-checks the
// source state, reports drift as "superseded" instead of failed, and STAGES the
// result without moving any channel.
//
// reference: sysexits(3) (FreeBSD man page, read 2026-11-15) - EX_TEMPFAIL
// (75) is "temporary failure, indicating something that is not really an
// error... the request should be reattempted later". The CLI uses it for the
// retryable outcomes (SUPERSEDED, BUSY), so a caller can tell "run me again"
// from "it does not compile" by the exit code alone.
use super::{
    BuildLockGuard, PublishedBuild, SourceState, current_source_state, ensure_source_state_matches,
    find_dev_binary, run_selfdev_build, stage_local_current_build_for_source,
    write_current_dev_binary_source_metadata,
};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// What one deterministic build attempt produced.
#[derive(Debug, Clone)]
pub enum DeterministicBuildOutcome {
    /// The build compiled, matched the source state and was STAGED. No channel
    /// moved: the landing owns that.
    Staged {
        version: String,
        fingerprint: String,
        versioned_path: PathBuf,
    },
    /// The tree moved while the build ran; nothing was published. The caller
    /// should build again against the CURRENT tree.
    Superseded {
        expected_fingerprint: String,
        current_fingerprint: String,
        reason: String,
    },
    /// The compile failed, or the staging guard refused the binary for a reason
    /// that is not a drift.
    Failed { reason: String },
    /// The target binary is held open by a running session and could not be
    /// replaced. The holder is NEVER killed: the caller retries once the
    /// session is gone (or the binary is renamed aside).
    Locked { reason: String },
    /// Another build holds this worktree's build lock.
    Busy { reason: String },
    /// The current source state is already the staged build; nothing to do.
    UpToDate {
        version: String,
        fingerprint: String,
    },
}

impl DeterministicBuildOutcome {
    /// Whether the outcome is a successful staging.
    pub fn is_staged(&self) -> bool {
        matches!(self, DeterministicBuildOutcome::Staged { .. })
    }

    /// Whether the caller should simply run the build again.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            DeterministicBuildOutcome::Superseded { .. } | DeterministicBuildOutcome::Busy { .. }
        )
    }

    /// A one-line human summary.
    pub fn summary(&self) -> String {
        match self {
            DeterministicBuildOutcome::Staged { version, .. } => format!("staged {version}"),
            DeterministicBuildOutcome::UpToDate { version, .. } => format!("up to date {version}"),
            DeterministicBuildOutcome::Superseded { reason, .. } => format!("superseded: {reason}"),
            DeterministicBuildOutcome::Failed { reason } => format!("failed: {reason}"),
            DeterministicBuildOutcome::Locked { reason } => format!("locked: {reason}"),
            DeterministicBuildOutcome::Busy { reason } => format!("busy: {reason}"),
        }
    }
}

/// The source state a build would be bound to right now.
///
/// The carrier reads this to decide whether a build is needed at all, without
/// starting one.
pub fn current_buildable_source(repo_dir: &Path) -> Result<SourceState> {
    current_source_state(repo_dir)
}

/// Compile and STAGE the current source state. It moves NO channel.
///
/// The retry after a drift is NOT done here: the carrier owns it, because the
/// carrier also owns the decision to build at all. This function reports the
/// drift as SUPERSEDED and lets the caller decide.
pub fn run_deterministic_build(repo_dir: &Path) -> Result<DeterministicBuildOutcome> {
    let source = current_source_state(repo_dir)?;

    // The UP-TO-DATE check is what makes this safe to run on every carrier
    // tick: the staged version label names the exact source state it was built
    // from (`<short hash>-dirty-<fingerprint>`), so an equal label means the
    // current tree is already staged and nothing has to be compiled.
    if let Some(staged) = super::read_staged_version()?
        && staged == source.version_label
    {
        return Ok(DeterministicBuildOutcome::UpToDate {
            version: staged,
            fingerprint: source.fingerprint,
        });
    }

    run_staging_build_with(
        repo_dir,
        &source,
        || {
            // MEASURED on this host (2026-11-15): a live self-dev session holds
            // a Windows handle on `target/selfdev/jcode.exe`, so cargo's
            // `remove_file` fails with access denied and the build dies with
            // exit 101. Windows ALLOWS RENAMING a file with an open handle, so
            // the locked binary is moved aside and cargo writes a fresh one.
            // The holder is never killed: it is a real live session.
            if let Some(binary) = super::find_dev_binary(repo_dir) {
                super::prepare_binary_for_overwrite(&binary)?;
            }
            run_selfdev_build(repo_dir).map(|_| ())
        },
        stage_local_current_build_for_source,
    )
}

/// The BUILD half with the compile and the staging injected, so the staging
/// contract can be tested without a real cargo run and without an executable
/// binary.
pub fn run_staging_build_with(
    repo_dir: &Path,
    source: &SourceState,
    compile: impl FnOnce() -> Result<()>,
    stage: impl FnOnce(&Path, &SourceState) -> Result<PublishedBuild>,
) -> Result<DeterministicBuildOutcome> {
    // The build lock is the SAME lock the session-driven build uses, so the
    // deterministic executor and a session's build never compile into the same
    // artifact directory at the same time.
    let Some(_lock) = BuildLockGuard::try_acquire(&source.worktree_scope)? else {
        return Ok(DeterministicBuildOutcome::Busy {
            reason: format!(
                "another build holds the lock for worktree scope {}",
                source.worktree_scope
            ),
        });
    };

    if let Err(error) = compile() {
        return Ok(classify_error(repo_dir, source, error));
    }

    // The drift check: the compile is bound to the source state it started
    // from, and a binary built from a state that no longer exists must not be
    // published.
    let after = match ensure_source_state_matches(repo_dir, source) {
        Ok(after) => after,
        Err(error) => return Ok(classify_error(repo_dir, source, error)),
    };

    if find_dev_binary(repo_dir).is_none() {
        return Ok(DeterministicBuildOutcome::Failed {
            reason: "binary not found in target/selfdev or target/release".to_string(),
        });
    }
    write_current_dev_binary_source_metadata(repo_dir, &after)?;

    match stage(repo_dir, &after) {
        Ok(published) => Ok(DeterministicBuildOutcome::Staged {
            version: published.version,
            fingerprint: published.source_fingerprint,
            versioned_path: published.versioned_path,
        }),
        Err(error) => {
            let reason = format!("{error:#}");
            // The staging guard refuses a binary whose reported source does not
            // match the tree. That is a drift, not a broken build.
            if reason.contains("Refusing to publish") {
                return Ok(DeterministicBuildOutcome::Superseded {
                    expected_fingerprint: source.fingerprint.clone(),
                    current_fingerprint: current_fingerprint(repo_dir, source),
                    reason,
                });
            }
            Ok(DeterministicBuildOutcome::Failed { reason })
        }
    }
}

/// Turn an error into SUPERSEDED when the tree moved, and keep it a failure
/// otherwise.
///
/// A compile that failed because the tree moved is not a broken build: the
/// binary it produced describes a source state that no longer exists.
fn classify_error(
    repo_dir: &Path,
    source: &SourceState,
    error: anyhow::Error,
) -> DeterministicBuildOutcome {
    let reason = format!("{error:#}");
    // A locked target binary is not a broken build: the code may compile
    // perfectly, but the compiler could not replace the running image.
    if is_lock_error(&reason) {
        return DeterministicBuildOutcome::Locked { reason };
    }
    let current = current_fingerprint(repo_dir, source);
    if current != source.fingerprint {
        return DeterministicBuildOutcome::Superseded {
            expected_fingerprint: source.fingerprint.clone(),
            current_fingerprint: current,
            reason,
        };
    }
    DeterministicBuildOutcome::Failed { reason }
}

/// Whether a compile error is the measured "the target binary is held open"
/// failure rather than a compilation error.
///
/// The measured error text is cargo's "failed to remove file <path>" followed
/// by "Отказано в доступе. (os error 5)" / "Access is denied. (os error 5)".
fn is_lock_error(reason: &str) -> bool {
    let lowered = reason.to_lowercase();
    (lowered.contains("failed to remove file") || lowered.contains("permission denied"))
        && (lowered.contains("os error 5") || lowered.contains("access is denied"))
}

/// The current tree's fingerprint, or the expected one when the tree cannot be
/// read (in which case there is no evidence of a drift).
fn current_fingerprint(repo_dir: &Path, source: &SourceState) -> String {
    current_source_state(repo_dir)
        .map(|current| current.fingerprint)
        .unwrap_or_else(|_| source.fingerprint.clone())
}
