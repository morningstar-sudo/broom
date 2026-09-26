# Progress — Diskless Bootrom

Source of truth: `plan.md`. Rules: `rule.md`.
Update the matching section after each phase (code review + test steps + actual results).

| Phase | Status | Date | Notes |
|---|---|---|---|
| Deliverable files setup | **done** | 2026-08-21 | plan.md / progress.md / rule.md created |
| Phase 1 — Server base + boot network | **done** | 2026-08-22 | UEFI VM boots → dynamic iPXE menu from the mgmt app. Full DHCP (the lab has no real DHCP). |
| Phase 2 — Diskless Linux | **done** | 2026-08-22 | Diskless desktop + autologin + network + SSD /games. Reset test: RAM root gone, SSD /games kept. ✅ |
| Phase 2b — Local guest user, independent of the server | **done** | 2026-08-23 | POST_INIT decodes a base64 script → `sh` (loop DELETES ALL image users uid≥1000, creates a fresh guest + password + skel home). pamltsp cut out (PAM common-auth/session commented) → login without SSHFS to the server. Autologin via local `/etc/gdm3/custom.conf` AutomaticLogin (not through pamltsp). Test: any user name works, login/autologin OK, zero server dependency (NFS root only). ✅ |
| ~~Phase 3 — Windows~~ | **removed** | 2026-08-25 | Removed from scope + codebase (publish_windows, iSCSI targetcli, BIOS/undionly PXE, phase3-windows.md, scripts/windows-*.sh). Optimize Linux first. Details below. |
| Phase 4 — Mgmt app (Rust/axum) | **doing** | 2026-08-22 | Linux binary built + API smoke test PASS; preflight/deploy not yet tested on a real server |
| Tooling (2026-08-22) | done | | ltsp-script + zip bundle upload on the web + auto unzip/publish + **ltsp.conf generated inside the binary (guest user/password/home/SSD, base64 no-newline)** + guest-user API/web + delete + web polish |
| Phase 5 — Operations & hardening | todo | | backup, SPOF, docs |
| Phase W — Windows native VHDX boot (SSD) | **doing** | 2026-09-24 | Design B, Win Pro, golden = sysprepped VM + .vmdk upload, the server handles the rest. Client VM: stage → golden.vhdx → automatic specialize/OOBE → desktop ✅. Reset ✅ when PXE is first in BootOrder (disk first → stage skipped; the stage deletes `\EFI\Boot` + keeps "Broom Windows" after PXE). Open: 2 machines/models, games + anti-cheat. Details below. |
| Phase T — Fewer external tools (libguestfs, ZFS gone) | **doing** | 2026-09-25 | T1 linuxfs.rs (ext4 + LVM linear) replaces libguestfs; T2 versions.rs (chunk + manifest snapshot/rollback + UI) replaces ZFS. Unit + live LVM + end-to-end publish/snapshot/rollback PASS in WSL; real hardware pending. T3 (Windows golden read-only) later. |
| Phase S — Built-in DHCP/TFTP/iSCSI, db driver, latest toolchain | **doing** | 2026-09-24 | dnsmasq + targetcli replaced by code in the binary; SQL behind the `Db` trait; Rust stable + edition 2024 + latest crates. Unit + live + netns DHCP/TFTP tests PASS in WSL; real hardware pending. Details below. |
| Phase 6 — Golden vmdk→raw + iSCSI + SSD overlay | **doing** | 2026-08-25 | Mechanism changed: LTSP RAM overlay dropped → golden raw (from vmdk) shared RO over iSCSI + overlayroot writeback on the **SSD** (reset every boot) + per-image zram cache. B1–B4 coded. B5: client boots Ubuntu over iSCSI OK. **B6 done (2026-09-24): LTSP removed** (ltsp.rs, upload-ltsp, ltsp-script, scripts/, docs/phase2-linux.md). Details below. |

---

## Phase 1 — Server base + boot network
**Status:** doing (artifacts written, NOT yet tested for real — no Linux server in the dev env)

### Artifacts written (infra/)
- `dnsmasq/pxe.conf` — TFTP + default proxyDHCP + full DHCP block (commented).
- `dnsmasq/bindings.conf.example` — MAC→IP+hostname binding.
- `tftp/boot.ipxe` — static iPXE script, Linux/Windows menu + countdown.
- `setup-server.sh`, `preflight.sh`, `README.md`.

