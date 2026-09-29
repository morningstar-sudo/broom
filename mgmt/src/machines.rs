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
        .route("/api/machines/group", post(set_group))
        .route("/api/machines/license", post(set_license))
        .route("/api/machines/license/rearm", post(rearm_license))
        .route("/api/license", post(license))
        .route("/api/license/result", post(license_result))
}

// ---- Windows license keys (retail, one per machine) ----
// broom-done.ps1 (when base.vhdx is built) asks GET /api/license. The server picks the key ONLY from the TCP
// peer IP → registered machine; nothing in the request is trusted. Handed out once (armed → sent): the guest
// user is an Administrator and could otherwise fetch it any time. Re-arm on the web to allow one more.

pub(crate) const KEY_RULE: &str = "License key: 25 letters/digits as XXXXX-XXXXX-XXXXX-XXXXX-XXXXX";

/// Windows product key format. Keep in sync with keyOk() in index.html.
pub(crate) fn key_ok(k: &str) -> bool {
    k.len() == 29
        && k.split('-').count() == 5
        && k.split('-').all(|g| g.len() == 5 && g.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
}

/// The registered machine behind a client IP, for the license endpoints. Only a machine's OWN bound IP counts —
/// NOT a DHCP lease (an OFFER hold is trivially spoofed). So a machine that should receive a key must have a fixed
/// IP set (Devices page). `arp` = the peer's
/// MAC as the kernel sees it; when known it must match the machine's MAC (raises the bar; ARP can still be faked).
fn machine_by_ip<'a>(ip: &str, machines: &'a [Machine], arp: Option<&str>) -> Option<&'a Machine> {
    let m = machines.iter().find(|m| m.ip.as_deref() == Some(ip))?;
    match arp {
        Some(mac) if !m.mac.eq_ignore_ascii_case(mac) => None, // IP right, MAC wrong → spoofed
        _ => Some(m),
    }
}

