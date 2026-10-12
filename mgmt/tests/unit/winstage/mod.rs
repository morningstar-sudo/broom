/// The stage reuses iPXE's address (static, card by BOOTIF): no second DHCP + duplicate-address probe.
#[test]
fn boot_script_reuses_ipxe_address() {
    let s = super::boot_script("win11", "abc");
    assert!(
        s.contains(" ip=${ip}::${gateway}:${netmask}:::off BOOTIF=01-${mac:hexhyp} "),
        "{s}"
    );
    assert!(!s.contains("ip=dhcp"));
    assert!(s.contains("broom.name=win11 broom.hash=abc ") && s.ends_with("boot"));
}

#[test]
fn ntfs_parts_order() {
    use crate::disk::{Part, Table};
    let part = |s: u64, n: u64, k: &str| Part {
        start: s * 512,
        size: n * 512,
        kind: k.into(),
    };
    // UEFI VM: ESP, MSR, C:, Recovery (other types) → only C: is basic data.
    let t = Table {
        gpt: true,
        sector: 512,
        parts: vec![
            part(2048, 204800, "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
            part(206848, 32768, "E3C9E316-0B5C-4DB8-817D-F92DF00215AE"),
            part(239616, 124000000, "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"),
            part(124239616, 1000000, "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC"),
        ],
    };
    assert_eq!(super::ntfs_parts(&t), vec![(239616 * 512, 124000000 * 512)]);
    // MBR (BIOS VM): type "7", largest first.
    let t = Table {
        gpt: false,
        sector: 512,
        parts: vec![part(2048, 100000, "7"), part(102048, 9000000, "7")],
    };
    let p = super::ntfs_parts(&t);
    assert_eq!((p[0].0, p[1].0), (102048 * 512, 2048 * 512));
}
