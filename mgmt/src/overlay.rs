// overlay.rs — golden raw disk → extract kernel/initrd for iPXE + prep script that bakes the overlay hook.
//
// Model (replaced LTSP): golden = raw disk (from vmdk), served over iSCSI as shared RO. Clients boot with
// the golden's own kernel/initrd; the initrd (open-iscsi + overlayroot + reset hook baked in the
// golden VM via PREP_SCRIPT) does: attach iSCSI (iBFT set by iPXE sanhook) → mount root RO =
// lower → build the local SSD writeback (reset every boot) → overlayfs → boot. Writeback goes to the SSD,
// not RAM. Users/apps are baked into the img.
//
// Server side: uses **libguestfs** (virt-ls/virt-copy-out/guestfish) to read the golden — an ISOLATED
// appliance that handles LVM/ext4/xfs itself and does NOT touch the host's LVM/mounts (the server runs LVM too;
// mounting the partition directly hits 'LVM2_member' + risk of duplicate VGs). Extracts the newest vmlinuz+initrd to
// /srv/tftp/broom/<name>/ + reads the root UUID (for boot_script root=UUID=).
//
// ⚠ The overlay/iSCSI part inside the initrd (PREP_SCRIPT) is the riskiest part — MUST be PoC'd + tuned on
// a real server (B5).
use std::path::Path;
use std::process::Command;

/// Run a command, return stdout (trimmed). Failure → Err(stderr).
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

/// Run guestfish --ro with a script on stdin, return stdout. Failure → Err(stderr).
fn guestfish_stdin(img: &str, script: &str) -> Result<String, String> {
    use std::io::Write;
    let mut child = Command::new("guestfish")
        .args(["--ro", "-a", img])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("guestfish: {e}"))?;
    child.stdin.take().ok_or("stdin")?.write_all(script.as_bytes()).map_err(|e| e.to_string())?;
    let o = child.wait_with_output().map_err(|e| e.to_string())?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
    }
}

/// From guestfish output ("@@<fs>" then vmlinuz-* paths) pick (fs, path) of the NEWEST kernel version.
fn pick_kernel(listing: &str) -> Option<(String, String)> {
    let mut fs = "";
    let mut best: Option<(String, String)> = None;
    for l in listing.lines().map(str::trim) {
        if let Some(f) = l.strip_prefix("@@") {
            fs = f;
        } else if let Some((_, kv)) = l.rsplit_once("/vmlinuz-") {
            let newer = best.as_ref().map_or(true, |(_, p)| {
                ver_key(kv) > ver_key(p.rsplit_once("/vmlinuz-").unwrap().1)
            });
            if newer {
                best = Some((fs.to_string(), l.to_string()));
            }
        }
    }
    best
}

/// Version sort key like `sort -V`: split digits/letters, digits compare by value (5.15.0-119 > 5.15.0-91).
fn ver_key(v: &str) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut digit = false;
    for c in v.chars() {
        if !cur.is_empty() && c.is_ascii_digit() != digit {
            out.push(if digit { (cur.parse().unwrap_or(0), String::new()) } else { (0, cur.clone()) });
            cur.clear();
        }
        digit = c.is_ascii_digit();
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(if digit { (cur.parse().unwrap_or(0), String::new()) } else { (0, cur) });
    }
    out
}

