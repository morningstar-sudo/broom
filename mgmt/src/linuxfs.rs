// linuxfs.rs — read a Linux golden raw disk without mounting it (replaces libguestfs): partitions
// (disk.rs) → ext4 directly, or LVM2 PV → linear LVs → ext4 (crate ext4-view, read-only).
// Used by overlay::build_boot to copy the newest kernel + initrd and read the root UUID.
// Supported: ext2/3/4, LVM2 linear LVs (Ubuntu default). xfs/btrfs/other LVM layouts → clear error.
use ext4_view::{Ext4, Ext4Read};
use std::fs::File;
use std::os::unix::fs::FileExt;

type BoxedError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A volume = byte ranges of the raw disk mapped to one contiguous address space.
/// segs: (logical start, raw offset, length), sorted by logical start.
struct Region {
    f: File,
    segs: Vec<(u64, u64, u64)>,
}

impl Ext4Read for Region {
    fn read(&mut self, start: u64, dst: &mut [u8]) -> Result<(), BoxedError> {
        let mut done = 0usize;
        while done < dst.len() {
            let pos = start + done as u64;
            let &(ls, off, len) = self
                .segs
                .iter()
                .find(|(ls, _, len)| pos >= *ls && pos < ls + len)
                .ok_or_else(|| format!("read beyond the volume at {pos}"))?;
            let n = ((ls + len - pos) as usize).min(dst.len() - done);
            self.f.read_exact_at(&mut dst[done..done + n], off + (pos - ls))?;
            done += n;
        }
        Ok(())
    }
}

/// (start, size) in bytes of every partition; a disk without a partition table = one volume.
fn partitions(raw: &str) -> Result<Vec<(u64, u64)>, String> {
    let size = std::fs::metadata(raw).map_err(|e| format!("{raw}: {e}"))?.len();
    // Unreadable / no table → treat the whole image as one volume.
    Ok(match crate::disk::read(raw) {
        Ok(Some(t)) => t.parts.iter().map(|p| (p.start, p.size)).collect(),
        _ => vec![(0, size)],
    })
}

// ---- LVM2 (on-disk label + text metadata) ----

/// Minimal LVM2 metadata value tree.
#[derive(Debug, Clone, PartialEq)]
enum V {
    Num(i64),
    Str(String),
    List(Vec<V>),
    Sec(Vec<(String, V)>),
}

impl V {
    fn get(&self, k: &str) -> Option<&V> {
        match self {
            V::Sec(items) => items.iter().find(|(n, _)| n == k).map(|(_, v)| v),
            _ => None,
        }
    }
    fn num(&self, k: &str) -> Option<i64> {
        match self.get(k)? {
            V::Num(n) => Some(*n),
            _ => None,
        }
    }
    fn str(&self, k: &str) -> Option<&str> {
        match self.get(k)? {
            V::Str(s) => Some(s),
            _ => None,
        }
    }
    fn sections(&self) -> Vec<(&str, &V)> {
        match self {
            V::Sec(items) => items.iter().filter(|(_, v)| matches!(v, V::Sec(_))).map(|(n, v)| (n.as_str(), v)).collect(),
            _ => Vec::new(),
        }
    }
}

