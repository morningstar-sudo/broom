// winstage/stage.rs — the Windows client STAGE: a Linux kernel + initrd that runs on every client boot (scripts/
// stage.sh: partition/update the SSD, rebuild the child VHDX, BootNext into Windows). Built by CI into a bundle
// (`build-stage`), pinned in the binary (stage.pin), installed on the server by ensure_stage — never built there.
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::{run, stable_key, stage_dir};

fn write_exec(path: &str, body: &str) -> Result<(), String> {
    std::fs::write(path, body).map_err(|e| format!("{path}: {e}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())
}

/// mkinitramfs hook for the stage: tools + modules for partitioning / NTFS / download / EFI.
/// Copied to /broom/bin (ahead of busybox in PATH — needs the full od/tar/wget...).
fn stage_hook() -> &'static str {
    crate::assets::text("scripts/stage-hook.sh")
}

/// Tools of the machine building the bundle that the hook copies into the stage (/broom/bin, ahead of busybox).
/// tee: the golden is hashed while downloading through `tee` — the initramfs busybox's copies one byte per write()
/// (built without block I/O): ~240 KB/s for a golden instead of the network's speed. dmesg: the kernel's log goes
/// into stage.log (busybox-initramfs has none).
const STAGE_TOOLS: &str =
    "sfdisk blkid mkfs.fat mkntfs ntfsfix efibootmgr wget sha256sum tee tar gzip od dd awk dmesg";

/// Stage init-premount script — runs on the client, does NOT mount root; reboots into Windows when done.
fn stage_script() -> &'static str {
    crate::assets::text("scripts/stage.sh")
}

/// Build the stage: kernel `kernel` (None = the running one) + initrd (mkinitramfs, own confdir) → `sd`.
/// Kernel + script + hook unchanged → keep the previous build (mkinitramfs MODULES=most takes about a minute).
/// Returns the kernel release used.
fn build_stage(kernel: Option<&str>, sd: &str) -> Result<String, String> {
    let kv = match kernel {
        Some(k) => k.to_string(),
        None => std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map_err(|e| format!("kernel release: {e}"))?
            .trim()
            .to_string(),
    };
    run("modinfo", &["-k", &kv, "ntfs3"])
        .map_err(|_| format!("kernel {kv} has no ntfs3 module — the stage must write NTFS"))?;
    // zstd: multithreaded compression + faster decompression than gzip; a builder without zstd → gzip.
    let compress = if run("sh", &["-c", "command -v zstd"]).is_ok() {
        "zstd"
    } else {
        "gzip"
    };
    let initramfs_conf = format!("MODULES=most\nBUSYBOX=y\nCOMPRESS={compress}\n");
    let hook = stage_hook().replace("__TOOLS__", STAGE_TOOLS);
    // Where each tool resolves on the builder is part of the key: a tool installed later (e.g. wget) → rebuilt.
    let tools = run(
        "sh",
        &[
            "-c",
            &format!("for b in {STAGE_TOOLS}; do command -v $b; done; true"),
        ],
    )
    .unwrap_or_default();
    let key = stable_key(&[
        kv.as_str(),
        initramfs_conf.as_str(),
        hook.as_str(),
        stage_script(),
        tools.as_str(),
    ]);
    let key_file = format!("{sd}/stage.key");
    let have = |f: &str| Path::new(&format!("{sd}/{f}")).exists();
    if have("stage.img")
        && have("vmlinuz")
        && std::fs::read_to_string(&key_file).ok().as_deref() == Some(key.as_str())
    {
        return Ok(kv);
    }
    let conf = &crate::work_dir()
        .join("stage-conf")
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_dir_all(conf);
    for d in ["scripts/init-premount", "hooks", "conf.d"] {
        std::fs::create_dir_all(format!("{conf}/{d}")).map_err(|e| e.to_string())?;
    }
    std::fs::write(format!("{conf}/initramfs.conf"), &initramfs_conf).map_err(|e| e.to_string())?;
    std::fs::write(format!("{conf}/modules"), "").map_err(|e| e.to_string())?;
    write_exec(&format!("{conf}/hooks/broom-stage"), &hook)?;
    write_exec(
        &format!("{conf}/scripts/init-premount/broom-stage"),
        stage_script(),
    )?;
    std::fs::create_dir_all(sd).map_err(|e| e.to_string())?;
    let tmp = format!("{sd}/stage.img.tmp");
    run("mkinitramfs", &["-d", conf, "-o", &tmp, &kv])?;
    // Tools WITHOUT a busybox replacement must really be in the initrd — report missing ones now
    // at build time, not when a client gets stuck in a shell.
    let list = run("lsinitramfs", &[&tmp])?;
    // wget: busybox's (initramfs build) lacks --post-file / -T → the driver list needs GNU wget.
    let missing: Vec<&str> = [
        "sfdisk",
        "mkfs.fat",
        "mkntfs",
        "ntfsfix",
        "efibootmgr",
        "awk",
        "wget",
        "tee",
        "dmesg",
    ]
    .into_iter()
    .filter(|b| !list.lines().any(|l| l.ends_with(&format!("broom/bin/{b}"))))
    .collect();
    if !missing.is_empty() {
        return Err(format!(
            "stage initrd is missing {} — install fdisk ntfs-3g dosfstools efibootmgr wget mawk on the machine that builds the bundle",
            missing.join(", ")
        ));
    }
    // The hook prunes what the stage never uses: refuse a build that lost NTFS, a common NIC / storage driver or the
    // Realtek NIC firmware (a client would boot a stage that sees no network or no SSD).
    let has_module = |m: &str| {
        list.lines()
            .any(|l| l.ends_with(&format!("/{m}.ko")) || l.contains(&format!("/{m}.ko.")))
    };
    let mut lost: Vec<&str> = ["ntfs3", "e1000e", "igc", "r8169", "tg3", "nvme", "ahci"]
        .into_iter()
        .filter(|m| !has_module(m))
        .collect();
    if !list.lines().any(|l| l.contains("firmware/rtl_nic/")) {
        lost.push("rtl_nic firmware");
    }
    if !lost.is_empty() {
        return Err(format!(
            "stage initrd lost {} — check the pruning in scripts/stage-hook.sh",
            lost.join(", ")
        ));
    }
    // Size in the build log: the initrd is downloaded on every boot, a growth shows here first.
    let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    eprintln!("stage.img: {:.1} MB (kernel {kv})", size as f64 / 1e6);
    std::fs::rename(&tmp, format!("{sd}/stage.img")).map_err(|e| e.to_string())?;
    std::fs::copy(format!("/boot/vmlinuz-{kv}"), format!("{sd}/vmlinuz"))
        .map_err(|e| format!("copy /boot/vmlinuz-{kv}: {e}"))?;
    let _ = std::fs::remove_dir_all(conf);
    std::fs::write(&key_file, &key).map_err(|e| e.to_string())?;
    Ok(kv)
}

