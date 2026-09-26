// publish.rs — after a golden is uploaded, process it automatically so the image can boot.
// Linux: golden raw (from vmdk) → shared RO iSCSI (disk or zram) + kernel/initrd (overlay.rs)
// → iPXE boot_script loads kernel+initrd + attaches iSCSI + SSD overlay.
// Windows: winstage.rs (native VHDX boot from the client SSD).
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

/// Move a file (rename, fall back to copy across filesystems).
fn mv(src: &Path, dst: &Path) -> Result<(), String> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
    let _ = std::fs::remove_file(src);
    Ok(())
}

/// sha256 of a file (clients compare it to detect a golden change). None on error.
pub(crate) fn file_hash(path: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Sha256::new(); // SHA-NI when the CPU has it (same speed as sha256sum)
    let mut buf = vec![0u8; 4 << 20];
    loop {
        match f.read(&mut buf).ok()? {
            0 => break,
            n => h.update(&buf[..n]),
        }
    }
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Extract a .zip into `dir` (entries with unsafe paths are skipped).
pub(crate) fn unzip(zip_path: &Path, dir: &Path) -> Result<(), String> {
    let f = std::fs::File::open(zip_path).map_err(|e| format!("{}: {e}", zip_path.display()))?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| format!("zip: {e}"))?;
    for i in 0..z.len() {
        let mut entry = z.by_index(i).map_err(|e| format!("zip entry {i}: {e}"))?;
        let Some(rel) = entry.enclosed_name() else { continue };
        let out = dir.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        let mut w = std::fs::File::create(&out).map_err(|e| format!("{}: {e}", out.display()))?;
        std::io::copy(&mut entry, &mut w).map_err(|e| format!("unzip {}: {e}", out.display()))?;
    }
    Ok(())
}

/// Turn the upload folder into raw `dest` (image.img), then delete the folder. Blocking.
/// The folder holds one .img/.raw/.vmdk, a VM folder (.vmx + .vmdk files), or a .zip of either.
/// Written to a temp file, then renamed over `dest`: a live iSCSI target (disk mode) keeps the old file open,
/// so running clients still read the old golden until publish swaps the target (never a half-written image).
pub fn prepare_golden(dir: &Path, dest: &Path) -> Result<(), String> {
    let tmp = dest.with_extension("img.new");
    let _ = std::fs::remove_file(&tmp);
    let out = golden_from(dir, &tmp).and_then(|()| std::fs::rename(&tmp, dest).map_err(|e| format!("rename {}: {e}", tmp.display())));
    if out.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    let _ = std::fs::remove_dir_all(dir);
    out
}

fn ext(p: &Path) -> String {
    p.extension().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

fn golden_from(dir: &Path, dest: &Path) -> Result<(), String> {
    // A zipped VM folder → extract first (drop the zip right away: disk space).
    if let Some(z) = walk(dir).into_iter().find(|p| ext(p) == "zip") {
        unzip(&z, &dir.join("unzip"))?;
        let _ = std::fs::remove_file(&z);
    }
    let files = walk(dir);
    let mut vmdks: Vec<_> = files.iter().filter(|p| ext(p) == "vmdk").cloned().collect();
    let raw = files.iter().find(|p| ext(p) == "img" || ext(p) == "raw");
    // Pick the vmdk to convert: a .vmx names the disk the VM ACTUALLY uses (even with a branching
    // snapshot tree) → preferred. No .vmx: one file → use it; several → pick_vmdk.
    // qemu-img reads extents/parents from the same directory.
    let from_vmx = files.iter().filter(|p| ext(p) == "vmx").find_map(|vmx| {
        let disk = vmx_disk(&std::fs::read_to_string(vmx).ok()?)?;
        let p = vmx.with_file_name(disk);
        p.exists().then_some(p)
    });
    let chosen_vmdk = if from_vmx.is_some() {
        from_vmx
    } else if vmdks.len() == 1 {
        Some(vmdks.remove(0))
    } else {
        pick_vmdk(&vmdks)
    };
    if let Some(v) = chosen_vmdk {
        tracing::info!("golden: converting {}", v.display());
        // -m 16: 16 parallel I/O coroutines (default 8); -W: out-of-order writes (sparse raw target).
        run("qemu-img", &["convert", "-m", "16", "-W", "-O", "raw", &v.to_string_lossy(), &dest.to_string_lossy()])
    } else if let Some(r) = raw {
        mv(r, dest)
    } else if !vmdks.is_empty() {
        Err("several .vmdk files but no descriptor file — upload the whole VM folder (with the .vmx) or a single monolithic vmdk".into())
    } else {
        Err("upload contains no .vmdk/.img/.raw".into())
    }
}

/// Head of a vmdk file (text descriptor, or the descriptor embedded at sector 1 of a monolithicSparse).
fn vmdk_head(p: &Path) -> String {
    use std::io::Read;
    let mut buf = [0u8; 8192];
    let n = std::fs::File::open(p).and_then(|mut f| f.read(&mut buf)).unwrap_or(0);
    String::from_utf8_lossy(&buf[..n]).to_string()
}

/// vmdk descriptor = text file (contains "# Disk DescriptorFile" / "createType") pointing to extents.
/// Otherwise = binary extent (monolithicSparse starts with magic "KDMV") — can't be converted on its own.
fn is_vmdk_descriptor(p: &Path) -> bool {
    let head = vmdk_head(p);
    head.contains("# Disk DescriptorFile") || head.contains("createType")
}

/// First disk the VM uses according to the .vmx: line `<bus>N:M.fileName = "x.vmdk"` (skip CD/ISO).
fn vmx_disk(vmx: &str) -> Option<String> {
    vmx.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        let v = v.trim().trim_matches('"');
        (k.trim().ends_with(".fileName") && v.to_ascii_lowercase().ends_with(".vmdk"))
            .then(|| v.rsplit(['\\', '/']).next().unwrap_or(v).to_string())
    })
}

