# Progress — Bootrom Diskless Tiệm Net

Nguồn sự thật: `plan.md`. Quy tắc: `rule.md`.
Cập nhật mục tương ứng sau mỗi phase (review code + bước test + kết quả thực).

| Phase | Trạng thái | Ngày | Ghi chú |
|---|---|---|---|
| Setup file bàn giao | **done** | 2026-08-21 | plan.md / progress.md / rule.md tạo xong |
| Phase 1 — Server base + mạng boot | **done** | 2026-08-22 | VM UEFI boot → iPXE menu động từ mgmt app. Full DHCP (lab không có DHCP thật). |
| Phase 2 — Diskless Linux | **done** | 2026-08-22 | Desktop diskless + autologin + net + SSD /games. Test reset: root RAM mất, /games SSD còn. ✅ |
| Phase 2b — User café local, sạch server | **done** | 2026-08-23 | POST_INIT decode script base64 → `sh` (loop XOÁ HẾT user image uid≥1000, tạo café fresh + pass + home skel). Cắt pamltsp (comment PAM common-auth/session) → login không SSHFS server. Autologin bằng `/etc/gdm3/custom.conf` AutomaticLogin local (không qua pamltsp). Test: đổi tên user tuỳ ý, login/autologin OK, 0 phụ thuộc server (chỉ NFS root). ✅ |
| ~~Phase 3 — Windows~~ | **gỡ** | 2026-08-25 | Đã gỡ khỏi scope + codebase (publish_windows, iSCSI targetcli, BIOS/undionly PXE, phase3-windows.md, scripts/windows-*.sh). Tối ưu Linux trước. Chi tiết: mục dưới. |
| Phase 4 — Mgmt app (Rust/axum) | **doing** | 2026-08-22 | Build xong binary Linux + smoke test API PASS; chưa test preflight/deploy trên server thật |
| Tooling (2026-08-22) | done | | ltsp-script + zip bundle upload web + auto giải nén/publish + **ltsp.conf tự sinh trong binary (café user/pass/home/SSD, base64 no-newline)** + café-user API/web + delete + web polish |
| Phase 5 — Vận hành & hardening | todo | | backup, SPOF, tài liệu |
| Phase 6 — Golden vmdk→raw + iSCSI + overlay SSD | **doing** | 2026-08-25 | Đổi cơ chế: bỏ LTSP RAM overlay → golden raw (từ vmdk) iSCSI RO shared + overlayroot writeback **SSD** (reset mỗi boot) + zram cache per-image. B1–B4 code xong (cargo check PASS). B5 PoC server chưa chạy; B6 gỡ LTSP sau PoC. Chi tiết mục dưới. |

---

## Phase 1 — Server base + mạng boot
**Trạng thái:** doing (soạn xong artifact, CHƯA test thật — không có server Linux ở dev env)

### Artifact đã soạn (infra/)
- `dnsmasq/pxe.conf` — TFTP + proxyDHCP mặc định + khối full DHCP (comment).
- `dnsmasq/bindings.conf.example` — binding MAC→IP+hostname.
- `tftp/boot.ipxe` — iPXE script tĩnh, menu Linux/Windows + countdown.
- `setup-server.sh`, `preflight.sh`, `README.md`.

### Review code (tự review)
- pxe.conf: dùng tag `efi-x64` + `!ipxe` để nạp snponly.efi, tag `ipxe` (opt 175)
  chainload HTTP → tránh boot-loop. proxyDHCP có `port=0` + `dhcp-range=...,proxy`. OK.
- Chỉ UEFI (đúng plan). Full DHCP tách khối, cảnh báo 2-DHCP. OK.
- ⚠ Chưa verify tên file iPXE trên gói Debian (`snponly.efi` vs `ipxe.efi`) —
  setup-server.sh có fallback, nhưng phải xác nhận trên server thật.

### Bước test chi tiết — CHẠY TRÊN SERVER (chưa chạy)
```bash
sudo bash infra/setup-server.sh eth0 192.168.1.2 192.168.1.0
sudo bash infra/preflight.sh                 # kỳ vọng: PREFLIGHT PASS
( cd /srv/http && sudo python3 -m http.server 80 )
```
- [ ] preflight → `PREFLIGHT PASS`
- [ ] Client UEFI (Secure Boot off) PXE → tải snponly.efi qua TFTP → chạy iPXE
- [ ] iPXE chainload `http://SERVER/boot.ipxe` → hiện **menu Linux/Windows**, countdown chạy
- [ ] `journalctl -u dnsmasq` / `tcpdump -i eth0 port 67 or 69` thấy DHCP+TFTP đúng
- [ ] Full DHCP mode: client nhận đúng IP tĩnh + hostname (binding), resolve `PC01`
- [ ] (sau, cần golden) NFS export golden read-only cho nhiều client (LTSP)

