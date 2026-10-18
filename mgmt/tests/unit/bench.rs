use super::*;

#[test]
fn reads_and_picks_largest() {
    // Under target/ (a real disk): /tmp may be tmpfs, which refuses O_DIRECT.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("bench_test");
    let _ = std::fs::remove_dir_all(&dir);
    for (n, mb) in [("small", 1), ("big", 8)] {
        std::fs::create_dir_all(dir.join(n)).unwrap();
        std::fs::write(dir.join(n).join("image.img"), vec![7u8; mb << 20]).unwrap();
    }
    let (name, path) = largest_image(&dir).unwrap();
    assert_eq!(name, "big");
    assert!(seq_read(&path, SEQ_BYTES, Duration::from_secs(2)).unwrap() > 0.0);
    assert!(rand_read(&path, 4, Duration::from_millis(200)).unwrap() > 0.0);
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(largest_image(&dir).is_none());
}
