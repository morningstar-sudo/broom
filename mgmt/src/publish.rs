// publish.rs — after a golden is uploaded (golden.rs: upload → image.img), make the image bootable. Linux: kernel +
// initrd out of the golden (overlay.rs) + shared read-only iSCSI target (file or RAM copy, served by the iSCSI daemon)
// → iPXE boot_script loads the kernel/initrd, the initrd attaches iSCSI + the SSD overlay. Windows: winstage/ (native
// VHDX boot from the SSD). Also the target life cycle (restore at start, new generation per publish, gc) and the
// Secure Boot shim copy.
use std::path::Path;

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

/// RAM (bytes) a RAM copy must leave free for the OS and the rest of the server (config zram_reserve_mb, web System
/// page). The copy checks it itself, when it claims its RAM (iscsid::ramimg).
pub(crate) fn ram_reserve(st: &SharedState) -> u64 {
    st.db.get_config("zram_reserve_mb", "2048").parse::<u64>().unwrap_or(2048).saturating_mul(1 << 20)
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
    // Copied next to it, then renamed over it: a client downloading the shim meanwhile never gets a half-written file.
    let dir = crate::tftp_dir().join("shim");
    let tmp = dir.join("shimx64.efi.tmp");
    let r = std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::copy(src, &tmp))
        .and_then(|_| std::fs::rename(&tmp, dir.join("shimx64.efi")));
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
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

/// Shared RO iSCSI target for an image (the iSCSI daemon, iscsi.rs): the golden file, or a compressed copy of it in
/// RAM (`ram`, cache_mode zram). Idempotent (re-creates). Returns the IQN.
fn export_target(st: &SharedState, name: &str, g: u64, ram: bool, path: &str) -> Result<String, String> {
    let iqn = iqn_at(st, name, g);
    crate::iscsi::export(&iqn, path, ram, ram_reserve(st)).map_err(|e| format!("iSCSI target: {e}"))?;
    tracing::info!("iSCSI target {iqn} ready ({}: {path})", if ram { "RAM" } else { "disk" });
    Ok(iqn)
}

/// At start: LIO targets of an older version handed over (cleared, so the daemon gets port 3260), the iSCSI daemon
/// checked / started, then every published Linux image's target restored if the daemon lacks it, and the games disk
/// settings sent. Blocking.
pub fn start_iscsi(st: &SharedState) {
    let base = st.db.get_config("iqn_base", "iqn.2026-01.local.broom");
    if crate::iscsi::clear_lio(&[&base, "iqn.2026-08.net.tiem:"]) {
        clear_legacy_zram(st);
    }
    match crate::iscsi::ensure_daemon() {
        Ok(s) => tracing::info!("{s}"),
        Err(e) => return tracing::error!("iSCSI targets not served: {e}"),
    }
    restore_targets(st);
    if let Err(e) = crate::games::sync(st) {
        tracing::error!("games disk not served: {e}");
    }
}

/// zram devices an older version loaded goldens into (kernel RAM held until reset). Their LIO targets are gone.
fn clear_legacy_zram(st: &SharedState) {
    for img in st.db.images().unwrap_or_default() {
        let keys = std::iter::once(format!("zram_dev:{}", img.name)).chain((0..=gen_of(st, &img.name)).map(|g| format!("zram_dev:{}:g{g}", img.name)));
        for k in keys {
            let dev = st.db.get_config(&k, "");
            if let Some(n) = dev.strip_prefix("/dev/zram") {
                let _ = std::fs::write(format!("/sys/block/zram{n}/reset"), "1");
                let _ = std::fs::write("/sys/class/zram-control/hot_remove", n);
                let _ = st.db.set_config(&k, "");
                tracing::info!("freed {dev} (image {}: RAM copies now live in the iSCSI daemon)", img.name);
            }
        }
    }
}

