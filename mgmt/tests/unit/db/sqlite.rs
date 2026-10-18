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
fn old_db_migrates_to_user_version() {
    let p = tmp("broom_test_migrate.db");
    Connection::open(&p)
            .unwrap()
            .execute_batch(
                "CREATE TABLE images(id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL, os TEXT NOT NULL,
                     is_default INTEGER NOT NULL DEFAULT 0, dataset TEXT);
                 CREATE TABLE machines(id INTEGER PRIMARY KEY, mac TEXT UNIQUE NOT NULL, ip TEXT, hostname TEXT,
                     image_id INTEGER REFERENCES images(id));
                 INSERT INTO images(name,os,dataset) VALUES('old','linux','pool/old');
                 INSERT INTO machines(mac) VALUES('aa:bb:cc:dd:ee:01');",
            )
            .unwrap();
    for _ in 0..2 {
        let db = Sqlite::open(&p).unwrap(); // second open: already migrated, nothing to do
        assert_eq!(db.images().unwrap()[0].cache_mode, "disk");
        assert_eq!(db.machines().unwrap()[0].mac, "aa:bb:cc:dd:ee:01");
    }
    let c = Connection::open(&p).unwrap();
    assert_eq!(
        c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert!(
        !has_column(&c, "images", "dataset").unwrap()
            && has_column(&c, "machines", "notes").unwrap()
            && has_column(&c, "machines", "base_image").unwrap()
    );
    // A new DB starts at the latest version.
    let p2 = tmp("broom_test_migrate_new.db");
    drop(Sqlite::open(&p2).unwrap());
    let v: i64 = Connection::open(&p2)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    let sync: i64 = Sqlite::open(&p2)
        .unwrap()
        .c()
        .query_row("PRAGMA synchronous", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sync, 1, "NORMAL on every open (per-connection pragma)");
    tmp("broom_test_migrate.db");
    tmp("broom_test_migrate_new.db");
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
fn dhcp_mode_proxy_becomes_off() {
    let p = tmp("broom_test_proxy.db");
    Sqlite::open(&p)
        .unwrap()
        .set_config("dhcp_mode", "proxy")
        .unwrap();
    assert_eq!(Sqlite::open(&p).unwrap().get_config("dhcp_mode", ""), "off");
    tmp("broom_test_proxy.db");
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
        source: "full".into(),
    };
    db.put_lease(&l).unwrap();
    // None ip/hostname keep the stored values.
    db.put_lease(&Lease {
        ip: None,
        hostname: None,
        expires: 9,
        source: "proxy".into(),
        ..l.clone()
    })
    .unwrap();
    assert_eq!(
        db.leases().unwrap(),
        vec![Lease {
            expires: 9,
            source: "proxy".into(),
            ..l
        }]
    );
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

/// A DB from before v3 with rows breaking the rules: fixed on open (the oldest row keeps the value), then the database
/// itself refuses a second machine with the same MAC (any case) / fixed IP / hostname (any case), a second default
/// image and a machine pointing at an image that does not exist.
#[test]
fn v3_rules_fixed_then_kept() {
    let p = tmp("broom_test_v3.db");
    Connection::open(&p)
        .unwrap()
        .execute_batch(
            "PRAGMA foreign_keys=OFF; -- rows an older build could have left
             CREATE TABLE images(id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL, os TEXT NOT NULL,
                 is_default INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE machines(id INTEGER PRIMARY KEY, mac TEXT UNIQUE NOT NULL, ip TEXT, hostname TEXT,
                 image_id INTEGER REFERENCES images(id));
             INSERT INTO images(id,name,os,is_default) VALUES(1,'a','linux',1),(2,'b','linux',1);
             INSERT INTO machines(id,mac,ip,hostname,image_id) VALUES
                 (1,'aa:bb:cc:dd:ee:01','10.0.0.11','MAY01',1),
                 (2,'AA:BB:CC:DD:EE:01',NULL,NULL,NULL),
                 (3,'aa:bb:cc:dd:ee:03','10.0.0.11','may01',99);",
        )
        .unwrap();
    let db = Sqlite::open(&p).unwrap();
    let ms = db.machines().unwrap();
    assert_eq!(ms.len(), 2, "the second spelling of a MAC removed");
    let m3 = ms.iter().find(|m| m.id == 3).unwrap();
    assert_eq!(
        (m3.ip.as_deref(), m3.hostname.as_deref(), m3.image_id),
        (None, None, None)
    );
    let m1 = ms.iter().find(|m| m.id == 1).unwrap();
    assert_eq!(
        (m1.ip.as_deref(), m1.hostname.as_deref(), m1.image_id),
        (Some("10.0.0.11"), Some("MAY01"), Some(1))
    );
    assert_eq!(
        db.images().unwrap().iter().filter(|i| i.is_default).count(),
        1
    );

    // From now on the database refuses them itself.
    assert!(
        db.add_machine("AA:BB:CC:DD:EE:03", None, None).is_err(),
        "same MAC, other case"
    );
    let mut m = m3.clone();
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
    tmp("broom_test_v3.db");
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
    drop(db);
    tmp("broom_test_edit.db");
}

/// A v3 database (before base mode per machine) gets machines.base_image on open.
#[test]
fn v3_gets_base_image() {
    let p = tmp("broom_test_v4.db");
    drop(Sqlite::open(&p).unwrap());
    {
        let c = Connection::open(&p).unwrap();
        c.execute_batch("ALTER TABLE machines DROP COLUMN base_image; PRAGMA user_version=3;")
            .unwrap();
        assert!(!has_column(&c, "machines", "base_image").unwrap());
    }
    let db = Sqlite::open(&p).unwrap();
    let m = db.add_machine("aa:bb:cc:dd:ee:09", None, None).unwrap();
    db.set_machine_base(m, None).unwrap();
    assert!(has_column(&db.c(), "machines", "base_image").unwrap());
    tmp("broom_test_v4.db");
}
