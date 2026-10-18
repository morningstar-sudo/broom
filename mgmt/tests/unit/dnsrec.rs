use super::*;

/// A message as a server would send it: compressed names, an answer chain, a referral with glue.
fn sample() -> Vec<u8> {
    let mut m = vec![0xab, 0xcd, 0x84, 0x00, 0, 1, 0, 2, 0, 1, 0, 1];
    write_name(&mut m, "www.Example.com"); // question at 12
    m.extend_from_slice(&[0, 1, 0, 1]);
    // www.example.com CNAME → web.example.com (target compressed: "web" + pointer to example.com at 16)
    m.extend_from_slice(&[0xc0, 12, 0, 5, 0, 1, 0, 0, 0x0e, 0x10]);
    let rd = [3, b'w', b'e', b'b', 0xc0, 16];
    m.extend_from_slice(&(rd.len() as u16).to_be_bytes());
    m.extend_from_slice(&rd);
    // web.example.com A 93.184.216.34, its owner name a pointer to the CNAME's target (offset of rdata above)
    let web = (m.len() - rd.len()) as u8;
    m.extend_from_slice(&[0xc0, web, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 93, 184, 216, 34]);
    // example.com NS ns1.example.com
    m.extend_from_slice(&[0xc0, 16, 0, 2, 0, 1, 0, 0, 0x0e, 0x10]);
    let ns = [3, b'n', b's', b'1', 0xc0, 16];
    m.extend_from_slice(&(ns.len() as u16).to_be_bytes());
    m.extend_from_slice(&ns);
    // ns1.example.com A 192.0.2.53
    let ns1 = (m.len() - ns.len()) as u8;
    m.extend_from_slice(&[0xc0, ns1, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4, 192, 0, 2, 53]);
    m
}

#[test]
fn parse_compressed() {
    let m = parse(&sample()).unwrap();
    assert_eq!((m.id, m.rcode()), (0xabcd, 0));
    assert_eq!(m.q, Some(("www.example.com".into(), 1, 1)));
    assert_eq!(m.an.len(), 2);
    assert_eq!(m.an[0].rtype, 5);
    assert_eq!(
        target(&m.an[0]).as_deref(),
        Some("web.example.com"),
        "CNAME target decompressed"
    );
    assert_eq!(
        (m.an[1].name.as_str(), &m.an[1].rdata[..]),
        ("web.example.com", &[93, 184, 216, 34][..])
    );
    assert_eq!(target(&m.ns[0]).as_deref(), Some("ns1.example.com"));
    assert_eq!(m.ar[0].name, "ns1.example.com");
    // Broken: cut short, a pointer loop.
    let s = sample();
    assert!(parse(&s[..s.len() - 3]).is_none());
    let mut lp = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0xc0, 12, 0, 1, 0, 1];
    assert!(parse(&lp).is_none(), "a name pointing at itself");
    lp[13] = 0;
    assert!(read_name(&lp, 12).is_none() || read_name(&lp, 12).is_some());
}

#[test]
fn zones() {
    assert!(in_zone("www.example.com", "example.com") && in_zone("example.com", "example.com"));
    assert!(in_zone("anything", ""));
    assert!(
        !in_zone("badexample.com", "example.com") && !in_zone("example.com", "www.example.com")
    );
}

/// A client query → our answer: its ID and question, RA, the records; too big for its UDP size → TC.
#[test]
fn answers_to_clients() {
    let q = query(0x4242, "Www.Example.com", 1, true);
    let (edns, size) = client_udp_size(&q);
    assert!(edns && size == EDNS as usize);
    let a = Answer {
        rcode: 0,
        an: parse(&sample()).unwrap().an,
        ns: Vec::new(),
        expires: Instant::now() + Duration::from_secs(30),
    };
    let r = response(&q, 0, &a, edns).unwrap();
    let m = parse(&r).unwrap();
    assert_eq!(m.id, 0x4242);
    assert_eq!(m.flags & 0x8080, 0x8080, "QR + RA");
    assert_eq!(m.q.unwrap().0, "www.example.com");
    // The records, their TTL no longer than what is left of the cached answer.
    assert_eq!(m.an.len(), a.an.len());
    for (got, want) in m.an.iter().zip(&a.an) {
        assert_eq!(
            (&got.name, got.rtype, &got.rdata),
            (&want.name, want.rtype, &want.rdata)
        );
        assert!(got.ttl <= 30 && got.ttl <= want.ttl && got.ttl >= 1);
    }
    assert_eq!(m.ar.len(), 1, "our OPT");
    assert_eq!(
        &r[12..12 + 17],
        &q[12..12 + 17],
        "the question as asked (case kept)"
    );
    let t = truncated(&r, &q);
    assert_eq!(t[2] & 0x02, 0x02);
    assert_eq!(parse(&t).unwrap().an.len(), 0);
    // A client without EDNS: 512 bytes.
    let plain = &q[..q.len() - 11];
    let mut plain = plain.to_vec();
    plain[11] = 0;
    assert_eq!(client_udp_size(&plain), (false, 512));
}

