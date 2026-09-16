use anyhow::Result;
use std::{
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand},
    time::{Duration, Instant},
};

pub use crate::session_rebuild::{hot_rebuild, spawn_background_session_rebuild};

use crate::{build, tui::RunResult, update};

const CLIENT_HANDOFF_WATCHDOG_ARG: &str = "__jcode-client-handoff-watchdog";

/// Handle the private Windows handoff helper invocation before normal CLI
/// startup. The helper is deliberately a subcommand of the same binary so it
/// does not add another installed executable that can drift out of sync.
pub fn maybe_run_client_handoff_watchdog() -> Result<bool> {
    #[cfg(windows)]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.first().map(String::as_str) != Some(CLIENT_HANDOFF_WATCHDOG_ARG) {
            return Ok(false);
        }

        let spec = ClientHandoffSpec::parse(&args[1..])?;
        run_client_handoff_watchdog(spec)?;
        return Ok(true);
    }

    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

/// Signal the watchdog only after the replacement client has completed the
/// real server handshake and history bootstrap. The implementation lives in
/// the shared platform crate because the TUI crate cannot depend on the root
/// CLI module.
pub fn mark_client_handoff_ready() {
    crate::platform::mark_client_handoff_ready();
}

fn replace_client_process(
    exe: &Path,
    session_id: &str,
    cwd: &Path,
    is_selfdev: bool,
) -> Result<()> {
    #[cfg(windows)]
    {
        if std::env::var_os("JCODE_HANDOFF_SKIP_WATCHDOG").is_none() {
            let fallback = std::env::current_exe()?;
            schedule_client_handoff_watchdog(exe, &fallback, session_id, cwd, is_selfdev)?;
            // The watchdog owns the rest of this handoff. Exiting here is
            // intentional: it closes every old client handle before the
            // candidate is allowed to start.
            std::process::exit(0);
        }
    }

    let mut cmd = ProcessCommand::new(exe);
    if is_selfdev {
        cmd.arg("self-dev");
    }
    cmd.arg("--resume")
        .arg(session_id)
        .arg("--no-update")
        .env("JCODE_RELOAD_FAST_START", "1")
        .env_remove("JCODE_MIGRATE_BINARY")
        .current_dir(cwd);
    let err = crate::platform::replace_process(&mut cmd);
    Err(anyhow::anyhow!("Failed to exec {:?}: {}", exe, err))
}

#[cfg(windows)]
const CLIENT_HANDOFF_PARENT_WAIT: Duration = Duration::from_secs(15);

#[cfg(windows)]
const CLIENT_HANDOFF_READY_GRACE: Duration = Duration::from_secs(3);

#[cfg(windows)]
#[derive(Debug)]
struct ClientHandoffSpec {
    parent_pid: u32,
    candidate: PathBuf,
    fallback: PathBuf,
    session_id: String,
    cwd: PathBuf,
    ready_file: PathBuf,
    timeout: Duration,
    is_selfdev: bool,
}

#[cfg(windows)]
impl ClientHandoffSpec {
    fn parse(args: &[String]) -> Result<Self> {
        let mut parent_pid = None;
        let mut candidate = None;
        let mut fallback = None;
        let mut session_id = None;
        let mut cwd = None;
        let mut ready_file = None;
        let mut timeout_ms = None;
        let mut is_selfdev = false;

        let mut index = 0;
        while index < args.len() {
            let arg = args[index].as_str();
            if arg == "--selfdev" {
                is_selfdev = true;
                index += 1;
                continue;
            }
            let value = |name: &str| -> Result<String> {
                args.get(index + 1)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("missing value for {name}"))
            };
            match arg {
                "--parent-pid" => parent_pid = Some(value(arg)?.parse::<u32>()?),
                "--candidate" => candidate = Some(PathBuf::from(value(arg)?)),
                "--fallback" => fallback = Some(PathBuf::from(value(arg)?)),
                "--session" => session_id = Some(value(arg)?),
                "--cwd" => cwd = Some(PathBuf::from(value(arg)?)),
                "--ready-file" => ready_file = Some(PathBuf::from(value(arg)?)),
                "--timeout-ms" => timeout_ms = Some(value(arg)?.parse::<u64>()?),
                other => anyhow::bail!("unknown handoff watchdog argument: {other}"),
            }
            index += 2;
        }

