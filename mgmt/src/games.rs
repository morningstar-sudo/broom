// games.rs — the games disks of the Windows machines: shared disks (<home>/games/<name>/games.img) the iSCSI daemon
// serves read-only (iscsid/games.rs), each mounted under its drive letter by broom-games.ps1 on the machines of its
// groups (none = every machine), each machine's writes in a differencing VHDX on its own SSD. Each disk can have one
// update machine: it gets that disk writable (its writes kept apart on the server); "save" makes them a new version at
// once, which each machine gets at its next boot (machines already playing keep theirs). The list lives in the config
// table (games_disks, JSON).
use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::SharedState;
use crate::api::{ApiError, bad, ise, ok};
use crate::iscsid::games::Config;
use crate::machines::{clean_groups, for_group, machine_at, machine_by_id, who};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/games", get(status))
        .route("/api/games/disk", post(upsert))
        .route("/api/games/delete", post(delete))
        .route("/api/games/update", post(set_update))
        .route("/api/games/cache", post(set_cache))
        .route("/api/games/save", post(save))
        .route("/api/games/discard", post(discard))
        .route("/api/games/for", get(for_client))
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Disk {
    pub name: String,
    pub size_gb: u64,
    pub letter: String,
    /// Groups whose machines get it; empty = every machine.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Its update machine (machine id).
    #[serde(default)]
    pub update: Option<i64>,
    /// Served from a copy in the server's RAM (the iSCSI daemon) instead of reading games.img.
    #[serde(default)]
    pub ram: bool,
}

pub(crate) fn disks(st: &SharedState) -> Vec<Disk> {
    serde_json::from_str(&st.db.get_config("games_disks", "[]")).unwrap_or_default()
}

pub(crate) fn put(st: &SharedState, d: &[Disk]) -> Result<(), ApiError> {
    st.db
        .set_config("games_disks", &serde_json::to_string(d).unwrap())
        .map_err(ise)
}

fn img(name: &str) -> PathBuf {
    crate::home().join("games").join(name).join("games.img")
}

/// IP of a machine: its fixed IP, else its DHCP lease.
fn ip_of(st: &SharedState, id: i64) -> Option<String> {
    let m = st.db.machines().ok()?.into_iter().find(|m| m.id == id)?;
    m.ip.or_else(|| {
        st.db
            .leases()
            .ok()?
            .into_iter()
            .find(|l| l.mac.eq_ignore_ascii_case(&m.mac))
            .and_then(|l| l.ip)
    })
}

fn configs(st: &SharedState) -> Vec<Config> {
    let base = st.db.get_config("iqn_base", "iqn.2026-01.local.broom");
    disks(st)
        .into_iter()
        .filter(|d| img(&d.name).exists())
        .map(|d| Config {
            iqn: format!("{base}:games-{}", d.name),
            path: img(&d.name).to_string_lossy().into(),
            update_ip: d.update.and_then(|id| ip_of(st, id)),
            ram: d.ram,
            reserve: crate::publish::ram_reserve(st),
            name: d.name,
        })
        .collect()
}

/// Why the games disks are not served (last sync), shown on the Images page; None = fine.
static LAST_ERR: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

/// Push the list to the iSCSI daemon (at start, on change, and every minute: an update machine's lease may move).
/// A new error is logged once as a warning (it repeats every minute until fixed). Blocking.
pub fn sync(st: &SharedState) -> Result<(), String> {
    let list = configs(st);
    let r = if list.is_empty() && crate::iscsi::games_status().is_none() {
        Ok(()) // nothing to serve, no daemon to tell
    } else {
        crate::iscsi::games_set(list)
    };
    let mut last = LAST_ERR.lock();
    match &r {
        Err(e) if last.as_deref() != Some(e.as_str()) => {
            tracing::warn!("games disks not served: {e}")
        }
        Ok(()) if last.is_some() => tracing::info!("games disks served again"),
        _ => {}
    }
    *last = r.as_ref().err().cloned();
    r
}

async fn sync_now(st: SharedState) -> Result<(), ApiError> {
    tokio::task::spawn_blocking(move || sync(&st))
        .await
        .map_err(ise)?
        .map_err(|e| (StatusCode::CONFLICT, e))
}

/// Two disks a machine could both get (their groups overlap; empty = every machine) can't share a drive letter.
pub(crate) fn clash(a: &Disk, b: &Disk) -> bool {
    a.name != b.name
        && a.letter == b.letter
        && (a.groups.is_empty()
            || b.groups.is_empty()
            || a.groups
                .iter()
                .any(|g| b.groups.iter().any(|x| x.eq_ignore_ascii_case(g))))
}