/// Header of SOURCES.txt in the stage bundle; the package list follows.
const SOURCES_HEAD: &str = "\
The Broom Windows stage (vmlinuz, stage.img, shimx64.efi) holds UNMODIFIED binaries from the Ubuntu packages
listed below, under their own licenses (GPL-2.0 / GPL-3.0 and others: Linux kernel, busybox, ntfs-3g, wget,
efibootmgr, util-linux, dosfstools, ...; shim: BSD-2-Clause). They are not covered by Broom's Apache-2.0 license.
Corresponding source for each: `apt-get source <source package>=<source version>` on Ubuntu, or
https://launchpad.net/ubuntu/+source/<source package>/<source version>

<binary package> <version>  source: <source package> <source version>
";

/// SOURCES.txt for the bundle: the Ubuntu package of every file in the initrd that exists on this builder, of the
/// stage tools, the kernel and the shim, with versions + source package (license notice, see NOTICE).
fn sources_list(stage_img: &str, kv: &str, shim: &str) -> Result<String, String> {
    let script = format!(
        "{{ lsinitramfs '{stage_img}' | sed 's#^#/#'; for b in {STAGE_TOOLS}; do command -v $b; done; \
           echo /boot/vmlinuz-{kv}; echo '{shim}'; }} \
         | while read -r f; do [ -f \"$f\" ] && echo \"$f\" && readlink -f \"$f\"; done | sort -u \
         | xargs -r dpkg -S 2>/dev/null | grep -v '^diversion' | sed 's/: .*//; s/, /\\n/g' | sed 's/:.*//' | sort -u \
         | xargs -r dpkg-query -W -f='${{binary:Package}} ${{Version}}  source: ${{source:Package}} ${{source:Version}}\\n'"
    );
    let list = run("sh", &["-c", &script])?;
    if !list.contains("linux-image") {
        return Err(format!(
            "SOURCES.txt: no kernel package found for /boot/vmlinuz-{kv} (dpkg -S)"
        ));
    }
    Ok(format!("{SOURCES_HEAD}{list}"))
}

