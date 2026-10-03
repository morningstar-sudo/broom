// versions.rs — image versions without ZFS. A version = manifest of 4 MB chunks of images/<name>/image.img:
//   storage/chunks/<ab>/<blake3>           chunk data, stored once (dedup across versions and images)
//   storage/manifests/<name>/<vN>.json     {version, label, created, size, chunk_size, chunks: [hash|"zero"]}
// All-zero chunks and holes are never stored ("zero"). Rollback rewrites only the chunks that differ and
// punches holes for zero chunks, so image.img stays sparse. Snapshot/rollback run as publish jobs (images.rs).
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

const CHUNK: u64 = 4 << 20;
const ZERO: &str = "zero";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Manifest {
    pub version: String,
    pub label: String,
    /// Unix seconds.
    pub created: u64,
    pub size: u64,
    pub chunk_size: u64,
    /// Empty in list() results (not needed there).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<String>,
    /// Bytes that differ from the active version (what a rollback to this one rewrites). Filled by list();
    /// None when the image has no active version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<u64>,
}

/// `<home>/storage` by default (next to the binary); override with BOOTROM_STORAGE_DIR.
fn storage() -> PathBuf {
    std::env::var("BOOTROM_STORAGE_DIR").map(PathBuf::from).unwrap_or_else(|_| crate::home().join("storage"))
}

fn chunk_path(root: &Path, hash: &str) -> PathBuf {
    root.join("chunks").join(&hash[..2]).join(hash)
}

fn manifest_dir(root: &Path, name: &str) -> PathBuf {
    root.join("manifests").join(name)
}

fn e<E: std::fmt::Display>(ctx: &str) -> impl Fn(E) -> String + '_ {
    move |err| format!("{ctx}: {err}")
}

/// True if [off, off+len) is entirely a hole (SEEK_DATA finds no data before its end).
fn is_hole(f: &File, off: u64, len: u64) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: valid fd; lseek has no memory effects.
    let data = unsafe { libc::lseek(f.as_raw_fd(), off as libc::off_t, libc::SEEK_DATA) };
    data < 0 || data as u64 >= off + len // ENXIO (no data after off) → -1
}

/// Chunk `i` of `f` → (hash or "zero", bytes if non-zero).
fn read_chunk(f: &File, i: u64, size: u64, buf: &mut Vec<u8>) -> Result<Option<String>, String> {
    let off = i * CHUNK;
    let len = CHUNK.min(size - off);
    if is_hole(f, off, len) {
        return Ok(None);
    }
    buf.resize(len as usize, 0);
    f.read_exact_at(buf, off).map_err(e("read image"))?;
    if buf.iter().all(|&b| b == 0) {
        return Ok(None);
    }
    Ok(Some(blake3::hash(buf).to_hex().to_string()))
}

fn image_path(name: &str) -> PathBuf {
    crate::images_dir().join(name).join("image.img")
}

/// Versions of an image, newest first (chunk lists omitted), each with `diff` vs the `active` version.
pub fn list(name: &str, active: Option<&str>) -> Vec<Manifest> {
    list_at(&storage(), name, active)
}

/// Versions of an image, newest first, each `diff` = bytes that differ from image.img AS IT IS NOW (the golden on
/// the image list), not from another version. `active` only saves the hashing when image.img is still that version.
pub fn list_vs_current(name: &str, active: Option<&str>) -> Vec<Manifest> {
    let root = storage();
    let cur = current_at(&root, &image_path(name), &crate::images_dir().join(name).join("current.chunks"), name, active);
    let mut v = manifests(&root, name);
    for m in &mut v {
        m.diff = cur.as_ref().map(|c| diff_bytes(c, m));
        m.chunks.clear();
    }
    v
}

/// Chunk list of image.img now. Free when it is untouched since the active version was saved; otherwise hashed once
/// (a 30 GB golden ≈ a minute) and cached in `cache`, keyed by size + mtime.
fn current_at(root: &Path, img: &Path, cache: &Path, name: &str, active: Option<&str>) -> Option<Manifest> {
    let meta = std::fs::metadata(img).ok()?;
    let mtime = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    if let Some(m) = active.and_then(|a| load(root, name, a).ok()).filter(|m| m.size == meta.len() && mtime.as_secs() <= m.created) {
        return Some(m);
    }
    let size = meta.len();
    let stamp = format!("{size} {}", mtime.as_nanos());
    let cur = |chunks| Manifest { version: "current".into(), label: String::new(), created: 0, size, chunk_size: CHUNK, chunks, diff: None };
    if let Some((head, rest)) = std::fs::read_to_string(cache).ok().as_deref().and_then(|s| s.split_once('\n')) {
        if head == stamp {
            return Some(cur(rest.lines().map(String::from).collect()));
        }
    }
    let f = File::open(img).ok()?;
    let (mut chunks, mut buf) = (Vec::new(), Vec::new());
    for i in 0..size.div_ceil(CHUNK) {
        chunks.push(read_chunk(&f, i, size, &mut buf).ok()?.unwrap_or_else(|| ZERO.to_string()));
    }
    let _ = std::fs::write(cache, format!("{stamp}\n{}", chunks.join("\n")));
    Some(cur(chunks))
}

