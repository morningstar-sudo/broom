// sysmon.rs — the server's own resources. Limits sized from the hardware (iSCSI buffers + connections, TFTP transfers)
// instead of fixed numbers, and a sampler every 2 s (CPU, RAM, the boot NIC, the image disk) whose numbers go to every
// open web page over SSE ("res" on /api/events): a banner on every page while one stays above the threshold
// (Server page, default 80%), the live numbers on the Server page.
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use crate::SharedState;

/// Total RAM in bytes (/proc/meminfo). 0 if unreadable.
pub fn mem_total() -> u64 {
    meminfo("MemTotal:")
}

fn meminfo(key: &str) -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|r| r.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

/// `pct`% of RAM in units of `each` bytes, never under `min` (also what an unreadable /proc/meminfo gives).
pub fn ram_share(pct: u64, each: u64, min: usize) -> usize {
    ((mem_total() / 100 * pct / each) as usize).max(min)
}

fn rlimit_nofile() -> Option<libc::rlimit> {
    let mut r = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit only writes the struct it is given.
    (unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } == 0).then_some(r)
}

/// Files (sockets included) this process may have open.
pub fn fd_limit() -> u64 {
    rlimit_nofile().map_or(1024, |r| r.rlim_cur)
}

/// Soft open-file limit up to the hard one (systemd starts services at 1024, a default meant for desktop programs):
/// the connection limits below then follow the machine. Called first thing, by mgmt and by the iSCSI daemon.
pub fn raise_fd_limit() {
    let Some(mut r) = rlimit_nofile() else { return };
    let want = r.rlim_max.min(1 << 20); // hard limit "unlimited" → the kernel's nr_open default
    if r.rlim_cur < want {
        r.rlim_cur = want;
        // SAFETY: setrlimit reads the struct; a refusal leaves the old limit.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &r) };
    }
}

// ---------- services ----------

static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

pub fn routes() -> axum::Router<SharedState> {
    axum::Router::new().route("/api/server/status", axum::routing::get(services))
}

/// GET /api/server/status — is each service up (Server page, Services card).
async fn services(
    axum::extract::State(st): axum::extract::State<SharedState>,
) -> axum::Json<serde_json::Value> {
    let full = st.db.get_config("dhcp_mode", "off") == "full";
    // net[0] = TFTP, net[1] = DHCP (only in full mode, and only if it could bind): dhcp::start().
    let (tftp, dhcp) = {
        let net = st.net.lock();
        let up = |i: usize| net.get(i).is_some_and(|h| !h.is_finished());
        (up(0), full && up(1))
    };
    let iscsi = tokio::task::spawn_blocking(crate::iscsi::stats)
        .await
        .ok()
        .flatten();
    let (golden_running, golden_waiting) = crate::goldenram::queue_stats();
    let host_up = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok());
    axum::Json(serde_json::json!({
        "dhcp": {"mode": if full { "full" } else { "off" }, "running": dhcp,
                 "iface": st.db.get_config("dhcp_iface", "")},
        "tftp": {"running": tftp},
        "iscsi": iscsi.map(|s| serde_json::json!({
            "targets": s.targets.len(),
            "sessions": s.targets.iter().map(|t| t.sessions).sum::<usize>(),
            "clients": s.peers.map(|p| p.iter().filter(|p| p.connected).count()),
        })),
        "agents": crate::vnc::count(),
        "dns": {"on": crate::dns::on(&st), "running": crate::dns::active(), "allowed": crate::dns::allowed_count(),
                "recursive": crate::dns::recursive(&st), "cached": crate::dnsrec::cached(),
                "upstream": crate::dns::upstream(&st)},
        "golden": {"running": golden_running, "waiting": golden_waiting},
        "uptime": STARTED.elapsed().as_secs(), "host_uptime": host_up.map(|u| u as u64),
    }))
}

// ---------- sampler ----------

/// Every open page gets the samples ("res" events); a page that just opened gets the last one at once.
pub static RES: LazyLock<tokio::sync::broadcast::Sender<String>> =
    LazyLock::new(|| tokio::sync::broadcast::channel(4).0);
pub static LATEST: LazyLock<Mutex<String>> = LazyLock::new(Default::default);

