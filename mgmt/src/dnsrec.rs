// dnsrec.rs — the recursive mode of the clients' DNS server (dns.rs, Network page → DNS → Resolve: recursive): names
// are resolved from the root servers down (root → TLD → the domain's own servers) instead of being passed to another
// resolver, so the room depends on no outside DNS service. Answers and delegations are cached for their TTL.
// Kept small: IPv4 to the servers, no DNSSEC validation, records re-encoded without name compression (a large answer
// goes over TCP). A forged reply must guess a random ID and source port and name the question asked; records are only
// taken from servers responsible for them (in-bailiwick glue, CNAME targets outside the zone looked up separately).
use parking_lot::Mutex;
use std::collections::HashMap;
use std::future::Future;
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const T_A: u16 = 1;
const T_NS: u16 = 2;
const T_CNAME: u16 = 5;
const T_SOA: u16 = 6;
const T_OPT: u16 = 41;
const SERVFAIL: u8 = 2;
const NXDOMAIN: u8 = 3;
/// What one server gets to answer.
const SERVER_WAIT: Duration = Duration::from_millis(1500);
/// Our EDNS buffer (to the servers, and the most we send a client over UDP).
pub(crate) const EDNS: u16 = 1232;

/// a.root-servers.net … m.root-servers.net.
const ROOTS: [Ipv4Addr; 13] = [
    Ipv4Addr::new(198, 41, 0, 4),
    Ipv4Addr::new(170, 247, 170, 2),
    Ipv4Addr::new(192, 33, 4, 12),
    Ipv4Addr::new(199, 7, 91, 13),
    Ipv4Addr::new(192, 203, 230, 10),
    Ipv4Addr::new(192, 5, 5, 241),
    Ipv4Addr::new(192, 112, 36, 4),
    Ipv4Addr::new(198, 97, 190, 53),
    Ipv4Addr::new(192, 36, 148, 17),
    Ipv4Addr::new(192, 58, 128, 30),
    Ipv4Addr::new(193, 0, 14, 129),
    Ipv4Addr::new(199, 7, 83, 42),
    Ipv4Addr::new(202, 12, 27, 33),
];

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Rr {
    pub name: String,
    pub rtype: u16,
    pub class: u16,
    pub ttl: u32,
    /// Uncompressed: names inside (NS, CNAME, PTR, MX, SOA, SRV) written out in full.
    pub rdata: Vec<u8>,
}

#[derive(Debug, Default)]
pub(crate) struct Msg {
    pub id: u16,
    pub flags: u16,
    /// The first question: (name, type, class).
    pub q: Option<(String, u16, u16)>,
    pub an: Vec<Rr>,
    pub ns: Vec<Rr>,
    pub ar: Vec<Rr>,
}

impl Msg {
    pub fn rcode(&self) -> u8 {
        (self.flags & 0x0f) as u8
    }
}

/// A (possibly compressed) name at `pos`: lower case, no trailing dot ("" = the root), and where it ends.
pub(crate) fn read_name(m: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    let mut jumps = 0;
    loop {
        let len = *m.get(pos)? as usize;
        if len & 0xc0 == 0xc0 {
            let ptr = ((len & 0x3f) << 8) | *m.get(pos + 1)? as usize;
            end.get_or_insert(pos + 2);
            pos = ptr;
            jumps += 1;
            if jumps > 32 {
                return None;
            }
            continue;
        }
        if len & 0xc0 != 0 {
            return None;
        }
        if len == 0 {
            end.get_or_insert(pos + 1);
            break;
        }
        labels.push(String::from_utf8_lossy(m.get(pos + 1..pos + 1 + len)?).to_ascii_lowercase());
        pos += 1 + len;
        if labels.len() > 127 {
            return None;
        }
    }
    Some((labels.join("."), end?))
}

pub(crate) fn write_name(out: &mut Vec<u8>, name: &str) {
    for l in name.split('.').filter(|l| !l.is_empty()) {
        let l = &l.as_bytes()[..l.len().min(63)];
        out.push(l.len() as u8);
        out.extend_from_slice(l);
    }
    out.push(0);
}

