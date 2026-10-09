// images.rs — images API: create / delete / default, chunked golden upload → convert + publish jobs, versions
// (versions.rs: snapshot / rollback / new image from a version), export as a VMware VM, and the golden
// prep scripts (Linux /broom-prep, Windows one-time link).
use axum::{
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post, put},
    Json, Router,
};
use serde::Deserialize;
use std::collections::HashMap;

use crate::api::{ise, ApiError};
use crate::db::NewImage as NewImageRow;
use crate::SharedState;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/images", get(list).post(create))
        .route("/api/images/default", post(set_default))
        .route("/api/images/delete", post(delete))
        .route("/api/images/publish", post(publish_now))
        .route("/api/images/boot-script", post(set_boot_script))
        .route("/api/images/cache-mode", post(set_cache_mode))
        .route("/api/images/base-mode", post(set_base_mode))
        .route("/api/images/ssd", post(set_use_ssd))
        .route("/api/images/groups", post(set_groups))
        .route("/api/images/preload", post(set_preload))
        .route("/api/cache-list", get(cache_list))
        .route("/api/images/job", get(job_status))
        .route("/broom-prep", get(broom_prep))
        .route("/broom-prep-win", get(broom_prep_win))
        .route("/api/images/upload-start", post(upload_start))
        .route("/api/images/upload-chunk", put(upload_chunk).layer(DefaultBodyLimit::max(CHUNK_MAX)))
        .route("/api/images/upload-done", post(upload_done))
        .route("/api/images/snapshot", post(snapshot))
        .route("/api/images/snapshots", get(snapshots))
        .route("/api/images/rollback", post(rollback))
        .route("/api/images/version-delete", post(version_delete))
        .route("/api/images/export", post(export))
        .route("/api/images/from-version", post(from_version))
        .route("/api/images/export-file", get(export_file))
}

/// Image name by id, 404 if missing.
fn name_of(st: &SharedState, id: i64) -> Result<String, ApiError> {
    st.db.image(id).map_err(ise)?.map(|i| i.name).ok_or((StatusCode::NOT_FOUND, format!("image {id} not found")))
}

/// Image rows + size of the golden being served (image.img): `size` = virtual disk, `used` = bytes on disk (sparse);
/// `job` = its job status (a page opened while a job runs picks it up from here).
async fn list(State(st): State<SharedState>) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    use std::os::unix::fs::MetadataExt;
    let jobs = st.jobs.lock().unwrap().clone();
    let rows = st.db.images().map_err(ise)?.into_iter().map(|i| {
        let m = std::fs::metadata(crate::images_dir().join(&i.name).join("image.img")).ok();
        let mut v = serde_json::to_value(&i).unwrap_or_default();
        v["size"] = m.as_ref().map(|m| m.len()).into();
        v["used"] = m.as_ref().map(|m| m.blocks() * 512).into();
        v["export"] = crate::export::export_info(&i.name);
        v["job"] = jobs.get(&i.name).cloned().into();
        v
    });
    Ok(Json(rows.collect()))
}

#[derive(Deserialize)]
struct NewImage {
    name: String,
    os: String,
    boot_script: Option<String>,
    cache_mode: Option<String>,
}

/// Image / package name: letters, digits, `_`, `-`, at most 64 (the boot menu passes it on as is, /boot/start keeps 64).
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

async fn create(
    State(st): State<SharedState>,
    Json(b): Json<NewImage>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !valid_name(&b.name) {
        return Err((StatusCode::BAD_REQUEST, "name: 1-64 letters/digits/_/-".into()));
    }
    if b.os != "linux" && b.os != "windows" {
        return Err((StatusCode::BAD_REQUEST, "os must be 'linux' or 'windows'".into()));
    }
    // One folder per image under the binary's directory.
    std::fs::create_dir_all(crate::images_dir().join(&b.name))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let cache_mode = match b.cache_mode.as_deref() {
        Some("zram") => "zram",
        _ => "disk",
    };
    let id = st
        .db
        .add_image(&NewImageRow {
            name: &b.name,
            os: &b.os,
            boot_script: b.boot_script.as_deref(),
            cache_mode,
        })
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    tracing::info!("image {} created ({}, cache {cache_mode})", b.name, b.os);
    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}