/// Every manifest of an image (with chunks), newest first.
fn manifests(root: &Path, name: &str) -> Vec<Manifest> {
    let mut v: Vec<Manifest> = std::fs::read_dir(manifest_dir(root, name))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|d| serde_json::from_slice::<Manifest>(&std::fs::read(d.path()).ok()?).ok())
        .collect();
    v.sort_by_key(|m| std::cmp::Reverse(m.version[1..].parse::<u64>().unwrap_or(0)));
    v
}

fn list_at(root: &Path, name: &str, active: Option<&str>) -> Vec<Manifest> {
    let mut v = manifests(root, name);
    let base = active.and_then(|a| v.iter().find(|m| m.version == a)).cloned();
    for m in &mut v {
        m.diff = base.as_ref().map(|b| diff_bytes(b, m));
        m.chunks.clear();
    }
    v
}

/// Bytes at chunk positions where two versions differ (a size change counts as differing chunks).
fn diff_bytes(a: &Manifest, b: &Manifest) -> u64 {
    let size = a.size.max(b.size);
    (0..a.chunks.len().max(b.chunks.len()))
        .filter(|&i| a.chunks.get(i) != b.chunks.get(i))
        .map(|i| CHUNK.min(size - i as u64 * CHUNK))
        .sum()
}

/// Snapshot image.img as a new version "v<N>". Returns the manifest (without chunks) + new chunk count.
pub fn snapshot(name: &str, label: &str) -> Result<(Manifest, usize), String> {
    snapshot_at(&storage(), &image_path(name), name, label)
}

fn snapshot_at(root: &Path, img: &Path, name: &str, label: &str) -> Result<(Manifest, usize), String> {
    let f = File::open(img).map_err(e(&img.display().to_string()))?;
    let size = f.metadata().map_err(e("stat image"))?.len();
    let mut chunks = Vec::new();
    let mut new = 0;
    let mut buf = Vec::new();
    for i in 0..size.div_ceil(CHUNK) {
        match read_chunk(&f, i, size, &mut buf)? {
            None => chunks.push(ZERO.to_string()),
            Some(h) => {
                let p = chunk_path(root, &h);
                if !p.exists() {
                    std::fs::create_dir_all(p.parent().unwrap()).map_err(e("mkdir chunks"))?;
                    let tmp = p.with_extension("tmp");
                    std::fs::write(&tmp, &buf).map_err(e("write chunk"))?;
                    std::fs::rename(&tmp, &p).map_err(e("store chunk"))?;
                    new += 1;
                }
                chunks.push(h);
            }
        }
    }
    let dir = manifest_dir(root, name);
    std::fs::create_dir_all(&dir).map_err(e("mkdir manifests"))?;
    let n = list_at(root, name, None).first().and_then(|m| m.version[1..].parse::<u64>().ok()).unwrap_or(0) + 1;
    let created = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let m = Manifest { version: format!("v{n}"), label: label.to_string(), created, size, chunk_size: CHUNK, chunks, diff: None };
    let tmp = dir.join(format!("{}.json.tmp", m.version));
    std::fs::write(&tmp, serde_json::to_vec(&m).unwrap()).map_err(e("write manifest"))?;
    std::fs::rename(&tmp, dir.join(format!("{}.json", m.version))).map_err(e("store manifest"))?;
    Ok((Manifest { chunks: Vec::new(), ..m }, new))
}

fn load(root: &Path, name: &str, version: &str) -> Result<Manifest, String> {
    if !version.starts_with('v') || !version[1..].chars().all(|c| c.is_ascii_digit()) || version.len() < 2 {
        return Err(format!("invalid version {version:?}"));
    }
    let p = manifest_dir(root, name).join(format!("{version}.json"));
    let data = std::fs::read(&p).map_err(|_| format!("image {name} has no version {version}"))?;
    serde_json::from_slice(&data).map_err(e("manifest"))
}

/// Make image.img equal to `version`: rewrite only chunks that differ, zero chunks → holes.
/// The caller must make sure nothing serves the file meanwhile (iSCSI target removed). Returns chunks rewritten.
pub fn rehydrate(name: &str, version: &str) -> Result<usize, String> {
    rehydrate_at(&storage(), &image_path(name), name, version)
}

/// `version` of image `from` → v1 of the new image `to`: only the manifest is copied, the chunks are shared (dedup;
/// gc keeps a chunk while any manifest of any image names it).
pub fn clone_to(from: &str, version: &str, to: &str, label: &str) -> Result<(), String> {
    clone_at(&storage(), from, version, to, label)
}

