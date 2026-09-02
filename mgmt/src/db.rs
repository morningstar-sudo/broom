// db.rs — SQLite schema + open. Nguồn định danh máy trạm + image + config.
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
            dataset     TEXT,                    -- ZFS dataset chứa golden
            is_default  INTEGER NOT NULL DEFAULT 0,
            boot_script TEXT,                    -- đoạn iPXE boot (kernel/initrd); rỗng = TODO
            hash        TEXT,                    -- sha256 image (so version cho cache SSD)
            cache_mode  TEXT NOT NULL DEFAULT 'disk'  -- golden lưu ở đâu: 'disk' | 'zram'
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
        INSERT OR IGNORE INTO config(key,value) VALUES('ltsp_user','khach');
        INSERT OR IGNORE INTO config(key,value) VALUES('ltsp_password','123456');
        INSERT OR IGNORE INTO config(key,value) VALUES('ltsp_ssd_dev','auto');
        INSERT OR IGNORE INTO config(key,value) VALUES('ltsp_ssd_mount','/games');
        INSERT OR IGNORE INTO config(key,value) VALUES('ltsp_image_cache','off');
        INSERT OR IGNORE INTO config(key,value) VALUES('ltsp_user_sudo','1');
        "#,
    )?;
    // Migration DB cũ: thêm cột, bỏ qua nếu đã có.
    let _ = c.execute("ALTER TABLE images ADD COLUMN boot_script TEXT", []);
    let _ = c.execute("ALTER TABLE images ADD COLUMN hash TEXT", []);
    let _ = c.execute("ALTER TABLE images ADD COLUMN cache_mode TEXT NOT NULL DEFAULT 'disk'", []);
    Ok(c)
}

/// Lấy 1 giá trị config, có default.
pub fn get_config(c: &Connection, key: &str, default: &str) -> String {
    c.query_row("SELECT value FROM config WHERE key=?1", [key], |r| r.get(0))
        .unwrap_or_else(|_| default.to_string())
}

pub fn set_config(c: &Connection, key: &str, value: &str) -> Result<()> {
    c.execute(
        "INSERT INTO config(key,value) VALUES(?1,?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [key, value],
    )?;
    Ok(())
}
