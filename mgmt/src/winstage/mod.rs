// winstage/mod.rs — Windows diskless, design B: native VHDX boot from the client SSD.
//
// Flow: golden = Windows Pro VM that ran the Windows prep script (one-time link from the Images page: tweaks + EFI
// bundle + unattend + boot-start disk drivers), sysprepped → upload → publish(): extract the Windows partition →
// golden.vhdx + efi.tar.gz + 2 empty child VHDX (vhdx.rs) → make sure the stage bundle is installed (stage.rs) → boot_script.
//
// Every client boot: iPXE → STAGE (Linux, no root fs) on the SSD:
//   p1 ESP BROOMEFI, p2 NTFS BROOMWIN\broom\: golden.vhdx ← base.vhdx (golden specialized on THIS
//   machine, created once) ← child.vhdx (reset every boot). Hash mismatch → re-download golden over HTTP.
//   First boot (no base yet): child = base-template (parent golden) → Windows specialize/OOBE/first logon
//   write into it → broom-done.ps1 writes base.ok + reboots (base mode on the image: waits for a technician's
//   restart instead) → stage renames child→base → from then on each boot builds child = child-template with the
//   GUID of base patched in (instant; the small templates are checked against the server's sha256 every boot).
//   Done → efibootmgr BootNext "Broom Windows" → reboot → Windows boots the child from the SSD.
//
// Split: stage.rs (the client stage bundle), prep.rs (what goes into the golden), here: publish + golden build.
mod prep;
mod stage;

pub use prep::prep_script;
use prep::{boot_storage_done, silent_oobe, write_broom_done, BOOT_STORAGE, BROOM_BOOTORDER, BROOM_DONE};
pub use stage::{build_bundle, ensure_stage};

use std::path::Path;
use std::process::Command;

use crate::{images_dir, SharedState};

/// Windows stage (kernel + initrd) served at /tftp/broom-stage/.
fn stage_dir() -> String {
    crate::tftp_dir().join("broom-stage").to_string_lossy().into_owned()
}

pub(crate) fn run(bin: &str, args: &[&str]) -> Result<String, String> {
    tracing::debug!("exec: {bin} {}", args.join(" "));
    let o = Command::new(bin).args(args).output().map_err(|e| format!("{bin}: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        Err(format!("{bin} {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()))
    }
}


/// NTFS/basic-data partitions, LARGEST first. (start, size) in bytes.
fn ntfs_parts(t: &crate::disk::Table) -> Vec<(u64, u64)> {
    let mut parts: Vec<(u64, u64)> = t
        .parts
        .iter()
        .filter(|p| p.kind.eq_ignore_ascii_case(crate::disk::BASIC_DATA) || p.kind == "7")
        .map(|p| (p.start, p.size))
        .collect();
    parts.sort_by(|a, b| b.1.cmp(&a.1));
    parts
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
    // The kernel's ntfs3 driver (no ntfs-3g package). It refuses a dirty volume → same hint as a read-only mount.
    let r = run("mount", &["-t", "ntfs3", "-o", opt, &dev, mnt])
        .map_err(|e| {
            format!(
                "mount the Windows partition (ntfs3): {e} — if the kernel has ntfs3, Windows was not shut down cleanly \
                 (hibernated, Fast Startup or forced power-off): boot the VM, run the prep command again, let it power off"
            )
        })
        .and_then(|_| {
            // A read-only fallback must not pass as success: probe a write first.
            let probe = format!("{mnt}/.broom-rw");
            let r = if !ro && std::fs::write(&probe, b"").is_err() {
                Err("Windows partition mounted read-only: Windows was not shut down cleanly (hibernated, Fast Startup or \
                     forced power-off) — boot the VM, run the prep command again and let it power off by itself, then upload"
                    .into())
            } else {
                let _ = std::fs::remove_file(&probe);
                f(mnt)
            };
            let _ = run("umount", &[mnt]);
            r
        });
    let _ = run("losetup", &["-d", &dev]);
    r
}

