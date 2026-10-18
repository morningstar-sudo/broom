// settings.rs — server settings on the web: boot menu countdown, zram reserve, DHCP (mode + parameters → restarts
// the built-in DHCP/TFTP, dhcp.rs), and the Windows guest user baked into goldens.
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;

use crate::SharedState;
use crate::api::{ise, ok};
use crate::dhcp::{self, DhcpOpts};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/config", get(get_config))
        .route("/api/config/timeout", post(set_timeout))
        .route("/api/config/zram-reserve", post(set_zram_reserve))
        .route("/api/config/alert", post(set_alert))
        .route("/api/config/ssd", post(set_ssd))
        .route("/api/config/golden-streams", post(set_golden_streams))
        .route("/api/client-config", get(client_config))
        .route("/api/dhcp", get(get_dhcp).post(set_dhcp))
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
    (1..=20).contains(&u.chars().count())
        && u.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

pub(crate) fn cafe_pass_ok(p: &str) -> bool {
    (1..=64).contains(&p.chars().count())
        && p.chars()
            .all(|c| c.is_ascii_graphic() && !"\"'`$<>&\\".contains(c))
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
        return Err((
            StatusCode::BAD_REQUEST,
            "user: 1-20 letters/digits/-_. only".into(),
        ));
    }
    st.db.set_config("ltsp_user", user).map_err(ise)?;
    // Password write-only: an empty value keeps the stored one (the web never sends it back).
    if !b.password.is_empty() {
        if !cafe_pass_ok(&b.password) {
            return Err((
                StatusCode::BAD_REQUEST,
                "password: 1-64 printable characters, no \" ' ` $ < > & \\".into(),
            ));
        }
        st.db
            .set_config("ltsp_password", &b.password)
            .map_err(ise)?;
    }
    tracing::info!("guest user set to {user}");
    Ok(ok())
}

/// System page values (same defaults as their readers: boot.rs, publish.rs).
async fn get_config(State(st): State<SharedState>) -> Json<serde_json::Value> {
    let (room, warn, reboot, pagefile) = ssd_limits(&st);
    let (streams, _, link) = crate::goldenram::streams(&st);
    let (active, waiting) = crate::goldenram::queue_stats();
    Json(serde_json::json!({
        "boot_timeout": st.db.get_config("boot_timeout", "3").parse::<u64>().unwrap_or(3),
        "zram_reserve_mb": st.db.get_config("zram_reserve_mb", "2048").parse::<u64>().unwrap_or(2048),
        "alert_pct": st.db.get_config("alert_pct", "80").parse::<u64>().unwrap_or(80),
        "ssd_room_gb": room,
        "ssd_warn_gb": warn,
        "ssd_reboot_gb": reboot,
        "pagefile_gb": pagefile,
        "golden_streams": streams,
        "golden_streams_auto": crate::goldenram::limit_for("auto", link.as_ref().and_then(|l| l.1)),
        "golden_link": link.map(|(i, s)| match s {
            Some(m) => format!("{i} {} Gbit/s", m as f64 / 1000.0),
            None => format!("{i}, speed unknown"),
        }),
        "golden_active": active,
        "golden_waiting": waiting,
    }))
}

/// Windows SSD (BROOMWIN), GB: free room the stage keeps for each session (parked images go to make it), and the free
/// room at which the session watchdog (broom-watch.ps1) warns the guest / restarts the machine — before Windows itself
/// would crash with its boot disk out of room. Last: the pagefile Windows gets on BROOMWIN (0 = none; its C: is a
/// VHDX, where Windows refuses one), made by broom-bootorder.ps1 at every boot, kept free on top of the room.
fn ssd_limits(st: &SharedState) -> (u64, u64, u64, u64) {
    let n = |k, d: u64| {
        st.db
            .get_config(k, &d.to_string())
            .parse::<u64>()
            .unwrap_or(d)
    };
    (
        n("ssd_room_gb", 20),
        n("ssd_warn_gb", 10),
        n("ssd_reboot_gb", 3),
        n("pagefile_gb", 0),
    )
}

/// GET /api/client-config (public: the Windows stage + watchdog) → "<room GB> <warn GB> <reboot GB> <pagefile GB>".
pub(crate) async fn client_config(State(st): State<SharedState>) -> String {
    let (room, warn, reboot, pagefile) = ssd_limits(&st);
    format!("{room} {warn} {reboot} {pagefile}\n")
}

#[derive(Deserialize)]
struct SsdLimits {
    room_gb: u64,
    warn_gb: u64,
    reboot_gb: u64,
    /// Older pages send none: unchanged.
    #[serde(default)]
    pagefile_gb: Option<u64>,
}