/// Parse LVM2 text metadata (`key = value`, `name { … }`, `[a, b]`, `# comments`).
fn parse_meta(text: &str) -> V {
    #[derive(Debug, PartialEq)]
    enum T {
        Word(String),
        Str(String),
        Sym(char),
    }
    let mut toks = Vec::new();
    let mut it = text.chars().peekable();
    while let Some(&c) = it.peek() {
        match c {
            '#' => {
                while it.next().is_some_and(|c| c != '\n') {}
            }
            '"' => {
                it.next();
                let mut s = String::new();
                while let Some(c) = it.next() {
                    match c {
                        '"' => break,
                        '\\' => s.extend(it.next()),
                        c => s.push(c),
                    }
                }
                toks.push(T::Str(s));
            }
            '{' | '}' | '=' | '[' | ']' | ',' => {
                toks.push(T::Sym(c));
                it.next();
            }
            c if c.is_whitespace() => {
                it.next();
            }
            _ => {
                let mut w = String::new();
                while let Some(&c) = it.peek() {
                    if c.is_whitespace() || "{}=[],\"#".contains(c) {
                        break;
                    }
                    w.push(c);
                    it.next();
                }
                toks.push(T::Word(w));
            }
        }
    }
    fn value(toks: &[T], i: &mut usize) -> V {
        match toks.get(*i) {
            Some(T::Str(s)) => {
                *i += 1;
                V::Str(s.clone())
            }
            Some(T::Word(w)) => {
                *i += 1;
                w.parse().map(V::Num).unwrap_or_else(|_| V::Str(w.clone()))
            }
            Some(T::Sym('[')) => {
                *i += 1;
                let mut l = Vec::new();
                while *i < toks.len() && toks[*i] != T::Sym(']') {
                    if toks[*i] == T::Sym(',') {
                        *i += 1;
                    } else {
                        l.push(value(toks, i));
                    }
                }
                *i += 1;
                V::List(l)
            }
            _ => {
                *i += 1;
                V::Str(String::new())
            }
        }
    }
    fn section(toks: &[T], i: &mut usize) -> V {
        let mut items = Vec::new();
        while *i < toks.len() {
            match &toks[*i] {
                T::Sym('}') => {
                    *i += 1;
                    break;
                }
                T::Word(name) => {
                    let name = name.clone();
                    *i += 1;
                    match toks.get(*i) {
                        Some(T::Sym('{')) => {
                            *i += 1;
                            items.push((name, section(toks, i)));
                        }
                        Some(T::Sym('=')) => {
                            *i += 1;
                            items.push((name, value(toks, i)));
                        }
                        _ => {}
                    }
                }
                _ => *i += 1,
            }
        }
        V::Sec(items)
    }
    section(&toks, &mut 0)
}

/// If the partition at `start` is an LVM2 PV: (pv uuid without dashes, metadata text).
fn lvm_pv(f: &File, start: u64) -> Option<(String, String)> {
    let mut head = [0u8; 2048];
    f.read_exact_at(&mut head, start).ok()?;
    // Label in one of the first 4 sectors: "LABELONE" … type "LVM2 001" at +24, pv header at +offset_xl.
    let s = (0..4).map(|n| n * 512).find(|&s| &head[s..s + 8] == b"LABELONE" && &head[s + 24..s + 32] == b"LVM2 001")?;
    // Every offset below comes from the uploaded golden → bounds-checked (None = not a usable PV, never a panic).
    let get = |b: &[u8], o: usize, n: usize| b.get(o..o.checked_add(n)?).map(<[u8]>::to_vec);
    let le64 = |b: &[u8], o: usize| get(b, o, 8).map(|x| u64::from_le_bytes(x.try_into().unwrap()));
    let pvh = s.checked_add(u32::from_le_bytes(get(&head, s + 20, 4)?.try_into().ok()?) as usize)?;
    let uuid = String::from_utf8_lossy(&get(&head, pvh, 32)?).into_owned();
    // After uuid + device_size: data-area list (offset,size)… ended by 0,0, then metadata-area list.
    let mut o = pvh + 40;
    while le64(&head, o)? != 0 {
        o += 16;
    }
    o += 16;
    let (mda_off, mda_size) = (le64(&head, o)?, le64(&head, o + 8)?);
    if mda_off == 0 {
        return None;
    }
    // mda header: magic at +4, raw_locn[0] (offset, size) at +40, relative to the metadata area.
    let mda = start.checked_add(mda_off)?;
    let mut mh = [0u8; 512];
    f.read_exact_at(&mut mh, mda).ok()?;
    if &mh[4..20] != b" LVM2 x[5A%r0N*>" {
        return None;
    }
    let (off, size) = (le64(&mh, 40)?, le64(&mh, 48)?);
    // VG metadata text is a few KB: a bigger claim is not a real PV (and must not size a buffer).
    if size > 1 << 20 || off > mda_size {
        return None;
    }
    let size = size as usize;
    // Circular buffer: text may wrap back to just after the 512-byte header.
    let mut text = vec![0u8; size];
    let first = size.min((mda_size - off) as usize);
    f.read_exact_at(&mut text[..first], mda.checked_add(off)?).ok()?;
    if first < size {
        f.read_exact_at(&mut text[first..], mda + 512).ok()?;
    }
    Some((uuid, String::from_utf8_lossy(&text).trim_end_matches('\0').to_string()))
}

