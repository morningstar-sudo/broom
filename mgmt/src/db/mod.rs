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
    /// Windows retail product key. Never serialized (the web admin has no login) — see `license_tail`.
    #[serde(skip)]
    pub license_key: Option<String>,
    /// Last 5 characters of the key, for the web.
    pub license_tail: Option<String>,
    /// "armed" = Windows may fetch it once | "sent" = fetched (re-arm to allow again) | None = no key.
    pub license_state: Option<String>,
    /// Bumped on set / re-arm → the stage rebuilds base so broom-done fetches the key.
    pub license_gen: i64,
    /// slmgr output the client reported after installing the key.
    pub license_result: Option<String>,
    /// Free-text group (e.g. "VIP") — driver packages can be assigned to a group.
    pub grp: Option<String>,
    /// Free text for the admin (seat, hardware notes…).
    pub notes: Option<String>,
}

/// Windows driver package (drivers.rs): an extracted driver folder, served as a .tar.gz to the stage.
#[derive(Serialize, Clone, Debug)]
pub struct Driver {
    pub id: i64,
    pub name: String,
    pub sha256: String,
    /// Bytes of the .tar.gz.
    pub size: u64,
    /// Hardware IDs read from the .inf files (`PCI\VEN_xxxx&DEV_yyyy`, `USB\VID_xxxx&PID_yyyy`).
    pub hwids: Vec<String>,
    pub all_machines: bool,
    /// Machine groups it is assigned to.
    pub groups: Vec<String>,
    /// Unix seconds.
    pub created: i64,
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
    /// Replace the editable fields of a machine (mac must stay unique).
    fn update_machine(&self, m: &Machine) -> DbResult<()>;
    fn delete_machine(&self, machine_id: i64) -> DbResult<()>;
    /// The machine's own default image in the boot menu (None = the global default).
    fn set_machine_image(&self, machine_id: i64, image_id: Option<i64>) -> DbResult<()>;
    /// Set a license key (→ armed, gen + 1) or remove it (None → no key, no state).
    fn set_license(&self, machine_id: i64, key: Option<&str>) -> DbResult<()>;
    /// Allow one more delivery of the stored key (→ armed, gen + 1).
    fn rearm_license(&self, machine_id: i64) -> DbResult<()>;
    /// armed → sent atomically; returns the key only if it WAS armed (two requests never both get it).
    fn take_license(&self, machine_id: i64) -> DbResult<Option<String>>;
    fn set_license_result(&self, machine_id: i64, result: &str) -> DbResult<()>;
    fn set_machine_group(&self, machine_id: i64, grp: Option<&str>) -> DbResult<()>;
    /// Hardware IDs a machine's stage reported (by MAC, registered or not) + when.
    fn put_machine_hw(&self, mac: &str, hwids: &[String], seen: i64) -> DbResult<()>;
    fn machine_hw(&self) -> DbResult<Vec<(String, Vec<String>)>>;

    // --- driver packages ---
    fn drivers(&self) -> DbResult<Vec<Driver>>;
    /// Insert or replace the files of a package by name; a re-upload keeps its targets (all / groups).
    fn put_driver(&self, name: &str, sha256: &str, size: u64, hwids: &[String], created: i64) -> DbResult<()>;
    fn set_driver_targets(&self, id: i64, all_machines: bool, groups: &[String]) -> DbResult<()>;
    fn delete_driver(&self, id: i64) -> DbResult<()>;

    // --- DHCP leases ---
    fn leases(&self) -> DbResult<Vec<Lease>>;
    /// Insert or update by mac; a None ip/hostname keeps the stored value.
    fn put_lease(&self, lease: &Lease) -> DbResult<()>;
    fn delete_lease(&self, mac: &str) -> DbResult<()>;
    /// Delete leases whose expiry is before `cutoff` (unix s). Returns how many were removed. Keeps the table from
    /// growing without bound when many MACs are seen (a DHCP flood).
    fn prune_leases(&self, cutoff: i64) -> DbResult<usize>;
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