/// POST /api/config/ssd {room_gb, warn_gb, reboot_gb, pagefile_gb} — read by the clients at each boot.
async fn set_ssd(
    State(st): State<SharedState>,
    Json(b): Json<SsdLimits>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !(5..=2000).contains(&b.room_gb)
        || !(1..=b.room_gb).contains(&b.warn_gb)
        || !(1..b.warn_gb).contains(&b.reboot_gb)
        || b.pagefile_gb.is_some_and(|p| p > 64)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "need 5 ≤ room ≤ 2000, 1 ≤ warn ≤ room, 1 ≤ restart < warn, pagefile ≤ 64 (GB)".into(),
        ));
    }
    if let Some(p) = b.pagefile_gb {
        st.db
            .set_config("pagefile_gb", &p.to_string())
            .map_err(ise)?;
    }
    st.db
        .set_config("ssd_room_gb", &b.room_gb.to_string())
        .map_err(ise)?;
    st.db
        .set_config("ssd_warn_gb", &b.warn_gb.to_string())
        .map_err(ise)?;
    st.db
        .set_config("ssd_reboot_gb", &b.reboot_gb.to_string())
        .map_err(ise)?;
    tracing::info!(
        "SSD: {} GB kept free per session, warn below {} GB, restart below {} GB, pagefile {} GB",
        b.room_gb,
        b.warn_gb,
        b.reboot_gb,
        ssd_limits(&st).3
    );
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
    st.db
        .set_config("boot_timeout", &secs.to_string())
        .map_err(ise)?;
    tracing::info!("boot menu countdown set to {secs} s");
    Ok(ok())
}

#[derive(Deserialize)]
struct Alert {
    pct: u64,
}

/// Server resource alert threshold (%): a banner on every page while CPU / RAM / network / the image disk stays at or
/// above it (sysmon.rs). POST /api/config/alert {pct}
async fn set_alert(
    State(st): State<SharedState>,
    Json(b): Json<Alert>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !(50..=99).contains(&b.pct) {
        return Err(crate::api::bad("alert threshold: 50–99 %"));
    }
    st.db
        .set_config("alert_pct", &b.pct.to_string())
        .map_err(ise)?;
    tracing::info!("server resource alert threshold set to {}%", b.pct);
    Ok(ok())
}

#[derive(Deserialize)]
struct GoldenStreams {
    value: String,
}

/// Windows golden downloads at once (goldenram.rs queue): "auto" (from the server's link speed) or a number set by the
/// admin, 0 = no queue. POST /api/config/golden-streams {value}
async fn set_golden_streams(
    State(st): State<SharedState>,
    Json(b): Json<GoldenStreams>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let v = b.value.trim().to_ascii_lowercase();
    let max = crate::goldenram::MAX_STREAMS;
    if v != "auto" && !v.parse::<usize>().is_ok_and(|n| n <= max) {
        return Err(crate::api::bad(format!(
            "golden downloads at once: auto or 0–{max} (0 = no queue)"
        )));
    }
    st.db.set_config("golden_streams", &v).map_err(ise)?;
    crate::goldenram::refresh_limit(&st);
    tracing::info!("golden downloads at once: {v}");
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
    st.db
        .set_config("zram_reserve_mb", &mb.to_string())
        .map_err(ise)?;
    tracing::info!("zram RAM reserve set to {mb} MB");
    Ok(ok())
}

/// Current DHCP config (mode, parameters, optional behaviours) for the web to display.
/// (addresses in the pool, machines/devices that need one from it): registered machines without a fixed IP, plus the
/// other MACs holding a live lease. A room of 100 PCs does not fit the default .100–.200 with the router and phones.
pub(crate) fn pool_need(
    start: std::net::Ipv4Addr,
    end: std::net::Ipv4Addr,
    machines: &[crate::db::Machine],
    leases: &[crate::db::Lease],
    now: i64,
) -> (u32, usize) {
    let pool = u32::from(end)
        .checked_sub(u32::from(start))
        .map_or(0, |d| d + 1);
    let registered: std::collections::HashSet<String> =
        machines.iter().map(|m| m.mac.to_lowercase()).collect();
    let dynamic = machines
        .iter()
        .filter(|m| m.ip.as_deref().is_none_or(|ip| ip.trim().is_empty()))
        .count();
    let others = leases
        .iter()
        .filter(|l| {
            l.expires > now
                && !l.mac.starts_with("declined-")
                && !registered.contains(&l.mac.to_lowercase())
        })
        .count();
    (pool, dynamic + others)
}

