// iscsid/overlay.rs — writes on top of a disk (the games disk): a sparse file the size of the disk, same offsets, plus
// the set of 64 KB blocks written (saved next to it, so it survives a restart of the machine or the daemon). Reading a
// block: from the overlay once written, else from what is under it. The first write to a block copies it from below
// (read-modify-write), so a block in the set always holds its whole current content. The game update machine writes
// into one; saved, it becomes a frozen layer of a games disk version (games.rs), later merged into the disk itself.
use std::collections::HashSet;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const BLOCK: u64 = 64 << 10;

pub struct Overlay {
    file: File,
    size: u64,
    /// Blocks written (whole content in `file`). Lock order: `write` then `blocks`.
    blocks: Mutex<HashSet<u32>>,
    /// Serializes writes (two first writes into one block must not both copy the disk over each other's data); holds
    /// the block count last saved to `list`.
    write: Mutex<usize>,
    list: PathBuf,
}

type Base<'a> = &'a dyn Fn(u64, &mut [u8]) -> Result<(), String>;

impl Overlay {
    /// The overlay at `path` with its block list `list`: reopened as it was, or new.
    pub fn open(path: &Path, list: &Path, size: u64) -> Result<Overlay, String> {
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if file.metadata().map_err(|e| e.to_string())?.len() < size {
            file.set_len(size).map_err(|e| format!("{}: {e}", path.display()))?; // new, or the disk grew
        }
        let blocks: HashSet<u32> = std::fs::read(list)
            .map(|b| b.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect())
            .unwrap_or_default();
        let n = blocks.len();
        Ok(Overlay { file, size, blocks: Mutex::new(blocks), write: Mutex::new(n), list: list.into() })
    }

    /// Bytes held (written blocks).
    pub fn used(&self) -> u64 {
        self.blocks.lock().unwrap().len() as u64 * BLOCK
    }

    fn has(&self, b: u32) -> bool {
        self.blocks.lock().unwrap().contains(&b)
    }

    pub fn read_at(&self, base: Base, mut off: u64, mut buf: &mut [u8]) -> Result<(), String> {
        while !buf.is_empty() {
            let b = (off / BLOCK) as u32;
            let n = ((BLOCK - off % BLOCK) as usize).min(buf.len());
            if self.has(b) {
                self.file.read_exact_at(&mut buf[..n], off).map_err(|e| format!("overlay read: {e}"))?;
            } else {
                base(off, &mut buf[..n])?;
            }
            buf = &mut buf[n..];
            off += n as u64;
        }
        Ok(())
    }

    pub fn write_at(&self, base: Base, mut off: u64, mut data: &[u8]) -> Result<(), String> {
        if off.checked_add(data.len() as u64).is_none_or(|end| end > self.size) {
            return Err("write past the end of the disk".into());
        }
        let io = |e: std::io::Error| format!("overlay write: {e}");
        let _w = self.write.lock().unwrap();
        while !data.is_empty() {
            let b = (off / BLOCK) as u32;
            let n = ((BLOCK - off % BLOCK) as usize).min(data.len());
            if !self.has(b) {
                // First write here: the whole block from the disk first, so the overlay holds all of it.
                let start = b as u64 * BLOCK;
                let mut whole = vec![0u8; (BLOCK.min(self.size - start)) as usize];
                base(start, &mut whole)?;
                whole[(off - start) as usize..(off - start) as usize + n].copy_from_slice(&data[..n]);
                self.file.write_all_at(&whole, start).map_err(io)?;
                self.blocks.lock().unwrap().insert(b); // only now: readers see the disk until the block is complete
            } else {
                self.file.write_all_at(&data[..n], off).map_err(io)?;
            }
            data = &data[n..];
            off += n as u64;
        }
        Ok(())
    }

