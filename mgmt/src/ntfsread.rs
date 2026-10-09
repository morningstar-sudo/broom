// ntfsread.rs — read files of an NTFS partition inside a raw disk image, in process (crate ntfs, read-only): no loop
// device, no kernel ntfs3. The Windows publish (winstage) only reads the golden, so the server needs neither module.
use ntfs::indexes::NtfsFileNameIndex;
use ntfs::structured_values::{NtfsFileNamespace, NtfsVolumeFlags};
use ntfs::{Ntfs, NtfsFile, NtfsReadSeek};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

type Fs = BufReader<Part>;

/// Bytes [base, base+len) of a file as a stream: the partition inside the disk image.
struct Part {
    f: File,
    base: u64,
    len: u64,
    pos: u64,
}

impl Read for Part {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::os::unix::fs::FileExt;
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        let n = self.f.read_at(&mut buf[..n], self.base + self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Part {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let p = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = p.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before the partition"))?;
        Ok(self.pos)
    }
}

fn err(e: ntfs::NtfsError) -> String {
    format!("NTFS: {e}")
}

/// An NTFS volume opened read-only.
pub struct Vol {
    fs: Fs,
    ntfs: Ntfs,
}

impl Vol {
    /// The NTFS volume in [start, start+size) of the raw image `raw`.
    pub fn open(raw: &str, (start, size): (u64, u64)) -> Result<Vol, String> {
        let f = File::open(raw).map_err(|e| format!("{raw}: {e}"))?;
        let mut fs = BufReader::new(Part { f, base: start, len: size, pos: 0 });
        let mut ntfs = Ntfs::new(&mut fs).map_err(err)?;
        // Needed for the case-insensitive name lookups.
        ntfs.read_upcase_table(&mut fs).map_err(err)?;
        Ok(Vol { fs, ntfs })
    }

    /// Windows did not unmount it cleanly (hibernated, Fast Startup, forced power-off): its files may be half-written.
    pub fn is_dirty(&mut self) -> Result<bool, String> {
        Ok(self.ntfs.volume_info(&mut self.fs).map_err(err)?.flags().contains(NtfsVolumeFlags::IS_DIRTY))
    }

    pub fn exists(&mut self, path: &str) -> bool {
        matches!(find(&self.ntfs, &mut self.fs, path), Ok(Some(_)))
    }

    /// Whole content of a file; None when it doesn't exist (or is a directory).
    pub fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>, String> {
        let Vol { fs, ntfs } = self;
        match find(ntfs, fs, path)? {
            Some(f) if !f.is_directory() => {
                let mut v = Vec::new();
                copy_data(&f, fs, &mut v).map_err(|e| format!("{path}: {e}"))?;
                Ok(Some(v))
            }
            _ => Ok(None),
        }
    }

    /// Up to `max` names in the root directory (for error messages).
    pub fn root_names(&mut self, max: usize) -> Vec<String> {
        let Vol { fs, ntfs } = self;
        ntfs.root_directory(fs).and_then(|root| entries(&root, fs)).map_or_else(|_| Vec::new(), |v| {
            v.into_iter().map(|(n, _, _)| n).filter(|n| !n.starts_with('$') && n != ".").take(max).collect()
        })
    }

    /// Copy directory `path` (recursively) to `dest` on the local disk.
    pub fn extract_dir(&mut self, path: &str, dest: &Path) -> Result<(), String> {
        let Vol { fs, ntfs } = self;
        let dir = find(ntfs, fs, path)?.filter(NtfsFile::is_directory).ok_or_else(|| format!("{path}: no such directory"))?;
        extract(ntfs, fs, &dir, dest)
    }
}

/// The file at `path` ('/'-separated, case-insensitive like Windows), or None.
fn find<'n>(ntfs: &'n Ntfs, fs: &mut Fs, path: &str) -> Result<Option<NtfsFile<'n>>, String> {
    let mut f = ntfs.root_directory(fs).map_err(err)?;
    for name in path.split('/').filter(|s| !s.is_empty()) {
        if !f.is_directory() {
            return Ok(None);
        }
        let next = {
            let index = f.directory_index(fs).map_err(err)?;
            let mut finder = index.finder();
            match NtfsFileNameIndex::find(&mut finder, ntfs, fs, name) {
                None => return Ok(None),
                Some(e) => e.map_err(err)?.to_file(ntfs, fs).map_err(err)?,
            }
        };
        f = next;
    }
    Ok(Some(f))
}

/// (name, file record, is directory) of every entry of a directory. DOS 8.3 aliases are skipped (the same file
/// under its long name is listed too).
fn entries(dir: &NtfsFile, fs: &mut Fs) -> ntfs::Result<Vec<(String, ntfs::NtfsFileReference, bool)>> {
    let index = dir.directory_index(fs)?;
    let mut it = index.entries();
    let mut v = Vec::new();
    while let Some(e) = it.next(fs) {
        let e = e?;
        let Some(key) = e.key() else { continue };
        let key = key?;
        if key.namespace() == NtfsFileNamespace::Dos {
            continue;
        }
        v.push((key.name().to_string_lossy(), e.file_reference(), key.is_directory()));
    }
    Ok(v)
}