/// The MAC the kernel has for `ip` in the ARP cache (/proc/net/arp), lower-case `aa:bb:…`. None = no entry
/// (the machine may just not have talked to the server yet — the caller treats that as "can't verify", not "deny").
fn arp_mac(ip: &str) -> Option<String> {
    let arp = std::fs::read_to_string("/proc/net/arp").ok()?;
    for line in arp.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        // IP address, HW type, Flags, HW address, Mask, Device
        if f.first() == Some(&ip) && f.get(2) != Some(&"0x0") {
            let mac = f.get(3)?.to_lowercase();
            return (mac.len() == 17 && mac != "00:00:00:00:00:00").then_some(mac);
        }
    }
    None
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub(crate) fn who(m: &Machine) -> String {
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
        notes: None,
    };
    let ms = [m(1, "aa:00:00:00:00:01", Some("10.0.0.51")), m(2, "AA:00:00:00:00:02", None)];
    // Only a machine's OWN bound IP counts, never a lease.
    assert_eq!(machine_by_ip("10.0.0.51", &ms, None).map(|m| m.id), Some(1)); // bound IP, ARP unknown → allowed
    assert_eq!(machine_by_ip("10.0.0.51", &ms, Some("aa:00:00:00:00:01")).map(|m| m.id), Some(1)); // ARP matches
    assert_eq!(machine_by_ip("10.0.0.51", &ms, Some("bb:bb:bb:bb:bb:bb")).map(|m| m.id), None); // ARP MAC mismatch → spoofed
    assert_eq!(machine_by_ip("10.0.0.102", &ms, None).map(|m| m.id), None); // a lease IP is not accepted
    assert_eq!(machine_by_ip("10.0.0.200", &ms, None).map(|m| m.id), None);
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
    let machines = st.db.machines().map_err(ise)?;
    let refuse = |why: String| {
        tracing::warn!("license request from {ip} refused ({why})");
        Err((StatusCode::FORBIDDEN, "no license for this machine".to_string()))
    };
    let Some(m) = machine_by_ip(&ip, &machines, arp_mac(&ip).as_deref()) else {
        return refuse("not a registered machine at this IP/MAC".into());
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
    let machines = st.db.machines().map_err(ise)?;
    let Some(m) = machine_by_ip(&ip, &machines, arp_mac(&ip).as_deref()).filter(|m| m.license_state.as_deref() == Some("sent")) else {
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
    // Never return the password (it is the shared local Administrator password baked into the golden — write-only,
    // like license keys). Only say whether one is set.
    Json(serde_json::json!({
        "user": st.db.get_config("ltsp_user", "guest"),
        "password_set": !st.db.get_config("ltsp_password", "").is_empty(),
    }))
}

/// Guest user/password go into the Windows unattend.xml (inside a PowerShell here-string). Only safe printable
/// characters — this both keeps the XML/PowerShell valid and blocks injection. Keep in sync with the web page.
pub(crate) fn cafe_user_ok(u: &str) -> bool {
    (1..=20).contains(&u.chars().count()) && u.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

pub(crate) fn cafe_pass_ok(p: &str) -> bool {
    (1..=64).contains(&p.chars().count()) && p.chars().all(|c| c.is_ascii_graphic() && !"\"'`$<>&\\".contains(c))
}

type ApiError = (StatusCode, String);

fn ise(e: String) -> ApiError {
    tracing::error!("internal error: {e}"); // keep OS paths/errors in the server log, not the response (L7)
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
}

fn ok() -> Json<serde_json::Value> {
    Json(serde_json::json!({"ok": true}))
}

#[derive(Deserialize)]
struct CafeUser {
    user: String,
    #[serde(default)]
    password: String,
}

async fn set_cafe_user(
    State(st): State<SharedState>,
    Json(b): Json<CafeUser>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let user = b.user.trim();
    if !cafe_user_ok(user) {
        return Err((StatusCode::BAD_REQUEST, "user: 1-20 letters/digits/-_. only".into()));
    }
    st.db.set_config("ltsp_user", user).map_err(ise)?;
    // Password write-only: an empty value keeps the stored one (the web never sends it back).
    if !b.password.is_empty() {
        if !cafe_pass_ok(&b.password) {
            return Err((StatusCode::BAD_REQUEST, "password: 1-64 printable characters, no \" ' ` $ < > & \\".into()));
        }
        st.db.set_config("ltsp_password", &b.password).map_err(ise)?;
    }
    tracing::info!("guest user set to {user}");
    Ok(ok())
}

async fn list(State(st): State<SharedState>) -> Result<Json<Vec<Machine>>, ApiError> {
    Ok(Json(st.db.machines().map_err(ise)?))
}

pub(crate) const HOSTNAME_RULE: &str =
    "Hostname: 1-15 characters, only letters/digits/'-', must not start or end with '-', not all digits (Windows computer name)";

/// Windows computer name (NetBIOS) — becomes the Windows name via stage/broom-done. Keep in sync with hostOk() in index.html.
pub(crate) fn hostname_ok(h: &str) -> bool {
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

#[cfg(test)]
#[test]
fn dhcp_field_validation() {
    assert!(dhcp_field_ok("dhcp_server_ip", "10.0.0.12").is_ok());
    assert!(dhcp_field_ok("dhcp_server_ip", "10.0.0.999").is_err());
    assert!(dhcp_field_ok("dhcp_server_ip", "10.0.0.12\nfoo").is_err()); // newline injection into scripts
    assert!(dhcp_field_ok("dhcp_mode", "full").is_ok() && dhcp_field_ok("dhcp_mode", "off").is_ok());
    assert!(dhcp_field_ok("dhcp_mode", "proxy").is_err() && dhcp_field_ok("dhcp_mode", "rogue").is_err());
    assert!(dhcp_field_ok("dhcp_dns", "8.8.8.8, 1.1.1.1").is_ok() && dhcp_field_ok("dhcp_dns", "").is_ok());
    assert!(dhcp_field_ok("dhcp_dns", "8.8.8.8,notip").is_err());
    assert!(dhcp_field_ok("dhcp_iface", "eth0").is_ok() && dhcp_field_ok("dhcp_iface", "eth 0;rm").is_err());
    assert!(dhcp_field_ok("dhcp_lease", "12h").is_ok() && dhcp_field_ok("dhcp_lease", "3600").is_ok());
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
    // Same checks as the Devices page: MAC normalized, hostname rule, MAC / hostname / IP unique.
    let mut m = Machine {
        id: 0,
        mac: b.mac,
        ip: b.ip,
        hostname: b.hostname,
        image_id: None,
        license_key: None,
        license_tail: None,
        license_state: None,
        license_gen: 0,
        license_result: None,
        grp: None,
        notes: None,
    };
    let all = st.db.machines().map_err(ise)?;
    crate::devices::validate(&mut m, &all).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let id = st.db.add_machine(&m.mac, m.ip.as_deref(), m.hostname.as_deref()).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    tracing::info!("machine registered - mac {} - ip {} - hostname {}", m.mac, m.ip.as_deref().unwrap_or("-"), m.hostname.as_deref().unwrap_or("-"));
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
    let secs = b.seconds.min(3600); // used as timeout_s * 1000 in the iPXE menu
    st.db.set_config("boot_timeout", &secs.to_string()).map_err(ise)?;
    tracing::info!("boot menu countdown set to {secs} s");
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
    let mb = b.mb.min(1 << 30); // reserve * 1024 * 1024 later; cap well below u64 overflow
    st.db.set_config("zram_reserve_mb", &mb.to_string()).map_err(ise)?;
    tracing::info!("zram RAM reserve set to {mb} MB");
    Ok(ok())
}

/// Current DHCP config (mode + parameters) for the web to display.
async fn get_dhcp(State(st): State<SharedState>) -> Json<serde_json::Value> {
    let g = |k: &str, d: &str| st.db.get_config(k, d);
    Json(serde_json::json!({
        "mode": g("dhcp_mode", "full"),
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

/// Validate one DHCP field. IPv4 fields must parse; a stored server IP / gateway / DNS flows into scripts + boot
/// options later, so a bad value (newline, non-IP) is refused HERE, before it is saved.
fn dhcp_field_ok(key: &str, v: &str) -> Result<(), String> {
    use std::net::Ipv4Addr;
    let ipv4 = |s: &str| s.parse::<Ipv4Addr>().is_ok();
    let ok = match key {
        "dhcp_mode" => v == "full" || v == "off",
        "dhcp_iface" => v.is_empty() || (v.len() <= 15 && v.chars().all(|c| c.is_ascii_alphanumeric() || ".-_@".contains(c))),
        "dhcp_server_ip" | "dhcp_subnet" | "dhcp_netmask" | "dhcp_range_start" | "dhcp_range_end" => ipv4(v),
        "dhcp_gateway" | "dhcp_dns" => v.is_empty() || v.split(',').all(|p| ipv4(p.trim())),
        "dhcp_lease" => v.parse::<u32>().is_ok() || matches!(v.chars().last(), Some('h' | 'm' | 's')),
        _ => true,
    };
    if ok { Ok(()) } else { Err(format!("{}: invalid value {v:?}", key.trim_start_matches("dhcp_"))) }
}

/// Save the DHCP config + restart the built-in DHCP/TFTP listeners (dhcp.rs). Every field is validated FIRST; if any
/// is bad, nothing is saved (a half-saved bad config would keep DHCP/TFTP down after the next restart).
async fn set_dhcp(
    State(st): State<SharedState>,
    Json(b): Json<DhcpBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let fields = [
        ("dhcp_mode", &b.mode),
        ("dhcp_iface", &b.iface),
        ("dhcp_server_ip", &b.server_ip),
        ("dhcp_subnet", &b.subnet),
        ("dhcp_range_start", &b.range_start),
        ("dhcp_range_end", &b.range_end),
        ("dhcp_netmask", &b.netmask),
        ("dhcp_gateway", &b.gateway),
        ("dhcp_dns", &b.dns),
        ("dhcp_lease", &b.lease),
    ];
    for (k, v) in &fields {
        if let Some(val) = v {
            dhcp_field_ok(k, val.trim()).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        }
    }
    // When the DHCP server is on, it needs a bound interface (empty = all interfaces → a rogue DHCP server on a
    // WAN/VPN link). "off" needs nothing.
    let mode = b.mode.as_deref().unwrap_or(&st.db.get_config("dhcp_mode", "full")).to_string();
    let iface = b.iface.clone().unwrap_or_else(|| st.db.get_config("dhcp_iface", ""));
    if mode == "full" && iface.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "the DHCP server needs a specific interface (leaving it blank binds every interface)".into()));
    }
    for (k, v) in &fields {
        if let Some(val) = v {
            st.db.set_config(k, val.trim()).map_err(ise)?;
        }
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
