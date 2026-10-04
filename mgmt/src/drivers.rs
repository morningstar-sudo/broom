// drivers.rs — custom Windows driver packages (VGA, LAN, audio…) installed into each machine's base.
// Upload: a .zip of an EXTRACTED driver folder (at least one .inf) → <home>/tftp/broom-drivers/<name>.tar.gz
// (the stage has tar/gzip, no unzip) + the hardware IDs read from its .inf files.
// Stage, every boot: POST /api/drivers/for?mac= (body = its PCI/USB IDs) → "name sha256" lines of the packages
// for this machine: a hardware ID matches, the package targets its group, or it is ticked "all machines".
// broom-done.ps1 (scripts/, written into the golden by winstage/prep.rs) pnputil-installs them while base is built.
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Query, State},
    http::StatusCode,
    routing::{get, post, put},
    Json, Router,
};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use crate::db::Driver;
use crate::api::{bad, ise, ApiError};
use crate::images::{valid_name, write_chunk, CHUNK_MAX};
use crate::SharedState;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/drivers", get(list))
        .route("/api/drivers/upload-start", post(upload_start))
        .route("/api/drivers/upload-chunk", put(upload_chunk).layer(DefaultBodyLimit::max(CHUNK_MAX)))
        .route("/api/drivers/upload-done", post(upload_done))
        .route("/api/drivers/targets", post(set_targets))
        .route("/api/drivers/delete", post(delete))
        .route("/api/drivers/for", post(for_machine))
}

// Distinct prefixes so a package named e.g. "up-foo" can't collide with the upload/extract dir of "foo"
// (both names are valid_name, so a shared prefix would overlap).
fn upload_dir(name: &str) -> PathBuf {
    crate::work_dir().join(format!("drvup.{name}"))
}

/// What the stage downloads: /tftp/broom-drivers/<name>.tar.gz.
fn tar_path(name: &str) -> PathBuf {
    crate::tftp_dir().join("broom-drivers").join(format!("{name}.tar.gz"))
}

/// .inf text: UTF-16 LE/BE when it has a BOM (NVIDIA/AMD ship those), else UTF-8/ANSI.
fn inf_text(b: &[u8]) -> String {
    let utf16 = |le: bool| {
        let u: Vec<u16> = b[2..]
            .chunks_exact(2)
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        String::from_utf16_lossy(&u)
    };
    match b {
        [0xFF, 0xFE, ..] => utf16(true),
        [0xFE, 0xFF, ..] => utf16(false),
        _ => String::from_utf8_lossy(b).into_owned(),
    }
}

/// Hardware IDs an .inf supports: `PCI\VEN_xxxx&DEV_yyyy`, `USB\VID_xxxx&PID_yyyy` (upper-case; suffixes like
/// &SUBSYS/&REV dropped — the stage reports vendor+device only).
fn inf_hwids(text: &str) -> BTreeSet<String> {
    let t = text.to_ascii_uppercase();
    let hex4 = |s: &str| s.len() >= 4 && s.as_bytes()[..4].iter().all(u8::is_ascii_hexdigit);
    let mut out = BTreeSet::new();
    for (pre, mid) in [("PCI\\VEN_", "&DEV_"), ("USB\\VID_", "&PID_")] {
        let mut rest = t.as_str();
        while let Some(i) = rest.find(pre) {
            rest = &rest[i + pre.len()..];
            if hex4(rest) && rest[4..].starts_with(mid) && hex4(&rest[4 + mid.len()..]) {
                out.insert(format!("{pre}{}{mid}{}", &rest[..4], &rest[4 + mid.len()..4 + mid.len() + 4]));
            }
        }
    }
    out
}

/// Packages one machine gets: a hardware ID matches, the package targets its group, or "all machines".
pub(crate) fn pick<'a>(drivers: &'a [Driver], hw: &BTreeSet<String>, grp: Option<&str>) -> Vec<&'a Driver> {
    drivers
        .iter()
        .filter(|d| {
            d.all_machines
                || grp.is_some_and(|g| d.groups.iter().any(|x| x.eq_ignore_ascii_case(g)))
                || d.hwids.iter().any(|id| hw.contains(id))
        })
        .collect()
}