### Code review (self-review)
- pxe.conf: uses tags `efi-x64` + `!ipxe` to load snponly.efi, tag `ipxe` (opt 175)
  chainloads HTTP → avoids a boot loop. proxyDHCP has `port=0` + `dhcp-range=...,proxy`. OK.
- UEFI only (per plan). Full DHCP in a separate block, 2-DHCP warning. OK.
- ⚠ The iPXE file name in the Debian package (`snponly.efi` vs `ipxe.efi`) is not verified yet —
  setup-server.sh has a fallback, but it must be confirmed on a real server.

### Detailed test steps — RUN ON THE SERVER (not run yet)
```bash
sudo bash infra/setup-server.sh eth0 192.168.1.2 192.168.1.0
sudo bash infra/preflight.sh                 # expected: PREFLIGHT PASS
( cd /srv/http && sudo python3 -m http.server 80 )
```
- [ ] preflight → `PREFLIGHT PASS`
- [ ] UEFI client (Secure Boot off) PXE → downloads snponly.efi over TFTP → runs iPXE
- [ ] iPXE chainloads `http://SERVER/boot.ipxe` → shows the **Linux/Windows menu**, countdown runs
- [ ] `journalctl -u dnsmasq` / `tcpdump -i eth0 port 67 or 69` shows correct DHCP+TFTP
- [ ] Full DHCP mode: the client gets the right static IP + hostname (binding), `PC01` resolves
- [ ] (later, needs a golden) NFS export of the golden read-only to many clients (LTSP)

**Actual result (2026-08-22, server ccvi-5963 + VMware UEFI client VM):**
- preflight PASS, web admin `/` OK, dnsmasq active listening on :67/:69/:4011.
- proxyDHCP can NOT boot: the lab network has no real DHCP handing the client an IP →
  UEFI doesn't move on to 4011/TFTP (tcpdump only shows the proxy reply, no IP offer).
- **Switched to FULL DHCP** (dnsmasq gives IP+bootfile in one offer) → **the client VM boots into the dynamic
  iPXE menu** (old title "Bootrom Tiem Net") with an Ubuntu item + countdown. ✅ Phase 1 DONE.
- Lesson: proxyDHCP needs a real DHCP alongside; a lab/site that runs its own network → full DHCP.
- The DHCP mode is now managed by the mgmt app (DB config → dnsmasq.rs generates pxe.conf), changed via
  the web `/` DHCP section or `setup --mode full`. No more hand-edited files.
- Cosmetic: the iPXE menu uses ASCII (no accented letters/em-dash, to avoid font garbage).
- **Verified again (self-managed binary):** `sudo ./bootrom-mgmt setup --mode full` generates
  pxe.conf + restarts → serves → VM boots the iPXE menu OK; changing the DHCP mode on the web regenerates the config OK.
  All hand configs dropped. `infra/` deleted (replaced by the binary). 33/33 integration tests PASS (WSL).

---

## Phase 2 — Diskless Linux
**Status:** todo

### Code review
_(fill in later)_

### App seam boot_script (DONE + tested 5/5, 2026-08-22)
- Images have a `boot_script` column; `/boot.ipxe` emits the real kernel/initrd/nfsroot instead of `# TODO`.
- API: `POST /api/images/boot-script {id,boot_script}`; the web has a Boot script button.
- Old DB migration: `ALTER TABLE images ADD COLUMN boot_script` (skipped if present).
- Not set → boot.ipxe says the image has no boot_script + returns to the menu (no hang).

### Detailed test steps (real golden — run on the server, see docs/phase2-linux.md)
- [x] Golden = **a separate Ubuntu Desktop VM** (the headless server image has no desktop/user).
      `ltsp image /` on the desktop VM → scp x86_64.img (2.4G) to the server.
