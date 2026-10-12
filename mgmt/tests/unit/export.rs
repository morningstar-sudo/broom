use super::*;

#[test]
fn layout_and_single_partition() {
    use crate::disk::Part;
    let part = |s: u64, n: u64, k: &str| Part {
        start: s * 512,
        size: n * 512,
        kind: k.into(),
    };
    let gpt = |parts: Vec<Part>| Table {
        gpt: true,
        sector: 512,
        parts,
    };
    let esp = part(2048, 204800, "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
    let win = part(239616, 1000000, "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7");
    // Fresh UEFI upload: several partitions with an ESP → not trimmed, efi.
    assert_eq!(layout(&gpt(vec![esp.clone(), win.clone()])), (false, "efi"));
    // After publish: one GPT partition → trimmed.
    assert!(layout(&gpt(vec![win.clone()])).0);
    assert_eq!(
        single_part(&gpt(vec![win.clone()])),
        Some((239616 * 512, 1000000 * 512))
    );
    assert_eq!(single_part(&gpt(vec![esp, win])), None);
    // Legacy BIOS (MBR, System Reserved + Windows): not trimmed even with one partition, bios.
    let dos = Table {
        gpt: false,
        sector: 512,
        parts: vec![part(2048, 100, "7")],
    };
    assert_eq!(layout(&dos), (false, "bios"));
}

#[test]
fn region_copy_keeps_bytes_and_holes() {
    use std::os::unix::fs::{FileExt, MetadataExt};
    let d = std::env::temp_dir().join("broom_t_export");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let src = d.join("src");
    let f = std::fs::File::create(&src).unwrap();
    f.set_len(8 << 20).unwrap();
    f.write_all_at(b"head", 100).unwrap();
    f.write_all_at(b"tail", (7 << 20) + 5).unwrap();
    let dst = d.join("dst");
    copy_region(&src, 50, (8 << 20) - 50, &dst).unwrap();
    let got = std::fs::read(&dst).unwrap();
    assert_eq!(got.len(), (8 << 20) - 50);
    assert_eq!(&got[50..54], b"head");
    assert_eq!(&got[(7 << 20) + 5 - 50..(7 << 20) + 9 - 50], b"tail");
    // 2 data MB written, the 6 zero MB between stay holes.
    assert!(std::fs::metadata(&dst).unwrap().blocks() * 512 <= 3 << 20);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn vmx_firmware_and_disk() {
    let w = vmx("win", "windows", "efi");
    assert!(
        w.contains("firmware = \"efi\"")
            && w.contains("sata0:0.fileName = \"win.vmdk\"")
            && w.contains("windows9-64")
    );
    let l = vmx("ubnt", "linux", "bios");
    assert!(
        !l.contains("firmware")
            && l.contains("ubuntu-64")
            && l.contains("ethernet0.virtualDev = \"e1000e\"")
    );
}
