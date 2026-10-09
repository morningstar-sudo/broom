// versions.rs — image versions without ZFS. A version = the image file itself at that moment, kept as a hard link:
//   storage/files/<name>/<vN>.img          hard link to images/<name>/image.img when the version was saved
//   storage/manifests/<name>/<vN>.json     {version, label, created, size, file}
// Saving one takes no time and no space until image.img is replaced; restoring one is a hard link again. Sound
// because image.img is never written in place: an upload, a rollback, a restored version all REPLACE it (rename),
// and the Windows publish trims only an image not trimmed yet (on its own copy if a version shares the file).
// Storage on another filesystem than the images (no hard link possible) → the version is a copy (holes kept).
// Versions saved by older releases are 4 MB chunk lists (storage/chunks/<ab>/<blake3>, "zero" = hole): no new ones
// are made, but they still restore, export, clone and delete (gc frees their chunks with them).
// Snapshot/rollback run as publish jobs (images.rs), serialized by versions_lock.
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
    #[serde(default = "chunk")]
    pub chunk_size: u64,
    /// Chunk version (older releases); empty in list() results.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<String>,
    /// Bytes that differ (list(): from the active version; list_vs_current(): from image.img) — what a rollback
    /// rewrites. None when that can't be told without reading whole files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<u64>,
    /// The version's file, relative to the storage root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

fn chunk() -> u64 {
    CHUNK
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

fn image_path(name: &str) -> PathBuf {
    crate::images_dir().join(name).join("image.img")
}

fn e<E: std::fmt::Display>(ctx: &str) -> impl Fn(E) -> String + '_ {
    move |err| format!("{ctx}: {err}")
}

/// Same file (inode): a version that still IS image.img.
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Every manifest of an image (with chunks), newest first.
fn manifests(root: &Path, name: &str) -> Vec<Manifest> {
    let mut v: Vec<Manifest> = std::fs::read_dir(manifest_dir(root, name))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|d| d.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|d| serde_json::from_slice::<Manifest>(&std::fs::read(d.path()).ok()?).ok())
        .collect();
    v.sort_by_key(|m| std::cmp::Reverse(m.version[1..].parse::<u64>().unwrap_or(0)));
    v
}

/// Versions of an image, newest first, each with `diff` vs the `active` version.
pub fn list(name: &str, active: Option<&str>) -> Vec<Manifest> {
    list_at(&storage(), name, active)
}

fn list_at(root: &Path, name: &str, active: Option<&str>) -> Vec<Manifest> {
    let mut v = manifests(root, name);
    let base = active.and_then(|a| v.iter().find(|m| m.version == a)).cloned();
    for m in &mut v {
        m.diff = base.as_ref().and_then(|b| diff_of(root, b, m));
        m.chunks.clear();
    }
    v
}

/// Versions of an image, newest first, `diff` = 0 for the one that is image.img right now, unknown for the others
/// (comparing would mean reading whole files).
pub fn list_vs_current(name: &str) -> Vec<Manifest> {
    let (root, img) = (storage(), image_path(name));
    let _ = std::fs::remove_file(crate::images_dir().join(name).join("current.chunks")); // cache of older releases
    let mut v = manifests(&root, name);
    for m in &mut v {
        m.diff = m.file.as_ref().and_then(|f| same_file(&root.join(f), &img).then_some(0));
        m.chunks.clear();
    }
    v
}

/// Bytes between two versions; None when it can't be told without reading whole files.
fn diff_of(root: &Path, a: &Manifest, b: &Manifest) -> Option<u64> {
    match (&a.file, &b.file) {
        (Some(x), Some(y)) => same_file(&root.join(x), &root.join(y)).then_some(0),
        (None, None) => {
            // Two chunk versions: chunk positions that differ (a size change counts as differing chunks).
            let size = a.size.max(b.size);
            Some(
                (0..a.chunks.len().max(b.chunks.len()))
                    .filter(|&i| a.chunks.get(i) != b.chunks.get(i))
                    .map(|i| a.chunk_size.min(size - i as u64 * a.chunk_size))
                    .sum(),
            )
        }
        _ => None,
    }
}

