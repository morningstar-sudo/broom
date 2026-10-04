#!/bin/sh
case "$1" in prereqs) echo ""; exit 0;; esac
# Log to console + /run/broom-wb.log (/run moves to the real root → readable after boot).
log(){ echo "broom: $*"; echo "$*" >> /run/broom-wb.log; }
WB_GB=30   # writeback size of p1; the rest = cache + /games. Change = wipefs the disk to repartition.
modprobe iscsi_tcp 2>/dev/null
modprobe iscsi_ibft 2>/dev/null
NAME=""; HASH=""; SIZE=""; NOCACHE=""; REG=""
for a in $(cat /proc/cmdline); do
  case "$a" in
    broom.name=*) NAME=${a#*=};; broom.hash=*) HASH=${a#*=};; broom.size=*) SIZE=${a#*=};;
    broom.nocache) NOCACHE=1;;
    broom.reg=*) REG=${a#*=};;
  esac
done
# LOCAL disks = physical disks present BEFORE attaching iSCSI (golden iSCSI not visible yet). Skip removable and
# USB disks (an external USB HDD/SSD often reports removable=0).
localdisks=""
for d in /sys/block/*; do
  n=${d##*/}
  case "$n" in loop*|ram*|dm-*|sr*|nbd*|md*|fd*|zram*) continue;; esac
  [ "$(cat "$d/removable" 2>/dev/null)" = 1 ] && continue
  case "$(readlink -f "$d")" in */usb*) continue;; esac
  sz=$(cat "$d/size" 2>/dev/null || echo 0); [ "$sz" -gt 0 ] || continue
  localdisks="$localdisks $n"
done
log "local disks: ${localdisks:-(none)} | image=$NAME"
# Partition N of a disk: sda→/dev/sda1, nvme0n1→/dev/nvme0n1p1.
part(){ case "$1" in *[0-9]) echo "/dev/${1}p$2";; *) echo "/dev/$1$2";; esac; }
has_label(){ [ "$(blkid -s LABEL -o value "$1" 2>/dev/null)" = "$2" ]; }

