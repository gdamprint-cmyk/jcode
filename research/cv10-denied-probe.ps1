$sig = @'
using System;
using System.Runtime.InteropServices;
public static class Live3 {
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
  // Try several access masks to tell EPERM-style denial from a genuinely gone pid.
  public static string Diagnose(uint pid) {
    uint[] masks = { 0x1000, 0x1000 | 0x0400, 0x1FFFFF, 0x0001 }; // LIMITED, +QUERY_INFO, ALL, TERMINATE
    string outp = "";
    foreach (uint m in masks) {
      IntPtr h = OpenProcess(m, false, pid);
      if (h == IntPtr.Zero) { outp += ("mask=0x" + m.ToString("X") + ":err" + GetLastError()); }
      else { outp += ("mask=0x" + m.ToString("X") + ":ok"); CloseHandle(h); }
    }
    return outp;
  }
}
'@
Add-Type -TypeDefinition $sig

$rows = @()
foreach ($p in Get-Process) {
  if (-not [Live3]::Alive([uint32]$p.Id)) {
    $rows += ("pid=" + $p.Id + " name=" + $p.ProcessName + " lasterr=" + [Live3]::LastErr)
    $rows += ("   diag: " + [Live3]::Diagnose([uint32]$p.Id))
  }
}
$rows += ("--- total scanned=" + (Get-Process | Measure-Object).Count)
$rows | Out-File -Encoding utf8 C:\Users\WorkShop1\proj\jcode-src\research\cv10-denied-identity.txt
$rows
