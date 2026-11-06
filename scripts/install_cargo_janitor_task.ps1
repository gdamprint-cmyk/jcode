# reference: Windows Task Scheduler CLI (schtasks.exe) is the only built-in
# mechanism used here; no prior art in this repo (searched scripts/ for
# "schtasks" and "ScheduledTask").
#
# Registers a daily Windows scheduled task that runs the Cargo target janitor.
#
#   .\scripts\install_cargo_janitor_task.ps1
#   .\scripts\install_cargo_janitor_task.ps1 -TaskName jcode-cargo-janitor -PruneDays 7 -Apply
#
# The janitor itself is scripts/clean_target_windows.py. It runs in dry-run by
# default; -Apply switches the registered task to real deletion.
#
# Why an external task instead of Rust code in the daemon:
#   * the target dirs live on D:/E: drive roots, outside ~/.jcode/scratch, so
#     the existing scratch pruner never sees them;
#   * deciding "is a cargo build running right now" needs the process list;
#     the janitor already does that with psutil/PowerShell, and re-implementing
#     it in Rust adds a dependency for a once-a-day side effect;
#   * a task that runs outside the daemon cannot be killed by an exec reload
#     mid-walk, and its dry-run output lands in a file the user can read.

param(
    [string]$TaskName = "jcode-cargo-janitor",
    [int]$PruneDays = 7,
    [int]$SweepDays = 7,
    [switch]$Apply,
    [string]$Python = "python"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$script = Join-Path $repoRoot "scripts\clean_target_windows.py"
if (-not (Test-Path $script)) { throw "janitor not found: $script" }

$args = @($script, "--prune-lanes", "$PruneDays", "--sweep", "$SweepDays")
if ($Apply) { $args += "--apply" }
$action = "powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -Command `"& '$Python' $($args -join ' ') *>&1 | Out-File -Encoding utf8 (Join-Path $env:TEMP 'jcode-cargo-janitor.log')`""

schtasks.exe /Create /TN $TaskName /TR $action /SC DAILY /ST 04:30 /F | Out-Null
if ($LASTEXITCODE -ne 0) { throw "schtasks /Create failed with exit code $LASTEXITCODE" }
Write-Host "installed task '$TaskName' (daily 04:30, apply=$Apply, prune=${PruneDays}d, sweep=${SweepDays}d)"
