// disk.rs — partition tables of disk images, in-process (replaces `sfdisk -J` + the sfdisk rewrite of a golden).
// Reads GPT (512- or 4096-byte sectors, header CRC checked) or MBR (primary + logical partitions in an extended
// one); writes the single-partition GPT a published Windows golden keeps.
use std::fs::File;
use std::os::unix::fs::FileExt;

/// GPT type of an EFI system partition / Microsoft basic data (NTFS) partition.
pub const ESP: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
pub const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";

#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    /// Bytes from the start of the disk.
    pub start: u64,
    pub size: u64,
    /// GPT type GUID (upper case, `XXXXXXXX-…`) or MBR type in lower-case hex without 0x (`7`, `ef`, `83`) — the
    /// same spelling `sfdisk -J` used.
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub gpt: bool,
    pub sector: u64,
    pub parts: Vec<Part>,
}

pub(crate) fn rd(f: &File, off: u64, len: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; len];
    f.read_exact_at(&mut b, off)
        .map_err(|e| format!("read at {off}: {e}"))?;
    Ok(b)
}

pub(crate) fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
pub(crate) fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// GPT's mixed-endian GUID bytes → `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`.
pub fn guid_str(g: &[u8]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        u32le(g, 0),
        u16::from_le_bytes([g[4], g[5]]),
        u16::from_le_bytes([g[6], g[7]]),
        g[8],
        g[9],
        g[10],
        g[11],
        g[12],
        g[13],
        g[14],
        g[15]
    )
}

/// `XXXXXXXX-…` (braces allowed) → GPT bytes.
pub(crate) fn guid_bytes(s: &str) -> Option<[u8; 16]> {
    let h: String = s
        .chars()
        .filter(|c| !matches!(c, '-' | '{' | '}'))
        .collect();
    if h.len() != 32 {
        return None;
    }
    let b: Vec<u8> = (0..16)
        .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok())
        .collect::<Option<_>>()?;
    let mut g = [0u8; 16];
    g[..4].copy_from_slice(&[b[3], b[2], b[1], b[0]]);
    g[4..6].copy_from_slice(&[b[5], b[4]]);
    g[6..8].copy_from_slice(&[b[7], b[6]]);
    g[8..].copy_from_slice(&b[8..]);
    Some(g)
}

/// Bytes in a disk image file or a disk (/dev/sda: its metadata says 0, the end of the device is its size).
pub fn size(f: &File) -> std::io::Result<u64> {
    match f.metadata()?.len() {
        0 => std::io::Seek::seek(&mut &*f, std::io::SeekFrom::End(0)),
        n => Ok(n),
    }
}

/// The partition table of a disk image or a disk; None = no table (a bare filesystem / empty disk).
pub fn read(path: &str) -> Result<Option<Table>, String> {
    let f = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let len = size(&f).map_err(|e| format!("{path}: {e}"))?;
    if len < 1024 {
        return Ok(None);
    }
    for ss in [512u64, 4096] {
        if len >= ss * 2
            && let Some(t) = read_gpt(&f, ss)?
        {
            return Ok(Some(t));
        }
    }
    read_mbr(&f, len)
}