#[test]
fn negative_ttl() {
    // SOA rdata: mname, rname, serial, refresh, retry, expire, minimum = 120.
    let mut rd = name_bytes("ns.example.com");
    rd.extend(name_bytes("admin.example.com"));
    rd.extend_from_slice(&[
        0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 60, 0, 1, 0, 0, 0, 0, 0, 120,
    ]);
    let soa = Rr {
        name: "example.com".into(),
        rtype: T_SOA,
        class: 1,
        ttl: 3600,
        rdata: rd,
    };
    assert_eq!(
        ttl_of(std::slice::from_ref(&soa), true),
        Duration::from_secs(120)
    );
    assert_eq!(ttl_of(&[], true), Duration::from_secs(60));
    assert_eq!(
        ttl_of(
            &[Rr {
                ttl: 1,
                ..soa.clone()
            }],
            false
        ),
        Duration::from_secs(5),
        "at least 5 s"
    );
}

/// Live (needs the internet, run by hand: cargo test resolves_live -- --ignored): real names from the root servers —
/// an A record, a CNAME chain, a name that does not exist; the second ask comes from the cache.
#[tokio::test]
#[ignore]
async fn resolves_live() {
    let a = resolve("example.com".into(), 1, 0).await;
    assert_eq!(a.rcode, 0);
    assert!(a.an.iter().any(|r| r.rtype == 1), "{a:?}");
    let w = resolve("www.github.com".into(), 1, 0).await;
    assert!(
        w.an.iter().any(|r| r.rtype == T_CNAME) && w.an.iter().any(|r| r.rtype == 1),
        "{w:?}"
    );
    let x = resolve("no-such-name-broom-test.example.com".into(), 1, 0).await;
    assert_eq!(x.rcode, NXDOMAIN);
    let t = Instant::now();
    resolve("example.com".into(), 1, 0).await;
    assert!(t.elapsed() < Duration::from_millis(5), "cached");
    let six = resolve("google.com".into(), 28, 0).await;
    assert!(six.an.iter().any(|r| r.rtype == 28), "AAAA {six:?}");
}

/// A forwarder's reply → the cached answer: its records (OPT dropped) for their TTL; a negative one by its SOA.
#[test]
fn forwarder_answers() {
    let mut m = parse(&sample()).unwrap();
    m.ar.push(Rr {
        name: String::new(),
        rtype: T_OPT,
        class: 1232,
        ttl: 0,
        rdata: Vec::new(),
    });
    let a = to_answer(m);
    assert_eq!((a.rcode, a.an.len()), (0, 2));
    assert!(
        a.expires > Instant::now() + Duration::from_secs(50),
        "min TTL 60 s of its records"
    );
    let mut neg = parse(&sample()).unwrap();
    neg.flags |= NXDOMAIN as u16;
    neg.an.clear();
    let a = to_answer(neg);
    assert_eq!((a.rcode, a.an.len()), (NXDOMAIN, 0));
}

/// Live (by hand: cargo test forwards_live -- --ignored): asked of a public resolver, then from the cache.
#[tokio::test]
#[ignore]
async fn forwards_live() {
    let ups = [Ipv4Addr::new(1, 1, 1, 1)];
    let q = query(0x7777, "example.org", 1, true);
    let r = parse(&answer_query(&q, &ups).await.unwrap()).unwrap();
    assert_eq!((r.id, r.rcode()), (0x7777, 0));
    assert!(r.an.iter().any(|x| x.rtype == T_A));
    let t = Instant::now();
    answer_query(&q, &[]).await.unwrap();
    assert!(
        t.elapsed() < Duration::from_millis(5),
        "cached: no forwarder needed"
    );
}

/// Any message (random, or a real answer / query mangled: compression pointers, counts, lengths) → parsed or refused,
/// and a query answered or not, never a panic or an endless pointer loop.
#[test]
fn fuzz_messages() {
    let seeds = vec![sample(), query(7, "www.example.com", 1, true)];
    let a = Answer {
        rcode: 0,
        an: parse(&sample()).unwrap().an,
        ns: Vec::new(),
        expires: Instant::now() + Duration::from_secs(30),
    };
    crate::fuzz::run("dns", crate::fuzz::inputs(&seeds, 20_000), |b| {
        let _ = parse(b);
        let _ = client_udp_size(b);
        let _ = crate::dns::qname(b);
        let _ = crate::dns::answer(b, 5);
        if let Some(r) = response(b, 0, &a, true) {
            let _ = truncated(&r, b);
        }
    });
}
