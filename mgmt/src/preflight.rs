// preflight.rs — check packages/services before serving.
// Missing packages are grouped into one apt command; misc (files/services/permissions) reported separately.
use std::path::Path;
use std::process::Command;

/// (binary, apt package, purpose) — shared source for preflight + setup.
pub const BINS: &[(&str, &str, &str)] = &[
    ("dnsmasq", "dnsmasq", "DHCP proxy/full + TFTP + DNS/hostname"),
    ("targetcli", "targetcli-fb", "iSCSI target (golden raw RO shared)"),
    ("qemu-img", "qemu-utils", "convert vmdk → raw img golden"),
    ("virt-copy-out", "libguestfs-tools", "read the golden (kernel/initrd/UUID) safely, handles LVM"),
    ("iscsistart", "open-iscsi", "iSCSI initiator binary (injected into the golden initrd)"),
    ("zfs", "zfsutils-linux", "image store + snapshot/rollback (optional)"),
    ("zpool", "zfsutils-linux", "ZFS pool (optional)"),
    ("ping", "iputils-ping", "on/off monitoring (M7)"),
    ("unzip", "unzip", "unpack golden bundles (.zip)"),
    // Windows (winstage.rs): split golden partitions + tools embedded into the client stage initrd.
    ("sfdisk", "fdisk", "partition the Windows golden + client SSD (stage)"),
    ("mkntfs", "ntfs-3g", "format the client SSD as NTFS (stage) + read the Windows golden"),
    ("mkfs.fat", "dosfstools", "format the client SSD ESP (stage)"),
    ("efibootmgr", "efibootmgr", "BootNext into Windows on the client SSD (stage)"),
    ("mkinitramfs", "initramfs-tools", "build the Windows stage initrd"),
    ("wget", "wget", "client stage downloads golden.vhdx"),
    ("hivexregedit", "libwin-hivex-perl", "enable boot-start disk drivers in the Windows golden registry"),
];

fn has_bin(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn svc_active(name: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

/// List of missing apt packages (deduplicated).
pub fn missing_pkgs() -> Vec<String> {
    let mut pkgs: Vec<String> = Vec::new();
    for (bin, pkg, _why) in BINS {
        if !has_bin(bin) {
            let p = pkg.to_string();
            if !pkgs.contains(&p) {
                pkgs.push(p);
            }
        }
    }
    pkgs
}

/// Preflight result when it fails.
pub struct Report {
    pub pkgs: Vec<String>,
    pub other: Vec<String>,
}

impl Report {
    pub fn install_cmd(&self) -> Option<String> {
        if self.pkgs.is_empty() {
            None
        } else {
            Some(format!("sudo apt install -y {}", self.pkgs.join(" ")))
        }
    }
}

pub fn run() -> Result<(), Report> {
    let pkgs = missing_pkgs();
    let mut other: Vec<String> = Vec::new();

    if !Path::new("/srv/tftp/snponly.efi").exists() {
        other.push("/srv/tftp/snponly.efi → run `bootrom-mgmt setup`".into());
    }
    if !svc_active("dnsmasq") {
        other.push("dnsmasq not running → `bootrom-mgmt setup` (or systemctl enable --now dnsmasq)".into());
    }
    if !is_root() {
        other.push("must run as root (targetcli/qemu-img/zram/reload dnsmasq/bind :80)".into());
    }

    if pkgs.is_empty() && other.is_empty() {
        Ok(())
    } else {
        Err(Report { pkgs, other })
    }
}
