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
        (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect()
    };
    f.write_all_at(&noise(4096), 0).unwrap(); // block 0
    f.write_all_at(&noise(3 * MB as usize), 70 * MB).unwrap(); // block 2, spans 1MB pieces
    f.write_all_at(&noise(700), len - 700).unwrap(); // partial last block
    f.write_all_at(&vec![0u8; 4 * MB as usize], 130 * MB)
        .unwrap(); // written zeros: data on disk → stored
    drop(f);
    let v = Virtual::open(&raw, "stamp-1").unwrap();
    // The virtual golden, written out once here only to check it with qemu-img.
    let out = d.join("g.vhdx");
    let mut whole = vec![0u8; v.len() as usize];
    v.read_at(0, &mut whole).unwrap();
    std::fs::write(&out, &whole).unwrap();
    let i = read_info(out.to_str().unwrap()).unwrap();
    assert_eq!(
        (i.virtual_size, i.logical_sector, i.physical_sector),
        (len, 512, 4096)
    );
    assert_eq!(
        i.data_write_guid, v.info.data_write_guid,
        "templates get the GUID the file really has"
    );
    let q = |args: &[&str]| {
        std::process::Command::new("qemu-img")
            .args(args)
            .status()
            .unwrap()
            .success()
    };
    assert!(
        q(&["check", "-q", "-f", "vhdx", out.to_str().unwrap()]),
        "qemu-img check"
    );
    let back = d.join("back.raw");
    assert!(q(&[
        "convert",
        "-f",
        "vhdx",
        "-O",
        "raw",
        out.to_str().unwrap(),
        back.to_str().unwrap()
    ]));
    assert!(
        std::fs::read(&back).unwrap() == std::fs::read(&raw).unwrap(),
        "same bytes"
    );
    // 4 stored blocks (0, 2, 4, last) after header/log/BAT/metadata; holes (1, 3, 5) left out.
    assert_eq!(v.len(), 4 * MB + 4 * GOLDEN_BLOCK);
    // Any range reads the same bytes as the whole; the same raw + id → the same file (hash), another id → another.
    let mut part = vec![0u8; 5 * MB as usize];
    v.read_at(3 * MB + 7, &mut part).unwrap();
    assert!(part[..] == whole[(3 * MB + 7) as usize..(8 * MB + 7) as usize]);
    assert!(v.read_at(v.len() - 1, &mut [0u8; 2]).is_err());
    let sha = v.sha256().unwrap();
    assert_eq!(sha, crate::hash::file_hash(out.to_str().unwrap()).unwrap());
    assert_eq!(
        Virtual::open(&raw, "stamp-1").unwrap().sha256().unwrap(),
        sha,
        "stable across opens (server restart)"
    );
    assert_ne!(
        Virtual::open(&raw, "stamp-2").unwrap().sha256().unwrap(),
        sha,
        "a new golden gets new GUIDs"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// Manual timing vs qemu-img: BROOM_BENCH_RAW=/path/disk.raw cargo test --release bench_convert -- --ignored --nocapture
#[test]
#[ignore]
fn bench_convert() {
    let Ok(raw) = std::env::var("BROOM_BENCH_RAW") else {
        return;
    };
    let _ = std::process::Command::new("sync").status();
    let out = format!("{raw}.broom.vhdx");
    let t = std::time::Instant::now();
    let sha = Virtual::open(std::path::Path::new(&raw), "bench")
        .unwrap()
        .sha256()
        .unwrap();
    println!(
        "broom vhdx (virtual: layout + sha256 {}): {:?}",
        &sha[..12],
        t.elapsed()
    );
    let t = std::time::Instant::now();
    let q = format!("{raw}.qemu.vhdx");
    assert!(
        std::process::Command::new("qemu-img")
            .args([
                "convert",
                "-m",
                "16",
                "-O",
                "vhdx",
                "-o",
                "subformat=dynamic",
                &raw,
                &q
            ])
            .status()
            .unwrap()
            .success()
    );
    let _ = std::process::Command::new("sync").status(); // both sides pay the flush
    println!("qemu-img vhdx: {:?}", t.elapsed());
    let v = format!("{raw}.broom.vmdk");
    let t = std::time::Instant::now();
    crate::vmdk::write_sparse(
        &crate::disk::Source::file(std::path::Path::new(&raw)).unwrap(),
        std::path::Path::new(&v),
    )
    .unwrap();
    let _ = std::process::Command::new("sync").status(); // both sides pay the flush
    println!("broom vmdk:    {:?}", t.elapsed());
    let back = format!("{raw}.back");
    let t = std::time::Instant::now();
    crate::vmdk::to_raw(std::path::Path::new(&v), std::path::Path::new(&back)).unwrap();
    let _ = std::process::Command::new("sync").status(); // both sides pay the flush
    println!("broom vmdk→raw:{:?}", t.elapsed());
    let t = std::time::Instant::now();
    assert!(
        std::process::Command::new("qemu-img")
            .args([
                "convert",
                "-m",
                "16",
                "-W",
                "-f",
                "vmdk",
                "-O",
                "raw",
                &v,
                &format!("{raw}.qback")
            ])
            .status()
            .unwrap()
            .success()
    );
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
    let off = write_empty(p, &parent, ".\\golden.vhdx").unwrap();
    let i = read_info(p).unwrap();
    assert_eq!(i.virtual_size, parent.virtual_size);
    assert_eq!((i.logical_sector, i.physical_sector), (512, 4096));
    let mut f = File::open(p).unwrap();
    let raw = read_at(&mut f, off, 76).unwrap();
    let s: Vec<u16> = raw
        .chunks(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    assert_eq!(
        String::from_utf16(&s).unwrap(),
        "{11223344-5566-7788-99aa-bbccddeeff00}"
    );
    // 80GB / 2MB = 40960 block, chunk 2048 → 20 SB → BAT 20*2049*8 B → 1MB region.
    assert_eq!(std::fs::metadata(p).unwrap().len(), 4 * MB);
    let _ = std::fs::remove_file(p);
}

// The VHDX reader: the stage reads goldens on the clients; here only the tests check what was written.
fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn read_at(f: &mut File, off: u64, len: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; len];
    f.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
    f.read_exact(&mut buf)
        .map_err(|e| format!("read VHDX @{off}: {e}"))?;
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
pub(crate) fn read_info(path: &str) -> Result<Info, String> {
    let mut f = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    if read_at(&mut f, 0, 8)? != b"vhdxfile" {
        return Err(format!("{path}: not a VHDX"));
    }
    let mut hdr: Option<Vec<u8>> = None;
    for off in [KB64, 2 * KB64] {
        let h = read_at(&mut f, off, 4096)?;
        if &h[0..4] == b"head"
            && crc_ok(&h)
            && hdr.as_ref().is_none_or(|c| u64le(&h, 8) > u64le(c, 8))
        {
            hdr = Some(h);
        }
    }
    let hdr = hdr.ok_or(format!("{path}: corrupt VHDX header"))?;
    if hdr[48..64].iter().any(|&x| x != 0) {
        return Err(format!(
            "{path}: VHDX has an unreplayed log (unclean shutdown?)"
        ));
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