/// The unnamed $DATA stream of `f` into `w`.
fn copy_data(f: &NtfsFile, fs: &mut Fs, w: &mut impl Write) -> Result<(), String> {
    let Some(item) = f.data(fs, "") else { return Ok(()) }; // no data stream = empty file
    let attr = item.map_err(err)?;
    let attr = attr.to_attribute().map_err(err)?;
    let mut value = attr.value(fs).map_err(err)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match value.read(fs, &mut buf).map_err(err)? {
            0 => return Ok(()),
            n => w.write_all(&buf[..n]).map_err(|e| e.to_string())?,
        }
    }
}

fn extract(ntfs: &Ntfs, fs: &mut Fs, dir: &NtfsFile, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    for (name, r, is_dir) in entries(dir, fs).map_err(err)? {
        // Names come from an uploaded disk: never let one leave `dest`.
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
            continue;
        }
        let f = r.to_file(ntfs, fs).map_err(err)?;
        let to = dest.join(&name);
        if is_dir {
            extract(ntfs, fs, &f, &to)?;
        } else {
            let mut out = File::create(&to).map_err(|e| format!("{}: {e}", to.display()))?;
            copy_data(&f, fs, &mut out).map_err(|e| format!("{name}: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_reads_inside_its_range() {
        let p = std::env::temp_dir().join("broom_test_part.bin");
        std::fs::write(&p, (0u8..100).collect::<Vec<_>>()).unwrap();
        let mut part = Part { f: File::open(&p).unwrap(), base: 10, len: 20, pos: 0 };
        let mut b = [0u8; 8];
        part.read_exact(&mut b).unwrap();
        assert_eq!(b, [10, 11, 12, 13, 14, 15, 16, 17]);
        assert_eq!(part.seek(SeekFrom::End(-4)).unwrap(), 16);
        let mut rest = Vec::new();
        part.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, [26, 27, 28, 29], "stops at the partition end, not the file end");
        assert!(part.seek(SeekFrom::Current(-100)).is_err());
        let _ = std::fs::remove_file(p);
    }

    /// Needs root + mkntfs/ntfs-3g (package ntfs-3g): builds a small NTFS image and reads it back.
    #[test]
    #[ignore]
    fn ntfs_live() {
        use std::process::Command;
        let d = std::env::temp_dir().join("broom_ntfs_live");
        let _ = Command::new("umount").arg(d.join("mnt")).status();
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("mnt")).unwrap();
        let img = d.join("vol.img");
        let sh = |c: &str| assert!(Command::new("sh").args(["-c", c]).status().unwrap().success(), "{c}");
        // 1 MiB of something else before the partition, like a disk image.
        sh(&format!("truncate -s 65M {0} && truncate -s 64M {0}.p && mkntfs -F -f -q {0}.p", img.display()));
        sh(&format!("ntfs-3g {}.p {}", img.display(), d.join("mnt").display()));
        let m = d.join("mnt");
        std::fs::create_dir_all(m.join("Windows/System32/config")).unwrap();
        std::fs::write(m.join("Windows/System32/config/SYSTEM"), b"hive").unwrap();
        std::fs::create_dir_all(m.join("broom/efi/EFI/Microsoft/Boot")).unwrap();
        std::fs::write(m.join("broom/efi/EFI/Microsoft/Boot/BCD"), vec![7u8; 300_000]).unwrap();
        std::fs::write(m.join("broom/boot-storage.ok"), b"storahci stornvme\r\n").unwrap();
        sh(&format!("umount {}", m.display()));
        sh(&format!("dd if={0}.p of={0} bs=1M seek=1 conv=notrunc status=none", img.display()));

        let mut v = Vol::open(img.to_str().unwrap(), (1 << 20, 64 << 20)).unwrap();
        assert!(!v.is_dirty().unwrap());
        assert!(v.exists("windows/system32/CONFIG/system"), "case-insensitive like Windows");
        assert!(!v.exists("Windows/nope"));
        assert_eq!(v.read("broom/boot-storage.ok").unwrap().unwrap(), b"storahci stornvme\r\n");
        assert_eq!(v.read("broom").unwrap(), None, "a directory has no content");
        assert!(v.root_names(12).iter().any(|n| n == "Windows"));
        let out = d.join("efi");
        v.extract_dir("broom/efi", &out).unwrap();
        assert_eq!(std::fs::read(out.join("EFI/Microsoft/Boot/BCD")).unwrap(), vec![7u8; 300_000]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
