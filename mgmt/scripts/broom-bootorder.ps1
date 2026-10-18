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
    # BroomFw (broom-fw.cs: UEFI variables): the release's broom-fw.dll, compiled by CI and checked by the stage on
    # BROOMWIN like these scripts, copied to Temp and loaded from there - no C# compiler run on a machine someone is
    # playing on. A local build ships only broom-fw.cs: compiled into the same Temp file. The Temp name carries the
    # source's sha256: C:\Windows\Temp is written while base is built and ends up in base, where a fixed name would keep
    # that build for good.
    $src = $v.Path + 'broom\broom-fw.cs'
    $ship = $v.Path + 'broom\broom-fw.dll'
    $from = if ([IO.File]::Exists($ship)) { $ship } else { $src }
    $fs = [IO.File]::OpenRead($from)
    try { $h = -join ([Security.Cryptography.SHA256]::Create().ComputeHash($fs)[0..7] | ForEach-Object { $_.ToString('x2') }) }
    finally { $fs.Dispose() }
    $dll = "$env:SystemRoot\Temp\broom-fw-$h.dll"
    if (-not [IO.File]::Exists($dll) -and $from -eq $ship) { try { [IO.File]::Copy($ship, $dll, $true) } catch { } }
    try { Add-Type -Path $dll -ErrorAction Stop }
    catch {
      # Not shipped / cut off while written: compile it again; still failing -> in memory.
      try {
        Add-Type -TypeDefinition ([IO.File]::ReadAllText($src)) -OutputAssembly $dll -OutputType Library -ErrorAction Stop
        Add-Type -Path $dll -ErrorAction Stop
      } catch { if (-not ('BroomFw' -as [type])) { Add-Type -TypeDefinition ([IO.File]::ReadAllText($src)) } }
    }
    $of = $v.Path + 'broom\bootorder.txt'
    if (-not [IO.File]::Exists($of)) { return }
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
# Pagefile (Windows page -> SSD room; the stage leaves its size in GB in broom\pagefile.txt): C: is a VHDX booted
# natively, where Windows refuses a paging file -> one of that fixed size next to it on BROOMWIN, made now
# (NtCreatePagingFile: no drive letter, no registry entry, so no "temporary paging file" message), encrypted with a key
# of this boot. The stage deletes it with the child at every reset. How it went -> broom\pagefile.result (the stage
# logs it at the next boot).
try {
  $pf = $v.Path + 'broom\pagefile.txt'
  $res = $v.Path + 'broom\pagefile.result'
  if ([IO.File]::Exists($pf) -and ('BroomFw' -as [type])) {
    $gb = [int64]([IO.File]::ReadAllText($pf).Trim())
    $file = $v.Path + 'broom\pagefile.sys'
    $have = 0; if ([IO.File]::Exists($file)) { $have = (New-Object IO.FileInfo $file).Length }
    $free = [BroomFw]::Free($v.Path)
    if ($gb -lt 1 -or $gb -gt 64) { $msg = "bad size $gb GB" }
    elseif ($free -ge 0 -and $free + $have - $gb * 1GB -lt 3GB) { $msg = 'skipped: only {0:N1} GB free' -f ($free / 1GB) }
    else {
      & fsutil behavior set encryptpagingfile 1 | Out-Null
      $st = [BroomFw]::Pagefile('\??\' + $v.Path.Substring(4) + 'broom\pagefile.sys', $gb * 1GB)
      $msg = if ($st -eq 0) { "$gb GB in use" } else { 'failed, NTSTATUS 0x{0:X8}' -f $st }
    }
    [IO.File]::WriteAllText($res, $msg)
  }
} catch { try { [IO.File]::WriteAllText($res, "failed: $_") } catch { } }
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
# Drivers (Drivers page): the stage says when to install this machine's packages (checked against the server's sha256,
# extracted into broom\drivers) in broom\drvinstall.txt: "keep" = into the base being built / the open base (base
# mode); "once" = a driver session: into a fresh driver layer over the base (drvs.vhdx), then drvs.ok + a restart, and
# the stage keeps that layer under every later boot - a new or updated package is installed once, not at every boot.
# Once per boot (this task also runs every 5 minutes). Before the games disk: a network driver drops the link for a
# moment. Copied to a local folder first (pnputil wants a normal path); only drivers matching real devices get
# installed; a new display driver blanks the screen for a moment.
$dmark = "$env:SystemRoot\Temp\broom-drivers.txt"
$dd = $v.Path + 'broom\drivers'
$di = $v.Path + 'broom\drvinstall.txt'
if ([IO.Directory]::Exists($dd) -and [IO.File]::Exists($di) -and (-not (Test-Path $dmark) -or (Get-Content $dmark -Raw).Trim() -ne $bootId)) {
  try {
    $tmp = "$env:SystemRoot\Temp\broom-drivers"
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    foreach ($f in [IO.Directory]::GetFiles($dd, '*', [IO.SearchOption]::AllDirectories)) {
      if ($f.EndsWith('.sha256') -or $f.EndsWith('.tar.gz')) { continue }
      $t = $tmp + $f.Substring($dd.Length)
      [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($t)) | Out-Null
      [IO.File]::Copy($f, $t, $true)
    }
    if (Test-Path $tmp) { & pnputil /add-driver "$tmp\*.inf" /subdirs /install | Out-Null }
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    Set-Content -Encoding ascii $dmark $bootId
    if ([IO.File]::ReadAllText($di).Trim() -eq 'once') {
      [IO.File]::WriteAllText($v.Path + 'broom\drvs.ok', 'ok')
      Restart-Computer -Force
      exit
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
