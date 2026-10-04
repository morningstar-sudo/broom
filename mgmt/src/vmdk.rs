// vmdk.rs — VMware disks in-process (replaces qemu-img for uploads and exports).
// Read (upload → raw golden): the descriptor (a text file, or embedded in a monolithic sparse file) lists extents —
// SPARSE (grain directory/tables; streamOptimized = zlib-compressed grains, tables in the footer), FLAT/VMFS (plain
// bytes at an offset) or ZERO. Snapshots / linked clones (parentCID) are refused, and extent names must be plain
// file names next to the descriptor (an upload must never make the server read /dev/sda or its own files).
// Write (export): one monolithicSparse file VMware Workstation opens directly.
use flate2::read::ZlibDecoder;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::disk::Source;

const SECTOR: u64 = 512;
const GD_AT_END: u64 = u64::MAX;
const COMPRESSED_GRAINS: u32 = 1 << 16;

enum Kind {
    Sparse,
    /// Start sector inside the extent file.
    Flat(u64),
    Zero,
}

struct Extent {
    sectors: u64,
    kind: Kind,
    file: Option<PathBuf>,
}

struct Header {
    flags: u32,
    capacity: u64,
    grain: u64,
    desc_off: u64,
    desc_size: u64,
    gtes: u64,
    gd_off: u64,
}

fn rd(f: &File, off: u64, len: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; len];
    f.read_exact_at(&mut b, off).map_err(|e| format!("read VMDK at {off}: {e}"))?;
    Ok(b)
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn header(b: &[u8]) -> Option<Header> {
    (b.len() >= 512 && &b[..4] == b"KDMV").then(|| Header {
        flags: u32le(b, 8),
        capacity: u64le(b, 12),
        grain: u64le(b, 20),
        desc_off: u64le(b, 28),
        desc_size: u64le(b, 36),
        gtes: u32le(b, 44) as u64,
        gd_off: u64le(b, 56),
    })
}

/// Header of a sparse extent; streamOptimized keeps the real one (with the grain directory offset) in its footer:
/// … footer marker | footer | end-of-stream marker.
fn sparse_header(f: &File) -> Result<Header, String> {
    let h = header(&rd(f, 0, 512)?).ok_or("not a sparse VMDK extent")?;
    if h.gd_off != GD_AT_END {
        return Ok(h);
    }
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    header(&rd(f, len.checked_sub(1024).ok_or("truncated VMDK")?, 512)?).ok_or_else(|| "streamOptimized VMDK without its footer".into())
}

/// The descriptor text of the disk `path` (a descriptor file, or a monolithic sparse file with one inside).
fn descriptor(path: &Path) -> Result<String, String> {
    let f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    let first = rd(&f, 0, len.min(512) as usize)?;
    let text = if let Some(h) = header(&first) {
        if h.desc_off == 0 || h.desc_size == 0 || h.desc_size > 2048 {
            return Err(format!("{} is a data extent, not the disk — upload the .vmdk descriptor (or the whole VM folder)", path.display()));
        }
        rd(&f, h.desc_off.checked_mul(SECTOR).ok_or("corrupt VMDK header")?, (h.desc_size * SECTOR) as usize)?
    } else if len <= 64 * 1024 {
        rd(&f, 0, len as usize)?
    } else {
        return Err(format!("{} is not a VMDK", path.display()));
    };
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    Ok(String::from_utf8_lossy(&text[..end]).into_owned())
}

fn parse(text: &str, dir: &Path) -> Result<Vec<Extent>, String> {
    let mut out = Vec::new();
    for l in text.lines().map(str::trim) {
        if let Some(v) = l.strip_prefix("parentCID").and_then(|r| r.trim_start().strip_prefix('=')) {
            if !v.trim().eq_ignore_ascii_case("ffffffff") {
                return Err("this VMDK is a snapshot or linked clone (it has a parent disk) — delete the VM's snapshots \
                            or make a full clone, then upload"
                    .into());
            }
            continue;
        }
        let Some((access, rest)) = l.split_once(char::is_whitespace) else { continue };
        if !matches!(access, "RW" | "RDONLY" | "NOACCESS") {
            continue;
        }
        let mut it = rest.trim_start().splitn(2, char::is_whitespace);
        let sectors: u64 = it.next().and_then(|s| s.parse().ok()).ok_or_else(|| format!("bad extent line: {l}"))?;
        let rest = it.next().unwrap_or("").trim_start();
        let (ty, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let (name, after) = match (rest.find('"'), rest.rfind('"')) {
            (Some(a), Some(b)) if b > a => (&rest[a + 1..b], rest[b + 1..].trim()),
            _ => ("", ""),
        };
        let file = || -> Result<Option<PathBuf>, String> {
            // Plain name next to the descriptor only — never a path (an upload naming /dev/sda or a server file).
            if name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
                return Err(format!("VMDK extent {name:?} must be a file name in the same folder"));
            }
            Ok(Some(dir.join(name)))
        };
        let (kind, file) = match ty.to_ascii_uppercase().as_str() {
            "SPARSE" => (Kind::Sparse, file()?),
            "FLAT" | "VMFS" => (Kind::Flat(after.parse().unwrap_or(0)), file()?),
            "ZERO" => (Kind::Zero, None),
            other => return Err(format!("VMDK extent type {other} is not supported (snapshot or SE sparse disk?)")),
        };
        out.push(Extent { sectors, kind, file });
    }
    if out.is_empty() {
        return Err("VMDK descriptor lists no extents".into());
    }
    Ok(out)
}

