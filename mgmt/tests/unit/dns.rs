use super::*;

/// A query for `name` (type A, class IN), ID 0x1234, RD set.
fn query(name: &str) -> Vec<u8> {
    let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for l in name.split('.') {
        q.push(l.len() as u8);
        q.extend_from_slice(l.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    q
}

#[test]
fn packets() {
    let q = query("Example.COM");
    assert_eq!(qname(&q).as_deref(), Some("example.com"));
    let r = answer(&q, 5).unwrap();
    assert_eq!(&r[..2], &[0x12, 0x34], "same ID");
    assert_eq!(r[2], 0x81, "a response, RD kept");
    assert_eq!(r[3] & 0x0f, 5, "REFUSED");
    assert_eq!(
        &r[4..12],
        &[0, 1, 0, 0, 0, 0, 0, 0],
        "the question, no records"
    );
    assert_eq!(&r[12..], &q[12..], "question copied");
    assert_eq!(answer(&q, 3).unwrap()[3] & 0x0f, 3, "NXDOMAIN");
    // Malformed: too short, no question, a label running past the end, extra bytes after the question are dropped.
    assert!(qname(&q[..10]).is_none() && answer(&q[..10], 5).is_none());
    let mut none = q.clone();
    none[5] = 0;
    assert!(qname(&none).is_none());
    assert!(qname(&q[..q.len() - 3]).is_none());
    let mut extra = q.clone();
    extra.extend_from_slice(&[0; 11]); // an EDNS record
    assert_eq!(answer(&extra, 5).unwrap().len(), q.len());
}

#[test]
fn ranges() {
    let n = parse_nets("10.0.0.5\n192.168.1.0/24, 172.16.0.0/12\n\n").unwrap();
    assert_eq!(n.len(), 3);
    let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
    assert!(
        cidr_contains(n[0].0, n[0].1, ip("10.0.0.5"))
            && !cidr_contains(n[0].0, n[0].1, ip("10.0.0.6"))
    );
    assert!(
        cidr_contains(n[1].0, n[1].1, ip("192.168.1.200"))
            && !cidr_contains(n[1].0, n[1].1, ip("192.168.2.1"))
    );
    assert!(cidr_contains(n[2].0, n[2].1, ip("172.31.255.1")));
    assert!(
        cidr_contains(ip("0.0.0.0"), 0, ip("8.8.8.8")),
        "/0 = everything"
    );
    assert!(
        parse_nets("10.0.0.300").is_err()
            && parse_nets("10.0.0.0/33").is_err()
            && parse_nets("x").is_err()
    );
    assert!(parse_nets("").unwrap().is_empty());
}

/// Healthy: its agent connected, an exempt group, or a PXE boot within the grace time.
#[test]
fn who_may_resolve() {
    let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
    let c = |i: &str, agent, exempt_group, pxe| Client {
        ips: vec![ip(i)],
        agent,
        exempt_group,
        pxe,
    };
    let now = 10_000;
    let set = allowed_ips(
        &[
            c("10.0.0.1", true, false, None),
            c("10.0.0.2", false, true, None),
            c("10.0.0.3", false, false, Some(now - 200)),
            c("10.0.0.4", false, false, Some(now - 400)),
            c("10.0.0.5", false, false, None),
        ],
        300,
        now,
    );
    assert!(set.contains(&ip("10.0.0.1")), "agent connected");
    assert!(set.contains(&ip("10.0.0.2")), "exempt group");
    assert!(
        set.contains(&ip("10.0.0.3")),
        "PXE boot 200 s ago, grace 300 s"
    );
    assert!(!set.contains(&ip("10.0.0.4")), "grace over, no agent");
    assert!(!set.contains(&ip("10.0.0.5")), "nothing");
}

/// A query from a client: refused unless allowed; the Firefox canary answers NXDOMAIN; the rest is forwarded.
#[tokio::test]
async fn replies() {
    let ok = "10.77.0.1".parse::<Ipv4Addr>().unwrap();
    let other = "10.77.0.2".parse::<Ipv4Addr>().unwrap();
    {
        let mut a = ALLOWED.lock();
        a.ips.insert(ok);
        a.nets.push(("10.78.0.0".parse().unwrap(), 16));
    }
    let fwd = |q: Vec<u8>| async move {
        let mut r = q;
        r[2] |= 0x80;
        Some(r)
    };
    let q = query("example.com");
    let r = reply(&q, ok, fwd).await.unwrap();
    assert_eq!((r[2] & 0x80, r[3] & 0x0f), (0x80, 0), "forwarded");
    let r = reply(&q, other, fwd).await.unwrap();
    assert_eq!(r[3] & 0x0f, 5, "not allowed: REFUSED");
    let r = reply(&q, "10.78.3.4".parse().unwrap(), fwd).await.unwrap();
    assert_eq!(r[3] & 0x0f, 0, "inside an exempt range");
    let r = reply(&query("use-application-dns.net"), ok, fwd)
        .await
        .unwrap();
    assert_eq!(r[3] & 0x0f, 3, "Firefox canary: NXDOMAIN");
    assert!(
        reply(&q[..8], ok, fwd).await.is_none(),
        "malformed: no answer"
    );
}
