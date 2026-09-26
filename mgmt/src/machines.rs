// machines.rs — M8 mgmt-config. Machine table (MAC/IP/hostname), image assignment,
// default image + countdown timeout, DHCP mode/parameters → restart the built-in DHCP (dhcp.rs).
use axum::{
    extract::{ConnectInfo, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::net::SocketAddr;

use crate::db::{Lease, Machine};
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
        .route("/api/machines/group", post(set_group))
        .route("/api/machines/license", post(set_license))
        .route("/api/machines/license/rearm", post(rearm_license))
        .route("/api/license", get(license))
        .route("/api/license/result", post(license_result))
}

// ---- Windows license keys (retail, one per machine) ----
// broom-done.ps1 (when base.vhdx is built) asks GET /api/license. The server picks the key ONLY from the TCP
// peer IP → registered machine; nothing in the request is trusted. Handed out once (armed → sent): the guest
// user is an Administrator and could otherwise fetch it any time. Re-arm on the web to allow one more.

const KEY_RULE: &str = "License key: 25 letters/digits as XXXXX-XXXXX-XXXXX-XXXXX-XXXXX";

/// Windows product key format. Keep in sync with keyOk() in index.html.
fn key_ok(k: &str) -> bool {
    k.len() == 29
        && k.split('-').count() == 5
        && k.split('-').all(|g| g.len() == 5 && g.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
}

/// The registered machine behind a client IP: bound IP first, else its unexpired lease.
fn machine_by_ip<'a>(ip: &str, machines: &'a [Machine], leases: &[Lease], now: i64) -> Option<&'a Machine> {
    machines.iter().find(|m| m.ip.as_deref() == Some(ip)).or_else(|| {
        let l = leases.iter().find(|l| l.ip.as_deref() == Some(ip) && l.expires > now)?;
        machines.iter().find(|m| m.mac.eq_ignore_ascii_case(&l.mac))
    })
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn who(m: &Machine) -> String {
    m.hostname.clone().unwrap_or_else(|| m.mac.clone())
}

#[cfg(test)]
#[test]
fn license_key_and_lookup() {
    assert!(key_ok("ABCDE-12345-FGHIJ-67890-KLMNO"));
    for bad in ["", "abcde-12345-fghij-67890-klmno", "ABCDE-12345-FGHIJ-67890", "ABCDE-12345-FGHIJ-67890-KLMN!", "ABCDE12345FGHIJ67890KLMNO1234"] {
        assert!(!key_ok(bad), "{bad}");
    }
    let m = |id, mac: &str, ip: Option<&str>| Machine {
        id,
        mac: mac.into(),
        ip: ip.map(Into::into),
        hostname: Some(format!("PC{id}")),
        image_id: None,
        license_key: None,
        license_tail: None,
        license_state: None,
        license_gen: 0,
        license_result: None,
        grp: None,
    };
    let ms = [m(1, "aa:00:00:00:00:01", Some("10.0.0.51")), m(2, "AA:00:00:00:00:02", None)];
    let lease = |mac: &str, ip: &str, expires| Lease { mac: mac.into(), ip: Some(ip.into()), hostname: None, expires, source: "full".into() };
    let ls = [lease("aa:00:00:00:00:02", "10.0.0.102", 100), lease("aa:00:00:00:00:09", "10.0.0.109", 100)];
    let id = |ip| machine_by_ip(ip, &ms, &ls, 50).map(|m| m.id);
    assert_eq!(id("10.0.0.51"), Some(1)); // bound IP
    assert_eq!(id("10.0.0.102"), Some(2)); // lease → mac (case-insensitive)
    assert_eq!(id("10.0.0.109"), None); // lease of an unregistered machine
    assert_eq!(id("10.0.0.200"), None);
    assert_eq!(machine_by_ip("10.0.0.102", &ms, &ls, 200).map(|m| m.id), None); // expired lease
}

#[derive(Deserialize)]
struct LicenseBody {
    id: i64,
    key: String,
}

fn machine_by_id(st: &SharedState, id: i64) -> Result<Machine, ApiError> {
    st.db.machines().map_err(ise)?.into_iter().find(|m| m.id == id).ok_or((StatusCode::NOT_FOUND, format!("machine {id} not found")))
}

/// POST /api/machines/license {id, key} — set (armed: fetched once at the next base build) or remove (empty key).
async fn set_license(State(st): State<SharedState>, Json(b): Json<LicenseBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let m = machine_by_id(&st, b.id)?;
    let key = b.key.trim().to_ascii_uppercase();
    if key.is_empty() {
        st.db.set_license(m.id, None).map_err(ise)?;
        tracing::info!("license key removed from {}", who(&m));
        return Ok(ok());
    }
    if !key_ok(&key) {
        return Err((StatusCode::BAD_REQUEST, KEY_RULE.into()));
    }
    st.db.set_license(m.id, Some(&key)).map_err(ise)?;
    tracing::info!("license key …{} set for {} — handed out once when its base is rebuilt (next boot)", &key[24..], who(&m));
    Ok(ok())
}

#[derive(Deserialize)]
struct IdBody {
    id: i64,
}

#[derive(Deserialize)]
struct GroupBody {
    id: i64,
    grp: String,
}

/// POST /api/machines/group {id, grp} — free-text group (driver packages can target it); empty = none.
async fn set_group(State(st): State<SharedState>, Json(b): Json<GroupBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let m = machine_by_id(&st, b.id)?;
    let grp = b.grp.trim();
    if !grp.is_empty() && !crate::drivers::group_ok(grp) {
        return Err((StatusCode::BAD_REQUEST, "group: letters/digits/_/-, max 32".into()));
    }
    st.db.set_machine_group(m.id, (!grp.is_empty()).then_some(grp)).map_err(ise)?;
    tracing::info!("{} group set to {}", who(&m), if grp.is_empty() { "-" } else { grp });
    Ok(ok())
}

/// POST /api/machines/license/rearm {id} — allow the stored key to be fetched once more (base rebuilt next boot).
async fn rearm_license(State(st): State<SharedState>, Json(b): Json<IdBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let m = machine_by_id(&st, b.id)?;
    if m.license_key.is_none() {
        return Err((StatusCode::BAD_REQUEST, "this machine has no license key".into()));
    }
    st.db.rearm_license(m.id).map_err(ise)?;
    tracing::info!("license key of {} re-armed", who(&m));
    Ok(ok())
}

/// GET /api/license (Windows, broom-done.ps1) → the key of the machine at the peer IP, once. 403 otherwise.
async fn license(State(st): State<SharedState>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Result<String, ApiError> {
    let ip = peer.ip().to_string();
    let (machines, leases) = (st.db.machines().map_err(ise)?, st.db.leases().map_err(ise)?);
    let refuse = |why: String| {
        tracing::warn!("license request from {ip} refused ({why})");
        Err((StatusCode::FORBIDDEN, "no license for this machine".to_string()))
    };
    let Some(m) = machine_by_ip(&ip, &machines, &leases, now()) else {
        return refuse("not a registered machine".into());
    };
    if m.license_key.is_none() {
        return refuse(format!("{}: no key set", who(m)));
    }
    match st.db.take_license(m.id).map_err(ise)? {
        Some(key) => {
            tracing::info!("license sent to {} - mac {} - ip {ip}", who(m), m.mac);
            Ok(key)
        }
        None => refuse(format!("{}: already sent — re-arm it on the Machines page", who(m))),
    }
}

/// POST /api/license/result (body = slmgr output) — only from the IP of a machine whose key was just sent.
async fn license_result(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: String,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ip = peer.ip().to_string();
    let (machines, leases) = (st.db.machines().map_err(ise)?, st.db.leases().map_err(ise)?);
    let Some(m) = machine_by_ip(&ip, &machines, &leases, now()).filter(|m| m.license_state.as_deref() == Some("sent")) else {
        tracing::warn!("license result from {ip} ignored (no key was sent to it)");
        return Err((StatusCode::FORBIDDEN, "no license was sent to this machine".into()));
    };
    let text: String = body.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" | ").chars().take(500).collect();
    st.db.set_license_result(m.id, &text).map_err(ise)?;
    tracing::info!("license result from {} - ip {ip}: {text}", who(m));
    Ok(ok())
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
