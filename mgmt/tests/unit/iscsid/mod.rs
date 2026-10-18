use super::*;
use tokio::io::AsyncWriteExt;

fn pdu(op: u8, flags: u8, itt: u32, cmdsn: u32, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 48];
    b[0] = op;
    b[1] = flags;
    b[5..8].copy_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
    b[8..14].copy_from_slice(&[0x80, 0, 0, 0x12, 0x34, 0x56]);
    b[16..20].copy_from_slice(&itt.to_be_bytes());
    b[24..28].copy_from_slice(&cmdsn.to_be_bytes());
    b.extend(data);
    b.resize(b.len().div_ceil(4) * 4, 0);
    b
}

async fn login(s: &mut tokio::net::TcpStream, target: &str) -> conn::Pdu {
    let keys = format!(
        "InitiatorName=iqn.test:pc-{target}\0TargetName={target}\0SessionType=Normal\0MaxRecvDataSegmentLength=4096\0MaxBurstLength=65536\0"
    );
    s.write_all(&pdu(0x43, 0x80 | 1 << 2 | 3, 1, 0, keys.as_bytes()))
        .await
        .unwrap();
    conn::read_pdu(s).await.unwrap()
}

/// Login + three READs sent back to back on one connection (answered concurrently), checked byte for byte; the
/// session is counted while connected; an unknown target is refused.
#[tokio::test]
async fn serves_pipelined_reads() {
    let p = std::env::temp_dir().join("broom_test_iscsid.img");
    let file: Vec<u8> = (0..1u32 << 20)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    std::fs::write(&p, &file).unwrap();
    let iqn = "iqn.test:img.g1";
    export(Entry {
        iqn: iqn.into(),
        path: p.to_string_lossy().into(),
        ram: false,
        reserve: 0,
        stamp: String::new(),
    })
    .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            tokio::spawn(conn::serve(s, "127.0.0.1".into()));
        }
    });

    let mut bad = tokio::net::TcpStream::connect(addr).await.unwrap();
    let r = login(&mut bad, "iqn.test:nope").await;
    assert_eq!(
        (r.bhs[0], r.bhs[36], r.bhs[37]),
        (0x23, 0x02, 0x03),
        "target not found"
    );

    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let r = login(&mut s, iqn).await;
    assert_eq!(
        (r.bhs[0], r.bhs[1] & 0x83, r.bhs[36]),
        (0x23, 0x83, 0),
        "login → full feature"
    );
    assert_ne!(&r.bhs[14..16], &[0, 0], "TSIH given");
    assert!(String::from_utf8_lossy(&r.data).contains("TargetPortalGroupTag=1"));
    assert_eq!(list().iter().find(|t| t.iqn == iqn).unwrap().sessions, 1);

    let reads = [(10u32, 0u32, 8u16), (11, 100, 16), (12, 2040, 8)]; // (itt, lba, blocks)
    for (i, (itt, lba, blocks)) in reads.iter().enumerate() {
        let mut cmd = pdu(0x01, 0xc0, *itt, i as u32 + 1, &[]);
        cmd[8..16].fill(0); // LUN 0 (the helper puts an ISID there, for logins)
        cmd[20..24].copy_from_slice(&(*blocks as u32 * 512).to_be_bytes());
        cmd[32] = 0x28;
        cmd[34..38].copy_from_slice(&lba.to_be_bytes());
        cmd[39..41].copy_from_slice(&blocks.to_be_bytes());
        s.write_all(&cmd).await.unwrap();
    }
    let mut got: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut done = 0;
    while done < reads.len() {
        let r = conn::read_pdu(&mut s).await.unwrap();
        assert_eq!(r.bhs[0], 0x25, "Data-In");
        assert!(
            r.data.len() <= 4096,
            "split to the initiator's segment size"
        );
        let itt = u32::from_be_bytes(r.bhs[16..20].try_into().unwrap());
        let at = u32::from_be_bytes(r.bhs[40..44].try_into().unwrap()) as usize;
        let buf = got.entry(itt).or_default();
        buf.resize(buf.len().max(at + r.data.len()), 0);
        buf[at..at + r.data.len()].copy_from_slice(&r.data);
        if r.bhs[1] & 0x01 != 0 {
            assert_eq!(r.bhs[3], 0, "GOOD");
            done += 1;
        }
    }
    for (itt, lba, blocks) in reads {
        let off = lba as usize * 512;
        assert_eq!(
            got[&itt],
            &file[off..off + blocks as usize * 512],
            "ITT {itt}"
        );
    }
    drop(s);
    for _ in 0..50 {
        if list().iter().find(|t| t.iqn == iqn).unwrap().sessions == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        list().iter().find(|t| t.iqn == iqn).unwrap().sessions,
        0,
        "session freed on disconnect"
    );
    TARGETS.lock().remove(iqn);
    let _ = std::fs::remove_file(p);
}

