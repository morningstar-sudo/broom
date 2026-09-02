// machines.rs — M8 mgmt-config. Bảng máy (MAC/IP/hostname), gán image,
// set default image + countdown timeout, chọn DHCP mode → sinh bindings.conf + reload dnsmasq.
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{db, dnsmasq, SharedState};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/machines", get(list).post(add))
        .route("/api/machines/assign", post(assign))
        .route("/api/config/timeout", post(set_timeout))
        .route("/api/config/zram-reserve", post(set_zram_reserve))
        .route("/api/dhcp", get(get_dhcp).post(set_dhcp))
        .route("/api/dhcp/apply", post(apply_dhcp))
        .route("/api/ltsp-config", get(get_ltsp).post(set_ltsp))
}

/// Config café user (Linux/LTSP): user, password, SSD.
async fn get_ltsp(State(st): State<SharedState>) -> Json<serde_json::Value> {
    let conn = st.db.lock().unwrap();
    let g = |k: &str, d: &str| db::get_config(&conn, k, d);
    Json(serde_json::json!({
        "user": g("ltsp_user", "khach"),
        "password": g("ltsp_password", "123456"),
        "ssd_dev": g("ltsp_ssd_dev", "auto"),
        "ssd_mount": g("ltsp_ssd_mount", "/games"),
        "image_cache": g("ltsp_image_cache", "off"),
        "user_sudo": g("ltsp_user_sudo", "1"),
    }))
}

#[derive(Deserialize)]
struct LtspBody {
    user: Option<String>,
    password: Option<String>,
    ssd_dev: Option<String>,
    ssd_mount: Option<String>,
    image_cache: Option<String>,
    user_sudo: Option<String>,
}

/// Đặt café user + sinh lại ltsp.conf + `ltsp initrd` (áp cho mọi client).
async fn set_ltsp(
    State(st): State<SharedState>,
    Json(b): Json<LtspBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    {
        let conn = st.db.lock().unwrap();
        let put = |k: &str, v: &Option<String>| {
            if let Some(val) = v {
                db::set_config(&conn, k, val).ok();
            }
        };
        put("ltsp_user", &b.user);
        put("ltsp_password", &b.password);
        put("ltsp_ssd_dev", &b.ssd_dev);
        put("ltsp_ssd_mount", &b.ssd_mount);
        put("ltsp_image_cache", &b.image_cache);
        put("ltsp_user_sudo", &b.user_sudo);
    }
    // write_conf + ltsp initrd (blocking) → tách thread.
    let st2 = st.clone();
    let res = tokio::task::spawn_blocking(move || {
        let conn = st2.db.lock().unwrap();
        crate::ltsp::apply(&conn)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match res {
        Ok(()) => Ok(Json(serde_json::json!({"ok": true}))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

#[derive(Serialize)]
struct Machine {
    id: i64,
    mac: String,
    ip: Option<String>,
    hostname: Option<String>,
    image_id: Option<i64>,
}

async fn list(State(st): State<SharedState>) -> Json<Vec<Machine>> {
    let conn = st.db.lock().unwrap();
    let mut stmt = conn
        .prepare("SELECT id,mac,ip,hostname,image_id FROM machines ORDER BY hostname")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok(Machine {
                id: r.get(0)?,
                mac: r.get(1)?,
                ip: r.get(2)?,
                hostname: r.get(3)?,
                image_id: r.get(4)?,
            })
        })
        .unwrap();
    Json(rows.filter_map(|r| r.ok()).collect())
}

#[derive(Deserialize)]
struct NewMachine {
    mac: String,
    ip: Option<String>,
    hostname: Option<String>,
}

async fn add(
    State(st): State<SharedState>,
    Json(b): Json<NewMachine>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let conn = st.db.lock().unwrap();
    conn.execute(
        "INSERT INTO machines(mac,ip,hostname) VALUES(?1,?2,?3)",
        rusqlite::params![b.mac, b.ip, b.hostname],
    )
    .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true, "id": conn.last_insert_rowid()})))
}

