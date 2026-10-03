// tftp.rs — built-in read-only TFTP server (replaces dnsmasq's): RFC 1350 + blksize (RFC 2348) +
// tsize/timeout (RFC 2349). Only boot files: snponly.efi straight from the bytes embedded in the
// binary, anything else read-only from <home>/tftp. One task + one socket per transfer.
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;
use tokio::net::UdpSocket;

const RRQ: u16 = 1;
const DATA: u16 = 3;
const ACK: u16 = 4;
const ERROR: u16 = 5;
const OACK: u16 = 6;
/// Largest block that fits a 1500-byte MTU (what UEFI firmware asks for); bigger requests are lowered.
const MAX_BLKSIZE: usize = 1468;
const RETRIES: usize = 5;

pub async fn serve(sock: UdpSocket, server: Ipv4Addr, iface: String) {
    serve_inner(sock, server, iface).await
}

async fn serve_inner(sock: UdpSocket, server: Ipv4Addr, iface: String) {
    let mut buf = [0u8; 1500];
    loop {
        let Ok((n, peer)) = sock.recv_from(&mut buf).await else { continue };
        // Only RRQ starts a transfer; ignore stray packets on :69 before spawning a task + socket.
        if op(&buf[..n]) != RRQ {
            continue;
        }
        let req = buf[..n].to_vec();
        let iface = iface.clone();
        tokio::spawn(async move {
            if let Err(e) = transfer(&req, peer, server, &iface).await {
                tracing::warn!("tftp {peer}: {e}");
            }
        });
    }
}

fn op(b: &[u8]) -> u16 {
    if b.len() < 2 { 0 } else { u16::from_be_bytes([b[0], b[1]]) }
}

/// RRQ body "file\0mode\0[opt\0value\0]*" → (file, [(opt lowercase, value)]).
fn parse_rrq(b: &[u8]) -> Option<(String, Vec<(String, String)>)> {
    let mut f = b.split(|&c| c == 0).map(|s| String::from_utf8_lossy(s).into_owned());
    let name = f.next().filter(|n| !n.is_empty())?;
    f.next()?; // mode: octet/netascii — always served as binary
    let mut opts = Vec::new();
    while let (Some(k), Some(v)) = (f.next(), f.next()) {
        if !k.is_empty() {
            opts.push((k.to_ascii_lowercase(), v));
        }
    }
    Some((name, opts))
}

/// TFTP serves ONLY the embedded iPXE binaries, from memory: our own `snponly.efi`, and the official Secure Boot pair
/// `sb/snponly-shim.efi` + `sb/snponly.efi`. Firmware fetches just those over TFTP; iPXE then pulls kernel/initrd/golden
/// over HTTP (`/tftp/...` ServeDir). Refusing everything else stops a spoofed UDP packet from making the server read a
/// multi-GB golden into RAM (no handshake on UDP → an amplification/OOM vector).
fn load(name: &str) -> Result<&'static [u8], String> {
    let name = name.replace('\\', "/");
    match name.trim_start_matches('/') {
        "snponly.efi" => Ok(crate::boot::SNPONLY_EFI),
        "sb/snponly-shim.efi" => Ok(crate::boot::SB_SHIM_EFI),
        "sb/snponly.efi" => Ok(crate::boot::SB_IPXE_EFI),
        other => Err(format!("TFTP serves only the iPXE binaries (asked {other:?}); other files go over HTTP")),
    }
}

async fn send_error(sock: &UdpSocket, to: SocketAddr, code: u16, msg: &str) {
    let mut p = vec![0, ERROR as u8];
    p.extend(code.to_be_bytes());
    p.extend(msg.as_bytes());
    p.push(0);
    let _ = sock.send_to(&p, to).await;
}

