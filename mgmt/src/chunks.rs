// chunks.rs — a golden as 4 MB chunks: sha256 of the whole file + one hash per chunk (golden.chunks manifest), and the
// zstd-compressed chunks /api/golden-chunk hands to clients for delta updates (cached per hash).
use std::path::Path;

/// sha256 of a file (clients compare it to detect a golden change). None on error.
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

/// Chunk size of the golden manifests (golden.chunks) — clients fetch only the chunks they lack.
pub(crate) const MANIFEST_CHUNK: usize = 4 << 20;

/// sha256 of a file + the sha256 of each 4 MB chunk ("zero" = all zero), in ONE read pass. None on error.
fn file_hash_chunks(path: &Path) -> Option<(String, Vec<String>)> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let hex = |d: &[u8]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut f = std::fs::File::open(path).ok()?;
    let (mut whole, mut chunks) = (Sha256::new(), Vec::new());
    let mut buf = vec![0u8; MANIFEST_CHUNK];
    loop {
        let mut n = 0; // fill a whole chunk (read may return less)
        while n < buf.len() {
            match f.read(&mut buf[n..]).ok()? {
                0 => break,
                k => n += k,
            }
        }
        if n == 0 {
            break;
        }
        let c = &buf[..n];
        whole.update(c);
        chunks.push(if c.iter().all(|&b| b == 0) { "zero".to_string() } else { hex(&Sha256::digest(c)) });
        if n < buf.len() {
            break;
        }
    }
    Some((hex(&whole.finalize()), chunks))
}

/// Write `<dir>/golden.chunks` for the served golden `file`: first line `size <bytes>`, then one line per 4 MB
/// chunk (sha256 or `zero`). Clients diff it against the manifest of the copy they have → delta update.
/// Returns the whole-file sha256 (the golden hash).
pub(crate) fn write_manifest(file: &Path, dir: &Path) -> Result<String, String> {
    let (hash, chunks) = file_hash_chunks(file).ok_or_else(|| format!("sha256 of {} failed", file.display()))?;
    let size = std::fs::metadata(file).map_err(|e| e.to_string())?.len();
    let tmp = dir.join("golden.chunks.tmp");
    std::fs::write(&tmp, format!("size {size}\n{}\n", chunks.join("\n"))).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join("golden.chunks")).map_err(|e| e.to_string())?;
    Ok(hash)
}

/// Cache of zstd-compressed golden chunks, by content hash: a room of clients asks for the same changed chunks →
/// each is compressed once. Emptied when a Windows golden is rebuilt (content-addressed, only space is at stake).
pub(crate) fn chunk_cache_dir() -> std::path::PathBuf {
    crate::work_dir().join("zchunks")
}

/// Chunk `i` (4 MB) of the Windows golden.vhdx of image `name`, zstd-compressed, checked against `sha` (its line in
/// golden.chunks). Err(true) = the golden no longer has that content (republished meanwhile); Err(false) = I/O.
pub(crate) fn golden_chunk_zst(name: &str, i: u64, sha: &str) -> Result<Vec<u8>, (bool, String)> {
    let golden = crate::tftp_dir().join("broom-win").join(name).join("golden.vhdx");
    chunk_zst(&golden, &chunk_cache_dir(), i, sha)
}