/// What the server sends each client now, bytes/s by IP: (iSCSI — games disks, Linux goldens; HTTP — golden downloads).
/// Measured from byte counters between two samples: no rate before the second one, none guessed.
static TRAFFIC: LazyLock<Mutex<HashMap<String, (f64, f64)>>> = LazyLock::new(Default::default);

/// The traffic to `ip` now (Machines page); None = nothing measured for it.
pub fn traffic(ip: &str) -> Option<(f64, f64)> {
    TRAFFIC.lock().get(ip).copied()
}

/// Rate per key between two counter samples `secs` apart. A key without a previous sample, or whose counter went back
/// (the iSCSI daemon restarted), gets none.
fn rates(
    prev: &HashMap<String, u64>,
    now: &HashMap<String, u64>,
    secs: f64,
) -> HashMap<String, f64> {
    now.iter()
        .filter_map(|(k, &n)| {
            let p = *prev.get(k)?;
            (n >= p && secs > 0.0).then(|| (k.clone(), (n - p) as f64 / secs))
        })
        .collect()
}

const EVERY: Duration = Duration::from_secs(2);
/// Samples in a row above the threshold before the banner shows (6 s): a short burst is not an alert.
const STREAK: u32 = 3;

/// cpu line of /proc/stat → (total, idle + iowait) jiffies.
fn cpu_times() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let v: Vec<u64> = s
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .skip(1)
        .take(8)
        .filter_map(|x| x.parse().ok())
        .collect();
    (
        v.iter().sum(),
        v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0),
    )
}

/// The NIC clients boot from: Network page's interface, else the one with the default route.
fn boot_iface(configured: &str) -> Option<String> {
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    let r = std::fs::read_to_string("/proc/net/route").ok()?;
    r.lines()
        .skip(1)
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|f| f.get(1) == Some(&"00000000"))
        .map(|f| f[0].to_string())
}

/// (received, sent) bytes of `iface` (/proc/net/dev).
fn net_bytes(iface: &str) -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/net/dev").ok()?;
    let rest = s
        .lines()
        .find_map(|l| l.trim_start().strip_prefix(iface)?.strip_prefix(':'))?;
    let f: Vec<u64> = rest
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    Some((*f.first()?, *f.get(8)?))
}

/// Link speed in Mbit/s; None when the NIC doesn't say.
fn link_mbit(iface: &str) -> Option<u64> {
    std::fs::read_to_string(format!("/sys/class/net/{iface}/speed"))
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|s| *s > 0)
        .map(|s| s as u64)
}

/// The boot NIC and its real link speed in Mbit/s (None: the NIC doesn't say, or a virtual NIC makes it up). A bond
/// reports the sum of its ports.
pub(crate) fn boot_link(configured: &str) -> Option<(String, Option<u64>)> {
    let iface = boot_iface(configured)?;
    let speed = if virtual_nic(&iface).is_some() {
        None
    } else {
        link_mbit(&iface)
    };
    Some((iface, speed))
}

/// The hypervisor this server runs under (DMI), None on bare metal.
fn hypervisor() -> Option<&'static str> {
    static H: LazyLock<Option<&'static str>> = LazyLock::new(|| {
        let r =
            |f: &str| std::fs::read_to_string(format!("/sys/class/dmi/id/{f}")).unwrap_or_default();
        let (vendor, product) = (r("sys_vendor"), r("product_name"));
        let s = format!("{vendor} {product}");
        [
            ("VMware", "VMware"),
            ("QEMU", "QEMU/KVM"),
            ("KVM", "QEMU/KVM"),
            ("VirtualBox", "VirtualBox"),
            ("innotek", "VirtualBox"),
            ("Xen", "Xen"),
        ]
        .into_iter()
        .find(|(k, _)| s.contains(k))
        .map(|(_, h)| h)
        .or_else(|| {
            (vendor.contains("Microsoft") && product.contains("Virtual Machine"))
                .then_some("Hyper-V")
        })
    });
    *H
}