- [x] `ltsp kernel <image>` + `ltsp initrd` + `ltsp nfs`; boot_script points to /tftp/ltsp/x86_64/.
- [x] Kernel/initrd served over **HTTP** (mgmt app route /tftp, much faster than TFTP).
- [x] Client VM boots → **diskless GNOME desktop** + network OK (ping server + LAN).
- [x] Guest user autologin: create `khach` on the server + `ltsp.conf` AUTOLOGIN + PASSWORDS_x86_64 → the client logs into the khach desktop by itself.
- [x] **Local SSD /games**: SSD-full-overlay approach dropped (LTSP doesn't support it natively). Decided
      option B — RAM root overlay + POST_INIT sets up `/dev/sdb`→`/games` (formatted on first boot, persistent).
      Heavy games/data on the SSD, OS in RAM. `/dev/sdb1` ext4 mounted on /games OK.
- [x] Reset test: `/test-reset` (RAM root overlay) GONE after reboot; `/games/no-reset`
      (local SSD) KEPT. Clean session every boot + persistent data. ✅ Phase 2 DONE (2026-08-22).

### LTSP lessons (important)
- `ltsp image` **drops users uid≥1000** from the image; `ltsp initrd` **injects the SERVER's users** into the client
  → the client logs in with a server user (ccvi), not an image user. Thin-client model.
- A site wanting its own user → `ltsp.conf` `AUTOLOGIN=khach` + `PASSWORDS_x86_64="khach/<base64>"`,
  create the khach user on the server, `ltsp initrd`. Fresh profile every boot (stateless, suits a public site).
- The NIC showing "Wired Unmanaged" is normal (NFS-root NIC, not managed by NM) — the network still works.
- Internet needs gateway+DNS set in DHCP (mgmt `/api/dhcp`).

---

## Phase 3 — Windows — REMOVED FROM SCOPE (2026-08-25)
**Status:** removed. Focus on optimizing Linux first.

### Removed from the codebase
- Code: `publish_windows` + `targetcli_script` (publish.rs), os validation down to `linux` only
  (images.rs), BIOS/`undionly.kpxe` PXE (dnsmasq.rs + setup.rs), `targetcli`/`qemu-img`
  out of preflight, the Windows option on the web (index.html).
- Files: `docs/phase3-windows.md`, `scripts/windows-install-target.sh`, `scripts/windows-publish.sh`.

### Old notes (if it's ever redone)
- Approach tried: golden over iSCSI **shared RO** + **UWF** (needs Win Edu/Enterprise/LTSC) overlay on the
  local SSD → runtime writes local, reset at boot. Or differencing VHDX + Native VHD Boot.
- **UEFI iSCSI FAILS on VMware Workstation** (iPXE sanboot → "unexpected exception") →
  moved to BIOS/MBR at one point. **DON'T use CCBoot** (no API, can't be automated).
- To revive: needs a commercial writeback driver/UWF; PoC on one machine before a wide rollout.

---

## Phase 4 — Mgmt app (Rust/axum)
**Status:** doing (build + local smoke test done; deploy/preflight/zfs need a real server)

### Build
- Toolchain: WSL Ubuntu-24.04, cargo 1.75. `CARGO_TARGET_DIR=$HOME/broom-target cargo build --release`.
- Result: binary `mgmt/dist/bootrom-mgmt` — ELF x86-64 Linux, **3.3M**, stripped, **0 warnings**.
- Deps: axum 0.7, tokio, rusqlite 0.31 (bundled → no system libsqlite needed), serde.
- **Web admin embedded in the binary** (`include_str!`) → deploy a single file, tower-http dropped.
- **Preflight groups missing packages into one command** `sudo apt install -y ...` (deduplicated),
  file/service/root errors reported separately. Fail path tested in WSL: exit=1 + prints the right command.

### Code review (self-review)
- State = one global `Mutex<Connection>` (ponytail, enough for admin load). Tight lock scope,
  released before shelling out (ping/zfs) → no deadlock.
- WOL sends the magic packet over std UDP, no etherwake dependency for the app.
- boot.rs sanitizes iPXE labels ([A-Za-z0-9_]). Each image's boot body = boot_script
  (LTSP kernel/initrd/nfsroot) — menu/countdown/default are real.

