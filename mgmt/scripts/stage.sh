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
# GNU wget copied by the hook, called by path: the initramfs busybox wget has no --header / --post-file / -T
# (delta Range requests + driver list need them) and must never be picked instead.
if [ -x /broom/bin/wget ]; then wget(){ /broom/bin/wget "$@"; }
else log "WARNING: no GNU wget in the stage (publish again) -> delta updates + drivers fall back / skip"; fi
# Whole-file download showing ONLY a progress bar (file name, %, bytes, speed, ETA) — no URL / connecting / headers /
# "saved" text. GNU wget: -q hides everything, --show-progress brings the bar back; bar:force because the initramfs
# console is not always seen as a tty; noscroll keeps the name still. Busybox wget: already just its own bar.
getfile(){
  if [ -x /broom/bin/wget ]; then /broom/bin/wget -q --show-progress --progress=bar:force:noscroll "$@"
  else wget "$@"; fi
}
NAME=""; HASH=""; SRV=""; HOST=""; LIC=""; MAC=""; REG=""; BASE=""; STRICT=""
for a in $(cat /proc/cmdline); do
  case "$a" in broom.name=*) NAME=${a#*=};; broom.hash=*) HASH=${a#*=};; broom.srv=*) SRV=${a#*=};;
    broom.host=*) HOST=${a#*=};; broom.lic=*) LIC=${a#*=};; BOOTIF=01-*) MAC=$(echo "${a#BOOTIF=01-}" | tr - :);;
    broom.reg=*) REG=${a#*=};; broom.base=*) BASE=${a#*=};; broom.strict=*) STRICT=${a#*=};; esac
done
[ -n "$NAME" ] && [ -n "$HASH" ] && [ -n "$SRV" ] || die "missing broom.name/hash/srv on cmdline"
for m in ntfs3 vfat nls_cp437 nls_iso8859_1 nls_utf8 efivarfs; do modprobe $m 2>/dev/null; done
udevadm settle 2>/dev/null

# Local disk: internal only — not removable, not on USB (an external USB HDD/SSD often reports removable=0), size > 0.
part(){ case "$1" in *[0-9]) echo "/dev/${1}p$2";; *) echo "/dev/$1$2";; esac; }
lbl(){ blkid -s LABEL -o value "$1" 2>/dev/null; }
SYSB=/sys/block; CON=/dev/console
disks=""
for d in $SYSB/*; do
  n=${d##*/}
  case "$n" in loop*|ram*|dm-*|sr*|nbd*|md*|fd*|zram*) continue;; esac
  [ "$(cat $d/removable 2>/dev/null)" = 1 ] && continue
  case "$(readlink -f $d)" in */usb*) continue;; esac
  [ "$(cat $d/size 2>/dev/null || echo 0)" -gt 0 ] || continue
  disks="$disks $n"
