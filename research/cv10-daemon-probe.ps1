$sig = @'
using System;
using System.Runtime.InteropServices;
public static class Live4 {
  [DllImport("kernel32.dll", SetLastError=true)] public static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
  [DllImport("kernel32.dll")] public static extern bool GetExitCodeProcess(IntPtr h, out uint code);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
  [DllImport("kernel32.dll")] public static extern uint GetLastError();
  public static bool Alive(uint pid) {
    if (pid == 0) return false;
    IntPtr h = OpenProcess(0x1000, false, pid);
    if (h == IntPtr.Zero) return false;
    uint code; bool ok = GetExitCodeProcess(h, out code);
    CloseHandle(h);
    return ok && code == 259;
  }
}
'@
Add-Type -TypeDefinition $sig

$rows = @()
$rows += "=== jcode daemon processes ==="
foreach ($p in Get-Process jcode -ErrorAction SilentlyContinue) {
  $rows += ("pid=" + $p.Id + " alive=" + [Live4]::Alive([uint32]$p.Id) + " start=" + $p.StartTime)
}
$rows += "=== the identity file's pid ==="
$id = Get-Content "C:\Users\WorkShop1\AppData\Local\jcode\builds\daemon-identity.json" -Raw
$rows += $id
($j = $id | ConvertFrom-Json)
$rows += ("identity_pid=" + $j.pid + " alive=" + [Live4]::Alive([uint32]$j.pid))
$rows += "=== elevated? ==="
$pr = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
$rows += ("elevated=" + $pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))
$rows | Out-File -Encoding utf8 C:\Users\WorkShop1\proj\jcode-src\research\cv10-daemon-alive.txt
$rows
