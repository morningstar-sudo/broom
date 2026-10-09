# Session watchdog: the guest's writes (C: = child.vhdx, games drives = games-*-child.vhdx) all land on BROOMWIN; when
# it fills up Windows cannot grow its boot disk and stops with a blue screen (VHD_BOOT_HOST_VOLUME_NOT_ENOUGH_SPACE).
# And C: shows the VHDX's virtual free space, so Windows never warns. Checks BROOMWIN's real free space every 15 s:
# below the warn threshold -> a message to the guest (every 5 minutes) + a report to the server (Machines page); below
# the restart threshold -> restart in 60 s with a message: the session is lost either way, this way with time to save.
# Thresholds: Settings -> SSD room (GET /api/client-config "<room> <warn> <reboot>" GB). Started once per boot by
# broom-bootorder.ps1 as its own task (SYSTEM). ASCII only.
param([string]$Srv)
$ErrorActionPreference = 'SilentlyContinue'
$warn = 10GB; $reboot = 3GB
try {
  $c = (Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 -Uri "http://$Srv/api/client-config").Content.Trim() -split '\s+'
  if ($c.Count -ge 3 -and $c[1] -match '^\d+$' -and $c[2] -match '^\d+$') { $warn = [int64]$c[1] * 1GB; $reboot = [int64]$c[2] * 1GB }
} catch { }
function Report($free) {
  try { Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 -Method Post -Uri "http://$Srv/api/ssd-low?free=$free" | Out-Null } catch { }
}
$warned = [datetime]::MinValue
$reported = $false
while ($true) {
  $v = Get-Volume -FileSystemLabel BROOMWIN
  if ($v) {
    $free = [int64]$v.SizeRemaining
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
  Start-Sleep 15
}
