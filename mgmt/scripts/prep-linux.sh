#!/usr/bin/env bash
# Run INSIDE the golden VM (Ubuntu) once: installs packages for iSCSI-root + overlay.
# SSD reset hook (broom-wb) + overlayroot.conf are injected into the initrd by the SERVER → tune without a golden rebuild.
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "Run with sudo"; exit 1; }

export DEBIAN_FRONTEND=noninteractive
apt-get update
# open-iscsi = attach iSCSI inside the initramfs (iBFT set by iPXE sanhook).
# overlayroot = root RO + overlay upper (writeback). parted/e2fsprogs for the reset hook.
apt-get install -y open-iscsi overlayroot parted e2fsprogs

# REQUIRED for iSCSI-root: this marker tells update-initramfs to EMBED iSCSI (iscsistart+module)
# in the initramfs. Without it → the initramfs can't attach iSCSI → "root does not exist".
mkdir -p /etc/iscsi && touch /etc/iscsi/iscsi.initramfs

# Diskless: /boot, /boot/efi live on the RO golden → mount fails on the client → Emergency Mode.
# Do NOT comment them out (the golden VM needs /boot mounted so apt updates the kernel in the right place; the server
# reads the kernel by filesystem) → uncomment (old prep used to comment them) + add nofail: skipped on client errors.
sed -i -E 's@^#+([^[:space:]]+[[:space:]]+/boot(/efi)?[[:space:]])@\1@' /etc/fstab
awk '($2=="/boot"||$2=="/boot/efi") && $4!~/nofail/ {$4=$4",nofail"} {print}' /etc/fstab >/etc/fstab.broom \
  && cat /etc/fstab.broom >/etc/fstab && rm -f /etc/fstab.broom
mount /boot 2>/dev/null || true; mount /boot/efi 2>/dev/null || true
# swap on the RO golden → swapon needs RW → comment it out.
sed -i '/[[:space:]]swap[[:space:]]/ s/^#*/#/' /etc/fstab

# Diskless: cloud-init is useless + hangs boot → disable. wait-online hangs waiting for the network → disable.
touch /etc/cloud/cloud-init.disabled 2>/dev/null || true
systemctl disable cloud-init cloud-init-local cloud-config cloud-final 2>/dev/null || true
systemctl mask systemd-networkd-wait-online.service NetworkManager-wait-online.service 2>/dev/null || true

# iSCSI-ROOT: the NIC must be STABLE. NetworkManager reconfigures the NIC at boot → link bounce →
# the iSCSI session (bound to that NIC) drops → "blk_update_request: I/O error dev sdb". Use
# systemd-networkd + KeepConfiguration=yes (no deconfigure on restart) instead of NM.
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

# iSCSI tolerates network blips: raise replacement_timeout (the session waits instead of failing at once).
if [ -f /etc/iscsi/iscsid.conf ]; then
  sed -i 's/^node.session.timeo.replacement_timeout.*/node.session.timeo.replacement_timeout = 120/' /etc/iscsi/iscsid.conf
fi

# Hide broom's own SSD partitions (writeback + golden cache) from the desktop file manager — they are internal,
# and letting a guest browse or mount the cache could expose or corrupt the shared golden.
mkdir -p /etc/udev/rules.d
cat >/etc/udev/rules.d/99-broom-hide.rules <<'UDEV'
ENV{ID_FS_LABEL}=="broomwb", ENV{UDISKS_IGNORE}="1"
ENV{ID_FS_LABEL}=="broomcache", ENV{UDISKS_IGNORE}="1"
UDEV

# Remove artifacts of the old prep (hook/overlayroot.conf are now injected into the initrd by the SERVER).
rm -f /etc/initramfs-tools/scripts/init-top/broom-wb /etc/overlayroot.conf

# Embed mkfs.ext4 in the initramfs (the default initramfs does NOT have it) — the broom-iscsi hook runs mkfs on
# the local disk LABEL=broomwb every boot for overlayroot.
cat >/etc/initramfs-tools/hooks/broom-tools <<'EOF'
#!/bin/sh
PREREQ=""
prereqs(){ echo "$PREREQ"; }
case $1 in prereqs) prereqs; exit 0;; esac
. /usr/share/initramfs-tools/hook-functions
copy_exec /sbin/mkfs.ext4
copy_exec /sbin/blkid 2>/dev/null || true
# sfdisk (partition the SSD writeback+cache) + util-linux losetup (-P, busybox lacks it) for the SSD cache.
copy_exec /sbin/sfdisk
copy_exec /sbin/losetup
manual_add_modules zram
manual_add_modules loop
mkdir -p "$DESTDIR/etc"
cp /etc/mke2fs.conf "$DESTDIR/etc/" 2>/dev/null || true
EOF
chmod +x /etc/initramfs-tools/hooks/broom-tools

# SSD cache: stub that calls the script the initrd (server-injected) copied to /run → mount /games + copy the golden
# to the SSD in the background on cache MISS. Logic lives server-side, changes need no golden rebuild.
cat >/etc/systemd/system/broom-cache.service <<'EOF'
[Unit]
Description=broom: /games + golden cache on SSD
ConditionPathExists=/run/broom-cache.sh
[Service]
Type=simple
ExecStart=/bin/sh /run/broom-cache.sh
[Install]
WantedBy=multi-user.target
EOF
systemctl enable broom-cache.service

update-initramfs -u

echo
echo "==================================================================="
echo " DONE. The golden now has the iSCSI + overlay packages."
echo " -> Power off the VM, take this VM's .vmdk file, UPLOAD it via the web admin:"
echo "      http://__IP__/   (section 'Golden (.vmdk)')"
echo "==================================================================="