fn extents(path: &Path) -> Result<Vec<Extent>, String> {
    parse(&descriptor(path)?, path.parent().unwrap_or(Path::new(".")))
}

/// Virtual disk size in bytes (replaces `qemu-img info`).
pub fn virtual_size(path: &Path) -> Result<u64, String> {
    extents(path)?.iter().try_fold(0u64, |a, e| a.checked_add(e.sectors.checked_mul(SECTOR)?)).ok_or_else(|| "VMDK size overflows".into())
}

/// VMDK (descriptor at `path`) → sparse raw disk at `dest` (replaces `qemu-img convert -O raw`).
pub fn to_raw(path: &Path, dest: &Path) -> Result<(), String> {
    let exts = extents(path)?;
    let out = File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    out.set_len(virtual_size(path)?).map_err(|e| e.to_string())?;
    let mut base = 0u64;
    for e in &exts {
        let n = e.sectors * SECTOR;
        match (&e.kind, &e.file) {
            (Kind::Sparse, Some(p)) => sparse_to(p, n, &out, base)?,
            (Kind::Flat(start), Some(p)) => flat_to(p, start.checked_mul(SECTOR).ok_or("VMDK extent offset overflows")?, n, &out, base)?,
            _ => {} // ZERO: stays a hole
        }
        base += n;
    }
    out.sync_all().map_err(|e| format!("{}: {e}", dest.display()))
}

fn write_nonzero(out: &File, data: &[u8], at: u64) -> Result<(), String> {
    if data.iter().any(|&b| b != 0) {
        out.write_all_at(data, at).map_err(|e| format!("write raw: {e}"))?;
    }
    Ok(())
}

/// Pieces copied per thread.
const PIECE: u64 = 32 << 20;

fn flat_to(p: &Path, start: u64, n: u64, out: &File, base: u64) -> Result<(), String> {
    let src = Source::new(&[(p, start, Some(n))])?;
    let mut pieces = Vec::new();
    for (a, b) in src.data_ranges() {
        pieces.extend((a..b).step_by(PIECE as usize).map(|o| (o, (b - o).min(PIECE))));
    }
    crate::disk::par_map(&pieces, |&(o, len)| {
        let mut buf = vec![0u8; len as usize];
        src.read_at(o, &mut buf)?;
        for (k, mb) in buf.chunks(1 << 20).enumerate() {
            write_nonzero(out, mb, base + o + ((k as u64) << 20))?;
        }
        Ok(())
    })?;
    Ok(())
}

