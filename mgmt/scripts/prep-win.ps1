$ErrorActionPreference = 'Stop'
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not ([Security.Principal.WindowsPrincipal]$id).IsInRole('Administrators')) { throw 'Run PowerShell as Administrator' }

# 1. Native VHD boot: do NOT expand the VHDX to full size; no automatic device encryption.
reg add HKLM\SYSTEM\CurrentControlSet\Services\FsDepends\Parameters /v VirtualDiskExpandOnMount /t REG_DWORD /d 4 /f | Out-Null
reg add HKLM\SYSTEM\CurrentControlSet\Control\BitLocker /v PreventDeviceEncryption /t REG_DWORD /d 1 /f | Out-Null
# VBS + Memory integrity (HVCI), needs only Secure Boot: runs on clients with VT-x, ignored elsewhere.
# Drivers that aren't HVCI-compatible get blocked (e.g. VMware e1000 NIC → use e1000e).
$dg = 'HKLM\SYSTEM\CurrentControlSet\Control\DeviceGuard'
reg add $dg /v EnableVirtualizationBasedSecurity /t REG_DWORD /d 1 /f | Out-Null
reg add $dg /v RequirePlatformSecurityFeatures /t REG_DWORD /d 1 /f | Out-Null
reg add "$dg\Scenarios\HypervisorEnforcedCodeIntegrity" /v Enabled /t REG_DWORD /d 1 /f | Out-Null
# The web shows ON/OFF by ICMP ping (monitor.rs) — Windows Firewall drops echo requests by default.
netsh advfirewall firewall delete rule name="Broom ping" | Out-Null
netsh advfirewall firewall add rule name="Broom ping" protocol=icmpv4:8,any dir=in action=allow profile=any | Out-Null
# 2. Reset every boot → turn off pointless writes.
powercfg /h off
Disable-ComputerRestore -Drive "$env:SystemDrive\" -ErrorAction SilentlyContinue
Disable-ScheduledTask -TaskPath '\Microsoft\Windows\Defrag\' -TaskName ScheduledDefrag -ErrorAction SilentlyContinue | Out-Null
Set-Service WSearch -StartupType Disabled -ErrorAction SilentlyContinue
reg add HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU /v NoAutoUpdate /t REG_DWORD /d 1 /f | Out-Null
# Reserved storage (space kept for updates): a pending update holding it fails sysprep with 0x800F0975.
dism.exe /Online /Set-ReservedStorageState /State:Disabled /Quiet | Out-Null
$rm = 'HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\ReserveManager'
reg add $rm /v ShippedWithReserves /t REG_DWORD /d 0 /f | Out-Null
reg add $rm /v ActiveScenario /t REG_DWORD /d 0 /f | Out-Null
$cs = Get-CimInstance Win32_ComputerSystem
if ($cs.AutomaticManagedPagefile -or (Get-CimInstance Win32_PageFileSetting)) {
  Set-CimInstance $cs -Property @{AutomaticManagedPagefile = $false}
  Get-CimInstance Win32_PageFileSetting | Remove-CimInstance
}
if (Get-CimInstance Win32_PageFileUsage) {
  Write-Host '>>> Pagefile disabled. The machine will RESTART - then RUN this command AGAIN.' -ForegroundColor Yellow
  Start-Sleep 5; Restart-Computer -Force; return
}

