// bench.rs — numbers for the Benchmark page: the server's image disk (O_DIRECT reads), and per machine its boot time
// (every boot: menu choice → desktop, by the server's clock) and, when the admin asks, its disk + network read speed
// (measured at its next boot, before anyone plays on it). Results are kept in the DB (bench). Also the live iSCSI
// throughput the Server page shows.
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use parking_lot::Mutex;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use crate::SharedState;
use crate::api::{ApiError, bad, ise, ok};
use crate::db::BenchRow;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/bench", get(overview))
        .route("/api/bench/history", get(history))
        .route("/api/bench/server", post(run_server))
        .route("/api/bench/iscsi/live", get(iscsi_live)) // SSE
        .route("/api/bench/request", post(request))
        // Clients (public, auth.rs): the Windows stage, Windows, Linux.
        .route("/api/bench/pending", get(pending))
        .route("/api/bench/blob", get(blob))
        .route("/api/bench/report", post(report))
        .route("/api/bench/booted", post(booted))
}

/// Results older than this are dropped, and never more than KEEP_ROWS (a bound against junk reports).
const KEEP_S: i64 = 90 * 24 * 3600;
const KEEP_ROWS: usize = 20_000;
/// A requested client measurement waits this long for the machine's next boot.
const PENDING_S: u64 = 24 * 3600;
/// Bytes a client downloads for the network figure (~2 s at 1 Gbit/s), and the most it may ask for.
const BLOB_SIZE: u64 = 256 << 20;
const BLOB_MAX: u64 = 1 << 30;
/// Downloads one pending request allows (a retry or two of the stage's wget).
const BLOB_TRIES: u8 = 3;
/// A desktop reported later than this after the menu choice is not counted as a boot (someone logged in by hand).
const BOOT_WINDOW: Duration = Duration::from_secs(30 * 60);
/// Most boots in flight remembered (a room is far below it; the endpoints are public).
const BOOTS_MAX: usize = 4096;
/// Server read test: stop after this many bytes or this long, whichever comes first.
const SEQ_BYTES: u64 = 2 << 30;
const TEST_TIME: Duration = Duration::from_secs(10);
const RAND_THREADS: usize = 16;

/// The job name on the web (SSE "job" events, /api/images/job).
const JOB: &str = "bench:server";

struct Boot {
    t0: Instant,
    stage: Option<Instant>,
    image: String,
}

/// mac → its boot in flight (menu choice seen, desktop not reported yet).
static BOOTS: LazyLock<Mutex<HashMap<String, Boot>>> = LazyLock::new(Default::default);
/// mac → (asked at unix s, blob downloads so far): disk + network measured at its next boot.
static PENDING: LazyLock<Mutex<HashMap<String, (u64, u8)>>> = LazyLock::new(Default::default);
/// One /booted per IP per minute (public endpoint).
static BOOTED_SEEN: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(Default::default);

/// A menu choice that boots `image` (boot.rs): the boot clock of `mac` starts.
pub fn boot_started(mac: &str, image: &str) {
    let mut b = BOOTS.lock();
    b.retain(|_, x| x.t0.elapsed() <= BOOT_WINDOW);
    if b.len() >= BOOTS_MAX {
        return;
    }
    b.insert(
        mac.to_string(),
        Boot {
            t0: Instant::now(),
            stage: None,
            image: image.to_string(),
        },
    );
}

/// The Windows stage reached its driver query (drivers.rs): the stage part of the boot is over.
pub fn stage_reached(mac: &str) {
    if let Some(b) = BOOTS.lock().get_mut(mac) {
        b.stage.get_or_insert_with(Instant::now);
    }
}

/// Old results out (main.rs, every 10 min).
pub fn prune(st: &SharedState) {
    match st
        .db
        .prune_bench(crate::now_secs() as i64 - KEEP_S, KEEP_ROWS)
    {
        Ok(n) if n > 0 => tracing::info!("pruned {n} old benchmark result(s)"),
        Err(e) => tracing::warn!("benchmark prune: {e}"),
        _ => {}
    }
}

