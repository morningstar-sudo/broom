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

/// "<size> <mtime ns>" of a file: changes with every write to it (or a new file renamed over it).
pub(crate) fn stamp(path: &std::path::Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    Some(format!("{} {}", meta.len(), meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos()))
}

/// `file_hash` kept in `cache` ("<size> <mtime ns>\n<hash>"): a publish of an unchanged 12–30 GB golden (republish,
/// cache-mode change, zram restore at server start) skips the full read. Every write of the file moves its mtime.
pub(crate) fn file_hash_cached(path: &std::path::Path, cache: &std::path::Path) -> Option<String> {
    let stamp = stamp(path)?;
    if let Some((head, h)) = std::fs::read_to_string(cache).ok().as_deref().and_then(|s| s.split_once('\n')) {
        if head == stamp && h.len() == 64 {
            return Some(h.to_string());
        }
    }
    let h = file_hash(&path.to_string_lossy())?;
    let _ = std::fs::write(cache, format!("{stamp}\n{h}"));
    Some(h)
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

    #[test]
    fn cached_hash_follows_size_and_mtime() {
        let (p, c) = (std::env::temp_dir().join("broom_test_shac.img"), std::env::temp_dir().join("broom_test_shac.sha256"));
        let _ = std::fs::remove_file(&c);
        std::fs::write(&p, "abc").unwrap();
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(super::file_hash_cached(&p, &c).unwrap(), abc, "no cache → hashed");
        // Same stamp → the cached value is returned without reading the file.
        let stamp = std::fs::read_to_string(&c).unwrap().lines().next().unwrap().to_string();
        std::fs::write(&c, format!("{stamp}\n{}", "0".repeat(64))).unwrap();
        assert_eq!(super::file_hash_cached(&p, &c).unwrap(), "0".repeat(64));
        // File changed (size + mtime) → hashed again.
        std::fs::write(&p, "abcd").unwrap();
        assert_eq!(super::file_hash_cached(&p, &c).unwrap(), "88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589");
        let _ = (std::fs::remove_file(p), std::fs::remove_file(c));
    }
}
