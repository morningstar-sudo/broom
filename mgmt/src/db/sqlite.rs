// db/sqlite.rs — SQLite driver (rusqlite, bundled). The only place with SQL.
// One Mutex<Connection> — admin + DHCP load is a few queries/second at most; a pool
// (r2d2) only if there are ever many concurrent writers.
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::sync::{Mutex, MutexGuard};

use super::{Db, DbResult, Driver, Image, Lease, Machine, NewImage};

/// Newline-separated list column → items (empty lines dropped).
fn lines(s: &str) -> Vec<String> {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

pub struct Sqlite {
    c: Mutex<Connection>,
}

const SCHEMA: &str = r#"
    PRAGMA journal_mode = WAL;

    CREATE TABLE IF NOT EXISTS images(
        id          INTEGER PRIMARY KEY,
        name        TEXT UNIQUE NOT NULL,
        os          TEXT NOT NULL,           -- 'linux' | 'windows'
        is_default  INTEGER NOT NULL DEFAULT 0,
        boot_script TEXT,                    -- iPXE boot snippet; empty = not published
        hash        TEXT,                    -- sha256 of the golden (version check for the SSD cache)
        cache_mode  TEXT NOT NULL DEFAULT 'disk', -- where the golden is kept: 'disk' | 'zram'
        active_version TEXT,                 -- versions.rs version image.img equals (NULL = none)
        base_mode   INTEGER NOT NULL DEFAULT 0 -- Windows: 1 = first logon waits in BASE MODE (technician), 0 = auto-commit
    );

    CREATE TABLE IF NOT EXISTS machines(
        id       INTEGER PRIMARY KEY,
        mac      TEXT UNIQUE NOT NULL,
        ip       TEXT,
        hostname TEXT,
        image_id INTEGER REFERENCES images(id),
        license_key    TEXT,                 -- Windows retail key, handed out once (license.rs)
        license_state  TEXT,                 -- 'armed' | 'sent' | NULL
        license_gen    INTEGER NOT NULL DEFAULT 0,
        license_result TEXT,                 -- slmgr output reported by the client
        grp            TEXT,                 -- free-text group, driver packages can target it
        notes          TEXT                  -- free text for the admin
    );

    -- Windows driver packages (drivers.rs). hwids / groups: newline-separated lists.
    CREATE TABLE IF NOT EXISTS drivers(
        id           INTEGER PRIMARY KEY,
        name         TEXT UNIQUE NOT NULL,
        sha256       TEXT NOT NULL,
        size         INTEGER NOT NULL,
        hwids        TEXT NOT NULL DEFAULT '',
        all_machines INTEGER NOT NULL DEFAULT 0,
        groups       TEXT NOT NULL DEFAULT '',
        created      INTEGER NOT NULL
    );

    -- Hardware IDs each machine's stage reported (drivers.rs), by MAC.
    CREATE TABLE IF NOT EXISTS machine_hw(
        mac   TEXT PRIMARY KEY,
        hwids TEXT NOT NULL,
        seen  INTEGER NOT NULL
    );

    -- DHCP leases (dhcp.rs). source 'full' = our lease ('proxy' rows only in DBs from old versions).
    CREATE TABLE IF NOT EXISTS leases(
        mac      TEXT PRIMARY KEY,
        ip       TEXT,
        hostname TEXT,
        expires  INTEGER NOT NULL,
        source   TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS config(
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    INSERT OR IGNORE INTO config(key,value) VALUES('boot_timeout','10');
    INSERT OR IGNORE INTO config(key,value) VALUES('zram_reserve_mb','2048');
    INSERT OR IGNORE INTO config(key,value) VALUES('dhcp_mode','off');
    INSERT OR IGNORE INTO config(key,value) VALUES('dhcp_iface','');
    INSERT OR IGNORE INTO config(key,value) VALUES('dhcp_server_ip','');
    INSERT OR IGNORE INTO config(key,value) VALUES('dhcp_subnet','');
    INSERT OR IGNORE INTO config(key,value) VALUES('dhcp_range_start','');
    INSERT OR IGNORE INTO config(key,value) VALUES('dhcp_range_end','');
    INSERT OR IGNORE INTO config(key,value) VALUES('dhcp_netmask','255.255.255.0');
    INSERT OR IGNORE INTO config(key,value) VALUES('dhcp_gateway','');
    INSERT OR IGNORE INTO config(key,value) VALUES('dhcp_dns','');
    INSERT OR IGNORE INTO config(key,value) VALUES('dhcp_lease','12h');
    INSERT OR IGNORE INTO config(key,value) VALUES('ltsp_user','guest');
    INSERT OR IGNORE INTO config(key,value) VALUES('ltsp_password','123456');
"#;

const IMAGE_COLS: &str = "id,name,os,is_default,boot_script,hash,cache_mode,active_version,base_mode";

fn image_row(r: &Row) -> rusqlite::Result<Image> {
    Ok(Image {
        id: r.get(0)?,
        name: r.get(1)?,
        os: r.get(2)?,
        is_default: r.get::<_, i64>(3)? == 1,
        boot_script: r.get(4)?,
        hash: r.get(5)?,
        cache_mode: r.get(6)?,
        active_version: r.get(7)?,
        base_mode: r.get::<_, i64>(8)? == 1,
    })
}

fn e(err: rusqlite::Error) -> String {
    err.to_string()
}

/// "iqn.2026-01.local.broom-<8 hex>" — RandomState is seeded from OS randomness (std only).
fn random_iqn_base() -> String {
    use std::hash::{BuildHasher, Hasher};
    let r = std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("iqn.2026-01.local.broom-{:08x}", r as u32)
}

impl Sqlite {
    pub fn open(path: &str) -> DbResult<Sqlite> {
        let c = Connection::open(path).map_err(|err| format!("open {path}: {err}"))?;
        // SQLite silently falls back to read-only when it can't open for writing (e.g. a file owned by
        // another user in /tmp: fs.protected_regular blocks even root).
        if c.is_readonly(rusqlite::MAIN_DB).unwrap_or(false) {
            return Err(format!(
                "{path} is read-only (file owner/permissions; in /tmp only the file's owner can write it) \
                 — run from a fixed directory like /opt/bootrom, or fix the owner: chown root {path}"
            ));
        }
        c.execute_batch(SCHEMA).map_err(e)?;
        // Old DB migration: add columns, ignore if they already exist.
        let _ = c.execute("ALTER TABLE images ADD COLUMN boot_script TEXT", []);
        let _ = c.execute("ALTER TABLE images ADD COLUMN hash TEXT", []);
        let _ = c.execute("ALTER TABLE images ADD COLUMN cache_mode TEXT NOT NULL DEFAULT 'disk'", []);
        let _ = c.execute("ALTER TABLE images ADD COLUMN active_version TEXT", []);
        let _ = c.execute("ALTER TABLE images ADD COLUMN base_mode INTEGER NOT NULL DEFAULT 0", []);
        let _ = c.execute("ALTER TABLE images DROP COLUMN dataset", []); // ZFS versioning removed
        let _ = c.execute("ALTER TABLE machines ADD COLUMN license_key TEXT", []);
        let _ = c.execute("ALTER TABLE machines ADD COLUMN license_state TEXT", []);
        let _ = c.execute("ALTER TABLE machines ADD COLUMN license_gen INTEGER NOT NULL DEFAULT 0", []);
        let _ = c.execute("ALTER TABLE machines ADD COLUMN license_result TEXT", []);
        let _ = c.execute("ALTER TABLE machines ADD COLUMN grp TEXT", []);
        let _ = c.execute("ALTER TABLE machines ADD COLUMN notes TEXT", []);
        // ProxyDHCP mode removed: off is the same for broom (the LAN's DHCP points clients at it).
        let _ = c.execute("UPDATE config SET value='off' WHERE key='dhcp_mode' AND value='proxy'", []);
        // iSCSI IQN base, random per server, generated once (first open without it) and kept.
        c.execute("INSERT OR IGNORE INTO config(key,value) VALUES('iqn_base',?1)", [random_iqn_base()]).map_err(e)?;
        Ok(Sqlite { c: Mutex::new(c) })
    }

    fn c(&self) -> MutexGuard<'_, Connection> {
        self.c.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn one_image(&self, sql_where: &str, p: impl rusqlite::Params) -> DbResult<Option<Image>> {
        self.c()
            .query_row(&format!("SELECT {IMAGE_COLS} FROM images WHERE {sql_where}"), p, image_row)
            .optional()
            .map_err(e)
    }
}

impl Db for Sqlite {
    fn get_config(&self, key: &str, default: &str) -> String {
        self.c()
            .query_row("SELECT value FROM config WHERE key=?1", [key], |r| r.get(0))
            .unwrap_or_else(|_| default.to_string())
    }

    fn set_config(&self, key: &str, value: &str) -> DbResult<()> {
        self.c()
            .execute(
                "INSERT INTO config(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [key, value],
            )
            .map(|_| ())
            .map_err(e)
    }

    fn images(&self) -> DbResult<Vec<Image>> {
        let c = self.c();
        let mut s = c.prepare(&format!("SELECT {IMAGE_COLS} FROM images ORDER BY id")).map_err(e)?;
        let rows = s.query_map([], image_row).map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    fn image(&self, id: i64) -> DbResult<Option<Image>> {
        self.one_image("id=?1", [id])
    }

    fn image_by_name(&self, name: &str) -> DbResult<Option<Image>> {
        self.one_image("name=?1", [name])
    }

    fn add_image(&self, i: &NewImage) -> DbResult<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO images(name,os,boot_script,cache_mode) VALUES(?1,?2,?3,?4)",
            params![i.name, i.os, i.boot_script, i.cache_mode],
        )
        .map_err(e)?;
        Ok(c.last_insert_rowid())
    }

    fn delete_image(&self, id: i64) -> DbResult<()> {
        // No foreign keys, and SQLite reuses the highest rowid → machines still pointing at this id would boot whatever
        // image gets it next. Drop the link; the default (if it was this one) moves to the oldest image left.
        let mut c = self.c();
        let t = c.transaction().map_err(e)?;
        t.execute("UPDATE machines SET image_id=NULL WHERE image_id=?1", [id]).map_err(e)?;
        t.execute("DELETE FROM images WHERE id=?1", [id]).map_err(e)?;
        t.execute(
            "UPDATE images SET is_default=1 WHERE id=(SELECT MIN(id) FROM images) AND NOT EXISTS(SELECT 1 FROM images WHERE is_default=1)",
            [],
        )
        .map_err(e)?;
        t.commit().map_err(e)
    }

    fn set_default_image(&self, id: i64) -> DbResult<()> {
        let mut c = self.c();
        let t = c.transaction().map_err(e)?;
        t.execute("UPDATE images SET is_default=0", []).map_err(e)?;
        t.execute("UPDATE images SET is_default=1 WHERE id=?1", [id]).map_err(e)?;
        t.commit().map_err(e)
    }

    fn set_boot_script(&self, id: i64, boot_script: &str) -> DbResult<()> {
        self.c()
            .execute("UPDATE images SET boot_script=?1 WHERE id=?2", params![boot_script, id])
            .map(|_| ())
            .map_err(e)
    }

    fn set_published(&self, id: i64, boot_script: &str, hash: &str) -> DbResult<()> {
        self.c()
            .execute("UPDATE images SET boot_script=?1, hash=?2 WHERE id=?3", params![boot_script, hash, id])
            .map(|_| ())
            .map_err(e)
    }

    fn set_cache_mode(&self, id: i64, mode: &str) -> DbResult<()> {
        self.c()
            .execute("UPDATE images SET cache_mode=?1 WHERE id=?2", params![mode, id])
            .map(|_| ())
            .map_err(e)
    }

    fn set_base_mode(&self, id: i64, on: bool) -> DbResult<()> {
        self.c()
            .execute("UPDATE images SET base_mode=?1 WHERE id=?2", params![on as i64, id])
            .map(|_| ())
            .map_err(e)
    }

    fn set_active_version(&self, id: i64, version: Option<&str>) -> DbResult<()> {
        self.c()
            .execute("UPDATE images SET active_version=?1 WHERE id=?2", params![version, id])
            .map(|_| ())
            .map_err(e)
    }

    fn machines(&self) -> DbResult<Vec<Machine>> {
        let c = self.c();
        let mut s = c
            .prepare(
                "SELECT id,mac,ip,hostname,image_id,license_key,license_state,license_gen,license_result,grp,notes
                 FROM machines ORDER BY hostname",
            )
            .map_err(e)?;
        let rows = s
            .query_map([], |r| {
                let key: Option<String> = r.get(5)?;
                Ok(Machine {
                    id: r.get(0)?,
                    mac: r.get(1)?,
                    ip: r.get(2)?,
                    hostname: r.get(3)?,
                    image_id: r.get(4)?,
                    license_tail: key.as_ref().map(|k| k[k.len().saturating_sub(5)..].to_string()),
                    license_key: key,
                    license_state: r.get(6)?,
                    license_gen: r.get(7)?,
                    license_result: r.get(8)?,
                    grp: r.get(9)?,
                    notes: r.get(10)?,
                })
            })
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    fn update_machine(&self, m: &Machine) -> DbResult<()> {
        self.c()
            .execute(
                "UPDATE machines SET mac=?1, ip=?2, hostname=?3, grp=?4, notes=?5, image_id=?6 WHERE id=?7",
                params![m.mac, m.ip, m.hostname, m.grp, m.notes, m.image_id, m.id],
            )
            .map(|_| ())
            .map_err(e)
    }

    fn delete_machine(&self, machine_id: i64) -> DbResult<()> {
        self.c().execute("DELETE FROM machines WHERE id=?1", [machine_id]).map(|_| ()).map_err(e)
    }

    fn set_machine_image(&self, machine_id: i64, image_id: Option<i64>) -> DbResult<()> {
        self.c()
            .execute("UPDATE machines SET image_id=?1 WHERE id=?2", params![image_id, machine_id])
            .map(|_| ())
            .map_err(e)
    }

    fn set_license(&self, machine_id: i64, key: Option<&str>) -> DbResult<()> {
        let sql = if key.is_some() {
            "UPDATE machines SET license_key=?1, license_state='armed', license_gen=license_gen+1, license_result=NULL WHERE id=?2"
        } else {
            "UPDATE machines SET license_key=?1, license_state=NULL, license_result=NULL WHERE id=?2"
        };
        self.c().execute(sql, params![key, machine_id]).map(|_| ()).map_err(e)
    }

    fn rearm_license(&self, machine_id: i64) -> DbResult<()> {
        self.c()
            .execute(
                "UPDATE machines SET license_state='armed', license_gen=license_gen+1, license_result=NULL
                 WHERE id=?1 AND license_key IS NOT NULL",
                [machine_id],
            )
            .map(|_| ())
            .map_err(e)
    }

    fn rearm_quiet(&self, machine_id: i64) -> DbResult<bool> {
        self.c()
            .execute("UPDATE machines SET license_state='armed' WHERE id=?1 AND license_state='sent'", [machine_id])
            .map(|n| n > 0)
            .map_err(e)
    }

    fn take_license(&self, machine_id: i64) -> DbResult<Option<String>> {
        let c = self.c(); // one connection lock → check + flip are atomic
        let n = c
            .execute("UPDATE machines SET license_state='sent' WHERE id=?1 AND license_state='armed'", [machine_id])
            .map_err(e)?;
        if n == 0 {
            return Ok(None);
        }
        c.query_row("SELECT license_key FROM machines WHERE id=?1", [machine_id], |r| r.get(0)).map_err(e)
    }

    fn set_license_result(&self, machine_id: i64, result: &str) -> DbResult<()> {
        self.c()
            .execute("UPDATE machines SET license_result=?1 WHERE id=?2", params![result, machine_id])
            .map(|_| ())
            .map_err(e)
    }

    fn set_machine_group(&self, machine_id: i64, grp: Option<&str>) -> DbResult<()> {
        self.c().execute("UPDATE machines SET grp=?1 WHERE id=?2", params![grp, machine_id]).map(|_| ()).map_err(e)
    }

    fn put_machine_hw(&self, mac: &str, hwids: &[String], seen: i64) -> DbResult<()> {
        self.c()
            .execute(
                "INSERT INTO machine_hw(mac,hwids,seen) VALUES(?1,?2,?3)
                 ON CONFLICT(mac) DO UPDATE SET hwids=excluded.hwids, seen=excluded.seen",
                params![mac, hwids.join("\n"), seen],
            )
            .map(|_| ())
            .map_err(e)
    }

    fn machine_hw(&self) -> DbResult<Vec<(String, Vec<String>)>> {
        let c = self.c();
        let mut s = c.prepare("SELECT mac,hwids FROM machine_hw").map_err(e)?;
        let rows = s.query_map([], |r| Ok((r.get::<_, String>(0)?, lines(&r.get::<_, String>(1)?)))).map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    fn drivers(&self) -> DbResult<Vec<Driver>> {
        let c = self.c();
        let mut s = c
            .prepare("SELECT id,name,sha256,size,hwids,all_machines,groups,created FROM drivers ORDER BY name")
            .map_err(e)?;
        let rows = s
            .query_map([], |r| {
                Ok(Driver {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    sha256: r.get(2)?,
                    size: r.get::<_, i64>(3)? as u64,
                    hwids: lines(&r.get::<_, String>(4)?),
                    all_machines: r.get::<_, i64>(5)? == 1,
                    groups: lines(&r.get::<_, String>(6)?),
                    created: r.get(7)?,
                })
            })
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    fn put_driver(&self, name: &str, sha256: &str, size: u64, hwids: &[String], created: i64) -> DbResult<()> {
        self.c()
            .execute(
                "INSERT INTO drivers(name,sha256,size,hwids,created) VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(name) DO UPDATE SET sha256=excluded.sha256, size=excluded.size,
                   hwids=excluded.hwids, created=excluded.created",
                params![name, sha256, size as i64, hwids.join("\n"), created],
            )
            .map(|_| ())
            .map_err(e)
    }

    fn set_driver_targets(&self, id: i64, all_machines: bool, groups: &[String]) -> DbResult<()> {
        self.c()
            .execute(
                "UPDATE drivers SET all_machines=?1, groups=?2 WHERE id=?3",
                params![all_machines as i64, groups.join("\n"), id],
            )
            .map(|_| ())
            .map_err(e)
    }

    fn delete_driver(&self, id: i64) -> DbResult<()> {
        self.c().execute("DELETE FROM drivers WHERE id=?1", [id]).map(|_| ()).map_err(e)
    }

    fn add_machine(&self, mac: &str, ip: Option<&str>, hostname: Option<&str>) -> DbResult<i64> {
        let c = self.c();
        c.execute("INSERT INTO machines(mac,ip,hostname) VALUES(?1,?2,?3)", params![mac, ip, hostname])
            .map_err(e)?;
        Ok(c.last_insert_rowid())
    }

    fn assign_image(&self, machine_id: i64, image_id: i64) -> DbResult<()> {
        self.c()
            .execute("UPDATE machines SET image_id=?1 WHERE id=?2", [image_id, machine_id])
            .map(|_| ())
            .map_err(e)
    }

    fn leases(&self) -> DbResult<Vec<Lease>> {
        let c = self.c();
        let mut s = c.prepare("SELECT mac,ip,hostname,expires,source FROM leases").map_err(e)?;
        let rows = s
            .query_map([], |r| {
                Ok(Lease { mac: r.get(0)?, ip: r.get(1)?, hostname: r.get(2)?, expires: r.get(3)?, source: r.get(4)? })
            })
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    fn put_lease(&self, l: &Lease) -> DbResult<()> {
        self.c()
            .execute(
                "INSERT INTO leases(mac,ip,hostname,expires,source) VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(mac) DO UPDATE SET ip=COALESCE(excluded.ip,ip),
                   hostname=COALESCE(excluded.hostname,hostname), expires=excluded.expires, source=excluded.source",
                params![l.mac, l.ip, l.hostname, l.expires, l.source],
            )
            .map(|_| ())
            .map_err(e)
    }

    fn delete_lease(&self, mac: &str) -> DbResult<()> {
        self.c().execute("DELETE FROM leases WHERE mac=?1", [mac]).map(|_| ()).map_err(e)
    }

    fn prune_leases(&self, cutoff: i64) -> DbResult<usize> {
        self.c().execute("DELETE FROM leases WHERE expires < ?1", [cutoff]).map_err(e)
    }
}

#[cfg(test)]
mod tests {
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
        assert!(a.starts_with("iqn.2026-01.local.broom-") && a.len() == 32, "{a}");
        assert_eq!(Sqlite::open(&p).unwrap().get_config("iqn_base", ""), a, "kept on reopen");
        assert_ne!(Sqlite::open(":memory:").unwrap().get_config("iqn_base", ""), a, "new DB → new base");
        tmp("broom_test_iqn.db");
    }

    #[test]
    fn delete_image_unlinks_machines_and_moves_default() {
        let db = Sqlite::open(":memory:").unwrap();
        let new = |name| NewImage { name, os: "linux", boot_script: None, cache_mode: "disk" };
        let (a, b) = (db.add_image(&new("a")).unwrap(), db.add_image(&new("b")).unwrap());
        db.set_default_image(b).unwrap();
        let m = db.add_machine("aa:bb:cc:dd:ee:01", None, None).unwrap();
        db.set_machine_image(m, Some(b)).unwrap();
        db.delete_image(b).unwrap();
        assert_eq!(db.machines().unwrap()[0].image_id, None, "no link to a deleted (reusable) id");
        assert!(db.image(a).unwrap().unwrap().is_default, "default moves to the image left");
        let c = db.add_image(&new("c")).unwrap();
        assert_eq!(db.machines().unwrap()[0].image_id, None, "a new image with the reused id {c} is not picked up");
    }

    #[test]
    fn dhcp_mode_proxy_becomes_off() {
        let p = tmp("broom_test_proxy.db");
        Sqlite::open(&p).unwrap().set_config("dhcp_mode", "proxy").unwrap();
        assert_eq!(Sqlite::open(&p).unwrap().get_config("dhcp_mode", ""), "off");
        tmp("broom_test_proxy.db");
    }

    #[test]
    fn images_machines_leases() {
        let db = Sqlite::open(":memory:").unwrap();
        let new = |name| NewImage { name, os: "linux", boot_script: None, cache_mode: "disk" };
        let a = db.add_image(&new("a")).unwrap();
        let b = db.add_image(&new("b")).unwrap();
        assert!(db.add_image(&new("a")).is_err(), "unique name");
        db.set_default_image(a).unwrap();
        db.set_default_image(b).unwrap();
        let imgs = db.images().unwrap();
        assert_eq!(imgs.iter().filter(|i| i.is_default).map(|i| i.id).collect::<Vec<_>>(), vec![b]);
        db.set_published(a, "boot", "h1").unwrap();
        db.set_cache_mode(a, "zram").unwrap();
        db.set_active_version(a, Some("v2")).unwrap();
        let ia = db.image_by_name("a").unwrap().unwrap();
        assert_eq!((ia.boot_script.as_deref(), ia.hash.as_deref(), ia.cache_mode.as_str()), (Some("boot"), Some("h1"), "zram"));
        assert_eq!(ia.active_version.as_deref(), Some("v2"));
        assert!(!ia.base_mode, "base mode off by default (first logon commits base by itself)");
        db.set_base_mode(a, true).unwrap();
        assert!(db.image(a).unwrap().unwrap().base_mode);
        db.delete_image(a).unwrap();
        assert!(db.image(a).unwrap().is_none());

        let m = db.add_machine("aa:bb:cc:dd:ee:01", Some("10.0.0.50"), Some("PC01")).unwrap();
        db.assign_image(m, b).unwrap();
        assert_eq!(db.machines().unwrap()[0].image_id, Some(b));

        // License: armed → taken once → re-armed → taken again; the key never reaches JSON.
        let key = "ABCDE-FGHIJ-KLMNO-PQRST-VWXYZ";
        db.set_license(m, Some(key)).unwrap();
        let mc = &db.machines().unwrap()[0];
        assert_eq!((mc.license_state.as_deref(), mc.license_gen, mc.license_tail.as_deref()), (Some("armed"), 1, Some("VWXYZ")));
        let json = serde_json::to_string(mc).unwrap();
        assert!(!json.contains("ABCDE") && !json.contains("license_key"), "{json}");
        assert_eq!(db.take_license(m).unwrap().as_deref(), Some(key));
        assert_eq!(db.take_license(m).unwrap(), None, "only once");
        db.set_license_result(m, "activated").unwrap();
        db.rearm_license(m).unwrap();
        let mc = &db.machines().unwrap()[0];
        assert_eq!((mc.license_state.as_deref(), mc.license_gen, mc.license_result.as_deref()), (Some("armed"), 2, None));
        assert_eq!(db.take_license(m).unwrap().as_deref(), Some(key));
        // Quiet re-arm (base rebuilt by the server's own change): armed again, SAME generation (no extra rebuild).
        assert!(db.rearm_quiet(m).unwrap());
        assert!(!db.rearm_quiet(m).unwrap(), "only from 'sent'");
        let mc = &db.machines().unwrap()[0];
        assert_eq!((mc.license_state.as_deref(), mc.license_gen), (Some("armed"), 2));
        assert_eq!(db.take_license(m).unwrap().as_deref(), Some(key));
        db.set_license(m, None).unwrap();
        let mc = &db.machines().unwrap()[0];
        assert_eq!((mc.license_state.as_deref(), mc.license_tail.as_deref()), (None, None));
        db.rearm_license(m).unwrap(); // no key → stays without state
        assert_eq!(db.machines().unwrap()[0].license_state, None);

        // Edit / image / delete; the mac stays unique.
        let m2 = db.add_machine("aa:bb:cc:dd:ee:02", None, Some("PC02")).unwrap();
        let mut row = db.machines().unwrap().into_iter().find(|x| x.id == m2).unwrap();
        row.hostname = Some("PC22".into());
        row.notes = Some("seat 22".into());
        row.ip = Some("10.0.0.22".into());
        db.update_machine(&row).unwrap();
        db.set_machine_image(m2, Some(b)).unwrap();
        let got = db.machines().unwrap().into_iter().find(|x| x.id == m2).unwrap();
        assert_eq!((got.hostname.as_deref(), got.notes.as_deref(), got.image_id), (Some("PC22"), Some("seat 22"), Some(b)));
        db.set_machine_image(m2, None).unwrap();
        assert_eq!(db.machines().unwrap().into_iter().find(|x| x.id == m2).unwrap().image_id, None);
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
        assert_eq!(db.machine_hw().unwrap(), vec![("aa:bb:cc:dd:ee:01".to_string(), ids.clone())]);
        db.put_driver("nvidia", "s1", 10, &ids, 1).unwrap();
        let d = db.drivers().unwrap()[0].clone();
        db.set_driver_targets(d.id, true, &["VIP".into(), "Pro".into()]).unwrap();
        db.put_driver("nvidia", "s2", 20, &[], 2).unwrap();
        let d = db.drivers().unwrap()[0].clone();
        assert_eq!((d.sha256.as_str(), d.size, d.hwids.len(), d.all_machines), ("s2", 20, 0, true));
        assert_eq!(d.groups, vec!["VIP", "Pro"]);
        db.delete_driver(d.id).unwrap();
        assert!(db.drivers().unwrap().is_empty());

        let l = Lease { mac: "m1".into(), ip: Some("10.0.0.100".into()), hostname: Some("x".into()), expires: 5, source: "full".into() };
        db.put_lease(&l).unwrap();
        // None ip/hostname keep the stored values.
        db.put_lease(&Lease { ip: None, hostname: None, expires: 9, source: "proxy".into(), ..l.clone() }).unwrap();
        assert_eq!(db.leases().unwrap(), vec![Lease { expires: 9, source: "proxy".into(), ..l }]);
        db.delete_lease("m1").unwrap();
        assert!(db.leases().unwrap().is_empty());

        db.set_config("k", "v1").unwrap();
        db.set_config("k", "v2").unwrap();
        assert_eq!((db.get_config("k", ""), db.get_config("missing", "d")), ("v2".into(), "d".into()));
    }
}
