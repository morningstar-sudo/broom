// auth.rs — admin login for the web UI + API. The web admin controls DHCP, images and license keys and runs as
// root on a shared LAN, so every admin route needs a session. Client/boot routes stay open (a PXE client can't log
// in). One admin password (argon2 hash in config `admin_pw`), set on first use with a one-time setup token that is
// printed to the server log (so only someone with access to the server can claim the install). Sessions are
// STATELESS signed cookies: `<expiry>.<blake3-keyed-MAC>` with a per-install secret in config `session_secret`. No
// server-side session map, so restarting the binary does not log admins out; changing the password rotates the
// secret, which logs every session out. Password checks are throttled per client IP and run off the async workers.
// Also a Host-header check (anti DNS-rebinding).
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{LazyLock, Mutex, OnceLock};

use crate::now_secs as now;
use crate::SharedState;

const COOKIE: &str = "broom_session";
const SESSION_SECS: u64 = 8 * 3600;

type ApiErr = (StatusCode, String);

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/auth/status", get(status))
        .route("/api/auth/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        // Not under /api/auth/ (public prefix): the guard requires a session for it.
        .route("/api/password", post(change_pw))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn random32() -> [u8; 32] {
    let mut k = [0u8; 32];
    OsRng.fill_bytes(&mut k);
    k
}

/// 128-bit random token, hex (setup token, one-time prep links).
pub(crate) fn random_token() -> String {
    hex(&random32()[..16])
}

/// Constant-time string equality (blake3::Hash eq is constant-time).
pub(crate) fn same(a: &str, b: &str) -> bool {
    blake3::hash(a.as_bytes()) == blake3::hash(b.as_bytes())
}

/// argon2id hash (PHC string) — stored in config `admin_pw`.
fn hash_pw(pw: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default().hash_password(pw.as_bytes(), &salt).map(|h| h.to_string()).map_err(|e| e.to_string())
}

fn verify_pw(pw: &str, phc: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|p| Argon2::default().verify_password(pw.as_bytes(), &p).is_ok())
}

/// argon2 costs ~tens of ms of CPU: at most 2 run at once, on blocking threads, so a login flood (even from many
/// IPs) can neither stall the async workers nor take over the blocking pool other handlers need.
static ARGON: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Failed password tries per client IP: (failures, locked until unix secs).
type Fails = HashMap<IpAddr, (u32, u64)>;
static FAILS: LazyLock<Mutex<Fails>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const FREE_TRIES: u32 = 5;
const MAX_LOCK_SECS: u64 = 300;

/// Seconds `ip` must still wait before its next password try (0 = may try now).
fn locked_for(f: &Fails, ip: IpAddr, now: u64) -> u64 {
    f.get(&ip).map_or(0, |&(_, until)| until.saturating_sub(now))
}

/// Record one password check. Success forgets the IP; from the 5th failure on, each failure locks it for 1, 2, 4 …
/// up to 300 s. Entries idle for 15 min are dropped, so the map stays small.
fn record(f: &mut Fails, ip: IpAddr, ok: bool, now: u64) {
    if ok {
        f.remove(&ip);
        return;
    }
    f.retain(|_, &mut (_, until)| until + 900 > now);
    let e = f.entry(ip).or_insert((0, now));
    e.0 += 1;
    e.1 = now + if e.0 >= FREE_TRIES { (1u64 << (e.0 - FREE_TRIES).min(9)).min(MAX_LOCK_SECS) } else { 0 };
}

fn too_many(wait: u64) -> ApiErr {
    (StatusCode::TOO_MANY_REQUESTS, format!("too many wrong passwords — retry in {wait}s"))
}

/// Throttled password check: 429 while `ip` is locked out, else argon2-verify on a blocking thread and record it.
async fn check_pw(ip: IpAddr, pw: String, phc: String) -> Result<bool, ApiErr> {
    let ise = |e: String| (StatusCode::INTERNAL_SERVER_ERROR, e);
    let wait = || locked_for(&FAILS.lock().unwrap(), ip, now());
    if wait() > 0 {
        return Err(too_many(wait()));
    }
    let _permit = ARGON.acquire().await.map_err(|e| ise(e.to_string()))?;
    if wait() > 0 {
        return Err(too_many(wait())); // locked while queued behind other checks
    }
    let ok = tokio::task::spawn_blocking(move || verify_pw(&pw, &phc)).await.map_err(|e| ise(e.to_string()))?;
    record(&mut FAILS.lock().unwrap(), ip, ok, now());
    Ok(ok)
}

