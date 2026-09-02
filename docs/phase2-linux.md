# Phase 2 — Golden Linux diskless (chạy trên server)

Mục tiêu: 1 client boot Ubuntu diskless, ghi xuống SSD local (overlay), reset mỗi boot,
nhiều máy share 1 golden. Mgmt app đã sẵn seam `boot_script` — chỉ cần điền lệnh boot thật.

## Cách nhanh nhất để CÓ máy Linux boot (LTSP, overlay RAM) — chứng minh chuỗi diskless
LTSP tự lo squashfs golden + initrd overlay + NFS. Overlay mặc định = RAM (stateless).

```bash
sudo apt install -y ltsp ltsp-binaries nfs-kernel-server ipxe-binaries

# 1. Tạo golden: cài Ubuntu vào 1 chroot /srv/golden (hoặc dùng 1 máy mẫu).
#    Cài sẵn game/app khách cần vào đó. (Cách nhanh: debootstrap + chroot cài thêm.)
sudo debootstrap --include=linux-image-generic,ubuntu-desktop-minimal noble /srv/golden \
     http://archive.ubuntu.com/ubuntu
#    (vào chroot cài thêm app: sudo ltsp chroot /srv/golden ... )

# 2. Đóng image + initrd + kernel + NFS export
sudo ltsp image /srv/golden      # -> squashfs golden RO
sudo ltsp initrd                 # initrd có overlay
sudo ltsp kernel /srv/golden     # copy vmlinuz/initrd ra /srv/tftp/ltsp/
sudo ltsp nfs                    # export NFS RO
sudo ltsp ipxe                   # sinh /srv/tftp/ltsp/ltsp.ipxe (có dòng kernel/initrd/imgargs)
```

Xem lệnh boot LTSP sinh ra:
```bash
cat /srv/tftp/ltsp/ltsp.ipxe
```
→ copy phần `kernel ... / initrd ... / imgargs ... / boot` vào **boot_script** của image Ubuntu:
- Web `http://10.0.0.12/` → bảng Image → nút **Boot script** → dán.
- Hoặc API:
```bash
curl -X POST localhost/api/images/boot-script -H 'content-type: application/json' \
  -d '{"id":1,"boot_script":"<dán các dòng kernel/initrd/imgargs/boot ở đây, \\n giữa dòng>"}'
```

⚠ LTSP phục vụ kernel/initrd qua **TFTP** (`/srv/tftp/ltsp/`), nên trong boot_script để
`kernel tftp://10.0.0.12/ltsp/vmlinuz ...` (hoặc copy 2 file sang /srv/http rồi dùng http://).

Boot 1 VM → chọn Ubuntu → lên desktop diskless. Ghi thử file → nằm overlay (RAM) → reboot mất.

## Chuyển writeback sang SSD LOCAL (đúng plan, làm sau khi RAM overlay chạy)
LTSP overlay mặc định tmpfs (RAM). Để ghi xuống SSD local mỗi máy:
- Client có 1 ổ SSD trống → tạo partition dành cho writeback.
- Chỉnh initrd/overlay trỏ upper vào ổ đó thay tmpfs (LTSP: cân nhắc thay bằng
  initramfs `overlayroot=device:dev=LABEL=writeback` — hướng explicit, không dùng LTSP overlay).
- Reset upper mỗi boot = xoá nội dung partition đầu initramfs.

Cách explicit (không LTSP) nếu cần kiểm soát SSD-local chặt:
- Golden rootfs export NFS RO.
- initramfs (MODULES=most, có nfs + overlay): mount NFS RO = lower, SSD local = upper, overlayfs.
- boot_script: `kernel .../vmlinuz root=/dev/nfs nfsroot=10.0.0.12:/srv/golden,ro ip=dhcp overlayroot=device:dev=LABEL=writeback` + `initrd` + `boot`.

## Test (ghi vào progress.md)
- [ ] VM boot Ubuntu diskless qua menu mgmt app
- [ ] Ghi file → reboot → mất (reset OK)
- [ ] 2 VM cùng lúc share 1 golden, không đụng nhau
- [ ] (tune) writeback nằm SSD local, không phải RAM
