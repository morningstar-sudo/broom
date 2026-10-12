use super::*;

fn base_of(data: &[u8]) -> impl Fn(u64, &mut [u8]) -> Result<(), String> + '_ {
    move |off, buf| {
        buf.copy_from_slice(&data[off as usize..off as usize + buf.len()]);
        Ok(())
    }
}

#[test]
fn reads_see_writes_over_the_disk() {
    let d = std::env::temp_dir().join("broom_test_overlay");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let size = 5 * BLOCK + 1000; // a partial last block
    let base: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let b = base_of(&base);
    let o = Overlay::open(&d.join("u.ovl"), &d.join("u.blocks"), size).unwrap();
    // Unaligned write across a block boundary: two blocks taken, the rest of each copied from the disk.
    o.write_at(&b, BLOCK - 10, &[7u8; 30]).unwrap();
    o.write_at(&b, BLOCK + 100, &[9u8; 4]).unwrap();
    assert_eq!(o.used(), 2 * BLOCK);
    let mut want = base.clone();
    want[(BLOCK - 10) as usize..(BLOCK + 20) as usize].fill(7);
    want[(BLOCK + 100) as usize..(BLOCK + 104) as usize].fill(9);
    let mut got = vec![0u8; size as usize];
    o.read_at(&b, 0, &mut got).unwrap();
    assert!(got == want);
    o.write_at(&b, size - 1, &[3]).unwrap(); // the partial last block
    assert!(o.write_at(&b, size - 1, &[1, 2]).is_err(), "past the end");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn survives_reopen_and_merges() {
    let d = std::env::temp_dir().join("broom_test_overlay_kept");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let size = 4 * BLOCK;
    let base_path = d.join("games.img");
    std::fs::write(&base_path, vec![1u8; size as usize]).unwrap();
    let base = std::fs::read(&base_path).unwrap();
    let (ovl, list) = (d.join("update.ovl"), d.join("update.blocks"));
    let o = Overlay::open(&ovl, &list, size).unwrap();
    o.write_at(&base_of(&base), 2 * BLOCK + 5, b"game").unwrap();
    o.flush().unwrap();
    drop(o);
    // Reopened (daemon restart): the block is still the overlay's.
    let o = Overlay::open(&ovl, &list, size).unwrap();
    let mut buf = [0u8; 4];
    o.read_at(&base_of(&base), 2 * BLOCK + 5, &mut buf).unwrap();
    assert_eq!(&buf, b"game");
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&base_path)
        .unwrap();
    assert_eq!(o.merge_into(&f).unwrap(), 1);
    let merged = std::fs::read(&base_path).unwrap();
    assert_eq!(
        &merged[(2 * BLOCK + 5) as usize..(2 * BLOCK + 9) as usize],
        b"game"
    );
    assert_eq!(
        merged.iter().filter(|&&x| x != 1).count(),
        4,
        "only those bytes changed"
    );
    let _ = std::fs::remove_dir_all(&d);
}
