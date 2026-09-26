// boot.rs — M5 boot menu.
// GET /boot.ipxe?mac=&ip=  (URL handed out by the built-in DHCP) → dynamic iPXE menu: images, default
//   item + countdown; on timeout → the default.
// GET /boot/start?image=&mac=&ip=  (a menu choice chains here) → logs "client … started" and returns
//   that image's boot script.
// iPXE (snponly.efi) is embedded in the binary — the built-in TFTP server (tftp.rs) serves it from memory.
use axum::{
    extract::{Query, State},
    http::header,
    response::IntoResponse,
};
use std::collections::HashMap;
use tracing::{info, warn};

use crate::db::Machine;
use crate::SharedState;

/// iPXE built by mgmt/ipxe/build.sh from mgmt/ipxe/ipxe-src (upgrading the mgmt binary = upgrading iPXE too).
pub(crate) const SNPONLY_EFI: &[u8] = include_bytes!("../ipxe/snponly.efi");

type Q = Query<HashMap<String, String>>;

/// Client identity from the query (iPXE expands ${net0/mac} = "34:5a:60:7b:2b:1d", ${net0/ip}):
/// (mac, ip, its row in the Machines table).
fn client(st: &SharedState, q: &HashMap<String, String>) -> (String, String, Option<Machine>) {
    let norm = |m: &str| m.to_lowercase().replace('-', ":");
    let mac = q.get("mac").map(|m| norm(m)).unwrap_or_default();
    let ip = q.get("ip").cloned().unwrap_or_else(|| "?".into());
    let m = st.db.machines().unwrap_or_default().into_iter().find(|m| norm(&m.mac) == mac);
    (mac, ip, m)
}

fn script(body: String) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body)
}

pub async fn render(State(st): State<SharedState>, Query(q): Q) -> impl IntoResponse {
    let (mac, ip, m) = client(&st, &q);
    let host = m.as_ref().and_then(|m| m.hostname.clone());
    // License generation (a counter, never the key) → stage rebuilds base when it changes.
    let lic = m.as_ref().filter(|m| m.license_key.is_some()).map(|m| m.license_gen);
    let h = host.as_deref().unwrap_or("-");
    info!("client {} boot menu - mac {mac} - ip {ip} - hostname {h}", host.as_deref().unwrap_or(&mac));

    let timeout_s: u64 = st.db.get_config("boot_timeout", "10").parse().unwrap_or(10);
    let images: Vec<MenuImage> = st
        .db
        .images()
        .unwrap_or_default()
        .into_iter()
        .map(|i| MenuImage { name: i.name, is_default: i.is_default })
        .collect();
    script(menu_script(&images, timeout_s, host.as_deref(), lic))
}

/// A menu choice: log the boot + hand over the image's boot script.
pub async fn start(State(st): State<SharedState>, Query(q): Q) -> impl IntoResponse {
    let (mac, ip, m) = client(&st, &q);
    let host = m.and_then(|m| m.hostname);
    let name = q.get("image").cloned().unwrap_or_default();
    let who = host.clone().unwrap_or_else(|| mac.clone());
    let h = host.as_deref().unwrap_or("-");
    let img = st.db.image_by_name(&name).ok().flatten();
    let boot = img.as_ref().and_then(|i| i.boot_script.as_deref()).map(str::trim).filter(|s| !s.is_empty());
    script(match (&img, boot) {
        (Some(i), Some(bs)) => {
            info!("client {who} started - mac {mac} - ip {ip} - hostname {h} - image {name} ({})", i.os);
            format!("#!ipxe\n{bs}\n")
        }
        _ => {
            warn!("client {who} chose image {name:?} - mac {mac} - ip {ip}: not published, back to the menu");
            format!(
                "#!ipxe\necho Image '{name}' is not published yet (no boot_script)\nsleep 3\n\
                 chain /boot.ipxe?mac=${{net0/mac}}&ip=${{net0/ip}}\n"
            )
        }
    })
}

struct MenuImage {
    name: String,
    is_default: bool,
}

/// iPXE menu script. ASCII only (the iPXE console font has no accented characters).
/// Layout (ipxe-src/src/hci/tui/menu_ui.c): title left + `menu-hint` + countdown right, horizontal line,
/// list `[1] NAME`, horizontal line, `menu-footer` "left|center|right". White text on dark, selected black/white.
/// ESC → shell (technical).
fn menu_script(images: &[MenuImage], timeout_s: u64, host: Option<&str>, lic: Option<i64>) -> String {
    let mut items = String::new();
    let mut targets = String::new();
    let mut default = None;
    for (i, img) in images.iter().enumerate() {
        let label = format!("img_{}", sanitize(&img.name));
        let key = if i < 9 { format!("--key {} ", i + 1) } else { String::new() };
        items.push_str(&format!("item {key}{label} [{}] {}\n", i + 1, img.name));
        // The server logs the boot and returns the image's script (image names are URL-safe: [A-Za-z0-9_-]).
        targets.push_str(&format!(
            ":{label}\nchain /boot/start?image={}&mac=${{net0/mac}}&ip=${{net0/ip}} || goto start\n\n",
            img.name
        ));
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
    let mut set_host = if host.is_empty() { String::new() } else { format!("set broom-host {host}\n") };
    // broom-lic → stage cmdline (broom.lic=): license key set / re-armed → base rebuilt → broom-done fetches it.
    if let Some(g) = lic {
        set_host.push_str(&format!("set broom-lic {g}\n"));
    }
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

    fn img(name: &str, def: bool) -> MenuImage {
        MenuImage { name: name.into(), is_default: def }
    }

    #[test]
    fn menu_default_keys_ascii() {
        let s = menu_script(&[img("win-11", true), img("ubuntu", false)], 5, Some("FPS-43 $x|"), Some(3));
        assert!(s.starts_with("#!ipxe\n"));
        assert!(s.is_ascii(), "the iPXE font is ASCII only");
        assert!(s.contains("item --key 1 img_win_11 [1] win-11\n"));
        assert!(s.contains("item --key 2 img_ubuntu [2] ubuntu\n"));
        assert!(s.contains("choose --default img_win_11 --timeout 5000 sel || goto shell"));
        // A choice goes through the server (boot log + the image's script).
        assert!(s.contains(":img_win_11\nchain /boot/start?image=win-11&mac=${net0/mac}&ip=${net0/ip} || goto start\n"));
        assert!(s.contains("set menu-footer Host: FPS-43x|IP: ${net0/ip}|MAC: ${net0/mac}\n"));
        assert!(s.contains("set broom-host FPS-43x\n"));
        assert!(s.contains("set broom-lic 3\n"), "license generation, never the key");
        // menu_ui.c draws the countdown in the last 32 columns of the hint row (from col 46 on 80x25).
        let hint = s.lines().find_map(|l| l.strip_prefix("set menu-hint ")).unwrap();
        assert!(2 + hint.len() <= 80 - 2 - 32, "menu-hint would be overwritten by the countdown");
    }

    #[test]
    fn menu_no_default_no_timeout() {
        let s = menu_script(&[img("a", false)], 10, None, None);
        assert!(s.contains("choose  sel || goto shell"));
        assert!(s.contains("Host: not registered|"));
        assert!(!s.contains("broom-host") && !s.contains("broom-lic"));
        let s = menu_script(&[], 10, None, None);
        assert!(s.contains("item --key s shell [S] iPXE shell (no image yet"));
    }
}
