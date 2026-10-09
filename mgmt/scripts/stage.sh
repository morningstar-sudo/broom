#!/bin/sh
PREREQ=""
prereqs(){ echo "$PREREQ"; }
case $1 in prereqs) prereqs; exit 0;; esac
. /scripts/functions
PATH=/broom/bin:$PATH
# Log to screen + /run (copied to BROOMWIN\broom\stage.log at the end → readable from Windows).
log(){ echo "broom: $*"; echo "$*" >> /run/broom-stage.log; }
# panic = shell (initramfs); fix by hand then `exit` → the script CONTINUES from the failed step (on-site debug).
die(){ panic "broom stage ERROR: $*"; }
restart(){ log "$*"; sleep 2; reboot -f 2>/dev/null || echo b > /proc/sysrq-trigger; sleep 30; }
# GNU wget copied by the hook, called by path: the initramfs busybox wget has no --post-file / -T (the driver list
# needs them) and must never be picked instead. Its defaults wait 15 min per read and retry 20 times — a stalled server
# must not hang the stage that long: 30 s, 3 tries (an -T given by the caller still wins).
if [ -x /broom/bin/wget ]; then wget(){ /broom/bin/wget -T 30 -t 3 "$@"; }
else log "WARNING: no GNU wget in the stage (publish again) -> drivers skipped"; fi
# Whole-file download showing ONLY a progress bar (file name, %, bytes, speed, ETA) — no URL / connecting / headers /
# "saved" text. GNU wget: -q hides everything, --show-progress brings the bar back; bar:force because the initramfs
# console is not always seen as a tty; noscroll keeps the name still. Busybox wget: already just its own bar.
getfile(){
  if [ -x /broom/bin/wget ]; then /broom/bin/wget -T 30 -t 3 -q --show-progress --progress=bar:force:noscroll "$@"
  else wget "$@"; fi
}
NAME=""; HASH=""; SRV=""; HOST=""; LIC=""; MAC=""; REG=""; BASE=""; STRICT=""; LX=""; WB=""
for a in $(cat /proc/cmdline); do
  case "$a" in broom.name=*) NAME=${a#*=};; broom.hash=*) HASH=${a#*=};; broom.srv=*) SRV=${a#*=};;
    broom.host=*) HOST=${a#*=};; broom.lic=*) LIC=${a#*=};; BOOTIF=01-*) MAC=$(echo "${a#BOOTIF=01-}" | tr - :);;
    broom.reg=*) REG=${a#*=};; broom.base=*) BASE=${a#*=};; broom.strict=*) STRICT=${a#*=};; broom.lxgb=*) LX=${a#*=};; broom.wbgb=*) WB=${a#*=};; esac
done
[ -n "$NAME" ] && [ -n "$HASH" ] && [ -n "$SRV" ] || die "missing broom.name/hash/srv on cmdline"
for m in ntfs3 vfat nls_cp437 nls_iso8859_1 nls_utf8 efivarfs; do modprobe $m 2>/dev/null; done
udevadm settle 2>/dev/null

# Local disk: internal only — not removable, not on USB (an external USB HDD/SSD often reports removable=0), size > 0.
part(){ case "$1" in *[0-9]) echo "/dev/${1}p$2";; *) echo "/dev/$1$2";; esac; }
lbl(){ blkid -s LABEL -o value "$1" 2>/dev/null; }
SYSB=/sys/block; CON=/dev/console
scan(){
  disks=""
  for d in $SYSB/*; do
    n=${d##*/}
    case "$n" in loop*|ram*|dm-*|sr*|nbd*|md*|fd*|zram*) continue;; esac
    [ "$(cat $d/removable 2>/dev/null)" = 1 ] && continue
    case "$(readlink -f $d)" in */usb*) continue;; esac
    [ "$(cat $d/size 2>/dev/null || echo 0)" -gt 0 ] || continue
    disks="$disks $n"
  done
}
# The Broom SSD — ONE disk for Windows and Linux images alike (other disks stay normal disks), known by its GPT
# partition names (read from sysfs: no tool needed, the Linux side does the same):
#   p1 BROOMEFI 512M | p2 BROOMWIN (Windows) | p3 broomwb (WB GB, broom.wbgb) + p4 broomcache (Linux, only when LX > 0)
# LX = GB kept for the Linux side, sized by the server from its Linux goldens (broom.lxgb; 0 = no Linux image).
case "$LX" in ''|*[!0-9]*) LX=0;; esac
case "$WB" in ""|*[!0-9]*) WB=30;; esac   # writeback GB (broom.wbgb); 30 = boot script of an older version
pn(){ p=$(part $1 $2); sed -n 's/^PARTNAME=//p' $SYSB/$1/${p##*/}/uevent 2>/dev/null; }
broomdisk(){
  for n in $disks; do
    for p in $SYSB/$n/$n*; do
      case "$(sed -n 's/^PARTNAME=//p' $p/uevent 2>/dev/null)" in BROOMEFI|BROOMWIN|broomwb|broomcache) echo $n; return;; esac
    done
  done
}
# sfdisk script of that layout for disk $1: Windows gets what LX leaves; under 32 GB left → no Linux part.
layout(){
  gb=$(( $(cat $SYSB/$1/size) / 2097152 )); lx=$LX
  [ $((gb - 1 - lx)) -ge 32 ] || lx=0
  printf 'label: gpt\nsize=512MiB, type=U, name=BROOMEFI\n'
  if [ $lx -gt 0 ]; then
    printf 'size=%sGiB, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name=BROOMWIN, attrs="GUID:63"\nsize=%sGiB, name=broomwb\nname=broomcache\n' $((gb - 1 - lx)) $WB
  else
    printf 'type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name=BROOMWIN, attrs="GUID:63"\n'
  fi
}
scan
# Never guess which disk to wipe: an unknown machine (not on the Machines page — anyone can PXE-boot) or one with
# several disks asks on its screen. Prints the disk typed, nothing for Enter / anything else.
ask_disk(){
  echo "broom: no Broom disk on this machine yet - one disk must be ERASED for Windows." > $CON
  if [ "$REG" != 1 ]; then
    # Its IP + MAC, to find it on the server's Machines page. The network was brought up by the caller: NOT here —
    # this runs in $(...), and configure_networking remembers it is done only in a shell variable (IP=done) that a
    # subshell loses → the next call would run DHCP again on the configured NIC (seen: downloads at ~200 KB/s).
    ip=$(sed -n "s/^IPV4ADDR=['\"]*\([0-9.]*\).*/\1/p" /run/net-*.conf 2>/dev/null | head -n 1)
    echo "broom: this machine is NOT registered on the server (Machines page): IP ${ip:-?}, MAC ${MAC:-?}" > $CON
  fi
  for n in "$@"; do
    printf 'broom:   %-10s %6s GB  %s\n' "$n" "$(( $(cat $SYSB/$n/size) / 2097152 ))" "$(cat $SYSB/$n/device/model 2>/dev/null)" > $CON
  done
  printf 'broom: type the disk to ERASE (e.g. %s), or press Enter to reboot without touching any disk: ' "$1" > $CON
  read -r ans < $CON
  for n in "$@"; do [ "$ans" = "$n" ] && { echo "$n"; return; }; done
}
disk=$(broomdisk); NEWDISK=""; FORMAT=""; one=""
set -- $disks
if [ -z "$disk" ] && [ "$REG" = 1 ] && [ $# -eq 1 ]; then
  # The single disk would be wiped without asking — but a disk that shows up late (slow controller, async probe)
  # could be the Broom SSD, making this one the wrong choice. Look again after a pause: still that one disk → go.
  one=$1; sleep 5; udevadm settle 2>/dev/null; scan; disk=$(broomdisk)
fi
if [ -n "$disk" ]; then
  # p1/p2 not Windows' = a Linux-only layout of an older version → laid out again (only broom data on it).
  # Laid out by a Linux boot → Windows' partitions are there, just not formatted yet.
  if [ "$(pn $disk 1)" != BROOMEFI ] || [ "$(pn $disk 2)" != BROOMWIN ]; then NEWDISK=1
  elif [ "$(lbl $(part $disk 2))" != BROOMWIN ]; then FORMAT=1; fi
fi
if [ -z "$disk" ]; then
  set -- $disks
  [ $# -gt 0 ] || die "no local disk found"
  if [ "$REG" = 1 ] && [ $# -eq 1 ] && [ "$1" = "$one" ]; then
    disk=$1
  else
    [ "$REG" = 1 ] || configure_networking # in this shell (see ask_disk): its IP is shown on the screen
    disk=$(ask_disk "$@")
    [ -n "$disk" ] || restart "no disk chosen -> nothing was touched"
  fi
  NEWDISK=1
fi
# end disk choice
if [ -n "$NEWDISK" ]; then
  log "partitioning /dev/$disk as the Broom SSD (WIPES the disk; Linux part: ${LX} GB)"
  layout $disk | sfdisk -q --wipe always --wipe-partitions always /dev/$disk >/dev/null || die "sfdisk /dev/$disk"
  udevadm settle 2>/dev/null
  i=0; while [ ! -b "$(part $disk 2)" ] && [ $i -lt 10 ]; do sleep 1; i=$((i+1)); done
  FORMAT=1
fi
# Windows' partitions only — the Linux side formats its own.
if [ -n "$FORMAT" ]; then
  mkfs.fat -F32 -n BROOMEFI "$(part $disk 1)" >/dev/null || die "mkfs.fat"
  mkntfs -Q -F -L BROOMWIN "$(part $disk 2)" >/dev/null || die "mkntfs"
fi
W=/run/broomwin; E=/run/broomefi; B=$W/broom
mkdir -p $W $E
# BROOMWIN has no drive letter in Windows (GPT bit 63): users can't see/delete broom files;
# vhdmp still opens the VHDX normally. Disk partitioned by an older version → set the flag again.
[ "$(sfdisk --part-attrs /dev/$disk 2 2>/dev/null)" = GUID:63 ] \
  || { sfdisk -q --part-attrs /dev/$disk 2 GUID:63 >/dev/null 2>&1; udevadm settle 2>/dev/null; }
# Hard power-off while Windows runs (happens often) → NTFS host "dirty" → ntfs3 refuses to mount.
# ntfsfix -d clears the dirty flag + resets the journal (only broom files live here, child resets every boot)
# → Windows also skips autochk at boot. No ntfsfix → mount with force.
ntfsfix -d "$(part $disk 2)" >/dev/null 2>&1
# discard: every cluster freed here (old child.vhdx on reset, old base/golden) is TRIMmed on the SSD at once →
# the last session's data is not left readable on the disk. Device without TRIM → plain mount.
mnt(){ mount -t ntfs3 -o "$1" "$(part $disk 2)" $W 2>/dev/null; }
if mnt discard || mnt force,discard; then log "BROOMWIN: discard mount (freed space is TRIMmed)"
elif mnt rw || mnt force; then log "BROOMWIN: plain mount (the disk takes no TRIM)"
else die "mount ntfs3 $(part $disk 2)"; fi
mkdir -p $B

# 0. Every Windows image this machine boots keeps its files on BROOMWIN: the one booting now at the top of broom\ —
#    where the boot loader (\broom\child.vhdx) and broom-done look — the others parked in broom\img.<image>\ (image
#    names have no dot). Parking is a rename on the same volume (instant) and the VHDX parent links are relative, so a
#    set works in either place. active.txt = the image at the top; every step can simply run again after a power cut.
#    img.<image>/.parked = when it was parked = its last use (the oldest goes first when space runs out).
IMGSET="golden.vhdx golden.sha256 base.vhdx base.host base.lic base.drv base-template.vhdx child-template.vhdx child-template.off efi.tar.gz"
cd $B
act=$(cat active.txt 2>/dev/null)
case "$act" in ''|*[!A-Za-z0-9_-]*) act=$NAME;; esac   # a disk from before image folders: the top set is this image's
if [ "$act" != "$NAME" ]; then
  log "image $act parked, $NAME to the top"
  rm -f child.vhdx first.pending base.ok   # the last session / an unfinished base build of the parked image
  mkdir -p "img.$act"
  for f in $IMGSET dl-*; do [ -e "$f" ] && mv "$f" "img.$act/"; done
  date +%s > "img.$act/.parked"
  echo "$NAME" > active.txt; sync
fi
if [ -d "img.$NAME" ]; then
  for f in "img.$NAME"/*; do [ -e "$f" ] && mv "$f" .; done
  rm -f "img.$NAME/.parked"; rmdir "img.$NAME" 2>/dev/null
  # A parked set sat on BROOMWIN while other sessions ran: its golden is checked against the SERVER's hash (cmdline,
  # not the copy's own golden.sha256) before it is used again — one read, only when switching images. Mismatch
  # (changed meanwhile, or republished) → step 1 downloads it again.
  if [ -f golden.vhdx ] && [ "$(cat golden.sha256 2>/dev/null)" = "$HASH" ]; then
    log "checking the golden of $NAME after its time parked..."
    [ "$(sha256sum golden.vhdx | cut -c1-64)" = "$HASH" ] || { rm -f golden.sha256; log "golden of $NAME does not match the server's -> downloaded again"; }
  fi
fi
echo "$NAME" > active.txt
# Parked images the server no longer lets this machine keep (deleted / republished: "name hash" lines) → removed.
# No answer → kept.
configure_networking
if wget -q -O /run/broom-cache-list "http://$SRV/api/cache-list" 2>/dev/null; then
  for p in img.*; do
    [ -d "$p" ] || continue
    grep -qxF "${p#img.} $(cat $p/golden.sha256 2>/dev/null)" /run/broom-cache-list \
      || { rm -rf "$p"; log "image ${p#img.} removed from this disk (no longer cached here / other version)"; }
  done
fi
# The parked image unused the longest (smallest .parked; busybox ls can't sort by time) → $old, "" when none.
oldest(){
  old=""; t=""
  for p in $B/img.*; do
    [ -d "$p" ] || continue
    v=$(cat $p/.parked 2>/dev/null); case "$v" in ''|*[!0-9]*) v=0;; esac
    if [ -z "$t" ] || [ "$v" -lt "$t" ]; then t=$v; old=$p; fi
  done
}
# Room for this session (the child grows with what the guest writes; a base build needs a few GB): parked images go,
# the oldest first, while BROOMWIN has less than ROOM GB free (Settings → SSD room; 20 when the server doesn't say).
# (A new golden makes its own room in step 1.)
ROOM=$(wget -q -O - "http://$SRV/api/client-config" 2>/dev/null | awk 'NR == 1 && $1 ~ /^[0-9]+$/ { print $1 }')
[ -n "$ROOM" ] || ROOM=20
while df -Pk $W | tail -1 | awk -v r="$ROOM" '{ exit !($4 < r * 1048576) }'; do
  oldest; [ -n "$old" ] || break
  rm -rf "$old"; log "image ${old##*/img.} removed from this disk (unused the longest) to keep room for the session"
done
cd /

gb(){ echo "$1" | awk '{ printf "%.1f GB", $1 / 1073741824 }'; }
fsize(){ ls -ln "$1" 2>/dev/null | awk '{ print $5 }'; }

# 1. Golden hash mismatch → download the whole golden again. No delta: one sequential write into free space (nothing
#    to compare, the file stays unfragmented). The old golden + base/child go first: they belong to the old version
#    and their space is needed.
if [ "$(cat $B/golden.sha256 2>/dev/null)" != "$HASH" ]; then
  log "new golden ($HASH) -> downloading from $SRV"
  configure_networking
  # Server's golden.sha256: gone while a publish/rollback rebuilds the files → wait; another hash → this boot's
  # hash is stale (image changed after iPXE) → reboot for the new boot script. Never mix files of two versions.
  srv_hash(){ wget -q -O - http://$SRV/tftp/broom-win/$NAME/golden.sha256 2>/dev/null; }
  i=0
  while :; do
    h=$(srv_hash)
    [ "$h" = "$HASH" ] && break
    [ -n "$h" ] && restart "image $NAME changed on the server -> reboot to get the new version"
    i=$((i+1)); [ $i -gt 40 ] && die "server has no golden.sha256 for $NAME (publish failed?) -> Publish again, then reboot"
    log "image $NAME is being published on the server -> waiting ($i/40)"; sleep 15
  done
  D=$B/dl-$HASH
  for x in $B/dl-*; do [ "$x" = "$D" ] || rm -rf "$x"; done
  # (golden.chunks / .patching / .spare: leftovers of the delta update of older versions.)
  rm -f $B/golden.sha256 $B/golden.vhdx $B/golden.chunks $B/golden.vhdx.patching $B/base.vhdx $B/base.ok $B/first.pending \
    $B/child.vhdx $B/child-local.vhdx
  rm -rf $B/golden.vhdx.spare
  mkdir -p $D
  # Golden size from the server. NO $((...)) on external data (an ash arithmetic error exits the whole script) → awk.
  # Free space: the golden (minus the part a cut download already holds) + 1 GB for base/child to start with.
  size=$(wget -q -O - http://$SRV/tftp/broom-win/$NAME/golden.size 2>/dev/null)
  case "$size" in ''|*[!0-9]*) die "server has no golden.size for $NAME (published by an older version) -> Publish again, then reboot";; esac
  room(){
    have=$(fsize $D/golden.vhdx); avail=$(df -Pk $W | tail -1 | awk '{ print $4 }')
    awk -v a="$avail" -v h="${have:-0}" -v s="$size" 'BEGIN { exit !(a * 1024 + h >= s + 1073741824) }'
  }
  # Short of space → parked images go, the one unused the longest first.
  until room; do
    oldest
    [ -n "$old" ] || die "not enough space on BROOMWIN: the golden needs $(gb $size) + 1 GB -> a bigger disk or a smaller image"
    rm -rf "$old"; log "image ${old##*/img.} removed from this disk (unused the longest) to make room"
  done
  # $f.ok = file fully downloaded (power loss midway → next boot skips finished files, resumes the partial one).
  # Only -c -O: works with both busybox and GNU wget. -c failing (e.g. 416 when the file is complete but not yet .ok)
  # → download again from scratch.
  n=0
  for f in golden.vhdx base-template.vhdx child-template.vhdx child-template.off efi.tar.gz; do
    n=$((n + 1))
    [ -f $D/$f.ok ] && continue
    sz=""; [ "$f" = golden.vhdx ] && sz=", $(gb $size)"
    log "downloading $f ($n/5$sz)"
    U=http://$SRV/tftp/broom-win/$NAME/$f
    # Fresh golden: sha256 WHILE downloading (tee) → no re-read of the whole file afterwards; counted only with the
    # right length too (tee goes on after a write error, e.g. disk full). Cut midway → the file stays; -c below
    # resumes it and the whole-file check runs at the end.
    if [ "$f" = golden.vhdx ] && [ ! -s $D/$f ]; then
      h=$( (cd $D && getfile -O - $U) | tee $D/$f | sha256sum | cut -c1-64 )
      [ "$h" = "$HASH" ] && [ "$(fsize $D/$f)" = "$size" ] && touch $D/golden.hashed
    fi
    # cd + relative -O: the bar shows the file name ("golden.vhdx"), not the long dl-<hash> path cut short.
    if [ "$f" != golden.vhdx ] || [ ! -f $D/golden.hashed ]; then
      ( cd $D && getfile -c -O $f $U ) || { rm -f $D/$f; ( cd $D && getfile -O $f $U ); } || die "download $f"
    fi
    if [ "$f" = golden.vhdx ] && [ "$(fsize $D/$f)" != "$size" ]; then
      rm -f $D/$f $D/golden.hashed; die "golden.vhdx is not complete (disk full?) -> fix, then exit to retry"
    fi
    touch $D/$f.ok
  done
  [ "$(srv_hash)" = "$HASH" ] || { rm -rf $D; restart "image $NAME changed on the server during the download -> reboot to get the new version"; }
  # Hashed while downloading → no second full read. Otherwise (resumed download) → whole-file sha256.
  if [ ! -f $D/golden.hashed ]; then
    log "checking golden sha256 - rereads the whole file, may take a few minutes, DO NOT power off..."
    [ "$(sha256sum $D/golden.vhdx | cut -d' ' -f1)" = "$HASH" ] || { rm -rf $D; die "golden sha256 mismatch"; }
  fi
  rm -f $D/*.ok $D/golden.hashed
  mv $D/* $B/ && rmdir $D && sync && echo "$HASH" > $B/golden.sha256 && sync
fi

# 1b. The small boot files (EFI bundle, VHDX templates: the image's; the broom-done / boot-order / games scripts the
# golden's stubs run: the server's, same for every image) are checked against the server's sha256 on EVERY boot: the
# guest is a local admin and could swap them on BROOMWIN (e.g. a child template with its own data) to outlive the
# reset. Wrong or missing → downloaded again.
# check_list <list> <url dir> <names...>: the listed files among <names> made equal to the server's.
check_list(){
  l=$1; u=$2; shift 2
  while read -r h f; do
    case " $* " in *" $f "*) ;; *) continue;; esac
    [ "$(sha256sum $B/$f 2>/dev/null | cut -c1-64)" = "$h" ] && continue
    log "$f differs from the server's -> downloading it again"
    if wget -q -O $B/$f.tmp $u/$f && [ "$(sha256sum $B/$f.tmp | cut -c1-64)" = "$h" ]; then
      mv $B/$f.tmp $B/$f
    else
      rm -f $B/$f.tmp; restart "could not get a good $f (publish or server update running?) -> retrying"
    fi
  done < $l
}
configure_networking
if wget -q -O /run/broom-files.sha256 http://$SRV/tftp/broom-win/$NAME/files.sha256 2>/dev/null && [ -s /run/broom-files.sha256 ]; then
  # These files belong to the golden named on the list's "golden" line: another one (published meanwhile) → reboot
  # for the new boot script instead of putting new templates next to this golden.
  g=$(sed -n 's/^\([0-9a-f]*\)  golden$/\1/p' /run/broom-files.sha256)
  [ -z "$g" ] || [ "$g" = "$HASH" ] || restart "image $NAME changed on the server -> reboot to get the new version"
  check_list /run/broom-files.sha256 http://$SRV/tftp/broom-win/$NAME efi.tar.gz child-template.vhdx child-template.off base-template.vhdx
else
  log "no files.sha256 on the server for $NAME (published by an older version) -> Publish again to check the boot files"
fi
if wget -q -O /run/broom-scripts.sha256 http://$SRV/tftp/broom-scripts/scripts.sha256 2>/dev/null && [ -s /run/broom-scripts.sha256 ]; then
  check_list /run/broom-scripts.sha256 http://$SRV/tftp/broom-scripts broom-done.ps1 broom-bootorder.ps1 broom-games.ps1 broom-watch.ps1
else
  log "no scripts.sha256 on the server -> Windows scripts not checked this boot"
fi

# 1c. Preload: the other Windows images of this machine set to preload (cache-list lines "preload name hash windows")
#     are fetched now into broom\img.<name>\ as parked sets, so choosing one later needs no download (its base is
#     still built on its first boot). Only into free space beyond the session's room (ROOM GB): a preload never
#     evicts anything. Downloaded into pre.<name>.<hash>\ first, resumed after a cut; the golden is checked against the
#     server's hash when the set is taken out (step 0), like any parked set.
pre_room(){ df -Pk $W | tail -1 | awk -v s="$1" -v h="${2:-0}" -v r="${ROOM:-20}" '{ exit !($4 * 1024 + h >= s + r * 1073741824) }'; }
if [ -s /run/broom-cache-list ]; then
  pl=$(sed -n 's/^preload \([A-Za-z0-9_-]*\) \([0-9a-f]*\) windows$/\1 \2/p' /run/broom-cache-list)
  for d in $B/pre.*; do
    [ -d "$d" ] || continue
    printf '%s\n' "$pl" | grep -qxF "$(echo "${d#$B/pre.}" | sed 's/\./ /')" || rm -rf "$d"
  done
  printf '%s\n' "$pl" | while read -r n h; do
    [ -n "$n" ] && [ "$n" != "$NAME" ] || continue
    [ "$(cat $B/img.$n/golden.sha256 2>/dev/null)" = "$h" ] && continue
    U=http://$SRV/tftp/broom-win/$n
    [ "$(wget -q -O - $U/golden.sha256 2>/dev/null)" = "$h" ] || continue   # being published: next boot
    size=$(wget -q -O - $U/golden.size 2>/dev/null); case "$size" in ''|*[!0-9]*) continue;; esac
    P=$B/pre.$n.$h; mkdir -p $P
    pre_room $size "$(fsize $P/golden.vhdx)" || { rm -rf $P; log "preload $n ($(gb $size)): not enough room on BROOMWIN -> skipped"; continue; }
    log "preloading image $n ($(gb $size)) for later..."
    ok=1
    for f in golden.vhdx base-template.vhdx child-template.vhdx child-template.off efi.tar.gz; do
      [ -f $P/$f.ok ] && continue
      ( cd $P && getfile -c -O $f $U/$f ) || { rm -f $P/$f; ( cd $P && getfile -O $f $U/$f ); } || { ok=""; break; }
      touch $P/$f.ok
    done
    if [ -z "$ok" ] || [ "$(fsize $P/golden.vhdx)" != "$size" ]; then log "preload $n: cut -> resumed next boot"; continue; fi
    rm -f $P/*.ok; rm -rf $B/img.$n
    mv $P $B/img.$n && echo "$h" > $B/img.$n/golden.sha256 && date +%s > $B/img.$n/.parked && sync && log "image $n preloaded"
  done
fi

# DataWriteGuid of the current header (highest seq) as {..}; empty if the log was not replayed.
vhdx_guid(){
  f=$1; best=0; g=""
  for o in 65536 131072; do
    [ "$(dd if=$f bs=1 skip=$o count=4 2>/dev/null)" = head ] || continue
    # SequenceNumber u64 LE → number (od -tx1 only: busybox od may lack -tu8).
    set -- $(od -An -tx1 -j $((o+8)) -N8 $f)
    [ $# -eq 8 ] || continue
    s=$((0x$8$7$6$5$4$3$2$1))
    [ "$s" -gt "$best" ] || continue
    best=$s; g=""
    [ "$(od -An -tx1 -j $((o+48)) -N16 $f | tr -d ' \n')" = 00000000000000000000000000000000 ] || continue
    set -- $(od -An -tx1 -j $((o+32)) -N16 $f)
    g="{$4$3$2$1-$6$5-$8$7-$9${10}-${11}${12}${13}${14}${15}${16}}"
  done
  echo "$g"
}
# Write ASCII string $2 as UTF-16LE into file $1 at offset $3.
patch16(){
  s=$2; fmt=""
  while [ -n "$s" ]; do c=${s%"${s#?}"}; s=${s#?}; fmt="$fmt$c\\000"; done
  printf "$fmt" | dd of=$1 bs=1 seek=$3 conv=notrunc 2>/dev/null
}

# 2. base/child state machine.
cd $B
# first.pending + base.ok while a base already exists = not written by a base build (the guest is a local admin and
# can write BROOMWIN): committing would make the reset child (parent .\base.vhdx) the base → base points at itself
# and Windows never boots again. Ignore them.
if [ -f first.pending ] && [ -f base.vhdx ]; then
  log "first.pending found next to an existing base -> ignored (not a base build)"; rm -f first.pending base.ok
fi
if [ -f first.pending ]; then
  if [ -f base.ok ]; then
    g=$(vhdx_guid child.vhdx)
    if [ -n "$g" ]; then
      mv child.vhdx base.vhdx
      # Name that broom-done set inside base = host.txt of the boot that created base.
      cp host.txt base.host 2>/dev/null
      cp lic.txt base.lic 2>/dev/null
      cp drv.txt base.drv 2>/dev/null
      rm -f first.pending base.ok
      log "base done (specialized once on this machine) -> reset mode"
    else
      log "base was not shut down cleanly -> redo first boot"; rm -f base.ok
    fi
  else
    log "first boot not finished -> redo (repeating every boot = Windows cannot write base.ok: old golden/broom-done error -> Publish again)"
  fi
fi
# Machine name (Machines table on the web, by MAC) → host.txt; broom-done sets it when creating base. Every boot
# resets to base → renaming = rebuild base (specialize once more). Unregistered machine: keep the Windows name.
if [ -n "$HOST" ]; then
  echo "$HOST" > host.txt
  if [ -f base.vhdx ] && [ "$(cat base.host 2>/dev/null)" != "$HOST" ]; then
    log "machine name -> $HOST: rebuilding base"
    rm -f base.vhdx child-local.vhdx base.host
  fi
else
  rm -f host.txt
fi
# License key (Machines page): LIC = generation from the server (a counter, never the key). Set / re-armed →
# rebuild base so broom-done fetches the key (once) while base is built. srv.txt = where broom-done asks.
echo "$SRV" > srv.txt
# Base mode (per image, Images page): broom-done waits for a technician instead of committing base right away.
if [ "$BASE" = 1 ]; then echo 1 > basemode.txt; else rm -f basemode.txt; fi
if [ -n "$LIC" ]; then
  echo "$LIC" > lic.txt
  if [ -f base.vhdx ] && [ "$(cat base.lic 2>/dev/null)" != "$LIC" ]; then
    log "license key set/re-armed: rebuilding base"
    rm -f base.vhdx child-local.vhdx base.host base.lic
  fi
else
  rm -f lic.txt
fi
# Drivers (Drivers page): send this machine's PCI/USB IDs, the server answers "name sha256" for its packages
# (hardware match / group / all machines) → kept in broom\drivers\<name>\; broom-done pnputil-installs them
# while base is built. The set present differs from base's → rebuild base. No answer → keep what is here.
# printf, NOT echo: dash/ash echo mangles backslashes (PCI\VEN_...).
hwids(){
  for d in /sys/bus/pci/devices/*; do
    [ -f $d/vendor ] && printf 'PCI\\VEN_%s&DEV_%s\n' "$(cut -c3- $d/vendor)" "$(cut -c3- $d/device)"
  done
  for d in /sys/bus/usb/devices/*; do
    [ -f $d/idVendor ] && printf 'USB\\VID_%s&PID_%s\n' "$(cat $d/idVendor)" "$(cat $d/idProduct)"
  done
}
configure_networking
hwids | tr a-f A-F | sort -u > /run/broom-hw.txt
if [ -n "$MAC" ] && wget -q -T 10 -O /run/broom-drv.txt --post-file=/run/broom-hw.txt "http://$SRV/api/drivers/for?mac=$MAC"; then
  mkdir -p drivers; : > /run/broom-drv-have.txt
  while read -r n h; do
    [ -n "$n" ] || continue
    if [ "$(cat drivers/$n.sha256 2>/dev/null)" != "$h" ] || [ ! -f drivers/$n.tar.gz ]; then
      log "driver $n: downloading"
      rm -rf drivers/$n drivers/$n.sha256 drivers/$n.tmp drivers/$n.tar.gz; mkdir -p drivers/$n.tmp
      # Checked against the server's sha256 BEFORE extracting: broom-done pnputil-installs these into base.
      if wget -q -O drivers/$n.tar.gz "http://$SRV/tftp/broom-drivers/$n.tar.gz" \
         && [ "$(sha256sum drivers/$n.tar.gz | cut -c1-64)" = "$h" ] && tar -xzf drivers/$n.tar.gz -C drivers/$n.tmp; then
        mv drivers/$n.tmp drivers/$n && echo "$h" > drivers/$n.sha256
      else
        rm -rf drivers/$n.tmp drivers/$n.tar.gz; log "driver $n: download failed or sha256 mismatch (retried next boot)"
      fi
    fi
    [ -f drivers/$n.sha256 ] && echo "$n $h" >> /run/broom-drv-have.txt
  done < /run/broom-drv.txt
  for f in drivers/*.sha256; do
    [ -f "$f" ] || continue; n=${f##*/}; n=${n%.sha256}
    grep -q "^$n " /run/broom-drv.txt || { rm -rf drivers/$n "$f" drivers/$n.tar.gz; log "driver $n: removed"; }
  done
  log "drivers: $(grep -c . /run/broom-drv.txt) package(s) for this machine, $(grep -c . /run/broom-drv-have.txt) ready"
  # Signature of the packages actually here; none → empty (a base built without drivers stays valid).
  DRV=""; [ -s /run/broom-drv-have.txt ] && DRV=$(sha256sum /run/broom-drv-have.txt | cut -c1-16)
  if [ -n "$DRV" ]; then echo "$DRV" > drv.txt; else rm -f drv.txt; fi
  if [ -f base.vhdx ] && [ "$(cat base.drv 2>/dev/null)" != "$DRV" ]; then
    log "drivers changed: rebuilding base"
    rm -f base.vhdx child-local.vhdx base.host base.lic base.drv
  fi
else
  log "drivers: server did not answer -> keeping the current ones"
fi
# Last session's writes: delete first → freed (and TRIMmed, discard mount) before the fresh child is written. Same for
# its writes on the games disk (broom-games.ps1 makes that child again once Windows is up).
# The child is rebuilt from the (checked) template every boot, with base's DataWriteGuid as its parent link — base is
# only ever opened read-only, so that GUID is still the one it had when committed. (child-local.vhdx: older layout.)
rm -f child.vhdx child-local.vhdx games-*-child.vhdx
g=""; [ -f base.vhdx ] && g=$(vhdx_guid base.vhdx)
if [ -f base.vhdx ] && [ -z "$g" ]; then
  log "base.vhdx header unreadable -> rebuilding base"; rm -f base.vhdx base.host base.lic base.drv
fi
if [ -n "$g" ]; then
  cp child-template.vhdx child.vhdx && patch16 child.vhdx "$g" "$(cat child-template.off)" || die "build child.vhdx"
  MODE=reset
else
  # The extracted driver folders broom-done installs into base are on BROOMWIN, writable by the guest → extract them
  # again from the checked archives (server's list when it answered, else the stored sha256) before base is built.
  for x in drivers/*; do [ -d "$x" ] && rm -rf "$x"; done
  for t in drivers/*.tar.gz; do
    [ -f "$t" ] || continue; n=${t##*/}; n=${n%.tar.gz}
    h=$(sed -n "s/^$n //p" /run/broom-drv.txt 2>/dev/null); [ -n "$h" ] || h=$(cat drivers/$n.sha256 2>/dev/null)
    rm -rf drivers/$n; mkdir -p drivers/$n
    if [ -n "$h" ] && [ "$(sha256sum $t | cut -c1-64)" = "$h" ] && tar -xzf $t -C drivers/$n; then :
    else rm -rf drivers/$n drivers/$n.sha256 $t; log "driver $n: archive does not match -> left out of this base"; fi
  done
  cp base-template.vhdx child.vhdx || die "build child.vhdx"
  touch first.pending; MODE="first boot (specialize, a few minutes)"
fi
cd /

# 3. ESP: bootmgr + BCD (vhd=[locate]\broom\child.vhdx) — a fresh filesystem every boot, so nothing written to the
#    ESP during a session (another loader, a changed BCD) survives it; the partition (PARTUUID) stays.
mkfs.fat -F32 -n BROOMEFI "$(part $disk 1)" >/dev/null || die "mkfs.fat ESP"
mount -t vfat "$(part $disk 1)" $E || die "mount ESP"
tar -xzf $B/efi.tar.gz -C $E || die "extract efi.tar.gz"
# Drop the fallback loader \EFI\Boot\bootx64.efi (copied by bcdboot): with it the firmware boots the SSD directly
# (default disk entry) → skips the stage → NO reset. The only way in is the stage's BootNext.
rm -rf $E/EFI/Boot
sync; umount $E

# 4. "Broom Windows" entry (SSD) comes after PXE in BootOrder: PXE (first) always runs the stage to check for a new
#    image + reset; the SSD is only for booting Windows — the stage enters it via BootNext, and if the server/PXE
#    does not answer the firmware falls through to the SSD (boots, but that session is NOT reset/updated) — unless
#    strict reset is on (Network page): then no Windows entry stays in BootOrder at all (see below).
mount -t efivarfs efivarfs /sys/firmware/efi/efivars 2>/dev/null
pu=$(blkid -s PARTUUID -o value "$(part $disk 1)")
bootnum(){ efibootmgr -v | grep "$1" | grep -i "$pu" | sed -n 's/^Boot\([0-9A-Fa-f]\{4\}\).*/\1/p' | head -1; }
num(){ bootnum "Broom Windows"; }
n=$(num)
if [ -z "$n" ]; then
  for s in $(efibootmgr | grep "Broom Windows" | sed -n 's/^Boot\([0-9A-Fa-f]\{4\}\).*/\1/p'); do efibootmgr -q -b $s -B; done
  efibootmgr -q -C -d /dev/$disk -p 1 -L "Broom Windows" -l '\EFI\Microsoft\Boot\bootmgfw.efi' || die "efibootmgr create entry"
  n=$(num)
fi
[ -n "$n" ] || die "Broom Windows entry not found"
# FORCE the order: PXE → Broom Windows → Windows Boot Manager (+ other bootmgfw entries) → other network → rest.
# Windows pulls "Windows Boot Manager" to the top on every boot → fixed here + by the BroomBootOrder task
# inside Windows, which restores broom\bootorder.txt by number. Without strict reset, Broom Windows stays in the order
# (server down still boots, that session is not reset).
all=$(efibootmgr -v)
# printf, NOT echo: dash/ash echo interprets "\b" in "\Boot\bootmgfw.efi" → no match.
nums(){ printf '%s\n' "$all" | grep -Ei "$1" | sed -n 's/^Boot\([0-9A-Fa-f]\{4\}\).*/\1/p' | tr '\n' ' '; }
line(){ printf '%s\n' "$all" | grep -i "^Boot$1"; }
wins=" $(nums 'bootmgfw\.efi') "
net=" $(nums 'MAC\(|IPv4\(|IPv6\(|PXE|Network') "
# The PXE entry = BootCurrent: the firmware booted THIS session from it (PXE → iPXE → stage), whatever its name
# ("IBA GE Slot 0100", "Realtek PXE B03", "UEFI: PXE IPv4 ..."). Unknown → every network entry goes first.
pxe=$(efibootmgr | sed -n 's/^BootCurrent: //p')
case "$wins $n " in *" $pxe "*) pxe="";; esac
[ -n "$pxe" ] && [ -n "$(line $pxe)" ] || pxe=""
order=$(efibootmgr | sed -n 's/^BootOrder: //p')
b=""; c=""; d=""; e=""; old=$IFS; IFS=,
# Every entry that boots from this SSD (its ESP's PARTUUID), whatever its name or loader file.
mine=""; [ -n "$pu" ] && mine=" $(nums "$pu") "
for x in $order; do
  case ",$n,$pxe," in *",$x,"*) continue;; esac
  case "$wins" in *" $x "*) c="${c:+$c,}$x"; continue;; esac
  # Strict reset: no way into this SSD but the stage's BootNext — any other entry on it stays out of the order too.
  [ "$STRICT" = 1 ] && case "$mine" in *" $x "*) continue;; esac
  # Other NICs / IPv6 PXE after Windows: server down → the firmware reaches Broom Windows without their timeouts.
  case "$net" in *" $x "*) if [ -n "$pxe" ]; then d="${d:+$d,}$x"; else b="${b:+$b,}$x"; fi; continue;; esac
  e="${e:+$e,}$x"
