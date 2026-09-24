// images.rs — M6 mgmt-image. CRUD + version (ZFS snapshot) + rollback + set default.
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post, put},
    Json, Router,
};
use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::io::AsyncWriteExt;

use crate::{zfs, SharedState};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/images", get(list).post(create))
        .route("/api/images/default", post(set_default))
        .route("/api/images/delete", post(delete))
        .route("/api/images/publish", post(publish_now))
        .route("/api/images/boot-script", post(set_boot_script))
        .route("/api/images/cache-mode", post(set_cache_mode))
        .route("/api/images/job", get(job_status))
        .route("/broom-prep", get(broom_prep))
        .route("/broom-prep-win", get(broom_prep_win))
        .route(
            "/api/images/upload",
            put(upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/api/images/snapshot", post(snapshot))
        .route("/api/images/snapshots", get(snapshots))
        .route("/api/images/rollback", post(rollback))
}

/// List the versions (ZFS snapshots) of an image. GET /api/images/snapshots?id=<id>
async fn snapshots(
    State(st): State<SharedState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    let id: i64 = q
        .get("id")
        .and_then(|s| s.parse().ok())
        .ok_or((StatusCode::BAD_REQUEST, "missing ?id=".to_string()))?;
    let dataset = dataset_of(&st, id)?;
    let snaps = zfs::list_snapshots(&dataset)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(snaps))
}

#[derive(Serialize)]
struct Image {
    id: i64,
    name: String,
    os: String,
    dataset: Option<String>,
    is_default: bool,
    boot_script: Option<String>,
    hash: Option<String>,
    cache_mode: String,
}

async fn list(State(st): State<SharedState>) -> Json<Vec<Image>> {
    let conn = st.db.lock().unwrap();
    let mut stmt = conn
        .prepare("SELECT id,name,os,dataset,is_default,boot_script,hash,cache_mode FROM images ORDER BY id")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok(Image {
                id: r.get(0)?,
                name: r.get(1)?,
                os: r.get(2)?,
                dataset: r.get(3)?,
                is_default: r.get::<_, i64>(4)? == 1,
                boot_script: r.get(5)?,
                hash: r.get(6)?,
                cache_mode: r.get(7)?,
            })
        })
        .unwrap();
    Json(rows.filter_map(|r| r.ok()).collect())
}

#[derive(Deserialize)]
struct NewImage {
    name: String,
    os: String,
    dataset: Option<String>,
    boot_script: Option<String>,
    cache_mode: Option<String>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

async fn create(
    State(st): State<SharedState>,
    Json(b): Json<NewImage>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !valid_name(&b.name) {
        return Err((StatusCode::BAD_REQUEST, "name may only contain letters/digits/_/-".into()));
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
    let conn = st.db.lock().unwrap();
    conn.execute(
        "INSERT INTO images(name,os,dataset,boot_script,cache_mode) VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![b.name, b.os, b.dataset, b.boot_script, cache_mode],
    )
    .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true, "id": conn.last_insert_rowid()})))
}

