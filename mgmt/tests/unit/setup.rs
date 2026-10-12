use super::cidr_parts;

#[test]
fn dns_from_resolv_conf() {
    assert_eq!(
        super::nameservers("nameserver 127.0.0.53\noptions edns0\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        super::nameservers("# x\nnameserver 192.168.1.1\nnameserver  8.8.8.8\nnameserver ::1\n"),
        ["192.168.1.1", "8.8.8.8"]
    );
}

#[test]
fn unit_runs_binary_from_its_home() {
    let env = [("BOOTROM_DB".to_string(), "sqlite:///data/b.db".to_string())];
    let u = super::unit_text(
        "/opt/bootrom/bootrom-mgmt",
        "/opt/bootrom",
        &["--port".to_string(), "8080".to_string()],
        &env,
    );
    assert!(u.contains("\nExecStart=/opt/bootrom/bootrom-mgmt --port 8080\n"));
    assert!(u.contains("\nWorkingDirectory=/opt/bootrom\n"));
    assert!(u.contains("\nEnvironment=\"BOOTROM_DB=sqlite:///data/b.db\"\n"));
    assert!(u.contains("\nWantedBy=multi-user.target\n") && u.contains("\nRestart=on-failure\n"));
}
#[test]
fn cidr_to_netmask() {
    assert_eq!(
        cidr_parts("10.0.0.0/23"),
        Some(("10.0.0.0".into(), "255.255.254.0".into()))
    );
    assert_eq!(
        cidr_parts("192.168.1.0/24"),
        Some(("192.168.1.0".into(), "255.255.255.0".into()))
    );
    assert_eq!(
        cidr_parts("10.0.0.0"),
        Some(("10.0.0.0".into(), "255.255.255.0".into())),
        "no prefix → /24"
    );
    assert_eq!(cidr_parts("10.0.0.0/33"), None);
}
