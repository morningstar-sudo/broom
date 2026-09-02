// overlay.rs — golden raw disk → trích kernel/initrd cho iPXE + prep script bake overlay hook.
//
// Mô hình (thay LTSP): golden = raw disk (từ vmdk), serve iSCSI RO shared. Client boot bằng
// chính kernel/initrd của golden; initrd (đã bake open-iscsi + overlayroot + reset hook trong
// golden VM qua PREP_SCRIPT) tự: attach iSCSI (iBFT do iPXE sanhook set) → mount root RO =
// lower → dựng writeback SSD local (reset mỗi boot) → overlayfs → boot. Writeback xuống SSD,
// không RAM. User/app bake sẵn trong img.
//
// Server-side: dùng **libguestfs** (virt-ls/virt-copy-out/guestfish) đọc golden — appliance
// CÔ LẬP, tự xử LVM/ext4/xfs, KHÔNG đụng LVM/mount của host (server cũng chạy LVM, mount thẳng
// partition sẽ gặp 'LVM2_member' + rủi ro trùng VG). Trích vmlinuz+initrd mới nhất ra
// /srv/tftp/broom/<name>/ + đọc UUID root (cho boot_script root=UUID=).
//
// ⚠ Phần overlay/iSCSI trong initrd (PREP_SCRIPT) là phần rủi ro nhất — PHẢI PoC + tune trên
// server thật (B5). Bản dưới là draft đầu, giống ltsp-script từng tiến hoá trên server.
use std::path::Path;
use std::process::Command;

/// Chạy lệnh, trả stdout (trim). Lỗi → Err(stderr).
fn out(bin: &str, args: &[&str]) -> Result<String, String> {
    let o = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("{bin}: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        Err(format!("{bin} {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()))
    }
}

/// Trích vmlinuz + initrd.img từ golden (qua libguestfs) → /srv/tftp/broom/<name>/. Trả UUID root.
/// Blocking, cần root (libguestfs appliance).
pub fn build_boot(img: &Path, name: &str) -> Result<String, String> {
    let img = img.to_string_lossy().to_string();
    let dst = format!("/srv/tftp/broom/{name}");
    std::fs::create_dir_all(&dst).map_err(|e| e.to_string())?;

    // 1. Kernel version MỚI NHẤT trong /boot. virt-ls tự inspect+mount (gồm LVM); sort -V để
    //    so version đúng số (5.15.0-119 > 5.15.0-91, không phải string sort).
    let kv = out(
        "sh",
        &[
            "-c",
            &format!("virt-ls -a '{img}' /boot | sed -n 's/^vmlinuz-//p' | sort -V | tail -1"),
        ],
    )
    .map_err(|e| format!("virt-ls /boot: {e} (cài libguestfs-tools?)"))?;
    if kv.is_empty() {
        return Err(
            "golden không có /boot/vmlinuz-* — golden có kernel + đã chạy prep script chưa?".into(),
        );
    }

    // 2. Copy kernel + initrd ra dst rồi đổi tên chuẩn.
    out("virt-copy-out", &["-a", &img, &format!("/boot/vmlinuz-{kv}"), &dst])
        .map_err(|e| format!("virt-copy-out vmlinuz: {e}"))?;
    out("virt-copy-out", &["-a", &img, &format!("/boot/initrd.img-{kv}"), &dst])
        .map_err(|e| format!("virt-copy-out initrd: {e}"))?;
    let vm_src = format!("{dst}/vmlinuz-{kv}");
    let ir_src = format!("{dst}/initrd.img-{kv}");
    if !Path::new(&vm_src).exists() {
        return Err(format!("virt-copy-out không tạo {vm_src} (kernel version dò sai?)"));
    }
    if !Path::new(&ir_src).exists() {
        return Err(format!("virt-copy-out không tạo {ir_src}"));
    }
    std::fs::rename(&vm_src, format!("{dst}/vmlinuz"))
        .map_err(|e| format!("rename {vm_src}: {e}"))?;
    let initrd = format!("{dst}/initrd.img");
    std::fs::rename(&ir_src, &initrd)
        .map_err(|e| format!("rename {ir_src}: {e}"))?;

    // Inject hook broom-wb + overlayroot.conf vào initrd (append cpio → override bản golden).
    // → tune reset/overlay = sửa Rust + Publish lại, KHỎI rebuild golden.
    inject_initrd(&initrd, name)?;

    // 3. UUID root: guestfish inspect-os → device root (LV hoặc partition) → vfs-uuid.
    let root_dev = out("guestfish", &["--ro", "-a", &img, "run", ":", "inspect-os"])
        .map_err(|e| format!("guestfish inspect-os: {e}"))?;
    let root_dev = root_dev
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or("inspect-os không trả root device".to_string())?;
    let uuid = out("guestfish", &["--ro", "-a", &img, "run", ":", "vfs-uuid", &root_dev])
        .map_err(|e| format!("guestfish vfs-uuid {root_dev}: {e}"))?;
    if uuid.is_empty() {
        return Err(format!("không đọc được UUID root ({root_dev})"));
    }
    Ok(uuid)
}

