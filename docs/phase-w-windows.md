# Phase W — Windows diskless (native VHDX boot from the client SSD)

Golden = a sysprepped Windows **Pro** VM → upload the `.vmdk` → the server handles the rest. Clients run
Windows **from the local SSD**; once synced, the network/server is barely used; every boot **resets clean**.

## How it works (for debugging)
Client SSD (the stage partitions it on first boot — **WIPES the disk**):
```
p1 ESP  BROOMEFI  \EFI\Microsoft\Boot\{bootmgfw.efi, BCD → vhd=[locate]\broom\child.vhdx}
p2 NTFS BROOMWIN  \broom\golden.vhdx      (downloaded from the server, sha256 compared every boot)
                        base.vhdx          (golden specialized on THIS machine — created once)
                        child.vhdx         (write layer, reset every boot)
```
Every power-on: PXE → iPXE → **stage** (small Linux, server kernel) → sync/reset → `efibootmgr`
BootNext → reboot → Windows Boot Manager on the SSD → `child.vhdx`. Adds ~20–30 s per boot.

**BootOrder:** PXE (network) **first** — set in each machine's BIOS/UEFI. Windows pulls "Windows Boot
Manager" to the top on every boot → the order `EFI Network (PXE) → Broom Windows → Windows Boot
Manager → the rest` is **enforced** from 2 sides: the stage (every run) + the **BroomBootOrder** task inside
Windows (SYSTEM, at startup + every 5 minutes, created when base is committed). NVRAM is written only when
the order differs. The stage also removes the fallback loader `\EFI\Boot` (copied by bcdboot) so the
firmware's default disk entry can't boot Windows directly. Server/PXE not answering → the firmware falls
through to "Broom Windows": the machine still works but that session is **not reset and gets no new golden**.
To check the stage ran: the stage screen prints `-> Windows (reset)` (don't infer it from the absence of OOBE).
The BROOMWIN volume has no drive letter in Windows (GPT bit 63) — users can't see the broom files.

First boot of each machine (or after publishing a new golden): the stage downloads the golden (slow) →
Windows runs specialize + OOBE (automatic via unattend) + first logon → `broom-done.ps1` writes `base.ok` +
reboots by itself → the stage commits `base.vhdx` → from then on only the child is reset. The machine's
hardware drivers are installed during this pass → machines of different models don't affect each other.
The Windows computer name comes from the Machines table (by MAC); renaming a machine rebuilds its base once.

## 1. Server
Deploy the new binary → preflight installs the missing packages by itself (fdisk, ntfs-3g, dosfstools,
efibootmgr, initramfs-tools, wget). The server kernel must have `ntfs3` (Ubuntu 22.04+ does) — publish
reports a clear error if it's missing.

## 2. Golden VM (VMware)
1. New VM with **UEFI firmware**, 1 disk, install Windows 10/11 **Pro**.
2. At the OOBE screen: **Ctrl+Shift+F3** → Audit Mode (logged in as Administrator, no user created yet —
   fewest sysprep errors).
3. Load the client machines' drivers into the driver store (the VM lacks that hardware, so add only, no install):
   `pnputil /add-driver D:\drivers\*.inf /subdirs`
4. Install games/apps. Turn off Store auto-update if sysprep complains about Appx.
5. **Snapshot the VM** (to change the golden later: revert the snapshot → change → rerun step 6 → upload).
6. PowerShell as **Administrator** inside the VM:
   ```powershell
   irm http://SERVER/broom-prep-win | iex
   ```
   The first run may restart by itself to disable the pagefile → **run the command again**. The script:
   diskless tweaks (no VHDX expansion, hibernate/pagefile/restore/defrag/indexing/Windows Update off),
   builds the EFI bundle in `C:\broom\efi`, unattend (guest user + autologon, same user/password setting
   as Linux on the web), then **sysprep → the VM powers off by itself**. Don't start the VM again
   (it would run OOBE and change the golden).
7. Web → Images → Golden card: Name, **OS = windows**, choose the `.vmdk` (VMware split into several files →
   `vmware-vdiskmanager -r disk.vmdk -t 0 golden.vmdk` or zip the whole VM folder) → Create + Upload +
   Publish. The job shows ✓ when done (convert + partition extraction + VHDX + stage build).

Check on the server:
```bash
ls -la /srv/tftp/broom-win/<name>/   # golden.vhdx base-template.vhdx child-template.vhdx child-template.off efi.tar.gz
ls -la /srv/tftp/broom-stage/        # vmlinuz stage.img
```

## 3. Client — test
A real machine of the target model, UEFI, **Secure Boot OFF**, PXE first in BootOrder.

- [ ] Boot 1 (empty SSD): the stage prints `partitioning ... for the first time`, downloads the golden,
      `-> Windows (first boot ...)` → Windows specializes, reaches the guest desktop, **reboots by itself** →
      stage `base done` → Windows → desktop.
- [ ] Boot 2: stage `-> Windows (reset)`, nothing downloaded; fast to the desktop, no OOBE.
- [ ] Create a file on the Desktop + install an app → reboot → **all gone**.
- [ ] After Windows has run: Linux live USB `efibootmgr` → BootOrder **not** pushed ahead of PXE by
      Windows. If it is → report it (another guard is needed).
- [ ] Main games + anti-cheat run.
- [ ] Publish a new golden → the client re-downloads + specializes once more.
- [ ] 2 machines of different models run side by side without affecting each other.

## Troubleshooting
- Stage error → stops at the `(initramfs)` shell with a `broom stage ERROR: ...` line (send a screenshot).
- Windows reports the VHD not found / BSOD while booting the child → suspect the child VHDX (vhdx.rs) →
  capture the error code. Quick try: Linux live USB, delete `\broom\base.vhdx` + `child-local.vhdx` →
  boot again (first-boot path).
- Sysprep errors: `C:\Windows\System32\Sysprep\Panther\setupact.log` inside the VM.
- Unattend uses the `Administrators` group (English Windows). Windows in another language renames the
  group → the user can't be created.
- Limit: publishing a new golden = every machine downloads the full file again (30 machines × ~25 GB over
  1 GbE ≈ 1–2 h) → publish after hours + WOL all machines.
