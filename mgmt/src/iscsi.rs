// iscsi.rs — mgmt's side of the iSCSI target: starts the daemon (iscsid/, same binary, `iscsid` subcommand) and drives
// it over its control socket. The daemon runs from its own copy of the binary (<home>/run/iscsid-<id>), outside the
// mgmt service's cgroup: restarting / upgrading mgmt (overwriting bootrom-mgmt) never stops it, so clients stay
// connected. A daemon of another build is replaced only once no client is logged in.
//
// Also clears targets an older version made in the kernel's LIO (configfs) — they hold port 3260.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub use crate::iscsid::TargetInfo;
use crate::iscsid::{Entry, Req, Resp, SOCK};

/// Identity of this build (blake3 of the executable): tells a daemon of another build apart, even with the same version.
pub fn exe_id() -> String {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        std::fs::read("/proc/self/exe").map_or_else(|_| env!("CARGO_PKG_VERSION").to_string(), |b| blake3::hash(&b).to_hex()[..16].to_string())
    })
    .clone()
}

/// One request to the daemon. Err = not running / unreachable, or its error.
fn call(req: &Req) -> Result<Resp, String> {
    use std::io::{BufRead, Write};
    let mut s = std::os::unix::net::UnixStream::connect(SOCK).map_err(|e| format!("iSCSI daemon not running ({SOCK}: {e})"))?;
    let mut line = serde_json::to_vec(req).unwrap();
    line.push(b'\n');
    s.write_all(&line).map_err(|e| format!("iSCSI daemon: {e}"))?;
    let mut out = String::new();
    std::io::BufReader::new(s).read_line(&mut out).map_err(|e| format!("iSCSI daemon: {e}"))?;
    let r: Resp = serde_json::from_str(&out).map_err(|e| format!("iSCSI daemon answer: {e}"))?;
    match r.err {
        Some(e) => Err(e),
        None => Ok(r),
    }
}

/// Like call(), starting the daemon first if it isn't running (it crashed and systemd hasn't restarted it yet…).
fn call_up(req: &Req) -> Result<Resp, String> {
    if std::os::unix::net::UnixStream::connect(SOCK).is_err() {
        ensure_daemon()?;
    }
    call(req)
}

/// Serve `path` read-only as target `iqn` (replaces one with that IQN). `ram`: from a compressed copy in RAM, refused
/// unless `reserve` bytes stay free next to it. Blocking (a RAM copy takes ~a minute for 12 GB).
pub fn export(iqn: &str, path: &str, ram: bool, reserve: u64) -> Result<(), String> {
    call_up(&Req::Export(Entry { iqn: iqn.into(), path: path.into(), ram, reserve, stamp: String::new() })).map(|_| ())
}

pub fn remove(iqn: &str) {
    let _ = call(&Req::Remove { iqn: iqn.into() });
}

/// Every target with its logged-in session count. None = no daemon (so no client can be logged in either).
pub fn list() -> Option<Vec<TargetInfo>> {
    call(&Req::List).ok().map(|r| r.targets)
}

/// Serve exactly these games disks.
pub fn games_set(disks: Vec<crate::iscsid::games::Config>) -> Result<(), String> {
    call_up(&Req::GamesSet { disks }).map(|_| ())
}

/// Every games disk served. None = no daemon (or one of a build without games disks).
pub fn games_status() -> Option<Vec<crate::iscsid::games::Info>> {
    call(&Req::GamesStatus).ok().map(|r| r.games)
}

pub fn games_save(name: &str) -> Result<String, String> {
    call(&Req::GamesSave { name: name.into() }).map(|r| r.msg)
}

pub fn games_discard(name: &str) -> Result<(), String> {
    call(&Req::GamesDiscard { name: name.into() }).map(|_| ())
}

/// The daemon of this build is running: reused if so; started if none; one of another build is asked to quit once no
/// client uses it (until then it keeps serving — upgrade_if_idle retries). Blocking.
pub fn ensure_daemon() -> Result<String, String> {
    match call(&Req::Version) {
        Ok(r) if r.version == exe_id() => Ok("iSCSI daemon running".into()),
        Ok(r) => match upgrade_if_idle() {
            true => Ok("iSCSI daemon upgraded".into()),
            false => Ok(format!("iSCSI daemon of an older build ({}) kept until its clients disconnect", r.version)),
        },
        Err(_) => start_daemon().map(|_| "iSCSI daemon started".into()),
    }
}

