// groups.rs — the Groups page: machine groups and what each one gets. A group is a name, kept where it is used: a
// machine's group (one per machine), and the groups of each image / games disk (none = every machine) and driver
// package. Groups created on the page with nothing in them yet are kept in the config table (groups, JSON).
// Changes made here go through the same rules as on each resource's own page: a driver change re-arms the license
// keys of the machines whose driver set changes (their base is rebuilt), a games disk change goes to the iSCSI daemon.
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::api::{bad, ise, ok, ApiError};
use crate::machines::who;
use crate::SharedState;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/groups", get(list))
        .route("/api/groups/create", post(create))
        .route("/api/groups/rename", post(rename))
        .route("/api/groups/delete", post(delete))
        .route("/api/groups/set", post(set))
}

fn saved(st: &SharedState) -> Vec<String> {
    serde_json::from_str(&st.db.get_config("groups", "[]")).unwrap_or_default()
}

fn save(st: &SharedState, v: &[String]) -> Result<(), ApiError> {
    st.db.set_config("groups", &serde_json::to_string(v).unwrap()).map_err(ise)
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn name_ok(name: &str) -> Result<String, ApiError> {
    let n = name.trim();
    if crate::drivers::group_ok(n) { Ok(n.to_string()) } else { Err(bad(format!("group {n:?}: letters/digits/_/-, max 32"))) }
}

/// Every group in use or created here, one spelling each, sorted.
fn all(st: &SharedState) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let mut add = |g: &str| {
        if !g.is_empty() && !v.iter().any(|x| same(x, g)) {
            v.push(g.to_string());
        }
    };
    saved(st).iter().for_each(|g| add(g));
    st.db.machines().unwrap_or_default().iter().filter_map(|m| m.grp.as_deref()).for_each(&mut add);
    st.db.images().unwrap_or_default().iter().flat_map(|i| i.groups.iter()).for_each(|g| add(g));
    crate::games::disks(st).iter().flat_map(|d| d.groups.iter()).for_each(|g| add(g));
    st.db.drivers().unwrap_or_default().iter().flat_map(|d| d.groups.iter()).for_each(|g| add(g));
    v.sort_by_key(|g| g.to_ascii_lowercase());
    v
}

