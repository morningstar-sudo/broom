use super::*;

fn tmp(name: &str) -> String {
    let p = std::env::temp_dir().join(name);
    for s in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{s}", p.display()));
    }
    p.to_string_lossy().into_owned()
}

#[test]
fn iqn_base_random_and_kept() {
    let p = tmp("broom_test_iqn.db");
    let a = Sqlite::open(&p).unwrap().get_config("iqn_base", "");
    assert!(
        a.starts_with("iqn.2026-01.local.broom-") && a.len() == 32,
        "{a}"
    );
    assert_eq!(
        Sqlite::open(&p).unwrap().get_config("iqn_base", ""),
        a,
        "kept on reopen"
    );
    assert_ne!(
        Sqlite::open(":memory:").unwrap().get_config("iqn_base", ""),
        a,
        "new DB → new base"
    );
    tmp("broom_test_iqn.db");
}

#[test]
fn delete_image_unlinks_machines_and_moves_default() {
    let db = Sqlite::open(":memory:").unwrap();
    let new = |name, os| NewImage {
        name,
        os,
        boot_script: Some("boot"),
        cache_mode: "disk",
    };
    let (a, b) = (
        db.add_image(&new("a", "linux")).unwrap(),
        db.add_image(&new("b", "linux")).unwrap(),
    );
    let w = db
        .add_image(&NewImage {
            name: "w",
            os: "windows",
            boot_script: Some("boot"),
            cache_mode: "disk",
        })
        .unwrap();
    db.set_default_image(b).unwrap();
    let m = db.add_machine("aa:bb:cc:dd:ee:01", None, None).unwrap();
    db.set_machine_image(m, Some(b)).unwrap();
    assert_eq!(
        db.machines().unwrap()[0].base_image,
        None,
        "base mode off by default"
    );
    db.set_machine_base(m, Some(b)).unwrap();
    assert_eq!(db.machines().unwrap()[0].base_image, Some(b));
    db.delete_image(b).unwrap();
    let after = &db.machines().unwrap()[0];
    assert_eq!(
        (after.image_id, after.base_image),
        (None, None),
        "no link to a deleted (reusable) id"
    );
    assert!(
        db.image(a).unwrap().unwrap().is_default,
        "default moves to the published image of the same OS"
    );
    assert!(!db.image(w).unwrap().unwrap().is_default);
    let c = db.add_image(&new("c", "linux")).unwrap();
    assert_eq!(
        db.machines().unwrap()[0].image_id,
        None,
        "a new image with the reused id {c} is not picked up"
    );
    // Last Linux image gone → no default at all (the Windows one is not picked: another OS).
    db.delete_image(a).unwrap();
    db.delete_image(c).unwrap();
    assert!(
        db.images().unwrap().iter().all(|i| !i.is_default),
        "never another OS"
    );
}

#[test]
fn synchronous_normal_on_every_open() {
    let p = tmp("broom_test_sync.db");
    drop(Sqlite::open(&p).unwrap());
    let sync: i64 = Sqlite::open(&p)
        .unwrap()
        .c()
        .query_row("PRAGMA synchronous", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sync, 1, "NORMAL on every open (per-connection pragma)");
    tmp("broom_test_sync.db");
}

#[test]
fn db_files_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let p = tmp("broom_test_perm.db");
    std::fs::write(&p, b"").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap(); // an older install
    let db = Sqlite::open(&p).unwrap();
    db.set_config("x", "y").unwrap();
    for f in [p.clone(), format!("{p}-wal")] {
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600,
            "{f}"
        );
    }
    drop(db);
    tmp("broom_test_perm.db");
}

