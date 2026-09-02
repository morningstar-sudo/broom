// zfs.rs — helper shell-out ZFS cho version/rollback image (M6). Không tự viết COW.
use std::process::Command;

pub fn snapshot(dataset: &str, snap: &str) -> std::io::Result<bool> {
    Ok(Command::new("zfs")
        .args(["snapshot", &format!("{dataset}@{snap}")])
        .status()?
        .success())
}

pub fn rollback(dataset: &str, snap: &str) -> std::io::Result<bool> {
    Ok(Command::new("zfs")
        .args(["rollback", "-r", &format!("{dataset}@{snap}")])
        .status()?
        .success())
}

/// Danh sách snapshot của 1 dataset (tên đầy đủ dataset@snap).
pub fn list_snapshots(dataset: &str) -> std::io::Result<Vec<String>> {
    let out = Command::new("zfs")
        .args(["list", "-H", "-t", "snapshot", "-o", "name", "-r", dataset])
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect())
}
