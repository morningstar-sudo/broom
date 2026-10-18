use super::*;

fn m(id: i64, mac: &str, host: Option<&str>, ip: Option<&str>) -> Machine {
    Machine {
        id,
        mac: mac.into(),
        ip: ip.map(Into::into),
        hostname: host.map(Into::into),
        ..Default::default()
    }
}

#[test]
fn mac_normalize_and_validate() {
    assert_eq!(
        norm_mac("AA-BB-CC-DD-EE-0F").as_deref(),
        Some("aa:bb:cc:dd:ee:0f")
    );
    assert_eq!(
        norm_mac("aabb.ccdd.ee0f").as_deref(),
        Some("aa:bb:cc:dd:ee:0f")
    );
    assert_eq!(norm_mac("aa:bb:cc:dd:ee"), None);
    assert_eq!(norm_mac("zz:bb:cc:dd:ee:0f"), None);
    let all = [m(1, "aa:00:00:00:00:01", Some("PC01"), Some("10.0.0.51"))];
    let mut x = m(2, "AA-00-00-00-00-02", Some(" PC02 "), Some("10.0.0.52"));
    validate(&mut x, &all).unwrap();
    assert_eq!(
        (x.mac.as_str(), x.hostname.as_deref()),
        ("aa:00:00:00:00:02", Some("PC02"))
    );
    for (bad, why) in [
        (m(2, "aa:00:00:00:00:01", None, None), "MAC"),
        (m(2, "aa:00:00:00:00:02", Some("pc01"), None), "hostname"),
        (
            m(2, "aa:00:00:00:00:02", None, Some("10.0.0.51")),
            "IP 10.0.0.51 is already",
        ),
        (
            m(2, "aa:00:00:00:00:02", None, Some("10.0.0.300")),
            "not an IPv4",
        ),
        (m(2, "aa:00:00:00:00:02", Some("PC 2"), None), "Hostname"),
    ] {
        let mut b = bad;
        let e = validate(&mut b, &all).unwrap_err();
        assert!(e.contains(why), "{e}");
    }
    let mut same = all[0].clone(); // editing a machine never clashes with itself
    validate(&mut same, &all).unwrap();
}

#[test]
fn static_ip_not_held_by_a_live_lease() {
    let lease = |mac: &str, ip: &str, expires| crate::db::Lease {
        mac: mac.into(),
        ip: Some(ip.into()),
        hostname: None,
        expires,
        source: "full".into(),
    };
    let leases = [
        lease("bb:00:00:00:00:01", "10.0.0.50", 2000),
        lease("declined-10.0.0.51", "10.0.0.51", 2000),
        lease("bb:00:00:00:00:02", "10.0.0.52", 500),
    ];
    let pc = |ip| m(1, "aa:00:00:00:00:01", Some("PC01"), Some(ip));
    assert!(
        ip_free(&pc("10.0.0.50"), &leases, 1000)
            .unwrap_err()
            .contains("leased to bb:00:00:00:00:01")
    );
    assert!(
        ip_free(&pc("10.0.0.51"), &leases, 1000).is_err(),
        "declined = in use on the LAN"
    );
    assert!(
        ip_free(&pc("10.0.0.52"), &leases, 1000).is_ok(),
        "expired lease"
    );
    assert!(
        ip_free(
            &m(1, "bb:00:00:00:00:01", None, Some("10.0.0.50")),
            &leases,
            1000
        )
        .is_ok(),
        "its own lease"
    );
    assert!(ip_free(&m(1, "aa:00:00:00:00:01", None, None), &leases, 1000).is_ok());
}

#[test]
fn numbering_skips_taken() {
    let taken: HashSet<String> = ["pc01", "pc03"].iter().map(|s| s.to_string()).collect();
    assert_eq!(next_names("PC", 1, 2, &taken, 3), ["PC02", "PC04", "PC05"]);
    assert_eq!(
        next_names("VIP-", 9, 1, &HashSet::new(), 2),
        ["VIP-9", "VIP-10"]
    );
}

#[test]
fn csv_parse_and_import_plan() {
    let rows = parse_csv(
        "mac,notes\r\n\"aa:00:00:00:00:01\",\"a, \"\"b\"\"\"\n\naa:00:00:00:00:02,\"x\ny\"\n",
    )
    .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[1],
        (2, vec!["aa:00:00:00:00:01".into(), "a, \"b\"".into()])
    );
    assert_eq!(
        rows[2],
        (4, vec!["aa:00:00:00:00:02".into(), "x\ny".into()])
    );
    assert!(parse_csv("mac\n\"open").is_err());

    let existing = [m(1, "aa:00:00:00:00:01", Some("PC01"), Some("10.0.0.51"))];
    let img = crate::db::Image {
        id: 7,
        name: "win11".into(),
        os: "windows".into(),
        active_version: None,
        is_default: false,
        boot_script: None,
        hash: None,
        cache_mode: "disk".into(),
        use_ssd: true,
        groups: Vec::new(),
        preload: false,
    };
    // Columns in any order; existing row updated (ip column absent → kept), new row added with a key.
    let csv = "Hostname,MAC,group,image,license_key\nPC01,AA-00-00-00-00-01,VIP,win11,\nPC02,aa:00:00:00:00:02,,,abcde-12345-fghij-67890-klmno\n";
    let (w, keys) = plan_import(csv, &existing, std::slice::from_ref(&img)).unwrap();
    assert_eq!(w.len(), 2);
    assert_eq!(
        (
            w[0].id,
            w[0].ip.as_deref(),
            w[0].grp.as_deref(),
            w[0].image_id
        ),
        (1, Some("10.0.0.51"), Some("VIP"), Some(7))
    );
    assert!(w[1].id < 0 && w[1].hostname.as_deref() == Some("PC02"));
    assert_eq!(
        keys,
        vec![(
            "aa:00:00:00:00:02".to_string(),
            "ABCDE-12345-FGHIJ-67890-KLMNO".to_string()
        )]
    );
    // Every bad line reported with its number; duplicates inside the file too.
    let errs = plan_import(
        "mac,hostname,image\nxx,PC05,\naa:00:00:00:00:03,PC01,\naa:00:00:00:00:04,PC04,nope\n",
        &existing,
        &[img],
    )
    .unwrap_err();
    assert_eq!(errs.len(), 3, "{errs:?}");
    assert!(
        errs[0].starts_with("line 2")
            && errs[1].starts_with("line 3")
            && errs[2].contains("no image named"),
        "{errs:?}"
    );
    assert!(
        plan_import("hostname\nPC9\n", &existing, &[]).unwrap_err()[0].contains("\"mac\" column")
    );
}
