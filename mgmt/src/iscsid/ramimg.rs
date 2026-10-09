// iscsid/ramimg.rs — a golden held in RAM, compressed (the RAM cache mode of Linux targets and Windows goldens):
// 16 KB blocks, each zstd-compressed on its own so a read only unpacks the blocks it touches; holes and all-zero
// blocks take no memory. Recently read blocks stay unpacked in a small shared cache: in a boot storm every client
// reads the same blocks.
//
// RAM is claimed BEFORE loading: the worst case of the compressed copy (every block holding data, at zstd's bound)
// is mapped, faulted in and locked in one go, after checking it fits; then the blocks are compressed into it and the
// part compression saved is given back. So a load either is refused up front or finishes — RAM taken meanwhile by
// another process can't make it fail halfway (a failed allocation would abort the whole process, and with it every
// client it serves). Check + claim run under a lock shared by every broom process (mgmt + iSCSI daemon).
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

pub(crate) const BLOCK: usize = 16 << 10;
/// Blocks read from the file per worker job while loading (16 MB).
const BATCH: usize = 1024;
/// Unpacked blocks kept (64 MB). One lock per image: shard it if a profiler ever shows contention.
const CACHE_BLOCKS: usize = 4096;
/// Held while one process checks free RAM and claims its share (two loads must not both see the same free RAM).
const CLAIM_LOCK: &str = "/run/broom-ram.lock";

type Cache = (HashMap<u32, Arc<[u8]>>, VecDeque<u32>);

