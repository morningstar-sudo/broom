// vhdx.rs — minimal VHDX (MS-VHDX v1.0) reader/writer for Windows native boot.
// Read: disk parameters + DataWriteGuid of the golden (created by qemu-img). Write: an empty DIFFERENCING file
// (BAT all "not present" → every read falls through to the parent) pointing to its parent via parent_linkage + relative_path.
// No Linux tool can create a differencing VHDX (qemu-img doesn't support it) → written by hand.
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

fn u16le(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
fn u32le(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
fn u64le(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

fn read_at(f: &mut File, off: u64, len: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; len];
    f.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
    f.read_exact(&mut buf).map_err(|e| format!("read VHDX @{off}: {e}"))?;
    Ok(buf)
}

/// Verify the checksum of a block whose crc field is at bytes 4..8.
fn crc_ok(block: &[u8]) -> bool {
    let mut t = block.to_vec();
    let want = u32le(&t, 4);
    t[4..8].fill(0);
    crc32c(&t) == want
}

/// Read parameters + DataWriteGuid (current header = highest valid seq).
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

/// Write an empty VHDX with the same size as `parent`. `rel_parent` Some → differencing pointing to the parent
/// (parent_linkage = parent DataWriteGuid, relative_path); None → plain dynamic (tests only).
/// Returns the absolute offset of the parent_linkage GUID string (UTF-16, 76 bytes) — the client stage patches
/// it there when the parent (base.vhdx) GUID is only known at run time.
pub fn write_empty(path: &str, parent: &Info, rel_parent: Option<&str>) -> Result<u64, String> {
    let ls = parent.logical_sector as u64;
    let chunk = (1u64 << 23) * ls / CHILD_BLOCK;
    let data_blocks = parent.virtual_size.div_ceil(CHILD_BLOCK);
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
    let (fw, dw) = (rand_guid(), rand_guid());
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
            let mut v = (CHILD_BLOCK as u32).to_le_bytes().to_vec();
            v.extend_from_slice(&(if rel_parent.is_some() { 2u32 } else { 0 }).to_le_bytes()); // HasParent
            v
        }, 4), // IsRequired
        (VDISK_SIZE, parent.virtual_size.to_le_bytes().to_vec(), 6), // IsVirtualDisk|IsRequired
        (PAGE83, rand_guid().to_vec(), 6),
        (LOGICAL_SS, parent.logical_sector.to_le_bytes().to_vec(), 6),
        (PHYSICAL_SS, parent.physical_sector.to_le_bytes().to_vec(), 6),
    ];
    let mut linkage_in_item = 0u64;
    if let Some(rel) = rel_parent {
        let kv = [("parent_linkage", guid_str(&parent.data_write_guid)), ("relative_path", rel.to_string())];
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

    // BAT + log = all zeros (sparse, set_len). Write the header area + metadata.
    let mut f = File::create(path).map_err(|e| format!("{path}: {e}"))?;
    f.set_len(meta_off + MB).map_err(|e| e.to_string())?;
    f.write_all(&head).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(meta_off)).map_err(|e| e.to_string())?;
    f.write_all(&meta).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    Ok(linkage_abs)
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
