// winstage.rs — Windows diskless, design B: native VHDX boot from the client SSD.
//
// Flow: golden = Windows Pro VM that ran /broom-prep-win (tweaks + EFI bundle + unattend), then sysprep →
// upload .vmdk → publish(): extract the Windows partition → golden.vhdx + efi.tar.gz + 2 empty child VHDX
// (vhdx.rs) → build stage (server kernel + initrd) → boot_script.
//
// Every client boot: iPXE → STAGE (Linux, no root fs) on the SSD:
//   p1 ESP BROOMEFI, p2 NTFS BROOMWIN\broom\: golden.vhdx ← base.vhdx (golden specialized on THIS
//   machine, created once) ← child.vhdx (reset every boot). Hash mismatch → re-download golden over HTTP.
//   First boot (no base yet): child = base-template (parent golden) → Windows specialize/OOBE/first logon
//   write into it → broom-done.ps1 writes base.ok + reboots → stage renames child→base, patches the GUID
//   into child-template (parent base) → from then on each boot only copies child-local → child (instant).
//   Done → efibootmgr BootNext "Broom Windows" → reboot → Windows boots the child from the SSD.
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use crate::db::Db;
use crate::{images_dir, SharedState};

/// Windows stage (kernel + initrd) served at /tftp/broom-stage/.
fn stage_dir() -> String {
    crate::tftp_dir().join("broom-stage").to_string_lossy().into_owned()
}

fn run(bin: &str, args: &[&str]) -> Result<String, String> {
    tracing::debug!("exec: {bin} {}", args.join(" "));
    let o = Command::new(bin).args(args).output().map_err(|e| format!("{bin}: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        Err(format!("{bin} {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()))
    }
}

fn write_exec(path: &str, body: &str) -> Result<(), String> {
    std::fs::write(path, body).map_err(|e| format!("{path}: {e}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

/// mkinitramfs hook for the stage: tools + modules for partitioning / NTFS / download / EFI.
/// Copied to /broom/bin (ahead of busybox in PATH — needs the full od/tar/wget...).
const STAGE_HOOK: &str = r#"#!/bin/sh
PREREQ=""
prereqs(){ echo "$PREREQ"; }
case $1 in prereqs) prereqs; exit 0;; esac
. /usr/share/initramfs-tools/hook-functions
for b in __TOOLS__; do
  p=$(command -v $b) && copy_exec "$p" /broom/bin/$b
done
manual_add_modules ntfs3 vfat nls_cp437 nls_iso8859_1 nls_utf8 efivarfs
"#;

/// Server tools the hook copies into the stage (/broom/bin, ahead of busybox).
const STAGE_TOOLS: &str = "sfdisk blkid mkfs.fat mkntfs ntfsfix efibootmgr wget sha256sum tar gzip od dd awk zstd";

/// Stage init-premount script — runs on the client, does NOT mount root; reboots into Windows when done.
const STAGE_SCRIPT: &str = r#"#!/bin/sh
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
NAME=""; HASH=""; SRV=""; HOST=""; LIC=""; MAC=""
for a in $(cat /proc/cmdline); do
  case "$a" in broom.name=*) NAME=${a#*=};; broom.hash=*) HASH=${a#*=};; broom.srv=*) SRV=${a#*=};;
    broom.host=*) HOST=${a#*=};; broom.lic=*) LIC=${a#*=};; BOOTIF=01-*) MAC=$(echo "${a#BOOTIF=01-}" | tr - :);; esac
done
[ -n "$NAME" ] && [ -n "$HASH" ] && [ -n "$SRV" ] || die "missing broom.name/hash/srv on cmdline"
for m in ntfs3 vfat nls_cp437 nls_iso8859_1 nls_utf8 efivarfs; do modprobe $m 2>/dev/null; done
udevadm settle 2>/dev/null

# Local SSD: not removable, size > 0.
part(){ case "$1" in *[0-9]) echo "/dev/${1}p$2";; *) echo "/dev/$1$2";; esac; }
lbl(){ blkid -s LABEL -o value "$1" 2>/dev/null; }
disks=""
for d in /sys/block/*; do
  n=${d##*/}
  case "$n" in loop*|ram*|dm-*|sr*|nbd*|md*|fd*|zram*) continue;; esac
  [ "$(cat $d/removable 2>/dev/null)" = 1 ] && continue
  [ "$(cat $d/size 2>/dev/null || echo 0)" -gt 0 ] || continue
  disks="$disks $n"
done
disk=""
for n in $disks; do [ "$(lbl $(part $n 2))" = BROOMWIN ] && { disk=$n; break; }; done
if [ -z "$disk" ]; then
  set -- $disks; disk=$1
  [ -n "$disk" ] || die "no local SSD found"
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
# Unchanged chunks are never touched → disk IO ≈ 2×moved + downloaded instead of copying the whole golden.
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
  avail=$(df -k $W | tail -1 | awk '{print $4}')
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
          s) continue;;
          c) if [ -f $SP/$k ]; then T=$SP/$k; else dl $i $h || { touch $F; break; }; fi; put $i;;
          d) dl $i $h || { touch $F; break; }; put $i;;
          z) zero $i;;
        esac
      fi
      echo >> $P.done
    done
    rm -f /run/broom-chunk.$1 /run/broom-chunk.$1.z; }
  tot=$((2 * c + d + z))
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
  # Delta: only the chunks the old copy lacks cross the network (see delta()). No old copy / no manifest /
  # any failure → drop the old golden + partial file and download the whole golden below.
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
  for f in golden.vhdx base-template.vhdx child-template.vhdx child-template.off efi.tar.gz; do
    [ -f $D/$f.ok ] && continue
    log "downloading $f"
    U=http://$SRV/tftp/broom-win/$NAME/$f
    wget -c -O $D/$f $U || { rm -f $D/$f; wget -O $D/$f $U; } || die "download $f"
    touch $D/$f.ok
  done
  [ "$(srv_hash)" = "$HASH" ] || { rm -rf $D; restart "image $NAME changed on the server during the download -> reboot to get the new version"; }
  # Delta: every chunk was checked against the manifest → no second full read. Full download → whole-file sha256.
  if [ ! -f $D/golden.delta ]; then
    log "checking golden sha256 - rereads the whole file, may take a few minutes, DO NOT power off..."
    [ "$(sha256sum $D/golden.vhdx | cut -d' ' -f1)" = "$HASH" ] || { rm -rf $D; die "golden sha256 mismatch"; }
  fi
  rm -f $D/*.ok $D/golden.delta
  # golden.chunks (if fetched) moves along → the manifest of the copy we now have = next delta's source.
  mv $D/* $B/ && rmdir $D && sync && echo "$HASH" > $B/golden.sha256 && sync
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
      cp child-template.vhdx child-local.vhdx
      patch16 child-local.vhdx "$g" "$(cat child-template.off)"
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
      rm -rf drivers/$n drivers/$n.sha256 drivers/$n.tmp; mkdir -p drivers/$n.tmp
      if wget -q -O - "http://$SRV/tftp/broom-drivers/$n.tar.gz" | tar -xzf - -C drivers/$n.tmp; then
        mv drivers/$n.tmp drivers/$n && echo "$h" > drivers/$n.sha256
      else
        rm -rf drivers/$n.tmp; log "driver $n: download failed (retried next boot)"
      fi
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
rm -f child.vhdx
if [ -f base.vhdx ] && [ -f child-local.vhdx ]; then
  cp child-local.vhdx child.vhdx; MODE=reset
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
want=$(echo "$pxe,$b,$n,$c,$d,$e" | sed 's/,,*/,/g; s/^,//; s/,$//')
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
"#;

