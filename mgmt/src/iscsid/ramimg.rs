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
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

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

// SAFETY: an owned buffer like a Vec: every write takes &mut self and reads borrow &self, so the usual borrow rules keep
// them apart across threads too.
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
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!(
                "reserve {:.1} GB of RAM: {}",
                len as f64 / 1e9,
                std::io::Error::last_os_error()
            ));
        }
        // Locked: never pushed to swap (a golden read from swap is slower than from its disk). Best effort.
        // SAFETY: the range was just mapped.
        if unsafe { libc::mlock(ptr, len) } != 0 {
            tracing::warn!(
                "RAM copy: mlock {:.1} GB: {} (may be swapped out under pressure)",
                len as f64 / 1e9,
                std::io::Error::last_os_error()
            );
        }
        Ok(Arena {
            ptr: ptr.cast(),
            len,
        })
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
        assert!(off.checked_add(len).is_some_and(|e| e <= self.len));
        // SAFETY: inside the mapping (checked), which lives as long as self.
        unsafe { std::slice::from_raw_parts(self.ptr.add(off), len) }
    }

    fn put(&mut self, off: usize, data: &[u8]) {
        assert!(off.checked_add(data.len()).is_some_and(|e| e <= self.len));
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

/// Blocks of a file that hold data (not entirely inside a hole).
fn data_blocks(path: &str) -> u64 {
    let b = BLOCK as u64;
    let (mut n, mut counted) = (0, 0); // blocks [0, counted) already looked at
    let ranges = crate::disk::Source::file(std::path::Path::new(path)).map(|s| s.data_ranges());
    for (start, end) in ranges.unwrap_or_default() {
        let (first, last) = ((start / b).max(counted), end.div_ceil(b));
        n += last.saturating_sub(first);
        counted = counted.max(last);
    }
    n
}

pub struct RamImage {
    arena: Arena,
    /// Per block: (offset in the arena, compressed length); length 0 = all zero, BLOCK = stored as is.
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
        let blocks = data_blocks(path);
        Self::load_with(path, size, blocks, reserve, |off, buf| {
            f.read_exact_at(buf, off)
                .map_err(|e| format!("{path} @{off}: {e}"))
        })
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
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(CLAIM_LOCK);
            // SAFETY: flock on a valid fd; released when `lock` is dropped at the end of this block.
            if let Ok(l) = &lock {
                unsafe { libc::flock(l.as_raw_fd(), libc::LOCK_EX) };
            }
            let avail = crate::sysmon::meminfo("MemAvailable:"); // free + reclaimable page cache; 0 if unreadable
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
                    .map(|b| {
                        if b.iter().all(|&x| x == 0) {
                            return Ok(Vec::new());
                        }
                        let c = z.compress(b).map_err(|e| format!("zstd: {e}"))?;
                        if c.len() < b.len() {
                            return Ok(c);
                        }
                        // Not smaller (already compressed data: games, media) → kept as is, a whole block (the last
                        // one padded): length BLOCK marks it, reads copy it without unpacking.
                        let mut raw = b.to_vec();
                        raw.resize(BLOCK, 0);
                        Ok(raw)
                    })
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
        Ok(RamImage {
            arena,
            index,
            size,
            cache: Mutex::new(Default::default()),
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Bytes at `off` (must lie inside the image).
    pub fn read_at(&self, mut off: u64, mut buf: &mut [u8]) -> Result<(), String> {
        if off
            .checked_add(buf.len() as u64)
            .is_none_or(|end| end > self.size)
        {
            return Err("read past the end of the image".into());
        }
        while !buf.is_empty() {
            let (i, at) = ((off / BLOCK as u64) as usize, (off % BLOCK as u64) as usize);
            let n = (BLOCK - at).min(buf.len());
            let (packed, len) = self.index[i];
            if len == 0 {
                buf[..n].fill(0);
            } else if len as usize == BLOCK {
                buf[..n].copy_from_slice(self.arena.get(packed as usize + at, n));
            } else {
                buf[..n].copy_from_slice(&self.block(i)?[at..at + n]);
            }
            buf = &mut buf[n..];
            off += n as u64;
        }
        Ok(())
    }

    fn block(&self, i: usize) -> Result<Arc<[u8]>, String> {
        if let Some(b) = self.cache.lock().0.get(&(i as u32)) {
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
        let mut c = self.cache.lock();
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
#[path = "../../tests/unit/iscsid/ramimg.rs"]
mod tests;