**Kết quả thực (2026-08-22, server ccvi-5963 + VM client VMware UEFI):**
- preflight PASS, web admin `/` OK, dnsmasq active nghe :67/:69/:4011.
- proxyDHCP KHÔNG boot được: mạng lab không có DHCP thật cấp IP cho client →
  UEFI không sang 4011/TFTP (tcpdump chỉ thấy proxy reply, không có IP offer).
- **Chuyển FULL DHCP** (dnsmasq cấp IP+bootfile 1 offer) → **VM client boot lên iPXE
  menu động** "Bootrom Tiem Net" item Ubuntu + countdown. ✅ Phase 1 DONE.
- Bài học: proxyDHCP cần 1 DHCP thật song song; lab/tiệm tự quản mạng → full DHCP.
- DHCP mode giờ do mgmt app quản (config DB → dnsmasq.rs sinh pxe.conf), đổi qua
  web `/` mục DHCP hoặc `setup --mode full`. Bỏ chỉnh file tay.
- Cosmetic: menu iPXE dùng ASCII (bỏ dấu tiếng Việt/em-dash cho khỏi rác font).
- **Verified lần 2 (binary tự quản):** `sudo ./bootrom-mgmt setup --mode full` sinh
  pxe.conf + restart → serve → VM boot iPXE menu OK; web đổi DHCP mode sinh lại config OK.
  Bỏ hết config tay. `infra/` đã xóa (binary thay). 33/33 test tích hợp PASS (WSL).

---

## Phase 2 — Diskless Linux
**Trạng thái:** todo

### Review code
_(điền sau)_

### App seam boot_script (ĐÃ LÀM + test 5/5, 2026-08-22)
- Image có cột `boot_script`; `/boot.ipxe` nhả kernel/initrd/nfsroot thật thay `# TODO`.
- API: `POST /api/images/boot-script {id,boot_script}`; web có nút Boot script.
- Migration DB cũ: `ALTER TABLE images ADD COLUMN boot_script` (bỏ qua nếu có).
- Chưa set → boot.ipxe báo "chua co boot_script" + về menu (không treo).

### Bước test chi tiết (golden thật — chạy trên server, xem docs/phase2-linux.md)
- [x] Golden = **VM Ubuntu Desktop riêng** (image server headless không có desktop/user).
      `ltsp image /` trên VM desktop → scp x86_64.img (2.4G) sang server.
- [x] `ltsp kernel <image>` + `ltsp initrd` + `ltsp nfs`; boot_script trỏ /tftp/ltsp/x86_64/.
- [x] Kernel/initrd serve qua **HTTP** (mgmt app route /tftp, nhanh hơn TFTP nhiều).
- [x] VM client boot → **GNOME desktop diskless** + mạng OK (ping server + LAN).
- [x] Autologin user café: tạo `khach` trên server + `ltsp.conf` AUTOLOGIN + PASSWORDS_x86_64 → client tự vào desktop khach.
- [x] **SSD local /games**: bỏ hướng SSD-full-overlay (LTSP không hỗ trợ native). Chốt
      option B — root overlay RAM + POST_INIT dựng `/dev/sdb`→`/games` (format lần đầu, persist).
      Game/data nặng xuống SSD, OS ở RAM. `/dev/sdb1` ext4 mount /games OK.
- [x] Test reset: `/test-reset` (root RAM overlay) MẤT sau reboot; `/games/no-reset`
      (SSD local) CÒN. Phiên sạch mỗi boot + data persist. ✅ Phase 2 DONE (2026-08-22).

### Bài học LTSP (quan trọng)
- `ltsp image` **loại user uid≥1000** của image; `ltsp initrd` **tiêm user SERVER** vào client
  → client login bằng user server (ccvi), không phải user image. Mô hình thin-client.
- Café muốn user riêng → dùng `ltsp.conf` `AUTOLOGIN=khach` + `PASSWORDS_x86_64="khach/<base64>"`,
  tạo user khach trên server, `ltsp initrd`. Fresh profile mỗi boot (stateless, hợp quán net).
- NIC hiện "Wired Unmanaged" = bình thường (NFS-root NIC, NM không quản) — mạng vẫn chạy.
- Internet cần set gateway+DNS trong DHCP (mgmt `/api/dhcp`).

---

