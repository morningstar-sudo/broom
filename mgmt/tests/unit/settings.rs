use super::*;

#[test]
fn dhcp_field_validation() {
    assert!(dhcp_field_ok("dhcp_server_ip", "10.0.0.12").is_ok());
    assert!(dhcp_field_ok("dhcp_server_ip", "10.0.0.999").is_err());
    assert!(dhcp_field_ok("dhcp_server_ip", "10.0.0.12\nfoo").is_err()); // newline injection into scripts
    assert!(
        dhcp_field_ok("dhcp_mode", "full").is_ok() && dhcp_field_ok("dhcp_mode", "off").is_ok()
    );
    assert!(
        dhcp_field_ok("dhcp_mode", "proxy").is_err()
            && dhcp_field_ok("dhcp_mode", "rogue").is_err()
    );
    assert!(
        dhcp_field_ok("dhcp_dns", "8.8.8.8, 1.1.1.1").is_ok()
            && dhcp_field_ok("dhcp_dns", "").is_ok()
    );
    assert!(dhcp_field_ok("dhcp_dns", "8.8.8.8,notip").is_err());
    assert!(
        dhcp_field_ok("dhcp_iface", "eth0").is_ok()
            && dhcp_field_ok("dhcp_iface", "eth 0;rm").is_err()
    );
    assert!(
        dhcp_field_ok("dhcp_lease", "12h").is_ok() && dhcp_field_ok("dhcp_lease", "3600").is_ok()
    );
    assert!(dhcp_field_ok("dhcp_lease", "1d").is_ok() && dhcp_field_ok("dhcp_lease", "h").is_err());
    // DHCP server off (fresh install): the range fields are blank and must not block Apply.
    assert!(
        dhcp_field_ok("dhcp_range_start", "").is_ok() && dhcp_field_ok("dhcp_subnet", "").is_ok()
    );
    assert!(dhcp_field_ok("dhcp_range_end", "10.0.0.x").is_err());
    // Option 3 carries one router; a list would be silently dropped.
    assert!(
        dhcp_field_ok("dhcp_gateway", "10.0.0.1").is_ok()
            && dhcp_field_ok("dhcp_gateway", "10.0.0.1,10.0.0.2").is_err()
    );
}

#[test]
fn dhcp_range_checked() {
    assert!(range_ok("10.0.0.100", "10.0.0.200", "10.0.0.12", "255.255.255.0").is_ok());
    assert!(
        range_ok("10.0.1.100", "10.0.1.200", "10.0.0.12", "255.255.254.0").is_ok(),
        "/23"
    );
    assert!(
        range_ok("10.0.0.200", "10.0.0.100", "10.0.0.12", "").is_err(),
        "start after end"
    );
    assert!(
        range_ok("10.0.1.100", "10.0.1.200", "10.0.0.12", "").is_err(),
        "outside the /24"
    );
    assert!(
        range_ok("", "10.0.0.200", "10.0.0.12", "").is_err(),
        "blank start in full mode"
    );
}

#[test]
fn dns_list_normalized() {
    assert_eq!(
        normalize_dns("8.8.8.8 1.1.1.1, 8.8.8.8;9.9.9.9").unwrap(),
        "8.8.8.8,1.1.1.1,9.9.9.9"
    );
    assert_eq!(normalize_dns("  ").unwrap(), "");
    assert!(
        normalize_dns("8.8.8.8, dns.google")
            .unwrap_err()
            .contains("dns.google")
    );
    let nine: Vec<String> = (1..=9).map(|i| format!("10.0.0.{i}")).collect();
    assert!(
        normalize_dns(&nine.join(","))
            .unwrap_err()
            .contains("at most 8")
    );
}
