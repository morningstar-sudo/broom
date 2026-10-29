use super::*;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;

const MB: u64 = 1 << 20;

fn iowr(t: u32, nr: u32, size: usize) -> u32 {
    (3 << 30) | ((size as u32) << 16) | (t << 8) | nr
}

/// The kernel structs and ioctl numbers written by hand match linux/fs.h and linux/fiemap.h.
#[test]
fn kernel_interface() {
    assert_eq!(std::mem::offset_of!(DedupeRange, dest_fd), 24);
    assert_eq!(std::mem::size_of::<DedupeRange>(), 24 + 32);
    assert_eq!(std::mem::size_of::<FiemapExtent>(), 56);
    assert_eq!(std::mem::offset_of!(Fiemap, extents), 32);
    assert_eq!(FIDEDUPERANGE as u32, iowr(0x94, 54, 24));
    assert_eq!(FS_IOC_FIEMAP as u32, iowr(b'f' as u32, 11, 32));
}

fn s(n: u8) -> Sum {
    [n; 16]
}

#[test]
fn segments_match_runs() {
    let min = 2 * BLOCK;
    // blocks: 0-2 same, 3 differs, 4 same alone (too short), 5 hole in both, 6-7 same, 8.. only in src.
    let src = [s(1), s(2), s(3), s(4), s(5), HOLE, s(6), s(7), s(8)];
    let dst = [s(1), s(2), s(3), s(9), s(5), HOLE, s(6), s(7)];
    assert_eq!(
        segments(&src, &dst, min),
        [(0, 3 * BLOCK), (6 * BLOCK, 2 * BLOCK)]
    );
    // A run up to the very end; a single block when the threshold allows it.
    assert_eq!(segments(&src, &dst, BLOCK)[1], (4 * BLOCK, BLOCK));
    assert_eq!(segments(&[s(1); 4], &[s(1); 4], min), [(0, 4 * BLOCK)]);
    assert!(
        segments(&[HOLE; 4], &[HOLE; 4], BLOCK).is_empty(),
        "holes never match"
    );
    assert!(segments(&[], &[s(1)], BLOCK).is_empty());
}

#[test]
fn subtract_ranges() {
    let segs = [(0, 100), (200, 100), (400, 50)];
    assert_eq!(subtract(&segs, &[]), segs);
    assert_eq!(
        subtract(&segs, &[(10, 20), (90, 150), (420, 100)]),
        [(0, 10), (30, 60), (240, 60), (400, 20)]
    );
    assert!(subtract(&segs, &[(0, 1000)]).is_empty());
    // One range of `minus` over two segments.
    assert_eq!(
        subtract(&[(0, 10), (20, 10)], &[(5, 20)]),
        [(0, 5), (25, 5)]
    );
}

#[test]
fn shared_extents() {
    let e = |logical, physical, len| Ext {
        logical,
        physical,
        len,
        shared: false,
    };
    // dst: one extent; src: the first half at the same blocks, the second half elsewhere.
    let dst = [e(0, 1000, 100)];
    let src = [e(0, 1000, 50), e(50, 9000, 50)];
    assert_eq!(shared(&dst, &src), [(0, 50)]);
    // Same physical at another offset is no match; adjacent matches merge.
    assert!(shared(&[e(0, 1000, 10)], &[e(10, 1000, 10)]).is_empty());
    assert_eq!(
        shared(&[e(0, 500, 10), e(10, 510, 10)], &[e(0, 500, 20)]),
        [(0, 20)]
    );
}