## Phase 3 — Windows — ĐÃ GỠ KHỎI SCOPE (2026-08-25)
**Trạng thái:** gỡ. Tập trung tối ưu Linux trước.

### Đã xoá khỏi codebase
- Code: `publish_windows` + `targetcli_script` (publish.rs), validate os chỉ còn `linux`
  (images.rs), BIOS/`undionly.kpxe` PXE (dnsmasq.rs + setup.rs), `targetcli`/`qemu-img`
  khỏi preflight, option Windows trong web (index.html).
- File: `docs/phase3-windows.md`, `scripts/windows-install-target.sh`, `scripts/windows-publish.sh`.

### Ghi chú cũ (nếu sau này làm lại)
- Hướng đã thử: golden iSCSI **RO shared** + **UWF** (cần Win Edu/Enterprise/LTSC) overlay
  SSD local → ghi runtime local, reset boot. Hoặc differencing VHDX + Native VHD Boot.
- **UEFI iSCSI FAIL trên VMware Workstation** (iPXE sanboot → "unexpected exception") →
  từng chuyển BIOS/MBR. **KHÔNG dùng CCBoot** (không API, không tự động hoá được).
- Khôi phục: cần driver writeback thương mại/UWF; PoC 1 máy trước khi triển khai đại trà.

---

## Phase 4 — Mgmt app (Rust/axum)
**Trạng thái:** doing (build + smoke test local xong; deploy/preflight/zfs cần server thật)

### Build
- Toolchain: WSL Ubuntu-24.04, cargo 1.75. `CARGO_TARGET_DIR=$HOME/broom-target cargo build --release`.
- Kết quả: binary `mgmt/dist/bootrom-mgmt` — ELF x86-64 Linux, **3.3M**, stripped, **0 warning**.
- Deps: axum 0.7, tokio, rusqlite 0.31 (bundled → không cần libsqlite hệ thống), serde.
- **Web admin nhúng vào binary** (`include_str!`) → deploy chỉ 1 file, bỏ tower-http.
- **Preflight gom gói thiếu thành 1 lệnh** `sudo apt install -y ...` (khử trùng lặp),
  lỗi file/service/root báo riêng. Đã test fail-path trong WSL: exit=1 + in đúng lệnh.

### Review code (tự review)
- State = 1 `Mutex<Connection>` global (ponytail, đủ cho tải admin). Lock scope gọn,
  thả trước khi shell-out (ping/zfs) → không deadlock.
- WOL gửi magic packet bằng std UDP, bỏ phụ thuộc etherwake cho app.
- boot.rs sanitize label iPXE ([A-Za-z0-9_]). Thân boot mỗi image = boot_script
  (kernel/initrd/nfsroot LTSP) — menu/countdown/default đã thật.

### Bước test chi tiết
**Smoke test local (WSL, --skip-preflight, port 8899) — ĐÃ CHẠY, PASS:**
```
POST /api/images {Win11/windows}          → {"id":1,"ok":true}
POST /api/images {Ubuntu/linux}           → {"id":2,"ok":true}
POST /api/images/default {id:2}           → {"ok":true}
POST /api/config/timeout {seconds:15}     → {"ok":true}
GET  /boot.ipxe?mac=...  → #!ipxe menu đúng 2 item, --default Ubuntu --timeout 15000
GET  /api/images         → Ubuntu is_default=true
POST /api/wake {mac}      → {"ok":true} (magic packet gửi)
```
- [x] Menu iPXE render đúng image + default + countdown
- [x] Set default / set timeout phản ánh vào /boot.ipxe
- [x] WOL gửi magic packet OK
- [x] **Server thật (ccvi-5963):** preflight PASS sau khi cài gói + snponly.efi; web admin `/` lên OK
- [ ] **Trên server thật:** preflight FAIL đúng khi thiếu gói (exit ≠ 0)
- [ ] **Server thật:** tạo version → rollback (`zfs rollback`) đúng bản (cần ZFS)
- [ ] **Server thật:** apply_dhcp sinh bindings.conf + reload dnsmasq
- [ ] **Server thật:** /api/status ping báo đúng on/off

**Kết quả thực:** smoke local PASS (2026-08-22). Server thật: preflight PASS, web admin OK.

### Subcommand `setup` (ĐÃ LÀM 2026-08-22)
- `bootrom-mgmt setup [--iface X --ip Y --subnet Z]` (`src/setup.rs`): tự dò
  IFACE/SERVER_IP/SUBNET (`ip route`), cài gói thiếu (`apt`), copy snponly.efi từ
  `/usr/lib/ipxe`, sinh `/etc/dnsmasq.d/pxe.conf` (proxyDHCP), enable+restart dnsmasq,
  chạy preflight. Thay `infra/setup-server.sh`.
