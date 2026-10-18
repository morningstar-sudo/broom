// Shared by the integration tests: each test runs as root in its own namespaces (sandbox), the real binary is the
// server in a network namespace of its own ("srv", behind a veth pair), and the test talks to it from the other end
// like a client PC would (clients.rs: DHCP, TFTP, HTTP, iSCSI).
#![allow(dead_code)] // every test file uses a different part

pub mod clients;

use clients::Http;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Server side of the veth pair (netns "srv") and the client side (the test's own netns).
pub const SRV: &str = "10.99.0.1";
pub const CLI: &str = "10.99.0.2";
pub const SRV_IF: &str = "srv0";
pub const CLI_IF: &str = "cli0";
pub const PASSWORD: &str = "integration-test";
const BIN: &str = env!("CARGO_BIN_EXE_bootrom-mgmt");
const INNER: &str = "BROOM_IT_INNER";

/// Run `body` as root in fresh user + network + mount + PID namespaces: ports 67/69/80/3260 bind without sudo, /run is
/// a private tmpfs (no systemd, no host sockets or locks), and every process started inside (the server, its iSCSI
/// daemon) dies with the test. The test runs itself again inside `unshare`; outside it only checks that run passed.
pub fn sandbox(test: &str, body: impl FnOnce()) {
    if std::env::var_os(INNER).is_some() {
        net_setup();
        body();
        return;
    }
    let out = Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--net",
            "--mount",
            "--pid",
            "--fork",
            "--mount-proc",
            "--",
        ])
        .arg(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(INNER, "1")
        .output()
        .expect("run unshare (util-linux)");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    print!("{stdout}");
    eprint!("{stderr}");
    assert!(
        out.status.success(),
        "{test} failed inside its sandbox. If unshare itself was refused (\"Operation not permitted\"), this host blocks \
         unprivileged user namespaces: sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0 (Ubuntu 24.04) \
         or kernel.unprivileged_userns_clone=1"
    );
    assert!(
        stdout.contains("1 passed"),
        "{test}: the sandboxed run did not run the test"
    );
}

fn sh(script: &str) {
    let st = Command::new("sh").args(["-ec", script]).status().unwrap();
    assert!(st.success(), "{script}");
}

/// /run as tmpfs, then netns "srv" joined to this one by a veth pair: srv0 = SRV, cli0 = CLI.
fn net_setup() {
    sh(&format!(
        "mount -t tmpfs tmpfs /run
         ip link set lo up
         ip netns add srv
         ip link add {CLI_IF} type veth peer name {SRV_IF} netns srv
         ip addr add {CLI}/24 dev {CLI_IF}
         ip link set {CLI_IF} up
         ip -n srv link set lo up
         ip -n srv addr add {SRV}/24 dev {SRV_IF}
         ip -n srv link set {SRV_IF} up"
    ));
}

/// A server data directory (BOOTROM_HOME), removed when the test passes (kept for a look when it fails).
pub struct Home(pub PathBuf);

