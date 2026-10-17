// goldenram.rs — what the stages download as tftp/broom-win/<name>/golden.vhdx. It is not a file: the VHDX is made
// on the fly from image.img (vhdx::Virtual — header in memory, payload read straight from the raw disk), so a publish
// never writes a 50 GB copy. Cache mode RAM: the same bytes held compressed in RAM (iscsid::ramimg, the same 16 KB
// zstd blocks as the Linux RAM targets) — after a publish a whole room fetches the new golden at once, 30 streams at
// 30 offsets would keep the disk seeking for an hour; from RAM it is never touched.
// The golden being served keeps image.img open: a new upload / rollback renamed over it changes nothing until the
// publish that installs the new golden. An image published by an older version (a real golden.vhdx file, no
// golden.id) is served from that file until it is published again.
//
// Download queue: TCP shares the link evenly, so a whole room downloading at once would all finish at the very end.
// At most `limit` downloads run at once instead (each at a client's full speed), so machines finish one batch after
// the other. The stage asks GET /api/golden-slot until it says "go"; a machine is known by its IP.
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::SharedState;
use crate::iscsid::ramimg::RamImage;

/// The golden an image serves right now.
pub struct Golden {
    /// hash::stamp of image.img when it was built (saved as golden.id): the same id → the same bytes and sha256.
    pub id: String,
    pub vhdx: Arc<crate::vhdx::Virtual>,
    ram: Option<Arc<RamImage>>,
}

impl Golden {
    fn size(&self) -> u64 {
        self.vhdx.len()
    }
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        match &self.ram {
            Some(r) => r.read_at(off, buf),
            None => self.vhdx.read_at(off, buf),
        }
    }
}

static SERVED: LazyLock<Mutex<HashMap<String, Arc<Golden>>>> = LazyLock::new(Default::default);

fn served(name: &str) -> Option<Arc<Golden>> {
    SERVED.lock().get(name).cloned()
}

/// Id of the golden an image serves (None: none built by this version yet).
pub fn served_id(name: &str) -> Option<String> {
    served(name).map(|g| g.id.clone())
}

/// Serve this golden from now on (publish, start); no RAM copy yet (set_ram).
pub fn install(name: &str, id: &str, vhdx: crate::vhdx::Virtual) {
    SERVED.lock().insert(
        name.to_string(),
        Arc::new(Golden {
            id: id.to_string(),
            vhdx: Arc::new(vhdx),
            ram: None,
        }),
    );
}

/// Make the served golden's RAM copy follow the cache mode: `ram` → loaded (kept if there), else dropped. Blocking
/// (~a minute for 12 GB). Err = refused (RAM) or failed: the caller goes back to disk.
pub fn set_ram(st: &SharedState, name: &str, ram: bool) -> Result<(), String> {
    let g = served(name).ok_or("golden not built yet")?;
    if ram == g.ram.is_some() {
        return Ok(());
    }
    let copy = if ram {
        let v = g.vhdx.clone();
        let blocks = v.len().div_ceil(crate::iscsid::ramimg::BLOCK as u64);
        Some(Arc::new(RamImage::load_with(
            name,
            v.len(),
            blocks,
            crate::publish::ram_reserve(st),
            |off, buf| v.read_at(off, buf),
        )?))
    } else {
        None
    };
    let mut s = SERVED.lock();
    // Only onto the golden it was made from (jobs of an image never overlap, so it is still that one).
    if s.get(name).is_some_and(|cur| cur.id == g.id) {
        s.insert(
            name.to_string(),
            Arc::new(Golden {
                id: g.id.clone(),
                vhdx: g.vhdx.clone(),
                ram: copy,
            }),
        );
    }
    Ok(())
}

/// The image is gone.
pub fn forget(name: &str) {
    SERVED.lock().remove(name);
}

fn dir(name: &str) -> PathBuf {
    crate::tftp_dir().join("broom-win").join(name)
}

/// At start: the golden of every published Windows image, made again from image.img (same id → same bytes, so no
/// client downloads again), plus its RAM copy when set to RAM. image.img changed since (an upload whose publish did not
/// finish) → not served until published.
pub fn restore_all(st: &SharedState) {
    refresh_limit(st);
    for img in st.db.images().unwrap_or_default() {
        if img.os != "windows" || img.boot_script.is_none() {
            continue;
        }
        let raw = crate::images_dir().join(&img.name).join("image.img");
        let Some(id) = crate::hash::stamp(&raw) else {
            continue;
        };
        if std::fs::read_to_string(dir(&img.name).join("golden.id"))
            .ok()
            .as_deref()
            .map(str::trim)
            != Some(id.as_str())
        {
            continue; // published by an older version (golden.vhdx file) or image.img changed since
        }
        match crate::vhdx::Virtual::open(&raw, &id) {
            Ok(v) => install(&img.name, &id, v),
            Err(e) => {
                tracing::error!("image {}: golden not served: {e}", img.name);
                continue;
            }
        }
        if img.cache_mode == "zram" {
            match set_ram(st, &img.name, true) {
                Ok(()) => tracing::info!("image {}: golden.vhdx in RAM", img.name),
                Err(e) => {
                    tracing::warn!(
                        "image {}: golden not loaded into RAM ({e}) → cache_mode=disk",
                        img.name
                    );
                    let _ = st.db.set_cache_mode(img.id, "disk");
                }
            }
        }
    }
}

