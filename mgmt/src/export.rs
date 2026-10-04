// export.rs — an image (current golden or a saved version) → a VMware VM: <name>.vmx + <name>.vmdk, to edit the
// golden again (Windows: boots through the broom unattend, edit, run the Windows prep command, upload).
// Windows publish trims image.img in place (winstage::build_golden: holes outside the Windows partition + a
// one-partition GPT) → keep_boot_regions first saves what the trim destroys (the partition table, ESP/MSR/Recovery
// or System Reserved) to images/<name>/orig/, and the export stitches it back around the Windows partition.
// Linux goldens are never modified → converted as they are.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::disk::Table;
use crate::SharedState;


/// What the upload had outside the Windows partition: head.raw = [0, start), tail.raw = [start+size, total).
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Orig {
    start: u64,
    size: u64,
    total: u64,
    firmware: String,
}

fn orig_dir(name: &str) -> PathBuf {
    crate::images_dir().join(name).join("orig")
}

/// work/export/<name>/: <name>.vmx, <name>.vmdk, `version` (what was exported).
pub(crate) fn export_dir(name: &str) -> PathBuf {
    crate::work_dir().join("export").join(name)
}

/// Partition table → (already trimmed by publish = GPT with exactly one partition, firmware the disk boots with).
fn layout(t: &Table) -> (bool, &'static str) {
    // ESP: GPT type GUID, or MBR type ef.
    let esp = t.parts.iter().any(|p| p.kind.eq_ignore_ascii_case(crate::disk::ESP) || p.kind == "ef");
    (t.gpt && t.parts.len() == 1, if esp { "efi" } else { "bios" })
}

/// The single partition (start, size) in bytes of a trimmed golden, None if it has not exactly one.
fn single_part(t: &Table) -> Option<(u64, u64)> {
    match t.parts.as_slice() {
        [p] => Some((p.start, p.size)),
        _ => None,
    }
}

fn table(p: &Path) -> Result<Table, String> {
    crate::disk::read(&p.to_string_lossy())?.ok_or_else(|| format!("{}: no partition table", p.display()))
}
/// Copy [off, off+len) of `src` into a new sparse file `dst` (all-zero MB blocks stay holes).
fn copy_region(src: &Path, off: u64, len: u64, dst: &Path) -> Result<(), String> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let out = std::fs::File::create(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    out.set_len(len).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(buf.len() as u64) as usize;
        f.read_exact_at(&mut buf[..n], off + done).map_err(|e| format!("read {}: {e}", src.display()))?;
        if buf[..n].iter().any(|&b| b != 0) {
            out.write_all_at(&buf[..n], done).map_err(|e| format!("write {}: {e}", dst.display()))?;
        }
        done += n as u64;
    }
    Ok(())
}

