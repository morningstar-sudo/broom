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

/// Chunk size of the golden manifests (golden.chunks) — clients fetch only the chunks they lack.
pub(crate) const MANIFEST_CHUNK: usize = 4 << 20;

/// sha256 of a file + the sha256 of each 4 MB chunk ("zero" = all zero), in ONE read pass. None on error.
fn file_hash_chunks(path: &Path) -> Option<(String, Vec<String>)> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let hex = |d: &[u8]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut f = std::fs::File::open(path).ok()?;
    let (mut whole, mut chunks) = (Sha256::new(), Vec::new());
    let mut buf = vec![0u8; MANIFEST_CHUNK];
    loop {
        let mut n = 0; // fill a whole chunk (read may return less)
        while n < buf.len() {
            match f.read(&mut buf[n..]).ok()? {
                0 => break,
                k => n += k,
            }
        }
        if n == 0 {
            break;
        }
        let c = &buf[..n];
        whole.update(c);
        chunks.push(if c.iter().all(|&b| b == 0) { "zero".to_string() } else { hex(&Sha256::digest(c)) });
        if n < buf.len() {
            break;
        }
    }
    Some((hex(&whole.finalize()), chunks))
}

/// Write `<dir>/golden.chunks` for the served golden `file`: first line `size <bytes>`, then one line per 4 MB
/// chunk (sha256 or `zero`). Clients diff it against the manifest of the copy they have → delta update.
/// Returns the whole-file sha256 (the golden hash).
pub(crate) fn write_manifest(file: &Path, dir: &Path) -> Result<String, String> {
    let (hash, chunks) = file_hash_chunks(file).ok_or_else(|| format!("sha256 of {} failed", file.display()))?;
    let size = std::fs::metadata(file).map_err(|e| e.to_string())?.len();
    let tmp = dir.join("golden.chunks.tmp");
    std::fs::write(&tmp, format!("size {size}\n{}\n", chunks.join("\n"))).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join("golden.chunks")).map_err(|e| e.to_string())?;
    Ok(hash)
}

/// Cache of zstd-compressed golden chunks, by content hash: a room of clients asks for the same changed chunks →
/// each is compressed once. Emptied when a Windows golden is rebuilt (content-addressed, only space is at stake).
pub(crate) fn chunk_cache_dir() -> std::path::PathBuf {
    crate::work_dir().join("zchunks")
}

/// Chunk `i` (4 MB) of the Windows golden.vhdx of image `name`, zstd-compressed, checked against `sha` (its line in
/// golden.chunks). Err(true) = the golden no longer has that content (republished meanwhile); Err(false) = I/O.
pub(crate) fn golden_chunk_zst(name: &str, i: u64, sha: &str) -> Result<Vec<u8>, (bool, String)> {
    let golden = crate::tftp_dir().join("broom-win").join(name).join("golden.vhdx");
    chunk_zst(&golden, &chunk_cache_dir(), i, sha)
}

fn chunk_zst(golden: &Path, cache: &Path, i: u64, sha: &str) -> Result<Vec<u8>, (bool, String)> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::FileExt;
    let cached = cache.join(format!("{sha}.zst"));
    if let Ok(z) = std::fs::read(&cached) {
        return Ok(z);
    }
    let f = std::fs::File::open(golden).map_err(|e| {
        tracing::error!("open golden {}: {e}", golden.display()); // L7: keep the path in the log, not the response
        (false, "golden not available".to_string())
    })?;
    let off = i.checked_mul(MANIFEST_CHUNK as u64).ok_or((true, "chunk index out of range".to_string()))?;
    // Reject an out-of-range chunk from the file length (cheap) before reading + hashing 4 MB (M4).
    if off >= f.metadata().map_err(|e| (false, e.to_string()))?.len() {
        return Err((true, "chunk index out of range".to_string()));
    }
    let mut buf = vec![0u8; MANIFEST_CHUNK];
    let mut n = 0;
    while n < buf.len() {
        match f.read_at(&mut buf[n..], off + n as u64).map_err(|e| (false, e.to_string()))? {
            0 => break,
            k => n += k,
        }
    }
    buf.truncate(n);
    let got: String = Sha256::digest(&buf).iter().map(|b| format!("{b:02x}")).collect();
    if n == 0 || got != sha {
        return Err((true, format!("chunk {i} not available (golden republished?)")));
    }
    // Level 3: ~55 % of Windows data, fast enough to keep a 10 Gbps link busy with a few cores.
    let z = zstd::bulk::compress(&buf, 3).map_err(|e| (false, e.to_string()))?;
    let _ = std::fs::create_dir_all(cache);
    let tmp = cached.with_extension(format!("tmp{}", std::process::id() ^ i as u32));
    if std::fs::write(&tmp, &z).is_ok() {
        let _ = std::fs::rename(&tmp, &cached);
    }
    Ok(z)
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

