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

/// CI's pins appended to the release binary: found at its very end, a malformed line or a binary without them = none.
#[test]
fn pins_appended_to_the_binary() {
    let (a, s) = ("a".repeat(64), "b".repeat(64));
    let mut bin = b"\x7fELF...code \n#broom-pins\n(a string in the code)".to_vec();
    bin.extend_from_slice(
        format!("\n#broom-pins\nstage {s} https://x/broom-stage.tar.gz\nassets {a} https://x/broom-assets.zip\nbad line\n")
            .as_bytes(),
    );
    let p = parse_trailer(&bin);
    assert_eq!(p.len(), 2, "only the last trailer, only well-formed lines");
    assert_eq!(p["stage"], format!("{s}\nhttps://x/broom-stage.tar.gz"));
    assert_eq!(
        parse_pin(&p["assets"]),
        Some((a.as_str(), Some("https://x/broom-assets.zip")))
    );
    assert!(parse_trailer(b"\x7fELF no pins").is_empty());
    // The committed pin files are comments only (a local build) and no trailer in a test binary.
    assert_eq!(parse_pin(pin("assets", PIN)), None);
}
