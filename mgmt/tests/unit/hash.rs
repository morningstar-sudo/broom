#[test]
fn sha256_known_vector() {
    let p = std::env::temp_dir().join("broom_test_sha.txt");
    std::fs::write(&p, "abc").unwrap();
    assert_eq!(
        super::file_hash(p.to_str().unwrap()).unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let _ = std::fs::remove_file(p);
}

#[test]
fn cached_hash_follows_size_and_mtime() {
    let (p, c) = (
        std::env::temp_dir().join("broom_test_shac.img"),
        std::env::temp_dir().join("broom_test_shac.sha256"),
    );
    let _ = std::fs::remove_file(&c);
    std::fs::write(&p, "abc").unwrap();
    let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(
        super::file_hash_cached(&p, &c).unwrap(),
        abc,
        "no cache → hashed"
    );
    // Same stamp → the cached value is returned without reading the file.
    let stamp = super::stamp(&p).unwrap();
    assert_eq!(
        std::fs::read_to_string(&c).unwrap(),
        format!("{stamp}\t{abc}\n")
    );
    std::fs::write(&c, format!("{stamp}\t{}\n", "0".repeat(64))).unwrap();
    assert_eq!(super::file_hash_cached(&p, &c).unwrap(), "0".repeat(64));
    // File changed (size + mtime) → hashed again.
    std::fs::write(&p, "abcd").unwrap();
    assert_eq!(
        super::file_hash_cached(&p, &c).unwrap(),
        "88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589"
    );
    let _ = (std::fs::remove_file(p), std::fs::remove_file(c));
}

#[test]
fn cached_hash_keeps_earlier_stamps() {
    let (p, c) = (
        std::env::temp_dir().join("broom_test_shav.img"),
        std::env::temp_dir().join("broom_test_shav.sha256"),
    );
    let _ = std::fs::remove_file(&c);
    std::fs::write(&p, "abc").unwrap();
    let (s1, t1) = (
        super::stamp(&p).unwrap(),
        std::fs::metadata(&p).unwrap().modified().unwrap(),
    );
    super::file_hash_cached(&p, &c).unwrap();
    std::fs::write(&p, "abcd").unwrap();
    super::file_hash_cached(&p, &c).unwrap();
    assert_eq!(std::fs::read_to_string(&c).unwrap().lines().count(), 2);
    // v1 back (same size + mtime = same stamp): its hash comes from the cache, the file is not read.
    let fake = "1".repeat(64);
    let text = std::fs::read_to_string(&c)
        .unwrap()
        .lines()
        .map(|l| match l.split_once('\t') {
            Some((s, _)) if s == s1 => format!("{s}\t{fake}\n"),
            _ => format!("{l}\n"),
        })
        .collect::<String>();
    std::fs::write(&c, text).unwrap();
    std::fs::write(&p, "abc").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(t1)
        .unwrap();
    assert_eq!(super::file_hash_cached(&p, &c).unwrap(), fake);
    let _ = (std::fs::remove_file(p), std::fs::remove_file(c));
}

#[test]
fn pipelined_sha256_matches_one_pass() {
    use sha2::{Digest, Sha256};
    // Not a multiple of the 8 MB piece: the last piece is short.
    let data: Vec<u8> = (0..(17u32 << 20) + 5).map(|i| (i % 253) as u8).collect();
    let want: String = Sha256::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let got = super::sha256_with(data.len() as u64, |off, buf| {
        buf.copy_from_slice(&data[off as usize..off as usize + buf.len()]);
        Ok(())
    })
    .unwrap();
    assert_eq!(got, want);
    assert!(super::sha256_with(9, |_, _| Err("disk gone".into())).is_err());
}
