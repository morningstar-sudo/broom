// hash.rs — sha256 of a file: golden hash (clients compare it to detect a change and check the whole download),
// driver packages, stage bundle.

/// sha256 of a file, lower-case hex. None on error.
pub(crate) fn file_hash(path: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Sha256::new(); // SHA-NI when the CPU has it (same speed as sha256sum)
    let mut buf = vec![0u8; 4 << 20];
    loop {
        match f.read(&mut buf).ok()? {
            0 => break,
            n => h.update(&buf[..n]),
        }
    }
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
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
}