done
IFS=$old
if [ "$STRICT" = 1 ]; then
  # Strict reset (Network page): no Windows entry of this SSD in BootOrder at all — Windows is reached only through
  # this stage's BootNext. strict.txt tells the BroomBootOrder task in Windows to keep them out (+ drop the loader).
  want=$(echo "$pxe,$b,$d,$e" | sed 's/,,*/,/g; s/^,//; s/,$//')
  echo "$n,$c" | sed 's/,,*/,/g; s/,$//' > $B/strict.txt
else
  want=$(echo "$pxe,$b,$n,$c,$d,$e" | sed 's/,,*/,/g; s/^,//; s/,$//')
  rm -f $B/strict.txt
fi
if [ "$want" != "$order" ]; then
  efibootmgr -q -o "$want" && log "BootOrder forced [$want]"
fi
if [ -n "$pxe" ]; then p="Boot$pxe ($(line $pxe | cut -f1 | sed 's/^Boot[0-9A-Fa-f]*\** *//'))"
else p="unknown (no BootCurrent) -> every network entry first"; fi
log "boot order: PXE $p -> Broom Windows Boot$n"
# The order for the BroomBootOrder task in Windows (numbers, no name matching there).
echo "$want" > $B/bootorder.txt
# Stage log on BROOMWIN (last 300 lines) — readable from Windows: mountvol + type broom\stage.log.
{ echo "=== $(date '+%F %T') $MODE"; cat /run/broom-stage.log 2>/dev/null; } >> $B/stage.log
tail -n 300 $B/stage.log > $B/stage.log.t && mv $B/stage.log.t $B/stage.log
sync; umount $W
efibootmgr -q -n $n || die "efibootmgr BootNext"
restart "-> Windows ($MODE)"
