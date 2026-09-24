// tftp.rs — built-in read-only TFTP server (replaces dnsmasq's): RFC 1350 + blksize (RFC 2348) +
// tsize/timeout (RFC 2349). Only boot files: snponly.efi straight from the bytes embedded in the
// binary, anything else read-only from <home>/tftp. One task + one socket per transfer.
use std::borrow::Cow;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::Path;
use std::time::Duration;
use tokio::net::UdpSocket;

const RRQ: u16 = 1;
const WRQ: u16 = 2;
const DATA: u16 = 3;
const ACK: u16 = 4;
const ERROR: u16 = 5;
const OACK: u16 = 6;
/// Largest block that fits a 1500-byte MTU (what UEFI firmware asks for); bigger requests are lowered.
const MAX_BLKSIZE: usize = 1468;
const RETRIES: usize = 5;

pub async fn serve(sock: UdpSocket, server: Ipv4Addr, iface: String) {
    serve_root(sock, server, iface, crate::tftp_dir()).await
}

async fn serve_root(sock: UdpSocket, server: Ipv4Addr, iface: String, root: &'static Path) {
    let mut buf = [0u8; 1500];
    loop {
        let Ok((n, peer)) = sock.recv_from(&mut buf).await else { continue };
        let req = buf[..n].to_vec();
        let iface = iface.clone();
        tokio::spawn(async move {
            if let Err(e) = transfer(&req, peer, server, &iface, root).await {
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

/// File contents; rejects ".." and anything outside the root.
fn load(root: &Path, name: &str) -> Result<Cow<'static, [u8]>, String> {
    let name = name.replace('\\', "/");
    let name = name.trim_start_matches('/');
    if name.is_empty() || name.split('/').any(|c| c == "..") {
        return Err(format!("invalid path {name:?}"));
    }
    if name == "snponly.efi" {
        return Ok(Cow::Borrowed(crate::boot::SNPONLY_EFI));
    }
    std::fs::read(root.join(name)).map(Cow::Owned).map_err(|e| format!("{name}: {e}"))
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

async fn transfer(req: &[u8], peer: SocketAddr, server: Ipv4Addr, iface: &str, root: &Path) -> Result<(), String> {
    let sock = match crate::dhcp::udp(SocketAddrV4::new(server, 0), iface, false) {
        Ok(s) => s,
        Err(_) => crate::dhcp::udp(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0), iface, false).map_err(|e| e.to_string())?,
    };
    match op(req) {
        RRQ => {}
        WRQ => {
            send_error(&sock, peer, 2, "read-only server").await;
            return Err("write request refused".into());
        }
        _ => return Ok(()), // stray packet on :69
    }
    let (name, opts) = parse_rrq(&req[2..]).ok_or("malformed RRQ")?;
    let data = match load(root, &name) {
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
            "timeout" => match v.parse::<u64>() {
                Ok(t) if (1..=255).contains(&t) => {
                    timeout = Duration::from_secs(t);
                    t.to_string()
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

    async fn server(root: &'static Path) -> SocketAddr {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(serve_root(sock, Ipv4Addr::LOCALHOST, String::new(), root));
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
        let dir: &'static Path = Box::leak(std::env::temp_dir().join("broom_tftp_test").into_boxed_path());
        std::fs::create_dir_all(dir).unwrap();
        let data: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        std::fs::write(dir.join("a.bin"), &data).unwrap();
        let srv = server(dir).await;
        let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        c.send_to(&rrq("/a.bin", &[("blksize", "1024"), ("tsize", "0"), ("timeout", "1")]), srv).await.unwrap();
        let (oack, tid) = recv(&c).await;
        assert_eq!(oack, b"\0\x06blksize\01024\0tsize\03000\0timeout\01\0");
        c.send_to(&ack(0), tid).await.unwrap();
        let (b1, _) = recv(&c).await;
        assert_eq!((op(&b1), &b1[2..4], b1.len()), (DATA, &[0, 1][..], 4 + 1024));
        // Don't ACK block 1 → the server resends it after the 1 s timeout.
        let (again, _) = recv(&c).await;
        assert_eq!(again, b1);
        let mut got = b1[4..].to_vec();
        for n in 1..=3u16 {
            c.send_to(&ack(n), tid).await.unwrap();
            if n == 3 {
                break;
            }
            let (b, _) = recv(&c).await;
            assert_eq!(&b[2..4], &(n + 1).to_be_bytes());
            got.extend(&b[4..]);
        }
        assert_eq!(got, data); // 1024 + 1024 + 952
    }

    #[tokio::test]
    async fn embedded_ipxe_and_refusals() {
        let srv = server(Path::new("/nonexistent")).await;
        let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // snponly.efi comes from the binary itself; tsize probe then abort (UEFI does this).
        c.send_to(&rrq("snponly.efi", &[("tsize", "0")]), srv).await.unwrap();
        let (oack, tid) = recv(&c).await;
        assert_eq!(oack, format!("\0\x06tsize\0{}\0", crate::boot::SNPONLY_EFI.len()).into_bytes());
        c.send_to(b"\0\x05\0\x08abort\0", tid).await.unwrap();
        // Path traversal, missing file, write request → ERROR packets.
        for req in [rrq("../etc/passwd", &[]), rrq("nope.bin", &[]), vec![0, 2, b'x', 0, b'o', 0]] {
            c.send_to(&req, srv).await.unwrap();
            assert_eq!(op(&recv(&c).await.0), ERROR);
        }
    }
}
