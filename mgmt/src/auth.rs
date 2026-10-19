// auth.rs — login for the web UI + API. The web admin controls DHCP, images and license keys and runs as root on a
// shared LAN, so every admin route needs a session. Client/boot routes stay open (a PXE client can't log in).
// Accounts (config `users`: a password verifier each, no password) with one of two roles: owner (everything) or staff
// (the day-to-day: look, Remote, Wake, save a games update — staff_may()). Signing in is a handshake that never sends
// the password (pwproof.rs: hello → salt + nonce → proof → the server's proof). The first owner is made with a
// one-time setup token printed to the server log (so only someone with access to the server can claim the install);
// it seals that account's verifier, it is never sent either. Sessions live in the database
// (table `sessions`): the cookie holds a random token, the table only its blake3 hash, so a copied database opens no
// session. Logout deletes the session; a password change or a deleted account ends all of that account's sessions; a
// role change applies at the next request; a restart logs nobody out. Every change (not a GET) is logged with who
// made it. Wrong proofs are throttled per client IP. Also a Host-header check
// (anti DNS-rebinding).
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
    /// The password's verifier (pwproof.rs).
    pw: crate::pwproof::Verifier,
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

/// The accounts. Unreadable = none (nobody signs in; edit_users refuses to overwrite it, so setup can't claim it).
pub(crate) fn users(st: &SharedState) -> Vec<User> {
    serde_json::from_str(&st.db.get_config("users", "[]")).unwrap_or_else(|e| {
        tracing::error!("users: stored value unreadable: {e}");
        Vec::new()
    })
}

/// The accounts changed as one step: two owners editing at once both land. An Err of `f` changes nothing; `f` must
/// not use the database.
pub(crate) fn edit_users<R>(
    st: &SharedState,
    f: impl FnOnce(&mut Vec<User>) -> Result<R, ApiError>,
) -> Result<R, ApiError> {
    crate::db::edit_json(&*st.db, "users", f).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
}

/// A user with a new password verifier (the caller ends its sessions: end_sessions).
pub(crate) fn with_password(mut u: User, v: crate::pwproof::Verifier) -> User {
    u.pw = v;
    u
}

pub(crate) fn new_user(name: &str, role: Role, v: crate::pwproof::Verifier) -> User {
    User {
        name: name.into(),
        role,
        pw: v,
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
        .route("/api/auth/hello", post(hello))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        // Not under /api/auth/ (public prefix): the guard requires a session for it.
        .route("/api/password", post(change_pw))
}

pub(crate) use crate::pwproof::hex;

/// 32 random bytes from the kernel (getrandom(2): waits only until the pool is first ready, at boot).
pub(crate) fn random32() -> [u8; 32] {
    let mut k = [0u8; 32];
    // SAFETY: k is a valid buffer of k.len() bytes.
    let n = unsafe { libc::getrandom(k.as_mut_ptr().cast(), k.len(), 0) };
    assert_eq!(n, 32, "getrandom: {}", std::io::Error::last_os_error());
    k
}

/// 128-bit random token, hex (the first-run setup token, agent challenges).
pub(crate) fn random_token() -> String {
    hex(&random32()[..16])
}

/// Constant-time string equality (blake3::Hash eq is constant-time).
pub(crate) fn same(a: &str, b: &str) -> bool {
    blake3::hash(a.as_bytes()) == blake3::hash(b.as_bytes())
}

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
    let (name, _) = st
        .db
        .session(&session_id(tok), now() as i64)
        .ok()
        .flatten()?;
    users(st).into_iter().find(|u| u.name == name).map(|u| Who {
        name: u.name,
        role: u.role,
    })
}

/// The session key of the request's session (what a new password for an account comes sealed with).
pub(crate) fn session_key(st: &SharedState, h: &axum::http::HeaderMap) -> Option<[u8; 32]> {
    let tok = session_token(h)?;
    let (_, key) = st
        .db
        .session(&session_id(&tok), now() as i64)
        .ok()
        .flatten()?;
    crate::pwproof::unhex32(&key)
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
            || path == "/crypto.js"
            || path == "/icons.svg"
            || path == "/app.css"
            || path == "/bootstrap.min.css")
}