### Detailed test steps
**Local smoke test (WSL, --skip-preflight, port 8899) — RUN, PASS:**
```
POST /api/images {Win11/windows}          → {"id":1,"ok":true}
POST /api/images {Ubuntu/linux}           → {"id":2,"ok":true}
POST /api/images/default {id:2}           → {"ok":true}
POST /api/config/timeout {seconds:15}     → {"ok":true}
GET  /boot.ipxe?mac=...  → #!ipxe menu with exactly 2 items, --default Ubuntu --timeout 15000
GET  /api/images         → Ubuntu is_default=true
POST /api/wake {mac}      → {"ok":true} (magic packet sent)
```
- [x] iPXE menu renders the right images + default + countdown
- [x] Set default / set timeout reflected in /boot.ipxe
- [x] WOL sends the magic packet OK
- [x] **Real server (ccvi-5963):** preflight PASS after installing packages + snponly.efi; web admin `/` up OK
- [ ] **On a real server:** preflight FAILS correctly when packages are missing (exit ≠ 0)
- [ ] **Real server:** create a version → rollback (`zfs rollback`) to the right copy (needs ZFS)
- [ ] **Real server:** apply_dhcp generates bindings.conf + reloads dnsmasq
- [ ] **Real server:** /api/status ping reports on/off correctly (ping is not logged)

**Actual result:** local smoke PASS (2026-08-22). Real server: preflight PASS, web admin OK.

### `setup` subcommand (DONE 2026-08-22)
- `bootrom-mgmt setup [--iface X --ip Y --subnet Z]` (`src/setup.rs`): detects
  IFACE/SERVER_IP/SUBNET (`ip route`), installs missing packages (`apt`), copies snponly.efi from
  `/usr/lib/ipxe`, generates `/etc/dnsmasq.d/pxe.conf` (proxyDHCP), enables+restarts dnsmasq,
  runs preflight. Replaces `infra/setup-server.sh`.
- WSL test: detection correct (eth0/172.17.2.183/172.17.0.0), flag override OK, root gate OK.
- [ ] Full test on a real server (needs root + apt).

### Changes 2026-09-24 (built + unit tests PASS; NOT yet tested on real hardware)
- **Own iPXE build**: upstream iPXE source vendored in `mgmt/ipxe/ipxe-src/`, edited in place — menu
  layout (title + hint + countdown, rules, `[1] NAME` list, footer `Host|IP|MAC`), one-line banner,
  no autoexec.ipxe lookup, no 2 s Ctrl-B wait. `snponly.efi` embedded in the binary.
- The DHCP boot URL carries `?mac=${net0/mac}` → the menu shows the machine name from the Machines table.
- **Windows computer name from the server**: name → iPXE `broom-host` → stage `broom.host=` →
  `broom\host.txt` → broom-done `Rename-Computer` when base is created; renaming = base rebuilt once.
  Hostname validated on web + API (1–15 chars, letters/digits/'-', NetBIOS rules).
- All code comments, logs and UI text translated to English; generic naming ("Diskless System").
- iSCSI IQN base is random per server (`iqn_base` config, generated on first DB open); the old fixed
  `iqn.2026-08.net.tiem:<name>` target is removed on the next publish.
- [ ] Deploy + screenshot of the new menu and boot banner on a real client.
- [ ] Windows: machine registered with a name → after one re-specialize, Windows shows that name.

---

## Phase T — Fewer external tools (selective)
**Status:** doing — T1 + T2 built and tested in WSL (below); real client/server test pending. T3 later.
Plan + reasoning: `plan.md` → "Phase T".

### Test steps + actual results (WSL, 2026-09-25)
- `./build.sh` → 36 unit tests PASS: `linuxfs` (LVM metadata parse + linear mapping; real ext4 in a GPT
  partition made with `mkfs.ext4 -d` → newest kernel 6.8.0-45 chosen over 5.15.0-91, initrd, UUID),
  `versions` (snapshot/dedup/zero chunks/rollback only rewrites differing chunks/holes kept/GC), DB `active_version`.
- Live as root: `lvm_live` — Ubuntu Server layout (p1 `/boot` ext4, p2 PV → VG `brtest` → LV `root` ext4)
  → kernel + initrd + root UUID read correctly. PASS.
- End to end as root (`scratchpad/e2e_versions.sh`): fake Linux golden 200 MB → publish (linuxfs + initrd
  hook + LIO target) ✓ → snapshot v1 (4 chunks) ✓ → 4 MB changed → snapshot v2 (+2 chunks) ✓ → rollback v1
  (target removed, 2 chunks rewritten, publish again) ✓ → sha256 identical to the original ✓ → delete v2
  (2 chunks freed) ✓; image stays sparse (3.1 MB used of 200 MB).
