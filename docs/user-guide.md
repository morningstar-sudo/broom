---
title: Broom user guide — set up and run a diskless room, step by step
description: "The Broom user guide: install the server, network boot, server disks, build and publish goldens, register PCs, versions and rollback, games disks, drivers, remote control, monitoring, backup and troubleshooting."
date: 2026-10-29T10:00:00+07:00
last_modified_at: 2026-10-29
---

[← Broom](index.md)

This guide walks through Broom from an empty server to a room in daily use. Each part says where to click in the web
admin. The [README](https://github.com/morningstar-sudo/broom#readme) is the reference: every option, the security
model and how to build from source.

**Contents:** [1. Plan](#plan) · [2. Install the server](#install) ·
[3. Network boot](#network-boot) · [4. Server disks](#server-disks) · [5. Make a golden](#golden) ·
[6. Upload and publish](#publish) · [7. Add the PCs](#pcs) ·
[8. Change a golden: versions and rollback](#versions) ·
[9. Games and apps on iSCSI disks](#games) · [10. Drivers](#drivers) ·
[11. Windows settings](#windows) · [12. Watch and control the room](#monitor) ·
[13. Accounts](#accounts) · [14. Backup, restore, upgrade](#backup) ·
[15. Troubleshooting](#troubleshooting)

## 1. Plan {#plan}

- **Server:** an x86_64 Linux machine (Debian/Ubuntu or Rocky Linux 10 are tested), run as root. Its network link and
  disks decide how many PCs it serves: up to 30 PCs on 1 GbE with an SSD, 30–60 on 10 GbE with NVMe, 60–100 on 2×
  10 GbE bonded or 25 GbE with PCIe 4 NVMe and 64 GB of RAM ([Sizing](https://github.com/morningstar-sudo/broom#sizing)).
- **Disk for the images:** a golden is a raw disk, usually 20–50 GB of data. Keep room for the goldens, their versions
  and one extra golden while a rollback rebuilds it. **Give the images a disk formatted by Broom** (XFS): there,
  versions share the data they have in common and each one costs only what changed. On ext4 every version takes its
  full size (see [Server disks](#server-disks)).
- **Clients:** x86_64 UEFI PCs with network boot (PXE) first in the boot order. Windows images need a local SSD in each
  PC; Linux images also run without one (writes in RAM).
- **Network:** 1 GbE to the PCs, PortFast / edge port on the PC switch ports. Keep the boot LAN for the room only: the
  goldens and iSCSI are readable by any host on it.

## 2. Install the server {#install}

Use a **fixed directory**: the binary keeps its database and images next to itself.

```bash
sudo mkdir -p /opt/bootrom && cd /opt/bootrom
sudo curl -fLO https://github.com/morningstar-sudo/broom/releases/latest/download/bootrom-mgmt
sudo curl -fLO https://github.com/morningstar-sudo/broom/releases/latest/download/SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS   # must say: bootrom-mgmt: OK
sudo chmod +x bootrom-mgmt
sudo ./bootrom-mgmt install-service
journalctl -u bootrom-mgmt -f      # shows the one-time setup token
```

Open `http://<server-ip>/`, enter the setup token and set the **owner** password. The first start downloads the web
UI, iPXE and scripts (`broom-assets.zip`) of its own release; an offline server needs that file, and
`broom-stage.tar.gz` for Windows, copied next to the binary.

## 3. Network boot {#network-boot}

**Network** page. Pick one:

- **Broom's DHCP server:** switch the router's DHCP off first, then turn on Broom's: interface, address range,
  gateway, DNS. Two DHCP servers on one LAN break it, so Broom's is off until you turn it on.
- **Keep the router's DHCP:** set its PXE options: next-server / option 66 = the Broom server's IP, boot file /
  option 67 = `snponly.efi` (Secure Boot clients: `sb/snponly-shim.efi`).

**Secure Boot:** tick **Secure Boot clients** and enable the "Microsoft 3rd-party UEFI CA" in the PCs' firmware. Every
step of the boot is then signed; no MOK to enroll. Needed for anti-cheats such as
[FACEIT](faceit-diskless.md).

Other options on the same page, all optional: **Strict reset** (Windows starts only through PXE, so a PC whose network
boot fails can't run without the reset) and **DNS — only for healthy machines** (see
[Watch and control the room](#monitor)).

## 4. Server disks {#server-disks}

**Server → Disks** lists the server's disks with their health. An image or iSCSI disk can live on any of them.

- **Format** a free disk (type its name to confirm): it is erased, formatted **XFS with reflink** (ext4 when the
  server has no `mkfs.xfs`: install `xfsprogs`) and mounted by Broom at every start.
- **Add a folder** you mounted yourself (LVM, RAID…): Broom uses it as one of its disks.

On XFS with reflink or btrfs, image versions share the data they have in common (Broom finds out by trying, not by
the filesystem's name). On ext4, including a server whose own disk is ext4, everything works the same, only every
version takes its full size: keep the images on a disk formatted here.

Pick the disk when creating an image or iSCSI disk, or move one later with its **Server disk** column. A move copies
the files; the versions then share their data again by themselves.

## 5. Make a golden {#golden}

The golden is the disk every PC boots. Make it in a VM (VMware or any hypervisor that writes `.vmdk` / raw disks) or
on a real PC you then capture.

### Windows 11 Pro

1. **Windows → Guest user:** set the account and password the customers use.
2. Install Windows 11 Pro in a VM. At the first setup screen (OOBE) press **Ctrl+Shift+F3** to enter **Audit Mode**.
3. Install the software and game launchers the whole room should have, and set it up. Keep big games out of it:
   they go on [iSCSI disks](#games). Drivers that need Secure Boot / HVCI must be
   HVCI-compatible (VMware: use the `e1000e` NIC, not `e1000`).
4. PowerShell as Administrator:

   ```powershell
   irm http://<server-ip>/broom-prep-win | iex
   ```

   It prepares the image, syspreps and powers off by itself. To read the script first: `irm
   http://<server-ip>/broom-prep-win -OutFile prep.ps1`, read it, then `.\prep.ps1`.

### Linux

In the VM: `curl -fsSL http://<server-ip>/broom-prep | sudo bash`, then power it off. The script installs what the
iSCSI root and the SSD overlay need.

### Capture a real PC

Prepare the PC the same way (it powers off after the prep). Then network-boot it and pick **[C] Capture / deploy →
Capture** at the bottom of the boot menu. Sign in with an owner account, pick the disk and type the image name. Broom
only reads the disk and sends what NTFS uses (40 GB of Windows on a 1 TB disk sends about 40 GB), then publishes it.
Turn BitLocker off first (`manage-bde -off C:`), and keep C: small: every client gets a C: that size.

## 6. Upload and publish {#publish}

**Images** page:

1. Type a **name**, pick the **OS** (Linux / Windows (Pro)), the **cache** (disk, or **RAM (compressed)**) and the
   **server disk**.
2. Choose the powered-off VM folder (only `.vmx` / `.vmdk` are sent) or a single `.vmdk` / `.img` / `.zip`.
3. **Create + Upload + Publish.** The upload goes in parallel chunks and retries by itself. Then the server converts
   the disk, builds the boot files and serves the image. The first golden of an image is saved as version **v1**.

The image list then shows, per image:

| Column / button | What it does |
|---|---|
| **Groups** | the machine groups that see it in their boot menu (none = every machine) |
| **Cache** | where the golden is served from: the disk, or a compressed copy in RAM (a room fetching a new golden at once never hits the disk) |
| **Default** | the image a PC boots when it has no own choice |
| **SSD** (Linux) | cache the golden and keep the session's writes on the PC's SSD, or leave the SSD untouched (writes in RAM) |
| **Preload** (Windows) | PCs fetch it onto their SSD ahead of use, while they boot another image |
| **Size** | data on the server's disk / size of the virtual disk, and the active version |
| **Server disk** | which server disk it is on; pick another to move it |
| **Republish** | publish again (after a server upgrade or a settings change) |
| **Versions** | the version list (see [section 8](#versions)) |
| **Boot** | the iPXE boot snippet (Linux) |
| **Delete** | the image, its versions and its boot files |

A job runs per image (convert, publish, rollback…): its steps and timings show next to it. One job at a time per image.
A Linux image cached on disk can't be republished while PCs are booted from it: shut them down or publish off-hours.

## 7. Add the PCs {#pcs}

1. **Devices → Add machine** (or import a list): MAC address, name, group. Register a PC **before its first Windows
   boot**: on a registered PC with one internal disk the stage lays out the SSD by itself. An unknown PC asks on its
   screen which disk to wipe (Enter = touch nothing).
2. PCs with several disks: **Devices → the PC → Disks → Use as Broom disk** on the one that should hold Windows, and
   hide the others from the guest (per PC, or per group on the **Groups** page).
3. **Groups** page: a group decides its boot menu, the images kept on its PCs' SSDs, its iSCSI disks, its drivers and
   which disks are hidden. Things set for no group go to everyone. Groups organize; they are not access control.
4. Power the PCs on. A Windows PC downloads the golden to its SSD (once per new golden, checked with sha256 while it
   downloads), builds its **base** (machine name, license) and restarts once by itself. From then on every boot is
   clean: `base ← drivers ← fresh child`.

**Settings → Countdown boot menu** sets how long the boot menu waits before booting the PC's image.

## 8. Change a golden: versions and rollback {#versions}

A **version** is a saved state of an image's golden. Versions are whole disks: any of them can be served again,
exported or turned into a new image, and deleting one never damages another.

**Update an image**

- **In a VM:** start the golden's VM (or **Export** one, below), make the change, run the prep command again, upload
  it to the same image. Or
- **On a real PC:** boot menu **[C] Capture / deploy → Deploy** writes the image onto a PC's disk (erased) so it boots
  that Windows natively. Change it, run the prep again, **Capture** it back under the same name.

Each upload replaces the golden being served; PCs get it at their next boot.

**Save a version: Versions → + Snapshot now** (with an optional label). Instant: a version is the golden file itself,
kept as a hard link. Save one before every change you may want to undo.

**How much space versions take.** A new golden is a new file. On XFS with reflink or btrfs, right after the publish
the version before it gives back every block it has in common with the new golden: a version then costs only what
changed. The golden the PCs load is never touched by this; only the stored version is. On ext4 each version keeps its
full size.

**Roll back: Versions → Rollback** on a version. The image serves that version again and is published; PCs get it at
their next boot (a Windows PC downloads it like any new golden).

- Usually instant. A version that shares its data with a newer golden is scattered over the disk, and PCs read the
  golden from start to end: Broom first **rebuilds it in one piece** (a copy of the data, a few minutes for 20–50 GB,
  shown as a step of the job), then publishes it. Whatever Broom serves is always in one piece; the time is spent in
  the job, never on the PCs.
- The rebuild needs free space about the size of the golden plus 1 GB; after the publish the other versions share
  their data with the rebuilt one again.
- A Linux image cached on disk can't be rolled back while PCs are booted from it.

**Other version actions**

- **→ New image:** a version becomes a new image on the list (same OS and cache), published at once.
- **Export:** a VMware VM (`.vmx` + `.vmdk`) of the image or a version, to open and change it again.
- **Delete:** the version goes; what no other version uses is freed.
- **Dedup:** versions share the data they have in common. A new upload does it by itself; press it for versions
  saved before an upgrade to this Broom version, after moving an image to another disk, or after restoring a backup.

## 9. Games and apps on iSCSI disks {#games}

Big, often-updated software (game libraries) stays out of the golden, on shared **iSCSI disks** each PC mounts under a
drive letter.

1. **iSCSI Disks → Add / change a disk:** name, drive letter, size in GB, groups (empty = all machines), server disk.
2. Pick the disk's **update machine** in its row. Boot that PC, install or update the games on that drive, shut it down properly, then press **Save**:
   a new version of the disk. Each PC picks it up at its next boot; PCs playing keep the old one until then.
   **Discard** throws the update away.
3. **Cache** serves a disk from the server's RAM (busy rooms).

Only the update machine can write to the disk. Let only it download game updates: a launcher updating on a customer
PC fills its SSD and is lost at the restart anyway.

## 10. Drivers {#drivers}

**Drivers → Upload a driver package (.zip)**, then choose who gets it: hardware IDs, groups or all machines, and
**Save**. A changed set is installed once into a driver layer over each PC's base (one extra restart); later boots
start with it. Two packages for the same device: Windows picks the best match, then the newest `DriverVer`.

## 11. Windows settings {#windows}

**Windows** page:

- **Guest user:** the account and password the customers use (written into the prep script).
- **SSD room:** the stage frees *Keep free* GB on each boot; the guest is warned below *Warn* and the PC restarts
  below *Restart* (a full SSD would blue-screen Windows). *Pagefile*: N GB on the SSD, new every boot.
- **Golden downloads at once:** how many PCs download a new golden at the same time, each at full speed. Auto = the
  server's link speed / 1 Gbit/s.

**Base mode** (Machines page, per PC + image): the PC keeps everything over as many restarts as it needs (Windows
Update, an anti-cheat's first run). Turned off, its next boot saves that as its base. The Machines page marks it
**BASE**; don't leave it on for customers.

## 12. Watch and control the room {#monitor}

- **Machines:** each PC's state, image, health (CPU, RAM, temperatures, SMART / bad sectors, uptime, traffic), license,
  and **Wake**, **Manage**, **Remote**, base mode. A PC that booted Windows without the reset is flagged "not reset".
- **Remote:** a live wall of every Windows PC's screen. Click one to take it over: mouse, keyboard, **Ctrl+Alt+Del**,
  **Windows** key, **Alt+Tab** and **Paste** from the toolbar. Chrome / Edge play it smoothly (H.264); lower the
  quality on a busy network. The PC shows no notice while watched: tell your customers if your rules require it.
- **Server:** load now and the last 10 minutes, disks and their health, the **Services** table (DHCP, TFTP, iSCSI),
  iSCSI sessions, the RAM kept for the server when caching in RAM, and a **resource alert**.
- **Logs** (Tools): the server's log, filtered by level, searched by PC name, followed live while a PC boots.
- **Benchmark** (Tools): the server disk's speed, each PC's boot time and read speed, and their history.
- **DNS — only for healthy machines** (Network page, optional): the server is the room's DNS and answers only PCs whose
  Remote agent runs, PCs that just booted and exempt groups / IPs (cashier, staff). Block outside DNS (ports 53, 853)
  on the router.

## 13. Accounts {#accounts}

**Settings → Users:** **owner** accounts can do everything; **staff** can look, use Remote and Wake, and save an iSCSI
disk update, but not delete anything or change settings. **Your password** changes your own. Passwords never cross the
network: signing in is a challenge–response handshake.

## 14. Backup, restore, upgrade {#backup}

**Back up** while no job runs:

- `bootrom.db`: `sqlite3 bootrom.db ".backup /backup/bootrom.db"`. Keep it private: it holds license keys and the
  guest password.
- `images/` and `storage/` together (`rsync -aSH`), `games/`, and `disks/<id>/` (or follow the symlinks with
  `rsync -L`). The `*.bhash` files are only a cache and can be left out.

A copy does not keep the data versions share: restored, each version takes its full size until you press **Dedup**
on the image's versions.

**Restore:** stop the service, put the files back, start it.

**Upgrade:** stop the service, replace the binary (check it with `SHA256SUMS` as when installing), start it. The
Windows scripts come from the server, so every PC gets the new ones at its next boot without a republish.

**Forgot the owner password:** stop the service, run `sqlite3 bootrom.db "DELETE FROM config WHERE key='users'"`
(this removes every account), start it and use the new setup token from the log.

## 15. Troubleshooting {#troubleshooting}

| Symptom | What to check |
|---|---|
| PC doesn't reach the boot menu | PXE first in the boot order; PortFast on the switch port; only one DHCP server answers; Server → Services shows DHCP / TFTP up |
| Secure Boot PC stops at the start | **Secure Boot clients** ticked on the Network page; "Microsoft 3rd-party UEFI CA" enabled in the firmware |
| Upload refused: "not shut down cleanly" | boot the VM, run the prep again and let it power off by itself (no Fast Startup, no forced power-off) |
| Publish / rollback refused: "clients are connected" | a Linux image cached on disk: shut its PCs down, or set its cache to RAM |
| Job says "needs ~N GB free" | free space on that server disk, or move the image to a bigger one |
| Rollback takes minutes | the version is being rebuilt in one piece before it is served (section 8); the job shows the step |
| Versions take full size | the images are on ext4 (section 4), or the data was copied: press **Dedup** |
| PC flagged "not reset" | it booted Windows from its SSD without PXE; turn on **Strict reset** |

**Tools → Logs** with *Warn+* or the PC's name shows what went wrong. Report bugs with the release shown at the top
left of the web page: [github.com/morningstar-sudo/broom/issues](https://github.com/morningstar-sudo/broom/issues).
