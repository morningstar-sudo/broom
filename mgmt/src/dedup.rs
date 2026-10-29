// dedup.rs — versions that share their common data on disk (FIDEDUPERANGE: XFS with reflink, btrfs). A new golden is
// a new file, so the version kept before it shares none of its blocks: each version took its full size. After a
// publish (images.rs) the newest version gives back the blocks it has in common with the new image.img: the kernel
// compares the bytes itself and points the version at the golden's copy. Only the snapshot store changes: the served
// image.img is never the file deduped (its blocks, its layout stay as they were — clients load it as before), the
// version is. Each file stays a whole raw disk on its own — no parent/child chain: deleting either never touches the
// other, a later write (trim) to one copies just that block. Nothing in any file changes: bytes, size, holes and mtime
// (the stamp hash.rs and the Windows golden.id go by). A filesystem that cannot (ext4) is found by trying it (two
// small files), never guessed from its name; then nothing happens at all.
// What is compared: blake3 per 64 KB block, only where the file has data, kept in `<file>.bhash` next to it (this
// image.img is the version of next time: never read twice), matched at the same offset (Deploy → edit → Capture
// keeps the partition layout), consecutive matches merged, runs under MIN_LEN left alone (fragments cost more in
// extents than they save). Ranges both already share (FIEMAP: same physical block at the same offset) are skipped →
// a second run, or one after an interrupted run (each ioctl is all or nothing), only does what is left.
// Runs at idle I/O priority on its own thread, busy at most half the time: the iSCSI daemon comes first.
// A deduped version is in pieces on the disk. Whatever is served is in one piece (clients read the golden start to
// end): every publish first rebuilds an image.img in pieces (make_whole: a rollback, a new image from a version) — a
// copy that becomes the version's file too — and the other versions are then deduped against it. The time goes to
// the admin's job, never to the clients.
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::SharedState;

/// Compared per block of this size (a multiple of any filesystem block size).
const BLOCK: u64 = 64 << 10;
/// Shortest run of matching blocks worth sharing.
const MIN_LEN: u64 = 1 << 20;
/// Most bytes per ioctl (about the kernel's own cap): the files' locks are never held longer than that takes.
const CALL: u64 = 16 << 20;
/// Read size while hashing.
const PIECE: u64 = 8 << 20;

pub(crate) const UNSUPPORTED: &str = "this filesystem cannot share data between files (XFS with reflink or btrfs can) — nothing to do";

/// A block's sum: blake3 cut to 16 bytes (the kernel compares the bytes; a collision only costs a wasted ioctl).
type Sum = [u8; 16];
/// The sum of a block in a hole (never matches).
const HOLE: Sum = [0; 16];

// ---------- kernel interface (linux/fs.h, linux/fiemap.h — not in libc for every target) ----------

/// _IOWR(0x94, 54, struct file_dedupe_range): its 24-byte header.
const FIDEDUPERANGE: libc::Ioctl = 0xC018_9436u32 as libc::Ioctl;
/// _IOWR('f', 11, struct fiemap): its 32-byte header.
const FS_IOC_FIEMAP: libc::Ioctl = 0xC020_660Bu32 as libc::Ioctl;
const FILE_DEDUPE_RANGE_DIFFERS: i32 = 1;
const FIEMAP_FLAG_SYNC: u32 = 0x1;
const FIEMAP_EXTENT_LAST: u32 = 0x1;
const FIEMAP_EXTENT_SHARED: u32 = 0x2000;
/// Extents whose physical address means nothing: unknown, delayed allocation, encoded, inline.
const FIEMAP_EXTENT_NO_ADDR: u32 = 0x2 | 0x4 | 0x8 | 0x200;
const FIEMAP_EXTENTS: usize = 256;
const IOPRIO_WHO_PROCESS: libc::c_int = 1;
/// IOPRIO_PRIO_VALUE(IOPRIO_CLASS_IDLE, 0).
const IOPRIO_IDLE: libc::c_int = 3 << 13;

