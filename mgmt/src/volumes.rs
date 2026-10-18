// volumes.rs — more disks for the server: images and games disks can live on another disk than the binary's home.
// A volume = a disk formatted here (ext4, else xfs: mkfs from the distro, the only outside tool) and mounted by broom
// itself at <home>/disks/<id> at every start (no fstab), or a folder the admin mounted (LVM, RAID…).
// An image / games disk on a volume = a symlink at its usual place (images/<name>, storage/files/<name>,
// games/<name>) to the same name on the volume: every path the rest of the code builds stays valid, a version is
// still a hard link of its golden (both on the volume), and free-space checks see the volume (statvfs follows).
// Kept in the config table ("volumes", JSON).
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::SharedState;
use crate::api::{ApiError, bad, ise, ok};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/volumes", get(list))
        .route("/api/volumes/format", post(format_disk))
        .route("/api/volumes/folder", post(add_folder))
        .route("/api/volumes/remove", post(remove))
        .route("/api/volumes/move", post(move_item))
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Volume {
    pub id: String,
    pub path: PathBuf,
    /// Formatted here: mounted by broom from /dev/disk/by-uuid/<uuid>. None = a folder the admin mounted.
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub fstype: Option<String>,
}

fn load(st: &SharedState) -> Vec<Volume> {
    serde_json::from_str(&st.db.get_config("volumes", "[]")).unwrap_or_default()
}

fn save(st: &SharedState, v: &[Volume]) -> Result<(), String> {
    st.db
        .set_config("volumes", &serde_json::to_string(v).unwrap())
}

// ---------- where things live ----------

/// The places an image / games disk keeps its files (home side), each with its sub-path on a volume.
fn places(kind: &str, name: &str) -> Vec<(PathBuf, PathBuf)> {
    match kind {
        "image" => vec![
            (
                crate::images_dir().join(name),
                Path::new("images").join(name),
            ),
            (
                crate::versions::storage_dir().join("files").join(name),
                Path::new("storage/files").join(name),
            ),
        ],
        _ => vec![(
            crate::home().join("games").join(name),
            Path::new("games").join(name),
        )],
    }
}

/// The volume an image / games disk is on (its first place is a symlink into it); None = home.
pub fn volume_of(st: &SharedState, kind: &str, name: &str) -> Option<String> {
    let (home, _) = places(kind, name).into_iter().next()?;
    let to = std::fs::read_link(home).ok()?;
    load(st)
        .into_iter()
        .find(|v| to.starts_with(&v.path))
        .map(|v| v.id)
}

/// A new image / games disk created on volume `vol` (None = home): its folders made there, linked from home.
pub fn create_on(
    st: &SharedState,
    vol: Option<&str>,
    kind: &str,
    name: &str,
) -> Result<(), String> {
    let Some(id) = vol.filter(|v| !v.is_empty() && *v != "home") else {
        return Ok(());
    };
    let v = find(st, id)?;
    if !mounted_or_folder(&v) {
        return Err(format!("disk {} is not mounted", v.id));
    }
    for (home, sub) in places(kind, name) {
        if std::fs::symlink_metadata(&home).is_ok() {
            continue; // already there (an upload retried)
        }
        let to = v.path.join(sub);
        std::fs::create_dir_all(&to).map_err(|e| format!("{}: {e}", to.display()))?;
        if let Some(p) = home.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        std::os::unix::fs::symlink(&to, &home).map_err(|e| format!("{}: {e}", home.display()))?;
    }
    Ok(())
}

/// Remove a folder that may be a symlink to a volume: the files on the volume go too (remove_dir_all alone only
/// drops the link).
pub fn remove_tree(p: &Path) -> std::io::Result<()> {
    match std::fs::read_link(p) {
        Ok(to) => {
            let r = std::fs::remove_dir_all(&to);
            std::fs::remove_file(p)?;
            r.or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(e)
                }
            })
        }
        Err(_) => std::fs::remove_dir_all(p),
    }
}

/// Remove an empty folder, or a symlink to an empty folder on a volume (and that folder).
pub fn remove_dir_if_empty(p: &Path) {
    match std::fs::read_link(p) {
        Ok(to) => {
            if std::fs::remove_dir(&to).is_ok() {
                let _ = std::fs::remove_file(p);
            }
        }
        Err(_) => {
            let _ = std::fs::remove_dir(p);
        }
    }
}