        let timeout_ms = timeout_ms.unwrap_or(90_000).clamp(5_000, 300_000);
        Ok(Self {
            parent_pid: parent_pid.ok_or_else(|| anyhow::anyhow!("missing --parent-pid"))?,
            candidate: candidate.ok_or_else(|| anyhow::anyhow!("missing --candidate"))?,
            fallback: fallback.ok_or_else(|| anyhow::anyhow!("missing --fallback"))?,
            session_id: session_id.ok_or_else(|| anyhow::anyhow!("missing --session"))?,
            cwd: cwd.ok_or_else(|| anyhow::anyhow!("missing --cwd"))?,
            ready_file: ready_file.ok_or_else(|| anyhow::anyhow!("missing --ready-file"))?,
            timeout: Duration::from_millis(timeout_ms),
            is_selfdev,
        })
    }
}

#[cfg(windows)]
fn schedule_client_handoff_watchdog(
    candidate: &Path,
    fallback: &Path,
    session_id: &str,
    cwd: &Path,
    is_selfdev: bool,
) -> Result<()> {
    let ready_file = new_client_handoff_ready_file()?;
    let watchdog_exe = std::env::current_exe()?;
    let mut cmd = ProcessCommand::new(&watchdog_exe);
    cmd.arg(CLIENT_HANDOFF_WATCHDOG_ARG)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--candidate")
        .arg(candidate)
        .arg("--fallback")
        .arg(fallback)
        .arg("--session")
        .arg(session_id)
        .arg("--cwd")
        .arg(cwd)
        .arg("--ready-file")
        .arg(&ready_file)
        .arg("--timeout-ms")
        .arg("90000")
        .current_dir(cwd);
    if is_selfdev {
        cmd.arg("--selfdev");
    }

    crate::platform::reap_detached(crate::platform::spawn_detached(&mut cmd)?);
    Ok(())
}

#[cfg(windows)]
fn new_client_handoff_ready_file() -> Result<PathBuf> {
    let directory = crate::storage::jcode_dir()?.join("client-handoff");
    std::fs::create_dir_all(&directory)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    Ok(directory.join(format!("{}-{nonce}.ready", std::process::id())))
}

#[cfg(windows)]
fn spawn_handoff_client(spec: &ClientHandoffSpec, binary: &Path) -> std::io::Result<Child> {
    let mut cmd = ProcessCommand::new(binary);
    if spec.is_selfdev {
        cmd.arg("self-dev");
    }
    cmd.arg("--resume")
        .arg(&spec.session_id)
        .arg("--no-update")
        .env("JCODE_RESUMING", "1")
        .env("JCODE_RELOAD_FAST_START", "1")
        .env("JCODE_HANDOFF_READY_FILE", &spec.ready_file)
        .env("JCODE_HANDOFF_SKIP_WATCHDOG", "1")
        .env_remove("JCODE_MIGRATE_BINARY")
        .current_dir(&spec.cwd);
    cmd.spawn()
}