/// struct file_dedupe_range with its one struct file_dedupe_range_info.
#[repr(C)]
struct DedupeRange {
    src_offset: u64,
    src_length: u64,
    dest_count: u16,
    reserved1: u16,
    reserved2: u32,
    dest_fd: i64,
    dest_offset: u64,
    bytes_deduped: u64,
    status: i32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FiemapExtent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct Fiemap {
    start: u64,
    length: u64,
    flags: u32,
    mapped_extents: u32,
    extent_count: u32,
    reserved: u32,
    extents: [FiemapExtent; FIEMAP_EXTENTS],
}

/// [dst_off, +len) of `dst` made to share `src`'s blocks at src_off, when the bytes are equal (the kernel checks).
/// Ok(Some(bytes done)) — maybe fewer than asked; Ok(None) = they differ.
fn dedupe_range(
    src: &File,
    src_off: u64,
    dst: &File,
    dst_off: u64,
    len: u64,
) -> std::io::Result<Option<u64>> {
    let mut r = DedupeRange {
        src_offset: src_off,
        src_length: len,
        dest_count: 1,
        reserved1: 0,
        reserved2: 0,
        dest_fd: dst.as_raw_fd() as i64,
        dest_offset: dst_off,
        bytes_deduped: 0,
        status: 0,
        reserved: 0,
    };
    // SAFETY: r is a file_dedupe_range with room for the one info its dest_count says; both fds are open.
    if unsafe { libc::ioctl(src.as_raw_fd(), FIDEDUPERANGE, &mut r as *mut DedupeRange) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    match r.status {
        0 => Ok(Some(r.bytes_deduped)),
        FILE_DEDUPE_RANGE_DIFFERS => Ok(None),
        e => Err(std::io::Error::from_raw_os_error(-e)),
    }
}

/// [logical, +len) of a file at `physical` on its disk; `shared` with another file.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Ext {
    logical: u64,
    physical: u64,
    len: u64,
    shared: bool,
}

/// The extents of a file that have a real address (FIEMAP, flushed first), sorted.
fn extents(f: &File) -> std::io::Result<Vec<Ext>> {
    let mut m = Box::new(Fiemap {
        start: 0,
        length: 0,
        flags: FIEMAP_FLAG_SYNC,
        mapped_extents: 0,
        extent_count: FIEMAP_EXTENTS as u32,
        reserved: 0,
        extents: [FiemapExtent::default(); FIEMAP_EXTENTS],
    });
    let mut out = Vec::new();
    loop {
        m.length = u64::MAX - m.start;
        m.mapped_extents = 0;
        // SAFETY: m is a fiemap with room for the extent_count extents it says; f is open.
        if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FIEMAP, &mut *m as *mut Fiemap) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let n = (m.mapped_extents as usize).min(FIEMAP_EXTENTS);
        let Some(last) = m.extents[..n].last().copied() else {
            break;
        };
        out.extend(
            m.extents[..n]
                .iter()
                .filter(|e| e.flags & FIEMAP_EXTENT_NO_ADDR == 0)
                .map(|e| Ext {
                    logical: e.logical,
                    physical: e.physical,
                    len: e.length,
                    shared: e.flags & FIEMAP_EXTENT_SHARED != 0,
                }),
        );
        let next = last.logical + last.length;
        if last.flags & FIEMAP_EXTENT_LAST != 0 || next <= m.start {
            break;
        }
        m.start = next;
        m.flags = 0; // flushed once is enough
    }
    Ok(out)
}

