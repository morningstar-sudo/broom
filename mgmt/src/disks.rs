// disks.rs — a client machine's local disks. The stage reports them at every boot (POST /api/disks/report) and gets
// back what the server decided: which disk is the Broom disk (chosen on the web, by a stable disk id — sda / nvme0n1
// names change between boots) and whether the machine's other disks are hidden from the guest. Kept in the config
// table, one key per registered machine ("disks:<mac>"): a room booting at once writes many keys, never one shared
// value.
// Hidden = offline in Windows (broom-watch.ps1) / removed from the kernel on a Linux image (the iSCSI hook): keeps
// the guest's writes off a disk that would keep them across boots. Not a security boundary: the guest is an admin.
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::SharedState;
use crate::api::{ApiError, bad, ise, ok};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/disks/report", post(report))
        .route("/api/disks", get(list))
        .route("/api/disks/broom", post(set_broom))
        .route("/api/disks/hide", post(set_hide))
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Disk {
    /// Stable id: WWN, else serial (udev), else "<dev>-<sectors>".
    pub id: String,
    pub dev: String,
    pub size: u64,
    pub model: String,
    /// Holds the Broom partitions now.
    pub broom: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct Entry {
    /// Unix time of the last report.
    #[serde(default)]
    seen: u64,
    #[serde(default)]
    disks: Vec<Disk>,
    /// Disk id chosen on the web as the Broom disk.
    #[serde(default)]
    broom: Option<String>,
    /// Hide the other disks: Some = set for this machine, None = as its group.
    #[serde(default)]
    hide: Option<bool>,
}

fn key(mac: &str) -> String {
    format!("disks:{}", mac.to_lowercase())
}

fn load(st: &SharedState, mac: &str) -> Entry {
    serde_json::from_str(&st.db.get_config(&key(mac), "{}")).unwrap_or_default()
}

fn store(st: &SharedState, mac: &str, e: &Entry) -> Result<(), String> {
    st.db
        .set_config(&key(mac), &serde_json::to_string(e).unwrap())
}

fn mac_ok(mac: &str) -> bool {
    mac.len() == 17
        && mac
            .split(':')
            .all(|b| b.len() == 2 && b.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The stage's lines `<id> <bytes> <broom 0|1> <dev> <model…>`; malformed lines skipped, at most 32 disks.
pub fn parse_report(body: &str) -> Vec<Disk> {
    let id_ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 128
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c))
    };
    body.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (id, size, broom, dev) = (f.next()?, f.next()?, f.next()?, f.next()?);
            let model: String = f
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .filter(|c| !c.is_control())
                .take(64)
                .collect();
            if !id_ok(id) || !id_ok(dev) {
                return None;
            }
            Some(Disk {
                id: id.to_string(),
                dev: dev.to_string(),
                size: size.parse().ok()?,
                model,
                broom: broom == "1",
            })
        })
        .take(32)
        .collect()
}

pub fn hidden_groups(st: &SharedState) -> Vec<String> {
    serde_json::from_str(&st.db.get_config("hide_disks_groups", "[]")).unwrap_or_default()
}

/// Hide the other disks of machine `mac`: its own setting, else its group's.
pub fn hide_for(st: &SharedState, mac: &str) -> bool {
    load(st, mac).hide.unwrap_or_else(|| group_hides(st, mac))
}

fn group_hides(st: &SharedState, mac: &str) -> bool {
    let grp = st
        .db
        .machines()
        .unwrap_or_default()
        .into_iter()
        .find(|m| m.mac.eq_ignore_ascii_case(mac))
        .and_then(|m| m.grp);
    grp.is_some_and(|g| hidden_groups(st).iter().any(|h| h.eq_ignore_ascii_case(&g)))
}

#[derive(Deserialize)]
struct MacQ {
    mac: String,
}

/// POST /api/disks/report?mac= (public: the Windows stage, every boot) — body: its disks, one per line. Answers
/// `broom=<id>` (the disk chosen on the web, empty = none) and `hide=0|1`.
async fn report(
    State(st): State<SharedState>,
    Query(q): Query<MacQ>,
    body: String,
) -> Result<String, ApiError> {
    let mac = q.mac.to_lowercase();
    if !mac_ok(&mac) {
        return Err(bad("bad mac"));
    }
    // Open to anyone: only a registered machine is stored (made-up MACs can't grow the table); any other one gets
    // the defaults — no Broom disk chosen, nothing hidden.
    let registered = st
        .db
        .machines()
        .map_err(ise)?
        .iter()
        .any(|m| m.mac.eq_ignore_ascii_case(&mac));
    if !registered {
        return Ok("broom=\nhide=0\n".into());
    }
    let mut e = load(&st, &mac);
    e.disks = parse_report(&body);
    e.seen = crate::now_secs();
    store(&st, &mac, &e).map_err(ise)?;
    Ok(format!(
        "broom={}\nhide={}\n",
        e.broom.as_deref().unwrap_or(""),
        e.hide.unwrap_or_else(|| group_hides(&st, &mac)) as u8
    ))
}

/// GET /api/disks?mac= — the machine's disks as last reported + its settings.
async fn list(State(st): State<SharedState>, Query(q): Query<MacQ>) -> Json<serde_json::Value> {
    let e = load(&st, &q.mac);
    Json(serde_json::json!({
        "disks": e.disks, "seen": e.seen, "broom": e.broom,
        "hide": e.hide, "group_hide": group_hides(&st, &q.mac),
    }))
}

#[derive(Deserialize)]
struct BroomReq {
    mac: String,
    id: String,
}

/// POST /api/disks/broom {mac, id} — make disk `id` the machine's Broom disk at its next boot (WIPES it; the old one
/// loses its Broom partitions). A registered machine and a disk it reported only.
async fn set_broom(
    State(st): State<SharedState>,
    Json(b): Json<BroomReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let m = st
        .db
        .machines()
        .map_err(ise)?
        .into_iter()
        .find(|m| m.mac.eq_ignore_ascii_case(&b.mac))
        .ok_or_else(|| bad("register the machine first (Machines page)"))?;
    let mut e = load(&st, &b.mac);
    let d = e
        .disks
        .iter()
        .find(|d| d.id == b.id)
        .ok_or_else(|| bad("this machine has not reported that disk"))?;
    tracing::warn!(
        "disks: {} — Broom disk set to {} ({} {} GB): it is ERASED at the next boot",
        crate::machines::who(&m),
        d.dev,
        d.model,
        d.size >> 30
    );
    e.broom = Some(b.id);
    store(&st, &b.mac, &e).map_err(ise)?;
    Ok(ok())
}

#[derive(Deserialize)]
struct HideReq {
    mac: String,
    /// None = as its group.
    hide: Option<bool>,
}

/// POST /api/disks/hide {mac, hide: true|false|null}.
async fn set_hide(
    State(st): State<SharedState>,
    Json(b): Json<HideReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !mac_ok(&b.mac.to_lowercase()) {
        return Err(bad("bad mac"));
    }
    let mut e = load(&st, &b.mac);
    e.hide = b.hide;
    store(&st, &b.mac, &e).map_err(ise)?;
    Ok(ok())
}

/// Set the groups whose machines hide their other disks (Groups page: edit / rename / delete).
pub fn set_hidden_groups(st: &SharedState, groups: &[String]) -> Result<(), String> {
    st.db
        .set_config("hide_disks_groups", &serde_json::to_string(groups).unwrap())
}

#[cfg(test)]
#[path = "../tests/unit/disks.rs"]
mod tests;
