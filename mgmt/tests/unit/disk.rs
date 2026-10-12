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
