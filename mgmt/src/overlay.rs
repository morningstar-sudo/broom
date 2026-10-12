// overlay.rs — golden raw disk → extract kernel/initrd for iPXE + prep script that bakes the overlay hook.
//
// Model: golden = raw disk (from the uploaded VM), served over iSCSI as shared RO. Clients boot with
// the golden's own kernel/initrd; the initrd (open-iscsi + overlayroot + reset hook baked in the
// golden VM by scripts/prep-linux.sh) does: attach iSCSI (iBFT set by iPXE sanhook) → mount root RO =
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

/// Extract vmlinuz + initrd.img from the golden → `dst` (publish.rs stages it, then swaps it in as tftp/broom/<name>/),
/// inject the broom hook into the initrd. Returns the root UUID. Blocking.
pub fn build_boot(img: &Path, name: &str, dst: &Path) -> Result<String, String> {
    let dst = dst.to_string_lossy().into_owned();
    let b = crate::linuxfs::extract_boot(&img.to_string_lossy(), &dst)?;
    tracing::info!(
        "image {name}: kernel {} + initrd copied, root UUID {}",
        b.kver,
        b.root_uuid
    );

    // Inject the broom-wb hook + overlayroot.conf into the initrd (append a cpio → overrides the golden's copy).
    // → tuning reset/overlay = edit scripts/linux-*.sh, rebuild the binary, Publish again — NO golden rebuild.
    inject_initrd(&format!("{dst}/initrd.img"), name)?;
    Ok(b.root_uuid)
}

/// overlayroot.conf injected into the initrd (owned by the server → tune without rebuilding the golden).
/// root RO (iSCSI) + upper on the local SSD LABEL=broomwb (the broom-wb hook formats it).
const OVERLAYROOT_CONF: &str = "overlayroot=\"device:dev=/dev/disk/by-label/broomwb,recurse=0\"\noverlayroot_cfgdisk=\"disabled\"\n";

/// COMBINED hook, OVERRIDES `scripts/local-top/iscsi` (that file is ALREADY in ORDER so it surely runs —
/// a self-added script NOT in ORDER is skipped by initramfs-tools). Every boot:
///  1. The Broom SSD (local disk present BEFORE attaching iSCSI), shared with the Windows stage by partition names:
///     broomwb (writeback, mkfs every boot) + broomcache (persistent: golden cache + /games). None yet → laid out once.
///  2. Cache HIT (broomcache/<name>.sha256 == broom.hash on cmdline) → losetup RO the copy
///     on the SSD as root, NO iSCSI attach (zero network/server load).
///     MISS → bring up the NIC + iscsistart -b (iBFT from iPXE sanhook) as before; broom-cache.service (golden)
///     copies golden iSCSI → SSD in the background for the next boot.
///  3. No sfdisk/losetup (old golden prep), unregistered machine or several disks → writeback in zram, no disk touched.
fn broom_iscsi() -> &'static str {
    crate::assets::text("scripts/linux-iscsi-hook.sh")
}

/// Runs in the REAL ROOT: broom-cache.service (baked into the golden via broom-prep) calls
/// /run/broom-cache.sh copied out by the initrd hook → logic still injected by the server, tune without a golden rebuild.
/// Mount /games from the SSD cache; MISS → copy golden iSCSI → SSD for the next boot.
fn cache_script() -> &'static str {
    crate::assets::text("scripts/linux-cache.sh")
}

/// Append one cpio.gz (overrides local-top/iscsi = attach golden + SSD writeback; + /etc/overlayroot.conf)
/// to the end of the initrd → the kernel concatenates cpios, the later one overrides the golden's.
fn inject_initrd(initrd: &str, name: &str) -> Result<(), String> {
    use crate::archive::Entry;
    use std::io::Write;
    // OVERRIDE scripts/local-top/iscsi (already in ORDER → surely runs). Does both attach + writeback.
    let cpio = crate::archive::cpio_gz(&[
        Entry {
            path: "scripts",
            mode: 0o040755,
            data: &[],
        },
        Entry {
            path: "scripts/local-top",
            mode: 0o040755,
            data: &[],
        },
        Entry {
            path: "scripts/local-top/iscsi",
            mode: 0o100755,
            data: broom_iscsi().as_bytes(),
        },
        Entry {
            path: "scripts/broom-cache.sh",
            mode: 0o100644,
            data: cache_script().as_bytes(),
        },
        Entry {
            path: "etc",
            mode: 0o040755,
            data: &[],
        },
        Entry {
            path: "etc/overlayroot.conf",
            mode: 0o100644,
            data: OVERLAYROOT_CONF.as_bytes(),
        },
    ]);
    std::fs::OpenOptions::new()
        .append(true)
        .open(initrd)
        .and_then(|mut f| f.write_all(&cpio))
        .map_err(|e| format!("inject_initrd {name}: append to {initrd}: {e}"))
}

/// Script RUN INSIDE THE GOLDEN VM: installs open-iscsi + overlayroot + update-initramfs (packages only;
/// the broom-wb hook + overlayroot.conf are injected into the initrd by the server → no golden rebuild when tuning).
/// Usage: curl -fsSL http://<server>/broom-prep | sudo bash
/// __IP__ is replaced by the server IP.
pub fn prep_script() -> &'static str {
    crate::assets::text("scripts/prep-linux.sh")
}

#[cfg(test)]
#[path = "../tests/unit/overlay.rs"]
mod tests;