/// Byte ranges where `a` and `b` already use the same blocks (the same physical address at the same offset), merged.
fn shared(a: &[Ext], b: &[Ext]) -> Vec<(u64, u64)> {
    let (mut i, mut j) = (0, 0);
    let mut out: Vec<(u64, u64)> = Vec::new();
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        let lo = x.logical.max(y.logical);
        let hi = (x.logical + x.len).min(y.logical + y.len);
        if lo < hi && x.physical + (lo - x.logical) == y.physical + (lo - y.logical) {
            match out.last_mut() {
                Some((s, l)) if *s + *l == lo => *l += hi - lo,
                _ => out.push((lo, hi - lo)),
            }
        }
        if x.logical + x.len <= y.logical + y.len {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// Runs of blocks with the same sum at the same index in both (a hole never matches), at least `min` bytes → byte
/// ranges, sorted.
fn segments(src: &[Sum], dst: &[Sum], min: u64) -> Vec<(u64, u64)> {
    let n = src.len().min(dst.len());
    let mut out = Vec::new();
    let mut run: Option<usize> = None;
    for i in 0..=n {
        let same = i < n && dst[i] != HOLE && dst[i] == src[i];
        match (same, run) {
            (true, None) => run = Some(i),
            (false, Some(s)) => {
                let (off, len) = (s as u64 * BLOCK, (i - s) as u64 * BLOCK);
                if len >= min {
                    out.push((off, len));
                }
                run = None;
            }
            _ => {}
        }
    }
    out
}

/// `segs` without what `minus` covers (both sorted, neither overlapping itself).
fn subtract(segs: &[(u64, u64)], minus: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut j = 0;
    for &(s, l) in segs {
        let (mut pos, end) = (s, s + l);
        while j < minus.len() && minus[j].0 + minus[j].1 <= pos {
            j += 1;
        }
        let mut k = j;
        while pos < end && k < minus.len() && minus[k].0 < end {
            let (ms, me) = (minus[k].0, minus[k].0 + minus[k].1);
            if ms > pos {
                out.push((pos, ms - pos));
            }
            pos = pos.max(me);
            k += 1;
        }
        if pos < end {
            out.push((pos, end - pos));
        }
    }
    out
}

// ---------- block sums and their index ----------

const MAGIC: &[u8; 8] = b"BRBHASH1";
/// magic, block size, 0, size, mtime ns, inode, block count.
const HEAD: usize = 48;

/// The dedup index of a file: `<file>.bhash` next to it (versions.rs gc keeps it with its file, a snapshot links it).
pub(crate) fn index_of(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".bhash");
    PathBuf::from(s)
}

/// What an index was made of: the file (inode) as it was. Any write moves the mtime; a file renamed over it is
/// another inode (a hard link — a version — is the same one, its index too).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Stamp {
    size: u64,
    mtime_ns: u64,
    ino: u64,
}

fn stamp(f: &File) -> std::io::Result<Stamp> {
    let m = f.metadata()?;
    Ok(Stamp {
        size: m.len(),
        mtime_ns: (m.mtime() as u64)
            .wrapping_mul(1_000_000_000)
            .wrapping_add(m.mtime_nsec() as u64),
        ino: m.ino(),
    })
}

