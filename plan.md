# Plan: Hệ thống Bootrom Diskless cho Tiệm Net

## Context

Xây hệ thống **diskless boot (bootrom)** cho tiệm net 10–30 máy: client PC boot OS
qua mạng từ 1 server trung tâm, không dùng ổ cứng làm nguồn OS. Mục tiêu:

- 1 image "vàng" dùng chung cho nhiều máy → update 1 chỗ, tất cả máy nhận.
- Mỗi phiên chơi của khách sạch sẽ (reset khi tắt máy).
- Boot **Linux** (LTSP). *(Windows diskless đã gỡ khỏi scope — tối ưu Linux trước;
  xem cuối file "Windows — đã gỡ" nếu sau này cần làm lại.)*
- Admin chọn 1 image làm default boot toàn hệ thống.

**Chốt từ khách:**
- Hướng: **hybrid** — nền open-source + tool quản lý tự viết.
- Server OS: **Linux (Debian/Ubuntu)**.
- Firmware client: **UEFI, tắt Secure Boot**.
- Write-cache: **SSD local mỗi máy** (nơi ghi thay đổi runtime).
- Tool tự viết: **quản lý image (version/rollback)** + **giám sát/điều khiển máy**.
- Chọn OS: **có menu iPXE tại máy** cho khách chọn image; **hết countdown
  (timeout cố định) không chọn → tự boot default image** admin đã set.
- KHÔNG cần: đồng bộ game tự động, tích hợp billing (dùng phần mềm tính giờ riêng).

## Scope

Chỉ **Linux diskless (LTSP)**. Linux chuẩn và ít rủi ro (overlayfs/overlay RAM +
NFS root). Windows diskless đã gỡ khỏi codebase (khó, cần driver thương mại/UWF)
— ưu tiên hoàn thiện + tối ưu Linux trước.

## Kiến trúc tổng thể

```
[Router/DHCP tiệm] ---- LAN (2.5/10GbE uplink từ server) ---- [Switch] ---- [Client x10-30]
                                                                              UEFI, no SecureBoot
                                                                              + SSD local (/games)
        [SERVER Debian]
        ├── dnsmasq       : proxyDHCP/full DHCP + TFTP + binding/hostname
        ├── iPXE          : snponly.efi (UEFI), chainload script từ HTTP
        ├── NFS (Linux)   : root read-only cho client Linux (LTSP)
        └── Mgmt app      : Rust/axum + SQLite — web admin + serve HTTP boot
                            (/boot.ipxe động + kernel/initrd/assets qua ServeDir)
```

**Nguyên tắc ghi (Linux LTSP):**
- LTSP diskless — squashfs golden RO + **overlay RAM** (native LTSP), reset mỗi boot.
  Bỏ SSD-local overlay (LTSP không hỗ trợ native, hack initrd fragile).
- **Game/data nặng: SSD local `/games`** (POST_INIT format lần đầu, persist) và/hoặc
  share từ server qua NFS (FSTAB_x). Không nhồi vào overlay RAM.

**Golden image RO chia sẻ nhiều máy:** NFS export **read-only** cho nhiều client
đọc chung 1 image. Tuyệt đối KHÔNG mở ghi chung 1 image (hỏng ngay) — ghi runtime
chỉ xuống overlay RAM + SSD local.

**Chọn OS lúc boot:** iPXE hiện **menu** liệt kê image Linux (các version). Khách
chọn → boot image đó. **Countdown N giây (cấu hình được)**; hết giờ không chọn →
boot **default image** admin đặt. Menu do mgmt app sinh động (biết máy nào được
phép thấy image nào, default là gì).

## Phân rã module (mỗi module tách riêng, độc lập cấu hình/test)

| Module | Trách nhiệm | Thành phần chính |
|---|---|---|
| **M1 net-boot** | DHCP/PXE/iPXE chainload; DHCP binding + hostname | dnsmasq (**proxyDHCP hoặc full DHCP**) + TFTP, snponly.efi, `dhcp-host` binding |
| **M2 image-store** | Lưu golden image, version, snapshot | ZFS pool / file trên ZFS |
| **M3 linux-diskless** | Boot Linux RO + overlay RAM + SSD `/games` | LTSP, NFS root, overlay RAM |
| **M5 boot-menu** | Sinh iPXE menu động + countdown + default | `ipxe_render` (HTTP endpoint) |
| **M6 mgmt-image** | CRUD/version/rollback image (web) | Rust/axum + ZFS snapshot |
| **M7 mgmt-monitor** | Giám sát on/off + WOL + reboot | Rust/axum + ping/agent + WOL |
| **M8 mgmt-config** | Máy client, gán image, set default, timeout | Rust/axum + SQLite |