/// A place granted ("go") and not used yet, or left by a cut download (wget -c resumes it): kept this long.
const HOLD: Duration = Duration::from_secs(60);
/// A waiting machine that stopped asking for this long (powered off, gave up) leaves the queue.
const STALE: Duration = Duration::from_secs(30);
/// Highest number of downloads at once an admin can set.
pub const MAX_STREAMS: usize = 256;

#[derive(Default)]
pub struct Queue {
    /// Downloads at once; 0 = no queue.
    limit: usize,
    /// Downloads running, per machine.
    active: HashMap<IpAddr, usize>,
    held: HashMap<IpAddr, Instant>,
    waiting: VecDeque<(IpAddr, Instant)>,
}

#[derive(Debug, PartialEq)]
pub enum Slot {
    Go,
    /// Place in the queue (1 = next), machines waiting.
    Wait(usize, usize),
}

impl Queue {
    fn expire(&mut self, now: Instant) {
        self.held.retain(|_, t| now.duration_since(*t) < HOLD);
        self.waiting.retain(|(_, t)| now.duration_since(*t) < STALE);
    }

    /// The stage asks for its turn.
    fn decide(&mut self, ip: IpAddr, now: Instant) -> Slot {
        self.expire(now);
        if self.limit == 0 || self.active.contains_key(&ip) {
            return Slot::Go;
        }
        if let Some(t) = self.held.get_mut(&ip) {
            *t = now;
            return Slot::Go;
        }
        let free = self
            .limit
            .saturating_sub(self.active.len() + self.held.len());
        let pos = match self.waiting.iter().position(|w| w.0 == ip) {
            Some(p) => {
                self.waiting[p].1 = now;
                p
            }
            None => {
                self.waiting.push_back((ip, now));
                self.waiting.len() - 1
            }
        };
        if pos < free {
            self.waiting.remove(pos);
            self.held.insert(ip, now);
            return Slot::Go;
        }
        Slot::Wait(pos + 1, self.waiting.len())
    }

    /// A download starts: only with a place (or no queue).
    fn start(&mut self, ip: IpAddr, now: Instant) -> bool {
        self.expire(now);
        if self.limit != 0 && !self.active.contains_key(&ip) && self.held.remove(&ip).is_none() {
            return false;
        }
        *self.active.entry(ip).or_default() += 1;
        true
    }

    /// A download ended; `done` = all of it sent, else the place is held for the resume.
    fn finish(&mut self, ip: IpAddr, now: Instant, done: bool) {
        if let Some(n) = self.active.get_mut(&ip) {
            *n -= 1;
            if *n == 0 {
                self.active.remove(&ip);
            }
        }
        if !done && !self.active.contains_key(&ip) {
            self.held.insert(ip, now);
        }
    }
}

static QUEUE: LazyLock<Mutex<Queue>> = LazyLock::new(Default::default);

/// Downloads at once for config value `cfg`: a number (0 = no queue, at most MAX_STREAMS), else auto = the server's
/// link speed over a client's 1 Gbit/s, 4–64; 8 when the speed is unknown (virtual NIC).
pub fn limit_for(cfg: &str, link_mbit: Option<u64>) -> usize {
    match cfg.trim().parse::<usize>() {
        Ok(n) => n.min(MAX_STREAMS),
        Err(_) => link_mbit.map_or(8, |m| (m / 1000).clamp(4, 64) as usize),
    }
}

/// Config value (default "auto"), the limit it gives now, and the boot NIC with its speed.
pub fn streams(st: &SharedState) -> (String, usize, Option<(String, Option<u64>)>) {
    let cfg = st.db.get_config("golden_streams", "auto");
    let link = crate::sysmon::boot_link(&st.db.get_config("dhcp_iface", ""));
    let n = limit_for(&cfg, link.as_ref().and_then(|l| l.1));
    (cfg, n, link)
}

/// Read the limit again (start, settings change, each slot request: a NIC change or a bond port follows by itself).
pub fn refresh_limit(st: &SharedState) {
    let n = streams(st).1;
    QUEUE.lock().limit = n;
}

/// (downloads running, machines waiting).
pub fn queue_stats() -> (usize, usize) {
    let q = QUEUE.lock();
    (q.active.values().sum(), q.waiting.len())
}

/// Golden bytes sent to each client since the server started (the per-machine traffic on the Machines page).
static SENT: LazyLock<Mutex<HashMap<IpAddr, u64>>> = LazyLock::new(Default::default);