fn name_bytes(name: &str) -> Vec<u8> {
    let mut v = Vec::new();
    write_name(&mut v, name);
    v
}

/// The record data at `start..start + len`, names in it written out (they may point anywhere in the message).
fn rdata(m: &[u8], rtype: u16, start: usize, len: usize) -> Option<Vec<u8>> {
    let raw = m.get(start..start + len)?;
    Some(match rtype {
        2 | 5 | 12 => name_bytes(&read_name(m, start)?.0),
        15 => {
            let mut v = raw.get(..2)?.to_vec();
            v.extend(name_bytes(&read_name(m, start + 2)?.0));
            v
        }
        6 => {
            let (mname, p) = read_name(m, start)?;
            let (rname, p2) = read_name(m, p)?;
            let mut v = name_bytes(&mname);
            v.extend(name_bytes(&rname));
            v.extend_from_slice(m.get(p2..p2 + 20)?);
            v
        }
        33 => {
            let mut v = raw.get(..6)?.to_vec();
            v.extend(name_bytes(&read_name(m, start + 6)?.0));
            v
        }
        _ => raw.to_vec(),
    })
}

pub(crate) fn parse(m: &[u8]) -> Option<Msg> {
    let u16at =
        |p: usize| -> Option<u16> { Some(u16::from_be_bytes([*m.get(p)?, *m.get(p + 1)?])) };
    let mut msg = Msg {
        id: u16at(0)?,
        flags: u16at(2)?,
        ..Default::default()
    };
    let (qd, an, ns, ar) = (u16at(4)?, u16at(6)?, u16at(8)?, u16at(10)?);
    let mut pos = 12;
    for i in 0..qd {
        let (name, p) = read_name(m, pos)?;
        if i == 0 {
            msg.q = Some((name, u16at(p)?, u16at(p + 2)?));
        }
        pos = p + 4;
    }
    for (n, sec) in [(an, 0), (ns, 1), (ar, 2)] {
        for _ in 0..n {
            let (name, p) = read_name(m, pos)?;
            let (rtype, class) = (u16at(p)?, u16at(p + 2)?);
            let ttl = u32::from_be_bytes(m.get(p + 4..p + 8)?.try_into().ok()?);
            let len = u16at(p + 8)? as usize;
            let rr = Rr {
                name,
                rtype,
                class,
                ttl,
                rdata: rdata(m, rtype, p + 10, len)?,
            };
            pos = p + 10 + len;
            match sec {
                0 => msg.an.push(rr),
                1 => msg.ns.push(rr),
                _ => msg.ar.push(rr),
            }
        }
    }
    Some(msg)
}

fn write_rr(out: &mut Vec<u8>, r: &Rr, ttl: u32) {
    write_name(out, &r.name);
    out.extend_from_slice(&r.rtype.to_be_bytes());
    out.extend_from_slice(&r.class.to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&(r.rdata.len() as u16).to_be_bytes());
    out.extend_from_slice(&r.rdata);
}

/// The name a NS / CNAME record points at.
fn target(r: &Rr) -> Option<String> {
    read_name(&r.rdata, 0).map(|(n, _)| n)
}

/// `name` is `zone` or below it ("" = the root holds everything).
pub(crate) fn in_zone(name: &str, zone: &str) -> bool {
    zone.is_empty() || name == zone || name.ends_with(&format!(".{zone}"))
}