/// The volumes usable now (the Disk space alert).
pub fn mounted(st: &SharedState) -> Vec<PathBuf> {
    load(st)
        .into_iter()
        .filter(mounted_or_folder)
        .map(|v| v.path)
        .collect()
}

fn find(st: &SharedState, id: &str) -> Result<Volume, String> {
    load(st)
        .into_iter()
        .find(|v| v.id == id)
        .ok_or_else(|| format!("no disk {id}"))
}

// ---------- mounts ----------

/// Mount points by device number (major:minor) from /proc/self/mountinfo, with the filesystem type.
fn mounts() -> HashMap<String, (String, String)> {
    parse_mountinfo(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default())
}

pub(crate) fn parse_mountinfo(s: &str) -> HashMap<String, (String, String)> {
    let mut m = HashMap::new();
    for l in s.lines() {
        let f: Vec<&str> = l.split(' ').collect();
        let Some(sep) = f.iter().position(|x| *x == "-") else {
            continue;
        };
        if f.len() < 5 || f.len() <= sep + 1 {
            continue;
        }
        // Mount points escape space / tab / newline / backslash as octal.
        let point = f[4]
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        m.entry(f[2].to_string())
            .or_insert((point, f[sep + 1].to_string()));
    }
    m
}

fn is_mountpoint(p: &Path) -> bool {
    let Ok(c) = std::fs::canonicalize(p) else {
        return false;
    };
    mounts().values().any(|(mp, _)| Path::new(mp) == c)
}

fn mounted_or_folder(v: &Volume) -> bool {
    if v.uuid.is_some() {
        is_mountpoint(&v.path)
    } else {
        v.path.is_dir()
    }
}

fn mount(dev: &Path, at: &Path, fstype: &str) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|e| e.to_string());
    let (src, dst, t) = (
        c(dev)?,
        c(at)?,
        std::ffi::CString::new(fstype).map_err(|e| e.to_string())?,
    );
    // SAFETY: valid NUL-terminated strings; no data argument.
    let r = unsafe {
        libc::mount(
            src.as_ptr(),
            dst.as_ptr(),
            t.as_ptr(),
            libc::MS_NOATIME,
            std::ptr::null(),
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(format!(
            "mount {} on {}: {}",
            dev.display(),
            at.display(),
            std::io::Error::last_os_error()
        ))
    }
}

/// At start: every disk formatted here mounted again (before the images / games disks on it are served).
pub fn mount_all(st: &SharedState) {
    for v in load(st) {
        let Some(uuid) = &v.uuid else { continue };
        if is_mountpoint(&v.path) {
            continue;
        }
        let dev = Path::new("/dev/disk/by-uuid").join(uuid);
        let _ = std::fs::create_dir_all(&v.path);
        match mount(&dev, &v.path, v.fstype.as_deref().unwrap_or("ext4")) {
            Ok(()) => tracing::info!("disk {} mounted on {}", v.id, v.path.display()),
            Err(e) => tracing::error!(
                "disk {}: {e} — the images / iSCSI disks on it are not available (disk unplugged?)",
                v.id
            ),
        }
    }
}

// ---------- the server's disks ----------

#[derive(Serialize)]
struct Part {
    name: String,
    size: u64,
    mount: Option<String>,
    fstype: Option<String>,
}

#[derive(Serialize)]
struct BlockDisk {
    name: String,
    size: u64,
    model: String,
    rotational: bool,
    parts: Vec<Part>,
    /// Mounted on the whole disk (no partition table).
    mount: Option<String>,
    fstype: Option<String>,
    /// Device-mapper / RAID devices built on it (LVM, md, LUKS).
    holders: Vec<String>,
    /// "system" (/, /boot, broom's home) | "in use" | "broom" (a volume) | "free" (may be formatted).
    state: &'static str,
    /// Bytes on it that look like a filesystem / partition table (shown before a format).
    old_data: bool,
    health: Option<crate::sysmon::DiskHealth>,
}

