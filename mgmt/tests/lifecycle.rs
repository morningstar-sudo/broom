// The server as a process: first start on an empty directory, restart keeping its data, one instance per data
// directory and per port, and the iSCSI daemon outliving the server (clients keep their disk) or being restarted by it.
mod common;

use common::clients::{Http, Iscsi};
use common::*;
use serde_json::json;

#[test]
fn restart_keeps_data() {
    sandbox("restart_keeps_data", || {
        let home = Home::new("restart");
        let srv = Server::start(&home);
        assert!(
            home.0.join("bootrom.db").is_file(),
            "database created on first start"
        );
        let h = admin(&home);
        assert_eq!(
            h.post(
                "/api/machines",
                &json!({"mac": "52:54:00:00:00:01", "hostname": "pc-r"})
            )
            .status,
            200
        );
        srv.stop();

        let _srv = Server::start(&home);
        assert_eq!(
            Http::new().get("/api/auth/status").json()["configured"],
            true
        );
        assert_eq!(
            h.get("/api/status").status,
            200,
            "session still valid (secret kept)"
        );
        assert!(
            login()
                .get("/api/machines/export.csv")
                .text()
                .contains("pc-r")
        );
    });
}

#[test]
fn one_instance_per_home_and_port() {
    sandbox("one_instance_per_home_and_port", || {
        let home = Home::new("single");
        let _srv = Server::start(&home);

        // Same data directory on another port: past the HTTP bind, stopped by the lock.
        let mut twin = Server::spawn(&home, &["--port", "8080"]);
        assert_eq!(
            twin.exit_within(20),
            Some(1),
            "second instance on the same data must exit"
        );
        assert!(home.log().contains("already running on"), "{}", home.log());

        // Another data directory on the same port: stopped by the HTTP bind, before touching anything.
        let other = Home::new("single-other");
        let mut port = Server::spawn(&other, &[]);
        assert_eq!(
            port.exit_within(20),
            Some(1),
            "second instance on port 80 must exit"
        );
        assert!(other.log().contains("bind http"), "{}", other.log());
        assert!(
            !other.0.join("bootrom.db").exists(),
            "nothing created by the refused instance"
        );
        assert_eq!(
            Http::new().get("/api/auth/status").status,
            200,
            "the first one still serves"
        );
    });
}

#[test]
fn iscsi_daemon_outlives_server() {
    sandbox("iscsi_daemon_outlives_server", || {
        let home = Home::new("iscsid");
        let srv = Server::start(&home);
        let h = admin(&home);
        configure_network(&h);
        let g = linux_golden(&home.0);
        publish_golden(&h, "ubu", &g);
        let iqn = iqn_of(
            h.get("/api/images").json()[0]["boot_script"]
                .as_str()
                .expect("boot_script"),
        );
        let check = |what: &str| {
            let mut s =
                Iscsi::login(&iqn).unwrap_or_else(|e| panic!("{what}: login refused {e:?}"));
            assert!(
                s.read(2048, 16) == g.disk[2048 * 512..2064 * 512],
                "{what}: data"
            );
        };
        check("published");

        // Server stopped: clients keep their disk (a session open before keeps working too).
        let mut open = Iscsi::login(&iqn).unwrap();
        srv.stop();
        check("server stopped");
        assert!(
            open.read(0, 8) == g.disk[..8 * 512],
            "open session survives the server stop"
        );
        drop(open);

        // Daemon killed while the server is down: the next start brings it back with the target.
        let daemons = pids_of("iscsid --state");
        assert_eq!(daemons.len(), 1, "one iSCSI daemon: {daemons:?}");
        // SAFETY: kill(2) on a pid read from /proc of this PID namespace.
        unsafe { libc::kill(daemons[0], libc::SIGKILL) };
        let _srv = Server::start(&home);
        let t = std::time::Instant::now();
        while Iscsi::try_login(&iqn).is_none() {
            assert!(
                t.elapsed().as_secs() < 30,
                "target not restored:\n{}",
                home.log()
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        check("restarted");
    });
}
