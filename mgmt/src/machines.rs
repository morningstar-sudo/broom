// machines.rs — M8 mgmt-config. Machine table (MAC/IP/hostname), image assignment,
// default image + countdown timeout, DHCP mode/parameters → restart the built-in DHCP (dhcp.rs).
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::db::Machine;
use crate::{dhcp, SharedState};

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
    Json(serde_json::json!({
        "user": st.db.get_config("ltsp_user", "guest"),
        "password": st.db.get_config("ltsp_password", "123456"),
    }))
}

type ApiError = (StatusCode, String);

fn ise(e: String) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e)
}

fn ok() -> Json<serde_json::Value> {
    Json(serde_json::json!({"ok": true}))
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
    st.db.set_config("ltsp_user", b.user.trim()).map_err(ise)?;
    st.db.set_config("ltsp_password", &b.password).map_err(ise)?;
    tracing::info!("guest user set to {}", b.user.trim());
    Ok(ok())
}

async fn list(State(st): State<SharedState>) -> Result<Json<Vec<Machine>>, ApiError> {
    Ok(Json(st.db.machines().map_err(ise)?))
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
    let id = st.db.add_machine(&b.mac, b.ip.as_deref(), hostname).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    tracing::info!("machine registered - mac {} - ip {} - hostname {}", b.mac, b.ip.as_deref().unwrap_or("-"), hostname.unwrap_or("-"));
    Ok(Json(serde_json::json!({"ok": true, "id": id})))
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
    st.db.assign_image(b.machine_id, b.image_id).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    tracing::info!("machine {} assigned image {}", b.machine_id, b.image_id);
    Ok(ok())
}

#[derive(Deserialize)]
struct Timeout {
    seconds: u64,
}

async fn set_timeout(
    State(st): State<SharedState>,
    Json(b): Json<Timeout>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    st.db.set_config("boot_timeout", &b.seconds.to_string()).map_err(ise)?;
    tracing::info!("boot menu countdown set to {} s", b.seconds);
    Ok(ok())
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
    st.db.set_config("zram_reserve_mb", &b.mb.to_string()).map_err(ise)?;
    tracing::info!("zram RAM reserve set to {} MB", b.mb);
    Ok(ok())
}

/// Current DHCP config (mode + parameters) for the web to display.
async fn get_dhcp(State(st): State<SharedState>) -> Json<serde_json::Value> {
    let g = |k: &str, d: &str| st.db.get_config(k, d);
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

/// Save the DHCP config + restart the built-in DHCP/TFTP listeners (dhcp.rs).
async fn set_dhcp(
    State(st): State<SharedState>,
    Json(b): Json<DhcpBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    {
        let put = |k: &str, v: &Option<String>| {
            if let Some(val) = v {
                st.db.set_config(k, val).ok();
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
    }
    restart(&st).await
}

/// Restart the DHCP/TFTP listeners with the current config. (Machine bindings need no restart:
/// every DHCP request reads them from the DB.)
async fn apply_dhcp(State(st): State<SharedState>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    restart(&st).await
}

async fn restart(st: &SharedState) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let status = dhcp::start(st).await.map_err(|e| {
        tracing::error!("network settings: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e)
    })?;
    tracing::info!("network settings applied: {status}");
    Ok(Json(serde_json::json!({"ok": true, "reloaded": true, "status": status})))
}
