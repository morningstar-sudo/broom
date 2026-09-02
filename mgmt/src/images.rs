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
        .route("/api/images/hash", get(get_hash))
        .route(
            "/api/images/upload",
            put(upload).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/images/upload-ltsp",
            put(upload_ltsp).layer(DefaultBodyLimit::disable()),
        )
        .route("/ltsp-script", get(ltsp_script))
        .route("/api/images/snapshot", post(snapshot))
        .route("/api/images/snapshots", get(snapshots))
        .route("/api/images/rollback", post(rollback))
}

/// Liệt kê version (ZFS snapshot) của 1 image. GET /api/images/snapshots?id=<id>
async fn snapshots(
    State(st): State<SharedState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    let id: i64 = q
        .get("id")
        .and_then(|s| s.parse().ok())
        .ok_or((StatusCode::BAD_REQUEST, "thiếu ?id=".to_string()))?;
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
        return Err((StatusCode::BAD_REQUEST, "name chỉ gồm chữ/số/_/-".into()));
    }
    if b.os != "linux" {
        return Err((StatusCode::BAD_REQUEST, "os phải 'linux'".into()));
    }
    // Mỗi image 1 folder dưới level binary.
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

/// Xoá image: bản ghi DB + folder images/<name>/. (File LTSP đã publish dọn tay — ponytail.)
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

/// Publish lại 1 image (chạy publish::run_publish). Dùng sau khi sửa golden Linux.
async fn publish_now(
    State(st): State<SharedState>,
    Json(b): Json<IdBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let name: String = {
        let conn = st.db.lock().unwrap();
        conn.query_row("SELECT name FROM images WHERE id=?1", [b.id], |r| r.get(0))
            .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?
    };
    spawn_publish(&st, name, None);
    Ok(Json(serde_json::json!({"ok": true, "async": true})))
}

#[derive(Deserialize)]
struct BootScriptBody {
    id: i64,
    boot_script: String,
}

/// Đặt đoạn iPXE boot cho 1 image (kernel/initrd Linux).
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

/// Tạo version = ZFS snapshot dataset của image.
async fn snapshot(
    State(st): State<SharedState>,
    Json(b): Json<SnapBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let dataset = dataset_of(&st, b.id)?;
    let ok = zfs::snapshot(&dataset, &b.snap)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": ok, "snap": format!("{dataset}@{}", b.snap)})))
}

/// Rollback image về 1 snapshot.
async fn rollback(
    State(st): State<SharedState>,
    Json(b): Json<SnapBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let dataset = dataset_of(&st, b.id)?;
    let ok = zfs::rollback(&dataset, &b.snap)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": ok})))
}

/// Upload golden → <images_dir>/<name>/image.img (stream, không nuốt RAM), convert nếu
/// cần (vmdk/zip → raw), rồi TỰ publish (iSCSI + boot_script overlay).
/// PUT /api/images/upload?name=<name>&src=raw|vmdk|zip  body = bytes file
async fn upload(
    State(st): State<SharedState>,
    Query(q): Query<HashMap<String, String>>,
    body: Body,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let name = q.get("name").cloned().unwrap_or_default();
    if !valid_name(&name) {
        return Err((StatusCode::BAD_REQUEST, "name chỉ gồm chữ/số/_/-".into()));
    }
    // src: raw (mặc định, body chính là image.img) | vmdk | zip (chứa vmdk/img).
    let src = q.get("src").cloned().unwrap_or_else(|| "raw".into());
    if !["raw", "vmdk", "zip"].contains(&src.as_str()) {
        return Err((StatusCode::BAD_REQUEST, "src phải raw|vmdk|zip".into()));
    }
    // Image phải được tạo trước (có os).
    let exists = {
        let c = st.db.lock().unwrap();
        c.query_row("SELECT 1 FROM images WHERE name=?1", [&name], |_| Ok(()))
            .is_ok()
    };
    if !exists {
        return Err((StatusCode::BAD_REQUEST, "tạo image trước (POST /api/images) rồi mới upload".into()));
    }

    let dir = crate::images_dir().join(&name);
    tokio::fs::create_dir_all(&dir).await.ok();
    // Nguồn tải về: raw → ghi thẳng image.img.uploading; vmdk/zip → giữ nguyên để convert.
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

    // Convert (vmdk/zip → raw) + publish chạy NỀN → trả ngay; web poll /api/images/job.
    spawn_publish(&st, name.clone(), Some((src, uploaded, dir.join("image.img"))));
    Ok(Json(serde_json::json!({"ok": true, "uploaded": true, "async": true})))
}

