// db.rs — SQLite schema + open. Source of machine + image + config identity.
use rusqlite::{Connection, Result};

pub fn open(path: &str) -> Result<Connection> {
    let c = Connection::open(path)?;
    c.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;

        CREATE TABLE IF NOT EXISTS images(
            id          INTEGER PRIMARY KEY,
            name        TEXT UNIQUE NOT NULL,
            os          TEXT NOT NULL,           -- 'linux'
            dataset     TEXT,                    -- ZFS dataset holding the golden
            is_default  INTEGER NOT NULL DEFAULT 0,
            boot_script TEXT,                    -- iPXE boot snippet (kernel/initrd); empty = TODO
            hash        TEXT,                    -- sha256 of the image (version check for the SSD cache)
            cache_mode  TEXT NOT NULL DEFAULT 'disk'  -- where the golden is kept: 'disk' | 'zram'
        );

        CREATE TABLE IF NOT EXISTS machines(
            id       INTEGER PRIMARY KEY,
            mac      TEXT UNIQUE NOT NULL,
            ip       TEXT,
            hostname TEXT,
            image_id INTEGER REFERENCES images(id)
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
        "#,
    )?;
    // Old DB migration: add columns, ignore if they already exist.
    let _ = c.execute("ALTER TABLE images ADD COLUMN boot_script TEXT", []);
    let _ = c.execute("ALTER TABLE images ADD COLUMN hash TEXT", []);
    let _ = c.execute("ALTER TABLE images ADD COLUMN cache_mode TEXT NOT NULL DEFAULT 'disk'", []);
    // iSCSI IQN base, random per server, generated once (first open without it) and kept.
    c.execute("INSERT OR IGNORE INTO config(key,value) VALUES('iqn_base',?1)", [random_iqn_base()])?;
    Ok(c)
}

/// "iqn.2026-01.local.broom-<8 hex>" — RandomState is seeded from OS randomness (std only).
fn random_iqn_base() -> String {
    use std::hash::{BuildHasher, Hasher};
    let r = std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("iqn.2026-01.local.broom-{:08x}", r as u32)
}

/// Get a config value, with a default.
pub fn get_config(c: &Connection, key: &str, default: &str) -> String {
    c.query_row("SELECT value FROM config WHERE key=?1", [key], |r| r.get(0))
        .unwrap_or_else(|_| default.to_string())
}

#[cfg(test)]
#[test]
fn iqn_base_random_and_kept() {
    let p = std::env::temp_dir().join("broom_test_iqn.db");
    let _ = std::fs::remove_file(&p);
    let p = p.to_str().unwrap();
    let a = get_config(&open(p).unwrap(), "iqn_base", "");
    assert!(a.starts_with("iqn.2026-01.local.broom-") && a.len() == 32, "{a}");
    assert_eq!(get_config(&open(p).unwrap(), "iqn_base", ""), a, "kept on reopen");
    assert_ne!(get_config(&open(":memory:").unwrap(), "iqn_base", ""), a, "new DB → new base");
    for s in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{p}{s}"));
    }
}

pub fn set_config(c: &Connection, key: &str, value: &str) -> Result<()> {
    c.execute(
        "INSERT INTO config(key,value) VALUES(?1,?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [key, value],
    )?;
    Ok(())
}
