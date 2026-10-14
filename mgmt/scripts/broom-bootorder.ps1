$v = Get-Volume -FileSystemLabel BROOMWIN -ErrorAction SilentlyContinue
if (-not $v) { exit }
# Boot order first (no network needed): done right after startup, whatever the steps below wait for (the games disk
# waits up to 90 s for the network) - a machine switched off meanwhile must still come back through PXE (the reset).
# Its own block: it ends early (`return`) without skipping the steps after it; an error in it skips only it.
try {
  & {
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
    if (-not [IO.File]::Exists($of)) { return }
    # BroomFw (broom-fw.cs: UEFI variables): the release's broom-fw.dll, compiled by CI and checked by the stage on
    # BROOMWIN like these scripts, copied to Temp (C: is reset every boot) and loaded from there - no C# compiler run on a
    # machine someone is playing on. A local build ships only broom-fw.cs: compiled once per boot into the same Temp file.
    $dll = "$env:SystemRoot\Temp\broom-fw.dll"
    $src = $v.Path + 'broom\broom-fw.cs'
    $ship = $v.Path + 'broom\broom-fw.dll'
    if (-not [IO.File]::Exists($dll) -and [IO.File]::Exists($ship)) { try { [IO.File]::Copy($ship, $dll, $true) } catch { } }
    try { Add-Type -Path $dll -ErrorAction Stop }
    catch {
      # Not shipped / cut off while written: compile it again; still failing -> in memory.
      try {
        Add-Type -TypeDefinition ([IO.File]::ReadAllText($src)) -OutputAssembly $dll -OutputType Library -ErrorAction Stop
        Add-Type -Path $dll -ErrorAction Stop
      } catch { if (-not ('BroomFw' -as [type])) { Add-Type -TypeDefinition ([IO.File]::ReadAllText($src)) } }
    }
    [BroomFw]::EnablePrivilege()
    $cur = [BroomFw]::Get('BootOrder')
    if ($null -eq $cur) { return }
    $now = @(); for ($i = 0; $i + 1 -lt $cur.Length; $i += 2) { $now += [BitConverter]::ToUInt16($cur, $i) }
    $want = @()
    foreach ($x in ([IO.File]::ReadAllText($of).Trim() -split ',')) {
      if ($x -notmatch '^[0-9A-Fa-f]{4}$') { continue }
      $n = [Convert]::ToUInt16($x, 16)
      # Only entries that still exist (a Boot#### variable), each once.
      if (($want -notcontains $n) -and ($null -ne [BroomFw]::Get(('Boot{0:X4}' -f $n)))) { $want += $n }
    }
    if ($want.Count -eq 0) { return }
    foreach ($n in $now) { if ($want -notcontains $n) { $want += $n } }
    # Strict: the SSD's Windows entries stay OUT (Windows re-adds "Windows Boot Manager" on every boot).
    if ($strict) {
      $drop = @()
      foreach ($x in ([IO.File]::ReadAllText($sf).Trim() -split ',')) { if ($x -match '^[0-9A-Fa-f]{4}$') { $drop += [Convert]::ToUInt16($x, 16) } }
      $want = @($want | Where-Object { $drop -notcontains $_ })
      if ($want.Count -eq 0) { return }
    }
    if (($want -join ',') -ne ($now -join ',')) {
      $b = New-Object byte[] ($want.Count * 2)
      for ($i = 0; $i -lt $want.Count; $i++) { [BitConverter]::GetBytes([uint16]$want[$i]).CopyTo($b, $i * 2) }
      [BroomFw]::Set('BootOrder', $b) | Out-Null
    }
  }
} catch { }
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
# Session watchdog (broom-watch.ps1: SSD room): its own task (SYSTEM, no time limit), started once per boot - it loops,
# it must not hold this task up. C: is reset every boot, so the task is registered again each time.
$wf = $v.Path + 'broom\broom-watch.ps1'
if ([IO.File]::Exists($wf) -and [IO.File]::Exists($srvf)) {
  try {
    $t = Get-ScheduledTask -TaskName BroomWatch -ErrorAction SilentlyContinue
    if (-not $t -or $t.State -ne 'Running') {
      $run = "$env:SystemRoot\Temp\broom-watch.run.ps1"
      [IO.File]::Copy($wf, $run, $true)
      # -Vol: BROOMWIN's volume path, for its free-space check without the Storage module. No quotes and no trailing
      # backslash: `\"` would end up as a quote in the argument.
      $arg = '-NoProfile -ExecutionPolicy Bypass -File "' + $run + '" -Srv ' + [IO.File]::ReadAllText($srvf).Trim() + ' -Vol ' + $v.Path.TrimEnd('\')
      $act = New-ScheduledTaskAction -Execute powershell.exe -Argument $arg
      $set = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
      Register-ScheduledTask -TaskName BroomWatch -Action $act -Settings $set -User SYSTEM -RunLevel Highest -Force | Out-Null
      Start-ScheduledTask -TaskName BroomWatch
    }
  } catch { }
}
# Last: the games disk (if the server has one) as a drive letter: broom-games.ps1, copied out like the stub does
# (volume path). Waits for the network at startup.
$gf = $v.Path + 'broom\broom-games.ps1'
if ([IO.File]::Exists($gf) -and [IO.File]::Exists($srvf)) {
  $run = "$env:SystemRoot\Temp\broom-games.run.ps1"
  [IO.File]::Copy($gf, $run, $true)
  # In this process (not a second powershell.exe every 5 minutes on a machine someone is playing on).
  try { & $run -Srv ([IO.File]::ReadAllText($srvf).Trim()) } catch { }
}