/// Đặt job status cho 1 image.
fn set_job(st: &SharedState, name: &str, msg: String) {
    st.jobs.lock().unwrap().insert(name.to_string(), msg);
}

/// Chạy publish (kèm convert nếu có) ở NỀN, cập nhật job status. Trả ngay.
/// convert = Some((src, uploaded, dest)) khi cần convert vmdk/zip → raw trước.
fn spawn_publish(
    st: &SharedState,
    name: String,
    convert: Option<(String, std::path::PathBuf, std::path::PathBuf)>,
) {
    set_job(st, &name, "⏳ đang xử lý (convert + build initrd + iSCSI)...".into());
    let st_bg = st.clone();
    tokio::spawn(async move {
        let st_run = st_bg.clone();
        let name_run = name.clone();
        let res = tokio::task::spawn_blocking(move || {
            if let Some((src, uploaded, dest)) = convert {
                crate::publish::prepare_golden(&src, &uploaded, &dest)?;
            }
            crate::publish::run_publish(&st_run, &name_run)
        })
        .await;
        let msg = match res {
            Ok(Ok(m)) => format!("✓ {m}"),
            Ok(Err(e)) => format!("✗ {e}"),
            Err(e) => format!("✗ task lỗi: {e}"),
        };
        st_bg.jobs.lock().unwrap().insert(name, msg);
    });
}

/// Trạng thái job publish của 1 image. GET /api/images/job?name=<name>
/// status: "" (chưa có) | "⏳ ..." đang chạy | "✓ ..." xong | "✗ ..." lỗi.
async fn job_status(
    State(st): State<SharedState>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let name = q.get("name").cloned().unwrap_or_default();
    let s = st.jobs.lock().unwrap().get(&name).cloned().unwrap_or_default();
    Json(serde_json::json!({"status": s}))
}

/// Script chạy trên VM desktop để đóng golden + đẩy về server (dùng __IP__ thay IP thật).
const LTSP_SCRIPT: &str = r#"#!/usr/bin/env bash
# Chạy TRÊN VM Ubuntu Desktop (golden): đóng golden thành 1 file golden.zip.
# Dùng: curl -fsSL http://__IP__/ltsp-script | sudo bash
# Sau đó UPLOAD golden.zip qua web admin (server tự giải nén + publish).
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "Chạy bằng sudo"; exit 1; }
apt-get update -y && apt-get install -y ltsp zip
KV=$(ls -1 /boot/vmlinuz-* | sort -V | tail -1 | xargs -n1 basename | sed 's/vmlinuz-//')
ln -sf boot/vmlinuz-$KV /vmlinuz
ln -sf boot/initrd.img-$KV /initrd.img
ltsp image /
ltsp kernel /srv/ltsp/images/x86_64.img
cd /tmp && rm -f golden.zip
zip -j golden.zip /srv/ltsp/images/x86_64.img /srv/tftp/ltsp/x86_64/vmlinuz /srv/tftp/ltsp/x86_64/initrd.img
echo
echo "==================================================================="
echo " XONG. File: /tmp/golden.zip ($(du -h /tmp/golden.zip | cut -f1))"
echo " -> Copy file ve may ban, roi UPLOAD qua web admin:"
echo "      http://__IP__/   (muc 'Golden Linux (.zip)')"
echo "==================================================================="
"#;

