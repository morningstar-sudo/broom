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
    // Extents are read from the same directory.
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
        // Untrusted upload processed as root: every vmdk's extents must stay inside the folder (no /dev/sda, no server
        // files — vmdk.rs refuses those too), no parent disk, and the virtual size must be sane.
        for vmdk in walk(dir).iter().filter(|p| ext(p) == "vmdk") {
            vmdk_refs_safe(vmdk)?;
        }
        let sz = crate::vmdk::virtual_size(&v).map_err(|e| format!("{}: {e}", v.display()))?;
        if sz > MAX_GOLDEN {
            return Err(format!("golden virtual size {sz} bytes is above the {MAX_GOLDEN} limit"));
        }
        // The raw holds about the data stored in the vmdk files (zero grains stay holes).
        let data: u64 = walk(dir).iter().filter(|p| ext(p) == "vmdk").filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
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
mod tests {
    use super::*;

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
}
