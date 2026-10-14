use super::*;

/// A zip at `p` of every listed asset, plus `extra` entries.
fn zip_of(p: &std::path::Path, extra: &[(&str, &[u8])]) -> HashMap<String, &'static [u8]> {
    let mut z = zip::ZipWriter::new(std::fs::File::create(p).unwrap());
    let all = NAMES
        .iter()
        .map(|n| (*n, bytes(n)))
        .chain(extra.iter().copied());
    for (n, b) in all {
        z.start_file(n, zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut z, b).unwrap();
    }
    z.finish().unwrap();
    let m = unzip(&std::fs::read(p).unwrap()).unwrap();
    let _ = std::fs::remove_file(p);
    m
}

/// Every listed asset is there; a zip of them unpacks to the same bytes; the committed pin = local build.
#[test]
fn pack_and_unzip() {
    assert!(NAMES.iter().all(|n| !bytes(n).is_empty()));
    assert!(text("scripts/stage.sh").starts_with("#!"));
    let p = std::env::temp_dir().join("broom_test_assets.zip");
    // A local build's zip: no optional asset.
    let m = zip_of(&p, &[]);
    assert!(NAMES.iter().all(|n| m[*n] == bytes(n)));
    assert!(!m.contains_key("scripts/broom-fw.dll"));
    // A release's: + broom-fw.dll (CI).
    let m = zip_of(&p, &[("scripts/broom-fw.dll", b"MZ dll")]);
    assert_eq!(m["scripts/broom-fw.dll"], b"MZ dll");
    assert!(unzip(b"not a zip").is_err());
    assert_eq!(
        parse_pin(PIN),
        None,
        "committed assets.pin = local build, no pin"
    );
    // Local build: none shipped.
    assert_eq!(optional("scripts/broom-fw.dll"), None);
}
