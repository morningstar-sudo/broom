# Broom — Bootrom Diskless cho Tiệm Net

Hệ thống boot OS qua mạng cho quán net 10–30 máy: 1 golden image dùng chung, mỗi
phiên sạch (reset khi tắt). **Scope hiện tại: Linux diskless (LTSP)** — Windows đã gỡ
để tập trung tối ưu Linux trước.

## Cấu trúc
```
broom/
├── plan.md          # kiến trúc + phase + verification (nguồn sự thật)
├── progress.md      # nhật ký tiến độ từng phase
├── rule.md          # quy tắc làm việc
├── docs/            # hướng dẫn chi tiết từng phase
│   └── phase2-linux.md
├── scripts/         # script vận hành
│   ├── build-linux-golden.sh      # trên VM desktop: đóng golden Linux + đẩy sang server
│   └── publish-linux-image.sh     # publish Linux tay (mgmt tự publish khi upload web)
└── mgmt/            # mgmt app (Rust/axum, 1 binary)
    ├── src/         # M5 boot, M6 image+publish, M7 monitor, M8 config, dnsmasq, setup, preflight
    ├── static/      # web admin (nhúng vào binary)
    ├── dist/        # binary build sẵn (Linux x86_64)
    └── images/      # (runtime) mỗi image 1 folder: images/<tên>/image.img
```

## Thêm golden Linux

**Cơ chế mới (Phase 6 — iSCSI + overlay SSD, đang PoC):**
1. **Trong VM Ubuntu Desktop** (golden): `curl -fsSL http://<server>/broom-prep | sudo bash`
   → bake open-iscsi + overlayroot + reset-hook (writeback SSD, reset mỗi boot).
2. Tắt VM → lấy file **`.vmdk`** → **web admin** mục **Golden (.vmdk/.img/.zip)** → Upload
   (chọn cache **disk** hoặc **zram**).
3. Server **convert raw + serve iSCSI RO + copy kernel/initrd** → boot được ngay.
   OS ghi runtime xuống **SSD local** (không RAM); mỗi máy isolate, reset mỗi boot.

**Cơ chế cũ (LTSP, còn tới khi PoC mới xong):** `curl .../ltsp-script | sudo bash` trên VM
desktop → `golden.zip` → web mục **Golden Linux (.zip)**.

## Quickstart (server Debian/Ubuntu, chạy root)
⚠ Deploy vào **thư mục cố định** (vd `/opt/bootrom`), KHÔNG chạy trong `/tmp` — image +
`bootrom.db` nằm cạnh binary, `/tmp` bị xoá khi reboot.
```bash
sudo mkdir -p /opt/bootrom && cd /opt/bootrom
sudo cp <path>/bootrom-mgmt .

# Chạy thẳng. Lần đầu preflight FAIL → TỰ setup (dò mạng, cài gói, snponly.efi, dnsmasq)
# rồi serve. proxyDHCP mặc định; muốn full DHCP: thêm --mode full.
sudo ./bootrom-mgmt --mode full
```
(Không còn subcommand `setup` riêng — gộp vào preflight: pass thì serve luôn, fail thì auto-fix.)
(images + DB lưu tại `/opt/bootrom/images` + `/opt/bootrom/bootrom.db`; hoặc đặt
`BOOTROM_IMAGES_DIR=/srv/bootrom/images` để tách chỗ.)
Web admin: `http://<server-ip>/` — quản image, DHCP, máy, giám sát.

## Trạng thái
- **Phase 1** (mạng boot): done — iPXE menu động, UEFI PXE.
- **Phase 2** (Linux diskless): done — LTSP desktop, autologin, overlay RAM + SSD `/games`.
- **Phase 4** (mgmt app): doing — binary Rust build + smoke test PASS.
- **Phase 6** (golden vmdk→iSCSI + overlay SSD): doing — code B1–B4 xong, PoC server chưa chạy.
- **Windows diskless**: đã gỡ khỏi scope — tối ưu Linux trước (block-level để sau dễ mở lại).

Xem `plan.md` / `progress.md` để chi tiết.