fn save(
    st: &SharedState,
    mac: Option<&str>,
    kind: &str,
    value: f64,
    detail: String,
) -> Result<(), String> {
    st.db.add_bench(&BenchRow {
        at: crate::now_secs() as i64,
        mac: mac.map(String::from),
        kind: kind.into(),
        value,
        detail,
    })
}

/// The client's MAC: ?mac= (the stage knows it from BOOTIF), else the peer IP's (ARP / lease / fixed IP).
fn client_mac(st: &SharedState, q: Option<&str>, peer: &SocketAddr) -> Option<String> {
    match q {
        Some(m) => crate::devices::norm_mac(m),
        None => crate::license::mac_at(st, &peer.ip().to_canonical().to_string()),
    }
}

fn is_pending(mac: &str) -> bool {
    let now = crate::now_secs();
    let mut p = PENDING.lock();
    p.retain(|_, (t, _)| now.saturating_sub(*t) <= PENDING_S);
    p.contains_key(mac)
}

// ---------- admin ----------

/// GET /api/bench — the newest result of each kind per machine + the server, the machines, what waits for a boot.
async fn overview(State(st): State<SharedState>) -> Result<Json<serde_json::Value>, ApiError> {
    let latest = st.db.bench_latest().map_err(ise)?;
    let machines: Vec<_> = st
        .db
        .machines()
        .map_err(ise)?
        .into_iter()
        .map(|m| serde_json::json!({"mac": m.mac.to_lowercase(), "hostname": m.hostname, "ip": m.ip}))
        .collect();
    is_pending(""); // drops expired requests
    let pending: Vec<String> = PENDING.lock().keys().cloned().collect();
    let job = st.jobs.lock().get(JOB).cloned();
    Ok(Json(
        serde_json::json!({"latest": latest, "machines": machines, "pending": pending, "job": job}),
    ))
}

#[derive(Deserialize)]
struct HistQ {
    mac: Option<String>,
}

/// GET /api/bench/history?mac= — the newest 200 results (of one machine; mac=server → the server's).
async fn history(
    State(st): State<SharedState>,
    Query(q): Query<HistQ>,
) -> Result<Json<Vec<BenchRow>>, ApiError> {
    let rows = match q.mac.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        None => st.db.bench_rows(None, 200).map_err(ise)?,
        Some("server") => st
            .db
            .bench_rows(None, 2000)
            .map_err(ise)?
            .into_iter()
            .filter(|r| r.mac.is_none())
            .take(200)
            .collect(),
        Some(m) => st
            .db
            .bench_rows(
                Some(&crate::devices::norm_mac(m).ok_or_else(|| bad("bad mac"))?),
                200,
            )
            .map_err(ise)?,
    };
    Ok(Json(rows))
}

/// POST /api/bench/server — read test of the image disk, as a background job.
async fn run_server(State(st): State<SharedState>) -> Result<Json<serde_json::Value>, ApiError> {
    crate::images::spawn_job(&st, JOB.into(), "benchmark", |st, _, steps| {
        server_bench(st, steps)
    })?;
    Ok(ok())
}

#[derive(Deserialize)]
struct ReqBody {
    macs: Vec<String>,
    #[serde(default)]
    cancel: bool,
}

/// POST /api/bench/request {macs, cancel} — measure these machines' disk + network at their next boot (or not).
async fn request(Json(b): Json<ReqBody>) -> Result<Json<serde_json::Value>, ApiError> {
    let macs = b
        .macs
        .iter()
        .map(|m| crate::devices::norm_mac(m).ok_or_else(|| bad(format!("bad mac {m:?}"))))
        .collect::<Result<Vec<_>, _>>()?;
    if macs.len() > BOOTS_MAX {
        return Err(bad("too many machines"));
    }
    let now = crate::now_secs();
    let mut p = PENDING.lock();
    for m in macs {
        if b.cancel {
            p.remove(&m);
        } else {
            p.insert(m, (now, 0));
        }
    }
    Ok(ok())
}

/// One sample of the iSCSI daemon's counters (None = no daemon) and when it was taken.
type Sample = (Option<crate::iscsid::Resp>, Instant);

