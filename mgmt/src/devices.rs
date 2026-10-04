// devices.rs — the Devices page: full machine management (the Machines page stays the quick dashboard).
// Edit / delete, bulk actions, auto-numbered registration, CSV export/import, per-machine detail.
// License keys: importable by CSV, never exported; only the logged-in admin sees them (machine detail / status).
use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::collections::{BTreeSet, HashSet};
use std::net::Ipv4Addr;

use crate::db::Machine;
use crate::api::{bad, ise, ApiError};
use crate::license::{key_ok, KEY_RULE};
use crate::machines::{hostname_ok, who, HOSTNAME_RULE};
use crate::SharedState;

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

fn ok_json(v: serde_json::Value) -> Json<serde_json::Value> {
    Json(v)
}

/// "AA-BB-CC-DD-EE-FF", "aabb.ccdd.eeff", "aabbccddeeff" → "aa:bb:cc:dd:ee:ff"; None unless 12 hex digits.
pub(crate) fn norm_mac(s: &str) -> Option<String> {
    let s = s.trim();
    if !s.chars().all(|c| c.is_ascii_hexdigit() || ":-. ".contains(c)) {
        return None;
    }
    let hex: Vec<char> = s.chars().filter(char::is_ascii_hexdigit).map(|c| c.to_ascii_lowercase()).collect();
    (hex.len() == 12).then(|| hex.chunks(2).map(|p| p.iter().collect::<String>()).collect::<Vec<_>>().join(":"))
}

