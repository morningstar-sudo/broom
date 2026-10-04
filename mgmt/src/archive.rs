// archive.rs — the two archive formats broom writes, in-process (replaces the `cpio`/`gzip`/`tar` tools):
//   cpio "newc" + gzip — the small cpio appended to a Linux golden's initrd (overlay.rs);
//   tar.gz (ustar + GNU long names) — the EFI bundle and the driver packages the Windows stage unpacks.
// Both deterministic (mtime 0, uid/gid 0, sorted): same input → same bytes → same sha256 on the client.
use flate2::{write::GzEncoder, Compression};
use std::io::Write;
use std::path::Path;

/// One cpio member: path inside the archive (no leading /), mode (with type bits: 0o040000 dir, 0o100000 file), data.
pub struct Entry<'a> {
    pub path: &'a str,
    pub mode: u32,
    pub data: &'a [u8],
}

fn pad4(v: &mut Vec<u8>) {
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

/// cpio newc archive (the kernel's initramfs format), gzip-compressed.
pub fn cpio_gz(entries: &[Entry]) -> Vec<u8> {
    let mut c = Vec::new();
    let mut add = |ino: u32, name: &str, mode: u32, data: &[u8]| {
        let fields = [ino, mode, 0, 0, 1, 0, data.len() as u32, 0, 0, 0, 0, name.len() as u32 + 1, 0];
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
            gz.write_all(&vec![0u8; (512 - data.len() % 512) % 512]).map_err(|e| e.to_string())?;
        }
    }
    gz.write_all(&[0u8; 1024]).map_err(|e| e.to_string())?; // end of archive: two zero blocks
    gz.finish().and_then(|mut w| w.flush()).map_err(|e| format!("{}: {e}", out.display()))
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
        let field = |a: usize, b: usize| String::from_utf8_lossy(&h[a..b]).trim_end_matches('\0').trim().to_string();
        let size = u64::from_str_radix(&field(124, 136), 8).map_err(|_| format!("{}: bad tar header", file.display()))?;
        let mut data = vec![0u8; size as usize];
        r.read_exact(&mut data).map_err(err)?;
        let pad = (512 - size % 512) % 512;
        r.read_exact(&mut vec![0u8; pad as usize]).map_err(err)?;
        let kind = h[156];
        if kind == b'L' {
            long = Some(String::from_utf8_lossy(&data).trim_end_matches('\0').to_string());
            continue;
        }
        let name = long.take().unwrap_or_else(|| field(0, 100));
        let rel = Path::new(name.trim_end_matches('/'));
        if rel.is_absolute() || rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            return Err(format!("{}: unsafe path {name:?} in the archive", file.display()));
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
            _ => return Err(format!("{}: unsupported entry {name:?} (type {})", file.display(), kind as char)),
        }
    }
}

fn walk(root: &Path, d: &Path, out: &mut Vec<(String, bool)>) -> Result<(), String> {
    for e in std::fs::read_dir(d).map_err(|e| format!("{}: {e}", d.display()))? {
        let e = e.map_err(|e| e.to_string())?;
        let ft = e.file_type().map_err(|e| e.to_string())?;
        let p = e.path();
        let rel = p.strip_prefix(root).map_err(|e| e.to_string())?.to_string_lossy().into_owned();
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
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn cpio_lists_and_extracts() {
        let d = std::env::temp_dir().join("broom_t_cpio");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("x")).unwrap();
        let gz = cpio_gz(&[
            Entry { path: "scripts", mode: 0o040755, data: &[] },
            Entry { path: "scripts/local-top", mode: 0o040755, data: &[] },
            Entry { path: "scripts/local-top/iscsi", mode: 0o100755, data: b"#!/bin/sh\necho hi\n" },
            Entry { path: "etc/overlayroot.conf", mode: 0o100644, data: b"x=1\n" },
        ]);
        std::fs::write(d.join("a.cpio.gz"), &gz).unwrap();
        let sh = "cd x && gzip -dc ../a.cpio.gz | cpio -idm --quiet 2>/dev/null; true";
        assert!(Command::new("sh").args(["-c", sh]).current_dir(&d).status().unwrap().success());
        use std::os::unix::fs::PermissionsExt;
        let hook = d.join("x/scripts/local-top/iscsi");
        assert_eq!(std::fs::read(&hook).unwrap(), b"#!/bin/sh\necho hi\n");
        assert_eq!(std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777, 0o755, "hook stays executable");
        let listed = Command::new("sh").args(["-c", "gzip -dc a.cpio.gz | cpio -t --quiet"]).current_dir(&d).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&listed.stdout).lines().count(), 4);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tar_roundtrip_long_names_deterministic() {
        let d = std::env::temp_dir().join("broom_t_tar");
        let _ = std::fs::remove_dir_all(&d);
        let src = d.join("src");
        let long = format!("{}/{}.inf", "very-long-directory-name".repeat(4), "x".repeat(60));
        std::fs::create_dir_all(src.join("EFI/Microsoft/Boot")).unwrap();
        std::fs::create_dir_all(src.join(&long).parent().unwrap()).unwrap();
        std::fs::write(src.join("EFI/Microsoft/Boot/BCD"), vec![7u8; 70000]).unwrap();
        std::fs::write(src.join("empty.txt"), b"").unwrap();
        std::fs::write(src.join(&long), b"[Version]\n").unwrap();
        tar_gz(&src, &d.join("a.tar.gz")).unwrap();
        tar_gz(&src, &d.join("b.tar.gz")).unwrap();
        assert_eq!(std::fs::read(d.join("a.tar.gz")).unwrap(), std::fs::read(d.join("b.tar.gz")).unwrap(), "deterministic");
        std::fs::create_dir_all(d.join("out")).unwrap();
        assert!(Command::new("tar").args(["-xzf", "../a.tar.gz"]).current_dir(d.join("out")).status().unwrap().success());
        assert_eq!(std::fs::read(d.join("out/EFI/Microsoft/Boot/BCD")).unwrap(), vec![7u8; 70000]);
        assert_eq!(std::fs::read(d.join("out").join(&long)).unwrap(), b"[Version]\n", "GNU long name");
        assert!(d.join("out/empty.txt").exists());
        // Our own reader (stage bundle) gets the same tree back.
        untar_gz(&d.join("a.tar.gz"), &d.join("mine")).unwrap();
        assert_eq!(std::fs::read(d.join("mine/EFI/Microsoft/Boot/BCD")).unwrap(), vec![7u8; 70000]);
        assert_eq!(std::fs::read(d.join("mine").join(&long)).unwrap(), b"[Version]\n");
        assert!(d.join("mine/empty.txt").is_file());
        // Paths escaping the target are refused.
        for bad in ["../x", "/etc/x", "a/../../x"] {
            let mut gz = GzEncoder::new(Vec::new(), Compression::default());
            header(&mut gz, bad, 0o644, 2, b'0').unwrap();
            gz.write_all(&[b'h', b'i']).unwrap();
            gz.write_all(&[0u8; 510 + 1024]).unwrap();
            let evil = d.join("evil.tar.gz");
            std::fs::write(&evil, gz.finish().unwrap()).unwrap();
            assert!(untar_gz(&evil, &d.join("evil")).unwrap_err().contains("unsafe path"), "{bad}");
        }
        std::os::unix::fs::symlink("/etc/passwd", src.join("evil")).unwrap();
        assert!(tar_gz(&src, &d.join("c.tar.gz")).is_err(), "symlinks refused");
        let _ = std::fs::remove_dir_all(&d);
    }
}
