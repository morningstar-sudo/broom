// machines.rs — M8 mgmt-config. Machine table (MAC/IP/hostname), image assignment,
// default image + countdown timeout, DHCP mode selection → generate bindings.conf + reload dnsmasq.
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
        .route("/api/cafe-user", get(get_cafe_user).post(set_cafe_user))
}

/// Guest user (created in the Windows golden by broom-prep-win → autologon). The DB keys keep the old names
/// `ltsp_user`/`ltsp_password` so running DBs need no migration.
async fn get_cafe_user(State(st): State<SharedState>) -> Json<serde_json::Value> {
    let conn = st.db.lock().unwrap();
    Json(serde_json::json!({
        "user": db::get_config(&conn, "ltsp_user", "guest"),
        "password": db::get_config(&conn, "ltsp_password", "123456"),
    }))
}

#[derive(Deserialize)]
struct CafeUser {
    user: String,
    password: String,
}

async fn set_cafe_user(
    State(st): State<SharedState>,
    Json(b): Json<CafeUser>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if b.user.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "user is empty".into()));
    }
    let conn = st.db.lock().unwrap();
    db::set_config(&conn, "ltsp_user", b.user.trim()).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    db::set_config(&conn, "ltsp_password", &b.password).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true})))
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

const HOSTNAME_RULE: &str =
    "Hostname: 1-15 characters, only letters/digits/'-', must not start or end with '-', not all digits (Windows computer name)";

/// Windows computer name (NetBIOS) — becomes the Windows name via stage/broom-done. Keep in sync with hostOk() in index.html.
fn hostname_ok(h: &str) -> bool {
    (1..=15).contains(&h.len())
        && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !h.starts_with('-')
        && !h.ends_with('-')
        && !h.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
#[test]
fn hostname_rule() {
    for ok in ["PC01", "FPS-43", "a", "ABCDEFGHIJKLMNO"] {
        assert!(hostname_ok(ok), "{ok}");
    }
    for bad in ["", "ABCDEFGHIJKLMNOP", "PC 01", "PC_01", "-PC", "PC-", "123", "Zoë1"] {
        assert!(!hostname_ok(bad), "{bad}");
    }
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
    let hostname = b.hostname.as_deref().map(str::trim).filter(|h| !h.is_empty());
    if let Some(h) = hostname {
        if !hostname_ok(h) {
            return Err((StatusCode::BAD_REQUEST, HOSTNAME_RULE.into()));
        }
    }
    let conn = st.db.lock().unwrap();
    conn.execute(
        "INSERT INTO machines(mac,ip,hostname) VALUES(?1,?2,?3)",
        rusqlite::params![b.mac, b.ip, hostname],
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

/// RAM (MB) kept for the server when loading a golden into zram (RAM overflow check). POST /api/config/zram-reserve
async fn set_zram_reserve(
    State(st): State<SharedState>,
    Json(b): Json<ZramReserve>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let conn = st.db.lock().unwrap();
    db::set_config(&conn, "zram_reserve_mb", &b.mb.to_string())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Current DHCP config (mode + parameters) for the web to display.
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

/// Set the DHCP config + regenerate pxe.conf + restart dnsmasq (dnsmasq.rs).
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

/// Only regenerate pxe.conf from the current config + bindings (e.g. after editing the machine table).
async fn apply_dhcp(
    State(st): State<SharedState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let conn = st.db.lock().unwrap();
    let reloaded = dnsmasq::apply(&conn)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("ghi pxe.conf: {e}")))?;
    Ok(Json(serde_json::json!({"ok": true, "reloaded": reloaded})))
}
