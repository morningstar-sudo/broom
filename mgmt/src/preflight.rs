// preflight.rs — before serving: root (required) + kernel modules (warnings). No distro packages: everything the
// server does is built in, and the Windows stage comes as a bundle (winstage::ensure_stage).
use std::process::Command;

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

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Ok, or what stops the server from running.
pub fn run() -> Result<(), String> {
    // Kernel features: only a warning (each serves one kind of image).
    for (m, why) in KERNEL_MODULES {
        if !kernel_has(m) {
            tracing::warn!("preflight: kernel module {m} not available — {why}");
        }
    }
    if is_root() {
        Ok(())
    } else {
        Err("must run as root (DHCP :67 / TFTP :69 / HTTP :80, iSCSI, zram, loop mounts)".into())
    }
}
