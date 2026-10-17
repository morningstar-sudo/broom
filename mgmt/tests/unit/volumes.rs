use super::*;

#[test]
fn mountinfo_by_device() {
    let m = parse_mountinfo(
        "22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw\n\
         30 22 8:17 / /opt/bootrom/disks/1a2b3c4d rw,noatime shared:5 - xfs /dev/sdb1 rw\n\
         31 22 259:3 / /mnt/my\\040data rw - ext4 /dev/nvme0n1p3 rw\n\
         32 22 8:2 /x /bind rw - ext4 /dev/sda2 rw\n\
         bad line\n",
    );
    assert_eq!(
        m["8:2"],
        ("/".to_string(), "ext4".to_string()),
        "first mount kept"
    );
    assert_eq!(m["8:17"].1, "xfs");
    assert_eq!(m["259:3"].0, "/mnt/my data", "octal escapes");
    assert_eq!(m.len(), 3);
}

#[test]
fn uuid_from_superblock() {
    let mut b = vec![0u8; 4096];
    assert_eq!(fs_uuid(&b, "ext4"), None, "no filesystem");
    let u: Vec<u8> = (1..=16).collect();
    b[1024 + 0x68..1024 + 0x68 + 16].copy_from_slice(&u);
    assert_eq!(
        fs_uuid(&b, "ext4").as_deref(),
        Some("01020304-0506-0708-090a-0b0c0d0e0f10")
    );
    b[32..48].copy_from_slice(&u);
    assert_eq!(fs_uuid(&b, "xfs"), fs_uuid(&b, "ext4"));
}

#[test]
fn signatures() {
    let mut b = vec![0u8; 4096];
    assert!(!has_signature(&b));
    b[510] = 0x55;
    b[511] = 0xAA;
    assert!(has_signature(&b), "MBR / protective MBR");
    let mut b = vec![0u8; 4096];
    b[1080] = 0x53;
    b[1081] = 0xEF;
    assert!(has_signature(&b), "ext");
    let mut b = vec![0u8; 4096];
    b[..4].copy_from_slice(b"XFSB");
    assert!(has_signature(&b), "xfs");
}

/// A golden + a version hard-linked to it (two trees), and a sparse file: copied once, linked again, holes kept.
#[test]
fn copy_keeps_hard_links_and_holes() {
    use std::os::unix::fs::MetadataExt;
    let d = std::env::temp_dir().join(format!("broom_t_vol_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let (img, ver) = (d.join("src/images/w"), d.join("src/storage/files/w"));
    std::fs::create_dir_all(&img).unwrap();
    std::fs::create_dir_all(&ver).unwrap();
    let f = std::fs::File::create(img.join("image.img")).unwrap();
    f.set_len(64 << 20).unwrap(); // sparse
    f.write_all_at(b"golden", 32 << 20).unwrap();
    drop(f);
    std::fs::hard_link(img.join("image.img"), ver.join("v1.img")).unwrap();
    std::fs::write(img.join("note.txt"), "x").unwrap();

    let mut links = HashMap::new();
    let (di, dv) = (d.join("dst/images/w"), d.join("dst/storage/files/w"));
    copy_tree(&img, &di, &mut links).unwrap();
    copy_tree(&ver, &dv, &mut links).unwrap();
    let (a, b) = (
        std::fs::metadata(di.join("image.img")).unwrap(),
        std::fs::metadata(dv.join("v1.img")).unwrap(),
    );
    assert_eq!(a.ino(), b.ino(), "still one file");
    assert_eq!(a.len(), 64 << 20);
    assert!(a.blocks() * 512 < 8 << 20, "holes kept");
    let mut got = [0u8; 6];
    std::fs::File::open(dv.join("v1.img"))
        .unwrap()
        .read_exact_at(&mut got, 32 << 20)
        .unwrap();
    assert_eq!(&got, b"golden");
    assert_eq!(std::fs::read_to_string(di.join("note.txt")).unwrap(), "x");

    // A folder that is a link to another disk goes with its files.
    std::os::unix::fs::symlink(&di, d.join("link")).unwrap();
    remove_tree(&d.join("link")).unwrap();
    assert!(!di.exists() && std::fs::symlink_metadata(d.join("link")).is_err());
    let _ = std::fs::remove_dir_all(&d);
}