fn scsi_cmd(itt: u32, cmdsn: u32, flags: u8, op: u8, lba: u32, blocks: u16) -> Vec<u8> {
    let mut c = pdu(0x01, flags, itt, cmdsn, &[]);
    c[8..16].fill(0);
    c[20..24].copy_from_slice(&(blocks as u32 * 512).to_be_bytes());
    c[32] = op;
    c[34..38].copy_from_slice(&lba.to_be_bytes());
    c[39..41].copy_from_slice(&blocks.to_be_bytes());
    c
}

async fn read_blocks(
    s: &mut tokio::net::TcpStream,
    itt: u32,
    cmdsn: u32,
    lba: u32,
    blocks: u16,
) -> Vec<u8> {
    s.write_all(&scsi_cmd(itt, cmdsn, 0xc0, 0x28, lba, blocks))
        .await
        .unwrap();
    let mut buf = vec![0u8; blocks as usize * 512];
    loop {
        let r = conn::read_pdu(s).await.unwrap();
        assert_eq!(r.bhs[0], 0x25, "Data-In");
        let at = u32::from_be_bytes(r.bhs[40..44].try_into().unwrap()) as usize;
        buf[at..at + r.data.len()].copy_from_slice(&r.data);
        if r.bhs[1] & 0x01 != 0 {
            return buf;
        }
    }
}

fn info_of(name: &str) -> games::Info {
    games::info().into_iter().find(|i| i.name == name).unwrap()
}