async fn sample() -> Sample {
    (
        tokio::task::spawn_blocking(crate::iscsi::stats)
            .await
            .ok()
            .flatten(),
        Instant::now(),
    )
}

/// GET /api/bench/iscsi/live (SSE) — "iscsi" every second while the Server page is open: read throughput of each
/// target and each client since the previous sample. Sampling stops when the tab closes (the stream is dropped).
async fn iscsi_live(
    State(st): State<SharedState>,
) -> axum::response::sse::Sse<
    impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    let s = futures_util::stream::unfold((st, None::<Sample>), |(st, prev)| async move {
        let prev = match prev {
            Some(p) => p,
            None => sample().await,
        };
        tokio::time::sleep(Duration::from_secs(1)).await;
        let cur = sample().await;
        let data = rates(&st, &prev, &cur).to_string();
        Some((
            Ok(Event::default().event("iscsi").data(data)),
            (st, Some(cur)),
        ))
    });
    Sse::new(s).keep_alive(KeepAlive::default())
}

/// Throughput between two samples, as the page shows it.
fn rates(st: &SharedState, (a, at): &Sample, (b, bt): &Sample) -> serde_json::Value {
    let (Some(a), Some(b)) = (a, b) else {
        return serde_json::json!({"daemon": "none"});
    };
    let (Some(p1), Some(p2)) = (&a.peers, &b.peers) else {
        return serde_json::json!({"daemon": "old"});
    };
    let secs = bt.duration_since(*at).as_secs_f64().max(1e-3);
    let mbps = |a: u64, b: u64| b.saturating_sub(a) as f64 / 1e6 / secs;
    // Golden targets (iqn …:<image>.<gen>) and games disks, those with a client logged in.
    let mut targets: Vec<_> = b
        .targets
        .iter()
        .filter(|t| t.sessions > 0)
        .map(|t| {
            let before = a.targets.iter().find(|x| x.iqn == t.iqn).and_then(|x| x.read_bytes).unwrap_or(0);
            let name = t.iqn.rsplit(':').next().unwrap_or(&t.iqn);
            serde_json::json!({"name": name, "sessions": t.sessions, "ram": t.ram, "mbps": mbps(before, t.read_bytes.unwrap_or(0))})
        })
        .collect();
    targets.extend(b.games.iter().filter(|g| g.sessions > 0).map(|g| {
        let before = a.games.iter().find(|x| x.name == g.name).map_or(0, |x| x.read_bytes);
        serde_json::json!({"name": format!("iSCSI disk {}", g.name), "sessions": g.sessions, "ram": false, "mbps": mbps(before, g.read_bytes)})
    }));
    let hosts: HashMap<String, String> = st
        .db
        .machines()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| Some((m.ip.clone()?, crate::machines::who(&m))))
        .collect();
    // Clients logged in (reading or idle this second) + any that read and logged out within it.
    let mut peers: Vec<_> = p2
        .iter()
        .map(|p| {
            let before = p1.iter().find(|x| x.ip == p.ip).map_or(0, |x| x.read_bytes);
            (p.ip.clone(), mbps(before, p.read_bytes), p.connected)
        })
        .filter(|(_, r, on)| *on || *r > 0.0)
        .collect();
    peers.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let peers: Vec<_> = peers
        .into_iter()
        .map(|(ip, r, _)| serde_json::json!({"ip": ip, "host": hosts.get(&ip), "mbps": r}))
        .collect();
    serde_json::json!({"daemon": "ok", "targets": targets, "peers": peers})
}

// ---------- clients (public) ----------

#[derive(Deserialize)]
struct MacQ {
    mac: Option<String>,
    size: Option<u64>,
}

/// GET /api/bench/pending?mac= → "run=1 size=<bytes>" when the admin asked for a measurement, else "run=0".
async fn pending(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(q): Query<MacQ>,
) -> String {
    match client_mac(&st, q.mac.as_deref(), &peer) {
        Some(m) if is_pending(&m) => format!("run=1 size={BLOB_SIZE}\n"),
        _ => "run=0\n".into(),
    }
}

