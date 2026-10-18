// metrics.rs — per-machine hardware health (CPU %, RAM %, temperatures, disks) the Remote agent (broom-vnc) gathers
// and pushes over its own WebSocket (vnc.rs routes a "metrics" text message here). No HTTP endpoint and no ping: it
// rides the connection the agent already keeps open. The newest reading per machine is kept; one older than STALE is
// treated as gone (the machine is off / the agent stopped).
use crate::now_secs as now;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

/// A reading older than this is hidden (the agent sends one about every 15 s).
const STALE: u64 = 60;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Disk {
    pub name: String,
    /// "Healthy" / "Warning" / "Unhealthy" (Windows HealthStatus).
    pub health: String,
    /// °C from the drive's own sensor.
    #[serde(default)]
    pub temp: Option<i16>,
    /// SMART reallocated sectors (bad sectors moved aside).
    #[serde(default)]
    pub realloc: Option<u32>,
    /// SMART current-pending sectors (unstable, not yet reallocated).
    #[serde(default)]
    pub pending: Option<u32>,
    /// SMART predicts the drive is failing.
    #[serde(default)]
    pub predict_fail: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Metrics {
    pub cpu: u8,
    pub ram: u8,
    /// ACPI thermal-zone temperatures (°C), unlabeled — no kernel driver, so not per-core / per-board.
    #[serde(default)]
    pub temps: Vec<i16>,
    #[serde(default)]
    pub disks: Vec<Disk>,
    /// Free bytes on the system drive.
    #[serde(default)]
    pub free: Option<u64>,
    /// Unix seconds it was received (set here, never trusted from the agent).
    #[serde(skip_deserializing, default)]
    pub at: u64,
}

static METRICS: LazyLock<Mutex<HashMap<String, Metrics>>> = LazyLock::new(Default::default);

/// Store the agent's JSON reading for `mac` (ignored if it doesn't parse).
pub fn store(mac: &str, json: &str) {
    if let Ok(mut m) = serde_json::from_str::<Metrics>(json) {
        m.cpu = m.cpu.min(100);
        m.ram = m.ram.min(100);
        m.at = now();
        METRICS.lock().insert(mac.to_lowercase(), m);
    }
}

/// The machine's newest reading, if recent enough.
pub fn get(mac: &str) -> Option<Metrics> {
    let g = METRICS.lock();
    g.get(&mac.to_lowercase())
        .filter(|m| now().saturating_sub(m.at) <= STALE)
        .cloned()
}

/// The machine is gone (deleted).
pub fn forget(mac: &str) {
    METRICS.lock().remove(&mac.to_lowercase());
}

#[cfg(test)]
#[path = "../tests/unit/metrics.rs"]
mod tests;
