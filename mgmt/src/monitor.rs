// monitor.rs — Machine list (registered + discovered via DHCP) + on/off (ICMP ping) + Wake-on-LAN.
// The web polls /api/status every 15 s → pings are never logged (only real events are).
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::db::Machine;
use crate::{SharedState, wol};

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
    /// Registered machines only: id + license state and the key's last 5 characters. The full key is not in this list
    /// (polled every few seconds); the machine detail (devices.rs) shows it to the operator on request.
    id: Option<i64>,
    license_tail: Option<String>,
    license_state: Option<String>,
    license_result: Option<String>,
    grp: Option<String>,
    image_id: Option<i64>,
    notes: Option<String>,
    /// Started Windows without a PXE boot just before (session not reset) — unix time, until the next PXE boot.
    not_reset: Option<u64>,
    /// SSD almost full during the session: (bytes free, unix time), until the next PXE boot.
    ssd_low: Option<(u64, u64)>,
    /// Its remote agent is connected (Remote page).
    remote: bool,
    /// Hardware health the agent last reported (metrics.rs); None = no recent reading.
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics: Option<crate::metrics::Metrics>,
}

/// Machines = registered (machines table) + discovered via DHCP (leases table, dhcp.rs) or a PXE boot (pxe_ip).
async fn status(State(st): State<SharedState>) -> Json<Vec<MachineStatus>> {
    // (ip, hostname, registered) by mac.
    let mut map: HashMap<String, (Option<String>, Option<String>, bool)> = HashMap::new();
    let mut rows: HashMap<String, Machine> = HashMap::new();

    // 1. Registered, from the DB.
    for m in st.db.machines().unwrap_or_default() {
        map.insert(
            m.mac.to_lowercase(),
            (m.ip.clone(), m.hostname.clone(), true),
        );
        rows.insert(m.mac.to_lowercase(), m);
    }

    // 2. Discovered via the built-in DHCP server (active leases).
    for l in st
        .db
        .leases()
        .unwrap_or_default()
        .into_iter()
        .filter(|l| !l.mac.starts_with("declined-"))
    {
        let (mac, ip, host) = (l.mac, l.ip, l.hostname);
        map.entry(mac.to_lowercase())
            .and_modify(|e| {
                if e.0.is_none() {
                    e.0 = ip.clone();
                }
            })
            .or_insert((ip, host, false));
    }

    // 3. Seen PXE-booting (boot menu choice) — how new machines show up when the LAN's own DHCP serves them.
    let seen = st.pxe_ip.lock().clone();
    for (mac, ip) in seen {
        map.entry(mac)
            .and_modify(|e| {
                if e.0.is_none() {
                    e.0 = Some(ip.clone());
                }
            })
            .or_insert((Some(ip), None, false));
    }

    // PARALLEL ping (JoinSet + spawn_blocking): 30 offline machines waiting in sequence = 30 × PING_WAIT
    // and blocks the runtime → concurrently it is one PING_WAIT, without blocking a tokio worker.
    let entries: Vec<(String, Option<String>, Option<String>, bool)> = map
        .into_iter()
        .map(|(mac, (ip, hostname, registered))| (mac, ip, hostname, registered))
        .collect();
    // A machine running the remote agent (vnc.rs) holds a connection to us and reopens it within 5 s of every boot, so
    // that connection is its presence signal — no ping needed for it. Ping covers the rest: Linux clients, and Windows
    // machines before their agent is up (a new machine, or one still building its base).
    // At most 32 pings at once: each holds a blocking thread up to PING_WAIT, and the list can hold many machines (it
    // also shows every MAC seen PXE-booting) — they must not crowd out logins and jobs on the blocking pool. Each IP
    // once, however many MACs list it.
    let ips: std::collections::HashSet<String> = entries
        .iter()
        .filter(|e| !crate::vnc::connected(&e.0))
        .filter_map(|e| e.1.clone())
        .collect();
    let mut set = tokio::task::JoinSet::new();
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(32));
    for ip in ips {
        let slots = slots.clone();
        set.spawn(async move {
            let _slot = slots.acquire_owned().await;
            let on = tokio::task::spawn_blocking({
                let ip = ip.clone();
                move || ping(&ip)
            })
            .await
            .unwrap_or(false);
            (ip, on)
        });
    }
    let mut up = std::collections::HashSet::new();
    while let Some(res) = set.join_next().await {
        if let Ok((ip, true)) = res {
            up.insert(ip);
        }
    }
    // Online = its agent is connected, or (no agent) it answered a ping.
    let online: Vec<bool> = entries
        .iter()
        .map(|e| crate::vnc::connected(&e.0) || e.1.as_ref().is_some_and(|ip| up.contains(ip)))
        .collect();
    let not_reset = st.not_reset.lock().clone();
    let ssd_low = st.ssd_low.lock().clone();
    let mut out: Vec<MachineStatus> = entries
        .into_iter()
        .zip(online)
        .map(|((mac, ip, hostname, registered), online)| {
            let m = rows.remove(&mac);
            MachineStatus {
                id: m.as_ref().map(|m| m.id),
                license_tail: m.as_ref().and_then(|m| m.license_tail.clone()),
                license_state: m.as_ref().and_then(|m| m.license_state.clone()),
                license_result: m.as_ref().and_then(|m| m.license_result.clone()),
                grp: m.as_ref().and_then(|m| m.grp.clone()),
                image_id: m.as_ref().and_then(|m| m.image_id),
                notes: m.and_then(|m| m.notes),
                not_reset: not_reset.get(&mac).copied(),
                ssd_low: ssd_low.get(&mac).copied(),
                remote: crate::vnc::connected(&mac),
                metrics: crate::metrics::get(&mac),
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

/// How long a ping waits for its reply: a LAN answers in under a millisecond; an offline machine holds the whole
/// status call this long.
const PING_WAIT: std::time::Duration = std::time::Duration::from_millis(300);

/// One ICMP echo over a raw socket (root) — no iputils `ping` needed. true = reply within PING_WAIT. Silent.
fn ping(ip: &str) -> bool {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::io::Read;
    use std::time::Instant;
    let Ok(addr) = ip.parse::<std::net::Ipv4Addr>() else {
        return false;
    };
    let Ok(mut s) = Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4)) else {
        return false;
    };
    // Identifier distinguishes our echo from other pings in flight (raw sockets see every ICMP packet).
    let id = (std::process::id() as u16) ^ (u32::from(addr) as u16);
    let mut pkt = [
        8,
        0,
        0,
        0,
        (id >> 8) as u8,
        id as u8,
        0,
        1,
        b'b',
        b'r',
        b'o',
        b'o',
        b'm',
        0,
        0,
        0,
    ];
    let ck = icmp_checksum(&pkt);
    pkt[2..4].copy_from_slice(&ck.to_be_bytes());
    if s.send_to(&pkt, &std::net::SocketAddrV4::new(addr, 0).into())
        .is_err()
    {
        return false;
    }
    let deadline = Instant::now() + PING_WAIT;
    let mut buf = [0u8; 1500];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || s.set_read_timeout(Some(left)).is_err() {
            return false;
        }
        let Ok(n) = s.read(&mut buf) else {
            return false;
        };
        // Raw IPv4 socket: IP header first (source at 12..16), then ICMP.
        let hl = (buf[0] & 0x0f) as usize * 4;
        if n >= hl + 8
            && buf[12..16] == addr.octets()
            && buf[hl] == 0
            && buf[hl + 4..hl + 6] == id.to_be_bytes()
        {
            return true;
        }
    }
}

/// RFC 1071 internet checksum.
fn icmp_checksum(b: &[u8]) -> u16 {
    let mut s: u32 = b
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32)
        .sum();
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

#[derive(Deserialize)]
struct WakeBody {
    mac: String,
}

async fn wake(
    State(st): State<SharedState>,
    Json(b): Json<WakeBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    wol::wake(&*st.db, &b.mac).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    tracing::info!(mac = %b.mac, "wake-on-LAN sent");
    Ok(Json(serde_json::json!({"ok": true})))
}

#[cfg(test)]
#[path = "../tests/unit/monitor.rs"]
mod tests;
