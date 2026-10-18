// auth.rs — login for the web UI + API. The web admin controls DHCP, images and license keys and runs as root on a
// shared LAN, so every admin route needs a session. Client/boot routes stay open (a PXE client can't log in).
// Accounts (config `users`, argon2 hashes) with one of two roles: owner (everything) or staff (the day-to-day: look,
// Remote, Wake, save a games update — staff_may()). The first owner is made with a one-time setup token printed to the
// server log (so only someone with access to the server can claim the install). Sessions live in the database
// (table `sessions`): the cookie holds a random token, the table only its blake3 hash, so a copied database opens no
// session. Logout deletes the session; a password change or a deleted account ends all of that account's sessions; a
// role change applies at the next request; a restart logs nobody out. Every change (not a GET) is logged with who
// made it. Password checks are throttled per client IP and run off the async workers. Also a Host-header check
// (anti DNS-rebinding).
use argon2::Argon2;
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{LazyLock, OnceLock};

use crate::SharedState;
use crate::api::{ApiError, ise};
use crate::now_secs as now;

const COOKIE: &str = "broom_session";
const SESSION_SECS: u64 = 8 * 3600;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Staff,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct User {
    pub name: String,
    pub role: Role,
    /// argon2id PHC string.
    pw: String,
}

/// Who made a request (the guard puts it in the request's extensions).
#[derive(Clone, Debug)]
pub struct Who {
    pub name: String,
    pub role: Role,
}