/// Linear LVs of every VG found → Regions. pvs: (uuid without dashes, partition start).
fn lvm_volumes(raw: &str, pvs: &[(String, u64)], meta: &str) -> Result<Vec<(String, Region)>, String> {
    let root = parse_meta(meta);
    let mut out = Vec::new();
    for (vg_name, vg) in root.sections() {
        let ext = vg.num("extent_size").ok_or("LVM: no extent_size")? as u64 * 512;
        // pv name in this VG ("pv0") → raw byte offset of its first extent.
        let mut pv_at = Vec::new();
        for (pv_name, pv) in vg.get("physical_volumes").map(V::sections).unwrap_or_default() {
            let id = pv.str("id").unwrap_or("").replace('-', "");
            if let Some((_, start)) = pvs.iter().find(|(u, _)| *u == id) {
                pv_at.push((pv_name, start + pv.num("pe_start").unwrap_or(0) as u64 * 512));
            }
        }
        for (lv_name, lv) in vg.get("logical_volumes").map(V::sections).unwrap_or_default() {
            let mut segs = Vec::new();
            for (_, seg) in lv.sections() {
                let (Some(se), Some(ec)) = (seg.num("start_extent"), seg.num("extent_count")) else { continue };
                let stripes = match seg.get("stripes") {
                    Some(V::List(l)) if seg.str("type") == Some("striped") && seg.num("stripe_count") == Some(1) => l,
                    _ => return Err(format!("LVM: {vg_name}/{lv_name} is not a linear LV (only linear is supported)")),
                };
                let (Some(V::Str(pv)), Some(V::Num(pe))) = (stripes.first(), stripes.get(1)) else {
                    return Err(format!("LVM: bad stripes in {vg_name}/{lv_name}"));
                };
                let base = pv_at.iter().find(|(n, _)| n == pv).map(|(_, b)| *b).ok_or(format!(
                    "LVM: {vg_name}/{lv_name} uses PV {pv} not found on this disk"
                ))?;
                segs.push((se as u64 * ext, base + *pe as u64 * ext, ec as u64 * ext));
            }
            segs.sort();
            let f = File::open(raw).map_err(|e| e.to_string())?;
            out.push((format!("{vg_name}/{lv_name}"), Region { f, segs }));
        }
    }
    Ok(out)
}

/// Every volume that may hold a filesystem: plain partitions + LVM2 linear LVs.
fn volumes(raw: &str) -> Result<Vec<(String, Region)>, String> {
    let f = File::open(raw).map_err(|e| format!("{raw}: {e}"))?;
    let mut out = Vec::new();
    let mut pvs = Vec::new();
    let mut meta = None;
    for (i, (start, size)) in partitions(raw)?.into_iter().enumerate() {
        match lvm_pv(&f, start) {
            Some((uuid, text)) => {
                pvs.push((uuid, start));
                meta = Some(text); // every PV carries the whole VG metadata; the last copy is fine
            }
            None => out.push((format!("partition {}", i + 1), Region { f: f.try_clone().map_err(|e| e.to_string())?, segs: vec![(0, start, size)] })),
        }
    }
    if let Some(m) = meta {
        out.extend(lvm_volumes(raw, &pvs, &m)?);
    }
    Ok(out)
}