done
# Never guess which disk to wipe: an unknown machine (not on the Machines page — anyone can PXE-boot) or one with
# several disks asks on its screen. Prints the disk typed, nothing for Enter / anything else.
ask_disk(){
  echo "broom: no Broom disk on this machine yet - one disk must be ERASED for Windows." > $CON
  [ "$REG" = 1 ] || echo "broom: this machine is NOT registered on the server (Machines page)." > $CON
  for n in "$@"; do
    printf 'broom:   %-10s %6s GB  %s\n' "$n" "$(( $(cat $SYSB/$n/size) / 2097152 ))" "$(cat $SYSB/$n/device/model 2>/dev/null)" > $CON
  done
  printf 'broom: type the disk to ERASE (e.g. %s), or press Enter to reboot without touching any disk: ' "$1" > $CON
  read -r ans < $CON
  for n in "$@"; do [ "$ans" = "$n" ] && { echo "$n"; return; }; done
}
disk=""; NEWDISK=""
for n in $disks; do [ "$(lbl $(part $n 2))" = BROOMWIN ] && { disk=$n; break; }; done
if [ -z "$disk" ]; then
  set -- $disks
  [ $# -gt 0 ] || die "no local disk found"
  if [ "$REG" = 1 ] && [ $# -eq 1 ]; then
    disk=$1
  else
    disk=$(ask_disk "$@")
    [ -n "$disk" ] || restart "no disk chosen -> nothing was touched"
  fi
  NEWDISK=1
fi
# end disk choice
if [ -n "$NEWDISK" ]; then
  log "partitioning /dev/$disk for the first time (WIPES the disk)"
  printf 'label: gpt\nsize=512MiB, type=U, name=BROOMEFI\ntype=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name=BROOMWIN, attrs="GUID:63"\n' \
    | sfdisk -q --wipe always --wipe-partitions always /dev/$disk >/dev/null || die "sfdisk /dev/$disk"
  udevadm settle 2>/dev/null
  i=0; while [ ! -b "$(part $disk 2)" ] && [ $i -lt 10 ]; do sleep 1; i=$((i+1)); done
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

# Delta golden update: turn $1 (old golden, $2 = its manifest) into the new golden ($3 = its manifest) IN PLACE,
# then rename it to $4. Only the 4 MB chunks the old copy lacks come from $5 (the server's /api/golden-chunk?name=X,
# zstd-compressed). Manifest = "size N" + one
# line per chunk (sha256 | zero). A new VHDX block early in the disk shifts later ones → those chunks exist in the
# old copy at another offset ("moved"): phase 1 saves every moved chunk (sha256-checked) to a spare dir BEFORE
# anything is overwritten; phase 2 writes moved chunks from there, downloads the missing ones, zeroes zero ones.
# Unchanged chunks are only read + checked → disk IO ≈ size + 2×moved + downloaded instead of a whole download.
# A moved chunk that fails its check is downloaded instead; interrupted → the next run re-plans from the manifests
# and the checks catch overwritten sources. Returns 1 → the caller does a full download.
# DW workers in parallel (every DW-th chunk each, own temp file, disjoint regions): keeps the link busy while
# others hash/write, and sha256 runs on several cores.
DW=4
delta(){
  old=$1; om=$2; nm=$3; new=$4; url=$5; P=/run/broom-plan; F=/run/broom-delta-fail; SP=$1.spare
  size=$(sed -n '1s/^size //p' $nm)
  case "$size" in ''|*[!0-9]*) log "delta: bad manifest"; return 1;; esac
  # $1.patching = the new manifest a previous (interrupted) run was patching towards. Same target → the same plan,
  # safe to redo. Another target → unchanged chunks can't be trusted any more → full download.
  id=$(sha256sum $nm | cut -c1-64)
  if [ -f $1.patching ] && [ "$(cat $1.patching)" != "$id" ]; then
    log "delta: old golden half-patched towards another version -> full download"; return 1
  fi
  # s = same position, c = moved (at old index, spare slot), d = download, z = zero
  awk 'NR==FNR { if (FNR>1) { o[FNR-2]=$1; if (!($1 in at)) at[$1]=FNR-2 } next }
       FNR>1 { i=FNR-2; h=$1
         if (h=="zero") print "z", i, h
         else if (o[i]==h) print "s", i, h
         else if (h in at) print "c", i, h, at[h], nc++
         else print "d", i, h }' $om $nm > $P || return 1
  s=$(grep -c '^s ' $P); c=$(grep -c '^c ' $P); d=$(grep -c '^d ' $P); z=$(grep -c '^z ' $P)
  # Mostly new (a re-installed golden, not an update) → one plain whole-file stream beats thousands of 4 MB requests
  # (and the server compressing them). Checked before anything is written → the old copy is untouched.
  all=$((s + c + d + z))
  if [ $all -gt 0 ] && [ $((d * 2)) -gt $all ]; then
    log "delta: $((d * 100 / all))% changed -> whole file instead"; rm -f $P; return 1
  fi
  # -P: one line per filesystem (busybox df wraps long device names like /dev/mapper/... otherwise).
  avail=$(df -Pk $W | tail -1 | awk '{print $4}')
  [ "$avail" -gt $((c * 4096 + 65536)) ] 2>/dev/null || { log "delta: not enough space to keep $c moved chunks"; return 1; }
  rm -rf $SP; mkdir -p $SP
  out=$old
  log "delta: $s unchanged, $c moved, $d downloaded ($((d * 4)) MB), $z zero, $DW workers"
  ok(){ [ "$(sha256sum $T | cut -c1-64)" = "$1" ]; }
  # Progress: every finished step appends a line to $P.done, every download one to $P.dl (O_APPEND: safe from
  # several workers); a reporter redraws one console line every 2 s.
  # Chunk i comes zstd-compressed from /api/golden-chunk (Windows data ≈ 55 % → less on the wire); the sha256 check
  # is on the decompressed bytes. Compressed sizes go to $P.dlz (the final log shows what really crossed the network).
  dl(){ try=0
    while [ $try -lt 3 ]; do
      if wget -q -O $T.z "$url&i=$1&h=$2" 2>/dev/null && zstd -dqf $T.z -o $T 2>/dev/null && ok $2; then
        wc -c < $T.z >> $P.dlz; rm -f $T.z; echo >> $P.dl; return 0
      fi
      try=$((try + 1))
    done; rm -f $T.z; log "delta: chunk $1 failed 3 times"; return 1; }
  put(){ dd if=$T of=$out bs=4M seek=$1 conv=notrunc 2>/dev/null; }
  # Chunk $1 already right in place (a run interrupted by a power cut wrote it) → no download (4 MB read < network).
  have(){ dd if=$out of=$T bs=4M skip=$1 count=1 2>/dev/null && ok $2; }
  zero(){ dd if=/dev/zero of=$out bs=4M seek=$1 count=1 conv=notrunc 2>/dev/null; }
  # One worker: plan lines NR % DW == $1, phase $2. A failure leaves $F (a background job can't return into delta).
  work(){
    awk -v n=$DW -v w=$1 'NR % n == w' $P | while read a i h j k; do
      [ -f $F ] && break   # another worker failed → stop early
      if [ "$2" = 1 ]; then
        # phase 1: save moved chunks (exact bytes; a bad one is dropped → downloaded in phase 2)
        [ "$a" = c ] || continue
        T=$SP/$k; dd if=$old of=$T bs=4M skip=$j count=1 2>/dev/null; ok $h || rm -f $T
      else
        T=/run/broom-chunk.$1
        case "$a" in
          # Same position: still verified (a 4 MB read) — a chunk gone bad on the SSD would otherwise survive every
          # update, since the plan only compares manifests.
          s) have $i $h || { dl $i $h || { touch $F; break; }; put $i; };;
          c) if [ -f $SP/$k ]; then T=$SP/$k; else dl $i $h || { touch $F; break; }; fi; put $i;;
          d) have $i $h || { dl $i $h || { touch $F; break; }; put $i; };;
          z) zero $i;;
        esac
      fi
      echo >> $P.done
    done
    rm -f /run/broom-chunk.$1 /run/broom-chunk.$1.z; }
  tot=$((2 * c + s + d + z))
  rm -f $F; : > $P.done; : > $P.dl; : > $P.dlz
  t0=$(date +%s); rp=""
  if [ $tot -gt 0 ]; then
    ( while :; do
        sleep 2; n=$(wc -l < $P.done); m=$(wc -l < $P.dl); t=$(( $(date +%s) - t0 )); [ $t -gt 0 ] || t=1
        eta=0; [ $n -gt 0 ] && eta=$(( (tot - n) * t / n ))
        # MB/s = chunks done (copied + downloaded) per second — the real pace, not only the network part.
        printf '\rbroom: delta %3d%%  %d/%d chunks  %d/%d MB downloaded  %d MB/s  %dm%02ds left   ' \
          $((n * 100 / tot)) $n $tot $((m * 4)) $((d * 4)) $((n * 4 / t)) $((eta / 60)) $((eta % 60))
      done ) &
    rp=$!
  fi
  # Phase 1 (every moved chunk saved) completes before phase 2 overwrites anything.
  for phase in 1 2; do
    [ $phase = 2 ] && { echo "$id" > $1.patching; sync; }
    ts=$(date +%s)
    pids=""; w=0; while [ $w -lt $DW ]; do work $w $phase & pids="$pids $!"; w=$((w + 1)); done
    wait $pids
    # Per-phase time in stage.log: 1 = reading the moved chunks (local disk), 2 = downloads + writes.
    # (a newline first: the progress line is redrawn with \r and has none)
    [ -n "$rp" ] && echo
    log "delta phase $phase done in $(( $(date +%s) - ts ))s"
  done
  [ -n "$rp" ] && { kill $rp 2>/dev/null; echo; }
  t=$(( $(date +%s) - t0 )); m=$(wc -l < $P.dl); mz=$(awk '{ s += $1 } END { print int(s / 1048576) }' $P.dlz)
  rm -rf $P.done $P.dl $P.dlz $SP
  [ -f $F ] && { rm -f $F $P; return 1; }
  [ $t -gt 0 ] || t=1
  log "delta done in ${t}s: $((tot * 4)) MB of disk work ($((tot * 4 / t)) MB/s), $((m * 4)) MB downloaded as $mz MB compressed"
  dd if=/dev/null of=$out bs=1 seek=$size 2>/dev/null   # exact size (the last chunk may be partial)
  mv $old $new && rm -f $1.patching
  rm -f $P
  return 0
}
# end delta