/// overlayroot.conf inject vào initrd (server sở hữu → tune khỏi rebuild golden).
/// root RO (iSCSI) + upper trên SSD local LABEL=broomwb (broom-wb hook lo format).
const OVERLAYROOT_CONF: &str =
    "overlayroot=\"device:dev=/dev/disk/by-label/broomwb,recurse=0\"\noverlayroot_cfgdisk=\"disabled\"\n";

/// Hook GỘP, OVERRIDE `scripts/local-top/iscsi` (file này ĐÃ có trong ORDER nên chắc chạy —
/// script tự thêm KHÔNG có trong ORDER sẽ bị initramfs-tools bỏ qua). Làm 2 việc mỗi boot:
///  1. attach golden iSCSI: bật NIC (ipconfig dhcp) + modprobe iscsi_ibft + iscsistart -b
///     (root=UUID nên initramfs không tự bật mạng — phải tự lo, trước khi chờ root device).
///  2. prep writeback SSD LOCAL: mkfs LABEL=broomwb (reset sạch mỗi boot) cho overlayroot.
///     Ổ local = ổ CÓ TRƯỚC khi attach iSCSI (chụp danh sách trước) → không đụng golden iSCSI.
const BROOM_ISCSI: &str = r#"#!/bin/sh
case "$1" in prereqs) echo ""; exit 0;; esac
modprobe iscsi_tcp 2>/dev/null
modprobe iscsi_ibft 2>/dev/null
# Ổ LOCAL = ổ vật lý CÓ TRƯỚC khi attach iSCSI (golden iSCSI chưa hiện lúc này).
localdisks=""
for d in /sys/block/*; do
  n=${d##*/}
  case "$n" in loop*|ram*|dm-*|sr*|nbd*|md*|fd*|zram*) continue;; esac
  sz=$(cat "$d/size" 2>/dev/null || echo 0); [ "$sz" -gt 0 ] || continue
  localdisks="$localdisks $n"
done
# Bật mạng: DHCP trên từng NIC (trừ lo) cho tới khi 1 cái lên.
for ni in /sys/class/net/*; do
  n=${ni##*/}; [ "$n" = lo ] && continue
  ipconfig -t 15 "$n" >/dev/null 2>&1 && break
done
# Đợi iBFT sysfs (module vừa load parse ACPI iBFT) → login target theo iBFT.
i=0; while [ ! -d /sys/firmware/ibft ] && [ $i -lt 5 ]; do sleep 1; i=$((i+1)); done
iscsistart -b 2>/dev/null || true
udevadm settle 2>/dev/null || sleep 3
echo "broom: iscsi attach -> $(ls /dev/sd* 2>/dev/null)"
# Writeback SSD local: mkfs THẲNG CẢ Ổ (khỏi parted — initramfs không có) LABEL=broomwb,
# reset mỗi boot. Chỉ cần mkfs.ext4 trong initramfs (broom-prep nhúng qua hook nếu thiếu).
for n in $localdisks; do
  if mkfs.ext4 -qF -L broomwb "/dev/$n" 2>/dev/null; then
    echo "broom: writeback tren /dev/$n"; break
  else
    echo "broom: mkfs /dev/$n loi (mkfs.ext4 co trong initramfs?)"
  fi
done
"#;

/// Append 1 cpio.gz (override local-top/iscsi = attach golden + writeback SSD; + /etc/overlayroot.conf)
/// vào cuối initrd → kernel nối nhiều cpio, bản sau override bản golden.
fn inject_initrd(initrd: &str, name: &str) -> Result<(), String> {
    let work = format!("/tmp/broom-inject-{name}");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(format!("{work}/scripts/local-top")).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(format!("{work}/etc")).map_err(|e| e.to_string())?;
    // OVERRIDE scripts/local-top/iscsi (đã có trong ORDER → chắc chạy). Làm cả attach + writeback.
    let iscsi_hook = format!("{work}/scripts/local-top/iscsi");
    std::fs::write(&iscsi_hook, BROOM_ISCSI).map_err(|e| e.to_string())?;
    std::fs::write(format!("{work}/etc/overlayroot.conf"), OVERLAYROOT_CONF)
        .map_err(|e| e.to_string())?;
    let _ = Command::new("chmod").args(["0755", &iscsi_hook]).status();
    // cd work → cpio newc gzip → append vào initrd (đường tuyệt đối).
    let sh = format!(
        "cd '{work}' && find . -mindepth 1 -print0 | cpio --null -o -H newc 2>/dev/null | gzip -9 >> '{initrd}'"
    );
    let ok = Command::new("sh").arg("-c").arg(&sh).status().map(|s| s.success()).unwrap_or(false);
    let _ = std::fs::remove_dir_all(&work);
    if ok {
        Ok(())
    } else {
        Err("inject_initrd: append cpio lỗi (thiếu cpio/gzip?)".into())
    }
}

