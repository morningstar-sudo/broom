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
