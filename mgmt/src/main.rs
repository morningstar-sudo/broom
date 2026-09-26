// main.rs — bootrom mgmt app (Rust/axum). One binary, runs on the Linux server.
// Modules split per the plan: M5 boot, M6 images(+versions), M7 monitor(+wol), M8 machines.
mod boot;
mod db;
mod dhcp;
mod drivers;
mod images;
mod iscsi;
mod linuxfs;
mod machines;
mod monitor;
mod overlay;
mod preflight;
mod publish;
mod setup;
mod tftp;
mod versions;
mod vhdx;
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

/// Web admin embedded in the binary — deploy a single file, no static/ directory to ship.
const INDEX_HTML: &str = include_str!("../static/index.html");
/// Version (Cargo.toml) — shown on the web + in logs to tell deployed builds apart.
const VERSION: &str = env!("CARGO_PKG_VERSION");
async fn index() -> Html<String> {
    Html(INDEX_HTML.replace("__VERSION__", VERSION))
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
async fn ui_page(axum::extract::Path(p): axum::extract::Path<String>) -> Html<&'static str> {
    Html(match p.as_str() {
        "machines" => PAGE_MACHINES,
        "images" => PAGE_IMAGES,
        "network" => PAGE_NETWORK,
        "system" => PAGE_SYSTEM,
        "drivers" => PAGE_DRIVERS,
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
    /// Running network boot listeners (DHCP/TFTP) — dhcp::start() replaces them on config change.
    pub net: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}
pub type SharedState = Arc<AppState>;

impl AppState {
    /// Record an image job status + push it to open web tabs.
    pub fn set_job(&self, name: &str, status: String) {
        self.jobs.lock().unwrap().insert(name.to_string(), status.clone());
        let _ = self.job_tx.send((name.to_string(), status)); // Err = no tab listening
    }
}

#[tokio::main]
async fn main() {
    init_logging();
    migrate_old_layout();
    info!("data in {}", home().display());
    let args: Vec<String> = std::env::args().collect();

    let skip_preflight = args.iter().any(|a| a == "--skip-preflight");
    let port: u16 = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(80);

    // Preflight: pass → continue. Fail, or network never configured → run setup BY ITSELF (detect network +
    // install packages + seed DHCP config) then preflight again; still failing → report clearly + exit ≠ 0
    // (rule.md). (dev: --skip-preflight skips it.)
    if !skip_preflight {
        let unconfigured = db::open(&db::url())
            .map(|d| d.get_config("dhcp_server_ip", "").is_empty())
            .unwrap_or(true);
        if unconfigured || preflight::run().is_err() {
            warn!("preflight failed or network not configured → running setup");
            setup::run(&args); // detect network + install packages + seed DHCP config (needs root)
            match preflight::run() {
                Ok(()) => info!("preflight passed after setup"),
                Err(report) => {
                    for m in &report.other {
                        error!("preflight: {m}");
                    }
                    if let Some(cmd) = report.install_cmd() {
                        error!("preflight: install the missing packages by hand: {cmd}");
                    }
                    std::process::exit(1);
                }
            }
        } else {
            info!("preflight passed");
        }
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
        net: Mutex::new(Vec::new()),
    });

    // Built-in DHCP (+ proxy boot server) + TFTP + iSCSI targets. Distro services from older versions
    // (dnsmasq, tftpd, targetcli's restore service) → stopped first; stopping the latter clears LIO.
    if preflight::is_root() {
        setup::takeover(&["dnsmasq", "tftpd-hpa", "rtslib-fb-targetctl", "target"]);
        match dhcp::start(&state).await {
            Ok(s) => info!("{s}"),
            Err(e) => error!("DHCP/TFTP not started: {e} (fix it on the Network page → Apply)"),
        }
        // configfs targets + zram are lost on server reboot → re-export / rebuild (background, zram is slow).
        let st = state.clone();
        tokio::task::spawn_blocking(move || publish::restore_targets(&st));
    }

    let app = Router::new()
        .route("/", get(index)) // web admin shell (embedded in the binary)
        // One URL per page (F5 / bookmarks keep the page); same shell, JS picks the page from the path.
        .route("/machines", get(index))
        .route("/images", get(index))
        .route("/network", get(index))
        .route("/system", get(index))
        .route("/drivers", get(index))
        .route("/ui/{page}", get(ui_page)) // fragment tab on-demand
        .route("/api/events", get(events)) // SSE: server liveness (sidebar dot) + image job status
        .route("/boot.ipxe", get(boot::render)) // M5
        .route("/boot/start", get(boot::start)) // menu choice → "client started" log + image boot script
        .merge(images::routes()) // M6
        .merge(drivers::routes()) // Windows driver packages
        .merge(machines::routes()) // M8
        .merge(monitor::routes()) // M7
        // Serve boot assets over HTTP (kernel/initrd much faster than TFTP).
        // /tftp/... -> <home>/tftp/... (e.g. http://SERVER/tftp/broom-stage/vmlinuz)
        .nest_service("/tftp", ServeDir::new(tftp_dir()))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap_or_else(|e| {
        error!("bind http {addr}: {e}");
        std::process::exit(1)
    });
    info!("bootrom-mgmt v{VERSION} serving on http://{addr}");
    // ConnectInfo: /api/license picks a machine by the peer IP (machines.rs).
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
