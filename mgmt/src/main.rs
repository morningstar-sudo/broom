// main.rs — bootrom mgmt app (Rust/axum). One binary, runs on the Linux server.
// Modules: boot (iPXE menu), dhcp/tftp/iscsi (network boot), images + golden/hash/versions/export (images), publish
// (Linux) + winstage (Windows), machines/devices/license/settings (web API), auth, disk/archive/vmdk/vhdx (formats).
mod agentauth;
mod api;
mod archive;
mod assets;
mod auth;
mod bench;
mod boot;
mod capture;
mod db;
mod dedup;
mod devices;
mod dhcp;
mod disk;
mod disks;
mod dns;
mod dnsrec;
mod drivers;
mod export;
#[cfg(test)]
#[path = "../tests/unit/fuzz.rs"]
mod fuzz;
mod games;
mod golden;
mod goldenram;
mod groups;
mod hash;
mod images;
mod iscsi;
mod iscsid;
mod license;
mod linuxfs;
mod logbuf;
mod machines;
mod metrics;
mod monitor;
mod ntfsread;
mod overlay;
mod preflight;
mod publish;
mod pwproof;
mod settings;
mod setup;
mod sysmon;
mod tftp;
mod users;
mod versions;
mod vhdx;
mod vmdk;
mod vnc;
mod volumes;
mod winstage;
mod wol;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::{Router, response::Html, routing::get};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

/// The static musl build's own malloc takes a global lock: with every tokio worker allocating (iSCSI reads, HTTP),
/// it halves the iSCSI daemon's throughput. mimalloc keeps per-thread heaps.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Home of all server data = the binary's own directory (wherever it is started from); override with
/// BOOTROM_HOME. Layout: bootrom.db, images/<name>/image.img, storage/ (versions), tftp/ (boot files).
pub fn home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| match std::env::var("BOOTROM_HOME") {
        Ok(h) => std::path::absolute(&h).unwrap_or_else(|_| PathBuf::from(h)), // absolute: shell steps cd elsewhere
        Err(_) => std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from(".")),
    })
}

/// Directory holding the images, one subfolder per image: `<images_dir>/<name>/image.img`.
/// Default `<home>/images`; override with BOOTROM_IMAGES_DIR (e.g. a bigger disk).
pub fn images_dir() -> PathBuf {
    std::env::var("BOOTROM_IMAGES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join("images"))
}

/// Boot files served over HTTP /tftp/... and TFTP: broom/<name> (Linux kernel/initrd),
/// broom-win/<name> (golden.vhdx + templates), broom-stage (Windows stage).
pub fn tftp_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| home().join("tftp"))
}

/// Scratch space for publish steps (initrd builds, mount points): <home>/work.
pub fn work_dir() -> PathBuf {
    home().join("work")
}

/// Temp files a job leaves when the server stops midway (crash, power loss, restart). At start no job runs yet, so
/// every one of them is a leftover: removed before anything else (they can be GBs).
fn clean_leftovers() {
    let rm = |p: &Path| {
        let r = if p.is_dir() {
            std::fs::remove_dir_all(p)
        } else {
            std::fs::remove_file(p)
        };
        if r.is_ok() {
            info!("removed leftover {}", p.display());
        }
    };
    let each = |dir: &Path, keep: &dyn Fn(&str) -> bool| {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if !keep(&e.file_name().to_string_lossy()) {
                rm(&e.path());
            }
        }
    };
    // images/<name>/image.img.new (convert), image.img.bhash.tmp (dedup index), tftp/broom-win/<name>/golden.vhdx.tmp
    // (publish). (upload/ itself is kept: an upload refused by its publish waits there for a retry; its unzip/ is a
    // convert temp.)
    for e in std::fs::read_dir(images_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        rm(&e.path().join("image.img.new"));
        rm(&e.path().join("image.img.bhash.tmp"));
        rm(&e.path().join("image.img.new.bhash")); // a golden rebuilt in one piece (dedup::make_whole)
        rm(&e.path().join("upload").join("unzip"));
    }
    each(&tftp_dir().join("broom-drivers"), &|n| !n.ends_with(".tmp")); // driver archive being written
    for e in std::fs::read_dir(tftp_dir().join("broom-win"))
        .into_iter()
        .flatten()
        .flatten()
    {
        rm(&e.path().join("golden.vhdx.tmp"));
    }
    // tftp/broom/<name>.new|.old (Linux publish swap); tftp/broom-stage.new-*|.old-* (stage bundle install).
    each(&tftp_dir().join("broom"), &|n| {
        !(n.ends_with(".new") || n.ends_with(".old"))
    });
    each(tftp_dir(), &|n| {
        !(n.starts_with("broom-stage.new") || n.starts_with("broom-stage.old"))
    });
    // work/: bundle downloads, driver upload/extract dirs, the stage build's initramfs config, half-built exports.
    each(&work_dir(), &|n| {
        !((n.starts_with("broom-stage-") && n.ends_with(".tar.gz"))
            || n.starts_with("drvup.")
            || n.starts_with("drvex.")
            || n == "stage-conf")
    });
    each(&work_dir().join("export"), &|n| !n.ends_with(".new"));
}