/// Delete an image: DB row + folder images/<name>/ + its versions (freed chunks collected).
async fn delete(
    State(st): State<SharedState>,
    Json(b): Json<IdBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let name = name_of(&st, b.id)?;
    {
        // Check + claim under one lock (as spawn_job): no publish can start while the files go away.
        let mut jobs = st.jobs.lock().unwrap();
        if jobs.get(&name).is_some_and(|s| s.starts_with('⏳')) {
            return Err((StatusCode::CONFLICT, format!("image '{name}' has a running job — wait for it to finish")));
        }
        jobs.insert(name.clone(), "⏳ deleting...".into());
    }
    let (n, st2, id) = (name.clone(), st.clone(), b.id);
    let res = tokio::task::spawn_blocking(move || {
        // A Linux client booted from it reads its root disk from this target → never pull it from under it.
        if valid_name(&n) && crate::publish::image_in_use(&st2, &n) {
            return Err((StatusCode::CONFLICT, format!("clients are booted from image '{n}' — shut them down first")));
        }
        // Files first, row last: a crash midway leaves the image listed (delete it again), never orphan files.
        let mut freed = 0;
        if valid_name(&n) {
            crate::publish::unpublish(&st2, &n); // target, zram, tftp/ boot files (golden.vhdx)
            let _ = std::fs::remove_dir_all(crate::images_dir().join(&n));
            let _ = std::fs::remove_dir_all(crate::export::export_dir(&n));
            let _g = st2.versions_lock.lock().unwrap_or_else(|p| p.into_inner());
            freed = crate::versions::delete_all(&n).unwrap_or(0);
        }
        st2.db.delete_image(id).map_err(ise)?;
        Ok(freed)
    })
    .await
    .map_err(|e| ise(e.to_string()))
    .and_then(|r| r);
    st.jobs.lock().unwrap().remove(&name);
    let freed = res?;
    tracing::info!("image {name} deleted ({freed} version chunks freed)");
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Publish an image again (runs publish::run_publish). Used after changing a Linux golden.
async fn publish_now(
    State(st): State<SharedState>,
    Json(b): Json<IdBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    spawn_publish(&st, name_of(&st, b.id)?, None)?;
    Ok(Json(serde_json::json!({"ok": true, "async": true})))
}

#[derive(Deserialize)]
struct BootScriptBody {
    id: i64,
    boot_script: String,
}

/// Set the iPXE boot snippet of an image (Linux kernel/initrd).
async fn set_boot_script(
    State(st): State<SharedState>,
    Json(b): Json<BootScriptBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    st.db.set_boot_script(b.id, &b.boot_script).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct IdBody {
    id: i64,
}

async fn set_default(
    State(st): State<SharedState>,
    Json(b): Json<IdBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    st.db.set_default_image(b.id).map_err(ise)?;
    tracing::info!("image {} is now the default", name_of(&st, b.id)?);
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct SnapBody {
    id: i64,
    #[serde(default)]
    label: String,
}

/// Save image.img as a new version (versions.rs: a hard link, instant). Background job.
async fn snapshot(State(st): State<SharedState>, Json(b): Json<SnapBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let name = name_of(&st, b.id)?;
    let label: String = b.label.chars().filter(|c| !c.is_control()).take(80).collect();
    spawn_job(&st, name, "snapshot", move |st, name, steps| {
        steps.go("snapshot");
        let _g = st.versions_lock.lock().unwrap_or_else(|p| p.into_inner());
        let m = crate::versions::snapshot(name, label.trim())?;
        st.db.set_active_version(name_id(st, name)?, Some(&m.version))?;
        Ok(format!("version {} saved", m.version))
    })?;
    Ok(Json(serde_json::json!({"ok": true, "async": true})))
}

#[derive(Deserialize)]
struct VersionBody {
    id: i64,
    version: String,
}

/// Roll an image back to a version, then publish again (clients get it via the new hash). Background job.
async fn rollback(State(st): State<SharedState>, Json(b): Json<VersionBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let img = st.db.image(b.id).map_err(ise)?.ok_or((StatusCode::NOT_FOUND, format!("image {} not found", b.id)))?;
    let version = b.version;
    spawn_job(&st, img.name.clone(), "rollback", move |st, name, steps| {
        let disk_target = img.os == "linux" && img.cache_mode != "zram";
        let busy = || {
            Err::<(), String>(
                "clients are connected; rolling back replaces the shared disk golden they are reading. \
                 Reboot/close the clients (do it off-hours), or set this image to zram cache."
                    .into(),
            )
        };
        // Everything is checked and the version rebuilt into image.img.new BEFORE the served golden or its target is
        // touched: a missing chunk, a full disk or a crash leaves the image exactly as it was.
        if disk_target && crate::publish::image_in_use(st, name) {
            busy()?;
        }
        let data = {
            let _g = st.versions_lock.lock().unwrap_or_else(|p| p.into_inner());
            crate::versions::check(name, &version)?
        };
        let dir = crate::images_dir().join(name);
        crate::publish::need_space(&dir, data, "rolling back")?;
        steps.go(&format!("restore {version}"));
        let (img_path, tmp) = (dir.join("image.img"), dir.join("image.img.new"));
        let _ = std::fs::remove_file(&tmp);
        {
            let _g = st.versions_lock.lock().unwrap_or_else(|p| p.into_inner());
            crate::versions::rehydrate_to(name, &version, &tmp)
        }
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        // A disk-cache target serves image.img: nobody may be attached when it is replaced; the target goes with the
        // old file (a client booting before the republish below stops at its shell instead of reading a mix).
        if disk_target {
            if crate::publish::image_in_use(st, name) {
                let _ = std::fs::remove_file(&tmp);
                busy()?;
            }
            crate::publish::drop_all_targets(st, name);
        }
        std::fs::rename(&tmp, &img_path).map_err(|e| format!("{}: {e}", img_path.display()))?;
        st.db.set_active_version(img.id, Some(&version))?;
        let msg = crate::publish::run_publish(st, name, steps)?;
        Ok(format!("rolled back to {version} — {msg}"))
    })?;
    Ok(Json(serde_json::json!({"ok": true, "async": true})))
}

#[derive(Deserialize)]
struct FromVersionBody {
    id: i64,
    version: String,
    name: String,
}

/// A saved version → a NEW image on the list (same OS + cache mode): its manifest becomes the new image's v1 (chunks
/// shared, nothing copied), restored to its image.img, then published. Background job on the new image.
async fn from_version(State(st): State<SharedState>, Json(b): Json<FromVersionBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let src = st.db.image(b.id).map_err(ise)?.ok_or((StatusCode::NOT_FOUND, format!("image {} not found", b.id)))?;
    if !valid_name(&b.name) {
        return Err((StatusCode::BAD_REQUEST, "name: 1-64 letters/digits/_/-".into()));
    }
    let id = st
        .db
        .add_image(&NewImageRow { name: &b.name, os: &src.os, boot_script: None, cache_mode: &src.cache_mode })
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    std::fs::create_dir_all(crate::images_dir().join(&b.name)).map_err(|e| ise(e.to_string()))?;
    tracing::info!("image {} created from {} {}", b.name, src.name, b.version);
    let version = b.version;
    spawn_job(&st, b.name, "from-version", move |st, name, steps| {
        steps.go(&format!("restore {} {version}", src.name));
        {
            let _g = st.versions_lock.lock().unwrap_or_else(|p| p.into_inner());
            crate::versions::clone_to(&src.name, &version, name, &format!("from {} {version}", src.name))?;
            crate::versions::rehydrate(name, "v1")?;
        }
        // Windows: the boot partitions kept at upload go along (Export needs them).
        let orig = crate::images_dir().join(&src.name).join("orig");
        if orig.exists() {
            let dst = crate::images_dir().join(name).join("orig");
            crate::winstage::run("cp", &["-r", "--sparse=always", &orig.to_string_lossy(), &dst.to_string_lossy()])?;
        }
        st.db.set_active_version(id, Some("v1"))?;
        crate::publish::run_publish(st, name, steps)
    })?;
    Ok(Json(serde_json::json!({"ok": true, "async": true, "id": id})))
}

#[derive(Deserialize)]
struct ExportBody {
    id: i64,
    /// None = the current golden (image.img).
    version: Option<String>,
}

/// Export an image (or one of its versions) as a VMware VM → work/export/<name>/ (export.rs). Background job.
async fn export(State(st): State<SharedState>, Json(b): Json<ExportBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let img = st.db.image(b.id).map_err(ise)?.ok_or((StatusCode::NOT_FOUND, format!("image {} not found", b.id)))?;
    spawn_job(&st, img.name, "export", move |st, name, steps| crate::export::run_export(st, name, &img.os, b.version.as_deref(), steps))?;
    Ok(Json(serde_json::json!({"ok": true, "async": true})))
}

/// GET /api/images/export-file?id=&f=vmx|vmdk — the exported files; Range supported (a 30 GB download can resume).
async fn export_file(State(st): State<SharedState>, Query(q): Query<HashMap<String, String>>, req: axum::extract::Request) -> Result<axum::response::Response, ApiError> {
    let id: i64 = q.get("id").and_then(|s| s.parse().ok()).ok_or((StatusCode::BAD_REQUEST, "missing ?id=".to_string()))?;
    let f = q.get("f").map(String::as_str).filter(|f| matches!(*f, "vmx" | "vmdk")).ok_or((StatusCode::BAD_REQUEST, "f must be vmx or vmdk".to_string()))?;
    let name = name_of(&st, id)?;
    let file = format!("{name}.{f}"); // name is [A-Za-z0-9_-] (valid_name) → safe in the path + header
    let p = crate::export::export_dir(&name).join(&file);
    if !p.exists() {
        return Err((StatusCode::NOT_FOUND, "no export yet — press Export first".into()));
    }
    let mut res = tower_http::services::ServeFile::new(p).try_call(req).await.map_err(|e| ise(e.to_string()))?.map(Body::new);
    if let Ok(v) = header::HeaderValue::from_str(&format!("attachment; filename=\"{file}\"")) {
        res.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(res)
}

/// Versions of an image, newest first. GET /api/images/snapshots?id=<id>
async fn snapshots(State(st): State<SharedState>, Query(q): Query<HashMap<String, String>>) -> Result<Json<serde_json::Value>, ApiError> {
    let id: i64 = q.get("id").and_then(|s| s.parse().ok()).ok_or((StatusCode::BAD_REQUEST, "missing ?id=".to_string()))?;
    let img = st.db.image(id).map_err(ise)?.ok_or((StatusCode::NOT_FOUND, format!("image {id} not found")))?;
    let active = img.active_version.clone();
    let versions = tokio::task::spawn_blocking(move || crate::versions::list_vs_current(&img.name))
        .await
        .map_err(|e| ise(e.to_string()))?;
    Ok(Json(serde_json::json!({"active": active, "versions": versions})))
}

/// Delete a version + free chunks nothing references anymore.
async fn version_delete(State(st): State<SharedState>, Json(b): Json<VersionBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let img = st.db.image(b.id).map_err(ise)?.ok_or((StatusCode::NOT_FOUND, format!("image {} not found", b.id)))?;
    if st.jobs.lock().unwrap().get(&img.name).is_some_and(|s| s.starts_with('⏳')) {
        return Err((StatusCode::CONFLICT, format!("image '{}' has a running job — wait for it to finish", img.name)));
    }
    let (name, version, st2) = (img.name.clone(), b.version.clone(), st.clone());
    let freed = tokio::task::spawn_blocking(move || {
        let _g = st2.versions_lock.lock().unwrap_or_else(|p| p.into_inner());
        crate::versions::delete(&name, &version)
    })
    .await
    .map_err(|e| ise(e.to_string()))?
    .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    if img.active_version.as_deref() == Some(b.version.as_str()) {
        let _ = st.db.set_active_version(img.id, None);
    }
    tracing::info!("image {}: version {} deleted ({freed} chunks freed)", img.name, b.version);
    Ok(Json(serde_json::json!({"ok": true, "freed_chunks": freed})))
}

// Golden upload, chunked: the web sends every file (a VM folder, a .vmdk/.img, or a .zip) in 8 MB
// chunks over several parallel requests → start, chunk × N, done. Files land in <image>/upload/, then
// convert (→ raw image.img) + publish run as a job.
pub(crate) const CHUNK_MAX: usize = 16 << 20;

fn upload_dir(name: &str) -> std::path::PathBuf {
    crate::images_dir().join(name).join("upload")
}

/// Plain file name only (no dirs, no ..): it is joined onto the upload folder.
fn valid_file(f: &str) -> bool {
    !f.is_empty() && f != "." && f != ".." && !f.contains(['/', '\\', '\0'])
}

#[test]
fn upload_file_names() {
    assert!(valid_file("Windows 11 x64-s001.vmdk"));
    for bad in ["", ".", "..", "../x", "a/b", "a\\b", "x\0"] {
        assert!(!valid_file(bad), "{bad:?}");
    }
}

#[derive(Deserialize)]
struct UploadName {
    name: String,
}

/// Check the image can take an upload now: exists (it has the os) + no job running on it.
fn upload_ready(st: &SharedState, name: &str) -> Result<(), ApiError> {
    if !valid_name(name) {
        return Err((StatusCode::BAD_REQUEST, "name: 1-64 letters/digits/_/-".into()));
    }
    if st.db.image_by_name(name).map_err(ise)?.is_none() {
        return Err((StatusCode::BAD_REQUEST, "create the image first (POST /api/images), then upload".into()));
    }
    if st.jobs.lock().unwrap().get(name).is_some_and(|s| s.starts_with('⏳')) {
        return Err((StatusCode::CONFLICT, format!("image '{name}' has a running job — wait for it to finish")));
    }
    Ok(())
}

/// POST /api/images/upload-start {name} — fresh upload folder (drops a half-done earlier upload).
async fn upload_start(State(st): State<SharedState>, Json(b): Json<UploadName>) -> Result<Json<serde_json::Value>, ApiError> {
    upload_ready(&st, &b.name)?;
    let dir = upload_dir(&b.name);
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.map_err(|e| ise(e.to_string()))?;
    tracing::info!("image {}: upload started", b.name);
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct ChunkQuery {
    name: String,
    file: String,
    offset: u64,
    total: u64,
}

/// PUT /api/images/upload-chunk?name=&file=&offset=&total=  body = up to 16 MB of `file` at `offset`.
/// Chunks may arrive in any order, in parallel, or twice (retry): each is written at its own offset.
async fn upload_chunk(State(st): State<SharedState>, Query(q): Query<ChunkQuery>, body: Body) -> Result<Json<serde_json::Value>, ApiError> {
    // Don't accept new chunks for an image whose publish/rollback job is running: the staging upload it feeds
    // would be consumed by the wrong job. Cheap in-memory check (no DB hit per chunk).
    if st.jobs.lock().unwrap().get(&q.name).is_some_and(|s| s.starts_with('⏳')) {
        return Err((StatusCode::CONFLICT, "image has a running job — wait for it to finish".into()));
    }
    if !valid_name(&q.name) {
        return Err((StatusCode::BAD_REQUEST, "bad image name".into()));
    }
    write_chunk(upload_dir(&q.name), &q.file, q.offset, q.total, MAX_UPLOAD, body).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// A single uploaded file (vmdk/img/zip) can't exceed this — matches the golden size cap.
pub(crate) const MAX_UPLOAD: u64 = 4 << 40; // 4 TiB

/// Write one upload chunk into `dir/file` at `offset` (the file is sparse-extended to `total`, capped at `max`).
/// Shared by the golden and driver-package uploads.
pub(crate) async fn write_chunk(dir: std::path::PathBuf, file: &str, offset: u64, total: u64, max: u64, body: Body) -> Result<(), ApiError> {
    if !valid_file(file) {
        return Err((StatusCode::BAD_REQUEST, "bad file name".into()));
    }
    if total > max {
        return Err((StatusCode::BAD_REQUEST, format!("file too large ({total} > {max} bytes)")));
    }
    let data = axum::body::to_bytes(body, CHUNK_MAX).await.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    if offset.checked_add(data.len() as u64).is_none_or(|end| end > total) {
        return Err((StatusCode::BAD_REQUEST, "chunk goes past the end of the file".into()));
    }
    if !dir.is_dir() {
        return Err((StatusCode::BAD_REQUEST, "no upload in progress (upload-start first)".into()));
    }
    let path = dir.join(file);
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        let f = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(&path)?;
        if f.metadata()?.len() < total {
            f.set_len(total)?; // sparse; chunks fill it in any order
        }
        f.write_all_at(&data, offset)
    })
    .await
    .map_err(|e| ise(e.to_string()))?
    .map_err(|e| ise(e.to_string()))
}

/// POST /api/images/upload-done {name} — convert (→ raw) + publish in the BACKGROUND; the web polls /api/images/job.
async fn upload_done(State(st): State<SharedState>, Json(b): Json<UploadName>) -> Result<Json<serde_json::Value>, ApiError> {
    upload_ready(&st, &b.name)?;
    let dir = upload_dir(&b.name);
    let (mut n, mut size) = (0, 0u64);
    let mut rd = tokio::fs::read_dir(&dir).await.map_err(|_| (StatusCode::BAD_REQUEST, "no upload in progress".to_string()))?;
    while let Ok(Some(e)) = rd.next_entry().await {
        n += 1;
        size += e.metadata().await.map(|m| m.len()).unwrap_or(0);
    }
    if n == 0 {
        return Err((StatusCode::BAD_REQUEST, "upload is empty".into()));
    }
    tracing::info!("image {}: upload done ({n} files, {:.1} GB) → converting + publishing", b.name, size as f64 / 1e9);
    spawn_publish(&st, b.name, Some(dir))?;
    Ok(Json(serde_json::json!({"ok": true, "async": true})))
}

/// Run publish in the BACKGROUND, updating the job status. Returns at once.
/// upload = Some(upload folder) when a new golden must be converted to raw image.img first.
fn spawn_publish(st: &SharedState, name: String, upload: Option<std::path::PathBuf>) -> Result<(), ApiError> {
    spawn_job(st, name, "publish", move |st, name, steps| {
        let first = upload.is_some() && crate::versions::list(name, None).is_empty();
        if let Some(dir) = upload {
            // The convert replaces image.img, which a disk-cache target serves: with clients on it the publish below
            // would refuse — after the old golden is already gone. Refuse first (the upload stays for a retry).
            if let Some(img) = st.db.image_by_name(name)?.filter(|i| i.os == "linux" && i.cache_mode != "zram") {
                if crate::publish::image_in_use(st, &img.name) {
                    return Err("clients are connected to this disk-cache image — shut them down (or set it to zram cache), \
                                then press Upload done again"
                        .into());
                }
            }
            steps.go("convert upload→raw");
            crate::golden::prepare_golden(&dir, &crate::images_dir().join(name).join("image.img"))?;
            st.db.set_active_version(name_id(st, name)?, None)?; // new golden = no version yet
        }
        let published = crate::publish::run_publish(st, name, steps);
        if !first {
            return published;
        }
        // First golden of this image → keep it as v1: there is always a version to roll back to — also when the
        // publish failed (a later Publish has no upload left to take it from). A failed snapshot only gets a warning.
        // A hard link to image.img: instant, no copy of the golden.
        steps.go("snapshot v1");
        let first_snap = {
            let _g = st.versions_lock.lock().unwrap_or_else(|p| p.into_inner());
            crate::versions::snapshot(name, "first upload")
        };
        let saved = match first_snap {
            Ok(m) => {
                st.db.set_active_version(name_id(st, name)?, Some(&m.version))?;
                format!("saved as {}", m.version)
            }
            Err(e) => {
                tracing::warn!("image {name}: snapshot v1 failed: {e}");
                format!("⚠ snapshot v1 failed: {e}")
            }
        };
        match published {
            Ok(msg) => Ok(format!("{msg}; {saved}")),
            Err(e) => Err(format!("{e} (upload {saved})")),
        }
    })
}

/// Background job on an image (publish / snapshot / rollback): one at a time per image (overlapping
/// jobs share temp files + mount points → they break each other); status via /api/images/job.
pub(crate) fn spawn_job<F>(st: &SharedState, name: String, what: &'static str, work: F) -> Result<(), ApiError>
where
    F: FnOnce(&SharedState, &str, &mut crate::publish::Steps) -> Result<String, String> + Send + 'static,
{
    {
        let mut jobs = st.jobs.lock().unwrap();
        if jobs.get(&name).is_some_and(|s| s.starts_with('⏳')) {
            return Err((StatusCode::CONFLICT, format!("image '{name}' already has a running job — wait for it to finish")));
        }
        jobs.insert(name.clone(), "⏳ starting...".into()); // check + claim under one lock
    }
    let _ = st.job_tx.send((name.clone(), "⏳ starting...".into()));
    let st_bg = st.clone();
    tokio::spawn(async move {
        let (st_run, name_run) = (st_bg.clone(), name.clone());
        let res = tokio::task::spawn_blocking(move || {
            let mut steps = crate::publish::Steps::new(&st_run, &name_run);
            let msg = work(&st_run, &name_run, &mut steps)?;
            Ok::<_, String>(format!("{msg} (⏱ {})", steps.summary()))
        })
        .await;
        let msg = match res {
            Ok(Ok(m)) => {
                tracing::info!("{what} {name}: done — {m}");
                format!("✓ {m}")
            }
            Ok(Err(e)) => {
                tracing::error!("{what} {name} failed: {e}");
                format!("✗ {e}")
            }
            Err(e) => {
                tracing::error!("{what} {name}: task crashed: {e}");
                format!("✗ task failed: {e}")
            }
        };
        st_bg.set_job(&name, msg);
    });
    Ok(())
}

fn name_id(st: &SharedState, name: &str) -> Result<i64, String> {
    Ok(st.db.image_by_name(name)?.ok_or(format!("image {name} not found"))?.id)
}

/// Publish job status of an image. GET /api/images/job?name=<name>
/// status: "" (none) | "⏳ ..." running | "✓ ..." done | "✗ ..." failed.
async fn job_status(
    State(st): State<SharedState>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let name = q.get("name").cloned().unwrap_or_default();
    let s = st.jobs.lock().unwrap().get(&name).cloned().unwrap_or_default();
    Json(serde_json::json!({"status": s}))
}

#[derive(Deserialize)]
struct CacheModeBody {
    id: i64,
    mode: String,
}

/// Set the cache_mode (disk|zram) of an image then republish (changes backing + iSCSI target).
/// POST /api/images/cache-mode {id, mode}
async fn set_cache_mode(
    State(st): State<SharedState>,
    Json(b): Json<CacheModeBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if b.mode != "disk" && b.mode != "zram" {
        return Err((StatusCode::BAD_REQUEST, "mode must be disk|zram".into()));
    }
    let name = name_of(&st, b.id)?;
    // Changed INSIDE the job, then republished in the background so backing/target follow it (zram dd of the img can
    // take long): with another job running this is refused (409) and the mode stays as it was — never a new mode in
    // the DB without the publish that applies it. publish_iscsi falls back zram→disk by itself on RAM overflow.
    let (id, mode) = (b.id, b.mode);
    spawn_job(&st, name, "publish", move |st, name, steps| {
        let before = st.db.image(id)?.map(|i| i.cache_mode);
        st.db.set_cache_mode(id, &mode)?;
        tracing::info!("image {name}: cache mode → {mode}");
        // The publish refused or failed → the mode in the DB goes back to what is still being served.
        crate::publish::run_publish(st, name, steps).inspect_err(|_| {
            if let Some(b) = &before {
                let _ = st.db.set_cache_mode(id, b);
            }
        })
    })?;
    Ok(Json(serde_json::json!({"ok": true, "async": true})))
}

/// Linux golden prep script (packages for the iSCSI root + SSD overlay), run INSIDE the golden VM. GET /broom-prep
async fn broom_prep(State(st): State<SharedState>) -> impl IntoResponse {
    let ip = st.db.get_config("dhcp_server_ip", "10.0.0.12");
    (
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        crate::overlay::prep_script().replace("__IP__", &ip),
    )
}

#[derive(Deserialize)]
struct BaseModeBody {
    id: i64,
    on: bool,
}

/// POST /api/images/base-mode {id, on} — Windows: BASE MODE on the first logon of each machine (a technician sets up
/// apps, then restarts) instead of committing base at once. Read by /boot/start → no republish needed.
async fn set_base_mode(State(st): State<SharedState>, Json(b): Json<BaseModeBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let name = name_of(&st, b.id)?;
    st.db.set_base_mode(b.id, b.on).map_err(ise)?;
    tracing::info!("image {name}: base mode {}", if b.on { "ON (first logon waits for a technician)" } else { "off" });
    Ok(Json(serde_json::json!({"ok": true})))
}

/// GET /api/cache-list (public: Windows stage + Linux cache script, every boot) → "name hash" per image the machine at
/// the peer IP may keep on its SSD: published, using the SSD (Windows always), and for its group. A cached image not
/// listed, or with another hash (deleted, SSD switched off, republished, no longer for its group), is removed on the
/// machine. Then "preload name hash os" per such image set to preload: the machine fetches it ahead of use.
async fn cache_list(State(st): State<SharedState>, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>) -> Result<String, ApiError> {
    let grp = crate::machines::machine_at(&st, &peer.ip().to_canonical().to_string()).and_then(|m| m.grp);
    Ok(cache_lines(&st.db.images().map_err(ise)?, grp.as_deref()))
}

fn cache_lines(images: &[crate::db::Image], grp: Option<&str>) -> String {
    let mine: Vec<(&crate::db::Image, &str)> = images
        .iter()
        .filter(|i| (i.use_ssd || i.os == "windows") && crate::machines::for_group(&i.groups, grp))
        .filter_map(|i| i.hash.as_deref().map(|h| (i, h)))
        .collect();
    let keep = mine.iter().map(|(i, h)| format!("{} {h}\n", i.name));
    let preload = mine.iter().filter(|(i, _)| i.preload).map(|(i, h)| format!("preload {} {h} {}\n", i.name, i.os));
    keep.chain(preload).collect()
}

#[derive(Deserialize)]
struct GroupsBody {
    id: i64,
    groups: Vec<String>,
}

/// POST /api/images/groups {id, groups} — the machine groups that get it in their boot menu (empty = every machine).
/// Read at each boot → no republish needed.
async fn set_groups(State(st): State<SharedState>, Json(b): Json<GroupsBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let name = name_of(&st, b.id)?;
    let groups = crate::machines::clean_groups(&b.groups).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    st.db.set_image_groups(b.id, &groups).map_err(ise)?;
    tracing::info!("image {name}: for {}", if groups.is_empty() { "every machine".into() } else { format!("groups {}", groups.join(", ")) });
    Ok(Json(serde_json::json!({"ok": true})))
}

/// POST /api/images/preload {id, on} — Windows: machines that may boot it fetch it onto their SSD ahead of use (the
/// stage, while another image boots), so choosing it later needs no download. Linux needs none: it boots over the
/// network at once and copies itself onto the SSD in the background.
async fn set_preload(State(st): State<SharedState>, Json(b): Json<BaseModeBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let img = st.db.image(b.id).map_err(ise)?.ok_or((StatusCode::NOT_FOUND, format!("image {} not found", b.id)))?;
    if img.os != "windows" {
        return Err((StatusCode::BAD_REQUEST, "preload is for Windows images (Linux boots over the network at once)".into()));
    }
    let name = img.name;
    st.db.set_preload(b.id, b.on).map_err(ise)?;
    tracing::info!("image {name}: preload {}", if b.on { "on" } else { "off" });
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct SsdBody {
    id: i64,
    on: bool,
}

/// POST /api/images/ssd {id, on} — Linux: cache the golden + keep the session's writes on the machine's SSD (on), or
/// leave the SSD untouched (off: golden over the network, writes in RAM). Windows boots from a VHDX on the SSD → always
/// on. Read by /boot/start → no republish needed.
async fn set_use_ssd(State(st): State<SharedState>, Json(b): Json<SsdBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let img = st.db.image(b.id).map_err(ise)?.ok_or((StatusCode::NOT_FOUND, format!("image {} not found", b.id)))?;
    if img.os == "windows" && !b.on {
        return Err((StatusCode::BAD_REQUEST, "Windows images always use the SSD (they boot from a VHDX on it)".into()));
    }
    // A boot script from before the switch existed doesn't pass it to the client, which would keep using the SSD.
    if img.boot_script.as_deref().is_some_and(|s| !s.contains("broom.ssd=")) {
        return Err((StatusCode::BAD_REQUEST, format!("publish image {} again first (its boot script predates the SSD switch)", img.name)));
    }
    st.db.set_use_ssd(b.id, b.on).map_err(ise)?;
    tracing::info!("image {}: SSD {}", img.name, if b.on { "on (cache + writes on the SSD)" } else { "off (SSD untouched, RAM only)" });
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Script that prepares a Windows golden (tweaks + EFI + unattend + sysprep), run INSIDE the Windows VM.
/// GET /broom-prep-win  →  irm http://<server>/broom-prep-win | iex. Open on purpose (same command every time): it
/// carries the guest user + password, readable by anyone on the boot LAN — like the golden itself.
async fn broom_prep_win(State(st): State<SharedState>) -> impl IntoResponse {
    tracing::info!("broom-prep-win script handed out");
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], crate::winstage::prep_script(&*st.db))
}

#[cfg(test)]
mod tests {
    #[test]
    fn cache_list_lines() {
        let img = |name: &str, os: &str, hash: Option<&str>, use_ssd: bool| crate::db::Image {
            id: 0, name: name.into(), os: os.into(), active_version: None, is_default: false, boot_script: None,
            hash: hash.map(Into::into), cache_mode: "disk".into(), base_mode: false, use_ssd, groups: Vec::new(),
            preload: false,
        };
        let mut list = vec![
            img("win11", "windows", Some("aa"), false), // Windows always uses the SSD
            img("ubuntu", "linux", Some("bb"), true),
            img("kiosk", "linux", Some("cc"), false), // SSD off → not cached
            img("draft", "linux", None, true),        // not published
        ];
        assert_eq!(super::cache_lines(&list, None), "win11 aa\nubuntu bb\n");
        // Another group's image is not kept; a preloaded one is also listed to fetch ahead (after every keep line).
        list.push(crate::db::Image { groups: vec!["VIP".into()], preload: true, ..img("stream", "windows", Some("dd"), true) });
        list[1].preload = true;
        assert_eq!(super::cache_lines(&list, Some("Thuong")), "win11 aa\nubuntu bb\npreload ubuntu bb linux\n");
        assert_eq!(super::cache_lines(&list, Some("vip")), "win11 aa\nubuntu bb\nstream dd\npreload ubuntu bb linux\npreload stream dd windows\n");
    }
}
