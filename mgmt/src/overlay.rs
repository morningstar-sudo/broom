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

/// Extract vmlinuz + initrd.img from the golden → <home>/tftp/broom/<name>/, inject the broom hook into the
/// initrd. Returns the root UUID. Blocking.
pub fn build_boot(img: &Path, name: &str) -> Result<String, String> {
    let dst = crate::tftp_dir().join("broom").join(name).to_string_lossy().into_owned();
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

    /// Cache delta (cut from CACHE_SCRIPT): the old SSD copy is patched in place from the "iSCSI" golden — only the
    /// chunks whose hash changed; the result equals the new golden. Golden not matching the manifest → no .sha256.
    #[test]
    fn cache_delta_patch() {
        const C: usize = crate::chunks::MANIFEST_CHUNK;
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
            crate::chunks::write_manifest(&d.join("c/ubuntu.img"), &d.join("c")).unwrap();
            std::fs::rename(d.join("c/golden.chunks"), d.join("c/ubuntu.chunks")).unwrap();
            std::fs::write(d.join("srv/golden.img"), new).unwrap();
            crate::chunks::write_manifest(&d.join("srv/golden.img"), &d.join("srv")).unwrap();
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
