use super::*;

#[test]
fn part_reads_inside_its_range() {
    let p = std::env::temp_dir().join("broom_test_part.bin");
    std::fs::write(&p, (0u8..100).collect::<Vec<_>>()).unwrap();
    let mut part = Part {
        f: File::open(&p).unwrap(),
        base: 10,
        len: 20,
        pos: 0,
    };
    let mut b = [0u8; 8];
    part.read_exact(&mut b).unwrap();
    assert_eq!(b, [10, 11, 12, 13, 14, 15, 16, 17]);
    assert_eq!(part.seek(SeekFrom::End(-4)).unwrap(), 16);
    let mut rest = Vec::new();
    part.read_to_end(&mut rest).unwrap();
    assert_eq!(
        rest,
        [26, 27, 28, 29],
        "stops at the partition end, not the file end"
    );
    assert!(part.seek(SeekFrom::Current(-100)).is_err());
    let _ = std::fs::remove_file(p);
}

/// Needs root + mkntfs/ntfs-3g (package ntfs-3g): builds a small NTFS image and reads it back.
#[test]
#[ignore]
fn ntfs_live() {
    use std::process::Command;
    let d = std::env::temp_dir().join("broom_ntfs_live");
    let _ = Command::new("umount").arg(d.join("mnt")).status();
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("mnt")).unwrap();
    let img = d.join("vol.img");
    let sh = |c: &str| {
        assert!(
            Command::new("sh")
                .args(["-c", c])
                .status()
                .unwrap()
                .success(),
            "{c}"
        )
    };
    // 1 MiB of something else before the partition, like a disk image.
    sh(&format!(
        "truncate -s 65M {0} && truncate -s 64M {0}.p && mkntfs -F -f -q {0}.p",
        img.display()
    ));
    sh(&format!(
        "ntfs-3g {}.p {}",
        img.display(),
        d.join("mnt").display()
    ));
    let m = d.join("mnt");
    std::fs::create_dir_all(m.join("Windows/System32/config")).unwrap();
    std::fs::write(m.join("Windows/System32/config/SYSTEM"), b"hive").unwrap();
    std::fs::create_dir_all(m.join("broom/efi/EFI/Microsoft/Boot")).unwrap();
    std::fs::write(
        m.join("broom/efi/EFI/Microsoft/Boot/BCD"),
        vec![7u8; 300_000],
    )
    .unwrap();
    std::fs::write(m.join("broom/boot-storage.ok"), b"storahci stornvme\r\n").unwrap();
    sh(&format!("umount {}", m.display()));
    sh(&format!(
        "dd if={0}.p of={0} bs=1M seek=1 conv=notrunc status=none",
        img.display()
    ));

    let mut v = Vol::open(img.to_str().unwrap(), (1 << 20, 64 << 20)).unwrap();
    assert!(!v.is_dirty().unwrap());
    assert!(
        v.exists("windows/system32/CONFIG/system"),
        "case-insensitive like Windows"
    );
    assert!(!v.exists("Windows/nope"));
    assert_eq!(
        v.read("broom/boot-storage.ok").unwrap().unwrap(),
        b"storahci stornvme\r\n"
    );
    assert_eq!(v.read("broom").unwrap(), None, "a directory has no content");
    assert!(v.root_names(12).iter().any(|n| n == "Windows"));
    let out = d.join("efi");
    v.extract_dir("broom/efi", &out).unwrap();
    assert_eq!(
        std::fs::read(out.join("EFI/Microsoft/Boot/BCD")).unwrap(),
        vec![7u8; 300_000]
    );
    let _ = std::fs::remove_dir_all(&d);
}