- **Upload chunked (2026-09-25):** web picks the VM folder (no zip; only .vmx/.vmdk/.img/.raw sent), 8 MB
  chunks × 4 parallel, retry 5× with backoff; `upload-start` / `upload-chunk` (written at offset) / `upload-done`
  → convert straight from `images/<name>/upload/` (no unzip copy; a .zip still works). E2E as root
  (`scratchpad/e2e_upload.sh`): split vmdk + .vmx, chunks shuffled + 1 sent twice → image.img sha256 = qemu-img
  convert ✓, `../x` + past-end chunk → 400 ✓, upload folder removed ✓.
- **TRIM on reset (2026-09-26):** loop device test (discards → holes in the backing file): writeback with
  200 MB of old data → `mkfs -E nodiscard` (old) leaves 201 MB, `mkfs` default (new) 1 MB ✓. ntfs3 discard not
  testable in WSL (kernel has no ntfs3) → real client: `broom\stage.log` says `BROOMWIN: discard mount`.
- **Windows license per machine (2026-09-26):** 41 unit tests PASS (key format, IP → machine lookup incl.
  expired lease / unregistered, DB armed→sent once→re-arm→remove, key absent from JSON, iPXE `set broom-lic`,
  stage rebuilds base on a new generation). E2E (`scratchpad/e2e_license.sh`): bad key 400 ✓, key stored
  upper-case ✓, never in /api/machines or /api/status ✓, 1st `GET /api/license` → key, 2nd → 403 ✓, result
  stored ✓, re-arm → `broom-lic 2` + key once more ✓, removed → 403 ✓.
- [ ] Real client: set a retail key → Publish (golden + stage rebuilt) → base rebuilt → `slmgr /dli` activated,
  web shows `sent …XXXXX ✓`.
- **Custom Windows drivers (2026-09-26):** 44 unit tests PASS (.inf IDs from UTF-8 / UTF-16 LE+BE, suffixes
  dropped; pick by hardware / group (case-insensitive) / all; DB drivers + groups + machine_hw; stage driver sync
  with mocked wget + real tar.gz: no packages keeps an old base, new package → downloaded + base rebuilt,
  unchanged → nothing, server down → keep, removed → rebuilt, failed download → not counted + retried).
  E2E (`scratchpad/e2e_drivers.sh`): 3 MB zip uploaded in shuffled chunks → 1 .inf (UTF-16) → 2 IDs ✓, zip with
  only an .exe → rejected ✓, served tar.gz = stored sha256 ✓, PC01 with GPU 10DE:2504 → nvidia ✓, PC02 other
  hardware → none, then group VIP → nvidia ✓, "machines" column [PC01, PC02] ✓, bad group → 400 ✓, delete
  removes the tar.gz ✓, no leftovers in work/ ✓.
- [ ] Real client: upload an extracted VGA driver → boot → stage.log `driver X: downloading` → base rebuilt →
  Device Manager shows the vendor driver.
- **Boot order independent of the PXE entry's name (2026-09-26):** the stage takes the PXE entry from
  `BootCurrent` (the entry that booted it) and saves the order to `broom\bootorder.txt`; the Windows task
  BroomBootOrder restores that list by number through the UEFI `BootOrder` variable (was: bcdedit descriptions
  matched against Network/PXE — boards naming it "IBA GE Slot…"/"Realtek PXE…" lost PXE-first). Tests: stage
  with a real-board-like entry list → `0003,0007,0000,0004,0005,0001` (PXE "IBA GE Slot 0100" first, IPv6/other
  network after Windows) ✓, no/odd BootCurrent → old matching ✓; PowerShell parsed + C# compiled on Windows ✓;
  ordering logic on a fake NVRAM: Windows-first fixed ✓, already right → no write ✓, missing entry dropped + new
  entry kept ✓, garbage file → no write ✓.
- [ ] Real board: power on → Windows → after 5 min `bcdedit /enum firmware` still lists the PXE entry first →
  next power-on goes through PXE (stage.log reset).
- [ ] Real server: upload a real VM folder (Windows + Linux), compare time vs the old zip upload.
- [ ] Real server: publish a real Ubuntu golden (LVM) → client boots over iSCSI.
- [ ] Real server: snapshot / rollback from the web Versions panel; Windows image rollback → clients re-download.

