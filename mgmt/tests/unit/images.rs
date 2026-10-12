#[test]
fn cache_list_lines() {
    let img = |name: &str, os: &str, hash: Option<&str>, use_ssd: bool| crate::db::Image {
        id: 0,
        name: name.into(),
        os: os.into(),
        active_version: None,
        is_default: false,
        boot_script: None,
        hash: hash.map(Into::into),
        cache_mode: "disk".into(),
        base_mode: false,
        use_ssd,
        groups: Vec::new(),
        preload: false,
    };
    let mut list = vec![
        img("win11", "windows", Some("aa"), false), // Windows always uses the SSD
        img("ubuntu", "linux", Some("bb"), true),
        img("kiosk", "linux", Some("cc"), false), // SSD off → not cached
        img("draft", "linux", None, true),        // not published
    ];
    assert_eq!(super::cache_lines(&list, None), "win11 aa\nubuntu bb\n");
    // Another group's image is not kept; a preloaded one is also listed to fetch ahead (after every keep line).
    list.push(crate::db::Image {
        groups: vec!["VIP".into()],
        preload: true,
        ..img("stream", "windows", Some("dd"), true)
    });
    list[1].preload = true;
    assert_eq!(
        super::cache_lines(&list, Some("Thuong")),
        "win11 aa\nubuntu bb\npreload ubuntu bb linux\n"
    );
    assert_eq!(
        super::cache_lines(&list, Some("vip")),
        "win11 aa\nubuntu bb\nstream dd\npreload ubuntu bb linux\npreload stream dd windows\n"
    );
}

#[test]
fn upload_file_names() {
    assert!(super::valid_file("Windows 11 x64-s001.vmdk"));
    for bad in ["", ".", "..", "../x", "a/b", "a\\b", "x\0"] {
        assert!(!super::valid_file(bad), "{bad:?}");
    }
}
