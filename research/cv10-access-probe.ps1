$sig = @'
using System;
using System.Runtime.InteropServices;
public static class Live2 {
  [DllImport("kernel32.dll", SetLastError=true)] public static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
  [DllImport("kernel32.dll")] public static extern bool GetExitCodeProcess(IntPtr h, out uint code);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
  [DllImport("kernel32.dll")] public static extern uint GetLastError();
  public static int LastErr;
  public static bool Alive(uint pid) {
    if (pid == 0) return false;
    IntPtr h = OpenProcess(0x1000, false, pid);
    if (h == IntPtr.Zero) { LastErr = (int)GetLastError(); return false; }
    uint code; bool ok = GetExitCodeProcess(h, out code);
    CloseHandle(h);
    return ok && code == 259;
  }
  public static bool AliveFullAccess(uint pid) {
    if (pid == 0) return false;
    IntPtr h = OpenProcess(0x1FFFFF, false, pid); // PROCESS_ALL_ACCESS
    if (h == IntPtr.Zero) { LastErr = (int)GetLastError(); return false; }
    uint code; bool ok = GetExitCodeProcess(h, out code);
    CloseHandle(h);
    return ok && code == 259;
  }
}
'@
Add-Type -TypeDefinition $sig

$rows = @()

# 1. Is this probe elevated?
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$pr = New-Object Security.Principal.WindowsPrincipal($id)
$rows += ("elevated=" + $pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))

# 2. Baseline: self
$rows += ("self_alive=" + [Live2]::Alive([uint32]$PID))

# 3. Negative control on the probe itself: bogus pid must be dead with an error code
$bogus = [uint32]655000
$rows += ("bogus_alive=" + [Live2]::Alive($bogus) + " lasterr=" + [Live2]::LastErr)

# 4. Every process: count how many are judged DEAD by the predicate while actually running
$alive = 0; $judgedDead = 0; $errCodes = @{}
foreach ($p in Get-Process) {
  if ([Live2]::Alive([uint32]$p.Id)) { $alive++ } else {
    $judgedDead++
    $e = [Live2]::LastErr
    if ($errCodes.ContainsKey($e)) { $errCodes[$e]++ } else { $errCodes[$e] = 1 }
  }
}
$rows += ("processes_total=" + (Get-Process | Measure-Object).Count)
$rows += ("judged_alive=" + $alive)
$rows += ("judged_dead_but_running=" + $judgedDead)
foreach ($k in $errCodes.Keys) { $rows += ("  lasterr=$k count=" + $errCodes[$k]) }

$rows | Out-File -Encoding utf8 C:\Users\WorkShop1\proj\jcode-src\research\cv10-access-denied-probe.txt
$rows
