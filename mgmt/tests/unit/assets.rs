use super::*;

/// Every listed asset is there; a zip of them unpacks to the same bytes; the committed pin = local build.
#[test]
fn pack_and_unzip() {
    assert!(NAMES.iter().all(|n| !bytes(n).is_empty()));
    assert!(text("scripts/stage.sh").starts_with("#!"));
    let p = std::env::temp_dir().join("broom_test_assets.zip");
    let mut z = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
    for n in NAMES {
        z.start_file(*n, zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut z, bytes(n)).unwrap();
    }
    z.finish().unwrap();
    let m = unzip(&std::fs::read(&p).unwrap()).unwrap();
    assert!(NAMES.iter().all(|n| m[*n] == bytes(n)));
    assert!(unzip(b"not a zip").is_err());
    assert_eq!(
        parse_pin(PIN),
        None,
        "committed assets.pin = local build, no pin"
    );
    let _ = std::fs::remove_file(p);
}
