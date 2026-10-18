// db/sqlite.rs — SQLite driver (rusqlite, bundled). The only place with SQL.
// One Mutex<Connection> — admin + DHCP load is a few queries/second at most; a pool
// (r2d2) only if there are ever many concurrent writers.
use parking_lot::{Mutex, MutexGuard};
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{BenchRow, Db, DbResult, Driver, Image, Lease, Machine, NewImage};

/// Newline-separated list column → items (empty lines dropped).
fn lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

pub struct Sqlite {
    c: Mutex<Connection>,
}

const SCHEMA: &str = r#"
    PRAGMA journal_mode = WAL;
    -- Per connection, so it is part of every open. WAL + NORMAL: no fsync per commit (each DHCP lease write),
    -- still consistent; a power loss can only drop the last commits.
    PRAGMA synchronous = NORMAL;
    -- machines.image_id must name an existing image. The bundled SQLite already defaults to this; said here so it
    -- does not depend on how SQLite was built.
    PRAGMA foreign_keys = ON;

    CREATE TABLE IF NOT EXISTS images(
        id          INTEGER PRIMARY KEY,
        name        TEXT UNIQUE NOT NULL,
        os          TEXT NOT NULL,           -- 'linux' | 'windows'
        is_default  INTEGER NOT NULL DEFAULT 0,
        boot_script TEXT,                    -- iPXE boot snippet; empty = not published
        hash        TEXT,                    -- sha256 of the golden (version check for the SSD cache)
        cache_mode  TEXT NOT NULL DEFAULT 'disk', -- where the golden is kept: 'disk' | 'zram'
        active_version TEXT,                 -- versions.rs version image.img equals (NULL = none)
        base_mode   INTEGER NOT NULL DEFAULT 0, -- Windows: 1 = first logon waits in BASE MODE (technician), 0 = auto-commit
        use_ssd     INTEGER NOT NULL DEFAULT 1, -- Linux: 1 = cache + writes on the machine's SSD, 0 = SSD untouched (RAM only)
        grps        TEXT NOT NULL DEFAULT '', -- machine groups that get it in their boot menu, comma-separated; '' = all
        preload     INTEGER NOT NULL DEFAULT 0  -- 1 = machines that may boot it keep it on their SSD ahead of use
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

    -- Benchmark results (bench.rs). mac NULL = the server.
    CREATE TABLE IF NOT EXISTS bench(
        id     INTEGER PRIMARY KEY,
        at     INTEGER NOT NULL,
        mac    TEXT,
        kind   TEXT NOT NULL,
        value  REAL NOT NULL,
        detail TEXT NOT NULL DEFAULT ''
    );
    CREATE INDEX IF NOT EXISTS bench_mac ON bench(mac, kind, at);

    CREATE TABLE IF NOT EXISTS config(
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    INSERT OR IGNORE INTO config(key,value) VALUES('boot_timeout','3');
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

const IMAGE_COLS: &str = "id,name,os,is_default,boot_script,hash,cache_mode,active_version,base_mode,use_ssd,grps,preload";

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
        use_ssd: r.get::<_, i64>(9)? == 1,
        groups: r
            .get::<_, String>(10)?
            .split(',')
            .filter(|g| !g.is_empty())
            .map(String::from)
            .collect(),
        preload: r.get::<_, i64>(11)? == 1,
    })
}

fn e(err: rusqlite::Error) -> String {
    err.to_string()
}

/// "iqn.2026-01.local.broom-<8 hex>" — RandomState is seeded from OS randomness (std only).
fn random_iqn_base() -> String {
    use std::hash::{BuildHasher, Hasher};
    let r = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("iqn.2026-01.local.broom-{:08x}", r as u32)
}