M5–M8 chung 1 app **Rust (axum)**, deploy 1 binary, nhưng **tách mỗi module 1 mod/file**
— không dồn 1 file, dễ handle từng phần.

## File bàn giao + Quy tắc làm việc
Xuất 3 file ở gốc project `d:\Windows\Desktop\code\broom\`:
- **`plan.md`**: bản plan này (kiến trúc, module, phase, verification). Nguồn sự thật.
- **`progress.md`**: nhật ký tiến độ. Mỗi phase 1 mục: trạng thái, ngày, kết quả
  review code, các bước test chi tiết đã chạy + kết quả thực tế.
- **`rule.md`**: quy tắc làm việc bắt buộc.

**Quy tắc — sau MỖI khi hoàn thành 1 giai đoạn:**
1. **Review code** phần vừa làm (đúng module, không phình, bám plan).
2. **Ghi các bước test chi tiết** cho giai đoạn hiện tại vào `progress.md` — lệnh cụ
   thể, đầu vào, kết quả mong đợi vs kết quả thực. Đủ để người khác chạy lại.
3. Chỉ đánh dấu phase **done** khi test pass thật (evidence). Fail → ghi rõ lỗi, không qua phase sau.
4. Cập nhật `plan.md`/`rule.md` nếu phát sinh thay đổi thiết kế.

## Tối ưu hiệu năng boot Linux (ưu tiên chính)
- **Page cache / ZFS ARC (RAM cache) trên server**: squashfs golden + NFS root
  đọc-nhiều nằm sẵn RAM → 10–30 máy đọc chung gần như từ RAM, không chạm đĩa. Đòn bẩy lớn nhất.
- **RAM server đủ ôm golden nóng**: squashfs LTSP ~2–4GB → RAM 16–32GB dư sức giữ nóng
  cả image + metadata NFS.
- **Uplink 2.5GbE tối thiểu, 10GbE nếu được** + **jumbo frames (MTU 9000)** cho NFS.
- **Overlay RAM (LTSP)**: ghi runtime nằm RAM client, không đụng network → giảm tải server.
- **SSD local `/games`**: data nặng đọc từ SSD tại chỗ, không kéo qua mạng mỗi phiên.
- Kernel/initrd serve qua **HTTP** (mgmt app) nhanh hơn TFTP; image đi **NFS**, không thêm lớp thừa.
- **NFS tuning**: `async`, `no_root_squash` cho root RO, `rsize/wsize` lớn (1MB) + `nconnect` nếu kernel hỗ trợ.

## Preflight — kiểm tra gói/dịch vụ trước khi chạy
Mgmt app khi khởi động (và script cài Phase 1) **tự kiểm toàn bộ dependency**, thiếu
thì in rõ gói nào + lệnh cài, **refuse chạy** thay vì lỗi nửa vời. 1 hàm preflight:
duyệt list `(binary/service, gói, mục đích)` → check `which` + `systemctl is-enabled/active`.

| Cần | Gói (Debian/Ubuntu) | Dùng cho |
|---|---|---|
| `dnsmasq` | dnsmasq | DHCP (proxy/full) + TFTP + DNS/hostname |
| `exportfs`/nfsd | nfs-kernel-server | NFS root cho Linux diskless |
| `zfs`/`zpool` | zfsutils-linux (+zfs-dkms nếu Debian) | image store + snapshot/rollback |
| `unzip` | unzip | giải nén bundle golden Linux (.zip) |
| `etherwake`/`wakeonlan` | etherwake | WOL bật máy (M7) |
| `ping` | iputils-ping | giám sát on/off (M7) |
| `snponly.efi` | ipxe / build iPXE | boot binary UEFI |

App còn check: file `/srv/tftp/snponly.efi` tồn tại, dnsmasq đang chạy, ZFS pool
mount, quyền chạy `zfs` (thường cần root/systemd service). Fail → exit code ≠ 0 + thông báo.

## Các phase

### Phase 1 — Server base + mạng boot
Server Debian mới. Cài & cấu hình:
- **dnsmasq** + TFTP: `/etc/dnsmasq.d/pxe.conf`. **Hai mode chọn được** (cùng dnsmasq):
  - **proxyDHCP** (mặc định): router tiệm vẫn cấp IP, dnsmasq chỉ trả boot info →
    KHÔNG xung đột DHCP sẵn. An toàn khi không kiểm soát được router.
  - **Full DHCP server**: tắt DHCP router, dnsmasq cấp IP range + gateway + DNS +
    boot. Server nắm toàn quyền. Bật được **DHCP Binding**: `dhcp-host=<MAC>,<IP>,
    <hostname>` → IP tĩnh + hostname cố định theo máy, trả hostname (option 12) cho
    client + đăng ký DNS nội bộ (resolve `PC01`...). Nền định danh máy trạm.
    ⚠ Chỉ bật khi chắc chắn KHÔNG còn DHCP nào khác trên LAN (2 DHCP = loạn mạng).
  Mgmt app (M8) cho admin chọn mode + tham số; sinh lại `pxe.conf` rồi reload dnsmasq.
  Tách UEFI vs Legacy nếu sau này cần; hiện chỉ UEFI.
- **iPXE**: dùng `snponly.efi` (UEFI). dnsmasq trỏ client tải iPXE qua **TFTP**
  (chỉ 1 file ~1MB lúc bootstrap — không phải nút cổ chai), rồi iPXE chainload
  script động từ HTTP: `http://<server>/boot.ipxe`. Kernel/initrd đi HTTP, root đi
  **NFS** sau đó, KHÔNG qua TFTP. (Tùy chọn bỏ TFTP: UEFI HTTP Boot nếu firmware hỗ trợ ổn.)