/// Anonymous memory mapped, faulted in and locked up front; read-only once the load is done.
struct Arena {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: written only by the loading thread before the image is shared; afterwards only read.
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

fn page_up(n: usize) -> usize {
    n.div_ceil(4096) * 4096
}

impl Arena {
    fn claim(len: usize) -> Result<Arena, String> {
        let len = page_up(len.max(1));
        // SAFETY: a fresh private anonymous mapping, no existing memory involved.
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE, -1, 0)
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!("reserve {:.1} GB of RAM: {}", len as f64 / 1e9, std::io::Error::last_os_error()));
        }
        // Locked: never pushed to swap (a golden read from swap is slower than from its disk). Best effort.
        // SAFETY: the range was just mapped.
        if unsafe { libc::mlock(ptr, len) } != 0 {
            tracing::warn!("RAM copy: mlock {:.1} GB: {} (may be swapped out under pressure)", len as f64 / 1e9, std::io::Error::last_os_error());
        }
        Ok(Arena { ptr: ptr.cast(), len })
    }

    /// Give back everything past `used`.
    fn shrink(&mut self, used: usize) {
        let keep = page_up(used.max(1));
        if keep < self.len {
            // SAFETY: the tail lies inside our mapping and nothing points into it.
            unsafe { libc::munmap(self.ptr.add(keep).cast(), self.len - keep) };
            self.len = keep;
        }
    }

    fn get(&self, off: usize, len: usize) -> &[u8] {
        assert!(off + len <= self.len);
        // SAFETY: inside the mapping (checked), which lives as long as self.
        unsafe { std::slice::from_raw_parts(self.ptr.add(off), len) }
    }

    fn put(&mut self, off: usize, data: &[u8]) {
        assert!(off + data.len() <= self.len);
        // SAFETY: inside the mapping (checked); &mut self = no reader yet.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(off), data.len()) };
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // SAFETY: our own mapping, unmapped once.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// Does `need` fit: needs MemAvailable >= need + reserve. avail=0 (unreadable) → allow.
pub fn fits(need: u64, avail: u64, reserve: u64) -> bool {
    avail == 0 || need.saturating_add(reserve) <= avail
}

/// Available RAM (bytes) from /proc/meminfo MemAvailable (free + reclaimable page cache). 0 if unreadable.
fn mem_available() -> u64 {
    let s = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    s.lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|r| r.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

/// Blocks of a file that hold data (not entirely inside a hole): SEEK_DATA / SEEK_HOLE over the file.
fn data_blocks(f: &std::fs::File, size: u64) -> u64 {
    use std::os::fd::AsRawFd;
    let (fd, b) = (f.as_raw_fd(), BLOCK as u64);
    let (mut n, mut pos) = (0u64, 0u64);
    while pos < size {
        // SAFETY: lseek on a valid fd, no memory involved.
        let data = unsafe { libc::lseek(fd, pos as libc::off_t, libc::SEEK_DATA) };
        if data < 0 {
            break; // ENXIO: no data after pos
        }
        let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
        let end = if hole < 0 { size } else { (hole as u64).min(size) };
        let (first, last) = (data as u64 / b, end.div_ceil(b));
        // Blocks [first, last), minus one already counted when this extent starts in the previous extent's last block.
        n += last - first - u64::from(pos > 0 && first < pos.div_ceil(b));
        pos = end;
    }
    n
}

pub struct RamImage {
    arena: Arena,
    /// Per block: (offset in the arena, compressed length); length 0 = all zero.
    index: Vec<(u64, u32)>,
    size: u64,
    cache: Mutex<Cache>,
}

impl RamImage {
    /// Claim the worst case RAM (refused if it doesn't fit next to `reserve` bytes kept free), then read + compress
    /// the whole file into it (in parallel). Blocking; ~a minute for a 12 GB golden.
    pub fn load(path: &str, reserve: u64) -> Result<RamImage, String> {
        use std::os::unix::fs::FileExt;
        let f = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
        let size = f.metadata().map_err(|e| format!("{path}: {e}"))?.len();
        let blocks = data_blocks(&f, size);
        Self::load_with(path, size, blocks, reserve, |off, buf| f.read_exact_at(buf, off).map_err(|e| format!("{path} @{off}: {e}")))
    }

    /// Same from any source of `size` bytes, `blocks` of them (16 KB) possibly holding data.
    pub fn load_with(
        path: &str,
        size: u64,
        blocks: u64,
        reserve: u64,
        read: impl Fn(u64, &mut [u8]) -> Result<(), String> + Sync,
    ) -> Result<RamImage, String> {
        let n = size.div_ceil(BLOCK as u64) as usize;
        let worst = blocks * zstd::zstd_safe::compress_bound(BLOCK) as u64;

        let mut arena = {
            use std::os::fd::AsRawFd;
            let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(CLAIM_LOCK);
            // SAFETY: flock on a valid fd; released when `lock` is dropped at the end of this block.
            if let Ok(l) = &lock {
                unsafe { libc::flock(l.as_raw_fd(), libc::LOCK_EX) };
            }
            let avail = mem_available();
            if !fits(worst, avail, reserve) {
                let gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
                return Err(format!(
                    "not enough RAM for the RAM copy: up to {:.1}GB (the golden's data, compressed worst case) + reserve \
                     {:.1}GB > available {:.1}GB. Use cache_mode=disk, lower the reserve, or add RAM to the server.",
                    gb(worst),
                    gb(reserve),
                    gb(avail)
                ));
            }
            Arena::claim(worst as usize)?
        };

        // Compressed in parallel a few batches at a time, each group copied into the arena right away: the scratch
        // RAM on top of the arena stays at ~workers × 16 MB.
        let jobs: Vec<usize> = (0..n).step_by(BATCH).collect();
        let mut index = Vec::with_capacity(n);
        let mut used = 0usize;
        for group in jobs.chunks(crate::disk::workers()) {
            let parts = crate::disk::par_map(group, |&first| {
                let off = (first * BLOCK) as u64;
                let mut buf = vec![0u8; ((size - off) as usize).min(BATCH * BLOCK)];
                read(off, &mut buf)?;
                let mut z = zstd::bulk::Compressor::new(3).map_err(|e| format!("zstd: {e}"))?;
                buf.chunks(BLOCK)
                    .map(|b| if b.iter().all(|&x| x == 0) { Ok(Vec::new()) } else { z.compress(b).map_err(|e| format!("zstd: {e}")) })
                    .collect::<Result<Vec<_>, String>>()
            })?;
            for c in parts.into_iter().flatten() {
                if used + c.len() > arena.len {
                    return Err(format!("{path} changed while loading into RAM"));
                }
                arena.put(used, &c);
                index.push((used as u64, c.len() as u32));
                used += c.len();
            }
        }
        arena.shrink(used);
        tracing::info!(
            "{path} in RAM: {:.1} GB → {:.1} GB compressed (claimed {:.1} GB up front)",
            size as f64 / 1e9,
            used as f64 / 1e9,
            worst as f64 / 1e9
        );
        Ok(RamImage { arena, index, size, cache: Mutex::new(Default::default()) })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Bytes at `off` (must lie inside the image).
    pub fn read_at(&self, mut off: u64, mut buf: &mut [u8]) -> Result<(), String> {
        if off.checked_add(buf.len() as u64).is_none_or(|end| end > self.size) {
            return Err("read past the end of the image".into());
        }
        while !buf.is_empty() {
            let (i, at) = ((off / BLOCK as u64) as usize, (off % BLOCK as u64) as usize);
            let n = (BLOCK - at).min(buf.len());
            if self.index[i].1 == 0 {
                buf[..n].fill(0);
            } else {
                buf[..n].copy_from_slice(&self.block(i)?[at..at + n]);
            }
            buf = &mut buf[n..];
            off += n as u64;
        }
        Ok(())
    }

    fn block(&self, i: usize) -> Result<Arc<[u8]>, String> {
        if let Some(b) = self.cache.lock().unwrap().0.get(&(i as u32)) {
            return Ok(b.clone());
        }
        let (off, len) = self.index[i];
        let packed = self.arena.get(off as usize, len as usize);
        // One zstd context per thread, reused: a new one per block is a large C malloc (mmap + munmap on musl) each time.
        thread_local! {
            static DCTX: std::cell::RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { std::cell::RefCell::new(None) };
        }
        let b: Arc<[u8]> = DCTX
            .with_borrow_mut(|d| {
                if d.is_none() {
                    *d = Some(zstd::bulk::Decompressor::new()?);
                }
                d.as_mut().unwrap().decompress(packed, BLOCK)
            })
            .map_err(|e| format!("zstd: {e}"))?
            .into();
        let mut c = self.cache.lock().unwrap();
        if c.0.insert(i as u32, b.clone()).is_none() {
            c.1.push_back(i as u32);
            if c.1.len() > CACHE_BLOCKS {
                let old = c.1.pop_front().unwrap();
                c.0.remove(&old);
            }
        }
        Ok(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const GB: u64 = 1 << 30;

    #[test]
    fn reads_match_the_file() {
        use std::os::unix::fs::FileExt;
        let p = std::env::temp_dir().join("broom_test_ramimg.img");
        // A hole, data across a block boundary, an odd-sized tail.
        let size = 5 * BLOCK as u64 + 777;
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(size).unwrap();
        let data: Vec<u8> = (0..40_000u32).map(|i| (i * 7 % 251) as u8).collect();
        f.write_all_at(&data, BLOCK as u64 * 2 - 100).unwrap();
        f.write_all_at(b"tail", size - 4).unwrap();
        let img = RamImage::load(p.to_str().unwrap(), 0).unwrap();
        assert_eq!(img.size(), size);
        assert!(img.index[0].1 == 0 && img.index[1].1 > 0, "zero block not stored");
        let file = std::fs::read(&p).unwrap();
        for (off, len) in [(0, 512), (BLOCK as u64 * 2 - 300, 50_000), (size - 1000, 1000), (0, size as usize)] {
            let mut b = vec![1u8; len];
            img.read_at(off, &mut b).unwrap();
            assert_eq!(b, &file[off as usize..off as usize + len], "@{off}+{len}");
        }
        assert!(img.read_at(size, &mut [0u8; 1]).is_err());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn claims_only_what_data_can_need() {
        use std::os::unix::fs::FileExt;
        // 1 GB virtual, 2 data extents (one straddling a block edge): only their blocks count.
        let p = std::env::temp_dir().join("broom_test_ramimg_sparse.img");
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(GB).unwrap();
        f.write_all_at(&[1u8; 8192], 0).unwrap();
        f.write_all_at(&[2u8; 100], 512 << 20).unwrap();
        let blocks = data_blocks(&f, GB);
        assert!((2..=4).contains(&blocks), "{blocks} blocks (fs block size may round extents up)");
        // Far more than any test box has: refused up front, nothing claimed.
        let e = RamImage::load(p.to_str().unwrap(), 1 << 60).err().unwrap();
        assert!(e.contains("not enough RAM"), "{e}");
        let img = RamImage::load(p.to_str().unwrap(), 0).unwrap();
        assert!(img.arena.len <= 64 << 10, "tail given back: {} bytes kept", img.arena.len);
        let mut b = [0u8; 4];
        img.read_at((512 << 20) + 98, &mut b).unwrap();
        assert_eq!(b, [2, 2, 0, 0]);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn cache_is_bounded() {
        let p = std::env::temp_dir().join("broom_test_ramimg_cache.img");
        let data: Vec<u8> = (0..CACHE_BLOCKS + 10).flat_map(|i| [i as u8 | 1; BLOCK]).collect();
        std::fs::write(&p, &data).unwrap();
        let img = RamImage::load(p.to_str().unwrap(), 0).unwrap();
        let mut b = [0u8; 1];
        for i in 0..CACHE_BLOCKS + 10 {
            img.read_at((i * BLOCK) as u64, &mut b).unwrap();
            assert_eq!(b[0], i as u8 | 1);
        }
        let c = img.cache.lock().unwrap();
        assert_eq!((c.0.len(), c.1.len()), (CACHE_BLOCKS, CACHE_BLOCKS));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn fits_logic() {
        assert!(fits(4 * GB, 8 * GB, 2 * GB));
        assert!(!fits(40 * GB, 16 * GB, 2 * GB));
        assert!(fits(6 * GB, 8 * GB, 2 * GB), "exactly at the limit");
        assert!(!fits(6 * GB + 1, 8 * GB, 2 * GB));
        assert!(fits(999 * GB, 0, 2 * GB), "meminfo unreadable → allowed");
    }
}