/// Group names: letters/digits/_/-, max 32. Shared with the Machines page.
pub(crate) fn group_ok(g: &str) -> bool {
    (1..=32).contains(&g.len()) && g.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The uploaded zip → tar.gz for the stage + hardware IDs. Blocking. Returns (sha256, size, IDs, .inf count).
fn process(name: &str) -> Result<(String, u64, Vec<String>, usize), String> {
    let up = upload_dir(name);
    let zip = crate::golden::walk(&up)
        .into_iter()
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")))
        .ok_or("upload a .zip of the extracted driver folder")?;
    let ex = crate::work_dir().join(format!("drvex.{name}"));
    let _ = std::fs::remove_dir_all(&ex);
    let r = (|| {
        crate::golden::unzip(&zip, &ex)?;
        let infs: Vec<PathBuf> = crate::golden::walk(&ex)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("inf")))
            .collect();
        if infs.is_empty() {
            return Err("no .inf file in the zip — upload an extracted driver folder, not a vendor installer (.exe)".into());
        }
        let mut ids = BTreeSet::new();
        for inf in &infs {
            ids.extend(inf_hwids(&inf_text(&std::fs::read(inf).map_err(|e| format!("{}: {e}", inf.display()))?)));
        }
        let tar = tar_path(name);
        std::fs::create_dir_all(tar.parent().unwrap()).map_err(|e| e.to_string())?;
        let tmp = tar.with_extension("tmp");
        if let Err(e) = crate::archive::tar_gz(&ex, &tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("packing the driver: {e}"));
        }
        std::fs::rename(&tmp, &tar).map_err(|e| e.to_string())?;
        let sha = crate::hash::file_hash(&tar.to_string_lossy()).ok_or("sha256 of the package failed")?;
        let size = std::fs::metadata(&tar).map_err(|e| e.to_string())?.len();
        Ok((sha, size, ids.into_iter().collect(), infs.len()))
    })();
    let _ = std::fs::remove_dir_all(&ex);
    let _ = std::fs::remove_dir_all(&up);
    r
}

