// dnsmasq.rs — sinh /etc/dnsmasq.d/pxe.conf từ config trong DB + reload.
// Nguồn sự thật = bảng config + machines. Dùng chung cho subcommand `setup` và API M8.
use rusqlite::Connection;
use std::process::Command;

use crate::db;

/// 3 octet đầu của subnet/ip ("10.0.0.0" -> "10.0.0").
fn prefix3(addr: &str) -> String {
    let p: Vec<&str> = addr.split('.').collect();
    if p.len() == 4 {
        format!("{}.{}.{}", p[0], p[1], p[2])
    } else {
        addr.to_string()
    }
}

/// Danh sách dhcp-host binding từ bảng máy (chỉ máy có đủ ip + hostname).
pub fn bindings(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT mac,ip,hostname FROM machines WHERE ip IS NOT NULL AND hostname IS NOT NULL")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok(format!(
                "dhcp-host={},{},{}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?
            ))
        })
        .unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

/// Sinh nội dung pxe.conf theo config hiện tại (proxy | full).
pub fn generate(conn: &Connection, binds: &[String]) -> String {
    let iface = db::get_config(conn, "dhcp_iface", "");
    let ip = db::get_config(conn, "dhcp_server_ip", "");
    let subnet = db::get_config(conn, "dhcp_subnet", "");
    let mode = db::get_config(conn, "dhcp_mode", "proxy");

    let mut s = String::new();
    s.push_str("# Auto-generated bởi bootrom-mgmt (dnsmasq.rs). ĐỪNG sửa tay.\n");
    s.push_str(&format!("interface={iface}\nbind-interfaces\nport=0\n\n"));
    s.push_str("enable-tftp\ntftp-root=/srv/tftp\n\n");
    // Nhận diện arch: UEFI x64 (7,9). iPXE đã nạp gửi option 175.
    s.push_str("dhcp-match=set:efi-x64,option:client-arch,7\n");
    s.push_str("dhcp-match=set:efi-x64,option:client-arch,9\n");
    s.push_str("dhcp-match=set:ipxe,175\n\n");

    if mode == "full" {
        let mut start = db::get_config(conn, "dhcp_range_start", "");
        if start.is_empty() {
            start = format!("{}.100", prefix3(&subnet));
        }
        let mut end = db::get_config(conn, "dhcp_range_end", "");
        if end.is_empty() {
            end = format!("{}.200", prefix3(&subnet));
        }
        let netmask = db::get_config(conn, "dhcp_netmask", "255.255.255.0");
        let lease = db::get_config(conn, "dhcp_lease", "12h");
        let gw = db::get_config(conn, "dhcp_gateway", "");
        let dns = db::get_config(conn, "dhcp_dns", "");

        s.push_str("# FULL DHCP mode — dnsmasq cấp IP + bootfile (UEFI ăn ngay)\n");
        s.push_str("dhcp-authoritative\n");
        s.push_str(&format!("dhcp-range={start},{end},{netmask},{lease}\n"));
        if !gw.is_empty() {
            s.push_str(&format!("dhcp-option=3,{gw}\n"));
        }
        if !dns.is_empty() {
            s.push_str(&format!("dhcp-option=6,{dns}\n"));
        }
        s.push_str(&format!("dhcp-boot=tag:efi-x64,tag:!ipxe,snponly.efi,,{ip}\n"));
        s.push_str(&format!("dhcp-boot=tag:ipxe,http://{ip}/boot.ipxe\n"));
        if !binds.is_empty() {
            s.push_str("\n# DHCP binding (M8)\n");
            for b in binds {
                s.push_str(b);
                s.push('\n');
            }
        }
    } else {
        s.push_str("# proxyDHCP mode — router khác cấp IP, chỉ trả boot info\n");
        s.push_str(&format!("dhcp-range={subnet},proxy\n"));
        s.push_str("dhcp-boot=tag:efi-x64,tag:!ipxe,snponly.efi\n");
        s.push_str(&format!("dhcp-boot=tag:ipxe,http://{ip}/boot.ipxe\n"));
        s.push_str("pxe-service=tag:efi-x64,x86-64_EFI,\"iPXE UEFI Boot\",snponly.efi\n");
    }

    s.push_str("\nlog-dhcp\n");
    s
}

/// Ghi pxe.conf + restart dnsmasq. Trả (ok_reload).
pub fn apply(conn: &Connection) -> std::io::Result<bool> {
    let binds = bindings(conn);
    let conf = generate(conn, &binds);
    std::fs::write("/etc/dnsmasq.d/pxe.conf", conf)?;
    let ok = Command::new("systemctl")
        .args(["restart", "dnsmasq"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    Ok(ok)
}
