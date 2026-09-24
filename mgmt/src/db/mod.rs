// db — storage driver interface. The rest of the app only uses the `Db` trait + the row types
// below; all SQL lives in a driver file. Built-in: SQLite (db/sqlite.rs).
// Adding MySQL/Postgres = a new file implementing `Db` + one branch in `open()`.
use serde::Serialize;

mod sqlite;

pub type DbResult<T> = Result<T, String>;

#[derive(Serialize, Clone, Debug)]
pub struct Image {
    pub id: i64,
    pub name: String,
    pub os: String,
    /// Version (versions.rs) image.img currently equals; None = changed since / never snapshotted.
    pub active_version: Option<String>,
    pub is_default: bool,
    pub boot_script: Option<String>,
    pub hash: Option<String>,
    /// Where a Linux golden is served from: "disk" | "zram".
    pub cache_mode: String,
}

pub struct NewImage<'a> {
    pub name: &'a str,
    pub os: &'a str,
    pub boot_script: Option<&'a str>,
    pub cache_mode: &'a str,
}

#[derive(Serialize, Clone, Debug)]
pub struct Machine {
    pub id: i64,
    pub mac: String,
    pub ip: Option<String>,
    pub hostname: Option<String>,
    pub image_id: Option<i64>,
}

/// DHCP lease (dhcp.rs). source "full" = handed out by us; "proxy" = PXE client seen in proxy mode.
#[derive(Clone, Debug, PartialEq)]
pub struct Lease {
    pub mac: String,
    pub ip: Option<String>,
    pub hostname: Option<String>,
    /// Unix seconds.
    pub expires: i64,
    pub source: String,
}

/// Everything the app stores. Implementations must be usable from many threads (axum handlers,
/// DHCP listeners, blocking publish jobs).
pub trait Db: Send + Sync {
    // --- config (key/value) ---
    fn get_config(&self, key: &str, default: &str) -> String;
    fn set_config(&self, key: &str, value: &str) -> DbResult<()>;

    // --- images ---
    /// All images, by id.
    fn images(&self) -> DbResult<Vec<Image>>;
    fn image(&self, id: i64) -> DbResult<Option<Image>>;
    fn image_by_name(&self, name: &str) -> DbResult<Option<Image>>;
    /// Returns the new id.
    fn add_image(&self, img: &NewImage) -> DbResult<i64>;
    fn delete_image(&self, id: i64) -> DbResult<()>;
    /// Make `id` the only default image.
    fn set_default_image(&self, id: i64) -> DbResult<()>;
    fn set_boot_script(&self, id: i64, boot_script: &str) -> DbResult<()>;
    /// Result of a publish: boot script + golden hash.
    fn set_published(&self, id: i64, boot_script: &str, hash: &str) -> DbResult<()>;
    fn set_cache_mode(&self, id: i64, mode: &str) -> DbResult<()>;
    fn set_active_version(&self, id: i64, version: Option<&str>) -> DbResult<()>;

    // --- machines ---
    /// All machines, by hostname.
    fn machines(&self) -> DbResult<Vec<Machine>>;
    /// Returns the new id.
    fn add_machine(&self, mac: &str, ip: Option<&str>, hostname: Option<&str>) -> DbResult<i64>;
    fn assign_image(&self, machine_id: i64, image_id: i64) -> DbResult<()>;

    // --- DHCP leases ---
    fn leases(&self) -> DbResult<Vec<Lease>>;
    /// Insert or update by mac; a None ip/hostname keeps the stored value.
    fn put_lease(&self, lease: &Lease) -> DbResult<()>;
    fn delete_lease(&self, mac: &str) -> DbResult<()>;
}

/// Open the database named by `url`: a plain path or `sqlite://path` → SQLite.
/// (Future drivers: `mysql://…`, `postgres://…`.) Creates/migrates the schema.
pub fn open(url: &str) -> DbResult<Box<dyn Db>> {
    match url.split_once("://") {
        None => Ok(Box::new(sqlite::Sqlite::open(url)?)),
        Some(("sqlite", path)) => Ok(Box::new(sqlite::Sqlite::open(path)?)),
        Some((scheme, _)) => Err(format!("unsupported database '{scheme}://' (built-in drivers: sqlite)")),
    }
}

/// Database location: env BOOTROM_DB (e.g. `sqlite:///var/lib/bootrom/bootrom.db`), default
/// `<home>/bootrom.db` (next to the binary).
pub fn url() -> String {
    std::env::var("BOOTROM_DB").unwrap_or_else(|_| crate::home().join("bootrom.db").to_string_lossy().into_owned())
}
