// archive.rs — the two archive formats broom writes, in-process (replaces the `cpio`/`gzip`/`tar` tools):
//   cpio "newc" + gzip — the small cpio appended to a Linux golden's initrd (overlay.rs);
//   tar.gz (ustar + GNU long names) — the EFI bundle and the driver packages the Windows stage unpacks.
// Both deterministic (mtime 0, uid/gid 0, sorted): same input → same bytes → same sha256 on the client.
use flate2::{Compression, write::GzEncoder};
use std::io::Write;
use std::path::Path;

/// One cpio member: path inside the archive (no leading /), mode (with type bits: 0o040000 dir, 0o100000 file), data.
pub struct Entry<'a> {
    pub path: &'a str,
    pub mode: u32,
    pub data: &'a [u8],
}

fn pad4(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}

/// cpio newc archive (the kernel's initramfs format), gzip-compressed.
pub fn cpio_gz(entries: &[Entry]) -> Vec<u8> {
    let mut c = Vec::new();
    let mut add = |ino: u32, name: &str, mode: u32, data: &[u8]| {
        let fields = [
            ino,
            mode,
            0,
            0,
            1,
            0,
            data.len() as u32,
            0,
            0,
            0,
            0,
            name.len() as u32 + 1,
            0,
        ];
        c.extend(b"070701");
        for f in fields {
            c.extend(format!("{f:08X}").as_bytes());
        }
        c.extend(name.as_bytes());
        c.push(0);
        pad4(&mut c);
        c.extend(data);
        pad4(&mut c);
    };
    for (i, e) in entries.iter().enumerate() {
        add(i as u32 + 1, e.path, e.mode, e.data);
    }
    add(0, "TRAILER!!!", 0, &[]);
    let mut gz = GzEncoder::new(Vec::new(), Compression::best());
    gz.write_all(&c).expect("write to Vec");
    gz.finish().expect("write to Vec")
}

/// tar.gz of everything under `dir` (paths relative to it, directories first in sorted order), written to `out`.
/// Regular files and directories only (a symlink in an uploaded package is refused, never followed).
pub fn tar_gz(dir: &Path, out: &Path) -> Result<(), String> {
    let f = std::fs::File::create(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut gz = GzEncoder::new(std::io::BufWriter::new(f), Compression::default());
    let mut paths = Vec::new();
    walk(dir, dir, &mut paths)?;
    paths.sort();
    for (rel, is_dir) in paths {
        let full = dir.join(&rel);
        if is_dir {
            header(&mut gz, &format!("{rel}/"), 0o755, 0, b'5')?;
        } else {
            let data = std::fs::read(&full).map_err(|e| format!("{}: {e}", full.display()))?;
            header(&mut gz, &rel, 0o644, data.len() as u64, b'0')?;
            gz.write_all(&data).map_err(|e| e.to_string())?;
            gz.write_all(&vec![0u8; (512 - data.len() % 512) % 512])
                .map_err(|e| e.to_string())?;
        }
    }
    gz.write_all(&[0u8; 1024]).map_err(|e| e.to_string())?; // end of archive: two zero blocks
    gz.finish()
        .and_then(|mut w| w.flush())
        .map_err(|e| format!("{}: {e}", out.display()))
}

/// Extract a tar.gz written by `tar_gz` (ustar + GNU long names; files + directories) into `dest`. Paths must stay
/// inside `dest` (no absolute paths, no `..`).
pub fn untar_gz(file: &Path, dest: &Path) -> Result<(), String> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let f = std::fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut r = GzDecoder::new(std::io::BufReader::new(f));
    let err = |e: std::io::Error| format!("{}: {e}", file.display());
    std::fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let mut long: Option<String> = None;
    loop {
        let mut h = [0u8; 512];
        r.read_exact(&mut h).map_err(err)?;
        if h.iter().all(|&b| b == 0) {
            return Ok(()); // end of archive
        }
        let field = |a: usize, b: usize| {
            String::from_utf8_lossy(&h[a..b])
                .trim_end_matches('\0')
                .trim()
                .to_string()
        };
        let size = u64::from_str_radix(&field(124, 136), 8)
            .map_err(|_| format!("{}: bad tar header", file.display()))?;
        let mut data = vec![0u8; size as usize];
        r.read_exact(&mut data).map_err(err)?;
        let pad = (512 - size % 512) % 512;
        r.read_exact(&mut vec![0u8; pad as usize]).map_err(err)?;
        let kind = h[156];
        if kind == b'L' {
            long = Some(
                String::from_utf8_lossy(&data)
                    .trim_end_matches('\0')
                    .to_string(),
            );
            continue;
        }
        let name = long.take().unwrap_or_else(|| field(0, 100));
        let rel = Path::new(name.trim_end_matches('/'));
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "{}: unsafe path {name:?} in the archive",
                file.display()
            ));
        }
        let out = dest.join(rel);
        match kind {
            b'5' => std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?,
            b'0' | 0 => {
                if let Some(p) = out.parent() {
                    std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
                }
                std::fs::write(&out, &data).map_err(|e| format!("{}: {e}", out.display()))?;
            }
            _ => {
                return Err(format!(
                    "{}: unsupported entry {name:?} (type {})",
                    file.display(),
                    kind as char
                ));
            }
        }
    }
}

fn walk(root: &Path, d: &Path, out: &mut Vec<(String, bool)>) -> Result<(), String> {
    for e in std::fs::read_dir(d).map_err(|e| format!("{}: {e}", d.display()))? {
        let e = e.map_err(|e| e.to_string())?;
        let ft = e.file_type().map_err(|e| e.to_string())?;
        let p = e.path();
        let rel = p
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned();
        if ft.is_dir() {
            out.push((rel, true));
            walk(root, &p, out)?;
        } else if ft.is_file() {
            out.push((rel, false));
        } else {
            return Err(format!("{rel}: not a regular file or directory"));
        }
    }
    Ok(())
}

/// One 512-byte ustar header (+ a GNU `././@LongLink` entry first when the name is over 100 bytes).
fn header(w: &mut impl Write, name: &str, mode: u32, size: u64, kind: u8) -> Result<(), String> {
    if name.len() > 100 {
        let mut long = name.as_bytes().to_vec();
        long.push(0);
        header(w, "././@LongLink", 0, long.len() as u64, b'L')?;
        long.resize(long.len().div_ceil(512) * 512, 0);
        w.write_all(&long).map_err(|e| e.to_string())?;
    }
    let mut h = [0u8; 512];
    let n = name.len().min(100);
    h[..n].copy_from_slice(&name.as_bytes()[..n]);
    let mut put = |off: usize, len: usize, v: u64| {
        let s = format!("{v:0w$o}", w = len - 1);
        h[off..off + len - 1].copy_from_slice(s.as_bytes());
    };
    put(100, 8, mode as u64);
    put(108, 8, 0); // uid
    put(116, 8, 0); // gid
    put(124, 12, size);
    put(136, 12, 0); // mtime
    h[156] = kind;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    // Checksum: sum of the header bytes with the checksum field read as spaces.
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    w.write_all(&h).map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "../tests/unit/archive.rs"]
mod tests;
