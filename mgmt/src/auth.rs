// auth.rs — admin login for the web UI + API. The web admin controls DHCP, images and license keys and runs as
// root on a shared LAN, so every admin route needs a session. Client/boot routes stay open (a PXE client can't log
// in). One admin password (argon2 hash in config `admin_pw`), set on first use. Sessions are STATELESS signed
// cookies: `<expiry>.<blake3-keyed-MAC>` with a per-install secret in config `session_secret`. No server-side
// session map, so restarting the binary does not log admins out. Also a Host-header check (anti DNS-rebinding).
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
use std::net::SocketAddr;

use crate::SharedState;

const COOKIE: &str = "broom_session";
const SESSION_SECS: u64 = 8 * 3600;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/auth/status", get(status))
        .route("/api/auth/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// argon2id hash (PHC string) — stored in config `admin_pw`.
pub fn hash_pw(pw: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default().hash_password(pw.as_bytes(), &salt).map(|h| h.to_string()).map_err(|e| e.to_string())
}

fn verify_pw(pw: &str, phc: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|p| Argon2::default().verify_password(pw.as_bytes(), &p).is_ok())
}

fn cookie_token(req: &Request<Body>) -> Option<String> {
    let raw = req.headers().get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|c| c.trim().strip_prefix(&format!("{COOKIE}=")).map(str::to_string))
}

/// Per-install 32-byte secret that signs session cookies, stored hex in config `session_secret` (generated once).
/// ponytail: a first-request race can generate it twice; last write wins and only invalidates a cookie minted in
/// the same instant — negligible for a single-admin tool.
fn secret(st: &SharedState) -> [u8; 32] {
    let parse = |h: &str| -> Option<[u8; 32]> {
        let b: Vec<u8> = (0..h.len() / 2).map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()).collect::<Option<_>>()?;
        b.try_into().ok()
    };
    if let Some(k) = parse(&st.db.get_config("session_secret", "")) {
        return k;
    }
    let mut k = [0u8; 32];
    OsRng.fill_bytes(&mut k);
    let _ = st.db.set_config("session_secret", &k.iter().map(|x| format!("{x:02x}")).collect::<String>());
    k
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
        "/api/golden-chunk",
        "/api/drivers/for",
        "/api/license",
        "/api/license/result",
        "/broom-prep",
        "/broom-prep-win",
    ];
    if OPEN.contains(&path) || path.starts_with("/tftp/") || path.starts_with("/api/auth/") {
        return true;
    }
    // Only the standalone login page is public; the app shell, its tab fragments and all /api/* need a session.
    method == Method::GET && path == "/login"
}

/// Server names the request may use (anti DNS-rebinding): the DHCP server IP + localhost. Empty config → allow all
/// (not set up yet). Port is ignored.
fn host_ok(st: &SharedState, req: &Request<Body>) -> bool {
    let ip = st.db.get_config("dhcp_server_ip", "");
    if ip.is_empty() {
        return true;
    }
    let Some(host) = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return true; // HTTP/1.0 / no Host: leave it, path guard still applies
    };
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    host == ip || host == "localhost" || host == "127.0.0.1"
}

/// Test mode (BOOTROM_TEST=1): skip the whole guard so automated e2e scripts don't need a login. Never set on a
/// real server; a WARN is logged at startup (main.rs) when it is on.
pub fn test_mode() -> bool {
    static M: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *M.get_or_init(|| std::env::var("BOOTROM_TEST").is_ok())
}

/// Guard middleware on the whole router: Host check, then a session for everything that is not public.
pub async fn guard(State(st): State<SharedState>, req: Request<Body>, next: Next) -> Response {
    if test_mode() {
        return next.run(req).await;
    }
    if !host_ok(&st, &req) {
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

fn session_cookie(token: &str, max_age: u64) -> [(header::HeaderName, String); 1] {
    [(
        header::SET_COOKIE,
        format!("{COOKIE}={token}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict"),
    )]
}

fn new_session(st: &SharedState) -> String {
    sign(&secret(st), now() + SESSION_SECS)
}

/// POST /api/auth/setup — set the admin password the FIRST time only (no password stored yet). Logs the caller.
async fn setup(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(b): Json<Pw>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    if !st.db.get_config("admin_pw", "").is_empty() {
        return Err((StatusCode::CONFLICT, "admin password already set — log in".into()));
    }
    if b.password.len() < 8 {
        return Err((StatusCode::BAD_REQUEST, "password must be at least 8 characters".into()));
    }
    let hash = hash_pw(&b.password).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    st.db.set_config("admin_pw", &hash).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    tracing::warn!("admin password set from {}", peer.ip());
    let token = new_session(&st);
    Ok((session_cookie(&token, SESSION_SECS), Json(serde_json::json!({"ok": true}))))
}

/// POST /api/auth/login — check the password, start a session. A wrong try is logged (brute-force visibility).
async fn login(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(b): Json<Pw>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let stored = st.db.get_config("admin_pw", "");
    if stored.is_empty() {
        return Err((StatusCode::CONFLICT, "no admin password set yet".into()));
    }
    if !verify_pw(&b.password, &stored) {
        tracing::warn!("failed admin login from {}", peer.ip());
        return Err((StatusCode::UNAUTHORIZED, "wrong password".into()));
    }
    let token = new_session(&st);
    Ok((session_cookie(&token, SESSION_SECS), Json(serde_json::json!({"ok": true}))))
}

/// POST /api/auth/logout — clear the cookie. ponytail: stateless tokens can't be revoked server-side; for a
/// single-admin LAN tool, clearing the browser's cookie is enough (add a denylist only if that ever matters).
async fn logout() -> impl IntoResponse {
    (session_cookie("", 0), Json(serde_json::json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

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
        assert!(pub_get("/login"), "the standalone sign-in page is public");
        assert!(pub_get("/boot.ipxe") && pub_get("/tftp/broom/x/vmlinuz"));
        assert!(pub_post("/api/license") && pub_post("/api/auth/login") && pub_get("/api/golden-chunk"));
        // guarded now: the app shell + its fragments (login is a separate page)
        assert!(!pub_get("/") && !pub_get("/machines") && !pub_get("/ui/images"));
        assert!(!pub_get("/api/status") && !pub_get("/api/images") && !pub_post("/api/images/delete"));
        assert!(!pub_post("/api/dhcp") && !pub_post("/api/machines/import") && !pub_get("/api/events"));
    }
}