    /// On disk: data, then the block list naming it (rewritten whole when it grew; ~3 MB for 50 GB of update).
    pub fn flush(&self) -> Result<(), String> {
        let mut saved = self.write.lock().unwrap();
        self.file.sync_data().map_err(|e| format!("overlay sync: {e}"))?;
        let mut v: Vec<u32> = self.blocks.lock().unwrap().iter().copied().collect();
        if v.len() == *saved {
            return Ok(());
        }
        v.sort_unstable();
        let bytes: Vec<u8> = v.iter().flat_map(|b| b.to_le_bytes()).collect();
        let tmp = self.list.with_extension("tmp");
        let w = || -> std::io::Result<()> {
            let mut f = File::create(&tmp)?;
            std::io::Write::write_all(&mut f, &bytes)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.list)
        };
        w().map_err(|e| format!("{}: {e}", self.list.display()))?;
        *saved = v.len();
        Ok(())
    }

    /// Copy every written block into `base` (opened for writing), flushed. Safe to run again after a crash midway.
    pub fn merge_into(&self, base: &File) -> Result<usize, String> {
        let mut v: Vec<u32> = self.blocks.lock().unwrap().iter().copied().collect();
        v.sort_unstable();
        let mut buf = vec![0u8; BLOCK as usize];
        for &b in &v {
            let start = b as u64 * BLOCK;
            let n = BLOCK.min(self.size - start) as usize;
            self.file.read_exact_at(&mut buf[..n], start).map_err(|e| format!("overlay read: {e}"))?;
            base.write_all_at(&buf[..n], start).map_err(|e| format!("games disk write: {e}"))?;
        }
        base.sync_all().map_err(|e| format!("games disk sync: {e}"))?;
        Ok(v.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_of(data: &[u8]) -> impl Fn(u64, &mut [u8]) -> Result<(), String> + '_ {
        move |off, buf| {
            buf.copy_from_slice(&data[off as usize..off as usize + buf.len()]);
            Ok(())
        }
    }

    #[test]
    fn reads_see_writes_over_the_disk() {
        let d = std::env::temp_dir().join("broom_test_overlay");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let size = 5 * BLOCK + 1000; // a partial last block
        let base: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let b = base_of(&base);
        let o = Overlay::open(&d.join("u.ovl"), &d.join("u.blocks"), size).unwrap();
        // Unaligned write across a block boundary: two blocks taken, the rest of each copied from the disk.
        o.write_at(&b, BLOCK - 10, &[7u8; 30]).unwrap();
        o.write_at(&b, BLOCK + 100, &[9u8; 4]).unwrap();
        assert_eq!(o.used(), 2 * BLOCK);
        let mut want = base.clone();
        want[(BLOCK - 10) as usize..(BLOCK + 20) as usize].fill(7);
        want[(BLOCK + 100) as usize..(BLOCK + 104) as usize].fill(9);
        let mut got = vec![0u8; size as usize];
        o.read_at(&b, 0, &mut got).unwrap();
        assert!(got == want);
        o.write_at(&b, size - 1, &[3]).unwrap(); // the partial last block
        assert!(o.write_at(&b, size - 1, &[1, 2]).is_err(), "past the end");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn survives_reopen_and_merges() {
        let d = std::env::temp_dir().join("broom_test_overlay_kept");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let size = 4 * BLOCK;
        let base_path = d.join("games.img");
        std::fs::write(&base_path, vec![1u8; size as usize]).unwrap();
        let base = std::fs::read(&base_path).unwrap();
        let (ovl, list) = (d.join("update.ovl"), d.join("update.blocks"));
        let o = Overlay::open(&ovl, &list, size).unwrap();
        o.write_at(&base_of(&base), 2 * BLOCK + 5, b"game").unwrap();
        o.flush().unwrap();
        drop(o);
        // Reopened (daemon restart): the block is still the overlay's.
        let o = Overlay::open(&ovl, &list, size).unwrap();
        let mut buf = [0u8; 4];
        o.read_at(&base_of(&base), 2 * BLOCK + 5, &mut buf).unwrap();
        assert_eq!(&buf, b"game");
        let f = std::fs::OpenOptions::new().write(true).open(&base_path).unwrap();
        assert_eq!(o.merge_into(&f).unwrap(), 1);
        let merged = std::fs::read(&base_path).unwrap();
        assert_eq!(&merged[(2 * BLOCK + 5) as usize..(2 * BLOCK + 9) as usize], b"game");
        assert_eq!(merged.iter().filter(|&&x| x != 1).count(), 4, "only those bytes changed");
        let _ = std::fs::remove_dir_all(&d);
    }
}