/// GET /api/games — the disks with what the daemon reports, the machines (update picker) and their groups.
async fn status(State(st): State<SharedState>) -> Json<serde_json::Value> {
    let info = tokio::task::spawn_blocking(crate::iscsi::games_status)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let machines = st.db.machines().unwrap_or_default();
    let list: Vec<_> = disks(&st)
        .into_iter()
        .map(|d| {
            let s = info.iter().find(|i| i.name == d.name);
            let upd = d.update.and_then(|id| machines.iter().find(|m| m.id == id));
            serde_json::json!({
                "update_name": upd.map(who),
                "update_ip": d.update.and_then(|id| ip_of(&st, id)),
                "status": s,
                "disk": d,
            })
        })
        .collect();
    let mut groups: Vec<String> = Vec::new();
    for g in machines.iter().filter_map(|m| m.grp.as_deref()) {
        if !groups.iter().any(|x| x.eq_ignore_ascii_case(g)) {
            groups.push(g.to_string());
        }
    }
    Json(serde_json::json!({
        "error": LAST_ERR.lock().clone(),
        "disks": list,
        "machines": machines.iter().map(|m| serde_json::json!({"id": m.id, "name": who(m), "grp": m.grp})).collect::<Vec<_>>(),
        "groups": groups,
    }))
}

#[derive(Deserialize)]
struct DiskBody {
    name: String,
    size_gb: u64,
    letter: String,
    #[serde(default)]
    groups: Vec<String>,
}

/// POST /api/games/disk {name, size_gb, letter, groups} — add a disk, or change one (same name). games.img is created
/// sparse and can only grow (the update machine extends its partitions to the new size on its next boot).
async fn upsert(
    State(st): State<SharedState>,
    Json(b): Json<DiskBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let name = b.name.trim().to_ascii_lowercase();
    if !(1..=24).contains(&name.len())
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(bad("name: 1-24 letters / digits / '-'"));
    }
    let letter = b.letter.trim().to_ascii_uppercase();
    if !(letter.len() == 1 && ('D'..='Z').contains(&letter.chars().next().unwrap())) {
        return Err(bad("drive letter: D to Z"));
    }
    let groups = clean_groups(&b.groups).map_err(bad)?;
    if b.size_gb == 0 || b.size_gb > 64 << 10 {
        return Err(bad("size: 1 GB to 64 TB"));
    }
    let mut list = disks(&st);
    let old = list.iter().position(|d| d.name == name);
    let d = Disk {
        name: name.clone(),
        size_gb: b.size_gb,
        letter,
        groups,
        update: old.and_then(|i| list[i].update),
        ram: old.is_some_and(|i| list[i].ram),
    };
    if let Some(o) = list.iter().find(|o| clash(&d, o)) {
        return Err(bad(format!(
            "drive {}: is already used by games disk {} for some of the same machines",
            d.letter, o.name
        )));
    }
    let p = img(&name);
    let (want, have) = (
        b.size_gb << 30,
        std::fs::metadata(&p).map_or(0, |m| m.len()),
    );
    if want < have {
        return Err(bad(format!(
            "games disk {name} is {} GB: it can only grow",
            have >> 30
        )));
    }
    if want > have {
        std::fs::create_dir_all(p.parent().unwrap()).map_err(ise)?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&p)
            .map_err(ise)?;
        f.set_len(want).map_err(ise)?;
        tracing::info!("games disk {name}: {} GB ({})", b.size_gb, p.display());
    }
    match old {
        Some(i) => list[i] = d,
        None => list.push(d),
    }
    put(&st, &list)?;
    sync_now(st).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

fn find(st: &SharedState, name: &str) -> Result<Disk, ApiError> {
    disks(st).into_iter().find(|d| d.name == name).ok_or((
        StatusCode::NOT_FOUND,
        format!("games disk {name} not found"),
    ))
}

