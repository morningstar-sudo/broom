// devices.rs — the Devices page: full machine management (the Machines page stays the quick dashboard).
// Edit / delete, bulk actions, auto-numbered registration, CSV export/import, per-machine detail.
// License keys: importable by CSV, never exported; only the logged-in admin sees them (machine detail / status).
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use std::collections::{BTreeSet, HashSet};
use std::net::Ipv4Addr;

use crate::SharedState;
use crate::api::{ApiError, bad, ise};
use crate::db::Machine;
use crate::license::{KEY_RULE, key_ok};
use crate::machines::{HOSTNAME_RULE, hostname_ok, who};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/machines/update", post(update))
        .route("/api/machines/delete", post(delete))
        .route("/api/machines/bulk", post(bulk))
        .route("/api/machines/register-bulk", post(register_bulk))
        .route("/api/machines/export.csv", get(export_csv))
        .route("/api/machines/import", post(import_csv))
        .route("/api/machines/detail", get(detail))
}

/// "AA-BB-CC-DD-EE-FF", "aabb.ccdd.eeff", "aabbccddeeff" → "aa:bb:cc:dd:ee:ff"; None unless 12 hex digits.
pub(crate) fn norm_mac(s: &str) -> Option<String> {
    let s = s.trim();
    if !s
        .chars()
        .all(|c| c.is_ascii_hexdigit() || ":-. ".contains(c))
    {
        return None;
    }
    let hex: Vec<char> = s
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    (hex.len() == 12).then(|| {
        hex.chunks(2)
            .map(|p| p.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join(":")
    })
}

fn blank(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Normalize + check one machine's editable fields against every OTHER machine (unique MAC / hostname / IP).
pub(crate) fn validate(m: &mut Machine, others: &[Machine]) -> Result<(), String> {
    m.mac = norm_mac(&m.mac).ok_or_else(|| {
        format!(
            "MAC {:?}: expected 12 hex digits (aa:bb:cc:dd:ee:ff)",
            m.mac
        )
    })?;
    m.hostname = blank(m.hostname.take());
    m.ip = blank(m.ip.take());
    m.grp = blank(m.grp.take());
    m.notes = blank(m.notes.take());
    if let Some(h) = &m.hostname
        && !hostname_ok(h)
    {
        return Err(format!("{h}: {HOSTNAME_RULE}"));
    }
    if let Some(ip) = &m.ip {
        let a: Ipv4Addr = ip
            .parse()
            .map_err(|_| format!("IP {ip:?} is not an IPv4 address"))?;
        if a.is_unspecified() || a.is_broadcast() || a.is_multicast() {
            return Err(format!("IP {ip} can't be given to a machine"));
        }
    }
    if let Some(g) = &m.grp
        && !crate::drivers::group_ok(g)
    {
        return Err(format!("group {g:?}: letters/digits/_/-, max 32"));
    }
    if m.notes.as_ref().is_some_and(|n| n.chars().count() > 200) {
        return Err("notes: max 200 characters".into());
    }
    for o in others.iter().filter(|o| o.id != m.id) {
        if o.mac.eq_ignore_ascii_case(&m.mac) {
            return Err(format!("MAC {} is already {}", m.mac, who(o)));
        }
        if m.hostname.is_some()
            && o.hostname.as_deref().map(str::to_ascii_lowercase)
                == m.hostname.as_deref().map(str::to_ascii_lowercase)
        {
            return Err(format!(
                "hostname {} is already used ({})",
                m.hostname.as_deref().unwrap_or(""),
                o.mac
            ));
        }
        if m.ip.is_some() && o.ip == m.ip {
            return Err(format!(
                "IP {} is already {}",
                m.ip.as_deref().unwrap_or(""),
                who(o)
            ));
        }
    }
    Ok(())
}

/// The static IP must not be held by a live lease of ANOTHER MAC (a pool lease or a declined address): handing it out
/// too would put two machines on one address until that lease ends.
pub(crate) fn ip_free(m: &Machine, leases: &[crate::db::Lease], now: i64) -> Result<(), String> {
    let Some(ip) = m.ip.as_deref() else {
        return Ok(());
    };
    match leases.iter().find(|l| {
        l.ip.as_deref() == Some(ip) && l.expires > now && !l.mac.eq_ignore_ascii_case(&m.mac)
    }) {
        Some(l) => Err(format!(
            "IP {ip} is leased to {} for another {} min — pick another IP or wait",
            l.mac,
            (l.expires - now) / 60 + 1
        )),
        None => Ok(()),
    }
}

/// Renaming a machine rebuilds its Windows base (the stage compares the name) → its license key is needed once more.
/// `old` = the row before the change (None = new row). Another image does NOT re-arm: the machine may already keep a
/// base of it (every image stays parked on the SSD), so the key would sit armed with nothing to fetch it — a new
/// image's base activates through Windows' digital license, or the admin re-arms by hand.
fn rearm_on_rename(st: &SharedState, old: Option<&Machine>, m: &Machine) {
    let Some(old) = old else { return }; // a new row has no key handed out
    if old.hostname != m.hostname && st.db.rearm_quiet(m.id).unwrap_or(false) {
        tracing::info!("license of {} armed again (renamed → base rebuilt)", who(m));
    }
}

fn by_id(machines: &[Machine], id: i64) -> Result<Machine, ApiError> {
    machines
        .iter()
        .find(|m| m.id == id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, format!("machine {id} not found")))
}

#[derive(Deserialize)]
struct UpdateBody {
    id: i64,
    mac: String,
    ip: Option<String>,
    hostname: Option<String>,
    grp: Option<String>,
    notes: Option<String>,
    image_id: Option<i64>,
}

/// POST /api/machines/update — edit one machine (license fields untouched).
async fn update(
    State(st): State<SharedState>,
    Json(b): Json<UpdateBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let all = st.db.machines().map_err(ise)?;
    let mut m = by_id(&all, b.id)?;
    let old = m.clone();
    (m.mac, m.ip, m.hostname, m.grp, m.notes) = (b.mac, b.ip, b.hostname, b.grp, b.notes);
    m.image_id = b.image_id;
    validate(&mut m, &all).map_err(bad)?;
    ip_free(&m, &st.db.leases().map_err(ise)?, crate::now_secs() as i64).map_err(bad)?;
    if let Some(i) = m.image_id {
        st.db
            .image(i)
            .map_err(ise)?
            .ok_or_else(|| bad(format!("image {i} does not exist")))?;
    }
    let before = crate::drivers::key_sets(&st);
    st.db.update_machine(&m).map_err(bad)?;
    crate::machines::moved(&st, &old.mac, &m.mac);
    tracing::info!(
        "machine {} updated - mac {} - ip {} - group {}",
        who(&m),
        m.mac,
        m.ip.as_deref().unwrap_or("-"),
        m.grp.as_deref().unwrap_or("-")
    );
    rearm_on_rename(&st, Some(&old), &m);
    crate::drivers::rearm_changed(&st, before); // another group → maybe other driver packages
    Ok(crate::api::ok())
}

#[derive(Deserialize)]
struct IdsBody {
    ids: Vec<i64>,
}

/// POST /api/machines/delete {ids} — its DHCP binding + license key go with it.
async fn delete(
    State(st): State<SharedState>,
    Json(b): Json<IdsBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let all = st.db.machines().map_err(ise)?;
    let mut n = 0;
    for id in b.ids {
        let m = by_id(&all, id)?;
        st.db.delete_machine(id).map_err(ise)?;
        let (st2, m2) = (st.clone(), m.clone());
        let _ = tokio::task::spawn_blocking(move || crate::machines::forget(&st2, &m2)).await;
        tracing::info!("machine {} deleted - mac {}", who(&m), m.mac);
        n += 1;
    }
    Ok(Json(serde_json::json!({"ok": true, "deleted": n})))
}

#[derive(Deserialize)]
struct BulkBody {
    ids: Vec<i64>,
    /// group | image | wake | delete
    action: String,
    #[serde(default)]
    value: String,
}

/// POST /api/machines/bulk {ids, action, value} — one action on many machines.
async fn bulk(
    State(st): State<SharedState>,
    Json(b): Json<BulkBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let all = st.db.machines().map_err(ise)?;
    let targets: Vec<Machine> = b
        .ids
        .iter()
        .map(|&id| by_id(&all, id))
        .collect::<Result<_, _>>()?;
    let v = b.value.trim();
    match b.action.as_str() {
        "group" => {
            if !v.is_empty() && !crate::drivers::group_ok(v) {
                return Err(bad("group: letters/digits/_/-, max 32"));
            }
            let before = crate::drivers::key_sets(&st);
            for m in &targets {
                st.db
                    .set_machine_group(m.id, (!v.is_empty()).then_some(v))
                    .map_err(ise)?;
            }
            crate::drivers::rearm_changed(&st, before);
        }
        "image" => {
            let img = if v.is_empty() {
                None
            } else {
                Some(v.parse::<i64>().map_err(|_| bad("image: an image id"))?)
            };
            if let Some(i) = img {
                st.db
                    .image(i)
                    .map_err(ise)?
                    .ok_or_else(|| bad(format!("image {i} does not exist")))?;
            }
            for m in &targets {
                st.db.set_machine_image(m.id, img).map_err(ise)?;
            }
        }
        "wake" => {
            // One per second, in the background: a whole room powered on in the same instant hits DHCP, TFTP and the
            // kernel/initrd downloads all together. One that fails doesn't stop the others.
            let list: Vec<(String, String)> =
                targets.iter().map(|m| (who(m), m.mac.clone())).collect();
            let st = st.clone();
            tokio::spawn(async move {
                for (i, (name, mac)) in list.into_iter().enumerate() {
                    if i > 0 {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                    if let Err(e) = crate::wol::wake(&*st.db, &mac) {
                        tracing::warn!("wake-on-LAN {name}: {e}");
                    }
                }
            });
        }
        "delete" => {
            for m in &targets {
                st.db.delete_machine(m.id).map_err(ise)?;
                let (st2, m2) = (st.clone(), m.clone());
                let _ =
                    tokio::task::spawn_blocking(move || crate::machines::forget(&st2, &m2)).await;
            }
        }
        a => return Err(bad(format!("unknown action {a:?}"))),
    }
    let names: Vec<String> = targets.iter().map(who).collect();
    tracing::info!(
        "bulk {} {v:?} on {} machine(s): {}",
        b.action,
        names.len(),
        names.join(", ")
    );
    Ok(Json(serde_json::json!({"ok": true, "count": names.len()})))
}

/// The next `count` free names prefix+number (zero-padded to `digits`), from `start`, skipping taken ones.
/// Scans a bounded window (never `(start..)` unbounded → a long prefix + big start could otherwise spin a CPU).
fn next_names(
    prefix: &str,
    start: u32,
    digits: usize,
    taken: &HashSet<String>,
    count: usize,
) -> Vec<String> {
    (start..=start.saturating_add(count as u32 + 10_000))
        .map(|n| format!("{prefix}{n:0digits$}"))
        .filter(|h| hostname_ok(h) && !taken.contains(&h.to_ascii_lowercase()))
        .take(count)
        .collect()
}

#[derive(Deserialize)]
struct RegisterBulk {
    macs: Vec<String>,
    prefix: String,
    start: u32,
    digits: usize,
}

/// POST /api/machines/register-bulk — new machines (seen by DHCP) get PC01, PC02… and their current IP as fixed IP.
async fn register_bulk(
    State(st): State<SharedState>,
    Json(b): Json<RegisterBulk>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let prefix = b.prefix.trim();
    if prefix.is_empty()
        || prefix.len() + b.digits.clamp(1, 4) > 15
        || !prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(bad(
            "prefix: letters/digits/'-', prefix + number max 15 characters",
        ));
    }
    let all = st.db.machines().map_err(ise)?;
    let leases = st.db.leases().map_err(ise)?;
    let taken: HashSet<String> = all
        .iter()
        .filter_map(|m| m.hostname.as_ref())
        .map(|h| h.to_ascii_lowercase())
        .collect();
    let macs: Vec<String> = b
        .macs
        .iter()
        .map(|m| norm_mac(m).ok_or_else(|| bad(format!("bad MAC {m:?}"))))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|m| !all.iter().any(|x| x.mac.eq_ignore_ascii_case(m)))
        .collect();
    let names = next_names(prefix, b.start, b.digits.clamp(1, 4), &taken, macs.len());
    let mut done = Vec::new();
    for (mac, host) in macs.iter().zip(names) {
        // The lease (DHCP server on), else the address it PXE-booted from (DHCP server off) — like single Register.
        let ip = leases
            .iter()
            .find(|l| l.mac.eq_ignore_ascii_case(mac))
            .and_then(|l| l.ip.clone())
            .or_else(|| st.pxe_ip.lock().get(&mac.to_lowercase()).cloned());
        let ip = ip.filter(|ip| !all.iter().any(|m| m.ip.as_deref() == Some(ip)));
        st.db
            .add_machine(mac, ip.as_deref(), Some(&host))
            .map_err(bad)?;
        tracing::info!(
            "machine registered - mac {mac} - ip {} - hostname {host}",
            ip.as_deref().unwrap_or("-")
        );
        done.push(serde_json::json!({"mac": mac, "hostname": host, "ip": ip}));
    }
    Ok(Json(serde_json::json!({"ok": true, "registered": done})))
}

