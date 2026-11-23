$sig = @'
using System;
using System.Runtime.InteropServices;
public static class Live {
  [DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
  [DllImport("kernel32.dll")] public static extern bool GetExitCodeProcess(IntPtr h, out uint code);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
  public static bool Alive(uint pid) {
    if (pid == 0) return false;
    IntPtr h = OpenProcess(0x1000, false, pid); // PROCESS_QUERY_LIMITED_INFORMATION
    if (h == IntPtr.Zero) return false;
    uint code; bool ok = GetExitCodeProcess(h, out code);
    CloseHandle(h);
    return ok && code == 259; // STILL_ACTIVE
  }
}
'@
Add-Type -TypeDefinition $sig

$me = [uint32]$PID
$bogus = [uint32]655000

# negative control: a pid that certainly does not exist
$negCtl = $false
foreach ($p in 654000..655000) { if (-not [Live]::Alive($p)) { $negCtl = $true; break } }

$rows = @()
$rows += ("self pid={0} alive={1} (expect True)" -f $me, [Live]::Alive($me))
$rows += ("bogus pid={0} alive={1} (expect False)" -f $bogus, [Live]::Alive($bogus))
$rows += ("pid 0 alive={0} (expect False)" -f [Live]::Alive(0))
$rows += ("negative_control_found_a_dead_pid_in_range={0} (expect True)" -f $negCtl)
$rows += ("live_pids_seen=" + ((Get-Process | Measure-Object).Count))

$rows | Out-File -Encoding utf8 C:\Users\WorkShop1\proj\jcode-src\research\cv10-liveness-probe.txt
$rows