# 1. Golden hash mismatch → re-download (delete old base/child: they point to the old golden).
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
  # The old golden + its manifest stay as the delta source; base/child point to the old golden → gone.
  rm -f $B/golden.sha256 $B/base.vhdx $B/base.ok $B/first.pending $B/child.vhdx $B/child-local.vhdx
  mkdir -p $D
  U=http://$SRV/tftp/broom-win/$NAME
  # Delta: only the chunks the old copy lacks cross the network (see delta()). No old copy / no manifest / mostly
  # changed / any failure → drop the old golden + partial file and download the whole golden below.
  if [ ! -f $D/golden.vhdx.ok ]; then
    if wget -q -O $D/golden.chunks $U/golden.chunks && [ -f $B/golden.vhdx ] && [ -f $B/golden.chunks ] \
       && delta $B/golden.vhdx $B/golden.chunks $D/golden.chunks $D/golden.vhdx "http://$SRV/api/golden-chunk?name=$NAME"; then
      touch $D/golden.vhdx.ok $D/golden.delta
    else
      rm -rf $D/golden.vhdx $B/golden.vhdx $B/golden.chunks $B/golden.vhdx.patching $B/golden.vhdx.spare
    fi
  fi
  # $f.ok = file fully downloaded (power loss midway → next boot skips finished files, resumes the partial one).
  # Only -c -O: works with both busybox and GNU wget. -c failing (e.g. 416 when the file is complete but not yet .ok)
  # → download again from scratch. NO $((...)) on external data: an ash arithmetic error exits the whole script.
  n=0
  for f in golden.vhdx base-template.vhdx child-template.vhdx child-template.off efi.tar.gz; do
    n=$((n + 1))
    [ -f $D/$f.ok ] && continue
    # Size of the golden from the manifest (awk formats it: no shell arithmetic on server data).
    sz=""
    [ "$f" = golden.vhdx ] && sz=$(sed -n '1s/^size //p' $D/golden.chunks 2>/dev/null | awk '{ printf ", %.1f GB", $1 / 1073741824 }')
    log "downloading $f ($n/5$sz)"
    U=http://$SRV/tftp/broom-win/$NAME/$f
    # Fresh golden: sha256 WHILE downloading (tee) → no re-read of the whole file afterwards. Cut midway → the file
    # stays; -c below resumes it and the full check runs at the end as before.
    if [ "$f" = golden.vhdx ] && [ ! -s $D/$f ]; then
      h=$( (cd $D && getfile -O - $U) | tee $D/$f | sha256sum | cut -c1-64 )
      [ "$h" = "$HASH" ] && touch $D/golden.hashed
    fi
    # cd + relative -O: the bar shows the file name ("golden.vhdx"), not the long dl-<hash> path cut short.
    if [ "$f" != golden.vhdx ] || [ ! -f $D/golden.hashed ]; then
      ( cd $D && getfile -c -O $f $U ) || { rm -f $D/$f; ( cd $D && getfile -O $f $U ); } || die "download $f"
    fi
    touch $D/$f.ok
  done
  [ "$(srv_hash)" = "$HASH" ] || { rm -rf $D; restart "image $NAME changed on the server during the download -> reboot to get the new version"; }
  # Delta: every chunk was checked against the manifest; fresh download: hashed while downloading → no second full
  # read. Otherwise (resumed download) → whole-file sha256.
  if [ ! -f $D/golden.delta ] && [ ! -f $D/golden.hashed ]; then
    log "checking golden sha256 - rereads the whole file, may take a few minutes, DO NOT power off..."
    [ "$(sha256sum $D/golden.vhdx | cut -d' ' -f1)" = "$HASH" ] || { rm -rf $D; die "golden sha256 mismatch"; }
  fi
  rm -f $D/*.ok $D/golden.delta $D/golden.hashed
  # golden.chunks (if fetched) moves along → the manifest of the copy we now have = next delta's source.
  mv $D/* $B/ && rmdir $D && sync && echo "$HASH" > $B/golden.sha256 && sync
fi

# 1b. The small boot files (EFI bundle, VHDX templates) are checked against the server's sha256 on EVERY boot: the
# guest is a local admin and could swap them on BROOMWIN (e.g. a child template with its own data) to outlive the
# reset. Wrong or missing → downloaded again.
configure_networking
if wget -q -O /run/broom-files.sha256 http://$SRV/tftp/broom-win/$NAME/files.sha256 2>/dev/null && [ -s /run/broom-files.sha256 ]; then
  while read -r h f; do
    case "$f" in efi.tar.gz|child-template.vhdx|child-template.off|base-template.vhdx) ;; *) continue;; esac
    [ "$(sha256sum $B/$f 2>/dev/null | cut -c1-64)" = "$h" ] && continue
    log "$f differs from the server's -> downloading it again"
    if wget -q -O $B/$f.tmp http://$SRV/tftp/broom-win/$NAME/$f && [ "$(sha256sum $B/$f.tmp | cut -c1-64)" = "$h" ]; then
      mv $B/$f.tmp $B/$f
    else
      rm -f $B/$f.tmp; restart "could not get a good $f (publish running?) -> retrying"
    fi
  done < /run/broom-files.sha256
else
  log "no files.sha256 on the server for $NAME (published by an older version) -> Publish again to check the boot files"
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
    if [ "$(cat drivers/$n.sha256 2>/dev/null)" != "$h" ]; then
      log "driver $n: downloading"
      rm -rf drivers/$n drivers/$n.sha256 drivers/$n.tmp drivers/$n.tar.gz; mkdir -p drivers/$n.tmp
      # Checked against the server's sha256 BEFORE extracting: broom-done pnputil-installs these into base.
      if wget -q -O drivers/$n.tar.gz "http://$SRV/tftp/broom-drivers/$n.tar.gz" \
         && [ "$(sha256sum drivers/$n.tar.gz | cut -c1-64)" = "$h" ] && tar -xzf drivers/$n.tar.gz -C drivers/$n.tmp; then
        mv drivers/$n.tmp drivers/$n && echo "$h" > drivers/$n.sha256
      else
        rm -rf drivers/$n.tmp; log "driver $n: download failed or sha256 mismatch (retried next boot)"
      fi
      rm -f drivers/$n.tar.gz
    fi
    [ -f drivers/$n.sha256 ] && echo "$n $h" >> /run/broom-drv-have.txt
  done < /run/broom-drv.txt
  for f in drivers/*.sha256; do
    [ -f "$f" ] || continue; n=${f##*/}; n=${n%.sha256}
    grep -q "^$n " /run/broom-drv.txt || { rm -rf drivers/$n "$f"; log "driver $n: removed"; }
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
# Last session's writes: delete first → freed (and TRIMmed, discard mount) before the fresh child is written.
# The child is rebuilt from the (checked) template every boot, with base's DataWriteGuid as its parent link — base is
# only ever opened read-only, so that GUID is still the one it had when committed. (child-local.vhdx: older layout.)
rm -f child.vhdx child-local.vhdx
g=""; [ -f base.vhdx ] && g=$(vhdx_guid base.vhdx)
if [ -f base.vhdx ] && [ -z "$g" ]; then
  log "base.vhdx header unreadable -> rebuilding base"; rm -f base.vhdx base.host base.lic base.drv
