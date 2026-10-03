// overlay.rs — golden raw disk → extract kernel/initrd for iPXE + prep script that bakes the overlay hook.
//
// Model (replaced LTSP): golden = raw disk (from vmdk), served over iSCSI as shared RO. Clients boot with
// the golden's own kernel/initrd; the initrd (open-iscsi + overlayroot + reset hook baked in the
// golden VM via PREP_SCRIPT) does: attach iSCSI (iBFT set by iPXE sanhook) → mount root RO =
// lower → build the local SSD writeback (reset every boot) → overlayfs → boot. Writeback goes to the SSD,
// not RAM. Users/apps are baked into the img.
//
// Server side: reads the golden with linuxfs.rs (partitions → ext4 / LVM2 linear → ext4, read-only, no
// mount/loop — so the server's own LVM never sees the golden's VG). Copies the newest vmlinuz+initrd to
// <home>/tftp/broom/<name>/ + reads the root UUID (for boot_script root=UUID=).
//
// The overlay/iSCSI hook inside the initrd is the most fragile part: after changing it, boot a real client
// (SSD cache hit and miss) before releasing.
use std::path::Path;
use std::process::Command;

/// Extract vmlinuz + initrd.img from the golden → <home>/tftp/broom/<name>/, inject the broom hook into the
/// initrd. Returns the root UUID. Blocking.
pub fn build_boot(img: &Path, name: &str) -> Result<String, String> {
    let dst = crate::tftp_dir().join("broom").join(name).to_string_lossy().into_owned();
    let b = crate::linuxfs::extract_boot(&img.to_string_lossy(), &dst)?;
    tracing::info!("image {name}: kernel {} + initrd copied, root UUID {}", b.kver, b.root_uuid);

    // Inject the broom-wb hook + overlayroot.conf into the initrd (append a cpio → overrides the golden's copy).
    // → tuning reset/overlay = edit Rust + Publish again, NO golden rebuild.
    inject_initrd(&format!("{dst}/initrd.img"), name)?;
    Ok(b.root_uuid)
}

/// overlayroot.conf injected into the initrd (owned by the server → tune without rebuilding the golden).
/// root RO (iSCSI) + upper on the local SSD LABEL=broomwb (the broom-wb hook formats it).
const OVERLAYROOT_CONF: &str =
    "overlayroot=\"device:dev=/dev/disk/by-label/broomwb,recurse=0\"\noverlayroot_cfgdisk=\"disabled\"\n";