/// GET /api/drivers — packages + which known machines get each (hardware reported by their stage, group, all).
async fn list(State(st): State<SharedState>) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let drivers = st.db.drivers().map_err(ise)?;
    let machines = st.db.machines().map_err(ise)?;
    let hw: HashMap<String, BTreeSet<String>> =
        st.db.machine_hw().map_err(ise)?.into_iter().map(|(mac, ids)| (mac.to_lowercase(), ids.into_iter().collect())).collect();
    let mut gets: HashMap<i64, Vec<String>> = HashMap::new();
    for m in &machines {
        let ids = hw.get(&m.mac.to_lowercase()).cloned().unwrap_or_default();
        for d in pick(&drivers, &ids, m.grp.as_deref()) {
            gets.entry(d.id).or_default().push(m.hostname.clone().unwrap_or_else(|| m.mac.clone()));
        }
    }
    Ok(Json(
        drivers
            .iter()
            .map(|d| {
                let mut v = serde_json::to_value(d).unwrap_or_default();
                v["machines"] = gets.remove(&d.id).unwrap_or_default().into();
                v
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

/// POST /api/drivers/upload-start {name} — fresh upload folder.
async fn upload_start(Json(b): Json<NameBody>) -> Result<Json<serde_json::Value>, ApiError> {
    if !valid_name(&b.name) {
        return Err(bad("package name: 1-64 letters/digits/_/-"));
    }
    let dir = upload_dir(&b.name);
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.map_err(ise)?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct ChunkQuery {
    name: String,
    file: String,
    offset: u64,
    total: u64,
}

/// PUT /api/drivers/upload-chunk?name=&file=&offset=&total= — same chunk protocol as the golden upload.
async fn upload_chunk(Query(q): Query<ChunkQuery>, body: Body) -> Result<Json<serde_json::Value>, ApiError> {
    if !valid_name(&q.name) {
        return Err(bad("bad package name"));
    }
    // A driver package (extracted .inf folder, zipped) is small — 2 GB is generous.
    write_chunk(upload_dir(&q.name), &q.file, q.offset, q.total, 2 << 30, body).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// POST /api/drivers/upload-done {name} — unzip, read the .inf hardware IDs, pack the tar.gz (tens of seconds
/// for a big VGA package; the web shows "processing"). A re-upload with the same name replaces the files
/// and keeps the targets.
async fn upload_done(State(st): State<SharedState>, Json(b): Json<NameBody>) -> Result<Json<serde_json::Value>, ApiError> {
    if !valid_name(&b.name) || !upload_dir(&b.name).is_dir() {
        return Err(bad("no upload in progress (upload-start first)"));
    }
    let name = b.name.clone();
    let (sha, size, ids, infs) = tokio::task::spawn_blocking(move || process(&name)).await.map_err(ise)?.map_err(bad)?;
    let before = key_sets(&st);
    st.db.put_driver(&b.name, &sha, size, &ids, crate::now_secs() as i64).map_err(ise)?;
    rearm_changed(&st, before);
    tracing::info!(
        "driver package {}: {infs} .inf, {} hardware IDs, {:.1} MB",
        b.name,
        ids.len(),
        size as f64 / 1e6
    );
    Ok(Json(serde_json::json!({"ok": true, "hwids": ids.len(), "infs": infs})))
}

#[derive(Deserialize)]
struct TargetsBody {
    id: i64,
    all_machines: bool,
    /// Comma-separated group names.
    groups: String,
}

/// POST /api/drivers/targets {id, all_machines, groups} — besides the hardware match.
async fn set_targets(State(st): State<SharedState>, Json(b): Json<TargetsBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let groups: Vec<String> = b.groups.split(',').map(str::trim).filter(|g| !g.is_empty()).map(String::from).collect();
    if let Some(g) = groups.iter().find(|g| !group_ok(g)) {
        return Err(bad(format!("group {g:?}: letters/digits/_/-, max 32")));
    }
    let d = st.db.drivers().map_err(ise)?.into_iter().find(|d| d.id == b.id).ok_or((StatusCode::NOT_FOUND, "no such package".to_string()))?;
    let before = key_sets(&st);
    st.db.set_driver_targets(d.id, b.all_machines, &groups).map_err(ise)?;
    rearm_changed(&st, before);
    tracing::info!("driver package {}: all machines {}, groups [{}]", d.name, b.all_machines, groups.join(", "));
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct IdBody {
    id: i64,
}

/// POST /api/drivers/delete {id} — machines that had it drop it on their next boot (base rebuilt).
async fn delete(State(st): State<SharedState>, Json(b): Json<IdBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let d = st.db.drivers().map_err(ise)?.into_iter().find(|d| d.id == b.id).ok_or((StatusCode::NOT_FOUND, "no such package".to_string()))?;
    let before = key_sets(&st);
    st.db.delete_driver(d.id).map_err(ise)?;
    rearm_changed(&st, before);
    let _ = std::fs::remove_file(tar_path(&d.name));
    tracing::info!("driver package {} deleted", d.name);
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Machine id → the "name sha256" packages its stage gets, for every machine with a license key (from the hardware
/// IDs its stage last reported). Taken before a driver / group change, compared after by rearm_changed.
pub(crate) fn key_sets(st: &SharedState) -> std::collections::HashMap<i64, Vec<String>> {
    let (Ok(machines), Ok(hw), Ok(drivers)) = (st.db.machines(), st.db.machine_hw(), st.db.drivers()) else {
        return Default::default();
    };
    machines
        .into_iter()
        .filter(|m| m.license_key.is_some())
        .map(|m| {
            let ids: BTreeSet<String> =
                hw.iter().find(|(mac, _)| mac.eq_ignore_ascii_case(&m.mac)).map(|(_, h)| h.iter().cloned().collect()).unwrap_or_default();
            let set = pick(&drivers, &ids, m.grp.as_deref()).iter().map(|d| format!("{} {}", d.name, d.sha256)).collect();
            (m.id, set)
        })
        .collect()
}

/// Another package set than `before` → that machine's stage rebuilds base, which needs the license key once more.
pub(crate) fn rearm_changed(st: &SharedState, before: std::collections::HashMap<i64, Vec<String>>) {
    for (id, set) in key_sets(st) {
        if before.get(&id).is_some_and(|old| *old != set) && st.db.rearm_quiet(id).unwrap_or(false) {
            tracing::info!("license of machine {id} armed again (driver set changed → base rebuilt)");
        }
    }
}

#[derive(Deserialize)]
struct ForQuery {
    mac: String,
}

/// POST /api/drivers/for?mac=  body = the machine's hardware IDs, one per line (Windows stage, every boot).
/// → "name sha256" per line. The MAC is trusted: drivers are not secret (unlike license keys).
async fn for_machine(State(st): State<SharedState>, Query(q): Query<ForQuery>, body: String) -> Result<String, ApiError> {
    let mac = q.mac.trim().to_lowercase().replace('-', ":");
    if mac.len() != 17 || !mac.chars().all(|c| c.is_ascii_hexdigit() || c == ':') {
        return Err(bad("bad mac"));
    }
    let hw: BTreeSet<String> = body.lines().map(|l| l.trim().to_ascii_uppercase()).filter(|l| !l.is_empty()).take(4096).collect();
    let m = st.db.machines().map_err(ise)?.into_iter().find(|m| m.mac.eq_ignore_ascii_case(&mac));
    // Only remember hardware for a MAC we already know (registered, or currently holding a lease). An unknown MAC
    // still gets its driver list, but can't pollute machine_hw with junk.
    let known = m.is_some() || st.db.leases().map_err(ise)?.iter().any(|l| l.mac.eq_ignore_ascii_case(&mac));
    if known {
        st.db.put_machine_hw(&mac, &hw.iter().cloned().collect::<Vec<_>>(), crate::now_secs() as i64).map_err(ise)?;
    }
    let drivers = st.db.drivers().map_err(ise)?;
    let got = pick(&drivers, &hw, m.as_ref().and_then(|m| m.grp.as_deref()));
    // The stage asks this on every boot, near its end → the machine IS going through PXE (license window,
    // "not reset" check in license.rs). Refreshed here because a golden download can outlast the window opened at
    // /boot/start; it grants nothing /boot/start doesn't (both are open to any client). It never re-arms a key:
    // that is decided on the server (rearm_changed), never by what a client sends.
    st.saw_pxe(&mac, None);
    let who = m.and_then(|m| m.hostname).unwrap_or_else(|| mac.clone());
    tracing::info!(
        "client {who} drivers: {}",
        if got.is_empty() { "none".to_string() } else { got.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ") }
    );
    Ok(got.iter().map(|d| format!("{} {}\n", d.name, d.sha256)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inf_ids_utf8_and_utf16() {
        let inf = "[NVIDIA_Devices.NTamd64]\n%NVIDIA_DEV.2504% = Section001, PCI\\VEN_10DE&DEV_2504&SUBSYS_12345678\n\
                   %X% = S, pci\\ven_10de&dev_2504\nUSB\\VID_046d&PID_C52B&MI_00\nPCI\\VEN_ZZZZ&DEV_1\nHDAUDIO\\FUNC_01&VEN_10EC";
        let want: Vec<&str> = vec!["PCI\\VEN_10DE&DEV_2504", "USB\\VID_046D&PID_C52B"];
        assert_eq!(inf_hwids(&inf_text(inf.as_bytes())).iter().map(String::as_str).collect::<Vec<_>>(), want);
        let mut le = vec![0xFF, 0xFE];
        le.extend(inf.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(inf_hwids(&inf_text(&le)).iter().map(String::as_str).collect::<Vec<_>>(), want);
        let mut be = vec![0xFE, 0xFF];
        be.extend(inf.encode_utf16().flat_map(u16::to_be_bytes));
        assert_eq!(inf_hwids(&inf_text(&be)).len(), 2);
    }

    #[test]
    fn pick_by_hardware_group_or_all() {
        let d = |id, name: &str, hw: &[&str], all, groups: &[&str]| Driver {
            id,
            name: name.into(),
            sha256: String::new(),
            size: 0,
            hwids: hw.iter().map(|s| s.to_string()).collect(),
            all_machines: all,
            groups: groups.iter().map(|s| s.to_string()).collect(),
            created: 0,
        };
        let ds = [
            d(1, "nvidia", &["PCI\\VEN_10DE&DEV_2504"], false, &[]),
            d(2, "amd", &["PCI\\VEN_1002&DEV_73BF"], false, &["VIP"]),
            d(3, "tools", &[], true, &[]),
        ];
        let names = |hw: &[&str], grp| {
            let hw: BTreeSet<String> = hw.iter().map(|s| s.to_string()).collect();
            pick(&ds, &hw, grp).iter().map(|d| d.name.clone()).collect::<Vec<_>>()
        };
        assert_eq!(names(&["PCI\\VEN_10DE&DEV_2504"], None), ["nvidia", "tools"]);
        assert_eq!(names(&[], Some("vip")), ["amd", "tools"]); // group, case-insensitive
        assert_eq!(names(&[], None), ["tools"]);
        assert!(group_ok("VIP-1") && !group_ok("") && !group_ok("a b"));
    }
}