async fn get_dhcp(State(st): State<SharedState>) -> Json<serde_json::Value> {
    let g = |k: &str, d: &str| st.db.get_config(k, d);
    let (pool_size, pool_need) = match dhcp::Cfg::load(&*st.db) {
        Ok(c) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            pool_need(
                c.start,
                c.end,
                &st.db.machines().unwrap_or_default(),
                &st.db.leases().unwrap_or_default(),
                now,
            )
        }
        Err(_) => (0, 0),
    };
    Json(serde_json::json!({
        "pool_size": pool_size,
        "pool_need": pool_need,
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
    for p in v
        .split([',', ' ', ';'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let ip = p
            .parse()
            .map_err(|_| format!("dns: {p:?} is not an IPv4 address"))?;
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    if out.len() > MAX_DNS {
        return Err(format!("dns: at most {MAX_DNS} servers"));
    }
    Ok(out
        .iter()
        .map(|ip| ip.to_string())
        .collect::<Vec<_>>()
        .join(","))
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
        "dhcp_iface" => {
            v.is_empty()
                || (v.len() <= 15
                    && v.chars()
                        .all(|c| c.is_ascii_alphanumeric() || ".-_@".contains(c)))
        }
        "dhcp_server_ip" => ipv4(v),
        // Only the DHCP server uses these (full mode checks them in set_dhcp) → blank is fine while it is off.
        "dhcp_subnet" | "dhcp_netmask" | "dhcp_range_start" | "dhcp_range_end" | "dhcp_gateway" => {
            v.is_empty() || ipv4(v)
        }
        "dhcp_dns" => v.is_empty() || v.split(',').all(|p| ipv4(p.trim())),
        "dhcp_lease" => dhcp::parse_lease(v).is_some(),
        "ipxe_signed" | "strict_reset" | "dhcp_rapid_commit" | "dhcp_ipxe_fast"
        | "dhcp_authoritative" | "dhcp_send_hostname" => v == "0" || v == "1",
        _ => true,
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{}: invalid value {v:?}",
            key.trim_start_matches("dhcp_")
        ))
    }
}

/// The DHCP range: both ends set, start ≤ end, both in the server's subnet (server IP + netmask).
fn range_ok(start: &str, end: &str, server: &str, mask: &str) -> Result<(), String> {
    use std::net::Ipv4Addr;
    let ip = |s: &str, what: &str| {
        s.trim()
            .parse::<Ipv4Addr>()
            .map(u32::from)
            .map_err(|_| format!("the DHCP server needs a valid {what}"))
    };
    let (s, e, srv) = (
        ip(start, "range start")?,
        ip(end, "range end")?,
        ip(server, "server IP")?,
    );
    let m = ip(
        if mask.trim().is_empty() {
            "255.255.255.0"
        } else {
            mask
        },
        "netmask",
    )?;
    if s > e {
        return Err(format!("range start {start} is after range end {end}"));
    }
    if s & m != srv & m || e & m != srv & m {
        return Err(format!(
            "range {start}-{end} is not in the server's subnet ({server} / {mask})"
        ));
    }
    Ok(())
}

/// Save the DHCP config + restart the built-in DHCP/TFTP listeners (dhcp.rs). Every field is validated FIRST; if any
/// is bad, nothing is saved (a half-saved bad config would keep DHCP/TFTP down after the next restart).
async fn set_dhcp(
    State(st): State<SharedState>,
    Json(b): Json<DhcpBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let dns = b
        .dns
        .as_deref()
        .map(normalize_dns)
        .transpose()
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
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
    let mode = b
        .mode
        .as_deref()
        .unwrap_or(&st.db.get_config("dhcp_mode", "off"))
        .to_string();
    let iface = b
        .iface
        .clone()
        .unwrap_or_else(|| st.db.get_config("dhcp_iface", ""));
    // A typo would be saved and take DHCP/TFTP down at the next start (binds fail): only interfaces this host has.
    if !iface.trim().is_empty()
        && !std::path::Path::new("/sys/class/net")
            .join(iface.trim())
            .exists()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("interface {:?} does not exist on this server", iface.trim()),
        ));
    }
    if mode == "full" && iface.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "the DHCP server needs a specific interface (leaving it blank binds every interface)"
                .into(),
        ));
    }
    // A lease without gateway/DNS takes every machine that gets it off the internet.
    let cur = |v: &Option<String>, k: &str| v.clone().unwrap_or_else(|| st.db.get_config(k, ""));
    if mode == "full"
        && (cur(&b.gateway, "dhcp_gateway").trim().is_empty()
            || cur(&dns, "dhcp_dns").trim().is_empty())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "the DHCP server needs a gateway and DNS (clients would get no internet)".into(),
        ));
    }
    let server = cur(&b.server_ip, "dhcp_server_ip");
    if mode == "full" {
        let mask = cur(&b.netmask, "dhcp_netmask");
        range_ok(
            &cur(&b.range_start, "dhcp_range_start"),
            &cur(&b.range_end, "dhcp_range_end"),
            &server,
            &mask,
        )
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    }
    // Boot scripts, the Host check (auth.rs) and DHCP option 54 all use it: an address this machine doesn't have
    // breaks every boot. Binding to it only works when it is ours.
    if b.server_ip.is_some() && std::net::UdpSocket::bind((server.trim(), 0)).is_err() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("server IP {server} is not an address of this server"),
        ));
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

async fn restart(st: &SharedState) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let status = dhcp::start(st).await.map_err(|e| {
        tracing::error!("network settings: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e)
    })?;
    tracing::info!("network settings applied: {status}");
    Ok(Json(
        serde_json::json!({"ok": true, "reloaded": true, "status": status}),
    ))
}

#[cfg(test)]
#[path = "../tests/unit/settings.rs"]
mod tests;
