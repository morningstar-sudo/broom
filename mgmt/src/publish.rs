// publish.rs — after a golden is uploaded (golden.rs: upload → image.img), make the image bootable. Linux: kernel +
// initrd out of the golden (overlay.rs) + shared read-only iSCSI target (disk or zram) → iPXE boot_script loads the
// kernel/initrd, the initrd attaches iSCSI + the SSD overlay. Windows: winstage/ (native VHDX boot from the SSD).
// Also the iSCSI/zram life cycle (restore at start, new generation per publish, gc) and the Secure Boot shim copy.
use std::path::Path;
use std::process::Command;

use crate::{images_dir, SharedState};

/// Publish job progress: each step shows "⏳ <step>..." on the web + is timed; the ✓ result includes
/// a per-step timing table → you see right away where it is slow.
pub(crate) struct Steps<'a> {
    st: &'a SharedState,
    name: String,
    cur: Option<(String, std::time::Instant)>,
    done: Vec<String>,
}

impl<'a> Steps<'a> {
    pub fn new(st: &'a SharedState, name: &str) -> Self {
        Steps { st, name: name.to_string(), cur: None, done: Vec::new() }
    }
    /// Finish the running step (record its time), start a new one.
    pub fn go(&mut self, label: &str) {
        self.end();
        tracing::info!("image {}: {label}", self.name);
        self.st.set_job(&self.name, format!("⏳ {label}..."));
        self.cur = Some((label.to_string(), std::time::Instant::now()));
    }
    fn end(&mut self) {
        if let Some((l, t)) = self.cur.take() {
            let s = t.elapsed().as_secs();
            self.done.push(if s >= 60 { format!("{l} {}m{:02}s", s / 60, s % 60) } else { format!("{l} {s}s") });
        }
    }
    /// "step1 12s, step2 3m05s, ..."
    pub fn summary(mut self) -> String {
        self.end();
        self.done.join(", ")
    }
}

/// Free bytes on the filesystem holding `path` (libc statvfs). 0 on error (callers treat 0 as "unknown → allow").
pub(crate) fn free_bytes(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return 0 };
    // SAFETY: c is a valid NUL-terminated path; s is written by statvfs before use.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return 0;
    }
    (s.f_bavail as u64).saturating_mul(s.f_frsize as u64)
}

/// Refuse a big write up front when the filesystem of `dir` can't take `need` bytes + 1 GiB (instead of failing hours
/// later on a full disk, which also starves the DB and every other job). Unknown free space → allowed.
pub(crate) fn need_space(dir: &Path, need: u64, what: &str) -> Result<(), String> {
    let free = free_bytes(dir);
    let want = need.saturating_add(1 << 30);
    if free != 0 && free < want {
        return Err(format!(
            "{what} needs ~{:.1} GB free in {}, only {:.1} GB",
            want as f64 / 1e9,
            dir.display(),
            free as f64 / 1e9
        ));
    }
    Ok(())
}

/// Does the img fit in RAM for zram: needs avail >= img + reserve. avail=0 (unreadable) → allow.
fn zram_fits(img: u64, avail: u64, reserve: u64) -> bool {
    avail == 0 || img.saturating_add(reserve) <= avail
}

/// Available RAM (bytes) from /proc/meminfo MemAvailable. 0 if unreadable (check skipped).
fn mem_available_bytes() -> u64 {
    let s = match std::fs::read_to_string("/proc/meminfo") {
        Ok(s) => s,
        Err(_) => return 0,
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            // "MemAvailable:   12345678 kB"
            if let Some(kb) = rest.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()) {
                return kb * 1024;
            }
        }
    }
    0
}