/// The partition that CONTAINS Windows (has the SYSTEM hive) — mounts each NTFS partition read-only, largest first.
/// No guessing by size: picking the wrong one would let the later in-place edits wreck image.img.
fn find_windows(raw: &str, mnt: &str) -> Result<(u64, u64), String> {
    let parts = ntfs_parts(&crate::disk::read(raw)?.ok_or("golden has no partition table")?);
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
    // Whatever can refuse the publish runs before anything is written: a failure here leaves the served golden as is.
    let ip = st.db.get_config("dhcp_server_ip", "");
    if ip.is_empty() {
        return Err("dhcp_server_ip is empty — run `setup` first".into());
    }
    steps.go("initrd stage");
    let stage = ensure_stage()?;

    // golden.vhdx gets a new GUID on every convert → new hash → every client re-downloads. Golden still newer than
    // image.img → keep it, only refresh the stage + boot_script (Publish with a new binary = cheap).
    // Note: changing the extract/registry logic needs a rebuild → upload again or `touch image.img`.
    let fresh = golden_fresh(&raw, &out);
    let drivers = if fresh {
        "kept (golden already built from image.img + the current embedded logic)".to_string()
    } else {
        build_golden(&raw, &out, name, steps)?
    };

    // Hash (clients compare it to know whether to re-download, and check the whole download against it) + size (the
    // stage checks its free space and the downloaded length). Golden kept + both present → reuse (13 GB takes ~2
    // minutes on a slow disk). golden.sha256 is written LAST: the stage treats its absence as "publish running".
    let (sum_file, size_file) = (format!("{out}/golden.sha256"), format!("{out}/golden.size"));
    let _ = std::fs::remove_file(format!("{out}/golden.chunks")); // delta manifest of older versions
    let cached = std::fs::read_to_string(&sum_file).ok().map(|s| s.trim().to_string()).filter(|s| s.len() == 64);
    let hash = match cached {
        Some(h) if fresh && Path::new(&size_file).exists() => h,
        _ => {
            let _ = std::fs::remove_file(&sum_file);
            steps.go("sha256 golden");
            let h = crate::hash::file_hash(&golden).ok_or_else(|| format!("sha256 of {golden} failed"))?;
            let size = std::fs::metadata(&golden).map_err(|e| e.to_string())?.len();
            std::fs::write(&size_file, size.to_string()).map_err(|e| format!("golden.size: {e}"))?;
            h
        }
    };
    // The stage checks these small files against this list on every boot (a guest could swap them on the SSD).
    let mut sums = String::new();
    for f in ["efi.tar.gz", "child-template.vhdx", "child-template.off", "base-template.vhdx"] {
        let h = crate::hash::file_hash(&format!("{out}/{f}")).ok_or(format!("sha256 of {f} failed"))?;
        sums.push_str(&format!("{h}  {f}\n"));
    }
    std::fs::write(format!("{out}/files.sha256"), sums).map_err(|e| format!("files.sha256: {e}"))?;
    std::fs::write(&sum_file, &hash).map_err(|e| format!("golden.sha256: {e}"))?;
    let bs = format!(
        "kernel http://{ip}/tftp/broom-stage/vmlinuz initrd=stage.img ip=dhcp BOOTIF=01-${{mac:hexhyp}} broom.name={name} broom.hash={hash} broom.srv={ip} broom.host=${{broom-host}} broom.lic=${{broom-lic}} broom.reg=${{broom-reg}} broom.base=${{broom-base}} broom.strict=${{broom-strict}}\n\
         initrd http://{ip}/tftp/broom-stage/stage.img\n\
         boot"
    );
    let before = st.db.image(id)?;
    st.db.set_published(id, &bs, &hash)?;
    // A new golden → every machine booting it rebuilds its base, which needs the license key once more.
    if let Some(img) = before.filter(|i| i.hash.as_deref() != Some(hash.as_str())) {
        rearm_for_image(st, &img);
    }
    Ok(format!(
        "Publish OK — Windows '{name}': golden.vhdx + EFI + child templates; stage {stage}; boot-start disk drivers: {drivers}"
    ))
}

/// License keys already handed out ('sent') go back to 'armed' for the machines that boot `img` by default (their own
/// image, or none set and `img` is the global default): their base is rebuilt on the next boot and fetches the key once.
fn rearm_for_image(st: &SharedState, img: &crate::db::Image) {
    for m in st.db.machines().unwrap_or_default() {
        let boots_it = m.image_id == Some(img.id) || (m.image_id.is_none() && img.is_default);
        if boots_it && st.db.rearm_quiet(m.id).unwrap_or(false) {
            tracing::info!("license of {} armed again (new golden {} → base rebuilt)", m.hostname.as_deref().unwrap_or(&m.mac), img.name);
        }
    }
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
    stable_key(&[BROOM_DONE, BROOM_BOOTORDER, &BOOT_STORAGE.join(","), "skip-oobe-v1"])
}

/// Hash of the parts, stable across Rust releases (std's DefaultHasher is not: a toolchain update would rebuild every
/// golden + stage, and every client would re-download and rebuild its base). Each part is length-prefixed.
fn stable_key(parts: &[&str]) -> String {
    let mut h = blake3::Hasher::new();
    for p in parts {
        h.update(&(p.len() as u64).to_le_bytes());
        h.update(p.as_bytes());
    }
    h.finalize().to_hex()[..16].to_string()
}