async fn wait_games(f: impl Fn(&games::Info) -> bool) {
    for _ in 0..100 {
        if f(&info_of("t")) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("games disk state not reached: {:?}", games::info());
}

/// Games disk: the update machine's WRITE comes in over two R2T bursts, lands in the update overlay and reads back;
/// a read-only session is refused writes. "save" makes version g1 at once: a new session of g1 reads the update while
/// a session still on g0 keeps reading the old content; once g0 is unused the layer is merged into games.img and
/// g0 is no longer served.
#[tokio::test]
async fn games_disk_versions() {
    let d = std::env::temp_dir().join("broom_test_games");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let (img, img2) = (d.join("t/games.img"), d.join("b/games.img"));
    std::fs::create_dir_all(img.parent().unwrap()).unwrap();
    std::fs::create_dir_all(img2.parent().unwrap()).unwrap();
    std::fs::write(&img, vec![0x11u8; 1 << 20]).unwrap();
    let (iqn, g0, g1) = ("iqn.test:games", "iqn.test:games.g0", "iqn.test:games.g1");
    std::fs::write(&img2, vec![0x22u8; 1 << 20]).unwrap();
    let other = games::Config {
        name: "b".into(),
        iqn: "iqn.test:games-b".into(),
        path: img2.to_string_lossy().into(),
        update_ip: None,
        ..Default::default()
    };
    let cfg = |ip: Option<&str>, ram: bool| {
        vec![
            games::Config {
                name: "t".into(),
                iqn: iqn.into(),
                path: img.to_string_lossy().into(),
                update_ip: ip.map(Into::into),
                ram,
                ..Default::default()
            },
            other.clone(),
        ]
    };
    games::set(cfg(Some("127.0.0.1"), false)).unwrap();
    let mut all = games::iqns();
    all.sort();
    assert_eq!(
        all,
        ["iqn.test:games-b.g0", g0],
        "every disk served, each with its own versions"
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            tokio::spawn(conn::serve(s, "127.0.0.1".into()));
        }
    });

    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    assert_eq!(
        login(&mut s, g0).await.bhs[36],
        0,
        "update machine logged in"
    );
    let data: Vec<u8> = (0..128 << 10).map(|i: u32| (i % 253) as u8).collect();
    s.write_all(&scsi_cmd(20, 1, 0xa0, 0x2a, 8, 256))
        .await
        .unwrap(); // WRITE(10), F + W
    for burst in 0..2u32 {
        let r = conn::read_pdu(&mut s).await.unwrap();
        assert_eq!(r.bhs[0], 0x31, "R2T");
        let g = |at: usize| u32::from_be_bytes(r.bhs[at..at + 4].try_into().unwrap());
        assert_eq!(
            (g(36), g(40), g(44)),
            (burst, burst * 65536, 65536),
            "R2TSN, offset, length"
        );
        for (i, chunk) in data[g(40) as usize..(g(40) + g(44)) as usize]
            .chunks(32768)
            .enumerate()
        {
            let mut p = pdu(0x05, if i == 1 { 0x80 } else { 0 }, 20, 0, chunk);
            p[8..16].fill(0);
            p[20..24].copy_from_slice(&r.bhs[20..24]); // TTT
            p[36..40].copy_from_slice(&(i as u32).to_be_bytes());
            p[40..44].copy_from_slice(&(g(40) + i as u32 * 32768).to_be_bytes());
            s.write_all(&p).await.unwrap();
        }
    }
    let r = conn::read_pdu(&mut s).await.unwrap();
    assert_eq!(
        (r.bhs[0], r.bhs[1] & 0x06, r.bhs[3]),
        (0x21, 0, 0),
        "GOOD, no residual"
    );
    assert!(
        read_blocks(&mut s, 21, 2, 8, 256).await == data,
        "reads its own write"
    );
    assert!(
        read_blocks(&mut s, 22, 3, 0, 8)
            .await
            .iter()
            .all(|&b| b == 0x11),
        "rest from the disk"
    );
    assert!(games::save("t").is_err(), "update machine still connected");
    drop(s);

    games::set(cfg(None, false)).unwrap(); // update mode off: every session read-only
    let mut old = tokio::net::TcpStream::connect(addr).await.unwrap();
    login(&mut old, g0).await;
    assert!(
        read_blocks(&mut old, 30, 1, 8, 8)
            .await
            .iter()
            .all(|&b| b == 0x11),
        "g0 unchanged"
    );
    old.write_all(&scsi_cmd(31, 2, 0xa0, 0x2a, 8, 1))
        .await
        .unwrap();
    let r = conn::read_pdu(&mut old).await.unwrap();
    assert_eq!((r.bhs[0], r.bhs[3]), (0x21, 0x02), "write refused, no R2T");

    wait_games(|i| !i.update_connected).await;
    assert!(games::save("t").unwrap().starts_with("version g1 ready"));
    assert!(games::iqns().iter().any(|i| i == g1), "new boots get g1");
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    assert_eq!(login(&mut s, g1).await.bhs[36], 0);
    assert!(
        read_blocks(&mut s, 40, 1, 8, 256).await == data,
        "g1 = the update"
    );
    assert!(
        read_blocks(&mut old, 32, 3, 8, 8)
            .await
            .iter()
            .all(|&b| b == 0x11),
        "a client on g0 keeps g0"
    );

    games::merge_if_idle("t", std::time::Duration::ZERO);
    assert_eq!(info_of("t").layers, 1, "not merged while g0 is in use");
    drop(old);
    wait_games(|i| i.old_sessions == 0).await;
    // RAM option on: the copy is loaded under the layer (games.img still holds the old bytes there).
    games::set(cfg(None, true)).unwrap();
    wait_games(|i| i.ram).await;
    games::merge_if_idle("t", std::time::Duration::ZERO);
    let i = info_of("t");
    assert_eq!((i.ver, i.layers, i.merging), (1, 0, false), "merged");
    let disk = std::fs::read(&img).unwrap();
    assert!(
        disk[8 * 512..8 * 512 + data.len()] == data[..]
            && disk[..8 * 512].iter().all(|&b| b == 0x11)
    );
    assert!(
        read_blocks(&mut s, 41, 2, 8, 256).await == data,
        "g1 session unaffected by the merge"
    );
    drop(s); // (a new login below from the same test initiator would replace it)
    // (Loaded again on a thread already: a 1 MB copy may be back before this line, so only the bytes are checked.)
    let mut fresh = tokio::net::TcpStream::connect(addr).await.unwrap();
    login(&mut fresh, g1).await;
    assert!(
        read_blocks(&mut fresh, 50, 1, 8, 256).await == data,
        "a new session right after the merge reads the merged bytes"
    );
    wait_games(|i| i.ram).await;
    let mut fresh2 = tokio::net::TcpStream::connect(addr).await.unwrap();
    login(&mut fresh2, g1).await;
    assert!(
        read_blocks(&mut fresh2, 60, 1, 8, 256).await == data,
        "and from the RAM copy loaded again"
    );
    drop((fresh, fresh2));
    games::set(cfg(None, false)).unwrap();
    assert!(!info_of("t").ram, "RAM option off → the file");
    let mut late = tokio::net::TcpStream::connect(addr).await.unwrap();
    assert_eq!(
        login(&mut late, g0).await.bhs[36],
        0x02,
        "g0 no longer served"
    );
    games::set(Vec::new()).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn requests_roundtrip() {
    let e = Entry {
        iqn: "iqn.x:a.g1".into(),
        path: "/i/a/image.img".into(),
        ram: true,
        reserve: 2 << 30,
        stamp: String::new(),
    };
    let s = serde_json::to_string(&Req::Export(e.clone())).unwrap();
    assert!(s.contains("\"op\":\"export\""));
    assert!(matches!(serde_json::from_str::<Req>(&s).unwrap(), Req::Export(x) if x == e));
    assert!(matches!(
        serde_json::from_str::<Req>("{\"op\":\"list\"}").unwrap(),
        Req::List
    ));
    // An older daemon's answer without the newer fields still parses.
    let r: Resp =
        serde_json::from_str("{\"targets\":[{\"iqn\":\"a\",\"sessions\":2,\"ram\":false}]}")
            .unwrap();
    assert_eq!(
        (r.targets[0].sessions, r.err, r.version.as_str()),
        (2, None, "")
    );
}