- **HTTP boot serving**: quy mô này **không cần nginx**.
  - Phase 1–2: dùng **iPXE script TĨNH** viết tay + 1 file server tối giản
    (`python -m http.server` hoặc axum stub) để boot/test — app đầy đủ chưa có.
  - Phase 4 trở đi: **mgmt app Rust/axum** thay thế, tự serve `/boot.ipxe` (động,
    có menu) + kernel/initrd/assets. App nằm trên đường boot-critical → chạy **systemd, auto-restart**.
- **NFS export (LTSP)**: golden Linux = squashfs export read-only cho nhiều client.
  Lưu image dạng file trên ZFS (ZFS cho snapshot/rollback rẻ → khuyến nghị ZFS).
- Test cột mốc: 1 client UEFI PXE → tải iPXE → thấy màn hình iPXE nói chuyện được server.

File tạo mới: `/etc/dnsmasq.d/pxe.conf`, `/srv/tftp/snponly.efi`, `/srv/http/boot.ipxe` (tĩnh tạm để test).

### Phase 2 — Diskless Linux (DONE)
- Tạo **golden image Linux** (Ubuntu Desktop đã cài game/app khách cần) qua LTSP.
- Boot: iPXE → kernel+initrd (HTTP) → root qua **NFS read-only** (LTSP).
- **Overlay RAM** (native LTSP): root RO + upper RAM, reset mỗi boot → phiên sạch.
- **SSD local `/games`**: POST_INIT dựng `/dev/sdb`→`/games` (format lần đầu, persist)
  cho game/data nặng. OS ở RAM, data ở SSD.
- User café local sạch server: POST_INIT xoá user image + tạo café fresh; autologin
  qua `gdm3` local (cắt pamltsp) → 0 phụ thuộc server ngoài NFS root.
- Test cột mốc (PASS): file ở root RAM mất sau reboot; file ở `/games` SSD còn.

### Windows diskless — đã gỡ khỏi scope (2026-08-25)
Hướng cũ (UWF + iSCSI RO shared, hoặc differencing VHDX + Native VHD Boot) đã **gỡ
khỏi codebase** để tập trung tối ưu Linux trước. Toàn bộ code/script/docs Windows
(publish_windows, iSCSI targetcli, BIOS/undionly PXE, phase3-windows.md,
scripts/windows-*.sh) đã xoá. Nếu sau này làm lại: khôi phục từ ghi chú này —
cần driver writeback (UWF cần Win Enterprise/Education/LTSC) hoặc CCBoot; PoC 1 máy trước.

### Phase 4 — Mgmt app (Rust: axum + SQLite) — module M5–M8
Stack: **axum + tokio**, **SQLite** (sqlx hoặc rusqlite), web tĩnh (askama/minijinja
hoặc chỉ HTML tĩnh + JSON API). Shell-out lệnh hệ thống qua `std::process::Command`.
Deploy = **1 binary**, không cần runtime. Tách theo module (mỗi module 1 mod/file):
- **M5 boot-menu** (`boot.rs`): handler `/boot.ipxe` sinh menu động — liệt kê image
  được phép cho từng máy (theo MAC), gắn `menu --timeout <N>` + item default. Menu
  chỉ là text → format thẳng, không cần template nặng. Điểm nối iPXE ↔ config.
- **M6 mgmt-image** (`images.rs`, `zfs.rs`): CRUD/version/rollback. Version =
  **ZFS snapshot** (gọi `zfs snapshot`/`zfs rollback`) — không tự viết COW.
