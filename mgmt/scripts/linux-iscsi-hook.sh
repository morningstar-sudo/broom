#!/bin/sh
case "$1" in prereqs) echo ""; exit 0;; esac
# Log to console + /run/broom-wb.log (/run moves to the real root → readable after boot).
log(){ echo "broom: $*"; echo "$*" >> /run/broom-wb.log; }
WB_GB=30   # writeback size (same 30 GB in stage.sh layout()); the rest of the Linux part = cache + /games.
modprobe iscsi_tcp 2>/dev/null
modprobe iscsi_ibft 2>/dev/null
NAME=""; HASH=""; SIZE=""; NOCACHE=""; REG=""; LX=""; SSD=""; SRV=""
for a in $(cat /proc/cmdline); do
  case "$a" in
    broom.name=*) NAME=${a#*=};; broom.hash=*) HASH=${a#*=};; broom.size=*) SIZE=${a#*=};;
    broom.nocache) NOCACHE=1;;
    broom.reg=*) REG=${a#*=};; broom.lxgb=*) LX=${a#*=};; broom.ssd=*) SSD=${a#*=};; broom.srv=*) SRV=${a#*=};;
  esac
done
case "$LX" in ''|*[!0-9]*) LX=0;; esac
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
has_label(){ [ "$(blkid -s LABEL -o value "$1" 2>/dev/null)" = "$2" ]; }

# GPT partition name of a sysfs partition dir, and the device of the partition named $2 on disk $1 (no tool needed).
pname(){ sed -n 's/^PARTNAME=//p' "$1/uevent" 2>/dev/null; }
byname(){ for p in /sys/block/$1/$1*; do [ "$(pname $p)" = "$2" ] && { echo /dev/${p##*/}; return; }; done; }
# 1. The Broom SSD — ONE disk shared with the Windows stage (same layout, same partition names):
#    p1 BROOMEFI 512M | p2 BROOMWIN (Windows) | p3 broomwb WB_GB (writeback, mkfs every boot) | p4 broomcache (kept)
#    LX = GB for the Linux part, sized by the server from its Linux goldens. No room for Windows (< 32 GB left) or no
#    LX (boot script of an older version) → Linux takes the whole disk (p1 broomwb + p2 broomcache, as before).
layout(){
  gb=$(( $(cat /sys/block/$1/size) / 2097152 )); win=$((gb - 1 - LX))
  [ $gb -ge $((WB_GB + 8)) ] || return 1
  printf 'label: gpt\n'
  if [ $LX -ge $((WB_GB + 8)) ] && [ $win -ge 32 ]; then
    printf 'size=512MiB, type=U, name=BROOMEFI\nsize=%sGiB, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name=BROOMWIN, attrs="GUID:63"\n' $win
  fi
  printf 'size=%sGiB, name=broomwb\nname=broomcache\n' $WB_GB
}
wb=""; cache=""
# Image set to not use the SSD (Images page): no disk is looked at, touched or formatted → RAM only.
[ "$SSD" = 0 ] && log "image set to not use the SSD -> golden over iSCSI, writes in RAM (zram)"
if [ "$SSD" != 0 ] && command -v sfdisk >/dev/null && command -v losetup >/dev/null; then
  # The Broom SSD = a local disk carrying broom's partition names (laid out by this hook or by the Windows stage).
  bd=""
  for n in $localdisks; do
    for p in /sys/block/$n/$n*; do
      case "$(pname $p)" in BROOMEFI|BROOMWIN|broomwb|broomcache) bd=$n; break 2;; esac
    done
  done
  [ -n "$bd" ] && { wb=$(byname $bd broomwb); cache=$(byname $bd broomcache); }
  # A disk is partitioned (WIPED) only when it is the Broom SSD without the Linux part (laid out by Windows while
  # there was no Linux image — only broom data on it), or on a REGISTERED machine (broom.reg=1, Machines page) with
  # exactly ONE local disk. An unknown machine that PXE-boots, or one with several disks and no Broom SSD yet (which
  # one is the scratch SSD?), keeps its disks untouched and writes to zram below.
  set -- $localdisks
  new=""
  if [ -n "$bd" ]; then
    [ -n "$cache" ] || new=$bd
  elif [ "$REG" = 1 ] && [ $# -eq 1 ]; then
    new=$1
  else
    log "not partitioning any disk (registered=${REG:-no}, local disks: $#) -> writeback in RAM (zram)"
  fi
  if [ -n "$new" ]; then
    if lay=$(layout $new); then
      log "partitioning /dev/$new as the Broom SSD (WIPES the disk; Linux part: ${LX} GB)"
      if printf '%s\n' "$lay" | sfdisk -q --wipe always --wipe-partitions always "/dev/$new" >/dev/null 2>&1; then
        udevadm settle 2>/dev/null
        i=0; while [ -z "$(byname $new broomcache)" ] && [ $i -lt 10 ]; do sleep 1; i=$((i+1)); done
        wb=$(byname $new broomwb); cache=$(byname $new broomcache)
      else
        log "sfdisk /dev/$new failed"; wb=""; cache=""
      fi
    else
      log "/dev/$new is too small for the writeback + cache -> zram"; wb=""; cache=""
    fi
  fi
  # broomcache laid out (here or by the Windows stage) but not formatted yet → ext4 (kept from now on).
  if [ -n "$cache" ] && ! has_label "$cache" broomcache; then
    mkfs.ext4 -qF -L broomcache "$cache" 2>/dev/null || { log "mkfs cache $cache failed"; cache=""; }
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
    # Its .sha256 mtime = last use: when the cache runs out of room, the copy unused the longest goes first.
    if losetup -f -r -P "$C/$NAME.img"; then MODE=hit; touch "$C/$NAME.sha256"; else log "losetup failed -> iSCSI"; fi
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
  printf 'MODE=%s\nNAME=%s\nHASH=%s\nSIZE=%s\nGOLDEN=%s\nSRV=%s\n' "$MODE" "$NAME" "$HASH" "$SIZE" "$GOLDEN" "$SRV" > /run/broom-cache.env
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
