use super::*;
use std::os::unix::fs::FileExt;

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
    f.set_len(5 << 22).unwrap();
    f.write_all_at(&[1u8; 1000], 1 << 22).unwrap();
    drop(f);
    let v1_hash = hash_file(&img);
    let v1 = snapshot_at(&root, &img, "img", "first upload").unwrap();
    let v1_file = root.join(&v1.file);
    assert_eq!((v1.version.as_str(), v1.size), ("v1", 5 << 22));
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
    let ver = manifests(&root, "img")
        .into_iter()
        .map(|m| m.version)
        .collect::<Vec<_>>();
    assert_eq!(ver, ["v2", "v1"], "newest first");
    // Rollback to v1 = hard link again: no space needed, same bytes, same file.
    check_at(&root, "img", "v1").unwrap();
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
    assert!(root.join(v2.file).exists());
    assert!(load(&root, "img", "../x").is_err());
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

/// A new golden renamed over image.img (an upload).
fn replace(img: &Path, data: &[u8]) {
    let new = img.with_extension("img.new");
    std::fs::write(&new, data).unwrap();
    std::fs::rename(&new, img).unwrap();
}

/// A dedup index follows its file: linked at snapshot (the same inode), kept by gc while a manifest names the file,
/// dropped with it; one of no file, or unfinished, is garbage.
#[test]
fn dedup_index_follows_its_file() {
    use crate::dedup::index_of;
    let (root, img, d) = dir("bhash");
    std::fs::write(&img, vec![5u8; 1000]).unwrap();
    std::fs::write(index_of(&img), b"idx").unwrap();
    let v1 = snapshot_at(&root, &img, "img", "").unwrap();
    let f1 = root.join(&v1.file);
    let i1 = index_of(&f1);
    assert!(same_file(&i1, &index_of(&img)), "linked with the version");
    let files = f1.parent().unwrap();
    let (orphan, tmp) = (files.join("v9.img.bhash"), files.join("v1.img.bhash.tmp"));
    std::fs::write(&orphan, b"x").unwrap();
    std::fs::write(&tmp, b"x").unwrap();
    gc(&root).unwrap();
    assert!(f1.exists() && i1.exists());
    assert!(!orphan.exists() && !tmp.exists());
    replace(&img, &[6u8; 1000]);
    let v2 = snapshot_at(&root, &img, "img", "").unwrap();
    let i2 = index_of(&root.join(&v2.file));
    assert!(i2.exists());
    delete_at(&root, "img", "v2").unwrap();
    assert!(!i2.exists() && i1.exists());
    delete_at(&root, "img", "v1").unwrap();
    assert!(
        !i1.exists() && !files.exists(),
        "nothing left of the image's files"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A version's file replaced by a copy of it (a rollback's rebuild): the path names the copy, its index goes along; a
/// file of another size is refused.
#[test]
fn version_file_replaced() {
    use crate::dedup::index_of;
    let (root, img, d) = dir("replace");
    std::fs::write(&img, vec![7u8; 5000]).unwrap();
    let v1 = snapshot_at(&root, &img, "img", "").unwrap();
    let f1 = root.join(&v1.file);
    let copy = d.join("copy.img");
    std::fs::write(&copy, vec![7u8; 4000]).unwrap();
    assert!(replace_file_at(&root, "img", "v1", &copy).is_err());
    assert!(same_file(&f1, &img), "refused: nothing changed");
    std::fs::write(&copy, vec![7u8; 5000]).unwrap();
    std::fs::write(index_of(&copy), b"idx").unwrap();
    replace_file_at(&root, "img", "v1", &copy).unwrap();
    assert!(same_file(&f1, &copy) && !same_file(&f1, &img));
    assert!(same_file(&index_of(&f1), &index_of(&copy)));
    assert_eq!(std::fs::read(&f1).unwrap(), vec![7u8; 5000]);
    let _ = std::fs::remove_dir_all(&d);
}

/// What a dedup pass pairs: a new golden with the newest version (none while image.img is a version itself), and
/// the version files, oldest first.
#[test]
fn dedup_pairs() {
    let (root, img, d) = dir("pairs");
    let names = |root: &Path| {
        chain_at(root, "img")
            .into_iter()
            .map(|(v, _)| v)
            .collect::<Vec<_>>()
    };
    std::fs::write(&img, b"one").unwrap();
    assert_eq!(dedupe_base_at(&root, &img, "img"), None, "no versions");
    assert!(names(&root).is_empty());
    let v1 = snapshot_at(&root, &img, "img", "").unwrap();
    assert_eq!(dedupe_base_at(&root, &img, "img"), None, "image.img is v1");
    replace(&img, b"two");
    assert_eq!(
        dedupe_base_at(&root, &img, "img"),
        Some(("v1".to_string(), root.join(&v1.file)))
    );
    snapshot_at(&root, &img, "img", "").unwrap();
    replace(&img, b"three");
    assert_eq!(
        dedupe_base_at(&root, &img, "img").unwrap().0,
        "v2",
        "the newest"
    );
    assert_eq!(names(&root), ["v1", "v2"]);
    // v1 restored: image.img is that file again.
    let tmp = img.with_extension("img.new");
    rehydrate_at(&root, &tmp, "img", "v1").unwrap();
    std::fs::rename(&tmp, &img).unwrap();
    assert_eq!(dedupe_base_at(&root, &img, "img"), None);
    let _ = std::fs::remove_dir_all(&d);
}

/// The version store where versions share blocks (XFS with reflink): the version before each new golden deduped
/// against it (the golden's blocks never move), saved, rolled back, and versions deleted on either side of a shared
/// block — every file left reads back exactly, from the disk. `sudo <test binary> --ignored versions_dedup_live`
#[test]
#[ignore]
fn versions_dedup_live() {
    use crate::dedup::tests::{Fs, XFS, drop_caches, file_hash, layout, random, write_file};
    use crate::dedup::{Side, dedupe, index_of};
    const MB: u64 = 1 << 20;
    let Some(fs) = Fs::new("xfs", XFS) else {
        return;
    };
    let (root, img) = (fs.mnt.join("storage"), fs.mnt.join("images/x/image.img"));
    std::fs::create_dir_all(img.parent().unwrap()).unwrap();
    // `data` with MB `at` changed, as a new golden renamed over image.img.
    let golden = |data: &[u8], at: u64, seed: &[u8]| {
        let mut d = data.to_vec();
        d[(at * MB) as usize..((at + 1) * MB) as usize].copy_from_slice(&random(MB, seed));
        let new = img.with_extension("img.new");
        write_file(&new, 40 * MB, &d);
        std::fs::rename(&new, &img).unwrap();
        d
    };
    let restore = |v: &str| {
        let tmp = img.with_extension("img.new");
        rehydrate_at(&root, &tmp, "x", v).unwrap();
        std::fs::rename(&tmp, &img).unwrap();
    };
    let data = random(32 * MB, b"v1");
    write_file(&img, 40 * MB, &data);
    let h1 = file_hash(&img);
    let v1 = snapshot_at(&root, &img, "x", "").unwrap();
    let f1 = root.join(&v1.file);

    // A new golden = v1 but MB 8: v1 gives back the 31 MB they share; the golden's own blocks never move. Saved as v2,
    // its index linked with it.
    let d2 = golden(&data, 8, b"two");
    let h2 = file_hash(&img);
    let (ver, base) = dedupe_base_at(&root, &img, "x").unwrap();
    assert_eq!((ver.as_str(), &base), ("v1", &f1));
    let served = layout(&img);
    let r = dedupe(&Side::open(&base).unwrap(), &Side::open(&img).unwrap()).unwrap();
    assert_eq!(r.deduped, 31 * MB);
    assert_eq!(layout(&img), served, "the served golden untouched");
    let v2 = snapshot_at(&root, &img, "x", "").unwrap();
    let f2 = root.join(&v2.file);
    assert!(
        same_file(&index_of(&f2), &index_of(&img)),
        "v2 got image.img's index"
    );
    assert_eq!(dedupe_base_at(&root, &img, "x"), None, "image.img is v2");

    // Back to v1, v2 (the side whose blocks v1 now uses) deleted: v1 whole.
    restore("v1");
    delete_at(&root, "x", "v2").unwrap();
    assert!(!f2.exists() && !index_of(&f2).exists());
    drop_caches();
    assert_eq!(file_hash(&img), h1);

    // A new golden = v2's bytes but MB 20 too: v1 gives back the 30 MB they share; saved as v3, restored, then v1 (the
    // side that was deduped) deleted: v3 whole.
    golden(&d2, 20, b"three");
    let h3 = file_hash(&img);
    assert_ne!(h3, h2);
    let served = layout(&img);
    let r = dedupe(&Side::open(&f1).unwrap(), &Side::open(&img).unwrap()).unwrap();
    assert_eq!(r.deduped, 30 * MB);
    assert_eq!(layout(&img), served, "the served golden untouched");
    let v3 = snapshot_at(&root, &img, "x", "").unwrap();
    restore("v1");
    restore(&v3.version);
    delete_at(&root, "x", "v1").unwrap();
    assert!(!f1.exists() && !index_of(&f1).exists());
    drop_caches();
    assert_eq!(file_hash(&img), h3);
    assert_eq!(file_hash(&root.join(&v3.file)), h3);
}

/// Rolling back to a version in pieces (deduped against a newer golden), as images.rs + the publish's make_whole do:
/// restored as a hard link, then rebuilt in one piece with its mtime and made the version's file before it is served;
/// the other version then deduped against it, the served file untouched. A golden in one piece is served as it is.
/// `sudo <test binary> --ignored rollback_rebuild_live`
#[test]
#[ignore]
fn rollback_rebuild_live() {
    use crate::dedup::tests::{Fs, XFS, drop_caches, file_hash, layout, random, write_file};
    use crate::dedup::{Side, dedupe, index_of, link_index, rebuild_in_place, scattered};
    const MB: u64 = 1 << 20;
    let Some(fs) = Fs::new("xfs", XFS) else {
        return;
    };
    let (root, img) = (fs.mnt.join("storage"), fs.mnt.join("images/x/image.img"));
    std::fs::create_dir_all(img.parent().unwrap()).unwrap();
    let data = random(40 * MB, b"v1");
    write_file(&img, 48 * MB, &data);
    let (h1, mt1) = (
        file_hash(&img),
        std::fs::metadata(&img).unwrap().modified().unwrap(),
    );
    let v1 = snapshot_at(&root, &img, "x", "").unwrap();
    let f1 = root.join(&v1.file);
    // A new golden with 64 KB changed every 2 MB: v1 deduped against it is in ~40 pieces.
    let mut d2 = data.clone();
    for i in 0..20u64 {
        let at = (i * 2 * MB + MB) as usize;
        d2[at..at + 64 * 1024].copy_from_slice(&random(64 * 1024, &i.to_le_bytes()));
    }
    let new = img.with_extension("img.new");
    write_file(&new, 48 * MB, &d2);
    std::fs::rename(&new, &img).unwrap();
    assert_eq!(scattered(&f1), None, "in one piece before the dedupe");
    dedupe(&Side::open(&f1).unwrap(), &Side::open(&img).unwrap()).unwrap();
    let n = scattered(&f1).expect("in pieces after it");
    assert!(n > 16, "{n} pieces");
    let v2 = snapshot_at(&root, &img, "x", "").unwrap();
    let (f2, h2) = (root.join(&v2.file), file_hash(&img));
    assert_eq!(scattered(&img), None, "the served golden: as it is");

    // The rollback to v1: its file linked back as image.img, in pieces → rebuilt before the publish serves it.
    let tmp = img.with_extension("img.new");
    rehydrate_at(&root, &tmp, "x", "v1").unwrap();
    std::fs::rename(&tmp, &img).unwrap();
    link_index(&f1, &img);
    assert!(scattered(&img).is_some());
    rebuild_in_place(&img, |tmp| replace_file_at(&root, "x", "v1", tmp)).unwrap();
    assert_eq!(scattered(&img), None);
    assert!(
        !tmp.exists() && !index_of(&tmp).exists(),
        "nothing left over"
    );
    assert!(same_file(&f1, &img), "image.img is v1's file again");
    assert_eq!(file_hash(&img), h1);
    assert_eq!(
        std::fs::metadata(&img).unwrap().modified().unwrap(),
        mt1,
        "its mtime kept: nothing hashed again"
    );
    assert!(layout(&img).len() <= 4, "in one piece: {:?}", layout(&img));
    assert!(same_file(&index_of(&img), &index_of(&f1)));
    // v2 gives back what it shares with the served v1; v1's blocks stay where they are.
    let served = layout(&img);
    let r = dedupe(&Side::open(&f2).unwrap(), &Side::open(&img).unwrap()).unwrap();
    assert!(r.deduped >= 30 * MB, "{r:?}");
    assert_eq!(layout(&img), served);
    drop_caches();
    assert_eq!((file_hash(&img), file_hash(&f2)), (h1, h2));
}
