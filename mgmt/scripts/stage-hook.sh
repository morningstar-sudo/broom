#!/bin/sh
PREREQ=""
prereqs(){ echo "$PREREQ"; }
case $1 in prereqs) prereqs; exit 0;; esac
. /usr/share/initramfs-tools/hook-functions
for b in __TOOLS__; do
  p=$(command -v $b) && copy_exec "$p" /broom/bin/$b
done
manual_add_modules ntfs3 vfat nls_cp437 nls_iso8859_1 nls_utf8 efivarfs
# Smaller initrd = faster download (iPXE) + unpack on every boot. Runs after the package hooks (conf dir hooks come
# last), before depmod. The stage never mounts a root filesystem (it reboots from init-premount), so out go: the LVM /
# RAID / crypto / btrfs tools of the local-* scripts, every other filesystem, RAID / device-mapper, InfiniBand and the
# data-centre NICs + their firmware, udev's hardware description database. Kept: every desktop Ethernet / USB /
# virtual NIC and its firmware, every storage controller, ntfs3 + vfat. stage.rs checks the must-haves after the build.
K="$DESTDIR/usr/lib/modules/$version/kernel"
FW="$DESTDIR/usr/lib/firmware"
E="$K/drivers/net/ethernet"
rm -rf "$K/drivers/infiniband" "$K/drivers/md" "$E/mellanox" "$E/chelsio" "$E/qlogic" "$E/netronome" "$E/cavium" \
  "$E/broadcom/bnx2x" "$E/broadcom/bnxt"
for f in "$K"/fs/*; do
  case "${f##*/}" in ntfs3|fat|nls|efivarfs) ;; *) rm -rf "$f" ;; esac
done
rm -rf "$FW/mellanox" "$FW/liquidio" "$FW/netronome" "$FW/cxgb3" "$FW/cxgb4" "$FW/bnx2x" "$FW/qed"
rm -f "$FW"/phanfw.bin* "$FW"/ct2fw-* "$FW"/ctfw-* "$FW"/cbfw-*
rm -f "$DESTDIR/usr/lib/udev/hwdb.bin" "$DESTDIR/usr/sbin/lvm" "$DESTDIR/usr/sbin/pdata_tools" \
  "$DESTDIR/usr/sbin/mdadm" "$DESTDIR/usr/sbin/cryptsetup" "$DESTDIR/usr/bin/btrfs"