# 1. SSD layout: p1 writeback (mkfs every boot), p2 cache (kept).
wb=""; cache=""
if command -v sfdisk >/dev/null && command -v losetup >/dev/null; then
  for n in $localdisks; do
    if has_label "$(part $n 2)" broomcache; then wb=$(part $n 1); cache=$(part $n 2); break; fi
  done
  # A disk is partitioned (WIPED) only on a REGISTERED machine (broom.reg=1, Machines page) with exactly ONE local
  # disk: an unknown machine that PXE-boots, or one with several disks (which one is the scratch SSD?), keeps
  # its disks untouched and writes to zram below.
  set -- $localdisks
  if [ -z "$cache" ] && { [ "$REG" != 1 ] || [ $# -ne 1 ]; }; then
    log "not partitioning any disk (registered=${REG:-no}, local disks: $#) -> writeback in RAM (zram)"
  elif [ -z "$cache" ]; then
    for n in $localdisks; do
      # Disk too small (< WB + 8GB) → skip (zram below).
      [ $(( $(cat /sys/block/$n/size) / 2097152 )) -ge $((WB_GB + 8)) ] || continue
      log "partitioning /dev/$n for the first time: p1 ${WB_GB}G writeback + p2 cache (WIPES the disk)"
      printf 'label: gpt\nsize=%sGiB, name=broomwb\nname=broomcache\n' "$WB_GB" \
        | sfdisk -q --wipe always --wipe-partitions always "/dev/$n" >/dev/null 2>&1 \
        || { log "sfdisk /dev/$n failed"; continue; }
      udevadm settle 2>/dev/null
      i=0; while [ ! -b "$(part $n 2)" ] && [ $i -lt 10 ]; do sleep 1; i=$((i+1)); done
      if mkfs.ext4 -qF -L broomcache "$(part $n 2)" 2>/dev/null; then
        wb=$(part $n 1); cache=$(part $n 2); break
      fi
      log "mkfs cache $(part $n 2) failed"
    done
  fi
fi
# mkfs discards (TRIMs) the whole writeback first → the last session's files are not left readable in the
# raw blocks. Near-instant on an SSD; a device without TRIM just skips it.
if [ -n "$wb" ]; then
  mkfs.ext4 -qF -L broomwb -O ^has_journal "$wb" 2>/dev/null || { log "mkfs $wb failed"; wb=""; }
fi
# Fallback without SSD: zram (compressed RAM). Needs the zram module in the initramfs (new broom-prep).
if [ -z "$wb" ] && modprobe zram 2>/dev/null && [ -e /sys/block/zram0/disksize ]; then
  mem=$(sed -n 's/^MemTotal: *\([0-9]*\) kB/\1/p' /proc/meminfo)
  echo "$((mem * 512))" > /sys/block/zram0/disksize
  mkfs.ext4 -qF -L broomwb -O ^has_journal /dev/zram0 2>/dev/null && wb=/dev/zram0
fi

# 2. Cache: mount p2 under /run (moves to the real root with /run → service + /games keep using it).
C=/run/broomcache; MODE=none; GOLDEN=""
if [ -n "$cache" ] && mkdir -p $C && mount -t ext4 "$cache" $C; then
  mkdir -p $C/games
  MODE=miss
  if [ -z "$NOCACHE" ] && [ -n "$HASH" ] && [ -f "$C/$NAME.img" ] \
     && [ "$(cat "$C/$NAME.sha256" 2>/dev/null)" = "$HASH" ]; then
    # Trust the sha256 written when the copy finished, don't rehash the whole golden every boot. Suspect corruption → broom.nocache.
    if losetup -f -r -P "$C/$NAME.img"; then MODE=hit; else log "losetup failed -> iSCSI"; fi
  fi
fi
log "cache: $MODE ($cache)"

# 3. MISS / no cache → attach iSCSI as before.
if [ "$MODE" != hit ]; then
  for ni in /sys/class/net/*; do
    n=${ni##*/}; [ "$n" = lo ] && continue
    ipconfig -t 15 "$n" >/dev/null 2>&1 && break
  done
  i=0; while [ ! -d /sys/firmware/ibft ] && [ $i -lt 5 ]; do sleep 1; i=$((i+1)); done
  iscsistart -b 2>/dev/null || true
  udevadm settle 2>/dev/null || sleep 3
  # iSCSI disk = sysfs path goes through an iSCSI session.
  for d in /sys/block/sd*; do
    readlink -f "$d" | grep -q /session && GOLDEN=/dev/${d##*/}
  done
  log "iscsi attach -> ${GOLDEN:-NO iSCSI disk found}"
fi
udevadm settle 2>/dev/null
# The golden may use LVM. Its device is read-only (iSCSI RO, or losetup -r on a cache HIT), so the stock lvm2 hook's
# writable vgchange fails on it (device-mapper -EROFS). Activate any volume groups read-only ourselves — dm then
# opens the PV read-only. No-op when the golden is a plain partition (no VGs found); overlayroot still puts the
# writeback on the SSD, so a read-only root LV is fine.
if command -v lvm >/dev/null 2>&1; then
  lvm pvscan --cache >/dev/null 2>&1 || true
  for vg in $(lvm vgs --noheadings -o vg_name 2>/dev/null); do
    [ -n "$vg" ] || continue
    lvm vgchange -ay --config "global{metadata_read_only=1} activation{read_only_volume_list=[\"$vg\"]}" "$vg" >/dev/null 2>&1 \
      && log "lvm: $vg activated read-only"
  done
  udevadm settle 2>/dev/null
fi
# Parameters for broom-cache.service (golden) after boot: mount /games + background copy on MISS.
if [ "$MODE" != none ]; then
  printf 'MODE=%s\nNAME=%s\nHASH=%s\nSIZE=%s\nGOLDEN=%s\n' "$MODE" "$NAME" "$HASH" "$SIZE" "$GOLDEN" > /run/broom-cache.env
  cp /scripts/broom-cache.sh /run/broom-cache.sh
fi

if [ -z "$wb" ]; then
  log "!!! NO WRITEBACK — root will be RO (no SSD, no zram)"
  exit 0
fi
# Wait for udev to create /dev/disk/by-label/broomwb — overlayroot (init-bottom) looks it up by label;
# without this step it's a race: works sometimes, not others.
udevadm trigger --action=change "/sys/class/block/${wb#/dev/}" 2>/dev/null
udevadm settle 2>/dev/null
i=0; while [ ! -e /dev/disk/by-label/broomwb ] && [ $i -lt 10 ]; do sleep 1; i=$((i+1)); done
if [ -e /dev/disk/by-label/broomwb ]; then
  log "writeback on $wb"
else
  log "!!! /dev/disk/by-label/broomwb did not appear after mkfs $wb"
fi
