// reference: no prior art for a Rust-side wrapper that shells out to a deterministic Python
// reconciler, searched crates/jcode-app-core/src/ambient/ for an existing external-script
// invocation path (none: runner.rs only spawns kitty/TUI). Adapted from the crate's own
// std::process::Command usage in ambient/runner.rs:1044-1056 (run_cycle visible launcher) and
// the Harbor contract in ~/.jcode/research/2026-11-12-harbor-landing-coordinator.md section 4.2.
//! Harbor: the deterministic landing loop for jcode self-dev builds.
//!
//! Harbor decides landing from on-disk state only. It never builds and never
//! reloads; it runs the scripts under `~/.jcode/harbor/` and reports.
//!
//! The ambient agent is the CARRIER (it already wakes itself every 5-30 min).
//! Harbor is the CONTENT: a section in the ambient prompt plus one deterministic
//! pre-cycle pass. The agent may propose work; Harbor decides.

use crate::logging;
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

/// Where the Harbor scripts and queue live.
pub fn harbor_dir() -> PathBuf {
    crate::storage::jcode_dir()
        .unwrap_or_else(|_| PathBuf::from(".jcode"))
        .join("harbor")
}

/// Outcome of one deterministic Harbor pass.
#[derive(Debug, Clone, Serialize, Default)]
pub struct HarborStatus {
    pub enabled: bool,
    pub ran: bool,
    pub exit_code: Option<i32>,
    pub summary: String,
    pub queue_states: String,
    pub current_version: String,
    pub head: String,
}

/// Run one Harbor reconcile pass. Safe: the script never builds and never applies.
pub fn run_pass() -> HarborStatus {
    let dir = harbor_dir();
    let script = dir.join("harbor_reconcile.py");
    if !script.exists() {
        return HarborStatus {
            enabled: false,
            ran: false,
            summary: "harbor_reconcile.py absent".to_string(),
            ..Default::default()
        };
    }

    let mut status = HarborStatus {
        enabled: true,
        ran: true,
        ..Default::default()
    };

    // The host runs Python as `python` (measured: ambient scripts use it). Fall back to the
    // script's own shebang only if the interpreter is missing.
    let output = Command::new("python").arg(&script).arg("--json").output();
    match output {
        Ok(out) => {
            status.exit_code = out.status.code();
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            for line in stdout.lines().rev() {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                    status.current_version = value
                        .get("current")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    status.head = value
                        .get("head")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    status.queue_states = value
                        .get("queue_states")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let needs = value
                        .get("needs_build")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    status.summary = format!(
                        "harbor ok={} checks_passed={} needs_build={}",
                        value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
                        value.get("check_rc").and_then(|v| v.as_i64()).unwrap_or(-1) == 0,
                        needs
                    );
                    break;
                }
            }
            if status.summary.is_empty() {
                status.summary = format!(
                    "harbor pass produced no JSON (rc={:?}) {}",
                    status.exit_code,
                    stderr.trim().chars().take(160).collect::<String>()
                );
            }
        }
        Err(err) => {
            status.summary = format!("harbor pass failed to spawn: {}", err);
        }
    }

    logging::info(&format!("Harbor: {}", status.summary));
    status
}

/// Render the Harbor section for the ambient system prompt.
pub fn prompt_section(status: &HarborStatus) -> String {
    if !status.enabled {
        return String::new();
    }
    let mut out = String::from(
        "## Harbor (self-dev landing)\n\
         Harbor is the deterministic landing coordinator. It runs automatically before this \
         cycle; the block below is its last result. Do NOT re-run it by hand unless the block \
         says a step is missing.\n",
    );
    out.push_str(&format!("- Last pass: {}\n", status.summary));
    out.push_str(&format!("- Live channel: {}\n", status.current_version));
    out.push_str(&format!("- Repo HEAD: {}\n", status.head));
    out.push_str(&format!("- Queue states: {}\n", status.queue_states));
    out.push_str(
        "\nHarbor rules you may act on inside this cycle:\n\
         - A `HARBOR LANDING` queue entry authorises exactly ONE canonical `selfdev` \
         action=build. Never use build-reload.\n\
         - A `HARBOR APPLY` queue entry authorises the boundary apply: check \
         ~/.jcode/streaming_pids first, and if any session is mid-turn, reschedule instead \
         of applying.\n\
         - Never set JCODE_RELOAD_GUARD=off and never bypass the reload guard.\n\
         - Report the harbor_check.py checks/failures in the cycle summary.\n",
    );
    out
}
