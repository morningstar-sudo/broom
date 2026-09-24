# Plan: Diskless Bootrom System

## Context

Build a **diskless boot (bootrom)** system for a site with 10–30 machines: client PCs boot their OS
over the network from one central server, no hard disk as the OS source. Goals:

- One shared "golden" image for many machines → update in one place, every machine gets it.
- Every user session is clean (reset on power-off).
- Boot **Linux** (iSCSI + SSD writeback/cache) and **Windows** (native VHDX boot from the SSD —
  see section "Phase W").
- The admin picks one image as the system-wide default boot.

**Agreed with the client:**
- Approach: **hybrid** — open-source base + home-made management tool.
- Server OS: **Linux (Debian/Ubuntu)**.
- Client firmware: **UEFI, Secure Boot off**.
- Write cache: **local SSD in each machine** (where runtime changes are written).
- Home-made tool: **image management (version/rollback)** + **machine monitoring/control**.
- OS choice: **an iPXE menu on the machine** lets the user pick an image; **when the countdown
  (fixed timeout) runs out with no choice → boot the default image** set by the admin.
- NOT needed: automatic game sync, billing integration (separate time-billing software is used).

## Scope

Only **Linux diskless (LTSP)**. Linux is standard and low-risk (overlayfs/RAM overlay +
NFS root). Windows diskless was removed from the codebase (hard, needs commercial drivers/UWF)
— finish + optimize Linux first.

## Overall architecture

```
[Site router/DHCP] ---- LAN (2.5/10GbE uplink from server) ---- [Switch] ---- [Client x10-30]
                                                                              UEFI, no SecureBoot
                                                                              + local SSD (/games)
        [SERVER Debian]
        ├── dnsmasq       : proxyDHCP/full DHCP + TFTP + binding/hostname
        ├── iPXE          : snponly.efi (UEFI), chainloads a script over HTTP
        ├── NFS (Linux)   : read-only root for Linux clients (LTSP)
        └── Mgmt app      : Rust/axum + SQLite — web admin + HTTP boot serving
                            (dynamic /boot.ipxe + kernel/initrd/assets via ServeDir)
```