/// Hash sha256 của image (text thuần) — cho initrd hook cache SSD so version.
/// GET /api/images/hash?name=<name>
async fn get_hash(
    State(st): State<SharedState>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let name = q.get("name").cloned().unwrap_or_default();
    let h: String = {
        let conn = st.db.lock().unwrap();
        conn.query_row("SELECT hash FROM images WHERE name=?1", [&name], |r| {
            r.get::<_, Option<String>>(0)
        })
        .ok()
        .flatten()
        .unwrap_or_default()
    };
    ([(header::CONTENT_TYPE, "text/plain")], h)
}

async fn ltsp_script(State(st): State<SharedState>) -> impl IntoResponse {
    let ip = {
        let conn = st.db.lock().unwrap();
        crate::db::get_config(&conn, "dhcp_server_ip", "10.0.0.12")
    };
    (
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        LTSP_SCRIPT.replace("__IP__", &ip),
    )
}

/// Nhận bundle zip golden Linux từ VM desktop, giải nén + đặt chỗ + publish (blocking).
/// PUT /api/images/upload-ltsp?name=<name>  body = golden.zip
async fn upload_ltsp(
    State(st): State<SharedState>,
    Query(q): Query<HashMap<String, String>>,
    body: Body,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let name = q.get("name").cloned().unwrap_or_default();
    if !valid_name(&name) {
        return Err((StatusCode::BAD_REQUEST, "name chỉ gồm chữ/số/_/-".into()));
    }
    // Đăng ký image linux nếu chưa có.
    {
        let conn = st.db.lock().unwrap();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO images(name,os) VALUES(?1,'linux')",
            [&name],
        );
    }
    std::fs::create_dir_all(crate::images_dir().join(&name)).ok();

    // Stream zip xuống /tmp (không nuốt RAM).
    let tmp = format!("/tmp/bootrom-{name}.zip");
    let ise = |e: std::io::Error| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let mut file = tokio::fs::File::create(&tmp).await.map_err(ise)?;
    let mut body = body;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        if let Ok(data) = frame.into_data() {
            file.write_all(&data).await.map_err(ise)?;
        }
    }
    file.flush().await.map_err(ise)?;
    drop(file);

    // Giải nén + đặt chỗ + ltsp initrd/nfs + boot_script (blocking).
    let st2 = st.clone();
    let name2 = name.clone();
    let tmp2 = tmp.clone();
    let res = tokio::task::spawn_blocking(move || {
        crate::publish::install_ltsp_bundle(&st2, &name2, &tmp2)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = std::fs::remove_file(&tmp);

    match res {
        Ok(msg) => Ok(Json(serde_json::json!({"ok": true, "result": msg}))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

#[derive(Deserialize)]
struct CacheModeBody {
    id: i64,
    mode: String,
}

/// Đặt cache_mode (disk|zram) cho 1 image rồi republish (đổi backing + iSCSI target).
/// POST /api/images/cache-mode {id, mode}
async fn set_cache_mode(
    State(st): State<SharedState>,
    Json(b): Json<CacheModeBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if b.mode != "disk" && b.mode != "zram" {
        return Err((StatusCode::BAD_REQUEST, "mode phải disk|zram".into()));
    }
    let name: String = {
        let conn = st.db.lock().unwrap();
        conn.execute("UPDATE images SET cache_mode=?1 WHERE id=?2", rusqlite::params![b.mode, b.id])
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        conn.query_row("SELECT name FROM images WHERE id=?1", [b.id], |r| r.get(0))
            .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?
    };
    // Republish NỀN để backing/target theo cache_mode mới (zram dd img có thể lâu).
    // publish_iscsi tự hạ zram→disk (cập nhật DB) nếu tràn RAM → image không kẹt.
    spawn_publish(&st, name, None);
    Ok(Json(serde_json::json!({"ok": true, "async": true})))
}

/// Script bake overlay hook, chạy TRONG golden VM. GET /broom-prep
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

fn dataset_of(st: &SharedState, id: i64) -> Result<String, (StatusCode, String)> {
    let conn = st.db.lock().unwrap();
    conn.query_row("SELECT dataset FROM images WHERE id=?1", [id], |r| {
        r.get::<_, Option<String>>(0)
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?
    .ok_or((StatusCode::BAD_REQUEST, "image chưa gán dataset ZFS".into()))
}