/// The stage bundle this binary belongs to (mgmt/stage.pin, filled by CI from `build-stage`: sha256, then the Release
/// URL): kernel + initrd + Ubuntu shim built on the CI runner, so the server needs none of the tools above and its
/// own kernel does not matter. Comments only (local builds) → a broom-stage.tar.gz copied next to the binary is used.
const STAGE_PIN: &str = include_str!("../../stage.pin");
/// Largest bundle accepted for download (kernel + initrd + shim are ~100 MB).
const STAGE_MAX: u64 = 1 << 30;

/// (sha256, URL) from a stage.pin text; None without a valid sha256.
fn parse_pin(pin: &str) -> Option<(&str, Option<&str>)> {
    let mut lines = pin
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let sha = lines
        .next()
        .filter(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))?;
    Some((sha, lines.next()))
}

pub fn stage_pinned() -> Option<&'static str> {
    parse_pin(STAGE_PIN).map(|(sha, _)| sha)
}

/// Make sure stage_dir() holds the stage — always from a bundle, never built on the server (no packages there):
/// `broom-stage.tar.gz` next to the binary if someone copied it there, else (release binary) downloaded from the
/// Release. A release binary accepts only its pinned bundle. What is installed is checked file by file (stage_ok)
/// on every call: missing, changed or another build's → installed again. Returns what happened.
pub fn ensure_stage() -> Result<String, String> {
    let sd = stage_dir();
    let local = crate::home().join("broom-stage.tar.gz");
    let Some(want) = stage_pinned() else {
        // Locally built binary: the bundle its builder made with `build-stage` (it matches their stage script).
        if local.is_file() {
            let sha = crate::hash::file_hash(&local.to_string_lossy())
                .ok_or_else(|| format!("{}: unreadable", local.display()))?;
            install_bundle(&local, &sha, &sd)?;
            let _ = std::fs::remove_file(&local);
            return Ok("installed from broom-stage.tar.gz next to the binary".into());
        }
        if stage_ok(&sd, None) {
            return Ok("kept (checked)".into());
        }
        return Err(format!(
            "this binary was built locally and has no Windows stage: on a machine with initramfs-tools fdisk ntfs-3g \
             dosfstools efibootmgr wget shim-signed and a -generic kernel, run `sudo ./bootrom-mgmt build-stage <dir> \
             --kernel <version>-generic`, copy <dir>/broom-stage.tar.gz to {} and publish again (or use a release binary)",
            local.display()
        ));
    };
    if stage_ok(&sd, Some(want)) {
        return Ok("kept (checked)".into());
    }
    let r = if local.is_file() {
        install_bundle(&local, want, &sd).map(|()| {
            let _ = std::fs::remove_file(&local); // unpacked into tftp/broom-stage/
            "installed from broom-stage.tar.gz next to the binary".to_string()
        })
    } else {
        let url = parse_pin(STAGE_PIN).and_then(|(_, u)| u).ok_or_else(|| {
            format!(
                "this binary has no stage URL — copy its broom-stage.tar.gz to {}",
                local.display()
            )
        })?;
        let dl = crate::work_dir().join(format!("broom-stage-{}.tar.gz", unique()));
        std::fs::create_dir_all(crate::work_dir()).map_err(|e| e.to_string())?;
        tracing::info!("downloading the Windows stage: {url}");
        let r = download(url, &dl, STAGE_MAX)
            .map_err(|e| format!("{e} — no internet on this server? download broom-stage.tar.gz of this release elsewhere and copy it to {}", local.display()))
            .and_then(|()| install_bundle(&dl, want, &sd))
            .map(|()| "installed from the release bundle".to_string());
        let _ = std::fs::remove_file(&dl);
        r
    };
    // Another call (startup, the Secure Boot switch, a publish) may have installed the same bundle meanwhile — what
    // counts is what is on disk now.
    r.or_else(|e| {
        if stage_ok(&sd, Some(want)) {
            Ok("installed (checked)".into())
        } else {
            Err(e)
        }
    })
}