#[test]
fn images_machines_leases() {
    let db = Sqlite::open(":memory:").unwrap();
    let new = |name| NewImage {
        name,
        os: "linux",
        boot_script: None,
        cache_mode: "disk",
    };
    let a = db.add_image(&new("a")).unwrap();
    let b = db.add_image(&new("b")).unwrap();
    assert!(db.add_image(&new("a")).is_err(), "unique name");
    db.set_default_image(a).unwrap();
    db.set_default_image(b).unwrap();
    let imgs = db.images().unwrap();
    assert_eq!(
        imgs.iter()
            .filter(|i| i.is_default)
            .map(|i| i.id)
            .collect::<Vec<_>>(),
        vec![b]
    );
    db.set_published(a, "boot", "h1").unwrap();
    db.set_cache_mode(a, "zram").unwrap();
    db.set_active_version(a, Some("v2")).unwrap();
    let ia = db.image_by_name("a").unwrap().unwrap();
    assert_eq!(
        (
            ia.boot_script.as_deref(),
            ia.hash.as_deref(),
            ia.cache_mode.as_str()
        ),
        (Some("boot"), Some("h1"), "zram")
    );
    assert_eq!(ia.active_version.as_deref(), Some("v2"));
    assert!(
        ia.use_ssd,
        "SSD on by default (cache + writes on the machine's SSD)"
    );
    db.set_use_ssd(a, false).unwrap();
    assert!(!db.image(a).unwrap().unwrap().use_ssd);
    db.delete_image(a).unwrap();
    assert!(db.image(a).unwrap().is_none());

    let m = db
        .add_machine("aa:bb:cc:dd:ee:01", Some("10.0.0.50"), Some("PC01"))
        .unwrap();
    db.set_machine_image(m, Some(b)).unwrap();
    assert_eq!(db.machines().unwrap()[0].image_id, Some(b));

    // License: armed → taken once → re-armed → taken again; the key never reaches JSON.
    let key = "ABCDE-FGHIJ-KLMNO-PQRST-VWXYZ";
    db.set_license(m, Some(key)).unwrap();
    let mc = &db.machines().unwrap()[0];
    assert_eq!(
        (
            mc.license_state.as_deref(),
            mc.license_gen,
            mc.license_tail.as_deref()
        ),
        (Some("armed"), 1, Some("VWXYZ"))
    );
    let json = serde_json::to_string(mc).unwrap();
    assert!(
        !json.contains("ABCDE") && !json.contains("license_key"),
        "{json}"
    );
    assert_eq!(db.take_license(m).unwrap().as_deref(), Some(key));
    assert_eq!(db.take_license(m).unwrap(), None, "only once");
    db.set_license_result(m, "activated").unwrap();
    db.rearm_license(m).unwrap();
    let mc = &db.machines().unwrap()[0];
    assert_eq!(
        (
            mc.license_state.as_deref(),
            mc.license_gen,
            mc.license_result.as_deref()
        ),
        (Some("armed"), 2, None)
    );
    assert_eq!(db.take_license(m).unwrap().as_deref(), Some(key));
    // Quiet re-arm (base rebuilt by the server's own change): armed again, SAME generation (no extra rebuild).
    assert!(db.rearm_quiet(m).unwrap());
    assert!(!db.rearm_quiet(m).unwrap(), "only from 'sent'");
    let mc = &db.machines().unwrap()[0];
    assert_eq!(
        (mc.license_state.as_deref(), mc.license_gen),
        (Some("armed"), 2)
    );
    assert_eq!(db.take_license(m).unwrap().as_deref(), Some(key));
    db.set_license(m, None).unwrap();
    let mc = &db.machines().unwrap()[0];
    assert_eq!(
        (mc.license_state.as_deref(), mc.license_tail.as_deref()),
        (None, None)
    );
    db.rearm_license(m).unwrap(); // no key → stays without state
    assert_eq!(db.machines().unwrap()[0].license_state, None);

    // Edit / image / delete; the mac stays unique.
    let m2 = db
        .add_machine("aa:bb:cc:dd:ee:02", None, Some("PC02"))
        .unwrap();
    let mut row = db
        .machines()
        .unwrap()
        .into_iter()
        .find(|x| x.id == m2)
        .unwrap();
    row.hostname = Some("PC22".into());
    row.notes = Some("seat 22".into());
    row.ip = Some("10.0.0.22".into());
    db.update_machine(&row).unwrap();
    db.set_machine_image(m2, Some(b)).unwrap();
    let got = db
        .machines()
        .unwrap()
        .into_iter()
        .find(|x| x.id == m2)
        .unwrap();
    assert_eq!(
        (got.hostname.as_deref(), got.notes.as_deref(), got.image_id),
        (Some("PC22"), Some("seat 22"), Some(b))
    );
    db.set_machine_image(m2, None).unwrap();
    assert_eq!(
        db.machines()
            .unwrap()
            .into_iter()
            .find(|x| x.id == m2)
            .unwrap()
            .image_id,
        None
    );
    row.mac = "aa:bb:cc:dd:ee:01".into();
    assert!(db.update_machine(&row).is_err(), "mac already used by PC01");
    db.delete_machine(m2).unwrap();
    assert_eq!(db.machines().unwrap().len(), 1);

    // Groups + reported hardware + driver packages (a re-upload keeps the targets).
    db.set_machine_group(m, Some("VIP")).unwrap();
    assert_eq!(db.machines().unwrap()[0].grp.as_deref(), Some("VIP"));
    let ids = vec!["PCI\\VEN_10DE&DEV_2504".to_string()];
    db.put_machine_hw("aa:bb:cc:dd:ee:01", &ids, 1).unwrap();
    db.put_machine_hw("aa:bb:cc:dd:ee:01", &ids, 2).unwrap();
    assert_eq!(
        db.machine_hw().unwrap(),
        vec![("aa:bb:cc:dd:ee:01".to_string(), ids.clone())]
    );
    db.put_driver("nvidia", "s1", 10, &ids, 1).unwrap();
    let d = db.drivers().unwrap()[0].clone();
    db.set_driver_targets(d.id, true, &["VIP".into(), "Pro".into()])
        .unwrap();
    db.put_driver("nvidia", "s2", 20, &[], 2).unwrap();
    let d = db.drivers().unwrap()[0].clone();
    assert_eq!(
        (d.sha256.as_str(), d.size, d.hwids.len(), d.all_machines),
        ("s2", 20, 0, true)
    );
    assert_eq!(d.groups, vec!["VIP", "Pro"]);
    db.delete_driver(d.id).unwrap();
    assert!(db.drivers().unwrap().is_empty());

    let l = Lease {
        mac: "m1".into(),
        ip: Some("10.0.0.100".into()),
        hostname: Some("x".into()),
        expires: 5,
    };
    db.put_lease(&l).unwrap();
    // None ip/hostname keep the stored values.
    db.put_lease(&Lease {
        ip: None,
        hostname: None,
        expires: 9,
        ..l.clone()
    })
    .unwrap();
    assert_eq!(db.leases().unwrap(), vec![Lease { expires: 9, ..l }]);
    db.delete_lease("m1").unwrap();
    assert!(db.leases().unwrap().is_empty());

    db.set_config("k", "v1").unwrap();
    db.set_config("k", "v2").unwrap();
    assert_eq!(
        (db.get_config("k", ""), db.get_config("missing", "d")),
        ("v2".into(), "d".into())
    );
}