/// Extract vmlinuz + initrd.img from the golden (via libguestfs) → /srv/tftp/broom/<name>/. Returns the root UUID.
/// Blocking, needs root (libguestfs appliance).
pub fn build_boot(img: &Path, name: &str) -> Result<String, String> {
    let img = img.to_string_lossy().to_string();
    let dst = format!("/srv/tftp/broom/{name}");
    std::fs::create_dir_all(&dst).map_err(|e| e.to_string())?;

    // 1. Look for the kernel on EVERY filesystem (not via fstab: /boot may be its own partition —
    //    Ubuntu Server LVM — and commented out in fstab → inspect doesn't mount it → empty /boot).
    //    One guestfish session: mount-ro each fs, glob /vmlinuz-* (/boot partition) + /boot/vmlinuz-* (inside root).
    let fss = out("virt-filesystems", &["-a", &img])
        .map_err(|e| format!("virt-filesystems: {e} (is libguestfs-tools installed?)"))?;
    let mut script = String::from("run\n");
    for fs in fss.lines().map(str::trim).filter(|s| !s.is_empty()) {
        script.push_str(&format!(
            "echo @@{fs}\n-mount-ro {fs} /\n-glob-expand /vmlinuz-*\n-glob-expand /boot/vmlinuz-*\n-umount-all\n"
        ));
    }
    let listing = guestfish_stdin(&img, &script)?;
    let (fs, kpath) = pick_kernel(&listing).ok_or(format!(
        "golden has no vmlinuz-* on any filesystem (fs: {}) — does the golden have a kernel installed?",
        fss.replace('\n', " ")
    ))?;
    // kpath = /boot/vmlinuz-X or /vmlinuz-X → initrd in the same directory.
    let (kdir, kv) = kpath.rsplit_once("/vmlinuz-").ok_or("unexpected kernel path")?;

    // 2. Copy kernel + initrd to dst (one guestfish session) then rename to the standard names.
    guestfish_stdin(
        &img,
        &format!(
            "run\nmount-ro {fs} /\ncopy-out {kdir}/vmlinuz-{kv} {dst}\ncopy-out {kdir}/initrd.img-{kv} {dst}\n"
        ),
    )
    .map_err(|e| format!("copy kernel/initrd from {fs}: {e}"))?;
    let vm_src = format!("{dst}/vmlinuz-{kv}");
    let ir_src = format!("{dst}/initrd.img-{kv}");
    if !Path::new(&vm_src).exists() {
        return Err(format!("virt-copy-out did not create {vm_src} (wrong kernel version detected?)"));
    }
    if !Path::new(&ir_src).exists() {
        return Err(format!("virt-copy-out did not create {ir_src}"));
    }
    std::fs::rename(&vm_src, format!("{dst}/vmlinuz"))
        .map_err(|e| format!("rename {vm_src}: {e}"))?;
    let initrd = format!("{dst}/initrd.img");
    std::fs::rename(&ir_src, &initrd)
        .map_err(|e| format!("rename {ir_src}: {e}"))?;

    // Inject the broom-wb hook + overlayroot.conf into the initrd (append a cpio → overrides the golden's copy).
    // → tuning reset/overlay = edit Rust + Publish again, NO golden rebuild.
    inject_initrd(&initrd, name)?;

    // 3. Root UUID: guestfish inspect-os → root device (LV or partition) → vfs-uuid.
    let root_dev = out("guestfish", &["--ro", "-a", &img, "run", ":", "inspect-os"])
        .map_err(|e| format!("guestfish inspect-os: {e}"))?;
    let root_dev = root_dev
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or("inspect-os returned no root device".to_string())?;
    let uuid = out("guestfish", &["--ro", "-a", &img, "run", ":", "vfs-uuid", &root_dev])
        .map_err(|e| format!("guestfish vfs-uuid {root_dev}: {e}"))?;
    if uuid.is_empty() {
        return Err(format!("could not read the root UUID ({root_dev})"));
    }
    Ok(uuid)
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
///  3. No sfdisk/losetup in the initramfs (old golden prep) → writeback on the whole disk, no cache.
const BROOM_ISCSI: &str = r#"#!/bin/sh
case "$1" in prereqs) echo ""; exit 0;; esac
# Log to console + /run/broom-wb.log (/run moves to the real root → readable after boot).
log(){ echo "broom: $*"; echo "$*" >> /run/broom-wb.log; }
WB_GB=30   # writeback size of p1; the rest = cache + /games. Change = wipefs the disk to repartition.
modprobe iscsi_tcp 2>/dev/null
modprobe iscsi_ibft 2>/dev/null
NAME=""; HASH=""; SIZE=""; NOCACHE=""
for a in $(cat /proc/cmdline); do
  case "$a" in
    broom.name=*) NAME=${a#*=};; broom.hash=*) HASH=${a#*=};; broom.size=*) SIZE=${a#*=};;
    broom.nocache) NOCACHE=1;;
  esac
done
# LOCAL disks = physical disks present BEFORE attaching iSCSI (golden iSCSI not visible yet). Skip removable USB.
localdisks=""
for d in /sys/block/*; do
  n=${d##*/}
  case "$n" in loop*|ram*|dm-*|sr*|nbd*|md*|fd*|zram*) continue;; esac
  [ "$(cat "$d/removable" 2>/dev/null)" = 1 ] && continue
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
  if [ -z "$cache" ]; then
    for n in $localdisks; do
      # Disk too small (< WB + 8GB) → skip, fall back to whole-disk writeback.
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
if [ -n "$wb" ]; then
  mkfs.ext4 -qF -L broomwb -O ^has_journal -E nodiscard "$wb" 2>/dev/null || { log "mkfs $wb failed"; wb=""; }
