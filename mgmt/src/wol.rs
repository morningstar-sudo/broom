// wol.rs — Wake-on-LAN. Sends the magic packet directly over UDP (std), no etherwake needed.
// Magic packet = 6×0xFF + 16×MAC, to the boot LAN's broadcast address.
use std::net::{Ipv4Addr, UdpSocket};

/// Parse "AA:BB:CC:DD:EE:FF" (or '-') → 6 bytes.
fn parse_mac(mac: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = mac.split(|c| c == ':' || c == '-').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(out)
}

/// The boot LAN's broadcast address (server IP | !netmask, Network page) — sent there, the packet leaves through the
/// NIC of that LAN, not the default route's (255.255.255.255 goes out of one interface only). Not set up → limited
/// broadcast.
fn lan_broadcast(server: &str, mask: &str) -> Ipv4Addr {
    match (server.trim().parse::<Ipv4Addr>(), mask.trim().parse::<Ipv4Addr>()) {
        (Ok(s), Ok(m)) => Ipv4Addr::from(u32::from(s) | !u32::from(m)),
        (Ok(s), Err(_)) => Ipv4Addr::from(u32::from(s) | 0xff),
        _ => Ipv4Addr::BROADCAST,
    }
}

pub fn wake(db: &dyn crate::db::Db, mac: &str) -> std::io::Result<()> {
    let mac = parse_mac(mac)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid MAC format"))?;
    let mut packet = vec![0xFFu8; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }
    let to = lan_broadcast(&db.get_config("dhcp_server_ip", ""), &db.get_config("dhcp_netmask", ""));
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_broadcast(true)?;
    sock.send_to(&packet, (to, 9))?;
    Ok(())
}

#[cfg(test)]
#[test]
fn broadcast_of_the_boot_lan() {
    assert_eq!(lan_broadcast("10.0.0.12", "255.255.255.0"), Ipv4Addr::new(10, 0, 0, 255));
    assert_eq!(lan_broadcast("10.0.1.12", "255.255.254.0"), Ipv4Addr::new(10, 0, 1, 255));
    assert_eq!(lan_broadcast("10.0.0.12", ""), Ipv4Addr::new(10, 0, 0, 255), "no netmask → /24");
    assert_eq!(lan_broadcast("", ""), Ipv4Addr::BROADCAST);
}