fn encode(st: Stamp, sums: &[Sum]) -> Vec<u8> {
    let mut b = Vec::with_capacity(HEAD + sums.len() * 16);
    b.extend_from_slice(MAGIC);
    b.extend_from_slice(&(BLOCK as u32).to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    for v in [st.size, st.mtime_ns, st.ino, sums.len() as u64] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    for s in sums {
        b.extend_from_slice(s);
    }
    b
}

/// The sums of an index made of `st`; None = of another file, of an older state of it, or damaged.
fn decode(b: &[u8], st: Stamp) -> Option<Vec<Sum>> {
    let h = b.get(..HEAD)?;
    let n = st.size / BLOCK;
    let ok = &h[..8] == MAGIC
        && crate::disk::u32le(h, 8) as u64 == BLOCK
        && [16, 24, 32, 40].map(|o| crate::disk::u64le(h, o)) == [st.size, st.mtime_ns, st.ino, n]
        && b.len() as u64 == HEAD as u64 + n * 16;
    ok.then(|| b[HEAD..].as_chunks::<16>().0.to_vec())
}

fn sum(b: &[u8]) -> Sum {
    let mut s: Sum = blake3::hash(b).as_bytes()[..16].try_into().unwrap();
    if s == HOLE {
        s[15] = 1;
    }
    s
}

/// Sums of every whole block of `f` (`size` bytes; a last partial block is never shared). Blocks of holes are not read.
fn hash_blocks(f: &File, size: u64) -> Result<Vec<Sum>, String> {
    let n = size / BLOCK;
    let mut sums = vec![HOLE; n as usize];
    let mut buf = vec![0u8; PIECE as usize];
    for (a, b) in crate::disk::file_data_ranges(f, 0, n * BLOCK) {
        let (mut blk, end) = (a / BLOCK, b.div_ceil(BLOCK).min(n));
        while blk < end {
            let k = (end - blk).min(PIECE / BLOCK);
            let buf = &mut buf[..(k * BLOCK) as usize];
            f.read_exact_at(buf, blk * BLOCK)
                .map_err(|e| format!("read at {}: {e}", blk * BLOCK))?;
            for (i, c) in buf.chunks(BLOCK as usize).enumerate() {
                sums[blk as usize + i] = sum(c);
            }
            blk += k;
        }
    }
    Ok(sums)
}

/// A file being deduped: opened once (under versions_lock), so a version deleted meanwhile stays whole to the end.
pub(crate) struct Side {
    path: PathBuf,
    file: File,
}

impl Side {
    pub(crate) fn open(p: &Path) -> Result<Side, String> {
        // Read-write: the kernel changes a dedupe's destination only through a writable fd (or for root). Nothing
        // here writes.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(p)
            .map_err(ioerr(p))?;
        Ok(Side {
            path: p.to_path_buf(),
            file,
        })
    }

    fn dev(&self) -> u64 {
        self.file.metadata().map_or(0, |m| m.dev())
    }

    fn is(&self, p: &Path) -> bool {
        match (self.file.metadata(), std::fs::metadata(p)) {
            (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
            _ => false,
        }
    }

    fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }
}

fn ioerr(p: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("{}: {e}", p.display())
}

/// The block sums of a file: from its index while that still describes it, else read (and the index written).
fn sums(s: &Side) -> Result<Vec<Sum>, String> {
    let st = stamp(&s.file).map_err(ioerr(&s.path))?;
    let idx = index_of(&s.path);
    if let Some(v) = std::fs::read(&idx).ok().and_then(|b| decode(&b, st)) {
        return Ok(v);
    }
    let v = hash_blocks(&s.file, st.size).map_err(|e| format!("{}: {e}", s.path.display()))?;
    // Labelled with the state it was read in: a write meanwhile moved the mtime → read again next time.
    if let Err(e) = crate::versions::write_durable(&idx, &encode(st, &v)) {
        tracing::warn!("{}: {e}", idx.display()); // only a cache
    }
    Ok(v)
}

// ---------- does this filesystem do it ----------

/// Filesystems (device, fsid) tried already → whether they can.
static TRIED: std::sync::Mutex<Vec<((u64, u64), bool)>> = std::sync::Mutex::new(Vec::new());

/// Can the filesystem holding `dir` share blocks between files? Tried once per filesystem. A folder that can't be
/// written to → false, and tried again next time.
pub(crate) fn supported(dir: &Path) -> bool {
    let Some(key) = fs_key(dir) else {
        return false;
    };
    let tried = || TRIED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, ok)) = tried().iter().find(|(k, _)| *k == key) {
        return ok;
    }
    let Some(ok) = probe(dir) else {
        return false;
    };
    if !ok {
        tracing::info!(
            "{}: this filesystem cannot share data between files → versions keep their full size",
            dir.display()
        );
    }
    tried().push((key, ok));
    ok
}

/// (device, fsid): a device number reused by another filesystem later (formatted again) is not taken for the old one.
fn fs_key(dir: &Path) -> Option<(u64, u64)> {
    let f = File::open(dir).ok()?;
    // SAFETY: a plain C struct, filled by fstatvfs below.
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid fd and pointer.
    if unsafe { libc::fstatvfs(f.as_raw_fd(), &mut sv) } != 0 {
        return None;
    }
    Some((f.metadata().ok()?.dev(), sv.f_fsid as u64))
}

/// Two small files with the same bytes in `dir`, unlinked at once (only their fds hold them), deduped: Some(it
/// worked); None = they could not be made.
fn probe(dir: &Path) -> Option<bool> {
    let blk = std::fs::metadata(dir).ok()?.blksize().clamp(512, BLOCK);
    let data = vec![0xA5u8; blk as usize];
    let make = |tag: &str| -> std::io::Result<File> {
        let p = dir.join(format!(".dedup-probe-{}-{tag}", std::process::id()));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&p)?;
        let _ = std::fs::remove_file(&p);
        f.write_all_at(&data, 0)?;
        f.sync_data()?;
        Ok(f)
    };
    let (a, b) = (make("a").ok()?, make("b").ok()?);
    Some(matches!(dedupe_range(&a, 0, &b, 0, blk), Ok(Some(n)) if n == blk))
}