- Test WSL: dò đúng (eth0/172.17.2.183/172.17.0.0), override flag OK, root-gate OK.
- [ ] Test full trên server thật (cần root + apt).

---

## Phase 6 — Golden vmdk→raw + iSCSI block RO + overlay SSD (thay LTSP)
**Trạng thái:** doing — B1–B4 code xong (cargo check PASS), B5 PoC server + B6 gỡ LTSP chưa.

### Vì sao đổi (user chốt 2026-08-25)
- Pain: LTSP writeback = **RAM overlay** → giới hạn dung lượng khi ghi nhiều. Muốn **overlay
  xuống SSD, ổn định**. Autologin/user bake thẳng trong img (bỏ inject LTSP). Block-level
  (iSCSI) để **Windows sau dễ** (dùng lại transport).
- Mô hình mới: golden = raw disk (convert từ vmdk) → iSCSI RO shared → client mount root RO
  (lower) + **overlayroot** writeback lên **SSD local** (reset mỗi boot) → OS ghi xuống SSD.

### Đã làm (B1–B4, code)
- **db.rs**: cột `cache_mode` (disk|zram) + migration.
- **preflight.rs**: +qemu-img, +targetcli, +iscsistart (open-iscsi); nfs/zfs → optional.
- **images.rs** `upload`: nhận `?src=raw|vmdk|zip`; create nhận `cache_mode`; API
  `/api/images/cache-mode` (đổi + republish); `/broom-prep` serve prep script; list có cache_mode.
- **publish.rs**: `prepare_golden` (vmdk/zip → qemu-img convert raw); `publish_iscsi` (targetcli
  RO shared fileio/block + boot_script sanhook+kernel+initrd+root=UUID); `ensure_zram`/`repopulate_zram`
  (nạp img vào /dev/zramN nén zstd, dựng lại lúc start). publish_windows/publish_linux cũ: LTSP
  `#[allow(dead_code)]` giữ tới B6.
- **overlay.rs (mới)**: `build_boot` (losetup golden, copy vmlinuz+initrd + đọc UUID root);
  `PREP_SCRIPT` (chạy trong golden VM: cài open-iscsi + overlayroot + reset-hook mkfs SSD mỗi
  boot + update-initramfs). **Đổi vs plan gốc**: bake hook trong golden thay vì mổ initrd server.
- **main.rs**: mod overlay; repopulate_zram lúc start (nền).
- **index.html**: card "Golden (.vmdk/.img/.zip)" + cache select + toggle cache mỗi image + broom-prep hint.

### Bước test chi tiết (B5 — CHẠY TRÊN SERVER, chưa chạy)
- [ ] Golden VM: `curl .../broom-prep | sudo bash` → tắt VM → lấy .vmdk.
- [ ] Web upload .vmdk → server `qemu-img convert` raw + `targetcli` iSCSI RO + copy kernel/initrd.
      Kiểm `targetcli ls`, `qemu-img info`, `/srv/tftp/broom/<name>/`.
- [ ] Client UEFI PXE → iPXE menu → sanhook iSCSI → boot → desktop lên (overlayroot).
- [ ] Ghi file OS (ngoài /games) → nằm SSD writeback → reboot → mất (reset). isolate 2 máy.
- [ ] Rút SSD → fallback RAM, máy vẫn boot.
- [ ] Toggle cache=zram → `zramctl` thấy device, đọc golden từ RAM (iostat ~0), restart repopulate OK.
- [ ] Đo boot time / tải server vs LTSP RAM overlay.

### Rủi ro (tune ở PoC)
- Rebuild/overlayroot flow trong golden = phần fragile nhất; PREP_SCRIPT là DRAFT.
- `/boot` tách rời root → `root=UUID` lấy nhầm partition (đa số desktop VM /boot trong root, OK).
- iSCSI attach initrd trên VMware (Windows từng fail UEFI **sanboot**; đây là sanhook+kernel/initrd, khác đường).
- Fallback transport: AoE/NBD nếu iSCSI initrd trục trặc (overlay/SSD giữ nguyên).

## Phase 5 — Vận hành & hardening
**Trạng thái:** todo

### Review code
_(điền sau)_

### Bước test chi tiết
- [ ] Update golden qua maintenance boot → publish version mới
- [ ] Mô phỏng hỏng golden → rollback → toàn tiệm boot lại OK
- [ ] Backup golden + config ra ngoài, thử dựng lại
