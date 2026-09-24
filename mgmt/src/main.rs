// main.rs — bootrom mgmt app (Rust/axum). One binary, runs on the Linux server.
// Modules split per the plan: M5 boot, M6 images(+zfs), M7 monitor(+wol), M8 machines.
mod boot;
mod db;
mod dnsmasq;
mod images;
mod machines;
mod monitor;
mod overlay;
mod preflight;
mod publish;
mod setup;
mod vhdx;
mod winstage;
mod wol;
mod zfs;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::{response::Html, routing::get, Router};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tower_http::services::ServeDir;

/// Directory holding the images, one subfolder per image: `<images_dir>/<name>/image.img`.
/// Default `./images` (next to the binary); override with BOOTROM_IMAGES_DIR.
pub fn images_dir() -> PathBuf {
    std::env::var("BOOTROM_IMAGES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("images"))
}

/// Web admin embedded in the binary — deploy a single file, no static/ directory to ship.
const INDEX_HTML: &str = include_str!("../static/index.html");
/// Version (Cargo.toml) — shown on the web + in logs to tell deployed builds apart.
const VERSION: &str = env!("CARGO_PKG_VERSION");
async fn index() -> Html<String> {
    Html(INDEX_HTML.replace("__VERSION__", VERSION))
}

/// Server liveness for the sidebar dot: SSE, a "ping" event on connect + every 5 s (keep-alive).
/// The browser marks the server offline on a connection error or when pings stop arriving.
async fn events() -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use futures_util::stream::{self, StreamExt};
    let ping = || Event::default().event("ping").data(VERSION);
    Sse::new(stream::once(async move { Ok(ping()) }).chain(stream::pending()))
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(5)).event(ping()))
}

// Per-tab fragments — loaded on demand via /ui/<page> (the web only fetches the tab being viewed).
const PAGE_MACHINES: &str = include_str!("../static/page-machines.html");
const PAGE_IMAGES: &str = include_str!("../static/page-images.html");
const PAGE_NETWORK: &str = include_str!("../static/page-network.html");
const PAGE_SYSTEM: &str = include_str!("../static/page-system.html");
async fn ui_page(axum::extract::Path(p): axum::extract::Path<String>) -> Html<&'static str> {
    Html(match p.as_str() {
        "machines" => PAGE_MACHINES,
        "images" => PAGE_IMAGES,
        "network" => PAGE_NETWORK,
        "system" => PAGE_SYSTEM,
        _ => "",
    })
}

/// Shared state. ponytail: one global Mutex<Connection> — admin load is a few req/min,
/// no pool needed. Move to r2d2 if there are ever many concurrent writers.
pub struct AppState {
    pub db: Mutex<rusqlite::Connection>,
    /// Publish job status by image name: "⏳ ..." running | "✓ ..." done | "✗ ..." failed.
    pub jobs: Mutex<std::collections::HashMap<String, String>>,
}
pub type SharedState = Arc<AppState>;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Subcommand: bootrom-mgmt print-dhcp — print the pxe.conf that would be generated (preview, no root needed).
    if args.get(1).map(|s| s == "print-dhcp").unwrap_or(false) {
        let conn = db::open("bootrom.db").expect("open DB");
        let binds = dnsmasq::bindings(&conn);
        print!("{}", dnsmasq::generate(&conn, &binds));
        return;
    }

    let skip_preflight = args.iter().any(|a| a == "--skip-preflight");
    let port: u16 = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(80);

    // iPXE embedded in the binary → /srv/tftp/snponly.efi (written if missing/different from the embedded one). Before
    // preflight so preflight doesn't ask for the distro's ipxe package.
    if preflight::is_root() {
        match boot::install_ipxe() {
            Ok(true) => println!("[tftp] snponly.efi (embedded iPXE) written"),
            Ok(false) => {}
            Err(e) => eprintln!("[tftp] writing snponly.efi failed: {e}"),
        }
    }

    // Preflight: pass → continue. Fail → run setup BY ITSELF (install packages + snponly + dnsmasq) then
    // preflight again; still failing → report clearly + exit ≠ 0 (rule.md). (dev: --skip-preflight skips it.)
    if !skip_preflight {
        if preflight::run().is_err() {
            eprintln!("PREFLIGHT FAIL → running setup automatically...\n");
            setup::run(&args); // detect network + install packages + snponly.efi + dnsmasq (needs root)
            match preflight::run() {
                Ok(()) => println!("\nPREFLIGHT PASS (sau setup)"),
                Err(report) => {
                    eprintln!("\nPREFLIGHT still FAILS after setup:");
                    for m in &report.other {
                        eprintln!("  - {m}");
                    }
                    if let Some(cmd) = report.install_cmd() {
                        eprintln!("\nInstall the missing packages by hand:\n  {cmd}");
                    }
                    std::process::exit(1);
                }
            }
        } else {
            println!("PREFLIGHT PASS");
        }
    }

    let conn = db::open("bootrom.db").expect("opening SQLite failed");
    std::fs::create_dir_all(images_dir()).ok();
    // A binary upgrade that changes the pxe.conf template (e.g. URL boot.ipxe?mac=) → rewrite + restart dnsmasq.
    if preflight::is_root() {
        let conf = dnsmasq::generate(&conn, &dnsmasq::bindings(&conn));
        let cur = std::fs::read_to_string("/etc/dnsmasq.d/pxe.conf").ok();
        if cur.is_some_and(|c| c != conf) {
            match dnsmasq::apply(&conn) {
                Ok(ok) => println!("[dnsmasq] pxe.conf updated (restart {})", if ok { "OK" } else { "FAIL" }),
                Err(e) => eprintln!("[dnsmasq] writing pxe.conf failed: {e}"),
            }
        }
    }
    let state: SharedState = Arc::new(AppState {
        db: Mutex::new(conn),
        jobs: Mutex::new(std::collections::HashMap::new()),
    });

    // zram is lost when the server reboots → rebuild + re-target images with cache_mode=zram (background, slow).
    {
        let st = state.clone();
        tokio::task::spawn_blocking(move || publish::repopulate_zram(&st));
    }

    let app = Router::new()
        .route("/", get(index)) // web admin shell (embedded in the binary)
        // One URL per page (F5 / bookmarks keep the page); same shell, JS picks the page from the path.
        .route("/machines", get(index))
        .route("/images", get(index))
        .route("/network", get(index))
        .route("/system", get(index))
        .route("/ui/:page", get(ui_page)) // fragment tab on-demand
        .route("/api/events", get(events)) // SSE server liveness (sidebar dot)
        .route("/boot.ipxe", get(boot::render)) // M5
        .merge(images::routes()) // M6
        .merge(machines::routes()) // M8
        .merge(monitor::routes()) // M7
        // Serve boot assets over HTTP (kernel/initrd much faster than TFTP).
        // /tftp/... -> /srv/tftp/... (vd http://SERVER/tftp/broom-stage/vmlinuz)
        .nest_service("/tftp", ServeDir::new("/srv/tftp"))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    println!("bootrom-mgmt v{VERSION} serving on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind port");
    axum::serve(listener, app).await.expect("serve");
}
