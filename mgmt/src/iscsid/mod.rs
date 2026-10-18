// iscsid/ — the iSCSI target of the Linux goldens and of the Windows games disk (games.rs), in its own process
// (`bootrom-mgmt iscsid`, started by mgmt, see iscsi.rs). Read-only LUN 0 per target, served from the golden file (disk
// cache mode) or from a compressed copy in RAM ("zram" mode, ramimg.rs). Its own process so an mgmt restart or upgrade
// never cuts a client off — the job the kernel's LIO target did before, without needing LIO or zram in the kernel.
//
// mgmt drives it over a Unix socket, one JSON request per connection (Req → Resp). Targets are saved to a state file
// and restored when the daemon itself starts again (crash → systemd restarts it; server reboot).
mod conn;
pub mod games;
mod overlay;
pub(crate) mod ramimg; // also the Windows goldens' RAM copies (goldenram.rs)
mod scsi;

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};

/// Control socket (root only).
pub const SOCK: &str = "/run/broom-iscsid.sock";
const PORT: u16 = 3260;
/// Open connections at most: each holds a socket (file descriptor) + a few buffers (~256 KB at worst) → what the open
/// file limit allows (minus a few for targets / control), and at most 5% of RAM.
fn max_conns() -> usize {
    let fds = crate::sysmon::fd_limit().saturating_sub(64) as usize;
    fds.min(crate::sysmon::ram_share(5, 256 << 10, 64)).max(64)
}

/// One exported target, as saved in the state file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub iqn: String,
    /// The golden file.
    pub path: String,
    /// Serve a compressed copy held in RAM instead of reading the file.
    pub ram: bool,
    /// RAM bytes that must stay free next to the copy (refused otherwise).
    #[serde(default)]
    pub reserve: u64,
    /// hash::stamp of the file when exported: a restore skips a target whose file changed since (its clients were
    /// reading the old content).
    #[serde(default)]
    pub stamp: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Req {
    Export(Entry),
    Remove {
        iqn: String,
    },
    List,
    Version,
    Quit,
    /// Serve exactly these games disks.
    GamesSet {
        disks: Vec<games::Config>,
    },
    GamesSave {
        name: String,
    },
    GamesDiscard {
        name: String,
    },
    GamesStatus,
    /// Close every session of this client IP (a machine known to be off). Answers the count in `msg`.
    KickPeer {
        ip: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TargetInfo {
    pub iqn: String,
    pub sessions: usize,
    pub ram: bool,
    /// Bytes read by clients since the target was built. None = a daemon of a build without the counter.
    #[serde(default)]
    pub read_bytes: Option<u64>,
}

/// Bytes read by one client IP since the daemon started (every session of it, games disks included).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PeerInfo {
    pub ip: String,
    pub read_bytes: u64,
    /// A session of it is logged in now.
    #[serde(default)]
    pub connected: bool,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Resp {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
    #[serde(default)]
    pub targets: Vec<TargetInfo>,
    #[serde(default)]
    pub version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub games: Vec<games::Info>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub msg: String,
    /// With List. None = a daemon of a build without the counters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peers: Option<Vec<PeerInfo>>,
}

pub struct Lun {
    pub iqn: String,
    pub size: u64,
    data: Data,
    pub sessions: AtomicUsize,
    /// Bytes read by clients (benchmark page: live throughput).
    pub read_bytes: AtomicU64,
}

enum Data {
    /// Kept open: a golden replaced on disk (rename over it) keeps serving the old content to this generation.
    File(std::fs::File),
    Ram(ramimg::RamImage),
}

impl Lun {
    fn new(iqn: String, size: u64, data: Data, read_bytes: u64) -> Lun {
        Lun {
            iqn,
            size,
            data,
            sessions: AtomicUsize::new(0),
            read_bytes: AtomicU64::new(read_bytes),
        }
    }

    /// A LUN reading the file at `path`.
    fn file(iqn: &str, path: &str) -> Result<Lun, String> {
        let f = std::fs::File::open(path).map_err(|err| format!("{path}: {err}"))?;
        let size = f.metadata().map_err(|err| format!("{path}: {err}"))?.len();
        Ok(Lun::new(iqn.into(), size, Data::File(f), 0))
    }

    fn open(e: &Entry) -> Result<Lun, String> {
        if !e.ram {
            return Lun::file(&e.iqn, &e.path);
        }
        let img = ramimg::RamImage::load(&e.path, e.reserve)?;
        Ok(Lun::new(e.iqn.clone(), img.size(), Data::Ram(img), 0))
    }

    pub fn is_ram(&self) -> bool {
        matches!(self.data, Data::Ram(_))
    }

    /// Blocking read (disk I/O, or unpacking many RAM blocks): run it off the async threads.
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        match &self.data {
            Data::File(f) => {
                std::os::unix::fs::FileExt::read_exact_at(f, buf, off).map_err(|e| e.to_string())
            }
            Data::Ram(r) => r.read_at(off, buf),
        }
    }

    /// The read when it can't block: file bytes already in the page cache (preadv2 RWF_NOWAIT), or a few RAM blocks.
    /// Saves the hop to a blocking thread — half the latency of a small read. None = use read_at.
    pub fn read_now(&self, off: u64, len: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; len];
        match &self.data {
            Data::File(f) => {
                use std::os::fd::AsRawFd;
                let iov = libc::iovec {
                    iov_base: buf.as_mut_ptr().cast(),
                    iov_len: len,
                };
                // SAFETY: iov points at `buf`, alive and `len` bytes long for the whole call.
                let n = unsafe {
                    libc::preadv2(f.as_raw_fd(), &iov, 1, off as libc::off_t, libc::RWF_NOWAIT)
                };
                (n == len as isize).then_some(buf) // short / EAGAIN (not cached) → the blocking path
            }
            Data::Ram(r) if len <= 64 << 10 => r.read_at(off, &mut buf).ok().map(|_| buf),
            Data::Ram(_) => None,
        }
    }
}

