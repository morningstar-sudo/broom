// iscsi.rs — shared read-only iSCSI targets for Linux goldens, configured straight through the
// kernel LIO configfs tree (/sys/kernel/config/target). Replaces targetcli + target.service: the
// kernel does the iSCSI I/O, this file only creates/removes the objects. configfs is not
// persistent → main() re-exports every published image at start (restore_targets in publish.rs).
//
// Per image:  core/fileio_0/<name>  (file golden)  or  core/iblock_0/<name>  (zram block device)
//             iscsi/<iqn>/tpgt_1/lun/lun_0/<link → backstore>, np/0.0.0.0:3260,
//             attrib: no auth, dynamic ACLs, demo-mode write-protect (read-only for every initiator).
use std::path::{Path, PathBuf};
use std::process::Command;

pub enum Backing<'a> {
    /// Golden image file (cache_mode=disk).
    File { path: &'a str, size: u64 },
    /// Block device holding the golden (cache_mode=zram → /dev/zramN).
    Block { dev: &'a str },
}

pub struct Lio {
    root: PathBuf,
}

const CONFIGFS: &str = "/sys/kernel/config";

fn write(p: &Path, v: &str) -> Result<(), String> {
    std::fs::write(p, v).map_err(|e| format!("write {} = {v:?}: {e}", p.display()))
}

fn mkdir(p: &Path) -> Result<(), String> {
    std::fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))
}

/// configfs drops a directory's default groups (attrib/, param/…) together with it on rmdir; a
/// plain directory (unit tests on a temp tree) needs remove_dir_all to behave the same.
fn rmdir(p: &Path) {
    if std::fs::remove_dir(p).is_err() && !p.starts_with(CONFIGFS) {
        let _ = std::fs::remove_dir_all(p);
    }
}

fn subdirs(p: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(p)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir() && !p.is_symlink()).collect())
        .unwrap_or_default()
}

impl Lio {
    /// The live kernel tree: loads the LIO modules (best effort — they may be built in) and mounts
    /// configfs if needed.
    pub fn system() -> Result<Lio, String> {
        let mods = ["target_core_mod", "target_core_file", "target_core_iblock", "iscsi_target_mod"];
        for m in mods {
            let _ = Command::new("modprobe").arg(m).status();
        }
        let root = Path::new(CONFIGFS).join("target");
        if !root.is_dir() {
            let _ = Command::new("mount").args(["-t", "configfs", "configfs", CONFIGFS]).status();
        }
        if !root.join("core").is_dir() {
            return Err(format!("{} missing — kernel without the LIO target ({})?", root.display(), mods.join(", ")));
        }
        Ok(Lio { root })
    }

    #[cfg(test)]
    fn at(root: &Path) -> Lio {
        Lio { root: root.to_path_buf() }
    }

    /// Create (or re-create) the backstore + target for one image.
    pub fn export(&self, name: &str, backing: Backing, iqn: &str) -> Result<(), String> {
        self.remove(name, iqn);
        // 1. Backstore.
        let (dev, control, udev) = match backing {
            Backing::File { path, size } => {
                (self.root.join("core/fileio_0").join(name), format!("fd_dev_name={path},fd_dev_size={size}"), path)
            }
            Backing::Block { dev } => {
                (self.root.join("core/iblock_0").join(name), format!("udev_path={dev},readonly=1"), dev)
            }
        };
        mkdir(&dev)?;
        write(&dev.join("control"), &control)?;
        write(&dev.join("udev_path"), udev)?;
        write(&dev.join("enable"), "1")?;
        // 2. Target: one TPG, LUN 0 → backstore, portal on every address.
        let tpg = self.root.join("iscsi").join(iqn).join("tpgt_1");
        let lun = tpg.join("lun/lun_0");
        mkdir(&lun)?;
        std::os::unix::fs::symlink(&dev, lun.join("golden")).map_err(|e| format!("link LUN 0 → {}: {e}", dev.display()))?;
        mkdir(&tpg.join("np/0.0.0.0:3260"))?;
        mkdir(&tpg.join("attrib"))?;
        for (k, v) in [("authentication", "0"), ("generate_node_acls", "1"), ("cache_dynamic_acls", "1"), ("demo_mode_write_protect", "1")] {
            write(&tpg.join("attrib").join(k), v)?;
        }
        write(&tpg.join("enable"), "1")
    }

    /// Target already configured (configfs survives an mgmt restart, not a reboot).
    pub fn has_target(&self, iqn: &str) -> bool {
        self.root.join("iscsi").join(iqn).join("tpgt_1").is_dir()
    }

