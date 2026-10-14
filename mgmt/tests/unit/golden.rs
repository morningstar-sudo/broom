use super::*;

#[test]
fn unzip_nested_and_skips_escape() {
    use std::io::Write;
    let d = std::env::temp_dir().join("broom_test_unzip");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let zp = d.join("vm.zip");
    let mut w = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
    let opt =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    w.start_file("VM/disk.vmdk", opt).unwrap();
    w.write_all(b"# Disk DescriptorFile").unwrap();
    w.start_file("../escape.txt", opt).unwrap();
    w.write_all(b"x").unwrap();
    w.finish().unwrap();
    let out = d.join("out");
    super::unzip(&zp, &out).unwrap();
    assert_eq!(
        std::fs::read(out.join("VM/disk.vmdk")).unwrap(),
        b"# Disk DescriptorFile"
    );
    assert!(!d.join("escape.txt").exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn write_sparse_leaves_zero_mbs_as_holes() {
    use std::os::unix::fs::MetadataExt;
    let p = std::env::temp_dir().join("broom_test_write_sparse.img");
    // 1 MB data, 8 MB zeros, 3 bytes data, then 2 MB zeros to the end: same bytes, the zero MBs not stored.
    let mut src = vec![7u8; 1 << 20];
    src.resize(9 << 20, 0);
    src.extend(b"abc");
    src.resize((11 << 20) + 3, 0);
    let w = std::fs::File::create(&p).unwrap();
    assert_eq!(
        super::write_sparse(&mut src.as_slice(), &w).unwrap(),
        src.len() as u64
    );
    drop(w);
    assert!(std::fs::read(&p).unwrap() == src);
    let used = std::fs::metadata(&p).unwrap().blocks() * 512;
    assert!(used < 4 << 20, "zero MBs written: {used} bytes on disk");
    let _ = std::fs::remove_file(p);
}

/// Upload folder → image.img: a plain .IMG (any case) is moved; a .zip holding one is extracted
/// first; the folder is removed either way.
#[test]
fn prepare_golden_raw_and_zip() {
    use std::io::Write;
    let d = std::env::temp_dir().join("broom_test_prep");
    let _ = std::fs::remove_dir_all(&d);
    let (up, dest) = (d.join("upload"), d.join("image.img"));
    std::fs::create_dir_all(&up).unwrap();
    std::fs::write(up.join("DISK.IMG"), b"RAW").unwrap();
    super::prepare_golden(&up, &dest).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"RAW");
    assert!(!up.exists());

    std::fs::create_dir_all(&up).unwrap();
    let mut w = zip::ZipWriter::new(std::fs::File::create(up.join("vm.zip")).unwrap());
    w.start_file("VM/disk.raw", zip::write::SimpleFileOptions::default())
        .unwrap();
    w.write_all(b"ZIPPED").unwrap();
    w.finish().unwrap();
    super::prepare_golden(&up, &dest).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"ZIPPED");
    assert!(!up.exists());

    std::fs::create_dir_all(&up).unwrap();
    std::fs::write(up.join("notes.txt"), b"x").unwrap();
    assert!(
        super::prepare_golden(&up, &dest)
            .unwrap_err()
            .contains("no .vmdk")
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"ZIPPED"); // failed upload leaves the current golden alone
    assert!(!d.join("image.img.new").exists());
    let _ = std::fs::remove_dir_all(&d);
}