// ---------- one pass ----------

/// What a pass did, in bytes.
#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub(crate) struct Report {
    /// Shared by this pass.
    pub deduped: u64,
    /// Matching ranges the two files shared already.
    pub already: u64,
    /// Ranges the sums said match but the bytes did not (an index out of date: both dropped, read again next time).
    pub differs: u64,
}

impl Report {
    fn add(&mut self, r: Report) {
        self.deduped += r.deduped;
        self.already += r.already;
        self.differs += r.differs;
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{:.1} GB shared ({:.1} GB by this pass)",
            (self.deduped + self.already) as f64 / 1e9,
            self.deduped as f64 / 1e9
        )
    }
}

/// `dst` shares with `src` the blocks they have in common (at the same offset). The same file → nothing to do.
pub(crate) fn dedupe(dst: &Side, src: &Side) -> Result<Report, String> {
    dedupe_with(dst, src, MIN_LEN, usize::MAX)
}

/// `dedupe`, sharing runs of at least `min` bytes, in at most `max_calls` ioctls (a test stops it midway).
fn dedupe_with(dst: &Side, src: &Side, min: u64, max_calls: usize) -> Result<Report, String> {
    let dm = dst.file.metadata().map_err(ioerr(&dst.path))?;
    let sm = src.file.metadata().map_err(ioerr(&src.path))?;
    // The same file, or `dst` is also some image's image.img (a new image made from this version): a served golden is
    // never deduped (it would be left in pieces).
    if (dm.dev(), dm.ino()) == (sm.dev(), sm.ino()) || dm.nlink() > 1 {
        return Ok(Report::default());
    }
    if dm.dev() != sm.dev() {
        return Err(format!(
            "{} and {} are on different filesystems",
            dst.path.display(),
            src.path.display()
        ));
    }
    if !BLOCK.is_multiple_of(dm.blksize()) || !supported(dst.dir()) {
        return Err(UNSUPPORTED.into());
    }
    let mtime = dm.modified().map_err(ioerr(&dst.path))?;
    let r = share(dst, src, min, max_calls);
    // The mtime is what hash.rs and the Windows golden.id know the file by: a kernel that moved it gets it set back
    // (the bytes are the same).
    if dst
        .file
        .metadata()
        .and_then(|m| m.modified())
        .is_ok_and(|t| t != mtime)
    {
        tracing::warn!(
            "{}: the dedupe moved its mtime → set back",
            dst.path.display()
        );
        dst.file.set_modified(mtime).map_err(ioerr(&dst.path))?;
    }
    r
}

fn share(dst: &Side, src: &Side, min: u64, max_calls: usize) -> Result<Report, String> {
    let segs = segments(&sums(src)?, &sums(dst)?, min);
    let both = shared(
        &extents(&dst.file).map_err(ioerr(&dst.path))?,
        &extents(&src.file).map_err(ioerr(&src.path))?,
    );
    let todo = subtract(&segs, &both);
    let total = |v: &[(u64, u64)]| v.iter().map(|(_, l)| l).sum::<u64>();
    let mut r = Report {
        already: total(&segs) - total(&todo),
        ..Default::default()
    };
    let mut calls = 0;
    'all: for (off, len) in todo {
        let (mut pos, end) = (off, off + len);
        while pos < end {
            if calls == max_calls {
                break 'all;
            }
            calls += 1;
            let n = (end - pos).min(CALL);
            let t = std::time::Instant::now();
            match dedupe_range(&src.file, pos, &dst.file, pos, n)
                .map_err(|e| format!("dedupe {} at {pos}: {e}", dst.path.display()))?
            {
                Some(0) => break, // nothing done (should not happen): this run is left as it is
                Some(k) => {
                    r.deduped += k;
                    pos += k;
                }
                None => {
                    r.differs += n;
                    pos += n;
                }
            }
            // Busy at most half the time: whatever else waits for the disk (the iSCSI daemon) gets the rest.
            std::thread::sleep(t.elapsed());
        }
    }
    if r.differs > 0 {
        tracing::warn!(
            "{} / {}: {} bytes differ where their indexes matched → indexes dropped",
            dst.path.display(),
            src.path.display(),
            r.differs
        );
        let _ = std::fs::remove_file(index_of(&dst.path));
        let _ = std::fs::remove_file(index_of(&src.path));
    }
    Ok(r)
}