/// The client's question (`q`, raw: its name keeps its case) answered: ID and RD from it, RA set.
pub(crate) fn response(q: &[u8], rcode: u8, a: &Answer, edns: bool) -> Option<Vec<u8>> {
    let qmsg = parse(q)?;
    let end = {
        let (_, p) = read_name(q, 12)?;
        p + 4
    };
    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(&qmsg.id.to_be_bytes());
    out.push(0x80 | (q[2] & 0x01)); // QR, RD as asked
    out.push(0x80 | (rcode & 0x0f)); // RA
    let left = a
        .expires
        .saturating_duration_since(Instant::now())
        .as_secs() as u32;
    let counts = [1u16, a.an.len() as u16, a.ns.len() as u16, edns as u16];
    for c in counts {
        out.extend_from_slice(&c.to_be_bytes());
    }
    out.extend_from_slice(&q[12..end]);
    for r in &a.an {
        write_rr(&mut out, r, r.ttl.min(left.max(1)));
    }
    for r in &a.ns {
        write_rr(&mut out, r, r.ttl.min(left.max(1)));
    }
    if edns {
        out.extend_from_slice(&[0, 0, 41]);
        out.extend_from_slice(&EDNS.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    }
    Some(out)
}

/// The UDP size the client accepts: its EDNS buffer (OPT record), else 512.
pub(crate) fn client_udp_size(q: &[u8]) -> (bool, usize) {
    match parse(q).and_then(|m| m.ar.into_iter().find(|r| r.rtype == T_OPT)) {
        Some(o) => (true, (o.class as usize).clamp(512, EDNS as usize)),
        None => (false, 512),
    }
}

/// Too big for UDP: just the header (TC set, no records) + the question — the client asks again over TCP.
pub(crate) fn truncated(r: &[u8], q: &[u8]) -> Vec<u8> {
    let end = read_name(q, 12).map_or(q.len(), |(_, p)| (p + 4).min(q.len()));
    let mut t = r[..12].to_vec();
    t[2] |= 0x02;
    t[4..6].copy_from_slice(&1u16.to_be_bytes());
    t[6..12].fill(0);
    t.extend_from_slice(&q[12..end]);
    t
}

// ---------- the resolver ----------

#[derive(Clone, Debug)]
pub(crate) struct Answer {
    pub rcode: u8,
    pub an: Vec<Rr>,
    /// The zone's SOA for a negative answer (clients cache "no such name" by it).
    pub ns: Vec<Rr>,
    pub expires: Instant,
}

const CACHE_MAX: usize = 20_000;
type Key = (String, u16);
static ANSWERS: LazyLock<Mutex<HashMap<Key, Answer>>> = LazyLock::new(Default::default);
/// Zone → its servers' addresses, until.
type Delegations = HashMap<String, (Vec<Ipv4Addr>, Instant)>;
static ZONES: LazyLock<Mutex<Delegations>> = LazyLock::new(Default::default);

fn ttl_of(rrs: &[Rr], negative: bool) -> Duration {
    let t = if negative {
        // SOA minimum (its last 4 bytes) bounded by the SOA's own TTL.
        rrs.iter().find(|r| r.rtype == T_SOA).map_or(60, |r| {
            let n = r.rdata.len();
            let min = r
                .rdata
                .get(n.saturating_sub(4)..)
                .and_then(|b| b.try_into().ok())
                .map_or(60, u32::from_be_bytes);
            min.min(r.ttl)
        })
    } else {
        rrs.iter().map(|r| r.ttl).min().unwrap_or(60)
    };
    Duration::from_secs(t.clamp(5, 3600) as u64)
}

/// Answers in the cache (Network page → DNS).
pub(crate) fn cached() -> usize {
    ANSWERS.lock().len()
}

fn remember(k: Key, a: &Answer) {
    let mut c = ANSWERS.lock();
    if c.len() >= CACHE_MAX {
        let now = Instant::now();
        c.retain(|_, v| v.expires > now);
        if c.len() >= CACHE_MAX {
            c.clear();
        }
    }
    c.insert(k, a.clone());
}

/// The deepest zone above `name` whose servers are known (cached), else the root.
fn closest(name: &str) -> (String, Vec<Ipv4Addr>) {
    let z = ZONES.lock();
    let now = Instant::now();
    let mut n = name;
    loop {
        if let Some((ips, until)) = z.get(n)
            && *until > now
            && !ips.is_empty()
        {
            return (n.to_string(), ips.clone());
        }
        match n.split_once('.') {
            Some((_, rest)) => n = rest,
            None => return (String::new(), ROOTS.to_vec()),
        }
    }
}

fn random_id() -> u16 {
    let mut b = [0u8; 2];
    // SAFETY: getrandom writes at most the 2 bytes of `b`.
    unsafe { libc::getrandom(b.as_mut_ptr().cast(), 2, 0) };
    u16::from_be_bytes(b)
}

/// A non-recursive question for a server, with our EDNS buffer.
pub(crate) fn query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut q = id.to_be_bytes().to_vec();
    q.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 1]);
    write_name(&mut q, name);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes());
    q.extend_from_slice(&[0, 0, 41]);
    q.extend_from_slice(&EDNS.to_be_bytes());
    q.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    q
}

