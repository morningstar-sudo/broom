// settings.rs — server settings on the web: boot menu countdown, zram reserve, DHCP (mode + parameters → restarts
// the built-in DHCP/TFTP, dhcp.rs), and the Windows guest user baked into goldens.
use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use serde::Deserialize;

use crate::api::{ise, ok};
use crate::dhcp::{self, DhcpOpts};
use crate::SharedState;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/config", get(get_config))
        .route("/api/config/timeout", post(set_timeout))
        .route("/api/config/zram-reserve", post(set_zram_reserve))
        .route("/api/dhcp", get(get_dhcp).post(set_dhcp))
        .route("/api/dhcp/apply", post(apply_dhcp))
        .route("/api/cafe-user", get(get_cafe_user).post(set_cafe_user))
}

/// Guest user (created in the Windows golden by the prep script → autologon). The DB keys keep the old names
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

/// System page values (same defaults as their readers: boot.rs, publish.rs).
async fn get_config(State(st): State<SharedState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "boot_timeout": st.db.get_config("boot_timeout", "10").parse::<u64>().unwrap_or(10),
        "zram_reserve_mb": st.db.get_config("zram_reserve_mb", "2048").parse::<u64>().unwrap_or(2048),
    }))
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

/// Current DHCP config (mode, parameters, optional behaviours) for the web to display.
async fn get_dhcp(State(st): State<SharedState>) -> Json<serde_json::Value> {
    let g = |k: &str, d: &str| st.db.get_config(k, d);
    Json(serde_json::json!({
        "mode": g("dhcp_mode", "off"),
        "iface": g("dhcp_iface", ""),
        "server_ip": g("dhcp_server_ip", ""),
        "subnet": g("dhcp_subnet", ""),
        "range_start": g("dhcp_range_start", ""),
        "range_end": g("dhcp_range_end", ""),
        "netmask": g("dhcp_netmask", "255.255.255.0"),
        "gateway": g("dhcp_gateway", ""),
        "dns": g("dhcp_dns", ""),
        "lease": g("dhcp_lease", "12h"),
        "ipxe_signed": g("ipxe_signed", "0") == "1",
        "strict_reset": g("strict_reset", "0") == "1",
        "rapid_commit": g(DhcpOpts::KEYS[0].0, DhcpOpts::KEYS[0].1) == "1",
        "ipxe_fast": g(DhcpOpts::KEYS[1].0, DhcpOpts::KEYS[1].1) == "1",
        "authoritative": g(DhcpOpts::KEYS[2].0, DhcpOpts::KEYS[2].1) == "1",
        "send_hostname": g(DhcpOpts::KEYS[3].0, DhcpOpts::KEYS[3].1) == "1",
    }))
}

/// Most DNS servers handed out (option 6) — a client only tries the first few anyway.
const MAX_DNS: usize = 8;

