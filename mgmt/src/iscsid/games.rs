// iscsid/games.rs — the games disks: shared disks served to the Windows clients READ-ONLY (which machine gets which
// disk is mgmt's business, games.rs there); each client puts a differencing VHDX on its own SSD over the games.vhdx
// inside a disk, so its writes stay local and go at reboot (broom-games.ps1).
//
// Versions ("generations"), per disk: gen k = games.img + the frozen layers 1..=k on top (layer-<k>/, the blocks that
// update changed, overlay.rs), each served as target "<iqn>.g<k>". The disk's update machine (by IP, set by mgmt) gets
// the current gen writable, its writes in the update overlay; "save" freezes that into the next layer = a new gen at
// once. Clients connect to the newest gen when they boot; a client already playing keeps its gen until it reboots.
// Once no client has used an older gen for a while, the layers are merged into games.img in the background — safe
// while clients of the newest gen read: merging only writes blocks a layer covers, and they read those from the layer.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::overlay::Overlay;
use super::Lun;

/// An older gen nobody used for this long is dropped (its layers merged): a client that lost its connection reconnects
/// within it.
const GRACE: Duration = Duration::from_secs(600);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Config {
    /// The disk's name (mgmt's list).
    pub name: String,
    /// Target name prefix: gen k is "<iqn>.g<k>".
    pub iqn: String,
    /// Its games.img; its folder holds the layers and the update.
    pub path: String,
    /// IP of the disk's update machine (None = no update running).
    #[serde(default)]
    pub update_ip: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Info {
    pub name: String,
    /// The gen clients get when they boot.
    pub ver: u64,
    pub sessions: usize,
    /// Sessions still on an older gen (clients that have not rebooted since the last save).
    pub old_sessions: usize,
    /// Layers not merged into games.img yet, and their size.
    pub layers: usize,
    pub layer_bytes: u64,
    /// The update not saved yet.
    pub update_bytes: u64,
    pub update_connected: bool,
    pub merging: bool,
}

/// What a session of one gen reads: games.img under `layers` (oldest first); `ov` = its writes (the update machine).
pub struct View {
    pub disk: String,
    pub ver: u64,
    pub layers: Vec<Arc<Overlay>>,
    pub ov: Option<Arc<Overlay>>,
}

struct Layer {
    ver: u64,
    ov: Arc<Overlay>,
}

struct State {
    cfg: Config,
    lun: Arc<Lun>,
    dir: PathBuf,
    /// games.img holds gens up to this one.
    base_ver: u64,
    ver: u64,
    layers: Vec<Layer>,
    sessions: HashMap<u64, usize>,
    /// Since when no session uses an older gen.
    old_idle: Instant,
    update: Option<Arc<Overlay>>,
    update_users: usize,
    /// Merging the layers up to this gen: older gens are no longer served.
    merging: Option<u64>,
}

/// The disks, by name.
static DISKS: Mutex<Option<HashMap<String, State>>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut HashMap<String, State>) -> T) -> T {
    f(DISKS.lock().unwrap().get_or_insert_with(HashMap::new))
}

fn update_files(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join("update.ovl"), dir.join("update.blocks"))
}

fn layer_dir(dir: &Path, ver: u64) -> PathBuf {
    dir.join(format!("layer-{ver}"))
}

fn open_layer(dir: &Path, ver: u64, size: u64) -> Result<Overlay, String> {
    let d = layer_dir(dir, ver);
    Overlay::open(&d.join("ovl"), &d.join("blocks"), size)
}

/// Where the configs are kept for the daemon's next start (next to its state file).
fn cfg_file() -> Option<PathBuf> {
    super::state_path().map(|p| p.with_file_name("iscsid-games.json"))
}

fn open_lun(cfg: &Config) -> Result<Arc<Lun>, String> {
    let f = std::fs::File::open(&cfg.path).map_err(|e| format!("{}: {e}", cfg.path))?;
    let size = f.metadata().map_err(|e| e.to_string())?.len();
    Ok(Arc::new(Lun { iqn: cfg.iqn.clone(), size, data: super::Data::File(f), sessions: AtomicUsize::new(0) }))
}

/// Blocking read of gen view `layers` (oldest first) over games.img.
pub fn read_chain(layers: &[Arc<Overlay>], lun: &Lun, off: u64, buf: &mut [u8]) -> Result<(), String> {
    match layers.split_last() {
        None => lun.read_at(off, buf),
        Some((top, rest)) => top.read_at(&|o, b| read_chain(rest, lun, o, b), off, buf),
    }
}