/// hash_pw on a blocking thread (same limit as checks).
async fn hash_blocking(pw: String) -> Result<String, ApiErr> {
    let ise = |e: String| (StatusCode::INTERNAL_SERVER_ERROR, e);
    let _permit = ARGON.acquire().await.map_err(|e| ise(e.to_string()))?;
    tokio::task::spawn_blocking(move || hash_pw(&pw)).await.map_err(|e| ise(e.to_string()))?.map_err(ise)
}

/// One-time setup token, made at startup when no admin password exists and printed to the server log. Setting the
/// first password needs it, so a random LAN host that opens the web first cannot claim the install.
static SETUP_TOKEN: OnceLock<String> = OnceLock::new();

pub fn init_setup_token(st: &SharedState) {
    if !st.db.get_config("admin_pw", "").is_empty() {
        return;
    }
    let tok = random_token();
    let ip = st.db.get_config("dhcp_server_ip", "<server-ip>");
    tracing::info!("no admin password yet — open http://{ip}/login and enter the setup token: {tok}");
    let _ = SETUP_TOKEN.set(tok);
}

fn cookie_token(req: &Request<Body>) -> Option<String> {
    let raw = req.headers().get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|c| c.trim().strip_prefix(&format!("{COOKIE}=")).map(str::to_string))
}

/// New random session secret, stored → every cookie signed with the old one stops working.
fn rotate_secret(st: &SharedState) -> Result<[u8; 32], String> {
    let k = random32();
    st.db.set_config("session_secret", &hex(&k))?;
    Ok(k)
}

/// Per-install 32-byte secret that signs session cookies, stored hex in config `session_secret` (generated once).
/// A first-request race can generate it twice; last write wins and only invalidates a cookie minted in the same
/// instant — negligible for a single-admin tool.
fn secret(st: &SharedState) -> [u8; 32] {
    let parse = |h: &str| -> Option<[u8; 32]> {
        let b: Vec<u8> = (0..h.len() / 2).map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()).collect::<Option<_>>()?;
        b.try_into().ok()
    };
    if let Some(k) = parse(&st.db.get_config("session_secret", "")) {
        return k;
    }
    rotate_secret(st).unwrap_or_else(|e| {
        tracing::error!("session secret not saved ({e}) — logins won't stick until the database is writable");
        random32()
    })
}

/// Stateless session token: `<expiry-unix-secs>.<blake3 keyed MAC of the expiry>`.
fn sign(key: &[u8; 32], exp: u64) -> String {
    format!("{exp}.{}", blake3::keyed_hash(key, exp.to_string().as_bytes()).to_hex())
}

/// A valid, unexpired session cookie? Verifies the signature (constant-time via blake3::Hash eq) and expiry.
fn logged_in(st: &SharedState, req: &Request<Body>) -> bool {
    let Some(tok) = cookie_token(req) else { return false };
    let Some((exp_s, sig)) = tok.split_once('.') else { return false };
    let Ok(exp) = exp_s.parse::<u64>() else { return false };
    exp > now() && blake3::Hash::from_hex(sig).is_ok_and(|h| h == blake3::keyed_hash(&secret(st), exp_s.as_bytes()))
}

/// Paths reachable without a session: boot/client endpoints + the login API + the static shell (HTML only, no data).
fn is_public(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    const OPEN: &[&str] = &[
        "/boot.ipxe",
        "/boot/start",
        "/api/cache-list",
        "/api/drivers/for",
        "/api/license",
        "/api/license/result",
        "/api/booted",
        "/broom-prep",
        "/broom-prep-win",
    ];
    if OPEN.contains(&path) || path.starts_with("/tftp/") || path.starts_with("/api/auth/") {
        return true;
    }
    // Only the standalone login page (+ its script) is public; the app shell, its tab fragments and all /api/* need a session.
    method == Method::GET && (path == "/login" || path == "/login.js")
}