fn blank(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Normalize + check one machine's editable fields against every OTHER machine (unique MAC / hostname / IP).
pub(crate) fn validate(m: &mut Machine, others: &[Machine]) -> Result<(), String> {
    m.mac = norm_mac(&m.mac).ok_or_else(|| format!("MAC {:?}: expected 12 hex digits (aa:bb:cc:dd:ee:ff)", m.mac))?;
    m.hostname = blank(m.hostname.take());
    m.ip = blank(m.ip.take());
    m.grp = blank(m.grp.take());
    m.notes = blank(m.notes.take());
    if let Some(h) = &m.hostname {
        if !hostname_ok(h) {
            return Err(format!("{h}: {HOSTNAME_RULE}"));
        }
    }
    if let Some(ip) = &m.ip {
        let a: Ipv4Addr = ip.parse().map_err(|_| format!("IP {ip:?} is not an IPv4 address"))?;
        if a.is_unspecified() || a.is_broadcast() || a.is_multicast() {
            return Err(format!("IP {ip} can't be given to a machine"));
        }
    }
    if let Some(g) = &m.grp {
        if !crate::drivers::group_ok(g) {
            return Err(format!("group {g:?}: letters/digits/_/-, max 32"));
        }
    }
    if m.notes.as_ref().is_some_and(|n| n.chars().count() > 200) {
        return Err("notes: max 200 characters".into());
    }
    for o in others.iter().filter(|o| o.id != m.id) {
        if o.mac.eq_ignore_ascii_case(&m.mac) {
            return Err(format!("MAC {} is already {}", m.mac, who(o)));
        }
        if m.hostname.is_some() && o.hostname.as_deref().map(str::to_ascii_lowercase) == m.hostname.as_deref().map(str::to_ascii_lowercase) {
            return Err(format!("hostname {} is already used ({})", m.hostname.as_deref().unwrap_or(""), o.mac));
        }
        if m.ip.is_some() && o.ip == m.ip {
            return Err(format!("IP {} is already {}", m.ip.as_deref().unwrap_or(""), who(o)));
        }
    }
    Ok(())
}

/// The static IP must not be held by a live lease of ANOTHER MAC (a pool lease or a declined address): handing it out
/// too would put two machines on one address until that lease ends.
pub(crate) fn ip_free(m: &Machine, leases: &[crate::db::Lease], now: i64) -> Result<(), String> {
    let Some(ip) = m.ip.as_deref() else { return Ok(()) };
    match leases.iter().find(|l| l.ip.as_deref() == Some(ip) && l.expires > now && !l.mac.eq_ignore_ascii_case(&m.mac)) {
        Some(l) => Err(format!("IP {ip} is leased to {} for another {} min — pick another IP or wait", l.mac, (l.expires - now) / 60 + 1)),
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
    machines.iter().find(|m| m.id == id).cloned().ok_or((StatusCode::NOT_FOUND, format!("machine {id} not found")))
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
async fn update(State(st): State<SharedState>, Json(b): Json<UpdateBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let all = st.db.machines().map_err(ise)?;
    let mut m = by_id(&all, b.id)?;
    let old = m.clone();
    (m.mac, m.ip, m.hostname, m.grp, m.notes) = (b.mac, b.ip, b.hostname, b.grp, b.notes);
    m.image_id = b.image_id;
    validate(&mut m, &all).map_err(bad)?;
    ip_free(&m, &st.db.leases().map_err(ise)?, crate::now_secs() as i64).map_err(bad)?;
    if let Some(i) = m.image_id {
        st.db.image(i).map_err(ise)?.ok_or_else(|| bad(format!("image {i} does not exist")))?;
    }
    let before = crate::drivers::key_sets(&st);
    st.db.update_machine(&m).map_err(bad)?;
    tracing::info!("machine {} updated - mac {} - ip {} - group {}", who(&m), m.mac, m.ip.as_deref().unwrap_or("-"), m.grp.as_deref().unwrap_or("-"));
    rearm_on_rename(&st, Some(&old), &m);
    crate::drivers::rearm_changed(&st, before); // another group → maybe other driver packages
    Ok(ok_json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct IdsBody {
    ids: Vec<i64>,
}

/// POST /api/machines/delete {ids} — its DHCP binding + license key go with it.
async fn delete(State(st): State<SharedState>, Json(b): Json<IdsBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let all = st.db.machines().map_err(ise)?;
    let mut n = 0;
    for id in b.ids {
        let m = by_id(&all, id)?;
        st.db.delete_machine(id).map_err(ise)?;
        tracing::info!("machine {} deleted - mac {}", who(&m), m.mac);
        n += 1;
    }
    Ok(ok_json(serde_json::json!({"ok": true, "deleted": n})))
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
async fn bulk(State(st): State<SharedState>, Json(b): Json<BulkBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let all = st.db.machines().map_err(ise)?;
    let targets: Vec<Machine> = b.ids.iter().map(|&id| by_id(&all, id)).collect::<Result<_, _>>()?;
    let v = b.value.trim();
    match b.action.as_str() {
        "group" => {
            if !v.is_empty() && !crate::drivers::group_ok(v) {
                return Err(bad("group: letters/digits/_/-, max 32"));
            }
            let before = crate::drivers::key_sets(&st);
            for m in &targets {
                st.db.set_machine_group(m.id, (!v.is_empty()).then_some(v)).map_err(ise)?;
            }
            crate::drivers::rearm_changed(&st, before);
        }
        "image" => {
            let img = if v.is_empty() { None } else { Some(v.parse::<i64>().map_err(|_| bad("image: an image id"))?) };
            if let Some(i) = img {
                st.db.image(i).map_err(ise)?.ok_or_else(|| bad(format!("image {i} does not exist")))?;
            }
            for m in &targets {
                st.db.set_machine_image(m.id, img).map_err(ise)?;
            }
        }
        "wake" => {
            for m in &targets {
                crate::wol::wake(&*st.db, &m.mac).map_err(|e| bad(format!("{}: {e}", who(m))))?;
            }
        }
        "delete" => {
            for m in &targets {
                st.db.delete_machine(m.id).map_err(ise)?;
            }
        }
        a => return Err(bad(format!("unknown action {a:?}"))),
    }
    let names: Vec<String> = targets.iter().map(who).collect();
    tracing::info!("bulk {} {v:?} on {} machine(s): {}", b.action, names.len(), names.join(", "));
    Ok(ok_json(serde_json::json!({"ok": true, "count": names.len()})))
}

/// The next `count` free names prefix+number (zero-padded to `digits`), from `start`, skipping taken ones.
/// Scans a bounded window (never `(start..)` unbounded → a long prefix + big start could otherwise spin a CPU).
fn next_names(prefix: &str, start: u32, digits: usize, taken: &HashSet<String>, count: usize) -> Vec<String> {
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
async fn register_bulk(State(st): State<SharedState>, Json(b): Json<RegisterBulk>) -> Result<Json<serde_json::Value>, ApiError> {
    let prefix = b.prefix.trim();
    if prefix.is_empty() || prefix.len() + b.digits.clamp(1, 4) > 15 || !prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(bad("prefix: letters/digits/'-', prefix + number max 15 characters"));
    }
    let all = st.db.machines().map_err(ise)?;
    let leases = st.db.leases().map_err(ise)?;
    let taken: HashSet<String> = all.iter().filter_map(|m| m.hostname.as_ref()).map(|h| h.to_ascii_lowercase()).collect();
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
        let ip = leases.iter().find(|l| l.mac.eq_ignore_ascii_case(mac)).and_then(|l| l.ip.clone());
        let ip = ip.filter(|ip| !all.iter().any(|m| m.ip.as_deref() == Some(ip)));
        st.db.add_machine(mac, ip.as_deref(), Some(&host)).map_err(bad)?;
        tracing::info!("machine registered - mac {mac} - ip {} - hostname {host}", ip.as_deref().unwrap_or("-"));
        done.push(serde_json::json!({"mac": mac, "hostname": host, "ip": ip}));
    }
    Ok(ok_json(serde_json::json!({"ok": true, "registered": done})))
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
        let img = m.image_id.and_then(|i| images.iter().find(|x| x.id == i)).map(|x| x.name.clone()).unwrap_or_default();
        let row = [m.mac.as_str(), m.hostname.as_deref().unwrap_or(""), m.ip.as_deref().unwrap_or(""),
                   m.grp.as_deref().unwrap_or(""), m.notes.as_deref().unwrap_or(""), img.as_str()];
        out.push_str(&row.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    Ok((
        [(header::CONTENT_TYPE, "text/csv; charset=utf-8"), (header::CONTENT_DISPOSITION, "attachment; filename=\"machines.csv\"")],
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

/// CSV rows → machines to write (upsert by MAC) + license keys to set. All errors listed; nothing when any.
/// Columns by header name (any order): mac (required), hostname, ip, group, notes, image, license_key.
/// A column that is absent keeps the stored value; present but empty clears it.
fn plan_import(text: &str, existing: &[Machine], images: &[crate::db::Image]) -> Result<(Vec<Machine>, Vec<(String, String)>), Vec<String>> {
    let rows = parse_csv(text).map_err(|e| vec![e])?;
    let Some(((_, head), body)) = rows.split_first() else { return Err(vec!["empty file".into()]) };
    let col = |name: &str| head.iter().position(|h| h.trim().eq_ignore_ascii_case(name));
    let Some(c_mac) = col("mac") else { return Err(vec!["header: a \"mac\" column is required".into()]) };
    let (c_host, c_ip, c_grp, c_notes, c_img, c_key) =
        (col("hostname"), col("ip"), col("group"), col("notes"), col("image"), col("license_key"));
    let mut all = existing.to_vec();
    let (mut writes, mut keys, mut errs) = (Vec::new(), Vec::new(), Vec::new());
    let mut next_new = -1i64; // placeholder ids for new rows (validation only)
    for (line, r) in body {
        let get = |c: Option<usize>| c.map(|i| r.get(i).map(|s| s.trim().to_string()).unwrap_or_default());
        let Some(mac) = norm_mac(&get(Some(c_mac)).unwrap_or_default()) else {
            errs.push(format!("line {line}: bad or missing MAC"));
            continue;
        };
        let mut m = all.iter().find(|x| x.mac == mac).cloned().unwrap_or_else(|| {
            next_new -= 1;
            Machine { id: next_new, mac: mac.clone(), ip: None, hostname: None, image_id: None, license_key: None, license_tail: None,
                      license_state: None, license_gen: 0, license_result: None, grp: None, notes: None }
        });
        if let Some(v) = get(c_host) { m.hostname = Some(v); }
        if let Some(v) = get(c_ip) { m.ip = Some(v); }
        if let Some(v) = get(c_grp) { m.grp = Some(v); }
        if let Some(v) = get(c_notes) { m.notes = Some(v); }
        if let Some(v) = get(c_img) {
            m.image_id = if v.is_empty() { None } else {
                match images.iter().find(|i| i.name == v) {
                    Some(i) => Some(i.id),
                    None => { errs.push(format!("line {line}: no image named {v:?}")); continue; }
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
    if errs.is_empty() { Ok((writes, keys)) } else { Err(errs) }
}

/// POST /api/machines/import (CSV body) — upsert by MAC; every line checked first, nothing written if one fails.
async fn import_csv(State(st): State<SharedState>, body: Body) -> Result<Json<serde_json::Value>, ApiError> {
    let text = axum::body::to_bytes(body, 4 << 20).await.map_err(bad)?;
    let text = String::from_utf8_lossy(&text);
    let (existing, images) = (st.db.machines().map_err(ise)?, st.db.images().map_err(ise)?);
    let (writes, keys) = plan_import(&text, &existing, &images).map_err(|e| bad(e.join("\n")))?;
    let (leases, now) = (st.db.leases().map_err(ise)?, crate::now_secs() as i64);
    let held: Vec<String> = writes.iter().filter_map(|m| ip_free(m, &leases, now).err()).collect();
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
        rearm_on_rename(&st, old.as_ref(), &m);
    }
    crate::drivers::rearm_changed(&st, before);
    for (mac, key) in &keys {
        if let Some(m) = st.db.machines().map_err(ise)?.into_iter().find(|m| &m.mac == mac) {
            st.db.set_license(m.id, Some(key)).map_err(ise)?;
        }
    }
    tracing::info!("machines imported: {added} added, {updated} updated, {} license key(s) set", keys.len());
    Ok(ok_json(serde_json::json!({"ok": true, "added": added, "updated": updated, "keys": keys.len()})))
}

#[derive(Deserialize)]
struct IdQuery {
    id: i64,
}

/// GET /api/machines/detail?id= — machine + lease + hardware its stage reported + driver packages it gets.
async fn detail(State(st): State<SharedState>, Query(q): Query<IdQuery>) -> Result<Json<serde_json::Value>, ApiError> {
    let m = by_id(&st.db.machines().map_err(ise)?, q.id)?;
    let lease = st.db.leases().map_err(ise)?.into_iter().find(|l| l.mac.eq_ignore_ascii_case(&m.mac));
    let hw: BTreeSet<String> = st
        .db
        .machine_hw()
        .map_err(ise)?
        .into_iter()
        .find(|(mac, _)| mac.eq_ignore_ascii_case(&m.mac))
        .map(|(_, ids)| ids.into_iter().collect())
        .unwrap_or_default();
    let drivers = st.db.drivers().map_err(ise)?;
    let got: Vec<&str> = crate::drivers::pick(&drivers, &hw, m.grp.as_deref()).iter().map(|d| d.name.as_str()).collect();
    let image = m.image_id.and_then(|i| st.db.image(i).ok().flatten()).map(|i| i.name);
    let license_key = m.license_key.clone(); // skip-serialized on Machine; the logged-in operator sees it here
    Ok(ok_json(serde_json::json!({
        "machine": m,
        "license_key": license_key,
        "image": image,
        "lease": lease.map(|l| serde_json::json!({"ip": l.ip, "expires_in": l.expires - crate::now_secs() as i64, "source": l.source})),
        "hwids": hw,
        "drivers": got,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: i64, mac: &str, host: Option<&str>, ip: Option<&str>) -> Machine {
        Machine { id, mac: mac.into(), ip: ip.map(Into::into), hostname: host.map(Into::into), image_id: None, license_key: None,
                  license_tail: None, license_state: None, license_gen: 0, license_result: None, grp: None, notes: None }
    }

    #[test]
    fn mac_normalize_and_validate() {
        assert_eq!(norm_mac("AA-BB-CC-DD-EE-0F").as_deref(), Some("aa:bb:cc:dd:ee:0f"));
        assert_eq!(norm_mac("aabb.ccdd.ee0f").as_deref(), Some("aa:bb:cc:dd:ee:0f"));
        assert_eq!(norm_mac("aa:bb:cc:dd:ee"), None);
        assert_eq!(norm_mac("zz:bb:cc:dd:ee:0f"), None);
        let all = [m(1, "aa:00:00:00:00:01", Some("PC01"), Some("10.0.0.51"))];
        let mut x = m(2, "AA-00-00-00-00-02", Some(" PC02 "), Some("10.0.0.52"));
        validate(&mut x, &all).unwrap();
        assert_eq!((x.mac.as_str(), x.hostname.as_deref()), ("aa:00:00:00:00:02", Some("PC02")));
        for (bad, why) in [
            (m(2, "aa:00:00:00:00:01", None, None), "MAC"),
            (m(2, "aa:00:00:00:00:02", Some("pc01"), None), "hostname"),
            (m(2, "aa:00:00:00:00:02", None, Some("10.0.0.51")), "IP 10.0.0.51 is already"),
            (m(2, "aa:00:00:00:00:02", None, Some("10.0.0.300")), "not an IPv4"),
            (m(2, "aa:00:00:00:00:02", Some("PC 2"), None), "Hostname"),
        ] {
            let mut b = bad;
            let e = validate(&mut b, &all).unwrap_err();
            assert!(e.contains(why), "{e}");
        }
        let mut same = all[0].clone(); // editing a machine never clashes with itself
        validate(&mut same, &all).unwrap();
    }

    #[test]
    fn static_ip_not_held_by_a_live_lease() {
        let lease = |mac: &str, ip: &str, expires| crate::db::Lease { mac: mac.into(), ip: Some(ip.into()), hostname: None, expires, source: "full".into() };
        let leases = [lease("bb:00:00:00:00:01", "10.0.0.50", 2000), lease("declined-10.0.0.51", "10.0.0.51", 2000), lease("bb:00:00:00:00:02", "10.0.0.52", 500)];
        let pc = |ip| m(1, "aa:00:00:00:00:01", Some("PC01"), Some(ip));
        assert!(ip_free(&pc("10.0.0.50"), &leases, 1000).unwrap_err().contains("leased to bb:00:00:00:00:01"));
        assert!(ip_free(&pc("10.0.0.51"), &leases, 1000).is_err(), "declined = in use on the LAN");
        assert!(ip_free(&pc("10.0.0.52"), &leases, 1000).is_ok(), "expired lease");
        assert!(ip_free(&m(1, "bb:00:00:00:00:01", None, Some("10.0.0.50")), &leases, 1000).is_ok(), "its own lease");
        assert!(ip_free(&m(1, "aa:00:00:00:00:01", None, None), &leases, 1000).is_ok());
    }

    #[test]
    fn numbering_skips_taken() {
        let taken: HashSet<String> = ["pc01", "pc03"].iter().map(|s| s.to_string()).collect();
        assert_eq!(next_names("PC", 1, 2, &taken, 3), ["PC02", "PC04", "PC05"]);
        assert_eq!(next_names("VIP-", 9, 1, &HashSet::new(), 2), ["VIP-9", "VIP-10"]);
    }

    #[test]
    fn csv_parse_and_import_plan() {
        let rows = parse_csv("mac,notes\r\n\"aa:00:00:00:00:01\",\"a, \"\"b\"\"\"\n\naa:00:00:00:00:02,\"x\ny\"\n").unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1], (2, vec!["aa:00:00:00:00:01".into(), "a, \"b\"".into()]));
        assert_eq!(rows[2], (4, vec!["aa:00:00:00:00:02".into(), "x\ny".into()]));
        assert!(parse_csv("mac\n\"open").is_err());

        let existing = [m(1, "aa:00:00:00:00:01", Some("PC01"), Some("10.0.0.51"))];
        let img = crate::db::Image { id: 7, name: "win11".into(), os: "windows".into(), active_version: None, is_default: false,
                                     boot_script: None, hash: None, cache_mode: "disk".into(), base_mode: false, use_ssd: true };
        // Columns in any order; existing row updated (ip column absent → kept), new row added with a key.
        let csv = "Hostname,MAC,group,image,license_key\nPC01,AA-00-00-00-00-01,VIP,win11,\nPC02,aa:00:00:00:00:02,,,abcde-12345-fghij-67890-klmno\n";
        let (w, keys) = plan_import(csv, &existing, std::slice::from_ref(&img)).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].id, w[0].ip.as_deref(), w[0].grp.as_deref(), w[0].image_id), (1, Some("10.0.0.51"), Some("VIP"), Some(7)));
        assert!(w[1].id < 0 && w[1].hostname.as_deref() == Some("PC02"));
        assert_eq!(keys, vec![("aa:00:00:00:00:02".to_string(), "ABCDE-12345-FGHIJ-67890-KLMNO".to_string())]);
        // Every bad line reported with its number; duplicates inside the file too.
        let errs = plan_import("mac,hostname,image\nxx,PC05,\naa:00:00:00:00:03,PC01,\naa:00:00:00:00:04,PC04,nope\n", &existing, &[img]).unwrap_err();
        assert_eq!(errs.len(), 3, "{errs:?}");
        assert!(errs[0].starts_with("line 2") && errs[1].starts_with("line 3") && errs[2].contains("no image named"), "{errs:?}");
        assert!(plan_import("hostname\nPC9\n", &existing, &[]).unwrap_err()[0].contains("\"mac\" column"));
    }
}