/// COMBINED hook, OVERRIDES `scripts/local-top/iscsi` (that file is ALREADY in ORDER so it surely runs —
/// a self-added script NOT in ORDER is skipped by initramfs-tools). Every boot:
///  1. Local SSD (disk present BEFORE attaching iSCSI): GPT p1 LABEL=broomwb (writeback, mkfs every boot)
///     + p2 LABEL=broomcache (persistent: golden cache + /games). No layout yet → partition once.
///  2. Cache HIT (broomcache/<name>.sha256 == broom.hash on cmdline) → losetup RO the copy
///     on the SSD as root, NO iSCSI attach (zero network/server load).
///     MISS → bring up the NIC + iscsistart -b (iBFT from iPXE sanhook) as before; broom-cache.service (golden)
///     copies golden iSCSI → SSD in the background for the next boot.
///  3. No sfdisk/losetup (old golden prep), unregistered machine or several disks → writeback in zram, no disk touched.
const BROOM_ISCSI: &str = r#"#!/bin/sh
case "$1" in prereqs) echo ""; exit 0;; esac
# Log to console + /run/broom-wb.log (/run moves to the real root → readable after boot).
log(){ echo "broom: $*"; echo "$*" >> /run/broom-wb.log; }
WB_GB=30   # writeback size of p1; the rest = cache + /games. Change = wipefs the disk to repartition.
modprobe iscsi_tcp 2>/dev/null
modprobe iscsi_ibft 2>/dev/null
NAME=""; HASH=""; SIZE=""; NOCACHE=""; SRV=""; REG=""
for a in $(cat /proc/cmdline); do
  case "$a" in
    broom.name=*) NAME=${a#*=};; broom.hash=*) HASH=${a#*=};; broom.size=*) SIZE=${a#*=};; broom.srv=*) SRV=${a#*=};;
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
"#;

/// Runs in the REAL ROOT: broom-cache.service (baked into the golden via broom-prep) calls
/// /run/broom-cache.sh copied out by the initrd hook → logic still injected by the server, tune without a golden rebuild.
/// Mount /games from the SSD cache; MISS → copy golden iSCSI → SSD for the next boot.
const CACHE_SCRIPT: &str = r#"#!/bin/sh
. /run/broom-cache.env
C=/run/broomcache
log(){ echo "$*" >> /run/broom-wb.log; }
# Hide broom's own SSD partitions (writeback + golden cache) from the desktop file manager — internal, and a guest
# browsing/mounting the cache could expose or corrupt the shared golden. Done here (server-injected) so it applies on
# a republish without re-prepping the golden. The overlay root is reset every boot, so re-create it each time.
if [ ! -f /etc/udev/rules.d/99-broom-hide.rules ] 2>/dev/null; then
  cat >/etc/udev/rules.d/99-broom-hide.rules 2>/dev/null <<'UDEV' && {
ENV{ID_FS_LABEL}=="broomwb", ENV{UDISKS_IGNORE}="1"
ENV{ID_FS_LABEL}=="broomcache", ENV{UDISKS_IGNORE}="1"
UDEV
    udevadm control --reload 2>/dev/null || true
    udevadm trigger --subsystem-match=block 2>/dev/null || true
  }
fi
mkdir -p /games && mount --bind $C/games /games && chmod 1777 $C/games
[ "$MODE" = miss ] && [ -b "$GOLDEN" ] && [ -n "$HASH" ] && [ -n "$SIZE" ] || exit 0
# Spread over 0–5 minutes so all clients don't pull the golden at once after Publish; still congested →
# limit bandwidth on the server side.
sleep $(( $(od -An -N2 -tu2 /dev/urandom) % 300 ))
# Only keep the cache of the running image (other images + partial files go).
for f in $C/*.img; do [ "$f" = "$C/$NAME.img" ] || rm -f "$f" "${f%.img}.sha256" "${f%.img}.chunks"; done
rm -f $C/*.tmp
# Delta: an older copy of THIS image + its manifest → patch it in place (raw = fixed positions): only chunks whose
# sha256 changed (server's golden.chunks) are read from the attached iSCSI golden, checked, written. Cache invalid
# while patching (re-running is idempotent). Any problem → the full copy below.
M=/run/broom-golden.chunks; T=/run/broom-chunk
if [ -f "$C/$NAME.img" ] && [ -f "$C/$NAME.chunks" ] && [ -n "$SRV" ] \
   && wget -q -O $M "http://$SRV/tftp/broom/$NAME/golden.chunks" && [ "$(sed -n '1s/^size //p' $M)" = "$SIZE" ]; then
  rm -f "$C/$NAME.sha256"
  awk 'NR==FNR { if (FNR>1) o[FNR-2]=$1; next } FNR>1 && o[FNR-2]!=$1 { print FNR-2, $1 }' "$C/$NAME.chunks" $M > /run/broom-diff
  truncate -s "$SIZE" "$C/$NAME.img"
  n=0; bad=""
  while read i h; do
    off=$((i * 4194304)); len=$((SIZE - off)); [ $len -gt 4194304 ] && len=4194304
    if [ "$h" = zero ]; then
      dd if=/dev/zero of="$C/$NAME.img" bs=4M iflag=count_bytes oflag=seek_bytes seek=$off count=$len conv=notrunc status=none
    else
      dd if="$GOLDEN" of=$T bs=4M iflag=skip_bytes,count_bytes skip=$off count=$len status=none
      [ "$(sha256sum $T | cut -c1-64)" = "$h" ] || { bad=$i; break; }
      dd if=$T of="$C/$NAME.img" bs=4M oflag=seek_bytes seek=$off conv=notrunc status=none
    fi
    n=$((n + 1))
  done < /run/broom-diff
  rm -f $T
  if [ -z "$bad" ]; then
    cp $M "$C/$NAME.chunks" && sync && echo "$HASH" > "$C/$NAME.sha256" && sync
    log "cache: delta OK — $n chunks ($((n * 4)) MB) patched, next boot runs from the SSD"
    exit 0
  fi
  log "cache: delta chunk $bad does not match the manifest → full copy"
fi
rm -f $C/*.img $C/*.sha256 $C/*.chunks
avail=$(df -B1 --output=avail $C | tail -1)
[ "$avail" -gt "$SIZE" ] || { log "cache: not enough SSD space ($avail < $SIZE)"; exit 0; }
log "cache: copying $GOLDEN -> $C/$NAME.img ..."
# count_bytes: a zram backstore can be larger than the source file (page rounding) → cut exactly SIZE so the hash matches.
ionice -c3 nice -n19 dd if="$GOLDEN" of="$C/$NAME.tmp" bs=4M iflag=count_bytes count="$SIZE" status=none \
  || { log "cache: dd failed"; exit 0; }
h=$(ionice -c3 nice -n19 sha256sum "$C/$NAME.tmp" | cut -d' ' -f1)
if [ "$h" = "$HASH" ]; then
  mv "$C/$NAME.tmp" "$C/$NAME.img" && sync && echo "$HASH" > "$C/$NAME.sha256" && sync
  # Manifest of this copy → the next golden update is a delta. No manifest (old server) → next one is full again.
  [ -n "$SRV" ] && wget -q -O "$C/$NAME.chunks" "http://$SRV/tftp/broom/$NAME/golden.chunks" || rm -f "$C/$NAME.chunks"
  log "cache: OK — next boot runs from the SSD"
else
  rm -f "$C/$NAME.tmp"; log "cache: hash mismatch ($h) — discarded"
fi
"#;

/// Append one cpio.gz (overrides local-top/iscsi = attach golden + SSD writeback; + /etc/overlayroot.conf)
/// to the end of the initrd → the kernel concatenates cpios, the later one overrides the golden's.
fn inject_initrd(initrd: &str, name: &str) -> Result<(), String> {
    let work = crate::work_dir().join(format!("inject-{name}")).to_string_lossy().into_owned();
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(format!("{work}/scripts/local-top")).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(format!("{work}/etc")).map_err(|e| e.to_string())?;
    // OVERRIDE scripts/local-top/iscsi (already in ORDER → surely runs). Does both attach + writeback.
    let iscsi_hook = format!("{work}/scripts/local-top/iscsi");
    std::fs::write(&iscsi_hook, BROOM_ISCSI).map_err(|e| e.to_string())?;
    std::fs::write(format!("{work}/scripts/broom-cache.sh"), CACHE_SCRIPT)
        .map_err(|e| e.to_string())?;
    std::fs::write(format!("{work}/etc/overlayroot.conf"), OVERLAYROOT_CONF)
        .map_err(|e| e.to_string())?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&iscsi_hook, std::fs::Permissions::from_mode(0o755));
    }
    // cd work → cpio newc gzip → append to the initrd (absolute path).
    let sh = format!(
        "cd '{work}' && find . -mindepth 1 -print0 | cpio --null -o -H newc 2>/dev/null | gzip -9 >> '{initrd}'"
    );
    let ok = Command::new("sh").arg("-c").arg(&sh).status().map(|s| s.success()).unwrap_or(false);
    let _ = std::fs::remove_dir_all(&work);
    if ok {
        Ok(())
    } else {
        Err("inject_initrd: appending the cpio failed (cpio/gzip missing?)".into())
    }
}

/// Script RUN INSIDE THE GOLDEN VM: installs open-iscsi + overlayroot + update-initramfs (packages only;
/// the broom-wb hook + overlayroot.conf are injected into the initrd by the server → no golden rebuild when tuning).
/// Usage: curl -fsSL http://<server>/broom-prep | sudo bash
/// __IP__ is replaced by the server IP.
pub const PREP_SCRIPT: &str = r#"#!/usr/bin/env bash
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
"#;

#[cfg(test)]
mod tests {
    /// The hook injected into the initrd must be valid sh (error = client hangs in the initramfs).
    #[test]
    fn hook_syntax() {
        for s in [super::BROOM_ISCSI, super::CACHE_SCRIPT, super::PREP_SCRIPT] {
            // Wrapped in a never-called function: parsed only, even by a shell that ignores -n (busybox 1.30).
            let ok = std::process::Command::new("sh").args(["-n", "-c", &format!("broom_syntax_check(){{\n{s}\n}}")]).status().unwrap();
            assert!(ok.success());
        }
    }

    /// Cache delta (cut from CACHE_SCRIPT): the old SSD copy is patched in place from the "iSCSI" golden — only the
    /// chunks whose hash changed; the result equals the new golden. Golden not matching the manifest → no .sha256.
    #[test]
    fn cache_delta_patch() {
        const C: usize = crate::publish::MANIFEST_CHUNK;
        let s = super::CACHE_SCRIPT;
        let part = &s[s.find("M=/run/broom-golden.chunks").unwrap()..s.find("rm -f $C/*.img $C/*.sha256 $C/*.chunks").unwrap()];
        let d = std::env::temp_dir().join("broom_t_cache_delta");
        let old = [vec![1u8; C], vec![2u8; C], vec![3u8; C], vec![4u8; 500]].concat();
        let run = |new: &[u8], iscsi: &[u8]| {
            let _ = std::fs::remove_dir_all(&d);
            for p in ["c", "run", "srv"] {
                std::fs::create_dir_all(d.join(p)).unwrap();
            }
            std::fs::write(d.join("c/ubuntu.img"), &old).unwrap();
            crate::publish::write_manifest(&d.join("c/ubuntu.img"), &d.join("c")).unwrap();
            std::fs::rename(d.join("c/golden.chunks"), d.join("c/ubuntu.chunks")).unwrap();
            std::fs::write(d.join("srv/golden.img"), new).unwrap();
            crate::publish::write_manifest(&d.join("srv/golden.img"), &d.join("srv")).unwrap();
            std::fs::write(d.join("iscsi.dev"), iscsi).unwrap();
            let body = part.replace("/run/", &format!("{}/run/", d.display()));
            let sh = format!(
                "C={d}/c; NAME=ubuntu; HASH=h1; SIZE={size}; GOLDEN={d}/iscsi.dev; SRV=x\n\
                 log(){{ echo \"$*\" >> {d}/log; }}\n\
                 wget(){{ cp {d}/srv/golden.chunks \"$3\"; }}\n{body}",
                d = d.display(),
                size = new.len()
            );
            std::process::Command::new("sh").args(["-c", &sh]).status().unwrap();
            let img_ok = std::fs::read(d.join("c/ubuntu.img")).unwrap() == new;
            let valid = std::fs::read_to_string(d.join("c/ubuntu.sha256")).unwrap_or_default().trim() == "h1";
            (img_ok, valid, std::fs::read_to_string(d.join("log")).unwrap_or_default())
        };
        // Chunk 1 changed, chunk 2 now zero, the tail grew: 3 chunks patched, the rest untouched.
        let new = [vec![1u8; C], vec![9u8; C], vec![0u8; C], vec![4u8; 3000]].concat();
        let (img_ok, valid, log) = run(&new, &new);
        assert!(img_ok && valid && log.contains("3 chunks"), "{log}");
        // The attached golden is not the manifest's → the cache stays invalid (the script then does a full copy).
        let (_, valid, log) = run(&new, &old);
        assert!(!valid && log.contains("does not match"), "{log}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