fn read_gpt(f: &File, ss: u64) -> Result<Option<Table>, String> {
    let h = rd(f, ss, 92)?;
    if &h[..8] != b"EFI PART" {
        return Ok(None);
    }
    let hsize = (u32le(&h, 12) as usize).clamp(92, 512);
    let mut h = rd(f, ss, hsize)?;
    let want = u32le(&h, 16);
    h[16..20].fill(0);
    if crc32fast::hash(&h) != want {
        return Err("GPT header checksum mismatch (corrupt partition table)".into());
    }
    let (lba, n, esize) = (
        u64le(&h, 72),
        u32le(&h, 80) as usize,
        u32le(&h, 84) as usize,
    );
    if !(128..=4096).contains(&esize) || n > 1024 {
        return Err(format!("GPT: unexpected entry table ({n} × {esize} bytes)"));
    }
    // LBAs come from the image: one past any disk is a corrupt table, not an overflow.
    let past = || "GPT: an LBA past any disk (corrupt partition table)".to_string();
    let ents = rd(f, lba.checked_mul(ss).ok_or_else(past)?, n * esize)?;
    let parts = ents
        .chunks(esize)
        .filter(|e| e[..16].iter().any(|&b| b != 0))
        .map(|e| {
            let (first, last) = (u64le(e, 32), u64le(e, 40));
            Some(Part {
                start: first.checked_mul(ss)?,
                size: last.checked_add(1)?.saturating_sub(first).checked_mul(ss)?,
                kind: guid_str(&e[..16]),
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(past)?;
    Ok(Some(Table {
        gpt: true,
        sector: ss,
        parts,
    }))
}

fn read_mbr(f: &File, len: u64) -> Result<Option<Table>, String> {
    let m = rd(f, 0, 512)?;
    if m[510..] != [0x55, 0xAA] {
        return Ok(None);
    }
    let entry = |b: &[u8], i: usize| {
        let e = &b[446 + i * 16..462 + i * 16];
        (e[4], u32le(e, 8) as u64, u32le(e, 12) as u64)
    };
    // A FAT/NTFS boot sector also ends in 55AA: a real MBR has sane entries inside the disk.
    let prim: Vec<(u8, u64, u64)> = (0..4)
        .map(|i| entry(&m, i))
        .filter(|(t, _, n)| *t != 0 && *n > 0)
        .collect();
    if prim.is_empty() || prim.iter().any(|(_, s, n)| (s + n) * 512 > len) {
        return Ok(None);
    }
    let mut parts = Vec::new();
    for (t, s, n) in prim {
        if matches!(t, 0x05 | 0x0f | 0x85) {
            // Extended: a chain of EBRs, each = one logical partition (relative to its EBR) + a link (relative to
            // the extended partition's start).
            let mut ebr = s;
            for _ in 0..128 {
                let b = rd(f, ebr * 512, 512)?;
                if b[510..] != [0x55, 0xAA] {
                    break;
                }
                let (lt, ls, ln) = entry(&b, 0);
                if lt != 0 && ln > 0 {
                    parts.push(Part {
                        start: (ebr + ls) * 512,
                        size: ln * 512,
                        kind: format!("{lt:x}"),
                    });
                }
                let (nt, ns, _) = entry(&b, 1);
                if nt == 0 || ns == 0 {
                    break;
                }
                ebr = s + ns;
            }
        } else {
            parts.push(Part {
                start: s * 512,
                size: n * 512,
                kind: format!("{t:x}"),
            });
        }
    }
    parts.sort_by_key(|p| p.start);
    Ok(Some(Table {
        gpt: false,
        sector: 512,
        parts,
    }))
}

/// Rewrite `path` (a GPT disk image, 512-byte sectors) so it keeps ONE basic-data partition [start, start+size)
/// — what a published Windows golden needs (standard native VHD boot). Nothing outside the tables is touched.
/// The disk GUID and the partition's own GUID/name/attributes are kept when the old table had that partition
/// (Windows' drive-letter mapping uses the partition GUID), so republishing gives the same bytes.
pub fn write_single_gpt(path: &str, start: u64, size: u64) -> Result<(), String> {
    const SS: u64 = 512;
    if !start.is_multiple_of(SS) || !size.is_multiple_of(SS) || size == 0 {
        return Err(format!("partition {start}+{size} is not sector-aligned"));
    }
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("{path}: {e}"))?;
    let total = f.metadata().map_err(|e| e.to_string())?.len() / SS;
    let (first, last) = (start / SS, (start + size) / SS - 1);
    if first < 34 || last + 34 > total {
        return Err(format!(
            "partition {start}+{size} does not fit a GPT on a {}-byte disk",
            total * SS
        ));
    }
    // Old table (if GPT): keep disk GUID + the matching entry's identity.
    let mut disk_guid = *blake3::hash(format!("broom-disk {path} {total}").as_bytes()).as_bytes();
    let mut entry = [0u8; 128];
    entry[16..32].copy_from_slice(
        &blake3::hash(format!("broom-part {start} {size}").as_bytes()).as_bytes()[..16],
    );
    if let Ok(h) = rd(&f, SS, 92)
        && &h[..8] == b"EFI PART"
    {
        disk_guid[..16].copy_from_slice(&h[56..72]);
        let (lba, n, esize) = (
            u64le(&h, 72),
            (u32le(&h, 80) as usize).min(1024),
            u32le(&h, 84) as usize,
        );
        if (128..=4096).contains(&esize)
            && let Ok(ents) = rd(&f, lba * SS, n * esize)
            && let Some(e) = ents
                .chunks(esize)
                .find(|e| u64le(e, 32) == first && e[..16].iter().any(|&b| b != 0))
        {
            entry.copy_from_slice(&e[..128]);
        }
    }
    entry[..16].copy_from_slice(&guid_bytes(BASIC_DATA).unwrap());
    entry[32..40].copy_from_slice(&first.to_le_bytes());
    entry[40..48].copy_from_slice(&last.to_le_bytes());
    let mut ents = vec![0u8; 128 * 128];
    ents[..128].copy_from_slice(&entry);
    let ents_crc = crc32fast::hash(&ents);
    let header = |my: u64, alt: u64, ents_lba: u64| {
        let mut h = vec![0u8; 92];
        h[..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&my.to_le_bytes());
        h[32..40].copy_from_slice(&alt.to_le_bytes());
        h[40..48].copy_from_slice(&34u64.to_le_bytes());
        h[48..56].copy_from_slice(&(total - 34).to_le_bytes());
        h[56..72].copy_from_slice(&disk_guid[..16]);
        h[72..80].copy_from_slice(&ents_lba.to_le_bytes());
        h[80..84].copy_from_slice(&128u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&ents_crc.to_le_bytes());
        let crc = crc32fast::hash(&h);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h.resize(SS as usize, 0);
        h
    };
    // Protective MBR: keep the boot code + disk signature bytes, one 0xEE entry over the whole disk.
    let mut mbr = rd(&f, 0, 512)?;
    mbr[446..510].fill(0);
    mbr[446 + 4] = 0xEE;
    mbr[446 + 1..446 + 4].copy_from_slice(&[0x00, 0x02, 0x00]); // CHS 0/0/2
    mbr[446 + 5..446 + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    mbr[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    mbr[446 + 12..446 + 16]
        .copy_from_slice(&((total - 1).min(u32::MAX as u64) as u32).to_le_bytes());
    mbr[510..].copy_from_slice(&[0x55, 0xAA]);
    let w = |off: u64, b: &[u8]| {
        f.write_all_at(b, off)
            .map_err(|e| format!("write {path} at {off}: {e}"))
    };
    w(0, &mbr)?;
    w(SS, &header(1, total - 1, 2))?;
    w(2 * SS, &ents)?;
    w((total - 33) * SS, &ents)?;
    w((total - 1) * SS, &header(total - 1, 1, total - 33))?;
    f.sync_all().map_err(|e| e.to_string())
}

/// A GPT disk image written onto a bigger disk (Deploy, capture.rs): the backup header + entries go to the disk's
/// real end, the primary header points there and may use the space up to it, the protective MBR covers it. The old
/// backup header is zeroed. Ok(false) = no GPT, or already at the end.
pub fn move_backup_gpt(path: &str) -> Result<bool, String> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("{path}: {e}"))?;
    let len = size(&f).map_err(|e| format!("{path}: {e}"))?;
    let w = |off: u64, b: &[u8]| {
        f.write_all_at(b, off)
            .map_err(|e| format!("write {path} at {off}: {e}"))
    };
    for ss in [512u64, 4096] {
        if len < ss * 2 || &rd(&f, ss, 92)?[..8] != b"EFI PART" {
            continue;
        }
        let hsize = (u32le(&rd(&f, ss, 92)?, 12) as usize).clamp(92, 512);
        let mut h = rd(&f, ss, hsize)?;
        let (last, old) = (len / ss - 1, u64le(&h, 32));
        if old == last {
            return Ok(false);
        }
        let (n, esize) = (u32le(&h, 80) as u64, u32le(&h, 84) as u64);
        let ent_sectors = (n * esize).div_ceil(ss);
        if old > last || ent_sectors + 2 >= last {
            return Err(format!("{path}: GPT does not fit this disk"));
        }
        let ents = rd(&f, u64le(&h, 72) * ss, (n * esize) as usize)?;
        let seal = |h: &mut Vec<u8>| {
            h[16..20].fill(0);
            let c = crc32fast::hash(h);
            h[16..20].copy_from_slice(&c.to_le_bytes());
        };
        h[32..40].copy_from_slice(&last.to_le_bytes()); // alternate LBA
        h[48..56].copy_from_slice(&(last - ent_sectors - 1).to_le_bytes()); // last usable LBA
        seal(&mut h);
        let mut b = h.clone();
        b[24..32].copy_from_slice(&last.to_le_bytes()); // my LBA
        b[32..40].copy_from_slice(&1u64.to_le_bytes());
        b[72..80].copy_from_slice(&(last - ent_sectors).to_le_bytes()); // its entries
        seal(&mut b);
        w((last - ent_sectors) * ss, &ents)?;
        w(last * ss, &b)?;
        w(ss, &h)?;
        w(old * ss, &vec![0u8; ss as usize])?;
        // Protective MBR (type ee): its size field covers the whole disk again (capped at 32 bits).
        let mbr = rd(&f, 0, 512)?;
        if mbr[450] == 0xee {
            w(446 + 12, &(last.min(u32::MAX as u64) as u32).to_le_bytes())?;
        }
        f.sync_all().map_err(|e| e.to_string())?;
        return Ok(true);
    }
    Ok(false)
}

/// I/O threads for image conversions: the CPU count, 2..=8 (more only queues behind the disk).
pub fn workers() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(2, 8)
}

/// `f` over `items` on `workers()` threads (a shared work queue), results in input order. The first error stops
/// the remaining items and is returned.
pub fn par_map<T: Sync, R: Send>(
    items: &[T],
    f: impl Fn(&T) -> Result<R, String> + Sync,
) -> Result<Vec<R>, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let out: Vec<parking_lot::Mutex<Option<Result<R, String>>>> =
        items.iter().map(|_| Default::default()).collect();
    std::thread::scope(|s| {
        for _ in 0..workers().min(items.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= items.len() {
                        break;
                    }
                    let r = f(&items[i]);
                    let failed = r.is_err();
                    *out[i].lock() = Some(r);
                    if failed {
                        next.store(items.len(), Ordering::Relaxed);
                        break;
                    }
                }
            });
        }
    });
    // Items before a failure were all claimed earlier, so they finished: the first Err in order is the real one.
    out.into_iter()
        .map(|m| {
            m.into_inner()
                .unwrap_or_else(|| Err("stopped after an error".into()))
        })
        .collect()
}

