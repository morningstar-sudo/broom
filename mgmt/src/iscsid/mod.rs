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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use serde::{Deserialize, Serialize};

/// Control socket (root only).
pub const SOCK: &str = "/run/broom-iscsid.sock";
const PORT: u16 = 3260;
/// Open connections at most (each holds a socket + a few buffers).
const MAX_CONNS: usize = 512;

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
    Remove { iqn: String },
    List,
    Version,
    Quit,
    /// Serve exactly these games disks.
    GamesSet { disks: Vec<games::Config> },
    GamesSave { name: String },
    GamesDiscard { name: String },
    GamesStatus,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TargetInfo {
    pub iqn: String,
    pub sessions: usize,
    pub ram: bool,
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
}

pub struct Lun {
    pub iqn: String,
    pub size: u64,
    data: Data,
    pub sessions: AtomicUsize,
}

enum Data {
    /// Kept open: a golden replaced on disk (rename over it) keeps serving the old content to this generation.
    File(std::fs::File),
    Ram(ramimg::RamImage),
}

impl Lun {
    fn open(e: &Entry) -> Result<Lun, String> {
        let (data, size) = if e.ram {
            let img = ramimg::RamImage::load(&e.path, e.reserve)?;
            let size = img.size();
            (Data::Ram(img), size)
        } else {
            let f = std::fs::File::open(&e.path).map_err(|err| format!("{}: {err}", e.path))?;
            let size = f.metadata().map_err(|err| format!("{}: {err}", e.path))?.len();
            (Data::File(f), size)
        };
        Ok(Lun { iqn: e.iqn.clone(), size, data, sessions: AtomicUsize::new(0) })
    }

    /// Blocking read (disk I/O, or unpacking many RAM blocks): run it off the async threads.
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        match &self.data {
            Data::File(f) => std::os::unix::fs::FileExt::read_exact_at(f, buf, off).map_err(|e| e.to_string()),
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
                let iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: len };
                // SAFETY: iov points at `buf`, alive and `len` bytes long for the whole call.
                let n = unsafe { libc::preadv2(f.as_raw_fd(), &iov, 1, off as libc::off_t, libc::RWF_NOWAIT) };
                (n == len as isize).then_some(buf) // short / EAGAIN (not cached) → the blocking path
            }
            Data::Ram(r) if len <= 64 << 10 => r.read_at(off, &mut buf).ok().map(|_| buf),
            Data::Ram(_) => None,
        }
    }
}

/// Exported targets. A session holds its own Arc<Lun>: removing or replacing a target never cuts a client off.
static TARGETS: LazyLock<Mutex<HashMap<String, (Entry, Arc<Lun>)>>> = LazyLock::new(Default::default);
/// Targets being built (RAM load takes a while): listed, so mgmt doesn't export them a second time.
static LOADING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
static STATE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

fn lookup(iqn: &str) -> Option<Arc<Lun>> {
    let l = TARGETS.lock().unwrap().get(iqn).map(|(_, l)| l.clone());
    l.or_else(|| games::lun(iqn))
}

fn iqns() -> Vec<String> {
    let mut v: Vec<String> = TARGETS.lock().unwrap().keys().cloned().collect();
    v.extend(games::iqns());
    v
}

fn state_path() -> Option<std::path::PathBuf> {
    STATE.get().cloned()
}

/// Save the target list (temp + rename: whole or not at all).
fn save() {
    let Some(p) = STATE.get() else { return };
    let mut list: Vec<Entry> = TARGETS.lock().unwrap().values().map(|(e, _)| e.clone()).collect();
    list.sort_by(|a, b| a.iqn.cmp(&b.iqn));
    let tmp = p.with_extension("tmp");
    let r = std::fs::write(&tmp, serde_json::to_vec_pretty(&list).unwrap()).and_then(|_| std::fs::rename(&tmp, p));
    if let Err(e) = r {
        tracing::warn!("iscsid: save {}: {e}", p.display());
    }
}