/// Host header check against DNS rebinding (a page on some domain re-pointing that domain at this server). A rebinding
/// request always carries the attacker's domain name, so any IP literal is fine (the server IP, a second NIC, after
/// an IP change), plus localhost and this machine's own hostname. Port is ignored.
fn host_ok(req: &Request<Body>) -> bool {
    let Some(host) = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return true; // HTTP/1.0 / no Host: leave it, path guard still applies
    };
    let own = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    host_allowed(host, own.trim())
}

fn host_allowed(host: &str, own: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    name.parse::<std::net::IpAddr>().is_ok()
        || name.eq_ignore_ascii_case("localhost")
        || (!own.is_empty() && (name.eq_ignore_ascii_case(own) || name.eq_ignore_ascii_case(&format!("{own}.local"))))
}

/// Guard middleware on the whole router: Host check, then a session for everything that is not public.
pub async fn guard(State(st): State<SharedState>, req: Request<Body>, next: Next) -> Response {
    if !host_ok(&req) {
        return (StatusCode::MISDIRECTED_REQUEST, "bad Host").into_response();
    }
    if is_public(req.method(), req.uri().path()) || logged_in(&st, &req) {
        return next.run(req).await;
    }
    // A browser navigating to a page → send it to the login page. An API/XHR/SSE call → 401 (the app's JS then
    // redirects to /login itself). Distinguish by the Accept header a top-level navigation sends.
    let wants_html = req.method() == axum::http::Method::GET
        && req.headers().get(header::ACCEPT).and_then(|a| a.to_str().ok()).is_some_and(|a| a.contains("text/html"));
    if wants_html {
        axum::response::Redirect::to("/login").into_response()
    } else {
        (StatusCode::UNAUTHORIZED, "login required").into_response()
    }
}

/// Security headers on every response. CSP: scripts only from our own files (no inline JS, no handler attributes —
/// app.js dispatches data-click etc.), so a value that slips past escaping can't run in the admin's session. Inline
/// styles stay allowed (style="" attributes all over the pages; CSS can't run script).
pub async fn security_headers(req: Request<Body>, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, header::HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, header::HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, header::HeaderValue::from_static("DENY"));
    res
}

async fn status(State(st): State<SharedState>, req: Request<Body>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "configured": !st.db.get_config("admin_pw", "").is_empty(),
        "authed": logged_in(&st, &req),
    }))
}

#[derive(Deserialize)]
struct Pw {
    password: String,
}

#[derive(Deserialize)]
struct SetupBody {
    password: String,
    #[serde(default)]
    token: String,
}

#[derive(Deserialize)]
struct NewPw {
    current: String,
    new: String,
}

fn session_cookie(token: &str, max_age: u64) -> [(header::HeaderName, String); 1] {
    [(
        header::SET_COOKIE,
        format!("{COOKIE}={token}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict"),
    )]
}

fn new_session(st: &SharedState) -> String {
    sign(&secret(st), now() + SESSION_SECS)
}

/// POST /api/auth/setup {password, token} — set the admin password the FIRST time only (no password stored yet),
/// with the setup token printed to the server log at startup. Logs the caller.
async fn setup(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(b): Json<SetupBody>,
) -> Result<impl IntoResponse, ApiErr> {
    if !st.db.get_config("admin_pw", "").is_empty() {
        return Err((StatusCode::CONFLICT, "admin password already set — log in".into()));
    }
    if !SETUP_TOKEN.get().is_some_and(|t| same(t, b.token.trim())) {
        tracing::warn!("admin setup with a wrong setup token from {}", peer.ip());
        return Err((StatusCode::FORBIDDEN, "wrong setup token — copy it from the server log".into()));
    }
    if b.password.len() < 8 {
        return Err((StatusCode::BAD_REQUEST, "password must be at least 8 characters".into()));
    }
    let hash = hash_blocking(b.password).await?;
    st.db.set_config("admin_pw", &hash).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    tracing::warn!("admin password set from {}", peer.ip());
    let token = new_session(&st);
    Ok((session_cookie(&token, SESSION_SECS), Json(serde_json::json!({"ok": true}))))
}

/// POST /api/auth/login — check the password (throttled per IP), start a session. A wrong try is logged.
async fn login(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(b): Json<Pw>,
) -> Result<impl IntoResponse, ApiErr> {
    let stored = st.db.get_config("admin_pw", "");
    if stored.is_empty() {
        return Err((StatusCode::CONFLICT, "no admin password set yet".into()));
    }
    if !check_pw(peer.ip(), b.password, stored).await? {
        tracing::warn!("failed admin login from {}", peer.ip());
        return Err((StatusCode::UNAUTHORIZED, "wrong password".into()));
    }
    let token = new_session(&st);
    Ok((session_cookie(&token, SESSION_SECS), Json(serde_json::json!({"ok": true}))))
}

