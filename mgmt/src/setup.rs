// setup.rs — auto-fix gọi từ main() khi preflight FAIL (không còn subcommand riêng).
// Dò IFACE/IP/SUBNET → cài gói → copy snponly.efi → seed config DHCP vào DB →
// dnsmasq::apply (sinh pxe.conf + restart). main() preflight lại sau khi xong.
//
// Flags (đọc từ args của lệnh chạy chính, vd `sudo ./bootrom-mgmt --mode full`):
//   --iface --ip --subnet --mode <proxy|full>
//   --range-start --range-end --netmask --gateway --dns
use std::path::Path;
use std::process::Command;

use crate::{db, dnsmasq, preflight};

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
    println!("  $ {bin} {}", args.join(" "));
    Command::new(bin)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Đọc `--flag value` từ args.
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

pub fn run(args: &[String]) {
    println!("== bootrom-mgmt setup ==");

    // 1. Tham số mạng (flag override detect).
    let (d_iface, d_ip) = detect_iface_ip();
    let iface = flag(args, "--iface").or(d_iface);
    let ip = flag(args, "--ip").or(d_ip);
    let subnet = flag(args, "--subnet").or_else(|| iface.as_deref().and_then(detect_subnet));

    let (iface, ip, subnet) = match (iface, ip, subnet) {
        (Some(i), Some(p), Some(s)) => (i, p, s),
        (i, p, s) => {
            eprintln!("Không dò đủ tham số mạng: IFACE={i:?} IP={p:?} SUBNET={s:?}");
            eprintln!("Chỉ định tay: setup --iface ens33 --ip 10.0.0.12 --subnet 10.0.0.0");
            std::process::exit(1);
        }
    };
    let mode = flag(args, "--mode").unwrap_or_else(|| "proxy".into());
    println!("IFACE={iface}  IP={ip}  SUBNET={subnet}  MODE={mode}");

    // 2. Root.
    if !preflight::is_root() {
        eprintln!("\nCần root. Chạy: sudo bootrom-mgmt setup ...");
        std::process::exit(1);
    }

    // 3. Cài gói thiếu.
    let pkgs = preflight::missing_pkgs();
    if pkgs.is_empty() {
        println!("[gói] đủ");
    } else {
        println!("[gói] cài: {}", pkgs.join(" "));
        run_cmd("apt-get", &["update", "-y"]);
        let mut a = vec!["install", "-y"];
        a.extend(pkgs.iter().map(|s| s.as_str()));
        if !run_cmd("apt-get", &a) {
            eprintln!("apt install thất bại");
            std::process::exit(1);
        }
    }

    // 4. iPXE binary: snponly.efi (UEFI).
    std::fs::create_dir_all("/srv/tftp").ok();
    for (dst, src, label) in [
        ("/srv/tftp/snponly.efi", "/usr/lib/ipxe/snponly.efi", "snponly.efi (UEFI)"),
    ] {
        if Path::new(dst).exists() {
            println!("[tftp] {label} đã có");
        } else if Path::new(src).exists() {
            std::fs::copy(src, dst).ok();
            println!("[tftp] copy {label}");
        } else {
            eprintln!("[tftp] thiếu {src} — apt install ipxe");
        }
    }

    // 5. Seed config DHCP vào DB (nguồn sự thật cho dnsmasq.rs).
    let conn = db::open("bootrom.db").expect("mở DB");
    let seed = |k: &str, v: &str| {
        db::set_config(&conn, k, v).ok();
    };
    seed("dhcp_iface", &iface);
    seed("dhcp_server_ip", &ip);
    seed("dhcp_subnet", &subnet);
    seed("dhcp_mode", &mode);
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

    // 6. Sinh pxe.conf + restart dnsmasq. (main() sẽ preflight lại sau khi return.)
    match dnsmasq::apply(&conn) {
        Ok(true) => println!("[dnsmasq] pxe.conf ghi + restart OK ({mode})"),
        Ok(false) => eprintln!("[dnsmasq] pxe.conf ghi xong nhưng restart LỖI — kiểm journalctl -u dnsmasq"),
        Err(e) => {
            eprintln!("[dnsmasq] ghi config lỗi: {e}");
            std::process::exit(1);
        }
    }
    println!("== setup xong ==");
}