/// `f` on a thread of its own at idle I/O priority (the disk serves everyone else first). Its own: the priority
/// belongs to the thread, and a pooled one (spawn_blocking) would keep it for the jobs after.
pub(crate) fn run_idle<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|s| {
        s.spawn(move || {
            // SAFETY: a plain syscall on this thread (who 0 = the caller); a failure only leaves the normal priority.
            unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, IOPRIO_IDLE) };
            f()
        })
        .join()
        .unwrap_or_else(|p| std::panic::resume_unwind(p))
    })
}

// ---------- a version served again (rollback) ----------

/// Data per piece on the disk that reads as well as one piece: a seek costs well under 1% of reading this from a disk.
const PIECE_OK: u64 = 256 << 20;

/// The pieces a file's data is in on the disk (runs contiguous on it, holes not counted) and its data bytes.
fn pieces(f: &File) -> std::io::Result<(u64, u64, bool)> {
    let (mut n, mut bytes, mut end, mut shared) = (0, 0, None, false);
    for e in extents(f)? {
        if end != Some(e.physical) {
            n += 1;
        }
        end = Some(e.physical + e.len);
        bytes += e.len;
        shared |= e.shared;
    }
    Ok((n, bytes, shared))
}

/// Some(pieces) = `file` is in pieces worth joining before it is served (a version deduped against a newer golden), on
/// a filesystem that dedupes. None = in one piece, or a filesystem where nothing is deduped.
pub(crate) fn scattered(file: &Path) -> Option<u64> {
    let f = File::open(file).ok()?;
    if !supported(file.parent()?) {
        return None;
    }
    // Only pieces a dedupe made (shared blocks): a file the filesystem laid out in pieces would come out of a copy
    // the same, again at every publish.
    let (n, bytes, shared) = pieces(&f).ok()?;
    (shared && n > 16 + bytes / PIECE_OK).then_some(n)
}

/// Before a publish: image.img in pieces is rebuilt in one piece (the same bytes and stamp). When it is a version's
/// file (a rollback, a new image from a version), the copy becomes that version's file too. Ok(true) = rebuilt.
pub(crate) fn make_whole(
    st: &SharedState,
    name: &str,
    steps: &mut crate::publish::Steps,
) -> Result<bool, String> {
    let img = crate::images_dir().join(name).join("image.img");
    let Some(n) = scattered(&img) else {
        return Ok(false);
    };
    steps.go(&format!("rebuild the golden in one piece (now {n})"));
    rebuild_in_place(&img, |tmp| {
        let _g = st.versions_lock.lock();
        match crate::versions::version_of_image(name) {
            Some(v) => crate::versions::replace_file(name, &v, tmp),
            None => Ok(()),
        }
    })?;
    // A Windows golden served from the old file reads the new one from now on.
    crate::goldenram::reopen(name, &img);
    tracing::info!("image {name}: golden in {n} pieces rebuilt in one");
    Ok(true)
}

/// `img` rebuilt in one piece: copied into `<img>.new` (rebuild), which `adopt` makes the version's file when it is one,
/// then renamed over `img`, its index along. A failure leaves `img` as it was.
pub(crate) fn rebuild_in_place(
    img: &Path,
    adopt: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let mut t = img.as_os_str().to_owned();
    t.push(".new");
    let tmp = PathBuf::from(t);
    let drop_tmp = || {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(index_of(&tmp));
    };
    drop_tmp();
    rebuild(img, &tmp)
        .and_then(|()| adopt(&tmp))
        .and_then(|()| {
            std::fs::rename(&tmp, img).map_err(ioerr(img))?;
            let _ = std::fs::rename(index_of(&tmp), index_of(img));
            Ok(())
        })
        .inspect_err(|_| drop_tmp())
}