/// Version sort key like `sort -V`: split digits/letters, digits compare by value (5.15.0-119 > 5.15.0-91).
fn ver_key(v: &str) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut digit = false;
    for c in v.chars() {
        if !cur.is_empty() && c.is_ascii_digit() != digit {
            out.push(if digit { (cur.parse().unwrap_or(0), String::new()) } else { (0, cur.clone()) });
            cur.clear();
        }
        digit = c.is_ascii_digit();
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(if digit { (cur.parse().unwrap_or(0), String::new()) } else { (0, cur) });
    }
    out
}

/// Kernel versions ("6.8.0-45-generic") with a vmlinuz-<v> in `dir`.
fn kernels_in(fs: &Ext4, dir: &str) -> Vec<String> {
    let Ok(rd) = fs.read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| e.file_name().as_str().ok()?.strip_prefix("vmlinuz-").map(str::to_string))
        .collect()
}

pub struct Boot {
    pub kver: String,
    pub root_uuid: String,
}

/// Copy the newest kernel + its initrd to `<dst>/vmlinuz` and `<dst>/initrd.img`; return its version +
/// the UUID of the root filesystem (the one with /etc/fstab).
/// Looks on EVERY filesystem: /boot may be its own partition (Ubuntu Server LVM layout).
pub fn extract_boot(raw: &str, dst: &str) -> Result<Boot, String> {
    let mut seen = Vec::new();
    let mut best: Option<(Ext4, String, String)> = None; // (fs, dir, version)
    let mut root_uuid = None;
    for (label, region) in volumes(raw)? {
        let Ok(fs) = Ext4::load(Box::new(region)) else {
            seen.push(format!("{label}: not ext4"));
            continue;
        };
        seen.push(format!("{label}: ext4"));
        if root_uuid.is_none() && fs.exists("/etc/fstab").unwrap_or(false) {
            root_uuid = Some(format!("{:?}", fs.uuid()));
        }
        for dir in ["/", "/boot"] {
            for v in kernels_in(&fs, dir) {
                if best.as_ref().is_none_or(|(_, _, b)| ver_key(&v) > ver_key(b)) {
                    best = Some((fs.clone(), dir.to_string(), v));
                }
            }
        }
    }
    let (fs, dir, kver) = best.ok_or(format!(
        "golden has no vmlinuz-* on any ext4 filesystem ({}) — kernel installed? (xfs/btrfs are not supported)",
        seen.join(", ")
    ))?;
    let root_uuid = root_uuid.ok_or(format!("no root filesystem (/etc/fstab) found ({})", seen.join(", ")))?;
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    let dir = dir.trim_end_matches('/');
    for (src, name) in [(format!("{dir}/vmlinuz-{kver}"), "vmlinuz"), (format!("{dir}/initrd.img-{kver}"), "initrd.img")] {
        let data = fs.read(src.as_str()).map_err(|e| format!("read {src} from the golden: {e}"))?;
        std::fs::write(format!("{dst}/{name}"), data).map_err(|e| format!("write {dst}/{name}: {e}"))?;
    }
    Ok(Boot { kver, root_uuid })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

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
        let lv = vg.get("logical_volumes").unwrap().get("ubuntu-lv").unwrap().get("segment1").unwrap();
        assert_eq!(lv.get("stripes"), Some(&V::List(vec![V::Str("pv0".into()), V::Num(5)])));
        // pv "AbCd" at raw offset 1 MiB → LV starts at 1 MiB + 2048*512 + 5*8192*512.
        let segs = lvm_volumes("/dev/null", &[("AbCd".into(), 1 << 20)], m).unwrap();
        assert_eq!(segs[0].1.segs, vec![(0, (1 << 20) + 2048 * 512 + 5 * 8192 * 512, 10 * 8192 * 512)]);
    }

    /// Real ext4 in a GPT partition (mkfs.ext4 -d, no root needed): kernel + initrd + root UUID.
    #[test]
    fn ext4_in_partition() {
        let d = std::env::temp_dir().join("broom_linuxfs_test");
        let _ = std::fs::remove_dir_all(&d);
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
            .args(["-q", "-F", "-U", "11111111-2222-3333-4444-555555555555", "-d"])
            .arg(&root)
            .arg(&fsimg)
            .arg("16M")
            .stdout(std::process::Stdio::null())
            .status();
        if !ok.is_ok_and(|s| s.success()) {
            eprintln!("mkfs.ext4 unavailable — skipped");
            return;
        }
        // Disk: GPT with one partition at 1 MiB, ext4 copied in after partitioning
        // (partitioning over an existing ext4 makes sfdisk print a signature warning).
        let disk = d.join("disk.img");
        let f = File::create(&disk).unwrap();
        f.set_len(20 << 20).unwrap();
        let mut sf = Command::new("sfdisk").args(["-q", "--no-reread"]).arg(&disk).stdin(std::process::Stdio::piped()).spawn().unwrap();
        use std::io::Write;
        sf.stdin.take().unwrap().write_all(b"label: gpt\nstart=2048, size=32768, type=L\n").unwrap();
        assert!(sf.wait().unwrap().success());
        f.write_all_at(&std::fs::read(&fsimg).unwrap(), 1 << 20).unwrap();

        let out = d.join("out");
        let b = extract_boot(disk.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        assert_eq!(b.kver, "6.8.0-45-generic");
        assert_eq!(b.root_uuid, "11111111-2222-3333-4444-555555555555");
        assert_eq!(std::fs::read(out.join("vmlinuz")).unwrap(), b"KERNEL");
        assert_eq!(std::fs::read(out.join("initrd.img")).unwrap(), b"INITRD");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Ubuntu Server layout with real LVM (root + lvm2): p1 = /boot ext4, p2 = PV → VG/LV root ext4.
    /// `cargo test -- --ignored lvm_live`
    #[test]
    #[ignore]
    fn lvm_live() {
        let sh = |c: &str| {
            let o = Command::new("sh").args(["-c", c]).output().unwrap();
            assert!(o.status.success(), "{c}: {}", String::from_utf8_lossy(&o.stderr));
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
        sh(&format!("mv {0}/boot/boot/* {0}/boot/ 2>/dev/null; truncate -s 200M {ds}", d.display()));
        sh(&format!("printf 'label: gpt\\nsize=64MiB, type=L\\ntype=E6D6D379-F507-44C2-A23C-238F2A3DF928\\n' | sfdisk -q {ds}"));
        let lo = sh(&format!("losetup -P --show -f {ds}"));
        let lvm = format!("--devices {lo}p2");
        sh(&format!("pvcreate -q {lvm} {lo}p2 && vgcreate -q {lvm} brtest {lo}p2 && lvcreate -q {lvm} -L 64M -n root brtest"));
        sh(&format!("mkfs.ext4 -q -U 99999999-8888-7777-6666-555555555555 -d {}/root /dev/brtest/root", d.display()));
        sh(&format!("mkfs.ext4 -q -d {}/boot {lo}p1", d.display()));
        sh(&format!("vgchange -q {lvm} -an brtest && losetup -d {lo}"));

        let out = d.join("out");
        let b = extract_boot(disk.to_str().unwrap(), out.to_str().unwrap()).unwrap();
        assert_eq!((b.kver.as_str(), b.root_uuid.as_str()), ("6.8.0-45-generic", "99999999-8888-7777-6666-555555555555"));
        assert_eq!(std::fs::read(out.join("initrd.img")).unwrap(), b"INITRD");
        let _ = std::fs::remove_dir_all(&d);
    }
}
