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
    Some(format!(
        "{} {}",
        meta.len(),
        meta.modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos()
    ))
}

/// `file_hash` kept in `cache` ("<size> <mtime ns>\n<hash>"): a publish of an unchanged 12–30 GB golden (republish,
/// cache-mode change, zram restore at server start) skips the full read. Every write of the file moves its mtime.
pub(crate) fn file_hash_cached(path: &std::path::Path, cache: &std::path::Path) -> Option<String> {
    let stamp = stamp(path)?;
    if let Some((head, h)) = std::fs::read_to_string(cache)
        .ok()
        .as_deref()
        .and_then(|s| s.split_once('\n'))
        && head == stamp
        && h.len() == 64
    {
        return Some(h.to_string());
    }
    let h = file_hash(&path.to_string_lossy())?;
    let _ = std::fs::write(cache, format!("{stamp}\n{h}"));
    Some(h)
}

#[cfg(test)]
#[path = "../tests/unit/hash.rs"]
mod tests;