/// Every published Linux image whose current target the daemon doesn't have (first start, daemon lost its state):
/// disk → export it again; zram → publish again (new generation + RAM copy, falls back to disk on RAM overflow).
/// Targets the daemon still has are left alone (clients stay connected).
fn restore_targets(st: &SharedState) {
    let have: Vec<String> = crate::iscsi::list().unwrap_or_default().into_iter().map(|t| t.iqn).collect();
    for img in st.db.images().unwrap_or_default() {
        if img.os != "linux" || img.boot_script.is_none() || have.contains(&iqn_of(st, &img.name)) {
            continue;
        }
        // Claim the job slot like any job: a publish / rollback / delete started from the web meanwhile is refused
        // instead of racing this one (same staging dir, same generation, same image.img).
        {
            let mut jobs = st.jobs.lock().unwrap();
            if jobs.get(&img.name).is_some_and(|s| s.starts_with('⏳')) {
                continue;
            }
            jobs.insert(img.name.clone(), "⏳ restoring the iSCSI target...".into());
        }
        let r = if img.cache_mode == "zram" {
            publish_iscsi(st, img.id, &img.name)
        } else {
            let path = images_dir().join(&img.name).join("image.img");
            std::fs::canonicalize(&path)
                .map_err(|e| format!("{}: {e}", path.display()))
                .and_then(|p| export_target(st, &img.name, gen_of(st, &img.name), false, &p.to_string_lossy()))
        };
        match r {
            Ok(_) => {
                tracing::info!("iSCSI target for image {} restored ({})", img.name, img.cache_mode);
                st.set_job(&img.name, format!("✓ iSCSI target restored ({})", img.cache_mode));
            }
            Err(e) => {
                tracing::error!("iSCSI target for image {} not restored: {e}", img.name);
                st.set_job(&img.name, format!("✗ iSCSI target not restored: {e}"));
            }
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
    // Boot scripts get it at boot (${broom-srv}, /boot/start); without one no client could reach the server.
    if st.db.get_config("dhcp_server_ip", "").is_empty() {
        return Err("dhcp_server_ip is empty — run `setup` first".into());
    }
    // Kernel and initrd are built in tftp/broom/<name>.new/ and swapped in only right before the new
    // boot script is saved: until then a booting client gets the old files that match the old cmdline.
    let live = crate::tftp_dir().join("broom").join(name);
    let staged = crate::tftp_dir().join("broom").join(format!("{name}.new"));
    let _ = std::fs::remove_dir_all(&staged);
    // The new generation (new IQN + backstore + zram device) is only RECORDED once everything worked: until then the
    // saved boot script, iqn_of() and gc all still mean the old one — a failed publish never lets gc drop the target
    // the boot script points at.
    let g = gen_of(st, name) + 1;
    let out = publish_iscsi_staged(st, id, name, g, &img_abs, &want, &staged).and_then(|r| {
        let old = crate::tftp_dir().join("broom").join(format!("{name}.old"));
        let _ = std::fs::remove_dir_all(&old);
        let _ = std::fs::rename(&live, &old);
        if let Err(e) = std::fs::rename(&staged, &live) {
            let _ = std::fs::rename(&old, &live); // the old kernel/initrd back where the old boot script expects them
            return Err(format!("swap in {}: {e}", live.display()));
        }
        let _ = std::fs::remove_dir_all(&old);
        Ok(r)
    });
    match out {
        Ok((bs, hash, cache_mode)) => {
            st.db.set_config(&format!("iscsi_gen:{name}"), &g.to_string())?;
            st.db.set_published(id, &bs, &hash)?;
            gc_superseded(st, name);
            Ok(format!("Publish OK — golden '{name}' iSCSI RO ({cache_mode}) + kernel/initrd + boot_script overlay"))
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staged);
            drop_generation(st, name, g); // its target / zram device, if it got that far
            Err(e)
        }
    }
}

/// Remove one generation's target (a publish that failed after making it).
fn drop_generation(st: &SharedState, name: &str, g: u64) {
    crate::iscsi::remove(&iqn_at(st, name, g));
}

/// publish_iscsi's work into `staged`: kernel/initrd, the iSCSI target. → (boot script, hash, cache mode).
fn publish_iscsi_staged(
    st: &SharedState,
    id: i64,
    name: &str,
    g: u64,
    img_abs: &Path,
    want: &str,
    staged: &Path,
) -> Result<(String, String, String), String> {
    // 1. Extract kernel + initrd from the golden, read the root UUID.
    let root_uuid = crate::overlay::build_boot(img_abs, name, staged)?;

    // 2+3. Shared RO iSCSI target, generation g (the caller records it on success): new IQN, leaving the previous
    //    target for connected clients (dropped once they leave). cache_mode (images column): disk → the daemon reads
    //    the file; zram → it serves a compressed copy in RAM. RAM refused (not enough, checked before anything is
    //    loaded) or failed → falls back to disk BY ITSELF (DB updated) so the image always boots.
    let path = img_abs.to_string_lossy();
    let ram = want == "zram";
    let (iqn, ram) = match export_target(st, name, g, ram, &path) {
        Ok(iqn) => (iqn, ram),
        Err(e) if ram => {
            tracing::warn!("image {name}: RAM cache not used ({e}) → falling back to cache_mode=disk");
            (export_target(st, name, g, false, &path)?, false)
        }
        Err(e) => return Err(e),
    };
    if want == "zram" && !ram {
        let _ = st.db.set_cache_mode(id, "disk");
    }
    let cache_mode = if ram { "zram" } else { "disk" }.to_string();

    // 4. iPXE boot_script. The initrd hook reads broom.name/hash/size/srv/reg/lxgb/ssd from the cmdline.
    // sanhook = iPXE attaches iSCSI via iBFT (does not boot the LUN); initrd open-iscsi reads the iBFT →
    // /dev/sda golden RO → root=UUID mounted RO; overlayroot (baked into the golden) overlays it onto the
    // SSD writeback (reset every boot). ip=dhcp gives the initrd a network.
    // overlayroot on the CMDLINE (takes precedence over the conf file) → root RO + upper on the SSD LABEL broomwb.
    // `quiet` left out so overlayroot/broom logs show on the client console (easier to debug a boot).
    // broom.name/hash/size: the initrd hook compares the hash with the SSD cache copy (match → boot from the SSD,
    // skip iSCSI; mismatch → iSCSI + background copy). Hash computed FIRST to embed it in the cmdline.
    let hash = crate::hash::file_hash_cached(img_abs, &images_dir().join(name).join("image.sha256"))
        .ok_or_else(|| format!("sha256 of {} failed", img_abs.display()))?;
    let size = std::fs::metadata(img_abs).map_err(|e| e.to_string())?.len();
    let bs = format!(
        "sanhook iscsi:${{broom-srv}}::::{iqn} || shell\n\
         kernel http://${{broom-srv}}/tftp/broom/{name}/vmlinuz initrd=initrd.img ip=dhcp root=UUID={root_uuid} ro fsck.mode=skip overlayroot=device:dev=/dev/disk/by-label/broomwb,recurse=0 broom.name={name} broom.hash={hash} broom.size={size} broom.srv=${{broom-srv}} broom.reg=${{broom-reg}} broom.lxgb=${{broom-lxgb}} broom.wbgb=${{broom-wbgb}} broom.ssd=${{broom-ssd}}\n\
         initrd http://${{broom-srv}}/tftp/broom/{name}/initrd.img\n\
         boot"
    );
    Ok((bs, hash, cache_mode))
}

/// Undo everything publish made for an image (image deleted): iSCSI target, zram device, boot files
/// (Linux kernel/initrd, Windows golden.vhdx + templates). Blocking.
pub fn unpublish(st: &SharedState, name: &str) {
    drop_targets(st, name, None, false); // every generation (the image is going away)
    crate::goldenram::forget(name);
    for d in ["broom", "broom-win"] {
        let _ = std::fs::remove_dir_all(crate::tftp_dir().join(d).join(name));
    }
    for s in ["new", "old"] {
        let _ = std::fs::remove_dir_all(crate::tftp_dir().join("broom").join(format!("{name}.{s}")));
    }
}

/// Publish generation for an image (config `iscsi_gen:<name>`, starts 0). Bumped on each Linux (re)publish so a new
/// target gets a NEW IQN, leaving the previous one serving already-connected clients.
fn gen_of(st: &SharedState, name: &str) -> u64 {
    st.db.get_config(&format!("iscsi_gen:{name}"), "0").parse().unwrap_or(0)
}

/// The current (published) IQN for an image — the one its saved boot script points at.
pub(crate) fn iqn_of(st: &SharedState, name: &str) -> String {
    iqn_at(st, name, gen_of(st, name))
}

/// IQN of generation `g`: `<iqn_base>:<name>.g<g>`. Image names are `[A-Za-z0-9_-]` (no dot), so `.g` is an
/// unambiguous separator.
fn iqn_at(st: &SharedState, name: &str, g: u64) -> String {
    format!("{}:{name}.g{g}", st.db.get_config("iqn_base", "iqn.2026-01.local.broom"))
}

/// This image's targets (every generation), with their session counts.
fn targets_of(st: &SharedState, name: &str) -> Vec<crate::iscsi::TargetInfo> {
    let prefix = format!("{}:{name}.g", st.db.get_config("iqn_base", "iqn.2026-01.local.broom"));
    crate::iscsi::list().unwrap_or_default().into_iter().filter(|t| t.iqn.starts_with(&prefix)).collect()
}

/// Remove this image's iSCSI targets (every generation except `keep`; a RAM copy goes with its target). `idle_only`:
/// skip a target a client is logged in to (it keeps reading its own generation; a later pass drops it).
fn drop_targets(st: &SharedState, name: &str, keep: Option<&str>, idle_only: bool) {
    for t in targets_of(st, name) {
        if keep == Some(t.iqn.as_str()) || (idle_only && t.sessions > 0) {
            continue;
        }
        crate::iscsi::remove(&t.iqn);
        tracing::info!("iSCSI: removed target {}", t.iqn);
    }
}

/// Drop every superseded generation of this image's target that no client uses any more. After each publish and
/// periodically (main.rs): a busy lab never has a moment with no client at all.
pub(crate) fn gc_superseded(st: &SharedState, name: &str) {
    drop_targets(st, name, Some(&iqn_of(st, name)), true);
}

/// Every target of this image, in use or not (rollback rewrites the file a disk-cache target serves).
pub(crate) fn drop_all_targets(st: &SharedState, name: &str) {
    drop_targets(st, name, None, false);
}

/// A client is logged in to some generation of this image's target. No daemon → nobody can be.
pub(crate) fn image_in_use(st: &SharedState, name: &str) -> bool {
    targets_of(st, name).iter().any(|t| t.sessions > 0)
}