/// Publish an image according to its os. Blocking (called from spawn_blocking).
/// Secure Boot clients (official signed iPXE, Network page) boot the Canonical-signed Ubuntu kernels — the Windows
/// stage and Linux goldens alike — through Ubuntu's Microsoft-signed shim (from the stage bundle, else the server's
/// shim-signed package): any Ubuntu shim verifies any Canonical-signed kernel. Copied to tftp/shim/shimx64.efi at every
/// publish (cheap); boot.rs adds the `shim` line. Missing → only Secure Boot clients are affected (warning).
pub(crate) fn refresh_shim() {
    // The stage bundle (CI-built, see winstage::ensure_stage) carries Ubuntu's shim; else the server's own shim-signed.
    let bundled = crate::tftp_dir().join("broom-stage/shimx64.efi").to_string_lossy().into_owned();
    let src = [bundled.as_str(), "/usr/lib/shim/shimx64.efi.signed.latest", "/usr/lib/shim/shimx64.efi.signed"]
        .into_iter()
        .find(|p| Path::new(p).is_file());
    let Some(src) = src else {
        return tracing::warn!("no shim: the stage bundle is not installed (see the 'stage bundle' log line) and no shim-signed package → Secure Boot clients cannot boot the kernels");
    };
    let dir = crate::tftp_dir().join("shim");
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::copy(src, dir.join("shimx64.efi"))) {
        tracing::warn!("copy {src} → {}: {e}", dir.display());
    }
}

/// The stage bundle (Windows stage + Secure Boot shim): checked, fetched when missing or not this binary's own, then
/// the shim copied to tftp/shim/. Runs at start and when Secure Boot is switched on, so neither waits for the first
/// Windows publish. Blocking (a download).
pub(crate) fn prepare_stage() {
    match crate::winstage::ensure_stage() {
        Ok(s) => tracing::info!("stage bundle: {s}"),
        Err(e) => tracing::warn!("stage bundle: {e}"),
    }
    refresh_shim();
}

pub fn run_publish(st: &SharedState, name: &str, steps: &mut Steps) -> Result<String, String> {
    let img = st.db.image_by_name(name)?.ok_or(format!("image '{name}' not found in DB"))?;
    let (id, os) = (img.id, img.os);
    let r = match os.as_str() {
        "linux" => {
            // Secure Boot clients boot Linux goldens through the shim, which comes with the stage bundle.
            if st.db.get_config("ipxe_signed", "0") == "1" {
                if let Err(e) = crate::winstage::ensure_stage() {
                    tracing::warn!("stage bundle (for its Secure Boot shim): {e}");
                }
            }
            steps.go("publish linux (kernel/initrd + iSCSI)");
            publish_iscsi(st, id, name)
        }
        "windows" => crate::winstage::publish(st, id, name, steps),
        other => Err(format!("invalid os: {other} (linux|windows)")),
    };
    refresh_shim(); // after: a Windows publish may just have installed the stage bundle (and its shim)
    r
}

/// Shared RO iSCSI target for an image (kernel LIO via configfs, iscsi.rs). `backing` = golden
/// file (disk) or /dev/zramN (zram). Idempotent (re-creates). Returns the IQN.
fn export_target(st: &SharedState, name: &str, cache_mode: &str, backing: &str) -> Result<String, String> {
    let iqn = iqn_of(st, name);
    let store = store_of(name, gen_of(st, name));
    let lio = crate::iscsi::Lio::system()?;
    // Targets made before iqn_base used a fixed IQN — remove that one too (legacy; drop once no such server is left).
    lio.remove(name, &format!("iqn.2026-08.net.tiem:{name}"));
    let b = if cache_mode == "zram" {
        crate::iscsi::Backing::Block { dev: backing }
    } else {
        let size = std::fs::metadata(backing).map_err(|e| format!("{backing}: {e}"))?.len();
        crate::iscsi::Backing::File { path: backing, size }
    };
    lio.export(&store, b, &iqn).map_err(|e| format!("iSCSI target: {e}"))?;
    tracing::info!("iSCSI target {iqn} ready ({cache_mode}: {backing})");
    Ok(iqn)
}

/// configfs targets are gone after a server reboot → re-export every published Linux image at
/// start (disk: target only; zram: publish again = new RAM copy + target).
/// An mgmt restart (new binary) keeps both → live targets are left alone (clients stay connected).
pub fn restore_targets(st: &SharedState) {
    let lio = match crate::iscsi::Lio::system() {
        Ok(l) => l,
        Err(e) => return tracing::error!("iSCSI targets not restored: {e}"),
    };
    for img in st.db.images().unwrap_or_default() {
        if img.os != "linux" || img.boot_script.is_none() || lio.has_target(&iqn_of(st, &img.name)) {
            continue;
        }
        let r = if img.cache_mode == "zram" {
            // The RAM copy died with the reboot (the old /dev/zramN may now be someone else's) →
            // forget it, publish again = new zram + target. Falls back to disk by itself on RAM overflow.
            let _ = st.db.set_config(&format!("zram_dev:{}", img.name), "");
            publish_iscsi(st, img.id, &img.name)
        } else {
            let path = images_dir().join(&img.name).join("image.img");
            std::fs::canonicalize(&path)
                .map_err(|e| format!("{}: {e}", path.display()))
                .and_then(|p| export_target(st, &img.name, "disk", &p.to_string_lossy()))
        };
        match r {
            Ok(_) => tracing::info!("iSCSI target for image {} restored ({})", img.name, img.cache_mode),
            Err(e) => tracing::error!("iSCSI target for image {} not restored: {e}", img.name),
        }
    }
}

