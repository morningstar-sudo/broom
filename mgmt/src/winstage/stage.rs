// winstage/stage.rs — the Windows client STAGE: a Linux kernel + initrd that runs on every client boot (scripts/
// stage.sh: partition/update the SSD, rebuild the child VHDX, BootNext into Windows). Built by CI into a bundle
// (`build-stage`), pinned in the binary (stage.pin), installed on the server by ensure_stage — never built there.
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::{run, stable_key, stage_dir};

fn write_exec(path: &str, body: &str) -> Result<(), String> {
    std::fs::write(path, body).map_err(|e| format!("{path}: {e}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

/// mkinitramfs hook for the stage: tools + modules for partitioning / NTFS / download / EFI.
/// Copied to /broom/bin (ahead of busybox in PATH — needs the full od/tar/wget...).
const STAGE_HOOK: &str = include_str!("../../scripts/stage-hook.sh");

/// Tools of the machine building the bundle that the hook copies into the stage (/broom/bin, ahead of busybox).
const STAGE_TOOLS: &str = "sfdisk blkid mkfs.fat mkntfs ntfsfix efibootmgr wget sha256sum tar gzip od dd awk";

/// Stage init-premount script — runs on the client, does NOT mount root; reboots into Windows when done.
const STAGE_SCRIPT: &str = include_str!("../../scripts/stage.sh");

/// Build the stage: kernel `kernel` (None = the running one) + initrd (mkinitramfs, own confdir) → `sd`.
/// Kernel + script + hook unchanged → keep the previous build (mkinitramfs MODULES=most takes about a minute).
/// Returns true if freshly built.
fn build_stage(kernel: Option<&str>, sd: &str) -> Result<bool, String> {
    let kv = match kernel {
        Some(k) => k.to_string(),
        None => std::fs::read_to_string("/proc/sys/kernel/osrelease").map_err(|e| format!("kernel release: {e}"))?.trim().to_string(),
    };
    run("modinfo", &["-k", &kv, "ntfs3"]).map_err(|_| format!("kernel {kv} has no ntfs3 module — the stage must write NTFS"))?;
    // zstd: multithreaded compression + faster decompression than gzip; a builder without zstd → gzip.
    let compress = if run("sh", &["-c", "command -v zstd"]).is_ok() { "zstd" } else { "gzip" };
    let initramfs_conf = format!("MODULES=most\nBUSYBOX=y\nCOMPRESS={compress}\n");
    let hook = STAGE_HOOK.replace("__TOOLS__", STAGE_TOOLS);
    // Where each tool resolves on the builder is part of the key: a tool installed later (e.g. wget) → rebuilt.
    let tools = run("sh", &["-c", &format!("for b in {STAGE_TOOLS}; do command -v $b; done; true")]).unwrap_or_default();
    let key = stable_key(&[kv.as_str(), initramfs_conf.as_str(), hook.as_str(), STAGE_SCRIPT, tools.as_str()]);
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
    // at build time, not when a client gets stuck in a shell.
    let list = run("lsinitramfs", &[&tmp])?;
    // wget: busybox's (initramfs build) lacks --post-file / -T → the driver list needs GNU wget.
    let missing: Vec<&str> = ["sfdisk", "mkfs.fat", "mkntfs", "ntfsfix", "efibootmgr", "awk", "wget"]
        .into_iter()
        .filter(|b| !list.lines().any(|l| l.ends_with(&format!("broom/bin/{b}"))))
        .collect();
    if !missing.is_empty() {
        return Err(format!("stage initrd is missing {} — install fdisk ntfs-3g dosfstools efibootmgr wget mawk on the machine that builds the bundle", missing.join(", ")));
    }
    std::fs::rename(&tmp, format!("{sd}/stage.img")).map_err(|e| e.to_string())?;
    std::fs::copy(format!("/boot/vmlinuz-{kv}"), format!("{sd}/vmlinuz"))
        .map_err(|e| format!("copy /boot/vmlinuz-{kv}: {e}"))?;
    let _ = std::fs::remove_dir_all(conf);
    std::fs::write(&key_file, &key).map_err(|e| e.to_string())?;
    Ok(true)
}

/// The stage bundle this binary belongs to (mgmt/stage.pin, filled by CI from `build-stage`: sha256, then the Release
/// URL): kernel + initrd + Ubuntu shim built on the CI runner, so the server needs none of the tools above and its
/// own kernel does not matter. Comments only (local builds) → a broom-stage.tar.gz copied next to the binary is used.
const STAGE_PIN: &str = include_str!("../../stage.pin");
/// Largest bundle accepted for download (kernel + initrd + shim are ~100 MB).
const STAGE_MAX: u64 = 1 << 30;

/// (sha256, URL) from a stage.pin text; None without a valid sha256.
fn parse_pin(pin: &str) -> Option<(&str, Option<&str>)> {
    let mut lines = pin.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
    let sha = lines.next().filter(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))?;
    Some((sha, lines.next()))
}

pub fn stage_pinned() -> Option<&'static str> {
    parse_pin(STAGE_PIN).map(|(sha, _)| sha)
}

/// Make sure stage_dir() holds the stage — always from a bundle, never built on the server (no packages there):
/// `broom-stage.tar.gz` next to the binary if someone copied it there, else (release binary) downloaded once from
/// the Release. A release binary accepts only its pinned bundle. Returns what happened.
pub fn ensure_stage() -> Result<String, String> {
    let sd = stage_dir();
    let local = crate::home().join("broom-stage.tar.gz");
    let mark = format!("{sd}/bundle.sha256");
    let have = |f: &str| Path::new(&format!("{sd}/{f}")).is_file();
    let installed = |want: &str| std::fs::read_to_string(&mark).is_ok_and(|s| s.trim() == want) && have("vmlinuz") && have("stage.img");
    let Some(want) = stage_pinned() else {
        // Locally built binary: the bundle its builder made with `build-stage` (it matches their stage script).
        if local.is_file() {
            let sha = crate::hash::file_hash(&local.to_string_lossy()).ok_or_else(|| format!("{}: unreadable", local.display()))?;
            install_bundle(&local, &sha, &sd)?;
            let _ = std::fs::remove_file(&local);
            return Ok("installed from broom-stage.tar.gz next to the binary".into());
        }
        if std::fs::read_to_string(&mark).is_ok() && have("vmlinuz") && have("stage.img") {
            return Ok("kept".into());
        }
        return Err(format!(
            "this binary was built locally and has no Windows stage: on a machine with initramfs-tools fdisk ntfs-3g \
             dosfstools efibootmgr wget zstd shim-signed and a -generic kernel, run `sudo ./bootrom-mgmt build-stage <dir> \
             --kernel <version>-generic`, copy <dir>/broom-stage.tar.gz to {} and publish again (or use a release binary)",
            local.display()
        ));
    };
    if installed(want) {
        return Ok("kept".into());
    }
    if local.is_file() {
        install_bundle(&local, want, &sd)?;
        let _ = std::fs::remove_file(&local); // unpacked into tftp/broom-stage/
        return Ok("installed from broom-stage.tar.gz next to the binary".into());
    }
    let url = parse_pin(STAGE_PIN).and_then(|(_, u)| u).ok_or_else(|| format!("this binary has no stage URL — copy its broom-stage.tar.gz to {}", local.display()))?;
    let dl = crate::work_dir().join("broom-stage.tar.gz");
    std::fs::create_dir_all(crate::work_dir()).map_err(|e| e.to_string())?;
    tracing::info!("downloading the Windows stage: {url}");
    download(url, &dl).map_err(|e| {
        format!("{e} — no internet on this server? download broom-stage.tar.gz of this release elsewhere and copy it to {}", local.display())
    })?;
    let r = install_bundle(&dl, want, &sd);
    let _ = std::fs::remove_file(&dl);
    r?;
    Ok("installed from the release bundle".into())
}

/// Check `file` against sha256 `want`, unpack it into `sd` (swapped in whole, never half-written), mark it.
fn install_bundle(file: &Path, want: &str, sd: &str) -> Result<(), String> {
    let got = crate::hash::file_hash(&file.to_string_lossy()).ok_or_else(|| format!("{}: unreadable", file.display()))?;
    if got != want {
        return Err(format!("{}: sha256 {got}, this binary expects its own build's stage {want}", file.display()));
    }
    let tmp = format!("{sd}.new");
    let _ = std::fs::remove_dir_all(&tmp);
    crate::archive::untar_gz(file, Path::new(&tmp))?;
    for f in ["vmlinuz", "stage.img"] {
        if !Path::new(&format!("{tmp}/{f}")).is_file() {
            return Err(format!("stage bundle has no {f}"));
        }
    }
    std::fs::write(format!("{tmp}/bundle.sha256"), want).map_err(|e| e.to_string())?;
    let old = format!("{sd}.old");
    let _ = std::fs::remove_dir_all(&old);
    let _ = std::fs::rename(&sd, &old);
    std::fs::rename(&tmp, sd).map_err(|e| format!("{sd}: {e}"))?;
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// HTTPS GET → `dest` (rustls, built-in CA roots: works on a server without ca-certificates).
fn download(url: &str, dest: &Path) -> Result<(), String> {
    let resp = ureq::get(url).call().map_err(|e| format!("download {url}: {e}"))?;
    let mut r = resp.into_body().into_with_config().limit(STAGE_MAX).reader();
    let mut f = std::fs::File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    std::io::copy(&mut r, &mut f).map_err(|e| format!("download {url}: {e}"))?;
    Ok(())
}

/// `bootrom-mgmt build-stage <outdir> [--kernel <version>]` (CI, as root): build the stage for that kernel, add the
/// Ubuntu shim, write <outdir>/broom-stage.tar.gz and print its sha256.
pub fn build_bundle(args: &[String]) -> ! {
    let r = (|| -> Result<String, String> {
        let out = args.get(2).filter(|a| !a.starts_with("--")).ok_or("usage: bootrom-mgmt build-stage <outdir> [--kernel <version>]")?;
        let kernel = args.iter().position(|a| a == "--kernel").and_then(|i| args.get(i + 1)).map(String::as_str);
        let dir = format!("{out}/broom-stage");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        build_stage(kernel, &dir)?;
        let shim = ["/usr/lib/shim/shimx64.efi.signed.latest", "/usr/lib/shim/shimx64.efi.signed"]
            .into_iter()
            .find(|p| Path::new(p).is_file())
            .ok_or("no /usr/lib/shim/shimx64.efi.signed* — apt install shim-signed")?;
        std::fs::copy(shim, format!("{dir}/shimx64.efi")).map_err(|e| format!("{shim}: {e}"))?;
        let tgz = format!("{out}/broom-stage.tar.gz");
        crate::archive::tar_gz(Path::new(&dir), Path::new(&tgz))?;
        crate::hash::file_hash(&tgz).ok_or_else(|| "sha256 of the bundle failed".into())
    })();
    match r {
        Ok(h) => {
            println!("{h}");
            std::process::exit(0)
        }
        Err(e) => {
            eprintln!("build-stage: {e}");
            std::process::exit(1)
        }
    }
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

    /// Shell functions are global: a helper defined inside another function silently
    /// replaces a top-level one of the same name → every `name(){` in the stage must be unique.
    #[test]
    fn stage_function_names_unique() {
        let mut seen = std::collections::HashSet::new();
        for l in super::STAGE_SCRIPT.lines() {
            let t = l.trim_start();
            if let Some(name) = t.split_once("(){").map(|(n, _)| n).filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
                assert!(seen.insert(name.to_string()), "stage function {name}() defined twice");
            }
        }
    }

    /// The stage script runs inside the initramfs — a syntax error = client stuck in a shell.
    #[test]
    fn stage_syntax() {
        for s in [super::STAGE_SCRIPT, super::STAGE_HOOK] {
            // Busybox 1.30 (Ubuntu 22.04) ignores -n with -c and RUNS the script (partitions /dev/sda, reboots) →
            // wrap it in a function that is never called: the whole body is parsed, nothing executes.
            let ok = stage_sh().args(["-n", "-c", &format!("broom_syntax_check(){{\n{s}\n}}")]).status().unwrap();
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

    /// Stage disk choice (cut from STAGE_SCRIPT, fake /sys/block): a registered machine with ONE internal disk is
    /// partitioned by itself; USB disks never count; anything else asks — Enter / an unknown name = reboot untouched;
    /// an existing BROOMWIN is reused without asking.
    #[test]
    fn stage_disk_choice() {
        let s = super::STAGE_SCRIPT;
        let part = &s[s.find("part(){").unwrap()..s.find("# end disk choice").unwrap()];
        let run = |name: &str, disks: &[(&str, bool)], reg: &str, answer: &str, broomwin: &str| {
            let d = std::env::temp_dir().join(format!("broom_t_disk_{name}"));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(d.join("block")).unwrap();
            for (n, usb) in disks {
                let dev = d.join(if *usb { "devices/pci0/usb1/1-1" } else { "devices/pci0/ata1" }).join(n);
                std::fs::create_dir_all(dev.join("device")).unwrap();
                std::fs::write(dev.join("removable"), "0\n").unwrap(); // USB disks often say 0 too
                std::fs::write(dev.join("size"), "500118192\n").unwrap();
                std::fs::write(dev.join("device/model"), "TestDisk\n").unwrap();
                std::os::unix::fs::symlink(&dev, d.join("block").join(n)).unwrap();
            }
            std::fs::write(d.join("answer"), format!("{answer}\n")).unwrap();
            let body = part
                .replace("lbl(){ blkid -s LABEL -o value \"$1\" 2>/dev/null; }", &format!("lbl(){{ [ \"$1\" = \"/dev/{broomwin}2\" ] && echo BROOMWIN; }}"))
                .replace("SYSB=/sys/block; CON=/dev/console", &format!("SYSB={}/block; CON=/dev/null", d.display()))
                .replace("read -r ans < $CON", &format!("read -r ans < {}/answer", d.display()));
            let sh = format!("REG={reg}\nlog(){{ :; }}; die(){{ echo DIE; exit; }}; restart(){{ echo RESTART; exit; }}\n{body}\necho \"DISK $disk $NEWDISK\"");
            let o = stage_sh().args(["-c", &sh]).output().unwrap();
            let _ = std::fs::remove_dir_all(&d);
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        assert_eq!(run("a", &[("sda", false), ("sdc", true)], "1", "", "-"), "DISK sda 1", "registered, one internal disk (USB ignored)");
        assert_eq!(run("b", &[("sda", false)], "", "", "-"), "RESTART", "unknown machine + Enter → untouched");
        assert_eq!(run("c", &[("sda", false)], "", "sda", "-"), "DISK sda 1", "unknown machine, typed");
        assert_eq!(run("d", &[("sda", false), ("sdb", false)], "1", "sdb", "-"), "DISK sdb 1", "two disks → asked");
        assert_eq!(run("e", &[("sda", false), ("sdb", false)], "1", "sdz", "-"), "RESTART", "not one of the listed disks");
        assert_eq!(run("f", &[("sda", false), ("sdb", false)], "", "", "sdb"), "DISK sdb", "existing BROOMWIN reused, no question");
        assert_eq!(run("g", &[("sdc", true)], "1", "", "-"), "DIE", "only a USB disk → no local disk");
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

    /// A guest (local admin) writes first.pending + base.ok next to the existing base → ignored, never committed
    /// (the reset child's parent is base itself).
    #[test]
    fn stage_forged_first_pending_ignored() {
        let s = super::STAGE_SCRIPT;
        let part = &s[s.find("# 2. base/child state machine").unwrap()..s.find("# Machine name (Machines table").unwrap()];
        let d = std::env::temp_dir().join("broom_t_forged");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for (f, v) in [("base.vhdx", "BASE"), ("child.vhdx", "CHILD"), ("first.pending", ""), ("base.ok", "ok")] {
            std::fs::write(d.join(f), v).unwrap();
        }
        let sh = format!("B={}\nlog(){{ :; }}; vhdx_guid(){{ echo '{{g}}'; }}\n{part}", d.display());
        assert!(stage_sh().args(["-c", &sh]).status().unwrap().success());
        assert_eq!(std::fs::read_to_string(d.join("base.vhdx")).unwrap(), "BASE", "base untouched");
        assert!(!d.join("first.pending").exists() && !d.join("base.ok").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Base build: the driver folders are extracted again from the checked archives (a guest's planted or edited
    /// folder is gone), an archive that doesn't match its sha256 is left out.
    #[test]
    fn stage_drivers_reextracted_for_base() {
        let s = super::STAGE_SCRIPT;
        let part = &s[s.find("  for x in drivers/*; do").unwrap()..s.find("  cp base-template.vhdx child.vhdx || die").unwrap()];
        let d = std::env::temp_dir().join("broom_t_drvx");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join("drivers/planted")).unwrap();
        std::fs::create_dir_all(d.join("drivers/nv")).unwrap();
        std::fs::write(d.join("src/nv.inf"), "good").unwrap();
        std::fs::write(d.join("drivers/nv/nv.inf"), "edited by the guest").unwrap();
        std::fs::write(d.join("drivers/nv/evil.inf"), "planted").unwrap();
        let tar = |n: &str| assert!(std::process::Command::new("tar").args(["-czf", &format!("../drivers/{n}.tar.gz"), "nv.inf"]).current_dir(d.join("src")).status().unwrap().success());
        tar("nv");
        tar("bad");
        let nv = crate::hash::file_hash(&d.join("drivers/nv.tar.gz").to_string_lossy()).unwrap();
        std::fs::write(d.join("drivers/nv.sha256"), format!("{nv}\n")).unwrap();
        std::fs::write(d.join("drivers/bad.sha256"), "0000\n").unwrap();
        let sh = format!("cd {}\nlog(){{ :; }}\n{part}", d.display());
        assert!(stage_sh().args(["-c", &sh]).status().unwrap().success());
        assert_eq!(std::fs::read_to_string(d.join("drivers/nv/nv.inf")).unwrap(), "good");
        assert!(!d.join("drivers/nv/evil.inf").exists() && !d.join("drivers/planted").exists());
        assert!(!d.join("drivers/bad").exists() && !d.join("drivers/bad.tar.gz").exists(), "mismatch → left out");
        let _ = std::fs::remove_dir_all(&d);
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
        let nv = crate::hash::file_hash(&d.join("pkgs/nv.tar.gz").to_string_lossy()).unwrap();
        // wget mock: POST → answer.txt (missing = server down); GET → pkgs/<file> into -O.
        let mock = format!(
            "cd {}; SRV=x; MAC=aa:bb:cc:dd:ee:01\nlog(){{ echo \"$*\" >> {}/log; }}; configure_networking(){{ :; }}\n\
             wget(){{ o=\"\"; p=\"\"; u=\"\"; while [ $# -gt 0 ]; do case \"$1\" in -O) o=$2; shift;; -T) shift;; --post-file=*) p=1;; -q) ;; *) u=$1;; esac; shift; done\n\
               if [ -n \"$p\" ]; then [ -f {d}/answer.txt ] && cp {d}/answer.txt \"$o\"; else cat {d}/pkgs/${{u##*/}} > \"$o\"; fi; }}\n{part}",
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
        let ans = format!("nv {nv}\n");
        assert_eq!(step(Some("")), (false, false), "no packages: an old base stays");
        assert_eq!(step(Some(&ans)), (true, true), "new package → downloaded + base rebuilt");
        assert!(b.join("drivers/nv.tar.gz").exists(), "archive kept: each base build extracts it again");
        assert_eq!(step(Some(&ans)), (false, true), "unchanged → nothing to do");
        assert_eq!(step(None), (false, true), "server down → keep");
        assert_eq!(step(Some("")), (true, false), "package gone → removed + rebuilt");
        assert_eq!(step(Some("bad s2\n")), (false, false), "download fails → not counted, retried next boot");
        assert_eq!(step(Some("nv 0000\n")), (false, false), "sha256 mismatch → not extracted, retried next boot");
        let log = std::fs::read_to_string(d.join("log")).unwrap();
        assert!(log.contains("driver bad: download failed") && log.contains("driver nv: download failed or sha256 mismatch"), "{log}");
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
        let run = |current: &str, strict: &str| {
            let d = std::env::temp_dir().join(format!("broom_t_order_{}_{strict}", if current.is_empty() { "none" } else { current }));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("v.txt"), v).unwrap();
            let plain: String = v.lines().map(|l| l.split('\t').next().unwrap().to_string() + "\n").collect();
            let head = if current.is_empty() { String::new() } else { format!("BootCurrent: {current}\n") };
            std::fs::write(d.join("plain.txt"), format!("{head}BootOrder: 0000,0004,0003,0007,0001,0005\n{plain}")).unwrap();
            let sh = format!(
                "cd {}; B=.; n=0007; STRICT={strict}\nlog(){{ echo \"$*\" >> log; }}\n\
                 efibootmgr(){{ case \"$1\" in -v) cat v.txt;; -q) echo \"$3\" > set.txt;; *) cat plain.txt;; esac; }}\n{part}",
                d.display()
            );
            assert!(stage_sh().args(["-c", &sh]).status().unwrap().success());
            let rd = |f: &str| std::fs::read_to_string(d.join(f)).unwrap_or_default().trim().to_string();
            let out = (rd("set.txt"), rd("bootorder.txt"), rd("log"), rd("strict.txt"));
            let _ = std::fs::remove_dir_all(&d);
            out
        };
        let (set, file, log, strict) = run("0003", "");
        assert_eq!(set, "0003,0007,0000,0004,0005,0001", "PXE (named IBA GE…) first, other network after Windows");
        assert_eq!(file, set, "the order Windows restores");
        assert!(log.contains("PXE Boot0003 (IBA GE Slot 0100 v1553)"), "{log}");
        assert_eq!(strict, "", "no strict.txt without strict reset");
        assert_eq!(run("", "").0, "0004,0003,0005,0007,0000,0001", "no BootCurrent → every network entry first");
        assert_eq!(run("0000", "").0, "0004,0003,0005,0007,0000,0001", "BootCurrent = Windows entry → ignored");
        // Strict reset: every Windows entry (Broom Windows + Windows Boot Manager) out of BootOrder, listed for Windows.
        let (set, file, _, strict) = run("0003", "1");
        assert_eq!((set.as_str(), file.as_str(), strict.as_str()), ("0003,0004,0005,0001", "0003,0004,0005,0001", "0007,0000"));
    }

    /// Stage golden download (cut from STAGE_SCRIPT, wget/getfile mocked): the old golden + base go, the golden is
    /// downloaded whole and hashed on the way; a short file (disk full / cut stream) is never accepted.
    #[test]
    fn stage_whole_golden_download() {
        let s = super::STAGE_SCRIPT;
        let part = &s[s.find("  D=$B/dl-$HASH").unwrap()..s.find("# 1b. ").unwrap()];
        let golden: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let run = |short: bool| {
            let d = std::env::temp_dir().join(format!("broom_t_dl_{short}"));
            let _ = std::fs::remove_dir_all(&d);
            for p in ["b", "srv"] {
                std::fs::create_dir_all(d.join(p)).unwrap();
            }
            std::fs::write(d.join("srv/golden.vhdx"), &golden).unwrap();
            std::fs::write(d.join("srv/golden.size"), golden.len().to_string()).unwrap();
            for f in ["base-template.vhdx", "child-template.vhdx", "child-template.off", "efi.tar.gz"] {
                std::fs::write(d.join("srv").join(f), f).unwrap();
            }
            for f in ["golden.vhdx", "golden.chunks", "base.vhdx", "child.vhdx"] {
                std::fs::write(d.join("b").join(f), "old").unwrap();
            }
            let hash = crate::hash::file_hash(&d.join("srv/golden.vhdx").to_string_lossy()).unwrap();
            // A short download = only the first 1 MB of the golden arrives.
            let cut = if short { "| head -c 1048576" } else { "" };
            let sh = format!(
                "B={d}/b; W={d}; SRV=x; NAME=w; HASH={hash}\nlog(){{ :; }}; die(){{ echo DIE; exit; }}; restart(){{ echo RESTART; exit; }}\n\
                 srv_hash(){{ echo {hash}; }}\n\
                 wget(){{ for u; do :; done; cat {d}/srv/${{u##*/}}; }}\n\
                 getfile(){{ if [ \"$1\" = -c ]; then shift; fi; o=$2; u=$3; if [ \"$o\" = - ]; then cat {d}/srv/${{u##*/}} {cut}; else cat {d}/srv/${{u##*/}} {cut} > \"$o\"; fi; }}\n\
                 if :; then\n{part}echo OK", // the cut ends with the `fi` of step 1's `if`
                d = d.display()
            );
            let o = stage_sh().args(["-c", &sh]).output().unwrap();
            let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let got = std::fs::read(d.join("b/golden.vhdx")).ok();
            let sum = std::fs::read_to_string(d.join("b/golden.sha256")).ok();
            let old_gone = !d.join("b/base.vhdx").exists() && !d.join("b/golden.chunks").exists();
            let _ = std::fs::remove_dir_all(&d);
            (out, got.is_some_and(|g| g == golden), sum.is_some_and(|s| s.trim() == hash), old_gone)
        };
        assert_eq!(run(false), ("OK".into(), true, true, true));
        let (out, _, sum_ok, old_gone) = run(true);
        assert_eq!((out.as_str(), sum_ok, old_gone), ("DIE", false, true), "short golden never accepted");
    }

    /// Real HTTPS download (GitHub release → redirect to its CDN): cargo test download_https -- --ignored
    #[test]
    #[ignore]
    fn download_https() {
        let p = std::env::temp_dir().join("broom_t_download");
        super::download("https://github.com/ipxe/ipxe/releases/download/v2.0.0/ipxeboot.tar.gz", &p).unwrap();
        let sha = crate::hash::file_hash(&p.to_string_lossy()).unwrap();
        assert_eq!(sha, "01a526d4cc791fc30362259c609d6c506cc64a7bdff51b9a5eb788354e17eee1", "pinned in fetch-signed.sh");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn stage_pin_file() {
        assert_eq!(super::parse_pin(include_str!("../../stage.pin")), None, "committed stage.pin = local build, no pin");
        let sha = "ab".repeat(32);
        let pin = format!("# comment\n{sha}\nhttps://github.com/o/r/releases/download/v1/broom-stage.tar.gz\n");
        assert_eq!(super::parse_pin(&pin), Some((sha.as_str(), Some("https://github.com/o/r/releases/download/v1/broom-stage.tar.gz"))));
        assert_eq!(super::parse_pin("nothex\nhttps://x\n"), None);
    }

    /// Stage bundle: only the exact pinned file is installed; it replaces the previous stage as a whole.
    #[test]
    fn stage_bundle_install() {
        use std::path::Path;
        let d = std::env::temp_dir().join("broom_t_bundle");
        let _ = std::fs::remove_dir_all(&d);
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        for (f, body) in [("vmlinuz", "kernel"), ("stage.img", "initrd"), ("shimx64.efi", "shim")] {
            std::fs::write(src.join(f), body).unwrap();
        }
        let tgz = d.join("broom-stage.tar.gz");
        crate::archive::tar_gz(&src, &tgz).unwrap();
        let sha = crate::hash::file_hash(&tgz.to_string_lossy()).unwrap();
        let sd = d.join("tftp/broom-stage");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(sd.join("stage.img"), "old").unwrap();
        let sd = sd.to_string_lossy().into_owned();
        let e = super::install_bundle(&tgz, &"0".repeat(64), &sd).unwrap_err();
        assert!(e.contains("expects its own build"), "{e}");
        assert_eq!(std::fs::read_to_string(format!("{sd}/stage.img")).unwrap(), "old", "untouched on a wrong file");
        super::install_bundle(&tgz, &sha, &sd).unwrap();
        assert_eq!(std::fs::read_to_string(format!("{sd}/stage.img")).unwrap(), "initrd");
        assert_eq!(std::fs::read_to_string(format!("{sd}/bundle.sha256")).unwrap(), sha);
        assert!(Path::new(&format!("{sd}/shimx64.efi")).is_file() && !Path::new(&format!("{sd}.old")).exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
