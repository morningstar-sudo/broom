#[test]
fn swap_renames_removes_merges() {
    let g = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        super::swap(&g(&["VIP", "Staff"]), "vip", Some("Gold")),
        g(&["Gold", "Staff"])
    );
    assert_eq!(
        super::swap(&g(&["VIP", "Staff"]), "VIP", None),
        g(&["Staff"])
    );
    assert_eq!(
        super::swap(&g(&["VIP", "Gold"]), "VIP", Some("gold")),
        g(&["gold"]),
        "merged into an existing one"
    );
}
