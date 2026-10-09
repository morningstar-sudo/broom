// machines.rs — the Machines table (MAC / IP / hostname / group / image) API used by the Machines page; the Devices
// page (devices.rs) edits the same rows in bulk. License keys: license.rs. Server settings: settings.rs.
use axum::{
    extract::State,
    http::StatusCode,
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::api::{ise, ApiError};
use crate::db::Machine;
use crate::SharedState;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/machines", post(add)) // Register (Machines + Devices pages)
}

pub(crate) fn who(m: &Machine) -> String {
    m.hostname.clone().unwrap_or_else(|| m.mac.clone())
}

/// The registered machine behind a client IP (its MAC from ARP / lease / fixed IP). Not proof of identity (ARP can be
/// faked): for choosing what to offer, not for handing out secrets (license.rs is stricter).
pub(crate) fn machine_at(st: &SharedState, ip: &str) -> Option<Machine> {
    let mac = crate::license::mac_at(st, ip)?;
    st.db.machines().ok()?.into_iter().find(|m| m.mac.eq_ignore_ascii_case(&mac))
}

/// A resource limited to `groups` (empty = every machine, registered or not) is for a machine of group `grp`.
pub(crate) fn for_group(groups: &[String], grp: Option<&str>) -> bool {
    groups.is_empty() || grp.is_some_and(|g| groups.iter().any(|x| x.eq_ignore_ascii_case(g)))
}

/// "VIP, Thuong ,vip" → ["VIP", "Thuong"]: trimmed, no empties, no case-insensitive duplicates; each a valid group
/// name (drivers::group_ok, like a machine's group), at most 50.
pub(crate) fn clean_groups(v: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for g in v.iter().flat_map(|s| s.split(',')).map(str::trim).filter(|g| !g.is_empty()) {
        if !crate::drivers::group_ok(g) {
            return Err(format!("group {g:?}: letters/digits/_/-, max 32"));
        }
        if !out.iter().any(|x| x.eq_ignore_ascii_case(g)) {
            out.push(g.to_string());
        }
    }
    if out.len() > 50 {
        return Err("at most 50 groups".into());
    }
    Ok(out)
}

pub(crate) fn machine_by_id(st: &SharedState, id: i64) -> Result<Machine, ApiError> {
    st.db.machines().map_err(ise)?.into_iter().find(|m| m.id == id).ok_or((StatusCode::NOT_FOUND, format!("machine {id} not found")))
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
    crate::devices::ip_free(&m, &st.db.leases().map_err(ise)?, crate::now_secs() as i64).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let id = st.db.add_machine(&m.mac, m.ip.as_deref(), m.hostname.as_deref()).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    tracing::info!("machine registered - mac {} - ip {} - hostname {}", m.mac, m.ip.as_deref().unwrap_or("-"), m.hostname.as_deref().unwrap_or("-"));
    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}