/// The CURRENT descriptor among the vmdk files (split + snapshots): the descriptor that is not the parent
/// (`parentFileNameHint`) of any other descriptor = top of the snapshot chain. Not by name:
/// "win.vmdk" > "win-000001.vmdk" as strings, yet 000001 is the newest state.
fn pick_vmdk(vmdks: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    let desc: Vec<_> = vmdks.iter().filter(|p| is_vmdk_descriptor(p)).collect();
    let parents: Vec<String> = desc
        .iter()
        .filter_map(|p| {
            let h = vmdk_head(p);
            let v = h.split("parentFileNameHint=\"").nth(1)?.split('"').next()?.to_string();
            Some(v.rsplit(['\\', '/']).next().unwrap_or(&v).to_ascii_lowercase())
        })
        .collect();
    let mut top: Vec<_> = desc
        .into_iter()
        .filter(|p| {
            let n = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            !parents.contains(&n)
        })
        .cloned()
        .collect();
    top.sort();
    top.pop()
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

/// Walk the files in dir (recursive). ponytail: enough for a golden zip with a few files.
pub(crate) fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn run(bin: &str, args: &[&str]) -> Result<(), String> {
    tracing::debug!("exec: {bin} {}", args.join(" "));
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("{bin}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{bin} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Publish an image according to its os. Blocking (called from spawn_blocking).
pub fn run_publish(st: &SharedState, name: &str, steps: &mut Steps) -> Result<String, String> {
    let img = st.db.image_by_name(name)?.ok_or(format!("image '{name}' not found in DB"))?;
    let (id, os) = (img.id, img.os);
    match os.as_str() {
        "linux" => {
            steps.go("publish linux (kernel/initrd + iSCSI)");
            publish_iscsi(st, id, name)
        }
        "windows" => crate::winstage::publish(st, id, name, steps),
        other => Err(format!("invalid os: {other} (linux|windows)")),
    }
}

/// Shared RO iSCSI target for an image (kernel LIO via configfs, iscsi.rs). `backing` = golden
/// file (disk) or /dev/zramN (zram). Idempotent (re-creates). Returns the IQN.
fn export_target(st: &SharedState, name: &str, cache_mode: &str, backing: &str) -> Result<String, String> {
    let iqn = iqn_of(st, name);
    let lio = crate::iscsi::Lio::system()?;
    // ponytail: target named by the old fixed IQN (before iqn_base) — remove it too; drop this line later.
    lio.remove(name, &format!("iqn.2026-08.net.tiem:{name}"));
    let b = if cache_mode == "zram" {
        crate::iscsi::Backing::Block { dev: backing }
    } else {
        let size = std::fs::metadata(backing).map_err(|e| format!("{backing}: {e}"))?.len();
        crate::iscsi::Backing::File { path: backing, size }
    };
    lio.export(name, b, &iqn).map_err(|e| format!("iSCSI target: {e}"))?;
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

    // 1. Extract kernel + initrd from the golden → <home>/tftp/broom/<name>/, read the root UUID.
    let root_uuid = crate::overlay::build_boot(&img_abs, name)?;

    // 2. cache_mode (images column): disk → serve the file directly; zram → load the img into /dev/zramN.
    // zram fails (RAM overflow / error) → fall back to disk BY ITSELF (DB updated) so the image always boots.
    let want = st.db.image(id)?.map_or_else(|| "disk".into(), |i| i.cache_mode);
    let (cache_mode, backing) = if want == "zram" {
        match ensure_zram(st, name, &img_abs) {
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

    // 3. Shared RO iSCSI target (zram = block backstore, disk = fileio).
    let iqn = export_target(st, name, &cache_mode, &backing)?;

    // 4. iPXE boot_script. The initrd reads broom.iscsi / broom.ssd from the cmdline.
    let ip = st.db.get_config("dhcp_server_ip", "");
    if ip.is_empty() {
        return Err("dhcp_server_ip is empty — run `setup` first".into());
    }
    // sanhook = iPXE attaches iSCSI via iBFT (does not boot the LUN); initrd open-iscsi reads the iBFT →
    // /dev/sda golden RO → root=UUID mounted RO; overlayroot (baked into the golden) overlays it onto the
    // SSD writeback (reset every boot). ip=dhcp gives the initrd a network.
    // overlayroot on the CMDLINE (takes precedence over the conf file) → root RO + upper on the SSD LABEL broomwb.
    // `quiet` left out so overlayroot/broom logs are visible during the PoC.
    // broom.name/hash/size: the initrd hook compares the hash with the SSD cache copy (match → boot from the SSD,
    // skip iSCSI; mismatch → iSCSI + background copy). Hash computed FIRST to embed it in the cmdline.
    let img_str = img_abs.to_string_lossy().to_string();
    let hash = file_hash(&img_str).ok_or("sha256sum golden failed")?;
    let size = std::fs::metadata(&img_abs).map_err(|e| e.to_string())?.len();
    let bs = format!(
        "sanhook iscsi:{ip}::::{iqn} || shell\n\
         kernel http://{ip}/tftp/broom/{name}/vmlinuz initrd=initrd.img ip=dhcp root=UUID={root_uuid} ro fsck.mode=skip overlayroot=device:dev=/dev/disk/by-label/broomwb,recurse=0 broom.name={name} broom.hash={hash} broom.size={size}\n\
         initrd http://{ip}/tftp/broom/{name}/initrd.img\n\
         boot"
    );
    st.db.set_published(id, &bs, &hash)?;
    Ok(format!(
        "Publish OK — golden '{name}' iSCSI RO ({cache_mode}) + kernel/initrd + boot_script overlay"
    ))
}

/// Undo everything publish made for an image (image deleted): iSCSI target, zram device, boot files
/// (Linux kernel/initrd, Windows golden.vhdx + templates). Blocking.
pub fn unpublish(st: &SharedState, name: &str) {
    if let Ok(lio) = crate::iscsi::Lio::system() {
        lio.remove(name, &iqn_of(st, name));
    }
    let key = format!("zram_dev:{name}");
    let dev = st.db.get_config(&key, "");
    if !dev.is_empty() {
        zram_remove(&dev);
        let _ = st.db.set_config(&key, "");
    }
    for d in ["broom", "broom-win"] {
        let _ = std::fs::remove_dir_all(crate::tftp_dir().join(d).join(name));
    }
}

/// Load the golden img into a zram device (zstd compressed), return /dev/zramN. Map stored in DB config.
/// Reset the image's old device (if any) before creating a new one.
fn ensure_zram(st: &SharedState, name: &str, img: &Path) -> Result<String, String> {
    // Free the old device if the image was on zram before — its iSCSI backstore holds it open,
    // so drop the target first (export_target re-creates it right after).
    let old = st.db.get_config(&format!("zram_dev:{name}"), "");
    if !old.is_empty() {
        if let Ok(lio) = crate::iscsi::Lio::system() {
            lio.remove(name, &iqn_of(st, name));
        }
        zram_remove(&old);
        let _ = st.db.set_config(&format!("zram_dev:{name}"), "");
    }
    let size = std::fs::metadata(img).map_err(|e| e.to_string())?.len();

    // VALIDATE RAM overflow: zram compresses but worst case (incompressible data) = full img size.
    // Require MemAvailable > img size + reserve (kept for the OS + iSCSI serving). The old device
    // was reset above so its RAM is returned; MemAvailable also reflects OTHER zram images being held.
    // reserve is set via the zram_reserve_mb config (web System page).
    let reserve = st.db.get_config("zram_reserve_mb", "2048").parse::<u64>().unwrap_or(2048) * 1024 * 1024;
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
    let _ = st.db.set_config(&format!("zram_dev:{name}"), &dev);
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

pub(crate) fn iqn_of(st: &SharedState, name: &str) -> String {
    format!("{}:{name}", st.db.get_config("iqn_base", "iqn.2026-01.local.broom"))
}

#[cfg(test)]
mod tests {
    use super::{is_vmdk_descriptor, pick_vmdk, vmx_disk, zram_fits};

    #[test]
    fn sha256_known_vector() {
        let p = std::env::temp_dir().join("broom_test_sha.txt");
        std::fs::write(&p, "abc").unwrap();
        assert_eq!(
            super::file_hash(p.to_str().unwrap()).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn unzip_nested_and_skips_escape() {
        use std::io::Write;
        let d = std::env::temp_dir().join("broom_test_unzip");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let zp = d.join("vm.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
        let opt = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("VM/disk.vmdk", opt).unwrap();
        w.write_all(b"# Disk DescriptorFile").unwrap();
        w.start_file("../escape.txt", opt).unwrap();
        w.write_all(b"x").unwrap();
        w.finish().unwrap();
        let out = d.join("out");
        super::unzip(&zp, &out).unwrap();
        assert_eq!(std::fs::read(out.join("VM/disk.vmdk")).unwrap(), b"# Disk DescriptorFile");
        assert!(!d.join("escape.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Upload folder → image.img: a plain .IMG (any case) is moved; a .zip holding one is extracted
    /// first; the folder is removed either way.
    #[test]
    fn prepare_golden_raw_and_zip() {
        use std::io::Write;
        let d = std::env::temp_dir().join("broom_test_prep");
        let _ = std::fs::remove_dir_all(&d);
        let (up, dest) = (d.join("upload"), d.join("image.img"));
        std::fs::create_dir_all(&up).unwrap();
        std::fs::write(up.join("DISK.IMG"), b"RAW").unwrap();
        super::prepare_golden(&up, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"RAW");
        assert!(!up.exists());

        std::fs::create_dir_all(&up).unwrap();
        let mut w = zip::ZipWriter::new(std::fs::File::create(up.join("vm.zip")).unwrap());
        w.start_file("VM/disk.raw", zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(b"ZIPPED").unwrap();
        w.finish().unwrap();
        super::prepare_golden(&up, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"ZIPPED");
        assert!(!up.exists());

        std::fs::create_dir_all(&up).unwrap();
        std::fs::write(up.join("notes.txt"), b"x").unwrap();
        assert!(super::prepare_golden(&up, &dest).unwrap_err().contains("no .vmdk"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"ZIPPED"); // failed upload leaves the current golden alone
        assert!(!d.join("image.img.new").exists());
        let _ = std::fs::remove_dir_all(&d);
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

    /// .vmx: take the disk in use (current snapshot), skip CD/ISO.
    #[test]
    fn vmx_current_disk() {
        let vmx = "displayName = \"win\"\n\
                   sata0:1.fileName = \"D:\\\\iso\\\\win11.iso\"\n\
                   nvme0:0.fileName = \"win-000003.vmdk\"\n\
                   nvme0:0.present = \"TRUE\"\n";
        assert_eq!(vmx_disk(vmx).unwrap(), "win-000003.vmdk");
        assert_eq!(vmx_disk("sata0:1.fileName = \"auto detect\"\n"), None);
    }

    /// VM with snapshots: pick the top of the chain (000002), not the base file whose name sorts "higher".
    #[test]
    fn pick_vmdk_snapshot_top() {
        let d = std::env::temp_dir().join("broom_test_snap");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let w = |n: &str, parent: &str| {
            let hint = if parent.is_empty() { String::new() } else { format!("parentFileNameHint=\"C:\\VMs\\win\\{parent}\"\n") };
            std::fs::write(d.join(n), format!("# Disk DescriptorFile\ncreateType=\"twoGbMaxExtentSparse\"\n{hint}")).unwrap();
            d.join(n)
        };
        let v = vec![
            w("win.vmdk", ""),
            w("win-000001.vmdk", "win.vmdk"),
            w("win-000002.vmdk", "win-000001.vmdk"),
        ];
        let mut all = v.clone();
        std::fs::write(d.join("win-s001.vmdk"), b"KDMV\x01binary").unwrap();
        all.push(d.join("win-s001.vmdk"));
        assert_eq!(pick_vmdk(&all).unwrap(), d.join("win-000002.vmdk"));
        assert_eq!(pick_vmdk(&v[..2]).unwrap(), d.join("win-000001.vmdk"));
        assert_eq!(pick_vmdk(&v[..1]).unwrap(), d.join("win.vmdk"));
        let _ = std::fs::remove_dir_all(&d);
    }
    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn vmdk_descriptor_detect() {
        let dir = std::env::temp_dir();
        // descriptor = text.
        let d = dir.join("broom_test_desc.vmdk");
        std::fs::write(&d, "# Disk DescriptorFile\nversion=1\ncreateType=\"twoGbMaxExtentSparse\"\n").unwrap();
        assert!(is_vmdk_descriptor(&d));
        // extent = binary (magic KDMV) → not a descriptor.
        let e = dir.join("broom_test_ext.vmdk");
        std::fs::write(&e, b"KDMV\x01\x00\x00\x00binarygarbage").unwrap();
        assert!(!is_vmdk_descriptor(&e));
        let _ = std::fs::remove_file(&d);
        let _ = std::fs::remove_file(&e);
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
