// vhdx.rs — minimal VHDX (MS-VHDX v1.0) reader/writer for Windows native boot.
// Write: the golden as a DYNAMIC VHDX of its raw disk, made on the fly, never stored (Virtual), and empty DIFFERENCING
// files (BAT all "not present" → every read falls through to the parent) pointing to their parent via
// parent_linkage + relative_path (write_empty). Read (tests only — they check what was written): disk parameters +
// DataWriteGuid (read_info).
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

const KB64: u64 = 64 * 1024;
const MB: u64 = 1024 * 1024;
/// Block size of child files (Hyper-V uses 2MB for differencing).
const CHILD_BLOCK: u64 = 2 * MB;

const BAT_GUID: &str = "2DC27766-F623-4200-9D64-115E9BFD4A08";
const META_GUID: &str = "8B7CA206-4790-4B9A-B8FE-575F050F886E";
const FILE_PARAMS: &str = "CAA16737-FA36-4D43-B3B6-33F0AA44E76B";
const VDISK_SIZE: &str = "2FA54224-CD1B-4876-B211-5DBED83BF4B8";
const PAGE83: &str = "BECA12AB-B2E6-4523-93EF-C309E000C746";
const LOGICAL_SS: &str = "8141BF1D-A96F-4709-BA47-F233A8FAAB5F";
const PHYSICAL_SS: &str = "CDA348C7-445D-4471-9CC9-E9885251C556";
const PARENT_LOC: &str = "A8D35F2D-B30B-454D-ABF7-D3D84834AB0C";
const PARENT_LOC_TYPE: &str = "B04AEFB7-D19E-4A81-B789-25B8E9445913";

/// Parameters of a VHDX file that a child must match.
pub struct Info {
    pub data_write_guid: [u8; 16],
    pub virtual_size: u64,
    pub logical_sector: u32,
    pub physical_sector: u32,
}

/// CRC-32C (Castagnoli) — checksum header/region table VHDX.
fn crc32c(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
        }
    }
    !c
}

