// dns.rs — the clients' DNS server (Network page, off by default). With it on, the built-in DHCP hands out this server
// as the only DNS, and a name is resolved — forwarded to the resolvers listed on the DNS card (else the DHCP card's
// list, else the gateway), or recursively from the root servers with a cache (dnsrec.rs) — only for a machine that is
// healthy: its Remote agent connected (it proved its key, agentauth.rs), or it PXE-booted
// a few minutes ago (Windows and the launchers want the network before the agent is up), or it is exempt (a group or an
// IP / range: the cashier, staff PCs, Linux machines…). Anything else gets REFUSED: a machine whose agent was killed,
// one that started Windows from its SSD without the stage, a laptop plugged into the LAN.
// A soft lock: a guest (an administrator on their machine) can set another DNS server or DNS over HTTPS — the router
// should block DNS to the outside (ports 53 / 853) for everything but this server. Firefox's own DNS over HTTPS is
// turned off for every client the standard way (its canary domain answers NXDOMAIN).
// This server down = no name resolves for the clients.
use axum::{Json, Router, extract::State, routing::get};
use parking_lot::Mutex;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::SharedState;
use crate::api::{ApiError, bad, ise};

pub fn routes() -> Router<SharedState> {
    Router::new().route("/api/dns", get(status).post(set))
}

/// What may resolve, recomputed every REFRESH (a query only looks it up).
#[derive(Default)]
struct Allowed {
    ips: HashSet<Ipv4Addr>,
    nets: Vec<(Ipv4Addr, u8)>,
}

static ALLOWED: LazyLock<Mutex<Allowed>> = LazyLock::new(Default::default);
/// The DNS server is running (Machines page: who is blocked; Services card).
static ACTIVE: AtomicBool = AtomicBool::new(false);
static HANDLES: Mutex<Vec<tokio::task::JoinHandle<()>>> = Mutex::new(Vec::new());
/// Last time a refusal was logged, per client (once every 10 min).
static LOGGED: LazyLock<Mutex<HashMap<Ipv4Addr, Instant>>> = LazyLock::new(Default::default);

const REFRESH: Duration = Duration::from_secs(5);
const UPSTREAM_WAIT: Duration = Duration::from_secs(2);
/// Firefox turns its default DNS over HTTPS off when this name does not resolve.
const FIREFOX_CANARY: &str = "use-application-dns.net";

pub fn on(st: &SharedState) -> bool {
    st.db.get_config("dns_mode", "off") == "on"
}

/// How names are resolved: "forward" (passed to the resolvers of `upstream`) or "recursive" (from the root servers,
/// dnsrec.rs).
pub fn recursive(st: &SharedState) -> bool {
    st.db.get_config("dns_resolve", "forward") == "recursive"
}

fn ip_list(s: &str) -> Vec<Ipv4Addr> {
    s.split([',', ' ', '\n'])
        .filter_map(|s| s.trim().parse().ok())
        .collect()
}

/// The resolvers queries are passed to in forward mode: the DNS card's list, else the DHCP card's DNS list (what
/// the clients had before), else the gateway.
pub fn upstream(st: &SharedState) -> Vec<Ipv4Addr> {
    for k in ["dns_upstream", "dhcp_dns"] {
        let v = ip_list(&st.db.get_config(k, ""));
        if !v.is_empty() {
            return v;
        }
    }
    st.db
        .get_config("dhcp_gateway", "")
        .trim()
        .parse()
        .ok()
        .into_iter()
        .collect()
}

/// IPs / ranges, one per line (or comma separated): `10.0.0.5`, `10.0.0.0/24`.
pub(crate) fn parse_nets(text: &str) -> Result<Vec<(Ipv4Addr, u8)>, String> {
    text.split(['\n', ','])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (ip, len) = l.split_once('/').unwrap_or((l, "32"));
            match (ip.trim().parse::<Ipv4Addr>(), len.trim().parse::<u8>()) {
                (Ok(ip), Ok(n)) if n <= 32 => Ok((ip, n)),
                _ => Err(format!("{l:?} is not an IP or IP/prefix")),
            }
        })
        .collect()
}