/// image.img (raw whole VM disk) → golden.vhdx + efi.tar.gz + 2 empty child VHDX. Returns the enabled drivers.
fn build_golden(raw: &str, out: &str, name: &str, steps: &mut crate::publish::Steps) -> Result<String, String> {
    let mnt = crate::work_dir().join(format!("mnt-{name}")).to_string_lossy().into_owned();
    // 1. The partition holding Windows — check the hive (read-only) BEFORE any write.
    steps.go("find Windows partition");
    let (start, size) = find_windows(raw, &mnt)?;

    // 2. Edit image.img IN PLACE (no temporary full-disk copy): punch holes outside the Windows partition (ESP/
    //    MSR/Recovery don't go into the golden) + GPT with a single partition (standard native VHD boot), KEEP start →
    //    NTFS "hidden sectors" still match. Re-running gives the same result (re-publishing is safe).
    steps.go("trim disk");
    // Export (export.rs) rebuilds the VM disk from what the trim below destroys → keep it first. Never blocks publish.
    if let Err(e) = crate::export::keep_boot_regions(Path::new(raw), name, (start, size)) {
        tracing::warn!("image {name}: boot partitions not kept, export won't work for this upload: {e}");
    }
    const MB: u64 = 1024 * 1024;
    let total = std::fs::metadata(raw).map_err(|e| e.to_string())?.len();
    let end = start + size;
    // Keep the first 1MB (primary GPT) + the last 1MB (backup GPT).
    for (off, len) in [(MB, start.saturating_sub(MB)), (end, total.saturating_sub(MB).saturating_sub(end))] {
        if len > 0 {
            punch_hole(raw, off, len)?;
        }
    }
    // Only the tables are rewritten — the NTFS data of the Windows partition stays as it is.
    crate::disk::write_single_gpt(raw, start, size).map_err(|e| format!("golden single partition: {e}"))?;

    // 3. Mount read-write: check the boot-start disk drivers (set by the prep) + silent OOBE + current broom-done /
    //    bootorder scripts + take the EFI bundle.
    steps.go("registry + EFI");
    let drivers = with_part(raw, (start, size), &mnt, false, |m| {
        let mut drv = boot_storage_done(m)?;
        if silent_oobe(m)? {
            drv.push_str("; OOBE runs silently (SkipMachineOOBE)");
        }
        if write_broom_done(m)? {
            drv.push_str("; new broom-done.ps1");
        }
        if !Path::new(&format!("{m}/broom/efi/EFI/Microsoft/Boot/BCD")).exists() {
            return Err("golden is missing C:\\broom\\efi\\EFI\\Microsoft\\Boot\\BCD — run broom-prep-win in the VM before sysprep".into());
        }
        crate::archive::tar_gz(Path::new(&format!("{m}/broom/efi")), Path::new(&format!("{out}/efi.tar.gz")))?;
        Ok(drv)
    })?;

    // 4. golden.vhdx (dynamic, blocks in disk order) + 2 empty child VHDX: base-template (parent golden) and
    //    child-template (parent base.vhdx — base's GUID is only known on the client → the stage patches it at an offset).
    steps.go("convert raw→vhdx");
    let golden = format!("{out}/golden.vhdx");
    let tmp = format!("{golden}.tmp");
    if let Err(e) = crate::disk::Source::file(Path::new(raw)).and_then(|src| crate::vhdx::write_dynamic(&src, &tmp)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // Until here the served files (old golden + templates + golden.sha256) still belong together, so a failure above
    // never disturbs a client. From now on they change: no golden.sha256 → a client downloading sees "publish running"
    // instead of mixing two versions; no golden.key → a failure below makes the next publish rebuild everything.
    for f in ["golden.sha256", "golden.size", "golden.key"] {
        let _ = std::fs::remove_file(format!("{out}/{f}"));
    }
    std::fs::rename(&tmp, &golden).map_err(|e| e.to_string())?;
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

#[cfg(test)]
mod tests {
    #[test]
    fn ntfs_parts_order() {
        use crate::disk::{Part, Table};
        let part = |s: u64, n: u64, k: &str| Part { start: s * 512, size: n * 512, kind: k.into() };
        // UEFI VM: ESP, MSR, C:, Recovery (other types) → only C: is basic data.
        let t = Table {
            gpt: true,
            sector: 512,
            parts: vec![
                part(2048, 204800, "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
                part(206848, 32768, "E3C9E316-0B5C-4DB8-817D-F92DF00215AE"),
                part(239616, 124000000, "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"),
                part(124239616, 1000000, "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC"),
            ],
        };
        assert_eq!(super::ntfs_parts(&t), vec![(239616 * 512, 124000000 * 512)]);
        // MBR (BIOS VM): type "7", largest first.
        let t = Table { gpt: false, sector: 512, parts: vec![part(2048, 100000, "7"), part(102048, 9000000, "7")] };
        let p = super::ntfs_parts(&t);
        assert_eq!((p[0].0, p[1].0), (102048 * 512, 2048 * 512));
    }
}