    /// Remove the target `iqn` and any backstore called `name` (any HBA — also ones targetcli made).
    /// Missing objects are fine.
    pub fn remove(&self, name: &str, iqn: &str) {
        let t = self.root.join("iscsi").join(iqn);
        for tpg in subdirs(&t).into_iter().filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("tpgt_"))) {
            let _ = std::fs::write(tpg.join("enable"), "0");
            for lun in subdirs(&tpg.join("lun")) {
                for e in std::fs::read_dir(&lun).into_iter().flatten().flatten() {
                    if e.path().is_symlink() {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
                rmdir(&lun);
            }
            for np in subdirs(&tpg.join("np")) {
                rmdir(&np);
            }
            rmdir(&tpg);
        }
        rmdir(&t);
        for hba in subdirs(&self.root.join("core")) {
            let d = hba.join(name);
            if d.is_dir() {
                rmdir(&d);
            }
        }
    }

    /// IQNs of every configured iSCSI target (configfs dir names under iscsi/).
    pub fn list_iqns(&self) -> Vec<String> {
        subdirs(&self.root.join("iscsi"))
            .into_iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect()
    }
}

/// Any iSCSI initiator currently connected to the portal (all targets share :3260, so this is portal-wide, not
/// per-target). Used to decide when it is safe to tear down a superseded target or republish a disk image (M9).
pub fn any_session() -> bool {
    match std::process::Command::new("ss")
        .args(["-H", "-tn", "state", "established", "( sport = :3260 )"])
        .output()
    {
        Ok(o) => o.stdout.iter().filter(|&&b| b == b'\n').count() > 0,
        Err(_) => true, // can't tell (no ss) → assume connected, never tear down blindly
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_tree_and_remove() {
        let root = std::env::temp_dir().join("broom_lio_test");
        let _ = std::fs::remove_dir_all(&root);
        let lio = Lio::at(&root);
        let iqn = "iqn.2026-01.local.broom-0000abcd:ubuntu";
        lio.export("ubuntu", Backing::File { path: "/img/ubuntu/image.img", size: 4096 }, iqn).unwrap();
        let rd = |p: &str| std::fs::read_to_string(root.join(p)).unwrap();
        assert_eq!(rd("core/fileio_0/ubuntu/control"), "fd_dev_name=/img/ubuntu/image.img,fd_dev_size=4096");
        assert_eq!(rd("core/fileio_0/ubuntu/enable"), "1");
        let tpg = format!("iscsi/{iqn}/tpgt_1");
        let link = root.join(format!("{tpg}/lun/lun_0/golden"));
        assert_eq!(std::fs::read_link(&link).unwrap(), root.join("core/fileio_0/ubuntu"));
        assert!(root.join(format!("{tpg}/np/0.0.0.0:3260")).is_dir());
        assert_eq!(rd(&format!("{tpg}/attrib/demo_mode_write_protect")), "1");
        assert_eq!(rd(&format!("{tpg}/attrib/generate_node_acls")), "1");
        assert_eq!(rd(&format!("{tpg}/enable")), "1");

        // Re-export as zram: old fileio backstore + target replaced.
        lio.export("ubuntu", Backing::Block { dev: "/dev/zram0" }, iqn).unwrap();
        assert!(!root.join("core/fileio_0/ubuntu").exists());
        assert_eq!(rd("core/iblock_0/ubuntu/control"), "udev_path=/dev/zram0,readonly=1");
        assert_eq!(std::fs::read_link(&link).unwrap(), root.join("core/iblock_0/ubuntu"));

        lio.remove("ubuntu", iqn);
        assert!(!root.join(format!("iscsi/{iqn}")).exists());
        assert!(!root.join("core/iblock_0/ubuntu").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Real kernel LIO (needs root + LIO modules): `cargo test -- --ignored lio_live`.
    #[test]
    #[ignore]
    fn lio_live() {
        let img = std::env::temp_dir().join("broom_lio_live.img");
        std::fs::write(&img, vec![0u8; 1 << 20]).unwrap();
        let lio = Lio::system().unwrap();
        let iqn = "iqn.2026-01.local.broom-test:live";
        lio.export("brtest", Backing::File { path: img.to_str().unwrap(), size: 1 << 20 }, iqn).unwrap();
        let tpg = lio.root.join(format!("iscsi/{iqn}/tpgt_1"));
        assert_eq!(std::fs::read_to_string(tpg.join("enable")).unwrap().trim(), "1");
        assert_eq!(std::fs::read_to_string(tpg.join("attrib/demo_mode_write_protect")).unwrap().trim(), "1");
        let listening = Command::new("ss").args(["-ltn", "sport = :3260"]).output().unwrap();
        assert!(String::from_utf8_lossy(&listening.stdout).contains("3260"), "portal not listening");
        lio.remove("brtest", iqn);
        assert!(!lio.root.join(format!("iscsi/{iqn}")).exists());
        assert!(!lio.root.join("core/fileio_0/brtest").exists());
        let _ = std::fs::remove_file(img);
    }
}