/// "XXXXXXXX-XXXX-..." → 16 bytes in Windows layout (first 3 fields little-endian).
fn guid_bytes(s: &str) -> [u8; 16] {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let mut b = [0u8; 16];
    for i in 0..16 {
        b[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
    }
    b[0..4].reverse();
    b[4..6].reverse();
    b[6..8].reverse();
    b
}

/// 16 bytes in Windows layout → "{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}" (parent_linkage format).
pub fn guid_str(b: &[u8; 16]) -> String {
    let h = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let mut a = b.to_vec();
    a[0..4].reverse();
    a[4..6].reverse();
    a[6..8].reverse();
    format!("{{{}-{}-{}-{}-{}}}", h(&a[0..4]), h(&a[4..6]), h(&a[6..8]), h(&a[8..10]), h(&a[10..16]))
}

fn rand_guid() -> [u8; 16] {
    let mut b = [0u8; 16];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b[7] = (b[7] & 0x0f) | 0x40; // version 4 (byte 7 = high byte of field 3, LE layout)
    b[8] = (b[8] & 0x3f) | 0x80;
    b
}

#[cfg(test)]
fn u16le(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
#[cfg(test)]
fn u32le(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
#[cfg(test)]
fn u64le(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

#[cfg(test)]
fn read_at(f: &mut File, off: u64, len: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; len];
    f.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
    f.read_exact(&mut buf).map_err(|e| format!("read VHDX @{off}: {e}"))?;
    Ok(buf)
}

/// Verify the checksum of a block whose crc field is at bytes 4..8.
#[cfg(test)]
fn crc_ok(block: &[u8]) -> bool {
    let mut t = block.to_vec();
    let want = u32le(&t, 4);
    t[4..8].fill(0);
    crc32c(&t) == want
}

/// Read parameters + DataWriteGuid (current header = highest valid seq).
#[cfg(test)] // the stage reads goldens on the clients; here only the tests check what was written
pub fn read_info(path: &str) -> Result<Info, String> {
    let mut f = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    if read_at(&mut f, 0, 8)? != b"vhdxfile" {
        return Err(format!("{path}: not a VHDX"));
    }
    let mut hdr: Option<Vec<u8>> = None;
    for off in [KB64, 2 * KB64] {
        let h = read_at(&mut f, off, 4096)?;
        if &h[0..4] == b"head" && crc_ok(&h) && hdr.as_ref().map_or(true, |c| u64le(&h, 8) > u64le(c, 8)) {
            hdr = Some(h);
        }
    }
    let hdr = hdr.ok_or(format!("{path}: corrupt VHDX header"))?;
    if hdr[48..64].iter().any(|&x| x != 0) {
        return Err(format!("{path}: VHDX has an unreplayed log (unclean shutdown?)"));
    }
    let rt = read_at(&mut f, 3 * KB64, KB64 as usize)?;
    if &rt[0..4] != b"regi" || !crc_ok(&rt) {
        return Err(format!("{path}: corrupt region table"));
    }
    let meta = guid_bytes(META_GUID);
    let (moff, mlen) = (0..u32le(&rt, 8) as usize)
        .map(|i| 16 + i * 32)
        .find(|&e| rt[e..e + 16] == meta)
        .map(|e| (u64le(&rt, e + 16), u32le(&rt, e + 24) as usize))
        .ok_or(format!("{path}: missing metadata region"))?;
    let m = read_at(&mut f, moff, mlen)?;
    if &m[0..8] != b"metadata" {
        return Err(format!("{path}: corrupt metadata"));
    }
    let item = |id: &str| -> Result<usize, String> {
        let g = guid_bytes(id);
        (0..u16le(&m, 10) as usize)
            .map(|i| 32 + i * 32)
            .find(|&e| m[e..e + 16] == g)
            .map(|e| u32le(&m, e + 16) as usize)
            .ok_or(format!("{path}: missing metadata {id}"))
    };
    Ok(Info {
        data_write_guid: hdr[32..48].try_into().unwrap(),
        virtual_size: u64le(&m, item(VDISK_SIZE)?),
        logical_sector: u32le(&m, item(LOGICAL_SS)?),
        physical_sector: u32le(&m, item(PHYSICAL_SS)?),
    })
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// Byte layout of a VHDX without its payload: 0..1MB file identifier + 2 headers + 2 region tables; log 1..2MB;
/// BAT from 2MB; metadata (1MB) after the BAT. Payload blocks start at `meta_off + 1MB`.
struct Layout {
    head: Vec<u8>,
    meta: Vec<u8>,
    bat_off: u64,
    meta_off: u64,
    /// Absolute offset of the parent_linkage GUID string (differencing only).
    linkage_abs: u64,
}

/// `block` = payload block size; `rel_parent` Some → differencing (parent_linkage = info's DataWriteGuid);
/// `ids` = FileWriteGuid, DataWriteGuid, page 83 id.
fn layout(info: &Info, block: u64, rel_parent: Option<&str>, ids: [[u8; 16]; 3]) -> Layout {
    let ls = info.logical_sector as u64;
    let chunk = (1u64 << 23) * ls / block;
    let data_blocks = info.virtual_size.div_ceil(block);
    let bat_entries = if rel_parent.is_some() {
        data_blocks.div_ceil(chunk) * (chunk + 1)
    } else {
        data_blocks + data_blocks.saturating_sub(1) / chunk
    };
    let bat_len = (bat_entries * 8).div_ceil(MB) * MB;
    let (log_off, bat_off) = (MB, 2 * MB);
    let meta_off = bat_off + bat_len;

    // 0..1MB: file identifier + 2 header + 2 region table.
    let mut head = vec![0u8; MB as usize];
    head[0..8].copy_from_slice(b"vhdxfile");
    let creator = utf16("broom");
    head[8..8 + creator.len()].copy_from_slice(&creator);
    let [fw, dw, page83] = ids;
    for (i, off) in [KB64, 2 * KB64].into_iter().enumerate() {
        let h = &mut head[off as usize..off as usize + 4096];
        h[0..4].copy_from_slice(b"head");
        h[8..16].copy_from_slice(&(i as u64 + 1).to_le_bytes());
        h[16..32].copy_from_slice(&fw);
        h[32..48].copy_from_slice(&dw);
        h[66..68].copy_from_slice(&1u16.to_le_bytes()); // Version
        h[68..72].copy_from_slice(&(MB as u32).to_le_bytes()); // LogLength
        h[72..80].copy_from_slice(&log_off.to_le_bytes());
        let c = crc32c(h);
        h[4..8].copy_from_slice(&c.to_le_bytes());
    }
    for off in [3 * KB64, 4 * KB64] {
        let r = &mut head[off as usize..(off + KB64) as usize];
        r[0..4].copy_from_slice(b"regi");
        r[8..12].copy_from_slice(&2u32.to_le_bytes());
        for (i, (g, o, l)) in [(BAT_GUID, bat_off, bat_len), (META_GUID, meta_off, MB)].iter().enumerate() {
            let e = 16 + i * 32;
            r[e..e + 16].copy_from_slice(&guid_bytes(g));
            r[e + 16..e + 24].copy_from_slice(&o.to_le_bytes());
            r[e + 24..e + 28].copy_from_slice(&(*l as u32).to_le_bytes());
            r[e + 28..e + 32].copy_from_slice(&1u32.to_le_bytes()); // Required
        }
        let c = crc32c(r);
        r[4..8].copy_from_slice(&c.to_le_bytes());
    }

    // Metadata region: entry table (≤64KB) + entry data from 64KB.
    let mut items: Vec<(&str, Vec<u8>, u32)> = vec![
        (FILE_PARAMS, {
            let mut v = (block as u32).to_le_bytes().to_vec();
            v.extend_from_slice(&(if rel_parent.is_some() { 2u32 } else { 0 }).to_le_bytes()); // HasParent
            v
        }, 4), // IsRequired
        (VDISK_SIZE, info.virtual_size.to_le_bytes().to_vec(), 6), // IsVirtualDisk|IsRequired
        (PAGE83, page83.to_vec(), 6),
        (LOGICAL_SS, info.logical_sector.to_le_bytes().to_vec(), 6),
        (PHYSICAL_SS, info.physical_sector.to_le_bytes().to_vec(), 6),
    ];
    let mut linkage_in_item = 0u64;
    if let Some(rel) = rel_parent {
        let kv = [("parent_linkage", guid_str(&info.data_write_guid)), ("relative_path", rel.to_string())];
        let mut loc = guid_bytes(PARENT_LOC_TYPE).to_vec();
        loc.extend_from_slice(&0u16.to_le_bytes());
        loc.extend_from_slice(&(kv.len() as u16).to_le_bytes());
        let mut strings = Vec::new();
        let base = 20 + kv.len() * 12;
        for (k, v) in &kv {
            let (kb, vb) = (utf16(k), utf16(v));
            let ko = base + strings.len();
            strings.extend_from_slice(&kb);
            let vo = base + strings.len();
            strings.extend_from_slice(&vb);
            if *k == "parent_linkage" {
                linkage_in_item = vo as u64;
            }
            loc.extend_from_slice(&(ko as u32).to_le_bytes());
            loc.extend_from_slice(&(vo as u32).to_le_bytes());
            loc.extend_from_slice(&(kb.len() as u16).to_le_bytes());
            loc.extend_from_slice(&(vb.len() as u16).to_le_bytes());
        }
        loc.extend_from_slice(&strings);
        items.push((PARENT_LOC, loc, 4));
    }
    let mut meta = vec![0u8; MB as usize];
    meta[0..8].copy_from_slice(b"metadata");
    meta[10..12].copy_from_slice(&(items.len() as u16).to_le_bytes());
    let mut data_off = KB64 as usize;
    let mut linkage_abs = 0u64;
    for (i, (id, data, flags)) in items.iter().enumerate() {
        let e = 32 + i * 32;
        meta[e..e + 16].copy_from_slice(&guid_bytes(id));
        meta[e + 16..e + 20].copy_from_slice(&(data_off as u32).to_le_bytes());
        meta[e + 20..e + 24].copy_from_slice(&(data.len() as u32).to_le_bytes());
        meta[e + 24..e + 28].copy_from_slice(&flags.to_le_bytes());
        meta[data_off..data_off + data.len()].copy_from_slice(data);
        if *id == PARENT_LOC {
            linkage_abs = meta_off + data_off as u64 + linkage_in_item;
        }
        data_off += data.len().div_ceil(8) * 8;
    }
    Layout { head, meta, bat_off, meta_off, linkage_abs }
}

/// Write an empty VHDX with the same size as `parent`. `rel_parent` Some → differencing pointing to the parent
/// (parent_linkage = parent DataWriteGuid, relative_path); None → plain dynamic (tests only).
/// Returns the absolute offset of the parent_linkage GUID string (UTF-16, 76 bytes) — the client stage patches
/// it there when the parent (base.vhdx) GUID is only known at run time.
pub fn write_empty(path: &str, parent: &Info, rel_parent: Option<&str>) -> Result<u64, String> {
    let l = layout(parent, CHILD_BLOCK, rel_parent, [rand_guid(), rand_guid(), rand_guid()]);
    // BAT + log = all zeros (sparse, set_len). Write the header area + metadata.
    let mut f = File::create(path).map_err(|e| format!("{path}: {e}"))?;
    f.set_len(l.meta_off + MB).map_err(|e| e.to_string())?;
    f.write_all(&l.head).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(l.meta_off)).map_err(|e| e.to_string())?;
    f.write_all(&l.meta).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    Ok(l.linkage_abs)
}

/// Block size of the golden (dynamic) VHDX — Hyper-V's default for dynamic disks.
const GOLDEN_BLOCK: u64 = 32 * MB;
/// BAT payload state: block fully present in the file.
const PAYLOAD_FULLY_PRESENT: u64 = 6;

/// The golden as a dynamic VHDX that is never written out: a VHDX of a raw disk is a few MB of header, BAT and
/// metadata followed by the raw disk's 32 MB blocks, byte for byte. So the header part is built here in memory and
/// every payload byte is read straight from the raw file when a client downloads it — no 50 GB copy on the server.
/// Blocks holding no data in the raw file (holes) are left out (BAT "not present" reads as zeros); the others are
/// stored in virtual order. The GUIDs come from `id` (hash::stamp of the raw file): the same raw gives the same bytes,
/// hence the same sha256, every time it is opened (a server restart doesn't make every client download again).
/// The raw file stays open: a new image.img renamed over it doesn't change what this golden serves.
pub struct Virtual {
    raw: crate::disk::Source,
    /// Bytes [0, payload start): file identifier, headers, region tables, log, BAT, metadata.
    prefix: Vec<u8>,
    /// Virtual block number of each stored payload block, in file order.
    blocks: Vec<u64>,
    pub info: Info,
}

impl Virtual {
    pub fn open(raw: &std::path::Path, id: &str) -> Result<Virtual, String> {
        let src = crate::disk::Source::file(raw)?;
        if src.len % 512 != 0 || src.len == 0 {
            return Err(format!("raw disk size {} is not a multiple of 512", src.len));
        }
        let guid = |tag: &str| -> [u8; 16] {
            let mut b: [u8; 16] = blake3::hash(format!("broom golden {id} {tag}").as_bytes()).as_bytes()[..16].try_into().unwrap();
            b[7] = (b[7] & 0x0f) | 0x40; // version 4 layout, like rand_guid
            b[8] = (b[8] & 0x3f) | 0x80;
            b
        };
        let ids = [guid("file"), guid("data"), guid("page83")];
        let info = Info { data_write_guid: ids[1], virtual_size: src.len, logical_sector: 512, physical_sector: 4096 };
        let l = layout(&info, GOLDEN_BLOCK, None, ids);
        let chunk = (1u64 << 23) * 512 / GOLDEN_BLOCK;
        let mut set = std::collections::BTreeSet::new();
        for (a, b) in src.data_ranges() {
            set.extend(a / GOLDEN_BLOCK..b.div_ceil(GOLDEN_BLOCK));
        }
        let blocks: Vec<u64> = set.into_iter().filter(|&i| i * GOLDEN_BLOCK < src.len).collect();
        let start = l.meta_off + MB;
        let mut prefix = vec![0u8; start as usize];
        prefix[..l.head.len()].copy_from_slice(&l.head);
        for (k, &i) in blocks.iter().enumerate() {
            let e = (((start + k as u64 * GOLDEN_BLOCK) / MB) << 20) | PAYLOAD_FULLY_PRESENT;
            let at = (l.bat_off + (i + i / chunk) * 8) as usize;
            prefix[at..at + 8].copy_from_slice(&e.to_le_bytes());
        }
        prefix[l.meta_off as usize..start as usize].copy_from_slice(&l.meta);
        Ok(Virtual { raw: src, prefix, blocks, info })
    }

    pub fn len(&self) -> u64 {
        self.prefix.len() as u64 + self.blocks.len() as u64 * GOLDEN_BLOCK
    }

    /// Bytes of the VHDX at `off` (inside it).
    pub fn read_at(&self, mut off: u64, mut buf: &mut [u8]) -> Result<(), String> {
        if off.checked_add(buf.len() as u64).is_none_or(|end| end > self.len()) {
            return Err("read past the end of the golden".into());
        }
        let p = self.prefix.len() as u64;
        while !buf.is_empty() {
            let n = if off < p {
                let n = ((p - off) as usize).min(buf.len());
                buf[..n].copy_from_slice(&self.prefix[off as usize..off as usize + n]);
                n
            } else {
                let (k, within) = ((off - p) / GOLDEN_BLOCK, (off - p) % GOLDEN_BLOCK);
                let n = ((GOLDEN_BLOCK - within) as usize).min(buf.len());
                // Past the raw disk's end (its last block is partial) Source reads zeros.
                self.raw.read_at(self.blocks[k as usize] * GOLDEN_BLOCK + within, &mut buf[..n])?;
                n
            };
            buf = &mut buf[n..];
            off += n as u64;
        }
        Ok(())
    }

    /// sha256 of the whole VHDX, lower-case hex (what the stage checks a download against). Reads the raw disk once.
    pub fn sha256(&self) -> Result<String, String> {
        use sha2::{Digest, Sha256};
        // The disk is the limit (sha2 uses SHA-NI): one thread reads ahead while this one hashes, so the time is the
        // read alone, not read + hash. Two 8 MB buffers go round between them.
        const PIECE: usize = 8 << 20;
        let len = self.len();
        std::thread::scope(|s| {
            let (full_tx, full_rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(1);
            let (free_tx, free_rx) = std::sync::mpsc::channel::<Vec<u8>>();
            for _ in 0..2 {
                free_tx.send(vec![0u8; PIECE]).unwrap();
            }
            s.spawn(move || {
                let mut off = 0u64;
                while off < len {
                    let Ok(mut buf) = free_rx.recv() else { return }; // the hasher stopped
                    let n = ((len - off) as usize).min(PIECE);
                    buf.truncate(n);
                    let r = self.read_at(off, &mut buf).map(|_| buf);
                    let failed = r.is_err();
                    if full_tx.send(r).is_err() || failed {
                        return;
                    }
                    off += n as u64;
                }
            });
            let mut h = Sha256::new();
            for buf in full_rx {
                let mut buf = buf?;
                h.update(&buf);
                buf.resize(PIECE, 0);
                let _ = free_tx.send(buf);
            }
            Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_vector() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn guid_roundtrip() {
        let s = "{2dc27766-f623-4200-9d64-115e9bfd4a08}";
        assert_eq!(guid_str(&guid_bytes(s)), s);
        // Windows layout: first 3 fields LE → first byte = 0x66.
        assert_eq!(guid_bytes(BAT_GUID)[0], 0x66);
    }

    /// Raw disk with data, holes and a partial last block → virtual dynamic VHDX → qemu-img (independent reader) reads
    /// back exactly the same bytes; holes take no block; blocks are stored in virtual order.
    #[test]
    fn dynamic_matches_qemu() {
        use std::os::unix::fs::FileExt;
        let d = std::env::temp_dir().join("broom_t_vhdx_dyn");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let raw = d.join("disk.raw");
        let f = File::create(&raw).unwrap();
        let len = 200 * MB + 512 * 3; // not a multiple of the 32 MB block
        f.set_len(len).unwrap();
        let mut seed = 0x1234_5678u32;
        let mut noise = |n: usize| -> Vec<u8> {
            (0..n).map(|_| { seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5; seed as u8 }).collect()
        };
        f.write_all_at(&noise(4096), 0).unwrap(); // block 0
        f.write_all_at(&noise(3 * MB as usize), 70 * MB).unwrap(); // block 2, spans 1MB pieces
        f.write_all_at(&noise(700), len - 700).unwrap(); // partial last block
        f.write_all_at(&vec![0u8; 4 * MB as usize], 130 * MB).unwrap(); // written zeros: data on disk → stored
        drop(f);
        let v = Virtual::open(&raw, "stamp-1").unwrap();
        // The virtual golden, written out once here only to check it with qemu-img.
        let out = d.join("g.vhdx");
        let mut whole = vec![0u8; v.len() as usize];
        v.read_at(0, &mut whole).unwrap();
        std::fs::write(&out, &whole).unwrap();
        let i = read_info(out.to_str().unwrap()).unwrap();
        assert_eq!((i.virtual_size, i.logical_sector, i.physical_sector), (len, 512, 4096));
        assert_eq!(i.data_write_guid, v.info.data_write_guid, "templates get the GUID the file really has");
        let q = |args: &[&str]| std::process::Command::new("qemu-img").args(args).status().unwrap().success();
        assert!(q(&["check", "-q", "-f", "vhdx", out.to_str().unwrap()]), "qemu-img check");
        let back = d.join("back.raw");
        assert!(q(&["convert", "-f", "vhdx", "-O", "raw", out.to_str().unwrap(), back.to_str().unwrap()]));
        assert!(std::fs::read(&back).unwrap() == std::fs::read(&raw).unwrap(), "same bytes");
        // 4 stored blocks (0, 2, 4, last) after header/log/BAT/metadata; holes (1, 3, 5) left out.
        assert_eq!(v.len(), 4 * MB + 4 * GOLDEN_BLOCK);
        // Any range reads the same bytes as the whole; the same raw + id → the same file (hash), another id → another.
        let mut part = vec![0u8; 5 * MB as usize];
        v.read_at(3 * MB + 7, &mut part).unwrap();
        assert!(part[..] == whole[(3 * MB + 7) as usize..(8 * MB + 7) as usize]);
        assert!(v.read_at(v.len() - 1, &mut [0u8; 2]).is_err());
        let sha = v.sha256().unwrap();
        assert_eq!(sha, crate::hash::file_hash(out.to_str().unwrap()).unwrap());
        assert_eq!(Virtual::open(&raw, "stamp-1").unwrap().sha256().unwrap(), sha, "stable across opens (server restart)");
        assert_ne!(Virtual::open(&raw, "stamp-2").unwrap().sha256().unwrap(), sha, "a new golden gets new GUIDs");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Manual timing vs qemu-img: BROOM_BENCH_RAW=/path/disk.raw cargo test --release bench_convert -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_convert() {
        let Ok(raw) = std::env::var("BROOM_BENCH_RAW") else { return };
        let _ = std::process::Command::new("sync").status();
        let out = format!("{raw}.broom.vhdx");
        let t = std::time::Instant::now();
        let sha = Virtual::open(std::path::Path::new(&raw), "bench").unwrap().sha256().unwrap();
        println!("broom vhdx (virtual: layout + sha256 {}): {:?}", &sha[..12], t.elapsed());
        let t = std::time::Instant::now();
        let q = format!("{raw}.qemu.vhdx");
        assert!(std::process::Command::new("qemu-img")
            .args(["convert", "-m", "16", "-O", "vhdx", "-o", "subformat=dynamic", &raw, &q])
            .status()
            .unwrap()
            .success());
        let _ = std::process::Command::new("sync").status(); // both sides pay the flush
        println!("qemu-img vhdx: {:?}", t.elapsed());
        let v = format!("{raw}.broom.vmdk");
        let t = std::time::Instant::now();
        crate::vmdk::write_sparse(&crate::disk::Source::file(std::path::Path::new(&raw)).unwrap(), std::path::Path::new(&v)).unwrap();
        let _ = std::process::Command::new("sync").status(); // both sides pay the flush
        println!("broom vmdk:    {:?}", t.elapsed());
        let back = format!("{raw}.back");
        let t = std::time::Instant::now();
        crate::vmdk::to_raw(std::path::Path::new(&v), std::path::Path::new(&back)).unwrap();
        let _ = std::process::Command::new("sync").status(); // both sides pay the flush
        println!("broom vmdk→raw:{:?}", t.elapsed());
        let t = std::time::Instant::now();
        assert!(std::process::Command::new("qemu-img")
            .args(["convert", "-m", "16", "-W", "-f", "vmdk", "-O", "raw", &v, &format!("{raw}.qback")])
            .status()
            .unwrap()
            .success());
        let _ = std::process::Command::new("sync").status(); // both sides pay the flush
        println!("qemu vmdk→raw: {:?}", t.elapsed());
        for f in [out, q, v, back, format!("{raw}.qback")] {
            let _ = std::fs::remove_file(f);
        }
    }

    /// Write a differencing file → reading back matches the parent parameters; parent_linkage sits at the returned offset.
    #[test]
    fn diff_roundtrip() {
        let p = std::env::temp_dir().join("broom_test_child.vhdx");
        let p = p.to_str().unwrap();
        let parent = Info {
            data_write_guid: guid_bytes("{11223344-5566-7788-99aa-bbccddeeff00}"),
            virtual_size: 80 * 1024 * MB,
            logical_sector: 512,
            physical_sector: 4096,
        };
        let off = write_empty(p, &parent, Some(".\\golden.vhdx")).unwrap();
        let i = read_info(p).unwrap();
        assert_eq!(i.virtual_size, parent.virtual_size);
        assert_eq!((i.logical_sector, i.physical_sector), (512, 4096));
        let mut f = File::open(p).unwrap();
        let raw = read_at(&mut f, off, 76).unwrap();
        let s: Vec<u16> = raw.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16(&s).unwrap(), "{11223344-5566-7788-99aa-bbccddeeff00}");
        // 80GB / 2MB = 40960 block, chunk 2048 → 20 SB → BAT 20*2049*8 B → 1MB region.
        assert_eq!(std::fs::metadata(p).unwrap().len(), 4 * MB);
        let _ = std::fs::remove_file(p);
    }
}
