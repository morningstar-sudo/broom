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