/// Golden Linux: build kernel/initrd (overlay.rs) + serve iSCSI RO shared (disk|zram) +
/// iPXE boot_script (loads kernel/initrd, the initrd attaches iSCSI + SSD overlay itself). Blocking.
fn publish_iscsi(st: &SharedState, id: i64, name: &str) -> Result<String, String> {
    let img = images_dir().join(name).join("image.img");
    let img_abs = std::fs::canonicalize(&img)
        .map_err(|e| format!("no golden raw yet ({}): {e}", img.display()))?;
    // Every refusal BEFORE anything is written: a refused publish leaves the served image exactly as it was.
    let want = st.db.image(id)?.map_or_else(|| "disk".into(), |i| i.cache_mode);
    // Disk cache serves ONE shared golden file → it can't be swapped under a live client. Refuse to (re)publish it
    // while a client of this image is connected (they would read changed bytes mid-session → FS corruption). zram is
    // fine: each publish makes a fresh device + target, and the old one is kept for already-connected clients.
    if want != "zram" && image_in_use(st, name) {
        return Err("clients are connected; a disk-cache image shares one golden file and can't be swapped live. \
                    Reboot/close the clients (publish off-hours), or set this image to zram cache."
            .into());
    }
    let ip = st.db.get_config("dhcp_server_ip", "");
    if ip.is_empty() {
        return Err("dhcp_server_ip is empty — run `setup` first".into());
    }
    // Kernel and initrd are built in tftp/broom/<name>.new/ and swapped in only right before the new
    // boot script is saved: until then a booting client gets the old files that match the old cmdline.
    let live = crate::tftp_dir().join("broom").join(name);
    let staged = crate::tftp_dir().join("broom").join(format!("{name}.new"));
    let _ = std::fs::remove_dir_all(&staged);
    let out = publish_iscsi_staged(st, id, name, &img_abs, &want, &ip, &staged);
    match out {
        Ok((bs, hash, cache_mode)) => {
            let old = crate::tftp_dir().join("broom").join(format!("{name}.old"));
            let _ = std::fs::remove_dir_all(&old);
            let _ = std::fs::rename(&live, &old);
            std::fs::rename(&staged, &live).map_err(|e| format!("swap in {}: {e}", live.display()))?;
            let _ = std::fs::remove_dir_all(&old);
            st.db.set_published(id, &bs, &hash)?;
            gc_superseded(st, name);
            Ok(format!("Publish OK — golden '{name}' iSCSI RO ({cache_mode}) + kernel/initrd + boot_script overlay"))
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staged);
            Err(e)
        }
    }
}