/// A virtual NIC → "VMware vmxnet3": its link speed is made up (vmxnet3 always says 10 Gbit), the host's real port is
/// invisible from inside. Paravirtual drivers always are; emulated real chips (e1000…) are inside a VM. A real card
/// passed through to the VM keeps its own driver and its true speed.
fn virtual_nic(iface: &str) -> Option<String> {
    let drv = std::fs::read_link(format!("/sys/class/net/{iface}/device/driver"))
        .ok()?
        .file_name()?
        .to_string_lossy()
        .into_owned();
    let paravirtual =
        ["vmxnet3", "virtio_net", "hv_netvsc", "vif", "xen_netfront"].contains(&drv.as_str());
    let emulated = [
        "e1000", "e1000e", "8139cp", "8139too", "pcnet32", "ne2k-pci",
    ]
    .contains(&drv.as_str());
    // No DMI (WSL, some clouds) → the paravirtual driver names its hypervisor.
    let by_driver = match drv.as_str() {
        "vmxnet3" => "VMware",
        "virtio_net" => "QEMU/KVM",
        "hv_netvsc" => "Hyper-V",
        _ => "Xen",
    };
    (paravirtual || (emulated && hypervisor().is_some()))
        .then(|| format!("{} {drv}", hypervisor().unwrap_or(by_driver)))
}

/// Counters of the block device holding a path, so far.
struct DiskStat {
    name: String,
    /// ms spent doing I/O (busy time).
    busy_ms: u64,
    read: u64,
    written: u64,
}

/// The block device holding `path`. None on a filesystem without one (zfs, tmpfs).
fn disk_stat(path: &Path) -> Option<DiskStat> {
    use std::os::unix::fs::MetadataExt;
    let d = std::fs::metadata(path).ok()?.dev();
    let sys = format!("/sys/dev/block/{}:{}", libc::major(d), libc::minor(d));
    let name = std::fs::read_link(&sys)
        .ok()?
        .file_name()?
        .to_string_lossy()
        .into_owned();
    // stat: reads, merges, sectors read, ms, writes, merges, sectors written, ms, in flight, io_ticks (ms busy), …
    // Sectors are 512 bytes whatever the disk's own sector size.
    let f: Vec<u64> = std::fs::read_to_string(format!("{sys}/stat"))
        .ok()?
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    Some(DiskStat {
        name,
        busy_ms: *f.get(9)?,
        read: f.get(2)? * 512,
        written: f.get(6)? * 512,
    })
}

/// SMART health of the server's image disk, read with a kernel ioctl (no smartctl / external tool): the NVMe
/// SMART/health log page, or ATA SMART over SG_IO for SATA. best-effort — None on a RAID / virtual disk / anything it
/// can't reach. The server runs as root, which the ioctls need.
#[derive(Clone, serde::Serialize)]
pub(crate) struct DiskHealth {
    /// "OK" / "WARNING" / "FAILING".
    status: String,
    temp: Option<i32>,
    realloc: Option<u64>,
    pending: Option<u64>,
}

/// Partition name -> its whole disk (nvme0n1p2 -> nvme0n1, sda2 -> sda).
fn whole_disk(part: &str) -> String {
    if let Some(i) = part.find('p').filter(|_| part.starts_with("nvme")) {
        // nvme0n1p2: the 'p' before the partition number.
        if part[i + 1..].chars().all(|c| c.is_ascii_digit()) {
            return part[..i].to_string();
        }
    }
    part.trim_end_matches(|c: char| c.is_ascii_digit())
        .to_string()
}

type HealthCache = parking_lot::Mutex<HashMap<String, (Instant, Option<DiskHealth>)>>;

/// SMART of a disk (the image disk; every disk on the Server page's Disks card), cached per disk: read at most once
/// a minute — a SMART command is slow and never changes fast.
pub(crate) fn disk_health(part: &str) -> Option<DiskHealth> {
    static CACHE: LazyLock<HealthCache> = LazyLock::new(Default::default);
    let disk = whole_disk(part);
    if let Some((at, h)) = CACHE.lock().get(&disk)
        && at.elapsed() < std::time::Duration::from_secs(60)
    {
        return h.clone();
    }
    let health = if disk.starts_with("nvme") {
        nvme_health(&disk)
    } else {
        sata_health(&disk)
    };
    CACHE.lock().insert(disk, (Instant::now(), health.clone()));
    health
}

/// Open /dev/<disk> read-only for an ioctl.
fn open_disk(disk: &str) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .open(format!("/dev/{disk}"))
        .ok()
}