#[test]
fn index_roundtrip() {
    let st = Stamp {
        size: 3 * BLOCK + 7,
        mtime_ns: 1_700_000_000_123_456_789,
        ino: 42,
    };
    let sums = [s(1), HOLE, s(3)];
    let b = encode(st, &sums);
    assert_eq!(decode(&b, st).unwrap(), sums);
    for other in [
        Stamp { ino: 43, ..st },
        Stamp {
            mtime_ns: st.mtime_ns + 1,
            ..st
        },
        Stamp {
            size: 4 * BLOCK,
            ..st
        },
    ] {
        assert!(decode(&b, other).is_none(), "{other:?}");
    }
    assert!(decode(&b[..b.len() - 1], st).is_none(), "cut short");
    assert!(decode(&b[..10], st).is_none());
    let mut bad = b.clone();
    bad[0] ^= 1;
    assert!(decode(&bad, st).is_none());
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("broom_dedup_{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub(crate) fn random(len: u64, seed: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; len as usize];
    blake3::Hasher::new()
        .update(seed)
        .finalize_xof()
        .fill(&mut b);
    b
}

/// Blocks in holes are not read and never match; equal blocks get equal sums; a block of written zeros is data.
#[test]
fn block_sums() {
    let d = tmp("sums");
    let p = d.join("f");
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&p)
        .unwrap();
    f.set_len(16 * BLOCK + 100).unwrap();
    let a = random(BLOCK, b"a");
    f.write_all_at(&a, 2 * BLOCK).unwrap();
    f.write_all_at(&a, 5 * BLOCK).unwrap();
    f.write_all_at(&vec![0u8; BLOCK as usize], 9 * BLOCK)
        .unwrap();
    f.write_all_at(b"tail", 16 * BLOCK).unwrap(); // the last partial block: never shared
    let sums = hash_blocks(&f, 16 * BLOCK + 100).unwrap();
    assert_eq!(sums.len(), 16);
    assert_eq!(sums[2], sums[5]);
    assert_ne!(sums[2], HOLE);
    // Only where the filesystem reports a hole (some report data everywhere: then zeros are hashed, also fine).
    if crate::disk::file_data_ranges(&f, 0, 16 * BLOCK).len() > 1 {
        assert_eq!(sums[0], HOLE);
    }
    assert_ne!(sums[9], HOLE, "written zeros are data");
    drop(f);
    // The index next to it: written once, read back; a file that grows is hashed again.
    let side = Side::open(&p).unwrap();
    assert_eq!(super::sums(&side).unwrap(), sums);
    assert!(index_of(&p).exists());
    std::fs::write(
        index_of(&p),
        encode(stamp(&side.file).unwrap(), &[s(7); 16]),
    )
    .unwrap();
    assert_eq!(super::sums(&side).unwrap(), [s(7); 16], "from the index");
    side.file.write_all_at(&a, 16 * BLOCK).unwrap();
    assert_eq!(super::sums(&side).unwrap().len(), 17, "grown → read again");
    let _ = std::fs::remove_dir_all(&d);
}

/// A version that is also some image's image.img (a new image made from it) is never deduped: it is served.
#[test]
fn never_into_a_served_golden() {
    let d = tmp("served");
    let (v1, served, golden) = (d.join("v1.img"), d.join("image.img"), d.join("new.img"));
    std::fs::write(&v1, vec![1u8; 4096]).unwrap();
    std::fs::hard_link(&v1, &served).unwrap();
    std::fs::write(&golden, vec![1u8; 4096]).unwrap();
    let r = dedupe(&Side::open(&v1).unwrap(), &Side::open(&golden).unwrap());
    assert_eq!(r, Ok(Report::default()));
    assert!(!index_of(&v1).exists(), "not even read");
    let _ = std::fs::remove_dir_all(&d);
}

/// The probe leaves no file behind, and gives the same answer twice (from the cache the second time).
#[test]
fn probe_leaves_nothing() {
    let d = tmp("probe");
    let first = supported(&d);
    assert_eq!(supported(&d), first);
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
    assert!(!supported(&d.join("missing")));
    let _ = std::fs::remove_dir_all(&d);
}

// ---------- live: real filesystems on loop devices (root) ----------

const LOOP_SET_FD: libc::Ioctl = 0x4C00 as libc::Ioctl;
const LOOP_CLR_FD: libc::Ioctl = 0x4C01 as libc::Ioctl;
const LOOP_CTL_GET_FREE: libc::Ioctl = 0x4C82 as libc::Ioctl;
pub(crate) const XFS: &[&str] = &["mkfs.xfs", "-f", "-q", "-m", "reflink=1"];
const EXT4: &[&str] = &["mkfs.ext4", "-F", "-q"];

/// A fresh filesystem on a 2 GB file, on a loop device (set up here, no losetup), mounted at `mnt`. Gone on drop.
pub(crate) struct Fs {
    dir: PathBuf,
    pub mnt: PathBuf,
    lo: File,
}

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap()
}

