// main.rs — bootrom mgmt app (Rust/axum). One binary, runs on the Linux server.
// Modules: boot (iPXE menu), dhcp/tftp/iscsi (network boot), images + golden/hash/versions/export (images), publish
// (Linux) + winstage (Windows), machines/devices/license/settings (web API), auth, disk/archive/vmdk/vhdx (formats).
mod api;
mod archive;
mod auth;
mod boot;
mod db;
mod devices;
mod dhcp;
mod disk;
mod drivers;
mod export;
mod golden;
mod hash;
mod images;
mod iscsi;
mod license;
mod linuxfs;
mod machines;
mod monitor;
mod overlay;
mod preflight;
mod publish;
mod settings;
mod setup;
mod tftp;
mod versions;
mod vhdx;
mod vmdk;
mod winstage;
mod wol;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::{response::Html, routing::get, Router};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

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
    std::env::var("BOOTROM_IMAGES_DIR").map(PathBuf::from).unwrap_or_else(|_| home().join("images"))
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

/// Older versions kept bootrom.db / images / storage in the CURRENT directory and boot files in /srv/tftp →
/// move them next to the binary once (rename: instant on the same filesystem; golden.vhdx keeps its hash →
/// clients don't download again). Runs before the database is opened. Items with an env override are left alone.
fn migrate_old_layout() {
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        let same = |a: &Path, b: &Path| a.canonicalize().ok() == b.canonicalize().ok();
        if !same(&cwd, home()) {
            // -wal/-shm: SQLite's last writes when the old server was killed — they belong with the db.
            for (item, env) in [
                ("bootrom.db", "BOOTROM_DB"),
                ("bootrom.db-wal", "BOOTROM_DB"),
                ("bootrom.db-shm", "BOOTROM_DB"),
                ("images", "BOOTROM_IMAGES_DIR"),
                ("storage", "BOOTROM_STORAGE_DIR"),
            ] {
                if std::env::var_os(env).is_none() {
                    moves.push((cwd.join(item), home().join(item)));
                }
            }
        }
    }
    for sub in ["broom", "broom-win", "broom-stage"] {
        moves.push((Path::new("/srv/tftp").join(sub), tftp_dir().join(sub)));
    }
    for (old, new) in moves {
        if !old.exists() || new.exists() {
            continue;
        }
        let _ = std::fs::create_dir_all(new.parent().unwrap_or(home()));
        match std::fs::rename(&old, &new) {
            Ok(()) => info!("moved {} → {}", old.display(), new.display()),
            Err(e) => warn!("could not move {} → {} ({e}): move it by hand", old.display(), new.display()),
        }
    }
}

/// Temp files a job leaves when the server stops midway (crash, power loss, restart). At start no job runs yet, so
/// every one of them is a leftover: removed before anything else (they can be GBs).
fn clean_leftovers() {
    let rm = |p: &Path| {
        let r = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
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
    // images/<name>/image.img.new (convert), tftp/broom-win/<name>/golden.vhdx.tmp (publish).
    for e in std::fs::read_dir(images_dir()).into_iter().flatten().flatten() {
        rm(&e.path().join("image.img.new"));
    }
    for e in std::fs::read_dir(tftp_dir().join("broom-win")).into_iter().flatten().flatten() {
        rm(&e.path().join("golden.vhdx.tmp"));
    }
    // tftp/broom/<name>.new|.old (Linux publish swap); tftp/broom-stage.new-*|.old-* (stage bundle install).
    each(&tftp_dir().join("broom"), &|n| !(n.ends_with(".new") || n.ends_with(".old")));
    each(tftp_dir(), &|n| !(n.starts_with("broom-stage.new") || n.starts_with("broom-stage.old")));
    // work/: bundle downloads, half-built exports.
    each(&work_dir(), &|n| !(n.starts_with("broom-stage-") && n.ends_with(".tar.gz")));
    each(&work_dir().join("export"), &|n| !n.ends_with(".new"));
}

/// Web admin embedded in the binary — deploy a single file, no static/ directory to ship.
const INDEX_HTML: &str = include_str!("../static/index.html");
/// Standalone login/setup page (auth.rs). Served at /login; unauthenticated page requests are redirected here.
const LOGIN_HTML: &str = include_str!("../static/login.html");
/// Version (Cargo.toml) — shown on the web + in logs to tell deployed builds apart.
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The web admin's script (index.html loads it as /app.js?v=<version>, so a new build is never served from cache).
const APP_JS: &str = include_str!("../static/app.js");
async fn index() -> Html<String> {
    Html(INDEX_HTML.replace("__VERSION__", VERSION))
}
async fn app_js() -> impl axum::response::IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS)
}
async fn login_page() -> Html<&'static str> {
    Html(LOGIN_HTML)
}

