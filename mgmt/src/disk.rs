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

fn rd(f: &File, off: u64, len: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; len];
    f.read_exact_at(&mut b, off).map_err(|e| format!("read at {off}: {e}"))?;
    Ok(b)
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// GPT's mixed-endian GUID bytes → `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`.
pub fn guid_str(g: &[u8]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        u32le(g, 0),
        u16::from_le_bytes([g[4], g[5]]),
        u16::from_le_bytes([g[6], g[7]]),
        g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]
    )
}

/// `XXXXXXXX-…` → GPT bytes.
fn guid_bytes(s: &str) -> Option<[u8; 16]> {
    let h: String = s.chars().filter(|c| *c != '-').collect();
    if h.len() != 32 {
        return None;
    }
    let b: Vec<u8> = (0..16).map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()).collect::<Option<_>>()?;
    let mut g = [0u8; 16];
    g[..4].copy_from_slice(&[b[3], b[2], b[1], b[0]]);
    g[4..6].copy_from_slice(&[b[5], b[4]]);
    g[6..8].copy_from_slice(&[b[7], b[6]]);
    g[8..].copy_from_slice(&b[8..]);
    Some(g)
}

/// The partition table of a disk image; None = no table (a bare filesystem / empty disk).
pub fn read(path: &str) -> Result<Option<Table>, String> {
    let f = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    if len < 1024 {
        return Ok(None);
    }
    for ss in [512u64, 4096] {
        if len >= ss * 2 {
            if let Some(t) = read_gpt(&f, ss)? {
                return Ok(Some(t));
            }
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
    let (lba, n, esize) = (u64le(&h, 72), u32le(&h, 80) as usize, u32le(&h, 84) as usize);
    if !(128..=4096).contains(&esize) || n > 1024 {
        return Err(format!("GPT: unexpected entry table ({n} × {esize} bytes)"));
    }
    let ents = rd(f, lba * ss, n * esize)?;
    let parts = ents
        .chunks(esize)
        .filter(|e| e[..16].iter().any(|&b| b != 0))
        .map(|e| {
            let (first, last) = (u64le(e, 32), u64le(e, 40));
            Part { start: first * ss, size: (last + 1).saturating_sub(first) * ss, kind: guid_str(&e[..16]) }
        })
        .collect();
    Ok(Some(Table { gpt: true, sector: ss, parts }))
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
    let prim: Vec<(u8, u64, u64)> = (0..4).map(|i| entry(&m, i)).filter(|(t, _, n)| *t != 0 && *n > 0).collect();
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
                    parts.push(Part { start: (ebr + ls) * 512, size: ln * 512, kind: format!("{lt:x}") });
                }
                let (nt, ns, _) = entry(&b, 1);
                if nt == 0 || ns == 0 {
                    break;
                }
                ebr = s + ns;
            }
        } else {
            parts.push(Part { start: s * 512, size: n * 512, kind: format!("{t:x}") });
        }
    }
    parts.sort_by_key(|p| p.start);
    Ok(Some(Table { gpt: false, sector: 512, parts }))
}