type Targets = HashMap<String, (Entry, Arc<Lun>)>;
/// Exported targets. A session holds its own Arc<Lun>: removing or replacing a target never cuts a client off.
static TARGETS: LazyLock<Mutex<Targets>> = LazyLock::new(Default::default);
/// Targets being built (RAM load takes a while): listed, so mgmt doesn't export them a second time.
static LOADING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
static STATE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

fn lookup(iqn: &str) -> Option<Arc<Lun>> {
    let l = TARGETS.lock().get(iqn).map(|(_, l)| l.clone());
    l.or_else(|| games::lun(iqn))
}

fn iqns() -> Vec<String> {
    let mut v: Vec<String> = TARGETS.lock().keys().cloned().collect();
    v.extend(games::iqns());
    v
}

fn state_path() -> Option<std::path::PathBuf> {
    STATE.get().cloned()
}

/// Save the target list (temp + rename: whole or not at all).
fn save() {
    let Some(p) = STATE.get() else { return };
    let mut list: Vec<Entry> = TARGETS.lock().values().map(|(e, _)| e.clone()).collect();
    list.sort_by(|a, b| a.iqn.cmp(&b.iqn));
    let r = crate::versions::write_durable(p, &serde_json::to_vec_pretty(&list).unwrap());
    if let Err(e) = r {
        tracing::warn!("iscsid: save {}: {e}", p.display());
    }
}

/// Build + register a target (replaces one with the same IQN). Blocking.
fn export(mut e: Entry) -> Result<(), String> {
    LOADING.lock().insert(e.iqn.clone());
    if e.stamp.is_empty() {
        e.stamp = crate::hash::stamp(std::path::Path::new(&e.path)).unwrap_or_default();
    }
    let r = Lun::open(&e);
    LOADING.lock().remove(&e.iqn);
    let lun = r?;
    tracing::info!(
        "iscsid: target {} ready ({}: {}, {} bytes)",
        e.iqn,
        if e.ram { "RAM" } else { "file" },
        e.path,
        lun.size
    );
    TARGETS.lock().insert(e.iqn.clone(), (e, Arc::new(lun)));
    save();
    Ok(())
}

fn list() -> Vec<TargetInfo> {
    let mut v: Vec<TargetInfo> = TARGETS
        .lock()
        .values()
        .map(|(e, l)| TargetInfo {
            iqn: e.iqn.clone(),
            sessions: l.sessions.load(Ordering::Relaxed),
            ram: e.ram,
            read_bytes: Some(l.read_bytes.load(Ordering::Relaxed)),
        })
        .collect();
    let loaded: HashSet<String> = v.iter().map(|t| t.iqn.clone()).collect();
    for iqn in LOADING.lock().iter().filter(|i| !loaded.contains(*i)) {
        v.push(TargetInfo {
            iqn: iqn.clone(),
            sessions: 0,
            ram: true,
            read_bytes: Some(0),
        });
    }
    v
}

async fn handle(req: Req) -> Resp {
    let mut r = Resp::default();
    match req {
        Req::Export(e) => match tokio::task::spawn_blocking(move || export(e)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => r.err = Some(e),
            Err(e) => r.err = Some(e.to_string()),
        },
        Req::Remove { iqn } => {
            if TARGETS.lock().remove(&iqn).is_some() {
                tracing::info!("iscsid: target {iqn} removed");
                save();
            }
        }
        Req::List => {
            r.targets = list();
            r.peers = Some(conn::peers());
            r.games = games::info(); // the benchmark page's throughput (list() is the goldens only: publish/gc use it)
        }
        Req::Version => r.version = crate::iscsi::exe_id(),
        Req::Quit => {}
        // Blocking: set may finish a merge cut off earlier; save links files.
        Req::GamesSet { disks } => {
            r.err = tokio::task::spawn_blocking(move || games::set(disks))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
                .err()
        }
        Req::GamesSave { name } => match tokio::task::spawn_blocking(move || games::save(&name))
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
        {
            Ok(m) => r.msg = m,
            Err(e) => r.err = Some(e),
        },
        Req::GamesDiscard { name } => r.err = games::discard(&name).err(),
        Req::GamesStatus => r.games = games::info(),
        Req::KickPeer { ip } => r.msg = conn::kick_peer(&ip).to_string(),
    }
    r
}