impl Fs {
    /// None = not root, or no such mkfs on this machine (skipped, said on stderr).
    pub(crate) fn new(fstype: &str, mkfs: &[&str]) -> Option<Fs> {
        // SAFETY: plain getter.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: needs root");
            return None;
        }
        let dir =
            std::env::temp_dir().join(format!("broom_dedup_live_{fstype}_{}", std::process::id()));
        let mnt = dir.join("mnt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&mnt).unwrap();
        let img = dir.join("fs.img");
        // Sparse; big enough for need_space's 1 GB margin (a rollback's rebuild).
        File::create(&img).unwrap().set_len(2048 * MB).unwrap();
        match std::process::Command::new(mkfs[0])
            .args(&mkfs[1..])
            .arg(&img)
            .output()
        {
            Err(_) => {
                eprintln!("skipped: no {}", mkfs[0]);
                let _ = std::fs::remove_dir_all(&dir);
                return None;
            }
            Ok(o) => assert!(
                o.status.success(),
                "{}: {}",
                mkfs[0],
                String::from_utf8_lossy(&o.stderr)
            ),
        }
        let back = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&img)
            .unwrap();
        let ctl = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/loop-control")
            .unwrap();
        // Another test may take the same free device first: try again with the next one.
        let (dev, lo) = (0..20)
            .find_map(|_| {
                // SAFETY: ioctl without argument on /dev/loop-control.
                let n = unsafe { libc::ioctl(ctl.as_raw_fd(), LOOP_CTL_GET_FREE) };
                assert!(
                    n >= 0,
                    "LOOP_CTL_GET_FREE: {}",
                    std::io::Error::last_os_error()
                );
                let dev = format!("/dev/loop{n}");
                let lo = OpenOptions::new().read(true).write(true).open(&dev).ok()?;
                // SAFETY: the argument is an open fd.
                let r = unsafe {
                    libc::ioctl(
                        lo.as_raw_fd(),
                        LOOP_SET_FD,
                        back.as_raw_fd() as libc::c_ulong,
                    )
                };
                (r == 0).then_some((dev, lo))
            })
            .expect("no free loop device");
        let (src, dst, ty) = (cstr(&dev), cstr(mnt.to_str().unwrap()), cstr(fstype));
        // SAFETY: valid NUL-terminated strings; no data argument.
        let r =
            unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), ty.as_ptr(), 0, std::ptr::null()) };
        let fs = Fs { dir, mnt, lo };
        assert_eq!(r, 0, "mount {dev}: {}", std::io::Error::last_os_error());
        Some(fs)
    }

    /// Free bytes, after flushing what was freed.
    fn free(&self) -> u64 {
        let f = File::open(&self.mnt).unwrap();
        // SAFETY: a plain C struct, filled below; valid fd and pointer.
        let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
        unsafe {
            libc::syncfs(f.as_raw_fd());
            assert_eq!(libc::fstatvfs(f.as_raw_fd(), &mut sv), 0);
        }
        sv.f_bavail as u64 * sv.f_frsize as u64
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        let m = cstr(self.mnt.to_str().unwrap());
        // SAFETY: valid path; the loop fd is open.
        unsafe {
            libc::umount2(m.as_ptr(), 0);
            libc::ioctl(self.lo.as_raw_fd(), LOOP_CLR_FD);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `data` at 0 of a new file of `size` bytes (the rest a hole), flushed.
pub(crate) fn write_file(p: &Path, size: u64, data: &[u8]) {
    let f = File::create(p).unwrap();
    f.set_len(size).unwrap();
    f.write_all_at(data, 0).unwrap();
    f.sync_all().unwrap();
}

pub(crate) fn file_hash(p: &Path) -> String {
    blake3::hash(&std::fs::read(p).unwrap())
        .to_hex()
        .to_string()
}

pub(crate) fn drop_caches() {
    // SAFETY: plain call.
    unsafe { libc::sync() };
    std::fs::write("/proc/sys/vm/drop_caches", "3").unwrap();
}

/// Where a file's data is on the disk, as runs contiguous on it (FIEMAP also splits where sharing starts or ends): the
/// served golden's must never move.
pub(crate) fn layout(p: &Path) -> Vec<(u64, u64, u64)> {
    let mut out: Vec<(u64, u64, u64)> = Vec::new();
    for e in extents(&File::open(p).unwrap()).unwrap() {
        match out.last_mut() {
            Some((l, ph, n)) if *l + *n == e.logical && *ph + *n == e.physical => *n += e.len,
            _ => out.push((e.logical, e.physical, e.len)),
        }
    }
    out
}

/// Where offset `off` of `f` is on the disk.
fn phys_at(f: &File, off: u64) -> Option<u64> {
    extents(f)
        .unwrap()
        .into_iter()
        .find(|e| e.logical <= off && off < e.logical + e.len)
        .map(|e| e.physical + off - e.logical)
}

fn mtime(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p).unwrap().modified().unwrap()
}

fn data_map(p: &Path) -> Vec<(u64, u64)> {
    let f = File::open(p).unwrap();
    crate::disk::file_data_ranges(&f, 0, f.metadata().unwrap().len())
}

/// A golden and a version alike but for a few blocks, on XFS with reflink: the common data shared, nothing about
/// either file changed (bytes, holes, mtime, the blocks that differ), the golden's layout untouched, a second run does
/// nothing, each stays whole without the other, a punch (the Windows trim) in one leaves the other, an interrupted run
/// finishes on the next. `sudo <test binary> --ignored dedup_xfs_live`
#[test]
#[ignore]
fn dedup_xfs_live() {
    let Some(fs) = Fs::new("xfs", XFS) else {
        return;
    };
    let m = &fs.mnt;
    assert!(supported(m), "XFS with reflink=1 dedupes");
    // a: the served golden (only read); b: the version that gives its blocks back.
    let (a, b) = (m.join("image.img"), m.join("v1.img"));
    let data = random(48 * MB, b"golden");
    write_file(&a, 64 * MB, &data); // 48 MB of data, a 16 MB hole after it
    let mut edited = data.clone();
    for off in [5 * MB, 20 * MB] {
        edited[off as usize..(off + BLOCK) as usize].copy_from_slice(&random(BLOCK, b"edit"));
    }
    write_file(&b, 64 * MB, &edited);
    crate::disk::punch_hole(
        &File::options().write(true).open(&b).unwrap(),
        30 * MB,
        2 * MB,
    )
    .unwrap();
    let (ha, hb, map_b, mt_b) = (file_hash(&a), file_hash(&b), data_map(&b), mtime(&b));
    let fb = File::open(&b).unwrap();
    let edits = [5 * MB, 20 * MB].map(|o| phys_at(&fb, o));
    let layout_a = layout(&a);
    let free0 = fs.free();

    let r = dedupe(&Side::open(&b).unwrap(), &Side::open(&a).unwrap()).unwrap();
    let common = 48 * MB - 2 * MB - 2 * BLOCK;
    assert_eq!(
        r,
        Report {
            deduped: common,
            already: 0,
            differs: 0
        }
    );
    let freed = fs.free().saturating_sub(free0);
    assert!(freed >= 40 * MB, "freed only {freed} bytes");
    assert_eq!(
        (file_hash(&a), file_hash(&b)),
        (ha.clone(), hb),
        "bytes unchanged"
    );
    assert_eq!(
        layout(&a),
        layout_a,
        "the served golden's blocks never move"
    );
    assert_eq!(data_map(&b), map_b, "holes stay holes");
    assert_eq!(mtime(&b), mt_b, "mtime unchanged (hash.rs stamp)");
    assert_eq!(
        [5 * MB, 20 * MB].map(|o| phys_at(&fb, o)),
        edits,
        "edited blocks not touched"
    );
    let fa = File::open(&a).unwrap();
    assert_ne!(phys_at(&fb, 5 * MB), phys_at(&fa, 5 * MB));
    assert_eq!(phys_at(&fb, MB), phys_at(&fa, MB), "common blocks shared");
    assert!(index_of(&a).exists() && index_of(&b).exists());

    // Again: all shared already, nothing done.
    let r = dedupe(&Side::open(&b).unwrap(), &Side::open(&a).unwrap()).unwrap();
    assert_eq!((r.deduped, r.already), (0, common));

    // A punch in the golden (the Windows trim) leaves the version as it was.
    let hb = file_hash(&b);
    crate::disk::punch_hole(&File::options().write(true).open(&a).unwrap(), 10 * MB, MB).unwrap();
    assert_eq!(file_hash(&b), hb);
    // Each whole on its own: the golden gone (the next upload replaced it), the version read back from the disk.
    drop((fa, fb));
    std::fs::remove_file(&a).unwrap();
    drop_caches();
    assert_eq!(file_hash(&b), hb);

    // Interrupted after one ioctl (a kill, a power cut): both whole; the next run does the rest.
    let (c, d) = (m.join("c.img"), m.join("d.img"));
    write_file(&c, 64 * MB, &data);
    write_file(&d, 64 * MB, &data);
    let (sc, sd) = (Side::open(&c).unwrap(), Side::open(&d).unwrap());
    let first = dedupe_with(&sd, &sc, MIN_LEN, 1).unwrap();
    assert!(first.deduped > 0 && first.deduped <= CALL, "{first:?}");
    assert_eq!((file_hash(&c), file_hash(&d)), (ha.clone(), ha.clone()));
    let rest = dedupe(&sd, &sc).unwrap();
    assert_eq!(
        (rest.already, rest.deduped),
        (first.deduped, 48 * MB - first.deduped)
    );
}

/// A filesystem that cannot (ext4): found by the probe, nothing read, written or changed.
/// `sudo <test binary> --ignored dedup_ext4_live`
#[test]
#[ignore]
fn dedup_ext4_live() {
    let Some(fs) = Fs::new("ext4", EXT4) else {
        return;
    };
    let m = &fs.mnt;
    assert!(!supported(m));
    let (a, b) = (m.join("v1.img"), m.join("image.img"));
    let data = random(8 * MB, b"golden");
    write_file(&a, 8 * MB, &data);
    write_file(&b, 8 * MB, &data);
    let mt = mtime(&b);
    let e = dedupe(&Side::open(&b).unwrap(), &Side::open(&a).unwrap()).unwrap_err();
    assert_eq!(e, UNSUPPORTED);
    assert!(
        !index_of(&a).exists() && !index_of(&b).exists(),
        "nothing written"
    );
    assert_eq!(mtime(&b), mt);
    let mut names: Vec<_> = std::fs::read_dir(m)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["image.img", "lost+found", "v1.img"],
        "no probe file left"
    );
}