/// One CSV field, quoted when needed.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// GET /api/machines/export.csv — mac,hostname,ip,group,notes,image (no license keys).
async fn export_csv(State(st): State<SharedState>) -> Result<impl IntoResponse, ApiError> {
    let images = st.db.images().map_err(ise)?;
    let mut out = String::from("mac,hostname,ip,group,notes,image\n");
    for m in st.db.machines().map_err(ise)? {
        let img = m
            .image_id
            .and_then(|i| images.iter().find(|x| x.id == i))
            .map(|x| x.name.clone())
            .unwrap_or_default();
        let row = [
            m.mac.as_str(),
            m.hostname.as_deref().unwrap_or(""),
            m.ip.as_deref().unwrap_or(""),
            m.grp.as_deref().unwrap_or(""),
            m.notes.as_deref().unwrap_or(""),
            img.as_str(),
        ];
        out.push_str(
            &row.iter()
                .map(|f| csv_field(f))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"machines.csv\"",
            ),
        ],
        out,
    ))
}

/// Minimal CSV: comma separated, "quoted" fields with "" escapes, CRLF or LF. Empty lines skipped.
/// Returns (line number, fields) per record.
fn parse_csv(text: &str) -> Result<Vec<(usize, Vec<String>)>, String> {
    let (mut rows, mut row, mut field) = (Vec::new(), Vec::new(), String::new());
    let (mut line, mut start, mut quoted) = (1, 1, false);
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            (true, '"') => quoted = false,
            (true, c) => {
                if c == '\n' {
                    line += 1;
                }
                field.push(c)
            }
            (false, '"') if field.is_empty() => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\r') => {}
            (false, '\n') => {
                row.push(std::mem::take(&mut field));
                if row.iter().any(|f| !f.trim().is_empty()) {
                    rows.push((start, std::mem::take(&mut row)));
                }
                row.clear();
                line += 1;
                start = line;
            }
            (false, c) => field.push(c),
        }
    }
    if quoted {
        return Err(format!("line {start}: unclosed quote"));
    }
    row.push(field);
    if row.iter().any(|f| !f.trim().is_empty()) {
        rows.push((start, row));
    }
    Ok(rows)
}

