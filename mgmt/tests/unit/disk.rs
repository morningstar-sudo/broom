use super::*;
use std::process::Command;

#[test]
fn par_map_order_and_errors() {
    let items: Vec<u32> = (0..100).collect();
    assert_eq!(
        par_map(&items, |&x| Ok(x * 2)).unwrap(),
        items.iter().map(|x| x * 2).collect::<Vec<_>>()
    );
    let e = par_map(&items, |&x| {
        if x == 37 {
            Err(format!("bad {x}"))
        } else {
            Ok(x)
        }
    })
    .unwrap_err();
    assert_eq!(e, "bad 37");
    assert!(par_map(&Vec::<u32>::new(), |&x| Ok(x)).unwrap().is_empty());
}

#[test]
fn source_segments_and_holes() {
    let d = std::env::temp_dir().join("broom_t_source");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let (a, b) = (d.join("a"), d.join("b"));
    std::fs::write(&a, b"HEAD").unwrap();
    let f = File::create(&b).unwrap();
    f.set_len(64 << 20).unwrap();
    f.write_all_at(b"DATA", 32 << 20).unwrap();
    let s = Source::new(&[(&a, 0, None), (&b, 16 << 20, Some(32 << 20))]).unwrap();
    assert_eq!(s.len, 4 + (32 << 20));
    let mut buf = [0u8; 8];
    s.read_at(2, &mut buf).unwrap();
    assert_eq!(&buf[..2], b"AD", "first segment, then zeros of the second");
    s.read_at(4 + (16 << 20), &mut buf[..4]).unwrap();
    assert_eq!(&buf[..4], b"DATA");
    let r = s.data_ranges();
    assert!(
        r.iter()
            .any(|&(x, y)| x <= 4 + (16 << 20) && y >= 8 + (16 << 20)),
        "{r:?}"
    );
    let total: u64 = r.iter().map(|(x, y)| y - x).sum();
    assert!(total < 8 << 20, "holes skipped: {r:?}");
    let _ = std::fs::remove_dir_all(&d);
}

fn sfdisk(path: &str, script: &str) {
    use std::io::Write;
    let mut c = Command::new("sfdisk")
        .args(["-q", "--no-reread", path])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    assert!(c.wait().unwrap().success(), "sfdisk {script}");
}