**Write policy (Linux LTSP):**
- LTSP diskless — RO golden squashfs + **RAM overlay** (native LTSP), reset every boot.
  Local-SSD overlay dropped (LTSP doesn't support it natively, initrd hacks are fragile).
- **Heavy games/data: local SSD `/games`** (POST_INIT formats it on first boot, persistent) and/or
  shared from the server over NFS (FSTAB_x). Not stuffed into the RAM overlay.

**RO golden image shared by many machines:** NFS export **read-only** so many clients
read the same image. NEVER open one image for shared writes (instant corruption) — runtime writes
go only to the RAM overlay + local SSD.

**OS choice at boot:** iPXE shows a **menu** listing the Linux images (versions). The user
picks → that image boots. **N-second countdown (configurable)**; timeout with no choice →
boot the **default image** set by the admin. The menu is generated dynamically by the mgmt app (knows which
machine may see which image, and what the default is).

## Module breakdown (each module separate, independently configured/tested)

| Module | Responsibility | Main components |
|---|---|---|
| **M1 net-boot** | DHCP/PXE/iPXE chainload; DHCP binding + hostname | dnsmasq (**proxyDHCP or full DHCP**) + TFTP, snponly.efi, `dhcp-host` binding |
| **M2 image-store** | Store golden images, versions, snapshots | ZFS pool / files on ZFS |
| **M3 linux-diskless** | Boot Linux RO + RAM overlay + SSD `/games` | LTSP, NFS root, RAM overlay |
| **M5 boot-menu** | Generate the dynamic iPXE menu + countdown + default | `ipxe_render` (HTTP endpoint) |
| **M6 mgmt-image** | Image CRUD/version/rollback (web) | Rust/axum + ZFS snapshot |
| **M7 mgmt-monitor** | On/off monitoring + WOL + reboot | Rust/axum + ping/agent + WOL |
| **M8 mgmt-config** | Client machines, image assignment, default, timeout | Rust/axum + SQLite |

M5–M8 share one **Rust (axum)** app, deployed as one binary, but **each module is its own mod/file**
— nothing dumped into one file, each part easy to handle.

## Deliverable files + working rules
Three files at the project root `d:\Windows\Desktop\code\broom\`:
- **`plan.md`**: this plan (architecture, modules, phases, verification). Source of truth.
- **`progress.md`**: progress log. One section per phase: status, date, code review
  result, detailed test steps run + actual results.
- **`rule.md`**: mandatory working rules.

**Rule — after EVERY completed phase:**
1. **Review the code** just written (right module, no bloat, follows the plan).
2. **Write detailed test steps** for the current phase into `progress.md` — exact
   commands, inputs, expected vs actual result. Enough for someone else to rerun.
3. Only mark a phase **done** when the tests really pass (evidence). Fail → record the error clearly, don't move on.
4. Update `plan.md`/`rule.md` if the design changes.

## Linux boot performance (main priority)
- **Page cache / ZFS ARC (RAM cache) on the server**: the golden squashfs + read-heavy NFS root
  stay in RAM → 10–30 machines read almost entirely from RAM, no disk access. The biggest lever.
- **Enough server RAM to keep the golden hot**: LTSP squashfs ~2–4GB → 16–32GB RAM easily keeps
  the whole image + NFS metadata hot.
- **2.5GbE uplink minimum, 10GbE if possible** + **jumbo frames (MTU 9000)** for NFS.
- **RAM overlay (LTSP)**: runtime writes stay in client RAM, no network → less server load.
- **Local SSD `/games`**: heavy data read locally from the SSD, not pulled over the network every session.
- Kernel/initrd served over **HTTP** (mgmt app), faster than TFTP; the image goes over **NFS**, no extra layers.
- **NFS tuning**: `async`, `no_root_squash` for the RO root, large `rsize/wsize` (1MB) + `nconnect` if the kernel supports it.

## Preflight — check packages/services before running
At startup (and in the Phase 1 install script) the mgmt app **checks every dependency itself**; anything
missing is printed clearly with the package + install command, and it **refuses to run** instead of half-failing.
One preflight function: walks a list of `(binary/service, package, purpose)` → checks `which` + `systemctl is-enabled/active`.

| Needed | Package (Debian/Ubuntu) | Used for |
|---|---|---|
| `dnsmasq` | dnsmasq | DHCP (proxy/full) + TFTP + DNS/hostname |
| `exportfs`/nfsd | nfs-kernel-server | NFS root for Linux diskless |
| `zfs`/`zpool` | zfsutils-linux (+zfs-dkms on Debian) | image store + snapshot/rollback |
| `unzip` | unzip | unpack Linux golden bundles (.zip) |
| `etherwake`/`wakeonlan` | etherwake | WOL power-on (M7) |
| `ping` | iputils-ping | on/off monitoring (M7) |
| `snponly.efi` | ipxe / build iPXE | UEFI boot binary |

The app also checks: `/srv/tftp/snponly.efi` exists, dnsmasq is running, the ZFS pool is
mounted, permission to run `zfs` (usually needs root/systemd service). Fail → exit code ≠ 0 + message.

## Phases

### Phase 1 — Server base + boot network
Fresh Debian server. Install & configure:
- **dnsmasq** + TFTP: `/etc/dnsmasq.d/pxe.conf`. **Two selectable modes** (same dnsmasq):
  - **proxyDHCP** (default): the site router keeps handing out IPs, dnsmasq only answers boot info →
    NO conflict with the existing DHCP. Safe when the router can't be controlled.
  - **Full DHCP server**: turn off the router's DHCP, dnsmasq hands out the IP range + gateway + DNS +
    boot. The server has full control. Enables **DHCP Binding**: `dhcp-host=<MAC>,<IP>,
    <hostname>` → static IP + fixed hostname per machine, hostname (option 12) sent to the
    client + registered in the internal DNS (resolve `PC01`...). The basis of machine identity.
    ⚠ Only enable when there is certainly NO other DHCP on the LAN (2 DHCP servers = network chaos).
  The mgmt app (M8) lets the admin pick the mode + parameters; regenerates `pxe.conf` then reloads dnsmasq.
  Split UEFI vs Legacy later if needed; UEFI only for now.
- **iPXE**: use `snponly.efi` (UEFI). dnsmasq points clients to download iPXE over **TFTP**
  (just one ~1MB file at bootstrap — not a bottleneck), then iPXE chainloads a
  dynamic script over HTTP: `http://<server>/boot.ipxe`. Kernel/initrd go over HTTP, root over
  **NFS** afterwards, NOT over TFTP. (Optionally drop TFTP: UEFI HTTP Boot if the firmware supports it well.)
- **HTTP boot serving**: at this scale **no nginx needed**.
  - Phases 1–2: a hand-written **STATIC iPXE script** + a minimal file server
    (`python -m http.server` or an axum stub) to boot/test — the full app doesn't exist yet.
  - Phase 4 onward: the **Rust/axum mgmt app** takes over, serving `/boot.ipxe` (dynamic,
    with menu) + kernel/initrd/assets itself. The app sits on the boot-critical path → runs under **systemd, auto-restart**.
- **NFS export (LTSP)**: Linux golden = a squashfs exported read-only to many clients.
  Store images as files on ZFS (ZFS makes snapshot/rollback cheap → ZFS recommended).
- Milestone test: one UEFI PXE client → downloads iPXE → the iPXE screen talks to the server.

New files: `/etc/dnsmasq.d/pxe.conf`, `/srv/tftp/snponly.efi`, `/srv/http/boot.ipxe` (temporary static one for testing).

### Phase 2 — Diskless Linux (DONE)
- Build a **Linux golden image** (Ubuntu Desktop with the games/apps users need) via LTSP.
- Boot: iPXE → kernel+initrd (HTTP) → root over **read-only NFS** (LTSP).
- **RAM overlay** (native LTSP): RO root + RAM upper, reset every boot → clean session.
- **Local SSD `/games`**: POST_INIT sets up `/dev/sdb`→`/games` (formatted on first boot, persistent)
  for heavy games/data. OS in RAM, data on the SSD.
- Local guest user independent of the server: POST_INIT deletes the image users + creates a fresh guest; autologin
  via local `gdm3` (pamltsp cut out) → zero server dependency besides the NFS root.
- Milestone test (PASS): a file in the RAM root is gone after reboot; a file in `/games` on the SSD stays.

### Phase W — Windows design B: native VHDX boot from the client SSD (decided 2026-09-24)
The old approach (UWF + iSCSI, removed 2026-08-25) is dropped: UWF needs Enterprise/Edu/LTSC, UEFI sanboot iSCSI failed
on VMware. Decided: **Windows Pro**, machines **run Windows only**, no persistent drive for users.
- Golden = **Windows VM + sysprep** (`/broom-prep-win` inside the VM: tweaks + EFI bundle + unattend →
  sysprep) → upload the `.vmdk` → the server by itself: extracts the Windows partition → `golden.vhdx` + `efi.tar.gz` +
  self-generated empty child VHDX (`vhdx.rs`) + builds the stage (`winstage.rs`) + boot_script.
- Client SSD: p1 ESP `BROOMEFI` (bootmgr + BCD `vhd=[locate]\broom\child.vhdx`), p2 NTFS
  `BROOMWIN`: `golden.vhdx` ← `base.vhdx` (specialized once per machine, drivers for its hardware) ←
  `child.vhdx` (reset every boot).
- Every boot: iPXE → **Linux stage** (server kernel) → partition on first boot / download the golden over HTTP on
  hash mismatch / commit base / reset child / extract EFI → `efibootmgr` BootNext → reboot → Windows. +1 reboot.
- Guide + checklist: `docs/phase-w-windows.md`.
- Main risks: the self-generated child VHDX (not yet tested on real Windows), Windows changing BootOrder,
  anti-cheat, publishing a new golden = every machine downloads the full file.

### Phase 4 — Mgmt app (Rust: axum + SQLite) — modules M5–M8
Stack: **axum + tokio**, **SQLite** (sqlx or rusqlite), static web (askama/minijinja
or plain static HTML + JSON API). System commands via `std::process::Command`.
Deploy = **one binary**, no runtime needed. Split by module (one mod/file each):
- **M5 boot-menu** (`boot.rs`): `/boot.ipxe` handler generates the dynamic menu — lists the images
  each machine may use (by MAC), adds `menu --timeout <N>` + the default item. The menu
  is just text → formatted directly, no heavy templates. The link between iPXE ↔ config.
- **M6 mgmt-image** (`images.rs`, `zfs.rs`): CRUD/version/rollback. Version =
  **ZFS snapshot** (calls `zfs snapshot`/`zfs rollback`) — no home-made COW.
- **M7 mgmt-monitor** (`monitor.rs`, `wol.rs`): on/off (ping + optional light agent),
  **WOL** power-on (`etherwake`/magic packet), reboot.
- **M8 mgmt-config** (`machines.rs`, `db.rs`): machine table (MAC/IP/name), assign images to
  machines, set **default image + countdown timeout**; pick **DHCP mode (proxy/full)** +
  parameters (IP range, gateway, DNS) → generate `pxe.conf` + reload dnsmasq.
- **DHCP Binding + hostname**: each machine gets `dhcp-host=<MAC>,<static IP>,<hostname>`
  → fixed IP + **hostname** (option 12) sent to the client + registered in the internal DNS
  (resolve `PC01`, `PC02`... on the LAN). The backbone of machine identity for the whole
  system (monitor/WOL/image assignment all look up MAC↔IP↔hostname). ⚠ Needs **full DHCP mode**.
- **Preflight** (`preflight.rs`): runs at the start of `main()`, checks the dependency table (the
  "Preflight" section above) before binding the port/serving. Missing → clear log + exit ≠ 0.
- **TODO (later) — `setup` subcommand** (`setup.rs`): fold all of Phase 1 into the binary,
  replacing `infra/setup-server.sh`. `bootrom-mgmt setup`:
  - detects IFACE + SERVER_IP + SUBNET (reads `ip`/netlink), overridable by flags.
  - installs missing packages (reuses the preflight list → `apt install -y ...`).
  - copies `/usr/lib/ipxe/snponly.efi` → `/srv/tftp/` (download as fallback if absent).
  - generates `/etc/dnsmasq.d/pxe.conf` (proxyDHCP by default) + `enable --now dnsmasq`.
  - runs preflight at the end; PASS = done. → deploy one file, one setup command.

Layout: `mgmt/` (Cargo) → `src/main.rs`, `src/db.rs`, `src/preflight.rs`, `src/{boot,images,publish,ltsp,zfs,monitor,wol,machines,dnsmasq,setup}.rs`, `static/`.

### Phase 5 — Operations & hardening
- Golden update procedure: snapshot before changing → change it in a "maintenance boot" (RW) →
  test → publish the new version → roll back if broken.
- Backup: the server ZFS pool + config (`/etc/dnsmasq.d`, mgmt DB). Push golden snapshots
  to an external disk/NAS regularly.
- **SPOF**: one dead server = the whole site stops. Acceptable at small scale, but there must be an
  **external golden image + config backup** for a quick rebuild; consider a cold standby server.
- Reset-on-boot (LTSP RAM overlay, built in Phase 2).
- A short operations guide for the site owner.

**Platform note:** ZFS on **Debian** needs `zfs-dkms` (contrib repo); **Ubuntu**
ships ZFS — if ZFS without hassle matters, pick Ubuntu Server LTS as the server OS.

## Verification (end-to-end tests)
- Phase 1: `tcpdump`/dnsmasq log shows a UEFI client downloading iPXE successfully in **both modes**;
  full DHCP: the client gets the right **static IP + hostname** from the binding, `PC01` resolves on the LAN.
- Phase 2: a client boots Linux; a file in the RAM root → gone after reboot; a file in `/games` on the SSD → stays;
  change the golden → reboot → every machine sees the change.
- Phase 4: the client shows the **iPXE menu** with all images; picking by hand → boots the right image;
  **no choice until the countdown ends → auto-boots the default**; the web app creates a version → rollback
  (`zfs rollback`) → the client boots the right copy; changing default/timeout → clients follow;
  WOL powers on a machine that is off; the monitoring table reports on/off correctly.
- Phase 5: simulate a broken golden → roll back to a good version → the whole site boots normally again.

## Points to confirm with the client at deployment
- Do the clients' network cards support **UEFI PXE + WOL** (check one sample machine).
- Server uplink: **2.5GbE minimum**, 10GbE if the budget allows (30 machines reading the image at once).
