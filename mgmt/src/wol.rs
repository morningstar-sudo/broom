// wol.rs — Wake-on-LAN. Gửi magic packet trực tiếp bằng UDP (std), khỏi cần etherwake.
// ponytail: magic packet = 6×0xFF + 16×MAC. 20 dòng, khỏi shell-out.
use std::net::UdpSocket;

/// Parse "AA:BB:CC:DD:EE:FF" (hoặc '-') → 6 byte.
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

pub fn wake(mac: &str) -> std::io::Result<()> {
    let mac = parse_mac(mac)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "MAC sai định dạng"))?;
    let mut packet = vec![0xFFu8; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_broadcast(true)?;
    sock.send_to(&packet, "255.255.255.255:9")?;
    Ok(())
}
