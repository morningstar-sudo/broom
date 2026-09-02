// monitor.rs — M7 mgmt-monitor. Giám sát on/off (ping) + WOL + reboot.
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Command;

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
}

/// Máy trạm = đã đăng ký (bảng machines) + phát hiện qua DHCP lease (dnsmasq).
/// ponytail: ping tuần tự, 10–30 máy ok.
async fn status(State(st): State<SharedState>) -> Json<Vec<MachineStatus>> {
    // (ip, hostname, registered) theo mac.
    let mut map: HashMap<String, (Option<String>, Option<String>, bool)> = HashMap::new();

    // 1. Đã đăng ký từ DB.
    {
        let conn = st.db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT mac, ip, hostname FROM machines")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .unwrap();
        for (mac, ip, hostname) in rows.filter_map(|r| r.ok()) {
            map.insert(mac.to_lowercase(), (ip, hostname, true));
        }
    }

    // 2. Phát hiện qua DHCP lease (chỉ full DHCP mode có file này).
    if let Ok(txt) = std::fs::read_to_string("/var/lib/misc/dnsmasq.leases") {
        for line in txt.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 4 {
                let mac = f[1].to_lowercase();
                let ip = Some(f[2].to_string());
                let host = if f[3] != "*" { Some(f[3].to_string()) } else { None };
                map.entry(mac)
                    .and_modify(|e| {
                        if e.0.is_none() {
                            e.0 = ip.clone();
                        }
                    })
                    .or_insert((ip, host, false));
            }
        }
    }

    // Ping SONG SONG (JoinSet + spawn_blocking): 30 máy offline chờ -W 1 tuần tự = ~30s
    // và chẹn cả runtime → chạy đồng thời còn ~1s, không chẹn tokio worker.
    let entries: Vec<(String, Option<String>, Option<String>, bool)> = map
        .into_iter()
        .map(|(mac, (ip, hostname, registered))| (mac, ip, hostname, registered))
        .collect();
    let mut set = tokio::task::JoinSet::new();
    for (i, (_, ip, _, _)) in entries.iter().enumerate() {
        let ip = ip.clone();
        set.spawn_blocking(move || (i, ip.as_deref().map(ping).unwrap_or(false)));
    }
    let mut online = vec![false; entries.len()];
    while let Some(res) = set.join_next().await {
        if let Ok((i, on)) = res {
            online[i] = on;
        }
    }
    let mut out: Vec<MachineStatus> = entries
        .into_iter()
        .enumerate()
        .map(|(i, (mac, ip, hostname, registered))| MachineStatus {
            mac,
            ip,
            hostname,
            online: online[i],
            registered,
        })
        .collect();
    // Đã đăng ký lên trước, rồi theo hostname/ip.
    out.sort_by(|a, b| {
        b.registered
            .cmp(&a.registered)
            .then(a.hostname.cmp(&b.hostname))
            .then(a.ip.cmp(&b.ip))
    });
    Json(out)
}

fn ping(ip: &str) -> bool {
    Command::new("ping")
        .args(["-c", "1", "-W", "1", ip])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[derive(Deserialize)]
struct WakeBody {
    mac: String,
}

async fn wake(Json(b): Json<WakeBody>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    wol::wake(&b.mac).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::json!({"ok": true})))
}
