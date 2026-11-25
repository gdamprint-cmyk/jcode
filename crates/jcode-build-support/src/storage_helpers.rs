// reference: in-repo prior art - `read_current_version` /
// `read_shared_server_version` in this same file (the marker-file pattern:
// one file per channel under `builds_dir()`, holding the channel's version
// label, written when the channel moves). `staged_version_file` follows that
// exact pattern for the staging marker.
use super::{MigrationContext, binary_name};
use anyhow::Result;
use jcode_storage as storage;
use std::path::PathBuf;

/// Get path to builds directory
pub fn builds_dir() -> Result<PathBuf> {
    let dir = resolve_builds_dir(
        std::env::var_os("JCODE_HOME").map(PathBuf::from),
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        storage::jcode_dir()?,
        cfg!(windows),
    );
    storage::ensure_dir(&dir)?;
    Ok(dir)
}

fn resolve_builds_dir(
    jcode_home: Option<PathBuf>,
    local_app_data: Option<PathBuf>,
    default_jcode_dir: PathBuf,
    is_windows: bool,
) -> PathBuf {
    if let Some(jcode_home) = jcode_home {
        return jcode_home.join("builds");
    }

    if is_windows && let Some(local_app_data) = local_app_data {
        // Keep runtime channel discovery aligned with scripts/install.ps1 and
        // the supported Windows layout under %LOCALAPPDATA%\jcode\builds.
        // Durable user state and logs still live under ~/.jcode.
        return local_app_data.join("jcode").join("builds");
    }

    default_jcode_dir.join("builds")
}

/// Get path to build manifest
pub fn manifest_path() -> Result<PathBuf> {
    Ok(builds_dir()?.join("manifest.json"))
}

/// Get path to the APPEND-ONLY publication ledger.
///
/// The ledger records every version that was PUBLISHED, one JSON object per
/// line, and is never truncated or garbage collected.
///
/// WHY IT EXISTS: the manifest history truncates to 20 rows
/// (`BuildManifest::add_to_history`) and `builds/versions/` is garbage
/// collected by the daily version GC
/// (`crates/jcode-build-support/src/version_gc.rs`). Both are lossy under
/// exactly the operation a landing performs (a daemon restart). Measured
/// 2026-11-16 on this host: a landing that restarted the daemon ran that GC,
/// which removed 9 version directories and evicted 2 more rows from the
/// truncated history, so four Harbor queue entries from other sessions lost
/// their ONLY publication proof and the acceptance checker reported 8 failures
/// for fixes that were demonstrably live.
///
/// This ledger is the durable proof that a version label was installed. It is
/// appended at the SINGLE point where a version directory is created
/// (`install_binary_at_version`), so every publishing path that goes through
/// that function - the session build request, the debug `reload`, the self-dev
/// `reload`, `/rebuild`, the release updater and `landing::stage_build` -
/// leaves a record. The session build request additionally writes a
/// build-request record, but the two reload paths did not, which left one
/// published version (`ac6859a22-dirty-a2b2105c06c0`) provable only by its
/// garbage-collectible directory.
///
/// The tarball release path (`crates/jcode-app-core/src/update.rs`) installs
/// the binary DIRECTLY, without going through `install_binary_at_version`, so
/// it records its row explicitly.
///
/// A row without `validated: true` proves INSTALLATION only: the ledger row is
/// written before the post-install smoke test and hash check. The staging path
/// appends a second, validated row once those pass.
///
/// reference: in-repo prior art - the marker-file pattern of
/// `read_current_version` in this same file, and the append-only semantics of
/// the session journal (`crates/jcode-base/src/session/persistence.rs`).
pub fn publication_ledger_path() -> Result<PathBuf> {
    Ok(builds_dir()?.join("published-versions.jsonl"))
}

/// Get path to a specific version's binary
pub fn version_binary_path(hash: &str) -> Result<PathBuf> {
    Ok(builds_dir()?
        .join("versions")
        .join(hash)
        .join(binary_name()))
}

/// Get path to stable symlink
pub fn stable_binary_path() -> Result<PathBuf> {
    Ok(builds_dir()?.join("stable").join(binary_name()))
}

/// Get path to current symlink (active local build channel)
pub fn current_binary_path() -> Result<PathBuf> {
    Ok(builds_dir()?.join("current").join(binary_name()))
}

/// Get path to the shared server symlink (approved daemon channel).
pub fn shared_server_binary_path() -> Result<PathBuf> {
    Ok(builds_dir()?.join("shared-server").join(binary_name()))
}

/// Get path to canary binary
pub fn canary_binary_path() -> Result<PathBuf> {
    Ok(builds_dir()?.join("canary").join(binary_name()))
}

/// Get path to migration context file
pub fn migration_context_path(session_id: &str) -> Result<PathBuf> {
    Ok(builds_dir()?
        .join("migrations")
        .join(format!("{}.json", session_id)))
}

/// Get path to stable version file (watched by other sessions)
pub fn stable_version_file() -> Result<PathBuf> {
    Ok(builds_dir()?.join("stable-version"))
}

/// Get path to current version file (active local build marker).
pub fn current_version_file() -> Result<PathBuf> {
    Ok(builds_dir()?.join("current-version"))
}

/// Get path to the shared server version file (approved daemon marker).
pub fn shared_server_version_file() -> Result<PathBuf> {
    Ok(builds_dir()?.join("shared-server-version"))
}

/// Get path to the staged version file (the newest BUILT build).
///
/// The staged marker is what makes a BUILD staging-only. A build installs
/// its immutable versioned binary and records the label here; it does NOT
/// touch the `current` channel or the launcher. The landing then moves every
/// channel onto the staged build in one transaction, so a client can never
/// attach to a daemon running a different build.
///
/// Before this marker existed, the `current` channel doubled as "the newest
/// build" and "the build the clients run", and those two are different
/// things: the first is a build output, the second is a release decision.
pub fn staged_version_file() -> Result<PathBuf> {
    Ok(builds_dir()?.join("staged-version"))
}