# 3. EFI bundle: temporary ESP (FAT32 vdisk) → bcdboot → BCD points to vhd=[locate]\broom\child.vhdx.
$B = "$env:SystemDrive\broom"
# Rebuilt from scratch, except the admin's base-hook.ps1 (broom-done runs it while base is built).
$hookf = "$B\base-hook.ps1"
$hook = if (Test-Path $hookf) { [IO.File]::ReadAllBytes($hookf) }
Remove-Item $B -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory "$B\efi" | Out-Null
if ($hook) { [IO.File]::WriteAllBytes($hookf, $hook); Write-Host '>>> keeping C:\broom\base-hook.ps1' -ForegroundColor Green }
$vd = "$env:TEMP\broom-esp.vhdx"
Remove-Item $vd -ErrorAction SilentlyContinue
$L = (69..90 | ForEach-Object { [char]$_ } | Where-Object { -not (Test-Path "${_}:\") } | Select-Object -Last 1)
@"
create vdisk file="$vd" maximum=300 type=expandable
attach vdisk
convert gpt
create partition primary
format quick fs=fat32 label=BROOMESP
assign letter=$L
"@ | Set-Content -Encoding ascii "$env:TEMP\broom-esp.txt"
diskpart /s "$env:TEMP\broom-esp.txt" | Out-Null
bcdboot "$env:SystemRoot" /s "${L}:" /f UEFI | Out-Null
$S = "${L}:\EFI\Microsoft\Boot\BCD"
bcdedit /store $S /set '{default}' device 'vhd=[locate]\broom\child.vhdx' | Out-Null
bcdedit /store $S /set '{default}' osdevice 'vhd=[locate]\broom\child.vhdx' | Out-Null
bcdedit /store $S /set '{default}' description 'Broom Windows' | Out-Null
bcdedit /store $S /timeout 0 | Out-Null
Copy-Item "${L}:\EFI" "$B\efi\EFI" -Recurse
@"
select vdisk file="$vd"
detach vdisk
"@ | Set-Content -Encoding ascii "$env:TEMP\broom-esp.txt"
diskpart /s "$env:TEMP\broom-esp.txt" | Out-Null
Remove-Item $vd, "$env:TEMP\broom-esp.txt" -ErrorAction SilentlyContinue

# 4. First logon on the client (when base is created): write base.ok to NTFS BROOMWIN + reboot → the stage commits base.
#    Both scripts here are fixed stubs: they run the current copy the stage puts in BROOMWIN broom\ (from the server)
#    → logic changes don't need prep again.
$done = @'
__STUB_DONE__
'@
$order = @'
__STUB_BOOTORDER__
'@
New-Item -ItemType Directory "$env:SystemRoot\Setup\Scripts" -Force | Out-Null
Set-Content -Encoding ascii "$env:SystemRoot\Setup\Scripts\broom-done.ps1" $done
Set-Content -Encoding ascii "$env:SystemRoot\Setup\Scripts\broom-bootorder.ps1" $order

# 5. Unattend: skip OOBE, guest user (Administrators — FirstLogonCommands need write access) + autologon.
$ui = (Get-UICulture).Name; $sl = (Get-WinSystemLocale).Name; $ul = (Get-Culture).Name
$tz = (Get-TimeZone).Id
$c = 'processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS"'
@"
<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
  <settings pass="specialize">
    <component name="Microsoft-Windows-Shell-Setup" $c>
      <ComputerName>*</ComputerName>
      <TimeZone>$tz</TimeZone>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-International-Core" $c>
      <InputLocale>$ul</InputLocale><SystemLocale>$sl</SystemLocale><UILanguage>$ui</UILanguage><UserLocale>$ul</UserLocale>
    </component>
    <component name="Microsoft-Windows-Shell-Setup" $c>
      <OOBE>
        <SkipMachineOOBE>true</SkipMachineOOBE>
        <SkipUserOOBE>true</SkipUserOOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <HideLocalAccountScreen>true</HideLocalAccountScreen>
        <ProtectYourPC>3</ProtectYourPC>
      </OOBE>
      <UserAccounts><LocalAccounts><LocalAccount wcm:action="add">
        <Name>__USER__</Name><DisplayName>__USER__</DisplayName><Group>Administrators</Group>
        <Password><Value>__PASS__</Value><PlainText>true</PlainText></Password>
      </LocalAccount></LocalAccounts></UserAccounts>
      <AutoLogon>
        <Enabled>true</Enabled><Username>__USER__</Username><LogonCount>999999</LogonCount>
        <Password><Value>__PASS__</Value><PlainText>true</PlainText></Password>
      </AutoLogon>
      <FirstLogonCommands>
        <SynchronousCommand wcm:action="add"><Order>1</Order>
          <CommandLine>powershell -NoProfile -ExecutionPolicy Bypass -File %WINDIR%\Setup\Scripts\broom-done.ps1</CommandLine>
        </SynchronousCommand>
      </FirstLogonCommands>
    </component>
  </settings>
</unattend>
"@ | Set-Content -Encoding utf8 "$B\unattend.xml"

# 6. Less churn between two builds of this golden (clients download only the 4 MB chunks that changed):
#    throw away caches/logs that differ on every build, then TRIM → freed space reads as zeros in the exported disk
#    (VMware thin disks reclaim it) → "zero" chunks, never downloaded. Best effort: nothing here may stop the prep.
$ErrorActionPreference = 'Continue'
Stop-Service wuauserv, bits, dosvc -Force -ErrorAction SilentlyContinue
foreach ($p in "$env:SystemRoot\SoftwareDistribution\Download", "$env:SystemRoot\Temp", $env:TEMP,
               "$env:SystemRoot\Prefetch", "$env:ProgramData\Microsoft\Windows\DeliveryOptimization\Cache",
               "$env:SystemRoot\Logs\CBS", "$env:SystemRoot\LiveKernelReports", "$env:SystemRoot\Minidump") {
  Get-ChildItem $p -Force -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
}
Remove-Item "$env:SystemRoot\MEMORY.DMP" -Force -ErrorAction SilentlyContinue
Write-Host '>>> Clearing event logs + TRIM of the free space...' -ForegroundColor Green
foreach ($l in (wevtutil el)) { wevtutil cl "$l" 2>&1 | Out-Null }
Optimize-Volume -DriveLetter $env:SystemDrive[0] -ReTrim -ErrorAction SilentlyContinue

# 7. Sysprep (generalize, then return here) → boot-start disk drivers → power off the VM.
#    Errors: see C:\Windows\System32\Sysprep\Panther\setupact.log.
$ErrorActionPreference = 'Stop'
Write-Host '>>> Sysprep... the VM will POWER OFF by itself. Then upload it on the web (OS = windows).' -ForegroundColor Green
$tag = "$env:SystemRoot\System32\Sysprep\Sysprep_succeeded.tag"
Remove-Item $tag -ErrorAction SilentlyContinue
Start-Process -Wait "$env:SystemRoot\System32\Sysprep\sysprep.exe" -ArgumentList '/generalize', '/oobe', '/quit', "/unattend:$B\unattend.xml"
if (-not (Test-Path $tag)) { throw 'sysprep failed - see C:\Windows\System32\Sysprep\Panther\setupact.log' }
# Windows' OWN storage drivers (SATA/NVMe/RAID) start at boot on ANY client controller: native VHD boot needs the
# SSD controller's driver before anything else runs. Set AFTER generalize (it would reset them), drivers this Windows
# has only. Per-machine drivers (NIC, GPU...) come from the Drivers page when each machine builds its base.
$on = @()
foreach ($d in @(__BOOT_STORAGE__)) {
  $k = "HKLM\SYSTEM\CurrentControlSet\Services\$d"
  if (Test-Path "Registry::$k") {
    reg add $k /v Start /t REG_DWORD /d 0 /f | Out-Null
    reg add "$k\StartOverride" /v 0 /t REG_DWORD /d 0 /f | Out-Null
    $on += $d
  }
}
Set-Content -Encoding ascii "$B\boot-storage.ok" ($on -join ',')
Stop-Computer -Force
