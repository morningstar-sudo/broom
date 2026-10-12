use super::*;

#[test]
fn checksum_rfc1071() {
    // Echo request id=1 seq=1, no payload: 0x0800 + 0x0001 + 0x0001 = 0x0802 → !0x0802 = 0xf7fd.
    assert_eq!(icmp_checksum(&[8, 0, 0, 0, 0, 1, 0, 1]), 0xf7fd);
    // A packet with its checksum filled in sums to zero.
    assert_eq!(icmp_checksum(&[8, 0, 0xf7, 0xfd, 0, 1, 0, 1]), 0);
}

/// Real ICMP (root): `cargo test -- --ignored ping_live`.
#[test]
#[ignore]
fn ping_live() {
    assert!(ping("127.0.0.1"));
    assert!(!ping("192.0.2.1")); // TEST-NET-1: never answers
}