/// NVMe SMART/health log page (log id 0x02) via NVME_IOCTL_ADMIN_CMD on the controller (/dev/nvmeN).
fn nvme_health(disk: &str) -> Option<DiskHealth> {
    use std::os::fd::AsRawFd;
    // nvme0n1 -> nvme0 (the controller char device the admin ioctl wants): "nvme" + the controller digits before "n".
    let num: String = disk
        .strip_prefix("nvme")?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let f = open_disk(&format!("nvme{num}"))?;

    #[repr(C)]
    #[derive(Default)]
    struct AdminCmd {
        opcode: u8,
        flags: u8,
        rsvd1: u16,
        nsid: u32,
        cdw2: u32,
        cdw3: u32,
        metadata: u64,
        addr: u64,
        metadata_len: u32,
        data_len: u32,
        cdw10: u32,
        cdw11: u32,
        cdw12: u32,
        cdw13: u32,
        cdw14: u32,
        cdw15: u32,
        timeout_ms: u32,
        result: u32,
    }
    const NVME_ADMIN: u32 = 0xC048_4E41; // _IOWR('N', 0x41, nvme_admin_cmd) — 72-byte struct
    let mut buf = [0u8; 512];
    let mut cmd = AdminCmd {
        opcode: 0x02,      // Get Log Page
        nsid: 0xffff_ffff, // whole controller
        addr: buf.as_mut_ptr() as u64,
        data_len: 512,
        cdw10: (127 << 16) | 0x02, // (dwords-1) << 16 | log id 0x02 (SMART/Health)
        ..Default::default()
    };
    // SAFETY: cmd.addr points at buf (512 bytes, data_len); f is a valid fd. The ioctl fills buf.
    if unsafe { libc::ioctl(f.as_raw_fd(), NVME_ADMIN as libc::Ioctl, &mut cmd) } != 0 {
        return None;
    }
    let crit = buf[0];
    let kelvin = u16::from_le_bytes([buf[1], buf[2]]) as i32;
    let media_errors = u64::from_le_bytes(buf[160..168].try_into().ok()?);
    Some(DiskHealth {
        status: if crit == 0 { "OK" } else { "WARNING" }.to_string(),
        temp: (kelvin > 0).then_some(kelvin - 273),
        realloc: None,
        pending: (media_errors > 0).then_some(media_errors),
    })
}

/// ATA SMART READ DATA (0xB0 / feature 0xD0) over SG_IO ATA PASS-THROUGH(16) for a SATA/SAS disk; parse attributes
/// 5 (reallocated), 197 (pending), 194 (temperature).
fn sata_health(disk: &str) -> Option<DiskHealth> {
    use std::os::fd::AsRawFd;
    let f = open_disk(disk)?;

    #[repr(C)]
    struct SgIoHdr {
        interface_id: i32,
        dxfer_direction: i32,
        cmd_len: u8,
        mx_sb_len: u8,
        iovec_count: u16,
        dxfer_len: u32,
        dxferp: u64,
        cmdp: u64,
        sbp: u64,
        timeout: u32,
        flags: u32,
        pack_id: i32,
        usr_ptr: u64,
        status: u8,
        masked_status: u8,
        msg_status: u8,
        sb_len_wr: u8,
        host_status: u16,
        driver_status: u16,
        resid: i32,
        duration: u32,
        info: u32,
    }
    const SG_IO: u32 = 0x2285;
    let mut data = [0u8; 512];
    let mut sense = [0u8; 32];
    // ATA PASS-THROUGH(16): PIO data-in, 1 sector from the device; READ SMART DATA at LBA mid/high 0x4F/0xC2.
    let cdb: [u8; 16] = [
        0x85, 0x08, 0x0e, 0x00, 0xd0, 0x00, 0x01, 0x00, 0x00, 0x00, 0x4f, 0x00, 0xc2, 0x00, 0xb0,
        0x00,
    ];
    let mut h: SgIoHdr = unsafe { std::mem::zeroed() };
    h.interface_id = b'S' as i32;
    h.dxfer_direction = -3; // SG_DXFER_FROM_DEV
    h.cmd_len = 16;
    h.mx_sb_len = sense.len() as u8;
    h.dxfer_len = 512;
    h.dxferp = data.as_mut_ptr() as u64;
    h.cmdp = cdb.as_ptr() as u64;
    h.sbp = sense.as_mut_ptr() as u64;
    h.timeout = 5000;
    // SAFETY: the pointers in h stay valid for the call; f is a valid fd. The ioctl fills `data`.
    if unsafe { libc::ioctl(f.as_raw_fd(), SG_IO as libc::Ioctl, &mut h) } != 0
        || h.host_status != 0
    {
        return None;
    }
    let (realloc, pending, temp) = parse_ata_smart(&data);
    // No overall-health command here: derive it from the bad-sector counts (a surface scan is not done).
    let bad = realloc.unwrap_or(0) > 0 || pending.unwrap_or(0) > 0;
    Some(DiskHealth {
        status: if bad { "WARNING" } else { "OK" }.to_string(),
        temp,
        realloc,
        pending,
    })
}

