// winstage/prep.rs — what goes INTO the Windows golden: the prep script run in the VM (scripts/prep-win.ps1, with the
// guest user, boot-start disk drivers and the two fixed script stubs filled in), and the checks the publish reads back
// from the golden (the server never writes inside it). The real broom-done / boot-order scripts reach the client
// through the stage (BROOMWIN broom\), see winstage::publish.
use crate::db::Db;
use crate::ntfsread::Vol;

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
pub(super) fn boot_storage_done(vol: &mut Vol) -> Result<String, String> {
    match vol.read("broom/boot-storage.ok")? {
        Some(s) => Ok(String::from_utf8_lossy(&s).trim().to_string()),
        None => Err("this golden was prepared by an older version (C:\\broom\\boot-storage.ok missing) — boot the VM, \
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
/// stubs broom-done.ps1 / broom-bootorder.ps1 → sysprep /generalize /quit → boot-start disk drivers (Start=0) →
/// power off the VM.
fn prep_win() -> &'static str { crate::assets::text("scripts/prep-win.ps1") }

/// First logon (when base.vhdx is created on each machine): write base.ok to BROOMWIN, then restart at once → the stage
/// commits base. With base mode on for the image (broom\basemode.txt from the stage): a popup tells the technician to
/// set up apps (FACEIT AC...) and restart; that restart → the stage commits base. BROOMWIN has no
/// drive letter (GPT bit 63, set by the stage) → write directly via the volume path `\\?\Volume{..}\`. ASCII only
/// (Set-Content -Encoding ascii).
pub(super) fn broom_done() -> &'static str { crate::assets::text("scripts/broom-done.ps1") }

/// BroomBootOrder task (SYSTEM, at startup + every 5 minutes): Windows pulls "Windows Boot Manager"
/// to the top of BootOrder every boot → the next power-on skips PXE (no reset). Restores the order the stage
/// wrote to broom\bootorder.txt (Boot#### numbers, PXE = the entry that booted the stage) — by NUMBER, never by
/// entry name (PXE entries are named anything: "IBA GE Slot 0100", "Realtek PXE B03"...). Reads/writes the UEFI
/// BootOrder variable directly (kernel32; SYSTEM + SeSystemEnvironmentPrivilege). Entries not in the file (added
/// later) are kept, after. Writes NVRAM only when different. ASCII only.
pub(super) fn broom_bootorder() -> &'static str { crate::assets::text("scripts/broom-bootorder.ps1") }

/// /broom-prep-win: embeds the guest user/password (config shared with Linux).
pub fn prep_script(db: &dyn Db) -> String {
    let user = db.get_config("ltsp_user", "guest");
    let pass = db.get_config("ltsp_password", "123456");
    // The values sit inside unattend.xml (XML-escaped) AND inside a PowerShell here-string (`$`/backtick would be
    // expanded). set_cafe_user already rejects those characters; escaping here too covers an old stored value.
    let esc = |s: &str| xml(s).replace('`', "``").replace('$', "`$");
    fill_prep(&esc(&user), &esc(&pass))
}

/// prep_win() with its placeholders filled (user/password already escaped).
fn fill_prep(user: &str, pass: &str) -> String {
    let drivers = BOOT_STORAGE.iter().map(|d| format!("'{d}'")).collect::<Vec<_>>().join(", ");
    prep_win()
        .replace("__STUB_DONE__", &stub().replace("__SCRIPT__", "broom-done.ps1"))
        .replace("__STUB_BOOTORDER__", &stub().replace("__SCRIPT__", "broom-bootorder.ps1"))
        .replace("__BOOT_STORAGE__", &drivers)
        .replace("__USER__", user)
        .replace("__PASS__", pass)
}

/// What the prep puts in the golden as broom-done.ps1 / broom-bootorder.ps1: runs the stage's copy from BROOMWIN.
fn stub() -> &'static str { crate::assets::text("scripts/broom-stub.ps1") }
const STUB_MARK: &str = "broom-stub v1";

/// The golden holds the stubs (prep of this version or newer). An older golden still has full scripts of the version
/// the server last wrote into it: they keep working, just never update.
pub(super) fn has_stub(vol: &mut Vol) -> bool {
    vol.read("Windows/Setup/Scripts/broom-done.ps1")
        .ok()
        .flatten()
        .is_some_and(|s| String::from_utf8_lossy(&s).contains(STUB_MARK))
}

#[cfg(test)]
mod tests {
    #[test]
    fn prep_win_filled() {
        let s = super::fill_prep("guest", "1");
        assert!(!s.contains("__"), "placeholder left unreplaced");
        assert!(super::broom_done().is_ascii(), "broom-done is written with -Encoding ascii");
        assert!(super::broom_bootorder().is_ascii());
        assert!(super::stub().is_ascii());
        // The golden gets the two stubs, not the scripts themselves (those come from BROOMWIN, newest every boot).
        assert!(s.contains("broom\\broom-done.ps1") && s.contains("broom\\broom-bootorder.ps1"));
        assert_eq!(s.matches(super::STUB_MARK).count(), 2);
        assert!(!s.contains("Register-ScheduledTask -TaskName BroomBootOrder"), "broom-done itself is not baked in");
        // Boot-start disk drivers: set by the prep AFTER sysprep generalized (/quit, not /shutdown), then power off.
        assert!(s.contains("@('storahci', 'stornvme', "));
        let (sp, reg, off) = (s.find("'/quit'").unwrap(), s.find("reg add $k /v Start").unwrap(), s.find("Stop-Computer -Force").unwrap());
        assert!(sp < reg && reg < off && s.contains("boot-storage.ok"));
    }
}
