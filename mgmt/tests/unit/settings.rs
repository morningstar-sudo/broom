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

#[test]
fn dhcp_pool_need() {
    use crate::db::{Lease, Machine};
    let ip = |s: &str| s.parse::<std::net::Ipv4Addr>().unwrap();
    let m = |mac: &str, fixed: Option<&str>| Machine {
        mac: mac.into(),
        ip: fixed.map(Into::into),
        ..Default::default()
    };
    let l = |mac: &str, expires: i64| Lease {
        mac: mac.into(),
        ip: Some("10.0.0.150".into()),
        hostname: None,
        expires,
    };
    let machines = [
        m("aa:00:00:00:00:01", None),              // needs a pool address
        m("aa:00:00:00:00:02", Some("10.0.0.50")), // fixed: does not
        m("aa:00:00:00:00:03", Some("")),          // empty = none
    ];
    let leases = [
        l("AA:00:00:00:00:01", 2000), // a registered machine's lease: already counted
        l("bb:00:00:00:00:01", 2000), // a phone: counts
        l("bb:00:00:00:00:02", 500),  // expired: no
        l("declined-10.0.0.160", 2000), // a declined address: no
    ];
    assert_eq!(
        pool_need(ip("10.0.0.100"), ip("10.0.0.200"), &machines, &leases, 1000),
        (101, 3)
    );
    assert_eq!(
        pool_need(ip("10.0.0.20"), ip("10.0.0.10"), &[], &[], 0),
        (0, 0),
        "inverted range"
    );
}