/// 1 MiB of zeros (bss: costs no file size) — the network test streams it, no disk involved.
static ZEROS: [u8; 1 << 20] = [0; 1 << 20];

/// GET /api/bench/blob?mac=&size= — `size` bytes of zeros for the network figure. Only for a machine with a pending
/// request (an open endpoint must not hand anyone gigabytes on demand), a few times per request.
async fn blob(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(q): Query<MacQ>,
) -> Response {
    let Some(mac) = client_mac(&st, q.mac.as_deref(), &peer) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if !is_pending(&mac) {
        return StatusCode::FORBIDDEN.into_response();
    }
    {
        let mut p = PENDING.lock();
        let Some((_, n)) = p.get_mut(&mac) else {
            return StatusCode::FORBIDDEN.into_response();
        };
        if *n >= BLOB_TRIES {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        *n += 1;
    }
    let size = q.size.unwrap_or(BLOB_SIZE).min(BLOB_MAX);
    let chunk = ZEROS.len() as u64;
    let parts = (0..size.div_ceil(chunk)).map(move |i| {
        Ok::<_, std::convert::Infallible>(Bytes::from_static(
            &ZEROS[..(size - i * chunk).min(chunk) as usize],
        ))
    });
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, size.to_string()),
        ],
        Body::from_stream(futures_util::stream::iter(parts)),
    )
        .into_response()
}

#[derive(Deserialize)]
struct ReportQ {
    mac: Option<String>,
    /// MB/s (10^6 bytes) read from the machine's own disk / downloaded from the server.
    disk: Option<f64>,
    net: Option<f64>,
    /// What the disk figure read (e.g. "ssd", "iscsi").
    src: Option<String>,
}