/// What `sfdisk -J` reports: (start, size, type) in bytes.
fn sfdisk_json(path: &str) -> (bool, Vec<(u64, u64, String)>) {
    let o = Command::new("sfdisk").args(["-J", path]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let t = &v["partitiontable"];
    let ss = t["sectorsize"].as_u64().unwrap_or(512);
    let parts = t["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| !matches!(p["type"].as_str(), Some("5" | "f" | "85")))
        .map(|p| {
            (
                p["start"].as_u64().unwrap() * ss,
                p["size"].as_u64().unwrap() * ss,
                p["type"].as_str().unwrap().to_uppercase(),
            )
        })
        .collect();
    (t["label"] == "gpt", parts)
}

fn mine(path: &str) -> (bool, Vec<(u64, u64, String)>) {
    let t = read(path).unwrap().unwrap();
    (
        t.gpt,
        t.parts
            .into_iter()
            .map(|p| (p.start, p.size, p.kind.to_uppercase()))
            .collect(),
    )
}

fn disk(name: &str, mb: u64) -> String {
    let p = std::env::temp_dir().join(format!("broom_t_disk_{name}.img"));
    let _ = std::fs::remove_file(&p);
    File::create(&p).unwrap().set_len(mb << 20).unwrap();
    p.to_string_lossy().into_owned()
}

#[test]
fn reads_like_sfdisk() {
    let g = disk("gpt", 64);
    sfdisk(
        &g,
        &format!(
            "label: gpt\nsize=8MiB, type={ESP}\nsize=1MiB, type=E3C9E316-0B5C-4DB8-817D-F92DF00215AE\ntype={BASIC_DATA}\n"
        ),
    );
    assert_eq!(mine(&g), sfdisk_json(&g));
    assert_eq!(read(&g).unwrap().unwrap().parts.len(), 3);
    let m = disk("mbr", 64);
    sfdisk(
        &m,
        "label: dos\nsize=8MiB, type=7, bootable\nsize=20MiB, type=83\ntype=5\nsize=10MiB, type=82\ntype=8e\n",
    );
    let (gpt, parts) = mine(&m);
    assert!(!gpt && parts.len() == 4, "{parts:?}");
    assert_eq!(
        (gpt, parts),
        sfdisk_json(&m),
        "primaries + logicals, extended container left out"
    );
    assert_eq!(read(&disk("empty", 4)).unwrap(), None, "no table");
    for p in [g, m] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn single_gpt_keeps_the_windows_partition() {
    let g = disk("single", 64);
    sfdisk(
        &g,
        &format!(
            "label: gpt\nsize=8MiB, type={ESP}\nsize=1MiB, type=E3C9E316-0B5C-4DB8-817D-F92DF00215AE\nsize=40MiB, type={BASIC_DATA}, name=\"Basic data partition\"\n"
        ),
    );
    let win = read(&g).unwrap().unwrap().parts[2].clone();
    let uuid = |p: &str| {
        String::from_utf8(
            Command::new("sfdisk")
                .args(["--part-uuid", p, "3"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
    };
    let before = uuid(&g);
    File::options()
        .write(true)
        .open(&g)
        .unwrap()
        .write_all_at(b"NTFS-DATA", win.start + 3)
        .unwrap();
    write_single_gpt(&g, win.start, win.size).unwrap();
    let (gpt, parts) = sfdisk_json(&g);
    assert!(gpt);
    assert_eq!(parts, vec![(win.start, win.size, BASIC_DATA.to_string())]);
    assert!(
        Command::new("sfdisk")
            .args(["--verify", &g])
            .status()
            .unwrap()
            .success(),
        "sfdisk accepts both tables"
    );
    let after = String::from_utf8(
        Command::new("sfdisk")
            .args(["--part-uuid", &g, "1"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(after, before, "partition GUID kept");
    let mut b = [0u8; 9];
    File::open(&g)
        .unwrap()
        .read_exact_at(&mut b, win.start + 3)
        .unwrap();
    assert_eq!(&b, b"NTFS-DATA", "partition contents untouched");
    let first = std::fs::read(&g).unwrap();
    write_single_gpt(&g, win.start, win.size).unwrap();
    assert!(
        std::fs::read(&g).unwrap() == first,
        "same input → same bytes"
    );
    let _ = std::fs::remove_file(g);
}

#[test]
fn guid_roundtrip() {
    let g = guid_bytes(BASIC_DATA).unwrap();
    assert_eq!(guid_str(&g), BASIC_DATA);
    assert_eq!(&g[..4], &[0xA2, 0xA0, 0xD0, 0xEB]);
}

/// A GPT disk written onto a bigger one (Deploy): the backup table goes to the new end, `sfdisk --verify` is clean,
/// the partitions stay, the free space after them becomes usable.
#[test]
fn backup_gpt_moves_to_the_end() {
    let g = disk("grow", 64);
    sfdisk(
        &g,
        "label: gpt\nsize=8MiB, type=U\ntype=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\n",
    );
    let before = mine(&g);
    File::options()
        .write(true)
        .open(&g)
        .unwrap()
        .set_len(100 << 20)
        .unwrap();
    assert!(move_backup_gpt(&g).unwrap());
    assert!(!move_backup_gpt(&g).unwrap(), "already at the end");
    assert_eq!(mine(&g), before);
    assert_eq!(sfdisk_json(&g), before);
    let o = Command::new("sfdisk")
        .args(["--verify", &g])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        o.status.success() && out.contains("No errors detected"),
        "{out}"
    );
    // "A total of N free sectors": the 36 MiB added (+ the old table's tail) are usable.
    let free: u64 = out
        .split("A total of ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(free * 512 >= 36 << 20, "{out}");
    let _ = std::fs::remove_file(&g);
}

/// Partition tables of an upload, mangled (header fields, entry counts / sizes, CRCs, an MBR's entries): read or
/// refused, never a panic or a giant buffer.
#[test]
fn fuzz_partition_tables() {
    let g = disk("fuzz_gpt", 4);
    write_single_gpt(&g, 1 << 20, 2 << 20).unwrap();
    let gpt = std::fs::read(&g).unwrap()[..64 << 10].to_vec();
    let mut mbr = vec![0u8; 4096];
    mbr[446 + 4] = 0x83; // Linux
    mbr[446 + 8..446 + 12].copy_from_slice(&2048u32.to_le_bytes());
    mbr[446 + 12..446 + 16].copy_from_slice(&4096u32.to_le_bytes());
    mbr[510..512].copy_from_slice(&[0x55, 0xaa]);
    let p = disk("fuzz_case", 0);
    crate::fuzz::run(
        "partition table",
        crate::fuzz::inputs(&[gpt, mbr], 3_000),
        |b| {
            std::fs::write(&p, b).unwrap();
            File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_len(4 << 20)
                .unwrap();
            let _ = read(&p);
        },
    );
    let _ = std::fs::remove_file(&g);
    let _ = std::fs::remove_file(&p);
}
