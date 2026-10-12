use super::*;

#[test]
fn broadcast_of_the_boot_lan() {
    assert_eq!(
        lan_broadcast("10.0.0.12", "255.255.255.0"),
        Ipv4Addr::new(10, 0, 0, 255)
    );
    assert_eq!(
        lan_broadcast("10.0.1.12", "255.255.254.0"),
        Ipv4Addr::new(10, 0, 1, 255)
    );
    assert_eq!(
        lan_broadcast("10.0.0.12", ""),
        Ipv4Addr::new(10, 0, 0, 255),
        "no netmask → /24"
    );
    assert_eq!(lan_broadcast("", ""), Ipv4Addr::BROADCAST);
}
