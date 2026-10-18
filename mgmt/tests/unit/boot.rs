use super::{MenuImage, linux_share_gb, menu_script};

#[test]
fn linux_share() {
    const G: u64 = 1 << 30;
    assert_eq!(
        linux_share_gb(std::iter::empty()),
        0,
        "no Linux image → Windows gets the disk"
    );
    assert_eq!(linux_share_gb([20 * G].into_iter()), 55);
    assert_eq!(
        linux_share_gb([8 * G, 20 * G + 1].into_iter()),
        64,
        "all of them cached: 8 + 21 (rounded up) + 35"
    );
}

fn img(name: &str, def: bool) -> MenuImage {
    MenuImage {
        name: name.into(),
        is_default: def,
    }
}

#[test]
fn menu_default_keys_ascii() {
    let s = menu_script(
        &[img("win-11", true), img("ubuntu", false)],
        5,
        Some("FPS-43 $x|"),
        Some(3),
        true,
        false,
    );
    assert!(s.starts_with("#!ipxe\n"));
    assert!(s.is_ascii(), "the iPXE font is ASCII only");
    assert!(s.contains("item --key 1 img1 [1] win-11\n"));
    assert!(s.contains("item --key 2 img2 [2] ubuntu\n"));
    assert!(s.contains("choose --default img1 --timeout 5000 sel || goto shell"));
    // A choice goes through the server (boot log + the image's script).
    assert!(s.contains(
        ":img1\nchain /boot/start?image=win-11&mac=${net0/mac}&ip=${net0/ip} || goto start\n"
    ));
    assert!(s.contains("set menu-footer Host: FPS-43x|IP: ${net0/ip}|MAC: ${net0/mac}\n"));
    assert!(s.contains("set broom-host FPS-43x\n"));
    assert!(
        s.contains("set broom-lic 3\n"),
        "license generation, never the key"
    );
    assert!(
        s.contains("set broom-reg 1\n"),
        "registered machine → the stage may partition its disk"
    );
    // menu_ui.c draws the countdown in the last 32 columns of the hint row (from col 46 on 80x25).
    let hint = s
        .lines()
        .find_map(|l| l.strip_prefix("set menu-hint "))
        .unwrap();
    assert!(
        2 + hint.len() <= 80 - 2 - 32,
        "menu-hint would be overwritten by the countdown"
    );
}

/// The menu holds the images of the machine's group; its own image is the default when on it, else the global one.
#[test]
fn menu_by_group_and_default() {
    let img = |id, name: &str, def, groups: &[&str]| crate::db::Image {
        id,
        name: name.into(),
        os: "windows".into(),
        active_version: None,
        is_default: def,
        boot_script: None,
        hash: None,
        cache_mode: "disk".into(),
        use_ssd: true,
        groups: groups.iter().map(|g| g.to_string()).collect(),
        preload: false,
    };
    let images = || {
        vec![
            img(1, "win11", true, &[]),
            img(2, "ubuntu", false, &[]),
            img(3, "stream", false, &["VIP"]),
        ]
    };
    let menu = |own: Option<i64>, grp: Option<&str>| {
        let m = crate::db::Machine {
            image_id: own,
            grp: grp.map(Into::into),
            ..Default::default()
        };
        let items = super::menu_images(images(), Some(&m));
        let names: Vec<String> = items.iter().map(|i| i.name.clone()).collect();
        (
            names,
            items
                .into_iter()
                .filter(|i| i.is_default)
                .map(|i| i.name)
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(
        menu(None, None),
        (vec!["win11".into(), "ubuntu".into()], vec!["win11".into()])
    );
    assert_eq!(menu(Some(2), None).1, ["ubuntu"]);
    assert_eq!(
        menu(Some(9), None).1,
        ["win11"],
        "deleted image → global default"
    );
    assert_eq!(
        menu(Some(3), Some("vip")).0,
        ["win11", "ubuntu", "stream"],
        "VIP (any case) also gets its own image"
    );
    assert_eq!(menu(Some(3), Some("vip")).1, ["stream"]);
    assert_eq!(
        menu(Some(3), None).1,
        ["win11"],
        "own image not for its group → global default"
    );
    let only_vip = super::menu_images(vec![img(3, "stream", false, &["VIP"])], None);
    assert!(
        only_vip.is_empty(),
        "unregistered machine: only images for every machine"
    );
}

/// Open endpoints log mac/ip from the query: junk (a forged log line) must never get through.
#[test]
fn client_ids_validated() {
    let s = |x: &str| x.to_string();
    let q = |mac: &str, ip: &str| {
        super::client_ids(&std::collections::HashMap::from([
            (s("mac"), s(mac)),
            (s("ip"), s(ip)),
        ]))
    };
    assert_eq!(
        q("34-5A-60-7B-2B-1D", "10.0.0.50"),
        (s("34:5a:60:7b:2b:1d"), s("10.0.0.50"))
    );
    assert_eq!(
        q("aa\nINFO client X started", "1.2.3.4\nfake"),
        (s(""), s("?"))
    );
    // Surrounding whitespace is trimmed off: what is logged is the clean MAC.
    assert_eq!(
        q("34:5a:60:7b:2b:1d\n", "999.1.1.1"),
        (s("34:5a:60:7b:2b:1d"), s("?"))
    );
    assert_eq!(super::client_ids(&Default::default()), (s(""), s("?")));
}

#[test]
fn menu_no_default_no_timeout() {
    let s = menu_script(&[img("a", false)], 10, None, None, false, false);
    assert!(s.contains("choose  sel || goto shell"));
    assert!(s.contains("Host: not registered|"));
    assert!(
        !s.contains("broom-host") && !s.contains("broom-lic") && !s.contains("broom-reg"),
        "unknown machine"
    );
    let s = menu_script(&[], 10, None, None, false, false);
    assert!(s.contains("item --key s shell [S] iPXE shell (no image yet"));
}

/// Official signed iPXE (Secure Boot) has no menu-hint/footer → the same info as `item --gap` lines, above the
/// images (number keys still map to images), still ASCII.
#[test]
fn menu_secure_boot_footer_as_gap_lines() {
    let s = menu_script(&[img("win-11", true)], 5, Some("PC05"), None, true, true);
    assert!(s.is_ascii());
    let item = s.find("item --key 1 img1 ").unwrap();
    let head = s
        .find(
            "item --gap Host : PC05\nitem --gap IP   : ${net0/ip}\nitem --gap MAC  : ${net0/mac}\n",
        )
        .unwrap();
    assert!(head < item && s.contains("item --gap Arrows/number to select, Enter to boot\n"));
    assert!(!menu_script(&[img("a", false)], 5, None, None, false, false).contains("item --gap"));
}

/// [C] Capture is on every menu (also with no image yet), last, never the default.
#[test]
fn menu_has_capture_last() {
    for imgs in [vec![img("a", true)], vec![]] {
        let s = menu_script(&imgs, 5, None, None, false, false);
        let c = s.find("item --key c capture [C] Capture").unwrap();
        assert!(s.find("item --key").unwrap() < c || imgs.is_empty());
        assert!(s.contains(
            ":capture\nchain /boot/capture?mac=${net0/mac}&ip=${net0/ip} || goto start\n"
        ));
        assert!(!s.contains("--default capture"));
    }
}
