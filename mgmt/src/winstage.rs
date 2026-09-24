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
for b in sfdisk blkid mkfs.fat mkntfs ntfsfix efibootmgr wget sha256sum tar gzip od dd; do
  p=$(command -v $b) && copy_exec "$p" /broom/bin/$b
done
manual_add_modules ntfs3 vfat nls_cp437 nls_iso8859_1 nls_utf8 efivarfs
"#;

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
NAME=""; HASH=""; SRV=""; HOST=""
for a in $(cat /proc/cmdline); do
  case "$a" in broom.name=*) NAME=${a#*=};; broom.hash=*) HASH=${a#*=};; broom.srv=*) SRV=${a#*=};;
    broom.host=*) HOST=${a#*=};; esac
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
mount -t ntfs3 "$(part $disk 2)" $W || mount -t ntfs3 -o force "$(part $disk 2)" $W \
  || die "mount ntfs3 $(part $disk 2)"
mkdir -p $B

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
  rm -f $B/golden.vhdx $B/golden.sha256 $B/base.vhdx $B/base.ok $B/first.pending $B/child.vhdx $B/child-local.vhdx
  mkdir -p $D
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
  log "checking golden sha256 - rereads the whole file, may take a few minutes, DO NOT power off..."
  [ "$(sha256sum $D/golden.vhdx | cut -d' ' -f1)" = "$HASH" ] || { rm -rf $D; die "golden sha256 mismatch"; }
  rm -f $D/*.ok
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
# Stage log on BROOMWIN (last 300 lines) — readable from Windows: mountvol + type broom\stage.log.
{ echo "=== $(date '+%F %T') $MODE"; cat /run/broom-stage.log 2>/dev/null; } >> $B/stage.log
tail -n 300 $B/stage.log > $B/stage.log.t && mv $B/stage.log.t $B/stage.log
sync; umount $E; umount $W

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
# FORCE the order: PXE/network → Broom Windows → Windows Boot Manager (+ other bootmgfw entries) → the rest.
# Windows pulls "Windows Boot Manager" to the top on every boot → fixed here + by the BroomBootOrder task
# inside Windows. Broom Windows always stays in the order (server down still boots, that session is not reset).
all=$(efibootmgr -v)
# printf, NOT echo: dash/ash echo interprets "\b" in "\Boot\bootmgfw.efi" → no match.
nums(){ printf '%s\n' "$all" | grep -Ei "$1" | sed -n 's/^Boot\([0-9A-Fa-f]\{4\}\).*/\1/p' | tr '\n' ' '; }
net=" $(nums 'MAC\(|IPv4\(|IPv6\(|PXE|Network') "
wins=" $(nums 'bootmgfw\.efi') "
order=$(efibootmgr | sed -n 's/^BootOrder: //p')
a=""; c=""; d=""; old=$IFS; IFS=,
for x in $order; do
  [ "$x" = "$n" ] && continue
  case "$wins" in *" $x "*) c="${c:+$c,}$x"; continue;; esac
  case "$net" in *" $x "*) a="${a:+$a,}$x"; continue;; esac
  d="${d:+$d,}$x"
done
IFS=$old
want=$(echo "$a,$n,$c,$d" | sed 's/,,*/,/g; s/^,//; s/,$//')
if [ "$want" != "$order" ]; then
  efibootmgr -q -o "$want" && log "BootOrder forced: PXE -> Broom Windows -> Windows Boot Manager [$want]"
