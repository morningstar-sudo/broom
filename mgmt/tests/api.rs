// The web admin's HTTP contract: first-run setup, sessions and the login throttle, the Host check, security headers,
// and the input checks of the main forms (network, images, machines).
mod common;

use common::clients::Http;
use common::*;
use serde_json::json;

#[test]
fn api_contract() {
    sandbox("api_contract", || {
        let home = Home::new("api");
        let _srv = Server::start(&home);
        let anon = Http::new();

        // First run: no password yet; setting it needs the token from the server log.
        let st = anon.get("/api/auth/status").json();
        assert_eq!(
            (st["configured"].as_bool(), st["authed"].as_bool()),
            (Some(false), Some(false))
        );
        assert_eq!(
            anon.post(
                "/api/auth/setup",
                &json!({"password": PASSWORD, "token": "nope"})
            )
            .status,
            403
        );
        assert_eq!(
            anon.post("/api/auth/login", &json!({"password": PASSWORD}))
                .status,
            409,
            "nothing to log in to"
        );
        let h = admin(&home);
        assert_eq!(
            anon.post(
                "/api/auth/setup",
                &json!({"password": "x".repeat(12), "token": "x"})
            )
            .status,
            409
        );
        assert_eq!(h.get("/api/auth/status").json()["authed"], true);

        // Sessions: data needs one, boot endpoints don't; a page navigation is sent to /login.
        assert_eq!(anon.get("/api/status").status, 401);
        assert_eq!(h.get("/api/status").status, 200);
        assert_eq!(anon.get("/boot.ipxe").status, 200);
        let nav = anon.request("GET", "/machines", &[("Accept", "text/html")], b"");
        assert_eq!((nav.status, nav.header("location")), (303, Some("/login")));
        let forged = Http {
            cookie: Some("broom_session=99999999999.00ff".into()),
        };
        assert_eq!(
            forged.get("/api/status").status,
            401,
            "unsigned cookie refused"
        );

        // DNS rebinding: a foreign Host name is refused even with a session; IP literals and localhost pass.
        assert_eq!(
            h.request("GET", "/api/status", &[("Host", "evil.example")], b"")
                .status,
            421
        );
        assert_eq!(
            h.request("GET", "/api/status", &[("Host", "localhost:80")], b"")
                .status,
            200
        );

        let r = anon.get("/login");
        assert!(
            r.header("content-security-policy")
                .is_some_and(|c| c.contains("script-src 'self'"))
        );
        assert_eq!(r.header("x-frame-options"), Some("DENY"));

        // Network: refused unless complete and real; nothing saved on a refusal.
        let net = |v: serde_json::Value| h.post("/api/dhcp", &v).status;
        assert_eq!(
            net(json!({"mode": "full", "iface": "nope0"})),
            400,
            "unknown interface"
        );
        assert_eq!(
            net(json!({"mode": "full", "iface": SRV_IF, "server_ip": SRV})),
            400,
            "no gateway/DNS"
        );
        assert_eq!(net(json!({"server_ip": "10.0.0.1\nx"})), 400, "injection");
        assert_eq!(
            net(json!({"mode": "off", "server_ip": "192.0.2.7"})),
            400,
            "not our address"
        );
        assert_eq!(
            h.get("/api/dhcp").json()["server_ip"]
                .as_str()
                .unwrap_or(""),
            "",
            "nothing saved"
        );
        configure_network(&h);
        assert_eq!(h.get("/api/dhcp").json()["server_ip"], SRV);

        // Images: name + os checked, listed, deleted.
        let img = |v: serde_json::Value| h.post("/api/images", &v);
        assert_eq!(img(json!({"name": "../x", "os": "linux"})).status, 400);
        assert_eq!(img(json!({"name": "a", "os": "dos"})).status, 400);
        let id = img(json!({"name": "web1", "os": "windows"})).json()["id"]
            .as_i64()
            .unwrap();
        assert_eq!(
            img(json!({"name": "web1", "os": "windows"})).status,
            400,
            "duplicate name"
        );
        assert!(h.get("/api/images").text().contains("\"web1\""));
        assert_eq!(
            h.post("/api/images/upload-done", &json!({"name": "web1"}))
                .status,
            400,
            "no upload started"
        );
        assert_eq!(h.post("/api/images/delete", &json!({"id": id})).status, 200);
        assert!(!h.get("/api/images").text().contains("\"web1\""));

        // Machines: MAC normalized, duplicates and bad hostnames refused, CSV export.
        let add = |v: serde_json::Value| h.post("/api/machines", &v).status;
        assert_eq!(
            add(json!({"mac": "52-54-00-AA-BB-CC", "hostname": "pc-a"})),
            200
        );
        assert_eq!(add(json!({"mac": "52:54:00:aa:bb:cc"})), 400, "same MAC");
        assert_eq!(add(json!({"mac": "zz:54:00:aa:bb:cd"})), 400, "bad MAC");
        assert_eq!(
            add(json!({"mac": "52:54:00:aa:bb:cd", "hostname": "<b>"})),
            400,
            "bad hostname"
        );
        let csv = h.get("/api/machines/export.csv").text();
        assert!(
            csv.contains("52:54:00:aa:bb:cc") && csv.contains("pc-a"),
            "{csv}"
        );

        // Login throttle: 5 free wrong tries, then this IP is locked out (1 s, doubling) → 429.
        let wrong = || {
            anon.post("/api/auth/login", &json!({"password": "wrong-pass"}))
                .status
        };
        for _ in 0..5 {
            assert_eq!(wrong(), 401);
        }
        assert!(
            (0..3).any(|_| wrong() == 429),
            "locked out after 5 wrong passwords"
        );
    });
}