/// Delete an image: DB row + folder images/<name>/. (Published LTSP files are cleaned by hand — ponytail.)
async fn delete(
    State(st): State<SharedState>,
    Json(b): Json<IdBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let name: String = {
        let conn = st.db.lock().unwrap();
        let name = conn
            .query_row("SELECT name FROM images WHERE id=?1", [b.id], |r| {
                r.get::<_, String>(0)
            })
            .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
        conn.execute("DELETE FROM images WHERE id=?1", [b.id])
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        name
    };
    if valid_name(&name) {
        let _ = std::fs::remove_dir_all(crate::images_dir().join(&name));
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Publish an image again (runs publish::run_publish). Used after changing a Linux golden.
async fn publish_now(
    State(st): State<SharedState>,
    Json(b): Json<IdBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let name: String = {
        let conn = st.db.lock().unwrap();
        conn.query_row("SELECT name FROM images WHERE id=?1", [b.id], |r| r.get(0))
            .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?
    };
    spawn_publish(&st, name, None)?;
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
    let conn = st.db.lock().unwrap();
    conn.execute(
        "UPDATE images SET boot_script=?1 WHERE id=?2",
        rusqlite::params![b.boot_script, b.id],
    )
    .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
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
    let conn = st.db.lock().unwrap();
    conn.execute("UPDATE images SET is_default=0", [])
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    conn.execute("UPDATE images SET is_default=1 WHERE id=?1", [b.id])
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct SnapBody {
    id: i64,
    snap: String,
}

/// Create a version = ZFS snapshot of the image's dataset.
async fn snapshot(
    State(st): State<SharedState>,
    Json(b): Json<SnapBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let dataset = dataset_of(&st, b.id)?;
    let ok = zfs::snapshot(&dataset, &b.snap)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": ok, "snap": format!("{dataset}@{}", b.snap)})))
}

/// Roll an image back to a snapshot then publish again (clients get the old golden via the new hash).
async fn rollback(
    State(st): State<SharedState>,
    Json(b): Json<SnapBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let dataset = dataset_of(&st, b.id)?;
    let ok = zfs::rollback(&dataset, &b.snap)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !ok {
        return Ok(Json(serde_json::json!({"ok": false})));
    }
    let name: String = {
        let conn = st.db.lock().unwrap();
        conn.query_row("SELECT name FROM images WHERE id=?1", [b.id], |r| r.get(0))
            .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?
    };
    // Rollback restores image.img with its OLD mtime → Windows publish thinks golden.vhdx is still fresh (golden_fresh)
    // and keeps it. Mark image.img as new so publish rebuilds the golden from the rolled-back copy.
    let img = crate::images_dir().join(&name).join("image.img");
    let _ = std::process::Command::new("touch").arg(&img).status();
    spawn_publish(&st, name, None)?;
    Ok(Json(serde_json::json!({"ok": true, "async": true})))
}

/// Upload a golden → <images_dir>/<name>/image.img (streamed, doesn't eat RAM), convert if
/// needed (vmdk/zip → raw), then publish BY ITSELF (iSCSI + overlay boot_script).
/// PUT /api/images/upload?name=<name>&src=raw|vmdk|zip  body = bytes file
async fn upload(
    State(st): State<SharedState>,
    Query(q): Query<HashMap<String, String>>,
    body: Body,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let name = q.get("name").cloned().unwrap_or_default();
    if !valid_name(&name) {
        return Err((StatusCode::BAD_REQUEST, "name may only contain letters/digits/_/-".into()));
    }
    // src: raw (default, the body is image.img) | vmdk | zip (containing vmdk/img).
    let src = q.get("src").cloned().unwrap_or_else(|| "raw".into());
    if !["raw", "vmdk", "zip"].contains(&src.as_str()) {
        return Err((StatusCode::BAD_REQUEST, "src must be raw|vmdk|zip".into()));
    }
    // The image must be created first (it has the os).
    let exists = {
        let c = st.db.lock().unwrap();
        c.query_row("SELECT 1 FROM images WHERE name=?1", [&name], |_| Ok(()))
            .is_ok()
    };
    if !exists {
        return Err((StatusCode::BAD_REQUEST, "create the image first (POST /api/images), then upload".into()));
    }

    let dir = crate::images_dir().join(&name);
    tokio::fs::create_dir_all(&dir).await.ok();
    // Download target: raw → written straight to image.img.uploading; vmdk/zip → kept as is for conversion.
    let upload_name = match src.as_str() {
        "vmdk" => "upload.vmdk",
        "zip" => "upload.zip",
        _ => "image.img.uploading",
    };
    let uploaded = dir.join(upload_name);

    let ise = |e: std::io::Error| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let mut file = tokio::fs::File::create(&uploaded).await.map_err(ise)?;
    let mut body = body;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        if let Ok(data) = frame.into_data() {
            file.write_all(&data).await.map_err(ise)?;
        }
    }
    file.flush().await.map_err(ise)?;
    drop(file);

    // Convert (vmdk/zip → raw) + publish run in the BACKGROUND → return at once; the web polls /api/images/job.
    spawn_publish(&st, name.clone(), Some((src, uploaded, dir.join("image.img"))))?;
    Ok(Json(serde_json::json!({"ok": true, "uploaded": true, "async": true})))
}

/// Run publish (plus convert if needed) in the BACKGROUND, updating the job status. Returns at once.
/// convert = Some((src, uploaded, dest)) when vmdk/zip must be converted to raw first.
fn spawn_publish(
    st: &SharedState,
    name: String,
    convert: Option<(String, std::path::PathBuf, std::path::PathBuf)>,
) -> Result<(), (StatusCode, String)> {
    // One job per image: 2 overlapping jobs share temp files + mount points → they break each other.
    {
        let mut jobs = st.jobs.lock().unwrap();
        if jobs.get(&name).map_or(false, |s| s.starts_with('⏳')) {
            return Err((StatusCode::CONFLICT, format!("image '{name}' already has a running job — wait for it to finish")));
        }
        jobs.insert(name.clone(), "⏳ starting...".into());
    }
    let st_bg = st.clone();
    tokio::spawn(async move {
        let st_run = st_bg.clone();
        let name_run = name.clone();
        let res = tokio::task::spawn_blocking(move || {
            let mut steps = crate::publish::Steps::new(&st_run, &name_run);
            if let Some((src, uploaded, dest)) = convert {
                steps.go(&format!("convert {src}→raw"));
                crate::publish::prepare_golden(&src, &uploaded, &dest)?;
            }
            let msg = crate::publish::run_publish(&st_run, &name_run, &mut steps)?;
            Ok::<_, String>(format!("{msg} (⏱ {})", steps.summary()))
        })
        .await;
        let msg = match res {
            Ok(Ok(m)) => format!("✓ {m}"),
            Ok(Err(e)) => format!("✗ {e}"),
            Err(e) => format!("✗ task failed: {e}"),
        };
        st_bg.jobs.lock().unwrap().insert(name, msg);
    });
    Ok(())
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
    let name: String = {
        let conn = st.db.lock().unwrap();
        conn.execute("UPDATE images SET cache_mode=?1 WHERE id=?2", rusqlite::params![b.mode, b.id])
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        conn.query_row("SELECT name FROM images WHERE id=?1", [b.id], |r| r.get(0))
            .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?
    };
    // Republish in the BACKGROUND so backing/target follow the new cache_mode (zram dd of the img can take long).
    // publish_iscsi falls back zram→disk by itself (DB updated) on RAM overflow → the image doesn't get stuck.
    spawn_publish(&st, name, None)?;
    Ok(Json(serde_json::json!({"ok": true, "async": true})))
}

/// Script that bakes the overlay hook, run INSIDE the golden VM. GET /broom-prep
async fn broom_prep(State(st): State<SharedState>) -> impl IntoResponse {
    let ip = {
        let conn = st.db.lock().unwrap();
        crate::db::get_config(&conn, "dhcp_server_ip", "10.0.0.12")
    };
    (
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        crate::overlay::PREP_SCRIPT.replace("__IP__", &ip),
    )
}

/// Script that prepares a Windows golden (tweaks + EFI + unattend + sysprep), run INSIDE the Windows VM.
/// GET /broom-prep-win  →  irm http://<server>/broom-prep-win | iex
async fn broom_prep_win(State(st): State<SharedState>) -> impl IntoResponse {
    let s = {
        let conn = st.db.lock().unwrap();
        crate::winstage::prep_script(&conn)
    };
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], s)
}

fn dataset_of(st: &SharedState, id: i64) -> Result<String, (StatusCode, String)> {
    let conn = st.db.lock().unwrap();
    conn.query_row("SELECT dataset FROM images WHERE id=?1", [id], |r| {
        r.get::<_, Option<String>>(0)
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?
    .ok_or((StatusCode::BAD_REQUEST, "image has no ZFS dataset assigned".into()))
}
