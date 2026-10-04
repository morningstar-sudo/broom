// setup.rs — first run, called from main() while the network isn't configured yet
// (no separate subcommand). Detect IFACE/IP/SUBNET/gateway/DNS → seed the DHCP config into
// the DB. The DHCP/TFTP servers themselves are built in (dhcp.rs/tftp.rs) and started by main().
// Also `bootrom-mgmt install-service` (systemd unit, install_service below).
//
// Flags (read from the args of the main command, e.g. `sudo ./bootrom-mgmt --mode full`):
//   --iface --ip --subnet --mode <full|off>
//   --range-start --range-end --netmask --gateway --dns
use std::process::Command;

use crate::{db, preflight};

/// Distro services that older versions installed and the binary now replaces: stop + disable them
/// if running so their ports (67/69) are free. Anything else holding a port is left alone
/// (the bind error then names the port).
pub fn takeover(services: &[&str]) {
    // Probes only: a unit that isn't installed makes systemctl print "Failed to get unit file state" even
    // with --quiet → stderr dropped.
    let probe = |verb: &str, s: &str| {
        Command::new("systemctl")
            .args([verb, "--quiet", s])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|x| x.success())
    };
    for s in services {
        let (active, enabled) = (probe("is-active", s), probe("is-enabled", s));
        if active || enabled {
            let ok = Command::new("systemctl").args(["disable", "--now", s]).status().is_ok_and(|x| x.success());
            if ok {
                tracing::info!("takeover: {s} stopped + disabled (now built into bootrom-mgmt)");
            } else {
                tracing::warn!("takeover: failed to stop {s}");
            }
        }
    }
}

const UNIT_PATH: &str = "/etc/systemd/system/bootrom-mgmt.service";

/// systemd unit that runs this binary from its own directory. `args` (e.g. `--port 8080`) and the BOOTROM_* /
/// RUST_LOG overrides of the installing shell are carried over.
fn unit_text(exe: &str, home: &str, args: &[String], env: &[(String, String)]) -> String {
    let exec = std::iter::once(exe.to_string()).chain(args.iter().cloned()).collect::<Vec<_>>().join(" ");
    let env: String = env.iter().map(|(k, v)| format!("Environment=\"{k}={v}\"\n")).collect();
    format!(
        "[Unit]\nDescription=Broom diskless boot server\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nExecStart={exec}\nWorkingDirectory={home}\n{env}Restart=on-failure\nRestartSec=3\n\n\
         [Install]\nWantedBy=multi-user.target\n"
    )
}

/// `bootrom-mgmt install-service [flags]` — write the systemd unit, enable + start it, exit.
pub fn install_service(args: &[String]) -> ! {
    if !preflight::is_root() {
        tracing::error!("install-service needs root: sudo {} install-service", args.first().map_or("./bootrom-mgmt", String::as_str));
        std::process::exit(1);
    }
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| args[0].clone());
    let env: Vec<(String, String)> = ["BOOTROM_HOME", "BOOTROM_IMAGES_DIR", "BOOTROM_STORAGE_DIR", "BOOTROM_DB", "RUST_LOG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
    let unit = unit_text(&exe, &crate::home().display().to_string(), &args[2..], &env);
    if let Err(e) = std::fs::write(UNIT_PATH, &unit) {
        tracing::error!("write {UNIT_PATH}: {e}");
        std::process::exit(1);
    }
    let ok = |a: &[&str]| Command::new("systemctl").args(a).status().is_ok_and(|s| s.success());
    if !(ok(&["daemon-reload"]) && ok(&["enable", "--now", "bootrom-mgmt"])) {
        tracing::error!("systemctl failed — see `systemctl status bootrom-mgmt`");
        std::process::exit(1);
    }
    tracing::info!("installed {UNIT_PATH}, enabled + started. Logs: journalctl -u bootrom-mgmt -f");
    std::process::exit(0);
}