/// Version = the build time in UTC (BROOM_VERSION at compile time: the release tag in CI, the build time in build.sh),
/// e.g. 2026-10-19T08-30-00Z — shown on the web + in logs to tell deployed builds apart.
pub(crate) const VERSION: &str = match option_env!("BROOM_VERSION") {
    Some(v) => v,
    None => "dev",
};
// Web admin (assets.rs: embedded, or the release's broom-assets.zip) — deploy a single file, no static/ directory.
// index.html loads /app.js?v=<version> + /app.css?v=<version>, so a new build is never served from cache. /login is the
// standalone login/setup page (auth.rs); unauthenticated page requests are redirected there.
async fn index() -> Html<String> {
    Html(assets::text("static/index.html").replace("__VERSION__", VERSION))
}
/// The web admin's files: `/app.js` → static/app.js, … Bootstrap 5.3.8 and Chart.js 4.4.7 (MIT) are vendored as released:
/// no CDN (offline servers, CSP 'self'). `no-cache`: after an upgrade a browser never runs an old file, also where it
/// is loaded without `?v=` (the login page).
async fn asset(uri: axum::http::Uri) -> impl axum::response::IntoResponse {
    use axum::http::header;
    let name = uri.path().trim_start_matches('/');
    let ctype = match name.rsplit('.').next() {
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        _ => "image/svg+xml",
    };
    (
        [
            (header::CONTENT_TYPE, ctype),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        assets::text(&format!("static/{name}")),
    )
}
async fn login_page() -> Html<&'static str> {
    Html(assets::text("static/login.html"))
}

/// GET /broom-capture — this very binary: the stage runs it as `bootrom-mgmt capture` (boot menu [C], capture.rs), so
/// the tool is always this server's version. Only a static (musl) build runs inside the stage.
async fn capture_bin() -> axum::response::Response {
    use axum::response::IntoResponse;
    if !cfg!(target_env = "musl") {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Capture needs the release (static) build of bootrom-mgmt",
        )
            .into_response();
    }
    match tokio::fs::read("/proc/self/exe").await {
        Ok(b) => (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            b,
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("read own binary: {e}"),
        )
            .into_response(),
    }
}

