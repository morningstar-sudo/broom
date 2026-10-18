use super::*;
use std::process::Command;

/// Deeply nested crafted metadata (a stack overflow would abort the whole server) → parsed up to the limit, no crash.
#[test]
fn deep_lvm_metadata_no_overflow() {
    let _ = parse_meta(&"a{".repeat(200_000));
    let _ = parse_meta(&format!("x = {}", "[".repeat(200_000)));
}

/// A crafted LVM label (pv header offset past the sector, a huge metadata size) → None, no panic / no giant buffer.
#[test]
fn crafted_lvm_label_refused() {
    let p = std::env::temp_dir().join("broom_t_lvm_crafted");
    let mut b = vec![0u8; 8192];
    b[512..520].copy_from_slice(b"LABELONE");
    b[536..544].copy_from_slice(b"LVM2 001");
    b[532..536].copy_from_slice(&5000u32.to_le_bytes()); // offset_xl beyond the 2 KB read
    std::fs::write(&p, &b).unwrap();
    assert!(lvm_pv(&File::open(&p).unwrap(), 0).is_none());
    // Plausible label, metadata area claiming 1 TB of text.
    b[532..536].copy_from_slice(&32u32.to_le_bytes());
    let pvh = 512 + 32;
    b[pvh + 56..pvh + 64].copy_from_slice(&4096u64.to_le_bytes()); // mda offset (after an empty data-area list)
    b[pvh + 64..pvh + 72].copy_from_slice(&4096u64.to_le_bytes()); // mda size
    b[4096 + 4..4096 + 20].copy_from_slice(b" LVM2 x[5A%r0N*>");
    b[4096 + 40..4096 + 48].copy_from_slice(&512u64.to_le_bytes());
    b[4096 + 48..4096 + 56].copy_from_slice(&(1u64 << 40).to_le_bytes());
    std::fs::write(&p, &b).unwrap();
    assert!(lvm_pv(&File::open(&p).unwrap(), 0).is_none());
    let _ = std::fs::remove_file(&p);
}

#[test]
fn version_order() {
    assert!(ver_key("5.15.0-119-generic") > ver_key("5.15.0-91-generic"));
    assert!(ver_key("6.8.0-45-generic") > ver_key("5.15.0-119-generic"));
}

#[test]
fn lvm_metadata_parse() {
    let m = r#"ubuntu-vg {
            id = "abc"  # comment
            extent_size = 8192
            physical_volumes { pv0 { id = "Ab-Cd" pe_start = 2048 } }
            logical_volumes { ubuntu-lv { segment1 { start_extent = 0 extent_count = 10
                type = "striped" stripe_count = 1 stripes = [ "pv0", 5 ] } } }
        }"#;
    let v = parse_meta(m);
    let vg = v.get("ubuntu-vg").unwrap();
    assert_eq!(vg.num("extent_size"), Some(8192));
    let lv = vg
        .get("logical_volumes")
        .unwrap()
        .get("ubuntu-lv")
        .unwrap()
        .get("segment1")
        .unwrap();
    assert_eq!(
        lv.get("stripes"),
        Some(&V::List(vec![V::Str("pv0".into()), V::Num(5)]))
    );
    // pv "AbCd" at raw offset 1 MiB → LV starts at 1 MiB + 2048*512 + 5*8192*512.
    let segs = lvm_volumes("/dev/null", &[("AbCd".into(), 1 << 20)], m).unwrap();
    assert_eq!(
        segs[0].1.segs,
        vec![(0, (1 << 20) + 2048 * 512 + 5 * 8192 * 512, 10 * 8192 * 512)]
    );
}

/// Real ext4 in a GPT partition (mkfs.ext4 -d, no root needed): kernel + initrd + root UUID.
#[test]
fn ext4_in_partition() {
    let d = std::env::temp_dir().join("broom_linuxfs_test");
    let Some(disk) = ext4_disk(&d) else {
        eprintln!("mkfs.ext4 unavailable — skipped");
        return;
    };
    let out = d.join("out");
    let b = extract_boot(disk.to_str().unwrap(), out.to_str().unwrap()).unwrap();
    assert_eq!(b.kver, "6.8.0-45-generic");
    assert_eq!(b.root_uuid, "11111111-2222-3333-4444-555555555555");
    assert_eq!(std::fs::read(out.join("vmlinuz")).unwrap(), b"KERNEL");
    assert_eq!(std::fs::read(out.join("initrd.img")).unwrap(), b"INITRD");
    let _ = std::fs::remove_dir_all(&d);
}

