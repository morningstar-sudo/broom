use super::*;

#[test]
fn parse_full_and_minimal() {
    // Full reading from the agent.
    let json = r#"{"t":"metrics","cpu":12,"ram":40,"temps":[52,48],
        "disks":[{"name":"Samsung 980","health":"Healthy","temp":41,"realloc":0,"pending":0,"predict_fail":false},
                 {"name":"WD Blue","health":"Warning","temp":55,"realloc":8,"pending":2,"predict_fail":true}],
        "free":123456789}"#;
    let m: Metrics = serde_json::from_str(json).unwrap();
    assert_eq!(
        (m.cpu, m.ram, m.at),
        (12, 40, 0),
        "at is set on store, not parsed"
    );
    assert_eq!(m.temps, [52, 48]);
    assert_eq!(m.free, Some(123456789));
    assert_eq!(m.disks.len(), 2);
    assert_eq!(
        (
            m.disks[1].realloc,
            m.disks[1].pending,
            m.disks[1].predict_fail
        ),
        (Some(8), Some(2), true)
    );

    // A machine that exposes nothing but CPU/RAM: optional fields absent.
    let m2: Metrics = serde_json::from_str(r#"{"cpu":5,"ram":60}"#).unwrap();
    assert!(m2.temps.is_empty() && m2.disks.is_empty() && m2.free.is_none());

    // Garbage is dropped, not panicked.
    store("AA:BB:CC:00:00:09", "not json");
    assert!(get("aa:bb:cc:00:00:09").is_none());
}

#[test]
fn store_and_expire() {
    let mac = "aa:bb:cc:00:00:07";
    store(mac, r#"{"cpu":200,"ram":40}"#); // cpu clamped
    let m = get("AA:BB:CC:00:00:07").expect("recent reading, any case");
    assert_eq!(m.cpu, 100);

    // Make it stale and it disappears.
    METRICS.lock().get_mut(mac).unwrap().at = now().saturating_sub(STALE + 5);
    assert!(get(mac).is_none(), "stale reading hidden");

    forget(mac);
    assert!(!METRICS.lock().contains_key(mac));
}
