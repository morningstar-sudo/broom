// boot.rs — M5 boot menu. Handler GET /boot.ipxe: generates a dynamic iPXE menu.
// iPXE calls: chain http://SERVER/boot.ipxe?mac=${net0/mac}
// The menu lists the images, marks the default item + countdown; on timeout → boot the default.
// iPXE (snponly.efi) is embedded in the binary — install_ipxe() writes it to /srv/tftp.
use axum::{
    extract::{Query, State},
    http::header,
    response::IntoResponse,
};
use std::collections::HashMap;

use crate::{db, SharedState};

/// Upstream iPXE built by mgmt/ipxe/build.sh (pinned commit in mgmt/ipxe/IPXE_COMMIT).
const SNPONLY_EFI: &[u8] = include_bytes!("../ipxe/snponly.efi");
const SNPONLY_PATH: &str = "/srv/tftp/snponly.efi";

/// Write the embedded iPXE to /srv/tftp if missing/different (upgrading the mgmt binary = upgrading iPXE too).
pub fn install_ipxe() -> std::io::Result<bool> {
    if std::fs::read(SNPONLY_PATH).ok().as_deref() == Some(SNPONLY_EFI) {
        return Ok(false);
    }
    std::fs::create_dir_all("/srv/tftp")?;
    let tmp = format!("{SNPONLY_PATH}.tmp");
    std::fs::write(&tmp, SNPONLY_EFI)?;
    std::fs::rename(&tmp, SNPONLY_PATH)?;
    Ok(true)
}

pub async fn render(
    State(st): State<SharedState>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let conn = st.db.lock().unwrap();
    // mac is appended to the URL by dnsmasq (iPXE expands ${net0/mac} = "34:5a:60:7b:2b:1d").
    let mac = q.get("mac").map(|m| m.to_lowercase().replace('-', ":")).unwrap_or_default();
    let host: Option<String> = conn
        .query_row(
            "SELECT hostname FROM machines WHERE lower(replace(mac,'-',':'))=?1",
            [&mac],
            |r| r.get(0),
        )
        .ok()
        .flatten();

    let timeout_s: u64 = db::get_config(&conn, "boot_timeout", "10")
        .parse()
        .unwrap_or(10);

    let mut stmt = conn
        .prepare("SELECT name, is_default, boot_script FROM images ORDER BY id")
        .unwrap();
    let images: Vec<MenuImage> = stmt
        .query_map([], |r| {
            Ok(MenuImage {
                name: r.get(0)?,
                is_default: r.get::<_, i64>(1)? == 1,
                boot_script: r.get(2)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], menu_script(&images, timeout_s, host.as_deref()))
}

struct MenuImage {
    name: String,
    is_default: bool,
    boot_script: Option<String>,
}

/// iPXE menu script. ASCII only (the iPXE console font has no accented characters).
/// Layout (ipxe-src/src/hci/tui/menu_ui.c): title left + `menu-hint` + countdown right, horizontal line,
/// list `[1] NAME`, horizontal line, `menu-footer` "left|center|right". White text on dark, selected black/white.
/// ESC → shell (technical).
fn menu_script(images: &[MenuImage], timeout_s: u64, host: Option<&str>) -> String {
    let mut items = String::new();
    let mut targets = String::new();
    let mut default = None;
    for (i, img) in images.iter().enumerate() {
        let label = format!("img_{}", sanitize(&img.name));
        let key = if i < 9 { format!("--key {} ", i + 1) } else { String::new() };
        items.push_str(&format!("item {key}{label} [{}] {}\n", i + 1, img.name));
        let body = match img.boot_script.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => s.to_string(),
            // Not published yet → tell the user + back to the menu (no hang).
            _ => format!(
                "echo Image '{}' is not published yet (no boot_script)\nsleep 3\ngoto start",
                img.name
            ),
        };
        targets.push_str(&format!(":{label}\n{body}\n\n"));
        if img.is_default {
            default = Some(label);
        }
    }
    // No default image → no countdown (never auto-boot the wrong OS).
    let choose = match &default {
        Some(l) => format!("--default {l} --timeout {}", timeout_s * 1000),
        None => String::new(),
    };
    if images.is_empty() {
        items.push_str("item --key s shell [S] iPXE shell (no image yet - upload one in the web admin)\n");
    }
    // Windows computer name (NetBIOS): letters/digits/'-', max 15 — also safe inside the iPXE script.
    // broom-host → stage cmdline (broom.host=) → Windows sets the name when base is created.
    let host: String = host
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(15)
        .collect();
    let set_host = if host.is_empty() { String::new() } else { format!("set broom-host {host}\n") };
    let host = if host.is_empty() { "not registered".into() } else { host };

    format!(
        "#!ipxe\n\
         cpair --foreground 7 --background 0 0 ||\n\
         cpair --foreground 7 --background 0 1 ||\n\
         cpair --foreground 0 --background 7 2 ||\n\
         cpair --foreground 7 --background 0 3 ||\n\
         set menu-hint Arrows/number to select, Enter to boot\n\
         set menu-footer Host: {host}|IP: ${{net0/ip}}|MAC: ${{net0/mac}}\n\
         {set_host}\
         :start\n\
         menu Select operating system to boot\n\
         {items}\
         choose {choose} sel || goto shell\n\
         goto ${{sel}}\n\n\
         {targets}\
         :shell\n\
         echo Type 'exit' to return to menu.\n\
         shell\n\
         goto start\n"
    )
}

/// iPXE labels should only use [A-Za-z0-9_]. Other characters become '_'.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{menu_script, MenuImage};

    fn img(name: &str, def: bool, bs: Option<&str>) -> MenuImage {
        MenuImage { name: name.into(), is_default: def, boot_script: bs.map(Into::into) }
    }

    #[test]
    fn menu_default_keys_ascii() {
        let s = menu_script(
            &[img("win-11", true, Some("kernel x\nboot")), img("ubuntu", false, None)],
            5,
            Some("FPS-43 $x|"),
        );
        assert!(s.starts_with("#!ipxe\n"));
        assert!(s.is_ascii(), "the iPXE font is ASCII only");
        assert!(s.contains("item --key 1 img_win_11 [1] win-11\n"));
        assert!(s.contains("item --key 2 img_ubuntu [2] ubuntu\n"));
        assert!(s.contains("choose --default img_win_11 --timeout 5000 sel || goto shell"));
        assert!(s.contains(":img_win_11\nkernel x\nboot"));
        assert!(s.contains(":img_ubuntu\necho Image 'ubuntu' is not published yet"));
        assert!(s.contains("set menu-footer Host: FPS-43x|IP: ${net0/ip}|MAC: ${net0/mac}\n"));
        assert!(s.contains("set broom-host FPS-43x\n"));
        // menu_ui.c draws the countdown in the last 32 columns of the hint row (from col 46 on 80x25).
        let hint = s.lines().find_map(|l| l.strip_prefix("set menu-hint ")).unwrap();
        assert!(2 + hint.len() <= 80 - 2 - 32, "menu-hint would be overwritten by the countdown");
    }

    #[test]
    fn menu_no_default_no_timeout() {
        let s = menu_script(&[img("a", false, Some("boot"))], 10, None);
        assert!(s.contains("choose  sel || goto shell"));
        assert!(s.contains("Host: not registered|"));
        assert!(!s.contains("broom-host"));
        let s = menu_script(&[], 10, None);
        assert!(s.contains("item --key s shell [S] iPXE shell (no image yet"));
    }
}
