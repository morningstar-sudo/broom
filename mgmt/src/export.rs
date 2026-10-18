// export.rs — an image (current golden or a saved version) → a VMware VM: <name>.vmx + <name>.vmdk, to edit the
// golden again (Windows: boots through the broom unattend, edit, run the Windows prep command, upload).
// Windows publish trims image.img in place (winstage::build_golden: holes outside the Windows partition + a
// one-partition GPT) → keep_boot_regions first saves what the trim destroys (the partition table, ESP/MSR/Recovery
// or System Reserved) to images/<name>/orig/, and the export stitches it back around the Windows partition.
// Linux goldens are never modified → converted as they are.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::SharedState;
use crate::disk::Table;

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
    let esp = t
        .parts
        .iter()
        .any(|p| p.kind.eq_ignore_ascii_case(crate::disk::ESP) || p.kind == "ef");
    (
        t.gpt && t.parts.len() == 1,
        if esp { "efi" } else { "bios" },
    )
}

/// The single partition (start, size) in bytes of a trimmed golden, None if it has not exactly one.
fn single_part(t: &Table) -> Option<(u64, u64)> {
    match t.parts.as_slice() {
        [p] => Some((p.start, p.size)),
        _ => None,
    }
}

fn table(p: &Path) -> Result<Table, String> {
    crate::disk::read(&p.to_string_lossy())?
        .ok_or_else(|| format!("{}: no partition table", p.display()))
}
/// Called by winstage::build_golden right BEFORE it trims image.img: keep the partition table + every partition
/// but Windows (start, size). An already-trimmed image.img (published before) keeps the orig/ of its upload.
pub(crate) fn keep_boot_regions(
    raw: &Path,
    name: &str,
    (start, size): (u64, u64),
) -> Result<(), String> {
    let (trimmed, firmware) = layout(&table(raw)?);
    if trimmed {
        return Ok(());
    }
    let d = orig_dir(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let total = std::fs::metadata(raw).map_err(|e| e.to_string())?.len();
    crate::disk::copy_region(raw, 0, Some(start), &d.join("head.raw"))?;
    crate::disk::copy_region(raw, start + size, None, &d.join("tail.raw"))?;
    // orig.json last: its presence = the set is complete.
    let o = Orig {
        start,
        size,
        total,
        firmware: firmware.into(),
    };
    std::fs::write(
        d.join("orig.json"),
        serde_json::to_vec(&o).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())
}

/// Build work/export/<name>/ from the current golden (version None) or a saved version. Blocking (job).
pub fn run_export(
    st: &SharedState,
    name: &str,
    os: &str,
    version: Option<&str>,
    steps: &mut crate::publish::Steps,
) -> Result<String, String> {
    // Built in <dir>.new and renamed when complete: a download meanwhile gets "no export yet", never a half-written
    // vmdk. The old export goes first (its space is needed).
    let done = export_dir(name);
    let out = done.with_extension("new");
    let _ = std::fs::remove_dir_all(&done);
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let r = build(st, name, os, version, &out, steps);
    let _ = std::fs::remove_file(out.join("src.raw"));
    let r = r.and_then(|msg| {
        std::fs::rename(&out, &done)
            .map(|()| msg)
            .map_err(|e| format!("{}: {e}", done.display()))
    });
    if r.is_err() {
        let _ = std::fs::remove_dir_all(&out);
    }
    r
}

fn build(
    st: &SharedState,
    name: &str,
    os: &str,
    version: Option<&str>,
    out: &Path,
    steps: &mut crate::publish::Steps,
) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let img = crate::images_dir().join(name).join("image.img");
    // The vmdk holds about the data of the golden (+ a restored version needs its own copy first).
    let used = std::fs::metadata(&img)
        .map_err(|_| format!("image {name} has no golden yet"))?
        .blocks()
        * 512;
    let copies = if version.is_some() { 2 } else { 1 };
    crate::publish::need_space(out, used * copies, "export")?;
    let src = match version {
        None => img,
        Some(v) => {
            steps.go(&format!("restore {v}"));
            let p = out.join("src.raw");
            let _g = st.versions_lock.lock();
            crate::versions::rehydrate_to(name, v, &p)?;
            p
        }
    };
    let vmdk = out.join(format!("{name}.vmdk"));
    steps.go("convert → vmdk");
    let (disk, firmware) = full_disk(name, os, &src)?;
    crate::vmdk::write_sparse(&disk, &vmdk)?;
    std::fs::write(out.join(format!("{name}.vmx")), vmx(name, os, &firmware))
        .map_err(|e| e.to_string())?;
    std::fs::write(out.join("version"), version.unwrap_or("current")).map_err(|e| e.to_string())?;
    let size = std::fs::metadata(&vmdk).map(|m| m.len()).unwrap_or(0);
    Ok(format!(
        "exported {} ({:.1} GB, {firmware}) — download {name}.vmx + {name}.vmdk on the Images page into ONE folder, open the .vmx",
        version.unwrap_or("current golden"),
        size as f64 / 1e9
    ))
}

