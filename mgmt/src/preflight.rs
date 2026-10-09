// preflight.rs — before serving: root (required). No distro packages and no kernel modules: everything the server
// does is built in (iSCSI target + RAM cache: iscsid/, NTFS reading: ntfsread.rs), and the Windows stage comes as a
// bundle (winstage::ensure_stage).

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Ok, or what stops the server from running.
pub fn run() -> Result<(), String> {
    if is_root() {
        Ok(())
    } else {
        Err("must run as root (DHCP :67 / TFTP :69 / HTTP :80 / iSCSI :3260)".into())
    }
}