/// The state of `dir` on disk: games.img's gen, the layers above it. Leftovers of a crash are cleaned up.
fn load(dir: &Path, size: u64) -> Result<(u64, Vec<Layer>), String> {
    let base_ver: u64 = std::fs::read_to_string(dir.join("base.gen")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let mut layers = Vec::new();
    for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(n) = name.strip_prefix("layer-") else { continue };
        match n.parse::<u64>() {
            Ok(g) if g > base_ver => layers.push(Layer { ver: g, ov: Arc::new(open_layer(dir, g, size)?) }),
            _ => {
                let _ = std::fs::remove_dir_all(e.path()); // merged already, or a save cut off (.tmp)
            }
        }
    }
    layers.sort_by_key(|l| l.ver);
    if layers.is_empty() {
        let _ = std::fs::remove_file(dir.join("merging")); // a merge that ended right before removing it
    }
    // A save cut off after the layer was in place: the update files are the same files (hard links) → gone.
    use std::os::unix::fs::MetadataExt;
    let (ovl, list) = update_files(dir);
    if let (Some(top), Ok(u)) = (layers.last(), std::fs::metadata(&ovl)) {
        if std::fs::metadata(layer_dir(dir, top.ver).join("ovl")).is_ok_and(|m| m.ino() == u.ino()) {
            let _ = std::fs::remove_file(&ovl);
            let _ = std::fs::remove_file(&list);
        }
    }
    Ok((base_ver, layers))
}

/// Serve exactly the disks `cfgs` (others stop being served; their connected clients stay connected). Each disk is
/// set up on its own: one that fails doesn't keep the others from being served.
pub fn set(cfgs: Vec<Config>) -> Result<(), String> {
    let mut errs = Vec::new();
    with(|d| d.retain(|name, _| cfgs.iter().any(|c| &c.name == name)));
    for c in &cfgs {
        if let Err(e) = set_one(c.clone()) {
            errs.push(format!("games disk {}: {e}", c.name));
        }
    }
    if let Some(p) = cfg_file() {
        let _ = std::fs::write(p, serde_json::to_vec(&cfgs).unwrap());
    }
    if errs.is_empty() { Ok(()) } else { Err(errs.join("; ")) }
}

fn set_one(cfg: Config) -> Result<(), String> {
    let dir = Path::new(&cfg.path).parent().ok_or("path has no folder")?.to_path_buf();
    let size = std::fs::metadata(&cfg.path).map_err(|e| format!("{}: {e}", cfg.path))?.len();
    let name = cfg.name.clone();
    with(|d| -> Result<(), String> {
        match d.get_mut(&name) {
            Some(s) if s.cfg.path == cfg.path => {
                if s.lun.size != size {
                    if s.update_users > 0 {
                        return Err("the update machine is connected — shut it down first".into());
                    }
                    // The disk grew: sessions from now on see the new size; the update overlay grows on reopen.
                    s.lun = open_lun(&cfg)?;
                    s.update = None;
                }
                s.cfg = cfg;
            }
            _ => {
                let lun = open_lun(&cfg)?;
                let (base_ver, layers) = load(&dir, size)?;
                let ver = layers.last().map_or(base_ver, |l| l.ver);
                let s = State {
                    cfg,
                    lun,
                    dir: dir.clone(),
                    base_ver,
                    ver,
                    layers,
                    sessions: HashMap::new(),
                    old_idle: Instant::now(),
                    update: None,
                    update_users: 0,
                    merging: None,
                };
                d.insert(name.clone(), s);
            }
        }
        Ok(())
    })?;
    // A merge cut off by a crash / restart: games.img is half merged, only the newest gen reads it right → finish now.
    if dir.join("merging").exists() {
        tracing::warn!("games disk {name}: finishing a merge cut off earlier");
        merge_if_idle(&name, Duration::ZERO);
    }
    Ok(())
}

/// At daemon start: the configs of the previous run.
pub fn restore() {
    let Some(cfgs) = cfg_file().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<Vec<Config>>(&b).ok()) else { return };
    if let Err(e) = set(cfgs) {
        tracing::error!("{e}");
    }
}

