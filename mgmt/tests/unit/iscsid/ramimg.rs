use super::*;
const GB: u64 = 1 << 30;

#[test]
fn reads_match_the_file() {
    use std::os::unix::fs::FileExt;
    let p = std::env::temp_dir().join("broom_test_ramimg.img");
    // A hole, data across a block boundary, an odd-sized tail.
    let size = 5 * BLOCK as u64 + 777;
    let f = std::fs::File::create(&p).unwrap();
    f.set_len(size).unwrap();
    let data: Vec<u8> = (0..40_000u32).map(|i| (i * 7 % 251) as u8).collect();
    f.write_all_at(&data, BLOCK as u64 * 2 - 100).unwrap();
    f.write_all_at(b"tail", size - 4).unwrap();
    let img = RamImage::load(p.to_str().unwrap(), 0).unwrap();
    assert_eq!(img.size(), size);
    assert!(
        img.index[0].1 == 0 && img.index[1].1 > 0,
        "zero block not stored"
    );
    let file = std::fs::read(&p).unwrap();
    for (off, len) in [
        (0, 512),
        (BLOCK as u64 * 2 - 300, 50_000),
        (size - 1000, 1000),
        (0, size as usize),
    ] {
        let mut b = vec![1u8; len];
        img.read_at(off, &mut b).unwrap();
        assert_eq!(b, &file[off as usize..off as usize + len], "@{off}+{len}");
    }
    assert!(img.read_at(size, &mut [0u8; 1]).is_err());
    let _ = std::fs::remove_file(p);
}

#[test]
fn claims_only_what_data_can_need() {
    use std::os::unix::fs::FileExt;
    // 1 GB virtual, 2 data extents (one straddling a block edge): only their blocks count.
    let p = std::env::temp_dir().join("broom_test_ramimg_sparse.img");
    let f = std::fs::File::create(&p).unwrap();
    f.set_len(GB).unwrap();
    f.write_all_at(&[1u8; 8192], 0).unwrap();
    f.write_all_at(&[2u8; 100], 512 << 20).unwrap();
    let blocks = data_blocks(p.to_str().unwrap());
    assert!(
        (2..=4).contains(&blocks),
        "{blocks} blocks (fs block size may round extents up)"
    );
    // Far more than any test box has: refused up front, nothing claimed.
    let e = RamImage::load(p.to_str().unwrap(), 1 << 60).err().unwrap();
    assert!(e.contains("not enough RAM"), "{e}");
    let img = RamImage::load(p.to_str().unwrap(), 0).unwrap();
    assert!(
        img.arena.len <= 64 << 10,
        "tail given back: {} bytes kept",
        img.arena.len
    );
    let mut b = [0u8; 4];
    img.read_at((512 << 20) + 98, &mut b).unwrap();
    assert_eq!(b, [2, 2, 0, 0]);
    let _ = std::fs::remove_file(p);
}

#[test]
fn cache_is_bounded() {
    let p = std::env::temp_dir().join("broom_test_ramimg_cache.img");
    let data: Vec<u8> = (0..CACHE_BLOCKS + 10)
        .flat_map(|i| [i as u8 | 1; BLOCK])
        .collect();
    std::fs::write(&p, &data).unwrap();
    let img = RamImage::load(p.to_str().unwrap(), 0).unwrap();
    let mut b = [0u8; 1];
    for i in 0..CACHE_BLOCKS + 10 {
        img.read_at((i * BLOCK) as u64, &mut b).unwrap();
        assert_eq!(b[0], i as u8 | 1);
    }
    let c = img.cache.lock();
    assert_eq!((c.0.len(), c.1.len()), (CACHE_BLOCKS, CACHE_BLOCKS));
    let _ = std::fs::remove_file(p);
}

#[test]
fn incompressible_blocks_stored_as_is() {
    // Random bytes (zstd can't shrink them) with an odd tail, then a compressible block.
    let p = std::env::temp_dir().join("broom_test_ramimg_raw.img");
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut data: Vec<u8> = (0..3 * BLOCK + 1000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    data.extend(std::iter::repeat_n(7u8, BLOCK - 1000));
    data.extend(std::iter::repeat_n(9u8, 500)); // odd-sized compressible tail
    std::fs::write(&p, &data).unwrap();
    let img = RamImage::load(p.to_str().unwrap(), 0).unwrap();
    assert!(
        img.index[..3].iter().all(|b| b.1 as usize == BLOCK),
        "random → as is"
    );
    assert!((img.index[4].1 as usize) < BLOCK, "compressible → zstd");
    for (off, len) in [(0, data.len()), (BLOCK - 10, 30), (3 * BLOCK + 990, 20)] {
        let mut b = vec![0u8; len];
        img.read_at(off as u64, &mut b).unwrap();
        assert_eq!(b, &data[off..off + len], "@{off}+{len}");
    }
    let _ = std::fs::remove_file(p);
}

#[test]
fn fits_logic() {
    assert!(fits(4 * GB, 8 * GB, 2 * GB));
    assert!(!fits(40 * GB, 16 * GB, 2 * GB));
    assert!(fits(6 * GB, 8 * GB, 2 * GB), "exactly at the limit");
    assert!(!fits(6 * GB + 1, 8 * GB, 2 * GB));
    assert!(fits(999 * GB, 0, 2 * GB), "meminfo unreadable → allowed");
}
