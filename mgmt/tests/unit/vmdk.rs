use super::*;
use std::process::Command;

fn qemu(args: &[&str]) {
    assert!(
        Command::new("qemu-img")
            .args(args)
            .status()
            .unwrap()
            .success(),
        "qemu-img {args:?}"
    );
}

/// A 70 MB raw disk with data in a few places, holes elsewhere and a length that is no grain multiple.
fn sample(d: &Path) -> PathBuf {
    let raw = d.join("disk.raw");
    let f = File::create(&raw).unwrap();
    let len = (70 << 20) + 512 * 5;
    f.set_len(len).unwrap();
    let mut seed = 0x9e37_79b9u32;
    let mut noise = |n: usize| -> Vec<u8> {
        (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect()
    };
    f.write_all_at(&noise(1000), 0).unwrap();
    f.write_all_at(&noise(3 << 20), 20 << 20).unwrap();
    f.write_all_at(b"tail", len - 4).unwrap();
    raw
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("broom_t_vmdk_{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A 1 KB crafted extent claiming 4 TiB with 1-sector grains and 1-entry tables (a 32 GiB grain directory) is
/// refused — not a giant allocation that aborts the server. Absurd offsets don't overflow either.
#[test]
fn crafted_header_refused() {
    let d = dir("crafted");
    let mut h = vec![0u8; 1024];
    h[..4].copy_from_slice(b"KDMV");
    h[12..20].copy_from_slice(&((4u64 << 40) / SECTOR).to_le_bytes()); // capacity
    h[20..28].copy_from_slice(&1u64.to_le_bytes()); // grain
    h[44..48].copy_from_slice(&1u32.to_le_bytes()); // gtes
    h[56..64].copy_from_slice(&1u64.to_le_bytes()); // gd_off
    std::fs::write(d.join("x.vmdk"), &h).unwrap();
    let out = File::create(d.join("out.raw")).unwrap();
    let e = super::sparse_to(&d.join("x.vmdk"), 4 << 40, &out, 0).unwrap_err();
    assert!(e.contains("grain directory larger"), "{e}");
    h[12..20].copy_from_slice(&8u64.to_le_bytes());
    h[56..64].copy_from_slice(&u64::MAX.to_le_bytes());
    std::fs::write(d.join("x.vmdk"), &h).unwrap();
    assert!(
        super::sparse_to(&d.join("x.vmdk"), 4096, &out, 0)
            .unwrap_err()
            .contains("corrupt VMDK header")
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// Every VMDK layout VMware/ESXi/OVA produce (made here by qemu-img) reads back to the exact raw bytes.
#[test]
fn reads_every_subformat() {
    let d = dir("read");
    let raw = sample(&d);
    let want = std::fs::read(&raw).unwrap();
    for sub in [
        "monolithicSparse",
        "monolithicFlat",
        "twoGbMaxExtentSparse",
        "twoGbMaxExtentFlat",
        "streamOptimized",
    ] {
        let sd = d.join(sub);
        std::fs::create_dir_all(&sd).unwrap();
        let v = sd.join("disk.vmdk");
        qemu(&[
            "convert",
            "-f",
            "raw",
            "-O",
            "vmdk",
            "-o",
            &format!("subformat={sub}"),
            raw.to_str().unwrap(),
            v.to_str().unwrap(),
        ]);
        assert_eq!(virtual_size(&v).unwrap(), want.len() as u64, "{sub}");
        let out = sd.join("back.raw");
        to_raw(&v, &out).unwrap();
        assert!(std::fs::read(&out).unwrap() == want, "{sub}: same bytes");
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// Grains stored in another order than on the virtual disk (how a VM that installed Windows looks) — read in file
/// order, every byte lands where it belongs; zero data stays a hole.
#[test]
fn reads_out_of_order_grains() {
    let d = dir("order");
    let v = d.join("disk.vmdk");
    qemu(&[
        "create",
        "-q",
        "-f",
        "vmdk",
        "-o",
        "subformat=monolithicSparse",
        v.to_str().unwrap(),
        "8M",
    ]);
    let io = |cmd: &str| {
        assert!(
            Command::new("qemu-io")
                .args(["-f", "vmdk", "-c", cmd, v.to_str().unwrap()])
                .output()
                .unwrap()
                .status
                .success(),
            "{cmd}"
        )
    };
    for (pat, off, len) in [
        (0x11, 6 << 20, 64 << 10),
        (0x22, 1 << 20, 192 << 10),
        (0x33, 3 << 20, 64 << 10),
        (0x00, 5 << 20, 64 << 10),
    ] {
        io(&format!("write -P {pat} {off} {len}"));
    }
    let out = d.join("back.raw");
    to_raw(&v, &out).unwrap();
    let got = std::fs::read(&out).unwrap();
    let mut want = vec![0u8; 8 << 20];
    want[6 << 20..(6 << 20) + (64 << 10)].fill(0x11);
    want[1 << 20..(1 << 20) + (192 << 10)].fill(0x22);
    want[3 << 20..(3 << 20) + (64 << 10)].fill(0x33);
    assert!(got == want, "same bytes");
    use std::os::unix::fs::MetadataExt;
    assert!(
        std::fs::metadata(&out).unwrap().blocks() * 512 <= 1 << 20,
        "zero grain + unallocated stay holes"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// Timing only: BROOM_VMDK=<file> BROOM_OUT=<raw> cargo test -- --ignored --exact vmdk::tests::bench_to_raw
#[test]
#[ignore]
fn bench_to_raw() {
    let (Ok(v), Ok(o)) = (std::env::var("BROOM_VMDK"), std::env::var("BROOM_OUT")) else {
        return;
    };
    to_raw(Path::new(&v), Path::new(&o)).unwrap();
}

#[test]
fn refuses_snapshots_and_paths() {
    let d = dir("refuse");
    let desc = |body: &str| {
        let p = d.join("x.vmdk");
        std::fs::write(
            &p,
            format!("# Disk DescriptorFile\nversion=1\nCID=12345678\n{body}"),
        )
        .unwrap();
        p
    };
    let e = virtual_size(&desc(
        "parentCID=87654321\ncreateType=\"monolithicSparse\"\nRW 100 SPARSE \"x-s001.vmdk\"\n",
    ))
    .unwrap_err();
    assert!(e.contains("snapshot"), "{e}");
    for bad in ["/dev/sda", "../server.db", "sub/x.vmdk"] {
        let e = virtual_size(&desc(&format!(
            "parentCID=ffffffff\nRW 100 FLAT \"{bad}\" 0\n"
        )))
        .unwrap_err();
        assert!(e.contains("file name"), "{bad}: {e}");
    }
    assert!(
        virtual_size(&desc("parentCID=ffffffff\nRW 100 SESPARSE \"x.vmdk\"\n"))
            .unwrap_err()
            .contains("not supported")
    );
    assert_eq!(
        virtual_size(&desc("parentCID=ffffffff\nRW 100 ZERO\n")).unwrap(),
        51200
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// Export: written VMDK passes qemu-img check, qemu-img and our reader both get the raw bytes back, zero grains
/// take no space.
#[test]
fn writes_monolithic_sparse() {
    let d = dir("write");
    let raw = sample(&d);
    let v = d.join("disk.vmdk");
    write_sparse(&Source::file(&raw).unwrap(), &v).unwrap();
    qemu(&["check", "-q", "-f", "vmdk", v.to_str().unwrap()]);
    let back = d.join("back.raw");
    qemu(&[
        "convert",
        "-f",
        "vmdk",
        "-O",
        "raw",
        v.to_str().unwrap(),
        back.to_str().unwrap(),
    ]);
    let want = std::fs::read(&raw).unwrap();
    assert!(
        std::fs::read(&back).unwrap() == want,
        "qemu-img reads it back"
    );
    let mine = d.join("mine.raw");
    to_raw(&v, &mine).unwrap();
    assert!(std::fs::read(&mine).unwrap() == want, "our reader too");
    assert!(
        std::fs::metadata(&v).unwrap().len() < 5 << 20,
        "only the written grains are stored"
    );
    let _ = std::fs::remove_dir_all(&d);
}