/// The whole disk an image's golden `src` came from, + the firmware it boots with ("efi" | "bios"): a Windows golden
/// trimmed by its publish gets its kept boot partitions back around the Windows partition (export, deploy); anything
/// else is `src` as it is.
pub(crate) fn full_disk(
    name: &str,
    os: &str,
    src: &Path,
) -> Result<(crate::disk::Source, String), String> {
    let t = table(src)?;
    let (trimmed, firmware) = layout(&t);
    if os != "windows" || !trimmed {
        return Ok((crate::disk::Source::file(src)?, firmware.to_string()));
    }
    let d = orig_dir(name);
    let o: Orig = std::fs::read(d.join("orig.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or("this Windows image was published before its boot partitions were kept — upload the VM again")?;
    let total = std::fs::metadata(src).map_err(|e| e.to_string())?.len();
    if single_part(&t) != Some((o.start, o.size)) || total != o.total {
        return Err("this version comes from another upload (different disk layout) — its boot partitions were not kept".into());
    }
    // One disk again: head + the Windows partition of the golden + tail.
    let (head, tail) = (d.join("head.raw"), d.join("tail.raw"));
    let mut parts: Vec<(&Path, u64, Option<u64>)> =
        vec![(&head, 0, None), (src, o.start, Some(o.size))];
    if o.total > o.start + o.size {
        parts.push((&tail, 0, None));
    }
    Ok((crate::disk::Source::new(&parts)?, o.firmware))
}

/// GET /api/images/deploy-map?name= (owner) — what a client's Deploy (capture.rs) writes: the image's whole disk
/// ({total, firmware, stamp, ranges: [[start, end]…]} — only the ranges that hold data). `stamp` names this golden:
/// the chunks are refused once it changed (a publish / upload meanwhile), so a disk never gets two goldens mixed.
pub(crate) async fn deploy_map(
    axum::extract::State(st): axum::extract::State<SharedState>,
    axum::extract::Query(q): axum::extract::Query<DeployQ>,
    axum::Extension(w): axum::Extension<crate::auth::Who>,
) -> Result<axum::Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let (img, src) = deploy_image(&st, &q.name)?;
    let name = img.name.clone();
    let (disk, firmware) = tokio::task::spawn_blocking(move || full_disk(&name, "windows", &src))
        .await
        .map_err(|e| bad(500, e.to_string()))?
        .map_err(|e| bad(409, e))?;
    tracing::warn!("{}: deploying image {} to a client disk", w.name, img.name);
    Ok(axum::Json(serde_json::json!({
        "total": disk.len,
        "firmware": firmware,
        "stamp": stamp(&q.name),
        "ranges": disk.data_ranges(),
    })))
}

/// GET /api/images/deploy-chunk?name=&stamp=&offset=&len= (owner) — `len` (≤ 16 MB) bytes of that whole disk.
pub(crate) async fn deploy_chunk(
    axum::extract::State(st): axum::extract::State<SharedState>,
    axum::extract::Query(q): axum::extract::Query<DeployQ>,
) -> Result<Vec<u8>, (axum::http::StatusCode, String)> {
    let (_, src) = deploy_image(&st, &q.name)?;
    if q.stamp.is_empty() || q.stamp != stamp(&q.name) {
        return Err(bad(
            409,
            "the image changed meanwhile (published again) — deploy again".into(),
        ));
    }
    let (off, len) = (q.offset, q.len.min(crate::images::CHUNK_MAX as u64));
    let name = q.name.clone();
    tokio::task::spawn_blocking(move || {
        let (disk, _) = full_disk(&name, "windows", &src)?;
        if off.checked_add(len).is_none_or(|e| e > disk.len) {
            return Err("past the end of the disk".to_string());
        }
        let mut buf = vec![0u8; len as usize];
        disk.read_at(off, &mut buf)?;
        Ok(buf)
    })
    .await
    .map_err(|e| bad(500, e.to_string()))?
    .map_err(|e| bad(400, e))
}

#[derive(Deserialize)]
pub(crate) struct DeployQ {
    name: String,
    #[serde(default)]
    stamp: String,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    len: u64,
}

fn bad(code: u16, msg: String) -> (axum::http::StatusCode, String) {
    (
        axum::http::StatusCode::from_u16(code).unwrap_or(axum::http::StatusCode::BAD_REQUEST),
        msg,
    )
}

/// A Windows image with a golden and no job running → (its row, image.img).
fn deploy_image(
    st: &SharedState,
    name: &str,
) -> Result<(crate::db::Image, PathBuf), (axum::http::StatusCode, String)> {
    let img = st
        .db
        .image_by_name(name)
        .map_err(|e| bad(500, e.to_string()))?
        .filter(|i| i.os == "windows")
        .ok_or_else(|| bad(404, format!("no Windows image {name:?}")))?;
    if st.job_running(name) {
        return Err(crate::api::busy(name));
    }
    let src = crate::images_dir().join(name).join("image.img");
    if !src.is_file() {
        return Err(bad(404, format!("image {name} has no golden yet")));
    }
    Ok((img, src))
}

/// Which golden image.img holds now (inode + change time): a new upload replaces the file, a publish trims it.
fn stamp(name: &str) -> String {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(crate::images_dir().join(name).join("image.img"))
        .map(|m| format!("{}-{}-{}", m.ino(), m.ctime(), m.ctime_nsec()))
        .unwrap_or_default()
}

/// Export on the Images page: {version, size, created} or null.
pub(crate) fn export_info(name: &str) -> serde_json::Value {
    let d = export_dir(name);
    let (Ok(m), Ok(ver)) = (
        std::fs::metadata(d.join(format!("{name}.vmdk"))),
        std::fs::read_to_string(d.join("version")),
    ) else {
        return serde_json::Value::Null;
    };
    let created = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    serde_json::json!({"version": ver, "size": m.len(), "created": created})
}

/// Plain VMware Workstation VM around the exported disk. SATA boots under BIOS and EFI alike (the Windows prep
/// set storahci to boot-start); guestOS windows9-64 = "Windows 10 and later" — windows11-64 insists on a TPM.
fn vmx(name: &str, os: &str, firmware: &str) -> String {
    let guest = if os == "windows" {
        "windows9-64"
    } else {
        "ubuntu-64"
    };
    let fw = if firmware == "efi" {
        "firmware = \"efi\"\n"
    } else {
        ""
    };
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
#[path = "../tests/unit/export.rs"]
mod tests;
