use super::*;

#[test]
fn merge_widens_joins_and_cuts() {
    let size = 100 * MIB + 512; // not a whole MiB: the backup GPT ends there
    let r = merge(
        vec![
            (10 * MIB + 5, 10),  // inside one MiB → that MiB
            (11 * MIB, 3 * MIB), // touches the one before → joined
            (12 * MIB, MIB),     // inside the one before
            (50 * MIB, 0),       // empty → dropped
            (99 * MIB, 4 * MIB), // past the end → cut at the disk's end
            (0, MIB),            // unsorted
        ],
        size,
    );
    assert_eq!(
        r,
        vec![(0, MIB), (10 * MIB, 4 * MIB), (99 * MIB, MIB + 512)]
    );
}

#[test]
fn pieces_at_most_max() {
    let p = pieces(&[(0, 40 * MIB), (100 * MIB, 16 * MIB)], 16 * MIB);
    assert_eq!(
        p,
        vec![
            (0, 16 * MIB),
            (16 * MIB, 16 * MIB),
            (32 * MIB, 8 * MIB),
            (100 * MIB, 16 * MIB)
        ]
    );
    assert!(pieces(&[], 16 * MIB).is_empty());
}

#[test]
fn partition_kinds() {
    let mut bs = [0u8; 512];
    assert_eq!(fs_kind(&bs), "other", "MSR: all zeros");
    bs[3..11].copy_from_slice(b"NTFS    ");
    assert_eq!(fs_kind(&bs), "NTFS");
    bs[3..11].copy_from_slice(b"-FVE-FS-");
    assert_eq!(fs_kind(&bs), "BitLocker");
    bs[3..11].copy_from_slice(b"MSDOS5.0");
    bs[82..90].copy_from_slice(b"FAT32   ");
    assert_eq!(fs_kind(&bs), "FAT", "EFI partition");
}

#[test]
fn sizes_for_the_screen() {
    assert_eq!(size_str(300 * MIB), "300 MB");
    assert_eq!(size_str(40 << 30), "40.0 GB");
    assert_eq!(ascii("✓ published → v1 (⏱ 3 s)"), "published -> v1 ( 3 s)");
}

/// The plan of a GPT disk image: tables, the NTFS clusters in use, the other partitions whole.
/// Needs mkntfs + sfdisk (packages ntfs-3g, fdisk), not root.
#[test]
#[ignore]
fn plan_of_a_disk_image() {
    use std::process::Command;
    let d = std::env::temp_dir().join("broom_capture_plan");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let img = d.join("disk.img");
    let sh = |c: &str| {
        assert!(
            Command::new("sh")
                .args(["-ec", c])
                .status()
                .unwrap()
                .success(),
            "{c}"
        )
    };
    // p1 32 MiB "EFI" (filled with a pattern), p2 NTFS 200 MiB.
    sh(&format!(
        "truncate -s 256M {0}
         printf 'label: gpt\\nsize=32MiB, type=U\\nsize=200MiB, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\\n' | sfdisk -q {0}
         truncate -s 0 {0}.p && truncate -s 200M {0}.p && mkntfs -F -f -q -c 4096 {0}.p
         dd if={0}.p of={0} bs=1M seek=33 conv=notrunc status=none
         yes broom | head -c 1000000 | dd of={0} bs=1M seek=1 conv=notrunc status=none",
        img.display()
    ));
    let p = plan(img.to_str().unwrap()).unwrap();
    assert_eq!(p.size, 256 * MIB);
    assert!(p.warn.is_empty(), "{:?}", p.warn);
    assert!(
        p.lines[0].contains("other 32 MB, read whole"),
        "{:?}",
        p.lines
    );
    assert!(p.lines[1].contains("NTFS 200 MB"), "{:?}", p.lines);
    let has = |off: u64| p.ranges.iter().any(|&(o, l)| o <= off && off < o + l);
    assert!(has(0) && has(256 * MIB - 1), "both GPT copies");
    assert!(has(MIB) && has(32 * MIB), "p1 whole");
    assert!(has(33 * MIB), "NTFS boot sector + MFT start");
    let total: u64 = p.ranges.iter().map(|r| r.1).sum();
    assert!(
        total < 33 * MIB + 100 * MIB,
        "a fresh NTFS is mostly free: {total}"
    );
    let _ = std::fs::remove_dir_all(&d);
}
