$v = Get-Volume -FileSystemLabel BROOMWIN -ErrorAction SilentlyContinue
if (-not $v) { exit }
# Tell the server this Windows boot happened (once per boot; retried every 5 minutes until it answers). The server
# flags the machine "not reset" when no PXE boot came just before = Windows started from the SSD without the stage.
$bootId = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString('o')
$mark = "$env:SystemRoot\Temp\broom-booted.txt"
$srvf = $v.Path + 'broom\srv.txt'
if ((-not (Test-Path $mark) -or (Get-Content $mark -Raw).Trim() -ne $bootId) -and [IO.File]::Exists($srvf)) {
  try {
    Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 -Method Post -Uri ('http://' + [IO.File]::ReadAllText($srvf).Trim() + '/api/booted') | Out-Null
    Set-Content -Encoding ascii $mark $bootId
  } catch { }
}
# Games disk (if the server has one) as a drive letter: broom-games.ps1, copied out like the stub does (volume path).
$gf = $v.Path + 'broom\broom-games.ps1'
if ([IO.File]::Exists($gf) -and [IO.File]::Exists($srvf)) {
  $run = "$env:SystemRoot\Temp\broom-games.run.ps1"
  [IO.File]::Copy($gf, $run, $true)
  & powershell -NoProfile -ExecutionPolicy Bypass -File $run -Srv ([IO.File]::ReadAllText($srvf).Trim())
}
# Session watchdog (broom-watch.ps1: SSD room): its own task (SYSTEM, no time limit), started once per boot - it loops,
# it must not hold this task up. C: is reset every boot, so the task is registered again each time.
$wf = $v.Path + 'broom\broom-watch.ps1'
if ([IO.File]::Exists($wf) -and [IO.File]::Exists($srvf)) {
  try {
    $t = Get-ScheduledTask -TaskName BroomWatch -ErrorAction SilentlyContinue
    if (-not $t -or $t.State -ne 'Running') {
      $run = "$env:SystemRoot\Temp\broom-watch.run.ps1"
      [IO.File]::Copy($wf, $run, $true)
      $arg = '-NoProfile -ExecutionPolicy Bypass -File "' + $run + '" -Srv ' + [IO.File]::ReadAllText($srvf).Trim()
      $act = New-ScheduledTaskAction -Execute powershell.exe -Argument $arg
      $set = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
      Register-ScheduledTask -TaskName BroomWatch -Action $act -Settings $set -User SYSTEM -RunLevel Highest -Force | Out-Null
      Start-ScheduledTask -TaskName BroomWatch
    }
  } catch { }
}
# Strict reset (stage wrote strict.txt = the SSD's Windows entries): remove the Windows boot loader from the ESP once
# Windows is up - the stage puts it back on every PXE boot, so without the stage (cable out, F12, BootOrder edited)
# the SSD cannot start Windows at all.
$sf = $v.Path + 'broom\strict.txt'
$strict = [IO.File]::Exists($sf)
if ($strict) {
  $L = (69..90 | ForEach-Object { [char]$_ } | Where-Object { -not (Test-Path "${_}:\") } | Select-Object -Last 1)
  if ($L) {
    & mountvol "${L}:" /s | Out-Null
    if (Test-Path "${L}:\EFI\Microsoft") { Remove-Item "${L}:\EFI\Microsoft" -Recurse -Force -ErrorAction SilentlyContinue }
    & mountvol "${L}:" /d | Out-Null
  }
}
$of = $v.Path + 'broom\bootorder.txt'
if (-not [IO.File]::Exists($of)) { exit }
Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices;
public static class BroomFw {
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  static extern uint GetFirmwareEnvironmentVariableExW(string name, string guid, byte[] buf, uint size, IntPtr attr);
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  static extern bool SetFirmwareEnvironmentVariableExW(string name, string guid, byte[] buf, uint size, uint attr);
  [DllImport("advapi32.dll", SetLastError=true)]
  static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
  [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  static extern bool LookupPrivilegeValueW(string system, string name, out long luid);
  [DllImport("advapi32.dll", SetLastError=true)]
  static extern bool AdjustTokenPrivileges(IntPtr token, bool disableAll, ref TokenPriv tp, uint len, IntPtr prev, IntPtr retLen);
  [DllImport("kernel32.dll")]
  static extern IntPtr GetCurrentProcess();
  [StructLayout(LayoutKind.Sequential, Pack = 4)]
  struct TokenPriv { public uint Count; public long Luid; public uint Attr; }
  const string EfiGlobal = "{8BE4DF61-93CA-11D2-AA0D-00E098032B8C}";
  public static void EnablePrivilege() {
    IntPtr t; OpenProcessToken(GetCurrentProcess(), 0x28, out t); // TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY
    TokenPriv tp = new TokenPriv(); tp.Count = 1; tp.Attr = 2;    // SE_PRIVILEGE_ENABLED
    LookupPrivilegeValueW(null, "SeSystemEnvironmentPrivilege", out tp.Luid);
    AdjustTokenPrivileges(t, false, ref tp, 0, IntPtr.Zero, IntPtr.Zero);
  }
  public static byte[] Get(string name) {
    byte[] b = new byte[4096];
    uint n = GetFirmwareEnvironmentVariableExW(name, EfiGlobal, b, (uint)b.Length, IntPtr.Zero);
    if (n == 0) return null;
    byte[] r = new byte[n]; Array.Copy(b, r, n); return r;
  }
  // 7 = NON_VOLATILE | BOOTSERVICE_ACCESS | RUNTIME_ACCESS (the attributes BootOrder has)
  public static bool Set(string name, byte[] data) { return SetFirmwareEnvironmentVariableExW(name, EfiGlobal, data, (uint)data.Length, 7); }
}
'@
[BroomFw]::EnablePrivilege()
$cur = [BroomFw]::Get('BootOrder')
if ($null -eq $cur) { exit }
$now = @(); for ($i = 0; $i + 1 -lt $cur.Length; $i += 2) { $now += [BitConverter]::ToUInt16($cur, $i) }
$want = @()
foreach ($x in ([IO.File]::ReadAllText($of).Trim() -split ',')) {
  if ($x -notmatch '^[0-9A-Fa-f]{4}$') { continue }
  $n = [Convert]::ToUInt16($x, 16)
  # Only entries that still exist (a Boot#### variable), each once.
  if (($want -notcontains $n) -and ($null -ne [BroomFw]::Get(('Boot{0:X4}' -f $n)))) { $want += $n }
}
if ($want.Count -eq 0) { exit }
foreach ($n in $now) { if ($want -notcontains $n) { $want += $n } }
# Strict: the SSD's Windows entries stay OUT (Windows re-adds "Windows Boot Manager" on every boot).
if ($strict) {
  $drop = @()
  foreach ($x in ([IO.File]::ReadAllText($sf).Trim() -split ',')) { if ($x -match '^[0-9A-Fa-f]{4}$') { $drop += [Convert]::ToUInt16($x, 16) } }
  $want = @($want | Where-Object { $drop -notcontains $_ })
  if ($want.Count -eq 0) { exit }
}
if (($want -join ',') -ne ($now -join ',')) {
  $b = New-Object byte[] ($want.Count * 2)
  for ($i = 0; $i -lt $want.Count; $i++) { [BitConverter]::GetBytes([uint16]$want[$i]).CopyTo($b, $i * 2) }
  [BroomFw]::Set('BootOrder', $b) | Out-Null
}