- **M7 mgmt-monitor** (`monitor.rs`, `wol.rs`): on/off (ping + optional agent nhẹ),
  **WOL** bật máy (`etherwake`/gói magic packet), reboot.
- **M8 mgmt-config** (`machines.rs`, `db.rs`): bảng máy (MAC/IP/tên), gán image cho
  máy, set **default image + countdown timeout**; chọn **DHCP mode (proxy/full)** +
  tham số (IP range, gateway, DNS) → sinh `pxe.conf` + reload dnsmasq.
- **DHCP Binding + hostname**: mỗi máy khai `dhcp-host=<MAC>,<IP tĩnh>,<hostname>`
  → IP cố định + trả **hostname** (option 12) cho client + đăng ký DNS nội bộ
  (resolve `PC01`, `PC02`... trong LAN). Là xương sống định danh máy trạm cho toàn
  hệ (monitor/WOL/gán image đều tra theo MAC↔IP↔hostname). ⚠ Cần **full DHCP mode**.
- **Preflight** (`preflight.rs`): chạy đầu `main()`, kiểm bảng dependency (mục
  "Preflight" trên) trước khi bind port/serve. Thiếu → log rõ + exit ≠ 0.
- **TODO (sau) — subcommand `setup`** (`setup.rs`): gom toàn bộ Phase 1 vào binary,
  thay `infra/setup-server.sh`. `bootrom-mgmt setup`:
  - tự dò IFACE + SERVER_IP + SUBNET (đọc `ip`/netlink), cho override bằng flag.
  - cài gói thiếu (dùng luôn danh sách preflight → `apt install -y ...`).
  - copy `/usr/lib/ipxe/snponly.efi` → `/srv/tftp/` (fallback tải nếu không có).
  - sinh `/etc/dnsmasq.d/pxe.conf` (proxyDHCP mặc định) + `enable --now dnsmasq`.
  - chạy preflight cuối, PASS thì xong. → deploy 1 file, 1 lệnh setup.

Cấu trúc: `mgmt/` (Cargo) → `src/main.rs`, `src/db.rs`, `src/preflight.rs`, `src/{boot,images,publish,ltsp,zfs,monitor,wol,machines,dnsmasq,setup}.rs`, `static/`.

### Phase 5 — Vận hành & hardening
- Quy trình update golden: snapshot trước khi sửa → sửa trên "maintenance boot" (RW) →
  test → publish version mới → rollback nếu hỏng.
- Backup: server ZFS pool + config (`/etc/dnsmasq.d`, mgmt DB). Đẩy snapshot golden
  ra ổ ngoài/NAS định kỳ.
- **SPOF**: 1 server chết = cả tiệm dừng. Quy mô nhỏ chấp nhận được, nhưng phải có
  **golden image + config backup ra ngoài** để dựng lại nhanh; cân nhắc 1 server dự phòng nguội.
- Reset-on-boot (overlay RAM LTSP, đã dựng ở Phase 2).
- Tài liệu vận hành ngắn cho chủ tiệm.

**Lưu ý nền tảng:** ZFS trên **Debian** cần `zfs-dkms` (repo contrib); **Ubuntu**
có ZFS sẵn — nếu ưu tiên ZFS đỡ phiền thì chọn Ubuntu Server LTS làm server OS.

## Verification (test end-to-end)
- Phase 1: `tcpdump`/log dnsmasq thấy 1 client UEFI tải iPXE thành công ở **cả 2 mode**;
  full DHCP: client nhận đúng **IP tĩnh + hostname** theo binding, resolve được `PC01` trong LAN.
- Phase 2: 1 client boot Linux; file ở root RAM → reboot mất; file ở `/games` SSD → còn;
  sửa golden → reboot → thấy thay đổi ở mọi máy.
- Phase 4: client boot thấy **menu iPXE** đủ image; chọn tay → boot đúng image;
  **không chọn hết countdown → auto boot default**; web app tạo version → rollback
  (`zfs rollback`) → client boot thấy đúng bản; đổi default/timeout → client theo;
  WOL bật 1 máy đang tắt; bảng giám sát báo đúng on/off.
- Phase 5: mô phỏng hỏng golden → rollback về version tốt → toàn tiệm boot lại bình thường.

## Điểm cần khách xác nhận thêm khi triển khai
- Client có card mạng hỗ trợ **PXE UEFI + WOL** không (kiểm 1 máy mẫu).
- Uplink server: **2.5GbE tối thiểu**, 10GbE nếu ngân sách cho phép (30 máy đọc image đồng thời).