#[test]
fn bench_rows_latest_prune() {
    let db = Sqlite::open(":memory:").unwrap();
    let row = |at: i64, mac: Option<&str>, kind: &str, value: f64| BenchRow {
        at,
        mac: mac.map(String::from),
        kind: kind.into(),
        value,
        detail: String::new(),
    };
    for r in [
        row(1, None, "srv_seq", 100.0),
        row(2, None, "srv_seq", 200.0),
        row(3, Some("m1"), "boot", 40.0),
        row(4, Some("m1"), "boot", 30.0),
        row(5, Some("m2"), "net", 110.0),
        row(0, Some("m1"), "boot", 99.0), // added last, older: not the latest
    ] {
        db.add_bench(&r).unwrap();
    }
    assert_eq!(db.bench_rows(None, 10).unwrap().len(), 6);
    assert_eq!(
        db.bench_rows(Some("m1"), 10).unwrap(),
        vec![
            row(4, Some("m1"), "boot", 30.0),
            row(3, Some("m1"), "boot", 40.0),
            row(0, Some("m1"), "boot", 99.0)
        ]
    );
    let mut latest = db.bench_latest().unwrap();
    latest.sort_by_key(|r| r.at);
    assert_eq!(
        latest,
        vec![
            row(2, None, "srv_seq", 200.0),
            row(4, Some("m1"), "boot", 30.0),
            row(5, Some("m2"), "net", 110.0)
        ]
    );
    // Older than the cutoff (at 0, 1) + over the cap (keep the newest 2: at 5, 4).
    assert_eq!(db.prune_bench(2, 2).unwrap(), 4);
    assert_eq!(
        db.bench_rows(None, 10)
            .unwrap()
            .iter()
            .map(|r| r.at)
            .collect::<Vec<_>>(),
        vec![5, 4]
    );
}