/// Build + register a target (replaces one with the same IQN). Blocking.
fn export(mut e: Entry) -> Result<(), String> {
    LOADING.lock().unwrap().insert(e.iqn.clone());
    if e.stamp.is_empty() {
        e.stamp = crate::hash::stamp(std::path::Path::new(&e.path)).unwrap_or_default();
    }
    let r = Lun::open(&e);
    LOADING.lock().unwrap().remove(&e.iqn);
    let lun = r?;
    tracing::info!("iscsid: target {} ready ({}: {}, {} bytes)", e.iqn, if e.ram { "RAM" } else { "file" }, e.path, lun.size);
    TARGETS.lock().unwrap().insert(e.iqn.clone(), (e, Arc::new(lun)));
    save();
    Ok(())
}

fn list() -> Vec<TargetInfo> {
    let mut v: Vec<TargetInfo> = TARGETS
        .lock()
        .unwrap()
        .values()
        .map(|(e, l)| TargetInfo { iqn: e.iqn.clone(), sessions: l.sessions.load(Ordering::Relaxed), ram: e.ram })
        .collect();
    let loaded: HashSet<String> = v.iter().map(|t| t.iqn.clone()).collect();
    for iqn in LOADING.lock().unwrap().iter().filter(|i| !loaded.contains(*i)) {
        v.push(TargetInfo { iqn: iqn.clone(), sessions: 0, ram: true });
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
            if TARGETS.lock().unwrap().remove(&iqn).is_some() {
                tracing::info!("iscsid: target {iqn} removed");
                save();
            }
        }
        Req::List => r.targets = list(),
        Req::Version => r.version = crate::iscsi::exe_id(),
        Req::Quit => {}
        // Blocking: set may finish a merge cut off earlier; save links files.
        Req::GamesSet { disks } => r.err = tokio::task::spawn_blocking(move || games::set(disks)).await.unwrap_or_else(|e| Err(e.to_string())).err(),
        Req::GamesSave { name } => match tokio::task::spawn_blocking(move || games::save(&name)).await.unwrap_or_else(|e| Err(e.to_string())) {
            Ok(m) => r.msg = m,
            Err(e) => r.err = Some(e),
        },
        Req::GamesDiscard { name } => r.err = games::discard(&name).err(),
        Req::GamesStatus => r.games = games::info(),
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
        Err(e) => (Resp { err: Some(format!("bad request: {e}")), ..Default::default() }, false),
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
    let state = args.iter().position(|a| a == "--state").and_then(|i| args.get(i + 1)).cloned();
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
            tracing::error!("iscsid: bind :{PORT}: {e} — port held by another iSCSI target (kernel LIO? `targetcli clearconfig confirm=True`)");
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
    let mut saved: Vec<Entry> = STATE.get().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    saved.retain(|e| {
        let same = crate::hash::stamp(std::path::Path::new(&e.path)).as_deref() == Some(e.stamp.as_str());
        if !same {
            tracing::warn!("iscsid: {} not restored: {} changed since it was exported", e.iqn, e.path);
        }
        same
    });
    saved.sort_by_key(|e| e.ram);
    LOADING.lock().unwrap().extend(saved.iter().map(|e| e.iqn.clone()));
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
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNS));
    loop {
        let Ok((s, peer)) = tcp.accept().await else { continue };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            tracing::warn!("iscsid: {MAX_CONNS} connections open, refusing {peer}");
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
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn pdu(op: u8, flags: u8, itt: u32, cmdsn: u32, data: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 48];
        b[0] = op;
        b[1] = flags;
        b[5..8].copy_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
        b[8..14].copy_from_slice(&[0x80, 0, 0, 0x12, 0x34, 0x56]);
        b[16..20].copy_from_slice(&itt.to_be_bytes());
        b[24..28].copy_from_slice(&cmdsn.to_be_bytes());
        b.extend(data);
        b.resize(b.len().div_ceil(4) * 4, 0);
        b
    }

    async fn login(s: &mut tokio::net::TcpStream, target: &str) -> conn::Pdu {
        let keys = format!("InitiatorName=iqn.test:pc-{target}\0TargetName={target}\0SessionType=Normal\0MaxRecvDataSegmentLength=4096\0MaxBurstLength=65536\0");
        s.write_all(&pdu(0x43, 0x80 | 1 << 2 | 3, 1, 0, keys.as_bytes())).await.unwrap();
        conn::read_pdu(s).await.unwrap()
    }

    /// Login + three READs sent back to back on one connection (answered concurrently), checked byte for byte; the
    /// session is counted while connected; an unknown target is refused.
    #[tokio::test]
    async fn serves_pipelined_reads() {
        let p = std::env::temp_dir().join("broom_test_iscsid.img");
        let file: Vec<u8> = (0..1u32 << 20).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        std::fs::write(&p, &file).unwrap();
        let iqn = "iqn.test:img.g1";
        export(Entry { iqn: iqn.into(), path: p.to_string_lossy().into(), ram: false, reserve: 0, stamp: String::new() }).unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                tokio::spawn(conn::serve(s, "127.0.0.1".into()));
            }
        });

        let mut bad = tokio::net::TcpStream::connect(addr).await.unwrap();
        let r = login(&mut bad, "iqn.test:nope").await;
        assert_eq!((r.bhs[0], r.bhs[36], r.bhs[37]), (0x23, 0x02, 0x03), "target not found");

        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let r = login(&mut s, iqn).await;
        assert_eq!((r.bhs[0], r.bhs[1] & 0x83, r.bhs[36]), (0x23, 0x83, 0), "login → full feature");
        assert_ne!(&r.bhs[14..16], &[0, 0], "TSIH given");
        assert!(String::from_utf8_lossy(&r.data).contains("TargetPortalGroupTag=1"));
        assert_eq!(list().iter().find(|t| t.iqn == iqn).unwrap().sessions, 1);

        let reads = [(10u32, 0u32, 8u16), (11, 100, 16), (12, 2040, 8)]; // (itt, lba, blocks)
        for (i, (itt, lba, blocks)) in reads.iter().enumerate() {
            let mut cmd = pdu(0x01, 0xc0, *itt, i as u32 + 1, &[]);
            cmd[8..16].fill(0); // LUN 0 (the helper puts an ISID there, for logins)
            cmd[20..24].copy_from_slice(&(*blocks as u32 * 512).to_be_bytes());
            cmd[32] = 0x28;
            cmd[34..38].copy_from_slice(&lba.to_be_bytes());
            cmd[39..41].copy_from_slice(&blocks.to_be_bytes());
            s.write_all(&cmd).await.unwrap();
        }
        let mut got: HashMap<u32, Vec<u8>> = HashMap::new();
        let mut done = 0;
        while done < reads.len() {
            let r = conn::read_pdu(&mut s).await.unwrap();
            assert_eq!(r.bhs[0], 0x25, "Data-In");
            assert!(r.data.len() <= 4096, "split to the initiator's segment size");
            let itt = u32::from_be_bytes(r.bhs[16..20].try_into().unwrap());
            let at = u32::from_be_bytes(r.bhs[40..44].try_into().unwrap()) as usize;
            let buf = got.entry(itt).or_default();
            buf.resize(buf.len().max(at + r.data.len()), 0);
            buf[at..at + r.data.len()].copy_from_slice(&r.data);
            if r.bhs[1] & 0x01 != 0 {
                assert_eq!(r.bhs[3], 0, "GOOD");
                done += 1;
            }
        }
        for (itt, lba, blocks) in reads {
            let off = lba as usize * 512;
            assert_eq!(got[&itt], &file[off..off + blocks as usize * 512], "ITT {itt}");
        }
        drop(s);
        for _ in 0..50 {
            if list().iter().find(|t| t.iqn == iqn).unwrap().sessions == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(list().iter().find(|t| t.iqn == iqn).unwrap().sessions, 0, "session freed on disconnect");
        TARGETS.lock().unwrap().remove(iqn);
        let _ = std::fs::remove_file(p);
    }

    fn scsi_cmd(itt: u32, cmdsn: u32, flags: u8, op: u8, lba: u32, blocks: u16) -> Vec<u8> {
        let mut c = pdu(0x01, flags, itt, cmdsn, &[]);
        c[8..16].fill(0);
        c[20..24].copy_from_slice(&(blocks as u32 * 512).to_be_bytes());
        c[32] = op;
        c[34..38].copy_from_slice(&lba.to_be_bytes());
        c[39..41].copy_from_slice(&blocks.to_be_bytes());
        c
    }

    async fn read_blocks(s: &mut tokio::net::TcpStream, itt: u32, cmdsn: u32, lba: u32, blocks: u16) -> Vec<u8> {
        s.write_all(&scsi_cmd(itt, cmdsn, 0xc0, 0x28, lba, blocks)).await.unwrap();
        let mut buf = vec![0u8; blocks as usize * 512];
        loop {
            let r = conn::read_pdu(s).await.unwrap();
            assert_eq!(r.bhs[0], 0x25, "Data-In");
            let at = u32::from_be_bytes(r.bhs[40..44].try_into().unwrap()) as usize;
            buf[at..at + r.data.len()].copy_from_slice(&r.data);
            if r.bhs[1] & 0x01 != 0 {
                return buf;
            }
        }
    }

    fn info_of(name: &str) -> games::Info {
        games::info().into_iter().find(|i| i.name == name).unwrap()
    }

    async fn wait_games(f: impl Fn(&games::Info) -> bool) {
        for _ in 0..100 {
            if f(&info_of("t")) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("games disk state not reached: {:?}", games::info());
    }

    /// Games disk: the update machine's WRITE comes in over two R2T bursts, lands in the update overlay and reads back;
    /// a read-only session is refused writes. "save" makes version g1 at once: a new session of g1 reads the update while
    /// a session still on g0 keeps reading the old content; once g0 is unused the layer is merged into games.img and
    /// g0 is no longer served.
    #[tokio::test]
    async fn games_disk_versions() {
        let d = std::env::temp_dir().join("broom_test_games");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let (img, img2) = (d.join("t/games.img"), d.join("b/games.img"));
        std::fs::create_dir_all(img.parent().unwrap()).unwrap();
        std::fs::create_dir_all(img2.parent().unwrap()).unwrap();
        std::fs::write(&img, vec![0x11u8; 1 << 20]).unwrap();
        let (iqn, g0, g1) = ("iqn.test:games", "iqn.test:games.g0", "iqn.test:games.g1");
        std::fs::write(&img2, vec![0x22u8; 1 << 20]).unwrap();
        let other = games::Config { name: "b".into(), iqn: "iqn.test:games-b".into(), path: img2.to_string_lossy().into(), update_ip: None };
        let cfg = |ip: Option<&str>| vec![games::Config { name: "t".into(), iqn: iqn.into(), path: img.to_string_lossy().into(), update_ip: ip.map(Into::into) }, other.clone()];
        games::set(cfg(Some("127.0.0.1"))).unwrap();
        let mut all = games::iqns();
        all.sort();
        assert_eq!(all, ["iqn.test:games-b.g0", g0], "every disk served, each with its own versions");
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                tokio::spawn(conn::serve(s, "127.0.0.1".into()));
            }
        });

        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert_eq!(login(&mut s, g0).await.bhs[36], 0, "update machine logged in");
        let data: Vec<u8> = (0..128 << 10).map(|i: u32| (i % 253) as u8).collect();
        s.write_all(&scsi_cmd(20, 1, 0xa0, 0x2a, 8, 256)).await.unwrap(); // WRITE(10), F + W
        for burst in 0..2u32 {
            let r = conn::read_pdu(&mut s).await.unwrap();
            assert_eq!(r.bhs[0], 0x31, "R2T");
            let g = |at: usize| u32::from_be_bytes(r.bhs[at..at + 4].try_into().unwrap());
            assert_eq!((g(36), g(40), g(44)), (burst, burst * 65536, 65536), "R2TSN, offset, length");
            for (i, chunk) in data[g(40) as usize..(g(40) + g(44)) as usize].chunks(32768).enumerate() {
                let mut p = pdu(0x05, if i == 1 { 0x80 } else { 0 }, 20, 0, chunk);
                p[8..16].fill(0);
                p[20..24].copy_from_slice(&r.bhs[20..24]); // TTT
                p[36..40].copy_from_slice(&(i as u32).to_be_bytes());
                p[40..44].copy_from_slice(&(g(40) + i as u32 * 32768).to_be_bytes());
                s.write_all(&p).await.unwrap();
            }
        }
        let r = conn::read_pdu(&mut s).await.unwrap();
        assert_eq!((r.bhs[0], r.bhs[1] & 0x06, r.bhs[3]), (0x21, 0, 0), "GOOD, no residual");
        assert!(read_blocks(&mut s, 21, 2, 8, 256).await == data, "reads its own write");
        assert!(read_blocks(&mut s, 22, 3, 0, 8).await.iter().all(|&b| b == 0x11), "rest from the disk");
        assert!(games::save("t").is_err(), "update machine still connected");
        drop(s);

        games::set(cfg(None)).unwrap(); // update mode off: every session read-only
        let mut old = tokio::net::TcpStream::connect(addr).await.unwrap();
        login(&mut old, g0).await;
        assert!(read_blocks(&mut old, 30, 1, 8, 8).await.iter().all(|&b| b == 0x11), "g0 unchanged");
        old.write_all(&scsi_cmd(31, 2, 0xa0, 0x2a, 8, 1)).await.unwrap();
        let r = conn::read_pdu(&mut old).await.unwrap();
        assert_eq!((r.bhs[0], r.bhs[3]), (0x21, 0x02), "write refused, no R2T");

        wait_games(|i| !i.update_connected).await;
        assert!(games::save("t").unwrap().starts_with("version g1 ready"));
        assert!(games::iqns().iter().any(|i| i == g1), "new boots get g1");
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert_eq!(login(&mut s, g1).await.bhs[36], 0);
        assert!(read_blocks(&mut s, 40, 1, 8, 256).await == data, "g1 = the update");
        assert!(read_blocks(&mut old, 32, 3, 8, 8).await.iter().all(|&b| b == 0x11), "a client on g0 keeps g0");

        games::merge_if_idle("t", std::time::Duration::ZERO);
        assert_eq!(info_of("t").layers, 1, "not merged while g0 is in use");
        drop(old);
        wait_games(|i| i.old_sessions == 0).await;
        games::merge_if_idle("t", std::time::Duration::ZERO);
        let i = info_of("t");
        assert_eq!((i.ver, i.layers, i.merging), (1, 0, false), "merged");
        let disk = std::fs::read(&img).unwrap();
        assert!(disk[8 * 512..8 * 512 + data.len()] == data[..] && disk[..8 * 512].iter().all(|&b| b == 0x11));
        assert!(read_blocks(&mut s, 41, 2, 8, 256).await == data, "g1 session unaffected by the merge");
        let mut late = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert_eq!(login(&mut late, g0).await.bhs[36], 0x02, "g0 no longer served");
        drop(s);
        games::set(Vec::new()).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }


    #[test]
    fn requests_roundtrip() {
        let e = Entry { iqn: "iqn.x:a.g1".into(), path: "/i/a/image.img".into(), ram: true, reserve: 2 << 30, stamp: String::new() };
        let s = serde_json::to_string(&Req::Export(e.clone())).unwrap();
        assert!(s.contains("\"op\":\"export\""));
        assert!(matches!(serde_json::from_str::<Req>(&s).unwrap(), Req::Export(x) if x == e));
        assert!(matches!(serde_json::from_str::<Req>("{\"op\":\"list\"}").unwrap(), Req::List));
        // An older daemon's answer without the newer fields still parses.
        let r: Resp = serde_json::from_str("{\"targets\":[{\"iqn\":\"a\",\"sessions\":2,\"ram\":false}]}").unwrap();
        assert_eq!((r.targets[0].sessions, r.err, r.version.as_str()), (2, None, ""));
    }
}
