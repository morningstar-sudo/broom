// monitor.rs — M7. Machine list (registered + discovered via DHCP) + on/off (ICMP ping) + Wake-on-LAN.
// The web polls /api/status every 15 s → pings are never logged (only real events are).
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::db::Machine;
use crate::{wol, SharedState};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/wake", post(wake))
}

#[derive(Serialize)]
struct MachineStatus {
    mac: String,
    ip: Option<String>,
    hostname: Option<String>,
    online: bool,
    registered: bool,
    /// Registered machines only: id + license. The web admin is behind a login (auth.rs), so the full key is shown
    /// to the operator here; it is still never handed to a client (only broom-done fetches it, once, over the LAN).
    id: Option<i64>,
    license_key: Option<String>,
    license_tail: Option<String>,
    license_state: Option<String>,
    license_result: Option<String>,
    grp: Option<String>,
    image_id: Option<i64>,
    notes: Option<String>,
}

/// Machines = registered (machines table) + discovered via DHCP (leases table, dhcp.rs).
async fn status(State(st): State<SharedState>) -> Json<Vec<MachineStatus>> {
    // (ip, hostname, registered) by mac.
    let mut map: HashMap<String, (Option<String>, Option<String>, bool)> = HashMap::new();
    let mut rows: HashMap<String, Machine> = HashMap::new();

    // 1. Registered, from the DB.
    for m in st.db.machines().unwrap_or_default() {
        map.insert(m.mac.to_lowercase(), (m.ip.clone(), m.hostname.clone(), true));
        rows.insert(m.mac.to_lowercase(), m);
    }

    // 2. Discovered via the built-in DHCP server (active leases).
    for l in st.db.leases().unwrap_or_default().into_iter().filter(|l| !l.mac.starts_with("declined-")) {
        let (mac, ip, host) = (l.mac, l.ip, l.hostname);
        map.entry(mac.to_lowercase())
            .and_modify(|e| {
                if e.0.is_none() {
                    e.0 = ip.clone();
                }
            })
            .or_insert((ip, host, false));
    }

    // PARALLEL ping (JoinSet + spawn_blocking): 30 offline machines waiting 1 s in sequence = ~30 s
    // and blocks the runtime → concurrently it's ~1 s, without blocking a tokio worker.
    let entries: Vec<(String, Option<String>, Option<String>, bool)> =
        map.into_iter().map(|(mac, (ip, hostname, registered))| (mac, ip, hostname, registered)).collect();
    let mut set = tokio::task::JoinSet::new();
    for (i, (_, ip, _, _)) in entries.iter().enumerate() {
        let ip = ip.clone();
        set.spawn_blocking(move || (i, ip.as_deref().is_some_and(ping)));
    }
    let mut online = vec![false; entries.len()];
    while let Some(res) = set.join_next().await {
        if let Ok((i, on)) = res {
            online[i] = on;
        }
    }
    let mut out: Vec<MachineStatus> = entries
        .into_iter()
        .zip(online)
        .map(|((mac, ip, hostname, registered), online)| {
            let m = rows.remove(&mac);
            MachineStatus {
                id: m.as_ref().map(|m| m.id),
                license_key: m.as_ref().and_then(|m| m.license_key.clone()),
                license_tail: m.as_ref().and_then(|m| m.license_tail.clone()),
                license_state: m.as_ref().and_then(|m| m.license_state.clone()),
                license_result: m.as_ref().and_then(|m| m.license_result.clone()),
                grp: m.as_ref().and_then(|m| m.grp.clone()),
                image_id: m.as_ref().and_then(|m| m.image_id),
                notes: m.and_then(|m| m.notes),
                mac,
                ip,
                hostname,
                online,
                registered,
            }
        })
        .collect();
    // Registered first, then by hostname/ip.
    out.sort_by(|a, b| {
        b.registered
            .cmp(&a.registered)
            .then(a.hostname.cmp(&b.hostname))
            .then(a.ip.cmp(&b.ip))
    });
    Json(out)
}

/// One ICMP echo over a raw socket (root) — no iputils `ping` needed. true = reply within 1 s. Silent.
fn ping(ip: &str) -> bool {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::io::Read;
    use std::time::{Duration, Instant};
    let Ok(addr) = ip.parse::<std::net::Ipv4Addr>() else { return false };
    let Ok(mut s) = Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4)) else { return false };
    // Identifier distinguishes our echo from other pings in flight (raw sockets see every ICMP packet).
    let id = (std::process::id() as u16) ^ (u32::from(addr) as u16);
    let mut pkt = [8, 0, 0, 0, (id >> 8) as u8, id as u8, 0, 1, b'b', b'r', b'o', b'o', b'm', 0, 0, 0];
    let ck = icmp_checksum(&pkt);
    pkt[2..4].copy_from_slice(&ck.to_be_bytes());
    if s.send_to(&pkt, &std::net::SocketAddrV4::new(addr, 0).into()).is_err() {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut buf = [0u8; 1500];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || s.set_read_timeout(Some(left)).is_err() {
            return false;
        }
        let Ok(n) = s.read(&mut buf) else { return false };
        // Raw IPv4 socket: IP header first (source at 12..16), then ICMP.
        let hl = (buf[0] & 0x0f) as usize * 4;
        if n >= hl + 8 && buf[12..16] == addr.octets() && buf[hl] == 0 && buf[hl + 4..hl + 6] == id.to_be_bytes() {
            return true;
        }
    }
}

/// RFC 1071 internet checksum.
fn icmp_checksum(b: &[u8]) -> u16 {
    let mut s: u32 = b.chunks(2).map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32).sum();
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

#[cfg(test)]
#[test]
fn checksum_rfc1071() {
    // Echo request id=1 seq=1, no payload: 0x0800 + 0x0001 + 0x0001 = 0x0802 → !0x0802 = 0xf7fd.
    assert_eq!(icmp_checksum(&[8, 0, 0, 0, 0, 1, 0, 1]), 0xf7fd);
    // A packet with its checksum filled in sums to zero.
    assert_eq!(icmp_checksum(&[8, 0, 0xf7, 0xfd, 0, 1, 0, 1]), 0);
}

/// Real ICMP (root): `cargo test -- --ignored ping_live`.
#[cfg(test)]
#[test]
#[ignore]
fn ping_live() {
    assert!(ping("127.0.0.1"));
    assert!(!ping("192.0.2.1")); // TEST-NET-1: never answers
}

#[derive(Deserialize)]
struct WakeBody {
    mac: String,
}

async fn wake(Json(b): Json<WakeBody>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    wol::wake(&b.mac).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    tracing::info!(mac = %b.mac, "wake-on-LAN sent");
    Ok(Json(serde_json::json!({"ok": true})))
}