/// The database itself refuses a second machine with the same MAC (any case) / fixed IP / hostname (any case), a
/// second default image and a machine pointing at an image that does not exist.
#[test]
fn rules_kept_by_the_db() {
    let p = tmp("broom_test_rules.db");
    let db = Sqlite::open(&p).unwrap();
    db.c()
        .execute_batch(
            "INSERT INTO images(id,name,os,is_default) VALUES(1,'a','linux',1),(2,'b','linux',0);
             INSERT INTO machines(id,mac,ip,hostname,image_id) VALUES
                 (1,'aa:bb:cc:dd:ee:01','10.0.0.11','MAY01',1),
                 (3,'aa:bb:cc:dd:ee:03',NULL,NULL,NULL);",
        )
        .unwrap();
    let mut m = db
        .machines()
        .unwrap()
        .into_iter()
        .find(|m| m.id == 3)
        .unwrap();
    assert!(
        db.add_machine("AA:BB:CC:DD:EE:03", None, None).is_err(),
        "same MAC, other case"
    );
    m.ip = Some("10.0.0.11".into());
    assert!(db.update_machine(&m).is_err(), "same fixed IP");
    m.ip = None;
    m.hostname = Some("May01".into());
    assert!(db.update_machine(&m).is_err(), "same hostname, other case");
    m.hostname = None;
    m.image_id = Some(99);
    assert!(db.update_machine(&m).is_err(), "no such image");
    assert!(db.set_machine_image(3, Some(42)).is_err(), "no such image");
    m.image_id = Some(2);
    db.update_machine(&m).unwrap();
    db.set_default_image(2).unwrap();
    assert_eq!(
        db.images()
            .unwrap()
            .iter()
            .filter(|i| i.is_default)
            .map(|i| i.id)
            .collect::<Vec<_>>(),
        vec![2]
    );
    // A deleted machine's hardware IDs go with it.
    db.put_machine_hw("aa:bb:cc:dd:ee:03", &["PCI_VEN_1".into()], 1)
        .unwrap();
    db.delete_machine(3).unwrap();
    assert!(
        db.machine_hw()
            .unwrap()
            .iter()
            .all(|(mac, _)| mac != "aa:bb:cc:dd:ee:03")
    );
    drop(db);
    tmp("broom_test_rules.db");
}

/// Many edits of one JSON value at the same time: every one lands (none overwrites another), and an edit that
/// refuses changes nothing.
#[test]
fn json_edits_are_atomic() {
    let p = tmp("broom_test_edit.db");
    let db: std::sync::Arc<dyn Db> = std::sync::Arc::new(Sqlite::open(&p).unwrap());
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let db = db.clone();
            std::thread::spawn(move || {
                for i in 0..25 {
                    super::super::edit_json(&*db, "list", |v: &mut Vec<String>| {
                        v.push(format!("{t}-{i}"));
                        Ok::<(), String>(())
                    })
                    .unwrap()
                    .unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let v: Vec<String> = serde_json::from_str(&db.get_config("list", "[]")).unwrap();
    assert_eq!(v.len(), 200, "no edit lost");
    let r = super::super::edit_json(&*db, "list", |v: &mut Vec<String>| {
        v.clear();
        Err::<(), _>("refused")
    })
    .unwrap();
    assert_eq!(r, Err("refused"));
    let v: Vec<String> = serde_json::from_str(&db.get_config("list", "[]")).unwrap();
    assert_eq!(v.len(), 200, "a refused edit writes nothing");
    db.set_config("broken", "not json").unwrap();
    let r = super::super::edit_json(&*db, "broken", |v: &mut Vec<String>| {
        v.push("x".into());
        Ok::<(), String>(())
    });
    assert!(
        r.is_err(),
        "an unreadable value is an error, not an empty list"
    );
    assert_eq!(
        db.get_config("broken", ""),
        "not json",
        "and stays as it was"
    );
    drop(db);
    tmp("broom_test_edit.db");
}

/// Sessions: found until they expire; logout ends one, a password change / deleted account all of an account's;
/// a new login drops the expired ones.
#[test]
fn sessions() {
    let db = Sqlite::open(":memory:").unwrap();
    db.put_session("a1", "admin", "k-a1", 100, 10).unwrap();
    db.put_session("a2", "admin", "k-a2", 100, 10).unwrap();
    db.put_session("s1", "staff", "k-s1", 50, 10).unwrap();
    assert_eq!(
        db.session("a1", 20).unwrap(),
        Some(("admin".into(), "k-a1".into()))
    );
    assert_eq!(db.session("s1", 50).unwrap(), None, "expired");
    assert_eq!(db.session("nope", 20).unwrap(), None);
    db.delete_session("a1").unwrap();
    assert_eq!(db.session("a1", 20).unwrap(), None, "logged out");
    assert_eq!(
        db.session("a2", 20).unwrap(),
        Some(("admin".into(), "k-a2".into()))
    );
    db.delete_sessions("admin").unwrap();
    assert_eq!(db.session("a2", 20).unwrap(), None, "all of admin's");
    db.put_session("n1", "staff", "k-n1", 200, 60).unwrap();
    let left: i64 = db
        .c()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 1, "the expired s1 dropped");
}