/// Build the stage: the server's running kernel + initrd (mkinitramfs, own confdir) → stage_dir().
/// Kernel + script + hook unchanged → keep the previous build (mkinitramfs MODULES=most takes about a minute).
/// Returns true if freshly built.
pub fn build_stage() -> Result<bool, String> {
    use std::hash::{Hash, Hasher};
    let kv = std::fs::read_to_string("/proc/sys/kernel/osrelease").map_err(|e| format!("kernel release: {e}"))?;
    let kv = kv.trim().to_string();
    run("modinfo", &["ntfs3"]).map_err(|_| format!("server kernel {kv} has no ntfs3 module — the stage must write NTFS"))?;
    // zstd: multithreaded compression + faster decompression than gzip; server without zstd → gzip.
    let compress = if run("sh", &["-c", "command -v zstd"]).is_ok() { "zstd" } else { "gzip" };
    let initramfs_conf = format!("MODULES=most\nBUSYBOX=y\nCOMPRESS={compress}\n");
    let hook = STAGE_HOOK.replace("__TOOLS__", STAGE_TOOLS);
    // Where each tool resolves on the server is part of the key: a tool installed later (e.g. wget) → rebuilt.
    let tools = run("sh", &["-c", &format!("for b in {STAGE_TOOLS}; do command -v $b; done; true")]).unwrap_or_default();
    let key = {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (&kv, &initramfs_conf, &hook, STAGE_SCRIPT, &tools).hash(&mut h);
        format!("{:016x}", h.finish())
    };
    let sd = stage_dir();
    let key_file = format!("{sd}/stage.key");
    let have = |f: &str| Path::new(&format!("{sd}/{f}")).exists();
    if have("stage.img") && have("vmlinuz") && std::fs::read_to_string(&key_file).ok().as_deref() == Some(key.as_str()) {
        return Ok(false);
    }
    let conf = &crate::work_dir().join("stage-conf").to_string_lossy().into_owned();
    let _ = std::fs::remove_dir_all(conf);
    for d in ["scripts/init-premount", "hooks", "conf.d"] {
        std::fs::create_dir_all(format!("{conf}/{d}")).map_err(|e| e.to_string())?;
    }
    std::fs::write(format!("{conf}/initramfs.conf"), &initramfs_conf).map_err(|e| e.to_string())?;
    std::fs::write(format!("{conf}/modules"), "").map_err(|e| e.to_string())?;
    write_exec(&format!("{conf}/hooks/broom-stage"), &hook)?;
    write_exec(&format!("{conf}/scripts/init-premount/broom-stage"), STAGE_SCRIPT)?;
    std::fs::create_dir_all(&sd).map_err(|e| e.to_string())?;
    let tmp = format!("{sd}/stage.img.tmp");
    run("mkinitramfs", &["-d", conf, "-o", &tmp, &kv])?;
    // Tools WITHOUT a busybox replacement must really be in the initrd — report missing ones now
    // at publish time, not when a client gets stuck in a shell.
    let list = run("lsinitramfs", &[&tmp])?;
    // wget: busybox's (initramfs build) lacks --header / --post-file → delta updates + drivers need GNU wget.
    // zstd: delta chunks arrive compressed.
    let missing: Vec<&str> = ["sfdisk", "mkfs.fat", "mkntfs", "ntfsfix", "efibootmgr", "awk", "wget", "zstd"]
        .into_iter()
        .filter(|b| !list.lines().any(|l| l.ends_with(&format!("broom/bin/{b}"))))
        .collect();
    if !missing.is_empty() {
        return Err(format!("stage initrd is missing {} — install the packages on the server (fdisk ntfs-3g dosfstools efibootmgr wget mawk zstd) then Publish again", missing.join(", ")));
    }
    std::fs::rename(&tmp, format!("{sd}/stage.img")).map_err(|e| e.to_string())?;
    std::fs::copy(format!("/boot/vmlinuz-{kv}"), format!("{sd}/vmlinuz"))
        .map_err(|e| format!("copy /boot/vmlinuz-{kv}: {e}"))?;
    let _ = std::fs::remove_dir_all(conf);
    std::fs::write(&key_file, &key).map_err(|e| e.to_string())?;
    Ok(true)
}

/// NTFS/basic-data partitions, LARGEST first. (start, size) in bytes.
fn ntfs_parts(sfdisk_json: &str) -> Result<Vec<(u64, u64)>, String> {
    let v: serde_json::Value = serde_json::from_str(sfdisk_json).map_err(|e| e.to_string())?;
    let t = &v["partitiontable"];
    let ss = t["sectorsize"].as_u64().unwrap_or(512);
    let mut parts: Vec<(u64, u64)> = t["partitions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| {
            let ty = p["type"].as_str().unwrap_or("").to_ascii_uppercase();
            ty == "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" || ty == "7"
        })
        .filter_map(|p| Some((p["start"].as_u64()? * ss, p["size"].as_u64()? * ss)))
        .collect();
    parts.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(parts)
}

/// Mount partition [start, start+size) of a raw file (loop), run f(mnt), always unmount + detach the loop.
fn with_part<T>(
    raw: &str,
    (start, size): (u64, u64),
    mnt: &str,
    ro: bool,
    f: impl FnOnce(&str) -> Result<T, String>,
) -> Result<T, String> {
    let (o, s) = (start.to_string(), size.to_string());
    let mut args = vec!["-f", "--show"];
    if ro {
        args.push("-r");
    }
    args.extend(["-o", o.as_str(), "--sizelimit", s.as_str(), raw]);
    let dev = run("losetup", &args)?;
    let _ = std::fs::create_dir_all(mnt);
    // Mount left over from an interrupted publish (mgmt restarted midway) → unmount everything first.
    while run("umount", &[mnt]).is_ok() {}
    let opt = if ro { "ro" } else { "rw" };
    let r = run("mount", &["-t", "ntfs-3g", "-o", opt, &dev, mnt])
        .or_else(|_| run("mount", &["-t", "ntfs3", "-o", opt, &dev, mnt]))
        .and_then(|_| {
            let r = f(mnt);
            let _ = run("umount", &[mnt]);
            r
        });
    let _ = run("losetup", &["-d", &dev]);
    r
}

/// The partition that CONTAINS Windows (has the SYSTEM hive) — mounts each NTFS partition read-only, largest first.
/// No guessing by size: picking the wrong one would let the later in-place edits wreck image.img.
fn find_windows(raw: &str, mnt: &str) -> Result<(u64, u64), String> {
    let parts = ntfs_parts(&run("sfdisk", &["-J", raw])?)?;
    let mut seen = Vec::new();
    for p in &parts {
        let probe = with_part(raw, *p, mnt, true, |m| {
            let hive = Path::new(&format!("{m}/Windows/System32/config/SYSTEM")).exists();
            let top: Vec<String> = std::fs::read_dir(m)
                .map(|rd| rd.flatten().take(12).map(|e| e.file_name().to_string_lossy().to_string()).collect())
                .unwrap_or_default();
            Ok((hive, top))
        });
        match probe {
            Ok((true, _)) => return Ok(*p),
            Ok((false, top)) => seen.push(format!("{}GB [{}]", p.1 >> 30, top.join(", "))),
            Err(e) => seen.push(format!("{}GB (mount error: {e})", p.1 >> 30)),
        }
    }
    Err(format!(
        "golden has no partition containing Windows (\\Windows\\System32\\config\\SYSTEM) — was the right VM/disk uploaded? NTFS partitions: {}",
        if seen.is_empty() { "none".to_string() } else { seen.join(" | ") }
    ))
}

/// Publish a Windows image from images/<name>/image.img (raw whole VM disk). Blocking.
pub fn publish(st: &SharedState, id: i64, name: &str, steps: &mut crate::publish::Steps) -> Result<String, String> {
    let raw = images_dir().join(name).join("image.img");
    let raw = std::fs::canonicalize(&raw).map_err(|e| format!("no golden raw yet ({}): {e}", raw.display()))?;
    let raw = raw.to_string_lossy().to_string();
    let out = crate::tftp_dir().join("broom-win").join(name).to_string_lossy().into_owned();
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let golden = format!("{out}/golden.vhdx");

    // golden.vhdx gets a new GUID on every convert → new hash → every client re-downloads. Golden still newer than
    // image.img → keep it, only rebuild stage + boot_script (Publish after changing the stage = cheap).
    // ponytail: changing the extract/registry logic needs a rebuild → upload again or `touch image.img`.
    let fresh = golden_fresh(&raw, &out);
    let drivers = if fresh {
        "kept (golden already built from image.img + the current embedded logic)".to_string()
    } else {
        build_golden(&raw, &out, name, steps)?
    };

    // Hash (clients compare it to know whether to re-download) + golden.chunks (the stage fetches only the chunks it
    // lacks), one read pass. Golden kept + both present → reuse (13 GB takes ~2 minutes on a slow disk).
    // golden.sha256 is written LAST: the stage treats its absence as "publish running".
    let sum_file = format!("{out}/golden.sha256");
    let cached = std::fs::read_to_string(&sum_file).ok().map(|s| s.trim().to_string()).filter(|s| s.len() == 64);
    let hash = match cached {
        Some(h) if fresh && Path::new(&format!("{out}/golden.chunks")).exists() => h,
        _ => {
            steps.go("sha256 + chunk manifest golden");
            let h = crate::publish::write_manifest(Path::new(&golden), Path::new(&out))?;
            std::fs::write(&sum_file, &h).map_err(|e| e.to_string())?;
            h
        }
    };
    steps.go("initrd stage");
    let stage = if build_stage()? { "rebuilt" } else { "kept" };
    let ip = st.db.get_config("dhcp_server_ip", "");
    if ip.is_empty() {
        return Err("dhcp_server_ip is empty — run `setup` first".into());
    }
    let bs = format!(
        "kernel http://{ip}/tftp/broom-stage/vmlinuz initrd=stage.img ip=dhcp BOOTIF=01-${{mac:hexhyp}} broom.name={name} broom.hash={hash} broom.srv={ip} broom.host=${{broom-host}} broom.lic=${{broom-lic}}\n\
         initrd http://{ip}/tftp/broom-stage/stage.img\n\
         boot"
    );
    st.db.set_published(id, &bs, &hash)?;
    Ok(format!(
        "Publish OK — Windows '{name}': golden.vhdx + EFI + child templates; stage {stage}; boot-start disk drivers: {drivers}"
    ))
}