/// publish_iscsi's work into `staged`: kernel/initrd, the iSCSI target. → (boot script, hash, cache mode).
fn publish_iscsi_staged(
    st: &SharedState,
    id: i64,
    name: &str,
    img_abs: &Path,
    want: &str,
    ip: &str,
    staged: &Path,
) -> Result<(String, String, String), String> {
    // 1. Extract kernel + initrd from the golden, read the root UUID.
    let root_uuid = crate::overlay::build_boot(img_abs, name, staged)?;

    // 2. cache_mode (images column): disk → serve the file directly; zram → load the img into /dev/zramN.
    // zram fails (RAM overflow / error) → fall back to disk BY ITSELF (DB updated) so the image always boots.
    // New generation: new IQN + backstore (+ new zram device), leaving the previous target for connected clients.
    let _ = bump_gen(st, name);
    let g = gen_of(st, name);
    let (cache_mode, backing) = if want == "zram" {
        match ensure_zram(st, name, g, img_abs) {
            Ok(dev) => ("zram".to_string(), dev),
            Err(e) => {
                tracing::warn!("image {name}: zram failed ({e}) → falling back to cache_mode=disk");
                let _ = st.db.set_cache_mode(id, "disk");
                ("disk".to_string(), img_abs.to_string_lossy().to_string())
            }
        }
    } else {
        ("disk".to_string(), img_abs.to_string_lossy().to_string())
    };

    // 3. Shared RO iSCSI target (zram = block backstore, disk = fileio). Superseded ones are dropped by the caller
    //    once the new boot script is saved (a client may still boot the old one until then).
    let iqn = export_target(st, name, &cache_mode, &backing)?;

    // 4. iPXE boot_script. The initrd hook reads broom.name/hash/size/srv/reg/lxgb/ssd from the cmdline.
    // sanhook = iPXE attaches iSCSI via iBFT (does not boot the LUN); initrd open-iscsi reads the iBFT →
    // /dev/sda golden RO → root=UUID mounted RO; overlayroot (baked into the golden) overlays it onto the
    // SSD writeback (reset every boot). ip=dhcp gives the initrd a network.
    // overlayroot on the CMDLINE (takes precedence over the conf file) → root RO + upper on the SSD LABEL broomwb.
    // `quiet` left out so overlayroot/broom logs show on the client console (easier to debug a boot).
    // broom.name/hash/size: the initrd hook compares the hash with the SSD cache copy (match → boot from the SSD,
    // skip iSCSI; mismatch → iSCSI + background copy). Hash computed FIRST to embed it in the cmdline.
    let hash = crate::hash::file_hash(&img_abs.to_string_lossy()).ok_or_else(|| format!("sha256 of {} failed", img_abs.display()))?;
    let size = std::fs::metadata(img_abs).map_err(|e| e.to_string())?.len();
    let bs = format!(
        "sanhook iscsi:{ip}::::{iqn} || shell\n\
         kernel http://{ip}/tftp/broom/{name}/vmlinuz initrd=initrd.img ip=dhcp root=UUID={root_uuid} ro fsck.mode=skip overlayroot=device:dev=/dev/disk/by-label/broomwb,recurse=0 broom.name={name} broom.hash={hash} broom.size={size} broom.srv={ip} broom.reg=${{broom-reg}} broom.lxgb=${{broom-lxgb}} broom.ssd=${{broom-ssd}}\n\
         initrd http://{ip}/tftp/broom/{name}/initrd.img\n\
         boot"
    );
    Ok((bs, hash, cache_mode))
}

/// Undo everything publish made for an image (image deleted): iSCSI target, zram device, boot files
/// (Linux kernel/initrd, Windows golden.vhdx + templates). Blocking.
pub fn unpublish(st: &SharedState, name: &str) {
    if let Ok(lio) = crate::iscsi::Lio::system() {
        // Every generation of this image's target + its zram device (the image is going away).
        drop_targets(st, &lio, name, None, false);
        lio.remove(name, &format!("iqn.2026-08.net.tiem:{name}")); // legacy fixed IQN (see restore above), drop later
    }
    // Legacy single-device key (pre-versioning).
    let old = st.db.get_config(&format!("zram_dev:{name}"), "");
    if !old.is_empty() {
        zram_remove(&old);
        let _ = st.db.set_config(&format!("zram_dev:{name}"), "");
    }
    for d in ["broom", "broom-win"] {
        let _ = std::fs::remove_dir_all(crate::tftp_dir().join(d).join(name));
    }
    for s in ["new", "old"] {
        let _ = std::fs::remove_dir_all(crate::tftp_dir().join("broom").join(format!("{name}.{s}")));
    }
}