---

## Phase S — Built-in services + storage driver + latest toolchain
**Status:** doing — built, unit + live tests PASS in WSL (below); real client/server test pending.
Plan: `plan.md` → "Phase S".

### Done
- `dhcp.rs` (full + proxy + PXE boot server 4011, leases in DB), `tftp.rs` (RRQ, blksize/tsize/timeout,
  snponly.efi from the binary), `iscsi.rs` (LIO via configfs), `dnsmasq.rs` deleted.
- `db/` driver: `Db` trait + `db/sqlite.rs`; no SQL outside it (`rusqlite` only used there).
- sha2 / zip / zram sysfs / ICMP ping / punch-hole / set_modified / set_permissions / osrelease / geteuid
  instead of sha256sum / unzip / zramctl+dd / ping / fallocate / touch / chmod / uname / id.
- Preflight no longer needs dnsmasq, targetcli-fb, iputils-ping, unzip; takeover of old services at start.
- Fixes found on the way: zram re-publish reset a device still held by the iSCSI backstore (target now
  dropped first); an mgmt restart re-created live targets (now skipped if present) and leaked the zram copy.
- Rust 1.98.1 stable (rustup) + edition 2024 + latest stable crates (axum 0.8.9, tokio 1.53.1,
  tower-http 0.7.1, rusqlite 0.40.2, socket2 0.6.5, sha2 0.11.0, zip 8.6.0, …; matchit stays 0.8.4,
  pinned by axum). `build.sh` sources `~/.cargo/env`.

### Test steps + actual results (WSL Ubuntu 24.04, kernel 6.6 WSL2, 2026-09-24)
- `./build.sh` → 33 unit tests PASS, 0 warnings (DHCP packet/allocation/proxy, TFTP over real sockets
  incl. resend + `..` refusal, SQLite driver, LIO tree on a temp dir, sha256 vector, unzip, checksum).
- Live, as root (`cargo test -- --ignored live`): LIO export → `tpgt_1/enable=1`, portal listening on
  :3260, remove leaves nothing; zram hot_add/write/read/hot_remove; ICMP ping 127.0.0.1 true / 192.0.2.1 false. PASS.
- DHCP/TFTP end to end (netns `bcli` + veth, python PXE client, `scratchpad/dhcp_e2e.sh`):
  full → DISCOVER/OFFER .100, REQUEST/ACK lease 43200 s, TFTP snponly.efi 314368 bytes = embedded ✓;
  proxy → OFFER without IP + opt 60/43, 4011 ACK, TFTP ✓. PASS.
- Web on axum 0.8: `/`, `/machines`, `/images`, `/network`, `/system` 200, unknown 404, SSE ping ✓.
- Logging (`tracing`, events not requests; on/off ping kept but silent): netns run (`scratchpad/log_demo.sh`) shows
  `client PC01 (UEFI PXE) DHCP offer/got IP 10.99.0.50`, `client 10.99.0.50 loaded snponly.efi over TFTP`,
  `client PC01 boot menu …`, `client PC01 started - mac … - ip … - hostname PC01 - image win11 (windows)` on
  stdout; unpublished image choice → WARN on stderr. 32 unit tests PASS.
- [ ] Real server: deploy → `ss -ulpn` shows bootrom-mgmt on :67/:69 (+:4011 proxy); dnsmasq/target disabled.
- [ ] Real UEFI client (VMware VM, full DHCP): PXE → iPXE menu with machine name; Windows image boots + resets.
- [ ] Linux image: client boots over iSCSI; reboot the server → target restored, client boots again.
- [ ] proxyDHCP next to a real router DHCP.

---

## Phase 6 — Golden vmdk→raw + RO block iSCSI + SSD overlay (replaces LTSP)
**Status:** doing — B1–B4 coded (cargo check PASS), B5 server PoC + B6 LTSP removal pending.

### Why the change (decided by the user 2026-08-25)
- Pain: LTSP writeback = **RAM overlay** → capacity limit under heavy writes. Want the **overlay
  on the SSD, stable**. Autologin/users baked straight into the img (LTSP injection dropped). Block level
  (iSCSI) so **Windows is easier later** (same transport).