/// GET /api/groups — the groups, and every machine / image / games disk / driver package with its groups (the page
/// works out each group's connections).
async fn list(State(st): State<SharedState>) -> Json<serde_json::Value> {
    let machines: Vec<_> = st.db.machines().unwrap_or_default().iter().map(|m| serde_json::json!({"id": m.id, "name": who(m), "grp": m.grp})).collect();
    let images: Vec<_> = st.db.images().unwrap_or_default().iter().map(|i| serde_json::json!({"id": i.id, "name": i.name, "os": i.os, "groups": i.groups})).collect();
    let disks: Vec<_> = crate::games::disks(&st).iter().map(|d| serde_json::json!({"name": d.name, "letter": d.letter, "groups": d.groups})).collect();
    let drivers: Vec<_> = st.db.drivers().unwrap_or_default().iter().map(|d| serde_json::json!({"id": d.id, "name": d.name, "all": d.all_machines, "groups": d.groups})).collect();
    Json(serde_json::json!({"groups": all(&st), "machines": machines, "images": images, "disks": disks, "drivers": drivers}))
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

/// POST /api/groups/create {name} — a new, empty group.
async fn create(State(st): State<SharedState>, Json(b): Json<NameBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let name = name_ok(&b.name)?;
    if all(&st).iter().any(|g| same(g, &name)) {
        return Err(bad(format!("group {name} already exists")));
    }
    let mut v = saved(&st);
    v.push(name.clone());
    save(&st, &v)?;
    tracing::info!("group {name} created");
    Ok(ok())
}

/// `groups` with `from` replaced by `to` (None = removed), no duplicate left.
fn swap(groups: &[String], from: &str, to: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for g in groups {
        let g = if same(g, from) { match to { Some(t) => t, None => continue } } else { g.as_str() };
        if !out.iter().any(|x| same(x, g)) {
            out.push(g.to_string());
        }
    }
    out
}

/// Apply `f` (groups → new groups) to every image, games disk and driver package, and `machine` (a machine's group →
/// its new one) to every machine. `allow_all`: an image / games disk may lose its last group (= every machine gets
/// it); otherwise that is refused, nothing changed, with their names.
fn apply(st: &SharedState, f: &dyn Fn(&str, &[String]) -> Vec<String>, machine: &dyn Fn(&crate::db::Machine) -> Option<String>, allow_all: bool) -> Result<(), ApiError> {
    let images = st.db.images().map_err(ise)?;
    let mut disks = crate::games::disks(st);
    if !allow_all {
        let to_all: Vec<String> = images
            .iter()
            .filter(|i| !i.groups.is_empty() && f(&format!("i{}", i.id), &i.groups).is_empty())
            .map(|i| format!("image {}", i.name))
            .chain(disks.iter().filter(|d| !d.groups.is_empty() && f(&format!("d{}", d.name), &d.groups).is_empty()).map(|d| format!("games disk {}", d.name)))
            .collect();
        if !to_all.is_empty() {
            return Err((StatusCode::CONFLICT, format!("{} would go to EVERY machine (no group left)", to_all.join(", "))));
        }
    }
    let mut disks_changed = false;
    for d in disks.iter_mut() {
        let g = f(&format!("d{}", d.name), &d.groups);
        if g != d.groups {
            d.groups = g;
            disks_changed = true;
        }
    }
    // Two games disks a machine could both get can't share a drive letter (checked before anything is written).
    for a in &disks {
        if let Some(b) = disks.iter().find(|b| crate::games::clash(a, b)) {
            return Err(bad(format!("games disks {} and {} would share drive {}: on the same machines", a.name, b.name, a.letter)));
        }
    }
    let before = crate::drivers::key_sets(st);
    for i in &images {
        let g = f(&format!("i{}", i.id), &i.groups);
        if g != i.groups {
            st.db.set_image_groups(i.id, &g).map_err(ise)?;
        }
    }
    if disks_changed {
        crate::games::put(st, &disks)?;
    }
    for d in st.db.drivers().map_err(ise)? {
        let g = f(&format!("r{}", d.id), &d.groups);
        if g != d.groups {
            st.db.set_driver_targets(d.id, d.all_machines, &g).map_err(ise)?;
        }
    }
    for m in st.db.machines().map_err(ise)? {
        let g = machine(&m);
        if g != m.grp {
            st.db.set_machine_group(m.id, g.as_deref()).map_err(ise)?;
        }
    }
    crate::drivers::rearm_changed(st, before);
    Ok(())
}

#[derive(Deserialize)]
struct RenameBody {
    from: String,
    to: String,
}

/// POST /api/groups/rename {from, to} — everywhere (machines, images, games disks, drivers). Into an existing group =
/// the two merged.
async fn rename(State(st): State<SharedState>, Json(b): Json<RenameBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let to = name_ok(&b.to)?;
    let from = b.from.trim().to_string();
    let st2 = st.clone();
    let (f2, t2) = (from.clone(), to.clone());
    tokio::task::spawn_blocking(move || {
        apply(&st2, &|_, g| swap(g, &f2, Some(&t2)), &|m| m.grp.as_ref().map(|g| if same(g, &f2) { t2.clone() } else { g.clone() }), true)?;
        save(&st2, &swap(&saved(&st2), &f2, Some(&t2)))
    })
    .await
    .map_err(ise)??;
    sync_games(&st).await;
    tracing::info!("group {from} renamed to {to}");
    Ok(ok())
}

/// POST /api/groups/delete {name} — its machines get no group; it leaves every image / games disk / driver. Refused
/// while an image or games disk is only for this group (it would go to every machine).
async fn delete(State(st): State<SharedState>, Json(b): Json<NameBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let name = b.name.trim().to_string();
    let (st2, n2) = (st.clone(), name.clone());
    tokio::task::spawn_blocking(move || {
        apply(&st2, &|_, g| swap(g, &n2, None), &|m| m.grp.clone().filter(|g| !same(g, &n2)), false)?;
        save(&st2, &swap(&saved(&st2), &n2, None))
    })
    .await
    .map_err(ise)??;
    sync_games(&st).await;
    tracing::info!("group {name} deleted");
    Ok(ok())
}

#[derive(Deserialize)]
struct SetBody {
    name: String,
    /// Machine ids in the group (a machine has one group: one picked here leaves its old one).
    #[serde(default)]
    machines: Vec<i64>,
    #[serde(default)]
    images: Vec<i64>,
    /// Games disk names.
    #[serde(default)]
    disks: Vec<String>,
    /// Driver package ids.
    #[serde(default)]
    drivers: Vec<i64>,
    /// An image / games disk may lose its last group (then every machine gets it).
    #[serde(default)]
    allow_all: bool,
}

/// POST /api/groups/set {name, machines, images, disks, drivers, allow_all} — exactly these are the group's. Images /
/// games disks for every machine (no group) are left as they are: limit them on their own page.
async fn set(State(st): State<SharedState>, Json(b): Json<SetBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let name = name_ok(&b.name)?;
    let (st2, n2) = (st.clone(), name.clone());
    tokio::task::spawn_blocking(move || {
        let want = |key: &str| match key.split_at(1) {
            ("i", id) => b.images.iter().any(|x| x.to_string() == id),
            ("d", d) => b.disks.iter().any(|x| x == d),
            ("r", id) => b.drivers.iter().any(|x| x.to_string() == id),
            _ => false,
        };
        let f = |key: &str, g: &[String]| {
            // Image / games disk for every machine: stays so (not made this group's only).
            if g.is_empty() && !key.starts_with('r') {
                return Vec::new();
            }
            let has = g.iter().any(|x| same(x, &n2));
            match (want(key), has) {
                (true, false) => g.iter().cloned().chain([n2.clone()]).collect(),
                (false, true) => swap(g, &n2, None),
                _ => g.to_vec(),
            }
        };
        let machine = |m: &crate::db::Machine| {
            if b.machines.contains(&m.id) {
                Some(n2.clone())
            } else {
                m.grp.clone().filter(|g| !same(g, &n2))
            }
        };
        apply(&st2, &f, &machine, b.allow_all)?;
        let mut v = saved(&st2);
        if !v.iter().any(|g| same(g, &n2)) {
            v.push(n2.clone());
            save(&st2, &v)?;
        }
        Ok::<_, ApiError>(())
    })
    .await
    .map_err(ise)??;
    sync_games(&st).await;
    tracing::info!("group {name}: machines, images, games disks and drivers set");
    Ok(ok())
}

async fn sync_games(st: &SharedState) {
    let st = st.clone();
    if let Ok(Err(e)) = tokio::task::spawn_blocking(move || crate::games::sync(&st)).await {
        tracing::warn!("games disks: {e}");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn swap_renames_removes_merges() {
        let g = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(super::swap(&g(&["VIP", "Staff"]), "vip", Some("Gold")), g(&["Gold", "Staff"]));
        assert_eq!(super::swap(&g(&["VIP", "Staff"]), "VIP", None), g(&["Staff"]));
        assert_eq!(super::swap(&g(&["VIP", "Gold"]), "VIP", Some("gold")), g(&["gold"]), "merged into an existing one");
    }
}