/// Save image.img as a new version "v<N>": a hard link (instant, no copy). Refused when image.img already is a version
/// (a rollback just restored it, or nothing changed since the last one).
pub fn snapshot(name: &str, label: &str) -> Result<Manifest, String> {
    snapshot_at(&storage(), &image_path(name), name, label)
}

fn snapshot_at(root: &Path, img: &Path, name: &str, label: &str) -> Result<Manifest, String> {
    let img = std::fs::canonicalize(img).map_err(e(&img.display().to_string()))?;
    let meta = std::fs::metadata(&img).map_err(e("stat image"))?;
    let have = manifests(root, name);
    if let Some(m) = have.iter().find(|m| m.file.as_ref().is_some_and(|f| same_file(&root.join(f), &img))) {
        return Err(format!("the golden is already saved as {}", m.version));
    }
    let n = have.first().and_then(|m| m.version[1..].parse::<u64>().ok()).unwrap_or(0) + 1;
    let rel = format!("files/{name}/v{n}.img");
    let p = root.join(&rel);
    std::fs::create_dir_all(p.parent().unwrap()).map_err(e("mkdir files"))?;
    let _ = std::fs::remove_file(&p); // left by an interrupted snapshot (no manifest named it)
    if let Err(err) = std::fs::hard_link(&img, &p) {
        // Another filesystem: a copy of the data (holes stay holes).
        tracing::info!("image {name}: version as a hard link not possible ({err}) → copied");
        let used = std::os::unix::fs::MetadataExt::blocks(&meta) * 512;
        crate::publish::need_space(root, used, "snapshot")?;
        copy_sparse(&img, &p).inspect_err(|_| {
            let _ = std::fs::remove_file(&p);
        })?;
    }
    sync_fs(root)?; // the file on disk before the manifest that names it
    let dir = manifest_dir(root, name);
    std::fs::create_dir_all(&dir).map_err(e("mkdir manifests"))?;
    let created = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let m = Manifest {
        version: format!("v{n}"),
        label: label.to_string(),
        created,
        size: meta.len(),
        chunk_size: CHUNK,
        chunks: Vec::new(),
        diff: None,
        file: Some(rel),
    };
    write_durable(&dir.join(format!("{}.json", m.version)), &serde_json::to_vec(&m).unwrap()).map_err(e("store manifest"))?;
    Ok(m)
}