/// A 512-byte ATA SMART data block → (reallocated, pending, temperature °C). Attribute table from offset 2, each
/// entry 12 bytes (id, flags[2], value, worst, raw[6]); raw is little-endian.
fn parse_ata_smart(data: &[u8; 512]) -> (Option<u64>, Option<u64>, Option<i32>) {
    let raw32 = |i: usize| -> u64 {
        u32::from_le_bytes([data[i + 5], data[i + 6], data[i + 7], data[i + 8]]) as u64
    };
    let (mut realloc, mut pending, mut temp) = (None, None, None);
    let mut i = 2;
    while i + 12 <= 362 {
        match data[i] {
            5 => realloc = Some(raw32(i)),
            197 => pending = Some(raw32(i)),
            194 => temp = Some(data[i + 5] as i32),
            _ => {}
        }
        i += 12;
    }
    (realloc, pending, temp)
}

/// (used, total) bytes of the filesystem holding `path`, counted like `df` (used + available = what can be used).
pub(crate) fn space(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: c is a valid NUL-terminated path; s is written by statvfs before it is read.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 || s.f_blocks == 0 {
        return None;
    }
    let used = (s.f_blocks - s.f_bfree) * s.f_frsize;
    Some((used, used + s.f_bavail * s.f_frsize))
}