pub(crate) fn cidr_contains(net: Ipv4Addr, len: u8, ip: Ipv4Addr) -> bool {
    let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
    u32::from(net) & mask == u32::from(ip) & mask
}

pub fn exempt_groups(st: &SharedState) -> Vec<String> {
    serde_json::from_str(&st.db.get_config("dns_exempt_groups", "[]")).unwrap_or_default()
}

pub fn set_exempt_groups(st: &SharedState, g: &[String]) -> Result<(), String> {
    st.db
        .set_config("dns_exempt_groups", &serde_json::to_string(g).unwrap())
}

fn grace_min(st: &SharedState) -> u64 {
    st.db.get_config("dns_grace_min", "5").parse().unwrap_or(5)
}

/// One machine (or a MAC seen PXE-booting): may it resolve?
pub(crate) struct Client {
    pub ips: Vec<Ipv4Addr>,
    pub agent: bool,
    pub exempt_group: bool,
    /// Unix time of its last PXE boot.
    pub pxe: Option<u64>,
}

/// The IPs that may resolve now.
pub(crate) fn allowed_ips(clients: &[Client], grace_s: u64, now: u64) -> HashSet<Ipv4Addr> {
    clients
        .iter()
        .filter(|c| {
            c.agent || c.exempt_group || c.pxe.is_some_and(|t| now.saturating_sub(t) <= grace_s)
        })
        .flat_map(|c| c.ips.iter().copied())
        .collect()
}

fn compute(st: &SharedState) -> Allowed {
    let now = crate::now_secs();
    let groups = exempt_groups(st);
    let leases = st.db.leases().unwrap_or_default();
    let pxe_seen = st.pxe_seen.lock().clone();
    let pxe_ip = st.pxe_ip.lock().clone();
    let ip4 = |s: &str| s.parse::<Ipv4Addr>().ok();
    let mut clients: Vec<Client> = Vec::new();
    let mut known = HashSet::new();
    for m in st.db.machines().unwrap_or_default() {
        let mac = m.mac.to_lowercase();
        let mut ips: Vec<Ipv4Addr> = m.ip.as_deref().and_then(ip4).into_iter().collect();
        ips.extend(
            leases
                .iter()
                .filter(|l| l.mac.eq_ignore_ascii_case(&mac))
                .filter_map(|l| l.ip.as_deref().and_then(ip4)),
        );
        ips.extend(pxe_ip.get(&mac).and_then(|i| ip4(i)));
        clients.push(Client {
            ips,
            agent: crate::vnc::connected(&mac),
            exempt_group: m
                .grp
                .as_ref()
                .is_some_and(|g| groups.iter().any(|x| x.eq_ignore_ascii_case(g))),
            pxe: pxe_seen.get(&mac).copied(),
        });
        known.insert(mac);
    }
    // Not registered, PXE-booting right now (a new machine): its grace too.
    for (mac, ip) in &pxe_ip {
        if !known.contains(mac) {
            clients.push(Client {
                ips: ip4(ip).into_iter().collect(),
                agent: false,
                exempt_group: false,
                pxe: pxe_seen.get(mac).copied(),
            });
        }
    }
    let mut ips = allowed_ips(&clients, grace_min(st) * 60, now);
    ips.extend(
        st.db
            .get_config("dhcp_server_ip", "")
            .trim()
            .parse::<Ipv4Addr>(),
    );
    Allowed {
        ips,
        nets: parse_nets(&st.db.get_config("dns_exempt_ips", "")).unwrap_or_default(),
    }
}

/// May this client resolve names?
pub fn allowed(ip: IpAddr) -> bool {
    let IpAddr::V4(ip) = ip.to_canonical() else {
        return false;
    };
    let a = ALLOWED.lock();
    a.ips.contains(&ip) || a.nets.iter().any(|(n, l)| cidr_contains(*n, *l, ip))
}

/// Machines page: this client's names are refused now (None = the DNS server is off).
pub fn blocked(ip: &str) -> Option<bool> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    ip.parse::<IpAddr>().ok().map(|ip| !allowed(ip))
}

/// Services card.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

pub fn allowed_count() -> usize {
    ALLOWED.lock().ips.len()
}

// ---------- packets ----------

