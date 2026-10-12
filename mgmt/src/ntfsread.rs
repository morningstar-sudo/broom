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
        self.pos = p.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before the partition",
            )
        })?;
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
        let mut fs = BufReader::new(Part {
            f,
            base: start,
            len: size,
            pos: 0,
        });
        let mut ntfs = Ntfs::new(&mut fs).map_err(err)?;
        // Needed for the case-insensitive name lookups.
        ntfs.read_upcase_table(&mut fs).map_err(err)?;
        Ok(Vol { fs, ntfs })
    }

    /// Windows did not unmount it cleanly (hibernated, Fast Startup, forced power-off): its files may be half-written.
    pub fn is_dirty(&mut self) -> Result<bool, String> {
        Ok(self
            .ntfs
            .volume_info(&mut self.fs)
            .map_err(err)?
            .flags()
            .contains(NtfsVolumeFlags::IS_DIRTY))
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
        ntfs.root_directory(fs)
            .and_then(|root| entries(&root, fs))
            .map_or_else(
                |_| Vec::new(),
                |v| {
                    v.into_iter()
                        .map(|(n, _, _)| n)
                        .filter(|n| !n.starts_with('$') && n != ".")
                        .take(max)
                        .collect()
                },
            )
    }

    /// Copy directory `path` (recursively) to `dest` on the local disk.
    pub fn extract_dir(&mut self, path: &str, dest: &Path) -> Result<(), String> {
        let Vol { fs, ntfs } = self;
        let dir = find(ntfs, fs, path)?
            .filter(NtfsFile::is_directory)
            .ok_or_else(|| format!("{path}: no such directory"))?;
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
fn entries(
    dir: &NtfsFile,
    fs: &mut Fs,
) -> ntfs::Result<Vec<(String, ntfs::NtfsFileReference, bool)>> {
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
        v.push((
            key.name().to_string_lossy(),
            e.file_reference(),
            key.is_directory(),
        ));
    }
    Ok(v)
}

/// The unnamed $DATA stream of `f` into `w`.
fn copy_data(f: &NtfsFile, fs: &mut Fs, w: &mut impl Write) -> Result<(), String> {
    let Some(item) = f.data(fs, "") else {
        return Ok(());
    }; // no data stream = empty file
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
#[path = "../tests/unit/ntfsread.rs"]
mod tests;