/// Account name: letters, digits, `_`, `-` (lower case), at most 32.
pub(crate) fn name_ok(n: &str) -> bool {
    (1..=32).contains(&n.len())
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The accounts.
pub(crate) fn users(st: &SharedState) -> Vec<User> {
    serde_json::from_str(&st.db.get_config("users", "[]")).unwrap_or_default()
}

/// The accounts changed as one step: two owners editing at once both land. An Err of `f` changes nothing; `f` must
/// not use the database.
pub(crate) fn edit_users<R>(
    st: &SharedState,
    f: impl FnOnce(&mut Vec<User>) -> Result<R, ApiError>,
) -> Result<R, ApiError> {
    crate::db::edit_json(&*st.db, "users", f).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
}

/// A user with a new password hash (the caller ends its sessions: end_sessions).
pub(crate) fn with_password(mut u: User, phc: String) -> User {
    u.pw = phc;
    u
}

pub(crate) fn new_user(name: &str, role: Role, phc: String) -> User {
    User {
        name: name.into(),
        role,
        pw: phc,
    }
}

/// What staff may do: read every page (but not the CSV with the license keys, a golden's export or the accounts) and,
/// of the changes, only Wake, Save / Discard a games update, their own password and logging out. Remote is a GET
/// (WebSocket).
pub(crate) fn staff_may(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    const OWNER_GET: &[&str] = &[
        "/api/machines/export.csv",
        "/api/images/export-file",
        "/api/images/deploy-map",
        "/api/images/deploy-chunk",
        "/api/users",
    ];
    const STAFF_POST: &[&str] = &[
        "/api/wake",
        "/api/games/save",
        "/api/games/discard",
        "/api/password",
        "/api/auth/logout",
    ];
    match *method {
        Method::GET | Method::HEAD => !OWNER_GET.contains(&path),
        Method::POST => STAFF_POST.contains(&path),
        _ => false,
    }
}

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/auth/status", get(status))
        .route("/api/auth/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        // Not under /api/auth/ (public prefix): the guard requires a session for it.
        .route("/api/password", post(change_pw))
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn random32() -> [u8; 32] {
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

/// argon2id hash (PHC string) of a password — what the accounts keep (config `users`).
fn hash_pw(pw: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

fn verify_pw(pw: &str, phc: &str) -> bool {
    PasswordHash::new(phc)
        .is_ok_and(|p| Argon2::default().verify_password(pw.as_bytes(), &p).is_ok())
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
    f.get(&ip)
        .map_or(0, |&(_, until)| until.saturating_sub(now))
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
    e.1 = now
        + if e.0 >= FREE_TRIES {
            (1u64 << (e.0 - FREE_TRIES).min(9)).min(MAX_LOCK_SECS)
        } else {
            0
        };
}

fn too_many(wait: u64) -> ApiError {
    (
        StatusCode::TOO_MANY_REQUESTS,
        format!("too many wrong passwords — retry in {wait}s"),
    )
}

/// Throttled password check: 429 while `ip` is locked out, else argon2-verify on a blocking thread and record it.
async fn check_pw(ip: IpAddr, pw: String, phc: String) -> Result<bool, ApiError> {
    let wait = || locked_for(&FAILS.lock(), ip, now());
    if wait() > 0 {
        return Err(too_many(wait()));
    }
    let _permit = ARGON.acquire().await.map_err(ise)?;
    if wait() > 0 {
        return Err(too_many(wait())); // locked while queued behind other checks
    }
    let ok = tokio::task::spawn_blocking(move || verify_pw(&pw, &phc))
        .await
        .map_err(ise)?;
    record(&mut FAILS.lock(), ip, ok, now());
    Ok(ok)
}

/// hash_pw on a blocking thread (same limit as checks).
pub(crate) async fn hash_blocking(pw: String) -> Result<String, ApiError> {
    let _permit = ARGON.acquire().await.map_err(ise)?;
    tokio::task::spawn_blocking(move || hash_pw(&pw))
        .await
        .map_err(ise)?
        .map_err(ise)
}

/// One-time setup token, made at startup when no admin password exists and printed to the server log. Setting the
/// first password needs it, so a random LAN host that opens the web first cannot claim the install.
static SETUP_TOKEN: OnceLock<String> = OnceLock::new();

pub fn init_setup_token(st: &SharedState) {
    if !users(st).is_empty() {
        return;
    }
    let tok = random_token();
    let ip = st.db.get_config("dhcp_server_ip", "<server-ip>");
    tracing::info!(
        "no admin password yet — open http://{ip}/login and enter the setup token: {tok}"
    );
    let _ = SETUP_TOKEN.set(tok);
}

/// The session cookie's token in these request headers.
pub(crate) fn session_token(h: &axum::http::HeaderMap) -> Option<String> {
    let raw = h.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|c| {
        c.trim()
            .strip_prefix(&format!("{COOKIE}="))
            .map(str::to_string)
    })
}

/// The sessions table's key for a cookie token: its blake3 hash (a copied database holds no usable token).
fn session_id(tok: &str) -> String {
    blake3::hash(tok.as_bytes()).to_hex().to_string()
}

/// Who holds this session token (None = no such session, expired, or its account is gone).
pub(crate) fn who(st: &SharedState, tok: &str) -> Option<Who> {
    let name = st
        .db
        .session_user(&session_id(tok), now() as i64)
        .ok()
        .flatten()?;
    users(st).into_iter().find(|u| u.name == name).map(|u| Who {
        name: u.name,
        role: u.role,
    })
}

/// End every session of an account (password changed, account deleted).
pub(crate) fn end_sessions(st: &SharedState, user: &str) {
    if let Err(e) = st.db.delete_sessions(user) {
        tracing::error!("sessions of {user} not ended: {e}");
    }
}

/// How often an open stream (SSE, Remote WebSocket) re-checks its session — the guard only checked when it opened.
pub(crate) const RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

/// Resolves once the session `tok` is no longer valid (checked every RECHECK): ends an SSE stream / WebSocket.
pub(crate) async fn until_invalid(st: SharedState, tok: Option<String>) {
    loop {
        tokio::time::sleep(RECHECK).await;
        if !tok.as_deref().is_some_and(|t| who(&st, t).is_some()) {
            return;
        }
    }
}

/// Paths reachable without a session: boot/client endpoints + the login API + the static shell (HTML only, no data).
fn is_public(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    const OPEN: &[&str] = &[
        "/boot.ipxe",
        "/boot/start",
        "/boot/capture",
        "/broom-capture",
        "/api/cache-list",
        "/api/drivers/for",
        "/api/disks/report",
        "/api/license",
        "/api/license/result",
        "/api/booted",
        "/api/games/for",
        "/api/ssd-low",
        "/api/client-config",
        "/api/golden-slot",
        "/api/vnc/agent",
        "/api/agent-key",
        "/api/bench/pending",
        "/api/bench/blob",
        "/api/bench/report",
        "/api/bench/booted",
        "/broom-prep",
        "/broom-prep-win",
    ];
    if OPEN.contains(&path) || path.starts_with("/tftp/") || path.starts_with("/api/auth/") {
        return true;
    }
    // Only the standalone login page (+ its script and stylesheets) is public; the app shell, its tab fragments and all /api/* need a session.
    method == Method::GET
        && (path == "/login"
            || path == "/login.js"
            || path == "/icons.svg"
            || path == "/app.css"
            || path == "/bootstrap.min.css")
}

/// Host header check against DNS rebinding (a page on some domain re-pointing that domain at this server). A rebinding
/// request always carries the attacker's domain name, so any IP literal is fine (the server IP, a second NIC, after
/// an IP change), plus localhost and this machine's own hostname. Port is ignored.
fn host_ok(req: &Request<Body>) -> bool {
    let Some(host) = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
    else {
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
        || (!own.is_empty()
            && (name.eq_ignore_ascii_case(own)
                || name.eq_ignore_ascii_case(&format!("{own}.local"))))
}

/// Guard middleware on the whole router: Host check, then a session for everything that is not public, then the
/// role's rights (staff_may). Who it is goes into the request (handlers that show secrets ask), and every change is
/// logged with their name (Logs page).
pub async fn guard(State(st): State<SharedState>, mut req: Request<Body>, next: Next) -> Response {
    if !host_ok(&req) {
        return (StatusCode::MISDIRECTED_REQUEST, "bad Host").into_response();
    }
    let path = req.uri().path().to_string();
    if is_public(req.method(), &path) {
        return next.run(req).await;
    }
    if let Some(w) = session_token(req.headers()).and_then(|t| who(&st, &t)) {
        let m = req.method().clone();
        if w.role == Role::Staff && !staff_may(&m, &path) {
            return (
                StatusCode::FORBIDDEN,
                "owner only — ask the owner (staff can look, Remote, Wake and save a games update)",
            )
                .into_response();
        }
        // A golden upload is many PUTs of one chunk: its start / done are logged instead.
        if m != axum::http::Method::GET
            && m != axum::http::Method::HEAD
            && path != "/api/images/upload-chunk"
        {
            tracing::info!("{}: {m} {path}", w.name);
        }
        req.extensions_mut().insert(w);
        return next.run(req).await;
    }
    // A browser navigating to a page → send it to the login page. An API/XHR/SSE call → 401 (the app's JS then
    // redirects to /login itself). Distinguish by the Accept header a top-level navigation sends.
    let wants_html = req.method() == axum::http::Method::GET
        && req
            .headers()
            .get(header::ACCEPT)
            .and_then(|a| a.to_str().ok())
            .is_some_and(|a| a.contains("text/html"));
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
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );
    res
}

/// GET /api/auth/status (public) — is there an account yet, is this browser signed in, as whom.
async fn status(State(st): State<SharedState>, req: Request<Body>) -> Json<serde_json::Value> {
    let w = session_token(req.headers()).and_then(|t| who(&st, &t));
    Json(serde_json::json!({
        "configured": !users(&st).is_empty(),
        "authed": w.is_some(),
        "user": w.as_ref().map(|w| w.name.clone()),
        "role": w.map(|w| w.role),
    }))
}

#[derive(Deserialize)]
struct LoginBody {
    /// Older login pages send no name: the first owner's account.
    #[serde(default)]
    user: String,
    password: String,
}

#[derive(Deserialize)]
struct SetupBody {
    #[serde(default)]
    user: String,
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

/// A new session for `u`: a random token for the cookie, its hash stored.
fn new_session(st: &SharedState, u: &User) -> Result<String, ApiError> {
    let tok = hex(&random32());
    let t = now() as i64;
    st.db
        .put_session(&session_id(&tok), &u.name, t + SESSION_SECS as i64, t)
        .map_err(ise)?;
    Ok(tok)
}

/// A hash to check a password against when the account does not exist: as slow as a real one, so the answer time
/// does not tell which names exist.
fn dummy_hash() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| hash_pw("not-an-account").unwrap_or_default())
}

/// POST /api/auth/setup {user, password, token} — the FIRST owner account (none exists yet), with the setup token
/// printed to the server log at startup. Logs the caller.
async fn setup(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(b): Json<SetupBody>,
) -> Result<impl IntoResponse, ApiError> {
    if !users(&st).is_empty() {
        return Err((StatusCode::CONFLICT, "already set up — log in".into()));
    }
    if !SETUP_TOKEN.get().is_some_and(|t| same(t, b.token.trim())) {
        tracing::warn!("setup with a wrong setup token from {}", peer.ip());
        return Err((
            StatusCode::FORBIDDEN,
            "wrong setup token — copy it from the server log".into(),
        ));
    }
    let name = Some(b.user.trim().to_lowercase())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "admin".into());
    if !name_ok(&name) {
        return Err(crate::api::bad("user name: a-z, 0-9, _ or -, at most 32"));
    }
    if b.password.len() < 8 {
        return Err(crate::api::bad("password must be at least 8 characters"));
    }
    let u = new_user(&name, Role::Owner, hash_blocking(b.password).await?);
    // Still no account (two setups at once: only the first one).
    edit_users(&st, |v| {
        if !v.is_empty() {
            return Err((StatusCode::CONFLICT, "already set up — log in".into()));
        }
        v.push(u.clone());
        Ok(())
    })?;
    tracing::warn!("owner account {name} created from {}", peer.ip());
    Ok((
        session_cookie(&new_session(&st, &u)?, SESSION_SECS),
        crate::api::ok(),
    ))
}

/// POST /api/auth/login {user, password} — check the password (throttled per IP), start a session. A wrong try is
/// logged; the answer does not say whether the name exists.
async fn login(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(b): Json<LoginBody>,
) -> Result<impl IntoResponse, ApiError> {
    let all = users(&st);
    if all.is_empty() {
        return Err((StatusCode::CONFLICT, "not set up yet".into()));
    }
    let name = b.user.trim().to_lowercase();
    let u = if name.is_empty() {
        all.iter().find(|u| u.role == Role::Owner)
    } else {
        all.iter().find(|u| u.name == name)
    };
    let phc = u.map_or_else(|| dummy_hash().to_string(), |u| u.pw.clone());
    if !check_pw(peer.ip(), b.password, phc).await? || u.is_none() {
        tracing::warn!("failed login as {name:?} from {}", peer.ip());
        return Err((StatusCode::UNAUTHORIZED, "wrong user or password".into()));
    }
    let u = u.unwrap();
    tracing::info!("{} logged in from {}", u.name, peer.ip());
    Ok((
        session_cookie(&new_session(&st, u)?, SESSION_SECS),
        crate::api::ok(),
    ))
}

/// POST /api/password {current, new} — change your own password. Your other sessions end; this browser gets a
/// fresh cookie and stays in.
async fn change_pw(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    axum::Extension(w): axum::Extension<Who>,
    Json(b): Json<NewPw>,
) -> Result<impl IntoResponse, ApiError> {
    if b.new.len() < 8 {
        return Err(crate::api::bad(
            "new password must be at least 8 characters",
        ));
    }
    let Some(was) = users(&st).into_iter().find(|u| u.name == w.name) else {
        return Err((StatusCode::FORBIDDEN, "account gone".into()));
    };
    if !check_pw(peer.ip(), b.current, was.pw.clone()).await? {
        tracing::warn!(
            "{}: password change with a wrong current password from {}",
            w.name,
            peer.ip()
        );
        // 403, not 401: the web app treats 401 as "session expired" and jumps to the login page.
        return Err((StatusCode::FORBIDDEN, "wrong current password".into()));
    }
    let phc = hash_blocking(b.new).await?;
    // As one step, and only over the password just checked (another change in between: try again).
    let u = edit_users(&st, |v| {
        let u = v
            .iter_mut()
            .find(|u| u.name == w.name)
            .ok_or((StatusCode::FORBIDDEN, "account gone".to_string()))?;
        if u.pw != was.pw {
            return Err((
                StatusCode::CONFLICT,
                "the password was changed meanwhile — try again".to_string(),
            ));
        }
        *u = with_password(u.clone(), phc);
        Ok(u.clone())
    })?;
    end_sessions(&st, &u.name);
    tracing::warn!(
        "{}: password changed from {} — their other sessions logged out",
        w.name,
        peer.ip()
    );
    Ok((
        session_cookie(&new_session(&st, &u)?, SESSION_SECS),
        crate::api::ok(),
    ))
}

/// POST /api/auth/logout — end this session (deleted on the server, a copy of the cookie is dead too) + clear the
/// cookie.
async fn logout(State(st): State<SharedState>, req: Request<Body>) -> impl IntoResponse {
    if let Some(t) = session_token(req.headers())
        && let Err(e) = st.db.delete_session(&session_id(&t))
    {
        tracing::error!("logout: session not deleted: {e}");
    }
    (session_cookie("", 0), crate::api::ok())
}

#[cfg(test)]
#[path = "../tests/unit/auth.rs"]
mod tests;
