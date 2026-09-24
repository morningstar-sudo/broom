// preflight.rs — check the tools the binary still shells out to before serving (DHCP/TFTP are built in).
// Missing packages are grouped into one apt command; misc (permissions) reported separately.
use std::process::Command;

/// (binary, apt package, purpose) — shared source for preflight + setup.
pub const BINS: &[(&str, &str, &str)] = &[
    ("qemu-img", "qemu-utils", "convert vmdk → raw img golden"),
    // Golden partitions + tools embedded into the Windows client stage initrd.
    ("sfdisk", "fdisk", "read golden partition tables (Linux + Windows) + partition the client SSD (stage)"),
    ("cpio", "cpio", "append the broom hook to Linux golden initrds"),
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

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
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

    if !is_root() {
        other.push("must run as root (DHCP :67 / TFTP :69 / HTTP :80, iSCSI, zram, qemu-img)".into());
    }

    if pkgs.is_empty() && other.is_empty() {
        Ok(())
    } else {
        Err(Report { pkgs, other })
    }
}