/// Server events (SSE): "ping" on connect + after 5 s without any other event (keep-alive) — the sidebar dot
/// counts every event as "server up" and marks it offline on a connection error or when events stop arriving
/// ("res" alone comes every 2 s, so pings are rare); "job" {name,status} on every
/// image job change (publish / snapshot / rollback steps) — replaces polling /api/images/job.
async fn events(
    axum::extract::State(st): axum::extract::State<SharedState>,
    headers: axum::http::HeaderMap,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use futures_util::stream::{self, StreamExt};
    use tokio::sync::broadcast::error::RecvError;
    // The guard checked the session when the stream opened; it stays open for hours, so it checks again every 30 s
    // and ends with a "logout" event once the session is gone (expired, or the password changed elsewhere).
    let gone = auth::until_invalid(st.clone(), auth::session_token(&headers));
    let ping = || Event::default().event("ping").data(VERSION);
    let jobs = stream::unfold(st.job_tx.subscribe(), |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok((name, status)) => {
                    let data = serde_json::json!({"name": name, "status": status}).to_string();
                    return Some((Ok(Event::default().event("job").data(data)), rx));
                }
                Err(RecvError::Lagged(_)) => continue, // slow tab missed a step: the next one catches up
                Err(RecvError::Closed) => return None,
            }
        }
    });
    // "res": the server's resources every 2 s (sysmon.rs) — the alert banner + the Server page; the last one at once.
    let latest = sysmon::LATEST.lock().clone();
    let first = (!latest.is_empty()).then(|| Ok(Event::default().event("res").data(latest)));
    let res = stream::unfold(sysmon::RES.subscribe(), |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(data) => return Some((Ok(Event::default().event("res").data(data)), rx)),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(
        stream::once(async move { Ok(ping()) })
            .chain(stream::iter(first))
            .chain(stream::select(jobs, res))
            .take_until(gone)
            .chain(stream::once(async {
                Ok(Event::default().event("logout").data("session ended"))
            })),
    )
    .keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(5))
            .event(ping()),
    )
}

/// The web admin's pages (one URL each, + their fragment /ui/<page>).
const PAGES: [&str; 13] = [
    "machines",
    "images",
    "network",
    "system",
    "drivers",
    "devices",
    "groups",
    "benchmark",
    "remote",
    "server",
    "games",
    "windows",
    "logs",
];

// Per-tab fragments — loaded on demand via /ui/<page> (the web only fetches the tab being viewed).
async fn ui_page(axum::extract::Path(p): axum::extract::Path<String>) -> Html<&'static str> {
    Html(match p.as_str() {
        p if PAGES.contains(&p) => assets::text(&format!("static/page-{p}.html")),
        _ => "",
    })
}

/// Shared state.
pub struct AppState {
    /// Storage driver (db/): SQLite built in, others plug in behind the same trait.
    pub db: Box<dyn db::Db>,
    /// Publish job status by image name: "⏳ ..." running | "✓ ..." done | "✗ ..." failed.
    pub jobs: Mutex<std::collections::HashMap<String, String>>,
    /// Job changes (name, status) → SSE "job" events.
    pub job_tx: tokio::sync::broadcast::Sender<(String, String)>,
    /// Serializes the shared image-version store (versions.rs): snapshot/rollback/delete/gc must not overlap
    /// (gc could free a chunk another op just decided to reuse).
    pub versions_lock: Mutex<()>,
    /// Running network boot listeners (DHCP/TFTP) — dhcp::start() replaces them on config change.
    pub net: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// mac → unix time of its last PXE boot (menu choice /boot/start, or the Windows stage's driver query): see saw_pxe.
    pub pxe_seen: Mutex<std::collections::HashMap<String, u64>>,
    /// mac → the IP its last menu choice came from (same entries as pxe_seen): with the DHCP server off this is how
    /// new machines are found (Machines page).
    pub pxe_ip: Mutex<std::collections::HashMap<String, String>>,
    /// mac → unix time Windows reported a boot WITHOUT a PXE boot just before (session not reset). Cleared by the
    /// next PXE boot. Shown on the Machines page.
    pub not_reset: Mutex<std::collections::HashMap<String, u64>>,
    /// mac → (bytes free, unix time) Windows reported its SSD (BROOMWIN) running out of room during a session
    /// (broom-watch.ps1). Cleared by the next PXE boot. Shown on the Machines page.
    pub ssd_low: Mutex<std::collections::HashMap<String, (u64, u64)>>,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
pub type SharedState = Arc<AppState>;

/// How long a PXE boot is remembered (the longest window anything checks: the license hand-out).
pub const PXE_MEMORY_S: u64 = 30 * 60;
/// Most MACs remembered at once (a room is far below it).
const PXE_MAX: usize = 4096;

impl AppState {
    /// `mac` just went through PXE (menu choice / the stage's driver query). Older entries are dropped on the way:
    /// both callers are public endpoints, so the map must not grow with every MAC someone makes up.
    pub fn saw_pxe(&self, mac: &str, ip: Option<&str>) {
        let now = now_secs();
        let mut seen = self.pxe_seen.lock();
        seen.retain(|_, t| now.saturating_sub(*t) <= PXE_MEMORY_S);
        // Bounded: both callers are public, a flood of made-up MACs must not grow it (and every scan of it) for 30 min.
        if seen.len() >= PXE_MAX
            && !seen.contains_key(mac)
            && let Some(oldest) = seen.iter().min_by_key(|(_, t)| **t).map(|(m, _)| m.clone())
        {
            seen.remove(&oldest);
        }
        seen.insert(mac.to_string(), now);
        let mut ips = self.pxe_ip.lock();
        ips.retain(|m, _| seen.contains_key(m));
        if let Some(ip) = ip {
            ips.insert(mac.to_string(), ip.to_string());
        }
        drop((seen, ips));
        self.not_reset.lock().remove(mac);
        self.ssd_low.lock().remove(mac);
        metrics::forget(mac);
    }