/// .vmx: take the disk in use (current snapshot), skip CD/ISO.
/// A descriptor pointing at an absolute path / traversal / another disk is refused; a monolithic vmdk and a
/// descriptor whose extents are plain names in the folder pass.
#[test]
fn vmdk_refs_rejects_outside_paths() {
    let d = std::env::temp_dir().join("broom_test_vmdkref");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let write = |name: &str, body: &str| {
        let p = d.join(name);
        std::fs::write(&p, body).unwrap();
        p
    };
    let ok = write(
        "good.vmdk",
        "# Disk DescriptorFile\ncreateType=\"twoGbMaxExtentSparse\"\nRW 4192256 SPARSE \"good-s001.vmdk\"\n",
    );
    assert!(super::vmdk_refs_safe(&ok).is_ok());
    // monolithic (binary magic, not a descriptor) → passes (no extent lines)
    let mono = write("mono.vmdk", "KDMV\x01\x00\x00\x00 binary sparse header");
    assert!(super::vmdk_refs_safe(&mono).is_ok());
    for bad in [
        "# Disk DescriptorFile\nRW 1 FLAT \"/dev/sda\" 0\n",
        "# Disk DescriptorFile\nRW 1 FLAT \"../../etc/passwd\" 0\n",
        "# Disk DescriptorFile\nRW 1 FLAT \"sub/disk.vmdk\" 0\n",
        "# Disk DescriptorFile\nparentFileNameHint=\"/root/secret.img\"\n",
    ] {
        let p = write("bad.vmdk", bad);
        assert!(super::vmdk_refs_safe(&p).is_err(), "{bad}");
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn vmx_current_disk() {
    let vmx = "displayName = \"win\"\n\
                   sata0:1.fileName = \"D:\\\\iso\\\\win11.iso\"\n\
                   nvme0:0.fileName = \"win-000003.vmdk\"\n\
                   nvme0:0.present = \"TRUE\"\n";
    assert_eq!(vmx_disk(vmx).unwrap(), "win-000003.vmdk");
    assert_eq!(vmx_disk("sata0:1.fileName = \"auto detect\"\n"), None);
}

/// VM with snapshots: pick the top of the chain (000002), not the base file whose name sorts "higher".
#[test]
fn pick_vmdk_snapshot_top() {
    let d = std::env::temp_dir().join("broom_test_snap");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let w = |n: &str, parent: &str| {
        let hint = if parent.is_empty() {
            String::new()
        } else {
            format!("parentFileNameHint=\"C:\\VMs\\win\\{parent}\"\n")
        };
        std::fs::write(
            d.join(n),
            format!("# Disk DescriptorFile\ncreateType=\"twoGbMaxExtentSparse\"\n{hint}"),
        )
        .unwrap();
        d.join(n)
    };
    let v = vec![
        w("win.vmdk", ""),
        w("win-000001.vmdk", "win.vmdk"),
        w("win-000002.vmdk", "win-000001.vmdk"),
    ];
    let mut all = v.clone();
    std::fs::write(d.join("win-s001.vmdk"), b"KDMV\x01binary").unwrap();
    all.push(d.join("win-s001.vmdk"));
    assert_eq!(pick_vmdk(&all).unwrap(), d.join("win-000002.vmdk"));
    assert_eq!(pick_vmdk(&v[..2]).unwrap(), d.join("win-000001.vmdk"));
    assert_eq!(pick_vmdk(&v[..1]).unwrap(), d.join("win.vmdk"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn vmdk_descriptor_detect() {
    let dir = std::env::temp_dir();
    // descriptor = text.
    let d = dir.join("broom_test_desc.vmdk");
    std::fs::write(
        &d,
        "# Disk DescriptorFile\nversion=1\ncreateType=\"twoGbMaxExtentSparse\"\n",
    )
    .unwrap();
    assert!(is_vmdk_descriptor(&d));
    // extent = binary (magic KDMV) → not a descriptor.
    let e = dir.join("broom_test_ext.vmdk");
    std::fs::write(&e, b"KDMV\x01\x00\x00\x00binarygarbage").unwrap();
    assert!(!is_vmdk_descriptor(&e));
    let _ = std::fs::remove_file(&d);
    let _ = std::fs::remove_file(&e);
}
/// The first disk the .vmx names (tests; golden_from uses vmx_disks to refuse multi-disk VMs).
fn vmx_disk(vmx: &str) -> Option<String> {
    vmx_disks(vmx).into_iter().next()
}
