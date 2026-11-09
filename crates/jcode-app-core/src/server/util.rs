use crate::build;
use std::collections::HashSet;
// reference: no prior art, searched "reload self-exec guard AlreadyCurrent" in crates/jcode-app-core/src and crates/jcode-build-support/src (only hit is build::SharedServerRepair::AlreadyCurrent, a different concern)
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::OnceCell;

/// Default embedding idle unload threshold. The local MiniLM runtime adds about
/// 150 MiB of resident memory after its first query, while loading it takes only
/// about 200 ms and memory retrieval runs off the interactive turn path. Keep it
/// warm for short bursts, but do not pin it through long quiet periods.
const EMBEDDING_IDLE_UNLOAD_DEFAULT_SECS: u64 = 60;

pub(crate) fn debug_control_allowed() -> bool {
    // Check config file setting
    if crate::config::config().display.debug_socket {
        return true;
    }
    if std::env::var("JCODE_DEBUG_CONTROL")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
    {
        return true;
    }
    // Check for file-based toggle (allows enabling without restart)
    if let Ok(jcode_dir) = crate::storage::jcode_dir()
        && jcode_dir.join("debug_control").exists()
    {
        return true;
    }
    false
}

pub(crate) fn embedding_idle_unload_secs() -> u64 {
    parse_embedding_idle_unload_secs(
        std::env::var("JCODE_EMBEDDING_IDLE_UNLOAD_SECS")
            .ok()
            .as_deref(),
    )
}

fn parse_embedding_idle_unload_secs(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(EMBEDDING_IDLE_UNLOAD_DEFAULT_SECS)
}

#[cfg(test)]
mod embedding_idle_tests {
    use super::*;

    #[test]
    fn idle_unload_defaults_to_one_minute_and_accepts_positive_override() {
        assert_eq!(parse_embedding_idle_unload_secs(None), 60);
        assert_eq!(parse_embedding_idle_unload_secs(Some("15")), 15);
        assert_eq!(parse_embedding_idle_unload_secs(Some("0")), 60);
        assert_eq!(parse_embedding_idle_unload_secs(Some("invalid")), 60);
    }
}

pub(crate) async fn get_shared_mcp_pool(
    cell: &OnceCell<Arc<crate::mcp::SharedMcpPool>>,
) -> Arc<crate::mcp::SharedMcpPool> {
    cell.get_or_init(|| async { Arc::new(crate::mcp::SharedMcpPool::from_default_config()) })
        .await
        .clone()
}

pub(crate) fn server_update_candidate(is_selfdev_session: bool) -> Option<(PathBuf, &'static str)> {
    build::shared_server_update_candidate(is_selfdev_session)
}

/// Resolve the binary the reload should actually exec into, with a hard
/// no-downgrade guard.
///
/// `server_update_candidate` can legitimately return an *older* binary (e.g. a
/// `shared-server` channel that an update never advanced, or a leftover self-dev
/// promotion synced from another machine). A forced reload bypasses
/// `server_has_newer_binary`, so without this guard it would silently exec into
/// that older binary and downgrade every connected client.
///
/// We never block a same-or-newer candidate (so self-dev builds, which are
/// freshly written and therefore newer by mtime, still apply). When the
/// candidate is *strictly older* than the running executable we refuse it and
/// re-exec into the current executable instead: same code, fresh process and
/// socket handoff, but no downgrade. Any mtime uncertainty is treated as "do
/// not downgrade".
///
/// Crucially, the candidate is the *newest* reload candidate across BOTH
/// self-dev flavors, not just the one matching `is_selfdev_session`. This keeps
/// the reload target consistent with `server_has_newer_binary`, which also scans
/// both flavors. Without this, a self-dev/canary daemon whose `shared-server`
/// channel is pinned to an *old* self-dev build would advertise
/// `server_has_update = true` (the normal-flavor probe self-heals to the freshly
/// installed release) yet reload into that same old pinned build -> the server
/// reports an update it can never apply, so the client upgrades while the server
/// stays stale and the auto-reload loops until it is suppressed. Selecting the
/// newest candidate across flavors still preserves a deliberately-pinned self-dev
/// build whenever that build is the freshest one on disk (the case the pin is
/// meant to protect).
pub(crate) fn reload_exec_target(is_selfdev_session: bool) -> Option<(PathBuf, &'static str)> {
    let candidate = newest_reload_candidate(is_selfdev_session)?;
    // On Linux a self-dev rebuild rewrites the running binary in place (a dirty
    // build reuses the same `versions/<hash>` path), which unlinks the running
    // inode. `current_exe()` then resolves `/proc/self/exe` to a path with a
    // trailing " (deleted)" marker that is NOT a real file. If we keep that
    // marker we (a) fail the "same binary" fast-path below, (b) read no mtime so
    // the freshly-built candidate looks like a downgrade, and (c) fall back to
    // re-execing the bogus " (deleted)" path, which does not exist -> the server
    // exits without a replacement and strands every connected client. Strip the
    // marker so we compare against (and can re-exec) the real on-disk path.
    let current_exe = running_server_binary();

    // Identity/mtime comparisons must look through release wrapper scripts to
    // the payload that actually runs (see `build::resolve_binary_payload`):
    // the running exe is the `.bin` payload while channel candidates are tiny
    // wrapper scripts, and comparing wrapper-vs-payload mtimes turned every
    // release install into a phantom "downgrade"/"update". The exec target
    // stays the original candidate path (the wrapper), which is what sets up
    // `LD_LIBRARY_PATH` correctly.
    let candidate_canonical = build::resolve_binary_payload(&candidate.0);
    let current_canonical = current_exe
        .as_ref()
        .map(|p| build::resolve_binary_payload(p));

    // The running binary's identity and mtime are PINNED at server start (see
    // `running_binary_snapshot`). On Windows a channel update replaces the
    // binary in place at the same path, so re-reading the path now would
    // report the NEW file's identity and mtime - the candidate would look
    // identical to "what we run" (refusing every real upgrade) and its mtime
    // would compare equal to ours (never advertising an update).
    let snapshot = running_binary_snapshot();
    let current_mtime = snapshot.mtime;
    let current_identity = snapshot.identity;
    let candidate_identity = binary_identity(candidate_canonical.as_path());
    let candidate_mtime = binary_mtime(candidate_canonical.as_path());

    match guarded_reload_target(
        candidate.clone(),
        candidate_canonical.as_path(),
        candidate_identity,
        current_exe.as_deref(),
        current_canonical.as_deref(),
        current_identity,
        current_mtime,
        candidate_mtime,
    ) {
        ReloadTargetDecision::UseCandidate(target) => Some(target),
        // reference: no prior art, searched "reload self-exec guard AlreadyCurrent" in crates/jcode-app-core/src and crates/jcode-build-support/src (only hit is build::SharedServerRepair::AlreadyCurrent, a different concern)
        ReloadTargetDecision::AlreadyCurrent => {
            // The candidate IS the binary we are already running. Exec'ing into
            // ourselves is a self-exec: it tears the daemon down and re-execs
            // the same image, which is exactly the state the client asked to
            // leave. Refuse the reload instead.
            crate::logging::warn(
                "reload exec guard: candidate is the running binary; refusing self-exec",
            );
            None
        }
        ReloadTargetDecision::DowngradeBlockedUseCurrent(target) => {
            // Never strand clients by re-execing a binary that is gone from disk.
            // If the running exe was unlinked (e.g. an in-place rebuild) but the
            // candidate still exists, prefer the candidate over refusing to
            // reload. The candidate may be older, but a live downgrade beats a
            // dead server with no replacement.
            if !target.0.exists() && candidate_canonical.exists() {
                crate::logging::warn(&format!(
                    "reload downgrade guard: current binary {:?} is missing on disk; falling back to candidate {:?} to avoid stranding clients",
                    target.0, candidate.0,
                ));
                return Some(candidate);
            }
            // Re-execing the current binary is a self-exec: same image, same
            // code, and the daemon dies in the middle of it. The reload has
            // nothing to apply, so it is a no-op instead.
            crate::logging::warn(&format!(
                "reload downgrade guard: refusing to exec into older candidate {:?}; no-op instead of re-execing current binary {:?}",
                candidate.0, target.0,
            ));
            None
        }
        ReloadTargetDecision::DowngradeUnverifiable(target) => {
            crate::logging::warn(&format!(
                "reload downgrade guard: older candidate {:?} detected but current exe is unavailable; proceeding with candidate",
                target.0,
            ));
            Some(target)
        }
    }
}

