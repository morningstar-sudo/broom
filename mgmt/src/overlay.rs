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
///  1. The Broom SSD (local disk present BEFORE attaching iSCSI), shared with the Windows stage by partition names:
///     broomwb (writeback, mkfs every boot) + broomcache (persistent: golden cache + /games). None yet → laid out once.
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

    /// Broom SSD layout in the Linux hook (cut from BROOM_ISCSI, fake /sys/block; the sfdisk mock creates the
    /// partitions its script names): shared with the Windows stage by partition names — Windows' layout without the
    /// Linux part is laid out again, a shared one is used as is (cache formatted only once), several unknown disks
    /// stay untouched.
    #[test]
    fn hook_shared_ssd_layout() {
        let s = super::BROOM_ISCSI;
        let block = &s[s.find("has_label(){").unwrap()..s.find("# mkfs discards").unwrap()];
        // disks: (name, partition names already there); formatted: devices whose filesystem label is broomcache.
        let run = |tag: &str, disks: &[(&str, &[&str])], reg: &str, lx: u32, formatted: &[&str]| {
            let d = std::env::temp_dir().join(format!("broom_t_hook_{tag}"));
            let _ = std::fs::remove_dir_all(&d);
            let sys = d.join("sys");
            for (n, parts) in disks {
                std::fs::create_dir_all(sys.join(n)).unwrap();
                std::fs::write(sys.join(n).join("size"), format!("{}\n", 500u64 * 2097152)).unwrap();
                for (i, p) in parts.iter().enumerate() {
                    std::fs::create_dir_all(sys.join(format!("{n}/{n}{}", i + 1))).unwrap();
                    std::fs::write(sys.join(format!("{n}/{n}{}/uevent", i + 1)), format!("PARTNAME={p}\n")).unwrap();
                }
            }
            let names: Vec<&str> = disks.iter().map(|(n, _)| *n).collect();
            // mkfs.ext4: a dot is not allowed in a dash function name → an executable on PATH.
            std::fs::create_dir_all(d.join("bin")).unwrap();
            std::fs::write(d.join("bin/mkfs.ext4"), format!("#!/bin/sh\nfor a; do :; done; echo \"$a\" >> {}/mkfs\n", d.display())).unwrap();
            std::fs::set_permissions(d.join("bin/mkfs.ext4"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            let sh = format!(
                "cd {d}; PATH={d}/bin:$PATH; WB_GB=30; REG={reg}; LX={lx}; SSD={ssd}; localdisks=\"{disks}\"\nlog(){{ echo \"$*\" >> log; }}\n\
                 losetup(){{ :; }}; udevadm(){{ :; }}; sleep(){{ :; }}\n\
                 has_label(){{ case \" {fmt} \" in *\" $1 \"*) [ \"$2\" = broomcache ];; *) false;; esac; }}\n\
                 sfdisk(){{ for a; do :; done; n=${{a##*/}}; cat > sfdisk.$n; rm -rf {sys}/$n/$n[0-9]*; i=0\n\
                   sed -n 's/.*name=\\([A-Za-z]*\\).*/\\1/p' sfdisk.$n | while read p; do i=$((i+1)); mkdir -p {sys}/$n/$n$i; echo PARTNAME=$p > {sys}/$n/$n$i/uevent; done; }}\n\
                 {body}\necho \"WB=$wb CACHE=$cache\"",
                d = d.display(),
                ssd = if tag == "nossd" { "0" } else { "" },
                disks = names.join(" "),
                fmt = formatted.join(" "),
                sys = sys.display(),
                // the script's has_label (reads a real superblock) is replaced by the mock above
                body = block.split_once('\n').unwrap().1.replace("/sys/block/", &format!("{}/", sys.display()))
            );
            let o = std::process::Command::new("sh").args(["-c", &sh]).output().unwrap();
            let rd = |f: &str| std::fs::read_to_string(d.join(f)).unwrap_or_default();
            let out = (String::from_utf8_lossy(&o.stdout).trim().to_string(), rd("sfdisk.sda"), rd("mkfs"));
            let _ = std::fs::remove_dir_all(&d);
            out
        };
        const WIN: &[&str] = &["BROOMEFI", "BROOMWIN"];
        const ALL: &[&str] = &["BROOMEFI", "BROOMWIN", "broomwb", "broomcache"];
        // Blank disk on a registered machine → the shared layout, the cache formatted.
        let (out, lay, mkfs) = run("blank", &[("sda", &[])], "1", 55, &[]);
        assert_eq!(out, "WB=/dev/sda3 CACHE=/dev/sda4");
        assert!(lay.contains("size=444GiB, type=EBD0") && lay.contains("name=broomcache") && mkfs.trim() == "/dev/sda4", "{lay}{mkfs}");
        // Laid out for Windows only (no Linux image then / too small) → NEVER laid out again here (Windows would wipe it
        // back on its next boot, and its goldens + bases would be lost): RAM only, disk untouched.
        assert_eq!(run("winonly", &[("sda", WIN)], "1", 55, &[]), ("WB= CACHE=".into(), String::new(), String::new()));
        // A broken Linux-only layout (no cache partition, no Windows part) → laid out again.
        let (out, lay, _) = run("broken", &[("sda", &["broomwb"])], "", 55, &[]);
        assert!(out == "WB=/dev/sda3 CACHE=/dev/sda4" && lay.contains("name=BROOMWIN"), "{out}{lay}");
        // Shared layout already there → used as is; the cache formatted only when it isn't yet.
        assert_eq!(run("ready", &[("sda", ALL)], "", 55, &["/dev/sda4"]), ("WB=/dev/sda3 CACHE=/dev/sda4".into(), String::new(), String::new()));
        assert_eq!(run("unfmt", &[("sda", ALL)], "", 55, &[]).2.trim(), "/dev/sda4", "laid out by Windows → formatted here");
        // Two unknown disks → nothing touched (zram).
        assert_eq!(run("two", &[("sda", &[]), ("sdb", &[])], "1", 55, &[]).0, "WB= CACHE=");
        // No Linux share (boot script of an older version) → Linux takes the disk as before.
        let (out, lay, _) = run("nolx", &[("sda", &[])], "1", 0, &[]);
        assert!(out == "WB=/dev/sda1 CACHE=/dev/sda2" && !lay.contains("BROOMWIN"), "{out}{lay}");
        // Image set to not use the SSD → no disk touched, even a blank one or a ready Broom SSD.
        assert_eq!(run("nossd", &[("sda", &[])], "1", 55, &[]), ("WB= CACHE=".into(), String::new(), String::new()));
        assert_eq!(run("nossd", &[("sda", ALL)], "1", 55, &[]), ("WB= CACHE=".into(), String::new(), String::new()));
    }

    /// Cache script (cut from CACHE_SCRIPT, wget mocked): copies the server no longer lists (or of another version)
    /// go, the others stay; the golden is copied whole and hashed on the way, valid only when hash and size match. No
    /// answer from the server → nothing removed.
    #[test]
    fn cache_list_and_copy() {
        let s = super::CACHE_SCRIPT;
        let part = &s[s.find("# Every image this machine uses").unwrap()..];
        let d = std::env::temp_dir().join("broom_t_cache_copy");
        let golden: Vec<u8> = (0..5_000_000u32).map(|i| (i % 253) as u8).collect();
        let run = |hash: &str, list: Option<&str>| {
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(d.join("c")).unwrap();
            for (f, v) in [("kept.img", "k"), ("kept.sha256", "hk\n"), ("gone.img", "g"), ("gone.sha256", "hg\n"), ("half.img", "x"), ("x.chunks", "")] {
                std::fs::write(d.join("c").join(f), v).unwrap();
            }
            std::fs::write(d.join("iscsi.dev"), &golden).unwrap();
            let wget = match list {
                Some(l) => format!("wget(){{ printf '{l}'; }}"),
                None => "wget(){ return 4; }".into(),
            };
            let sh = format!(
                "C={d}/c; NAME=ubuntu; HASH={hash}; SIZE={size}; GOLDEN={d}/iscsi.dev; MODE=miss; SRV=x\n\
                 log(){{ echo \"$*\" >> {d}/log; }}; sleep(){{ :; }}; {wget}\n{part}",
                d = d.display(),
                size = golden.len(),
                part = part.replace("[ -b \"$GOLDEN\" ]", "[ -f \"$GOLDEN\" ]")
            );
            std::process::Command::new("sh").args(["-c", &sh]).status().unwrap();
            let has = |f: &str| d.join("c").join(f).exists();
            let img_ok = std::fs::read(d.join("c/ubuntu.img")).is_ok_and(|b| b == golden);
            let valid = std::fs::read_to_string(d.join("c/ubuntu.sha256")).unwrap_or_default().trim() == hash;
            (img_ok, valid, has("kept.img"), has("gone.img"), has("half.img") || has("x.chunks"))
        };
        std::fs::write(d.with_extension("src"), &golden).unwrap();
        let h = crate::hash::file_hash(&d.with_extension("src").to_string_lossy()).unwrap();
        let _ = std::fs::remove_file(d.with_extension("src"));
        let list = format!("kept hk\\nubuntu {h}\\n");
        assert_eq!(run(&h, Some(&list)), (true, true, true, false, false), "listed copy kept, unlisted one removed");
        assert_eq!(run(&h, None), (true, true, true, true, false), "no answer → nothing removed");
        assert_eq!(run("0000", Some(&list)).1, false, "hash mismatch → discarded");
        let _ = std::fs::remove_dir_all(&d);
    }
}
