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
    let stamp = std::fs::read_to_string(&c)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    std::fs::write(&c, format!("{stamp}\n{}", "0".repeat(64))).unwrap();
    assert_eq!(super::file_hash_cached(&p, &c).unwrap(), "0".repeat(64));
    // File changed (size + mtime) → hashed again.
    std::fs::write(&p, "abcd").unwrap();
    assert_eq!(
        super::file_hash_cached(&p, &c).unwrap(),
        "88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589"
    );
    let _ = (std::fs::remove_file(p), std::fs::remove_file(c));
}
