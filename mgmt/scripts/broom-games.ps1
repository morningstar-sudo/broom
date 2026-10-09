# Games disks (Images page) as drive letters: the server says which ones this machine gets (its group), one line each
# "<name> <iqn> <letter> <1 = this machine is that disk's update machine>". Each comes over iSCSI READ-ONLY and holds
# games.vhdx; this machine opens it through a differencing VHDX on its SSD (BROOMWIN broom\games-<name>-child.vhdx,
# deleted by the stage every boot), so a guest's writes stay on this machine until the next boot and the games disk
# never changes. A disk's update machine gets it writable and attaches games.vhdx itself: the server keeps those
# writes apart until the admin saves them. Run by broom-bootorder.ps1 (SYSTEM, at startup + every 5 minutes): does
# what is missing, nothing once every drive is there. Errors -> %SystemRoot%\Temp\broom-games.log. ASCII only.
param([string]$Srv)
$ErrorActionPreference = 'Stop'
try { $list = (Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 -Uri "http://$Srv/api/games/for").Content } catch { exit }

$log = "$env:SystemRoot\Temp\broom-games.log"
function Log($m) { Add-Content -Encoding ascii $log ((Get-Date -Format s) + ' ' + $m) }
function DP([string[]]$lines) {
  $f = "$env:SystemRoot\Temp\broom-diskpart.txt"
  Set-Content -Encoding ascii $f $lines
  $o = & diskpart /s $f | Out-String
  if ($LASTEXITCODE -ne 0) { throw "diskpart: $o" }
}
# A volume mounted on a folder of C: (no drive letter for the guest to see; C: is reset every boot: done again).
function MountAt($part, $dir) {
  if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force $dir | Out-Null }
  if (-not ($part.AccessPaths -contains "$dir\")) { $part | Add-PartitionAccessPath -AccessPath $dir }
}
function BasicPart($n) { Get-Partition -DiskNumber $n -ErrorAction SilentlyContinue | Where-Object Type -eq 'Basic' | Select-Object -First 1 }
# Grow a partition to all the space after it (the disk under it grew).
function Grow($p) {
  $max = (Get-PartitionSupportedSize -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber).SizeMax
  if ($max -gt $p.Size + 1GB) { Resize-Partition -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber -Size $max }
  Get-Partition -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber
}
function NewNtfs($n, $label) {
  Initialize-Disk -Number $n -PartitionStyle GPT
  $p = New-Partition -DiskNumber $n -UseMaximumSize
  Format-Volume -Partition $p -FileSystem NTFS -NewFileSystemLabel $label -Confirm:$false | Out-Null
}
# The VHDX `$file` is attached and holds drive `$L` = this disk is up.
function Ready($file, $L) {
  $i = Get-DiskImage -ImagePath $file -ErrorAction SilentlyContinue
  $i -and $i.Attached -and ((($i | Get-Disk | Get-Partition).DriveLetter) -contains [char]$L)
}
function SetLetter($vd, $L) {
  if ($vd.IsOffline) { Set-Disk -Number $vd.Number -IsOffline $false }
  $q = BasicPart $vd.Number
  if ($q.DriveLetter -ne [char]$L) { Set-Partition -DiskNumber $vd.Number -PartitionNumber $q.PartitionNumber -NewDriveLetter $L }
}

function GamesDisk($name, $iqn, $L, $upd) {
  $store = "$env:ProgramData\broom\games\$name"
  $vhd = "$store\games.vhdx"
  $ssd = "$env:ProgramData\broom\ssd"
  $child = "$ssd\broom\games-$name-child.vhdx"
  if (Ready ($(if ($upd) { $vhd } else { $child })) $L) { return }

  # 1. Its iSCSI session (not persistent: made again every boot).
  if (-not (Get-IscsiTargetPortal | Where-Object TargetPortalAddress -eq $Srv)) { New-IscsiTargetPortal -TargetPortalAddress $Srv | Out-Null }
  $sess = Get-IscsiSession | Where-Object { $_.TargetNodeAddress -eq $iqn -and $_.IsConnected }
  if (-not $sess) { $null = Connect-IscsiTarget -NodeAddress $iqn -TargetPortalAddress $Srv }
  $disk = $null
  for ($i = 0; $i -lt 30 -and -not $disk; $i++) {
    $sess = Get-IscsiSession | Where-Object { $_.TargetNodeAddress -eq $iqn -and $_.IsConnected }
    if ($sess) { $disk = Get-Disk -iSCSISession $sess -ErrorAction SilentlyContinue }
    if (-not $disk) { Start-Sleep 1 }
  }
  if (-not $disk) { throw 'the disk did not show up' }
  if ($disk.IsOffline) { Set-Disk -Number $disk.Number -IsOffline $false }   # SAN policy: new shared disks offline
  if ($upd -and $disk.IsReadOnly) { Set-Disk -Number $disk.Number -IsReadOnly $false }
  $disk = Get-Disk -Number $disk.Number

  if ($upd) {
    # 2a. Its update machine: the disk itself, writable. A new (empty) one is formatted and gets games.vhdx; a grown
    # one is grown at every layer (partition, games.vhdx, the partition inside).
    if ($disk.PartitionStyle -eq 'RAW') { Log "$name - new disk: formatting"; NewNtfs $disk.Number 'BROOMGAMES' }
    $p = Grow (BasicPart $disk.Number)
    MountAt $p $store
    $mb = [math]::Floor($p.Size / 1MB) - 1024   # 1 GB left for NTFS + the VHDX's own metadata
    if (-not (Test-Path $vhd)) {
      DP "create vdisk file=`"$vhd`" maximum=$mb type=expandable"
    } elseif (-not (Get-DiskImage -ImagePath $vhd).Attached -and (Get-DiskImage -ImagePath $vhd).Size -lt ($mb - 1024) * 1MB) {
      Log "$name - games.vhdx grown to $mb MB"
      DP "select vdisk file=`"$vhd`"", "expand vdisk maximum=$mb"
    }
    if (-not (Get-DiskImage -ImagePath $vhd).Attached) { DP "select vdisk file=`"$vhd`"", 'attach vdisk' }
    $vd = Get-DiskImage -ImagePath $vhd | Get-Disk
    if ($vd.PartitionStyle -eq 'RAW') { NewNtfs $vd.Number $name.ToUpper() }
    $null = Grow (BasicPart $vd.Number)
    SetLetter $vd $L
    Log "$name - UPDATE mode: writable as ${L}: - shut down properly when done, then Save on the server"
  } else {
    # 2b. Every other machine: games.vhdx read-only under this boot's differencing disk on the SSD (BROOMWIN: C: is
    # a VHDX itself, another VHDX cannot live in it).
    $p = BasicPart $disk.Number
    if (-not $p) { throw 'the disk is empty: set it up from its update machine first' }
    MountAt $p $store
    if (-not (Test-Path $vhd)) { throw 'no games.vhdx on the disk yet: set it up from its update machine first' }
    MountAt (Get-Volume -FileSystemLabel BROOMWIN | Get-Partition) $ssd
    $ci = Get-DiskImage -ImagePath $child -ErrorAction SilentlyContinue
    if (-not ($ci -and $ci.Attached)) {
      if (Test-Path $child) { Remove-Item -Force $child }   # made over another version: never reused
      DP "create vdisk file=`"$child`" parent=`"$vhd`"", "select vdisk file=`"$child`"", 'attach vdisk'
    }
    SetLetter (Get-DiskImage -ImagePath $child | Get-Disk) $L
  }
}

$lines = @($list -split "`n" | Where-Object { $_ -match '^([a-z0-9-]+) (\S+) ([D-Z]) ([01])\s*$' })
if ($lines.Count -eq 0) { exit }
try {
  # The initiator service is manual by default.
  Set-Service MSiSCSI -StartupType Automatic
  Start-Service MSiSCSI
} catch { Log $_.Exception.Message; exit }
foreach ($l in $lines) {
  $null = $l -match '^([a-z0-9-]+) (\S+) ([D-Z]) ([01])\s*$'
  $name = $Matches[1]
  try { GamesDisk $name $Matches[2] $Matches[3] ($Matches[4] -eq '1') } catch { Log "$name - $($_.Exception.Message)" }
}