/// Ask one server (UDP from a fresh random port; over TCP when the answer is truncated). Only a reply to exactly this
/// question (ID, name, type) counts.
async fn ask(ip: Ipv4Addr, name: &str, qtype: u16) -> Option<Msg> {
    let id = random_id();
    let q = query(id, name, qtype);
    let matches = |m: &Msg| {
        m.id == id
            && m.flags & 0x8000 != 0
            && m.q
                .as_ref()
                .is_some_and(|(n, t, _)| n == name && *t == qtype)
    };
    let s = tokio::net::UdpSocket::bind("0.0.0.0:0").await.ok()?;
    s.send_to(&q, (ip, 53)).await.ok()?;
    let mut buf = vec![0u8; 4096];
    let m = tokio::time::timeout(SERVER_WAIT, async {
        loop {
            let (n, from) = s.recv_from(&mut buf).await.ok()?;
            if from.ip() != std::net::IpAddr::V4(ip) {
                continue;
            }
            if let Some(m) = parse(&buf[..n]).filter(matches) {
                return Some(m);
            }
        }
    })
    .await
    .ok()??;
    if m.flags & 0x0200 == 0 {
        return Some(m);
    }
    // Truncated: the same question over TCP.
    tokio::time::timeout(SERVER_WAIT, async {
        let mut t = tokio::net::TcpStream::connect((ip, 53)).await.ok()?;
        t.write_all(&(q.len() as u16).to_be_bytes()).await.ok()?;
        t.write_all(&q).await.ok()?;
        let n = t.read_u16().await.ok()? as usize;
        let mut b = vec![0u8; n];
        t.read_exact(&mut b).await.ok()?;
        parse(&b).filter(matches)
    })
    .await
    .ok()?
}

/// The first of `ips` (at most 4 tried) that answers with NOERROR or NXDOMAIN.
async fn ask_any(ips: &[Ipv4Addr], name: &str, qtype: u16) -> Option<Msg> {
    for &ip in ips.iter().take(4) {
        if let Some(m) = ask(ip, name, qtype).await
            && matches!(m.rcode(), 0 | NXDOMAIN)
        {
            return Some(m);
        }
    }
    None
}

fn servfail() -> Answer {
    Answer {
        rcode: SERVFAIL,
        an: Vec::new(),
        ns: Vec::new(),
        expires: Instant::now(),
    }
}