fn count(ip: IpAddr, n: usize) {
    *SENT.lock().entry(ip).or_default() += n as u64;
}

/// Bytes sent per client IP so far (counters only grow; sysmon turns them into a rate).
pub fn sent() -> HashMap<IpAddr, u64> {
    SENT.lock().clone()
}

/// GET /api/golden-slot (public: the Windows stage, before it downloads a golden) → "go" or "wait <place> <of>".
pub async fn slot(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> String {
    refresh_limit(&st);
    match QUEUE.lock().decide(peer.ip(), Instant::now()) {
        Slot::Go => "go\n".into(),
        Slot::Wait(p, n) => format!("wait {p} {n}\n"),
    }
}

/// One running download; gives its place back when the response body is dropped (sent, or the client went away).
struct Permit {
    ip: IpAddr,
    done: AtomicBool,
}

impl Drop for Permit {
    fn drop(&mut self) {
        QUEUE
            .lock()
            .finish(self.ip, Instant::now(), self.done.load(Ordering::Relaxed));
    }
}

/// One byte range of `Range: bytes=...` → (first, last) inclusive. None = no / multi / unsupported range → whole file.
/// Err = not satisfiable (416).
fn range(h: Option<&HeaderValue>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = h
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("bytes="))
    else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return Ok(None);
    };
    let (first, last) = match (a.parse::<u64>().ok(), b.parse::<u64>().ok()) {
        (Some(a), Some(b)) => (a, b.min(size.saturating_sub(1))),
        (Some(a), None) if b.is_empty() => (a, size.saturating_sub(1)),
        (None, Some(n)) if a.is_empty() && n > 0 => {
            (size.saturating_sub(n), size.saturating_sub(1))
        }
        _ => return Ok(None),
    };
    if first >= size || first > last {
        return Err(());
    }
    Ok(Some((first, last)))
}

/// A place for this download; None = no place (the stage asks /api/golden-slot first; this only enforces it).
fn permit(ip: IpAddr) -> Option<Arc<Permit>> {
    QUEUE.lock().start(ip, Instant::now()).then(|| {
        Arc::new(Permit {
            ip,
            done: AtomicBool::new(false),
        })
    })
}

fn queue_full() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "10")],
        "download queue full: ask /api/golden-slot\n",
    )
        .into_response()
}

/// GET /tftp/broom-win/{name}/golden.vhdx — the served golden (RAM copy or made from image.img); an image published
/// by an older version: its golden.vhdx file.
pub async fn get(
    Path(name): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    let Some(g) = crate::images::valid_name(&name)
        .then(|| served(&name))
        .flatten()
    else {
        use futures_util::StreamExt;
        use tower::ServiceExt;
        let Some(p) = permit(peer.ip()) else {
            return queue_full();
        };
        return match tower_http::services::ServeFile::new(dir(&name).join("golden.vhdx"))
            .oneshot(req)
            .await
        {
            Ok(r) => {
                // The place goes with the body (no "done": a short file is rare enough to hold it a minute).
                let (parts, body) = r.into_response().into_parts();
                let body = body.into_data_stream().map(move |c| {
                    if let Ok(b) = &c {
                        count(p.ip, b.len());
                    }
                    c
                });
                Response::from_parts(parts, Body::from_stream(body))
            }
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    };
    serve(g, req.headers(), peer.ip())
}

fn serve(img: Arc<Golden>, headers: &HeaderMap, ip: IpAddr) -> Response {
    let size = img.size();
    let (status, first, last) = match range(headers.get(header::RANGE), size) {
        Ok(Some((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b),
        Ok(None) => (StatusCode::OK, 0, size.saturating_sub(1)),
        Err(()) => {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{size}"))],
            )
                .into_response();
        }
    };
    let len = if size == 0 { 0 } else { last - first + 1 };
    let Some(p) = permit(ip) else {
        return queue_full();
    };
    if len == 0 {
        p.done.store(true, Ordering::Relaxed);
    }
    // 1 MB at a time, read (disk) / unpacked (RAM) off the async threads; the next MB is read while one is sent.
    use futures_util::StreamExt;
    let end = first + len;
    let body = futures_util::stream::iter((first..end).step_by(1 << 20))
        .map(move |at| {
            let img = img.clone();
            let n = (end - at).min(1 << 20) as usize;
            if at + n as u64 == end {
                p.done.store(true, Ordering::Relaxed);
            }
            count(p.ip, n);
            async move {
                tokio::task::spawn_blocking(move || {
                    let mut buf = vec![0u8; n];
                    img.read_at(at, &mut buf).map(|_| buf)
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r)
                .map_err(std::io::Error::other)
            }
        })
        .buffered(2);
    let mut r = Response::new(Body::from_stream(body));
    *r.status_mut() = status;
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if status == StatusCode::PARTIAL_CONTENT {
        h.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {first}-{last}/{size}")).unwrap(),
        );
    }
    r
}

#[cfg(test)]
#[path = "../tests/unit/goldenram.rs"]
mod tests;
