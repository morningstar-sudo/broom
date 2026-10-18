use super::*;

fn hash_file(p: &Path) -> String {
    blake3::hash(&std::fs::read(p).unwrap())
        .to_hex()
        .to_string()
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
    assert!(
        snapshot_at(&root, &img, "img", "")
            .unwrap_err()
            .contains("already saved as v1")
    );
    // image.img replaced (an upload renames a new file over it) → v2.
    let new = d.join("image.img.new");
    std::fs::write(&new, vec![9u8; 3000]).unwrap();
    std::fs::rename(&new, &img).unwrap();
    assert_eq!(hash_file(&v1_file), v1_hash, "v1 kept its bytes");
    let v2 = snapshot_at(&root, &img, "img", "").unwrap();
    let ver = list_at(&root, "img")
        .into_iter()
        .map(|m| m.version)
        .collect::<Vec<_>>();
    assert_eq!(ver, ["v2", "v1"], "newest first");
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
    let (h1, h3) = (
        blake3::hash(&c(1)).to_hex().to_string(),
        blake3::hash(&c(3)).to_hex().to_string(),
    );
    for (h, data) in [(&h1, c(1)), (&h3, c(3))] {
        std::fs::create_dir_all(chunk_path(&root, h).parent().unwrap()).unwrap();
        std::fs::write(chunk_path(&root, h), data).unwrap();
    }
    std::fs::create_dir_all(chunk_path(&root, "ffff").parent().unwrap()).unwrap();
    std::fs::write(chunk_path(&root, "ffff"), b"orphan").unwrap();
    std::fs::create_dir_all(manifest_dir(&root, "img")).unwrap();
    let m = format!(
        r#"{{"version":"v1","label":"","created":1,"size":{},"chunk_size":{CHUNK},"chunks":["{h1}","zero","{h3}"]}}"#,
        3 * CHUNK
    );
    std::fs::write(manifest_dir(&root, "img").join("v1.json"), m).unwrap();
    std::fs::write(manifest_dir(&root, "img").join("v2.tmp"), b"{").unwrap(); // half-written: not a manifest
    assert_eq!(check_at(&root, "img", "v1").unwrap(), 2 * CHUNK);
    rehydrate_at(&root, &img, "img", "v1").unwrap();
    assert_eq!(
        std::fs::read(&img).unwrap(),
        [c(1), vec![0; CHUNK as usize], c(3)].concat()
    );
    use std::os::unix::fs::MetadataExt;
    assert!(
        std::fs::metadata(&img).unwrap().blocks() * 512 <= 2 * CHUNK + (1 << 20),
        "zero chunk = hole"
    );
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
    crate::disk::copy_region(&a, 0, None, &b).unwrap();
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    assert!(
        std::fs::metadata(&b).unwrap().blocks() * 512 <= 1 << 20,
        "holes stay holes"
    );
    let _ = std::fs::remove_dir_all(&d);
}