/// POST /api/bench/report?mac=&disk=&net=&src= — the results of a requested measurement. Only accepted while the
/// request is pending (then it is done); the values are bounds-checked.
async fn report(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(q): Query<ReportQ>,
) -> StatusCode {
    let Some(mac) = client_mac(&st, q.mac.as_deref(), &peer) else {
        return StatusCode::FORBIDDEN;
    };
    if PENDING.lock().remove(&mac).is_none() {
        return StatusCode::FORBIDDEN;
    }
    let sane = |v: Option<f64>| v.filter(|v| v.is_finite() && *v > 0.0 && *v < 100_000.0);
    let src: String = q
        .src
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-'))
        .take(40)
        .collect();
    let mut r = Ok(());
    if let Some(v) = sane(q.disk) {
        r = r.and(save(&st, Some(&mac), "disk", v, src));
    }
    if let Some(v) = sane(q.net) {
        r = r.and(save(
            &st,
            Some(&mac),
            "net",
            v,
            format!("{} MB from the server", BLOB_SIZE >> 20),
        ));
    }
    match r {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(e) => {
            tracing::error!("benchmark report of {mac}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// POST /api/bench/booted — the desktop is up (Windows: explorer running; Linux: graphical/multi-user target). Boot
/// time = now − the menu choice, on the server's clock. Once per boot: the boot in flight is consumed.
async fn booted(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> StatusCode {
    let ip = peer.ip().to_canonical().to_string();
    if !crate::license::once_a_minute(&BOOTED_SEEN, &ip) {
        return StatusCode::TOO_MANY_REQUESTS;
    }
    let Some(mac) = crate::license::mac_at(&st, &ip) else {
        return StatusCode::NO_CONTENT;
    };
    let Some(b) = BOOTS.lock().remove(&mac) else {
        return StatusCode::NO_CONTENT;
    };
    let secs = b.t0.elapsed();
    if secs > BOOT_WINDOW {
        return StatusCode::NO_CONTENT;
    }
    let mut detail = format!("image {}", b.image);
    if let Some(s) = b.stage {
        detail += &format!(", stage {}s", s.duration_since(b.t0).as_secs());
    }
    match save(
        &st,
        Some(&mac),
        "boot",
        (secs.as_secs_f64() * 10.0).round() / 10.0,
        detail,
    ) {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(e) => {
            tracing::error!("boot time of {mac}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

// ---------- the server's disk ----------

fn server_bench(st: &SharedState, steps: &mut crate::publish::Steps) -> Result<String, String> {
    let (name, path) = largest_image(&crate::images_dir())
        .ok_or("no image to read — add one on the Images page first")?;
    steps.go("sequential read");
    let seq = seq_read(&path, SEQ_BYTES, TEST_TIME)?;
    steps.go("random 4K read");
    let iops = rand_read(&path, RAND_THREADS, TEST_TIME)?;
    let detail = format!("{name}/image.img, O_DIRECT");
    save(st, None, "srv_seq", seq.round(), detail.clone())?;
    save(
        st,
        None,
        "srv_rand",
        iops.round(),
        format!("{detail}, {RAND_THREADS} threads"),
    )?;
    Ok(format!(
        "{seq:.0} MB/s sequential, {iops:.0} IOPS random 4K ({name})"
    ))
}

/// The biggest images/<name>/image.img: the one clients read most of.
fn largest_image(dir: &Path) -> Option<(String, PathBuf)> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let p = e.path().join("image.img");
            Some((
                std::fs::metadata(&p).ok()?.len(),
                e.file_name().to_string_lossy().into_owned(),
                p,
            ))
        })
        .max_by_key(|x| x.0)
        .map(|(_, n, p)| (n, p))
}

/// Read past the page cache: the numbers are the disk's, and the cache iSCSI clients rely on is left alone.
fn open_direct(path: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::EINVAL) {
                format!(
                    "{}: this filesystem does not support direct reads (O_DIRECT)",
                    path.display()
                )
            } else {
                format!("{}: {e}", path.display())
            }
        })
}

/// `len` bytes at a 4096-aligned address (O_DIRECT needs it), carved out of a slightly bigger Vec.
fn aligned(v: &mut Vec<u8>, len: usize) -> &mut [u8] {
    *v = vec![0u8; len + 4096];
    let off = v.as_ptr().align_offset(4096);
    &mut v[off..off + len]
}

/// Sequential read in 4 MiB blocks → MB/s.
fn seq_read(path: &Path, max: u64, time: Duration) -> Result<f64, String> {
    use std::os::unix::fs::FileExt;
    let f = open_direct(path)?;
    let mut v = Vec::new();
    let buf = aligned(&mut v, 4 << 20);
    let (t, mut pos) = (Instant::now(), 0u64);
    while pos < max && t.elapsed() < time {
        let n = f
            .read_at(buf, pos)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        pos += n as u64;
    }
    Ok(pos as f64 / 1e6 / t.elapsed().as_secs_f64().max(1e-3))
}

/// Random 4 KiB reads from `threads` threads at once (many clients booting) → IOPS.
fn rand_read(path: &Path, threads: usize, time: Duration) -> Result<f64, String> {
    use std::os::unix::fs::FileExt;
    let f = open_direct(path)?;
    let blocks = f.metadata().map_err(|e| e.to_string())?.len() / 4096;
    if blocks == 0 {
        return Err(format!("{}: too small to test", path.display()));
    }
    let t = Instant::now();
    let total = std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|i| {
                let f = &f;
                s.spawn(move || {
                    let mut v = Vec::new();
                    let buf = aligned(&mut v, 4096);
                    // xorshift: any spread of offsets will do, no crate needed.
                    let mut x = (t.elapsed().as_nanos() as u64 ^ 0x9e37_79b9_7f4a_7c15)
                        .wrapping_mul(i as u64 + 1)
                        | 1;
                    let mut n = 0u64;
                    while t.elapsed() < time {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        f.read_at(buf, (x % blocks) * 4096)
                            .map_err(|e| e.to_string())?;
                        n += 1;
                    }
                    Ok::<_, String>(n)
                })
            })
            .collect();
        hs.into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err("read thread crashed".into()))
            })
            .sum::<Result<u64, String>>()
    })?;
    Ok(total as f64 / t.elapsed().as_secs_f64())
}

#[cfg(test)]
#[path = "../tests/unit/bench.rs"]
mod tests;