/// `file` copied whole into the new file `to`: laid out afresh (holes kept), with its mtime — the same bytes, so the
/// stamp hash.rs and the Windows golden.id know them by stays and nothing is hashed again — and its index.
pub(crate) fn rebuild(file: &Path, to: &Path) -> Result<(), String> {
    let f = File::open(file).map_err(ioerr(file))?;
    let (_, bytes, _) = pieces(&f).map_err(ioerr(file))?;
    crate::publish::need_space(to.parent().unwrap_or(Path::new(".")), bytes, "rebuild")?;
    let mtime = f
        .metadata()
        .and_then(|m| m.modified())
        .map_err(ioerr(file))?;
    run_idle(|| crate::disk::copy_region(file, 0, None, to))?;
    File::options()
        .write(true)
        .open(to)
        .and_then(|t| t.set_modified(mtime))
        .map_err(ioerr(to))?;
    // The sums are the same: the index made of `file` is written again for `to`.
    let sums = stamp(&f)
        .ok()
        .and_then(|st| decode(&std::fs::read(index_of(file)).ok()?, st));
    if let (Some(v), Ok(st)) = (sums, File::open(to).and_then(|t| stamp(&t))) {
        let _ = crate::versions::write_durable(&index_of(to), &encode(st, &v));
    }
    Ok(())
}

/// `to` is (now) the same file as `from`: its index is `from`'s (only a cache: may be missing).
pub(crate) fn link_index(from: &Path, to: &Path) {
    let _ = std::fs::remove_file(index_of(to));
    let _ = std::fs::hard_link(index_of(from), index_of(to));
}

// ---------- the jobs ----------

/// After a publish (images.rs): the newest version that is another file gives back what it has in common with the new
/// image.img (which is only read). None = nothing to do (no such version, image.img is a version, another filesystem,
/// one that cannot). Never fails the publish: a failure is a line in its result.
pub(crate) fn after_publish(
    st: &SharedState,
    name: &str,
    steps: &mut crate::publish::Steps,
) -> Option<String> {
    let opened = {
        let _g = st.versions_lock.lock();
        crate::versions::dedupe_base(name).map(|(ver, p)| {
            let img = crate::images_dir().join(name).join("image.img");
            Ok::<_, String>((ver, Side::open(&p)?, Side::open(&img)?))
        })
    }?;
    let (ver, dst, src) = match opened {
        Ok(x) => x,
        Err(e) => return Some(format!("⚠ dedup: {e}")),
    };
    if dst.dev() != src.dev() || !supported(dst.dir()) {
        return None;
    }
    steps.go(&format!("dedup {ver}"));
    Some(match run_idle(|| dedupe(&dst, &src)) {
        Ok(r) => format!("dedup {ver}: {r}"),
        Err(e) => {
            tracing::warn!("image {name}: dedup {ver}: {e}");
            format!("⚠ dedup: {e}")
        }
    })
}

/// Every version of an image gives back what it has in common with image.img (which is only read): after make_whole
/// rebuilt the golden, after a move (copies share nothing), and the Dedup button (versions saved before dedup existed).
pub(crate) fn dedupe_image(
    st: &SharedState,
    name: &str,
    steps: &mut crate::publish::Steps,
) -> Result<String, String> {
    let img = crate::images_dir().join(name).join("image.img");
    let (golden, others) = {
        let _g = st.versions_lock.lock();
        let g = Side::open(&img)?;
        let others = crate::versions::chain(name)
            .into_iter()
            .filter(|(_, p)| !g.is(p))
            .map(|(v, p)| Side::open(&p).map(|s| (v, s)))
            .collect::<Result<Vec<_>, _>>()?;
        (g, others)
    };
    if !supported(golden.dir()) {
        return Err(UNSUPPORTED.into());
    }
    let mut total = Report::default();
    // A clone's file on another disk is left out.
    for (v, dst) in others.iter().filter(|(_, s)| s.dev() == golden.dev()) {
        steps.go(&format!("dedup {v}"));
        total.add(run_idle(|| dedupe(dst, &golden)).map_err(|e| format!("{v}: {e}"))?);
    }
    Ok(format!("dedup: {total}"))
}

#[cfg(test)]
#[path = "../tests/unit/dedup.rs"]
pub(crate) mod tests; // its loop-device filesystems serve versions.rs's live test too
