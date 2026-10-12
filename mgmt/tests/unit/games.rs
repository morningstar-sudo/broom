use super::*;

#[test]
fn drive_letters_clash_only_for_shared_machines() {
    let d = |name: &str, letter: &str, groups: &[&str]| Disk {
        name: name.into(),
        size_gb: 1,
        letter: letter.into(),
        groups: groups.iter().map(|g| g.to_string()).collect(),
        update: None,
    };
    assert!(
        clash(&d("a", "G", &[]), &d("b", "G", &["VIP"])),
        "every machine vs VIP"
    );
    assert!(
        clash(&d("a", "G", &["vip", "x"]), &d("b", "G", &["VIP"])),
        "groups overlap (any case)"
    );
    assert!(
        !clash(&d("a", "G", &["Thuong"]), &d("b", "G", &["VIP"])),
        "different machines: same letter is fine"
    );
    assert!(
        !clash(&d("a", "G", &[]), &d("b", "H", &[])),
        "different letters"
    );
    assert!(!clash(&d("a", "G", &[]), &d("a", "G", &[])), "itself");
    assert!(
        for_group(&[], None)
            && for_group(&["VIP".into()], Some("vip"))
            && !for_group(&["VIP".into()], None)
    );
    assert_eq!(
        clean_groups(&["VIP, Thuong ,vip".into(), "".into()]).unwrap(),
        ["VIP", "Thuong"]
    );
    assert!(
        clean_groups(&["Thường".into()]).is_err(),
        "same rule as a machine's group"
    );
}