/// Called by winstage::build_golden right BEFORE it trims image.img: keep the partition table + every partition
/// but Windows (start, size). An already-trimmed image.img (published before) keeps the orig/ of its upload.
pub(crate) fn keep_boot_regions(raw: &Path, name: &str, (start, size): (u64, u64)) -> Result<(), String> {
    let (trimmed, firmware) = layout(&table(raw)?);
    if trimmed {
        return Ok(());
    }
    let d = orig_dir(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let total = std::fs::metadata(raw).map_err(|e| e.to_string())?.len();
    copy_region(raw, 0, start, &d.join("head.raw"))?;
    copy_region(raw, start + size, total.saturating_sub(start + size), &d.join("tail.raw"))?;
    // orig.json last: its presence = the set is complete.
    let o = Orig { start, size, total, firmware: firmware.into() };
    std::fs::write(d.join("orig.json"), serde_json::to_vec(&o).unwrap_or_default()).map_err(|e| e.to_string())
}

/// Build work/export/<name>/ from the current golden (version None) or a saved version. Blocking (job).
pub fn run_export(st: &SharedState, name: &str, os: &str, version: Option<&str>, steps: &mut crate::publish::Steps) -> Result<String, String> {
    // Built in <dir>.new and renamed when complete: a download meanwhile gets "no export yet", never a half-written
    // vmdk. The old export goes first (its space is needed).
    let done = export_dir(name);
    let out = done.with_extension("new");
    let _ = std::fs::remove_dir_all(&done);
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let r = build(st, name, os, version, &out, steps);
    let _ = std::fs::remove_file(out.join("src.raw"));
    let r = r.and_then(|msg| std::fs::rename(&out, &done).map(|()| msg).map_err(|e| format!("{}: {e}", done.display())));
    if r.is_err() {
        let _ = std::fs::remove_dir_all(&out);
    }
    r
}

fn build(st: &SharedState, name: &str, os: &str, version: Option<&str>, out: &Path, steps: &mut crate::publish::Steps) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let img = crate::images_dir().join(name).join("image.img");
    // The vmdk holds about the data of the golden (+ a restored version needs its own copy first).
    let used = std::fs::metadata(&img).map_err(|_| format!("image {name} has no golden yet"))?.blocks() * 512;
    let copies = if version.is_some() { 2 } else { 1 };
    crate::publish::need_space(out, used * copies, "export")?;
    let src = match version {
        None => img,
        Some(v) => {
            steps.go(&format!("restore {v}"));
            let p = out.join("src.raw");
            let _g = st.versions_lock.lock().unwrap_or_else(|p| p.into_inner());
            crate::versions::rehydrate_to(name, v, &p)?;
            p
        }
    };
    let t = table(&src)?;
    let vmdk = out.join(format!("{name}.vmdk"));
    steps.go("convert → vmdk");
    let firmware = if os == "windows" {
        let d = orig_dir(name);
        let o: Orig = std::fs::read(d.join("orig.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or("this Windows image was published before export existed (its boot partitions are gone) — upload the VM again, then export")?;
        let total = std::fs::metadata(&src).map_err(|e| e.to_string())?.len();
        if single_part(&t) != Some((o.start, o.size)) || total != o.total {
            return Err("this version comes from another upload (different disk layout) — its boot partitions were not kept".into());
        }
        // One disk again: head + the Windows partition of the golden + tail, in one pass.
        let (head, tail) = (d.join("head.raw"), d.join("tail.raw"));
        let mut parts: Vec<(&Path, u64, Option<u64>)> = vec![(&head, 0, None), (&src, o.start, Some(o.size))];
        if o.total > o.start + o.size {
            parts.push((&tail, 0, None));
        }
        crate::vmdk::write_sparse(&crate::disk::Source::new(&parts)?, &vmdk)?;
        o.firmware
    } else {
        crate::vmdk::write_sparse(&crate::disk::Source::file(&src)?, &vmdk)?;
        layout(&t).1.to_string()
    };
    std::fs::write(out.join(format!("{name}.vmx")), vmx(name, os, &firmware)).map_err(|e| e.to_string())?;
    std::fs::write(out.join("version"), version.unwrap_or("current")).map_err(|e| e.to_string())?;
    let size = std::fs::metadata(&vmdk).map(|m| m.len()).unwrap_or(0);
    Ok(format!(
        "exported {} ({:.1} GB, {firmware}) — download {name}.vmx + {name}.vmdk on the Images page into ONE folder, open the .vmx",
        version.unwrap_or("current golden"),
        size as f64 / 1e9
    ))
}

/// Export on the Images page: {version, size, created} or null.
pub(crate) fn export_info(name: &str) -> serde_json::Value {
    let d = export_dir(name);
    let (Ok(m), Ok(ver)) = (std::fs::metadata(d.join(format!("{name}.vmdk"))), std::fs::read_to_string(d.join("version"))) else {
        return serde_json::Value::Null;
    };
    let created = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
    serde_json::json!({"version": ver, "size": m.len(), "created": created})
}

/// Plain VMware Workstation VM around the exported disk. SATA boots under BIOS and EFI alike (the Windows prep
/// set storahci to boot-start); guestOS windows9-64 = "Windows 10 and later" — windows11-64 insists on a TPM.
fn vmx(name: &str, os: &str, firmware: &str) -> String {
    let guest = if os == "windows" { "windows9-64" } else { "ubuntu-64" };
    let fw = if firmware == "efi" { "firmware = \"efi\"\n" } else { "" };
    let bridges: String = (4..8)
        .map(|i| format!("pciBridge{i}.present = \"TRUE\"\npciBridge{i}.virtualDev = \"pcieRootPort\"\npciBridge{i}.functions = \"8\"\n"))
        .collect();
    format!(
        ".encoding = \"UTF-8\"\nconfig.version = \"8\"\nvirtualHW.version = \"19\"\ndisplayName = \"{name}\"\nguestOS = \"{guest}\"\n{fw}\
         memsize = \"8192\"\nnumvcpus = \"4\"\ncpuid.coresPerSocket = \"2\"\npciBridge0.present = \"TRUE\"\n{bridges}\
         sata0.present = \"TRUE\"\nsata0:0.present = \"TRUE\"\nsata0:0.fileName = \"{name}.vmdk\"\n\
         ethernet0.present = \"TRUE\"\nethernet0.virtualDev = \"e1000e\"\nethernet0.connectionType = \"nat\"\nethernet0.addressType = \"generated\"\n\
         usb.present = \"TRUE\"\nehci.present = \"TRUE\"\nusb_xhci.present = \"TRUE\"\nsvga.present = \"TRUE\"\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_single_partition() {
        use crate::disk::Part;
        let part = |s: u64, n: u64, k: &str| Part { start: s * 512, size: n * 512, kind: k.into() };
        let gpt = |parts: Vec<Part>| Table { gpt: true, sector: 512, parts };
        let esp = part(2048, 204800, "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        let win = part(239616, 1000000, "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7");
        // Fresh UEFI upload: several partitions with an ESP → not trimmed, efi.
        assert_eq!(layout(&gpt(vec![esp.clone(), win.clone()])), (false, "efi"));
        // After publish: one GPT partition → trimmed.
        assert!(layout(&gpt(vec![win.clone()])).0);
        assert_eq!(single_part(&gpt(vec![win.clone()])), Some((239616 * 512, 1000000 * 512)));
        assert_eq!(single_part(&gpt(vec![esp, win])), None);
        // Legacy BIOS (MBR, System Reserved + Windows): not trimmed even with one partition, bios.
        let dos = Table { gpt: false, sector: 512, parts: vec![part(2048, 100, "7")] };
        assert_eq!(layout(&dos), (false, "bios"));
    }

    #[test]
    fn region_copy_keeps_bytes_and_holes() {
        use std::os::unix::fs::{FileExt, MetadataExt};
        let d = std::env::temp_dir().join("broom_t_export");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let src = d.join("src");
        let f = std::fs::File::create(&src).unwrap();
        f.set_len(8 << 20).unwrap();
        f.write_all_at(b"head", 100).unwrap();
        f.write_all_at(b"tail", (7 << 20) + 5).unwrap();
        let dst = d.join("dst");
        copy_region(&src, 50, (8 << 20) - 50, &dst).unwrap();
        let got = std::fs::read(&dst).unwrap();
        assert_eq!(got.len(), (8 << 20) - 50);
        assert_eq!(&got[50..54], b"head");
        assert_eq!(&got[(7 << 20) + 5 - 50..(7 << 20) + 9 - 50], b"tail");
        // 2 data MB written, the 6 zero MB between stay holes.
        assert!(std::fs::metadata(&dst).unwrap().blocks() * 512 <= 3 << 20);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn vmx_firmware_and_disk() {
        let w = vmx("win", "windows", "efi");
        assert!(w.contains("firmware = \"efi\"") && w.contains("sata0:0.fileName = \"win.vmdk\"") && w.contains("windows9-64"));
        let l = vmx("ubnt", "linux", "bios");
        assert!(!l.contains("firmware") && l.contains("ubuntu-64") && l.contains("ethernet0.virtualDev = \"e1000e\""));
    }
}