/// Host header check against DNS rebinding (a page on some domain re-pointing that domain at this server). A rebinding
/// request always carries the attacker's domain name, so any IP literal is fine (the server IP, a second NIC, after
/// an IP change), plus localhost and this machine's own hostname (read once: a new hostname counts after a restart).
/// Port is ignored.
fn host_ok(req: &Request<Body>) -> bool {
    static OWN: LazyLock<String> = LazyLock::new(|| {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .unwrap_or_default()
            .trim()
            .to_string()
    });
    let Some(host) = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
    else {
        return true; // HTTP/1.0 / no Host: leave it, path guard still applies
    };
    host_allowed(host, &OWN)
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

fn session_cookie(token: &str, max_age: u64) -> [(header::HeaderName, String); 1] {
    [(
        header::SET_COOKIE,
        format!("{COOKIE}={token}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict"),
    )]
}

/// A new session for `u`: a random token for the cookie (its hash stored) + the handshake's session key `sk`.
fn new_session(st: &SharedState, u: &User, sk: &[u8; 32]) -> Result<String, ApiError> {
    let tok = hex(&random32());
    let t = now() as i64;
    st.db
        .put_session(
            &session_id(&tok),
            &u.name,
            &hex(sk),
            t + SESSION_SECS as i64,
            t,
        )
        .map_err(ise)?;
    Ok(tok)
}

#[derive(Deserialize)]
struct HelloBody {
    user: String,
    cnonce: String,
}

/// A handshake between its hello and its proof: the account, both nonces, until when, from where (hs_source).
struct Pending {
    user: String,
    cnonce: String,
    snonce: String,
    until: u64,
    from: IpAddr,
}

/// Open handshakes by id (RAM; at most HS_MAX, HS_PER_SOURCE per client, each one used once, HS_SECS to answer).
static HANDSHAKES: LazyLock<Mutex<HashMap<String, Pending>>> = LazyLock::new(Default::default);
const HS_SECS: u64 = 60;
const HS_MAX: usize = 4096;
const HS_PER_SOURCE: usize = 8;

/// Which client a handshake counts against: its IPv4 address, or its IPv6 /64 (one machine gets a whole /64 by
/// SLAAC). IPv4-mapped IPv6 is the IPv4 address, not one /64 for every IPv4 client.
fn hs_source(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & !0u128 << 64).into()),
        v4 => v4,
    }
}

/// Add an open handshake. A client at its HS_PER_SOURCE loses its own oldest one (its retries still work, it can't
/// fill the table); a full table loses the oldest of all instead of refusing everyone. Ceiling: a host faking many
/// LAN addresses can still push others' handshakes out — it then needs a per-port or switch-level answer.
fn admit(m: &mut HashMap<String, Pending>, hs: String, p: Pending) {
    let oldest = |m: &HashMap<String, Pending>, from: Option<IpAddr>| {
        m.iter()
            .filter(|(_, q)| from.is_none_or(|f| q.from == f))
            .min_by_key(|(_, q)| q.until)
            .map(|(k, _)| k.clone())
    };
    let gone = if m.values().filter(|q| q.from == p.from).count() >= HS_PER_SOURCE {
        oldest(m, Some(p.from))
    } else if m.len() >= HS_MAX {
        oldest(m, None)
    } else {
        None
    };
    if let Some(k) = gone {
        m.remove(&k);
    }
    m.insert(hs, p);
}

/// The salt shown for an account that does not exist: stable per name for this run, so the answer does not tell
/// which names exist.
fn fake_salt(user: &str) -> String {
    static K: LazyLock<[u8; 32]> = LazyLock::new(random32);
    hex(&blake3::keyed_hash(&K, user.as_bytes()).as_bytes()[..16])
}

/// POST /api/auth/hello {user, cnonce} (public) — handshake step 1 → 2: {hs, salt, iter, snonce} for that account.
async fn hello(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<SharedState>,
    Json(b): Json<HelloBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let wait = locked_for(&FAILS.lock(), peer.ip(), now());
    if wait > 0 {
        return Err(too_many(wait));
    }
    let all = users(&st);
    if all.is_empty() {
        return Err((StatusCode::CONFLICT, "not set up yet".into()));
    }
    let user = b.user.trim().to_lowercase();
    if !name_ok(&user)
        || !(16..=128).contains(&b.cnonce.len())
        || !b.cnonce.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(crate::api::bad("bad hello"));
    }
    let (salt, iter) = all.into_iter().find(|u| u.name == user).map_or_else(
        || (fake_salt(&user), crate::pwproof::ITER),
        |u| (u.pw.salt, u.pw.iter),
    );
    let (hs, snonce, t) = (random_token(), random_token(), now());
    let mut m = HANDSHAKES.lock();
    m.retain(|_, p| p.until > t);
    admit(
        &mut m,
        hs.clone(),
        Pending {
            user,
            cnonce: b.cnonce,
            snonce: snonce.clone(),
            until: t + HS_SECS,
            from: hs_source(peer.ip()),
        },
    );
    Ok(Json(
        serde_json::json!({"hs": hs, "salt": salt, "iter": iter, "snonce": snonce}),
    ))
}

#[derive(Deserialize)]
struct ProofBody {
    hs: String,
    proof: String,
}

/// Handshake step 3 → 4: the account whose password made `b.proof` (throttled per IP, the handshake used up),
/// with the server's proof and the session key. Err 401 on a wrong password / unknown account / stale handshake.
fn finish(
    st: &SharedState,
    ip: IpAddr,
    b: &ProofBody,
) -> Result<(User, String, [u8; 32]), ApiError> {
    let wait = locked_for(&FAILS.lock(), ip, now());
    if wait > 0 {
        return Err(too_many(wait));
    }
    let p = HANDSHAKES.lock().remove(&b.hs).filter(|p| p.until > now());
    let t = p
        .as_ref()
        .map(|p| crate::pwproof::transcript(&p.user, &p.cnonce, &p.snonce));
    let got = p.as_ref().zip(t.as_ref()).and_then(|(p, t)| {
        let u = users(st).into_iter().find(|u| u.name == p.user)?;
        let stored = crate::pwproof::unhex32(&u.pw.stored)?;
        let client = crate::pwproof::check(&stored, t, &crate::pwproof::unhex32(&b.proof)?)?;
        let server = crate::pwproof::unhex32(&u.pw.server)?;
        Some((
            u,
            hex(&crate::pwproof::mac(&server, t)),
            crate::pwproof::session_key(&client, t),
        ))
    });
    record(&mut FAILS.lock(), ip, got.is_some(), now());
    got.ok_or_else(|| {
        tracing::warn!(
            "failed login as {:?} from {ip}",
            p.map(|p| p.user).unwrap_or_default()
        );
        (
            StatusCode::UNAUTHORIZED,
            "wrong user or password".to_string(),
        )
    })
}

