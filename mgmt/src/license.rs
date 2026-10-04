// license.rs — Windows license keys (retail, one per machine) + Windows boot reports.
// broom-done.ps1 (when base.vhdx is built) asks POST /api/license. The server picks the key ONLY from the TCP peer IP
// → registered machine; nothing in the request is trusted. Handed out once (armed → sent), and only shortly after a
// PXE boot: the guest user is an Administrator and could otherwise fetch it any time. Re-arm on the web for one more.
use axum::{
    extract::{ConnectInfo, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use std::net::SocketAddr;

use crate::api::{ise, ok, ApiError};
use crate::db::Machine;
use crate::machines::{machine_by_id, who};
use crate::SharedState;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/machines/license", post(set_license))
        .route("/api/machines/license/rearm", post(rearm_license))
        .route("/api/license", post(license))
        .route("/api/license/result", post(license_result))
        .route("/api/booted", post(booted))
}

/// A key is only handed out this long after the machine's last PXE boot (/boot/start or the stage's driver query):
/// broom-done asks while base is being built right after the stage, not hours into a guest's session.
const LICENSE_WINDOW_S: u64 = 30 * 60;
/// A Windows boot reported more than this after the last PXE boot = Windows started from the SSD without the stage.
const BOOT_WINDOW_S: u64 = 15 * 60;

/// MAC of the host at `ip`: the kernel's ARP entry (it just talked to us), else its DHCP lease or static IP.
fn mac_at(st: &SharedState, ip: &str) -> Option<String> {
    arp_mac(ip)
        .or_else(|| st.db.leases().ok()?.into_iter().find(|l| l.ip.as_deref() == Some(ip)).map(|l| l.mac))
        .or_else(|| st.db.machines().ok()?.into_iter().find(|m| m.ip.as_deref() == Some(ip)).map(|m| m.mac))
        .map(|m| m.to_lowercase())
}

/// One report per IP per minute (public endpoint).
static BOOTED_SEEN: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> = std::sync::LazyLock::new(Default::default);

/// POST /api/booted — Windows (BroomBootOrder task) reports each boot. No PXE boot of that machine just before →
/// it started Windows from the SSD without the stage (cable out, server down, boot order changed): the session was
/// NOT reset → warning in the log + flag on the Machines page until its next PXE boot.
async fn booted(State(st): State<SharedState>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> StatusCode {
    let (ip, now) = (peer.ip().to_string(), crate::now_secs());
    {
        let mut seen = BOOTED_SEEN.lock().unwrap();
        seen.retain(|_, t| now.saturating_sub(*t) < 60);
        if seen.insert(ip.clone(), now).is_some() {
            return StatusCode::TOO_MANY_REQUESTS;
        }
    }
    let Some(mac) = mac_at(&st, &ip) else { return StatusCode::NO_CONTENT };
    let recent = st.pxe_seen.lock().unwrap().get(&mac).is_some_and(|t| now.saturating_sub(*t) <= BOOT_WINDOW_S);
    if !recent {
        let name = st.db.machines().unwrap_or_default().into_iter().find(|m| m.mac.eq_ignore_ascii_case(&mac)).map(|m| who(&m));
        tracing::warn!(
            "machine {} ({mac}, {ip}) started Windows WITHOUT a PXE boot just before — this session was NOT reset",
            name.as_deref().unwrap_or(&mac)
        );
        st.not_reset.lock().unwrap().insert(mac, now);
    }
    StatusCode::NO_CONTENT
}

pub(crate) const KEY_RULE: &str = "License key: 25 letters/digits as XXXXX-XXXXX-XXXXX-XXXXX-XXXXX";

/// Windows product key format. Keep in sync with keyOk() in static/app.js.
pub(crate) fn key_ok(k: &str) -> bool {
    k.len() == 29
        && k.split('-').count() == 5
        && k.split('-').all(|g| g.len() == 5 && g.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
}

/// The registered machine behind a client IP, for the license endpoints. Only a machine's OWN bound IP counts —
/// NOT a DHCP lease (an OFFER hold is trivially spoofed). So a machine that should receive a key must have a fixed
/// IP set (Devices page). `arp` = the peer's MAC as the kernel sees it: it must be there and match the machine's
/// MAC — a machine on the LAN that just opened a TCP connection always has an entry; none = a routed peer (another
/// subnet) → refused (raises the bar; ARP can still be faked).
fn machine_by_ip<'a>(ip: &str, machines: &'a [Machine], arp: Option<&str>) -> Option<&'a Machine> {
    let m = machines.iter().find(|m| m.ip.as_deref() == Some(ip))?;
    match arp {
        Some(mac) if !m.mac.eq_ignore_ascii_case(mac) => None, // IP right, MAC wrong → spoofed
        Some(_) => Some(m),
        None => None,
    }
}

/// The MAC the kernel has for `ip` in the ARP cache (/proc/net/arp), lower-case `aa:bb:…`. None = no entry
/// (not on this LAN, or not talked to the server — machine_by_ip refuses then).
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

#[derive(Deserialize)]
struct LicenseBody {
    id: i64,
    key: String,
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

/// POST /api/license (Windows, broom-done.ps1) → the key of the machine at the peer IP, once, shortly after its PXE
/// boot. 403 otherwise.
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
    let pxe = st.pxe_seen.lock().unwrap().get(&m.mac.to_lowercase()).copied();
    if !pxe.is_some_and(|t| crate::now_secs().saturating_sub(t) <= LICENSE_WINDOW_S) {
        return refuse(format!("{}: no PXE boot in the last {} min — keys only go to a base being built", who(m), LICENSE_WINDOW_S / 60));
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
    assert_eq!(machine_by_ip("10.0.0.51", &ms, None).map(|m| m.id), None); // bound IP, no ARP entry (routed peer) → refused
    assert_eq!(machine_by_ip("10.0.0.51", &ms, Some("aa:00:00:00:00:01")).map(|m| m.id), Some(1)); // ARP matches
    assert_eq!(machine_by_ip("10.0.0.51", &ms, Some("bb:bb:bb:bb:bb:bb")).map(|m| m.id), None); // ARP MAC mismatch → spoofed
    assert_eq!(machine_by_ip("10.0.0.102", &ms, None).map(|m| m.id), None); // a lease IP is not accepted
    assert_eq!(machine_by_ip("10.0.0.200", &ms, None).map(|m| m.id), None);
}