/// The ext4 filesystem of an upload, mangled (superblock, group descriptors, inodes, extents, directories in its
/// first MiB): its kernel extracted or refused, never a panic.
#[test]
fn fuzz_ext4() {
    let d = std::env::temp_dir().join("broom_linuxfs_fuzz");
    let Some(disk) = ext4_disk(&d) else {
        eprintln!("mkfs.ext4 unavailable — skipped");
        return;
    };
    let f = File::options().write(true).open(&disk).unwrap();
    let meta = std::fs::read(&disk).unwrap()[1 << 20..2 << 20].to_vec();
    let out = d.join("out");
    crate::fuzz::run(
        "ext4",
        crate::fuzz::inputs(std::slice::from_ref(&meta), 400),
        |b| {
            f.write_all_at(b, 1 << 20).unwrap();
            let _ = extract_boot(disk.to_str().unwrap(), out.to_str().unwrap());
            f.write_all_at(&meta, 1 << 20).unwrap();
            f.set_len(20 << 20).unwrap();
        },
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// GPT disk with one ext4 partition at 1 MiB (mkfs.ext4 -d, no root needed) holding two kernels and an fstab.
/// None without mkfs.ext4.
fn ext4_disk(d: &std::path::Path) -> Option<std::path::PathBuf> {
    let _ = std::fs::remove_dir_all(d);
    let root = d.join("root");
    for (p, c) in [
        ("etc/fstab", "UUID=x / ext4 defaults 0 1\n"),
        ("boot/vmlinuz-5.15.0-91-generic", "old-kernel"),
        ("boot/initrd.img-5.15.0-91-generic", "old-initrd"),
        ("boot/vmlinuz-6.8.0-45-generic", "KERNEL"),
        ("boot/initrd.img-6.8.0-45-generic", "INITRD"),
    ] {
        std::fs::create_dir_all(root.join(p).parent().unwrap()).unwrap();
        std::fs::write(root.join(p), c).unwrap();
    }
    let fsimg = d.join("fs.img");
    let ok = Command::new("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-U",
            "11111111-2222-3333-4444-555555555555",
            "-d",
        ])
        .arg(&root)
        .arg(&fsimg)
        .arg("16M")
        .stdout(std::process::Stdio::null())
        .status();
    if !ok.is_ok_and(|s| s.success()) {
        return None;
    }
    // Disk: GPT with one partition at 1 MiB, ext4 copied in after partitioning
    // (partitioning over an existing ext4 makes sfdisk print a signature warning).
    let disk = d.join("disk.img");
    let f = File::create(&disk).unwrap();
    f.set_len(20 << 20).unwrap();
    let mut sf = Command::new("sfdisk")
        .args(["-q", "--no-reread"])
        .arg(&disk)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    sf.stdin
        .take()
        .unwrap()
        .write_all(b"label: gpt\nstart=2048, size=32768, type=L\n")
        .unwrap();
    assert!(sf.wait().unwrap().success());
    f.write_all_at(&std::fs::read(&fsimg).unwrap(), 1 << 20)
        .unwrap();
    Some(disk)
}

/// Ubuntu Server layout with real LVM (root + lvm2): p1 = /boot ext4, p2 = PV → VG/LV root ext4.
/// `cargo test -- --ignored lvm_live`
#[test]
#[ignore]
fn lvm_live() {
    let sh = |c: &str| {
        let o = Command::new("sh").args(["-c", c]).output().unwrap();
        assert!(
            o.status.success(),
            "{c}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    let d = std::env::temp_dir().join("broom_lvm_live");
    let _ = std::fs::remove_dir_all(&d);
    for (p, c) in [
        ("boot/vmlinuz-6.8.0-45-generic", "KERNEL"),
        ("boot/initrd.img-6.8.0-45-generic", "INITRD"),
        ("root/etc/fstab", "/dev/brtest/root / ext4 defaults 0 1\n"),
    ] {
        std::fs::create_dir_all(d.join(p).parent().unwrap()).unwrap();
        std::fs::write(d.join(p), c).unwrap();
    }
    let disk = d.join("disk.img");
    let ds = disk.display();
    // Kernels live at the top of the /boot partition (as in the real layout: /boot/vmlinuz → /vmlinuz there).
    sh(&format!(
        "mv {0}/boot/boot/* {0}/boot/ 2>/dev/null; truncate -s 200M {ds}",
        d.display()
    ));
    sh(&format!(
        "printf 'label: gpt\\nsize=64MiB, type=L\\ntype=E6D6D379-F507-44C2-A23C-238F2A3DF928\\n' | sfdisk -q {ds}"
    ));
    let lo = sh(&format!("losetup -P --show -f {ds}"));
    let lvm = format!("--devices {lo}p2");
    sh(&format!(
        "pvcreate -q {lvm} {lo}p2 && vgcreate -q {lvm} brtest {lo}p2 && lvcreate -q {lvm} -L 64M -n root brtest"
    ));
    sh(&format!(
        "mkfs.ext4 -q -U 99999999-8888-7777-6666-555555555555 -d {}/root /dev/brtest/root",
        d.display()
    ));
    sh(&format!("mkfs.ext4 -q -d {}/boot {lo}p1", d.display()));
    sh(&format!("vgchange -q {lvm} -an brtest && losetup -d {lo}"));

    let out = d.join("out");
    let b = extract_boot(disk.to_str().unwrap(), out.to_str().unwrap()).unwrap();
    assert_eq!(
        (b.kver.as_str(), b.root_uuid.as_str()),
        ("6.8.0-45-generic", "99999999-8888-7777-6666-555555555555")
    );
    assert_eq!(std::fs::read(out.join("initrd.img")).unwrap(), b"INITRD");
    let _ = std::fs::remove_dir_all(&d);
}

/// LVM metadata text and PV labels of an upload, mangled: parsed or refused, never a panic or a giant buffer.
#[test]
fn fuzz_lvm() {
    let meta = br#"ubuntu-vg { id = "abc" extent_size = 8192 physical_volumes { pv0 { id = "Ab-Cd" pe_start = 2048 } }
        logical_volumes { ubuntu-lv { segment1 { start_extent = 0 extent_count = 10 type = "striped" stripe_count = 1
        stripes = [ "pv0", 5 ] } } } }"#
        .to_vec();
    crate::fuzz::run("lvm metadata", crate::fuzz::inputs(&[meta], 20_000), |b| {
        let m = String::from_utf8_lossy(b);
        let _ = parse_meta(&m);
        let _ = lvm_volumes("/dev/null", &[("AbCd".into(), 1 << 20)], &m);
    });
    // A plausible label + metadata area (as in crafted_lvm_label_refused, with sane sizes).
    let mut label = vec![0u8; 8192];
    label[512..520].copy_from_slice(b"LABELONE");
    label[536..544].copy_from_slice(b"LVM2 001");
    label[532..536].copy_from_slice(&32u32.to_le_bytes());
    let pvh = 512 + 32;
    label[pvh..pvh + 32].copy_from_slice(b"AbCdAbCdAbCdAbCdAbCdAbCdAbCdAbCd");
    label[pvh + 56..pvh + 64].copy_from_slice(&4096u64.to_le_bytes());
    label[pvh + 64..pvh + 72].copy_from_slice(&4096u64.to_le_bytes());
    label[4096 + 4..4096 + 20].copy_from_slice(b" LVM2 x[5A%r0N*>");
    label[4096 + 40..4096 + 48].copy_from_slice(&512u64.to_le_bytes());
    label[4096 + 48..4096 + 56].copy_from_slice(&200u64.to_le_bytes());
    let p = std::env::temp_dir().join("broom_t_lvm_fuzz");
    crate::fuzz::run("lvm label", crate::fuzz::inputs(&[label], 3_000), |b| {
        std::fs::write(&p, b).unwrap();
        let _ = lvm_pv(&File::open(&p).unwrap(), 0);
    });
    let _ = std::fs::remove_file(&p);
}
