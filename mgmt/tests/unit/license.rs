use super::*;

#[test]
fn license_key_and_lookup() {
    assert!(key_ok("ABCDE-12345-FGHIJ-67890-KLMNO"));
    for bad in [
        "",
        "abcde-12345-fghij-67890-klmno",
        "ABCDE-12345-FGHIJ-67890",
        "ABCDE-12345-FGHIJ-67890-KLMN!",
        "ABCDE12345FGHIJ67890KLMNO1234",
    ] {
        assert!(!key_ok(bad), "{bad}");
    }
    let m = |id, mac: &str, ip: Option<&str>| Machine {
        id,
        mac: mac.into(),
        ip: ip.map(Into::into),
        hostname: Some(format!("PC{id}")),
        image_id: None,
        license_key: None,
        license_tail: None,
        license_state: None,
        license_gen: 0,
        license_result: None,
        grp: None,
        notes: None,
    };
    let ms = [
        m(1, "aa:00:00:00:00:01", Some("10.0.0.51")),
        m(2, "AA:00:00:00:00:02", None),
    ];
    // Only a machine's OWN bound IP counts, never a lease.
    assert_eq!(machine_by_ip("10.0.0.51", &ms, None).map(|m| m.id), None); // bound IP, no ARP entry (routed peer) → refused
    assert_eq!(
        machine_by_ip("10.0.0.51", &ms, Some("aa:00:00:00:00:01")).map(|m| m.id),
        Some(1)
    ); // ARP matches
    assert_eq!(
        machine_by_ip("10.0.0.51", &ms, Some("bb:bb:bb:bb:bb:bb")).map(|m| m.id),
        None
    ); // ARP MAC mismatch → spoofed
    assert_eq!(machine_by_ip("10.0.0.102", &ms, None).map(|m| m.id), None); // a lease IP is not accepted
    assert_eq!(machine_by_ip("10.0.0.200", &ms, None).map(|m| m.id), None);
}