#[cfg(windows)]
fn wait_for_handoff_ready(spec: &ClientHandoffSpec, child: &mut Child) -> bool {
    let deadline = Instant::now() + spec.timeout;
    while Instant::now() < deadline {
        if spec.ready_file.exists() {
            return child_is_alive(child);
        }
        if !child_is_alive(child) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[cfg(windows)]
fn keep_handoff_alive(child: &mut Child) -> bool {
    let deadline = Instant::now() + CLIENT_HANDOFF_READY_GRACE;
    while Instant::now() < deadline {
        if !child_is_alive(child) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

#[cfg(windows)]
fn child_is_alive(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

#[cfg(windows)]
fn run_client_handoff_watchdog(spec: ClientHandoffSpec) -> Result<()> {
    let parent_deadline = Instant::now() + CLIENT_HANDOFF_PARENT_WAIT;
    while crate::platform::is_process_running(spec.parent_pid) && Instant::now() < parent_deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if crate::platform::is_process_running(spec.parent_pid) {
        anyhow::bail!(
            "client handoff parent {} did not exit; candidate was not started",
            spec.parent_pid
        );
    }

    let _ = std::fs::remove_file(&spec.ready_file);
    let mut candidate = match spawn_handoff_client(&spec, &spec.candidate) {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "jcode handoff candidate {:?} could not start: {error}; rolling back",
                spec.candidate
            );
            let mut fallback = spawn_handoff_client(&spec, &spec.fallback)?;
            if !keep_handoff_alive(&mut fallback) {
                eprintln!(
                    "jcode rollback client {:?} exited during its grace window",
                    spec.fallback
                );
            }
            let _ = std::fs::remove_file(&spec.ready_file);
            return Ok(());
        }
    };

    let healthy =
        wait_for_handoff_ready(&spec, &mut candidate) && keep_handoff_alive(&mut candidate);
    if healthy {
        let _ = std::fs::remove_file(&spec.ready_file);
        return Ok(());
    }

    eprintln!(
        "jcode handoff candidate {:?} failed readiness; rolling back to {:?}",
        spec.candidate, spec.fallback
    );
    let _ = candidate.kill();
    let _ = candidate.wait();
    let _ = std::fs::remove_file(&spec.ready_file);

    let mut fallback = spawn_handoff_client(&spec, &spec.fallback)?;
    if !keep_handoff_alive(&mut fallback) {
        eprintln!(
            "jcode rollback client {:?} exited during its grace window",
            spec.fallback
        );
    }
    let _ = std::fs::remove_file(&spec.ready_file);
    Ok(())
}

pub fn has_requested_action(run_result: &RunResult) -> bool {
    run_result.reload_session.is_some()
        || run_result.rebuild_session.is_some()
        || run_result.update_session.is_some()
        || run_result.restart_session.is_some()
}

pub fn execute_requested_action(run_result: &RunResult) -> Result<()> {
    if let Some(ref reload_session_id) = run_result.reload_session {
        hot_reload(reload_session_id)?;
    }

    if let Some(ref rebuild_session_id) = run_result.rebuild_session {
        hot_rebuild(rebuild_session_id)?;
    }

    if let Some(ref update_session_id) = run_result.update_session {
        hot_update(update_session_id)?;
    }

    if let Some(ref restart_session_id) = run_result.restart_session {
        hot_restart(restart_session_id)?;
    }

    Ok(())
}

pub fn hot_restart(session_id: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let exe = std::env::current_exe()?;
    let is_selfdev = crate::cli::selfdev::client_selfdev_requested();

    crate::logging::info(&format!("Restarting with current binary: {:?}", exe));

    crate::env::set_var("JCODE_RESUMING", "1");

    replace_client_process(&exe, session_id, &cwd, is_selfdev)
}

pub fn hot_reload(session_id: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;

    crate::env::set_var("JCODE_RESUMING", "1");

    if let Ok(migrate_binary) = std::env::var("JCODE_MIGRATE_BINARY") {
        let binary_path = std::path::PathBuf::from(&migrate_binary);
        if binary_path.exists() {
            crate::logging::info("Migrating to stable binary...");
            return replace_client_process(&binary_path, session_id, &cwd, false);
        } else {
            crate::logging::warn(&format!(
                "Migration binary not found at {:?}, falling back to local binary",
                binary_path
            ));
        }
    }

    let is_selfdev = crate::cli::selfdev::client_selfdev_requested();
    let (exe, _label) = build::preferred_reload_candidate(is_selfdev)
        .ok_or_else(|| anyhow::anyhow!("No reloadable binary found"))?;

    if let Ok(metadata) = std::fs::metadata(&exe) {
        let age = metadata
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .map(|d| {
                let secs = d.as_secs();
                if secs < 60 {
                    format!("{} seconds ago", secs)
                } else if secs < 3600 {
                    format!("{} minutes ago", secs / 60)
                } else {
                    format!("{} hours ago", secs / 3600)
                }
            })
            .unwrap_or_else(|| "unknown".to_string());
        crate::logging::info(&format!("Reloading with binary built {}...", age));
    }

    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if !exe.exists() {
                continue;
            }
        }
        match replace_client_process(&exe, session_id, &cwd, is_selfdev) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < 2 && error.to_string().contains("No such file") => {
                crate::logging::warn(&format!(
                    "exec attempt {} failed (ENOENT) for {:?}, retrying...",
                    attempt + 1,
                    exe
                ));
            }
            Err(error) => return Err(error),
        }
    }
    Err(anyhow::anyhow!(
        "Failed to exec {:?}: binary not found after retries",
        exe
    ))
}