fn clone_at(root: &Path, from: &str, version: &str, to: &str, label: &str) -> Result<(), String> {
    let m = load(root, from, version)?;
    let dir = manifest_dir(root, to);
    if std::fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_some()) {
        return Err(format!("image {to} already has versions"));
    }
    std::fs::create_dir_all(&dir).map_err(e("mkdir manifests"))?;
    let m = Manifest { version: "v1".into(), label: label.to_string(), ..m };
    std::fs::write(dir.join("v1.json"), serde_json::to_vec(&m).unwrap()).map_err(e("write manifest"))
}

/// `version` → a new file `dst` (export), image.img untouched.
pub fn rehydrate_to(name: &str, version: &str, dst: &Path) -> Result<usize, String> {
    rehydrate_at(&storage(), dst, name, version)
}

fn rehydrate_at(root: &Path, img: &Path, name: &str, version: &str) -> Result<usize, String> {
    let m = load(root, name, version)?;
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(img)
        .map_err(e(&img.display().to_string()))?;
    let cur = f.metadata().map_err(e("stat image"))?.len();
    if cur > m.size {
        f.set_len(m.size).map_err(e("truncate image"))?;
    }
    let mut changed = 0;
    let mut buf = Vec::new();
    for (i, want) in m.chunks.iter().enumerate() {
        let i = i as u64;
        let off = i * m.chunk_size;
        let len = m.chunk_size.min(m.size - off);
        let have = if off < cur.min(m.size) { read_chunk(&f, i, cur.min(m.size), &mut buf)? } else { None };
        if have.as_deref().unwrap_or(ZERO) == want {
            continue;
        }
        if want == ZERO {
            f.set_len(f.metadata().map_err(e("stat"))?.len().max(off + len)).map_err(e("extend image"))?;
            crate::winstage::punch_hole(&img.to_string_lossy(), off, len)?;
        } else {
            let data = std::fs::read(chunk_path(root, want)).map_err(e(&format!("chunk {want} missing")))?;
            if blake3::hash(&data).to_hex().as_str() != want {
                return Err(format!("chunk {want} is corrupt"));
            }
            f.write_all_at(&data, off).map_err(e("write image"))?;
        }
        changed += 1;
    }
    f.set_len(m.size).map_err(e("size image"))?;
    f.sync_all().map_err(e("sync image"))?;
    Ok(changed)
}

/// Delete a version, then remove chunks no manifest references anymore. Returns chunks freed.
pub fn delete(name: &str, version: &str) -> Result<usize, String> {
    delete_at(&storage(), name, version)
}

fn delete_at(root: &Path, name: &str, version: &str) -> Result<usize, String> {
    load(root, name, version)?; // validates + exists
    std::fs::remove_file(manifest_dir(root, name).join(format!("{version}.json"))).map_err(e("delete manifest"))?;
    gc(root)
}

/// Drop every version of an image (image deleted), then collect garbage.
pub fn delete_all(name: &str) -> Result<usize, String> {
    let root = storage();
    let _ = std::fs::remove_dir_all(manifest_dir(&root, name));
    gc(&root)
}