/// Save migration context before switching to canary
pub fn save_migration_context(ctx: &MigrationContext) -> Result<()> {
    let path = migration_context_path(&ctx.session_id)?;
    storage::write_json(&path, ctx)
}

/// Load migration context
pub fn load_migration_context(session_id: &str) -> Result<Option<MigrationContext>> {
    let path = migration_context_path(session_id)?;
    if path.exists() {
        Ok(Some(storage::read_json(&path)?))
    } else {
        Ok(None)
    }
}

/// Clear migration context after successful migration
pub fn clear_migration_context(session_id: &str) -> Result<()> {
    let path = migration_context_path(session_id)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Read the current stable version
pub fn read_stable_version() -> Result<Option<String>> {
    let path = stable_version_file()?;
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        let hash = content.trim();
        if hash.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hash.to_string()))
        }
    } else {
        Ok(None)
    }
}

/// Read the current active version.
pub fn read_current_version() -> Result<Option<String>> {
    let path = current_version_file()?;
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        let hash = content.trim();
        if hash.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hash.to_string()))
        }
    } else {
        Ok(None)
    }
}

/// Read the current shared-server version.
pub fn read_shared_server_version() -> Result<Option<String>> {
    let path = shared_server_version_file()?;
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        let hash = content.trim();
        if hash.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hash.to_string()))
        }
    } else {
        Ok(None)
    }
}

/// Record the version a BUILD produced, without moving any channel.
///
/// This is the staging half of the landing split. The build's output becomes
/// the landing's TARGET, but it does not become the build the clients run:
/// that happens only when the landing commits.
pub fn write_staged_version(version: &str) -> Result<()> {
    let path = staged_version_file()?;
    std::fs::write(path, version)?;
    Ok(())
}

/// Read the version of the newest STAGED build, if any.
pub fn read_staged_version() -> Result<Option<String>> {
    let path = staged_version_file()?;
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        let hash = content.trim();
        if hash.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hash.to_string()))
        }
    } else {
        Ok(None)
    }
}

/// Get path to build log file
pub fn build_log_path() -> Result<PathBuf> {
    Ok(storage::jcode_dir()?.join("build.log"))
}

/// Get path to build progress file (for TUI to watch)
pub fn build_progress_path() -> Result<PathBuf> {
    Ok(storage::jcode_dir()?.join("build-progress"))
}

/// Write current build progress (for TUI to display)
pub fn write_build_progress(status: &str) -> Result<()> {
    let path = build_progress_path()?;
    std::fs::write(&path, status)?;
    invalidate_build_progress_cache();
    Ok(())
}

/// Process-local cache for `read_build_progress`. Stores the last-read value
/// alongside the time it was read so per-frame TUI calls can be served without
/// a disk hit.
static BUILD_PROGRESS_CACHE: std::sync::Mutex<Option<(std::time::Instant, Option<String>)>> =
    std::sync::Mutex::new(None);

const BUILD_PROGRESS_TTL: std::time::Duration = std::time::Duration::from_millis(100);

fn invalidate_build_progress_cache() {
    if let Ok(mut guard) = BUILD_PROGRESS_CACHE.lock() {
        *guard = None;
    }
}

/// Read current build progress.
///
/// The TUI calls this from its per-frame redraw scheduler (several times per
/// frame, across every connected client), so a naive implementation performs a
/// synchronous disk read on every render tick even when no build is running.
/// Build progress is a purely cosmetic status string, so we cache the result
/// for a short window. The cache is invalidated immediately on
/// `write_build_progress`/`clear_build_progress` so progress still updates
/// promptly when a build is driven from the same process; cross-process updates
/// become visible within the TTL.
pub fn read_build_progress() -> Option<String> {
    if let Ok(guard) = BUILD_PROGRESS_CACHE.lock()
        && let Some((at, ref value)) = *guard
        && at.elapsed() < BUILD_PROGRESS_TTL
    {
        return value.clone();
    }

    let value = read_build_progress_uncached();

    if let Ok(mut guard) = BUILD_PROGRESS_CACHE.lock() {
        *guard = Some((std::time::Instant::now(), value.clone()));
    }

    value
}

fn read_build_progress_uncached() -> Option<String> {
    build_progress_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Clear build progress
pub fn clear_build_progress() -> Result<()> {
    let path = build_progress_path()?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    invalidate_build_progress_cache();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve_builds_dir;
    use std::path::PathBuf;

    #[test]
    fn windows_builds_use_local_app_data() {
        let resolved = resolve_builds_dir(
            None,
            Some(PathBuf::from("/local-app-data")),
            PathBuf::from("/home/test/.jcode"),
            true,
        );

        assert_eq!(resolved, PathBuf::from("/local-app-data/jcode/builds"));
    }

    #[test]
    fn jcode_home_override_wins_on_windows() {
        let resolved = resolve_builds_dir(
            Some(PathBuf::from("/isolated-jcode")),
            Some(PathBuf::from("/local-app-data")),
            PathBuf::from("/home/test/.jcode"),
            true,
        );

        assert_eq!(resolved, PathBuf::from("/isolated-jcode/builds"));
    }

    #[test]
    fn non_windows_builds_stay_under_jcode_home() {
        let resolved = resolve_builds_dir(
            None,
            Some(PathBuf::from("/ignored/local-app-data")),
            PathBuf::from("/home/test/.jcode"),
            false,
        );

        assert_eq!(resolved, PathBuf::from("/home/test/.jcode/builds"));
    }
}
