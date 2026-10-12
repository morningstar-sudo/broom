use super::*;

const MAC: [u8; 6] = [0x34, 0x5a, 0x60, 0x7b, 0x2b, 0x1d];

fn cfg() -> Cfg {
    Cfg {
        iface: String::new(),
        server: Ipv4Addr::new(10, 0, 0, 12),
        start: Ipv4Addr::new(10, 0, 0, 100),
        end: Ipv4Addr::new(10, 0, 0, 102),
        netmask: Ipv4Addr::new(255, 255, 255, 0),
        gateway: Some(Ipv4Addr::new(10, 0, 0, 1)),
        dns: vec![Ipv4Addr::new(1, 1, 1, 1)],
        lease_s: 3600,
        sb: false,
        opts: DhcpOpts::default(),
    }
}

fn store() -> Store {
    Store {
        bindings: HashMap::new(),
        leases: HashMap::new(),
        now: 1000,
    }
}

/// UEFI x64 PXE firmware request (option 60 PXEClient, 93 = 7, 97 GUID).
fn req(mt: u8, extra: &[(u8, &[u8])]) -> Packet {
    let mut chaddr = [0u8; 16];
    chaddr[..6].copy_from_slice(&MAC);
    let mut p = Packet {
        op: 1,
        htype: 1,
        hlen: 6,
        xid: [1, 2, 3, 4],
        flags: [0x80, 0],
        ciaddr: Ipv4Addr::UNSPECIFIED,
        yiaddr: Ipv4Addr::UNSPECIFIED,
        siaddr: Ipv4Addr::UNSPECIFIED,
        giaddr: Ipv4Addr::UNSPECIFIED,
        chaddr,
        file: String::new(),
        opts: vec![],
    };
    p.push(53, [mt]);
    p.push(60, *b"PXEClient:Arch:00007:UNDI:003016");
    p.push(93, [0, 7]);
    p.push(97, [0; 17]);
    for (c, v) in extra {
        p.push(*c, v.to_vec());
    }
    p
}

const MACS: &str = "34:5a:60:7b:2b:1d";

#[test]
fn roundtrip() {
    let mut p = req(DISCOVER, &[(12, b"pc01")]);
    p.file = "snponly.efi".into();
    let b = build(&p);
    assert!(b.len() >= 300);
    assert_eq!(parse(&b).unwrap(), p);
    assert!(parse(&b[..239]).is_none());
}

#[test]
fn full_offer_first_free_ip_and_pxe_bootfile() {
    let out = handle(&req(DISCOVER, &[]), &cfg(), &store());
    let (p, dest) = out.reply.unwrap();
    assert_eq!(dest, Dest::Broadcast);
    assert_eq!(p.opt(53), Some(&[OFFER][..]));
    assert_eq!(p.yiaddr, Ipv4Addr::new(10, 0, 0, 100));
    assert_eq!(
        (p.siaddr, p.file.as_str()),
        (Ipv4Addr::new(10, 0, 0, 12), "snponly.efi")
    );
    assert_eq!(p.opt(3), Some(&[10, 0, 0, 1][..]));
    assert_eq!(p.opt(51), Some(&3600u32.to_be_bytes()[..]));
    assert!(matches!(
        out.lease,
        LeaseOp::Set {
            expires: 1060,
            source: "full",
            ..
        }
    ));
}

#[test]
fn secure_boot_switch_picks_signed_ipxe() {
    let sb = Cfg { sb: true, ..cfg() };
    let (p, _) = handle(&req(DISCOVER, &[]), &sb, &store()).reply.unwrap();
    assert_eq!(
        p.file, "sb/snponly-shim.efi",
        "UEFI PXE → Microsoft-signed iPXE shim"
    );
    // iPXE itself (option 175) still gets the menu URL, whichever iPXE it is.
    let (p, _) = handle(&req(DISCOVER, &[(175, &[1])]), &sb, &store())
        .reply
        .unwrap();
    assert!(p.file.starts_with("http://10.0.0.12/boot.ipxe"));
}

#[test]
fn ipxe_gets_menu_url() {
    let out = handle(&req(DISCOVER, &[(175, &[1])]), &cfg(), &store());
    let p = out.reply.unwrap().0;
    assert_eq!(
        p.file,
        "http://10.0.0.12/boot.ipxe?mac=${net0/mac}&ip=${net0/ip}"
    );
    assert_eq!(p.opt(175), Some(&[0xb0, 1, 1][..])); // no-pxedhcp: don't wait for ProxyDHCP
    let plain = handle(&req(DISCOVER, &[]), &cfg(), &store())
        .reply
        .unwrap()
        .0;
    assert_eq!(plain.opt(175), None);
}

#[test]
fn binding_wins_and_sends_hostname() {
    let mut st = store();
    st.bindings.insert(
        MACS.into(),
        (Ipv4Addr::new(10, 0, 0, 50), Some("PC01".into())),
    );
    let (p, _) = handle(&req(DISCOVER, &[]), &cfg(), &st).reply.unwrap();
    assert_eq!(p.yiaddr, Ipv4Addr::new(10, 0, 0, 50));
    assert_eq!(p.opt(12), Some(&b"PC01"[..]));
}