fn sparse_to(p: &Path, n: u64, out: &File, base: u64) -> Result<(), String> {
    let f = File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let h = sparse_header(&f)?;
    if !(1..=2048).contains(&h.grain) || !(1..=4096).contains(&h.gtes) {
        return Err(format!("{}: unexpected VMDK grain layout", p.display()));
    }
    let gbytes = h.grain * SECTOR;
    let cap = h.capacity.saturating_mul(SECTOR).min(n);
    let ngr = cap.div_ceil(gbytes);
    let ngt = ngr.div_ceil(h.gtes);
    // The grain directory is read whole into RAM: a header promising more of it than the file holds is corrupt (a
    // tiny crafted upload would otherwise ask for a multi-GB buffer and abort the whole server).
    let flen = f.metadata().map_err(|e| e.to_string())?.len();
    // Also an absolute cap: an upload can be a sparse file of any apparent length. A real 4 TiB disk with 64 KiB grains
    // needs 512 KiB of grain directory.
    let gd_len = ngt
        .checked_mul(4)
        .filter(|&l| l <= flen && l <= 16 << 20)
        .ok_or_else(|| format!("{}: corrupt VMDK (grain directory larger than the file or 16 MiB)", p.display()))?;
    let gd_at = h.gd_off.checked_mul(SECTOR).ok_or_else(|| format!("{}: corrupt VMDK header", p.display()))?;
    let gd = rd(&f, gd_at, gd_len as usize)?;
    let compressed = h.flags & COMPRESSED_GRAINS != 0;
    // One grain table (its grains land at fixed raw offsets) per task, in parallel.
    let tables: Vec<(u64, u64)> = (0..ngt).map(|g| (g, u32le(&gd, g as usize * 4) as u64)).filter(|&(_, s)| s != 0).collect();
    crate::disk::par_map(&tables, |&(g, gt_sec)| {
        let mut buf = vec![0u8; gbytes as usize];
        let gt = rd(&f, gt_sec * SECTOR, (h.gtes * 4) as usize)?;
        for j in 0..h.gtes {
            let vg = g * h.gtes + j;
            if vg >= ngr {
                break;
            }
            let e = u32le(&gt, j as usize * 4) as u64;
            if e <= 1 {
                continue; // 0 = not allocated, 1 = zero grain
            }
            if compressed {
                // Grain marker: LBA (u64) + compressed size (u32) + zlib data.
                let size = u32le(&rd(&f, e * SECTOR, 12)?, 8) as u64;
                if size > 2 * gbytes + 4096 {
                    return Err(format!("{}: corrupt compressed grain", p.display()));
                }
                let z = rd(&f, e * SECTOR + 12, size as usize)?;
                buf.fill(0);
                let mut d = Vec::with_capacity(gbytes as usize);
                ZlibDecoder::new(&z[..]).take(gbytes).read_to_end(&mut d).map_err(|e| format!("{}: grain: {e}", p.display()))?;
                buf[..d.len()].copy_from_slice(&d);
            } else {
                f.read_exact_at(&mut buf, e * SECTOR).map_err(|e| format!("{}: grain: {e}", p.display()))?;
            }
            let off = vg * gbytes;
            let m = gbytes.min(cap - off) as usize;
            write_nonzero(out, &buf[..m], base + off)?;
        }
        Ok(())
    })?;
    Ok(())
}