/// The disk and gen `iqn` names, if it is a gen still served.
fn find<'a>(d: &'a mut HashMap<String, State>, iqn: &str) -> Option<(&'a mut State, u64)> {
    d.values_mut().find_map(|s| {
        let k: u64 = iqn.strip_prefix(&s.cfg.iqn)?.strip_prefix(".g")?.parse().ok()?;
        let ok = k >= s.merging.unwrap_or(s.base_ver) && k <= s.ver;
        ok.then_some((s, k))
    })
}

/// games.img of the disk if `iqn` names a gen still served.
pub fn lun(iqn: &str) -> Option<Arc<Lun>> {
    with(|d| find(d, iqn).map(|(s, _)| s.lun.clone()))
}

/// Target names of every disk's newest gen (discovery).
pub fn iqns() -> Vec<String> {
    with(|d| d.values().map(|s| format!("{}.g{}", s.cfg.iqn, s.ver)).collect())
}

/// A target name of a games disk (any gen): its sessions attach / detach.
pub fn owns(iqn: &str) -> bool {
    with(|d| d.values().any(|s| iqn.strip_prefix(&s.cfg.iqn).is_some_and(|r| r.starts_with(".g"))))
}

/// A session of `iqn` from `peer` logs in. The disk's update machine on the newest gen gets the update overlay
/// (writable). Every Ok is paired with a `detach`.
pub fn attach(iqn: &str, peer: &str) -> Result<View, String> {
    with(|d| {
        let (s, ver) = find(d, iqn).ok_or("version no longer served")?;
        let layers = s.layers.iter().filter(|l| l.ver <= ver).map(|l| l.ov.clone()).collect();
        let mut ov = None;
        if ver == s.ver && s.cfg.update_ip.as_deref() == Some(peer) {
            if s.update.is_none() {
                let (ovl, list) = update_files(&s.dir);
                s.update = Some(Arc::new(Overlay::open(&ovl, &list, s.lun.size)?));
            }
            s.update_users += 1;
            ov = s.update.clone();
            tracing::info!("games disk {}: {peer} in UPDATE mode on g{ver} — its writes are kept for saving", s.cfg.name);
        }
        *s.sessions.entry(ver).or_default() += 1;
        Ok(View { disk: s.cfg.name.clone(), ver, layers, ov })
    })
}

pub fn detach(v: &View) {
    with(|d| {
        let Some(s) = d.get_mut(&v.disk) else { return };
        if let Some(n) = s.sessions.get_mut(&v.ver) {
            *n -= 1;
            if *n == 0 {
                s.sessions.remove(&v.ver);
                if v.ver < s.ver && old_sessions(s) == 0 {
                    s.old_idle = Instant::now();
                }
            }
        }
        if let Some(ov) = &v.ov {
            if s.update.as_ref().is_some_and(|u| Arc::ptr_eq(u, ov)) {
                s.update_users = s.update_users.saturating_sub(1);
            }
        }
    });
    // Outside the lock: an fsync.
    if let Some(ov) = &v.ov {
        if let Err(e) = ov.flush() {
            tracing::error!("games disk {} update: {e}", v.disk);
        }
    }
}

fn old_sessions(s: &State) -> usize {
    s.sessions.iter().filter(|(g, _)| **g < s.ver).map(|(_, n)| n).sum()
}

/// Every 30 s: merge each disk's layers into its games.img once no session has used an older gen for GRACE. Blocking.
pub fn tick() {
    for name in with(|d| d.keys().cloned().collect::<Vec<_>>()) {
        merge_if_idle(&name, GRACE);
    }
}