fi
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
    let key = {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (&kv, &initramfs_conf, STAGE_HOOK, STAGE_SCRIPT).hash(&mut h);
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
    write_exec(&format!("{conf}/hooks/broom-stage"), STAGE_HOOK)?;
    write_exec(&format!("{conf}/scripts/init-premount/broom-stage"), STAGE_SCRIPT)?;
    std::fs::create_dir_all(&sd).map_err(|e| e.to_string())?;
    let tmp = format!("{sd}/stage.img.tmp");
    run("mkinitramfs", &["-d", conf, "-o", &tmp, &kv])?;
    // Tools WITHOUT a busybox replacement must really be in the initrd — report missing ones now
    // at publish time, not when a client gets stuck in a shell.
    let list = run("lsinitramfs", &[&tmp])?;
    let missing: Vec<&str> = ["sfdisk", "mkfs.fat", "mkntfs", "ntfsfix", "efibootmgr"]
        .into_iter()
        .filter(|b| !list.lines().any(|l| l.ends_with(&format!("broom/bin/{b}"))))
        .collect();
    if !missing.is_empty() {
        return Err(format!("stage initrd is missing {} — install the packages on the server (fdisk ntfs-3g dosfstools efibootmgr) then Publish again", missing.join(", ")));
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

    // Hash (clients compare it to know whether to re-download). Golden kept → reuse the stored hash (sha256 of 13GB
    // takes ~2 minutes on a slow disk).
    let sum_file = format!("{out}/golden.sha256");
    let cached = std::fs::read_to_string(&sum_file).ok().map(|s| s.trim().to_string()).filter(|s| s.len() == 64);
    let hash = match cached {
        Some(h) if fresh => h,
        _ => {
            steps.go("sha256 golden");
            let h = crate::publish::file_hash(&golden).ok_or("sha256sum golden.vhdx failed")?;
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
        "kernel http://{ip}/tftp/broom-stage/vmlinuz initrd=stage.img ip=dhcp BOOTIF=01-${{mac:hexhyp}} broom.name={name} broom.hash={hash} broom.srv={ip} broom.host=${{broom-host}}\n\
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

# 6. Sysprep → power off the VM. Errors: see C:\Windows\System32\Sysprep\Panther\setupact.log.
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
# Machine name from the server (stage writes broom\host.txt): rename -> takes effect after the reboot below, stored in base.
$h = $v.Path + 'broom\host.txt'
if ([IO.File]::Exists($h)) {
  $n = [IO.File]::ReadAllText($h).Trim()
  if ($n -and ($n -ne $env:COMPUTERNAME)) { Rename-Computer -NewName $n -Force -ErrorAction SilentlyContinue }
}
# Write base.ok DIRECTLY via the volume path (\\?\Volume{..}\broom\base.ok): no drive letter/mountvol needed
# (the old version picked a letter via Test-Path -> clashed with an empty CD drive -> write failed -> OOBE loop every boot).
$f = $v.Path + 'broom\base.ok'
try { [IO.File]::WriteAllText($f, 'ok') } catch { }
# Reboot only if it WAS written: otherwise stay at the desktop (instead of an endless OOBE loop).
if ([IO.File]::Exists($f)) { shutdown /r /t 5 }"#;

/// BroomBootOrder task (SYSTEM, at startup + every 5 minutes): Windows pulls "Windows Boot Manager"
/// to the top of BootOrder every boot → the next power-on skips PXE (no reset). FORCE the same order as the stage:
/// PXE/network → Broom Windows → Windows Boot Manager (+ other bootmgfw entries) → the rest.
/// Writes NVRAM only when different. bcdedit field names (identifier/description/displayorder) are not localized.
/// ASCII only.
const BROOM_BOOTORDER: &str = r#"$txt = (bcdedit /enum firmware) -join "`n"
$cur = @(); $net = @(); $broom = @(); $win = @(); $rest = @()
foreach ($b in ($txt -split "`n\s*`n")) {
  if ($b -notmatch 'identifier\s+(\{[^}]+\})') { continue }
  $id = $matches[1]
  if ($id -eq '{fwbootmgr}') {
    $in = $false
    foreach ($l in ($b -split "`n")) {
      if ($l -match '^displayorder\s+(\{[^}]+\})') { $in = $true; $cur += $matches[1]; continue }
      if ($in -and $l -match '^\s+(\{[^}]+\})') { $cur += $matches[1]; continue }
      $in = $false
    }
    continue
  }
  $desc = ''
  if ($b -match '(?m)^description\s+(.+)$') { $desc = $matches[1].Trim() }
  if ($desc -eq 'Broom Windows') { $broom += $id }
  elseif ($b -match 'bootmgfw\.efi') { $win += $id }
  elseif ($desc -match 'Network|PXE|IPv4|IPv6') { $net += $id }
  else { $rest += $id }
}
$want = @($net) + @($broom) + @($win) + @($rest)
if (($want -join ' ') -ne ($cur -join ' ')) {
  bcdedit /set '{fwbootmgr}' displayorder @want | Out-Null
}"#;

/// /broom-prep-win: embeds the guest user/password (config shared with Linux).
pub fn prep_script(db: &dyn Db) -> String {
    let user = db.get_config("ltsp_user", "guest");
    let pass = db.get_config("ltsp_password", "123456");
    PREP_WIN
        .replace("__BROOM_DONE__", BROOM_DONE)
        .replace("__USER__", &xml(&user))
        .replace("__PASS__", &xml(&pass))
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
    /// The stage script runs inside the initramfs — a syntax error = client stuck in a shell.
    #[test]
    fn stage_syntax() {
        for s in [super::STAGE_SCRIPT, super::STAGE_HOOK] {
            let ok = std::process::Command::new("sh").args(["-n", "-c", s]).status().unwrap();
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
        let o = std::process::Command::new("sh").args(["-c", &sh]).output().unwrap();
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
            let o = std::process::Command::new("sh").args(["-c", &sh]).output().unwrap();
            let _ = std::fs::remove_dir_all(&d);
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        assert_eq!(run(&["aa"]), "GO 1");
        assert_eq!(run(&["", "", "aa"]), "GO 3"); // publish running → waited twice
        assert_eq!(run(&["", "bb"]), "RESTART"); // newer version published → reboot for the new boot script
        assert_eq!(run(&[""; 50]), "DIE");
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
