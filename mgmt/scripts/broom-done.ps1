$v = Get-Volume -FileSystemLabel BROOMWIN -ErrorAction SilentlyContinue
if (-not $v) { exit }
# Progress: on this console (FirstLogonCommands window) + broom\done.log on BROOMWIN (kept, readable later).
$lf = $v.Path + 'broom\done.log'
function step($m) {
  Write-Host ('[broom {0:HH:mm:ss}] {1}' -f (Get-Date), $m) -ForegroundColor Cyan
  try { [IO.File]::AppendAllText($lf, ('{0:yyyy-MM-dd HH:mm:ss} {1}' -f (Get-Date), $m) + "`r`n") } catch { }
}
step 'building base for this machine (once) - wait for the BASE MODE message'
# SYSTEM task (stored in base.vhdx -> present every boot): keep Windows AFTER PXE in BootOrder, PXE first (strict
# reset: Windows entries out of BootOrder, boot loader removed from the ESP); also reports each boot to the server.
step 'boot order task (PXE first)'
$s = "$env:SystemRoot\Setup\Scripts\broom-bootorder.ps1"
$a = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File $s"
$t1 = New-ScheduledTaskTrigger -AtStartup
$t2 = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes 5) -RepetitionDuration (New-TimeSpan -Days 3650)
try { Register-ScheduledTask -TaskName BroomBootOrder -Action $a -Trigger $t1,$t2 -User SYSTEM -RunLevel Highest -Force -ErrorAction Stop | Out-Null }
catch { step "boot order task FAILED: $_ (PXE may not stay first, boots are not reported)" }
& powershell -NoProfile -ExecutionPolicy Bypass -File $s
# Drivers (Drivers page): the stage put this machine's packages in broom\drivers -> install them into base.
# Copied to a local folder first (pnputil wants a normal path); only drivers matching real devices get installed.
$dd = $v.Path + 'broom\drivers'
if ([IO.Directory]::Exists($dd)) {
  $tmp = "$env:SystemRoot\Temp\broom-drivers"
  Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
  foreach ($f in [IO.Directory]::GetFiles($dd, '*', [IO.SearchOption]::AllDirectories)) {
    if ($f.EndsWith('.sha256') -or $f.EndsWith('.tar.gz')) { continue }
    $t = $tmp + $f.Substring($dd.Length)
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($t)) | Out-Null
    [IO.File]::Copy($f, $t, $true)
  }
  step 'installing driver packages from the server'
  if (Test-Path $tmp) { & pnputil /add-driver "$tmp\*.inf" /subdirs /install | Out-Null }
  Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
# Machine name from the server (stage writes broom\host.txt): rename -> takes effect after the restart that saves base.
$h = $v.Path + 'broom\host.txt'
if ([IO.File]::Exists($h)) {
  $n = [IO.File]::ReadAllText($h).Trim()
  if ($n -and ($n -ne $env:COMPUTERNAME)) {
    step "machine name -> $n"
    try { Rename-Computer -NewName $n -Force -ErrorAction Stop } catch { step "rename FAILED: $_ (base keeps the old name)" }
  }
}
# License key (Machines page): the server picks it by this machine's IP and hands it out once, right after a PXE
# boot (403 = none).
# slmgr /cpky afterwards: the key is not left readable in the registry. Never blocks building base.
$sf = $v.Path + 'broom\srv.txt'
if ([IO.File]::Exists($sf)) {
  $srv = [IO.File]::ReadAllText($sf).Trim()
  $k = ''
  try { $k = (Invoke-WebRequest -UseBasicParsing -TimeoutSec 15 -Method Post -Uri "http://$srv/api/license").Content.Trim() } catch { }
  if ($k) {
    step 'activating the license key'
    $slmgr = "$env:SystemRoot\System32\slmgr.vbs"
    $r = (& cscript //nologo $slmgr /ipk $k | Out-String) + (& cscript //nologo $slmgr /ato | Out-String)
    & cscript //nologo $slmgr /cpky | Out-Null
    $k = ''
    try { Invoke-WebRequest -UseBasicParsing -TimeoutSec 15 -Method Post -Body $r -Uri "http://$srv/api/license/result" | Out-Null } catch { }
  }
}
# Golden hook C:\broom\base-hook.ps1 (optional, put there by the admin before prep): runs ONCE per machine while base
# is built, before the reboot that commits base -> what it does is kept every boot (e.g. an anti-cheat's first-run
# setup that wants one restart). Same console (its output shows), max 10 minutes, its errors never block base.
$hook = "$env:SystemDrive\broom\base-hook.ps1"
if ([IO.File]::Exists($hook)) {
  step 'running C:\broom\base-hook.ps1'
  try {
    $p = Start-Process powershell.exe -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$hook`"" -NoNewWindow -PassThru
    if ($p.WaitForExit(600000)) { step 'hook done' } else { $p.Kill(); step 'hook still running after 10 minutes -> stopped' }
  } catch { step "hook failed: $_" }
}
# Write base.ok DIRECTLY via the volume path (\\?\Volume{..}\broom\base.ok): no drive letter/mountvol needed
# (the old version picked a letter via Test-Path -> clashed with an empty CD drive -> write failed -> OOBE loop every boot).
$f = $v.Path + 'broom\base.ok'
try { [IO.File]::WriteAllText($f, 'ok') } catch { }
if (-not [IO.File]::Exists($f)) { step 'could not write base.ok on BROOMWIN -> base is rebuilt next boot'; exit }
# Default (base mode off for this image): restart NOW -> the stage commits base before anyone can use the machine, so
# a guest's session can never become the base.
if (-not [IO.File]::Exists($v.Path + 'broom\basemode.txt')) {
  step 'base ready - restarting to save it'
  Restart-Computer -Force
  exit
}
# BASE MODE (ticked on the image): nothing restarts by itself. The technician sets up what must survive the reset on
# THIS machine (e.g. FACEIT AC: open it, it wants one restart through its own RESTART button), then restarts -> that
# restart commits base. Nobody there -> the next restart / power-off commits it, whoever used the machine meanwhile.
step 'BASE MODE: set up apps now (e.g. open FACEIT AC), then RESTART - that restart saves base for every boot'
$msg = "BASE MODE - this machine is building its base.`n`n" +
  "Everything done now is KEPT on this machine after every reset.`n`n" +
  "1. Open FACEIT AC (or other apps) and set them up.`n" +
  "2. Restart (FACEIT's RESTART button, or Start > Restart).`n`n" +
  "That restart saves the base. Afterwards every boot resets to it."
# 0x40 information icon + 0x1000 system modal (stays on top of the desktop).
try { (New-Object -ComObject WScript.Shell).Popup($msg, 0, 'Broom - BASE MODE', 0x1040) | Out-Null } catch { }