/// "8.8.8.8 1.1.1.1, 8.8.8.8" → "8.8.8.8,1.1.1.1": commas or spaces, valid IPv4 only, duplicates dropped, order kept
/// (= the clients' order of preference), at most MAX_DNS.
fn normalize_dns(v: &str) -> Result<String, String> {
    let mut out: Vec<std::net::Ipv4Addr> = Vec::new();
    for p in v.split([',', ' ', ';']).map(str::trim).filter(|p| !p.is_empty()) {
        let ip = p.parse().map_err(|_| format!("dns: {p:?} is not an IPv4 address"))?;
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    if out.len() > MAX_DNS {
        return Err(format!("dns: at most {MAX_DNS} servers"));
    }
    Ok(out.iter().map(|ip| ip.to_string()).collect::<Vec<_>>().join(","))
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
    /// "Secure Boot clients": "1" = official signed iPXE for UEFI PXE, "0" = our own build.
    ipxe_signed: Option<String>,
    strict_reset: Option<String>,
    /// Optional DHCP behaviours ("0"/"1"), see dhcp::DhcpOpts.
    rapid_commit: Option<String>,
    ipxe_fast: Option<String>,
    authoritative: Option<String>,
    send_hostname: Option<String>,
}

/// Validate one DHCP field. IPv4 fields must parse; a stored server IP / gateway / DNS flows into scripts + boot
/// options later, so a bad value (newline, non-IP) is refused HERE, before it is saved.
fn dhcp_field_ok(key: &str, v: &str) -> Result<(), String> {
    use std::net::Ipv4Addr;
    let ipv4 = |s: &str| s.parse::<Ipv4Addr>().is_ok();
    let ok = match key {
        "dhcp_mode" => v == "full" || v == "off",
        "dhcp_iface" => v.is_empty() || (v.len() <= 15 && v.chars().all(|c| c.is_ascii_alphanumeric() || ".-_@".contains(c))),
        "dhcp_server_ip" => ipv4(v),
        // Only the DHCP server uses these (full mode checks them in set_dhcp) → blank is fine while it is off.
        "dhcp_subnet" | "dhcp_netmask" | "dhcp_range_start" | "dhcp_range_end" | "dhcp_gateway" => v.is_empty() || ipv4(v),
        "dhcp_dns" => v.is_empty() || v.split(',').all(|p| ipv4(p.trim())),
        "dhcp_lease" => dhcp::parse_lease(v).is_some(),
        "ipxe_signed" | "strict_reset" | "dhcp_rapid_commit" | "dhcp_ipxe_fast" | "dhcp_authoritative"
        | "dhcp_send_hostname" => v == "0" || v == "1",
        _ => true,
    };
    if ok { Ok(()) } else { Err(format!("{}: invalid value {v:?}", key.trim_start_matches("dhcp_"))) }
}

/// The DHCP range: both ends set, start ≤ end, both in the server's subnet (server IP + netmask).
fn range_ok(start: &str, end: &str, server: &str, mask: &str) -> Result<(), String> {
    use std::net::Ipv4Addr;
    let ip = |s: &str, what: &str| s.trim().parse::<Ipv4Addr>().map(u32::from).map_err(|_| format!("the DHCP server needs a valid {what}"));
    let (s, e, srv) = (ip(start, "range start")?, ip(end, "range end")?, ip(server, "server IP")?);
    let m = ip(if mask.trim().is_empty() { "255.255.255.0" } else { mask }, "netmask")?;
    if s > e {
        return Err(format!("range start {start} is after range end {end}"));
    }
    if s & m != srv & m || e & m != srv & m {
        return Err(format!("range {start}-{end} is not in the server's subnet ({server} / {mask})"));
    }
    Ok(())
}

/// Save the DHCP config + restart the built-in DHCP/TFTP listeners (dhcp.rs). Every field is validated FIRST; if any
/// is bad, nothing is saved (a half-saved bad config would keep DHCP/TFTP down after the next restart).
async fn set_dhcp(
    State(st): State<SharedState>,
    Json(b): Json<DhcpBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let dns = b.dns.as_deref().map(normalize_dns).transpose().map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let fields = [
        ("dhcp_mode", &b.mode),
        ("dhcp_iface", &b.iface),
        ("dhcp_server_ip", &b.server_ip),
        ("dhcp_subnet", &b.subnet),
        ("dhcp_range_start", &b.range_start),
        ("dhcp_range_end", &b.range_end),
        ("dhcp_netmask", &b.netmask),
        ("dhcp_gateway", &b.gateway),
        ("dhcp_dns", &dns),
        ("dhcp_lease", &b.lease),
        ("ipxe_signed", &b.ipxe_signed),
        ("strict_reset", &b.strict_reset),
        (DhcpOpts::KEYS[0].0, &b.rapid_commit),
        (DhcpOpts::KEYS[1].0, &b.ipxe_fast),
        (DhcpOpts::KEYS[2].0, &b.authoritative),
        (DhcpOpts::KEYS[3].0, &b.send_hostname),
    ];
    for (k, v) in &fields {
        if let Some(val) = v {
            dhcp_field_ok(k, val.trim()).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        }
    }
    // When the DHCP server is on, it needs a bound interface (empty = all interfaces → a rogue DHCP server on a
    // WAN/VPN link). "off" needs nothing.
    let mode = b.mode.as_deref().unwrap_or(&st.db.get_config("dhcp_mode", "off")).to_string();
    let iface = b.iface.clone().unwrap_or_else(|| st.db.get_config("dhcp_iface", ""));
    if mode == "full" && iface.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "the DHCP server needs a specific interface (leaving it blank binds every interface)".into()));
    }
    // A lease without gateway/DNS takes every machine that gets it off the internet.
    let cur = |v: &Option<String>, k: &str| v.clone().unwrap_or_else(|| st.db.get_config(k, ""));
    if mode == "full" && (cur(&b.gateway, "dhcp_gateway").trim().is_empty() || cur(&dns, "dhcp_dns").trim().is_empty()) {
        return Err((StatusCode::BAD_REQUEST, "the DHCP server needs a gateway and DNS (clients would get no internet)".into()));
    }
    let server = cur(&b.server_ip, "dhcp_server_ip");
    if mode == "full" {
        let mask = cur(&b.netmask, "dhcp_netmask");
        range_ok(&cur(&b.range_start, "dhcp_range_start"), &cur(&b.range_end, "dhcp_range_end"), &server, &mask)
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    }
    // Boot scripts, the Host check (auth.rs) and DHCP option 54 all use it: an address this machine doesn't have
    // breaks every boot. Binding to it only works when it is ours.
    if b.server_ip.is_some() && std::net::UdpSocket::bind((server.trim(), 0)).is_err() {
        return Err((StatusCode::BAD_REQUEST, format!("server IP {server} is not an address of this server")));
    }
    for (k, v) in &fields {
        if let Some(val) = v {
            st.db.set_config(k, val.trim()).map_err(ise)?;
        }
    }
    if b.ipxe_signed.as_deref() == Some("1") {
        // Secure Boot clients need the shim from the stage bundle → fetch it now if it isn't there (background).
        tokio::task::spawn_blocking(crate::publish::prepare_stage);
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
    assert!(dhcp_field_ok("dhcp_lease", "1d").is_ok() && dhcp_field_ok("dhcp_lease", "h").is_err());
    // DHCP server off (fresh install): the range fields are blank and must not block Apply.
    assert!(dhcp_field_ok("dhcp_range_start", "").is_ok() && dhcp_field_ok("dhcp_subnet", "").is_ok());
    assert!(dhcp_field_ok("dhcp_range_end", "10.0.0.x").is_err());
    // Option 3 carries one router; a list would be silently dropped.
    assert!(dhcp_field_ok("dhcp_gateway", "10.0.0.1").is_ok() && dhcp_field_ok("dhcp_gateway", "10.0.0.1,10.0.0.2").is_err());
}