/// Send `pkt` until the peer ACKs `block` (resend on timeout, RETRIES times).
/// Ok(false) = the peer answered with an ERROR (e.g. UEFI aborts after reading tsize).
async fn send_wait(sock: &UdpSocket, peer: SocketAddr, pkt: &[u8], block: u16, timeout: Duration) -> Result<bool, String> {
    let mut buf = [0u8; 1500];
    for _ in 0..RETRIES {
        sock.send_to(pkt, peer).await.map_err(|e| e.to_string())?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await {
                Err(_) => break, // timeout → resend
                Ok(Err(e)) => return Err(e.to_string()),
                Ok(Ok((_, from))) if from != peer => send_error(sock, from, 5, "unknown transfer ID").await,
                Ok(Ok((n, _))) => match op(&buf[..n]) {
                    ACK if n >= 4 && u16::from_be_bytes([buf[2], buf[3]]) == block => return Ok(true),
                    ERROR => return Ok(false),
                    _ => {} // duplicate/old ACK: ignore (no resend → no Sorcerer's Apprentice)
                },
            }
        }
    }
    Err(format!("no ACK for block {block} after {RETRIES} tries"))
}

async fn transfer(req: &[u8], peer: SocketAddr, server: Ipv4Addr, iface: &str) -> Result<(), String> {
    let sock = match crate::dhcp::udp(SocketAddrV4::new(server, 0), iface, false) {
        Ok(s) => s,
        Err(_) => crate::dhcp::udp(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0), iface, false).map_err(|e| e.to_string())?,
    };
    // serve_inner only forwards RRQ (WRQ etc. are dropped there).
    let (name, opts) = parse_rrq(&req[2..]).ok_or("malformed RRQ")?;
    let data = match load(&name) {
        Ok(d) => d,
        Err(e) => {
            send_error(&sock, peer, 1, "file not found").await;
            return Err(e);
        }
    };

    let mut blksize = 512usize;
    let mut timeout = Duration::from_secs(2);
    let mut oack = vec![0, OACK as u8];
    for (k, v) in &opts {
        let val = match k.as_str() {
            "blksize" => match v.parse::<usize>() {
                Ok(b) => {
                    blksize = b.clamp(8, MAX_BLKSIZE);
                    blksize.to_string()
                }
                Err(_) => continue,
            },
            "tsize" => data.len().to_string(),
            // Cap at 5 s: a client mustn't hold a transfer (and its socket) for up to 255 s × RETRIES.
            "timeout" => match v.parse::<u64>() {
                Ok(t) if (1..=255).contains(&t) => {
                    timeout = Duration::from_secs(t.min(5));
                    timeout.as_secs().to_string()
                }
                _ => continue,
            },
            _ => continue,
        };
        for s in [k.as_str(), val.as_str()] {
            oack.extend(s.as_bytes());
            oack.push(0);
        }
    }
    if oack.len() > 2 && !send_wait(&sock, peer, &oack, 0, timeout).await? {
        return Ok(()); // size probe: client read tsize, then aborted on purpose
    }

    let (mut block, mut off) = (1u16, 0usize);
    loop {
        let end = (off + blksize).min(data.len());
        let mut pkt = vec![0, DATA as u8];
        pkt.extend(block.to_be_bytes());
        pkt.extend(&data[off..end]);
        if !send_wait(&sock, peer, &pkt, block, timeout).await? {
            return Err(format!("{name}: client aborted at block {block}"));
        }
        if end - off < blksize {
            break; // short (possibly empty) block = end of file
        }
        off = end;
        block = block.wrapping_add(1);
    }
    tracing::info!("client {} loaded {name} over TFTP ({} bytes)", peer.ip(), data.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn server() -> SocketAddr {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(serve_inner(sock, Ipv4Addr::LOCALHOST, String::new()));
        addr
    }

    async fn recv(c: &UdpSocket) -> (Vec<u8>, SocketAddr) {
        let mut b = [0u8; 1600];
        let (n, from) = tokio::time::timeout(Duration::from_secs(5), c.recv_from(&mut b)).await.unwrap().unwrap();
        (b[..n].to_vec(), from)
    }

    fn rrq(name: &str, opts: &[(&str, &str)]) -> Vec<u8> {
        let mut p = vec![0, 1];
        for s in [name, "octet"].into_iter().chain(opts.iter().flat_map(|(k, v)| [*k, *v])) {
            p.extend(s.as_bytes());
            p.push(0);
        }
        p
    }

    fn ack(n: u16) -> Vec<u8> {
        let mut p = vec![0, 4];
        p.extend(n.to_be_bytes());
        p
    }

    #[tokio::test]
    async fn options_blocks_and_resend() {
        // TFTP serves only the embedded snponly.efi → use its first blocks to exercise OACK/blocks/resend/timeout.
        let want = crate::boot::SNPONLY_EFI;
        let srv = server().await;
        let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        c.send_to(&rrq("snponly.efi", &[("blksize", "1024"), ("tsize", "0"), ("timeout", "1")]), srv).await.unwrap();
        let (oack, tid) = recv(&c).await;
        assert_eq!(oack, format!("\0\x06blksize\01024\0tsize\0{}\0timeout\01\0", want.len()).into_bytes());
        c.send_to(&ack(0), tid).await.unwrap();
        let (b1, _) = recv(&c).await;
        assert_eq!((op(&b1), &b1[2..4], b1.len()), (DATA, &[0, 1][..], 4 + 1024));
        // Don't ACK block 1 → the server resends it after the 1 s timeout.
        let (again, _) = recv(&c).await;
        assert_eq!(again, b1);
        let mut got = b1[4..].to_vec();
        for n in 1..=3u16 {
            c.send_to(&ack(n), tid).await.unwrap();
            let (b, _) = recv(&c).await;
            assert_eq!(&b[2..4], &(n + 1).to_be_bytes());
            got.extend(&b[4..]);
        }
        assert_eq!(got, &want[..got.len()]); // first 4 blocks stream byte-for-byte
    }

    #[tokio::test]
    async fn embedded_ipxe_and_refusals() {
        let srv = server().await;
        let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // snponly.efi comes from the binary itself; tsize probe then abort (UEFI does this).
        c.send_to(&rrq("snponly.efi", &[("tsize", "0")]), srv).await.unwrap();
        let (oack, tid) = recv(&c).await;
        assert_eq!(oack, format!("\0\x06tsize\0{}\0", crate::boot::SNPONLY_EFI.len()).into_bytes());
        c.send_to(b"\0\x05\0\x08abort\0", tid).await.unwrap();
        // The official Secure Boot pair under sb/ (shim → loads sb/snponly.efi by name).
        for (name, want) in [("sb/snponly-shim.efi", crate::boot::SB_SHIM_EFI), ("sb/snponly.efi", crate::boot::SB_IPXE_EFI)] {
            c.send_to(&rrq(name, &[("tsize", "0")]), srv).await.unwrap();
            let (oack, tid) = recv(&c).await;
            assert_eq!(oack, format!("\0\x06tsize\0{}\0", want.len()).into_bytes(), "{name}");
            c.send_to(b"\0\x05\0\x08abort\0", tid).await.unwrap();
        }
        // Anything else (including a traversal attempt) → "file not found" ERROR, never a disk read.
        for req in [rrq("../etc/passwd", &[]), rrq("broom-win/x/golden.vhdx", &[]), rrq("nope.bin", &[]), rrq("sb/../x", &[])] {
            c.send_to(&req, srv).await.unwrap();
            assert_eq!(op(&recv(&c).await.0), ERROR);
        }
        // A WRQ (write) is dropped silently (no reflection): no reply within 300 ms.
        c.send_to(&[0, 2, b'x', 0, b'o', b'c', b't', b'e', b't', 0], srv).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(300), c.recv_from(&mut [0u8; 64])).await.is_err());
    }
}
