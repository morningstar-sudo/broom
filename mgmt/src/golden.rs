// golden.rs — an upload becomes image.img: a VM folder (.vmx + .vmdk files), a single .vmdk / .img / .raw, or a
// .zip of either. Untrusted input processed as root: zip bombs and escaping paths refused, the VMDK actually used
// by the VM picked, every extent kept inside the upload folder, the virtual size capped.
use std::path::Path;

/// Move a file (rename, fall back to copy across filesystems).
fn mv(src: &Path, dst: &Path) -> Result<(), String> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
    let _ = std::fs::remove_file(src);
    Ok(())
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
    let free = crate::publish::free_bytes(dir);
    let budget = if free == 0 {
        MAX_GOLDEN
    } else {
        MAX_GOLDEN.min(free.saturating_sub(1 << 30))
    };
    let mut written = 0u64;
    for i in 0..z.len() {
        let mut entry = z.by_index(i).map_err(|e| format!("zip entry {i}: {e}"))?;
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let out = dir.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        let w = std::fs::File::create(&out).map_err(|e| format!("{}: {e}", out.display()))?;
        // Cap the read so a lying uncompressed-size can't fill the disk; +1 detects overflow of the budget.
        let remain = budget.saturating_sub(written);
        let n = write_sparse(&mut std::io::Read::take(&mut entry, remain + 1), &w)
            .map_err(|e| format!("unzip {}: {e}", out.display()))?;
        written += n;
        if written > budget {
            let _ = std::fs::remove_file(&out);
            return Err("zip is too large (bomb?) or not enough free space".into());
        }
    }
    Ok(())
}