#[cfg(test)]
#[test]
fn dhcp_range_checked() {
    assert!(range_ok("10.0.0.100", "10.0.0.200", "10.0.0.12", "255.255.255.0").is_ok());
    assert!(range_ok("10.0.1.100", "10.0.1.200", "10.0.0.12", "255.255.254.0").is_ok(), "/23");
    assert!(range_ok("10.0.0.200", "10.0.0.100", "10.0.0.12", "").is_err(), "start after end");
    assert!(range_ok("10.0.1.100", "10.0.1.200", "10.0.0.12", "").is_err(), "outside the /24");
    assert!(range_ok("", "10.0.0.200", "10.0.0.12", "").is_err(), "blank start in full mode");
}

#[cfg(test)]
#[test]
fn dns_list_normalized() {
    assert_eq!(normalize_dns("8.8.8.8 1.1.1.1, 8.8.8.8;9.9.9.9").unwrap(), "8.8.8.8,1.1.1.1,9.9.9.9");
    assert_eq!(normalize_dns("  ").unwrap(), "");
    assert!(normalize_dns("8.8.8.8, dns.google").unwrap_err().contains("dns.google"));
    let nine: Vec<String> = (1..=9).map(|i| format!("10.0.0.{i}")).collect();
    assert!(normalize_dns(&nine.join(",")).unwrap_err().contains("at most 8"));
}
