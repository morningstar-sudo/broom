use super::*;

#[test]
fn inf_ids_utf8_and_utf16() {
    let inf = "[NVIDIA_Devices.NTamd64]\n%NVIDIA_DEV.2504% = Section001, PCI\\VEN_10DE&DEV_2504&SUBSYS_12345678\n\
                   %X% = S, pci\\ven_10de&dev_2504\nUSB\\VID_046d&PID_C52B&MI_00\nPCI\\VEN_ZZZZ&DEV_1\nHDAUDIO\\FUNC_01&VEN_10EC";
    let want: Vec<&str> = vec!["PCI\\VEN_10DE&DEV_2504", "USB\\VID_046D&PID_C52B"];
    assert_eq!(
        inf_hwids(&inf_text(inf.as_bytes()))
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        want
    );
    let mut le = vec![0xFF, 0xFE];
    le.extend(inf.encode_utf16().flat_map(u16::to_le_bytes));
    assert_eq!(
        inf_hwids(&inf_text(&le))
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        want
    );
    let mut be = vec![0xFE, 0xFF];
    be.extend(inf.encode_utf16().flat_map(u16::to_be_bytes));
    assert_eq!(inf_hwids(&inf_text(&be)).len(), 2);
}

#[test]
fn pick_by_hardware_group_or_all() {
    let d = |id, name: &str, hw: &[&str], all, groups: &[&str]| Driver {
        id,
        name: name.into(),
        sha256: String::new(),
        size: 0,
        hwids: hw.iter().map(|s| s.to_string()).collect(),
        all_machines: all,
        groups: groups.iter().map(|s| s.to_string()).collect(),
        created: 0,
    };
    let ds = [
        d(1, "nvidia", &["PCI\\VEN_10DE&DEV_2504"], false, &[]),
        d(2, "amd", &["PCI\\VEN_1002&DEV_73BF"], false, &["VIP"]),
        d(3, "tools", &[], true, &[]),
    ];
    let names = |hw: &[&str], grp| {
        let hw: BTreeSet<String> = hw.iter().map(|s| s.to_string()).collect();
        pick(&ds, &hw, grp)
            .iter()
            .map(|d| d.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(&["PCI\\VEN_10DE&DEV_2504"], None),
        ["nvidia", "tools"]
    );
    assert_eq!(names(&[], Some("vip")), ["amd", "tools"]); // group, case-insensitive
    assert_eq!(names(&[], None), ["tools"]);
    assert!(group_ok("VIP-1") && !group_ok("") && !group_ok("a b"));
}
