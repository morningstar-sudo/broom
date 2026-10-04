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
        std::os::unix::fs::symlink("/etc/passwd", src.join("evil")).unwrap();
        assert!(tar_gz(&src, &d.join("c.tar.gz")).is_err(), "symlinks refused");
        let _ = std::fs::remove_dir_all(&d);
    }
}