pub fn hot_update(session_id: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;

    update::print_centered("Checking for updates...");

    match update::check_for_update_blocking() {
        Ok(Some(release)) => {
            let current = jcode_build_meta::version();
            update::print_centered(&format!(
                "Update available: {} -> {}",
                current, release.tag_name
            ));
            update::print_centered(&format!("Downloading {}...", release.tag_name));

            match update::download_and_install_blocking_with_progress(&release, |progress| {
                update::print_centered(&format!(
                    "{} {}",
                    release.tag_name,
                    update::format_download_progress_bar(progress)
                ));
            }) {
                Ok(path) => {
                    update::print_centered(&format!("✓ Installed {}", release.tag_name));
                    reload_server_after_update("installed update");

                    let is_selfdev = crate::cli::selfdev::client_selfdev_requested();
                    let exe = build::client_update_candidate(is_selfdev)
                        .map(|(p, _)| p)
                        .unwrap_or(path);

                    update::print_centered(&format!("Restarting with session {}...", session_id));

                    crate::env::set_var("JCODE_RESUMING", "1");

                    return replace_client_process(&exe, session_id, &cwd, is_selfdev);
                }
                Err(e) => {
                    update::print_centered(&format!(
                        "✗ Download failed: {}",
                        update::summarize_update_error(&format!("{:#}", e))
                    ));
                }
            }
        }
        Ok(None) => {
            if repair_stale_shared_server_after_update_check() {
                reload_server_after_update("repaired stale server target");
            }
            update::print_centered(&format!(
                "Already up to date ({})",
                jcode_build_meta::version()
            ));
        }
        Err(e) => {
            update::print_centered(&format!(
                "✗ Update check failed: {}",
                update::summarize_update_error(&format!("{:#}", e))
            ));
        }
    }

    crate::env::set_var("JCODE_RESUMING", "1");
    let exe = std::env::current_exe()?;
    let is_selfdev = crate::cli::selfdev::client_selfdev_requested();
    replace_client_process(&exe, session_id, &cwd, is_selfdev)
}

pub fn get_repo_dir() -> Option<std::path::PathBuf> {
    build::get_repo_dir()
}

/// Minimum interval between `git fetch` update probes across all jcode
/// processes. Every source-build client spawn used to fetch unconditionally,
/// so spawning N clients at once ran N concurrent `git fetch` + ssh sessions
/// against the remote. One probe per interval per machine is plenty; a marker
/// file's mtime coordinates it (same pattern as the session-backup pruner).
const UPDATE_FETCH_INTERVAL_SECS: u64 = 15 * 60;

fn claim_update_fetch_slot() -> bool {
    let Ok(base) = crate::storage::jcode_dir() else {
        // Cannot coordinate without a home dir; fall back to probing.
        return true;
    };
    let marker = base.join("update-fetch.stamp");
    if let Ok(metadata) = std::fs::metadata(&marker)
        && let Ok(modified) = metadata.modified()
        && let Ok(age) = std::time::SystemTime::now().duration_since(modified)
        && age.as_secs() < UPDATE_FETCH_INTERVAL_SECS
    {
        return false;
    }
    // Touch before fetching so a spawn burst collapses to ~one fetch.
    std::fs::write(&marker, b"").is_ok()
}