/// End of the first question (name + type + class) of a DNS message; None = malformed.
fn question_end(q: &[u8]) -> Option<usize> {
    if q.len() < 12 || u16::from_be_bytes([q[4], q[5]]) == 0 {
        return None;
    }
    let mut i = 12;
    loop {
        let n = *q.get(i)? as usize;
        if n == 0 {
            i += 1;
            break;
        }
        if n & 0xc0 != 0 {
            i += 2; // a pointer ends the name
            break;
        }
        i += 1 + n;
    }
    (i + 4 <= q.len()).then_some(i + 4)
}

/// The first question's name, lower case, dot-separated (labels only — a query carries no compression).
pub(crate) fn qname(q: &[u8]) -> Option<String> {
    question_end(q)?;
    let mut i = 12;
    let mut labels = Vec::new();
    loop {
        let n = *q.get(i)? as usize;
        if n == 0 || n & 0xc0 != 0 {
            break;
        }
        labels.push(String::from_utf8_lossy(q.get(i + 1..i + 1 + n)?).to_ascii_lowercase());
        i += 1 + n;
    }
    Some(labels.join("."))
}

/// An answer to query `q` with no records and this response code (3 NXDOMAIN, 5 REFUSED): its ID, opcode, RD flag
/// and first question.
pub(crate) fn answer(q: &[u8], rcode: u8) -> Option<Vec<u8>> {
    let end = question_end(q)?;
    let mut r = q[..end].to_vec();
    r[2] = 0x80 | (q[2] & 0x79); // QR, opcode, RD (AA, TC off)
    r[3] = 0x80 | (rcode & 0x0f); // RA
    r[4..6].copy_from_slice(&1u16.to_be_bytes());
    r[6..12].fill(0);
    Some(r)
}

/// The reply to `q` for `client`, or None (malformed: no answer). `forward` asks the real resolvers.
async fn reply<F, Fut>(q: &[u8], client: Ipv4Addr, forward: F) -> Option<Vec<u8>>
where
    F: FnOnce(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Option<Vec<u8>>>,
{
    qname(q)?;
    if !allowed(IpAddr::V4(client)) {
        let mut l = LOGGED.lock();
        let now = Instant::now();
        if l.get(&client)
            .is_none_or(|t| now.duration_since(*t) > Duration::from_secs(600))
        {
            l.insert(client, now);
            tracing::info!(
                "dns: {client} refused (no Remote agent, no recent PXE boot, not exempt) — Network page → DNS"
            );
        }
        return answer(q, 5);
    }
    if qname(q).as_deref() == Some(FIREFOX_CANARY) {
        return answer(q, 3);
    }
    forward(q.to_vec()).await
}

/// One query to the real resolvers over UDP, each tried in turn; the reply must carry the query's ID.
async fn forward_udp(q: Vec<u8>, ups: Vec<Ipv4Addr>) -> Option<Vec<u8>> {
    for up in ups {
        let Ok(s) = tokio::net::UdpSocket::bind("0.0.0.0:0").await else {
            continue;
        };
        if s.send_to(&q, (up, 53)).await.is_err() {
            continue;
        }
        let mut buf = vec![0u8; 4096];
        if let Ok(Ok(n)) = tokio::time::timeout(UPSTREAM_WAIT, s.recv(&mut buf)).await
            && n >= 12
            && buf[..2] == q[..2]
        {
            buf.truncate(n);
            return Some(buf);
        }
    }
    None
}

/// Same over TCP (a client retries over TCP when an answer did not fit in UDP).
async fn forward_tcp(q: Vec<u8>, ups: Vec<Ipv4Addr>) -> Option<Vec<u8>> {
    for up in ups {
        let r = tokio::time::timeout(UPSTREAM_WAIT, async {
            let mut s = tokio::net::TcpStream::connect((up, 53)).await.ok()?;
            s.write_all(&(q.len() as u16).to_be_bytes()).await.ok()?;
            s.write_all(&q).await.ok()?;
            let n = s.read_u16().await.ok()? as usize;
            let mut buf = vec![0u8; n];
            s.read_exact(&mut buf).await.ok()?;
            Some(buf)
        })
        .await;
        if let Ok(Some(b)) = r {
            return Some(b);
        }
    }
    None
}

/// A query resolved for a UDP client: forwarded to `ups`, or (None) resolved here and cut to what the client takes
/// over UDP (it asks again over TCP).
async fn resolve_udp(q: Vec<u8>, ups: Option<Vec<Ipv4Addr>>) -> Option<Vec<u8>> {
    let Some(ups) = ups else {
        let r = crate::dnsrec::answer_query(&q).await?;
        let (_, size) = crate::dnsrec::client_udp_size(&q);
        return Some(if r.len() > size {
            crate::dnsrec::truncated(&r, &q)
        } else {
            r
        });
    };
    forward_udp(q, ups).await
}

async fn resolve_tcp(q: Vec<u8>, ups: Option<Vec<Ipv4Addr>>) -> Option<Vec<u8>> {
    match ups {
        Some(ups) => forward_tcp(q, ups).await,
        None => crate::dnsrec::answer_query(&q).await,
    }
}

async fn serve_udp(sock: tokio::net::UdpSocket, ups: Option<Vec<Ipv4Addr>>) {
    let sock = std::sync::Arc::new(sock);
    let mut buf = vec![0u8; 4096];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf).await else {
            continue;
        };
        let SocketAddr::V4(from4) = from else {
            continue;
        };
        let (q, sock, ups) = (buf[..n].to_vec(), sock.clone(), ups.clone());
        tokio::spawn(async move {
            if let Some(r) = reply(&q, *from4.ip(), |q| resolve_udp(q, ups)).await {
                let _ = sock.send_to(&r, from).await;
            }
        });
    }
}

