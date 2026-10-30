// tftp.rs — built-in read-only TFTP server: RFC 1350 + blksize (RFC 2348) + tsize/timeout (RFC 2349). Serves only
// what PXE firmware/iPXE fetch over TFTP, all from memory (see load()): the embedded iPXE builds and a generated
// autoexec.ipxe. Everything else goes over HTTP. Always on (independent of the DHCP switch). One task + socket per transfer.
use std::borrow::Cow;
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

/// Transfers at once. Each holds a socket for up to ~10 s (retries): a flood of (spoofed) RRQs must not use up the
/// process's file descriptors (HTTP and everything else would fail) → an eighth of what the open file limit allows.
/// A room booting at once stays far below it.
fn max_transfers() -> usize {
    ((crate::sysmon::fd_limit() / 8) as usize).max(64)
}

pub async fn serve(sock: UdpSocket, server: Ipv4Addr, iface: String) {
    let max = max_transfers();
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(max));
    let mut buf = [0u8; 1500];
    loop {
        let Ok((n, peer)) = sock.recv_from(&mut buf).await else {
            continue;
        };
        // Only RRQ starts a transfer; ignore stray packets on :69 before spawning a task + socket.
        if op(&buf[..n]) != RRQ {
            continue;
        }
        // All slots busy → drop the request (the client retries its RRQ).
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            tracing::debug!("tftp {peer}: {max} transfers running, request dropped");
            continue;
        };
        let req = buf[..n].to_vec();
        let iface = iface.clone();
        tokio::spawn(async move {
            let _slot = slot;
            if let Err(e) = transfer(&req, peer, server, &iface).await {
                tracing::warn!("tftp {peer}: {e}");
            }
        });
    }
}

fn op(b: &[u8]) -> u16 {
    if b.len() < 2 {
        0
    } else {
        u16::from_be_bytes([b[0], b[1]])
    }
}

/// RRQ body "file\0mode\0[opt\0value\0]*" → (file, [(opt lowercase, value)]).
fn parse_rrq(b: &[u8]) -> Option<(String, Vec<(String, String)>)> {
    let mut f = b
        .split(|&c| c == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned());
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

/// TFTP serves ONLY the embedded iPXE binaries, from memory: our own `snponly.efi` and `undionly.kpxe` (legacy BIOS),
/// and the official Secure Boot pair `sb/snponly-shim.efi` + `sb/snponly.efi` — plus `autoexec.ipxe`, which every iPXE
/// fetches from the TFTP server it was loaded from and runs first (EFI ones by themselves, undionly.kpxe through its
/// embedded bios.ipxe): it chains the boot menu, so iPXE works even when the LAN's
/// own DHCP server (router) hands out the boot file and would otherwise just give iPXE `snponly.efi` again (a loop).
/// iPXE then pulls kernel/initrd/golden over HTTP (`/tftp/...` ServeDir). Refusing everything else stops a spoofed UDP
/// packet from making the server read a multi-GB golden into RAM (no handshake on UDP → an amplification/OOM vector).
fn load(name: &str, server: Ipv4Addr) -> Result<Cow<'static, [u8]>, String> {
    let name = name.replace('\\', "/");
    match name.trim_start_matches('/') {
        "snponly.efi" => Ok(Cow::Borrowed(crate::boot::snponly_efi())),
        "undionly.kpxe" => Ok(Cow::Borrowed(crate::boot::undionly_kpxe())),
        "sb/snponly-shim.efi" => Ok(Cow::Borrowed(crate::boot::sb_shim_efi())),
        "sb/snponly.efi" => Ok(Cow::Borrowed(crate::boot::sb_ipxe_efi())),
        "autoexec.ipxe" | "sb/autoexec.ipxe" => Ok(Cow::Owned(autoexec(server).into_bytes())),
        other => Err(format!(
            "TFTP serves only the iPXE binaries (asked {other:?}); other files go over HTTP"
        )),
    }
}

/// First script of every iPXE: configure the NIC by DHCP, then the server's boot menu. `ifconf -c dhcp`, not `dhcp`:
/// that one runs every configurator, IPv6 router discovery too, which on a LAN without an IPv6 router waits out its
/// backoff (~3.9 s) before the menu. An address already there (the firmware's PXE DHCP answer, which iPXE loaded over
/// PXE takes over) is used as it is: a second DHCP waits ~2–3 s for a ProxyDHCP offer behind a router that sends none.
fn autoexec(server: Ipv4Addr) -> String {
    format!(
        "#!ipxe\nisset ${{net0/ip}} && ifopen net0 || ifconf -c dhcp || ifconf -c dhcp || shell\nchain http://{server}/boot.ipxe?mac=${{net0/mac}}&ip=${{net0/ip}} || shell\n"
    )
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
async fn send_wait(
    sock: &UdpSocket,
    peer: SocketAddr,
    pkt: &[u8],
    block: u16,
    timeout: Duration,
) -> Result<bool, String> {
    let mut buf = [0u8; 1500];
    for _ in 0..RETRIES {
        sock.send_to(pkt, peer).await.map_err(|e| e.to_string())?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await {
                Err(_) => break, // timeout → resend
                Ok(Err(e)) => return Err(e.to_string()),
                Ok(Ok((_, from))) if from != peer => {
                    send_error(sock, from, 5, "unknown transfer ID").await
                }
                Ok(Ok((n, _))) => match op(&buf[..n]) {
                    ACK if n >= 4 && u16::from_be_bytes([buf[2], buf[3]]) == block => {
                        return Ok(true);
                    }
                    ERROR => return Ok(false),
                    _ => {} // duplicate/old ACK: ignore (no resend → no Sorcerer's Apprentice)
                },
            }
        }
    }
    Err(format!("no ACK for block {block} after {RETRIES} tries"))
}

async fn transfer(
    req: &[u8],
    peer: SocketAddr,
    server: Ipv4Addr,
    iface: &str,
) -> Result<(), String> {
    let sock = match crate::dhcp::udp(SocketAddrV4::new(server, 0), iface, false) {
        Ok(s) => s,
        Err(_) => crate::dhcp::udp(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0), iface, false)
            .map_err(|e| e.to_string())?,
    };
    // serve_inner only forwards RRQ (WRQ etc. are dropped there).
    let (name, opts) = parse_rrq(&req[2..]).ok_or("malformed RRQ")?;
    let data = match load(&name, server) {
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
    tracing::info!(
        "client {} loaded {name} over TFTP ({} bytes)",
        peer.ip(),
        data.len()
    );
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/tftp.rs"]
mod tests;