/// Load the golden img into a zram device (zstd compressed), return /dev/zramN. Map stored in DB config.
/// Reset the image's old device (if any) before creating a new one.
fn ensure_zram(st: &SharedState, name: &str, g: u64, img: &Path) -> Result<String, String> {
    // The previous generation's device (if any) is left in place for connected clients and freed by gc_superseded
    // once they drain — this new publish gets its own device.
    let size = std::fs::metadata(img).map_err(|e| e.to_string())?.len();

    // VALIDATE RAM overflow: zram compresses but worst case (incompressible data) = full img size.
    // Require MemAvailable > img size + reserve (kept for the OS + iSCSI serving). The old device
    // was reset above so its RAM is returned; MemAvailable also reflects OTHER zram images being held.
    // reserve is set via the zram_reserve_mb config (web System page).
    let reserve = st.db.get_config("zram_reserve_mb", "2048").parse::<u64>().unwrap_or(2048).saturating_mul(1 << 20);
    let avail = mem_available_bytes();
    if !zram_fits(size, avail, reserve) {
        let gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
        return Err(format!(
            "zram would overflow RAM: img {:.1}GB + reserve {:.1}GB > available RAM {:.1}GB. \
             Use cache_mode=disk, lower the reserve, or add RAM to the server.",
            gb(size), gb(reserve), gb(avail)
        ));
    }

    // New zram device (zstd) + load the raw img into it.
    let dev = zram_add(size)?;
    let copy = || -> std::io::Result<()> {
        let mut r = std::fs::File::open(img)?;
        let mut w = std::fs::OpenOptions::new().write(true).open(&dev)?;
        std::io::copy(&mut r, &mut w)?;
        w.sync_all()
    };
    if let Err(e) = copy() {
        zram_remove(&dev);
        return Err(format!("copy golden → {dev}: {e}"));
    }
    let _ = st.db.set_config(&zram_key(name, g), &dev);
    tracing::info!("image {name}: golden loaded into {dev} ({:.1} GB, zstd)", size as f64 / 1e9);
    Ok(dev)
}

