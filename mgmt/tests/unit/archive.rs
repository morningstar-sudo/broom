use super::*;
use std::process::Command;

#[test]
fn cpio_lists_and_extracts() {
    let d = std::env::temp_dir().join("broom_t_cpio");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("x")).unwrap();
    let gz = cpio_gz(&[
        Entry {
            path: "scripts",
            mode: 0o040755,
            data: &[],
        },
        Entry {
            path: "scripts/local-top",
            mode: 0o040755,
            data: &[],
        },
        Entry {
            path: "scripts/local-top/iscsi",
            mode: 0o100755,
            data: b"#!/bin/sh\necho hi\n",
        },
        Entry {
            path: "etc/overlayroot.conf",
            mode: 0o100644,
            data: b"x=1\n",
        },
    ]);
    std::fs::write(d.join("a.cpio.gz"), &gz).unwrap();
    let sh = "cd x && gzip -dc ../a.cpio.gz | cpio -idm --quiet 2>/dev/null; true";
    assert!(
        Command::new("sh")
            .args(["-c", sh])
            .current_dir(&d)
            .status()
            .unwrap()
            .success()
    );
    use std::os::unix::fs::PermissionsExt;
    let hook = d.join("x/scripts/local-top/iscsi");
    assert_eq!(std::fs::read(&hook).unwrap(), b"#!/bin/sh\necho hi\n");
    assert_eq!(
        std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777,
        0o755,
        "hook stays executable"
    );
    let listed = Command::new("sh")
        .args(["-c", "gzip -dc a.cpio.gz | cpio -t --quiet"])
        .current_dir(&d)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&listed.stdout).lines().count(), 4);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn tar_roundtrip_long_names_deterministic() {
    let d = std::env::temp_dir().join("broom_t_tar");
    let _ = std::fs::remove_dir_all(&d);
    let src = d.join("src");
    let long = format!(
        "{}/{}.inf",
        "very-long-directory-name".repeat(4),
        "x".repeat(60)
    );
    std::fs::create_dir_all(src.join("EFI/Microsoft/Boot")).unwrap();
    std::fs::create_dir_all(src.join(&long).parent().unwrap()).unwrap();
    std::fs::write(src.join("EFI/Microsoft/Boot/BCD"), vec![7u8; 70000]).unwrap();
    std::fs::write(src.join("empty.txt"), b"").unwrap();
    std::fs::write(src.join(&long), b"[Version]\n").unwrap();
    tar_gz(&src, &d.join("a.tar.gz")).unwrap();
    tar_gz(&src, &d.join("b.tar.gz")).unwrap();
    assert_eq!(
        std::fs::read(d.join("a.tar.gz")).unwrap(),
        std::fs::read(d.join("b.tar.gz")).unwrap(),
        "deterministic"
    );
    std::fs::create_dir_all(d.join("out")).unwrap();
    assert!(
        Command::new("tar")
            .args(["-xzf", "../a.tar.gz"])
            .current_dir(d.join("out"))
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        std::fs::read(d.join("out/EFI/Microsoft/Boot/BCD")).unwrap(),
        vec![7u8; 70000]
    );
    assert_eq!(
        std::fs::read(d.join("out").join(&long)).unwrap(),
        b"[Version]\n",
        "GNU long name"
    );
    assert!(d.join("out/empty.txt").exists());
    // Our own reader (stage bundle) gets the same tree back.
    untar_gz(&d.join("a.tar.gz"), &d.join("mine")).unwrap();
    assert_eq!(
        std::fs::read(d.join("mine/EFI/Microsoft/Boot/BCD")).unwrap(),
        vec![7u8; 70000]
    );
    assert_eq!(
        std::fs::read(d.join("mine").join(&long)).unwrap(),
        b"[Version]\n"
    );
    assert!(d.join("mine/empty.txt").is_file());
    // Paths escaping the target are refused.
    for bad in ["../x", "/etc/x", "a/../../x"] {
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        header(&mut gz, bad, 0o644, 2, b'0').unwrap();
        gz.write_all(b"hi").unwrap();
        gz.write_all(&[0u8; 510 + 1024]).unwrap();
        let evil = d.join("evil.tar.gz");
        std::fs::write(&evil, gz.finish().unwrap()).unwrap();
        assert!(
            untar_gz(&evil, &d.join("evil"))
                .unwrap_err()
                .contains("unsafe path"),
            "{bad}"
        );
    }
    std::os::unix::fs::symlink("/etc/passwd", src.join("evil")).unwrap();
    assert!(
        tar_gz(&src, &d.join("c.tar.gz")).is_err(),
        "symlinks refused"
    );
    let _ = std::fs::remove_dir_all(&d);
}