/// Flush everything written on the filesystem holding `dir` (one call instead of an fsync per file).
fn sync_fs(dir: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let d = File::open(dir).map_err(e("open storage"))?;
    // SAFETY: valid fd; syncfs only flushes.
    if unsafe { libc::syncfs(d.as_raw_fd()) } != 0 {
        return Err(format!("syncfs {}: {}", dir.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Write `data` as `p`: a temp file, flushed to disk, then renamed — after a power loss `p` is either whole or absent.
fn write_durable(p: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = p.with_extension("tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, p)
}

fn load(root: &Path, name: &str, version: &str) -> Result<Manifest, String> {
    if !version.starts_with('v') || !version[1..].chars().all(|c| c.is_ascii_digit()) || version.len() < 2 {
        return Err(format!("invalid version {version:?}"));
    }
    let p = manifest_dir(root, name).join(format!("{version}.json"));
    let data = std::fs::read(&p).map_err(|_| format!("image {name} has no version {version}"))?;
    serde_json::from_slice(&data).map_err(e("manifest"))
}

/// `version` of image `from` → v1 of the new image `to`: only the manifest is copied, the data is shared (gc keeps a
/// file / chunk while any manifest of any image names it).
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
    write_durable(&dir.join("v1.json"), &serde_json::to_vec(&m).unwrap()).map_err(e("write manifest"))
}

/// Can `version` be restored? Its file (or every chunk) is there with the right length. → the bytes a restore must
/// write (0 for a file version: a hard link). Checked BEFORE anything served is touched.
pub fn check(name: &str, version: &str) -> Result<u64, String> {
    check_at(&storage(), name, version)
}

fn check_at(root: &Path, name: &str, version: &str) -> Result<u64, String> {
    let m = load(root, name, version)?;
    if let Some(f) = &m.file {
        return match std::fs::metadata(root.join(f)) {
            Ok(x) if x.len() == m.size => Ok(0),
            _ => Err(format!("version {version}: its file is missing or damaged — it can't be restored")),
        };
    }
    let mut data = 0;
    for (i, h) in m.chunks.iter().enumerate().filter(|(_, h)| h.as_str() != ZERO) {
        let len = m.chunk_size.min(m.size - i as u64 * m.chunk_size);
        if std::fs::metadata(chunk_path(root, h)).map(|x| x.len()).ok() != Some(len) {
            return Err(format!("version {version}: chunk {i} is missing or damaged — it can't be restored"));
        }
        data += len;
    }
    Ok(data)
}

/// Make image.img equal to `version` (a new image cloned from a version).
pub fn rehydrate(name: &str, version: &str) -> Result<(), String> {
    rehydrate_at(&storage(), &image_path(name), name, version)
}

/// `version` → `dst` (a new file: rollback renames it over image.img, export reads it).
pub fn rehydrate_to(name: &str, version: &str, dst: &Path) -> Result<(), String> {
    rehydrate_at(&storage(), dst, name, version)
}

fn rehydrate_at(root: &Path, dst: &Path, name: &str, version: &str) -> Result<(), String> {
    let m = load(root, name, version)?;
    let _ = std::fs::remove_file(dst);
    if let Some(file) = &m.file {
        // The version's file itself again (hard link), or a copy of its data on another filesystem.
        let src = root.join(file);
        if std::fs::hard_link(&src, dst).is_err() {
            copy_sparse(&src, dst)?;
        }
        return Ok(());
    }
    // A chunk version of an older release: written chunk by chunk (each checked against its hash), zero = hole.
    let f = File::create(dst).map_err(e(&dst.display().to_string()))?;
    f.set_len(m.size).map_err(e("size image"))?;
    for (i, h) in m.chunks.iter().enumerate().filter(|(_, h)| h.as_str() != ZERO) {
        let data = std::fs::read(chunk_path(root, h)).map_err(e(&format!("chunk {h} missing")))?;
        if blake3::hash(&data).to_hex().as_str() != h {
            return Err(format!("chunk {h} is corrupt"));
        }
        f.write_all_at(&data, i as u64 * m.chunk_size).map_err(e("write image"))?;
    }
    f.sync_all().map_err(e("sync image"))
}

/// `src` → new file `dst`, data ranges only (holes stay holes), flushed.
pub(crate) fn copy_sparse(src: &Path, dst: &Path) -> Result<(), String> {
    let s = crate::disk::Source::file(src)?;
    let out = File::create(dst).map_err(e(&dst.display().to_string()))?;
    out.set_len(s.len).map_err(e("size copy"))?;
    let mut buf = vec![0u8; 8 << 20];
    for (a, b) in s.data_ranges() {
        let mut at = a;
        while at < b {
            let n = ((b - at) as usize).min(buf.len());
            s.read_at(at, &mut buf[..n])?;
            out.write_all_at(&buf[..n], at).map_err(e("write copy"))?;
            at += n as u64;
        }
    }
    out.sync_all().map_err(e("sync copy"))
}

/// Delete a version, then free the files / chunks no manifest names anymore. Returns how many were freed.
pub fn delete(name: &str, version: &str) -> Result<usize, String> {
    delete_at(&storage(), name, version)
}

fn delete_at(root: &Path, name: &str, version: &str) -> Result<usize, String> {
    load(root, name, version)?; // validates + exists
    std::fs::remove_file(manifest_dir(root, name).join(format!("{version}.json"))).map_err(e("delete manifest"))?;
    // The version is gone either way; a gc that fails (an unreadable manifest) only leaves data for a later one.
    Ok(gc(root).unwrap_or_else(|err| {
        tracing::warn!("versions gc after deleting {name} {version}: {err}");
        0
    }))
}

/// Drop every version of an image (image deleted), then collect garbage.
pub fn delete_all(name: &str) -> Result<usize, String> {
    let root = storage();
    let _ = std::fs::remove_dir_all(manifest_dir(&root, name));
    gc(&root)
}

/// Mark & sweep: keep the files and chunks named by any manifest of any image.
fn gc(root: &Path) -> Result<usize, String> {
    let (mut chunks, mut files) = (std::collections::HashSet::new(), std::collections::HashSet::new());
    for img in std::fs::read_dir(root.join("manifests")).into_iter().flatten().flatten() {
        for mf in std::fs::read_dir(img.path()).into_iter().flatten().flatten() {
            // Only manifests (a leftover vN.tmp of an interrupted write is not one); an unreadable manifest still
            // stops gc — what it names may be in use.
            if mf.path().extension().is_none_or(|x| x != "json") {
                continue;
            }
            let m: Manifest = serde_json::from_slice(&std::fs::read(mf.path()).map_err(e("read manifest"))?)
                .map_err(e(&mf.path().display().to_string()))?;
            chunks.extend(m.chunks);
            files.extend(m.file.map(|f| root.join(f)));
        }
    }
    let mut freed = 0;
    for img in std::fs::read_dir(root.join("files")).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(img.path()).into_iter().flatten().flatten() {
            if !files.contains(&f.path()) && std::fs::remove_file(f.path()).is_ok() {
                freed += 1;
            }
        }
        let _ = std::fs::remove_dir(img.path()); // only when empty
    }
    for sub in std::fs::read_dir(root.join("chunks")).into_iter().flatten().flatten() {
        for c in std::fs::read_dir(sub.path()).into_iter().flatten().flatten() {
            if !chunks.contains(&*c.file_name().to_string_lossy()) && std::fs::remove_file(c.path()).is_ok() {
                freed += 1;
            }
        }
        let _ = std::fs::remove_dir(sub.path());
    }
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_file(p: &Path) -> String {
        blake3::hash(&std::fs::read(p).unwrap()).to_hex().to_string()
    }

    fn dir(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let d = std::env::temp_dir().join(format!("broom_versions_{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        (d.join("storage"), d.join("image.img"), d)
    }

    /// Versions are hard links: instant; survive image.img being replaced; restored as the same file again; a clone
    /// keeps the file alive; gc drops it with the last manifest naming it.
    #[test]
    fn file_versions() {
        let (root, img, d) = dir("files");
        let f = File::create(&img).unwrap();
        f.set_len(5 * CHUNK).unwrap();
        f.write_all_at(&[1u8; 1000], CHUNK).unwrap();
        drop(f);
        let v1_hash = hash_file(&img);
        let v1 = snapshot_at(&root, &img, "img", "first upload").unwrap();
        let v1_file = root.join(v1.file.as_ref().unwrap());
        assert_eq!((v1.version.as_str(), v1.size), ("v1", 5 * CHUNK));
        assert!(same_file(&v1_file, &img), "a hard link, no copy");
        assert!(snapshot_at(&root, &img, "img", "").unwrap_err().contains("already saved as v1"));
        // image.img replaced (an upload renames a new file over it) → v2.
        let new = d.join("image.img.new");
        std::fs::write(&new, vec![9u8; 3000]).unwrap();
        std::fs::rename(&new, &img).unwrap();
        assert_eq!(hash_file(&v1_file), v1_hash, "v1 kept its bytes");
        let v2 = snapshot_at(&root, &img, "img", "").unwrap();
        let ver = |active| list_at(&root, "img", active).into_iter().map(|m| (m.version, m.diff)).collect::<Vec<_>>();
        assert_eq!(ver(Some("v2")), [("v2".to_string(), Some(0)), ("v1".to_string(), None)]);
        // Rollback to v1 = hard link again: no space needed, same bytes, same file.
        assert_eq!(check_at(&root, "img", "v1").unwrap(), 0);
        rehydrate_at(&root, &new, "img", "v1").unwrap();
        std::fs::rename(&new, &img).unwrap();
        assert_eq!(hash_file(&img), v1_hash);
        assert!(same_file(&v1_file, &img));
        // Cloned to another image: the file stays while the clone names it, goes with the last manifest.
        clone_at(&root, "img", "v1", "copy", "").unwrap();
        delete_at(&root, "img", "v1").unwrap();
        assert!(v1_file.exists(), "the clone still uses it");
        let back = d.join("copy.img");
        rehydrate_at(&root, &back, "copy", "v1").unwrap();
        assert_eq!(hash_file(&back), v1_hash);
        delete_at(&root, "copy", "v1").unwrap();
        assert!(!v1_file.exists(), "no manifest names it any more");
        assert!(root.join(v2.file.unwrap()).exists());
        assert!(load(&root, "img", "../x").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A chunk version saved by an older release still checks, restores (holes stay holes), and frees its chunks.
    #[test]
    fn old_chunk_versions_still_restore() {
        let (root, img, d) = dir("chunks");
        let c = |b: u8| vec![b; CHUNK as usize];
        let (h1, h3) = (blake3::hash(&c(1)).to_hex().to_string(), blake3::hash(&c(3)).to_hex().to_string());
        for (h, data) in [(&h1, c(1)), (&h3, c(3))] {
            std::fs::create_dir_all(chunk_path(&root, h).parent().unwrap()).unwrap();
            std::fs::write(chunk_path(&root, h), data).unwrap();
        }
        std::fs::create_dir_all(chunk_path(&root, "ffff").parent().unwrap()).unwrap();
        std::fs::write(chunk_path(&root, "ffff"), b"orphan").unwrap();
        std::fs::create_dir_all(manifest_dir(&root, "img")).unwrap();
        let m = format!(r#"{{"version":"v1","label":"","created":1,"size":{},"chunk_size":{CHUNK},"chunks":["{h1}","zero","{h3}"]}}"#, 3 * CHUNK);
        std::fs::write(manifest_dir(&root, "img").join("v1.json"), m).unwrap();
        std::fs::write(manifest_dir(&root, "img").join("v2.tmp"), b"{").unwrap(); // half-written: not a manifest
        assert_eq!(check_at(&root, "img", "v1").unwrap(), 2 * CHUNK);
        rehydrate_at(&root, &img, "img", "v1").unwrap();
        assert_eq!(std::fs::read(&img).unwrap(), [c(1), vec![0; CHUNK as usize], c(3)].concat());
        use std::os::unix::fs::MetadataExt;
        assert!(std::fs::metadata(&img).unwrap().blocks() * 512 <= 2 * CHUNK + (1 << 20), "zero chunk = hole");
        assert_eq!(gc(&root).unwrap(), 1, "only the orphan chunk");
        assert_eq!(delete_at(&root, "img", "v1").unwrap(), 2);
        std::fs::remove_file(chunk_path(&root, &h1)).ok();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn copy_sparse_keeps_holes() {
        use std::os::unix::fs::MetadataExt;
        let (_, a, d) = dir("copy");
        let b = d.join("b");
        let f = File::create(&a).unwrap();
        f.set_len(64 << 20).unwrap();
        f.write_all_at(&[3u8; 5000], 40 << 20).unwrap();
        copy_sparse(&a, &b).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        assert!(std::fs::metadata(&b).unwrap().blocks() * 512 <= 1 << 20, "holes stay holes");
        let _ = std::fs::remove_dir_all(&d);
    }
}