/// POST /api/auth/login {hs, proof} (public) — handshake step 3 → 4: {server_proof} + a session cookie.
async fn login(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(b): Json<ProofBody>,
) -> Result<impl IntoResponse, ApiError> {
    let (u, server_proof, sk) = finish(&st, peer.ip(), &b)?;
    tracing::info!("{} logged in from {}", u.name, peer.ip());
    Ok((
        session_cookie(&new_session(&st, &u, &sk)?, SESSION_SECS),
        Json(serde_json::json!({"server_proof": server_proof})),
    ))
}

#[derive(Deserialize)]
struct SetupBody {
    #[serde(default)]
    user: String,
    /// The owner's password verifier, sealed with the setup token (pwproof::setup_key).
    cred: crate::pwproof::Sealed,
}

/// POST /api/auth/setup {user, cred} — the FIRST owner account (none exists yet). Its verifier comes sealed with the
/// setup token printed to the server log at startup: neither the token nor the password crosses the network. Logs the
/// caller. The page then signs in with the new password.
async fn setup(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(b): Json<SetupBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !users(&st).is_empty() {
        return Err((StatusCode::CONFLICT, "already set up — log in".into()));
    }
    let name = Some(b.user.trim().to_lowercase())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "admin".into());
    if !name_ok(&name) {
        return Err(crate::api::bad("user name: a-z, 0-9, _ or -, at most 32"));
    }
    let Some(v) = SETUP_TOKEN
        .get()
        .and_then(|t| crate::pwproof::open(&crate::pwproof::setup_key(t), &name, &b.cred))
    else {
        tracing::warn!("setup with a wrong setup token from {}", peer.ip());
        return Err((
            StatusCode::FORBIDDEN,
            "wrong setup token — copy it from the server log".into(),
        ));
    };
    // Still no account (two setups at once: only the first one).
    edit_users(&st, |all| {
        if !all.is_empty() {
            return Err((StatusCode::CONFLICT, "already set up — log in".into()));
        }
        all.push(new_user(&name, Role::Owner, v));
        Ok(())
    })?;
    tracing::warn!("owner account {name} created from {}", peer.ip());
    Ok(crate::api::ok())
}

#[derive(Deserialize)]
struct NewPw {
    /// A fresh handshake with the CURRENT password (hello first)…
    hs: String,
    proof: String,
    /// …and the new password's verifier, sealed with that handshake's session key.
    cred: crate::pwproof::Sealed,
}

/// POST /api/password {hs, proof, cred} — change your own password. Your other sessions end; this browser gets a
/// fresh cookie (the handshake's session key) and stays in. → {server_proof}
async fn change_pw(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    axum::Extension(w): axum::Extension<Who>,
    Json(b): Json<NewPw>,
) -> Result<impl IntoResponse, ApiError> {
    let (was, server_proof, sk) = finish(
        &st,
        peer.ip(),
        &ProofBody {
            hs: b.hs,
            proof: b.proof,
        },
    )
    .map_err(|e| {
        // 403, not 401: the web app treats 401 as "session expired" and jumps to the login page.
        (
            StatusCode::FORBIDDEN,
            if e.0 == StatusCode::UNAUTHORIZED {
                "wrong current password".into()
            } else {
                e.1
            },
        )
    })?;
    if was.name != w.name {
        return Err((StatusCode::FORBIDDEN, "that is another account".into()));
    }
    let v = crate::pwproof::open(&sk, &w.name, &b.cred)
        .ok_or_else(|| crate::api::bad("new password: bad seal"))?;
    // As one step, and only over the password just checked (another change in between: try again).
    let u = edit_users(&st, |all| {
        let u = all
            .iter_mut()
            .find(|u| u.name == w.name)
            .ok_or((StatusCode::FORBIDDEN, "account gone".to_string()))?;
        if u.pw != was.pw {
            return Err((
                StatusCode::CONFLICT,
                "the password was changed meanwhile — try again".to_string(),
            ));
        }
        *u = with_password(u.clone(), v);
        Ok(u.clone())
    })?;
    end_sessions(&st, &u.name);
    tracing::warn!(
        "{}: password changed from {} — their other sessions logged out",
        w.name,
        peer.ip()
    );
    Ok((
        session_cookie(&new_session(&st, &u, &sk)?, SESSION_SECS),
        Json(serde_json::json!({"server_proof": server_proof})),
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