/// All 5 output files exist AND golden.vhdx is newer than image.img (not re-uploaded since the last build).
fn golden_fresh(raw: &str, out: &str) -> bool {
    let mtime = |p: &str| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let files = ["golden.vhdx", "base-template.vhdx", "child-template.vhdx", "child-template.off", "efi.tar.gz"];
    files.iter().all(|f| Path::new(&format!("{out}/{f}")).exists())
        && matches!((mtime(&format!("{out}/golden.vhdx")), mtime(raw)), (Some(g), Some(r)) if g > r)
        && std::fs::read_to_string(format!("{out}/golden.key")).ok().as_deref() == Some(golden_key().as_str())
}

/// Version of the mgmt parts EMBEDDED in the golden (broom-* scripts, disk drivers, unattend patch). Changing
/// that code → key changes → the next publish rebuilds the golden BY ITSELF (no manual `touch image.img`; an old
/// golden keeps old scripts = out of sync with the new stage, e.g. old broom-done couldn't write base.ok → OOBE loop).
fn golden_key() -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (BROOM_DONE, BROOM_BOOTORDER, BOOT_STORAGE, "skip-oobe-v1").hash(&mut h);
    format!("{:016x}", h.finish())
}

/// image.img (raw whole VM disk) → golden.vhdx + efi.tar.gz + 2 empty child VHDX. Returns the enabled drivers.
fn build_golden(raw: &str, out: &str, name: &str, steps: &mut crate::publish::Steps) -> Result<String, String> {
    let mnt = crate::work_dir().join(format!("mnt-{name}")).to_string_lossy().into_owned();
    // 1. The partition holding Windows — check the hive (read-only) BEFORE any write.
    steps.go("find Windows partition");
    let (start, size) = find_windows(raw, &mnt)?;

    // No golden.sha256 while the files below change: a client downloading now sees "publish running" (the stage
    // compares it to its own hash before + after downloading) instead of mixing files of two versions.
    let _ = std::fs::remove_file(format!("{out}/golden.sha256"));
    // Compressed delta chunks of older goldens are useless now (content-addressed: only space is at stake).
    let _ = std::fs::remove_dir_all(crate::publish::chunk_cache_dir());

    // 2. Edit image.img IN PLACE (no temporary full-disk copy): punch holes outside the Windows partition (ESP/
    //    MSR/Recovery don't go into the golden) + GPT with a single partition (standard native VHD boot), KEEP start →
    //    NTFS "hidden sectors" still match. Re-running gives the same result (re-publishing is safe).
    steps.go("trim disk");
    const MB: u64 = 1024 * 1024;
    let total = std::fs::metadata(raw).map_err(|e| e.to_string())?.len();
    let end = start + size;
    // Keep the first 1MB (primary GPT) + the last 1MB (backup GPT).
    for (off, len) in [(MB, start.saturating_sub(MB)), (end, total.saturating_sub(MB).saturating_sub(end))] {
        if len > 0 {
            punch_hole(raw, off, len)?;
        }
    }
    let layout = format!(
        "label: gpt\nstart={}, size={}, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\n",
        start / 512,
        size / 512
    );
    // --wipe* never: do NOT erase the NTFS signature of the Windows partition whose data we keep.
    let sh = format!(
        "printf '{}' | sfdisk -q --wipe never --wipe-partitions never '{raw}'",
        layout.replace('\n', "\\n")
    );
    run("sh", &["-c", &sh]).map_err(|e| format!("sfdisk golden single partition: {e}"))?;

    // 3. Mount read-write: enable boot-start disk drivers + silent OOBE + new broom-done + take the EFI bundle.
    steps.go("registry + EFI");
    let drivers = with_part(raw, (start, size), &mnt, false, |m| {
        let mut drv = enable_boot_storage(m)?;
        if silent_oobe(m)? {
            drv.push_str("; OOBE runs silently (SkipMachineOOBE)");
        }
        if write_broom_done(m)? {
            drv.push_str("; new broom-done.ps1");
        }
        if !Path::new(&format!("{m}/broom/efi/EFI/Microsoft/Boot/BCD")).exists() {
            return Err("golden is missing C:\\broom\\efi\\EFI\\Microsoft\\Boot\\BCD — run broom-prep-win in the VM before sysprep".into());
        }
        run("tar", &["-czf", &format!("{out}/efi.tar.gz"), "-C", &format!("{m}/broom/efi"), "EFI"])?;
        Ok(drv)
    })?;

    // 4. golden.vhdx (dynamic, 16 I/O threads) + 2 empty child VHDX: base-template (parent golden) and
    //    child-template (parent base.vhdx — base's GUID is only known on the client → the stage patches it at an offset).
    steps.go("convert raw→vhdx");
    let golden = format!("{out}/golden.vhdx");
    run("qemu-img", &["convert", "-m", "16", "-O", "vhdx", "-o", "subformat=dynamic", raw, &format!("{golden}.tmp")])?;
    std::fs::rename(format!("{golden}.tmp"), &golden).map_err(|e| e.to_string())?;
    let gi = crate::vhdx::read_info(&golden)?;
    crate::vhdx::write_empty(&format!("{out}/base-template.vhdx"), &gi, Some(".\\golden.vhdx"))?;
    let placeholder = crate::vhdx::Info { data_write_guid: [0; 16], ..gi };
    let off = crate::vhdx::write_empty(&format!("{out}/child-template.vhdx"), &placeholder, Some(".\\base.vhdx"))?;
    std::fs::write(format!("{out}/child-template.off"), off.to_string()).map_err(|e| e.to_string())?;
    std::fs::write(format!("{out}/golden.key"), golden_key()).map_err(|e| e.to_string())?;
    Ok(drivers)
}

