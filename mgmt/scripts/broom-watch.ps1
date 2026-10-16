# Session watchdog: the guest's writes (C: = child.vhdx, games drives = games-*-child.vhdx) all land on BROOMWIN; when
# it fills up Windows cannot grow its boot disk and stops with a blue screen (VHD_BOOT_HOST_VOLUME_NOT_ENOUGH_SPACE).
# And C: shows the VHDX's virtual free space, so Windows never warns. Checks BROOMWIN's real free space every 15 s:
# below the warn threshold -> a message to the guest (every 5 minutes) + a report to the server (Machines page); below
# the restart threshold -> restart in 60 s with a message: the session is lost either way, this way with time to save.
# Thresholds: Settings -> SSD room (GET /api/client-config "<room> <warn> <reboot>" GB). Started once per boot by
# broom-bootorder.ps1 as its own task (SYSTEM). ASCII only.
param([string]$Srv, [string]$Vol)
$ErrorActionPreference = 'SilentlyContinue'
# BROOMWIN's free space: GetDiskFreeSpaceEx (broom-fw.dll) on the volume path broom-bootorder.ps1 passes as -Vol - well
# under a millisecond, and without the Storage module this process holds ~100 MB less all session. No DLL / no -Vol
# (older bootorder script): Get-Volume as before.
$fw = $false
if ($Vol) {
  $dll = "$env:SystemRoot\Temp\broom-fw.dll"
  if (-not [IO.File]::Exists($dll)) { try { [IO.File]::Copy("$Vol\broom\broom-fw.dll", $dll) } catch { } }
  try { Add-Type -Path $dll -ErrorAction Stop; $fw = [BroomFw]::Free("$Vol\") -ge 0 } catch { }
}
function FreeBytes {
  if ($fw) { return [BroomFw]::Free("$Vol\") }
  $v = Get-Volume -FileSystemLabel BROOMWIN
  if ($v) { [int64]$v.SizeRemaining } else { -1 }
}
$warn = 10GB; $reboot = 3GB
try {
  $c = (Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 -Uri "http://$Srv/api/client-config").Content.Trim() -split '\s+'
  if ($c.Count -ge 3 -and $c[1] -match '^\d+$' -and $c[2] -match '^\d+$') { $warn = [int64]$c[1] * 1GB; $reboot = [int64]$c[2] * 1GB }
} catch { }
function Report($free) {
  try { Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 -Method Post -Uri "http://$Srv/api/ssd-low?free=$free" | Out-Null } catch { }
}
# Remote agent (vnc.rs): start broom-vnc.exe (SYSTEM, this session 0) -> it relaunches itself into the console session.
# A Global mutex in the agent keeps it to one copy, so this is safe to call again. Local build (no exe shipped):
# compile broom-vnc.cs once into Temp. No -Vol (older bootorder) -> skip.
function StartAgent {
  if (-not $Vol) { return }
  $exe = "$env:SystemRoot\Temp\broom-vnc.exe"
  if (-not [IO.File]::Exists($exe)) {
    $src = "$Vol\broom\broom-vnc.exe"
    if ([IO.File]::Exists($src)) { try { [IO.File]::Copy($src, $exe) } catch { } }
    elseif ([IO.File]::Exists("$Vol\broom\broom-vnc.cs")) {
      # The .NET Framework's own csc (in every Windows), same flags as CI. Not Add-Type: PowerShell 5.1 has no
      # -CompilerOptions, and the agent needs /unsafe.
      $csc = "$env:SystemRoot\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
      if (-not [IO.File]::Exists($csc)) { $csc = "$env:SystemRoot\Microsoft.NET\Framework\v4.0.30319\csc.exe" }
      try { & $csc /nologo /unsafe /target:winexe /r:System.Drawing.dll /r:System.Windows.Forms.dll /r:System.Management.dll "/out:$exe" "$Vol\broom\broom-vnc.cs" | Out-Null } catch { }
    }
  }
  if ([IO.File]::Exists($exe)) { try { Start-Process $exe -ArgumentList $Srv -WindowStyle Hidden } catch { } }
}
StartAgent
# Boot time (Benchmark page): the desktop is up once explorer runs (guest logged on) -> tell the server, which times it
# from the menu choice on its own clock. Checked every second for up to 15 minutes, then the watchdog goes on.
for ($i = 0; $i -lt 900; $i++) {
  if (Get-Process explorer -ErrorAction SilentlyContinue) {
    try { Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 -Method Post -Uri "http://$Srv/api/bench/booted" | Out-Null } catch { }
    break
  }
  Start-Sleep 1
}
$warned = [datetime]::MinValue
$reported = $false
while ($true) {
  $free = FreeBytes
  if ($free -ge 0) {
    $gb = [math]::Round($free / 1GB, 1)
    if ($free -lt $reboot) {
      Report $free
      & msg * /TIME:60 "O cung may sap het cho (con $gb GB). May se tu khoi dong lai sau 60 giay - hay luu game / cong viec ngay."
      & shutdown /r /t 60 /c "Broom: SSD full - restarting to reset the session"
      break
    }
    if ($free -lt $warn -and ((Get-Date) - $warned).TotalMinutes -ge 5) {
      & msg * /TIME:60 "O cung may sap het cho (con $gb GB). Hay luu game / cong viec: may se tu khoi dong lai khi het cho."
      $warned = Get-Date
      if (-not $reported) { Report $free; $reported = $true }
    }
  }
  StartAgent   # re-ensure the remote agent is up (guest killed it, or it crashed)
  Start-Sleep 15
}