fn sh(cmd: &str) -> String {
    Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn tok_after(toks: &[&str], key: &str) -> Option<String> {
    toks.iter()
        .position(|t| *t == key)
        .and_then(|i| toks.get(i + 1))
        .map(|s| s.to_string())
}

/// (iface, own ip, gateway) of the default route.
fn detect_iface_ip() -> (Option<String>, Option<String>, Option<String>) {
    let out = sh("ip route get 1.1.1.1 2>/dev/null");
    let toks: Vec<&str> = out.split_whitespace().collect();
    (tok_after(&toks, "dev"), tok_after(&toks, "src"), tok_after(&toks, "via"))
}

/// Upstream DNS servers for the clients: the non-loopback nameservers of resolv.conf (systemd-resolved's real list
/// first — /etc/resolv.conf there only says 127.0.0.53, useless to a client).
fn nameservers(resolv: &str) -> Vec<String> {
    resolv
        .lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .filter_map(|s| s.trim().parse::<std::net::Ipv4Addr>().ok())
        .filter(|ip| !ip.is_loopback())
        .map(|ip| ip.to_string())
        .collect()
}

fn detect_dns() -> Vec<String> {
    ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"]
        .iter()
        .map(|f| nameservers(&std::fs::read_to_string(f).unwrap_or_default()))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// (network, netmask) of the interface's connected route, e.g. 10.0.0.0/23 → ("10.0.0.0", "255.255.254.0").
fn detect_subnet(iface: &str) -> Option<(String, String)> {
    let out = sh(&format!(
        "ip -o route show dev {iface} scope link proto kernel 2>/dev/null"
    ));
    cidr_parts(out.split_whitespace().next()?)
}

fn cidr_parts(cidr: &str) -> Option<(String, String)> {
    let (net, prefix) = cidr.split_once('/').unwrap_or((cidr, "24"));
    let p: u32 = prefix.parse().ok().filter(|p| *p <= 32)?;
    let mask = if p == 0 { 0 } else { u32::MAX << (32 - p) };
    Some((net.to_string(), std::net::Ipv4Addr::from(mask).to_string()))
}

#[cfg(test)]
#[test]
fn cidr_to_netmask() {
    assert_eq!(cidr_parts("10.0.0.0/23"), Some(("10.0.0.0".into(), "255.255.254.0".into())));
    assert_eq!(cidr_parts("192.168.1.0/24"), Some(("192.168.1.0".into(), "255.255.255.0".into())));
    assert_eq!(cidr_parts("10.0.0.0"), Some(("10.0.0.0".into(), "255.255.255.0".into())), "no prefix → /24");
    assert_eq!(cidr_parts("10.0.0.0/33"), None);
}

/// Read `--flag value` from args.
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

pub fn run(args: &[String]) {
    tracing::info!("setup: detecting the network");

    // 1. Network parameters (flags override detection).
    let (d_iface, d_ip, d_gw) = detect_iface_ip();
    let iface = flag(args, "--iface").or(d_iface);
    let ip = flag(args, "--ip").or(d_ip);
    let detected = iface.as_deref().and_then(detect_subnet);
    let subnet = flag(args, "--subnet").or_else(|| detected.as_ref().map(|(n, _)| n.clone()));
    let netmask = detected.map(|(_, m)| m);

    let (iface, ip, subnet) = match (iface, ip, subnet) {
        (Some(i), Some(p), Some(s)) => (i, p, s),
        (i, p, s) => {
            tracing::error!("setup: could not detect all network parameters: IFACE={i:?} IP={p:?} SUBNET={s:?}");
            tracing::error!("setup: give them by hand, e.g. --iface ens33 --ip 10.0.0.12 --subnet 10.0.0.0");
            std::process::exit(1);
        }
    };
    // The DHCP server starts OFF: most LANs already have one (a router), and a second one breaks the LAN. Turn it on
    // from the Network page (or `--mode full`). TFTP (iPXE for the router's PXE option) runs either way.
    let mode = if flag(args, "--mode").as_deref() == Some("full") { "full" } else { "off" };
    tracing::info!("setup: iface {iface} - ip {ip} - subnet {subnet}");

    // 2. Root.
    if !preflight::is_root() {
        tracing::error!("setup: root required — run: sudo ./bootrom-mgmt");
        std::process::exit(1);
    }

    // 3. Boot asset directory (kernels/initrds/golden files served over HTTP /tftp/...).
    if let Err(e) = std::fs::create_dir_all(crate::tftp_dir()) {
        tracing::error!("setup: mkdir {} failed: {e}", crate::tftp_dir().display());
    }

    // 4. Seed the DHCP config into the DB (source of truth for dhcp.rs).
    let database = db::open(&db::url()).unwrap_or_else(|e| {
        tracing::error!("database: {e}");
        std::process::exit(1)
    });
    // Setup runs again whenever the server IP is unset (e.g. cleared): never overwrite what the admin set on the
    // web. Detected values only fill EMPTY keys; flags given on the command line always win.
    let seed = |k: &str, detected: Option<String>, fl: &str| {
        let v = flag(args, fl).or_else(|| detected.filter(|_| database.get_config(k, "").is_empty()));
        if let Some(v) = v {
            database.set_config(k, &v).ok();
        }
    };
    let dns = detect_dns().join(",");
    seed("dhcp_iface", Some(iface), "--iface");
    seed("dhcp_server_ip", Some(ip), "--ip");
    seed("dhcp_subnet", Some(subnet), "--subnet");
    // The seed is 255.255.255.0 in a new DB → the detected mask replaces it (a /23 LAN gets 255.255.254.0).
    if flag(args, "--netmask").is_none() && database.get_config("dhcp_range_start", "").is_empty() {
        if let Some(m) = &netmask {
            database.set_config("dhcp_netmask", m).ok();
        }
    }
    if flag(args, "--mode").is_some() || database.get_config("dhcp_mode", "").is_empty() {
        database.set_config("dhcp_mode", mode).ok(); // normalized: full | off
    }
    seed("dhcp_gateway", d_gw.clone(), "--gateway");
    // No usable resolver found → the gateway (routers answer DNS).
    seed("dhcp_dns", if dns.is_empty() { d_gw } else { Some(dns) }, "--dns");
    for (fl, key) in [("--range-start", "dhcp_range_start"), ("--range-end", "dhcp_range_end"), ("--netmask", "dhcp_netmask")] {
        seed(key, None, fl);
    }

    tracing::info!(
        "setup done — DHCP server {} (Network page to change), TFTP serves iPXE",
        database.get_config("dhcp_mode", "off")
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn dns_from_resolv_conf() {
        assert_eq!(super::nameservers("nameserver 127.0.0.53\noptions edns0\n"), Vec::<String>::new());
        assert_eq!(super::nameservers("# x\nnameserver 192.168.1.1\nnameserver  8.8.8.8\nnameserver ::1\n"), ["192.168.1.1", "8.8.8.8"]);
    }

    #[test]
    fn unit_runs_binary_from_its_home() {
        let env = [("BOOTROM_DB".to_string(), "sqlite:///data/b.db".to_string())];
        let u = super::unit_text("/opt/bootrom/bootrom-mgmt", "/opt/bootrom", &["--port".to_string(), "8080".to_string()], &env);
        assert!(u.contains("\nExecStart=/opt/bootrom/bootrom-mgmt --port 8080\n"));
        assert!(u.contains("\nWorkingDirectory=/opt/bootrom\n"));
        assert!(u.contains("\nEnvironment=\"BOOTROM_DB=sqlite:///data/b.db\"\n"));
        assert!(u.contains("\nWantedBy=multi-user.target\n") && u.contains("\nRestart=on-failure\n"));
    }
}