/// Machines to write + (MAC, license key) pairs to set.
type Import = (Vec<Machine>, Vec<(String, String)>);

/// CSV rows → machines to write (upsert by MAC) + license keys to set. All errors listed; nothing when any.
/// Columns by header name (any order): mac (required), hostname, ip, group, notes, image, license_key.
/// A column that is absent keeps the stored value; present but empty clears it.
fn plan_import(
    text: &str,
    existing: &[Machine],
    images: &[crate::db::Image],
) -> Result<Import, Vec<String>> {
    let rows = parse_csv(text).map_err(|e| vec![e])?;
    let Some(((_, head), body)) = rows.split_first() else {
        return Err(vec!["empty file".into()]);
    };
    let col = |name: &str| {
        head.iter()
            .position(|h| h.trim().eq_ignore_ascii_case(name))
    };
    let Some(c_mac) = col("mac") else {
        return Err(vec!["header: a \"mac\" column is required".into()]);
    };
    let (c_host, c_ip, c_grp, c_notes, c_img, c_key) = (
        col("hostname"),
        col("ip"),
        col("group"),
        col("notes"),
        col("image"),
        col("license_key"),
    );
    let mut all = existing.to_vec();
    let (mut writes, mut keys, mut errs) = (Vec::new(), Vec::new(), Vec::new());
    let mut next_new = -1i64; // placeholder ids for new rows (validation only)
    for (line, r) in body {
        let get = |c: Option<usize>| {
            c.map(|i| r.get(i).map(|s| s.trim().to_string()).unwrap_or_default())
        };
        let Some(mac) = norm_mac(&get(Some(c_mac)).unwrap_or_default()) else {
            errs.push(format!("line {line}: bad or missing MAC"));
            continue;
        };
        let mut m = all
            .iter()
            .find(|x| x.mac == mac)
            .cloned()
            .unwrap_or_else(|| {
                next_new -= 1;
                Machine {
                    id: next_new,
                    mac: mac.clone(),
                    ..Default::default()
                }
            });
        if let Some(v) = get(c_host) {
            m.hostname = Some(v);
        }
        if let Some(v) = get(c_ip) {
            m.ip = Some(v);
        }
        if let Some(v) = get(c_grp) {
            m.grp = Some(v);
        }
        if let Some(v) = get(c_notes) {
            m.notes = Some(v);
        }
        if let Some(v) = get(c_img) {
            m.image_id = if v.is_empty() {
                None
            } else {
                match images.iter().find(|i| i.name == v) {
                    Some(i) => Some(i.id),
                    None => {
                        errs.push(format!("line {line}: no image named {v:?}"));
                        continue;
                    }
                }
            };
        }
        if let Err(e) = validate(&mut m, &all) {
            errs.push(format!("line {line}: {e}"));
            continue;
        }
        if let Some(k) = get(c_key).filter(|k| !k.is_empty()) {
            let k = k.to_ascii_uppercase();
            if !key_ok(&k) {
                errs.push(format!("line {line}: {KEY_RULE}"));
                continue;
            }
            if m.license_key.as_deref() != Some(k.as_str()) {
                keys.push((mac.clone(), k));
            }
        }
        match all.iter_mut().find(|x| x.id == m.id) {
            Some(x) => *x = m.clone(),
            None => all.push(m.clone()),
        }
        writes.retain(|w: &Machine| w.mac != m.mac);
        writes.push(m);
    }
    if errs.is_empty() {
        Ok((writes, keys))
    } else {
        Err(errs)
    }
}