async fn control(s: tokio::net::UnixStream) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (rd, mut wr) = s.into_split();
    let mut line = String::new();
    if BufReader::new(rd).read_line(&mut line).await.is_err() {
        return;
    }
    let (resp, quit) = match serde_json::from_str::<Req>(&line) {
        Ok(Req::Quit) => (Resp::default(), true),
        Ok(req) => (handle(req).await, false),
        Err(e) => (
            Resp {
                err: Some(format!("bad request: {e}")),
                ..Default::default()
            },
            false,
        ),
    };
    let mut out = serde_json::to_vec(&resp).unwrap();
    out.push(b'\n');
    let _ = wr.write_all(&out).await;
    let _ = wr.shutdown().await;
    if quit {
        tracing::info!("iscsid: quit requested (new version taking over)");
        std::process::exit(0);
    }
}

/// `bootrom-mgmt iscsid --state <file>` — never returns.
pub async fn run(args: &[String]) -> ! {
    let state = args
        .iter()
        .position(|a| a == "--state")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let Some(state) = state else {
        tracing::error!("iscsid: --state <file> missing");
        std::process::exit(2)
    };
    let _ = STATE.set(state.into());
    // One daemon: a live control socket means another one runs.
    if std::os::unix::net::UnixStream::connect(SOCK).is_ok() {
        tracing::error!("iscsid: already running ({SOCK})");
        std::process::exit(1);
    }
    let tcp = match tokio::net::TcpListener::bind(("0.0.0.0", PORT)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(
                "iscsid: bind :{PORT}: {e} — port held by another iSCSI target (kernel LIO? `targetcli clearconfig confirm=True`)"
            );
            std::process::exit(1)
        }
    };
    let _ = std::fs::remove_file(SOCK);
    let ctl = match tokio::net::UnixListener::bind(SOCK) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("iscsid: {SOCK}: {e}");
            std::process::exit(1)
        }
    };
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(SOCK, std::fs::Permissions::from_mode(0o600));
    tracing::info!("iscsid {} listening on :{PORT}", crate::iscsi::exe_id());

    // Targets of the previous run: file-backed ones first (instant), RAM ones after (slow). All of them are listed as
    // loading BEFORE the control socket answers: mgmt must not export one of them a second time meanwhile.
    let mut saved: Vec<Entry> = STATE
        .get()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    saved.retain(|e| {
        let same =
            crate::hash::stamp(std::path::Path::new(&e.path)).as_deref() == Some(e.stamp.as_str());
        if !same {
            tracing::warn!(
                "iscsid: {} not restored: {} changed since it was exported",
                e.iqn,
                e.path
            );
        }
        same
    });
    saved.sort_by_key(|e| e.ram);
    LOADING.lock().extend(saved.iter().map(|e| e.iqn.clone()));
    tokio::task::spawn_blocking(move || {
        for e in saved {
            if let Err(err) = export(e.clone()) {
                tracing::warn!("iscsid: {} not restored: {err}", e.iqn);
            }
        }
        save(); // drops the ones that were skipped or failed
        games::restore();
    });
    // Games disk versions nobody uses any more are merged into games.img.
    tokio::spawn(async {
        let mut t = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            t.tick().await;
            let _ = tokio::task::spawn_blocking(games::tick).await;
        }
    });

    tokio::spawn(async move {
        while let Ok((s, _)) = ctl.accept().await {
            tokio::spawn(control(s));
        }
    });
    let max = max_conns();
    tracing::info!(
        "iscsid: up to {max} connections, {} commands in flight (sized from this server)",
        *conn::IN_FLIGHT_MAX
    );
    let slots = Arc::new(tokio::sync::Semaphore::new(max));
    loop {
        let Ok((s, peer)) = tcp.accept().await else {
            continue;
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            tracing::warn!("iscsid: {max} connections open, refusing {peer}");
            continue;
        };
        // A client that died without closing (power cut, shutdown) is noticed in ~1 min and its session freed: keepalive
        // when the line is idle; TCP_USER_TIMEOUT when replies it never acknowledged are still queued (it went off
        // right after its last writes — keepalive doesn't probe then, and the kernel would retransmit for ~15 min,
        // keeping e.g. the game update machine "connected" and its save refused).
        let ka = socket2::TcpKeepalive::new()
            .with_time(std::time::Duration::from_secs(30))
            .with_interval(std::time::Duration::from_secs(10))
            .with_retries(3);
        let sock = socket2::SockRef::from(&s);
        let _ = sock.set_tcp_keepalive(&ka);
        let _ = sock.set_tcp_user_timeout(Some(std::time::Duration::from_secs(60)));
        let _ = s.set_nodelay(true);
        tokio::spawn(async move {
            conn::serve(s, peer.ip().to_canonical().to_string()).await;
            drop(permit);
        });
    }
}

#[cfg(test)]
#[path = "../../tests/unit/iscsid/mod.rs"]
mod tests;
