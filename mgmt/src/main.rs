// main.rs — mgmt app bootrom (Rust/axum). 1 binary, chạy trên server Linux.
// Module tách riêng theo plan: M5 boot, M6 images(+zfs), M7 monitor(+wol), M8 machines.
mod boot;
mod db;
mod dnsmasq;
mod images;
mod ltsp;
mod machines;
mod monitor;
mod overlay;
mod preflight;
mod publish;
mod setup;
mod wol;
mod zfs;

use axum::{response::Html, routing::get, Router};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tower_http::services::ServeDir;

/// Thư mục chứa image, mỗi image 1 folder con: `<images_dir>/<name>/image.img`.
/// Mặc định `./images` (dưới level binary); override bằng BOOTROM_IMAGES_DIR.
pub fn images_dir() -> PathBuf {
    std::env::var("BOOTROM_IMAGES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("images"))
}

/// Web admin nhúng thẳng vào binary — deploy chỉ 1 file, khỏi kèm thư mục static/.
const INDEX_HTML: &str = include_str!("../static/index.html");
/// Version (Cargo.toml) — hiện trên web + log để phân biệt build đã deploy.
const VERSION: &str = env!("CARGO_PKG_VERSION");
async fn index() -> Html<String> {
    Html(INDEX_HTML.replace("__VERSION__", VERSION))
}

// Fragment mỗi tab — nạp on-demand qua /ui/<page> (web chỉ tải phần đang xem).
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

/// State chia sẻ. ponytail: 1 Mutex<Connection> global — tải admin vài req/phút,
/// khỏi cần pool. Nâng lên r2d2 nếu sau này nhiều concurrent writer.
pub struct AppState {
    pub db: Mutex<rusqlite::Connection>,
    /// Trạng thái job publish theo image name: "⏳ ..." đang chạy | "✓ ..." xong | "✗ ..." lỗi.
    pub jobs: Mutex<std::collections::HashMap<String, String>>,
}
pub type SharedState = Arc<AppState>;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Subcommand: bootrom-mgmt print-dhcp — in pxe.conf sẽ sinh (preview, khỏi root).
    if args.get(1).map(|s| s == "print-dhcp").unwrap_or(false) {
        let conn = db::open("bootrom.db").expect("mở DB");
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

    // Preflight: pass → chạy tiếp. Fail → TỰ chạy setup (cài gói + snponly + dnsmasq) rồi
    // preflight lại; vẫn fail → báo rõ + exit ≠ 0 (rule.md). (dev: --skip-preflight bỏ qua.)
    if !skip_preflight {
        if preflight::run().is_err() {
            eprintln!("PREFLIGHT FAIL → chạy setup tự động...\n");
            setup::run(&args); // dò mạng + cài gói + snponly.efi + dnsmasq (cần root)
            match preflight::run() {
                Ok(()) => println!("\nPREFLIGHT PASS (sau setup)"),
                Err(report) => {
                    eprintln!("\nPREFLIGHT vẫn FAIL sau setup:");
                    for m in &report.other {
                        eprintln!("  - {m}");
                    }
                    if let Some(cmd) = report.install_cmd() {
                        eprintln!("\nCài tay gói thiếu:\n  {cmd}");
                    }
                    std::process::exit(1);
                }
            }
        } else {
            println!("PREFLIGHT PASS");
        }
    }

    let conn = db::open("bootrom.db").expect("mở SQLite thất bại");
    std::fs::create_dir_all(images_dir()).ok();
    let state: SharedState = Arc::new(AppState {
        db: Mutex::new(conn),
        jobs: Mutex::new(std::collections::HashMap::new()),
    });

    // zram mất khi server reboot → dựng lại + re-target cho image cache_mode=zram (nền, chậm).
    {
        let st = state.clone();
        tokio::task::spawn_blocking(move || publish::repopulate_zram(&st));
    }

    let app = Router::new()
        .route("/", get(index)) // web admin shell (nhúng trong binary)
        .route("/ui/:page", get(ui_page)) // fragment tab on-demand
        .route("/boot.ipxe", get(boot::render)) // M5
        .merge(images::routes()) // M6
        .merge(machines::routes()) // M8
        .merge(monitor::routes()) // M7
        // Serve boot assets qua HTTP (kernel/initrd nhanh hơn TFTP nhiều).
        // /tftp/... -> /srv/tftp/... (vd http://SERVER/tftp/ltsp/vmlinuz)
        .nest_service("/tftp", ServeDir::new("/srv/tftp"))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    println!("bootrom-mgmt v{VERSION} serving on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind port");
    axum::serve(listener, app).await.expect("serve");
}