/// Script CHẠY TRONG GOLDEN VM: cài open-iscsi + overlayroot + update-initramfs (chỉ package;
/// hook broom-wb + overlayroot.conf do server inject vào initrd → khỏi rebuild golden khi tune).
/// Dùng: curl -fsSL http://<server>/broom-prep | sudo bash
/// ⚠ DRAFT — tune trên PoC server (B5). __IP__ thay IP server.
pub const PREP_SCRIPT: &str = r#"#!/usr/bin/env bash
# Chạy TRONG golden VM (Ubuntu) 1 lần: cài package cho iSCSI-root + overlay.
# Hook reset SSD (broom-wb) + overlayroot.conf do SERVER inject vào initrd → tune khỏi rebuild golden.
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "Chạy bằng sudo"; exit 1; }

export DEBIAN_FRONTEND=noninteractive
apt-get update
# open-iscsi = attach iSCSI trong initramfs (iBFT do iPXE sanhook set).
# overlayroot = root RO + overlay upper (writeback). parted/e2fsprogs cho reset hook.
apt-get install -y open-iscsi overlayroot parted e2fsprogs

# BẮT BUỘC cho iSCSI-root: marker này bảo update-initramfs NHÚNG iSCSI (iscsistart+module)
# vào initramfs. Thiếu nó → initramfs không attach được iSCSI → "root does not exist".
mkdir -p /etc/iscsi && touch /etc/iscsi/iscsi.initramfs

# Diskless: /boot, /boot/efi, swap trong fstab nằm trên golden iSCSI RO → fsck/swapon cần RW
# → FAIL → Emergency Mode. Client PXE không cần mount chúng → comment khỏi fstab.
sed -i '/[[:space:]]\/boot\/efi[[:space:]]/ s/^#*/#/' /etc/fstab
sed -i '/[[:space:]]\/boot[[:space:]]/ s/^#*/#/' /etc/fstab
sed -i '/[[:space:]]swap[[:space:]]/ s/^#*/#/' /etc/fstab

# Café diskless: cloud-init vô dụng + gây treo boot → tắt. wait-online treo chờ mạng → tắt.
touch /etc/cloud/cloud-init.disabled 2>/dev/null || true
systemctl disable cloud-init cloud-init-local cloud-config cloud-final 2>/dev/null || true
systemctl mask systemd-networkd-wait-online.service NetworkManager-wait-online.service 2>/dev/null || true

# iSCSI-ROOT: NIC phải ỔN ĐỊNH. NetworkManager reconfigure NIC lúc boot → bounce link →
# iSCSI session (bám NIC đó) rớt → "blk_update_request: I/O error dev sdb". Dùng
# systemd-networkd + KeepConfiguration=yes (không deconfigure khi restart) thay NM.
systemctl disable NetworkManager 2>/dev/null || true
systemctl enable systemd-networkd systemd-resolved 2>/dev/null || true
cat >/etc/systemd/network/10-broom.network <<'NETEOF'
[Match]
Name=en* eth*
[Network]
DHCP=yes
KeepConfiguration=yes
[Link]
RequiredForOnline=no
NETEOF

# iSCSI chịu blip mạng: tăng replacement_timeout (session chờ thay vì lỗi ngay).
if [ -f /etc/iscsi/iscsid.conf ]; then
  sed -i 's/^node.session.timeo.replacement_timeout.*/node.session.timeo.replacement_timeout = 120/' /etc/iscsi/iscsid.conf
fi

# Dọn artifact prep bản cũ (hook/overlayroot.conf giờ do SERVER inject vào initrd).
rm -f /etc/initramfs-tools/scripts/init-top/broom-wb /etc/overlayroot.conf

# Nhúng mkfs.ext4 vào initramfs (initramfs mặc định KHÔNG có) — hook broom-iscsi mkfs
# ổ local LABEL=broomwb mỗi boot cho overlayroot.
cat >/etc/initramfs-tools/hooks/broom-tools <<'EOF'
#!/bin/sh
PREREQ=""
prereqs(){ echo "$PREREQ"; }
case $1 in prereqs) prereqs; exit 0;; esac
. /usr/share/initramfs-tools/hook-functions
copy_exec /sbin/mkfs.ext4
copy_exec /sbin/blkid 2>/dev/null || true
mkdir -p "$DESTDIR/etc"
cp /etc/mke2fs.conf "$DESTDIR/etc/" 2>/dev/null || true
EOF
chmod +x /etc/initramfs-tools/hooks/broom-tools

update-initramfs -u

echo
echo "==================================================================="
echo " XONG. Golden da co package iSCSI + overlay."
echo " -> Tat VM, lay file .vmdk cua VM nay, UPLOAD qua web admin:"
echo "      http://__IP__/   (muc 'Golden (.vmdk)')"
echo "==================================================================="
"#;
