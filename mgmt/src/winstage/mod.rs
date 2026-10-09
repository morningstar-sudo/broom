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
use prep::{boot_storage_done, has_stub, BROOM_BOOTORDER, BROOM_DONE};
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

/// The partition that CONTAINS Windows (has the SYSTEM hive) — reads each NTFS partition, largest first.
/// No guessing by size: picking the wrong one would let the later in-place edits wreck image.img.
fn find_windows(raw: &str) -> Result<(u64, u64), String> {
    let parts = ntfs_parts(&crate::disk::read(raw)?.ok_or("golden has no partition table")?);
    let mut seen = Vec::new();
    for p in &parts {
        match crate::ntfsread::Vol::open(raw, *p) {
            Ok(mut v) => {
                if v.exists("Windows/System32/config/SYSTEM") {
                    return Ok(*p);
                }
                seen.push(format!("{}GB [{}]", p.1 >> 30, v.root_names(12).join(", ")));
            }
            Err(e) => seen.push(format!("{}GB (read error: {e})", p.1 >> 30)),
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
    // image.img → keep it, only refresh the stage + scripts + boot_script (Publish with a new binary = cheap).
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
    // First line: the golden these files belong to — the stage applies them only to that golden (a client still on
    // the previous hash during a publish never gets the new templates next to its old golden).
    // broom-done / boot-order: the golden only holds a fixed stub (prep) that runs the copy the stage puts in BROOMWIN
    // broom\ — so a new mgmt version updates them without rebuilding the golden.
    for (f, body) in [("broom-done.ps1", BROOM_DONE), ("broom-bootorder.ps1", BROOM_BOOTORDER)] {
        let (p, tmp) = (format!("{out}/{f}"), format!("{out}/{f}.tmp"));
        std::fs::write(&tmp, body.replace('\n', "\r\n")).and_then(|_| std::fs::rename(&tmp, &p)).map_err(|e| format!("{f}: {e}"))?;
    }
    let mut sums = format!("{hash}  golden\n");
    for f in ["efi.tar.gz", "child-template.vhdx", "child-template.off", "base-template.vhdx", "broom-done.ps1", "broom-bootorder.ps1"] {
        let h = crate::hash::file_hash(&format!("{out}/{f}")).ok_or(format!("sha256 of {f} failed"))?;
        sums.push_str(&format!("{h}  {f}\n"));
    }
    // Cache mode RAM: the copy loaded BEFORE golden.sha256 tells the stages to download, so the room's first rush
    // reads RAM. Refused (not enough RAM) or failed → the image goes back to disk, the publish still succeeds.
    let mut cache = "disk";
    if st.db.image(id)?.is_some_and(|i| i.cache_mode == "zram") {
        steps.go("golden → RAM");
        match crate::goldenram::sync(st, name, true) {
            Ok(()) => cache = "RAM",
            Err(e) => {
                tracing::warn!("image {name}: golden not loaded into RAM ({e}) → cache_mode=disk");
                let _ = st.db.set_cache_mode(id, "disk");
                cache = "disk (RAM refused: see the log)";
            }
        }
    } else {
        let _ = crate::goldenram::sync(st, name, false);
    }
    std::fs::write(format!("{out}/files.sha256"), sums).map_err(|e| format!("files.sha256: {e}"))?;
    std::fs::write(&sum_file, &hash).map_err(|e| format!("golden.sha256: {e}"))?;
    let bs = format!(
        "kernel http://${{broom-srv}}/tftp/broom-stage/vmlinuz initrd=stage.img ip=dhcp BOOTIF=01-${{mac:hexhyp}} broom.name={name} broom.hash={hash} broom.srv=${{broom-srv}} broom.host=${{broom-host}} broom.lic=${{broom-lic}} broom.reg=${{broom-reg}} broom.base=${{broom-base}} broom.strict=${{broom-strict}} broom.lxgb=${{broom-lxgb}} broom.wbgb=${{broom-wbgb}}\n\
         initrd http://${{broom-srv}}/tftp/broom-stage/stage.img\n\
         boot"
    );
    let before = st.db.image(id)?;
    st.db.set_published(id, &bs, &hash)?;
    // A new golden → every machine booting it rebuilds its base, which needs the license key once more.
    if let Some(img) = before.filter(|i| i.hash.as_deref() != Some(hash.as_str())) {
        rearm_for_image(st, &img);
    }
    Ok(format!(
        "Publish OK — Windows '{name}': golden.vhdx ({cache}) + EFI + child templates; stage {stage}; boot-start disk drivers: {drivers}"
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

/// All output files exist (golden.key = the last build finished) AND golden.vhdx is newer than image.img (not
/// re-uploaded since). The server writes nothing into the golden, so new mgmt code never needs a rebuild (the scripts
/// go to the client through BROOMWIN instead); the key's content doesn't matter.
fn golden_fresh(raw: &str, out: &str) -> bool {
    let mtime = |p: &str| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let files = ["golden.vhdx", "base-template.vhdx", "child-template.vhdx", "child-template.off", "efi.tar.gz", "golden.key"];
    files.iter().all(|f| Path::new(&format!("{out}/{f}")).exists())
        && matches!((mtime(&format!("{out}/golden.vhdx")), mtime(raw)), (Some(g), Some(r)) if g > r)
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
    // 1. The partition holding Windows — check the hive BEFORE any write; then everything read from it that can refuse
    //    the publish (dirty volume, prep markers, EFI bundle) — the server never writes inside NTFS.
    steps.go("find Windows partition");
    let (start, size) = find_windows(raw)?;
    steps.go("read prep results + EFI");
    let mut vol = crate::ntfsread::Vol::open(raw, (start, size))?;
    if vol.is_dirty()? {
        return Err("Windows was not shut down cleanly (hibernated, Fast Startup or forced power-off) — boot the VM, run \
                    the prep command again and let it power off by itself, then upload"
            .into());
    }
    let mut drivers = boot_storage_done(&mut vol)?;
    if !vol.exists("broom/efi/EFI/Microsoft/Boot/BCD") {
        return Err("golden is missing C:\\broom\\efi\\EFI\\Microsoft\\Boot\\BCD — run broom-prep-win in the VM before sysprep".into());
    }
    let efi = crate::work_dir().join(format!("efi-{name}"));
    let _ = std::fs::remove_dir_all(&efi);
    // Served under its real name only once the new golden is in place (below), like every other output file.
    let r = vol.extract_dir("broom/efi", &efi).and_then(|_| crate::archive::tar_gz(&efi, Path::new(&format!("{out}/efi.tar.gz.new"))));
    let _ = std::fs::remove_dir_all(&efi);
    r?;
    if !has_stub(&mut vol) {
        drivers.push_str("; WARNING: golden prepped by an older version — broom-done/boot-order scripts inside it won't \
                          update with the server (run the Windows prep again when convenient)");
    }
    drop(vol);

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
    // Only the tables are rewritten — the NTFS data of the Windows partition stays as it is. FIRST, before any hole:
    // once the table shows a single partition the disk counts as trimmed, so a publish cut off during the holes
    // below never re-saves the (by then zeroed) boot partitions over the kept ones — it just punches again.
    crate::disk::write_single_gpt(raw, start, size).map_err(|e| format!("golden single partition: {e}"))?;
    // Keep the first 1MB (primary GPT) + the last 1MB (backup GPT).
    for (off, len) in [(MB, start.saturating_sub(MB)), (end, total.saturating_sub(MB).saturating_sub(end))] {
        if len > 0 {
            punch_hole(raw, off, len)?;
        }
    }

    // 3. golden.vhdx (dynamic, blocks in disk order) + 2 empty child VHDX: base-template (parent golden) and
    //    child-template (parent base.vhdx — base's GUID is only known on the client → the stage patches it at an offset).
    steps.go("convert raw→vhdx");
    let golden = format!("{out}/golden.vhdx");
    let tmp = format!("{golden}.tmp");
    // The VHDX holds the raw's allocated blocks (holes stay out), next to the old golden until it replaces it.
    let used = std::fs::metadata(raw).map(|m| std::os::unix::fs::MetadataExt::blocks(&m) * 512).unwrap_or(0);
    crate::publish::need_space(Path::new(out), used, "building golden.vhdx")?;
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
    std::fs::rename(format!("{out}/efi.tar.gz.new"), format!("{out}/efi.tar.gz")).map_err(|e| format!("efi.tar.gz: {e}"))?;
    let gi = crate::vhdx::read_info(&golden)?;
    crate::vhdx::write_empty(&format!("{out}/base-template.vhdx"), &gi, Some(".\\golden.vhdx"))?;
    let placeholder = crate::vhdx::Info { data_write_guid: [0; 16], ..gi };
    let off = crate::vhdx::write_empty(&format!("{out}/child-template.vhdx"), &placeholder, Some(".\\base.vhdx"))?;
    std::fs::write(format!("{out}/child-template.off"), off.to_string()).map_err(|e| e.to_string())?;
    std::fs::write(format!("{out}/golden.key"), "built").map_err(|e| e.to_string())?;
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