/// The resources at or above `limit` % for STREAK samples in a row ("CPU 92%"), in the order given. `streak` carries
/// each one's count from sample to sample; None (not measurable) never alerts.
fn alerts(
    vals: &[(&'static str, Option<f64>)],
    limit: f64,
    streak: &mut HashMap<&'static str, u32>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (name, v) in vals {
        let n = streak.entry(name).or_default();
        *n = if v.is_some_and(|v| v >= limit) {
            *n + 1
        } else {
            0
        };
        if *n >= STREAK {
            out.push(format!("{name} {:.0}%", v.unwrap_or(0.0)));
        }
    }
    out
}

/// Sample forever (mgmt's lifetime): push each to the open pages, log when an alert starts / ends.
pub async fn run(st: SharedState) {
    LazyLock::force(&STARTED); // mgmt's uptime counts from here
    let (mut cpu0, mut net0, mut io0, mut t0) = (
        cpu_times(),
        None::<(u64, u64)>,
        None::<DiskStat>,
        Instant::now(),
    );
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut streak = HashMap::new();
    let mut shown: Vec<String> = Vec::new();
    let (mut iscsi0, mut http0) = (HashMap::new(), HashMap::new());
    loop {
        tokio::time::sleep(EVERY).await;
        let iface = boot_iface(&st.db.get_config("dhcp_iface", ""));
        let limit = st
            .db
            .get_config("alert_pct", "80")
            .parse::<f64>()
            .unwrap_or(80.0);
        let images = crate::images_dir();
        let secs = t0.elapsed().as_secs_f64().max(1e-3);
        t0 = Instant::now();

        let cpu1 = cpu_times();
        let total = cpu1.0.saturating_sub(cpu0.0);
        let cpu = (total > 0).then(|| {
            100.0 * (total - cpu1.1.saturating_sub(cpu0.1).min(total)) as f64 / total as f64
        });
        cpu0 = cpu1;

        // 1-minute load average: runnable tasks, next to the core count.
        let load = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok());

        // Used = what programs hold (total − available, like `free`'s "used"); page cache counted apart: the kernel
        // gives it back whenever programs need it.
        let (mt, ma) = (mem_total(), meminfo("MemAvailable:"));
        let ram_used = mt.saturating_sub(ma);
        let ram = (mt > 0).then(|| 100.0 * ram_used as f64 / mt as f64);
        let cache = meminfo("Cached:") + meminfo("Buffers:");

        let net1 = iface.as_deref().and_then(net_bytes);
        let (rx, tx) = match (net0, net1) {
            (Some(a), Some(b)) => (
                b.0.saturating_sub(a.0) as f64 / secs,
                b.1.saturating_sub(a.1) as f64 / secs,
            ),
            _ => (0.0, 0.0),
        };
        net0 = net1;
        // A virtual NIC's speed is made up → no % (and no alert) rather than a % of a number that isn't the limit.
        let virt = iface.as_deref().and_then(virtual_nic);
        let link = iface
            .as_deref()
            .and_then(link_mbit)
            .filter(|_| virt.is_none());
        let net_pct = link.map(|l| 100.0 * rx.max(tx) * 8.0 / (l as f64 * 1e6));

        // Per client: iSCSI bytes read (the daemon's counters, a socket call → off the async threads) + golden bytes
        // sent over HTTP.
        let iscsi1: HashMap<String, u64> = tokio::task::spawn_blocking(crate::iscsi::stats)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.peers)
            .unwrap_or_default()
            .into_iter()
            .map(|p| (p.ip, p.read_bytes))
            .collect();
        let http1: HashMap<String, u64> = crate::goldenram::sent()
            .into_iter()
            .map(|(ip, n)| (ip.to_string(), n))
            .collect();
        let (ri, rh) = (rates(&iscsi0, &iscsi1, secs), rates(&http0, &http1, secs));
        (iscsi0, http0) = (iscsi1, http1);
        let egress = (ri.values().sum::<f64>(), rh.values().sum::<f64>());
        *TRAFFIC.lock() = ri
            .keys()
            .chain(rh.keys())
            .map(|ip| {
                let r = (*ri.get(ip).unwrap_or(&0.0), *rh.get(ip).unwrap_or(&0.0));
                (ip.clone(), r)
            })
            .collect();

        let disk = disk_stat(&images);
        let (busy, rd, wr) = match (&io0, &disk) {
            (Some(a), Some(b)) => (
                Some(
                    (100.0 * b.busy_ms.saturating_sub(a.busy_ms) as f64 / (secs * 1000.0))
                        .min(100.0),
                ),
                b.read.saturating_sub(a.read) as f64 / secs,
                b.written.saturating_sub(a.written) as f64 / secs,
            ),
            _ => (None, 0.0, 0.0),
        };
        let dev = disk.as_ref().map(|d| d.name.clone());
        let health = dev.as_deref().and_then(disk_health);
        io0 = disk;
        let sp = space(&images);
        let pct = |(u, t): (u64, u64)| 100.0 * u as f64 / t.max(1) as f64;
        // The alert: the fullest of the image disk and the other broom disks (volumes.rs).
        let used = sp
            .map(pct)
            .into_iter()
            .chain(
                crate::volumes::mounted(&st)
                    .into_iter()
                    .filter_map(|p| space(&p).map(pct)),
            )
            .reduce(f64::max);

        let now = alerts(
            &[
                ("CPU", cpu),
                ("RAM", ram),
                ("Network", net_pct),
                ("Disk busy", busy),
                ("Disk space", used),
            ],
            limit,
            &mut streak,
        );
        let names = |v: &[String]| {
            v.iter()
                .map(|a| a.split(' ').next().unwrap_or("").to_string())
                .collect::<Vec<_>>()
        };
        if names(&now) != names(&shown) {
            if now.is_empty() {
                tracing::info!("server resources back under {limit:.0}%");
            } else {
                tracing::warn!(
                    "server resources at their limit: {} (threshold {limit:.0}%) — clients boot / load slower",
                    now.join(", ")
                );
            }
        }
        shown = now;
        // Bytes as measured; a % only where the whole is known (none for a virtual NIC's link).
        let data = serde_json::json!({
            "cpu": {"pct": cpu, "cores": cores, "load": load},
            "ram": {"pct": ram, "used": ram_used, "total": mt, "cache": cache},
            "net": {"iface": iface, "rx": rx, "tx": tx, "link": link, "pct": net_pct, "virtual": virt},
            "egress": {"iscsi": egress.0, "http": egress.1},
            "disk": {"dev": dev, "busy": busy, "read": rd, "write": wr, "health": health},
            "space": {"pct": sp.map(pct), "used": sp.map(|s| s.0), "total": sp.map(|s| s.1)},
            "limit": limit, "alerts": shown,
        })
        .to_string();
        *LATEST.lock() = data.clone();
        let _ = RES.send(data); // Err = no page open
    }
}

#[cfg(test)]
#[path = "../tests/unit/sysmon.rs"]
mod tests;
