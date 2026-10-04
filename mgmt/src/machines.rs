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
