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
# Temp copies are named after their source's sha256: C:\Windows\Temp is written while base is built and so ends up in
# base, where a fixed name would keep that build for good (a newer one from the server never copied over it).
function Sha16($p) {
  $s = [IO.File]::OpenRead($p)
  try { -join ([Security.Cryptography.SHA256]::Create().ComputeHash($s)[0..7] | ForEach-Object { $_.ToString('x2') }) }
  finally { $s.Dispose() }
}
$fw = $false
if ($Vol -and [IO.File]::Exists("$Vol\broom\broom-fw.dll")) {
  try {
    $dll = "$env:SystemRoot\Temp\broom-fw-" + (Sha16 "$Vol\broom\broom-fw.dll") + '.dll'
    if (-not [IO.File]::Exists($dll)) { [IO.File]::Copy("$Vol\broom\broom-fw.dll", $dll, $true) }
    Add-Type -Path $dll -ErrorAction Stop; $fw = [BroomFw]::Free("$Vol\") -ge 0
  } catch { }
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
# The agent's Temp copy, named after its source's sha256 (see Sha16), worked out once: the source only changes at boot
# (the stage). Release: the shipped exe; local build: compiled from broom-vnc.cs.
$agentExe = $null
if ($Vol) {
  $ship = "$Vol\broom\broom-vnc.exe"; $cs = "$Vol\broom\broom-vnc.cs"
  $from = if ([IO.File]::Exists($ship)) { $ship } elseif ([IO.File]::Exists($cs)) { $cs } else { $null }
  if ($from) {
    try {
      $agentExe = "$env:SystemRoot\Temp\broom-vnc-" + (Sha16 $from) + '.exe'
      if (-not [IO.File]::Exists($agentExe)) {
        if ($from -eq $ship) { [IO.File]::Copy($ship, $agentExe, $true) }
        else {
          # The .NET Framework's own csc (in every Windows), same flags as CI. Not Add-Type: PowerShell 5.1 has no
          # -CompilerOptions, and the agent needs /unsafe.
          $csc = "$env:SystemRoot\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
          if (-not [IO.File]::Exists($csc)) { $csc = "$env:SystemRoot\Microsoft.NET\Framework\v4.0.30319\csc.exe" }
          & $csc /nologo /unsafe /target:winexe /r:System.Drawing.dll /r:System.Windows.Forms.dll /r:System.Management.dll "/out:$agentExe" $cs | Out-Null
        }
      }
    } catch { $agentExe = $null }
  }
}
function StartAgent {
  if (-not $agentExe -or -not [IO.File]::Exists($agentExe)) { return }
  # An agent of another build still running (an older Temp copy, kept in base): stop it, or its mutex keeps this one
  # out. Then its file can go.
  Get-Process -Name 'broom-vnc*' -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path -ne $agentExe } |
    ForEach-Object { try { $_.Kill(); $_.WaitForExit(5000) | Out-Null } catch { } }
  Get-ChildItem "$env:SystemRoot\Temp\broom-vnc*.exe" -ErrorAction SilentlyContinue | Where-Object { $_.FullName -ne $agentExe } |
    ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
  # Its key (agentauth.rs), left on BROOMWIN by the stage: read here (PowerShell reads the volume path the agent's
  # .NET 4 file API can't), handed over on its command line. No key yet = the agent waits for the next boot.
  $key = ''
  try { if ($Vol) { $key = ([IO.File]::ReadAllText("$Vol\broom\agent.key")).Trim() } } catch { }
  try { Start-Process $agentExe -ArgumentList $Srv, $key -WindowStyle Hidden } catch { }
}
StartAgent
# Other disks hidden from the guest (Devices / Groups page; the stage leaves broom\hide-disks): every internal disk but
# the Broom disk taken offline, again every 15 s (the guest is an admin and could bring one back). Offline is kept in
# the registry, which resets with the session: turned off on the server, the disks are back after the next boot.
# The Broom disk = the one holding BROOMWIN; exactly one must be found, else nothing is touched (taking that disk
# offline would pull Windows' own VHDX away). Virtual disks (C:, games) and USB stay. Get-Disk loads the Storage
# module: only on machines that hide.
$hide = $Vol -and [IO.File]::Exists("$Vol\broom\hide-disks")
function HideDisks {
  if (-not $hide) { return }
  try {
    $own = @(Get-Volume -FileSystemLabel BROOMWIN -ErrorAction Stop | Get-Partition | Get-Disk | ForEach-Object { $_.Number } | Select-Object -Unique)
    if ($own.Count -ne 1) { return }
    Get-Disk | Where-Object {
      $_.Number -ne $own[0] -and -not $_.IsOffline -and -not $_.IsBoot -and -not $_.IsSystem -and
      @('USB', 'File Backed Virtual', 'iSCSI', 'Virtual', 'SD', 'MMC') -notcontains [string]$_.BusType
    } | ForEach-Object { Set-Disk -Number $_.Number -IsOffline $true -ErrorAction SilentlyContinue }
  } catch { }
}
HideDisks
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
  HideDisks
  Start-Sleep 15
}