/// A golden can't be larger than this (raw/virtual). Guards against a VMDK declaring a petabyte virtual size, which
/// would make hashing/manifesting allocate one String per 4 MB → out of memory.
const MAX_GOLDEN: u64 = 4 << 40; // 4 TiB

/// Extract a .zip into `dir`. Refuses zip-bombs: total uncompressed bytes must fit in `MAX_GOLDEN` and the free space
/// (minus a margin), and at most 200k entries. Entries with unsafe paths are skipped.
pub(crate) fn unzip(zip_path: &Path, dir: &Path) -> Result<(), String> {
    let f = std::fs::File::open(zip_path).map_err(|e| format!("{}: {e}", zip_path.display()))?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| format!("zip: {e}"))?;
    if z.len() > 200_000 {
        return Err(format!("zip has {} entries (max 200000)", z.len()));
    }
    let free = free_bytes(dir);
    let budget = if free == 0 { MAX_GOLDEN } else { MAX_GOLDEN.min(free.saturating_sub(1 << 30)) };
    let mut written = 0u64;
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
        // Cap the read so a lying uncompressed-size can't fill the disk; +1 detects overflow of the budget.
        let remain = budget.saturating_sub(written);
        let n = std::io::copy(&mut std::io::Read::take(&mut entry, remain + 1), &mut w)
            .map_err(|e| format!("unzip {}: {e}", out.display()))?;
        written += n;
        if written > budget {
            let _ = std::fs::remove_file(&out);
            return Err("zip is too large (bomb?) or not enough free space".into());
        }
    }
    Ok(())
}

/// `qemu-img info` → the virtual size in bytes. None if it can't be read (caller treats that as too risky).
fn qemu_virtual_size(file: &Path) -> Option<u64> {
    let out = Command::new("qemu-img").args(["info", "--output=json"]).arg(file).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // "virtual-size": 123456,  — parse without a JSON dep.
    let v = text.split("\"virtual-size\"").nth(1)?.split([':', ',']).nth(1)?.trim().parse::<u64>().ok()?;
    Some(v)
}

