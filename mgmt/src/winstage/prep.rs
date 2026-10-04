// winstage/prep.rs — what goes INTO the Windows golden: the prep script run in the VM (scripts/prep-win.ps1, with the
// guest user + boot-start disk drivers filled in), the first-logon/boot-order scripts the server writes into the
// golden at publish, and the unattend tweak (silent OOBE).
use std::path::Path;

use crate::db::Db;

/// Insert SkipMachineOOBE/SkipUserOOBE into the <OOBE> block (if missing). None = no OOBE block.
fn add_skip_oobe(xml: &str) -> Option<String> {
    if xml.contains("SkipMachineOOBE") {
        return Some(xml.to_string());
    }
    let i = xml.find("<OOBE>")? + "<OOBE>".len();
    Some(format!(
        "{}\n        <SkipMachineOOBE>true</SkipMachineOOBE>\n        <SkipUserOOBE>true</SkipUserOOBE>{}",
        &xml[..i],
        &xml[i..]
    ))
}

/// Sysprep copies /unattend to C:\Windows\Panther\unattend.xml — Setup reads that file during
/// specialize/oobeSystem. Patching it there → OOBE shows no page at all (not even the network page),
/// and an old golden doesn't need prep again. Returns true if patched. File name matched case-insensitively.
pub(super) fn silent_oobe(mnt: &str) -> Result<bool, String> {
    let dir = format!("{mnt}/Windows/Panther");
    let Some(p) = std::fs::read_dir(&dir).ok().and_then(|rd| {
        rd.flatten()
            .map(|e| e.path())
            .find(|p| p.file_name().map_or(false, |n| n.to_string_lossy().eq_ignore_ascii_case("unattend.xml")))
    }) else {
        return Ok(false);
    };
    let xml = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    match add_skip_oobe(&xml) {
        Some(new) if new != xml => {
            std::fs::write(&p, new).map_err(|e| format!("{}: {e}", p.display()))?;
            Ok(true)
        }
        Some(_) => Ok(true),
        None => Ok(false),
    }
}

/// Disk controller drivers shipped with Windows 10/11 (AHCI, NVMe, Intel RST, LSI/Broadcom, AMD,
/// VMware PVSCSI…). The golden only loads the golden VM's controller driver at boot → a machine with another
/// controller = INACCESSIBLE_BOOT_DEVICE (the kernel can't read the SSD holding the VHDX). Enable all of them.
pub(super) const BOOT_STORAGE: &[&str] = &[
    "storahci", "stornvme", "iaStorAVC", "iaStorV", "LSI_SAS", "LSI_SAS2i", "LSI_SAS3i", "LSI_SSS",
    "megasas", "megasas2i", "megasas35i", "percsas2i", "percsas3i", "SmartSAMD", "arcsas", "ItSas35i",
    "amdsata", "amdsbs", "amdxata", "nvraid", "nvstor", "pvscsi",
];

/// The prep script set Start=0 + StartOverride\0=0 for the BOOT_STORAGE drivers this Windows has, right after
/// sysprep generalized it, and listed them in C:\broom\boot-storage.ok. A golden prepped by an older version (the
/// server used to edit the hive itself) lacks that marker → it would not boot on other controllers → refuse.
pub(super) fn boot_storage_done(mnt: &str) -> Result<String, String> {
    let f = format!("{mnt}/broom/boot-storage.ok");
    match std::fs::read_to_string(&f) {
        Ok(s) => Ok(s.trim().to_string()),
        Err(_) => Err("this golden was prepared by an older version (C:\\broom\\boot-storage.ok missing) — boot the VM, \
                       run the Windows prep command again (Images page), let it power off, then upload"
            .into()),
    }
}

/// XML escape for values embedded in unattend.
fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Script run INSIDE the golden Windows VM (PowerShell Admin, preferably in Audit Mode: Ctrl+Shift+F3 at OOBE).
/// Usage: irm "http://<server>/broom-prep-win?t=<one-time token>" | iex (Images page → Windows prep command).
/// Diskless tweaks → EFI bundle C:\broom\efi → unattend (guest user + autologon + skip OOBE) +
/// broom-done.ps1 (first logon: write base.ok to BROOMWIN + reboot) → sysprep /generalize /quit → boot-start disk
/// drivers (Start=0) → power off the VM.
const PREP_WIN: &str = include_str!("../../scripts/prep-win.ps1");