#[derive(Deserialize)]
struct Assign {
    machine_id: i64,
    image_id: i64,
}

async fn assign(
    State(st): State<SharedState>,
    Json(b): Json<Assign>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let conn = st.db.lock().unwrap();
    conn.execute(
        "UPDATE machines SET image_id=?1 WHERE id=?2",
        [b.image_id, b.machine_id],
    )
    .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct Timeout {
    seconds: u64,
}

async fn set_timeout(
    State(st): State<SharedState>,
    Json(b): Json<Timeout>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let conn = st.db.lock().unwrap();
    db::set_config(&conn, "boot_timeout", &b.seconds.to_string())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct ZramReserve {
    mb: u64,
}

/// RAM (MB) chừa cho server khi nạp golden vào zram (validate tràn RAM). POST /api/config/zram-reserve
async fn set_zram_reserve(
    State(st): State<SharedState>,
    Json(b): Json<ZramReserve>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let conn = st.db.lock().unwrap();
    db::set_config(&conn, "zram_reserve_mb", &b.mb.to_string())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Config DHCP hiện tại (mode + tham số) để web hiển thị.
async fn get_dhcp(State(st): State<SharedState>) -> Json<serde_json::Value> {
    let conn = st.db.lock().unwrap();
    let g = |k: &str, d: &str| db::get_config(&conn, k, d);
    Json(serde_json::json!({
        "mode": g("dhcp_mode", "proxy"),
        "iface": g("dhcp_iface", ""),
        "server_ip": g("dhcp_server_ip", ""),
        "subnet": g("dhcp_subnet", ""),
        "range_start": g("dhcp_range_start", ""),
        "range_end": g("dhcp_range_end", ""),
        "netmask": g("dhcp_netmask", "255.255.255.0"),
        "gateway": g("dhcp_gateway", ""),
        "dns": g("dhcp_dns", ""),
        "lease": g("dhcp_lease", "12h"),
    }))
}

#[derive(Deserialize)]
struct DhcpBody {
    mode: Option<String>,
    iface: Option<String>,
    server_ip: Option<String>,
    subnet: Option<String>,
    range_start: Option<String>,
    range_end: Option<String>,
    netmask: Option<String>,
    gateway: Option<String>,
    dns: Option<String>,
    lease: Option<String>,
}

/// Đặt config DHCP + sinh lại pxe.conf + restart dnsmasq (dnsmasq.rs).
async fn set_dhcp(
    State(st): State<SharedState>,
    Json(b): Json<DhcpBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let conn = st.db.lock().unwrap();
    let put = |k: &str, v: &Option<String>| {
        if let Some(val) = v {
            db::set_config(&conn, k, val).ok();
        }
    };
    put("dhcp_mode", &b.mode);
    put("dhcp_iface", &b.iface);
    put("dhcp_server_ip", &b.server_ip);
    put("dhcp_subnet", &b.subnet);
    put("dhcp_range_start", &b.range_start);
    put("dhcp_range_end", &b.range_end);
    put("dhcp_netmask", &b.netmask);
    put("dhcp_gateway", &b.gateway);
    put("dhcp_dns", &b.dns);
    put("dhcp_lease", &b.lease);

    let reloaded = dnsmasq::apply(&conn)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("ghi pxe.conf: {e}")))?;
    Ok(Json(serde_json::json!({"ok": true, "reloaded": reloaded})))
}

/// Chỉ sinh lại pxe.conf từ config + bindings hiện có (vd sau khi sửa bảng máy).
async fn apply_dhcp(
    State(st): State<SharedState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let conn = st.db.lock().unwrap();
    let reloaded = dnsmasq::apply(&conn)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("ghi pxe.conf: {e}")))?;
    Ok(Json(serde_json::json!({"ok": true, "reloaded": reloaded})))
}
