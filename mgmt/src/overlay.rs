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
    tracing::info!("image {name}: kernel {} + initrd copied, root UUID {}", b.kver, b.root_uuid);

    // Inject the broom-wb hook + overlayroot.conf into the initrd (append a cpio → overrides the golden's copy).
    // → tuning reset/overlay = edit scripts/linux-*.sh, rebuild the binary, Publish again — NO golden rebuild.
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
const BROOM_ISCSI: &str = include_str!("../scripts/linux-iscsi-hook.sh");

/// Runs in the REAL ROOT: broom-cache.service (baked into the golden via broom-prep) calls
/// /run/broom-cache.sh copied out by the initrd hook → logic still injected by the server, tune without a golden rebuild.
/// Mount /games from the SSD cache; MISS → copy golden iSCSI → SSD for the next boot.
const CACHE_SCRIPT: &str = include_str!("../scripts/linux-cache.sh");

/// Append one cpio.gz (overrides local-top/iscsi = attach golden + SSD writeback; + /etc/overlayroot.conf)
/// to the end of the initrd → the kernel concatenates cpios, the later one overrides the golden's.
fn inject_initrd(initrd: &str, name: &str) -> Result<(), String> {
    use crate::archive::Entry;
    use std::io::Write;
    // OVERRIDE scripts/local-top/iscsi (already in ORDER → surely runs). Does both attach + writeback.
    let cpio = crate::archive::cpio_gz(&[
        Entry { path: "scripts", mode: 0o040755, data: &[] },
        Entry { path: "scripts/local-top", mode: 0o040755, data: &[] },
        Entry { path: "scripts/local-top/iscsi", mode: 0o100755, data: BROOM_ISCSI.as_bytes() },
        Entry { path: "scripts/broom-cache.sh", mode: 0o100644, data: CACHE_SCRIPT.as_bytes() },
        Entry { path: "etc", mode: 0o040755, data: &[] },
        Entry { path: "etc/overlayroot.conf", mode: 0o100644, data: OVERLAYROOT_CONF.as_bytes() },
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
pub const PREP_SCRIPT: &str = include_str!("../scripts/prep-linux.sh");

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

    /// Cache copy (cut from CACHE_SCRIPT): the old copy goes, the golden is copied whole and hashed on the way; valid
    /// (.sha256 written) only when hash and size match.
    #[test]
    fn cache_whole_copy() {
        let s = super::CACHE_SCRIPT;
        let part = &s[s.find("# Whole-file copy (no delta)").unwrap()..];
        let d = std::env::temp_dir().join("broom_t_cache_copy");
        let golden: Vec<u8> = (0..5_000_000u32).map(|i| (i % 253) as u8).collect();
        let run = |hash: &str| {
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(d.join("c")).unwrap();
            for f in ["old.img", "old.sha256", "old.chunks"] {
                std::fs::write(d.join("c").join(f), "x").unwrap();
            }
            std::fs::write(d.join("iscsi.dev"), &golden).unwrap();
            let sh = format!(
                "C={d}/c; NAME=ubuntu; HASH={hash}; SIZE={size}; GOLDEN={d}/iscsi.dev\nlog(){{ echo \"$*\" >> {d}/log; }}\n{part}",
                d = d.display(),
                size = golden.len()
            );
            std::process::Command::new("sh").args(["-c", &sh]).status().unwrap();
            let img_ok = std::fs::read(d.join("c/ubuntu.img")).is_ok_and(|b| b == golden);
            let valid = std::fs::read_to_string(d.join("c/ubuntu.sha256")).unwrap_or_default().trim() == hash;
            let old_gone = !d.join("c/old.img").exists() && !d.join("c/old.chunks").exists();
            (img_ok, valid, old_gone)
        };
        std::fs::write(d.with_extension("src"), &golden).unwrap();
        let h = crate::hash::file_hash(&d.with_extension("src").to_string_lossy()).unwrap();
        let _ = std::fs::remove_file(d.with_extension("src"));
        assert_eq!(run(&h), (true, true, true));
        assert_eq!(run("0000"), (false, false, true), "hash mismatch → discarded");
        let _ = std::fs::remove_dir_all(&d);
    }
}