fn binary_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Stable identity of a file *object*, independent of the path that names it.
///
/// Unix uses (device, inode); Windows uses (volume serial number, file index).
/// Both identify the file itself, so they survive a rename and distinguish two
/// different files that happen to sit at the same path over time.
///
/// This matters on Windows: the channel file `builds/shared-server/jcode.exe`
/// is a real file that a channel update replaces IN PLACE (the publisher
/// renames the old entry aside and moves the new one in - see
/// `jcode_build_support::platform_support::atomic_symlink_swap`). Comparing
/// paths therefore cannot tell "the binary I am running" from "the new build
/// that just replaced it at the same path" - only the file identity can.
///
/// reference: Windows file identity via
/// `GetFileInformationByHandle` -> `BY_HANDLE_FILE_INFORMATION`
/// (dwVolumeSerialNumber, nFileIndexHigh/nFileIndexLow) from `windows-sys`
/// 0.59, `Win32_Storage_FileSystem`; the unix (device, inode) pair follows this
/// repo's existing `jcode_app_core::server::runtime_upgrade::FileIdentity`
/// (dev_inode). No prior art for the Windows half exists in this repo: searched
/// "GetFileInformationByHandle" / "BY_HANDLE_FILE_INFORMATION" across
/// crates/*/src (0 hits), and "file_index" / "volume_serial_number" in
/// crates/jcode-app-core/src and crates/jcode-base/src (0 hits).
/// std's `std::os::windows::fs::MetadataExt::file_index` /
/// ::volume_serial_number were rejected: they are gated behind the unstable
/// `windows_by_handle` feature (cargo check: error[E0658]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BinaryIdentity {
    volume: u64,
    index: u64,
}