/// Free [off, off+len) of a file without changing its size (fallocate PUNCH_HOLE|KEEP_SIZE): reads there give zeros.
pub fn punch_hole(f: &File, off: u64, len: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    // SAFETY: valid open fd; offsets are plain integers.
    match unsafe { libc::fallocate(f.as_raw_fd(), mode, off as i64, len as i64) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Pieces copied per thread.
const PIECE: u64 = 32 << 20;

/// [start, start+n) of `p` → `out` at `base`: only its data ranges are read, in parallel; all-zero MBs are not
/// written (`out` starts as a hole).
pub fn copy_into(
    p: &std::path::Path,
    start: u64,
    n: u64,
    out: &File,
    base: u64,
) -> Result<(), String> {
    let src = Source::new(&[(p, start, Some(n))])?;
    let mut pieces = Vec::new();
    for (a, b) in src.data_ranges() {
        pieces.extend(
            (a..b)
                .step_by(PIECE as usize)
                .map(|o| (o, (b - o).min(PIECE))),
        );
    }
    par_map(&pieces, |&(o, len)| {
        let mut buf = vec![0u8; len as usize];
        src.read_at(o, &mut buf)?;
        for (k, mb) in buf.chunks(1 << 20).enumerate() {
            if mb.iter().any(|&b| b != 0) {
                out.write_all_at(mb, base + o + ((k as u64) << 20))
                    .map_err(|e| format!("write: {e}"))?;
            }
        }
        Ok(())
    })?;
    Ok(())
}

/// [off, off+len) of `src` (None = to its end) → a new sparse file `dst`, flushed.
pub fn copy_region(
    src: &std::path::Path,
    off: u64,
    len: Option<u64>,
    dst: &std::path::Path,
) -> Result<(), String> {
    let len = match len {
        Some(l) => l,
        None => std::fs::metadata(src)
            .map_err(|e| format!("{}: {e}", src.display()))?
            .len()
            .saturating_sub(off),
    };
    let out = File::create(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    out.set_len(len).map_err(|e| e.to_string())?;
    copy_into(src, off, len, &out, 0)?;
    out.sync_all()
        .map_err(|e| format!("{}: {e}", dst.display()))
}

/// A virtual disk made of byte ranges of files, back to back (a whole raw golden, or head.raw + the Windows
/// partition of a golden + tail.raw for an export). Read side of the VHDX/VMDK writers.
pub struct Source {
    segs: Vec<(File, u64, u64)>, // (file, offset in it, length)
    pub len: u64,
}

impl Source {
    pub fn file(path: &std::path::Path) -> Result<Source, String> {
        Source::new(&[(path, 0, None)])
    }

    /// Segments (path, offset, length — None = to the end of the file).
    pub fn new(parts: &[(&std::path::Path, u64, Option<u64>)]) -> Result<Source, String> {
        let mut segs = Vec::new();
        let mut len = 0;
        for (p, off, l) in parts {
            let f = File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
            let size = f.metadata().map_err(|e| e.to_string())?.len();
            let l = l.unwrap_or(size.saturating_sub(*off));
            if off + l > size {
                return Err(format!(
                    "{}: range {off}+{l} beyond its {size} bytes",
                    p.display()
                ));
            }
            len += l;
            segs.push((f, *off, l));
        }
        Ok(Source { segs, len })
    }

    /// Read `buf.len()` bytes at virtual offset `off`; past the end reads zeros.
    pub fn read_at(&self, mut off: u64, buf: &mut [u8]) -> Result<(), String> {
        buf.fill(0);
        let (mut base, mut done) = (0u64, 0usize);
        for (f, so, sl) in &self.segs {
            if done == buf.len() {
                break;
            }
            if off < base + sl {
                let n = ((base + sl - off) as usize).min(buf.len() - done);
                f.read_exact_at(&mut buf[done..done + n], so + (off - base))
                    .map_err(|e| format!("read: {e}"))?;
                done += n;
                off += n as u64;
            }
            base += sl;
        }
        Ok(())
    }

    /// Virtual byte ranges that may hold data, sorted: the holes of sparse files are skipped (SEEK_DATA/SEEK_HOLE),
    /// so a mostly empty 100 GB golden is not read end to end. A filesystem without SEEK_DATA → everything.
    pub fn data_ranges(&self) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut base = 0u64;
        for (f, so, sl) in &self.segs {
            out.extend(
                file_data_ranges(f, *so, so + sl)
                    .into_iter()
                    .map(|(a, b)| (base + a - so, base + b - so)),
            );
            base += sl;
        }
        out
    }
}

/// Byte ranges of [start, end) of an open file that may hold data, sorted (SEEK_DATA/SEEK_HOLE). A filesystem
/// without SEEK_DATA → everything.
pub fn file_data_ranges(f: &File, start: u64, end: u64) -> Vec<(u64, u64)> {
    use std::os::fd::AsRawFd;
    let fd = f.as_raw_fd();
    let mut out = Vec::new();
    let mut pos = start;
    while pos < end {
        // SAFETY: lseek on an open fd; no memory is touched.
        let d = unsafe { libc::lseek(fd, pos as libc::off_t, libc::SEEK_DATA) };
        if d < 0 {
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENXIO) {
                out.push((pos, end)); // no SEEK_DATA here: assume data
            }
            break;
        }
        let d = (d as u64).max(pos);
        if d >= end {
            break;
        }
        // SAFETY: as above.
        let h = unsafe { libc::lseek(fd, d as libc::off_t, libc::SEEK_HOLE) };
        let h = if h < 0 { end } else { (h as u64).min(end) };
        out.push((d, h));
        pos = h;
    }
    out
}

#[cfg(test)]
#[path = "../tests/unit/disk.rs"]
mod tests;