fn read(p: impl AsRef<Path>) -> String {
    std::fs::read_to_string(p)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Mount point of a holder chain (dm-0 → mounted at /), for "system".
fn holder_mounts(name: &str, mounts: &HashMap<String, (String, String)>) -> Vec<String> {
    let mut out = Vec::new();
    for h in std::fs::read_dir(format!("/sys/block/{name}/holders"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let h = h.file_name().to_string_lossy().to_string();
        if let Some((mp, _)) = mounts.get(&read(format!("/sys/block/{h}/dev"))) {
            out.push(mp.clone());
        }
        out.extend(holder_mounts(&h, mounts));
    }
    out
}

fn holders(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir.join("holders"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|h| h.file_name().to_string_lossy().to_string())
        .collect()
}

/// First 4 KiB look like a partition table or a filesystem (MBR/GPT, ext, xfs, NTFS).
fn has_signature(b: &[u8]) -> bool {
    b.len() >= 1082
        && ((b[510] == 0x55 && b[511] == 0xAA)
            || &b[512..520] == b"EFI PART"
            || (b[1080] == 0x53 && b[1081] == 0xEF)
            || &b[0..4] == b"XFSB"
            || &b[3..11] == b"NTFS    ")
}

fn disks(st: &SharedState) -> Vec<BlockDisk> {
    let mounts = mounts();
    let swaps = std::fs::read_to_string("/proc/swaps").unwrap_or_default();
    let vols = load(st);
    let home = std::fs::canonicalize(crate::home()).unwrap_or_else(|_| crate::home().into());
    // The OS's own mounts, or the one broom's folder is on.
    let system = |mp: &str| {
        matches!(mp, "/" | "/boot" | "/boot/efi" | "/usr" | "/var") || home.starts_with(mp)
    };
    let mut out = Vec::new();
    for d in std::fs::read_dir("/sys/block")
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = d.file_name().to_string_lossy().to_string();
        if ["loop", "ram", "zram", "dm-", "sr", "fd", "nbd", "md"]
            .iter()
            .any(|p| name.starts_with(p))
        {
            continue;
        }
        let dir = d.path();
        let size = read(dir.join("size")).parse::<u64>().unwrap_or(0) * 512;
        if size == 0 {
            continue;
        }
        let mut parts = Vec::new();
        let mut points = Vec::new();
        let mut hold = holders(&dir);
        for p in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let pn = p.file_name().to_string_lossy().to_string();
            if !pn.starts_with(&name) || !p.path().join("partition").exists() {
                continue;
            }
            let m = mounts.get(&read(p.path().join("dev"))).cloned();
            points.extend(m.as_ref().map(|x| x.0.clone()));
            points.extend(holder_mounts(&pn, &mounts));
            hold.extend(holders(&p.path()));
            parts.push(Part {
                size: read(p.path().join("size")).parse::<u64>().unwrap_or(0) * 512,
                mount: m.as_ref().map(|x| x.0.clone()),
                fstype: m.map(|x| x.1),
                name: pn,
            });
        }
        parts.sort_by(|a, b| a.name.cmp(&b.name));
        let whole = mounts.get(&read(dir.join("dev"))).cloned();
        points.extend(whole.as_ref().map(|x| x.0.clone()));
        points.extend(holder_mounts(&name, &mounts));
        let swap = swaps.lines().skip(1).any(|l| {
            l.split_whitespace()
                .next()
                .is_some_and(|dev| dev.trim_start_matches("/dev/").starts_with(&name))
        });
        let state = if points.iter().any(|mp| system(mp)) {
            "system"
        } else if points
            .iter()
            .any(|mp| vols.iter().any(|v| Path::new(mp) == v.path))
        {
            "broom"
        } else if !points.is_empty() || !hold.is_empty() || swap {
            "in use"
        } else {
            "free"
        };
        let mut head = vec![0u8; 4096];
        let old_data = !parts.is_empty()
            || std::fs::File::open(format!("/dev/{name}"))
                .and_then(|f| f.read_exact_at(&mut head, 0))
                .is_ok_and(|()| has_signature(&head));
        out.push(BlockDisk {
            model: read(dir.join("device/model")),
            rotational: read(dir.join("queue/rotational")) == "1",
            health: crate::sysmon::disk_health(&name),
            mount: whole.as_ref().map(|x| x.0.clone()),
            fstype: whole.map(|x| x.1),
            holders: hold,
            name,
            size,
            parts,
            state,
            old_data,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// ---------- format ----------

/// UUID of a fresh ext4 (superblock at 1024, s_uuid at +0x68) or xfs (sb_uuid at 32), as /dev/disk/by-uuid names it.
pub(crate) fn fs_uuid(head: &[u8], fstype: &str) -> Option<String> {
    let at = if fstype == "xfs" { 32 } else { 1024 + 0x68 };
    let u = head.get(at..at + 16)?;
    if u.iter().all(|b| *b == 0) {
        return None;
    }
    let h: String = u.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

/// Wipe `disk` (whole), make one filesystem on it, mount it under <home>/disks/. Blocking.
fn format_blocking(st: &SharedState, disk: &str) -> Result<Volume, String> {
    let d = disks(st)
        .into_iter()
        .find(|d| d.name == disk)
        .ok_or_else(|| format!("no disk {disk}"))?;
    if d.state != "free" {
        return Err(format!("{disk} is {} — not formatted", d.state));
    }
    let dev = format!("/dev/{disk}");
    {
        // O_EXCL on a block device: the kernel refuses while anything holds it (mounted, LVM, RAID, swap).
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_EXCL)
            .open(&dev)
            .map_err(|e| format!("{dev}: {e} (in use?)"))?;
        // Old partition tables / filesystem signatures at both ends (GPT keeps a copy at the end) go first, then the
        // kernel forgets the old partitions.
        let zero = vec![0u8; 1 << 20];
        f.write_all_at(&zero, 0)
            .map_err(|e| format!("{dev}: {e}"))?;
        f.write_all_at(&zero, d.size.saturating_sub(1 << 20))
            .map_err(|e| format!("{dev}: {e}"))?;
        f.sync_all().map_err(|e| format!("{dev}: {e}"))?;
        const BLKRRPART: libc::Ioctl = 0x125F as libc::Ioctl;
        // SAFETY: plain ioctl without argument on an open block device.
        unsafe { libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(&f), BLKRRPART) };
    }
    // ≤ 12 characters: xfs's limit (ext4: 16).
    let label: String = "broom"
        .chars()
        .chain(disk.chars().filter(char::is_ascii_alphanumeric))
        .take(12)
        .collect();
    let run = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .map(|o| {
                (
                    o.status.success(),
                    String::from_utf8_lossy(&o.stderr).to_string(),
                )
            })
    };
    let fstype = match run("mkfs.ext4", &["-F", "-q", "-m", "0", "-L", &label, &dev]) {
        Ok((true, _)) => "ext4",
        Ok((false, err)) => return Err(format!("mkfs.ext4 {dev}: {}", err.trim())),
        Err(_) => match run("mkfs.xfs", &["-f", "-q", "-L", &label, &dev]) {
            Ok((true, _)) => "xfs",
            Ok((false, err)) => return Err(format!("mkfs.xfs {dev}: {}", err.trim())),
            Err(_) => {
                return Err(
                    "no mkfs.ext4 or mkfs.xfs on this server (apt install e2fsprogs / dnf install xfsprogs)"
                        .into(),
                );
            }
        },
    };
    let mut head = vec![0u8; 4096];
    std::fs::File::open(&dev)
        .and_then(|f| f.read_exact_at(&mut head, 0))
        .map_err(|e| format!("{dev}: {e}"))?;
    let uuid = fs_uuid(&head, fstype).ok_or("no filesystem UUID after mkfs")?;
    let v = Volume {
        id: uuid[..8].to_string(),
        path: crate::home().join("disks").join(&uuid[..8]),
        uuid: Some(uuid),
        fstype: Some(fstype.into()),
    };
    std::fs::create_dir_all(&v.path).map_err(|e| format!("{}: {e}", v.path.display()))?;
    mount(Path::new(&dev), &v.path, fstype)?;
    for sub in ["images", "games", "storage/files"] {
        std::fs::create_dir_all(v.path.join(sub)).map_err(|e| e.to_string())?;
    }
    let mut all = load(st);
    all.push(v.clone());
    save(st, &all)?;
    tracing::warn!(
        "disk {disk} ({} {} GB) ERASED and formatted {fstype}: broom disk {} on {}",
        d.model,
        d.size >> 30,
        v.id,
        v.path.display()
    );
    Ok(v)
}

// ---------- move ----------

/// Bytes a tree takes on disk (allocated blocks; hard links once).
fn used(p: &Path, seen: &mut std::collections::HashSet<(u64, u64)>) -> u64 {
    let Ok(m) = std::fs::metadata(p) else {
        return 0;
    };
    if m.is_dir() {
        std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| used(&e.path(), seen))
            .sum()
    } else if seen.insert((m.dev(), m.ino())) {
        m.blocks() * 512
    } else {
        0
    }
}

/// `src` tree → new `dst`: sparse files keep their holes, files hard-linked to each other (a golden and its versions)
/// stay hard links (`links`, shared across the trees of one move).
pub(crate) fn copy_tree(
    src: &Path,
    dst: &Path,
    links: &mut HashMap<(u64, u64), PathBuf>,
) -> Result<(), String> {
    let m = std::fs::metadata(src).map_err(|e| format!("{}: {e}", src.display()))?;
    if m.is_dir() {
        std::fs::create_dir_all(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        for e in std::fs::read_dir(src).map_err(|e| format!("{}: {e}", src.display()))? {
            let e = e.map_err(|e| e.to_string())?;
            copy_tree(&e.path(), &dst.join(e.file_name()), links)?;
        }
        return Ok(());
    }
    let key = (m.dev(), m.ino());
    if m.nlink() > 1
        && let Some(first) = links.get(&key)
    {
        return std::fs::hard_link(first, dst).map_err(|e| format!("{}: {e}", dst.display()));
    }
    crate::versions::copy_sparse(src, dst)?;
    if m.nlink() > 1 {
        links.insert(key, dst.to_path_buf());
    }
    Ok(())
}

/// Move an image / games disk to volume `to` (None = home). Its users must be stopped by the caller. Blocking.
fn move_blocking(st: &SharedState, kind: &str, name: &str, to: Option<&str>) -> Result<(), String> {
    let dest_root = match to {
        Some(id) => {
            let v = find(st, id)?;
            if !mounted_or_folder(&v) {
                return Err(format!("disk {id} is not mounted"));
            }
            Some(v.path)
        }
        None => None,
    };
    let items: Vec<(PathBuf, PathBuf, PathBuf)> = places(kind, name)
        .into_iter()
        .filter(|(home, _)| std::fs::symlink_metadata(home).is_ok())
        .map(|(home, sub)| {
            let real = std::fs::canonicalize(&home).unwrap_or_else(|_| home.clone());
            let dest = match &dest_root {
                Some(r) => r.join(&sub),
                None => home.with_file_name(format!(".{name}.moving")),
            };
            (home, real, dest)
        })
        .collect();
    if items.is_empty() {
        return Err(format!("{kind} {name} has no files"));
    }
    if volume_of(st, kind, name).as_deref() == to {
        return Err(format!("{kind} {name} is already there"));
    }
    let mut seen = Default::default();
    let need = items
        .iter()
        .map(|(_, real, _)| used(real, &mut seen))
        .sum::<u64>()
        + (1 << 30);
    let target = dest_root
        .clone()
        .unwrap_or_else(|| crate::home().to_path_buf());
    let free = crate::publish::free_bytes(&target);
    if free < need {
        return Err(format!(
            "not enough room: {} GB needed, {} GB free",
            need >> 30,
            free >> 30
        ));
    }
    let mut links = HashMap::new();
    for (_, real, dest) in &items {
        if std::fs::symlink_metadata(dest).is_ok() {
            return Err(format!("{} already exists", dest.display()));
        }
        if let Err(e) = copy_tree(real, dest, &mut links) {
            let _ = std::fs::remove_dir_all(dest);
            return Err(e);
        }
    }
    // Switch each place over (two renames: a path never points at half a copy), then the old copy goes.
    for (home, real, dest) in &items {
        let old = home.with_file_name(format!(".{name}.old"));
        let _ = std::fs::remove_file(&old);
        if dest_root.is_some() {
            let link = home.with_file_name(format!(".{name}.link"));
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(dest, &link).map_err(|e| e.to_string())?;
            std::fs::rename(home, &old).map_err(|e| format!("{}: {e}", home.display()))?;
            std::fs::rename(&link, home).map_err(|e| format!("{}: {e}", home.display()))?;
        } else {
            std::fs::rename(home, &old).map_err(|e| format!("{}: {e}", home.display()))?;
            std::fs::rename(dest, home).map_err(|e| format!("{}: {e}", home.display()))?;
        }
        // The old copy: a real folder at home, or (moved off a volume) the link and the folder it pointed at.
        if std::fs::symlink_metadata(&old).is_ok_and(|m| m.file_type().is_symlink()) {
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::remove_dir_all(real);
        } else {
            let _ = std::fs::remove_dir_all(&old);
        }
    }
    // A place it had no files in yet (no version so far): made on the volume too, so later versions are hard links.
    create_on(st, to, kind, name)
}

// ---------- API ----------

/// GET /api/volumes[?disks=1] — broom's volumes (with what is on them) and the home folder; with disks=1 also the
/// server's disks (Server page: SMART of each, a read of each disk's first sectors).
async fn list(
    State(st): State<SharedState>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let st2 = st.clone();
    let disks = if q.get("disks").is_some_and(|v| v == "1") {
        tokio::task::spawn_blocking(move || disks(&st2))
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let space = |p: &Path| {
        let (used, total) = crate::sysmon::space(p).unwrap_or((0, 0));
        serde_json::json!({"used": used, "total": total})
    };
    let images: Vec<String> = st
        .db
        .images()
        .unwrap_or_default()
        .into_iter()
        .map(|i| i.name)
        .collect();
    let games: Vec<String> = crate::games::disks(&st)
        .into_iter()
        .map(|d| d.name)
        .collect();
    let on = |id: &str, kind: &str, names: &[String]| {
        names
            .iter()
            .filter(|n| volume_of(&st, kind, n).as_deref() == Some(id))
            .cloned()
            .collect::<Vec<_>>()
    };
    let vols: Vec<_> = load(&st)
        .into_iter()
        .map(|v| {
            let up = mounted_or_folder(&v);
            serde_json::json!({
                "id": v.id, "path": v.path, "uuid": v.uuid, "fstype": v.fstype, "mounted": up,
                "space": if up { space(&v.path) } else { serde_json::Value::Null },
                "images": on(&v.id, "image", &images), "games": on(&v.id, "games", &games),
            })
        })
        .collect();
    Json(serde_json::json!({
        "disks": disks, "volumes": vols,
        "home": {"path": crate::home(), "space": space(crate::home())},
    }))
}

#[derive(Deserialize)]
struct FormatReq {
    disk: String,
    /// The disk's name typed again.
    confirm: String,
}

/// POST /api/volumes/format {disk, confirm} — ERASE a free disk and make it a broom disk.
async fn format_disk(
    State(st): State<SharedState>,
    Json(b): Json<FormatReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if b.confirm.trim() != b.disk {
        return Err(bad("type the disk's name to confirm"));
    }
    let v = tokio::task::spawn_blocking(move || format_blocking(&st, &b.disk))
        .await
        .map_err(ise)?
        .map_err(bad)?;
    Ok(Json(serde_json::json!({"ok": true, "id": v.id})))
}

#[derive(Deserialize)]
struct FolderReq {
    path: String,
}

/// POST /api/volumes/folder {path} — a folder the admin mounted (LVM, RAID, NFS…) used as a broom disk.
async fn add_folder(
    State(st): State<SharedState>,
    Json(b): Json<FolderReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let p = Path::new(b.path.trim());
    if !p.is_absolute() || !p.is_dir() {
        return Err(bad("an existing folder, full path (e.g. /mnt/data/broom)"));
    }
    let p = std::fs::canonicalize(p).map_err(bad)?;
    let home = std::fs::canonicalize(crate::home()).map_err(ise)?;
    if p.starts_with(&home) || home.starts_with(&p) {
        return Err(bad("a folder outside broom's own folder"));
    }
    let mut all = load(&st);
    if all
        .iter()
        .any(|v| p.starts_with(&v.path) || v.path.starts_with(&p))
    {
        return Err(bad("that folder is (inside) a broom disk already"));
    }
    for sub in ["images", "games", "storage/files"] {
        std::fs::create_dir_all(p.join(sub)).map_err(bad)?;
    }
    let id = format!(
        "dir-{}",
        &blake3::hash(p.as_os_str().as_encoded_bytes()).to_hex()[..6]
    );
    tracing::info!("folder {} added as broom disk {id}", p.display());
    all.push(Volume {
        id,
        path: p,
        uuid: None,
        fstype: None,
    });
    save(&st, &all).map_err(ise)?;
    Ok(ok())
}

#[derive(Deserialize)]
struct IdReq {
    id: String,
}

/// POST /api/volumes/remove {id} — no longer a broom disk (only when nothing is on it): unmounted if broom mounted
/// it. Its files stay on it.
async fn remove(
    State(st): State<SharedState>,
    Json(b): Json<IdReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let v = find(&st, &b.id).map_err(bad)?;
    let busy: Vec<String> = st
        .db
        .images()
        .map_err(ise)?
        .into_iter()
        .filter(|i| volume_of(&st, "image", &i.name).as_deref() == Some(v.id.as_str()))
        .map(|i| format!("image {}", i.name))
        .chain(
            crate::games::disks(&st)
                .into_iter()
                .filter(|d| volume_of(&st, "games", &d.name).as_deref() == Some(v.id.as_str()))
                .map(|d| format!("iSCSI disk {}", d.name)),
        )
        .collect();
    if !busy.is_empty() {
        return Err(bad(format!(
            "still on it: {} — move them first",
            busy.join(", ")
        )));
    }
    if v.uuid.is_some() && is_mountpoint(&v.path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(v.path.as_os_str().as_bytes()).map_err(ise)?;
        // SAFETY: valid NUL-terminated path.
        if unsafe { libc::umount2(c.as_ptr(), 0) } != 0 {
            return Err(bad(format!(
                "unmount {}: {}",
                v.path.display(),
                std::io::Error::last_os_error()
            )));
        }
        let _ = std::fs::remove_dir(&v.path);
    }
    save(
        &st,
        &load(&st)
            .into_iter()
            .filter(|x| x.id != v.id)
            .collect::<Vec<_>>(),
    )
    .map_err(ise)?;
    tracing::info!("broom disk {} removed ({})", v.id, v.path.display());
    Ok(ok())
}

#[derive(Deserialize)]
struct MoveReq {
    /// "image" | "games".
    kind: String,
    name: String,
    /// Volume id; "" / "home" = broom's own folder.
    to: String,
}

/// POST /api/volumes/move {kind, name, to} — a job (status as job `move:<kind>:<name>`). Refused while clients use
/// it: a Linux image someone booted, a games disk with sessions or an update not saved.
async fn move_item(
    State(st): State<SharedState>,
    Json(b): Json<MoveReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let to = Some(b.to.clone()).filter(|t| !t.is_empty() && t != "home");
    let (kind, name) = (b.kind.clone(), b.name.clone());
    // The image's own job slot (no publish / rollback / delete meanwhile); a games disk gets one of its own.
    let job = if kind == "image" {
        st.db
            .image_by_name(&name)
            .map_err(ise)?
            .ok_or_else(|| bad(format!("no image {name}")))?;
        name.clone()
    } else if kind == "games" {
        if !crate::games::disks(&st).iter().any(|d| d.name == name) {
            return Err(bad(format!("no iSCSI disk {name}")));
        }
        format!("games:{name}")
    } else {
        return Err(bad("kind: image | games"));
    };
    {
        let mut jobs = st.jobs.lock();
        if jobs.get(&job).is_some_and(|s| s.starts_with('⏳')) {
            return Err(bad(format!("{name} has a running job — wait for it")));
        }
        jobs.insert(job.clone(), "⏳ moving...".into());
    }
    let _ = st.job_tx.send((job.clone(), "⏳ moving...".into()));
    let st2 = st.clone();
    tokio::task::spawn_blocking(move || {
        let r = (|| -> Result<(), String> {
            if kind == "image" {
                if crate::publish::image_in_use(&st2, &name) {
                    return Err("clients are booted from it — shut them down first".into());
                }
                // Its iSCSI targets point at the old files: dropped, made again from the new place after.
                crate::publish::drop_all_targets(&st2, &name);
                let r = {
                    let _g = st2.versions_lock.lock();
                    move_blocking(&st2, "image", &name, to.as_deref())
                };
                crate::publish::restore_targets(&st2);
                r
            } else {
                crate::games::idle(&name)?;
                // Out of the iSCSI daemon while its files move (it holds them open), back in after.
                crate::games::sync_except(&st2, Some(&name))?;
                let r = move_blocking(&st2, "games", &name, to.as_deref());
                let _ = crate::games::sync(&st2);
                r
            }
        })();
        let status = match &r {
            Ok(()) => format!("✓ moved to {}", to.as_deref().unwrap_or("home")),
            Err(e) => format!("✗ move: {e}"),
        };
        match &r {
            Ok(()) => tracing::info!("{kind} {name} moved to {}", to.as_deref().unwrap_or("home")),
            Err(e) => tracing::warn!("{kind} {name} not moved: {e}"),
        }
        st2.jobs.lock().insert(job.clone(), status.clone());
        let _ = st2.job_tx.send((job, status));
    });
    Ok(ok())
}

#[cfg(test)]
#[path = "../tests/unit/volumes.rs"]
mod tests;