/// Per-call suffix for the download file + temp dirs: calls at the same time never write into each other's files.
fn unique() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// The installed stage is the bundle `want` (None = any bundle) AND every file it unpacked still has the sha256
/// recorded when it was installed (files.sha256) — vmlinuz + stage.img among them. Re-hashed on each call
/// (~100 MB: well under a second).
fn stage_ok(sd: &str, want: Option<&str>) -> bool {
    let mark = std::fs::read_to_string(format!("{sd}/bundle.sha256")).unwrap_or_default();
    let mark = mark.trim();
    if mark.is_empty() || want.is_some_and(|w| w != mark) {
        return false;
    }
    let Ok(sums) = std::fs::read_to_string(format!("{sd}/files.sha256")) else {
        return false;
    };
    let mut names = Vec::new();
    for l in sums.lines() {
        let Some((h, f)) = l.split_once("  ") else {
            return false;
        };
        if f.contains('/') || crate::hash::file_hash(&format!("{sd}/{f}")).as_deref() != Some(h) {
            return false;
        }
        names.push(f);
    }
    names.contains(&"vmlinuz") && names.contains(&"stage.img")
}

/// Check `file` against sha256 `want`, unpack it, record the sha256 of every file it holds (files.sha256, checked by
/// stage_ok), swap it in as `sd` whole (never half-written).
fn install_bundle(file: &Path, want: &str, sd: &str) -> Result<(), String> {
    let got = crate::hash::file_hash(&file.to_string_lossy())
        .ok_or_else(|| format!("{}: unreadable", file.display()))?;
    if got != want {
        return Err(format!(
            "{}: sha256 {got}, this binary expects its own build's stage {want}",
            file.display()
        ));
    }
    let u = unique();
    let (tmp, old) = (format!("{sd}.new-{u}"), format!("{sd}.old-{u}"));
    let r = (|| {
        crate::archive::untar_gz(file, Path::new(&tmp))?;
        for f in ["vmlinuz", "stage.img"] {
            if !Path::new(&format!("{tmp}/{f}")).is_file() {
                return Err(format!("stage bundle has no {f}"));
            }
        }
        let mut sums = String::new();
        let mut files: Vec<_> = std::fs::read_dir(&tmp)
            .map_err(|e| e.to_string())?
            .flatten()
            .filter(|e| e.path().is_file())
            .collect();
        files.sort_by_key(|e| e.file_name());
        for e in files {
            let p = e.path();
            let h = crate::hash::file_hash(&p.to_string_lossy())
                .ok_or_else(|| format!("{}: unreadable", p.display()))?;
            sums.push_str(&format!("{h}  {}\n", e.file_name().to_string_lossy()));
        }
        std::fs::write(format!("{tmp}/files.sha256"), sums).map_err(|e| e.to_string())?;
        std::fs::write(format!("{tmp}/bundle.sha256"), want).map_err(|e| e.to_string())?;
        let _ = std::fs::rename(sd, &old);
        std::fs::rename(&tmp, sd).map_err(|e| format!("{sd}: {e}"))
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&old);
    r
}

/// HTTPS GET → `dest` (rustls, built-in CA roots: works on a server without ca-certificates).
pub(crate) fn download(url: &str, dest: &Path, max: u64) -> Result<(), String> {
    let resp = ureq::get(url)
        .call()
        .map_err(|e| format!("download {url}: {e}"))?;
    let mut r = resp.into_body().into_with_config().limit(max).reader();
    let mut f = std::fs::File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    std::io::copy(&mut r, &mut f).map_err(|e| format!("download {url}: {e}"))?;
    Ok(())
}

/// `bootrom-mgmt build-stage <outdir> [--kernel <version>]` (CI, as root): build the stage for that kernel, add the
/// Ubuntu shim, write <outdir>/broom-stage.tar.gz and print its sha256.
pub fn build_bundle(args: &[String]) -> ! {
    let r = (|| -> Result<String, String> {
        let out = args
            .get(2)
            .filter(|a| !a.starts_with("--"))
            .ok_or("usage: bootrom-mgmt build-stage <outdir> [--kernel <version>]")?;
        let kernel = args
            .iter()
            .position(|a| a == "--kernel")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str);
        let dir = format!("{out}/broom-stage");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let kv = build_stage(kernel, &dir)?;
        let shim = [
            "/usr/lib/shim/shimx64.efi.signed.latest",
            "/usr/lib/shim/shimx64.efi.signed",
        ]
        .into_iter()
        .find(|p| Path::new(p).is_file())
        .ok_or("no /usr/lib/shim/shimx64.efi.signed* — apt install shim-signed")?;
        std::fs::copy(shim, format!("{dir}/shimx64.efi")).map_err(|e| format!("{shim}: {e}"))?;
        let sources = sources_list(&format!("{dir}/stage.img"), &kv, shim)?;
        std::fs::write(format!("{dir}/SOURCES.txt"), sources).map_err(|e| e.to_string())?;
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
#[path = "../../tests/unit/winstage/stage.rs"]
mod tests;