/// POST /api/password {current, new} — change the admin password (session required). Rotates the session secret,
/// so every other signed-in browser is logged out; the caller gets a fresh cookie and stays in.
async fn change_pw(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(b): Json<NewPw>,
) -> Result<impl IntoResponse, ApiErr> {
    let ise = |e: String| (StatusCode::INTERNAL_SERVER_ERROR, e);
    if b.new.len() < 8 {
        return Err((StatusCode::BAD_REQUEST, "new password must be at least 8 characters".into()));
    }
    if !check_pw(peer.ip(), b.current, st.db.get_config("admin_pw", "")).await? {
        tracing::warn!("password change with a wrong current password from {}", peer.ip());
        // 403, not 401: the web app treats 401 as "session expired" and jumps to the login page.
        return Err((StatusCode::FORBIDDEN, "wrong current password".into()));
    }
    let hash = hash_blocking(b.new).await?;
    st.db.set_config("admin_pw", &hash).map_err(ise)?;
    let key = rotate_secret(&st).map_err(ise)?;
    tracing::warn!("admin password changed from {} — all other sessions logged out", peer.ip());
    Ok((session_cookie(&sign(&key, now() + SESSION_SECS), SESSION_SECS), Json(serde_json::json!({"ok": true}))))
}

/// POST /api/auth/logout — clear the cookie. Stateless tokens can't be revoked one by one; changing the password
/// (rotates the signing secret) revokes them all.
async fn logout() -> impl IntoResponse {
    (session_cookie("", 0), Json(serde_json::json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn host_header() {
        for ok in ["10.0.0.12", "10.0.0.12:80", "192.168.1.5", "[::1]:80", "[fe80::1]", "localhost:8080", "broom", "BROOM.local"] {
            assert!(host_allowed(ok, "broom"), "{ok}");
        }
        for bad in ["evil.example.com", "evil.example.com:80", "10.0.0.12.nip.io", "broomx"] {
            assert!(!host_allowed(bad, "broom"), "{bad}");
        }
        assert!(!host_allowed("anything", ""), "no hostname known → only IPs / localhost");
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
    fn stateless_token_roundtrip() {
        let key = [7u8; 32];
        let tok = sign(&key, 1_800_000_000);
        let (exp_s, sig) = tok.split_once('.').unwrap();
        assert_eq!(exp_s, "1800000000");
        let sig = blake3::Hash::from_hex(sig).unwrap();
        assert!(sig == blake3::keyed_hash(&key, exp_s.as_bytes()), "valid signature verifies");
        assert!(sig != blake3::keyed_hash(&[8u8; 32], exp_s.as_bytes()), "wrong key rejected");
        assert!(sig != blake3::keyed_hash(&key, b"1900000000"), "tampered expiry rejected");
    }

    #[test]
    fn public_paths() {
        let pub_get = |p| is_public(&Method::GET, p);
        let pub_post = |p| is_public(&Method::POST, p);
        assert!(pub_get("/login") && pub_get("/login.js"), "the standalone sign-in page + its script are public");
        assert!(pub_get("/boot.ipxe") && pub_get("/tftp/broom/x/vmlinuz"));
        assert!(pub_post("/api/license") && pub_post("/api/auth/login") && pub_post("/api/booted"));
        assert!(!pub_get("/api/golden-chunk"), "delta chunks are gone");
        assert!(pub_get("/broom-prep-win") && !pub_post("/api/images/base-mode"), "prep script open, admin APIs not");
        // guarded now: the app shell + its fragments (login is a separate page)
        assert!(!pub_get("/") && !pub_get("/machines") && !pub_get("/ui/images"));
        assert!(!pub_get("/api/status") && !pub_get("/api/images") && !pub_post("/api/images/delete"));
        assert!(!pub_post("/api/dhcp") && !pub_post("/api/machines/import") && !pub_get("/api/events"));
        assert!(!pub_post("/api/password"), "changing the password needs a session");
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
}