/// An older build's daemon with no client logged in → replace it with this build's. True if replaced.
pub fn upgrade_if_idle() -> bool {
    let Ok(v) = call(&Req::Version) else { return false };
    if v.version == exe_id() || list().is_none_or(|t| t.iter().any(|t| t.sessions > 0)) || games_status().is_some_and(|g| g.iter().any(|d| d.sessions > 0)) {
        return false;
    }
    tracing::info!("iSCSI daemon {} idle → replaced by {}", v.version, exe_id());
    let _ = call(&Req::Quit);
    // Its port and socket are free once it is gone.
    for _ in 0..50 {
        if std::os::unix::net::UnixStream::connect(SOCK).is_err() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    match start_daemon() {
        Ok(()) => true,
        Err(e) => {
            tracing::error!("iSCSI daemon: {e}");
            false
        }
    }
}

/// Copy of this binary the daemon runs from: <home>/run/iscsid-<id>. Older copies are deleted (a running one keeps
/// its file open, Linux frees it when it exits).
fn daemon_exe() -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::home().join("run");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let file = dir.join(format!("iscsid-{}", exe_id()));
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("iscsid-") && e.path() != file {
            let _ = std::fs::remove_file(e.path());
        }
    }
    if !file.exists() {
        let tmp = file.with_extension("tmp");
        std::fs::copy("/proc/self/exe", &tmp)
            .and_then(|_| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700)))
            .and_then(|_| std::fs::rename(&tmp, &file))
            .map_err(|e| format!("copy the binary to {}: {e}", file.display()))?;
    }
    Ok(file)
}

fn start_daemon() -> Result<(), String> {
    let exe = daemon_exe()?;
    let state = crate::home().join("iscsid-targets.json");
    let args = ["iscsid", "--state", &state.to_string_lossy()].map(String::from);
    if Path::new("/run/systemd/system").is_dir() {
        // Its own transient unit: outside bootrom-mgmt.service, whose stop/restart kills every process of its cgroup.
        let _ = Command::new("systemctl").args(["reset-failed", "broom-iscsid"]).output();
        let mut c = Command::new("systemd-run");
        c.args(["--unit=broom-iscsid", "--collect", "-p", "Restart=on-failure", "-p", "RestartSec=2"]).arg(&exe).args(&args);
        let o = c.output().map_err(|e| format!("systemd-run: {e}"))?;
        if !o.status.success() {
            return Err(format!("systemd-run broom-iscsid: {}", String::from_utf8_lossy(&o.stderr).trim()));
        }
    } else {
        // No systemd: its own session, so it outlives this process; output to <home>/iscsid.log.
        use std::os::unix::process::CommandExt;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(crate::home().join("iscsid.log")).map_err(|e| e.to_string())?;
        let mut c = Command::new(&exe);
        c.args(&args).stdin(std::process::Stdio::null()).stdout(log.try_clone().map_err(|e| e.to_string())?).stderr(log);
        // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        c.spawn().map_err(|e| format!("start {}: {e}", exe.display()))?;
    }
    for _ in 0..100 {
        if call(&Req::Version).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err("iSCSI daemon did not come up in 10 s (journalctl -u broom-iscsid, or iscsid.log next to the binary)".into())
}

const CONFIGFS: &str = "/sys/kernel/config/target";

/// Remove the LIO targets an older version made (IQNs starting with one of `prefixes`) and their backstores, so the
/// kernel frees port 3260 for the daemon. Their clients log in again to the daemon (same IQN). True if any was there.
pub fn clear_lio(prefixes: &[&str]) -> bool {
    let root = Path::new(CONFIGFS);
    let subdirs = |p: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir() && !p.is_symlink()).collect()).unwrap_or_default()
    };
    let mut any = false;
    for t in subdirs(&root.join("iscsi")) {
        let iqn = t.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !prefixes.iter().any(|p| iqn.starts_with(p)) {
            continue;
        }
        any = true;
        for tpg in subdirs(&t).into_iter().filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("tpgt_"))) {
            let _ = std::fs::write(tpg.join("enable"), "0");
            for lun in subdirs(&tpg.join("lun")) {
                for e in std::fs::read_dir(&lun).into_iter().flatten().flatten() {
                    if e.path().is_symlink() {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
                let _ = std::fs::remove_dir(&lun);
            }
            for np in subdirs(&tpg.join("np")) {
                let _ = std::fs::remove_dir(&np);
            }
            let _ = std::fs::remove_dir(&tpg);
        }
        let _ = std::fs::remove_dir(&t);
        // Its backstore was named after the IQN's last part (<name>.g<gen>, or <name> for the oldest targets).
        let store = iqn.rsplit(':').next().unwrap_or_default().to_string();
        for hba in subdirs(&root.join("core")) {
            let _ = std::fs::remove_dir(hba.join(&store));
        }
        tracing::info!("removed kernel LIO target {iqn} (now served by the iSCSI daemon)");
    }
    any
}
