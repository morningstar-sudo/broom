// sysmon.rs — the server's own resources. Limits sized from the hardware (iSCSI buffers + connections, TFTP transfers)
// instead of fixed numbers, and a sampler every 2 s (CPU, RAM, the boot NIC, the image disk) whose numbers go to every
// open web page over SSE ("res" on /api/events): a banner on every page while one stays above the threshold
// (Settings, default 80%), the live numbers on the Benchmark page.
use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
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
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
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

// ---------- sampler ----------

/// Every open page gets the samples ("res" events); a page that just opened gets the last one at once.
pub static RES: LazyLock<tokio::sync::broadcast::Sender<String>> = LazyLock::new(|| tokio::sync::broadcast::channel(4).0);
pub static LATEST: LazyLock<Mutex<String>> = LazyLock::new(Default::default);

const EVERY: Duration = Duration::from_secs(2);
/// Samples in a row above the threshold before the banner shows (6 s): a short burst is not an alert.
const STREAK: u32 = 3;

/// cpu line of /proc/stat → (total, idle + iowait) jiffies.
fn cpu_times() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let v: Vec<u64> = s.lines().next().unwrap_or("").split_whitespace().skip(1).take(8).filter_map(|x| x.parse().ok()).collect();
    (v.iter().sum(), v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0))
}

/// The NIC clients boot from: Network page's interface, else the one with the default route.
fn boot_iface(configured: &str) -> Option<String> {
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    let r = std::fs::read_to_string("/proc/net/route").ok()?;
    r.lines().skip(1).map(|l| l.split_whitespace().collect::<Vec<_>>()).find(|f| f.get(1) == Some(&"00000000")).map(|f| f[0].to_string())
}

/// (received, sent) bytes of `iface` (/proc/net/dev).
fn net_bytes(iface: &str) -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/net/dev").ok()?;
    let rest = s.lines().find_map(|l| l.trim_start().strip_prefix(iface)?.strip_prefix(':'))?;
    let f: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    Some((*f.first()?, *f.get(8)?))
}

/// Link speed in Mbit/s; None for a virtual NIC that doesn't say (then only MB/s is shown, no %).
fn link_mbit(iface: &str) -> Option<u64> {
    std::fs::read_to_string(format!("/sys/class/net/{iface}/speed")).ok()?.trim().parse::<i64>().ok().filter(|s| *s > 0).map(|s| s as u64)
}

/// The block device holding `path` (name, ms spent doing I/O so far). None on a filesystem without one (zfs, tmpfs).
fn disk_ticks(path: &Path) -> Option<(String, u64)> {
    use std::os::unix::fs::MetadataExt;
    let d = std::fs::metadata(path).ok()?.dev();
    let sys = format!("/sys/dev/block/{}:{}", libc::major(d), libc::minor(d));
    let name = std::fs::read_link(&sys).ok()?.file_name()?.to_string_lossy().into_owned();
    // stat: reads, merges, sectors, ms, writes, merges, sectors, ms, in flight, io_ticks (ms busy), …
    let io = std::fs::read_to_string(format!("{sys}/stat")).ok()?.split_whitespace().nth(9)?.parse().ok()?;
    Some((name, io))
}

/// % of the filesystem holding `path` in use.
fn space_used(path: &Path) -> Option<f64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: c is a valid NUL-terminated path; s is written by statvfs before it is read.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 || s.f_blocks == 0 {
        return None;
    }
    Some(100.0 * (s.f_blocks - s.f_bavail) as f64 / s.f_blocks as f64)
}

