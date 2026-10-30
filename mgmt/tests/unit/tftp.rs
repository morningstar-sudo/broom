use super::*;

async fn server() -> SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    tokio::spawn(serve(sock, Ipv4Addr::LOCALHOST, String::new()));
    addr
}

async fn recv(c: &UdpSocket) -> (Vec<u8>, SocketAddr) {
    let mut b = [0u8; 1600];
    let (n, from) = tokio::time::timeout(Duration::from_secs(5), c.recv_from(&mut b))
        .await
        .unwrap()
        .unwrap();
    (b[..n].to_vec(), from)
}

fn rrq(name: &str, opts: &[(&str, &str)]) -> Vec<u8> {
    let mut p = vec![0, 1];
    for s in [name, "octet"]
        .into_iter()
        .chain(opts.iter().flat_map(|(k, v)| [*k, *v]))
    {
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
    let want = crate::boot::snponly_efi();
    let srv = server().await;
    let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    c.send_to(
        &rrq(
            "snponly.efi",
            &[("blksize", "1024"), ("tsize", "0"), ("timeout", "1")],
        ),
        srv,
    )
    .await
    .unwrap();
    let (oack, tid) = recv(&c).await;
    assert_eq!(
        oack,
        format!("\0\x06blksize\01024\0tsize\0{}\0timeout\01\0", want.len()).into_bytes()
    );
    c.send_to(&ack(0), tid).await.unwrap();
    let (b1, _) = recv(&c).await;
    assert_eq!(
        (op(&b1), &b1[2..4], b1.len()),
        (DATA, &[0, 1][..], 4 + 1024)
    );
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

/// iPXE runs autoexec.ipxe first (looked up next to its own image, then at /): it must chain THIS server's menu.
#[test]
fn autoexec_chains_the_menu() {
    let ip = Ipv4Addr::new(10, 0, 0, 12);
    for name in ["autoexec.ipxe", "/autoexec.ipxe", "sb/autoexec.ipxe"] {
        let s = String::from_utf8(load(name, ip).unwrap().into_owned()).unwrap();
        assert!(
            s.starts_with("#!ipxe\nisset ${net0/ip} && ifopen net0 || ifconf -c dhcp ||"),
            "{name}: the PXE address kept; else DHCPv4 only, no IPv6 router discovery wait"
        );
        assert!(
            s.contains("chain http://10.0.0.12/boot.ipxe?mac=${net0/mac}&ip=${net0/ip}"),
            "{name}"
        );
    }
}

#[tokio::test]
async fn embedded_ipxe_and_refusals() {
    let srv = server().await;
    let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // snponly.efi comes from the binary itself; tsize probe then abort (UEFI does this).
    c.send_to(&rrq("snponly.efi", &[("tsize", "0")]), srv)
        .await
        .unwrap();
    let (oack, tid) = recv(&c).await;
    assert_eq!(
        oack,
        format!("\0\x06tsize\0{}\0", crate::boot::snponly_efi().len()).into_bytes()
    );
    c.send_to(b"\0\x05\0\x08abort\0", tid).await.unwrap();
    // Legacy BIOS iPXE, and the official Secure Boot pair under sb/ (shim → loads sb/snponly.efi by name).
    for (name, want) in [
        ("undionly.kpxe", crate::boot::undionly_kpxe()),
        ("sb/snponly-shim.efi", crate::boot::sb_shim_efi()),
        ("sb/snponly.efi", crate::boot::sb_ipxe_efi()),
    ] {
        c.send_to(&rrq(name, &[("tsize", "0")]), srv).await.unwrap();
        let (oack, tid) = recv(&c).await;
        assert_eq!(
            oack,
            format!("\0\x06tsize\0{}\0", want.len()).into_bytes(),
            "{name}"
        );
        c.send_to(b"\0\x05\0\x08abort\0", tid).await.unwrap();
    }
    // Anything else (including a traversal attempt) → "file not found" ERROR, never a disk read.
    for req in [
        rrq("../etc/passwd", &[]),
        rrq("broom-win/x/golden.vhdx", &[]),
        rrq("nope.bin", &[]),
        rrq("sb/../x", &[]),
    ] {
        c.send_to(&req, srv).await.unwrap();
        assert_eq!(op(&recv(&c).await.0), ERROR);
    }
    // A WRQ (write) is dropped silently (no reflection): no reply within 300 ms.
    c.send_to(&[0, 2, b'x', 0, b'o', b'c', b't', b'e', b't', 0], srv)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), c.recv_from(&mut [0u8; 64]))
            .await
            .is_err()
    );
}

/// Any read request (random, or a real one with options mangled) → parsed or refused, never a panic.
#[test]
fn fuzz_rrq() {
    let seeds = vec![
        rrq("snponly.efi", &[("blksize", "1468"), ("tsize", "0")]),
        rrq("autoexec.ipxe", &[]),
    ];
    crate::fuzz::run("tftp", crate::fuzz::inputs(&seeds, 20_000), |b| {
        let _ = parse_rrq(b);
    });
}
