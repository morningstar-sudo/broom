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
use prep::{boot_storage_done, has_stub};
pub(crate) use stage::STAGE_PIN; // `bootrom-mgmt pins`
#[cfg(feature = "external-assets")]
pub(crate) use stage::download;
pub use stage::{build_bundle, ensure_stage}; // assets.rs: the release's broom-assets.zip

use std::path::Path;
use std::process::Command;

use crate::{SharedState, images_dir};

/// Windows stage (kernel + initrd) served at /tftp/broom-stage/.
fn stage_dir() -> String {
    crate::tftp_dir()
        .join("broom-stage")
        .to_string_lossy()
        .into_owned()
}

/// The Windows scripts (scripts/<name>) the stage puts in BROOMWIN broom\ (the golden only holds fixed stubs that run
/// them). ASCII only.
/// - broom-done.ps1: first logon (when base.vhdx is created on each machine): writes base.ok to BROOMWIN, then restarts
///   at once → the stage commits base. With base mode on for this machine (Machines page; broom\basemode.txt from the
///   stage): no restart, a popup says changes are kept until base mode is turned off on the server. BROOMWIN
///   has no drive letter (GPT bit 63, set by the stage) → written through its volume path `\\?\Volume{..}\`.
/// - broom-bootorder.ps1: the BroomBootOrder task (SYSTEM, at startup + every 5 minutes). Windows pulls "Windows Boot
///   Manager" to the top of BootOrder every boot → the next power-on would skip PXE (no reset): restores the order the
///   stage wrote to broom\bootorder.txt — by Boot#### NUMBER, never by entry name (PXE entries are named anything).
///   Entries not in the file (added later) are kept, after. Writes NVRAM only when different. Also the pagefile.
/// - broom-games.ps1: games disks as drive letters (games.rs), run by broom-bootorder.ps1.
/// - broom-watch.ps1: the session watchdog (SSD room) + Remote agent, started by broom-bootorder.ps1 as its own task.
/// - broom-fw.cs / broom-vnc.cs: C# sources of the UEFI/pagefile helper and of the Remote agent, compiled by the
///   client when the release ships no broom-fw.dll / broom-vnc.exe (BINARIES).
const SCRIPTS: [&str; 6] = [
    "broom-done.ps1",
    "broom-bootorder.ps1",
    "broom-games.ps1",
    "broom-watch.ps1",
    "broom-fw.cs",
    "broom-vnc.cs",
];
/// Compiled broom-fw.cs / broom-vnc.cs, next to the scripts when this release has them (CI). Not listed = the stage
/// removes BROOMWIN's copy: a DLL / exe SYSTEM runs must never be one a guest left there.
const BINARIES: [&str; 2] = ["broom-fw.dll", "broom-vnc.exe"];

/// At start: the Windows scripts of this version into tftp/broom-scripts/ + their scripts.sha256, which the stage
/// checks BROOMWIN's copies against on every boot — like the stage itself, a new mgmt version reaches every client at
/// its next boot, for every image, without a publish. Files first, list last (each by rename): a stage reading
/// meanwhile gets a mismatch at worst, and reboots to try again.
pub fn install_scripts() -> Result<(), String> {
    let dir = crate::tftp_dir().join("broom-scripts");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut files: Vec<(&str, std::borrow::Cow<[u8]>)> = SCRIPTS
        .iter()
        .map(|f| {
            let body = crate::assets::text(&format!("scripts/{f}"));
            (*f, body.replace('\n', "\r\n").into_bytes().into())
        })
        .collect();
    for b in BINARIES {
        match crate::assets::optional(&format!("scripts/{b}")) {
            Some(bin) => files.push((b, bin.into())),
            None => {
                let _ = std::fs::remove_file(dir.join(b)); // of another release
            }
        }
    }
    let mut sums = String::new();
    for (f, body) in files {
        let p = dir.join(f);
        if std::fs::read(&p).ok().as_deref() != Some(&body[..]) {
            crate::versions::write_durable(&p, &body)
                .map_err(|e| format!("{}: {e}", p.display()))?;
        }
        let h =
            crate::hash::file_hash(&p.to_string_lossy()).ok_or(format!("sha256 of {f} failed"))?;
        sums.push_str(&format!("{h}  {f}\n"));
    }
    let p = dir.join("scripts.sha256");
    if std::fs::read_to_string(&p).ok().as_deref() != Some(sums.as_str()) {
        crate::versions::write_durable(&p, sums.as_bytes())
            .map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(())
}

pub(crate) fn run(bin: &str, args: &[&str]) -> Result<String, String> {
    tracing::debug!("exec: {bin} {}", args.join(" "));
    let o = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("{bin}: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        Err(format!(
            "{bin} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        ))
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
    parts.sort_by_key(|p| std::cmp::Reverse(p.1));
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
        if seen.is_empty() {
            "none".to_string()
        } else {
            seen.join(" | ")
        }
    ))
}