/// The resources at or above `limit` % for STREAK samples in a row ("CPU 92%"), in the order given. `streak` carries
/// each one's count from sample to sample; None (not measurable) never alerts.
fn alerts(vals: &[(&'static str, Option<f64>)], limit: f64, streak: &mut HashMap<&'static str, u32>) -> Vec<String> {
    let mut out = Vec::new();
    for (name, v) in vals {
        let n = streak.entry(name).or_default();
        *n = if v.is_some_and(|v| v >= limit) { *n + 1 } else { 0 };
        if *n >= STREAK {
            out.push(format!("{name} {:.0}%", v.unwrap_or(0.0)));
        }
    }
    out
}

/// Sample forever (mgmt's lifetime): push each to the open pages, log when an alert starts / ends.
pub async fn run(st: SharedState) {
    let (mut cpu0, mut net0, mut io0, mut t0) = (cpu_times(), None::<(u64, u64)>, None::<u64>, Instant::now());
    let mut streak = HashMap::new();
    let mut shown: Vec<String> = Vec::new();
    loop {
        tokio::time::sleep(EVERY).await;
        let iface = boot_iface(&st.db.get_config("dhcp_iface", ""));
        let limit = st.db.get_config("alert_pct", "80").parse::<f64>().unwrap_or(80.0);
        let images = crate::images_dir();
        let secs = t0.elapsed().as_secs_f64().max(1e-3);
        t0 = Instant::now();

        let cpu1 = cpu_times();
        let total = cpu1.0.saturating_sub(cpu0.0);
        let cpu = (total > 0).then(|| 100.0 * (total - cpu1.1.saturating_sub(cpu0.1).min(total)) as f64 / total as f64);
        cpu0 = cpu1;

        let (mt, ma) = (mem_total(), meminfo("MemAvailable:"));
        let ram = (mt > 0).then(|| 100.0 * mt.saturating_sub(ma) as f64 / mt as f64);

        let net1 = iface.as_deref().and_then(net_bytes);
        let (rx, tx) = match (net0, net1) {
            (Some(a), Some(b)) => (b.0.saturating_sub(a.0) as f64 / secs, b.1.saturating_sub(a.1) as f64 / secs),
            _ => (0.0, 0.0),
        };
        net0 = net1;
        let link = iface.as_deref().and_then(link_mbit);
        let net_pct = link.map(|l| 100.0 * rx.max(tx) * 8.0 / (l as f64 * 1e6));

        let disk = disk_ticks(&images);
        let busy = match (io0, &disk) {
            (Some(a), Some((_, b))) => Some((100.0 * b.saturating_sub(a) as f64 / (secs * 1000.0)).min(100.0)),
            _ => None,
        };
        io0 = disk.as_ref().map(|d| d.1);
        let used = space_used(&images);

        let now = alerts(&[("CPU", cpu), ("RAM", ram), ("Network", net_pct), ("Disk busy", busy), ("Disk space", used)], limit, &mut streak);
        let names = |v: &[String]| v.iter().map(|a| a.split(' ').next().unwrap_or("").to_string()).collect::<Vec<_>>();
        if names(&now) != names(&shown) {
            if now.is_empty() {
                tracing::info!("server resources back under {limit:.0}%");
            } else {
                tracing::warn!("server resources at their limit: {} (threshold {limit:.0}%) — clients boot / load slower", now.join(", "));
            }
        }
        shown = now;
        let data = serde_json::json!({
            "cpu": cpu, "ram": ram,
            "net": {"iface": iface, "rx": rx / 1e6, "tx": tx / 1e6, "link": link, "pct": net_pct},
            "disk": {"dev": disk.map(|d| d.0), "busy": busy, "used": used},
            "limit": limit, "alerts": shown,
        })
        .to_string();
        *LATEST.lock().unwrap() = data.clone();
        let _ = RES.send(data); // Err = no page open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_after_a_streak_and_cleared_below() {
        let mut s = HashMap::new();
        let at = |cpu: f64, s: &mut HashMap<_, _>| alerts(&[("CPU", Some(cpu)), ("Network", None)], 80.0, s);
        assert!(at(95.0, &mut s).is_empty() && at(85.0, &mut s).is_empty(), "a short burst is not an alert");
        assert_eq!(at(80.0, &mut s), vec!["CPU 80%"]);
        assert!(at(79.0, &mut s).is_empty(), "back under → cleared at once");
        assert!(at(99.0, &mut s).is_empty(), "the streak starts again");
    }

    #[test]
    fn reads_this_machine() {
        assert!(mem_total() > 0 && fd_limit() > 0);
        let (total, idle) = cpu_times();
        assert!(total >= idle && total > 0);
        assert!(net_bytes("lo").is_some(), "loopback is in /proc/net/dev");
        assert!(space_used(Path::new("/")).is_some_and(|u| (0.0..=100.0).contains(&u)));
        assert!(ram_share(10, 1 << 20, 7) >= 7);
    }
}
