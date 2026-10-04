// preflight.rs — before serving: root, kernel modules (warnings), and — only for a binary without a pinned stage
// bundle — the tools the Windows stage is built from on this server.
// Missing packages are grouped into one apt command; misc (permissions) reported separately.
use std::process::Command;

/// (binary, apt package, purpose) — shared source for preflight + setup.
pub const BINS: &[(&str, &str, &str)] = &[
    // Tools copied into the Windows client stage initrd.
    ("sfdisk", "fdisk", "partition the client SSD (copied into the Windows stage initrd)"),
    ("mkntfs", "ntfs-3g", "format the client SSD as NTFS (stage)"),
    ("mkfs.fat", "dosfstools", "format the client SSD ESP (stage)"),
    ("efibootmgr", "efibootmgr", "BootNext into Windows on the client SSD (stage)"),
    ("mkinitramfs", "initramfs-tools", "build the Windows stage initrd"),
    ("wget", "wget", "client stage downloads golden.vhdx"),
    ("zstd", "zstd", "client stage decompresses delta golden chunks"),
];

/// Kernel modules broom uses (built in or loadable) and what breaks without them.
const KERNEL_MODULES: &[(&str, &str)] = &[
    ("loop", "Windows publish (mounting the golden's partition)"),
    ("ntfs3", "Windows publish (editing the golden)"),
    ("target_core_mod", "Linux images (iSCSI target)"),
    ("iscsi_target_mod", "Linux images (iSCSI target)"),
    ("zram", "the zram cache mode of Linux images"),
];

/// Loaded / built in (/sys/module) or loadable (modprobe dry run).
fn kernel_has(m: &str) -> bool {
    std::path::Path::new(&format!("/sys/module/{m}")).exists()
        || Command::new("modprobe").args(["-n", "-q", m]).status().is_ok_and(|s| s.success())
}

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
    // A CI-built binary brings its stage as a bundle (winstage::ensure_stage): the server needs none of these.
    if crate::winstage::stage_pinned().is_some() {
        return pkgs;
    }
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
        other.push("must run as root (DHCP :67 / TFTP :69 / HTTP :80, iSCSI, zram, loop mounts)".into());
    }
    // Kernel features, not packages: only a warning (each serves one kind of image).
    for (m, why) in KERNEL_MODULES {
        if !kernel_has(m) {
            tracing::warn!("preflight: kernel module {m} not available — {why}");
        }
    }

    if pkgs.is_empty() && other.is_empty() {
        Ok(())
    } else {
        Err(Report { pkgs, other })
    }
}
