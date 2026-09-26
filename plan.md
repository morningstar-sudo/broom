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
        [SERVER Debian/Ubuntu] — one binary, no distro services (Phase S):
        └── bootrom-mgmt  : Rust/axum, storage via the db/ driver (SQLite built in)
            ├── HTTP :80        web admin + dynamic /boot.ipxe + kernel/initrd/golden files
            ├── DHCP :67/:4011  full DHCP or proxyDHCP + PXE boot server (dhcp.rs)
            ├── TFTP :69        snponly.efi straight from the binary (tftp.rs)
            ├── iSCSI           shared RO targets, kernel LIO via configfs (iscsi.rs)
            └── iPXE            snponly.efi built from mgmt/ipxe/ipxe-src, embedded
```
(The LTSP/NFS text below is the original Phase 2 design, since replaced — see Phase 6 and Phase W.)

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
| **M1 net-boot** | DHCP/PXE/iPXE chainload; DHCP binding + hostname | built-in DHCP (**proxyDHCP or full DHCP**) + TFTP (`dhcp.rs`, `tftp.rs`), snponly.efi, Machines-table binding |
| **M2 image-store** | Store golden images, versions, snapshots | ZFS pool / files on ZFS |
| **M3 linux-diskless** | Boot Linux RO + RAM overlay + SSD `/games` | LTSP, NFS root, RAM overlay |
| **M5 boot-menu** | Generate the dynamic iPXE menu + countdown + default | `ipxe_render` (HTTP endpoint) |
| **M6 mgmt-image** | Image CRUD/version/rollback (web) | Rust/axum + ZFS snapshot |
| **M7 mgmt-monitor** | Machine list (registered + seen via DHCP) + on/off + WOL | Rust/axum + ICMP ping (not logged) + WOL |
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

Since Phase S the list only holds **tools** the binary still shells out to (the source of truth is
`BINS` in `mgmt/src/preflight.rs`): qemu-utils, libguestfs-tools, open-iscsi, zfsutils-linux
(optional), fdisk, ntfs-3g, dosfstools, efibootmgr, initramfs-tools, wget, libwin-hivex-perl.
Built in, no package: DHCP, TFTP, iSCSI target config, iPXE, sha256, unzip, zram setup, ping, WOL.

The app also checks it runs as root. Fail → exit code ≠ 0 + message.

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

Layout: `mgmt/` (Cargo) → `src/main.rs`, `src/db/{mod,sqlite}.rs`, `src/preflight.rs`,
`src/{boot,images,publish,overlay,winstage,vhdx,dhcp,tftp,iscsi,zfs,monitor,wol,machines,setup}.rs`, `static/`.

### Phase S — Built-in services, storage driver, latest toolchain (2026-09-24)
Goal: the binary does the service work itself instead of relying on distro services.
- **DHCP** (`dhcp.rs`, replaces dnsmasq): full DHCP (binding from the Machines table > previous lease >
  first free IP in range; leases in the DB) or proxyDHCP (answers PXE/iPXE clients only: offer with
  PXEClient + discovery control 8, boot server on UDP 4011). Protocol logic is one pure `handle()` function.
- **TFTP** (`tftp.rs`): read-only, blksize/tsize/timeout options, snponly.efi served from the binary.
- **iSCSI** (`iscsi.rs`, replaces targetcli + target.service): kernel LIO configured through configfs;
  targets re-exported at start (configfs doesn't survive a reboot), live ones left alone.
- **Small tools in Rust**: sha256 (sha2), unzip (zip), zram via sysfs, ICMP ping (raw socket),
  fallocate punch-hole, touch/chmod/uname/geteuid.
- **Storage driver** (`db/`): the `Db` trait + SQLite driver hold every SQL statement; MySQL/Postgres =
  a new driver file + a branch in `db::open()` (`BOOTROM_DB` URL).
- **Toolchain**: Rust stable (rustup, `rust-toolchain.toml`), edition 2024, all crates at the latest stable release.
- **Logging** (`tracing`): events, not requests — `client PC01 started - mac … - ip … - hostname … - image …`,
  DHCP offer/ack/NAK, TFTP loads, boot menu, publish steps + result, config changes, iSCSI/zram, errors.
  INFO/DEBUG → stdout, WARN/ERROR → stderr; `RUST_LOG=debug` adds every external command. A menu choice
  chains `/boot/start?image=&mac=&ip=` so the server knows (and logs) which image each client boots.
- On/off ping (every 15 s from the Machines page) is never logged — only real events are.
- Takeover: dnsmasq / tftpd-hpa / rtslib-fb-targetctl / target are stopped + disabled at start if present.
- Out of scope (kept as distro tools): qemu-img, libguestfs, hivexregedit, mkinitramfs, ntfs-3g, sfdisk,
  losetup/mount, tar, zfs, and the tools copied into the Windows stage initrd.

### Phase T — Fewer external tools, selectively (2026-09-25)
Review: preflight installs packages automatically and offline install is not required → only replace tools
that are heavy/fragile or where replacing lowers risk. Kept on purpose: qemu-img (proven converter; a
bug would corrupt goldens), the shell stage + mkinitramfs + stage tools (works on real hardware; a rewrite
is high risk on the boot path), ntfs-3g, sfdisk, tar, cpio, gzip, ip, modprobe, mount (base system).
- **T1 — libguestfs gone** (`linuxfs.rs`): partitions (sfdisk -J) → ext4, or LVM2 PV → linear LVs → ext4
  (crate `ext4-view`), read straight from image.img (no mount/loop, the server's LVM never sees the VG).
  Newest kernel + initrd + root UUID for `overlay::build_boot`. xfs/btrfs/non-linear LVM → clear error.
- **T2 — ZFS gone, built-in versions** (`versions.rs`): 4 MB chunks, blake3, dedup, zero chunks not stored
  (`storage/chunks/<ab>/<hash>`), manifest per version (`storage/manifests/<name>/vN.json`). Snapshot /
  rollback run as image jobs; rollback removes the iSCSI target first, rewrites only differing chunks
  (holes kept), then publishes again. Delete version + garbage collection; web "Versions" panel.
  DB: `dataset` column dropped, `active_version` added. (ZFS versioning never worked: no dataset was ever set.)
- **T3 — later** (user decision): server stops writing into Windows goldens (edits move into prep; server
  reads NTFS read-only) → drops hivexregedit + the read-write mount.

### Client privacy + Windows licensing (2026-09-26)
- **TRIM on reset:** the last session's writes are TRIMmed when the next boot resets them — Windows stage mounts
  BROOMWIN `ntfs3 -o discard` and deletes the old child.vhdx before writing the fresh one; the Linux hook's
  `mkfs.ext4` discards the whole writeback partition (no more `-E nodiscard`). Covers: data readable after the
  next boot. Does NOT cover: power off + pull the disk before the next boot (needs an ephemeral-key encrypted
  write layer: Linux overlayroot `crypt` possible later; Windows Pro native VHD boot has no BitLocker → UWF
  (Enterprise/Education) or a server-side write layer).
- **Per-machine Windows license (retail key):** key per machine on the Machines page (never shown in full,
  never in any API output). broom-done.ps1 (while base is built) asks `GET /api/license`; the server picks the
  machine ONLY by the TCP peer IP (bound IP, else unexpired lease) and hands the key out ONCE (armed → sent;
  the guest user is an Administrator and could ask any time). Then `slmgr /ipk` + `/ato` + `/cpky` (key not
  left in the registry), output posted back to `/api/license/result`. Setting / re-arming a key bumps
  `license_gen` → iPXE `broom-lic` → stage rebuilds base on the next boot. After one retail activation the
  digital license re-activates later bases by itself.
- **Known gap:** the web admin has no login — anyone on the LAN can open it. Admin auth is a separate task.

### Boot order by number, not by name (2026-09-26)
PXE must stay first or the next power-on skips the reset. Entry names vary per board ("IBA GE Slot 0100",
"Realtek PXE B03", "UEFI: PXE IPv4 …"), so nothing matches names any more: the stage takes the PXE entry from
`BootCurrent` (it booted the stage), orders PXE → Broom Windows → Windows Boot Manager → other network → rest
(other NICs / IPv6 after Windows: server down → no extra PXE timeouts), and writes `broom\bootorder.txt`.
The Windows task BroomBootOrder restores that exact list through the UEFI `BootOrder` variable
(kernel32 firmware-variable API), keeping entries added later at the end.

### Custom Windows drivers (2026-09-26)
Goldens are built in a VM → real clients lack vendor drivers (VGA, LAN, audio…). Each machine has its own
base (specialized once on its hardware) → drivers are installed into **base**, once, kept across resets.
- **Package** = .zip of an extracted driver folder (at least one .inf; vendor .exe installers not supported).
  Web "Drivers" page, chunked upload → server unzips to `<home>/drivers/<name>/`, re-packs
  `<home>/tftp/broom-drivers/<name>.tar.gz` (the stage has tar/gzip, no unzip) + sha256, and reads the
  hardware IDs from every .inf (UTF-8/UTF-16): `PCI\VEN_xxxx&DEV_yyyy`, `USB\VID_xxxx&PID_yyyy`.
- **Who gets it** — a machine gets a package when ANY of: its hardware matches an ID (stage reports its
  PCI/USB IDs), the package is assigned to the machine's group (free-text group on the Machines page), or the
  package is ticked "all machines".
- **Stage, every boot:** network up → `POST /api/drivers/for?mac=` with its IDs → list `name sha256` →
  new/changed packages downloaded + extracted into `BROOMWIN\broom\drivers\<name>\`, unlisted ones removed.
  The set actually present differs from the one base was built with → rebuild base. Server unreachable →
  keep what is there.
- **broom-done.ps1** (base build): copy broom\drivers to `C:\Windows\Temp`, `pnputil /add-driver *.inf
  /subdirs /install` (Windows installs only what matches the real devices), delete the copy.
- Windows only (Linux drivers come with the kernel/golden).

### Phase 5 — Operations & hardening
- Golden update procedure: snapshot before changing → change it in a "maintenance boot" (RW) →
  test → publish the new version → roll back if broken.
- Backup: `images/` + `storage/` (image versions) + the mgmt DB (all config lives there). Push them
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