fi
if [ -n "$g" ]; then
  cp child-template.vhdx child.vhdx && patch16 child.vhdx "$g" "$(cat child-template.off)" || die "build child.vhdx"
  MODE=reset
else
  cp base-template.vhdx child.vhdx; touch first.pending; MODE="first boot (specialize, a few minutes)"
fi
cd /

# 3. ESP: bootmgr + BCD (vhd=[locate]\broom\child.vhdx) — copied again every boot.
mount -t vfat "$(part $disk 1)" $E || die "mount ESP"
rm -rf $E/EFI/Microsoft
tar -xzf $B/efi.tar.gz -C $E || die "extract efi.tar.gz"
# Drop the fallback loader \EFI\Boot\bootx64.efi (copied by bcdboot): with it the firmware boots the SSD directly
# (default disk entry) → skips the stage → NO reset. The only way in is the stage's BootNext.
rm -rf $E/EFI/Boot
sync; umount $E

# 4. "Broom Windows" entry (SSD) comes after PXE in BootOrder: PXE (first) always runs the stage to check for a new
#    image + reset; the SSD is only for booting Windows — the stage enters it via BootNext, and if the server/PXE
#    does not answer the firmware falls through to the SSD (boots, but that session is NOT reset/updated).
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
# inside Windows, which restores broom\bootorder.txt by number. Broom Windows always stays in the order
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
for x in $order; do
  case ",$n,$pxe," in *",$x,"*) continue;; esac
  case "$wins" in *" $x "*) c="${c:+$c,}$x"; continue;; esac
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