/// Server events (SSE): "ping" on connect + every 5 s (keep-alive) for the sidebar dot — the browser marks
/// the server offline on a connection error or when pings stop arriving; "job" {name,status} on every
/// image job change (publish / snapshot / rollback steps) — replaces polling /api/images/job.
async fn events(
    axum::extract::State(st): axum::extract::State<SharedState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use futures_util::stream::{self, StreamExt};
    use tokio::sync::broadcast::error::RecvError;
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
    Sse::new(stream::once(async move { Ok(ping()) }).chain(jobs))
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(5)).event(ping()))
}

// Per-tab fragments — loaded on demand via /ui/<page> (the web only fetches the tab being viewed).
const PAGE_MACHINES: &str = include_str!("../static/page-machines.html");
const PAGE_IMAGES: &str = include_str!("../static/page-images.html");
const PAGE_NETWORK: &str = include_str!("../static/page-network.html");
const PAGE_SYSTEM: &str = include_str!("../static/page-system.html");
const PAGE_DRIVERS: &str = include_str!("../static/page-drivers.html");
const PAGE_DEVICES: &str = include_str!("../static/page-devices.html");
async fn ui_page(axum::extract::Path(p): axum::extract::Path<String>) -> Html<&'static str> {
    Html(match p.as_str() {
        "machines" => PAGE_MACHINES,
        "images" => PAGE_IMAGES,
        "network" => PAGE_NETWORK,
        "system" => PAGE_SYSTEM,
        "drivers" => PAGE_DRIVERS,
        "devices" => PAGE_DEVICES,
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
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
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
        let mut seen = self.pxe_seen.lock().unwrap();
        seen.retain(|_, t| now.saturating_sub(*t) <= PXE_MEMORY_S);
        // Bounded: both callers are public, a flood of made-up MACs must not grow it (and every scan of it) for 30 min.
        if seen.len() >= PXE_MAX && !seen.contains_key(mac) {
            if let Some(oldest) = seen.iter().min_by_key(|(_, t)| **t).map(|(m, _)| m.clone()) {
                seen.remove(&oldest);
            }
        }
        seen.insert(mac.to_string(), now);
        let mut ips = self.pxe_ip.lock().unwrap();
        ips.retain(|m, _| seen.contains_key(m));
        if let Some(ip) = ip {
            ips.insert(mac.to_string(), ip.to_string());
        }
        drop((seen, ips));
        self.not_reset.lock().unwrap().remove(mac);
    }

    /// Record an image job status + push it to open web tabs.
    pub fn set_job(&self, name: &str, status: String) {
        self.jobs.lock().unwrap().insert(name.to_string(), status.clone());
        let _ = self.job_tx.send((name.to_string(), status)); // Err = no tab listening
    }
}