/// Publish a Windows image from images/<name>/image.img (raw whole VM disk). Blocking.
pub fn publish(
    st: &SharedState,
    id: i64,
    name: &str,
    steps: &mut crate::publish::Steps,
) -> Result<String, String> {
    let raw = images_dir().join(name).join("image.img");
    let raw = std::fs::canonicalize(&raw)
        .map_err(|e| format!("no golden raw yet ({}): {e}", raw.display()))?;
    let raw = raw.to_string_lossy().to_string();
    let out = crate::tftp_dir()
        .join("broom-win")
        .join(name)
        .to_string_lossy()
        .into_owned();
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    // Whatever can refuse the publish runs before anything is written: a failure here leaves the served golden as is.
    let ip = st.db.get_config("dhcp_server_ip", "");
    if ip.is_empty() {
        return Err("dhcp_server_ip is empty — run `setup` first".into());
    }
    steps.go("initrd stage");
    let stage = ensure_stage()?;

    // image.img unchanged since the golden was built (golden.id = its stamp) → keep it (same bytes, same hash: no client
    // downloads again), only refresh the stage + scripts + boot_script (Publish with a new binary = cheap).
    // Hash (clients compare it to know whether to re-download, and check the whole download against it) + size (the
    // stage checks its free space and the downloaded length). golden.sha256 is written LAST: the stage treats its
    // absence as "publish running".
    let sum_file = format!("{out}/golden.sha256");
    let (drivers, hash) = match golden_fresh(&raw, &out) {
        Some((id, hash)) => {
            // After a server restart the golden is made again from image.img (same id → same bytes).
            if crate::goldenram::served_id(name).as_deref() != Some(id.as_str()) {
                crate::goldenram::install(
                    name,
                    &id,
                    crate::vhdx::Virtual::open(Path::new(&raw), &id)?,
                );
            }
            (
                "kept (golden already built from image.img)".to_string(),
                hash,
            )
        }
        None => build_golden(&raw, &out, name, steps)?,
    };
    // The stage checks these small files against this list on every boot (a guest could swap them on the SSD).
    // First line: the golden these files belong to — the stage applies them only to that golden (a client still on
    // the previous hash during a publish never gets the new templates next to its old golden).
    // (The Windows scripts are not per image: install_scripts.)
    let mut sums = format!("{hash}  golden\n");
    for f in [
        "efi.tar.gz",
        "child-template.vhdx",
        "child-template.off",
        "base-template.vhdx",
    ] {
        let h =
            crate::hash::file_hash(&format!("{out}/{f}")).ok_or(format!("sha256 of {f} failed"))?;
        sums.push_str(&format!("{h}  {f}\n"));
    }
    // Cache mode RAM: the copy loaded BEFORE golden.sha256 tells the stages to download, so the room's first rush
    // reads RAM. Refused (not enough RAM) or failed → the image goes back to disk, the publish still succeeds.
    let mut cache = "disk";
    if st.db.image(id)?.is_some_and(|i| i.cache_mode == "zram") {
        steps.go("golden → RAM");
        match crate::goldenram::set_ram(st, name, true) {
            Ok(()) => cache = "RAM",
            Err(e) => {
                tracing::warn!("image {name}: golden not loaded into RAM ({e}) → cache_mode=disk");
                let _ = st.db.set_cache_mode(id, "disk");
                cache = "disk (RAM refused: see the log)";
            }
        }
    } else {
        let _ = crate::goldenram::set_ram(st, name, false);
    }
    std::fs::write(format!("{out}/files.sha256"), sums)
        .map_err(|e| format!("files.sha256: {e}"))?;
    std::fs::write(&sum_file, &hash).map_err(|e| format!("golden.sha256: {e}"))?;
    let bs = boot_script(name, &hash);
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
/// The image's iPXE boot script: the stage kernel + initrd, its broom.* parameters (the `${broom-*}` ones are set per
/// boot by /boot/start). Network: the IP iPXE booted with, as a static setting (`ip=<ip>::<gw>:<mask>:::off`; the card
/// = BOOTIF) — `ip=dhcp` would ask DHCP again and wait out its duplicate-address probe (~5 s of every boot). Safe: the
/// stage lives ~10 s inside iPXE's fresh lease, then Windows asks DHCP itself. (A Linux image keeps `ip=dhcp`
/// (publish.rs): it runs for hours on that network and needs a lease it renews.)
fn boot_script(name: &str, hash: &str) -> String {
    format!(
        "kernel http://${{broom-srv}}/tftp/broom-stage/vmlinuz initrd=stage.img ip=${{ip}}::${{gateway}}:${{netmask}}:::off BOOTIF=01-${{mac:hexhyp}} broom.name={name} broom.hash={hash} broom.srv=${{broom-srv}} broom.host=${{broom-host}} broom.lic=${{broom-lic}} broom.reg=${{broom-reg}} broom.base=${{broom-base}} broom.strict=${{broom-strict}} broom.lxgb=${{broom-lxgb}} broom.wbgb=${{broom-wbgb}}\n\
         initrd http://${{broom-srv}}/tftp/broom-stage/stage.img\n\
         boot"
    )
}

fn rearm_for_image(st: &SharedState, img: &crate::db::Image) {
    for m in st.db.machines().unwrap_or_default() {
        let boots_it = m.image_id == Some(img.id) || (m.image_id.is_none() && img.is_default);
        if boots_it && st.db.rearm_quiet(m.id).unwrap_or(false) {
            tracing::info!(
                "license of {} armed again (new golden {} → base rebuilt)",
                m.hostname.as_deref().unwrap_or(&m.mac),
                img.name
            );
        }
    }
}

/// The last build is complete and still of image.img as it is (golden.id = its stamp: not re-uploaded / rolled back
/// since) → (id, sha256). The server writes nothing into the golden, so new mgmt code never needs a rebuild (the
/// scripts go to the client through BROOMWIN instead).
fn golden_fresh(raw: &str, out: &str) -> Option<(String, String)> {
    let files = [
        "base-template.vhdx",
        "child-template.vhdx",
        "child-template.off",
        "efi.tar.gz",
        "golden.size",
    ];
    let rd = |f: &str| {
        std::fs::read_to_string(format!("{out}/{f}"))
            .ok()
            .map(|s| s.trim().to_string())
    };
    let id = rd("golden.id")
        .filter(|id| crate::hash::stamp(Path::new(raw)).as_deref() == Some(id.as_str()))?;
    let hash = rd("golden.sha256").filter(|h| h.len() == 64)?;
    files
        .iter()
        .all(|f| Path::new(&format!("{out}/{f}")).exists())
        .then_some((id, hash))
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

/// image.img (raw whole VM disk) → the served golden.vhdx (made from it, not written) + efi.tar.gz + 2 empty child
/// VHDX. Returns (the enabled drivers, the golden's sha256).
fn build_golden(
    raw: &str,
    out: &str,
    name: &str,
    steps: &mut crate::publish::Steps,
) -> Result<(String, String), String> {
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
    let drivers = boot_storage_done(&mut vol)?;
    if !vol.exists("broom/efi/EFI/Microsoft/Boot/BCD") {
        return Err("golden is missing C:\\broom\\efi\\EFI\\Microsoft\\Boot\\BCD — run broom-prep-win in the VM before sysprep".into());
    }
    let efi = crate::work_dir().join(format!("efi-{name}"));
    let _ = std::fs::remove_dir_all(&efi);
    // Served under its real name only once the new golden is in place (below), like every other output file.
    let r = vol
        .extract_dir("broom/efi", &efi)
        .and_then(|_| crate::archive::tar_gz(&efi, Path::new(&format!("{out}/efi.tar.gz.new"))));
    let _ = std::fs::remove_dir_all(&efi);
    r?;
    if !has_stub(&mut vol) {
        return Err("golden prepped by an older version (its scripts would never update with the server) — boot the \
                    VM, run the Windows prep command again (Images page), let it power off, then upload"
            .into());
    }
    drop(vol);

    // 2. Edit image.img IN PLACE (no temporary full-disk copy): punch holes outside the Windows partition (ESP/
    //    MSR/Recovery don't go into the golden) + GPT with a single partition (standard native VHD boot), KEEP start →
    //    NTFS "hidden sectors" still match. An image already trimmed is not written at all: its stamp (= the golden's
    //    identity, see vhdx::Virtual) stays, and a whole-file version sharing it (versions::pin) is never touched.
    steps.go("trim disk");
    // Export (export.rs) rebuilds the VM disk from what the trim below destroys → keep it first. Never blocks publish.
    if let Err(e) = crate::export::keep_boot_regions(Path::new(raw), name, (start, size)) {
        tracing::warn!(
            "image {name}: boot partitions not kept, export won't work for this upload: {e}"
        );
    }
    const MB: u64 = 1024 * 1024;
    let total = std::fs::metadata(raw).map_err(|e| e.to_string())?.len();
    let end = start + size;
    // Keep the first 1MB (primary GPT) + the last 1MB (backup GPT).
    let holes = [
        (MB, start.saturating_sub(MB)),
        (end, total.saturating_sub(MB).saturating_sub(end)),
    ];
    let single = crate::disk::read(raw)?.is_some_and(|t| {
        t.parts.len() == 1 && (t.parts[0].start, t.parts[0].size) == (start, size)
    });
    let outside: Vec<(u64, u64)> = holes
        .into_iter()
        .filter(|&(off, len)| {
            // Not entirely a hole.
            len > 0
                && crate::disk::Source::new(&[(Path::new(raw), off, Some(len))])
                    .is_ok_and(|s| !s.data_ranges().is_empty())
        })
        .collect();
    if !single || !outside.is_empty() {
        // A whole-file version shares this very file (its publish failed before the trim): trim a copy of it.
        if std::os::unix::fs::MetadataExt::nlink(
            &std::fs::metadata(raw).map_err(|e| e.to_string())?,
        ) > 1
        {
            steps.go("trim disk (own copy: a version shares image.img)");
            let tmp = format!("{raw}.new");
            crate::disk::copy_region(Path::new(raw), 0, None, Path::new(&tmp))
                .and_then(|_| std::fs::rename(&tmp, raw).map_err(|e| e.to_string()))
                .inspect_err(|_| {
                    let _ = std::fs::remove_file(&tmp);
                })?;
        }
        // Only the tables are rewritten — the NTFS data of the Windows partition stays as it is. FIRST, before any
        // hole: once the table shows a single partition the disk counts as trimmed, so a publish cut off during the
        // holes below never re-saves the (by then zeroed) boot partitions over the kept ones — it just punches again.
        if !single {
            crate::disk::write_single_gpt(raw, start, size)
                .map_err(|e| format!("golden single partition: {e}"))?;
        }
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(raw)
            .map_err(|e| format!("{raw}: {e}"))?;
        for (off, len) in outside {
            crate::disk::punch_hole(&f, off, len)
                .map_err(|e| format!("punch hole {raw} +{len}: {e}"))?;
        }
    }

    // 3. The golden.vhdx stages download: made from image.img as it is now (trimmed) — header in memory, payload read
    //    from image.img, nothing written — identified by image.img's stamp. Its sha256 = one read of image.img.
    steps.go("sha256 golden");
    let id = crate::hash::stamp(Path::new(raw)).ok_or("image.img vanished")?;
    let v = crate::vhdx::Virtual::open(Path::new(raw), &id)?;
    let hash = v.sha256()?;

    // 4. + 2 empty child VHDX: base-template (parent golden) and child-template (parent base.vhdx — base's GUID is only
    //    known on the client → the stage patches it at an offset).
    // Until here the served files (old golden + templates + golden.sha256) still belong together, so a failure above
    // never disturbs a client. From now on they change: no golden.sha256 → a client downloading sees "publish running"
    // instead of mixing two versions; no golden.id → a failure below makes the next publish rebuild everything.
    for f in ["golden.sha256", "golden.size", "golden.id", "golden.key"] {
        let _ = std::fs::remove_file(format!("{out}/{f}"));
    }
    std::fs::rename(format!("{out}/efi.tar.gz.new"), format!("{out}/efi.tar.gz"))
        .map_err(|e| format!("efi.tar.gz: {e}"))?;
    crate::vhdx::write_empty(
        &format!("{out}/base-template.vhdx"),
        &v.info,
        ".\\golden.vhdx",
    )?;
    let placeholder = crate::vhdx::Info {
        data_write_guid: [0; 16],
        ..v.info
    };
    let off = crate::vhdx::write_empty(
        &format!("{out}/child-template.vhdx"),
        &placeholder,
        ".\\base.vhdx",
    )?;
    std::fs::write(format!("{out}/child-template.off"), off.to_string())
        .map_err(|e| e.to_string())?;
    std::fs::write(format!("{out}/golden.size"), v.len().to_string())
        .map_err(|e| format!("golden.size: {e}"))?;
    crate::goldenram::install(name, &id, v);
    std::fs::write(format!("{out}/golden.id"), &id).map_err(|e| format!("golden.id: {e}"))?;
    Ok((drivers, hash))
}

#[cfg(test)]
#[path = "../../tests/unit/winstage/mod.rs"]
mod tests;
