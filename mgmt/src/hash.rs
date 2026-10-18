// hash.rs — sha256 of a file: golden hash (clients compare it to detect a change and check the whole download),
// driver packages, stage bundle.

/// sha256 of `len` bytes that `read_at` fills, lower-case hex. The disk is the limit (sha2 uses SHA-NI): one thread
/// reads ahead while this one hashes, so the time is the read alone, not read + hash. Two 8 MB buffers go round.
pub(crate) fn sha256_with(
    len: u64,
    read_at: impl Fn(u64, &mut [u8]) -> Result<(), String> + Sync,
) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    const PIECE: usize = 8 << 20;
    std::thread::scope(|s| {
        let (full_tx, full_rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(1);
        let (free_tx, free_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        for _ in 0..2 {
            free_tx.send(vec![0u8; PIECE]).unwrap();
        }
        let read_at = &read_at;
        s.spawn(move || {
            let mut off = 0u64;
            while off < len {
                let Ok(mut buf) = free_rx.recv() else { return }; // the hasher stopped
                let n = ((len - off) as usize).min(PIECE);
                buf.truncate(n);
                let r = read_at(off, &mut buf).map(|_| buf);
                let failed = r.is_err();
                if full_tx.send(r).is_err() || failed {
                    return;
                }
                off += n as u64;
            }
        });
        let mut h = Sha256::new();
        for buf in full_rx {
            let mut buf = buf?;
            h.update(&buf);
            buf.resize(PIECE, 0);
            let _ = free_tx.send(buf);
        }
        Ok(crate::auth::hex(&h.finalize()))
    })
}

/// sha256 of a file, lower-case hex. None on error.
pub(crate) fn file_hash(path: &str) -> Option<String> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    sha256_with(len, |off, buf| {
        f.read_exact_at(buf, off).map_err(|e| e.to_string())
    })
    .ok()
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

/// Stamps remembered per cache file: going back and forth between versions (rollback = the version's file linked back
/// in, its own stamp) finds the hash instead of reading the whole golden again.
const CACHED: usize = 8;

/// `file_hash` kept in `cache` (one "<size> <mtime ns>\t<hash>" line per stamp, newest first): a publish of an
/// unchanged 12–30 GB golden (republish, cache-mode change, zram restore at server start, rollback) skips the full read.
/// Every write of the file moves its mtime.
pub(crate) fn file_hash_cached(path: &std::path::Path, cache: &std::path::Path) -> Option<String> {
    let stamp = stamp(path)?;
    let text = std::fs::read_to_string(cache).unwrap_or_default();
    let entries: Vec<(&str, &str)> = text.lines().filter_map(|l| l.split_once('\t')).collect();
    if let Some((_, h)) = entries.iter().find(|(s, h)| *s == stamp && h.len() == 64) {
        return Some(h.to_string());
    }
    let h = file_hash(&path.to_string_lossy())?;
    let keep = std::iter::once((stamp.as_str(), h.as_str()))
        .chain(entries.into_iter().filter(|(s, _)| *s != stamp))
        .take(CACHED)
        .map(|(s, h)| format!("{s}\t{h}\n"))
        .collect::<String>();
    let _ = std::fs::write(cache, keep);
    Some(h)
}

#[cfg(test)]
#[path = "../tests/unit/hash.rs"]
mod tests;