#[test]
fn skips_ips_in_use() {
    let mut st = store();
    st.leases.insert(
        "aa:aa:aa:aa:aa:aa".into(),
        (Ipv4Addr::new(10, 0, 0, 100), 2000),
    ); // active
    st.bindings.insert(
        "bb:bb:bb:bb:bb:bb".into(),
        (Ipv4Addr::new(10, 0, 0, 101), None),
    );
    let (p, _) = handle(&req(DISCOVER, &[]), &cfg(), &st).reply.unwrap();
    assert_eq!(p.yiaddr, Ipv4Addr::new(10, 0, 0, 102));
    // Expired lease of another machine → its IP is free again.
    st.leases.insert(
        "aa:aa:aa:aa:aa:aa".into(),
        (Ipv4Addr::new(10, 0, 0, 100), 500),
    );
    let (p, _) = handle(&req(DISCOVER, &[]), &cfg(), &st).reply.unwrap();
    assert_eq!(p.yiaddr, Ipv4Addr::new(10, 0, 0, 100));
    // Pool full → no offer.
    let mut st = store();
    for (i, m) in ["a", "b", "c"].iter().enumerate() {
        st.leases.insert(
            m.to_string(),
            (Ipv4Addr::new(10, 0, 0, 100 + i as u8), 2000),
        );
    }
    assert!(handle(&req(DISCOVER, &[]), &cfg(), &st).reply.is_none());
}

#[test]
fn decline_release_discover_validated() {
    let ip = Ipv4Addr::new(10, 0, 0, 101); // in the 100..=102 pool
    let held = || {
        let mut st = store();
        st.leases.insert(MACS.into(), (ip, 5000));
        st
    };
    // DECLINE only when this MAC holds the in-range IP (and opt 54 is us or absent).
    assert!(matches!(
        handle(&req(DECLINE, &[(50, &ip.octets())]), &cfg(), &store()).lease,
        LeaseOp::None
    ));
    assert!(matches!(
        handle(&req(DECLINE, &[(50, &ip.octets())]), &cfg(), &held()).lease,
        LeaseOp::Set { .. }
    ));
    let outrange = Ipv4Addr::new(192, 168, 1, 1);
    let mut st = store();
    st.leases.insert(MACS.into(), (outrange, 5000));
    assert!(matches!(
        handle(&req(DECLINE, &[(50, &outrange.octets())]), &cfg(), &st).lease,
        LeaseOp::None
    ));
    // RELEASE only when ciaddr == the held IP.
    let mut r = req(RELEASE, &[]);
    r.ciaddr = ip;
    assert!(matches!(
        handle(&r, &cfg(), &held()).lease,
        LeaseOp::Remove(_)
    ));
    let mut r2 = req(RELEASE, &[]);
    r2.ciaddr = Ipv4Addr::new(10, 0, 0, 200);
    assert!(matches!(handle(&r2, &cfg(), &held()).lease, LeaseOp::None));
    // DISCOVER must not shorten a longer lease this MAC already has.
    let mut st = store();
    st.leases.insert(MACS.into(), (ip, 9000));
    assert!(matches!(
        handle(&req(DISCOVER, &[]), &cfg(), &st).lease,
        LeaseOp::Set { expires: 9000, .. }
    ));
}

#[test]
fn request_ack_nak_and_other_server() {
    let ok = handle(&req(REQUEST, &[(50, &[10, 0, 0, 101])]), &cfg(), &store());
    let (p, _) = ok.reply.unwrap();
    assert_eq!(
        (p.opt(53), p.yiaddr),
        (Some(&[ACK][..]), Ipv4Addr::new(10, 0, 0, 101))
    );
    assert!(matches!(ok.lease, LeaseOp::Set { expires: 4600, .. }));
    // A client we know (it has a lease here) asking for a wrong IP → NAK.
    let mut known = store();
    known
        .leases
        .insert(MACS.into(), (Ipv4Addr::new(10, 0, 0, 100), 5000));
    let bad = handle(&req(REQUEST, &[(50, &[192, 168, 1, 5])]), &cfg(), &known);
    assert_eq!(bad.reply.unwrap().0.opt(53), Some(&[NAK][..]));
    let other = handle(
        &req(REQUEST, &[(50, &[10, 0, 0, 101]), (54, &[10, 0, 0, 9])]),
        &cfg(),
        &store(),
    );
    assert!(other.reply.is_none());
}

/// A REQUEST without option 54 for an IP outside our pool, from a MAC we have no record of = a client renewing
/// with ANOTHER DHCP server → never NAK it off the LAN.
#[test]
fn foreign_client_is_not_naked() {
    let out = handle(&req(REQUEST, &[(50, &[192, 168, 1, 5])]), &cfg(), &store());
    assert!(out.reply.is_none() && matches!(out.lease, LeaseOp::None));
    let mut renewing = req(REQUEST, &[]);
    renewing.ciaddr = Ipv4Addr::new(192, 168, 1, 5);
    assert!(handle(&renewing, &cfg(), &store()).reply.is_none());
}