pub fn check_for_updates() -> Option<bool> {
    let repo_dir = get_repo_dir()?;

    if claim_update_fetch_slot() {
        let fetch = ProcessCommand::new("git")
            .args(["fetch", "-q"])
            .current_dir(&repo_dir)
            .output()
            .ok()?;

        if !fetch.status.success() {
            return None;
        }
    }
    // When the fetch slot was claimed by another recent process, still answer
    // from the (fresh enough) local refs instead of skipping the check.

    let behind = ProcessCommand::new("git")
        .args(["rev-list", "--count", "HEAD..@{u}"])
        .current_dir(&repo_dir)
        .output()
        .ok()?;

    if behind.status.success() {
        let count: u32 = String::from_utf8_lossy(&behind.stdout)
            .trim()
            .parse()
            .unwrap_or(0);
        Some(count > 0)
    } else {
        None
    }
}

#[cfg(all(test, windows))]
mod handoff_tests {
    use super::ClientHandoffSpec;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn watchdog_parser_accepts_paths_and_selfdev_flag() {
        let spec = ClientHandoffSpec::parse(&args(&[
            "--parent-pid",
            "42",
            "--candidate",
            "C:\\candidate\\jcode.exe",
            "--fallback",
            "C:\\known-good\\jcode.exe",
            "--session",
            "session-1",
            "--cwd",
            "C:\\work",
            "--ready-file",
            "C:\\markers\\ready",
            "--timeout-ms",
            "12000",
            "--selfdev",
        ]))
        .expect("valid watchdog arguments");

        assert_eq!(spec.parent_pid, 42);
        assert_eq!(spec.session_id, "session-1");
        assert!(spec.is_selfdev);
        assert_eq!(spec.timeout, std::time::Duration::from_secs(12));
    }

    #[test]
    fn watchdog_parser_rejects_unknown_arguments() {
        let error = ClientHandoffSpec::parse(&args(&["--unexpected", "value"]))
            .expect_err("unknown arguments must be rejected");
        assert!(
            error
                .to_string()
                .contains("unknown handoff watchdog argument")
        );
    }
}