fi
# Could not partition (old golden prep / small disk) → writeback on the WHOLE DISK as before, no cache.
if [ -z "$wb" ] && [ -z "$cache" ]; then
  for n in $localdisks; do
    mkfs.ext4 -qF -L broomwb -O ^has_journal -E nodiscard "/dev/$n" 2>/dev/null && { wb=/dev/$n; break; }
    log "mkfs /dev/$n failed (is mkfs.ext4 in the initramfs?)"
  done
fi
# Fallback without SSD: zram (compressed RAM). ponytail: needs the zram module in the initramfs (new broom-prep).
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
    # ponytail: trust the sha256 written when the copy finished, don't rehash the whole golden every boot. Suspect corruption → broom.nocache.
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
"#;

/// Runs in the REAL ROOT: broom-cache.service (baked into the golden via broom-prep) calls
/// /run/broom-cache.sh copied out by the initrd hook → logic still injected by the server, tune without a golden rebuild.
/// Mount /games from the SSD cache; MISS → copy golden iSCSI → SSD for the next boot.
const CACHE_SCRIPT: &str = r#"#!/bin/sh
. /run/broom-cache.env
C=/run/broomcache
log(){ echo "$*" >> /run/broom-wb.log; }
mkdir -p /games && mount --bind $C/games /games && chmod 1777 $C/games
[ "$MODE" = miss ] && [ -b "$GOLDEN" ] && [ -n "$HASH" ] && [ -n "$SIZE" ] || exit 0
# ponytail: spread over 0–5 minutes so all clients don't pull the golden at once after Publish; still congested →
# limit bandwidth on the server side.
sleep $(( $(od -An -N2 -tu2 /dev/urandom) % 300 ))
# Only keep the cache of the running image (remove old images + partial files).
rm -f $C/*.img $C/*.sha256 $C/*.tmp
avail=$(df -B1 --output=avail $C | tail -1)
[ "$avail" -gt "$SIZE" ] || { log "cache: not enough SSD space ($avail < $SIZE)"; exit 0; }
log "cache: copying $GOLDEN -> $C/$NAME.img ..."
# count_bytes: a zram backstore can be larger than the source file (page rounding) → cut exactly SIZE so the hash matches.
ionice -c3 nice -n19 dd if="$GOLDEN" of="$C/$NAME.tmp" bs=4M iflag=count_bytes count="$SIZE" status=none \
  || { log "cache: dd failed"; exit 0; }
h=$(ionice -c3 nice -n19 sha256sum "$C/$NAME.tmp" | cut -d' ' -f1)
if [ "$h" = "$HASH" ]; then
  mv "$C/$NAME.tmp" "$C/$NAME.img" && sync && echo "$HASH" > "$C/$NAME.sha256" && sync
  log "cache: OK — next boot runs from the SSD"
else
  rm -f "$C/$NAME.tmp"; log "cache: hash mismatch ($h) — discarded"
fi
"#;

/// Append one cpio.gz (overrides local-top/iscsi = attach golden + SSD writeback; + /etc/overlayroot.conf)
/// to the end of the initrd → the kernel concatenates cpios, the later one overrides the golden's.
fn inject_initrd(initrd: &str, name: &str) -> Result<(), String> {
    let work = format!("/tmp/broom-inject-{name}");
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
    let _ = Command::new("chmod").args(["0755", &iscsi_hook]).status();
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
/// ⚠ DRAFT — tune on the PoC server (B5). __IP__ is replaced by the server IP.
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
            let ok = std::process::Command::new("sh").args(["-n", "-c", s]).status().unwrap();
            assert!(ok.success());
        }
    }

    /// Separate /boot partition (Ubuntu Server LVM) + pick the newest version numerically.
    #[test]
    fn pick_kernel_newest() {
        let l = "@@/dev/sda1\n@@/dev/sda2\n/vmlinuz-5.15.0-91-generic\n/vmlinuz-5.15.0-119-generic\n\
                 @@/dev/ubuntu-vg/ubuntu-lv\n";
        assert_eq!(
            super::pick_kernel(l),
            Some(("/dev/sda2".into(), "/vmlinuz-5.15.0-119-generic".into()))
        );
        // kernel inside the root's /boot.
        let l = "@@/dev/sda1\n/boot/vmlinuz-6.8.0-45-generic\n";
        assert_eq!(super::pick_kernel(l).unwrap().1, "/boot/vmlinuz-6.8.0-45-generic");
        assert_eq!(super::pick_kernel("@@/dev/sda1\n"), None);
    }
}
