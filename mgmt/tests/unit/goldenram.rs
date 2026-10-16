use super::*;

#[test]
fn ranges() {
    let r = |s: &str| range(Some(&HeaderValue::from_str(s).unwrap()), 1000);
    assert_eq!(range(None, 1000), Ok(None));
    assert_eq!(r("bytes=100-"), Ok(Some((100, 999))), "wget -c resume");
    assert_eq!(r("bytes=0-99"), Ok(Some((0, 99))));
    assert_eq!(r("bytes=900-5000"), Ok(Some((900, 999))), "end clamped");
    assert_eq!(r("bytes=-10"), Ok(Some((990, 999))), "suffix");
    assert_eq!(r("bytes=1000-"), Err(()), "past the end → 416");
    assert_eq!(r("bytes=5-1"), Err(()));
    assert_eq!(r("bytes=0-1,5-6"), Ok(None), "multi-range → whole file");
    assert_eq!(r("items=0-1"), Ok(None));
}

/// The route coexists with the /tftp file service (axum would panic on a conflicting route at start).
#[test]
fn route_beside_serve_dir() {
    let _: axum::Router = axum::Router::new()
        .route(
            "/tftp/broom-win/{name}/golden.vhdx",
            axum::routing::get(get),
        )
        .nest_service("/tftp", tower_http::services::ServeDir::new("."));
}

#[test]
fn limit_auto_and_admin() {
    assert_eq!(limit_for("auto", Some(20_000)), 20, "bond 2x10G");
    assert_eq!(limit_for("auto", Some(1_000)), 4, "1G: at least 4");
    assert_eq!(limit_for("auto", Some(100_000)), 64, "capped");
    assert_eq!(limit_for("auto", None), 8, "speed unknown");
    assert_eq!(limit_for("12", Some(20_000)), 12, "admin's number wins");
    assert_eq!(limit_for("0", Some(20_000)), 0, "0 = no queue");
    assert_eq!(limit_for("9999", None), MAX_STREAMS);
    assert_eq!(limit_for("junk", Some(10_000)), 10, "unreadable → auto");
}

#[test]
fn download_queue() {
    let ip = |n: u8| IpAddr::from([10, 0, 0, n]);
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let mut q = Queue {
        limit: 2,
        ..Default::default()
    };
    assert_eq!(q.decide(ip(1), at(0)), Slot::Go);
    assert_eq!(q.decide(ip(2), at(0)), Slot::Go);
    assert_eq!(q.decide(ip(3), at(0)), Slot::Wait(1, 1));
    assert_eq!(q.decide(ip(4), at(0)), Slot::Wait(2, 2));
    assert!(!q.start(ip(3), at(1)), "no place → 503");
    assert!(q.start(ip(1), at(1)) && q.start(ip(2), at(1)));
    assert_eq!(
        q.decide(ip(1), at(2)),
        Slot::Go,
        "a running download keeps its place"
    );

    // Cut download: the place is held for the resume, nobody moves up.
    q.finish(ip(1), at(5), false);
    assert_eq!(q.decide(ip(3), at(6)), Slot::Wait(1, 2));
    assert_eq!(q.decide(ip(1), at(7)), Slot::Go);
    assert!(q.start(ip(1), at(7)));

    // Done: the place goes to the first waiting at once.
    q.finish(ip(2), at(8), true);
    assert_eq!(q.decide(ip(4), at(9)), Slot::Wait(2, 2), "keeps its order");
    assert_eq!(q.decide(ip(3), at(9)), Slot::Go);
    assert_eq!(q.decide(ip(4), at(10)), Slot::Wait(1, 1));

    // A granted place not used within HOLD is lost; a waiting machine that stopped asking leaves.
    assert_eq!(q.decide(ip(5), at(10)), Slot::Wait(2, 2));
    assert_eq!(q.decide(ip(4), at(35)), Slot::Wait(1, 2));
    assert_eq!(
        q.decide(ip(4), at(60)),
        Slot::Wait(1, 1),
        "ip5 stopped asking"
    );
    assert_eq!(
        q.decide(ip(4), at(70)),
        Slot::Go,
        "ip3 never started: its place is free"
    );
    assert_eq!(q.decide(ip(5), at(71)), Slot::Wait(1, 1), "back at the end");

    q.limit = 0;
    assert_eq!(q.decide(ip(6), at(72)), Slot::Go, "no queue");
    assert!(q.start(ip(7), at(72)));
}
