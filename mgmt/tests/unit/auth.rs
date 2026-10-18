use super::*;
use axum::http::Method;

#[test]
fn host_header() {
    for ok in [
        "10.0.0.12",
        "10.0.0.12:80",
        "192.168.1.5",
        "[::1]:80",
        "[fe80::1]",
        "localhost:8080",
        "broom",
        "BROOM.local",
    ] {
        assert!(host_allowed(ok, "broom"), "{ok}");
    }
    for bad in [
        "evil.example.com",
        "evil.example.com:80",
        "10.0.0.12.nip.io",
        "broomx",
    ] {
        assert!(!host_allowed(bad, "broom"), "{bad}");
    }
    assert!(
        !host_allowed("anything", ""),
        "no hostname known → only IPs / localhost"
    );
}

#[test]
fn hash_and_verify() {
    let h = hash_pw("correct horse").unwrap();
    assert!(h.starts_with("$argon2"));
    assert!(verify_pw("correct horse", &h));
    assert!(!verify_pw("wrong", &h));
    assert_ne!(hash_pw("x").unwrap(), hash_pw("x").unwrap(), "random salt");
}

#[test]
fn public_paths() {
    let pub_get = |p| is_public(&Method::GET, p);
    let pub_post = |p| is_public(&Method::POST, p);
    assert!(
        pub_get("/login") && pub_get("/login.js"),
        "the standalone sign-in page + its script are public"
    );
    assert!(
        pub_get("/app.css")
            && pub_get("/bootstrap.min.css")
            && !pub_get("/bootstrap.bundle.min.js"),
        "its stylesheets too; the app's scripts are not"
    );
    assert!(pub_get("/boot.ipxe") && pub_get("/tftp/broom/x/vmlinuz"));
    assert!(pub_post("/api/license") && pub_post("/api/auth/login") && pub_post("/api/booted"));
    assert!(!pub_get("/api/golden-chunk"), "delta chunks are gone");
    assert!(
        pub_get("/broom-prep-win") && !pub_post("/api/images/base-mode"),
        "prep script open, admin APIs not"
    );
    // guarded now: the app shell + its fragments (login is a separate page)
    assert!(!pub_get("/") && !pub_get("/machines") && !pub_get("/ui/images"));
    assert!(!pub_get("/api/status") && !pub_get("/api/images") && !pub_post("/api/images/delete"));
    assert!(!pub_post("/api/dhcp") && !pub_post("/api/machines/import") && !pub_get("/api/events"));
    assert!(
        !pub_post("/api/password"),
        "changing the password needs a session"
    );
}

#[test]
fn throttle_per_ip() {
    let mut f = Fails::new();
    let ip: IpAddr = "10.0.0.5".parse().unwrap();
    let other: IpAddr = "10.0.0.6".parse().unwrap();
    for _ in 0..4 {
        record(&mut f, ip, false, 100);
        assert_eq!(locked_for(&f, ip, 100), 0, "first tries are free");
    }
    record(&mut f, ip, false, 100);
    assert_eq!(locked_for(&f, ip, 100), 1, "5th failure locks 1 s");
    record(&mut f, ip, false, 101);
    assert_eq!(locked_for(&f, ip, 101), 2, "then doubles");
    assert_eq!(locked_for(&f, ip, 103), 0, "lock expires");
    assert_eq!(locked_for(&f, other, 101), 0, "other IPs unaffected");
    for _ in 0..30 {
        record(&mut f, ip, false, 200);
    }
    assert_eq!(locked_for(&f, ip, 200), MAX_LOCK_SECS, "capped");
    record(&mut f, ip, true, 1000);
    assert!(f.is_empty(), "success forgets the IP");
    record(&mut f, ip, false, 0);
    record(&mut f, other, false, 10_000);
    assert!(!f.contains_key(&ip), "idle entries dropped");
}

#[test]
fn setup_token_compare() {
    assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", ""));
}

/// Staff: every page, but of the changes only Wake / games Save, Discard / their own password / logout.
#[test]
fn staff_rights() {
    for p in [
        "/api/status",
        "/api/machines/detail",
        "/api/vnc/view",
        "/api/logs",
        "/api/games",
    ] {
        assert!(staff_may(&Method::GET, p), "GET {p}");
    }
    for p in [
        "/api/machines/export.csv",
        "/api/images/export-file",
        "/api/users",
    ] {
        assert!(!staff_may(&Method::GET, p), "GET {p}");
    }
    for p in [
        "/api/wake",
        "/api/games/save",
        "/api/games/discard",
        "/api/password",
        "/api/auth/logout",
    ] {
        assert!(staff_may(&Method::POST, p), "POST {p}");
    }
    for p in [
        "/api/images/delete",
        "/api/volumes/format",
        "/api/dhcp",
        "/api/users",
        "/api/disks/broom",
        "/api/machines/update",
    ] {
        assert!(!staff_may(&Method::POST, p), "POST {p}");
    }
    assert!(!staff_may(&Method::PUT, "/api/images/upload-chunk"));
    assert!(name_ok("thu-ngan") && name_ok("admin_2"));
    assert!(!name_ok("Admin") && !name_ok("a.b") && !name_ok("") && !name_ok(&"x".repeat(33)));
}