- New model: golden = raw disk (converted from vmdk) → shared RO iSCSI → client mounts root RO
  (lower) + **overlayroot** writeback on the **local SSD** (reset every boot) → the OS writes to the SSD.

### Done (B1–B4, code)
- **db.rs**: `cache_mode` column (disk|zram) + migration.
- **preflight.rs**: +qemu-img, +targetcli, +iscsistart (open-iscsi); nfs/zfs → optional.
- **images.rs** `upload`: accepts `?src=raw|vmdk|zip`; create accepts `cache_mode`; API
  `/api/images/cache-mode` (change + republish); `/broom-prep` serves the prep script; the list includes cache_mode.
- **publish.rs**: `prepare_golden` (vmdk/zip → qemu-img convert raw); `publish_iscsi` (targetcli
  shared RO fileio/block + boot_script sanhook+kernel+initrd+root=UUID); `ensure_zram`/`repopulate_zram`
  (load the img into /dev/zramN, zstd compressed, rebuilt at start). Old publish_windows/publish_linux: LTSP
  `#[allow(dead_code)]` kept until B6.
- **overlay.rs (new)**: `build_boot` (losetup golden, copy vmlinuz+initrd + read the root UUID);
  `PREP_SCRIPT` (runs in the golden VM: installs open-iscsi + overlayroot + a reset hook that mkfs's the SSD every
  boot + update-initramfs). **Change vs the original plan**: the hook is baked into the golden instead of patching the initrd on the server.
- **main.rs**: mod overlay; repopulate_zram at start (background).
- **index.html**: "Golden (.vmdk/.img/.zip)" card + cache select + per-image cache toggle + broom-prep hint.

### Detailed test steps (B5 — RUN ON THE SERVER, not run yet)
- [ ] Golden VM: `curl .../broom-prep | sudo bash` → power off the VM → take the .vmdk.
- [ ] Web upload .vmdk → server `qemu-img convert` raw + `targetcli` RO iSCSI + copy kernel/initrd.
      Check `targetcli ls`, `qemu-img info`, `/srv/tftp/broom/<name>/`.
- [ ] UEFI client PXE → iPXE menu → sanhook iSCSI → boot → desktop comes up (overlayroot).
- [ ] Write an OS file (outside /games) → lands in the SSD writeback → reboot → gone (reset). Isolate 2 machines.
- [ ] Pull the SSD → RAM fallback, the machine still boots.
- [ ] Toggle cache=zram → `zramctl` shows the device, golden read from RAM (iostat ~0), restart repopulates OK.
- [ ] Measure boot time / server load vs the LTSP RAM overlay.

### Risks (tune during the PoC)
- The rebuild/overlayroot flow inside the golden = the most fragile part; PREP_SCRIPT is a DRAFT.
- `/boot` separate from root → `root=UUID` picks the wrong partition (most desktop VMs have /boot inside root, OK).
- iSCSI attach in the initrd on VMware (Windows once failed UEFI **sanboot**; this is sanhook+kernel/initrd, a different path).
- Fallback transport: AoE/NBD if the iSCSI initrd misbehaves (overlay/SSD unchanged).

## Phase W — Windows native VHDX boot from the client SSD
**Status:** doing. Guide + checklist: `docs/phase-w-windows.md`.

### Actual results (2026-09-24, server ccvi-5963 + VM `broom_client` VMware UEFI, NVMe 100GB, 8GB RAM, e1000)
- [x] Publish golden `11` (Win11 Pro, sysprepped VM): job ✓, 20 disk drivers set to boot-start
      (storahci, stornvme, LSI_SAS, pvscsi…); the stage initrd has sfdisk/mkfs.fat/mkntfs/ntfsfix/efibootmgr.
- [x] Stage: partitions the empty SSD → downloads golden.vhdx 12.3G + templates + efi over HTTP → checks sha256.
- [x] BootNext → "Broom Windows" (vmware.log `About to do EFI boot: Broom Windows`); the Windows kernel
      reads the child → golden chain (the child VHDX generated by `vhdx.rs` works on real Windows).
