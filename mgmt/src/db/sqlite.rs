// db/sqlite.rs — SQLite driver (rusqlite, bundled). The only place with SQL.
// ponytail: one Mutex<Connection> — admin + DHCP load is a few queries/second at most; a pool
// (r2d2) only if there are ever many concurrent writers.
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::sync::{Mutex, MutexGuard};

use super::{Db, DbResult, Image, Lease, Machine, NewImage};

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
        active_version TEXT                  -- versions.rs version image.img equals (NULL = none)
    );

    CREATE TABLE IF NOT EXISTS machines(
        id       INTEGER PRIMARY KEY,
        mac      TEXT UNIQUE NOT NULL,
        ip       TEXT,
        hostname TEXT,
        image_id INTEGER REFERENCES images(id)
    );

    -- DHCP leases (dhcp.rs). source 'full' = our lease; 'proxy' = PXE client seen in proxy mode.
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
    INSERT OR IGNORE INTO config(key,value) VALUES('dhcp_mode','proxy');
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

const IMAGE_COLS: &str = "id,name,os,is_default,boot_script,hash,cache_mode,active_version";

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
        let _ = c.execute("ALTER TABLE images DROP COLUMN dataset", []); // ZFS versioning removed
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
        self.c().execute("DELETE FROM images WHERE id=?1", [id]).map(|_| ()).map_err(e)
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

    fn set_active_version(&self, id: i64, version: Option<&str>) -> DbResult<()> {
        self.c()
            .execute("UPDATE images SET active_version=?1 WHERE id=?2", params![version, id])
            .map(|_| ())
            .map_err(e)
    }

    fn machines(&self) -> DbResult<Vec<Machine>> {
        let c = self.c();
        let mut s = c.prepare("SELECT id,mac,ip,hostname,image_id FROM machines ORDER BY hostname").map_err(e)?;
        let rows = s
            .query_map([], |r| {
                Ok(Machine { id: r.get(0)?, mac: r.get(1)?, ip: r.get(2)?, hostname: r.get(3)?, image_id: r.get(4)? })
            })
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
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
        db.delete_image(a).unwrap();
        assert!(db.image(a).unwrap().is_none());

        let m = db.add_machine("aa:bb:cc:dd:ee:01", Some("10.0.0.50"), Some("PC01")).unwrap();
        db.assign_image(m, b).unwrap();
        assert_eq!(db.machines().unwrap()[0].image_id, Some(b));

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