fn chunk_zst(golden: &Path, cache: &Path, i: u64, sha: &str) -> Result<Vec<u8>, (bool, String)> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::FileExt;
    let cached = cache.join(format!("{sha}.zst"));
    if let Ok(z) = std::fs::read(&cached) {
        return Ok(z);
    }
    let f = std::fs::File::open(golden).map_err(|e| {
        tracing::error!("open golden {}: {e}", golden.display()); // keep the path in the log, not the response
        (false, "golden not available".to_string())
    })?;
    let off = i.checked_mul(MANIFEST_CHUNK as u64).ok_or((true, "chunk index out of range".to_string()))?;
    // Reject an out-of-range chunk from the file length (cheap) before reading + hashing 4 MB.
    if off >= f.metadata().map_err(|e| (false, e.to_string()))?.len() {
        return Err((true, "chunk index out of range".to_string()));
    }
    let mut buf = vec![0u8; MANIFEST_CHUNK];
    let mut n = 0;
    while n < buf.len() {
        match f.read_at(&mut buf[n..], off + n as u64).map_err(|e| (false, e.to_string()))? {
            0 => break,
            k => n += k,
        }
    }
    buf.truncate(n);
    let got: String = Sha256::digest(&buf).iter().map(|b| format!("{b:02x}")).collect();
    if n == 0 || got != sha {
        return Err((true, format!("chunk {i} not available (golden republished?)")));
    }
    // Level 3: ~55 % of Windows data, fast enough to keep a 10 Gbps link busy with a few cores.
    let z = zstd::bulk::compress(&buf, 3).map_err(|e| (false, e.to_string()))?;
    let _ = std::fs::create_dir_all(cache);
    let tmp = cached.with_extension(format!("tmp{}", std::process::id() ^ i as u32));
    if std::fs::write(&tmp, &z).is_ok() {
        let _ = std::fs::rename(&tmp, &cached);
    }
    Ok(z)
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

    /// Compressed chunk: decompresses to exactly the chunk, wrong sha → Err(true), cached after the first call.
    #[test]
    fn golden_chunk_zst_roundtrip() {
        let home = std::env::temp_dir().join("broom_test_zchunk");
        let _ = std::fs::remove_dir_all(&home);
        let (dir, cache) = (home.join("w"), home.join("zchunks"));
        std::fs::create_dir_all(&dir).unwrap();
        let get = |i: u64, sha: &str| super::chunk_zst(&dir.join("golden.vhdx"), &cache, i, sha);
        let mut data: Vec<u8> = (0..super::MANIFEST_CHUNK).map(|k| (k % 251) as u8).collect();
        data.extend(vec![5u8; 777]); // partial last chunk
        std::fs::write(dir.join("golden.vhdx"), &data).unwrap();
        super::write_manifest(&dir.join("golden.vhdx"), &dir).unwrap();
        let m = std::fs::read_to_string(dir.join("golden.chunks")).unwrap();
        let shas: Vec<&str> = m.lines().skip(1).collect();
        for (i, sha) in shas.iter().enumerate() {
            let z = get(i as u64, sha).unwrap();
            let raw = zstd::bulk::decompress(&z, super::MANIFEST_CHUNK).unwrap();
            assert_eq!(raw, data[i * super::MANIFEST_CHUNK..data.len().min((i + 1) * super::MANIFEST_CHUNK)]);
            assert!(cache.join(format!("{sha}.zst")).exists());
        }
        assert!(z_len_small(&get(0, shas[0]).unwrap()));
        let _ = std::fs::remove_dir_all(&cache); // uncached: the sha check runs
        assert!(get(1, shas[0]).unwrap_err().0, "sha of another chunk");
        // Out-of-range chunk index → rejected from the file length, never read.
        assert!(get(999_999, shas[0]).unwrap_err().0, "chunk past end of golden");
        assert!(get(u64::MAX, shas[0]).unwrap_err().0, "index * chunk overflows");
        let _ = std::fs::remove_dir_all(&home);
        fn z_len_small(z: &[u8]) -> bool {
            z.len() < super::MANIFEST_CHUNK / 10 // a repeating pattern compresses a lot
        }
    }

    /// golden.chunks: size line + one sha256/zero per 4 MB (last chunk partial); whole hash = sha256 of the file.
    #[test]
    fn manifest_chunks_and_whole_hash() {
        let d = std::env::temp_dir().join("broom_test_manifest");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("golden.vhdx");
        let mut data = vec![7u8; super::MANIFEST_CHUNK]; // chunk 0: data
        data.extend(vec![0u8; super::MANIFEST_CHUNK]); // chunk 1: zero
        data.extend(vec![9u8; 1000]); // chunk 2: partial
        std::fs::write(&f, &data).unwrap();
        let hash = super::write_manifest(&f, &d).unwrap();
        assert_eq!(hash, super::file_hash(f.to_str().unwrap()).unwrap());
        let m = std::fs::read_to_string(d.join("golden.chunks")).unwrap();
        let lines: Vec<&str> = m.lines().collect();
        assert_eq!(lines[0], format!("size {}", data.len()));
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[2], "zero");
        let sha = |b: &[u8]| {
            use sha2::{Digest, Sha256};
            Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect::<String>()
        };
        assert_eq!(lines[1], sha(&data[..super::MANIFEST_CHUNK]));
        assert_eq!(lines[3], sha(&data[2 * super::MANIFEST_CHUNK..]));
        let _ = std::fs::remove_dir_all(&d);
    }

}