#[tokio::main]
async fn main() {
    init_logging();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("install-service") => setup::install_service(&args),
        Some("build-stage") => winstage::build_bundle(&args), // CI: the Windows stage bundle
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
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap_or_else(|e| {
        error!("bind http {addr}: {e} (another bootrom-mgmt already running?)");
        std::process::exit(1)
    });

    migrate_old_layout();
    info!("data in {}", home().display());
    // One instance per data directory: a second one on another --port gets past the HTTP bind above, and would then
    // delete the running one's temp files and share :67/:69 with it (SO_REUSEPORT). The kernel drops the lock when the
    // process dies, so a crash never leaves it stuck.
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(home().join("bootrom.lock"));
    // SAFETY: flock on a valid open fd.
    let locked = lock.as_ref().is_ok_and(|f| unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(f), libc::LOCK_EX | libc::LOCK_NB) } == 0);
    if !locked {
        error!("another bootrom-mgmt is already running on {} — stop it first", home().display());
        std::process::exit(1);
    }
    std::mem::forget(lock); // held for the life of the process
    clean_leftovers(); // no other instance (and so no job) can be running

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
    });
    auth::init_setup_token(&state);

    // Built-in DHCP server + TFTP + iSCSI targets. Distro services from older versions
    // (dnsmasq, tftpd, targetcli's restore service) → stopped first; stopping the latter clears LIO.
    if preflight::is_root() {
        setup::takeover(&["dnsmasq", "tftpd-hpa", "rtslib-fb-targetctl", "target"]);
        match dhcp::start(&state).await {
            Ok(s) => info!("{s}"),
            Err(e) => error!("DHCP/TFTP not started: {e} (fix it on the Network page → Apply)"),
        }
        // Stage bundle checked + fetched if missing (background: a download), then its Secure Boot shim → tftp/shim/.
        tokio::task::spawn_blocking(publish::prepare_stage);
        // configfs targets + zram are lost on server reboot → re-export / rebuild (background, zram is slow).
        let st = state.clone();
        tokio::task::spawn_blocking(move || publish::restore_targets(&st));
        // Prune expired DHCP leases every 10 min so a MAC flood can't grow the table without bound.
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
                match st.db.prune_leases(now) {
                    Ok(n) if n > 0 => info!("pruned {n} expired DHCP lease(s)"),
                    Err(e) => warn!("lease prune: {e}"),
                    _ => {}
                }
            }
        });
        // Superseded iSCSI generations (+ their zram RAM) go as soon as their last client logs out — a busy lab never
        // has a moment with no client at all, and a publish only cleans up what is idle at that moment. Images with
        // a running job are skipped (their old generation is still in the boot script until the publish saves it).
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let st = st.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    for img in st.db.images().unwrap_or_default().into_iter().filter(|i| i.os == "linux") {
                        let busy = st.jobs.lock().unwrap().get(&img.name).is_some_and(|s| s.starts_with('⏳'));
                        if !busy {
                            publish::gc_superseded(&st, &img.name);
                        }
                    }
                })
                .await;
            }
        });
    }

    let app = Router::new()
        .route("/login", get(login_page)) // standalone sign-in page (auth.rs); public
        .route("/", get(index)) // web admin shell (embedded in the binary)
        .route("/app.js", get(app_js))
        // One URL per page (F5 / bookmarks keep the page); same shell, JS picks the page from the path.
        .route("/machines", get(index))
        .route("/images", get(index))
        .route("/network", get(index))
        .route("/system", get(index))
        .route("/drivers", get(index))
        .route("/devices", get(index))
        .route("/ui/{page}", get(ui_page)) // fragment tab on-demand
        .route("/api/events", get(events)) // SSE: server liveness (sidebar dot) + image job status
        .route("/boot.ipxe", get(boot::render)) // iPXE menu
        .route("/boot/start", get(boot::start)) // menu choice → "client started" log + image boot script
        .merge(images::routes()) // images, versions, upload, publish
        .merge(drivers::routes()) // Windows driver packages
        .merge(machines::routes()) // Machines table
        .merge(license::routes()) // Windows license keys + boot reports
        .merge(settings::routes()) // boot menu, zram, DHCP, guest user
        .merge(devices::routes()) // Devices page: edit / bulk / CSV / detail
        .merge(monitor::routes()) // online status, Wake-on-LAN
        .merge(auth::routes()) // admin login
        // Serve boot assets over HTTP (kernel/initrd much faster than TFTP).
        // /tftp/... -> <home>/tftp/... (e.g. http://SERVER/tftp/broom-stage/vmlinuz)
        .nest_service("/tftp", ServeDir::new(tftp_dir()))
        // Guard EVERYTHING: Host check + a session for non-public routes (auth.rs::is_public lists the open ones).
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth::guard))
        .with_state(state);

    info!("bootrom-mgmt v{VERSION} serving on http://{addr}");
    // ConnectInfo: /api/license and /api/booted identify the machine by the peer IP (license.rs).
    if let Err(e) = axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await {
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
        .with_writer(std::io::stderr.with_max_level(tracing::Level::WARN).or_else(std::io::stdout))
        .with_ansi(std::io::stdout().is_terminal())
        .with_target(false)
        .init();
}