    /// A job (publish, snapshot, export…) is running on image `name`.
    pub fn job_running(&self, name: &str) -> bool {
        self.jobs
            .lock()
            .get(name)
            .is_some_and(|s| s.starts_with('⏳'))
    }

    /// Record an image job status + push it to open web tabs.
    pub fn set_job(&self, name: &str, status: String) {
        self.jobs.lock().insert(name.to_string(), status.clone());
        let _ = self.job_tx.send((name.to_string(), status)); // Err = no tab listening
    }
}

#[tokio::main]
async fn main() {
    init_logging();
    sysmon::raise_fd_limit(); // before the iSCSI daemon / TFTP size their limits from it
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("install-service") => setup::install_service(&args),
        Some("build-stage") => winstage::build_bundle(&args), // CI: the Windows stage bundle
        Some("iscsid") => iscsid::run(&args).await, // the iSCSI target daemon (started by iscsi.rs)
        Some("pack-assets") => assets::pack(&args), // CI: broom-assets.zip for the release binary
        Some("pins") => assets::print_pins(),       // CI: the bundle pins this binary reads
        Some("capture") => capture::run(&args), // on a client (boot menu [C]): its disk → an image
        _ => {}
    }

    let skip_preflight = args.iter().any(|a| a == "--skip-preflight");
    let port: u16 = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(80);
    // Bind HTTP FIRST: the UDP listeners use SO_REUSEPORT (hot Apply), so a second instance started by mistake would
    // happily share :67/:69 — it must die here, before it stops services, touches the DB or rebuilds iSCSI targets.
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            error!("bind http {addr}: {e} (another bootrom-mgmt already running?)");
            std::process::exit(1)
        });

    info!("data in {}", home().display());
    // One instance per data directory: a second one on another --port gets past the HTTP bind above, and would then
    // delete the running one's temp files and share :67/:69 with it (SO_REUSEPORT). The kernel drops the lock when the
    // process dies, so a crash never leaves it stuck.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home().join("bootrom.lock"));
    // SAFETY: flock on a valid open fd.
    let locked = lock.as_ref().is_ok_and(|f| unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(f), libc::LOCK_EX | libc::LOCK_NB) } == 0);
    if !locked {
        error!(
            "another bootrom-mgmt is already running on {} — stop it first",
            home().display()
        );
        std::process::exit(1);
    }
    std::mem::forget(lock); // held for the life of the process
    clean_leftovers(); // no other instance (and so no job) can be running
    // Web UI, iPXE, scripts: a release binary loads (first start: downloads) its broom-assets.zip. Without them clients
    // would get no iPXE → refuse to start, saying how to fix it.
    match tokio::task::spawn_blocking(assets::init)
        .await
        .unwrap_or_else(|e| Err(e.to_string()))
    {
        Ok(s) => info!("{s}"),
        Err(e) => {
            error!("{e}");
            std::process::exit(1);
        }
    }

    // Preflight (root; kernel modules only warn) → stop if it fails. Network never configured → setup detects it and
    // seeds the DHCP config (dev: --skip-preflight skips both).
    if !skip_preflight {
        if let Err(e) = preflight::run() {
            error!("preflight: {e}");
            std::process::exit(1);
        }
        let unconfigured = db::open(&db::url())
            .map(|d| d.get_config("dhcp_server_ip", "").is_empty())
            .unwrap_or(true);
        if unconfigured {
            warn!("network not configured yet → running setup");
            setup::run(&args);
        }
        info!("preflight passed");
    }

    let database = db::open(&db::url()).unwrap_or_else(|e| {
        error!("database: {e}");
        std::process::exit(1)
    });
    std::fs::create_dir_all(images_dir()).ok();
    std::fs::create_dir_all(tftp_dir()).ok();
    let state: SharedState = Arc::new(AppState {
        db: database,
        jobs: Mutex::new(std::collections::HashMap::new()),
        job_tx: tokio::sync::broadcast::channel(64).0,
        versions_lock: Mutex::new(()),
        net: Mutex::new(Vec::new()),
        pxe_seen: Mutex::new(std::collections::HashMap::new()),
        pxe_ip: Mutex::new(std::collections::HashMap::new()),
        not_reset: Mutex::new(std::collections::HashMap::new()),
        ssd_low: Mutex::new(std::collections::HashMap::new()),
    });
    auth::init_setup_token(&state);
    // Broom disks formatted here mounted again, before anything on them is served.
    if preflight::is_root() {
        volumes::mount_all(&state);
    }

    // Built-in DHCP server + TFTP + iSCSI targets.
    if preflight::is_root() {
        match dhcp::start(&state).await {
            Ok(s) => info!("{s}"),
            Err(e) => error!("DHCP/TFTP not started: {e} (fix it on the Network page → Apply)"),
        }
        // Stage bundle checked + fetched if missing (background: a download), then its Secure Boot shim → tftp/shim/.
        tokio::task::spawn_blocking(publish::prepare_stage);
        // The Windows scripts of this version for every image (stage: tftp/broom-scripts/).
        if let Err(e) = winstage::install_scripts() {
            error!("Windows scripts not installed: {e}");
        }
        // The iSCSI daemon (its own process: survives this one's restarts) checked / started, missing targets restored
        // (background: a RAM copy is slow).
        let st = state.clone();
        tokio::task::spawn_blocking(move || publish::start_iscsi(&st));
        // Windows goldens: made again from image.img (same bytes as before the restart) + their RAM copies when set to
        // RAM (they live in this process).
        let st = state.clone();
        tokio::task::spawn_blocking(move || goldenram::restore_all(&st));
        // Prune expired DHCP leases every 10 min so a MAC flood can't grow the table without bound.
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                let now = crate::now_secs() as i64;
                match st.db.prune_leases(now) {
                    Ok(n) if n > 0 => info!("pruned {n} expired DHCP lease(s)"),
                    Err(e) => warn!("lease prune: {e}"),
                    _ => {}
                }
                bench::prune(&st);
            }
        });
        // Superseded iSCSI generations (+ their RAM copies) go as soon as their last client logs out — a busy lab never
        // has a moment with no client at all, and a publish only cleans up what is idle at that moment. Images with
        // a running job are skipped (their old generation is still in the boot script until the publish saves it).
        // Same pass: an iSCSI daemon of an older build is replaced once no client uses it; the games disk settings are
        // pushed again (the update machine's lease may have moved).
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let st = st.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    iscsi::upgrade_if_idle();
                    let _ = games::sync(&st); // logs its own errors
                    for img in st
                        .db
                        .images()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|i| i.os == "linux")
                    {
                        let busy = st.job_running(&img.name);
                        if !busy {
                            publish::gc_superseded(&st, &img.name);
                        }
                    }
                })
                .await;
            }
        });
    }

    // Server resources every 2 s → web pages (alert banner over the threshold, Server page).
    tokio::spawn(sysmon::run(state.clone()));

    let app = Router::new()
        .route("/login", get(login_page)) // standalone sign-in page (auth.rs); public
        .route("/login.js", get(asset))
        .route("/crypto.js", get(asset))
        .route("/", get(index)) // web admin shell (embedded in the binary)
        .route("/app.js", get(asset))
        .route("/app.css", get(asset)) // public: the login page uses it too
        .route("/bootstrap.min.css", get(asset)) // public too
        .route("/bootstrap.bundle.min.js", get(asset))
        .route("/chart.umd.min.js", get(asset))
        .route("/icons.svg", get(asset)) // public: the login page uses it too
        // One URL per page (F5 / bookmarks keep the page); same shell, JS picks the page from the path.
        .merge(
            PAGES
                .iter()
                .fold(Router::new(), |r, p| r.route(&format!("/{p}"), get(index))),
        )
        .route("/ui/{page}", get(ui_page)) // fragment tab on-demand
        .route("/api/events", get(events)) // SSE: server liveness (sidebar dot) + image job status
        .route("/boot.ipxe", get(boot::render)) // iPXE menu
        .route("/boot/start", get(boot::start)) // menu choice → "client started" log + image boot script
        .route("/boot/capture", get(boot::capture)) // menu [C]: the stage in capture mode
        .route("/broom-capture", get(capture_bin)) // this binary, for that stage
        .merge(images::routes()) // images, versions, upload, publish
        .merge(drivers::routes()) // Windows driver packages
        .merge(games::routes()) // games disks (Windows)
        .merge(groups::routes()) // Groups page
        .merge(machines::routes()) // Machines table
        .merge(license::routes()) // Windows license keys + boot reports
        .merge(settings::routes()) // boot menu, zram, DHCP, guest user
        .merge(devices::routes()) // Devices page: edit / bulk / CSV / detail
        .merge(monitor::routes()) // online status, Wake-on-LAN
        .merge(bench::routes()) // Benchmark page + client measurements
        .merge(vnc::routes()) // Remote page: screen + mouse/keys of a Windows machine
        .merge(agentauth::routes()) // Remote: agent keys (handed to the stage, re-issued)
        .merge(logbuf::routes()) // Logs page: the newest log lines
        .merge(sysmon::routes()) // Server page: services up or down
        .merge(disks::routes()) // client disks: inventory, Broom disk, hide the others
        .merge(dns::routes()) // DNS server for healthy machines (Network page)
        .merge(volumes::routes()) // server disks: format, mount, images / games disks on them
        .merge(auth::routes()) // admin login
        .merge(users::routes()) // accounts (owners)
        // Serve boot assets over HTTP (kernel/initrd much faster than TFTP).
        // /tftp/... -> <home>/tftp/... (e.g. http://SERVER/tftp/broom-stage/vmlinuz); a Windows golden set to RAM
        // from its RAM copy.
        .route("/tftp/broom-win/{name}/golden.vhdx", get(goldenram::get))
        .route("/api/golden-slot", get(goldenram::slot))
        // 1 MB reads (default 64 KB): every boot of every client fetches its kernel + initrd here.
        .nest_service(
            "/tftp",
            ServeDir::new(tftp_dir()).with_buf_chunk_size(1 << 20),
        )
        // Guard EVERYTHING: Host check + a session for non-public routes (auth.rs::is_public lists the open ones).
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::guard,
        ))
        .layer(axum::middleware::from_fn(auth::security_headers)) // outermost: also on the guard's redirects / 401s
        .with_state(state);

    info!("bootrom-mgmt {VERSION} serving on http://{addr}");
    // ConnectInfo: /api/license and /api/booted identify the machine by the peer IP (license.rs).
    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    {
        error!("http server stopped: {e}");
    }
}

/// Events (client boots, publish steps, DHCP/TFTP, errors) → stdout; warnings/errors → stderr.
/// Level via RUST_LOG=error|warn|info|debug|trace (default info; debug also shows every external
/// command run). Colors only on a terminal (journald/systemd get plain text).
fn init_logging() {
    use std::io::IsTerminal;
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.parse::<tracing_subscriber::filter::LevelFilter>().ok())
        .unwrap_or(tracing_subscriber::filter::LevelFilter::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(
            std::io::stderr
                .with_max_level(tracing::Level::WARN)
                .or_else(std::io::stdout)
                .and(logbuf::Make), // + the Logs page's copy
        )
        .with_ansi(std::io::stdout().is_terminal())
        .with_target(false)
        .init();
}