/// Resolve `name` / `qtype` from the closest known zone down. `depth` bounds CNAME chains and server-name lookups.
pub(crate) fn resolve(
    name: String,
    qtype: u16,
    depth: u8,
) -> Pin<Box<dyn Future<Output = Answer> + Send>> {
    Box::pin(async move {
        let key = (name.clone(), qtype);
        if let Some(a) = ANSWERS
            .lock()
            .get(&key)
            .filter(|a| a.expires > Instant::now())
        {
            return a.clone();
        }
        if depth > 8 {
            return servfail();
        }
        let (mut zone, mut servers) = closest(&name);
        for _ in 0..24 {
            let Some(m) = ask_any(&servers, &name, qtype).await else {
                return servfail();
            };
            if m.rcode() == NXDOMAIN {
                let soa: Vec<Rr> = m.ns.into_iter().filter(|r| r.rtype == T_SOA).collect();
                let a = Answer {
                    rcode: NXDOMAIN,
                    expires: Instant::now() + ttl_of(&soa, true),
                    an: Vec::new(),
                    ns: soa,
                };
                remember(key, &a);
                return a;
            }
            // An answer: follow the CNAME chain inside this zone; a target elsewhere is looked up on its own.
            if !m.an.is_empty() {
                let mut out: Vec<Rr> = Vec::new();
                let mut cur = name.clone();
                for _ in 0..10 {
                    let hits: Vec<Rr> =
                        m.an.iter()
                            .filter(|r| r.name == cur && (r.rtype == qtype || qtype == 255))
                            .cloned()
                            .collect();
                    if !hits.is_empty() {
                        out.extend(hits);
                        break;
                    }
                    let Some(c) = m.an.iter().find(|r| r.name == cur && r.rtype == T_CNAME) else {
                        break;
                    };
                    out.push(c.clone());
                    let Some(t) = target(c) else { break };
                    if !in_zone(&t, &zone) || !m.an.iter().any(|r| r.name == t) {
                        let more = resolve(t, qtype, depth + 1).await;
                        if more.rcode != 0 && more.rcode != NXDOMAIN {
                            return servfail();
                        }
                        out.extend(more.an);
                        break;
                    }
                    cur = t;
                }
                if out.is_empty() {
                    return servfail();
                }
                let a = Answer {
                    rcode: 0,
                    expires: Instant::now() + ttl_of(&out, false),
                    an: out,
                    ns: Vec::new(),
                };
                remember(key, &a);
                return a;
            }
            // A referral: servers for a zone below this one.
            let refs: Vec<&Rr> =
                m.ns.iter()
                    .filter(|r| {
                        r.rtype == T_NS
                            && in_zone(&name, &r.name)
                            && r.name != zone
                            && in_zone(&r.name, &zone)
                    })
                    .collect();
            if refs.is_empty() {
                // Nothing for this type (NODATA), or a lame answer.
                if m.flags & 0x0400 != 0 || m.ns.iter().any(|r| r.rtype == T_SOA) {
                    let soa: Vec<Rr> = m.ns.into_iter().filter(|r| r.rtype == T_SOA).collect();
                    let a = Answer {
                        rcode: 0,
                        expires: Instant::now() + ttl_of(&soa, true),
                        an: Vec::new(),
                        ns: soa,
                    };
                    remember(key, &a);
                    return a;
                }
                return servfail();
            }
            let child = refs[0].name.clone();
            let names: Vec<String> = refs.iter().filter_map(|r| target(r)).collect();
            // Glue only for server names inside the zone that sent it.
            let mut ips: Vec<Ipv4Addr> =
                m.ar.iter()
                    .filter(|r| r.rtype == T_A && r.rdata.len() == 4)
                    .filter(|r| names.contains(&r.name) && in_zone(&r.name, &zone))
                    .map(|r| Ipv4Addr::new(r.rdata[0], r.rdata[1], r.rdata[2], r.rdata[3]))
                    .collect();
            if ips.is_empty() {
                for n in names.iter().take(2) {
                    let a = resolve(n.clone(), T_A, depth + 1).await;
                    ips.extend(
                        a.an.iter()
                            .filter(|r| r.rtype == T_A && r.rdata.len() == 4)
                            .map(|r| Ipv4Addr::new(r.rdata[0], r.rdata[1], r.rdata[2], r.rdata[3])),
                    );
                    if !ips.is_empty() {
                        break;
                    }
                }
            }
            if ips.is_empty() {
                return servfail();
            }
            let ttl = ttl_of(
                &refs.iter().map(|r| (*r).clone()).collect::<Vec<_>>(),
                false,
            );
            ZONES
                .lock()
                .insert(child.clone(), (ips.clone(), Instant::now() + ttl));
            zone = child;
            servers = ips;
        }
        servfail()
    })
}

/// The reply to a client's query `q` (raw), resolved here.
pub(crate) async fn answer_query(q: &[u8]) -> Option<Vec<u8>> {
    let m = parse(q)?;
    let (name, qtype, _) = m.q?;
    let (edns, _) = client_udp_size(q);
    let a = resolve(name, qtype, 0).await;
    response(q, a.rcode, &a, edns)
}

#[cfg(test)]
#[path = "../tests/unit/dnsrec.rs"]
mod tests;