- [x] First boot: Getting ready → OOBE passes by itself (unattend) → guest user autologon → writes base.ok → reboots by itself.
- [ ] ~~Power-cycled without OOBE = base committed~~ **WRONG**: the broom folder had base.ok + first.pending, NO
      base.vhdx → the stage did not run; the firmware booted the SSD directly (fallback loader `\EFI\Boot` copied by bcdboot),
      child.vhdx kept being used without reset (1.9GB and growing). Consequence: a newly published golden never reached the machine.
      Fix: the stage deletes `\EFI\Boot` after extracting EFI (only BootNext remains). Awaiting retest.
- [x] BROOMWIN no longer has a drive letter in Windows (GPT bit 63) — `DriveLetter = []` ✅.
- [x] Automatic first boot → base committed (no manual steps) ✅: broom\ has base.vhdx + child-local.vhdx,
      base.ok/first.pending gone; stage.log `base done -> reset mode`.
- [x] BroomBootOrder task runs (LastTaskResult 0); BootOrder: EFI Network → Broom Windows → others ✅.
- [x] Reset with an automatically built golden: create `C:\Users\Public\broom-reset-test.txt` → restart →
      `Test-Path` = False ✅ (2026-09-24).
- [x] Reset: a file created in a session is GONE after reboot — when **PXE is first in BootOrder** (2026-09-24, user test
      VM). Disk before PXE → the firmware boots Windows directly → no reset (a hard requirement).
- [ ] 2 machines of different models; games + anti-cheat; BootOrder not pushed ahead of PXE by Windows.

### Errors hit during testing + fixed
| Symptom | Cause | Fix |
|---|---|---|
| `golden has no /boot/vmlinuz-*` (Linux) | prep commented out `/boot` in fstab, Ubuntu Server LVM has a separate /boot | look for the kernel on every filesystem; prep uses `nofail` |
| BSOD `INACCESSIBLE_BOOT_DEVICE` | client disk controller ≠ golden VM's, driver not boot-start | publish edits the registry offline (hivexregedit) Start=0 for inbox disk drivers |
| stage stuck on `ntfs3: volume is dirty` | hard power-off while Windows ran | `ntfsfix -d` before mounting (NOT mount with force: the old journal gets replayed → corrupt volume) |
| `wget: unrecognized option` | the initrd uses busybox wget | only `-c -O` |
| `arithmetic syntax error` → `No root device` | `$((...))` on busybox `ls` output; an ash arithmetic error exits the script | removed; guard enough bytes before computing |
| `416 Range Not Satisfiable` | `-c` on an already complete file | mark each file `.ok`; `-c` fails → download again |
| Windows pulls "Windows Boot Manager" to the top of BootOrder | Windows behavior on every boot | force the order PXE → Broom Windows → WBM in the stage + the BroomBootOrder task (SYSTEM) |
| OOBE loop on every boot | the new stage hid BROOMWIN's drive letter, the golden kept the old `broom-done` (publish only compared mtimes) → base.ok not written | golden_fresh also compares `golden.key` (version of the embedded parts) → logic changes rebuild the golden by themselves |
| "Hi." (profile creation) loops every boot even with a new golden | `broom-done` at first logon: `mountvol D:` + writing `D:\broom\base.ok` did NOT produce the file (the task could still be created → admin rights present; exact cause unknown). Writing by hand via `$v.Path` (`\\?\Volume{..}\`) + `shutdown /r` → stage `base done` ✅ (base commit works) | write base.ok via the volume path, reboot only when `Exists` = True. ✅ Automatic test 2026-09-24: stage.log `17:13 first boot` → `17:17 base done -> reset mode`, no manual steps |
| black screen + cursor (4GB RAM) | suspected: no pagefile + 4GB during specialize/OOBE | retested with 8GB → passes. **Not conclusive** |

### Still open
- 4GB machines: consider having prep set a small fixed pagefile instead of disabling it entirely.
- The e1000 NIC (VMware) has no Windows 11 driver → no network inside Windows; real machines use a NIC with a driver.
- The `SkipMachineOOBE` patch (publish inserts it into Panther\unattend.xml) is coded, not deployed — the current unattend already passes by itself.

## Phase 5 — Operations & hardening
**Status:** todo

### Code review
_(fill in later)_

### Detailed test steps
- [ ] Update the golden via a maintenance boot → publish a new version
- [ ] Simulate a broken golden → roll back → the whole site boots normally again
- [ ] Back up golden + config externally, try a rebuild