/// New zram device of `size` bytes via sysfs (replaces zramctl) → "/dev/zramN". zstd when the
/// kernel has it, else the kernel default.
fn zram_add(size: u64) -> Result<String, String> {
    let _ = Command::new("modprobe").arg("zram").status(); // may be built in
    let n = std::fs::read_to_string("/sys/class/zram-control/hot_add")
        .map_err(|e| format!("zram hot_add: {e} (kernel without zram?)"))?;
    let n = n.trim();
    let dev = format!("/dev/zram{n}");
    let _ = std::fs::write(format!("/sys/block/zram{n}/comp_algorithm"), "zstd");
    if let Err(e) = std::fs::write(format!("/sys/block/zram{n}/disksize"), size.to_string()) {
        zram_remove(&dev);
        return Err(format!("zram{n} disksize {size}: {e}"));
    }
    // udev creates the device node.
    for _ in 0..50 {
        if Path::new(&dev).exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(dev)
}

/// Reset + remove a zram device made by zram_add (nothing may hold it open).
fn zram_remove(dev: &str) {
    let Some(n) = dev.strip_prefix("/dev/zram") else { return };
    let _ = std::fs::write(format!("/sys/block/zram{n}/reset"), "1");
    let _ = std::fs::write("/sys/class/zram-control/hot_remove", n);
}

/// Publish generation for an image (config `iscsi_gen:<name>`, starts 0). Bumped on each Linux (re)publish so a new
/// target gets a NEW IQN + backstore, leaving the previous one serving already-connected clients.
fn gen_of(st: &SharedState, name: &str) -> u64 {
    st.db.get_config(&format!("iscsi_gen:{name}"), "0").parse().unwrap_or(0)
}

/// Increment the generation and return the new (current) IQN.
fn bump_gen(st: &SharedState, name: &str) -> String {
    let g = gen_of(st, name) + 1;
    let _ = st.db.set_config(&format!("iscsi_gen:{name}"), &g.to_string());
    iqn_of(st, name)
}

/// The current IQN for an image: `<iqn_base>:<name>.g<gen>`. Image names are `[A-Za-z0-9_-]` (no dot), so `.g` is an
/// unambiguous separator.
pub(crate) fn iqn_of(st: &SharedState, name: &str) -> String {
    format!("{}:{name}.g{}", st.db.get_config("iqn_base", "iqn.2026-01.local.broom"), gen_of(st, name))
}

/// LIO backstore name for a generation (must differ per gen, or two targets would collide on one backstore).
fn store_of(name: &str, g: u64) -> String {
    format!("{name}.g{g}")
}

/// DB key holding the zram device for one generation of an image.
fn zram_key(name: &str, g: u64) -> String {
    format!("zram_dev:{name}:g{g}")
}

/// Remove this image's iSCSI targets (every generation except `keep`) and free their zram devices. `idle_only`: skip
/// a target a client is logged in to (it keeps reading its own generation; a later pass drops it).
fn drop_targets(st: &SharedState, lio: &crate::iscsi::Lio, name: &str, keep: Option<&str>, idle_only: bool) {
    let prefix = format!("{}:{name}.g", st.db.get_config("iqn_base", "iqn.2026-01.local.broom"));
    for iqn in lio.list_iqns() {
        if !iqn.starts_with(&prefix) || keep == Some(iqn.as_str()) || (idle_only && lio.has_sessions(&iqn)) {
            continue;
        }
        if let Some(g) = iqn.rsplit_once(".g").and_then(|(_, s)| s.parse::<u64>().ok()) {
            lio.remove(&store_of(name, g), &iqn);
            let dev = st.db.get_config(&zram_key(name, g), "");
            if !dev.is_empty() {
                zram_remove(&dev);
                let _ = st.db.set_config(&zram_key(name, g), "");
            }
            tracing::info!("iSCSI: removed target {iqn}");
        }
    }
}

/// Drop every superseded generation of this image's target that no client uses any more. After each publish and
/// periodically (monitor.rs): a busy lab never has a moment with no client at all.
pub(crate) fn gc_superseded(st: &SharedState, name: &str) {
    if let Some(lio) = crate::iscsi::Lio::existing() {
        drop_targets(st, &lio, name, Some(&iqn_of(st, name)), true);
    }
}

/// Every target of this image, in use or not (rollback rewrites the file a disk-cache target serves).
pub(crate) fn drop_all_targets(st: &SharedState, name: &str) {
    if let Some(lio) = crate::iscsi::Lio::existing() {
        drop_targets(st, &lio, name, None, false);
    }
}

/// A client is logged in to some generation of this image's target. No LIO at all → nobody can be.
pub(crate) fn image_in_use(st: &SharedState, name: &str) -> bool {
    let Some(lio) = crate::iscsi::Lio::existing() else { return false };
    let prefix = format!("{}:{name}.g", st.db.get_config("iqn_base", "iqn.2026-01.local.broom"));
    lio.list_iqns().iter().any(|iqn| iqn.starts_with(&prefix) && lio.has_sessions(iqn))
}

#[cfg(test)]
mod tests {
    use super::*;
    const GB: u64 = 1024 * 1024 * 1024;

    /// The generation parsed back out of a versioned IQN (gc_superseded / unpublish rely on this). Image names have
    /// no dot, so `.g` is an unambiguous separator even for names like `pc-01`.
    #[test]
    fn iqn_generation_roundtrip() {
        let parse = |iqn: &str| iqn.rsplit_once(".g").and_then(|(_, s)| s.parse::<u64>().ok());
        assert_eq!(store_of("win11", 7), "win11.g7");
        assert_eq!(parse("iqn.2026-01.local.broom:win11.g7"), Some(7));
        assert_eq!(parse("iqn.2026-01.local.broom:pc-01.g0"), Some(0));
        assert_eq!(parse("iqn.2026-01.local.broom:no-gen"), None);
    }

    /// Real zram via sysfs (root): `cargo test -- --ignored zram_live`.
    #[test]
    #[ignore]
    fn zram_live() {
        let dev = super::zram_add(16 << 20).unwrap();
        std::fs::write(&dev, vec![7u8; 1 << 20]).unwrap();
        assert_eq!(&std::fs::read(&dev).unwrap()[..4], &[7, 7, 7, 7]);
        super::zram_remove(&dev);
        assert!(!std::path::Path::new(&format!("/sys/block/{}", &dev[5..])).exists());
    }

    #[test]
    fn zram_fits_logic() {
        let reserve = 2 * GB;
        // img 4GB + 2GB reserve <= 8GB avail → fits.
        assert!(zram_fits(4 * GB, 8 * GB, reserve));
        // img 40GB + 2GB > 16GB avail → overflows.
        assert!(!zram_fits(40 * GB, 16 * GB, reserve));
        // exactly at the limit: 6GB + 2GB == 8GB → fits.
        assert!(zram_fits(6 * GB, 8 * GB, reserve));
        // 1 byte over the limit → overflows.
        assert!(!zram_fits(6 * GB + 1, 8 * GB, reserve));
        // avail=0 (meminfo unreadable) → always allowed.
        assert!(zram_fits(999 * GB, 0, reserve));
    }
}