/// First logon (when base.vhdx is created on each machine): write base.ok to BROOMWIN, then restart at once → the stage
/// commits base. With base mode on for the image (broom\basemode.txt from the stage): a popup tells the technician to
/// set up apps (FACEIT AC...) and restart; that restart → the stage commits base. BROOMWIN has no
/// drive letter (GPT bit 63, set by the stage) → write directly via the volume path `\\?\Volume{..}\`. ASCII only
/// (Set-Content -Encoding ascii).
pub(super) const BROOM_DONE: &str = include_str!("../../scripts/broom-done.ps1");

/// BroomBootOrder task (SYSTEM, at startup + every 5 minutes): Windows pulls "Windows Boot Manager"
/// to the top of BootOrder every boot → the next power-on skips PXE (no reset). Restores the order the stage
/// wrote to broom\bootorder.txt (Boot#### numbers, PXE = the entry that booted the stage) — by NUMBER, never by
/// entry name (PXE entries are named anything: "IBA GE Slot 0100", "Realtek PXE B03"...). Reads/writes the UEFI
/// BootOrder variable directly (kernel32; SYSTEM + SeSystemEnvironmentPrivilege). Entries not in the file (added
/// later) are kept, after. Writes NVRAM only when different. ASCII only.
pub(super) const BROOM_BOOTORDER: &str = include_str!("../../scripts/broom-bootorder.ps1");

/// /broom-prep-win: embeds the guest user/password (config shared with Linux).
pub fn prep_script(db: &dyn Db) -> String {
    let user = db.get_config("ltsp_user", "guest");
    let pass = db.get_config("ltsp_password", "123456");
    // The values sit inside unattend.xml (XML-escaped) AND inside a PowerShell here-string (`$`/backtick would be
    // expanded). set_cafe_user already rejects those characters; escaping here too covers an old stored value.
    let esc = |s: &str| xml(s).replace('`', "``").replace('$', "`$");
    fill_prep(&esc(&user), &esc(&pass))
}

/// PREP_WIN with its placeholders filled (user/password already escaped).
fn fill_prep(user: &str, pass: &str) -> String {
    let drivers = BOOT_STORAGE.iter().map(|d| format!("'{d}'")).collect::<Vec<_>>().join(", ");
    PREP_WIN
        .replace("__BROOM_DONE__", BROOM_DONE)
        .replace("__BOOT_STORAGE__", &drivers)
        .replace("__USER__", user)
        .replace("__PASS__", pass)
}

/// Overwrite broom-done.ps1 in the golden (a golden prepped with an old version still gets the new logic).
pub(super) fn write_broom_done(mnt: &str) -> Result<bool, String> {
    let dir = format!("{mnt}/Windows/Setup/Scripts");
    if !Path::new(&dir).is_dir() {
        return Ok(false);
    }
    for (f, body) in [("broom-done.ps1", BROOM_DONE), ("broom-bootorder.ps1", BROOM_BOOTORDER)] {
        std::fs::write(format!("{dir}/{f}"), body.replace('\n', "\r\n")).map_err(|e| format!("write {f}: {e}"))?;
    }
    // The default hook an earlier prep wrote (open FACEIT, kill it after 120 s) — broom-done handles FACEIT itself now.
    let hook = format!("{mnt}/broom/base-hook.ps1");
    if std::fs::read_to_string(&hook).is_ok_and(|s| s.contains("[hook] FACEIT AC")) {
        let _ = std::fs::remove_file(&hook);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn prep_win_filled() {
        let s = super::fill_prep("guest", "1");
        assert!(!s.contains("__"), "placeholder left unreplaced");
        assert!(super::BROOM_DONE.is_ascii(), "broom-done is written with -Encoding ascii");
        assert!(super::BROOM_BOOTORDER.is_ascii());
        assert!(s.contains("WriteAllText"));
        // Boot-start disk drivers: set by the prep AFTER sysprep generalized (/quit, not /shutdown), then power off.
        assert!(s.contains("@('storahci', 'stornvme', "));
        let (sp, reg, off) = (s.find("'/quit'").unwrap(), s.find("reg add $k /v Start").unwrap(), s.find("Stop-Computer -Force").unwrap());
        assert!(sp < reg && reg < off && s.contains("boot-storage.ok"));
    }

    #[test]
    fn skip_oobe_patch() {
        let x = "<OOBE>\n  <HideEULAPage>true</HideEULAPage>\n</OOBE>";
        let y = super::add_skip_oobe(x).unwrap();
        assert!(y.starts_with("<OOBE>\n        <SkipMachineOOBE>true</SkipMachineOOBE>"));
        assert!(y.contains("<SkipUserOOBE>true</SkipUserOOBE>") && y.contains("<HideEULAPage>"));
        assert_eq!(super::add_skip_oobe(&y).unwrap(), y); // not inserted twice
        assert!(super::add_skip_oobe("<unattend/>").is_none());
    }
}