/// Free [off, off+len) of a file without changing its size (fallocate PUNCH_HOLE|KEEP_SIZE; replaces
/// the util-linux `fallocate` binary). Reads there return zeros.
pub(crate) fn punch_hole(path: &str, off: u64, len: u64) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new().write(true).open(path).map_err(|e| format!("{path}: {e}"))?;
    // SAFETY: valid open fd; offsets are plain integers.
    let r = unsafe {
        libc::fallocate(f.as_raw_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, off as i64, len as i64)
    };
    if r != 0 {
        return Err(format!("punch hole {path} @{off}+{len}: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Insert SkipMachineOOBE/SkipUserOOBE into the <OOBE> block (if missing). None = no OOBE block.
fn add_skip_oobe(xml: &str) -> Option<String> {
    if xml.contains("SkipMachineOOBE") {
        return Some(xml.to_string());
    }
    let i = xml.find("<OOBE>")? + "<OOBE>".len();
    Some(format!(
        "{}\n        <SkipMachineOOBE>true</SkipMachineOOBE>\n        <SkipUserOOBE>true</SkipUserOOBE>{}",
        &xml[..i],
        &xml[i..]
    ))
}

/// Sysprep copies /unattend to C:\Windows\Panther\unattend.xml — Setup reads that file during
/// specialize/oobeSystem. Patching it there → OOBE shows no page at all (not even the network page),
/// and an old golden doesn't need prep again. Returns true if patched. File name matched case-insensitively.
fn silent_oobe(mnt: &str) -> Result<bool, String> {
    let dir = format!("{mnt}/Windows/Panther");
    let Some(p) = std::fs::read_dir(&dir).ok().and_then(|rd| {
        rd.flatten()
            .map(|e| e.path())
            .find(|p| p.file_name().map_or(false, |n| n.to_string_lossy().eq_ignore_ascii_case("unattend.xml")))
    }) else {
        return Ok(false);
    };
    let xml = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    match add_skip_oobe(&xml) {
        Some(new) if new != xml => {
            std::fs::write(&p, new).map_err(|e| format!("{}: {e}", p.display()))?;
            Ok(true)
        }
        Some(_) => Ok(true),
        None => Ok(false),
    }
}

/// Disk controller drivers shipped with Windows 10/11 (AHCI, NVMe, Intel RST, LSI/Broadcom, AMD,
/// VMware PVSCSI…). The golden only loads the golden VM's controller driver at boot → a machine with another
/// controller = INACCESSIBLE_BOOT_DEVICE (the kernel can't read the SSD holding the VHDX). Enable all of them.
const BOOT_STORAGE: &[&str] = &[
    "storahci", "stornvme", "iaStorAVC", "iaStorV", "LSI_SAS", "LSI_SAS2i", "LSI_SAS3i", "LSI_SSS",
    "megasas", "megasas2i", "megasas35i", "percsas2i", "percsas3i", "SmartSAMD", "arcsas", "ItSas35i",
    "amdsata", "amdsbs", "amdxata", "nvraid", "nvstor", "pvscsi",
];

/// Edit the SYSTEM hive offline (hivexregedit): Start=0 + StartOverride\0=0 for drivers in
/// BOOT_STORAGE that EXIST in the image (no empty service keys created). Returns the enabled list.
/// ponytail: ControlSet001 only (a sysprepped image always uses set 1).
fn enable_boot_storage(mnt: &str) -> Result<String, String> {
    let hive = format!("{mnt}/Windows/System32/config/SYSTEM");
    if !Path::new(&hive).exists() {
        // Include the root listing of the mounted partition to tell a wrong/empty mount from a case mismatch.
        let top: Vec<String> = std::fs::read_dir(mnt)
            .map(|rd| rd.flatten().take(20).map(|e| e.file_name().to_string_lossy().to_string()).collect())
            .unwrap_or_default();
        return Err(format!("hive {hive} not found — partition root: [{}]", top.join(", ")));
    }
    let on: Vec<&str> = BOOT_STORAGE
        .iter()
        .copied()
        .filter(|d| run("hivexregedit", &["--export", &hive, &format!("\\ControlSet001\\Services\\{d}")]).is_ok())
        .collect();
    let mut reg = String::from("Windows Registry Editor Version 5.00\n\n");
    for d in &on {
        let k = format!("HKEY_LOCAL_MACHINE\\SYSTEM\\ControlSet001\\Services\\{d}");
        reg.push_str(&format!("[{k}]\n\"Start\"=dword:00000000\n\n[{k}\\StartOverride]\n\"0\"=dword:00000000\n\n"));
    }
    let file = format!("{mnt}.reg");
    std::fs::write(&file, reg).map_err(|e| e.to_string())?;
    let r = run("hivexregedit", &["--merge", "--prefix", "HKEY_LOCAL_MACHINE\\SYSTEM", &hive, &file]);
    let _ = std::fs::remove_file(&file);
    r.map_err(|e| format!("disk driver registry edit: {e}"))?;
    Ok(on.join(","))
}

/// XML escape for values embedded in unattend.
fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Script run INSIDE the golden Windows VM (PowerShell Admin, preferably in Audit Mode: Ctrl+Shift+F3 at OOBE).
/// Usage: irm http://<server>/broom-prep-win | iex
/// Diskless tweaks → EFI bundle C:\broom\efi → unattend (guest user + autologon + skip OOBE) +
/// broom-done.ps1 (first logon: write base.ok to BROOMWIN + reboot) → sysprep /generalize → power off the VM.
const PREP_WIN: &str = r#"$ErrorActionPreference = 'Stop'
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not ([Security.Principal.WindowsPrincipal]$id).IsInRole('Administrators')) { throw 'Run PowerShell as Administrator' }

# 1. Native VHD boot: do NOT expand the VHDX to full size; no automatic device encryption.
reg add HKLM\SYSTEM\CurrentControlSet\Services\FsDepends\Parameters /v VirtualDiskExpandOnMount /t REG_DWORD /d 4 /f | Out-Null
reg add HKLM\SYSTEM\CurrentControlSet\Control\BitLocker /v PreventDeviceEncryption /t REG_DWORD /d 1 /f | Out-Null
# 2. Reset every boot → turn off pointless writes.
powercfg /h off
Disable-ComputerRestore -Drive "$env:SystemDrive\" -ErrorAction SilentlyContinue
Disable-ScheduledTask -TaskPath '\Microsoft\Windows\Defrag\' -TaskName ScheduledDefrag -ErrorAction SilentlyContinue | Out-Null
Set-Service WSearch -StartupType Disabled -ErrorAction SilentlyContinue
reg add HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU /v NoAutoUpdate /t REG_DWORD /d 1 /f | Out-Null
$cs = Get-CimInstance Win32_ComputerSystem
if ($cs.AutomaticManagedPagefile -or (Get-CimInstance Win32_PageFileSetting)) {
  Set-CimInstance $cs -Property @{AutomaticManagedPagefile = $false}
  Get-CimInstance Win32_PageFileSetting | Remove-CimInstance
}
if (Get-CimInstance Win32_PageFileUsage) {
  Write-Host '>>> Pagefile disabled. The machine will RESTART - then RUN this command AGAIN.' -ForegroundColor Yellow
  Start-Sleep 5; Restart-Computer -Force; return
}

# 3. EFI bundle: temporary ESP (FAT32 vdisk) → bcdboot → BCD points to vhd=[locate]\broom\child.vhdx.
$B = "$env:SystemDrive\broom"
Remove-Item $B -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory "$B\efi" | Out-Null
$vd = "$env:TEMP\broom-esp.vhdx"
Remove-Item $vd -ErrorAction SilentlyContinue
$L = (69..90 | ForEach-Object { [char]$_ } | Where-Object { -not (Test-Path "${_}:\") } | Select-Object -Last 1)
@"
create vdisk file="$vd" maximum=300 type=expandable
attach vdisk
convert gpt
create partition primary
format quick fs=fat32 label=BROOMESP
assign letter=$L
"@ | Set-Content -Encoding ascii "$env:TEMP\broom-esp.txt"
diskpart /s "$env:TEMP\broom-esp.txt" | Out-Null
bcdboot "$env:SystemRoot" /s "${L}:" /f UEFI | Out-Null
$S = "${L}:\EFI\Microsoft\Boot\BCD"
bcdedit /store $S /set '{default}' device 'vhd=[locate]\broom\child.vhdx' | Out-Null
bcdedit /store $S /set '{default}' osdevice 'vhd=[locate]\broom\child.vhdx' | Out-Null
bcdedit /store $S /set '{default}' description 'Broom Windows' | Out-Null
bcdedit /store $S /timeout 0 | Out-Null
Copy-Item "${L}:\EFI" "$B\efi\EFI" -Recurse
@"
select vdisk file="$vd"
detach vdisk
"@ | Set-Content -Encoding ascii "$env:TEMP\broom-esp.txt"
diskpart /s "$env:TEMP\broom-esp.txt" | Out-Null
Remove-Item $vd, "$env:TEMP\broom-esp.txt" -ErrorAction SilentlyContinue

# 4. First logon on the client (when base is created): write base.ok to NTFS BROOMWIN + reboot → the stage commits base.
#    (The server also overwrites this file at publish time → logic changes don't need prep again.)
$done = @'
__BROOM_DONE__
'@
New-Item -ItemType Directory "$env:SystemRoot\Setup\Scripts" -Force | Out-Null
Set-Content -Encoding ascii "$env:SystemRoot\Setup\Scripts\broom-done.ps1" $done

# 5. Unattend: skip OOBE, guest user (Administrators — FirstLogonCommands need write access) + autologon.
$ui = (Get-UICulture).Name; $sl = (Get-WinSystemLocale).Name; $ul = (Get-Culture).Name
$tz = (Get-TimeZone).Id
$c = 'processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS"'
@"
<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
  <settings pass="specialize">
    <component name="Microsoft-Windows-Shell-Setup" $c>
      <ComputerName>*</ComputerName>
      <TimeZone>$tz</TimeZone>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-International-Core" $c>
      <InputLocale>$ul</InputLocale><SystemLocale>$sl</SystemLocale><UILanguage>$ui</UILanguage><UserLocale>$ul</UserLocale>
    </component>
    <component name="Microsoft-Windows-Shell-Setup" $c>
      <OOBE>
        <SkipMachineOOBE>true</SkipMachineOOBE>
        <SkipUserOOBE>true</SkipUserOOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <HideLocalAccountScreen>true</HideLocalAccountScreen>
        <ProtectYourPC>3</ProtectYourPC>
      </OOBE>
      <UserAccounts><LocalAccounts><LocalAccount wcm:action="add">
        <Name>__USER__</Name><DisplayName>__USER__</DisplayName><Group>Administrators</Group>
        <Password><Value>__PASS__</Value><PlainText>true</PlainText></Password>
      </LocalAccount></LocalAccounts></UserAccounts>
      <AutoLogon>
        <Enabled>true</Enabled><Username>__USER__</Username><LogonCount>999999</LogonCount>
        <Password><Value>__PASS__</Value><PlainText>true</PlainText></Password>
      </AutoLogon>
      <FirstLogonCommands>
        <SynchronousCommand wcm:action="add"><Order>1</Order>
          <CommandLine>powershell -NoProfile -ExecutionPolicy Bypass -File %WINDIR%\Setup\Scripts\broom-done.ps1</CommandLine>
        </SynchronousCommand>
      </FirstLogonCommands>
    </component>
  </settings>
</unattend>
"@ | Set-Content -Encoding utf8 "$B\unattend.xml"

# 6. Less churn between two builds of this golden (clients download only the 4 MB chunks that changed):
#    throw away caches/logs that differ on every build, then TRIM → freed space reads as zeros in the exported disk
#    (VMware thin disks reclaim it) → "zero" chunks, never downloaded. Best effort: nothing here may stop the prep.
$ErrorActionPreference = 'Continue'
Stop-Service wuauserv, bits, dosvc -Force -ErrorAction SilentlyContinue
foreach ($p in "$env:SystemRoot\SoftwareDistribution\Download", "$env:SystemRoot\Temp", $env:TEMP,
               "$env:SystemRoot\Prefetch", "$env:ProgramData\Microsoft\Windows\DeliveryOptimization\Cache",
               "$env:SystemRoot\Logs\CBS", "$env:SystemRoot\LiveKernelReports", "$env:SystemRoot\Minidump") {
  Get-ChildItem $p -Force -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
}
Remove-Item "$env:SystemRoot\MEMORY.DMP" -Force -ErrorAction SilentlyContinue
Write-Host '>>> Clearing event logs + TRIM of the free space...' -ForegroundColor Green
foreach ($l in (wevtutil el)) { wevtutil cl "$l" 2>&1 | Out-Null }
Optimize-Volume -DriveLetter $env:SystemDrive[0] -ReTrim -ErrorAction SilentlyContinue

# 7. Sysprep → power off the VM. Errors: see C:\Windows\System32\Sysprep\Panther\setupact.log.
Write-Host '>>> Sysprep... the VM will POWER OFF. Then upload the .vmdk file on the web (OS = windows).' -ForegroundColor Green
& "$env:SystemRoot\System32\Sysprep\sysprep.exe" /generalize /oobe /shutdown /unattend:"$B\unattend.xml"
"#;

/// First logon (when base.vhdx is created on each machine): write base.ok to BROOMWIN then reboot → the stage
/// commits base. BROOMWIN has no drive letter (GPT bit 63, set by the stage) → write directly via the volume path
/// `\\?\Volume{..}\`; reboot only once written. ASCII only (Set-Content -Encoding ascii).
const BROOM_DONE: &str = r#"$v = Get-Volume -FileSystemLabel BROOMWIN -ErrorAction SilentlyContinue
if (-not $v) { exit }
# SYSTEM task (stored in base.vhdx -> present every boot): keep Windows AFTER PXE in BootOrder, PXE first.
$s = "$env:SystemRoot\Setup\Scripts\broom-bootorder.ps1"
$a = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File $s"
$t1 = New-ScheduledTaskTrigger -AtStartup
$t2 = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes 5) -RepetitionDuration (New-TimeSpan -Days 3650)
Register-ScheduledTask -TaskName BroomBootOrder -Action $a -Trigger $t1,$t2 -User SYSTEM -RunLevel Highest -Force | Out-Null
& powershell -NoProfile -ExecutionPolicy Bypass -File $s
# Drivers (Drivers page): the stage put this machine's packages in broom\drivers -> install them into base.
# Copied to a local folder first (pnputil wants a normal path); only drivers matching real devices get installed.
$dd = $v.Path + 'broom\drivers'
if ([IO.Directory]::Exists($dd)) {
  $tmp = "$env:SystemRoot\Temp\broom-drivers"
  Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
  foreach ($f in [IO.Directory]::GetFiles($dd, '*', [IO.SearchOption]::AllDirectories)) {
    if ($f.EndsWith('.sha256')) { continue }
    $t = $tmp + $f.Substring($dd.Length)
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($t)) | Out-Null
    [IO.File]::Copy($f, $t, $true)
  }
  if (Test-Path $tmp) { & pnputil /add-driver "$tmp\*.inf" /subdirs /install | Out-Null }
  Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
# Machine name from the server (stage writes broom\host.txt): rename -> takes effect after the reboot below, stored in base.
$h = $v.Path + 'broom\host.txt'
if ([IO.File]::Exists($h)) {
  $n = [IO.File]::ReadAllText($h).Trim()
  if ($n -and ($n -ne $env:COMPUTERNAME)) { Rename-Computer -NewName $n -Force -ErrorAction SilentlyContinue }
}
# License key (Machines page): the server picks it by this machine's IP and hands it out once (403 = none).
# slmgr /cpky afterwards: the key is not left readable in the registry. Never blocks building base.
$sf = $v.Path + 'broom\srv.txt'
if ([IO.File]::Exists($sf)) {
  $srv = [IO.File]::ReadAllText($sf).Trim()
  $k = ''
  try { $k = (Invoke-WebRequest -UseBasicParsing -TimeoutSec 15 -Method Post -Uri "http://$srv/api/license").Content.Trim() } catch { }
  if ($k) {
    $slmgr = "$env:SystemRoot\System32\slmgr.vbs"
    $r = (& cscript //nologo $slmgr /ipk $k | Out-String) + (& cscript //nologo $slmgr /ato | Out-String)
    & cscript //nologo $slmgr /cpky | Out-Null
    $k = ''
    try { Invoke-WebRequest -UseBasicParsing -TimeoutSec 15 -Method Post -Body $r -Uri "http://$srv/api/license/result" | Out-Null } catch { }
  }
}
# Write base.ok DIRECTLY via the volume path (\\?\Volume{..}\broom\base.ok): no drive letter/mountvol needed
# (the old version picked a letter via Test-Path -> clashed with an empty CD drive -> write failed -> OOBE loop every boot).
$f = $v.Path + 'broom\base.ok'
try { [IO.File]::WriteAllText($f, 'ok') } catch { }
# Reboot only if it WAS written: otherwise stay at the desktop (instead of an endless OOBE loop).
if ([IO.File]::Exists($f)) { shutdown /r /t 5 }"#;

/// BroomBootOrder task (SYSTEM, at startup + every 5 minutes): Windows pulls "Windows Boot Manager"
/// to the top of BootOrder every boot → the next power-on skips PXE (no reset). Restores the order the stage
/// wrote to broom\bootorder.txt (Boot#### numbers, PXE = the entry that booted the stage) — by NUMBER, never by
/// entry name (PXE entries are named anything: "IBA GE Slot 0100", "Realtek PXE B03"...). Reads/writes the UEFI
/// BootOrder variable directly (kernel32; SYSTEM + SeSystemEnvironmentPrivilege). Entries not in the file (added
/// later) are kept, after. Writes NVRAM only when different. ASCII only.
const BROOM_BOOTORDER: &str = r#"$v = Get-Volume -FileSystemLabel BROOMWIN -ErrorAction SilentlyContinue
if (-not $v) { exit }
$of = $v.Path + 'broom\bootorder.txt'
if (-not [IO.File]::Exists($of)) { exit }
Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices;
public static class BroomFw {
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  static extern uint GetFirmwareEnvironmentVariableExW(string name, string guid, byte[] buf, uint size, IntPtr attr);
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  static extern bool SetFirmwareEnvironmentVariableExW(string name, string guid, byte[] buf, uint size, uint attr);
  [DllImport("advapi32.dll", SetLastError=true)]
  static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
  [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  static extern bool LookupPrivilegeValueW(string system, string name, out long luid);
  [DllImport("advapi32.dll", SetLastError=true)]
  static extern bool AdjustTokenPrivileges(IntPtr token, bool disableAll, ref TokenPriv tp, uint len, IntPtr prev, IntPtr retLen);
  [DllImport("kernel32.dll")]
  static extern IntPtr GetCurrentProcess();
  [StructLayout(LayoutKind.Sequential, Pack = 4)]
  struct TokenPriv { public uint Count; public long Luid; public uint Attr; }
  const string EfiGlobal = "{8BE4DF61-93CA-11D2-AA0D-00E098032B8C}";
  public static void EnablePrivilege() {
    IntPtr t; OpenProcessToken(GetCurrentProcess(), 0x28, out t); // TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY
    TokenPriv tp = new TokenPriv(); tp.Count = 1; tp.Attr = 2;    // SE_PRIVILEGE_ENABLED
    LookupPrivilegeValueW(null, "SeSystemEnvironmentPrivilege", out tp.Luid);
    AdjustTokenPrivileges(t, false, ref tp, 0, IntPtr.Zero, IntPtr.Zero);
  }
  public static byte[] Get(string name) {
    byte[] b = new byte[4096];
    uint n = GetFirmwareEnvironmentVariableExW(name, EfiGlobal, b, (uint)b.Length, IntPtr.Zero);
    if (n == 0) return null;
    byte[] r = new byte[n]; Array.Copy(b, r, n); return r;
  }
  // 7 = NON_VOLATILE | BOOTSERVICE_ACCESS | RUNTIME_ACCESS (the attributes BootOrder has)
  public static bool Set(string name, byte[] data) { return SetFirmwareEnvironmentVariableExW(name, EfiGlobal, data, (uint)data.Length, 7); }
}
'@
[BroomFw]::EnablePrivilege()
$cur = [BroomFw]::Get('BootOrder')
if ($null -eq $cur) { exit }
$now = @(); for ($i = 0; $i + 1 -lt $cur.Length; $i += 2) { $now += [BitConverter]::ToUInt16($cur, $i) }
$want = @()
foreach ($x in ([IO.File]::ReadAllText($of).Trim() -split ',')) {
  if ($x -notmatch '^[0-9A-Fa-f]{4}$') { continue }
  $n = [Convert]::ToUInt16($x, 16)
  # Only entries that still exist (a Boot#### variable), each once.
  if (($want -notcontains $n) -and ($null -ne [BroomFw]::Get(('Boot{0:X4}' -f $n)))) { $want += $n }
}
if ($want.Count -eq 0) { exit }
foreach ($n in $now) { if ($want -notcontains $n) { $want += $n } }
if (($want -join ',') -ne ($now -join ',')) {
  $b = New-Object byte[] ($want.Count * 2)
  for ($i = 0; $i -lt $want.Count; $i++) { [BitConverter]::GetBytes([uint16]$want[$i]).CopyTo($b, $i * 2) }
  [BroomFw]::Set('BootOrder', $b) | Out-Null
}"#;

/// /broom-prep-win: embeds the guest user/password (config shared with Linux).
pub fn prep_script(db: &dyn Db) -> String {
    let user = db.get_config("ltsp_user", "guest");
    let pass = db.get_config("ltsp_password", "123456");
    // The values sit inside unattend.xml (XML-escaped) AND inside a PowerShell here-string (`$`/backtick would be
    // expanded). set_cafe_user already rejects those characters; escaping here too covers an old stored value.
    let esc = |s: &str| xml(s).replace('`', "``").replace('$', "`$");
    PREP_WIN
        .replace("__BROOM_DONE__", BROOM_DONE)
        .replace("__USER__", &esc(&user))
        .replace("__PASS__", &esc(&pass))
}

/// Overwrite broom-done.ps1 in the golden (a golden prepped with an old version still gets the new logic).
fn write_broom_done(mnt: &str) -> Result<bool, String> {
    let dir = format!("{mnt}/Windows/Setup/Scripts");
    if !Path::new(&dir).is_dir() {
        return Ok(false);
    }
    for (f, body) in [("broom-done.ps1", BROOM_DONE), ("broom-bootorder.ps1", BROOM_BOOTORDER)] {
        std::fs::write(format!("{dir}/{f}"), body.replace('\n', "\r\n")).map_err(|e| format!("write {f}: {e}"))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    /// The shell the stage really runs in: the initramfs busybox ash (package busybox-initramfs). It runs its own
    /// applets (wget, awk, od…) BEFORE anything in PATH — tests must see that. Not installed → the system sh.
    fn stage_sh() -> std::process::Command {
        const BB: &str = "/usr/lib/initramfs-tools/bin/busybox";
        if std::path::Path::new(BB).exists() {
            let mut c = std::process::Command::new(BB);
            c.arg("sh");
            c
        } else {
            std::process::Command::new("sh")
        }
    }

    /// The stage script runs inside the initramfs — a syntax error = client stuck in a shell.
    #[test]
    fn stage_syntax() {
        for s in [super::STAGE_SCRIPT, super::STAGE_HOOK] {
            let ok = stage_sh().args(["-n", "-c", s]).status().unwrap();
            assert!(ok.success());
        }
    }

    /// Shell functions vhdx_guid + patch16 (cut from STAGE_SCRIPT) run on a VHDX generated by vhdx.rs:
    /// read the right DataWriteGuid of "base" and patch it into child-template → Rust reads it back equal.
    #[test]
    fn stage_guid_patch() {
        use crate::vhdx;
        let s = super::STAGE_SCRIPT;
        let funcs = &s[s.find("vhdx_guid(){").unwrap()..s.find("# 2. base/child state machine").unwrap()];
        let dir = std::env::temp_dir();
        let (base, child) = (dir.join("broom_t_base.vhdx"), dir.join("broom_t_child.vhdx"));
        let (base, child) = (base.to_str().unwrap(), child.to_str().unwrap());
        let parent = vhdx::Info { data_write_guid: [7; 16], virtual_size: 1 << 30, logical_sector: 512, physical_sector: 4096 };
        vhdx::write_empty(base, &parent, Some(".\\golden.vhdx")).unwrap();
        let want = vhdx::guid_str(&vhdx::read_info(base).unwrap().data_write_guid);
        let off = vhdx::write_empty(child, &vhdx::Info { data_write_guid: [0; 16], ..parent }, Some(".\\base.vhdx")).unwrap();
        let sh = format!("{funcs}\ng=$(vhdx_guid {base}); echo \"$g\"; patch16 {child} \"$g\" {off}");
        let o = stage_sh().args(["-c", &sh]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), want);
        let raw = std::fs::read(child).unwrap();
        let u: Vec<u16> = raw[off as usize..off as usize + 76].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16(&u).unwrap(), want);
        let _ = std::fs::remove_file(base);
        let _ = std::fs::remove_file(child);
    }

    /// Stage vs server golden.sha256 before downloading (cut from STAGE_SCRIPT, wget mocked by a list of answers,
    /// "" = no file): same hash → go on; missing → wait; other hash → reboot; missing for good → die.
    #[test]
    fn stage_waits_for_server_hash() {
        let s = super::STAGE_SCRIPT;
        let lp = &s[s.find("  srv_hash(){").unwrap()..s.find("  D=$B/dl-$HASH").unwrap()];
        let run = |answers: &[&str]| {
            let d = std::env::temp_dir().join(format!("broom_t_hash_{}", answers.len()));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("seq"), answers.join("\n") + "\n").unwrap();
            std::fs::write(d.join("cnt"), "0").unwrap();
            let sh = format!(
                "cd {}; HASH=aa; NAME=w; SRV=x\nlog(){{ :; }}; sleep(){{ :; }}; die(){{ echo DIE; exit; }}; restart(){{ echo RESTART; exit; }}\n\
                 wget(){{ n=$(cat cnt); echo $((n+1)) > cnt; sed -n \"$((n+1))p\" seq; }}\n{lp}\necho \"GO $(cat cnt)\"",
                d.display()
            );
            let o = stage_sh().args(["-c", &sh]).output().unwrap();
            let _ = std::fs::remove_dir_all(&d);
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        assert_eq!(run(&["aa"]), "GO 1");
        assert_eq!(run(&["", "", "aa"]), "GO 3"); // publish running → waited twice
        assert_eq!(run(&["", "bb"]), "RESTART"); // newer version published → reboot for the new boot script
        assert_eq!(run(&[""; 50]), "DIE");
    }

    /// Stage license part (cut from STAGE_SCRIPT): a new generation rebuilds base, the same one keeps it,
    /// no key keeps base (activation stays) and drops lic.txt; srv.txt always written.
    #[test]
    fn stage_license_rebuilds_base() {
        let s = super::STAGE_SCRIPT;
        let part = &s[s.find("# License key (Machines page)").unwrap()..s.find("# Drivers (Drivers page)").unwrap()];
        let run = |lic: &str, base_lic: &str| {
            let d = std::env::temp_dir().join(format!("broom_t_lic_{lic}_{base_lic}"));
            std::fs::create_dir_all(&d).unwrap();
            for f in ["base.vhdx", "child-local.vhdx", "lic.txt"] {
                std::fs::write(d.join(f), "x").unwrap();
            }
            std::fs::write(d.join("base.lic"), format!("{base_lic}\n")).unwrap();
            let sh = format!("cd {}; SRV=10.0.0.12; LIC={lic}\nlog(){{ :; }}\n{part}", d.display());
            assert!(stage_sh().args(["-c", &sh]).status().unwrap().success());
            let has = |f: &str| d.join(f).exists();
            let out = (has("base.vhdx"), has("lic.txt"), std::fs::read_to_string(d.join("srv.txt")).unwrap());
            let _ = std::fs::remove_dir_all(&d);
            out
        };
        assert_eq!(run("2", "1"), (false, true, "10.0.0.12\n".into())); // set / re-armed → rebuild
        assert_eq!(run("1", "1"), (true, true, "10.0.0.12\n".into()));
        assert_eq!(run("", "1"), (true, false, "10.0.0.12\n".into()));
    }

    /// Stage drivers part (cut from STAGE_SCRIPT, wget mocked, real tar.gz): download + extract, base rebuilt only
    /// when the set present changes, removal, no answer → keep, failed download → not counted (retried).
    #[test]
    fn stage_drivers_sync() {
        let s = super::STAGE_SCRIPT;
        let d = std::env::temp_dir().join("broom_t_drv");
        let _ = std::fs::remove_dir_all(&d);
        let (b, run) = (d.join("b"), d.join("run"));
        for p in [&b, &run, &d.join("pkgs/src")] {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(d.join("pkgs/src/nv.inf"), "PCI\\VEN_10DE&DEV_2504").unwrap();
        assert!(std::process::Command::new("tar").args(["-czf", "../nv.tar.gz", "-C", ".", "nv.inf"]).current_dir(d.join("pkgs/src")).status().unwrap().success());
        let part = s[s.find("# Drivers (Drivers page)").unwrap()..s.find("# Last session's writes").unwrap()]
            .replace("/run/", &format!("{}/", run.display()));
        // wget mock: POST → answer.txt (missing = server down); GET → pkgs/<file> on stdout.
        let mock = format!(
            "cd {}; SRV=x; MAC=aa:bb:cc:dd:ee:01\nlog(){{ echo \"$*\" >> {}/log; }}; configure_networking(){{ :; }}\n\
             wget(){{ o=\"\"; p=\"\"; u=\"\"; while [ $# -gt 0 ]; do case \"$1\" in -O) o=$2; shift;; -T) shift;; --post-file=*) p=1;; -q) ;; *) u=$1;; esac; shift; done\n\
               if [ -n \"$p\" ]; then [ -f {d}/answer.txt ] && cp {d}/answer.txt \"$o\"; else cat {d}/pkgs/${{u##*/}}; fi; }}\n{part}",
            b.display(),
            d.display(),
            d = d.display()
        );
        let step = |answer: Option<&str>| {
            match answer {
                Some(a) => std::fs::write(d.join("answer.txt"), a).unwrap(),
                None => {
                    let _ = std::fs::remove_file(d.join("answer.txt"));
                }
            }
            std::fs::write(b.join("base.vhdx"), "x").unwrap_or(()); // a base exists before every boot
            assert!(stage_sh().args(["-c", &mock]).status().unwrap().success());
            let rebuilt = !b.join("base.vhdx").exists();
            // Commit what a finished base build would record (stage: cp drv.txt base.drv).
            match std::fs::read(b.join("drv.txt")) {
                Ok(v) => std::fs::write(b.join("base.drv"), v).unwrap(),
                Err(_) => {
                    let _ = std::fs::remove_file(b.join("base.drv"));
                }
            }
            (rebuilt, b.join("drivers/nv/nv.inf").exists())
        };
        assert_eq!(step(Some("")), (false, false), "no packages: an old base stays");
        assert_eq!(step(Some("nv s1\n")), (true, true), "new package → downloaded + base rebuilt");
        assert_eq!(step(Some("nv s1\n")), (false, true), "unchanged → nothing to do");
        assert_eq!(step(None), (false, true), "server down → keep");
        assert_eq!(step(Some("")), (true, false), "package gone → removed + rebuilt");
        assert_eq!(step(Some("bad s2\n")), (false, false), "download fails → not counted, retried next boot");
        let log = std::fs::read_to_string(d.join("log")).unwrap();
        assert!(log.contains("driver bad: download failed"), "{log}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Stage boot order (cut from STAGE_SCRIPT, efibootmgr mocked with a real-board-like list): the PXE entry is
    /// BootCurrent whatever its name; other network entries go after Windows; unknown BootCurrent → old matching.
    #[test]
    fn stage_boot_order_by_bootcurrent() {
        let s = super::STAGE_SCRIPT;
        let part = &s[s.find("all=$(efibootmgr -v)").unwrap()..s.find("# Stage log on BROOMWIN").unwrap()];
        let v = "Boot0000* Windows Boot Manager\tHD(1,GPT,aaaa)/File(\\EFI\\Microsoft\\Boot\\bootmgfw.efi)\n\
                 Boot0001* UEFI: SanDisk\tPciRoot(0x0)/Pci(0x14,0x0)/USB(1,0)\n\
                 Boot0003* IBA GE Slot 0100 v1553\tPciRoot(0x0)/Pci(0x1f,0x6)/MAC(001122334455,0)\n\
                 Boot0004* UEFI: PXE IPv6 Intel(R) I219-V\tPciRoot(0x0)/Pci(0x1f,0x6)/MAC(001122334455,0)/IPv6(0)\n\
                 Boot0005* EFI Network 1\tVenHw(1234)\n\
                 Boot0007* Broom Windows\tHD(1,GPT,bbbb)/File(\\EFI\\Microsoft\\Boot\\bootmgfw.efi)\n";
        let run = |current: &str| {
            let d = std::env::temp_dir().join(format!("broom_t_order_{}", if current.is_empty() { "none" } else { current }));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("v.txt"), v).unwrap();
            let plain: String = v.lines().map(|l| l.split('\t').next().unwrap().to_string() + "\n").collect();
            let head = if current.is_empty() { String::new() } else { format!("BootCurrent: {current}\n") };
            std::fs::write(d.join("plain.txt"), format!("{head}BootOrder: 0000,0004,0003,0007,0001,0005\n{plain}")).unwrap();
            let sh = format!(
                "cd {}; B=.; n=0007\nlog(){{ echo \"$*\" >> log; }}\n\
                 efibootmgr(){{ case \"$1\" in -v) cat v.txt;; -q) echo \"$3\" > set.txt;; *) cat plain.txt;; esac; }}\n{part}",
                d.display()
            );
            assert!(stage_sh().args(["-c", &sh]).status().unwrap().success());
            let rd = |f: &str| std::fs::read_to_string(d.join(f)).unwrap_or_default().trim().to_string();
            let out = (rd("set.txt"), rd("bootorder.txt"), rd("log"));
            let _ = std::fs::remove_dir_all(&d);
            out
        };
        let (set, file, log) = run("0003");
        assert_eq!(set, "0003,0007,0000,0004,0005,0001", "PXE (named IBA GE…) first, other network after Windows");
        assert_eq!(file, set, "the order Windows restores");
        assert!(log.contains("PXE Boot0003 (IBA GE Slot 0100 v1553)"), "{log}");
        assert_eq!(run("").0, "0004,0003,0005,0007,0000,0001", "no BootCurrent → every network entry first");
        assert_eq!(run("0000").0, "0004,0003,0005,0007,0000,0001", "BootCurrent = Windows entry → ignored");
    }

    /// Stage delta() (cut from STAGE_SCRIPT, real files + manifests from publish::write_manifest, wget mocked with
    /// HTTP Range): the result is byte-identical to the new golden and only the missing chunks are downloaded.
    #[test]
    fn stage_delta_golden() {
        const C: usize = crate::publish::MANIFEST_CHUNK;
        let s = super::STAGE_SCRIPT;
        let func = &s[s.find("DW=4").unwrap()..s.find("# end delta").unwrap()];
        let d = std::env::temp_dir().join("broom_t_delta");
        let chunk = |b: u8| vec![b; C];
        let file = |parts: &[Vec<u8>]| parts.concat();
        // old: chunks 1..5 + a partial tail
        let old = file(&[chunk(1), chunk(2), chunk(3), chunk(4), chunk(5), vec![6; 1000]]);
        let run = |new: &[u8], server: &[u8], corrupt_old_chunk: Option<usize>, stale_patch: bool| {
            let _ = std::fs::remove_dir_all(&d);
            for p in ["o", "n", "run"] {
                std::fs::create_dir_all(d.join(p)).unwrap();
            }
            std::fs::write(d.join("o/golden.vhdx"), &old).unwrap();
            crate::publish::write_manifest(&d.join("o/golden.vhdx"), &d.join("o")).unwrap();
            std::fs::write(d.join("n/golden.vhdx"), new).unwrap();
            crate::publish::write_manifest(&d.join("n/golden.vhdx"), &d.join("n")).unwrap();
            std::fs::write(d.join("server.bin"), server).unwrap();
            if let Some(k) = corrupt_old_chunk {
                let mut o = old.clone();
                o[k * C..(k + 1) * C].fill(0xEE);
                std::fs::write(d.join("o/golden.vhdx"), o).unwrap();
            }
            if stale_patch {
                std::fs::write(d.join("o/golden.vhdx.patching"), "another-version\n").unwrap();
            }
            let body = func.replace("/run/", &format!("{}/run/", d.display()));
            let sh = format!(
                "cd {d}; W={d}\nlog(){{ echo \"$*\" >> {d}/log; }}\n\
                 wget(){{ o=\"\"; u=\"\"; while [ $# -gt 0 ]; do case \"$1\" in -O) o=$2; shift;; -q) ;; *) u=$1;; esac; shift; done\n\
                   case \"$u\" in *golden-chunk\\?name=w\\&i=*\\&h=*) ;; *) return 1;; esac\n\
                   i=${{u#*&i=}}; i=${{i%%&*}}; echo \"$i\" >> {d}/dl\n\
                   tail -c +$((i * 4194304 + 1)) {d}/server.bin | head -c 4194304 | zstd -q -c > \"$o\"; }}\n\
                 {body}\ndelta {d}/o/golden.vhdx {d}/o/golden.chunks {d}/n/golden.chunks {d}/out.vhdx 'http://x/api/golden-chunk?name=w'",
                d = d.display()
            );
            let ok = stage_sh().args(["-c", &sh]).status().unwrap().success();
            let rd = |f: &str| std::fs::read_to_string(d.join(f)).unwrap_or_default();
            let same = std::fs::read(d.join("out.vhdx")).map(|o| o == new).unwrap_or(false);
            (ok, same, rd("dl").lines().count(), rd("log"))
        };
        // A: chunk 2 + the tail changed in place → nothing moved, 2 downloads.
        let a = file(&[chunk(1), chunk(2), chunk(9), chunk(4), chunk(5), vec![8; 1000]]);
        let (ok, same, dls, log) = run(&a, &a, None, false);
        assert!(ok && same && dls == 2 && log.contains("0 moved"), "{log}");
        // B: a new chunk early → every later chunk shifts → 5 moved (saved first, then written), 1 download.
        let b = file(&[chunk(1), chunk(7), chunk(2), chunk(3), chunk(4), chunk(5), vec![6; 1000]]);
        let (ok, same, dls, log) = run(&b, &b, None, false);
        assert!(ok && same && dls == 1 && log.contains("5 moved") && log.contains("delta done"), "{log}");
        // C: the old copy is damaged where a chunk is copied from → that chunk is downloaded instead.
        let (ok, same, dls, _) = run(&b, &b, Some(3), false);
        assert!(ok && same && dls == 2);
        // D: the server sends wrong data → fails after 3 tries → the caller falls back to a full download.
        let (ok, _, dls, log) = run(&b, &a, None, false);
        assert!(!ok && dls == 3 && log.contains("failed 3 times"), "{log}");
        // E: 4 workers over a longer file: reversed order + 3 new chunks, all workers busy → still byte-identical.
        let mut parts: Vec<Vec<u8>> = vec![chunk(30), chunk(31)];
        parts.extend([5u8, 4, 3, 2, 1].iter().map(|&b| chunk(b)));
        parts.push(chunk(32));
        parts.push(vec![6; 1000]);
        let e = file(&parts);
        let (ok, same, dls, log) = run(&e, &e, None, false);
        assert!(ok && same && dls == 3 && log.contains("4 workers"), "{log}");
        assert!(!d.join("o/golden.vhdx.spare").exists() && !d.join("o/golden.vhdx.patching").exists(), "cleaned up");
        // F: a previous run was interrupted while patching towards ANOTHER version → unchanged chunks can't be trusted
        // → no delta (the caller downloads everything).
        let (ok, _, dls, log) = run(&b, &b, None, true);
        assert!(!ok && dls == 0 && log.contains("half-patched"), "{log}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn prep_win_filled() {
        let s = super::PREP_WIN
            .replace("__BROOM_DONE__", super::BROOM_DONE)
            .replace("__USER__", "guest")
            .replace("__PASS__", "1");
        assert!(!s.contains("__"), "placeholder left unreplaced");
        assert!(super::BROOM_DONE.is_ascii(), "broom-done is written with -Encoding ascii");
        assert!(super::BROOM_BOOTORDER.is_ascii());
        assert!(s.contains("WriteAllText"));
    }

    #[test]
    fn skip_oobe_patch() {
        let x = "<OOBE>\n  <HideEULAPage>true</HideEULAPage>\n</OOBE>";
        let y = super::add_skip_oobe(x).unwrap();
        assert!(y.starts_with("<OOBE>\n        <SkipMachineOOBE>true</SkipMachineOOBE>"));
        assert!(y.contains("<SkipUserOOBE>true</SkipUserOOBE>") && y.contains("<HideEULAPage>"));
        assert_eq!(super::add_skip_oobe(&y).unwrap(), y); // not inserted twice
        assert!(super::add_skip_oobe("<unattend/>").is_none());
    }

    #[test]
    fn ntfs_parts_order() {
        // UEFI VM: ESP, MSR, C:, Recovery (other types) → only C: is basic data.
        let j = r#"{"partitiontable":{"label":"gpt","sectorsize":512,"partitions":[
          {"node":"d1","start":2048,"size":204800,"type":"C12A7328-F81F-11D2-BA4B-00A0C93EC93B"},
          {"node":"d2","start":206848,"size":32768,"type":"E3C9E316-0B5C-4DB8-817D-F92DF00215AE"},
          {"node":"d3","start":239616,"size":124000000,"type":"EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"},
          {"node":"d4","start":124239616,"size":1000000,"type":"DE94BBA4-06D1-4D40-A16A-BFD50179D6AC"}]}}"#;
        assert_eq!(super::ntfs_parts(j).unwrap(), vec![(239616 * 512, 124000000 * 512)]);
        // MBR (BIOS VM): type "7", largest first.
        let j = r#"{"partitiontable":{"label":"dos","partitions":[
          {"node":"d1","start":2048,"size":100000,"type":"7"},{"node":"d2","start":102048,"size":9000000,"type":"7"}]}}"#;
        let p = super::ntfs_parts(j).unwrap();
        assert_eq!((p[0].0, p[1].0), (102048 * 512, 2048 * 512));
    }
}