/// True when the source checkout has local commits that upstream lacks, so a
/// fast-forward pull (and therefore auto-update) can never succeed. Returns
/// `None` when the repo or upstream cannot be inspected.
pub fn local_commits_ahead_of_upstream() -> Option<bool> {
    let repo_dir = get_repo_dir()?;
    let ahead = ProcessCommand::new("git")
        .args(["rev-list", "--count", "@{u}..HEAD"])
        .current_dir(&repo_dir)
        .output()
        .ok()?;
    if !ahead.status.success() {
        return None;
    }
    let count: u32 = String::from_utf8_lossy(&ahead.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    Some(count > 0)
}

pub fn run_auto_update() -> Result<()> {
    use crate::bus::{Bus, BusEvent, UpdateStatus};

    let repo_dir =
        get_repo_dir().ok_or_else(|| anyhow::anyhow!("Could not find jcode repository"))?;

    update::run_git_pull_ff_only(&repo_dir, true)?;

    crate::logging::info("Building updated source version...");
    let build_output = ProcessCommand::new("cargo")
        .args(["build", "--release"])
        .current_dir(&repo_dir)
        .output()?;

    if !build_output.status.success() {
        let stderr = String::from_utf8_lossy(&build_output.stderr);
        let stdout = String::from_utf8_lossy(&build_output.stdout);
        if !stderr.trim().is_empty() {
            crate::logging::error(&format!("auto-update cargo stderr:\n{}", stderr.trim()));
        }
        if !stdout.trim().is_empty() {
            crate::logging::info(&format!("auto-update cargo stdout:\n{}", stdout.trim()));
        }
        anyhow::bail!("cargo build failed");
    }

    if let Err(e) = build::install_local_release(&repo_dir) {
        crate::logging::warn(&format!("auto-update install failed: {}", e));
    }

    let hash = ProcessCommand::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(&repo_dir)
        .output()?;
    let hash = String::from_utf8_lossy(&hash.stdout);
    let version = format!("main-{}", hash.trim());
    Bus::global().publish(BusEvent::UpdateStatus(UpdateStatus::Installed {
        version: version.clone(),
    }));

    // With a live TUI session, let the app reload gracefully (input line
    // saved, current turn finished, session resumed) instead of exec-ing over
    // the running interface, which visibly resets the screen.
    if let Some(session_id) = crate::get_current_session() {
        use crate::bus::{ClientMaintenanceAction, SessionUpdateStatus};
        crate::logging::info(&format!(
            "Updated to {}. Requesting graceful session reload...",
            version
        ));
        Bus::global().publish(BusEvent::SessionUpdateStatus(
            SessionUpdateStatus::ReadyToReload {
                session_id,
                action: ClientMaintenanceAction::Update,
                version,
            },
        ));
        return Ok(());
    }

    crate::logging::info(&format!("Updated to {}. Restarting...", version));
    std::thread::sleep(std::time::Duration::from_millis(250));

    let exe = build::client_update_candidate(false)
        .map(|(p, _)| p)
        .or_else(|| std::env::current_exe().ok())
        .ok_or_else(|| anyhow::anyhow!("No executable path found after update"))?;
    let args: Vec<String> = std::env::args().skip(1).collect();

    let err =
        crate::platform::replace_process(ProcessCommand::new(&exe).args(&args).arg("--no-update"));

    Err(anyhow::anyhow!(
        "Failed to exec new binary {:?}: {}",
        exe,
        err
    ))
}

/// Explicit updates follow the configured channel for release and dev builds alike.
/// Source rebuilds belong to self-dev and /rebuild, not the stable update path.
pub fn run_update() -> Result<()> {
    update::print_centered("Checking GitHub for latest release...");
    match update::check_for_update_blocking() {
        Ok(Some(release)) => {
            update::print_centered(&format!(
                "Downloading {} \u{2192} {}...",
                jcode_build_meta::version(),
                release.tag_name
            ));
            let _path =
                update::download_and_install_blocking_with_progress(&release, |progress| {
                    update::print_centered(&format!(
                        "{} {}",
                        release.tag_name,
                        update::format_download_progress_bar(progress)
                    ));
                })?;
            update::print_centered(&format!("✅ Updated to {}", release.tag_name));
            reload_server_after_update("installed update");
            update::print_centered("Restart jcode to use the new version.");
        }
        Ok(None) => {
            if repair_stale_shared_server_after_update_check() {
                reload_server_after_update("repaired stale server target");
            }
            update::print_centered(&format!(
                "Already up to date ({})",
                jcode_build_meta::version()
            ));
        }
        Err(e) => {
            anyhow::bail!(
                "Update check failed: {}",
                update::summarize_update_error(&format!("{:#}", e))
            );
        }
    }
    Ok(())
}

fn repair_stale_shared_server_after_update_check() -> bool {
    match build::repair_stale_shared_server_channel() {
        Ok(build::SharedServerRepair::Repaired {
            previous,
            repaired_to,
        }) => {
            crate::logging::info(&format!(
                "update: repaired stale shared-server channel {:?} -> {}",
                previous, repaired_to
            ));
            update::print_centered(&format!(
                "Repaired stale server reload target: {}",
                repaired_to
            ));
            true
        }
        Ok(build::SharedServerRepair::AlreadyCurrent) => false,
        Err(error) => {
            crate::logging::warn(&format!(
                "update: failed to repair stale shared-server channel: {}",
                error
            ));
            false
        }
    }
}

fn reload_server_after_update(reason: &str) {
    let exe = build::client_update_candidate(false)
        .map(|(path, _)| path)
        .or_else(|| std::env::current_exe().ok());
    let Some(exe) = exe else {
        crate::logging::warn("update: could not find jcode binary to reload stale server");
        return;
    };

    let output = ProcessCommand::new(&exe)
        .args(["--no-update", "server", "reload", "--force"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            crate::logging::info(&format!(
                "update: requested server reload after {} via {:?}",
                reason, exe
            ));
        }
        Ok(output) => {
            crate::logging::warn(&format!(
                "update: server reload after {} failed with status {:?}: {}",
                reason,
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Err(error) => {
            crate::logging::warn(&format!(
                "update: failed to request server reload after {} via {:?}: {}",
                reason, exe, error
            ));
        }
    }
}