/// `src` → one monolithicSparse VMDK at `path` (64 KB grains; zero grains not stored; redundant grain tables like
/// VMware writes). Replaces `qemu-img convert -O vmdk -o subformat=monolithicSparse`.
pub fn write_sparse(src: &Source, path: &Path) -> Result<(), String> {
    const GRAIN: u64 = 128; // sectors = 64 KB
    const GTES: u64 = 512;
    const DESC: u64 = 20; // descriptor sectors
    let cap = src.len.div_ceil(SECTOR);
    let ngr = cap.div_ceil(GRAIN);
    let ngt = ngr.div_ceil(GTES);
    let gd_secs = (ngt * 4).div_ceil(SECTOR);
    let gt_secs = ngt * GTES * 4 / SECTOR;
    let rgd = 1 + DESC;
    let rgt = rgd + gd_secs;
    let gd = rgt + gt_secs;
    let gt = gd + gd_secs;
    let overhead = (gt + gt_secs).div_ceil(GRAIN) * GRAIN;
    if overhead + ngr * GRAIN > u32::MAX as u64 {
        return Err("disk too large for a sparse VMDK".into());
    }

    let f = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let gbytes = GRAIN * SECTOR;
    let span = GTES * gbytes; // 32 MB: the grains of one grain table
    let mut spans = std::collections::BTreeSet::new();
    for (a, b) in src.data_ranges() {
        spans.extend(a / span..b.div_ceil(span));
    }
    let spans: Vec<u64> = spans.into_iter().filter(|&t| t < ngt).collect();
    let mut table = vec![0u8; (ngt * GTES * 4) as usize];
    let mut next = overhead;
    // A window of spans is read in parallel (each also listing its non-zero grains), the grains placed in disk
    // order, then each span's grains written by one thread. Buffers reused (RAM: workers × 32 MB).
    let w = crate::disk::workers();
    let bufs: Vec<std::sync::Mutex<Vec<u8>>> = (0..w).map(|_| std::sync::Mutex::new(vec![0u8; span as usize])).collect();
    for win in spans.chunks(w) {
        let slots: Vec<usize> = (0..win.len()).collect();
        let nonzero = crate::disk::par_map(&slots, |&k| {
            let mut b = bufs[k].lock().unwrap();
            src.read_at(win[k] * span, &mut b)?;
            Ok(b.chunks(gbytes as usize)
                .enumerate()
                .filter(|(j, g)| win[k] * GTES + (*j as u64) < ngr && g.iter().any(|&x| x != 0))
                .map(|(j, _)| j)
                .collect::<Vec<_>>())
        })?;
        let mut jobs: Vec<(usize, Vec<(usize, u64)>)> = Vec::new(); // (slot, [(grain in span, file sector)])
        for (k, js) in nonzero.into_iter().enumerate() {
            let mut placed = Vec::new();
            for j in js {
                let g = win[k] * GTES + j as u64;
                table[g as usize * 4..g as usize * 4 + 4].copy_from_slice(&(next as u32).to_le_bytes());
                placed.push((j, next));
                next += GRAIN;
            }
            jobs.push((k, placed));
        }
        crate::disk::par_map(&jobs, |(k, placed)| {
            let b = bufs[*k].lock().unwrap();
            for &(j, at) in placed {
                let g = &b[j * gbytes as usize..(j + 1) * gbytes as usize];
                f.write_all_at(g, at * SECTOR).map_err(|e| format!("write {}: {e}", path.display()))?;
            }
            Ok(())
        })?;
    }

    let mut h = vec![0u8; SECTOR as usize];
    h[0..4].copy_from_slice(b"KDMV");
    for (off, v) in [(4usize, 1u64), (8, 3)] {
        h[off..off + 4].copy_from_slice(&(v as u32).to_le_bytes()); // version 1; flags: newline test + redundant GT
    }
    for (off, v) in [(12usize, cap), (20, GRAIN), (28, 1), (36, DESC), (48, rgd), (56, gd), (64, overhead)] {
        h[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    h[44..48].copy_from_slice(&(GTES as u32).to_le_bytes());
    h[73..77].copy_from_slice(b"\n \r\n");
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let cyl = (cap / (16 * 63)).min(16383);
    let desc = format!(
        "# Disk DescriptorFile\nversion=1\nCID={:08x}\nparentCID=ffffffff\ncreateType=\"monolithicSparse\"\n\n\
         # Extent description\nRW {cap} SPARSE \"{name}\"\n\n# The Disk Data Base\n#DDB\n\n\
         ddb.virtualHWVersion = \"4\"\nddb.geometry.cylinders = \"{cyl}\"\nddb.geometry.heads = \"16\"\n\
         ddb.geometry.sectors = \"63\"\nddb.adapterType = \"ide\"\n",
        crate::now_secs() as u32 | 1
    );
    let mut dbuf = desc.into_bytes();
    dbuf.resize((DESC * SECTOR) as usize, 0);
    let dir = |gt0: u64| -> Vec<u8> { (0..ngt).flat_map(|i| ((gt0 + i * GTES * 4 / SECTOR) as u32).to_le_bytes()).collect() };
    let w = |off: u64, b: &[u8]| f.write_all_at(b, off * SECTOR).map_err(|e| format!("write {}: {e}", path.display()));
    w(0, &h)?;
    w(1, &dbuf)?;
    w(rgd, &dir(rgt))?;
    w(rgt, &table)?;
    w(gd, &dir(gt))?;
    w(gt, &table)?;
    f.set_len(next * SECTOR).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn qemu(args: &[&str]) {
        assert!(Command::new("qemu-img").args(args).status().unwrap().success(), "qemu-img {args:?}");
    }

    /// A 70 MB raw disk with data in a few places, holes elsewhere and a length that is no grain multiple.
    fn sample(d: &Path) -> PathBuf {
        let raw = d.join("disk.raw");
        let f = File::create(&raw).unwrap();
        let len = (70 << 20) + 512 * 5;
        f.set_len(len).unwrap();
        let mut seed = 0x9e37_79b9u32;
        let mut noise = |n: usize| -> Vec<u8> {
            (0..n).map(|_| { seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5; seed as u8 }).collect()
        };
        f.write_all_at(&noise(1000), 0).unwrap();
        f.write_all_at(&noise(3 << 20), 20 << 20).unwrap();
        f.write_all_at(b"tail", len - 4).unwrap();
        raw
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("broom_t_vmdk_{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A 1 KB crafted extent claiming 4 TiB with 1-sector grains and 1-entry tables (a 32 GiB grain directory) is
    /// refused — not a giant allocation that aborts the server. Absurd offsets don't overflow either.
    #[test]
    fn crafted_header_refused() {
        let d = dir("crafted");
        let mut h = vec![0u8; 1024];
        h[..4].copy_from_slice(b"KDMV");
        h[12..20].copy_from_slice(&((4u64 << 40) / SECTOR).to_le_bytes()); // capacity
        h[20..28].copy_from_slice(&1u64.to_le_bytes()); // grain
        h[44..48].copy_from_slice(&1u32.to_le_bytes()); // gtes
        h[56..64].copy_from_slice(&1u64.to_le_bytes()); // gd_off
        std::fs::write(d.join("x.vmdk"), &h).unwrap();
        let out = File::create(d.join("out.raw")).unwrap();
        let e = super::sparse_to(&d.join("x.vmdk"), 4 << 40, &out, 0).unwrap_err();
        assert!(e.contains("grain directory larger"), "{e}");
        h[12..20].copy_from_slice(&8u64.to_le_bytes());
        h[56..64].copy_from_slice(&u64::MAX.to_le_bytes());
        std::fs::write(d.join("x.vmdk"), &h).unwrap();
        assert!(super::sparse_to(&d.join("x.vmdk"), 4096, &out, 0).unwrap_err().contains("corrupt VMDK header"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Every VMDK layout VMware/ESXi/OVA produce (made here by qemu-img) reads back to the exact raw bytes.
    #[test]
    fn reads_every_subformat() {
        let d = dir("read");
        let raw = sample(&d);
        let want = std::fs::read(&raw).unwrap();
        for sub in ["monolithicSparse", "monolithicFlat", "twoGbMaxExtentSparse", "twoGbMaxExtentFlat", "streamOptimized"] {
            let sd = d.join(sub);
            std::fs::create_dir_all(&sd).unwrap();
            let v = sd.join("disk.vmdk");
            qemu(&["convert", "-f", "raw", "-O", "vmdk", "-o", &format!("subformat={sub}"), raw.to_str().unwrap(), v.to_str().unwrap()]);
            assert_eq!(virtual_size(&v).unwrap(), want.len() as u64, "{sub}");
            let out = sd.join("back.raw");
            to_raw(&v, &out).unwrap();
            assert!(std::fs::read(&out).unwrap() == want, "{sub}: same bytes");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn refuses_snapshots_and_paths() {
        let d = dir("refuse");
        let desc = |body: &str| {
            let p = d.join("x.vmdk");
            std::fs::write(&p, format!("# Disk DescriptorFile\nversion=1\nCID=12345678\n{body}")).unwrap();
            p
        };
        let e = virtual_size(&desc("parentCID=87654321\ncreateType=\"monolithicSparse\"\nRW 100 SPARSE \"x-s001.vmdk\"\n")).unwrap_err();
        assert!(e.contains("snapshot"), "{e}");
        for bad in ["/dev/sda", "../server.db", "sub/x.vmdk"] {
            let e = virtual_size(&desc(&format!("parentCID=ffffffff\nRW 100 FLAT \"{bad}\" 0\n"))).unwrap_err();
            assert!(e.contains("file name"), "{bad}: {e}");
        }
        assert!(virtual_size(&desc("parentCID=ffffffff\nRW 100 SESPARSE \"x.vmdk\"\n")).unwrap_err().contains("not supported"));
        assert_eq!(virtual_size(&desc("parentCID=ffffffff\nRW 100 ZERO\n")).unwrap(), 51200);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Export: written VMDK passes qemu-img check, qemu-img and our reader both get the raw bytes back, zero grains
    /// take no space.
    #[test]
    fn writes_monolithic_sparse() {
        let d = dir("write");
        let raw = sample(&d);
        let v = d.join("disk.vmdk");
        write_sparse(&Source::file(&raw).unwrap(), &v).unwrap();
        qemu(&["check", "-q", "-f", "vmdk", v.to_str().unwrap()]);
        let back = d.join("back.raw");
        qemu(&["convert", "-f", "vmdk", "-O", "raw", v.to_str().unwrap(), back.to_str().unwrap()]);
        let want = std::fs::read(&raw).unwrap();
        assert!(std::fs::read(&back).unwrap() == want, "qemu-img reads it back");
        let mine = d.join("mine.raw");
        to_raw(&v, &mine).unwrap();
        assert!(std::fs::read(&mine).unwrap() == want, "our reader too");
        assert!(std::fs::metadata(&v).unwrap().len() < 5 << 20, "only the written grains are stored");
        let _ = std::fs::remove_dir_all(&d);
    }
}