/// Rewrite `path` (a GPT disk image, 512-byte sectors) so it keeps ONE basic-data partition [start, start+size)
/// — what a published Windows golden needs (standard native VHD boot). Nothing outside the tables is touched.
/// The disk GUID and the partition's own GUID/name/attributes are kept when the old table had that partition
/// (Windows' drive-letter mapping uses the partition GUID), so republishing gives the same bytes.
pub fn write_single_gpt(path: &str, start: u64, size: u64) -> Result<(), String> {
    const SS: u64 = 512;
    if start % SS != 0 || size % SS != 0 || size == 0 {
        return Err(format!("partition {start}+{size} is not sector-aligned"));
    }
    let f = std::fs::OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("{path}: {e}"))?;
    let total = f.metadata().map_err(|e| e.to_string())?.len() / SS;
    let (first, last) = (start / SS, (start + size) / SS - 1);
    if first < 34 || last + 34 > total {
        return Err(format!("partition {start}+{size} does not fit a GPT on a {}-byte disk", total * SS));
    }
    // Old table (if GPT): keep disk GUID + the matching entry's identity.
    let mut disk_guid = *blake3::hash(format!("broom-disk {path} {total}").as_bytes()).as_bytes();
    let mut entry = [0u8; 128];
    entry[16..32].copy_from_slice(&blake3::hash(format!("broom-part {start} {size}").as_bytes()).as_bytes()[..16]);
    if let Ok(h) = rd(&f, SS, 92) {
        if &h[..8] == b"EFI PART" {
            disk_guid[..16].copy_from_slice(&h[56..72]);
            let (lba, n, esize) = (u64le(&h, 72), (u32le(&h, 80) as usize).min(1024), u32le(&h, 84) as usize);
            if (128..=4096).contains(&esize) {
                if let Ok(ents) = rd(&f, lba * SS, n * esize) {
                    if let Some(e) = ents.chunks(esize).find(|e| u64le(e, 32) == first && e[..16].iter().any(|&b| b != 0)) {
                        entry.copy_from_slice(&e[..128]);
                    }
                }
            }
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
    mbr[446 + 12..446 + 16].copy_from_slice(&((total - 1).min(u32::MAX as u64) as u32).to_le_bytes());
    mbr[510..].copy_from_slice(&[0x55, 0xAA]);
    let w = |off: u64, b: &[u8]| f.write_all_at(b, off).map_err(|e| format!("write {path} at {off}: {e}"));
    w(0, &mbr)?;
    w(SS, &header(1, total - 1, 2))?;
    w(2 * SS, &ents)?;
    w((total - 33) * SS, &ents)?;
    w((total - 1) * SS, &header(total - 1, 1, total - 33))?;
    f.sync_all().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sfdisk(path: &str, script: &str) {
        use std::io::Write;
        let mut c = Command::new("sfdisk").args(["-q", "--no-reread", path]).stdin(std::process::Stdio::piped()).spawn().unwrap();
        c.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
        assert!(c.wait().unwrap().success(), "sfdisk {script}");
    }

    /// What `sfdisk -J` reports: (start, size, type) in bytes.
    fn sfdisk_json(path: &str) -> (bool, Vec<(u64, u64, String)>) {
        let o = Command::new("sfdisk").args(["-J", path]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
        let t = &v["partitiontable"];
        let ss = t["sectorsize"].as_u64().unwrap_or(512);
        let parts = t["partitions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| !matches!(p["type"].as_str(), Some("5" | "f" | "85")))
            .map(|p| (p["start"].as_u64().unwrap() * ss, p["size"].as_u64().unwrap() * ss, p["type"].as_str().unwrap().to_uppercase()))
            .collect();
        (t["label"] == "gpt", parts)
    }

    fn mine(path: &str) -> (bool, Vec<(u64, u64, String)>) {
        let t = read(path).unwrap().unwrap();
        (t.gpt, t.parts.into_iter().map(|p| (p.start, p.size, p.kind.to_uppercase())).collect())
    }

    fn disk(name: &str, mb: u64) -> String {
        let p = std::env::temp_dir().join(format!("broom_t_disk_{name}.img"));
        let _ = std::fs::remove_file(&p);
        File::create(&p).unwrap().set_len(mb << 20).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn reads_like_sfdisk() {
        let g = disk("gpt", 64);
        sfdisk(&g, &format!("label: gpt\nsize=8MiB, type={ESP}\nsize=1MiB, type=E3C9E316-0B5C-4DB8-817D-F92DF00215AE\ntype={BASIC_DATA}\n"));
        assert_eq!(mine(&g), sfdisk_json(&g));
        assert_eq!(read(&g).unwrap().unwrap().parts.len(), 3);
        let m = disk("mbr", 64);
        sfdisk(&m, "label: dos\nsize=8MiB, type=7, bootable\nsize=20MiB, type=83\ntype=5\nsize=10MiB, type=82\ntype=8e\n");
        let (gpt, parts) = mine(&m);
        assert!(!gpt && parts.len() == 4, "{parts:?}");
        assert_eq!((gpt, parts), sfdisk_json(&m), "primaries + logicals, extended container left out");
        assert_eq!(read(&disk("empty", 4)).unwrap(), None, "no table");
        for p in [g, m] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn single_gpt_keeps_the_windows_partition() {
        let g = disk("single", 64);
        sfdisk(&g, &format!("label: gpt\nsize=8MiB, type={ESP}\nsize=1MiB, type=E3C9E316-0B5C-4DB8-817D-F92DF00215AE\nsize=40MiB, type={BASIC_DATA}, name=\"Basic data partition\"\n"));
        let win = read(&g).unwrap().unwrap().parts[2].clone();
        let uuid = |p: &str| String::from_utf8(Command::new("sfdisk").args(["--part-uuid", p, "3"]).output().unwrap().stdout).unwrap();
        let before = uuid(&g);
        File::options().write(true).open(&g).unwrap().write_all_at(b"NTFS-DATA", win.start + 3).unwrap();
        write_single_gpt(&g, win.start, win.size).unwrap();
        let (gpt, parts) = sfdisk_json(&g);
        assert!(gpt);
        assert_eq!(parts, vec![(win.start, win.size, BASIC_DATA.to_string())]);
        assert!(Command::new("sfdisk").args(["--verify", &g]).status().unwrap().success(), "sfdisk accepts both tables");
        let after = String::from_utf8(Command::new("sfdisk").args(["--part-uuid", &g, "1"]).output().unwrap().stdout).unwrap();
        assert_eq!(after, before, "partition GUID kept");
        let mut b = [0u8; 9];
        File::open(&g).unwrap().read_exact_at(&mut b, win.start + 3).unwrap();
        assert_eq!(&b, b"NTFS-DATA", "partition contents untouched");
        let first = std::fs::read(&g).unwrap();
        write_single_gpt(&g, win.start, win.size).unwrap();
        assert!(std::fs::read(&g).unwrap() == first, "same input → same bytes");
        let _ = std::fs::remove_file(g);
    }

    #[test]
    fn guid_roundtrip() {
        let g = guid_bytes(BASIC_DATA).unwrap();
        assert_eq!(guid_str(&g), BASIC_DATA);
        assert_eq!(&g[..4], &[0xA2, 0xA0, 0xD0, 0xEB]);
    }
}