async fn serve_tcp(l: tokio::net::TcpListener, ups: Option<Vec<Ipv4Addr>>) {
    loop {
        let Ok((mut s, from)) = l.accept().await else {
            continue;
        };
        let SocketAddr::V4(from4) = from else {
            continue;
        };
        let ups = ups.clone();
        tokio::spawn(async move {
            // Messages one after the other on the connection; quiet for 10 s = closed.
            while let Ok(Ok(n)) = tokio::time::timeout(Duration::from_secs(10), s.read_u16()).await
            {
                let mut q = vec![0u8; n as usize];
                if s.read_exact(&mut q).await.is_err() {
                    return;
                }
                let ups = ups.clone();
                let Some(r) = reply(&q, *from4.ip(), |q| resolve_tcp(q, ups)).await else {
                    return;
                };
                if s.write_all(&(r.len() as u16).to_be_bytes()).await.is_err()
                    || s.write_all(&r).await.is_err()
                {
                    return;
                }
            }
        });
    }
}

fn tcp_listener(addr: SocketAddrV4) -> std::io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    s.set_reuse_address(true)?;
    s.set_reuse_port(true)?;
    s.bind(&addr.into())?;
    s.listen(256)?;
    s.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(s.into())
}

/// (Re)start the DNS server with the current settings (DHCP Apply, server start, DNS settings saved): stops the old
/// one, starts a new one if it is on. Its own listeners — not in `st.net` (DHCP / TFTP).
pub async fn restart(st: &SharedState) -> Result<String, String> {
    let old = std::mem::take(&mut *HANDLES.lock());
    for h in old {
        h.abort();
        let _ = h.await;
    }
    ACTIVE.store(false, Ordering::Relaxed);
    if !on(st) {
        return Ok("DNS server off".into());
    }
    let server: Ipv4Addr = st
        .db
        .get_config("dhcp_server_ip", "")
        .trim()
        .parse()
        .map_err(|_| "DNS: the server IP is not set (Network page)".to_string())?;
    // Forward: the resolvers to pass queries to; recursive (None): resolved here from the root servers.
    let ups = if recursive(st) {
        None
    } else {
        let u = upstream(st);
        if u.is_empty() {
            return Err(
                "DNS: no resolver to forward to — enter them on the DNS card (or a gateway), or resolve recursively"
                    .into(),
            );
        }
        if u.contains(&server) {
            return Err("DNS: the forward list names this server itself — list the real resolvers (router, 1.1.1.1…)".into());
        }
        Some(u)
    };
    let addr = SocketAddrV4::new(server, 53);
    let udp =
        crate::dhcp::udp(addr, "", false).map_err(|e| format!("DNS: bind udp {addr}: {e}"))?;
    let tcp = tcp_listener(addr).map_err(|e| format!("DNS: bind tcp {addr}: {e}"))?;
    *ALLOWED.lock() = compute(st);
    let st2 = st.clone();
    let refresh = tokio::spawn(async move {
        loop {
            tokio::time::sleep(REFRESH).await;
            let st3 = st2.clone();
            if let Ok(a) = tokio::task::spawn_blocking(move || compute(&st3)).await {
                *ALLOWED.lock() = a;
            }
        }
    });
    *HANDLES.lock() = vec![
        refresh,
        tokio::spawn(serve_udp(udp, ups.clone())),
        tokio::spawn(serve_tcp(tcp, ups.clone())),
    ];
    ACTIVE.store(true, Ordering::Relaxed);
    let how = match &ups {
        Some(u) => format!(
            "forwarded to {}",
            u.iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => "resolved here from the root servers (cached)".into(),
    };
    Ok(format!(
        "DNS server on {server}:53, {how} — only for healthy machines"
    ))
}

// ---------- API ----------

/// GET /api/dns — the settings + what it does now.
async fn status(State(st): State<SharedState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "mode": st.db.get_config("dns_mode", "off"),
        "exempt_groups": exempt_groups(&st),
        "exempt_ips": st.db.get_config("dns_exempt_ips", ""),
        "grace_min": grace_min(&st),
        "resolve": st.db.get_config("dns_resolve", "forward"),
        "upstream": upstream(&st),
        "cached": crate::dnsrec::cached(),
        "active": active(),
        "allowed": allowed_count(),
        "dhcp_full": st.db.get_config("dhcp_mode", "off") == "full",
        "server": st.db.get_config("dhcp_server_ip", ""),
    }))
}