/// The update session needs the update machine's IP and, when the disk names it, its MAC as ARP sees it.
#[test]
fn games_update_needs_ip_and_mac() {
    let c = |mac: Option<&str>| games::Config {
        name: "t".into(),
        update_ip: Some("10.0.0.20".into()),
        update_mac: mac.map(Into::into),
        ..Default::default()
    };
    let arp = |m: Option<&'static str>| move || m.map(String::from);
    let mac = Some("aa:bb:cc:dd:ee:01");
    assert!(games::update_allowed(
        &c(mac),
        "10.0.0.20",
        arp(Some("AA:BB:CC:DD:EE:01")),
        "t"
    ));
    assert!(
        !games::update_allowed(&c(mac), "10.0.0.20", arp(Some("aa:bb:cc:dd:ee:99")), "t"),
        "same IP, other MAC"
    );
    assert!(
        !games::update_allowed(&c(mac), "10.0.0.20", arp(None), "t"),
        "no ARP entry"
    );
    assert!(
        !games::update_allowed(&c(mac), "10.0.0.21", arp(Some("aa:bb:cc:dd:ee:01")), "t"),
        "other IP"
    );
    assert!(
        games::update_allowed(&c(None), "10.0.0.20", arp(None), "t"),
        "no MAC named: IP only"
    );
}

/// Whole sessions from the LAN, mangled: a login + a READ, a WRITE, an INQUIRY, a Data-Out and a logout in one stream,
/// random bytes changed / cut / repeated. Each connection must end once the client closes (no hang) and its task
/// must never panic; the target keeps serving.
#[tokio::test]
async fn fuzz_sessions() {
    let p = std::env::temp_dir().join("broom_test_iscsid_fuzz.img");
    std::fs::write(&p, vec![7u8; 1 << 20]).unwrap();
    let iqn = "iqn.test:fuzz.g1";
    export(Entry {
        iqn: iqn.into(),
        path: p.to_string_lossy().into(),
        ram: false,
        reserve: 0,
        stamp: String::new(),
    })
    .unwrap();
    let keys = format!(
        "InitiatorName=iqn.test:pc-fuzz\0TargetName={iqn}\0SessionType=Normal\0MaxRecvDataSegmentLength=4096\0MaxBurstLength=65536\0"
    );
    let mut stream = pdu(0x43, 0x80 | 1 << 2 | 3, 1, 0, keys.as_bytes());
    let mut cmd = |itt: u32, sn: u32, flags: u8, cdb: &[u8], edtl: u32, data: &[u8]| {
        let mut c = pdu(0x01, flags, itt, sn, data);
        c[8..16].fill(0);
        c[20..24].copy_from_slice(&edtl.to_be_bytes());
        c[32..32 + cdb.len()].copy_from_slice(cdb);
        stream.extend(c);
    };
    cmd(2, 1, 0xc0, &[0x28, 0, 0, 0, 0, 10, 0, 0, 8], 4096, &[]);
    cmd(3, 2, 0xa0, &[0x2a, 0, 0, 0, 0, 20, 0, 0, 1], 512, &[]);
    cmd(4, 3, 0xc0, &[0x12, 0, 0, 0, 96], 96, &[]);
    let mut data_out = pdu(0x05, 0x80, 3, 0, &[1u8; 512]);
    data_out[20..24].copy_from_slice(&1u32.to_be_bytes());
    stream.extend(data_out);
    stream.extend(pdu(0x06, 0x80, 5, 4, &[]));

    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    for (i, input) in crate::fuzz::inputs(&[stream], 400).enumerate() {
        let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (s, _) = l.accept().await.unwrap();
        let task = tokio::spawn(conn::serve(s, "127.0.0.1".into()));
        let _ = c.write_all(&input).await;
        let _ = c.shutdown().await;
        let mut sink = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::io::AsyncReadExt::read_to_end(&mut c, &mut sink),
        )
        .await;
        drop(c);
        match tokio::time::timeout(std::time::Duration::from_secs(5), task).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("case {i}: the session task panicked: {e}"),
            Err(_) => panic!("case {i}: the session did not end after the client closed"),
        }
    }
    TARGETS.lock().remove(iqn);
    let _ = std::fs::remove_file(p);
}