/// `r` into the new, empty file `w`, all-zero MBs left as holes (a raw disk in a zip keeps its free space out of the
/// golden). Returns the bytes copied.
fn write_sparse(r: &mut impl std::io::Read, w: &std::fs::File) -> std::io::Result<u64> {
    use std::os::unix::fs::FileExt;
    let mut buf = vec![0u8; 4 << 20];
    let mut off = 0u64;
    loop {
        // Fill the buffer whole (a zip entry reads in small pieces): holes are only worth it on whole MBs.
        let mut n = 0;
        while n < buf.len() {
            match r.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        for (i, part) in buf[..n].chunks(1 << 20).enumerate() {
            if part.iter().any(|&b| b != 0) {
                w.write_all_at(part, off + (i << 20) as u64)?;
            }
        }
        off += n as u64;
        if n < buf.len() {
            w.set_len(off)?; // a trailing hole still counts in the size
            return Ok(off);
        }
    }
}

/// Every string a VMDK descriptor points at (extent file names + parentFileNameHint) must be a plain name inside the
/// upload folder — a descriptor can otherwise name `/dev/sda` or a server file as an "extent", and the conversion
/// (root) would copy it into the golden. Non-descriptor (monolithic) vmdks have no such lines and pass.
fn vmdk_refs_safe(vmdk: &Path) -> Result<(), String> {
    let head = vmdk_head(vmdk);
    if !is_vmdk_descriptor(vmdk) {
        return Ok(());
    }
    // Quoted names on extent lines (RW/RDONLY/NOACCESS … "name" …) and parentFileNameHint="name".
    let mut refs: Vec<&str> = Vec::new();
    for line in head.lines() {
        let t = line.trim_start();
        if (t.starts_with("RW") || t.starts_with("RDONLY") || t.starts_with("NOACCESS"))
            && let Some(q) = t.split('"').nth(1)
        {
            refs.push(q);
        }
    }
    if let Some(h) = head
        .split("parentFileNameHint=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
    {
        refs.push(h);
    }
    for r in refs {
        let bad = r.is_empty()
            || r.contains('/')
            || r.contains('\\')
            || r.contains("..")
            || r.starts_with(' ');
        if bad {
            return Err(format!(
                "VMDK references an unsafe path {r:?} — export the VM as a single monolithic vmdk"
            ));
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
    // Flushed before the rename: after a power loss image.img is the whole new golden or still the old one.
    let out = golden_from(dir, &tmp)
        .and_then(|()| {
            std::fs::File::open(&tmp)
                .and_then(|f| f.sync_all())
                .map_err(|e| format!("sync {}: {e}", tmp.display()))
        })
        .and_then(|()| {
            std::fs::rename(&tmp, dest).map_err(|e| format!("rename {}: {e}", tmp.display()))
        });
    if out.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    let _ = std::fs::remove_dir_all(dir);
    out
}

fn ext(p: &Path) -> String {
    p.extension()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
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
    // Extents are read from the same directory.
    // A .vmx lists every disk the VM attaches. broom serves ONE OS disk, so more than one disk is ambiguous (which
    // is the OS?) — fail loudly instead of silently converting whichever comes first (e.g. a stale SCSI disk while
    // the real OS is on nvme0:0).
    let vmx = files.iter().find(|p| ext(p) == "vmx");
    let chosen_vmdk = if let Some(vmx) = vmx {
        let txt =
            std::fs::read_to_string(vmx).map_err(|e| format!("read {}: {e}", vmx.display()))?;
        let disks: Vec<_> = vmx_disks(&txt)
            .into_iter()
            .map(|d| vmx.with_file_name(d))
            .filter(|p| p.exists())
            .collect();
        match disks.len() {
            0 if vmdks.len() == 1 => Some(vmdks.remove(0)),
            0 => pick_vmdk(&vmdks), // .vmx named no on-disk vmdk (odd) → best-effort
            1 => Some(disks[0].clone()),
            _ => {
                let names = disks
                    .iter()
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .collect::<Vec<_>>()
                    .join(", ");
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
        // Untrusted upload processed as root: every vmdk's extents must stay inside the folder (no /dev/sda, no server
        // files — vmdk.rs refuses those too), no parent disk, and the virtual size must be sane.
        for vmdk in walk(dir).iter().filter(|p| ext(p) == "vmdk") {
            vmdk_refs_safe(vmdk)?;
        }
        let sz = crate::vmdk::virtual_size(&v).map_err(|e| format!("{}: {e}", v.display()))?;
        if sz > MAX_GOLDEN {
            return Err(format!(
                "golden virtual size {sz} bytes is above the {MAX_GOLDEN} limit"
            ));
        }
        // The raw holds about the data stored in the vmdk files (zero grains stay holes).
        let data: u64 = walk(dir)
            .iter()
            .filter(|p| ext(p) == "vmdk")
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        crate::publish::need_space(dest.parent().unwrap_or(dir), data, "converting the vmdk")?;
        tracing::info!("golden: converting {}", v.display());
        crate::vmdk::to_raw(&v, dest)
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
    let n = std::fs::File::open(p)
        .and_then(|mut f| f.read(&mut buf))
        .unwrap_or(0);
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

/// The CURRENT descriptor among the vmdk files (split + snapshots): the descriptor that is not the parent
/// (`parentFileNameHint`) of any other descriptor = top of the snapshot chain. Not by name:
/// "win.vmdk" > "win-000001.vmdk" as strings, yet 000001 is the newest state.
fn pick_vmdk(vmdks: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    let desc: Vec<_> = vmdks.iter().filter(|p| is_vmdk_descriptor(p)).collect();
    let parents: Vec<String> = desc
        .iter()
        .filter_map(|p| {
            let h = vmdk_head(p);
            let v = h
                .split("parentFileNameHint=\"")
                .nth(1)?
                .split('"')
                .next()?
                .to_string();
            Some(
                v.rsplit(['\\', '/'])
                    .next()
                    .unwrap_or(&v)
                    .to_ascii_lowercase(),
            )
        })
        .collect();
    let mut top: Vec<_> = desc
        .into_iter()
        .filter(|p| {
            let n = p
                .file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            !parents.contains(&n)
        })
        .cloned()
        .collect();
    top.sort();
    top.pop()
}

/// Walk the files in dir (recursive). Plain recursion: a golden zip holds only a few files.
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

#[cfg(test)]
#[path = "../tests/unit/golden.rs"]
mod tests;