#[derive(Deserialize)]
struct DnsBody {
    mode: String,
    #[serde(default)]
    exempt_groups: Vec<String>,
    #[serde(default)]
    exempt_ips: String,
    grace_min: u64,
    /// "forward" | "recursive".
    #[serde(default)]
    resolve: String,
    /// Forward mode: the resolvers (IPv4).
    #[serde(default)]
    upstream: Vec<String>,
}

/// POST /api/dns {mode, resolve, upstream, exempt_groups, exempt_ips, grace_min} — saved, then DHCP (option 6) and DNS
/// restart.
async fn set(
    State(st): State<SharedState>,
    Json(b): Json<DnsBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if b.mode != "on" && b.mode != "off" {
        return Err(bad("mode: on | off"));
    }
    parse_nets(&b.exempt_ips).map_err(bad)?;
    if b.grace_min > 120 {
        return Err(bad("grace: 0 to 120 minutes"));
    }
    if let Some(g) = b
        .exempt_groups
        .iter()
        .find(|g| !crate::drivers::group_ok(g))
    {
        return Err(bad(format!("group {g:?}")));
    }
    let resolve = if b.resolve == "recursive" {
        "recursive"
    } else {
        "forward"
    };
    let mut ups: Vec<String> = Vec::new();
    for u in b
        .upstream
        .iter()
        .map(|u| u.trim())
        .filter(|u| !u.is_empty())
    {
        if u.parse::<Ipv4Addr>().is_err() {
            return Err(bad(format!("{u:?} is not an IPv4 address")));
        }
        ups.push(u.to_string());
    }
    for (k, v) in [
        ("dns_mode", b.mode.clone()),
        ("dns_resolve", resolve.to_string()),
        ("dns_upstream", ups.join(",")),
        ("dns_exempt_ips", b.exempt_ips.trim().to_string()),
        ("dns_grace_min", b.grace_min.to_string()),
    ] {
        st.db.set_config(k, &v).map_err(ise)?;
    }
    set_exempt_groups(&st, &b.exempt_groups).map_err(ise)?;
    tracing::info!("dns: settings saved (mode {})", b.mode);
    // The DHCP server hands out this server as DNS (or the list again): restart it like an Apply — it restarts DNS too.
    let msg = crate::dhcp::start(&st).await.map_err(bad)?;
    Ok(Json(serde_json::json!({"ok": true, "status": msg})))
}

#[cfg(test)]
#[path = "../tests/unit/dns.rs"]
mod tests;