pub fn merge_if_idle(name: &str, grace: Duration) {
    let job = with(|d| {
        let s = d.get_mut(name)?;
        if s.layers.is_empty() || s.merging.is_some() || old_sessions(s) > 0 || s.old_idle.elapsed() < grace {
            return None;
        }
        let target = s.ver;
        s.merging = Some(target); // older gens are not served from now on
        let layers: Vec<(u64, Arc<Overlay>)> = s.layers.iter().map(|l| (l.ver, l.ov.clone())).collect();
        Some((target, layers, s.cfg.path.clone(), s.dir.clone()))
    });
    let Some((target, layers, path, dir)) = job else { return };
    let marker = dir.join("merging");
    let r = (|| -> Result<(), String> {
        std::fs::write(&marker, target.to_string()).map_err(|e| format!("{}: {e}", marker.display()))?;
        let f = std::fs::OpenOptions::new().write(true).open(&path).map_err(|e| format!("{path}: {e}"))?;
        for (_, l) in &layers {
            l.merge_into(&f)?; // oldest first: a newer layer's block wins
        }
        let tmp = dir.join("base.gen.tmp");
        std::fs::write(&tmp, target.to_string()).and_then(|_| std::fs::rename(&tmp, dir.join("base.gen"))).map_err(|e| format!("base.gen: {e}"))
    })();
    with(|d| {
        let Some(s) = d.get_mut(name) else { return };
        s.merging = None;
        match r {
            Ok(()) => {
                // Sessions holding these layers keep reading them (open files) — same bytes as games.img now.
                s.base_ver = target;
                s.layers.retain(|l| l.ver > target);
                for (g, _) in &layers {
                    let _ = std::fs::remove_dir_all(layer_dir(&dir, *g));
                }
                let _ = std::fs::remove_file(&marker);
                tracing::info!("games disk {name}: versions up to g{target} merged into {path}");
            }
            Err(e) => tracing::error!("games disk {name}: merge failed (retried later): {e}"),
        }
    });
}

/// Freeze the update into a new gen, served at once (machines get it at their next boot). The update machine must be
/// off: its file system must be on disk, whole.
pub fn save(name: &str) -> Result<String, String> {
    with(|d| {
        let s = d.get_mut(name).ok_or("games disk not served")?;
        if s.update_users > 0 {
            return Err("the update machine is still connected — shut it down properly first".into());
        }
        let (ovl, list) = update_files(&s.dir);
        let ov = match s.update.take() {
            Some(o) => o,
            None if ovl.exists() => Arc::new(Overlay::open(&ovl, &list, s.lun.size)?),
            None => return Err("no update to save".into()),
        };
        ov.flush()?;
        if ov.used() == 0 {
            return Err("the update is empty".into());
        }
        let ver = s.ver + 1;
        // Hard links into layer-<ver>.tmp, then one rename: the layer appears whole or not at all.
        let (tmp, dst) = (s.dir.join(format!("layer-{ver}.tmp")), layer_dir(&s.dir, ver));
        let _ = std::fs::remove_dir_all(&tmp);
        let r = std::fs::create_dir(&tmp)
            .and_then(|_| std::fs::hard_link(&ovl, tmp.join("ovl")))
            .and_then(|_| std::fs::hard_link(&list, tmp.join("blocks")))
            .and_then(|_| std::fs::rename(&tmp, &dst));
        if let Err(e) = r {
            let _ = std::fs::remove_dir_all(&tmp);
            s.update = Some(ov);
            return Err(format!("{}: {e}", dst.display()));
        }
        drop(ov);
        let _ = std::fs::remove_file(&ovl);
        let _ = std::fs::remove_file(&list);
        let layer = open_layer(&s.dir, ver, s.lun.size)?;
        s.layers.push(Layer { ver, ov: Arc::new(layer) });
        s.ver = ver;
        if old_sessions(s) == 0 {
            s.old_idle = Instant::now();
        }
        tracing::info!("games disk {name}: version g{ver} ready");
        Ok(format!("version g{ver} ready: each machine gets it at its next boot"))
    })
}

/// Throw the update away (the update machine must be off).
pub fn discard(name: &str) -> Result<(), String> {
    with(|d| {
        let s = d.get_mut(name).ok_or("games disk not served")?;
        if s.update_users > 0 {
            return Err("the update machine is still connected — shut it down first".into());
        }
        let (ovl, list) = update_files(&s.dir);
        drop(s.update.take());
        let _ = std::fs::remove_file(ovl);
        let _ = std::fs::remove_file(list);
        Ok(())
    })
}

pub fn info() -> Vec<Info> {
    with(|d| {
        d.values()
            .map(|s| Info {
                name: s.cfg.name.clone(),
                ver: s.ver,
                sessions: s.sessions.values().sum(),
                old_sessions: old_sessions(s),
                layers: s.layers.len(),
                layer_bytes: s.layers.iter().map(|l| l.ov.used()).sum(),
                update_bytes: match &s.update {
                    Some(u) => u.used(),
                    None => std::fs::metadata(update_files(&s.dir).1).map_or(0, |m| m.len() / 4 * super::overlay::BLOCK),
                },
                update_connected: s.update_users > 0,
                merging: s.merging.is_some(),
            })
            .collect()
    })
}