/// Mark & sweep: keep chunks referenced by any manifest of any image.
fn gc(root: &Path) -> Result<usize, String> {
    let mut used = std::collections::HashSet::new();
    for img in std::fs::read_dir(root.join("manifests")).into_iter().flatten().flatten() {
        for mf in std::fs::read_dir(img.path()).into_iter().flatten().flatten() {
            let m: Manifest = serde_json::from_slice(&std::fs::read(mf.path()).map_err(e("read manifest"))?)
                .map_err(e(&mf.path().display().to_string()))?;
            used.extend(m.chunks);
        }
    }
    let mut freed = 0;
    for sub in std::fs::read_dir(root.join("chunks")).into_iter().flatten().flatten() {
        for c in std::fs::read_dir(sub.path()).into_iter().flatten().flatten() {
            let h = c.file_name().to_string_lossy().into_owned();
            if !used.contains(&h) && std::fs::remove_file(c.path()).is_ok() {
                freed += 1;
            }
        }
    }
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_file(p: &Path) -> String {
        blake3::hash(&std::fs::read(p).unwrap()).to_hex().to_string()
    }

    #[test]
    fn snapshot_rollback_dedup_gc() {
        let d = std::env::temp_dir().join("broom_versions_test");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let (root, img) = (d.join("storage"), d.join("image.img"));
        // 3.5 chunks: data, hole, data, partial data.
        let f = File::create(&img).unwrap();
        f.set_len(3 * CHUNK + CHUNK / 2).unwrap();
        f.write_all_at(&[1u8; 100], 0).unwrap();
        f.write_all_at(&[2u8; 100], 2 * CHUNK).unwrap();
        f.write_all_at(&[3u8; 100], 3 * CHUNK).unwrap();
        let v1_hash = hash_file(&img);
        let count = |root: &Path| walk_count(&root.join("chunks"));

        let (v1, new) = snapshot_at(&root, &img, "img", "first").unwrap();
        assert_eq!((v1.version.as_str(), new, count(&root)), ("v1", 3, 3)); // hole chunk not stored
        // Same content again → nothing new stored.
        let (v2, new) = snapshot_at(&root, &img, "img", "").unwrap();
        assert_eq!((v2.version.as_str(), new), ("v2", 0));

        // Change: overwrite chunk 0, fill the hole chunk, grow the file.
        f.write_all_at(&[9u8; 100], 0).unwrap();
        f.write_all_at(&[8u8; 100], CHUNK).unwrap();
        f.set_len(5 * CHUNK).unwrap();
        let (v3, _) = snapshot_at(&root, &img, "img", "changed").unwrap();
        assert_eq!(v3.version, "v3");
        assert_ne!(hash_file(&img), v1_hash);

        // Rollback to v1: only the differing chunks are rewritten, size restored, chunk 1 back to a hole.
        let changed = rehydrate_at(&root, &img, "img", "v1").unwrap();
        assert_eq!(changed, 2); // chunk 0 (data) + chunk 1 (→ hole); chunk 4 dropped by truncation
        assert_eq!(hash_file(&img), v1_hash);
        assert!(is_hole(&File::open(&img).unwrap(), CHUNK, CHUNK));
        // Diff vs image.img as it is now (= v1 content, no active version given): hashed once, then cached.
        let cache = d.join("current.chunks");
        let cur = current_at(&root, &img, &cache, "img", None).unwrap();
        assert_eq!(cur.chunks, load(&root, "img", "v1").unwrap().chunks);
        assert!(cache.exists());
        assert_eq!(current_at(&root, &img, &cache, "img", None).unwrap().chunks, cur.chunks);
        assert_eq!(diff_bytes(&cur, &load(&root, "img", "v3").unwrap()), 4 * CHUNK);
        assert_eq!(list_at(&root, "img", None).iter().map(|m| m.version.as_str()).collect::<Vec<_>>(), ["v3", "v2", "v1"]);
        // Diff vs active v1: v2 same content → 0; v3 differs at chunks 0, 1, 3 and the new chunk 4 (size grew).
        let diffs = |active| list_at(&root, "img", active).iter().map(|m| m.diff).collect::<Vec<_>>();
        assert_eq!(diffs(Some("v1")), [Some(4 * CHUNK), Some(0), Some(0)]);
        assert_eq!(diffs(None), [None, None, None]);

        // Delete the middle one (v2): manifests are full chunk lists, not deltas → v1 + v3 stay whole;
        // nothing freed (v2 = v1's chunks); v3 can still be restored exactly.
        assert_eq!(delete_at(&root, "img", "v2").unwrap(), 0);
        assert_eq!(diffs(Some("v3")), [Some(0), Some(4 * CHUNK)]);
        rehydrate_at(&root, &img, "img", "v3").unwrap();
        rehydrate_at(&root, &img, "img", "v1").unwrap();
        assert_eq!(hash_file(&img), v1_hash);

        // Delete v3 → its 3 unique chunks freed (0 and 1 rewritten, 3 now a full-length chunk); v1 chunks stay.
        assert_eq!(count(&root), 6);
        assert_eq!(delete_at(&root, "img", "v3").unwrap(), 3);
        assert_eq!(count(&root), 3);
        assert!(load(&root, "img", "../x").is_err());

        // v1 → new image "copy": manifest only (no new chunks), restores to the same bytes in a new file, and its
        // chunks survive when the source image's versions are all deleted.
        clone_at(&root, "img", "v1", "copy", "from img v1").unwrap();
        assert!(clone_at(&root, "img", "v1", "copy", "").is_err(), "target already has versions");
        assert_eq!(count(&root), 3);
        let img2 = d.join("copy.img");
        rehydrate_at(&root, &img2, "copy", "v1").unwrap();
        assert_eq!(hash_file(&img2), v1_hash);
        assert_eq!(delete_at(&root, "img", "v1").unwrap(), 0);
        let _ = std::fs::remove_file(&img2);
        rehydrate_at(&root, &img2, "copy", "v1").unwrap();
        assert_eq!(hash_file(&img2), v1_hash);
        let _ = std::fs::remove_dir_all(&d);
    }

    fn walk_count(p: &Path) -> usize {
        std::fs::read_dir(p).into_iter().flatten().flatten().map(|s| std::fs::read_dir(s.path()).map_or(0, |r| r.count())).sum()
    }
}