/// POST /api/games/delete {name} — the disk and every game on it. Machines using it keep it until they reboot.
async fn delete(
    State(st): State<SharedState>,
    Json(b): Json<NameBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    find(&st, &b.name)?;
    let list: Vec<Disk> = disks(&st)
        .into_iter()
        .filter(|d| d.name != b.name)
        .collect();
    put(&st, &list)?;
    sync_now(st).await?;
    let dir = img(&b.name).parent().unwrap().to_path_buf();
    // Off the async threads: freeing a sparse disk of many TB (+ its versions) can take seconds.
    tokio::task::spawn_blocking(move || {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))
    })
    .await
    .map_err(|e| ise(e.to_string()))?
    .map_err(ise)?;
    tracing::info!("games disk {} deleted", b.name);
    Ok(ok())
}

#[derive(Deserialize)]
struct UpdateBody {
    name: String,
    /// The disk's update machine; None = end update mode.
    id: Option<i64>,
}

/// POST /api/games/update {name, id} — the machine whose next boot gets that disk writable.
async fn set_update(
    State(st): State<SharedState>,
    Json(b): Json<UpdateBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    find(&st, &b.name)?;
    let who_ = match b.id {
        Some(id) => Some(who(&machine_by_id(&st, id)?)),
        None => None,
    };
    let mut list = disks(&st);
    for d in list.iter_mut().filter(|d| d.name == b.name) {
        d.update = b.id;
    }
    put(&st, &list)?;
    match who_ {
        Some(w) => tracing::info!(
            "games disk {}: {w} is its update machine (from its next boot)",
            b.name
        ),
        None => tracing::info!("games disk {}: update mode off", b.name),
    }
    sync_now(st).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct CacheBody {
    name: String,
    ram: bool,
}

/// POST /api/games/cache {name, ram} — serve the disk from a copy in RAM (loaded in the background, read from the
/// file meanwhile; not enough RAM → stays on the file) or from games.img.
async fn set_cache(
    State(st): State<SharedState>,
    Json(b): Json<CacheBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    find(&st, &b.name)?;
    let mut list = disks(&st);
    for d in list.iter_mut().filter(|d| d.name == b.name) {
        d.ram = b.ram;
    }
    put(&st, &list)?;
    tracing::info!(
        "games disk {}: cache {}",
        b.name,
        if b.ram { "RAM" } else { "disk" }
    );
    sync_now(st).await?;
    Ok(ok())
}

/// POST /api/games/save {name} — the update becomes the disk's new version (the update machine must be off).
async fn save(
    State(st): State<SharedState>,
    Json(b): Json<NameBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    find(&st, &b.name)?;
    let name = b.name.clone();
    let msg = tokio::task::spawn_blocking(move || crate::iscsi::games_save(&name))
        .await
        .map_err(ise)?
        .map_err(|e| (StatusCode::CONFLICT, e))?;
    tracing::info!("games disk {}: {msg}", b.name);
    Ok(Json(serde_json::json!({"ok": true, "status": msg})))
}

/// POST /api/games/discard {name} — throw the disk's update away.
async fn discard(
    State(st): State<SharedState>,
    Json(b): Json<NameBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    find(&st, &b.name)?;
    let name = b.name.clone();
    tokio::task::spawn_blocking(move || crate::iscsi::games_discard(&name))
        .await
        .map_err(ise)?
        .map_err(|e| (StatusCode::CONFLICT, e))?;
    tracing::info!("games disk {}: update discarded", b.name);
    Ok(ok())
}

/// GET /api/games/for (public, broom-games.ps1) → one line per games disk of the machine at the peer IP (its group):
/// "<name> <iqn of the newest version> <letter> <1 = its update machine | 0>"; nothing = no games disk. The update flag
/// is decided by the peer IP, exactly as the daemon decides who may write.
async fn for_client(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> String {
    let ip = peer.ip().to_canonical().to_string();
    let Some(info) = tokio::task::spawn_blocking(crate::iscsi::games_status)
        .await
        .ok()
        .flatten()
    else {
        return String::new();
    };
    let grp = machine_at(&st, &ip).and_then(|m| m.grp);
    let cfgs = configs(&st);
    let mut out = String::new();
    for d in disks(&st)
        .iter()
        .filter(|d| for_group(&d.groups, grp.as_deref()))
    {
        let (Some(c), Some(i)) = (
            cfgs.iter().find(|c| c.name == d.name),
            info.iter().find(|i| i.name == d.name),
        ) else {
            continue;
        };
        let upd = c.update_ip.as_deref() == Some(ip.as_str());
        out.push_str(&format!(
            "{} {}.g{} {} {}\n",
            d.name, c.iqn, i.ver, d.letter, upd as u8
        ));
    }
    out
}

#[cfg(test)]
#[path = "../tests/unit/games.rs"]
mod tests;