/// POST /api/machines/import (CSV body) — upsert by MAC; every line checked first, nothing written if one fails.
async fn import_csv(
    State(st): State<SharedState>,
    body: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let text = axum::body::to_bytes(body, 4 << 20).await.map_err(bad)?;
    let text = String::from_utf8_lossy(&text);
    let (existing, images) = (st.db.machines().map_err(ise)?, st.db.images().map_err(ise)?);
    let (writes, keys) = plan_import(&text, &existing, &images).map_err(|e| bad(e.join("\n")))?;
    let (leases, now) = (st.db.leases().map_err(ise)?, crate::now_secs() as i64);
    let held: Vec<String> = writes
        .iter()
        .filter_map(|m| ip_free(m, &leases, now).err())
        .collect();
    if !held.is_empty() {
        return Err(bad(held.join("\n")));
    }
    let (mut added, mut updated) = (0, 0);
    let before = crate::drivers::key_sets(&st);
    for mut m in writes {
        let old = existing.iter().find(|x| x.id == m.id).cloned();
        if m.id < 0 {
            m.id = st.db.add_machine(&m.mac, None, None).map_err(ise)?;
            added += 1;
        } else {
            updated += 1;
        }
        st.db.update_machine(&m).map_err(ise)?;
        if let Some(o) = &old {
            crate::machines::moved(&st, &o.mac, &m.mac);
        }
        rearm_on_rename(&st, old.as_ref(), &m);
    }
    crate::drivers::rearm_changed(&st, before);
    for (mac, key) in &keys {
        if let Some(m) = st
            .db
            .machines()
            .map_err(ise)?
            .into_iter()
            .find(|m| &m.mac == mac)
        {
            st.db.set_license(m.id, Some(key)).map_err(ise)?;
        }
    }
    tracing::info!(
        "machines imported: {added} added, {updated} updated, {} license key(s) set",
        keys.len()
    );
    Ok(Json(
        serde_json::json!({"ok": true, "added": added, "updated": updated, "keys": keys.len()}),
    ))
}