/// Every string a VMDK descriptor points at (extent file names + parentFileNameHint) must be a plain name inside the
/// upload folder — a descriptor can otherwise name `/dev/sda` or a server file as an "extent", and qemu-img (root)
/// would copy it into the golden. Non-descriptor (monolithic) vmdks have no such lines and pass.
fn vmdk_refs_safe(vmdk: &Path) -> Result<(), String> {
    let head = vmdk_head(vmdk);
    if !is_vmdk_descriptor(vmdk) {
        return Ok(());
    }
    // Quoted names on extent lines (RW/RDONLY/NOACCESS … "name" …) and parentFileNameHint="name".
    let mut refs: Vec<&str> = Vec::new();
    for line in head.lines() {
        let t = line.trim_start();
        if t.starts_with("RW") || t.starts_with("RDONLY") || t.starts_with("NOACCESS") {
            if let Some(q) = t.split('"').nth(1) {
                refs.push(q);
            }
        }
    }
    if let Some(h) = head.split("parentFileNameHint=\"").nth(1).and_then(|s| s.split('"').next()) {
        refs.push(h);
    }
    for r in refs {
        let bad = r.is_empty() || r.contains('/') || r.contains('\\') || r.contains("..") || r.starts_with(' ');
        if bad {
            return Err(format!("VMDK references an unsafe path {r:?} — export the VM as a single monolithic vmdk"));
        }
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
    // A .vmx lists every disk the VM attaches. broom serves ONE OS disk, so more than one disk is ambiguous (which
    // is the OS?) — fail loudly instead of silently converting whichever comes first (e.g. a stale SCSI disk while
    // the real OS is on nvme0:0).
    let vmx = files.iter().find(|p| ext(p) == "vmx");
    let chosen_vmdk = if let Some(vmx) = vmx {
        let txt = std::fs::read_to_string(vmx).map_err(|e| format!("read {}: {e}", vmx.display()))?;
        let disks: Vec<_> = vmx_disks(&txt).into_iter().map(|d| vmx.with_file_name(d)).filter(|p| p.exists()).collect();
        match disks.len() {
            0 if vmdks.len() == 1 => Some(vmdks.remove(0)),
            0 => pick_vmdk(&vmdks), // .vmx named no on-disk vmdk (odd) → best-effort
            1 => Some(disks[0].clone()),
            _ => {
                let names = disks.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect::<Vec<_>>().join(", ");
                return Err(format!(
                    "the VM has {} disks ({names}). broom serves ONE OS disk — in the VM settings remove the extra disk(s), \
                     keep only the disk Ubuntu/Windows boots from, then upload again.",
                    disks.len()
                ));
            }
        }
    } else if vmdks.len() == 1 {
        Some(vmdks.remove(0))
    } else {
        pick_vmdk(&vmdks)
    };
    if let Some(v) = chosen_vmdk {
        // Untrusted upload processed as root: every vmdk's extents must stay inside the folder (no /dev/sda,
        // no server files), the virtual size must be sane, and -f vmdk stops a disguised qcow2 backing file.
        for vmdk in walk(dir).iter().filter(|p| ext(p) == "vmdk") {
            vmdk_refs_safe(vmdk)?;
        }
        match qemu_virtual_size(&v) {
            Some(sz) if sz <= MAX_GOLDEN => {}
            Some(sz) => return Err(format!("golden virtual size {sz} bytes is above the {MAX_GOLDEN} limit")),
            None => return Err("could not read the vmdk (qemu-img info failed) — is it a valid disk?".into()),
        }
        tracing::info!("golden: converting {}", v.display());
        // -f vmdk: don't probe the format (a descriptor could disguise a qcow2 backing file). -m 16: 16 parallel I/O
        // coroutines (default 8); -W: out-of-order writes (sparse raw target).
        run("qemu-img", &["convert", "-f", "vmdk", "-m", "16", "-W", "-O", "raw", &v.to_string_lossy(), &dest.to_string_lossy()])
    } else if let Some(r) = raw {
        if std::fs::metadata(r).map(|m| m.len()).unwrap_or(0) > MAX_GOLDEN {
            return Err(format!("raw image is above the {MAX_GOLDEN} byte limit"));
        }
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

/// Every disk the .vmx attaches: lines `<bus>N:M.fileName = "x.vmdk"` (skip CD/ISO), basename only.
fn vmx_disks(vmx: &str) -> Vec<String> {
    vmx.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            let v = v.trim().trim_matches('"');
            (k.trim().ends_with(".fileName") && v.to_ascii_lowercase().ends_with(".vmdk"))
                .then(|| v.rsplit(['\\', '/']).next().unwrap_or(v).to_string())
        })
        .collect()
}

/// The first disk the .vmx names (tests; golden_from uses vmx_disks to refuse multi-disk VMs).
#[cfg(test)]
fn vmx_disk(vmx: &str) -> Option<String> {
    vmx_disks(vmx).into_iter().next()
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
/// Secure Boot clients (official signed iPXE, Network page) boot the Canonical-signed Ubuntu kernels — the Windows
/// stage and Linux goldens alike — through Ubuntu's Microsoft-signed shim (package shim-signed): any Ubuntu shim
/// verifies any Canonical-signed kernel. Copied to tftp/shim/shimx64.efi at every publish (cheap); boot.rs adds the
/// `shim` line. Missing → only Secure Boot clients are affected (warning).
pub(crate) fn refresh_shim() {
    let src = ["/usr/lib/shim/shimx64.efi.signed.latest", "/usr/lib/shim/shimx64.efi.signed"].into_iter().find(|p| Path::new(p).is_file());
    let Some(src) = src else {
        return tracing::warn!("no Ubuntu shim (apt install shim-signed): Secure Boot clients cannot boot the kernels");
    };
    let dir = crate::tftp_dir().join("shim");
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::copy(src, dir.join("shimx64.efi"))) {
        tracing::warn!("copy {src} → {}: {e}", dir.display());
    }
}

pub fn run_publish(st: &SharedState, name: &str, steps: &mut Steps) -> Result<String, String> {
    let img = st.db.image_by_name(name)?.ok_or(format!("image '{name}' not found in DB"))?;
    refresh_shim();
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
    let store = store_of(name, gen_of(st, name));
    let lio = crate::iscsi::Lio::system()?;
    // ponytail: target named by the old fixed IQN (before iqn_base) — remove it too; drop this line later.
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

    // 1. Extract kernel + initrd from the golden → <home>/tftp/broom/<name>/, read the root UUID.
    let root_uuid = crate::overlay::build_boot(&img_abs, name)?;

    // 2. cache_mode (images column): disk → serve the file directly; zram → load the img into /dev/zramN.
    // zram fails (RAM overflow / error) → fall back to disk BY ITSELF (DB updated) so the image always boots.
    let want = st.db.image(id)?.map_or_else(|| "disk".into(), |i| i.cache_mode);
    // Disk cache serves ONE shared golden file → it can't be swapped under a live client. Refuse to (re)publish it
    // while any client is connected (they would read changed bytes mid-session → FS corruption). zram is fine: each
    // publish makes a fresh device + target, and the old one is kept for already-connected clients (M9).
    if want != "zram" && crate::iscsi::any_session() {
        return Err("clients are connected; a disk-cache image shares one golden file and can't be swapped live. \
                    Reboot/close the clients (publish off-hours), or set this image to zram cache."
            .into());
    }
    // New generation: new IQN + backstore (+ new zram device), leaving the previous target for connected clients.
    let _ = bump_gen(st, name);
    let g = gen_of(st, name);
    let (cache_mode, backing) = if want == "zram" {
        match ensure_zram(st, name, g, &img_abs) {
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

    // 3. Shared RO iSCSI target (zram = block backstore, disk = fileio), then drop any now-superseded target.
    let iqn = export_target(st, name, &cache_mode, &backing)?;
    gc_superseded(st, name, &iqn);

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
    // Same pass writes golden.chunks (broom.srv: where the cache script fetches it → patches only changed chunks).
    let hash = write_manifest(&img_abs, &crate::tftp_dir().join("broom").join(name))?;
    let size = std::fs::metadata(&img_abs).map_err(|e| e.to_string())?.len();
    let bs = format!(
        "sanhook iscsi:{ip}::::{iqn} || shell\n\
         kernel http://{ip}/tftp/broom/{name}/vmlinuz initrd=initrd.img ip=dhcp root=UUID={root_uuid} ro fsck.mode=skip overlayroot=device:dev=/dev/disk/by-label/broomwb,recurse=0 broom.name={name} broom.hash={hash} broom.size={size} broom.srv={ip}\n\
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
        // Every generation of this image's target + its zram device (the image is going away).
        let prefix = format!("{}:{name}.g", st.db.get_config("iqn_base", "iqn.2026-01.local.broom"));
        for iqn in lio.list_iqns() {
            if !iqn.starts_with(&prefix) {
                continue;
            }
            if let Some(g) = iqn.rsplit_once(".g").and_then(|(_, s)| s.parse::<u64>().ok()) {
                lio.remove(&store_of(name, g), &iqn);
                let dev = st.db.get_config(&zram_key(name, g), "");
                if !dev.is_empty() {
                    zram_remove(&dev);
                    let _ = st.db.set_config(&zram_key(name, g), "");
                }
            }
        }
        lio.remove(name, &format!("iqn.2026-08.net.tiem:{name}")); // ponytail: legacy fixed IQN, drop later
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
/// target gets a NEW IQN + backstore, leaving the previous one serving already-connected clients (M9).
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

/// Remove every superseded target of this image (all generations except `keep_iqn`) and free their zram devices —
/// but only when no client is connected, so a running client is never cut off (M9). Runs after a new publish.
fn gc_superseded(st: &SharedState, name: &str, keep_iqn: &str) {
    if crate::iscsi::any_session() {
        return; // someone is attached (portal-wide) → keep the old targets, GC on a later publish when idle
    }
    let Ok(lio) = crate::iscsi::Lio::system() else { return };
    let prefix = format!("{}:{name}.g", st.db.get_config("iqn_base", "iqn.2026-01.local.broom"));
    for iqn in lio.list_iqns() {
        if !iqn.starts_with(&prefix) || iqn == keep_iqn {
            continue;
        }
        if let Some(g) = iqn.rsplit_once(".g").and_then(|(_, s)| s.parse::<u64>().ok()) {
            lio.remove(&store_of(name, g), &iqn);
            let dev = st.db.get_config(&zram_key(name, g), "");
            if !dev.is_empty() {
                zram_remove(&dev);
                let _ = st.db.set_config(&zram_key(name, g), "");
            }
            tracing::info!("iSCSI: removed superseded target {iqn}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_vmdk_descriptor, pick_vmdk, store_of, vmx_disk, zram_fits};

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

    /// Compressed chunk: decompresses to exactly the chunk, wrong sha → Err(true), cached after the first call.
    #[test]
    fn golden_chunk_zst_roundtrip() {
        let home = std::env::temp_dir().join("broom_test_zchunk");
        let _ = std::fs::remove_dir_all(&home);
        let (dir, cache) = (home.join("w"), home.join("zchunks"));
        std::fs::create_dir_all(&dir).unwrap();
        let get = |i: u64, sha: &str| super::chunk_zst(&dir.join("golden.vhdx"), &cache, i, sha);
        let mut data: Vec<u8> = (0..super::MANIFEST_CHUNK).map(|k| (k % 251) as u8).collect();
        data.extend(vec![5u8; 777]); // partial last chunk
        std::fs::write(dir.join("golden.vhdx"), &data).unwrap();
        super::write_manifest(&dir.join("golden.vhdx"), &dir).unwrap();
        let m = std::fs::read_to_string(dir.join("golden.chunks")).unwrap();
        let shas: Vec<&str> = m.lines().skip(1).collect();
        for (i, sha) in shas.iter().enumerate() {
            let z = get(i as u64, sha).unwrap();
            let raw = zstd::bulk::decompress(&z, super::MANIFEST_CHUNK).unwrap();
            assert_eq!(raw, data[i * super::MANIFEST_CHUNK..data.len().min((i + 1) * super::MANIFEST_CHUNK)]);
            assert!(cache.join(format!("{sha}.zst")).exists());
        }
        assert!(z_len_small(&get(0, shas[0]).unwrap()));
        let _ = std::fs::remove_dir_all(&cache); // uncached: the sha check runs
        assert!(get(1, shas[0]).unwrap_err().0, "sha of another chunk");
        // Out-of-range chunk index → rejected from the file length, never read (M4).
        assert!(get(999_999, shas[0]).unwrap_err().0, "chunk past end of golden");
        assert!(get(u64::MAX, shas[0]).unwrap_err().0, "index * chunk overflows");
        let _ = std::fs::remove_dir_all(&home);
        fn z_len_small(z: &[u8]) -> bool {
            z.len() < super::MANIFEST_CHUNK / 10 // a repeating pattern compresses a lot
        }
    }

    /// golden.chunks: size line + one sha256/zero per 4 MB (last chunk partial); whole hash = sha256 of the file.
    #[test]
    fn manifest_chunks_and_whole_hash() {
        let d = std::env::temp_dir().join("broom_test_manifest");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("golden.vhdx");
        let mut data = vec![7u8; super::MANIFEST_CHUNK]; // chunk 0: data
        data.extend(vec![0u8; super::MANIFEST_CHUNK]); // chunk 1: zero
        data.extend(vec![9u8; 1000]); // chunk 2: partial
        std::fs::write(&f, &data).unwrap();
        let hash = super::write_manifest(&f, &d).unwrap();
        assert_eq!(hash, super::file_hash(f.to_str().unwrap()).unwrap());
        let m = std::fs::read_to_string(d.join("golden.chunks")).unwrap();
        let lines: Vec<&str> = m.lines().collect();
        assert_eq!(lines[0], format!("size {}", data.len()));
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[2], "zero");
        let sha = |b: &[u8]| {
            use sha2::{Digest, Sha256};
            Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect::<String>()
        };
        assert_eq!(lines[1], sha(&data[..super::MANIFEST_CHUNK]));
        assert_eq!(lines[3], sha(&data[2 * super::MANIFEST_CHUNK..]));
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
    /// A descriptor pointing at an absolute path / traversal / another disk is refused; a monolithic vmdk and a
    /// descriptor whose extents are plain names in the folder pass.
    #[test]
    fn vmdk_refs_rejects_outside_paths() {
        let d = std::env::temp_dir().join("broom_test_vmdkref");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let write = |name: &str, body: &str| {
            let p = d.join(name);
            std::fs::write(&p, body).unwrap();
            p
        };
        let ok = write("good.vmdk", "# Disk DescriptorFile\ncreateType=\"twoGbMaxExtentSparse\"\nRW 4192256 SPARSE \"good-s001.vmdk\"\n");
        assert!(super::vmdk_refs_safe(&ok).is_ok());
        // monolithic (binary magic, not a descriptor) → passes (no extent lines)
        let mono = write("mono.vmdk", "KDMV\x01\x00\x00\x00 binary sparse header");
        assert!(super::vmdk_refs_safe(&mono).is_ok());
        for bad in [
            "# Disk DescriptorFile\nRW 1 FLAT \"/dev/sda\" 0\n",
            "# Disk DescriptorFile\nRW 1 FLAT \"../../etc/passwd\" 0\n",
            "# Disk DescriptorFile\nRW 1 FLAT \"sub/disk.vmdk\" 0\n",
            "# Disk DescriptorFile\nparentFileNameHint=\"/root/secret.img\"\n",
        ] {
            let p = write("bad.vmdk", bad);
            assert!(super::vmdk_refs_safe(&p).is_err(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

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