/// Static IP still leased to another MAC → the bound machine gets a pool IP for now (warned), not a duplicate.
#[test]
fn binding_waits_for_live_lease() {
    let mut st = store();
    let bound = Ipv4Addr::new(10, 0, 0, 50);
    st.bindings
        .insert(MACS.into(), (bound, Some("PC01".into())));
    st.leases.insert("aa:aa:aa:aa:aa:aa".into(), (bound, 2000)); // live
    let out = handle(&req(DISCOVER, &[]), &cfg(), &st);
    assert_eq!(out.reply.unwrap().0.yiaddr, Ipv4Addr::new(10, 0, 0, 100));
    assert!(out.warn && out.log.contains("still leased to aa:aa:aa:aa:aa:aa"));
    assert!(
        !allowed(MACS, bound, &cfg(), &st),
        "no ACK for the held static IP"
    );
    st.leases.insert("aa:aa:aa:aa:aa:aa".into(), (bound, 500)); // expired
    assert_eq!(
        handle(&req(DISCOVER, &[]), &cfg(), &st)
            .reply
            .unwrap()
            .0
            .yiaddr,
        bound
    );
    assert!(
        !allowed(MACS, Ipv4Addr::new(10, 0, 0, 100), &cfg(), &st),
        "once free: only the binding"
    );
}

#[test]
fn decline_of_static_ip_is_logged_not_parked() {
    let mut st = store();
    let bound = Ipv4Addr::new(10, 0, 0, 50);
    st.bindings
        .insert(MACS.into(), (bound, Some("PC01".into())));
    let out = handle(&req(DECLINE, &[(50, &bound.octets())]), &cfg(), &st);
    assert!(
        out.warn
            && out.log.contains("PC01 declined 10.0.0.50")
            && matches!(out.lease, LeaseOp::None)
    );
}

/// Network-page checkboxes: each optional behaviour on/off.
#[test]
fn optional_behaviours() {
    let with = |f: &dyn Fn(&mut DhcpOpts)| {
        let mut c = cfg();
        f(&mut c.opts);
        c
    };
    // Rapid Commit: only when enabled AND the client asks (option 80) → ACK + option 80 + full lease.
    let rc = with(&|o| o.rapid_commit = true);
    let out = handle(&req(DISCOVER, &[(80, &[])]), &rc, &store());
    let p = out.reply.unwrap().0;
    assert_eq!((p.opt(53), p.opt(80)), (Some(&[ACK][..]), Some(&[][..])));
    assert!(
        matches!(out.lease, LeaseOp::Set { expires: 4600, .. }) && out.log.contains("rapid commit")
    );
    assert_eq!(
        handle(&req(DISCOVER, &[]), &rc, &store())
            .reply
            .unwrap()
            .0
            .opt(53),
        Some(&[OFFER][..]),
        "client didn't ask"
    );
    assert_eq!(
        handle(&req(DISCOVER, &[(80, &[])]), &cfg(), &store())
            .reply
            .unwrap()
            .0
            .opt(53),
        Some(&[OFFER][..]),
        "off by default"
    );
    // iPXE fast off → no option 175.
    let ipxe = req(DISCOVER, &[(175, &[1])]);
    assert!(
        handle(&ipxe, &with(&|o| o.ipxe_fast = false), &store())
            .reply
            .unwrap()
            .0
            .opt(175)
            .is_none()
    );
    // Hostname off → no option 12 even for a bound machine.
    let mut bound = store();
    bound.bindings.insert(
        MACS.into(),
        (Ipv4Addr::new(10, 0, 0, 50), Some("PC01".into())),
    );
    assert!(
        handle(
            &req(DISCOVER, &[]),
            &with(&|o| o.send_hostname = false),
            &bound
        )
        .reply
        .unwrap()
        .0
        .opt(12)
        .is_none()
    );
    // Not authoritative → a known client asking for a wrong IP gets no NAK.
    let mut known = store();
    known
        .leases
        .insert(MACS.into(), (Ipv4Addr::new(10, 0, 0, 100), 5000));
    let quiet = with(&|o| o.authoritative = false);
    assert!(
        handle(&req(REQUEST, &[(50, &[192, 168, 1, 5])]), &quiet, &known)
            .reply
            .is_none()
    );
}

#[test]
fn relayed_packets_ignored() {
    let mut r = req(DISCOVER, &[]);
    r.giaddr = Ipv4Addr::new(10, 9, 0, 1);
    let out = handle(&r, &cfg(), &store());
    assert!(out.reply.is_none() && matches!(out.lease, LeaseOp::None));
}

#[test]
fn lease_parse() {
    assert_eq!(
        (
            lease_secs("12h"),
            lease_secs("30m"),
            lease_secs("1d"),
            lease_secs("600")
        ),
        (43200, 1800, 86400, 600)
    );
    assert_eq!(
        (
            lease_secs("junk"),
            lease_secs("90s"),
            parse_lease("0"),
            parse_lease("5x")
        ),
        (43200, 90, None, None)
    );
}