impl Home {
    pub fn new(test: &str) -> Home {
        let d = std::env::temp_dir().join(format!("broom-it-{test}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Home(d)
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.0.join("server.log")).unwrap_or_default()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("server data + log kept in {}", self.0.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// The real binary, started in netns "srv" on `home` (log appended to <home>/server.log).
pub struct Server {
    child: Child,
    home: PathBuf,
}

impl Server {
    /// Start and wait until the web answers.
    pub fn start(home: &Home) -> Server {
        let s = Server::spawn(home, &[]);
        s.wait_up();
        s
    }

    /// Start without waiting (for runs that are expected to exit).
    pub fn spawn(home: &Home, args: &[&str]) -> Server {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(home.0.join("server.log"))
            .unwrap();
        let child = Command::new("ip")
            .args(["netns", "exec", "srv", BIN, "--skip-preflight"])
            .args(args)
            .current_dir(&home.0) // old-layout migration looks at the current directory
            .env("BOOTROM_HOME", &home.0)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        Server {
            child,
            home: home.0.clone(),
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.home.join("server.log")).unwrap_or_default()
    }

    fn wait_up(&self) {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(30) {
            if std::net::TcpStream::connect((SRV, 80)).is_ok()
                && Http::new().get("/api/auth/status").status == 200
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("server not up after 30 s:\n{}", self.log());
    }

    /// Exit code of a run that should end by itself (None = still running after `secs`).
    pub fn exit_within(&mut self, secs: u64) -> Option<i32> {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(secs) {
            if let Some(st) = self.child.try_wait().unwrap() {
                return Some(st.code().unwrap_or(-1));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// SIGTERM, then SIGKILL after 10 s. The iSCSI daemon is its own process and keeps running.
    pub fn stop(mut self) {
        self.kill();
    }

    fn kill(&mut self) {
        // SAFETY: kill(2) on our own child's pid.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        if self.exit_within(10).is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.kill();
        }
    }
}

/// The sign-in handshake, written from its description (pwproof.rs header), not from broom's code.
pub mod pw {
    use hmac::{KeyInit, Mac};
    use sha2::{Digest, Sha256};

    pub fn mac(k: &[u8], m: &[u8]) -> [u8; 32] {
        let mut h = hmac::Hmac::<Sha256>::new_from_slice(k).unwrap();
        h.update(m);
        h.finalize().into_bytes().into()
    }
    pub fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    pub fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
    fn x(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
        std::array::from_fn(|i| a[i] ^ b[i])
    }
    /// (ClientKey, stored, server) of a password.
    pub fn keys(pw: &str, salt: &[u8], iter: u32) -> ([u8; 32], [u8; 32], [u8; 32]) {
        let mut k = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(pw.as_bytes(), salt, iter, &mut k);
        let c = mac(&k, b"client");
        (c, Sha256::digest(c).into(), mac(&k, b"server"))
    }
    pub fn setup_key(token: &str) -> [u8; 32] {
        mac(token.trim().as_bytes(), b"broom-setup")
    }
    /// A new password's verifier for `user`, sealed with `key` (the fewest rounds the server takes: quick tests).
    pub fn seal(key: &[u8; 32], user: &str, pw: &str) -> serde_json::Value {
        let (salt, iter) = (hex(&Sha256::digest(user.as_bytes())[..16]), 100_000u32);
        let (_, st, sv) = keys(pw, &unhex(&salt), iter);
        let l = format!("{user}|{salt}|{iter}");
        let stored = hex(&x(&st, &mac(key, format!("stored|{l}").as_bytes())));
        let server = hex(&x(&sv, &mac(key, format!("server|{l}").as_bytes())));
        let tag = hex(&mac(key, format!("cred|{l}|{stored}|{server}").as_bytes()));
        serde_json::json!({"salt": salt, "iter": iter, "stored": stored, "server": server, "tag": tag})
    }
    /// hello → proof: (status of the failing step or 200, the client signed in, the server's proof checked).
    pub fn sign_in(user: &str, pw: &str) -> Result<super::Http, u16> {
        let mut h = super::Http::new();
        let cnonce =
            hex(&Sha256::digest(format!("{user}{pw}{:?}", std::time::Instant::now()))[..16]);
        let r = h.post(
            "/api/auth/hello",
            &serde_json::json!({"user": user, "cnonce": cnonce}),
        );
        if r.status != 200 {
            return Err(r.status);
        }
        let a = r.json();
        let (salt, iter, snonce) = (
            unhex(a["salt"].as_str().unwrap()),
            a["iter"].as_u64().unwrap() as u32,
            a["snonce"].as_str().unwrap().to_string(),
        );
        let (c, st, sv) = keys(pw, &salt, iter);
        let t = format!("broom-login|{user}|{cnonce}|{snonce}");
        let proof = hex(&x(&c, &mac(&st, t.as_bytes())));
        let r = h.post(
            "/api/auth/login",
            &serde_json::json!({"hs": a["hs"], "proof": proof}),
        );
        if r.status != 200 {
            return Err(r.status);
        }
        assert_eq!(
            r.json()["server_proof"],
            hex(&mac(&sv, t.as_bytes())),
            "the server proves it holds the account"
        );
        h.take_cookie(&r);
        h.sk = Some(mac(
            &c,
            [b"session|".as_slice(), t.as_bytes()].concat().as_slice(),
        ));
        Ok(h)
    }
}

/// The owner account `admin` made with the setup token from the log (it seals the verifier) → a signed-in client.
pub fn admin(home: &Home) -> Http {
    let log = home.log();
    let token = log
        .lines()
        .rev()
        .find_map(|l| l.split("setup token: ").nth(1))
        .unwrap_or_else(|| panic!("no setup token in the log:\n{log}"))
        .trim()
        .to_string();
    let r = Http::new().post(
        "/api/auth/setup",
        &serde_json::json!({"user": "admin", "cred": pw::seal(&pw::setup_key(&token), "admin", PASSWORD)}),
    );
    assert_eq!(r.status, 200, "setup: {}", r.text());
    login()
}

/// Sign in as admin with PASSWORD (set up earlier).
pub fn login() -> Http {
    pw::sign_in("admin", PASSWORD).unwrap_or_else(|s| panic!("login: {s}"))
}

/// DHCP server on srv0 for 10.99.0.0/24 (pool .100-.120), server IP = SRV.
pub fn configure_network(h: &Http) {
    let r = h.post(
        "/api/dhcp",
        &serde_json::json!({
            "mode": "full", "iface": SRV_IF, "server_ip": SRV, "subnet": "10.99.0.0",
            "range_start": "10.99.0.100", "range_end": "10.99.0.120", "netmask": "255.255.255.0",
            "gateway": SRV, "dns": SRV,
        }),
    );
    assert_eq!(r.status, 200, "network settings: {}", r.text());
}

/// A Linux golden: GPT disk, one ext4 partition at 1 MiB holding a kernel + initrd (mkfs.ext4 -d, no root needed).
pub struct Golden {
    pub disk: Vec<u8>,
    pub kernel: Vec<u8>,
    pub initrd: Vec<u8>,
    pub uuid: &'static str,
}

/// Bytes that differ everywhere (a misplaced read can't match by accident).
pub fn pattern(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32)
        .map(|i| (i.wrapping_add(seed).wrapping_mul(2654435761) >> 13) as u8)
        .collect()
}

pub fn linux_golden(dir: &Path) -> Golden {
    let uuid = "11111111-2222-3333-4444-555555555555";
    let (kernel, initrd) = (pattern(96 << 10, 1), pattern(48 << 10, 2));
    let root = dir.join("golden-root");
    std::fs::create_dir_all(root.join("boot")).unwrap();
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::write(
        root.join("etc/fstab"),
        format!("UUID={uuid} / ext4 defaults 0 1\n"),
    )
    .unwrap();
    std::fs::write(root.join("boot/vmlinuz-6.8.0-1-generic"), &kernel).unwrap();
    std::fs::write(root.join("boot/initrd.img-6.8.0-1-generic"), &initrd).unwrap();
    let fs = dir.join("golden-fs.img");
    let disk = dir.join("golden.img");
    sh(&format!(
        "mkfs.ext4 -q -F -U {uuid} -d {root} {fs} 16M >/dev/null
         truncate -s 20M {disk}
         echo 'label: gpt
start=2048, size=32768, type=L' | sfdisk -q --no-reread {disk}
         dd if={fs} of={disk} bs=1M seek=1 conv=notrunc status=none",
        root = root.display(),
        fs = fs.display(),
        disk = disk.display(),
    ));
    Golden {
        disk: std::fs::read(&disk).unwrap(),
        kernel,
        initrd,
        uuid,
    }
}

/// Create a Linux image, upload `g` as its golden (two chunks), wait for the publish. → image id.
pub fn publish_golden(h: &Http, name: &str, g: &Golden) -> i64 {
    let r = h.post(
        "/api/images",
        &serde_json::json!({"name": name, "os": "linux"}),
    );
    assert_eq!(r.status, 200, "create image: {}", r.text());
    let id = r.json()["id"].as_i64().unwrap();
    assert_eq!(
        h.post(
            "/api/images/upload-start",
            &serde_json::json!({"name": name})
        )
        .status,
        200
    );
    let total = g.disk.len();
    for (i, chunk) in g.disk.chunks(8 << 20).enumerate() {
        let path = format!(
            "/api/images/upload-chunk?name={name}&file=golden.img&offset={}&total={total}",
            i * (8 << 20)
        );
        let r = h.put(&path, chunk);
        assert_eq!(r.status, 200, "chunk {i}: {}", r.text());
    }
    let r = h.post(
        "/api/images/upload-done",
        &serde_json::json!({"name": name}),
    );
    assert_eq!(r.status, 200, "upload done: {}", r.text());
    let status = wait_job(h, name);
    assert!(status.starts_with('✓'), "publish {name}: {status}");
    id
}

/// Poll an image job until it ends → its final status ("✓ …" / "✗ …").
pub fn wait_job(h: &Http, name: &str) -> String {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(120) {
        let s = h.get(&format!("/api/images/job?name={name}")).json()["status"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if s.starts_with('✓') || s.starts_with('✗') {
            return s;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("job of {name} still running after 120 s");
}

/// The IQN in a published Linux boot script (`sanhook iscsi:${broom-srv}::::<iqn>`).
pub fn iqn_of(boot_script: &str) -> String {
    boot_script
        .split("::::")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or_else(|| panic!("no sanhook in {boot_script}"))
        .to_string()
}

/// PIDs of processes whose command line contains `needle` (this PID namespace only).
pub fn pids_of(needle: &str) -> Vec<i32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| {
                String::from_utf8_lossy(&c)
                    .replace('\0', " ")
                    .contains(needle)
            })
        })
        .collect()
}
