// setup.rs — auto-fix called from main() when preflight FAILS or the network isn't configured yet
// (no separate subcommand). Detect IFACE/IP/SUBNET → install packages → seed the DHCP config into
// the DB. The DHCP/TFTP servers themselves are built in (dhcp.rs/tftp.rs) and started by main().
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

fn detect_iface_ip() -> (Option<String>, Option<String>) {
    let out = sh("ip route get 1.1.1.1 2>/dev/null");
    let toks: Vec<&str> = out.split_whitespace().collect();
    (tok_after(&toks, "dev"), tok_after(&toks, "src"))
}

fn detect_subnet(iface: &str) -> Option<String> {
    let out = sh(&format!(
        "ip -o route show dev {iface} scope link proto kernel 2>/dev/null"
    ));
    out.split_whitespace()
        .next()
        .map(|cidr| cidr.split('/').next().unwrap_or(cidr).to_string())
}

fn run_cmd(bin: &str, args: &[&str]) -> bool {
    tracing::info!("setup: running {bin} {}", args.join(" "));
    Command::new(bin)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Read `--flag value` from args.
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

pub fn run(args: &[String]) {
    tracing::info!("setup: detecting network + installing packages");

    // 1. Network parameters (flags override detection).
    let (d_iface, d_ip) = detect_iface_ip();
    let iface = flag(args, "--iface").or(d_iface);
    let ip = flag(args, "--ip").or(d_ip);
    let subnet = flag(args, "--subnet").or_else(|| iface.as_deref().and_then(detect_subnet));

    let (iface, ip, subnet) = match (iface, ip, subnet) {
        (Some(i), Some(p), Some(s)) => (i, p, s),
        (i, p, s) => {
            tracing::error!("setup: could not detect all network parameters: IFACE={i:?} IP={p:?} SUBNET={s:?}");
            tracing::error!("setup: give them by hand, e.g. --iface ens33 --ip 10.0.0.12 --subnet 10.0.0.0");
            std::process::exit(1);
        }
    };
    // broom is the LAN's DHCP server by default; `--mode off` seeds it disabled (another DHCP owns the LAN).
    let mode = if flag(args, "--mode").as_deref() == Some("off") { "off" } else { "full" };
    tracing::info!("setup: iface {iface} - ip {ip} - subnet {subnet} - DHCP {mode}");

    // 2. Root.
    if !preflight::is_root() {
        tracing::error!("setup: root required — run: sudo ./bootrom-mgmt [--mode full]");
        std::process::exit(1);
    }

    // 3. Install missing packages.
    let pkgs = preflight::missing_pkgs();
    if pkgs.is_empty() {
        tracing::info!("setup: all packages present");
    } else {
        tracing::info!("setup: installing packages: {}", pkgs.join(" "));
        run_cmd("apt-get", &["update", "-y"]);
        let mut a = vec!["install", "-y"];
        a.extend(pkgs.iter().map(|s| s.as_str()));
        if !run_cmd("apt-get", &a) {
            tracing::error!("setup: apt install failed");
            std::process::exit(1);
        }
    }

    // 4. Boot asset directory (kernels/initrds/golden files served over HTTP /tftp/...).
    if let Err(e) = std::fs::create_dir_all(crate::tftp_dir()) {
        tracing::error!("setup: mkdir {} failed: {e}", crate::tftp_dir().display());
    }

    // 5. Seed the DHCP config into the DB (source of truth for dhcp.rs).
    let database = db::open(&db::url()).unwrap_or_else(|e| {
        tracing::error!("database: {e}");
        std::process::exit(1)
    });
    let seed = |k: &str, v: &str| {
        database.set_config(k, v).ok();
    };
    seed("dhcp_iface", &iface);
    seed("dhcp_server_ip", &ip);
    seed("dhcp_subnet", &subnet);
    seed("dhcp_mode", mode);
    for (fl, key) in [
        ("--range-start", "dhcp_range_start"),
        ("--range-end", "dhcp_range_end"),
        ("--netmask", "dhcp_netmask"),
        ("--gateway", "dhcp_gateway"),
        ("--dns", "dhcp_dns"),
    ] {
        if let Some(v) = flag(args, fl) {
            seed(key, &v);
        }
    }

    tracing::info!("setup done (DHCP {mode} starts with the server)");
}
