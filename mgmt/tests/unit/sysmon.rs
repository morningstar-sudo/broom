use super::*;

#[test]
fn alert_after_a_streak_and_cleared_below() {
    let mut s = HashMap::new();
    let at =
        |cpu: f64, s: &mut HashMap<_, _>| alerts(&[("CPU", Some(cpu)), ("Network", None)], 80.0, s);
    assert!(
        at(95.0, &mut s).is_empty() && at(85.0, &mut s).is_empty(),
        "a short burst is not an alert"
    );
    assert_eq!(at(80.0, &mut s), vec!["CPU 80%"]);
    assert!(at(79.0, &mut s).is_empty(), "back under → cleared at once");
    assert!(at(99.0, &mut s).is_empty(), "the streak starts again");
}

#[test]
fn reads_this_machine() {
    assert!(mem_total() > 0 && fd_limit() > 0);
    let (total, idle) = cpu_times();
    assert!(total >= idle && total > 0);
    assert!(net_bytes("lo").is_some(), "loopback is in /proc/net/dev");
    assert!(space(Path::new("/")).is_some_and(|(used, total)| total > 0 && used <= total));
    assert!(
        disk_stat(Path::new("/")).is_none_or(|d| !d.name.is_empty()),
        "a block device, or none (overlay / tmpfs root)"
    );
    assert!(ram_share(10, 1 << 20, 7) >= 7);
}

#[test]
fn whole_disk_names() {
    assert_eq!(whole_disk("nvme0n1p2"), "nvme0n1");
    assert_eq!(whole_disk("nvme10n1p3"), "nvme10n1");
    assert_eq!(whole_disk("sda2"), "sda");
    assert_eq!(whole_disk("sdb"), "sdb");
    assert_eq!(whole_disk("vda1"), "vda");
}

#[test]
fn ata_smart_attributes() {
    let mut d = [0u8; 512];
    // Entry at offset 2: id 5 (reallocated), raw = 8.
    d[2] = 5;
    d[7] = 8;
    // Next entry: id 194 (temperature) raw low byte = 42.
    d[14] = 194;
    d[19] = 42;
    // Next: id 197 (pending) raw = 3.
    d[26] = 197;
    d[31] = 3;
    assert_eq!(parse_ata_smart(&d), (Some(8), Some(3), Some(42)));
    // A healthy drive: no 5/197/194 entries.
    assert_eq!(parse_ata_smart(&[0u8; 512]), (None, None, None));
}