fn binary_identity(path: &Path) -> Option<BinaryIdentity> {
    #[cfg(unix)]
    {
        let metadata = std::fs::metadata(path).ok()?;
        use std::os::unix::fs::MetadataExt;
        Some(BinaryIdentity {
            volume: metadata.dev(),
            index: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        windows_file_identity(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

/// Windows file identity: (volume serial number, file index).
///
/// reference: `GetFileInformationByHandle` with
/// `BY_HANDLE_FILE_INFORMATION` (windows-sys 0.59,
/// `Win32_Storage_FileSystem`). std's `MetadataExt::file_index` /
/// ::volume_serial_number are unstable (`windows_by_handle`), so the Win32 API
/// is called directly.
///
/// The handle comes from `std::fs::File` (via `AsRawHandle`): std opens files
/// on Windows with `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`,
/// which is exactly what is needed to inspect a binary that a channel update
/// replaces in place, and it closes the handle for us.
#[cfg(windows)]
fn windows_file_identity(path: &Path) -> Option<BinaryIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let file = std::fs::File::open(path).ok()?;
    let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;

    // SAFETY: `info` points at a writable, correctly sized buffer for the
    // duration of the call below.
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: `info` is a correctly sized output buffer and `handle` is a live
    // handle owned by `file`, which outlives the call.
    let ok = unsafe { GetFileInformationByHandle(handle, info.as_mut_ptr()) } != 0;
    if !ok {
        return None;
    }
    // SAFETY: `GetFileInformationByHandle` returned non-zero, so the output
    // buffer was fully initialized.
    let info = unsafe { info.assume_init() };

    let index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
    Some(BinaryIdentity {
        volume: info.dwVolumeSerialNumber as u64,
        index,
    })
}

/// The binary this daemon runs, captured once at server start.
///
/// Identity and mtime are pinned at startup on purpose. After a channel update
/// replaces the file at the same path, re-reading the path yields the NEW
/// file's identity and mtime. That would make a legitimate upgrade look like
/// "already running the candidate" (refused reload) and would make the update
/// probe compare the new file against itself (no update ever advertised).
#[derive(Clone, Debug)]
pub(crate) struct RunningBinarySnapshot {
    pub path: PathBuf,
    pub identity: Option<BinaryIdentity>,
    pub mtime: Option<std::time::SystemTime>,
}

static RUNNING_BINARY_SNAPSHOT: std::sync::OnceLock<RunningBinarySnapshot> =
    std::sync::OnceLock::new();

pub(crate) fn running_binary_snapshot() -> &'static RunningBinarySnapshot {
    RUNNING_BINARY_SNAPSHOT.get_or_init(|| {
        let path = running_server_binary().unwrap_or_default();
        let identity = binary_identity(&path);
        let mtime = binary_mtime(&path);
        RunningBinarySnapshot {
            path,
            identity,
            mtime,
        }
    })
}

/// Pick the newest reload candidate across BOTH self-dev flavors.
///
/// The session's own flavor (`is_selfdev_session`) is evaluated first so it wins
/// any exact-mtime tie, preserving self-dev semantics: a deliberately-pinned
/// self-dev `shared-server` build is honored whenever it is at least as fresh as
/// the other flavor's candidate. The other flavor only wins when it is
/// *strictly newer*, which is exactly the situation that makes
/// `server_has_newer_binary` report an update (e.g. `/update` installed a newer
/// release while the self-dev pin stayed on an older build).
fn newest_reload_candidate(is_selfdev_session: bool) -> Option<(PathBuf, &'static str)> {
    let ordered = [
        server_update_candidate(is_selfdev_session),
        server_update_candidate(!is_selfdev_session),
    ];
    let with_mtimes = ordered.into_iter().flatten().map(|candidate| {
        // Compare payloads, not release wrapper scripts (whose mtimes carry no
        // version information). Dedup also happens on the payload so a wrapper
        // and its payload never count as two distinct candidates.
        let canonical = build::resolve_binary_payload(&candidate.0);
        let mtime = binary_mtime(canonical.as_path());
        (candidate, canonical, mtime)
    });
    pick_newest_candidate(with_mtimes)
}

/// Pure, order-sensitive "newest candidate" selection used by
/// [`newest_reload_candidate`]. Candidates are provided in *preference order*
/// (the session's own flavor first). A later candidate only displaces an earlier
/// one when it is provably, strictly newer by mtime, so equal/unknown mtimes
/// never demote the higher-preference flavor (protecting a self-dev pin on a
/// tie). Canonical-path duplicates are collapsed to the first occurrence.
fn pick_newest_candidate(
    candidates: impl IntoIterator<
        Item = (
            (PathBuf, &'static str),
            PathBuf,
            Option<std::time::SystemTime>,
        ),
    >,
) -> Option<(PathBuf, &'static str)> {
    let mut best: Option<((PathBuf, &'static str), Option<std::time::SystemTime>)> = None;
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for (candidate, canonical, mtime) in candidates {
        if !seen.insert(canonical) {
            continue;
        }
        let replace = match (&best, mtime) {
            (None, _) => true,
            (Some((_, Some(best_mtime))), Some(new_mtime)) => new_mtime > *best_mtime,
            (Some((_, None)), Some(_)) => true,
            (Some(_), None) => false,
        };
        if replace {
            best = Some((candidate, mtime));
        }
    }
    best.map(|(candidate, _)| candidate)
}

#[derive(Debug)]
enum ReloadTargetDecision {
    UseCandidate((PathBuf, &'static str)),
    /// The candidate is the very binary we are running: exec'ing it would be a
    /// self-exec, so the reload must not happen.
    AlreadyCurrent,
    DowngradeBlockedUseCurrent((PathBuf, &'static str)),
    DowngradeUnverifiable((PathBuf, &'static str)),
}

/// Pure no-downgrade decision used by [`reload_exec_target`]. A candidate is
/// accepted unless it is strictly older than (or not provably as new as) the
/// running executable, in which case we prefer re-execing the current binary.
///
/// `candidate_identity` / `current_identity` identify the FILE OBJECTS, not the
/// paths. On Windows the channel file is replaced in place at the same path
/// (`atomic_symlink_swap` renames the old entry aside), so a path comparison
/// cannot tell a legitimate upgrade ("same path, new file") from a self-exec
/// ("same path, same file"). Only the file identity can.
///
/// reference: std::os::windows::fs::MetadataExt::file_index /
/// ::volume_serial_number (Windows file identity, std docs); unix (dev, ino)
/// per this repo's `jcode_app_core::server::runtime_upgrade::FileIdentity`.
/// No prior art for this guard exists in this repo: searched "reload self-exec
/// guard AlreadyCurrent" in crates/jcode-app-core/src and
/// crates/jcode-build-support/src (only hit is
/// build::SharedServerRepair::AlreadyCurrent, a different concern).
fn guarded_reload_target(
    candidate: (PathBuf, &'static str),
    candidate_canonical: &Path,
    candidate_identity: Option<BinaryIdentity>,
    current_exe: Option<&Path>,
    current_canonical: Option<&Path>,
    current_identity: Option<BinaryIdentity>,
    current_mtime: Option<std::time::SystemTime>,
    candidate_mtime: Option<std::time::SystemTime>,
) -> ReloadTargetDecision {
    // Reloading into the very file we are running is a self-exec: it would tear
    // the daemon down and re-exec the same image. Refuse it.
    //
    // The check is by file identity, not by path: on Windows a channel update
    // replaces the binary IN PLACE, so the same path legitimately holds a new
    // file that must be exec'd, while the unchanged file at that path is a
    // self-exec. Comparing paths would refuse every real upgrade on Windows.
    let same_file = match (candidate_identity, current_identity) {
        (Some(candidate_identity), Some(current_identity)) => {
            candidate_identity == current_identity
        }
        // Identity unavailable: fall back to the path comparison, which is
        // correct on Unix (the running exe is a symlink target and the
        // candidate resolves to the same inode path) and conservative on
        // Windows (it can only refuse an upgrade, never allow a self-exec
        // through a mismatching path).
        _ => current_canonical == Some(candidate_canonical),
    };
    if same_file {
        return ReloadTargetDecision::AlreadyCurrent;
    }

    let candidate_is_strictly_older = match (current_mtime, candidate_mtime) {
        (Some(current), Some(cand)) => cand < current,
        // Unknown mtimes: be conservative and treat as a potential downgrade so
        // we never silently swap to an unverifiable binary on a forced reload.
        _ => true,
    };

    if !candidate_is_strictly_older {
        return ReloadTargetDecision::UseCandidate(candidate);
    }

    match current_exe {
        Some(current_exe) => ReloadTargetDecision::DowngradeBlockedUseCurrent((
            current_exe.to_path_buf(),
            "current-exe (downgrade-guard)",
        )),
        None => ReloadTargetDecision::DowngradeUnverifiable(candidate),
    }
}

fn canonicalize_or(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

/// Strip the Linux `/proc/self/exe` " (deleted)" marker that appears when the
/// running binary has been unlinked or replaced in place. The marker is part of
/// the readlink target, not the real filename, so removing it recovers the path
/// that may now point at the freshly written replacement binary.
fn strip_deleted_suffix(path: PathBuf) -> PathBuf {
    const DELETED_MARKER: &str = " (deleted)";
    if let Some(stripped) = path.to_str().and_then(|s| s.strip_suffix(DELETED_MARKER)) {
        return PathBuf::from(stripped);
    }
    path
}

/// The real file this process was started from.
///
/// On macOS `current_exe()` returns the path the process was launched by,
/// symlinks included. The shared server is started through a channel link
/// (`builds/stable/jcode`), and installing a new build repoints that link. From
/// then on `current_exe()` resolves to the *new* binary, so every reload check
/// concluded the server already ran the newest build and `jcode server reload`
/// was skipped forever. Resolve the path once, at first use (server startup),
/// before any later install can move the link.
pub(crate) fn running_server_binary() -> Option<PathBuf> {
    static RUNNING: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    RUNNING
        .get_or_init(|| std::env::current_exe().ok().map(resolve_launch_path))
        .clone()
}

/// Follow channel symlinks in a launch path to the file it points at now.
fn resolve_launch_path(exe: PathBuf) -> PathBuf {
    let exe = strip_deleted_suffix(exe);
    std::fs::canonicalize(&exe).unwrap_or(exe)
}

#[cfg(all(test, unix))]
mod running_server_binary_tests {
    /// The server is started through a channel link; installing a new build
    /// repoints the link. The pinned path must stay on the binary the server
    /// was started from, so the new build counts as newer and reload runs.
    #[test]
    fn launch_path_pinned_before_the_channel_link_moves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("versions/old/jcode");
        let new = dir.path().join("versions/new/jcode");
        for binary in [&old, &new] {
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(binary, b"bin").unwrap();
        }
        let link = dir.path().join("stable/jcode");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&old, &link).unwrap();

        // Server start: what running_server_binary pins.
        let pinned = super::resolve_launch_path(link.clone());
        // An install repoints the channel.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&new, &link).unwrap();

        assert_eq!(pinned, std::fs::canonicalize(&old).unwrap());
        // Re-resolving the launch path now (the old behavior) lands on the new
        // build, which is why reload thought nothing was newer.
        assert_eq!(
            super::resolve_launch_path(link),
            std::fs::canonicalize(&new).unwrap()
        );
    }
}

pub(crate) fn git_common_dir_for(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(dir) = current {
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            return Some(canonicalize_or(dotgit));
        }
        if dotgit.is_file() {
            let content = std::fs::read_to_string(&dotgit).ok()?;
            let gitdir_line = content
                .lines()
                .find(|line| line.trim_start().starts_with("gitdir:"))?;
            let raw = gitdir_line
                .trim_start()
                .trim_start_matches("gitdir:")
                .trim();
            if raw.is_empty() {
                return None;
            }
            let gitdir = if Path::new(raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                dir.join(raw)
            };
            let gitdir = canonicalize_or(gitdir);
            // Worktree gitdir looks like: <repo>/.git/worktrees/<name>
            if let Some(parent) = gitdir.parent()
                && parent.file_name().and_then(|s| s.to_str()) == Some("worktrees")
                && let Some(common) = parent.parent()
            {
                return Some(canonicalize_or(common.to_path_buf()));
            }
            return Some(gitdir);
        }
        current = dir.parent();
    }
    None
}

pub(crate) fn swarm_id_for_dir(dir: Option<PathBuf>) -> Option<String> {
    if let Ok(sw_id) = std::env::var("JCODE_SWARM_ID") {
        let trimmed = sw_id.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let dir = dir?;
    if let Some(git_common) = git_common_dir_for(&dir) {
        return Some(git_common.to_string_lossy().to_string());
    }
    Some(dir.to_string_lossy().to_string())
}

/// Return the swarm identity for an independently-created root session.
///
/// Swarm plans are keyed by swarm id. Deriving that id from the working
/// directory made every session opened in one repository share one plan, even
/// when those sessions were unrelated. Root sessions therefore own a swarm by
/// default. `JCODE_SWARM_ID` remains an explicit opt-in to a shared swarm.
pub(crate) fn swarm_id_for_session(session_id: &str) -> Option<String> {
    if let Ok(sw_id) = std::env::var("JCODE_SWARM_ID") {
        let trimmed = sw_id.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    default_swarm_id_for_session(session_id)
}

pub(super) fn default_swarm_id_for_session(session_id: &str) -> Option<String> {
    if session_id.trim().is_empty() {
        None
    } else {
        Some(format!("session:{session_id}"))
    }
}

#[cfg(test)]
mod swarm_identity_tests {
    use super::default_swarm_id_for_session;

    #[test]
    fn independent_root_sessions_have_distinct_swarm_ids() {
        assert_eq!(
            default_swarm_id_for_session("session-one").as_deref(),
            Some("session:session-one")
        );
        assert_eq!(
            default_swarm_id_for_session("session-two").as_deref(),
            Some("session:session-two")
        );
        assert_ne!(
            default_swarm_id_for_session("session-one"),
            default_swarm_id_for_session("session-two")
        );
    }

    #[test]
    fn empty_session_cannot_own_a_swarm() {
        assert_eq!(default_swarm_id_for_session("  "), None);
    }
}

/// Decide whether any reload candidate is *provably* newer than the running
/// server binary.
///
/// This is intentionally conservative. An earlier version reported "update
/// available" whenever the mtime comparison was inconclusive (e.g. a metadata
/// read failed) as long as the candidate path differed from the running exe.
/// On some systems that fallback fired permanently, so the client would
/// auto-reload the server, the server would exec into the candidate, and the
/// freshly-exec'd server would again report an update -> an infinite reload
/// loop that flickers the terminal (see issue #277).
///
/// We now only report an update when we can read both mtimes and the candidate
/// is strictly newer than the running binary. Any uncertainty suppresses the
/// auto-reload signal so it can never wedge the client into a loop.
///
/// `current_identity` and each candidate's identity identify the FILE OBJECTS.
/// On Windows the channel file is replaced IN PLACE at the same path, so a path
/// comparison cannot distinguish "a new build just landed here" (an update)
/// from "the file we are already running" (a self-exec). Only the file identity
/// can, so the identities are passed in explicitly instead of being derived
/// from paths here.
///
/// reference: Windows file identity via `GetFileInformationByHandle`
/// (windows-sys 0.59, `Win32_Storage_FileSystem`); unix (dev, ino) per this
/// repo's `jcode_app_core::server::runtime_upgrade::FileIdentity` (dev_inode).
/// No prior art for a file-identity-based update probe exists in this repo:
/// searched "GetFileInformationByHandle" / "BY_HANDLE_FILE_INFORMATION" across
/// crates/*/src (0 hits), and "file_index" / "volume_serial_number" in
/// crates/jcode-app-core/src and crates/jcode-base/src (0 hits).
fn newer_binary_available(
    current_mtime: Option<std::time::SystemTime>,
    current_canonical: Option<&Path>,
    current_identity: Option<BinaryIdentity>,
    candidates: impl IntoIterator<
        Item = (
            PathBuf,
            Option<std::time::SystemTime>,
            Option<BinaryIdentity>,
        ),
    >,
) -> bool {
    let Some(current_time) = current_mtime else {
        crate::logging::warn(
            "server_has_newer_binary: current executable mtime unavailable; suppressing auto-reload update signal",
        );
        return false;
    };

    candidates
        .into_iter()
        .any(|(candidate, candidate_mtime, candidate_identity)| {
            // Reloading into ourselves is never an "update". The check is by file
            // identity: on Windows the channel file is replaced in place, so the
            // same path can legitimately hold a NEW build (an update) while an
            // unchanged file at that path is a self-exec (not an update).
            let same_file = match (candidate_identity, current_identity) {
                (Some(candidate_identity), Some(current_identity)) => {
                    candidate_identity == current_identity
                }
                // Identity unavailable: fall back to the path comparison.
                _ => current_canonical == Some(candidate.as_path()),
            };
            if same_file {
                return false;
            }

            match candidate_mtime {
                Some(candidate_time) => candidate_time > current_time,
                None => {
                    crate::logging::warn(&format!(
                        "server_has_newer_binary: candidate mtime unavailable for {}; suppressing auto-reload update signal",
                        candidate.display()
                    ));
                    false
                }
            }
        })
}

pub(crate) fn server_has_newer_binary() -> bool {
    // Directional check only: report an update solely when a reload *candidate*
    // binary is strictly newer than the binary we are running.
    //
    // We deliberately do NOT treat "my version differs from the installed
    // channel markers" as "I am outdated". That conflated *different* with
    // *older* and caused a real regression (issue #291): a newer self-dev /
    // shared-server daemon (e.g. v0.17.23-dev) running alongside an older
    // release client would be told to "reload" and downgrade itself, because
    // its git hash no longer matched the `current`/`stable` channel markers
    // after a release build moved them. It also fed the reload-loop family from
    // issue #277, since a server that merely "differs" can never make the
    // difference go away by reloading.
    //
    // `UPDATE_SEMVER` is the base Cargo version for every dev build, so it
    // cannot order two dev builds; binary mtime is the only robust, directional
    // signal we have. `newer_binary_available` compares candidate mtimes against
    // the running binary, excludes reloading into ourselves, and treats any
    // uncertainty (unreadable mtime) as "no update".
    //
    // Strip the Linux " (deleted)" marker (see `strip_deleted_suffix`) so an
    // in-place rebuild does not make the running binary's mtime unreadable and
    // suppress a legitimate update signal.
    //
    // All paths are resolved through `build::resolve_binary_payload` so release
    // installs (channel symlink -> wrapper script -> `.bin` payload) compare the
    // payload that actually runs. Comparing the wrapper script against the
    // running payload compared two different files with unrelated mtimes, which
    // could report a phantom update forever and wedge clients into an infinite
    // reload loop right after `/update`.
    let current_exe = running_server_binary();
    let current_canonical = current_exe
        .as_ref()
        .map(|path| build::resolve_binary_payload(path));
    // PINNED at server start: on Windows a channel update replaces the binary
    // in place at the same path, so re-reading the path now would yield the
    // NEW file's mtime and the candidate would compare equal to itself - the
    // probe could never advertise an update again.
    // reference: std::os::windows::fs::MetadataExt::file_index (std docs);
    // unix (dev, ino) per this repo's
    // `jcode_app_core::server::runtime_upgrade::FileIdentity` (dev_inode).
    let snapshot = running_binary_snapshot();
    let current_mtime = snapshot.mtime;
    let current_identity = snapshot.identity;

    // The probe must ask about the binary the reload would ACTUALLY exec into.
    //
    // Measuring every channel independently is what fed the reload loop: the probe
    // compared `current`'s mtime against the running binary and reported "update
    // available", while `reload_exec_target` -> `shared_server_update_candidate`
    // execs into the `shared-server` channel - which is the binary already running.
    // The daemon then advertised an update it could never apply, the client asked
    // for a reload, the daemon restarted ITSELF, and the condition stayed true
    // forever (each reload also gracefully shut every session down, so the client
    // lost its socket and reconnected into the same state).
    //
    // Measured on this host (2026-11-07): running `shared-server` mtime
    // 14:17:19, `current` mtime 15:55:52 -> "newer" -> TRUE, while all 10 logged
    // exec events targeted `shared-server` itself.
    //
    // So the probe judges EXACTLY ONE candidate: the one `reload_exec_target`
    // would exec into (the newest across both self-dev flavors, same helper).
    // Probe and exec can then never disagree about what "an update" means.
    let Some((candidate, _label)) = newest_reload_candidate(false) else {
        // No candidate at all: nothing to exec into, so nothing to update to.
        return false;
    };
    let candidate = build::resolve_binary_payload(&candidate);

    let candidates_with_mtimes = std::iter::once({
        let candidate_mtime = std::fs::metadata(&candidate)
            .ok()
            .and_then(|m| m.modified().ok());
        let candidate_identity = binary_identity(&candidate);
        (candidate, candidate_mtime, candidate_identity)
    });

    newer_binary_available(
        current_mtime,
        current_canonical.as_deref(),
        current_identity,
        candidates_with_mtimes,
    )
}

/// Server identity for multi-server support
#[derive(Debug, Clone)]
pub struct ServerIdentity {
    /// Full server ID (e.g., "server_blazing_1705012345678")
    pub id: String,
    /// Short name (e.g., "blazing")
    pub name: String,
    /// Icon for display (e.g., "🔥")
    pub icon: String,
    /// Git hash of the binary
    pub git_hash: String,
    /// Version string (e.g., "v0.1.123")
    pub version: String,
}

impl ServerIdentity {
    /// Display name with icon (e.g., "🔥 blazing")
    pub fn display_name(&self) -> String {
        format!("{} {}", self.icon, self.name)
    }
}

pub(crate) fn startup_headless_recovery_test_delay() -> Option<std::time::Duration> {
    let raw = std::env::var("JCODE_TEST_HEADLESS_STARTUP_RECOVERY_DELAY_MS").ok()?;
    let delay_ms = raw.trim().parse::<u64>().ok()?;
    (delay_ms > 0).then(|| std::time::Duration::from_millis(delay_ms))
}

#[cfg(test)]
mod newer_binary_tests {
    use super::{BinaryIdentity, newer_binary_available};
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    // reference: Windows file identity via `GetFileInformationByHandle`
    // (windows-sys 0.59, `Win32_Storage_FileSystem`); unix (dev, ino) per this
    // repo's `jcode_app_core::server::runtime_upgrade::FileIdentity` (dev_inode).
    // No prior art for a file-identity-based update probe exists in this repo:
    // searched "GetFileInformationByHandle" / "BY_HANDLE_FILE_INFORMATION" across
    // crates/*/src (0 hits), and "file_index" / "volume_serial_number" in
    // crates/jcode-app-core/src and crates/jcode-base/src (0 hits).
    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn identity(volume: u64, index: u64) -> Option<BinaryIdentity> {
        Some(BinaryIdentity { volume, index })
    }

    /// A candidate with no file identity (identity unavailable -> the path
    /// comparison decides).
    fn cand(
        path: &str,
        mtime: Option<SystemTime>,
    ) -> (PathBuf, Option<SystemTime>, Option<BinaryIdentity>) {
        (PathBuf::from(path), mtime, None)
    }

    /// A candidate with an explicit file identity.
    fn cand_with_identity(
        path: &str,
        mtime: Option<SystemTime>,
        volume: u64,
        index: u64,
    ) -> (PathBuf, Option<SystemTime>, Option<BinaryIdentity>) {
        (PathBuf::from(path), mtime, identity(volume, index))
    }

    /// Path-based variant: no file identity available.
    fn available(
        current_mtime: Option<SystemTime>,
        current_path: Option<&std::path::Path>,
        candidates: Vec<(PathBuf, Option<SystemTime>, Option<BinaryIdentity>)>,
    ) -> bool {
        newer_binary_available(current_mtime, current_path, None, candidates)
    }

    /// Identity-based variant: the running file and the candidate file are
    /// distinct objects unless the identity says otherwise.
    fn available_with_identity(
        current_mtime: Option<SystemTime>,
        current_path: Option<&std::path::Path>,
        current_identity: Option<BinaryIdentity>,
        candidates: Vec<(PathBuf, Option<SystemTime>, Option<BinaryIdentity>)>,
    ) -> bool {
        newer_binary_available(current_mtime, current_path, current_identity, candidates)
    }

    #[test]
    fn reports_update_when_candidate_is_strictly_newer() {
        let candidates = vec![cand("/x/stable/jcode", Some(t(200)))];
        assert!(available(
            Some(t(100)),
            Some(std::path::Path::new("/x/current/jcode")),
            candidates,
        ));
    }

    #[test]
    fn ignores_candidate_that_is_not_newer() {
        let candidates = vec![cand("/x/stable/jcode", Some(t(100)))];
        assert!(!available(
            Some(t(100)),
            Some(std::path::Path::new("/x/current/jcode")),
            candidates,
        ));
    }

    #[test]
    fn never_reloads_into_self_even_if_paths_were_equal() {
        // Same canonical path must never count as an update, regardless of mtime.
        let candidates = vec![cand("/x/current/jcode", Some(t(999)))];
        assert!(!available(
            Some(t(100)),
            Some(std::path::Path::new("/x/current/jcode")),
            candidates,
        ));
    }

    #[test]
    fn same_path_with_a_new_file_is_an_update() {
        // The Windows channel-update case: `builds/shared-server/jcode.exe` is
        // replaced IN PLACE, so the candidate path equals the running path while
        // the FILE is a different object. That is a legitimate upgrade, not a
        // self-exec, and the probe must report it - otherwise a promoted build
        // never reaches the daemon.
        let candidates = vec![cand_with_identity("/x/current/jcode", Some(t(200)), 7, 200)];
        assert!(available_with_identity(
            Some(t(100)),
            Some(std::path::Path::new("/x/current/jcode")),
            identity(7, 100),
            candidates,
        ));
    }

    #[test]
    fn same_file_object_is_never_an_update() {
        // The measured self-exec: candidate path and running path are the same
        // AND the file object is the same -> reloading would exec into the
        // running image again.
        let candidates = vec![cand_with_identity("/x/current/jcode", Some(t(200)), 7, 100)];
        assert!(!available_with_identity(
            Some(t(100)),
            Some(std::path::Path::new("/x/current/jcode")),
            identity(7, 100),
            candidates,
        ));
    }

    #[test]
    fn suppresses_update_when_current_mtime_unavailable() {
        // Regression for issue #277: an unreadable current mtime previously fell
        // through to a path-difference heuristic that could loop forever.
        let candidates = vec![cand("/x/stable/jcode", Some(t(200)))];
        assert!(!available(
            None,
            Some(std::path::Path::new("/x/current/jcode")),
            candidates,
        ));
    }

    #[test]
    fn suppresses_update_when_candidate_mtime_unavailable() {
        // The dangerous case from issue #277: candidate path differs but its
        // mtime cannot be read. Must NOT report an update.
        let candidates = vec![cand("/x/stable/jcode", None)];
        assert!(!available(
            Some(t(100)),
            Some(std::path::Path::new("/x/current/jcode")),
            candidates,
        ));
    }

    #[test]
    fn reports_update_if_any_candidate_is_newer() {
        let candidates = vec![
            cand("/x/stable/jcode", None),
            cand("/x/shared/jcode", Some(t(300))),
        ];
        assert!(available(
            Some(t(100)),
            Some(std::path::Path::new("/x/current/jcode")),
            candidates,
        ));
    }

    #[test]
    fn newer_server_is_not_outdated_by_older_channel_binary() {
        // Issue #291: a newer self-dev / shared-server daemon must NOT report an
        // update just because an *older* channel binary exists. Here the running
        // server (t=300) is newer than the only candidate (stable at t=100), so
        // there is no update. Previously a channel-version *mismatch* short-circuit
        // reported `true` here and told the newer server to downgrade itself.
        let candidates = vec![cand("/x/stable/jcode", Some(t(100)))];
        assert!(!available(
            Some(t(300)),
            Some(std::path::Path::new("/x/builds/versions/dev/jcode")),
            candidates,
        ));
    }

    #[test]
    fn equal_mtime_channel_binary_is_not_an_update() {
        // A candidate with the same mtime is not strictly newer, so it must not
        // trigger a reload (avoids the differ-but-not-newer reload loop, #277).
        let candidates = vec![cand("/x/stable/jcode", Some(t(100)))];
        assert!(!available(
            Some(t(100)),
            Some(std::path::Path::new("/x/builds/versions/dev/jcode")),
            candidates,
        ));
    }
}

#[cfg(test)]
mod reload_target_tests {
    use super::{BinaryIdentity, ReloadTargetDecision, guarded_reload_target};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    // reference: std::os::windows::fs::MetadataExt::file_index (std docs, Windows
    // file identity); unix (dev, ino) per this repo's
    // `jcode_app_core::server::runtime_upgrade::FileIdentity` (dev_inode). No
    // prior art for this guard exists in this repo: searched "reload self-exec
    // guard AlreadyCurrent" in crates/jcode-app-core/src and
    // crates/jcode-build-support/src (only hit is
    // build::SharedServerRepair::AlreadyCurrent, a different concern).
    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn candidate(path: &str) -> (PathBuf, &'static str) {
        (PathBuf::from(path), "shared-server")
    }

    fn identity(volume: u64, index: u64) -> Option<BinaryIdentity> {
        Some(BinaryIdentity { volume, index })
    }

    /// Decision without file identity: the path comparison decides.
    fn decide(
        candidate_path: &str,
        candidate_canonical: &str,
        current_exe: Option<&str>,
        current_canonical: Option<&str>,
        current_mtime: Option<SystemTime>,
        candidate_mtime: Option<SystemTime>,
    ) -> ReloadTargetDecision {
        guarded_reload_target(
            candidate(candidate_path),
            Path::new(candidate_canonical),
            None,
            current_exe.map(Path::new),
            current_canonical.map(Path::new),
            None,
            current_mtime,
            candidate_mtime,
        )
    }

    /// Decision with file identity: the running file and the candidate file are
    /// distinct objects unless the identity says otherwise.
    fn decide_with_identity(
        candidate_path: &str,
        candidate_canonical: &str,
        candidate_identity: Option<BinaryIdentity>,
        current_exe: Option<&str>,
        current_canonical: Option<&str>,
        current_identity: Option<BinaryIdentity>,
        current_mtime: Option<SystemTime>,
        candidate_mtime: Option<SystemTime>,
    ) -> ReloadTargetDecision {
        guarded_reload_target(
            candidate(candidate_path),
            Path::new(candidate_canonical),
            candidate_identity,
            current_exe.map(Path::new),
            current_canonical.map(Path::new),
            current_identity,
            current_mtime,
            candidate_mtime,
        )
    }

    #[test]
    fn same_binary_is_refused_as_self_exec() {
        // Reloading into ourselves is a self-exec: the daemon would tear itself
        // down and re-exec the very same image. The guard must refuse it.
        let decision = decide(
            "/x/current/jcode",
            "/x/current/jcode",
            Some("/x/current/jcode"),
            Some("/x/current/jcode"),
            Some(t(200)),
            Some(t(100)),
        );
        assert!(matches!(decision, ReloadTargetDecision::AlreadyCurrent));
    }

    #[test]
    fn same_file_object_is_refused_as_self_exec() {
        // The measured self-exec on Windows: candidate path == running path AND
        // the file object is the same file. Exec'ing it would re-exec the
        // running image and tear down every client session.
        let decision = decide_with_identity(
            "/x/shared-server/jcode",
            "/x/shared-server/jcode",
            identity(7, 100),
            Some("/x/shared-server/jcode"),
            Some("/x/shared-server/jcode"),
            identity(7, 100),
            Some(t(200)),
            Some(t(200)),
        );
        assert!(matches!(decision, ReloadTargetDecision::AlreadyCurrent));
    }

    #[test]
    fn same_path_with_a_new_file_is_an_upgrade() {
        // The Windows channel-update case: `builds/shared-server/jcode.exe` is
        // replaced IN PLACE, so the path is unchanged while the file object is
        // new. That must be treated as a legitimate upgrade, otherwise a
        // promoted build can never reach the daemon on Windows.
        let decision = decide_with_identity(
            "/x/shared-server/jcode",
            "/x/shared-server/jcode",
            identity(7, 200),
            Some("/x/shared-server/jcode"),
            Some("/x/shared-server/jcode"),
            identity(7, 100),
            Some(t(100)),
            Some(t(200)),
        );
        assert!(matches!(decision, ReloadTargetDecision::UseCandidate(_)));
    }

    #[test]
    fn newer_candidate_is_used() {
        // The self-dev case: a freshly written candidate is newer, so apply it.
        let decision = decide(
            "/x/shared-server/jcode",
            "/x/builds/versions/new/jcode",
            Some("/x/builds/versions/old/jcode"),
            Some("/x/builds/versions/old/jcode"),
            Some(t(100)),
            Some(t(200)),
        );
        match decision {
            ReloadTargetDecision::UseCandidate((path, _)) => {
                assert_eq!(path, PathBuf::from("/x/shared-server/jcode"));
            }
            other => panic!("expected candidate to be used, got {other:?}"),
        }
    }

    #[test]
    fn equal_mtime_candidate_is_used() {
        // Same mtime is not a downgrade.
        let decision = decide(
            "/x/shared-server/jcode",
            "/x/builds/versions/same/jcode",
            Some("/x/builds/versions/current/jcode"),
            Some("/x/builds/versions/current/jcode"),
            Some(t(100)),
            Some(t(100)),
        );
        assert!(matches!(decision, ReloadTargetDecision::UseCandidate(_)));
    }

    #[test]
    fn strictly_older_candidate_is_blocked_and_uses_current_exe() {
        // The reported bug: shared-server channel points at an older build than
        // the running client. Force reload must NOT downgrade; it re-execs the
        // current binary instead.
        let decision = decide(
            "/x/shared-server/jcode",
            "/x/builds/versions/old-0.14.3/jcode",
            Some("/x/builds/versions/new/jcode"),
            Some("/x/builds/versions/new/jcode"),
            Some(t(300)),
            Some(t(100)),
        );
        match decision {
            ReloadTargetDecision::DowngradeBlockedUseCurrent((path, _)) => {
                assert_eq!(path, PathBuf::from("/x/builds/versions/new/jcode"));
            }
            other => panic!("expected downgrade to be blocked, got {other:?}"),
        }
    }

    #[test]
    fn unreadable_candidate_mtime_is_treated_as_downgrade() {
        let decision = decide(
            "/x/shared-server/jcode",
            "/x/builds/versions/unknown/jcode",
            Some("/x/builds/versions/new/jcode"),
            Some("/x/builds/versions/new/jcode"),
            Some(t(300)),
            None,
        );
        assert!(matches!(
            decision,
            ReloadTargetDecision::DowngradeBlockedUseCurrent(_)
        ));
    }

    #[test]
    fn downgrade_without_current_exe_falls_back_to_candidate() {
        // If we cannot identify the running exe we cannot re-exec it, so we have
        // to proceed with the candidate rather than refuse to reload entirely.
        let decision = decide(
            "/x/shared-server/jcode",
            "/x/builds/versions/old/jcode",
            None,
            None,
            None,
            Some(t(100)),
        );
        assert!(matches!(
            decision,
            ReloadTargetDecision::DowngradeUnverifiable(_)
        ));
    }
}

#[cfg(test)]
mod binary_identity_tests {
    use super::binary_identity;

    /// A real file's identity must be readable, stable across calls, and
    /// different for a different file. This exercises the actual platform API
    /// (`GetFileInformationByHandle` on Windows, dev+ino on unix) rather than
    /// the pure decision function above.
    ///
    /// reference: no prior art, searched "binary_identity" in
    /// crates/jcode-app-core/src and crates/jcode-base/src (0 hits before this
    /// change).
    #[test]
    fn identity_is_stable_and_distinguishes_files() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let first = dir.path().join("first.bin");
        let second = dir.path().join("second.bin");
        std::fs::write(&first, b"first").expect("write first");
        std::fs::write(&second, b"second").expect("write second");

        let first_identity = binary_identity(&first).expect("first identity");
        let first_again = binary_identity(&first).expect("first identity again");
        let second_identity = binary_identity(&second).expect("second identity");

        assert_eq!(
            first_identity, first_again,
            "the same file must have a stable identity"
        );
        assert_ne!(
            first_identity, second_identity,
            "two different files must have different identities"
        );
    }

    #[test]
    fn missing_file_has_no_identity() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        assert!(binary_identity(&dir.path().join("absent.bin")).is_none());
    }

    /// A replaced file at the SAME path is a DIFFERENT file object. That is the
    /// Windows in-place channel update case, and the whole reason the reload
    /// guard compares identities instead of paths.
    #[test]
    fn replacing_a_file_changes_its_identity() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("channel.bin");
        std::fs::write(&path, b"old build").expect("write old");
        let before = binary_identity(&path).expect("identity before");

        // Replace in place: remove, then write a new file at the same path.
        std::fs::remove_file(&path).expect("remove");
        std::fs::write(&path, b"new build").expect("write new");
        let after = binary_identity(&path).expect("identity after");

        assert_ne!(
            before, after,
            "a new file at the same path must have a new identity"
        );
        assert_eq!(
            after,
            binary_identity(&path).expect("stable after"),
            "the replaced file's identity must be stable across calls"
        );
    }
}

#[cfg(test)]
mod pick_newest_candidate_tests {
    use super::pick_newest_candidate;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn entry(
        path: &str,
        label: &'static str,
        mtime: Option<SystemTime>,
    ) -> ((PathBuf, &'static str), PathBuf, Option<SystemTime>) {
        let p = PathBuf::from(path);
        ((p.clone(), label), p, mtime)
    }

    #[test]
    fn other_flavor_wins_when_strictly_newer() {
        // The /update bug: the session's own (self-dev) flavor is pinned to an
        // OLD build, but the other (normal) flavor self-healed to a NEWER
        // release. The reload target must follow the newer release so the daemon
        // can actually apply the update it advertises.
        let chosen = pick_newest_candidate([
            entry(
                "/x/versions/old-selfdev/jcode",
                "shared-server",
                Some(t(100)),
            ),
            entry("/x/versions/new-release/jcode", "stable", Some(t(200))),
        ])
        .expect("a candidate");
        assert_eq!(chosen.0, PathBuf::from("/x/versions/new-release/jcode"));
    }

    #[test]
    fn own_flavor_wins_on_tie() {
        // A deliberately-pinned self-dev build that is at least as fresh as the
        // other flavor must be preserved (self-dev pin protection).
        let chosen = pick_newest_candidate([
            entry("/x/versions/selfdev/jcode", "shared-server", Some(t(200))),
            entry("/x/versions/release/jcode", "stable", Some(t(200))),
        ])
        .expect("a candidate");
        assert_eq!(chosen.0, PathBuf::from("/x/versions/selfdev/jcode"));
    }

    #[test]
    fn own_flavor_wins_when_strictly_newer() {
        let chosen = pick_newest_candidate([
            entry(
                "/x/versions/fresh-selfdev/jcode",
                "shared-server",
                Some(t(300)),
            ),
            entry("/x/versions/release/jcode", "stable", Some(t(200))),
        ])
        .expect("a candidate");
        assert_eq!(chosen.0, PathBuf::from("/x/versions/fresh-selfdev/jcode"));
    }

    #[test]
    fn unknown_other_mtime_never_displaces_preferred() {
        // An unreadable mtime on the other flavor must not let it win, so we
        // never swap to an unverifiable binary.
        let chosen = pick_newest_candidate([
            entry("/x/versions/selfdev/jcode", "shared-server", Some(t(100))),
            entry("/x/versions/release/jcode", "stable", None),
        ])
        .expect("a candidate");
        assert_eq!(chosen.0, PathBuf::from("/x/versions/selfdev/jcode"));
    }

    #[test]
    fn duplicate_canonical_paths_collapse() {
        // Both flavors resolving to the same binary must not double-count; the
        // first (preferred) occurrence wins.
        let chosen = pick_newest_candidate([
            entry("/x/versions/same/jcode", "shared-server", Some(t(100))),
            entry("/x/versions/same/jcode", "stable", Some(t(999))),
        ])
        .expect("a candidate");
        assert_eq!(chosen.1, "shared-server");
    }

    #[test]
    fn empty_is_none() {
        assert!(pick_newest_candidate(std::iter::empty()).is_none());
    }
}

#[cfg(test)]
mod newest_reload_candidate_integration_tests {
    //! End-to-end-ish coverage that drives `newest_reload_candidate` through the
    //! REAL channel resolution (`build::shared_server_update_candidate`) against
    //! a temp `JCODE_HOME`. This reproduces the field "/update -> new client,
    //! stale server" state and proves the fix: a self-dev daemon now reloads into
    //! the freshly installed release instead of its old pinned binary.
    use super::{newer_binary_available, newest_reload_candidate};
    use crate::build;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    /// Set a file's mtime explicitly.
    ///
    /// `File::set_modified` needs a WRITABLE handle on Windows (a read-only
    /// handle fails with ERROR_ACCESS_DENIED), so open with write access.
    fn set_file_mtime(path: &Path, mtime: SystemTime) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime")
            .set_modified(mtime)
            .expect("set mtime");
    }

    fn install_versioned_binary(version: &str, mtime: SystemTime) -> std::path::PathBuf {
        // A real, distinct file per version so mtimes are independently settable
        // (install hard-links the source, which would share an inode/mtime).
        let dir = build::builds_dir()
            .expect("builds dir")
            .join("versions")
            .join(version);
        std::fs::create_dir_all(&dir).expect("create version dir");
        let path = dir.join(build::binary_name());
        std::fs::write(&path, format!("binary for {version}")).expect("write binary");
        set_file_mtime(&path, mtime);
        path
    }

    fn candidate_version_for(is_selfdev: bool) -> Option<String> {
        let (path, _label) = newest_reload_candidate(is_selfdev)?;
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
        canonical
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
    }

    #[test]
    fn selfdev_daemon_reloads_into_fresh_release_after_update() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        // Field state: shared-server pinned to an OLD self-dev build; stable
        // lags. Then `/update` installs a NEWER release and advances
        // stable/current (but NOT the pinned shared-server channel).
        let old_selfdev = "3f160da1-dirty-e756d52efca9";
        let new_release = "0.15.0";
        install_versioned_binary(old_selfdev, base);
        install_versioned_binary(new_release, base + Duration::from_secs(60));

        build::update_shared_server_symlink(old_selfdev).expect("pin shared-server");
        build::update_stable_symlink(new_release).expect("stable advanced by update");
        build::update_current_symlink(new_release).expect("current advanced by update");

        // The self-dev session's reload target must now be the fresh release, not
        // the stale pinned build. This is the fix.
        assert_eq!(
            candidate_version_for(true).as_deref(),
            Some(new_release),
            "self-dev daemon should reload into the freshly installed release"
        );
        // The normal session is unaffected (already healed to stable/release).
        assert_eq!(
            candidate_version_for(false).as_deref(),
            Some(new_release),
            "normal daemon should also target the fresh release"
        );

        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }

    #[test]
    fn selfdev_pin_is_preserved_when_it_is_the_freshest_build() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        // A deliberately-promoted self-dev build that is NEWER than stable must
        // still be honored: the whole point of pinning shared-server.
        let stable_old = "0.14.3";
        let selfdev_new = "56f43c3d-dirty-deadbeef";
        install_versioned_binary(stable_old, base);
        install_versioned_binary(selfdev_new, base + Duration::from_secs(120));

        build::update_stable_symlink(stable_old).expect("stable");
        build::update_shared_server_symlink(selfdev_new).expect("pin newer self-dev");

        assert_eq!(
            candidate_version_for(true).as_deref(),
            Some(selfdev_new),
            "a fresher self-dev pin must be preserved for self-dev sessions"
        );

        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }

    /// Re-implements `server_has_newer_binary`'s decision against an *injected*
    /// running-daemon path + mtime, so a test can model "the daemon is still the
    /// OLD binary" without spawning a real process. It scans the exact same
    /// candidate set (both flavors) and uses the same `newer_binary_available`
    /// core the production function uses, including the wrapper->payload
    /// resolution.
    fn daemon_reports_update(running: &Path, running_mtime: SystemTime) -> bool {
        let running_canonical = build::resolve_binary_payload(running);
        let mut candidates = std::collections::HashSet::new();
        for is_selfdev in [false, true] {
            if let Some((candidate, _label)) = super::server_update_candidate(is_selfdev) {
                candidates.insert(build::resolve_binary_payload(&candidate));
            }
        }
        let with_mtimes = candidates.into_iter().map(|candidate| {
            let m = std::fs::metadata(&candidate)
                .ok()
                .and_then(|m| m.modified().ok());
            let identity = super::binary_identity(&candidate);
            (candidate, m, identity)
        });
        newer_binary_available(
            Some(running_mtime),
            Some(running_canonical.as_path()),
            super::binary_identity(running_canonical.as_path()),
            with_mtimes,
        )
    }

    /// The question that matters for shipped users: after a NORMAL (non-self-dev)
    /// `/update`, does the long-lived daemon actually advertise + apply the
    /// upgrade on reconnect?
    ///
    /// Models a normal install: `shared-server` was tracking `stable`, the daemon
    /// is running the old release, and `/update` installs a newer release and
    /// advances stable/current/shared-server. We then drive the REAL
    /// update-detection core and reload-target resolver and assert both:
    /// (1) the daemon reports `server_has_update = true`, and
    /// (2) the binary it reloads into is the freshly installed release.
    #[test]
    fn normal_user_daemon_detects_and_targets_update_after_update() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let old_release = "0.14.3";
        let new_release = "0.15.0";
        let old_path = install_versioned_binary(old_release, base);
        install_versioned_binary(new_release, base + Duration::from_secs(60));

        // Pre-update state: every channel on the old release (shared-server
        // tracking stable). This is the steady state for a normal user.
        build::update_stable_symlink(old_release).expect("stable old");
        build::update_current_symlink(old_release).expect("current old");
        build::update_shared_server_symlink(old_release).expect("shared old");

        // `/update` installs the new release and advances the channels. Because
        // shared-server was tracking stable, it advances too.
        build::advance_shared_server_if_tracking_stable(new_release).expect("advance shared");
        build::update_stable_symlink(new_release).expect("stable new");
        build::update_current_symlink(new_release).expect("current new");

        // (1) The daemon (still the OLD binary) must now SEE the update so it
        // reports server_has_update = true to reconnecting clients.
        assert!(
            daemon_reports_update(&old_path, base),
            "normal-user daemon should report a server update after /update advanced the channels"
        );

        // (2) The binary it reloads into must be the freshly installed release.
        assert_eq!(
            candidate_version_for(false).as_deref(),
            Some(new_release),
            "normal-user daemon should reload into the freshly installed release"
        );

        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        }
    }

    /// Install a release-archive-style version dir: a tiny `jcode` wrapper
    /// script plus the real `jcode-linux-x86_64.bin` payload, with independently
    /// settable mtimes. This is exactly what `/update`'s tar.gz install path
    /// produces on disk.
    fn install_release_style_binary(
        version: &str,
        wrapper_mtime: SystemTime,
        payload_mtime: SystemTime,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = build::builds_dir()
            .expect("builds dir")
            .join("versions")
            .join(version);
        std::fs::create_dir_all(&dir).expect("create version dir");
        let payload = dir.join("jcode-linux-x86_64.bin");
        std::fs::write(&payload, format!("payload for {version}")).expect("write payload");
        set_file_mtime(&payload, payload_mtime);
        let wrapper = dir.join(build::binary_name());
        std::fs::write(
            &wrapper,
            "#!/usr/bin/env sh\nexec ./jcode-linux-x86_64.bin \"$@\"\n",
        )
        .expect("write wrapper");
        set_file_mtime(&wrapper, wrapper_mtime);
        (wrapper, payload)
    }

    /// Regression test for the post-`/update` infinite reload loop: release
    /// archives install a wrapper script + `.bin` payload, and the install copy
    /// loop can write the wrapper AFTER the payload. The running daemon's
    /// `current_exe()` is the payload, while the channel candidate resolves to
    /// the wrapper. Comparing wrapper-vs-payload mtimes made the freshly
    /// updated daemon report "newer binary available" against ITS OWN install
    /// forever -> the client force-reloaded the server in a loop and the
    /// session never attached.
    #[test]
    fn freshly_updated_release_daemon_reports_no_phantom_update() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        // Wrapper written strictly AFTER the payload (the bad copy order).
        let (wrapper, payload) =
            install_release_style_binary("0.25.1", base + Duration::from_secs(5), base);
        build::update_stable_symlink("0.25.1").expect("stable");
        build::update_current_symlink("0.25.1").expect("current");
        build::update_shared_server_symlink("0.25.1").expect("shared");

        // The daemon runs the payload; the candidate is the wrapper. Same
        // logical install -> no update must be reported.
        let payload_mtime = std::fs::metadata(&payload)
            .expect("payload metadata")
            .modified()
            .expect("payload mtime");
        assert!(
            !daemon_reports_update(&payload, payload_mtime),
            "a freshly updated daemon must not report an update against its own install"
        );
        // Sanity: the wrapper IS strictly newer than the payload on disk, so a
        // naive wrapper-vs-payload comparison would have reported a phantom
        // update (the bug this guards against).
        let wrapper_mtime = std::fs::metadata(&wrapper)
            .expect("wrapper metadata")
            .modified()
            .expect("wrapper mtime");
        assert!(wrapper_mtime > payload_mtime);

        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }
}

#[cfg(test)]
mod deleted_suffix_tests {
    use super::strip_deleted_suffix;
    use std::path::PathBuf;

    #[test]
    fn strips_linux_deleted_marker() {
        let p = PathBuf::from("/home/u/.jcode/builds/versions/abc/jcode (deleted)");
        assert_eq!(
            strip_deleted_suffix(p),
            PathBuf::from("/home/u/.jcode/builds/versions/abc/jcode")
        );
    }

    #[test]
    fn leaves_normal_paths_untouched() {
        let p = PathBuf::from("/home/u/.jcode/builds/versions/abc/jcode");
        assert_eq!(strip_deleted_suffix(p.clone()), p);
    }

    #[test]
    fn only_strips_trailing_marker() {
        // A path that merely contains the substring must not be altered.
        let p = PathBuf::from("/home/u/jcode (deleted)/jcode");
        assert_eq!(strip_deleted_suffix(p.clone()), p);
    }
}