#[derive(Deserialize)]
struct IdQuery {
    id: i64,
}

/// GET /api/machines/detail?id= — machine + lease + hardware its stage reported + driver packages it gets.
async fn detail(
    State(st): State<SharedState>,
    axum::Extension(w): axum::Extension<crate::auth::Who>,
    Query(q): Query<IdQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let m = by_id(&st.db.machines().map_err(ise)?, q.id)?;
    let lease = st
        .db
        .leases()
        .map_err(ise)?
        .into_iter()
        .find(|l| l.mac.eq_ignore_ascii_case(&m.mac));
    let hw: BTreeSet<String> = st
        .db
        .machine_hw()
        .map_err(ise)?
        .into_iter()
        .find(|(mac, _)| mac.eq_ignore_ascii_case(&m.mac))
        .map(|(_, ids)| ids.into_iter().collect())
        .unwrap_or_default();
    let drivers = st.db.drivers().map_err(ise)?;
    let got: Vec<&str> = crate::drivers::pick(&drivers, &hw, m.grp.as_deref())
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    let image = m
        .image_id
        .and_then(|i| st.db.image(i).ok().flatten())
        .map(|i| i.name);
    // Skip-serialized on Machine; an owner sees it here, staff do not.
    let license_key = m
        .license_key
        .clone()
        .filter(|_| w.role == crate::auth::Role::Owner);
    Ok(Json(serde_json::json!({
        "machine": m,
        "license_key": license_key,
        "image": image,
        "lease": lease.map(|l| serde_json::json!({"ip": l.ip, "expires_in": l.expires - crate::now_secs() as i64, "source": l.source})),
        "hwids": hw,
        "drivers": got,
        "agent_key": crate::agentauth::info(&st, &m.mac),
    })))
}

#[cfg(test)]
#[path = "../tests/unit/devices.rs"]
mod tests;