/// The DB holds the session-signing secret, license keys and the guest password → owner-only (0600). Made 0600
/// before SQLite opens it (SQLite gives a new -wal/-shm the main file's mode), and re-applied to files from older
/// versions created with the default umask.
fn private_files(path: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if path.is_empty() || path == ":memory:" {
        return Ok(());
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)?;
    for f in [
        path.to_string(),
        format!("{path}-wal"),
        format!("{path}-shm"),
    ] {
        if std::path::Path::new(&f).exists() {
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

/// Schema version in `PRAGMA user_version`. A change to SCHEMA's tables bumps it and adds an `if v < N` step to
/// migrate() (SCHEMA itself always holds the latest layout, for new DBs).
const SCHEMA_VERSION: i64 = 3;

fn has_column(c: &Connection, table: &str, col: &str) -> rusqlite::Result<bool> {
    c.query_row(
        "SELECT 1 FROM pragma_table_info(?1) WHERE name=?2",
        [table, col],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
}

/// Bring an older DB up to SCHEMA_VERSION, in one transaction: any error is real and stops the open.
fn migrate(c: &mut Connection) -> rusqlite::Result<()> {
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v >= SCHEMA_VERSION {
        return Ok(());
    }
    let tx = c.transaction()?;
    if v < 2 {
        // Columns added over time (a new DB already has them all from SCHEMA); v2: images per machine group + preloading.
        const COLS: &[(&str, &str, &str)] = &[
            ("images", "boot_script", "TEXT"),
            ("images", "hash", "TEXT"),
            ("images", "cache_mode", "TEXT NOT NULL DEFAULT 'disk'"),
            ("images", "active_version", "TEXT"),
            ("images", "base_mode", "INTEGER NOT NULL DEFAULT 0"),
            ("images", "use_ssd", "INTEGER NOT NULL DEFAULT 1"),
            ("machines", "license_key", "TEXT"),
            ("machines", "license_state", "TEXT"),
            ("machines", "license_gen", "INTEGER NOT NULL DEFAULT 0"),
            ("machines", "license_result", "TEXT"),
            ("machines", "grp", "TEXT"),
            ("machines", "notes", "TEXT"),
            ("images", "grps", "TEXT NOT NULL DEFAULT ''"),
            ("images", "preload", "INTEGER NOT NULL DEFAULT 0"),
        ];
        for (table, col, ddl) in COLS {
            if !has_column(&tx, table, col)? {
                tx.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {ddl}"), [])?;
            }
        }
        if has_column(&tx, "images", "dataset")? {
            tx.execute("ALTER TABLE images DROP COLUMN dataset", [])?; // ZFS versioning removed
        }
    }
    if v < 3 {
        harden(&tx)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()
}

/// Rules the web pages always checked, now also kept by the database itself (two requests at once, an import, a
/// bug can't break them): one machine per MAC (any case), per fixed IP and per hostname (any case); one default
/// image; a machine's image exists. Rows from older versions that break one are fixed first — the oldest row keeps
/// the value, a later one loses it (logged) — or the indexes could not be made.
fn harden(tx: &Connection) -> rusqlite::Result<()> {
    let fixed = |what: &str, n: usize| {
        if n > 0 {
            tracing::warn!("database: {n} {what}");
        }
    };
    // MACs in one spelling (lower case); a second row of the same MAC in another case goes.
    let n = tx.execute(
        "DELETE FROM machines WHERE id NOT IN (SELECT MIN(id) FROM machines GROUP BY lower(mac))",
        [],
    )?;
    fixed("duplicate machine row(s) (same MAC, other case) removed", n);
    tx.execute(
        "UPDATE machines SET mac=lower(mac) WHERE mac<>lower(mac)",
        [],
    )?;
    let n = tx.execute(
        "UPDATE machines SET image_id=NULL WHERE image_id IS NOT NULL AND image_id NOT IN (SELECT id FROM images)",
        [],
    )?;
    fixed(
        "machine(s) pointed at a deleted image — now the default image",
        n,
    );
    let n = tx.execute(
        "UPDATE machines SET ip=NULL WHERE ip IS NOT NULL AND id NOT IN (SELECT MIN(id) FROM machines WHERE ip IS NOT NULL GROUP BY ip)",
        [],
    )?;
    fixed(
        "machine(s) shared a fixed IP with an older one — their IP cleared",
        n,
    );
    let n = tx.execute(
        "UPDATE machines SET hostname=NULL WHERE hostname IS NOT NULL AND id NOT IN \
         (SELECT MIN(id) FROM machines WHERE hostname IS NOT NULL GROUP BY lower(hostname))",
        [],
    )?;
    fixed(
        "machine(s) shared a hostname with an older one — their hostname cleared",
        n,
    );
    tx.execute(
        "UPDATE images SET is_default=0 WHERE is_default=1 AND id<>(SELECT MIN(id) FROM images WHERE is_default=1)",
        [],
    )?;
    tx.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS machines_mac_any_case ON machines(lower(mac));
         CREATE UNIQUE INDEX IF NOT EXISTS machines_one_ip ON machines(ip) WHERE ip IS NOT NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS machines_one_hostname ON machines(lower(hostname)) WHERE hostname IS NOT NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS images_one_default ON images(is_default) WHERE is_default=1;",
    )
}

impl Sqlite {
    pub fn open(path: &str) -> DbResult<Sqlite> {
        private_files(path).map_err(|err| format!("{path}: {err}"))?;
        let mut c = Connection::open(path).map_err(|err| format!("open {path}: {err}"))?;
        // SQLite silently falls back to read-only when it can't open for writing (e.g. a file owned by
        // another user in /tmp: fs.protected_regular blocks even root).
        if c.is_readonly(rusqlite::MAIN_DB).unwrap_or(false) {
            return Err(format!(
                "{path} is read-only (file owner/permissions; in /tmp only the file's owner can write it) \
                 — run from a fixed directory like /opt/bootrom, or fix the owner: chown root {path}"
            ));
        }
        c.execute_batch(SCHEMA).map_err(e)?;
        migrate(&mut c).map_err(|err| format!("{path}: schema migration: {err}"))?;
        // ProxyDHCP mode removed: off is the same for broom (the LAN's DHCP points clients at it).
        c.execute(
            "UPDATE config SET value='off' WHERE key='dhcp_mode' AND value='proxy'",
            [],
        )
        .map_err(e)?;
        // iSCSI IQN base, random per server, generated once (first open without it) and kept.
        c.execute(
            "INSERT OR IGNORE INTO config(key,value) VALUES('iqn_base',?1)",
            [random_iqn_base()],
        )
        .map_err(e)?;
        Ok(Sqlite { c: Mutex::new(c) })
    }

    fn c(&self) -> MutexGuard<'_, Connection> {
        self.c.lock()
    }

    /// One statement whose row count is not needed.
    fn run(&self, sql: &str, p: impl rusqlite::Params) -> DbResult<()> {
        self.c().execute(sql, p).map(|_| ()).map_err(e)
    }

    /// Every row of a query, each through `f`.
    fn all<T>(
        &self,
        sql: &str,
        p: impl rusqlite::Params,
        f: impl FnMut(&Row) -> rusqlite::Result<T>,
    ) -> DbResult<Vec<T>> {
        let c = self.c();
        let mut s = c.prepare_cached(sql).map_err(e)?;
        let rows = s.query_map(p, f).map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    fn one_image(&self, sql_where: &str, p: impl rusqlite::Params) -> DbResult<Option<Image>> {
        self.c()
            .query_row(
                &format!("SELECT {IMAGE_COLS} FROM images WHERE {sql_where}"),
                p,
                image_row,
            )
            .optional()
            .map_err(e)
    }
}

impl Db for Sqlite {
    fn get_config(&self, key: &str, default: &str) -> String {
        // Cached statement: DHCP reads ~17 keys per packet.
        self.c()
            .prepare_cached("SELECT value FROM config WHERE key=?1")
            .and_then(|mut s| s.query_row([key], |r| r.get(0)))
            .unwrap_or_else(|_| default.to_string())
    }

    fn update_config(
        &self,
        key: &str,
        default: &str,
        f: &mut dyn FnMut(&str) -> Result<Option<String>, String>,
    ) -> DbResult<()> {
        // One connection behind one lock (self.c()) + an immediate transaction: no other write in between, from
        // this process or another one.
        let mut c = self.c();
        let t = c
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(e)?;
        let cur: Option<String> = t
            .query_row("SELECT value FROM config WHERE key=?1", [key], |r| r.get(0))
            .optional()
            .map_err(e)?;
        if let Some(new) = f(cur.as_deref().unwrap_or(default))? {
            t.execute(
                "INSERT INTO config(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [key, &new],
            )
            .map_err(e)?;
        }
        t.commit().map_err(e)
    }

    fn delete_config(&self, key: &str) -> DbResult<()> {
        self.run("DELETE FROM config WHERE key=?1", [key])
    }

    fn set_config(&self, key: &str, value: &str) -> DbResult<()> {
        self.run(
            "INSERT INTO config(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [key, value],
        )
    }

    fn images(&self) -> DbResult<Vec<Image>> {
        self.all(
            &format!("SELECT {IMAGE_COLS} FROM images ORDER BY id"),
            [],
            image_row,
        )
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
        // image gets it next. Drop the link. The default (if it was this one) moves only to the oldest PUBLISHED image of
        // the same OS — never auto-boot another OS or an image with no boot script; none → no default (the menu waits).
        let mut c = self.c();
        let t = c.transaction().map_err(e)?;
        let gone: Option<(String, bool)> = t
            .query_row("SELECT os,is_default FROM images WHERE id=?1", [id], |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)? == 1))
            })
            .ok();
        t.execute("UPDATE machines SET image_id=NULL WHERE image_id=?1", [id])
            .map_err(e)?;
        t.execute("DELETE FROM images WHERE id=?1", [id])
            .map_err(e)?;
        if let Some((os, true)) = gone {
            t.execute(
                "UPDATE images SET is_default=1 WHERE id=(SELECT MIN(id) FROM images WHERE os=?1 AND boot_script IS NOT NULL)",
                [os],
            )
            .map_err(e)?;
        }
        t.commit().map_err(e)
    }

    fn set_default_image(&self, id: i64) -> DbResult<()> {
        let mut c = self.c();
        let t = c.transaction().map_err(e)?;
        t.execute("UPDATE images SET is_default=0", []).map_err(e)?;
        t.execute("UPDATE images SET is_default=1 WHERE id=?1", [id])
            .map_err(e)?;
        t.commit().map_err(e)
    }

    fn set_boot_script(&self, id: i64, boot_script: &str) -> DbResult<()> {
        self.run(
            "UPDATE images SET boot_script=?1 WHERE id=?2",
            params![boot_script, id],
        )
    }

    fn set_published(&self, id: i64, boot_script: &str, hash: &str) -> DbResult<()> {
        self.run(
            "UPDATE images SET boot_script=?1, hash=?2 WHERE id=?3",
            params![boot_script, hash, id],
        )
    }

    fn set_cache_mode(&self, id: i64, mode: &str) -> DbResult<()> {
        self.run(
            "UPDATE images SET cache_mode=?1 WHERE id=?2",
            params![mode, id],
        )
    }

    fn set_base_mode(&self, id: i64, on: bool) -> DbResult<()> {
        self.run(
            "UPDATE images SET base_mode=?1 WHERE id=?2",
            params![on as i64, id],
        )
    }

    fn set_use_ssd(&self, id: i64, on: bool) -> DbResult<()> {
        self.run(
            "UPDATE images SET use_ssd=?1 WHERE id=?2",
            params![on as i64, id],
        )
    }

    fn set_image_groups(&self, id: i64, groups: &[String]) -> DbResult<()> {
        self.run(
            "UPDATE images SET grps=?1 WHERE id=?2",
            params![groups.join(","), id],
        )
    }

    fn set_preload(&self, id: i64, on: bool) -> DbResult<()> {
        self.run(
            "UPDATE images SET preload=?1 WHERE id=?2",
            params![on as i64, id],
        )
    }

    fn set_active_version(&self, id: i64, version: Option<&str>) -> DbResult<()> {
        self.run(
            "UPDATE images SET active_version=?1 WHERE id=?2",
            params![version, id],
        )
    }

    fn machines(&self) -> DbResult<Vec<Machine>> {
        self.all(
            "SELECT id,mac,ip,hostname,image_id,license_key,license_state,license_gen,license_result,grp,notes
             FROM machines ORDER BY hostname",
            [],
            |r| {
                let key: Option<String> = r.get(5)?;
                Ok(Machine {
                    id: r.get(0)?,
                    mac: r.get(1)?,
                    ip: r.get(2)?,
                    hostname: r.get(3)?,
                    image_id: r.get(4)?,
                    license_tail: key
                        .as_ref()
                        .map(|k| k[k.len().saturating_sub(5)..].to_string()),
                    license_key: key,
                    license_state: r.get(6)?,
                    license_gen: r.get(7)?,
                    license_result: r.get(8)?,
                    grp: r.get(9)?,
                    notes: r.get(10)?,
                })
            },
        )
    }

    fn update_machine(&self, m: &Machine) -> DbResult<()> {
        self.run(
            "UPDATE machines SET mac=?1, ip=?2, hostname=?3, grp=?4, notes=?5, image_id=?6 WHERE id=?7",
            params![m.mac, m.ip, m.hostname, m.grp, m.notes, m.image_id, m.id],
        )
    }

    fn delete_machine(&self, machine_id: i64) -> DbResult<()> {
        let mut c = self.c();
        let t = c.transaction().map_err(e)?;
        // Its reported hardware IDs go with it.
        t.execute(
            "DELETE FROM machine_hw WHERE lower(mac)=(SELECT lower(mac) FROM machines WHERE id=?1)",
            [machine_id],
        )
        .map_err(e)?;
        t.execute("DELETE FROM machines WHERE id=?1", [machine_id])
            .map_err(e)?;
        t.commit().map_err(e)
    }

    fn set_machine_image(&self, machine_id: i64, image_id: Option<i64>) -> DbResult<()> {
        self.run(
            "UPDATE machines SET image_id=?1 WHERE id=?2",
            params![image_id, machine_id],
        )
    }

    fn set_license(&self, machine_id: i64, key: Option<&str>) -> DbResult<()> {
        let sql = if key.is_some() {
            "UPDATE machines SET license_key=?1, license_state='armed', license_gen=license_gen+1, license_result=NULL WHERE id=?2"
        } else {
            "UPDATE machines SET license_key=?1, license_state=NULL, license_result=NULL WHERE id=?2"
        };
        self.run(sql, params![key, machine_id])
    }

    fn rearm_license(&self, machine_id: i64) -> DbResult<()> {
        self.run(
            "UPDATE machines SET license_state='armed', license_gen=license_gen+1, license_result=NULL
             WHERE id=?1 AND license_key IS NOT NULL",
            [machine_id],
        )
    }

    fn rearm_quiet(&self, machine_id: i64) -> DbResult<bool> {
        self.c()
            .execute(
                "UPDATE machines SET license_state='armed' WHERE id=?1 AND license_state='sent'",
                [machine_id],
            )
            .map(|n| n > 0)
            .map_err(e)
    }

    fn take_license(&self, machine_id: i64) -> DbResult<Option<String>> {
        let c = self.c(); // one connection lock → check + flip are atomic
        let n = c
            .execute(
                "UPDATE machines SET license_state='sent' WHERE id=?1 AND license_state='armed'",
                [machine_id],
            )
            .map_err(e)?;
        if n == 0 {
            return Ok(None);
        }
        c.query_row(
            "SELECT license_key FROM machines WHERE id=?1",
            [machine_id],
            |r| r.get(0),
        )
        .map_err(e)
    }

    fn set_license_result(&self, machine_id: i64, result: &str) -> DbResult<()> {
        self.run(
            "UPDATE machines SET license_result=?1 WHERE id=?2",
            params![result, machine_id],
        )
    }

    fn set_machine_group(&self, machine_id: i64, grp: Option<&str>) -> DbResult<()> {
        self.run(
            "UPDATE machines SET grp=?1 WHERE id=?2",
            params![grp, machine_id],
        )
    }

    fn put_machine_hw(&self, mac: &str, hwids: &[String], seen: i64) -> DbResult<()> {
        self.run(
            "INSERT INTO machine_hw(mac,hwids,seen) VALUES(?1,?2,?3)
             ON CONFLICT(mac) DO UPDATE SET hwids=excluded.hwids, seen=excluded.seen",
            params![mac, hwids.join("\n"), seen],
        )
    }

    fn machine_hw(&self) -> DbResult<Vec<(String, Vec<String>)>> {
        self.all("SELECT mac,hwids FROM machine_hw", [], |r| {
            Ok((r.get::<_, String>(0)?, lines(&r.get::<_, String>(1)?)))
        })
    }

    fn drivers(&self) -> DbResult<Vec<Driver>> {
        self.all(
            "SELECT id,name,sha256,size,hwids,all_machines,groups,created FROM drivers ORDER BY name",
            [],
            |r| {
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
            },
        )
    }

    fn put_driver(
        &self,
        name: &str,
        sha256: &str,
        size: u64,
        hwids: &[String],
        created: i64,
    ) -> DbResult<()> {
        self.run(
            "INSERT INTO drivers(name,sha256,size,hwids,created) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(name) DO UPDATE SET sha256=excluded.sha256, size=excluded.size,
               hwids=excluded.hwids, created=excluded.created",
            params![name, sha256, size as i64, hwids.join("\n"), created],
        )
    }

    fn set_driver_targets(&self, id: i64, all_machines: bool, groups: &[String]) -> DbResult<()> {
        self.run(
            "UPDATE drivers SET all_machines=?1, groups=?2 WHERE id=?3",
            params![all_machines as i64, groups.join("\n"), id],
        )
    }

    fn delete_driver(&self, id: i64) -> DbResult<()> {
        self.run("DELETE FROM drivers WHERE id=?1", [id])
    }

    fn add_machine(&self, mac: &str, ip: Option<&str>, hostname: Option<&str>) -> DbResult<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO machines(mac,ip,hostname) VALUES(?1,?2,?3)",
            params![mac, ip, hostname],
        )
        .map_err(e)?;
        Ok(c.last_insert_rowid())
    }

    fn leases(&self) -> DbResult<Vec<Lease>> {
        self.all(
            "SELECT mac,ip,hostname,expires,source FROM leases",
            [],
            |r| {
                Ok(Lease {
                    mac: r.get(0)?,
                    ip: r.get(1)?,
                    hostname: r.get(2)?,
                    expires: r.get(3)?,
                    source: r.get(4)?,
                })
            },
        )
    }

    fn put_lease(&self, l: &Lease) -> DbResult<()> {
        self.run(
            "INSERT INTO leases(mac,ip,hostname,expires,source) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(mac) DO UPDATE SET ip=COALESCE(excluded.ip,ip),
               hostname=COALESCE(excluded.hostname,hostname), expires=excluded.expires, source=excluded.source",
            params![l.mac, l.ip, l.hostname, l.expires, l.source],
        )
    }

    fn delete_lease(&self, mac: &str) -> DbResult<()> {
        self.run("DELETE FROM leases WHERE mac=?1", [mac])
    }

    fn prune_leases(&self, cutoff: i64) -> DbResult<usize> {
        self.c()
            .execute("DELETE FROM leases WHERE expires < ?1", [cutoff])
            .map_err(e)
    }

    fn add_bench(&self, r: &BenchRow) -> DbResult<()> {
        self.run(
            "INSERT INTO bench(at,mac,kind,value,detail) VALUES(?1,?2,?3,?4,?5)",
            params![r.at, r.mac, r.kind, r.value, r.detail],
        )
    }

    fn bench_rows(&self, mac: Option<&str>, limit: usize) -> DbResult<Vec<BenchRow>> {
        // Two statements: `?1 IS NULL OR mac=?1` would keep SQLite off the bench_mac index for one machine's rows.
        let sql = match mac {
            Some(_) => {
                "SELECT at,mac,kind,value,detail FROM bench WHERE mac=?1 ORDER BY at DESC, id DESC LIMIT ?2"
            }
            None => {
                "SELECT at,mac,kind,value,detail FROM bench WHERE ?1 IS NULL ORDER BY at DESC, id DESC LIMIT ?2"
            }
        };
        self.all(sql, params![mac, limit as i64], bench_row)
    }

    fn bench_latest(&self) -> DbResult<Vec<BenchRow>> {
        // One pass over the table (not a lookup per row: up to 20k rows, under the lock DHCP waits on).
        self.all(
            "SELECT at,mac,kind,value,detail FROM (SELECT *, ROW_NUMBER() OVER (PARTITION BY mac, kind
             ORDER BY at DESC, id DESC) AS n FROM bench) WHERE n=1",
            [],
            bench_row,
        )
    }

    fn prune_bench(&self, cutoff: i64, keep: usize) -> DbResult<usize> {
        let c = self.c();
        let old = c
            .execute("DELETE FROM bench WHERE at < ?1", [cutoff])
            .map_err(e)?;
        let over = c
            .execute("DELETE FROM bench WHERE id NOT IN (SELECT id FROM bench ORDER BY at DESC, id DESC LIMIT ?1)", [keep as i64])
            .map_err(e)?;
        Ok(old + over)
    }
}

fn bench_row(r: &Row) -> rusqlite::Result<BenchRow> {
    Ok(BenchRow {
        at: r.get(0)?,
        mac: r.get(1)?,
        kind: r.get(2)?,
        value: r.get(3)?,
        detail: r.get(4)?,
    })
}

#[cfg(test)]
#[path = "../../tests/unit/db/sqlite.rs"]
mod tests;
