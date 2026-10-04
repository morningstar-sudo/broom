// dhcp.rs — built-in DHCP server (replaces dnsmasq) for UEFI PXE + iPXE. It is the LAN's DHCP server: hands out IPs
// (Machines-table binding > previous lease > first free in range) + the boot file. Turn it off (`dhcp_mode` = "off")
// when another DHCP server owns the LAN — then broom serves no DHCP/TFTP (no proxyDHCP mode anymore). No DNS.
// Every packet reads config/bindings/leases from the DB → Machines/DHCP edits apply immediately;
// start() rebinds sockets only for interface changes. Leases live in the `leases` table.
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use crate::db::{Db, Lease};
use crate::now_secs as now;
use crate::SharedState;

const MAGIC: [u8; 4] = [99, 130, 83, 99];
const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const DECLINE: u8 = 4;
const ACK: u8 = 5;
const NAK: u8 = 6;
const RELEASE: u8 = 7;
const INFORM: u8 = 8;
/// An OFFER reserves its IP this long; the ACK extends it to the full lease.
const OFFER_HOLD_S: u64 = 60;

#[derive(Clone, Debug, PartialEq)]
pub struct Packet {
    pub op: u8,
    pub htype: u8,
    pub hlen: u8,
    pub xid: [u8; 4],
    pub flags: [u8; 2],
    pub ciaddr: Ipv4Addr,
    pub yiaddr: Ipv4Addr,
    pub siaddr: Ipv4Addr,
    pub giaddr: Ipv4Addr,
    pub chaddr: [u8; 16],
    pub file: String,
    pub opts: Vec<(u8, Vec<u8>)>,
}

impl Packet {
    pub fn opt(&self, code: u8) -> Option<&[u8]> {
        self.opts.iter().find(|(c, _)| *c == code).map(|(_, v)| v.as_slice())
    }
    fn opt_ip(&self, code: u8) -> Option<Ipv4Addr> {
        self.opt(code).filter(|v| v.len() == 4).map(|v| Ipv4Addr::new(v[0], v[1], v[2], v[3]))
    }
    fn push(&mut self, code: u8, v: impl Into<Vec<u8>>) {
        let mut v = v.into();
        v.truncate(255);
        self.opts.push((code, v));
    }
}

pub fn parse(b: &[u8]) -> Option<Packet> {
    if b.len() < 240 || b[236..240] != MAGIC {
        return None;
    }
    let ip = |o: usize| Ipv4Addr::new(b[o], b[o + 1], b[o + 2], b[o + 3]);
    let mut opts = Vec::new();
    let mut i = 240;
    while i < b.len() {
        match b[i] {
            0 => i += 1,
            255 => break,
            code => {
                let len = *b.get(i + 1)? as usize;
                opts.push((code, b.get(i + 2..i + 2 + len)?.to_vec()));
                i += 2 + len;
            }
        }
    }
    let file_end = b[108..236].iter().position(|&c| c == 0).unwrap_or(128);
    Some(Packet {
        op: b[0],
        htype: b[1],
        hlen: b[2],
        xid: b[4..8].try_into().ok()?,
        flags: b[10..12].try_into().ok()?,
        ciaddr: ip(12),
        yiaddr: ip(16),
        siaddr: ip(20),
        giaddr: ip(24),
        chaddr: b[28..44].try_into().ok()?,
        file: String::from_utf8_lossy(&b[108..108 + file_end]).into_owned(),
        opts,
    })
}

pub fn build(p: &Packet) -> Vec<u8> {
    let mut b = vec![0u8; 236];
    b[0] = p.op;
    b[1] = p.htype;
    b[2] = p.hlen;
    b[4..8].copy_from_slice(&p.xid);
    b[10..12].copy_from_slice(&p.flags);
    for (o, ip) in [(12, p.ciaddr), (16, p.yiaddr), (20, p.siaddr), (24, p.giaddr)] {
        b[o..o + 4].copy_from_slice(&ip.octets());
    }
    b[28..44].copy_from_slice(&p.chaddr);
    let f = p.file.as_bytes();
    let n = f.len().min(127);
    b[108..108 + n].copy_from_slice(&f[..n]);
    b.extend_from_slice(&MAGIC);
    for (c, v) in &p.opts {
        b.push(*c);
        b.push(v.len() as u8);
        b.extend_from_slice(v);
    }
    b.push(255);
    if b.len() < 300 {
        b.resize(300, 0); // BOOTP minimum size
    }
    b
}

/// "aa:bb:cc:dd:ee:ff" (same form as the Machines table and iPXE ${net0/mac}).
fn mac_str(chaddr: &[u8; 16]) -> String {
    chaddr[..6].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

pub struct Cfg {
    pub iface: String,
    pub server: Ipv4Addr,
    pub start: Ipv4Addr,
    pub end: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub lease_s: u32,
    /// "Secure Boot clients" (Network page): hand UEFI PXE the official signed iPXE (sb/…) instead of our own build.
    pub sb: bool,
}

/// "12h" / "30m" / "1d" / "3600" → seconds (default 12h).
fn lease_secs(s: &str) -> u32 {
    let s = s.trim();
    let (n, mul) = match s.chars().last() {
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('d') => (&s[..s.len() - 1], 86400),
        _ => (s, 1),
    };
    n.parse::<u32>().ok().filter(|&v| v > 0).map_or(43200, |v| v.saturating_mul(mul))
}

impl Cfg {
    pub fn load(db: &dyn Db) -> Result<Cfg, String> {
        let g = |k: &str, d: &str| db.get_config(k, d);
        let ip = |k: &str| g(k, "").trim().parse::<Ipv4Addr>().ok();
        let server = ip("dhcp_server_ip").ok_or("dhcp_server_ip is not set — run setup (start as root)")?;
        // Range default: .100–.200 of the subnet (same as the old dnsmasq config).
        let base = ip("dhcp_subnet").unwrap_or(server).octets();
        let start = ip("dhcp_range_start").unwrap_or(Ipv4Addr::new(base[0], base[1], base[2], 100));
        let end = ip("dhcp_range_end").unwrap_or(Ipv4Addr::new(base[0], base[1], base[2], 200));
        Ok(Cfg {
            iface: g("dhcp_iface", ""),
            server,
            start,
            end,
            netmask: ip("dhcp_netmask").unwrap_or(Ipv4Addr::new(255, 255, 255, 0)),
            gateway: ip("dhcp_gateway"),
            dns: g("dhcp_dns", "").split([',', ' ']).filter_map(|s| s.trim().parse().ok()).collect(),
            lease_s: lease_secs(&g("dhcp_lease", "12h")),
            sb: g("ipxe_signed", "0") == "1",
        })
    }
}

/// Snapshot of what IP allocation needs: static bindings + active full-mode leases.
pub struct Store {
    /// mac → (ip, hostname) from the Machines table.
    pub bindings: HashMap<String, (Ipv4Addr, Option<String>)>,
    /// mac → (ip, expires unix s), full-mode leases only.
    pub leases: HashMap<String, (Ipv4Addr, u64)>,
    pub now: u64,
}

impl Store {
    pub fn load(db: &dyn Db, now: u64) -> Store {
        let bindings = db
            .machines()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| {
                let ip = m.ip?.trim().parse().ok()?;
                Some((m.mac.to_lowercase().replace('-', ":"), (ip, m.hostname)))
            })
            .collect();
        let leases = db
            .leases()
            .unwrap_or_default()
            .into_iter()
            .filter(|l| l.source == "full")
            .filter_map(|l| Some((l.mac, (l.ip?.parse().ok()?, l.expires.max(0) as u64))))
            .collect();
        Store { bindings, leases, now }
    }
}

#[derive(Debug, PartialEq)]
pub enum Dest {
    Broadcast,
    Unicast(SocketAddrV4),
}

#[derive(Debug, PartialEq)]
pub enum LeaseOp {
    None,
    Set { mac: String, ip: Option<Ipv4Addr>, hostname: Option<String>, expires: u64, source: &'static str },
    Remove(String),
}

pub struct Outcome {
    pub reply: Option<(Packet, Dest)>,
    pub lease: LeaseOp,
    pub log: String,
    /// Log `log` as a warning (refused/declined/pool exhausted).
    pub warn: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Ipxe,
    /// UEFI PXE firmware, x64 (client arch 7/9).
    PxeEfi,
    /// PXE firmware of another arch (e.g. legacy BIOS) — no boot file for it.
    PxeOther,
    Plain,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Ipxe => "iPXE",
            Kind::PxeEfi => "UEFI PXE",
            Kind::PxeOther => "PXE, not UEFI x64",
            Kind::Plain => "DHCP",
        }
    }
}

/// Hostname from the Machines table, else the mac — how clients are named in the log.
fn who(mac: &str, st: &Store) -> String {
    st.bindings.get(mac).and_then(|(_, h)| h.clone()).unwrap_or_else(|| mac.to_string())
}

fn kind(req: &Packet) -> Kind {
    let ipxe = req.opt(175).is_some() || req.opt(77).is_some_and(|v| v.windows(4).any(|w| w == b"iPXE"));
    if ipxe {
        return Kind::Ipxe;
    }
    let pxe = req.opt(60).is_some_and(|v| v.starts_with(b"PXEClient"));
    let arch = req.opt(93).filter(|v| v.len() >= 2).map(|v| u16::from_be_bytes([v[0], v[1]]));
    match (pxe, arch) {
        (true, Some(7 | 9)) => Kind::PxeEfi,
        (true, _) => Kind::PxeOther,
        _ => Kind::Plain,
    }
}

/// Boot file for the client: iPXE → our dynamic menu URL; UEFI PXE firmware → snponly.efi over TFTP.
fn boot_file(k: Kind, cfg: &Cfg) -> Option<String> {
    match k {
        Kind::Ipxe => Some(format!("http://{}/boot.ipxe?mac=${{net0/mac}}&ip=${{net0/ip}}", cfg.server)),
        // Secure Boot: the iPXE shim (Microsoft-signed) then loads sb/snponly.efi (iPXE-signed) by name itself.
        Kind::PxeEfi => Some(if cfg.sb { "sb/snponly-shim.efi" } else { "snponly.efi" }.into()),
        _ => None,
    }
}

fn reply(req: &Packet, mt: u8, cfg: &Cfg) -> Packet {
    let mut p = Packet {
        op: 2,
        htype: req.htype,
        hlen: req.hlen,
        xid: req.xid,
        flags: req.flags,
        ciaddr: req.ciaddr,
        yiaddr: Ipv4Addr::UNSPECIFIED,
        siaddr: cfg.server,
        giaddr: req.giaddr,
        chaddr: req.chaddr,
        file: String::new(),
        opts: Vec::new(),
    };
    p.push(53, [mt]);
    p.push(54, cfg.server.octets());
    p
}

fn in_range(ip: Ipv4Addr, cfg: &Cfg) -> bool {
    (u32::from(cfg.start)..=u32::from(cfg.end)).contains(&u32::from(ip))
}

/// Another MAC holds a live lease on `ip`.
fn held_by_other<'a>(mac: &str, ip: Ipv4Addr, st: &'a Store) -> Option<&'a str> {
    st.leases.iter().find(|(m, (l, exp))| m.as_str() != mac && *l == ip && *exp > st.now).map(|(m, _)| m.as_str())
}

/// The machine's static IP, if it is free to hand out (no other MAC still holds a live lease on it — the admin may
/// have bound an address that a guest laptop got from the pool; handing it out twice = IP conflict until renewal).
fn usable_binding(mac: &str, st: &Store) -> Option<Ipv4Addr> {
    let (b, _) = st.bindings.get(mac)?;
    held_by_other(mac, *b, st).is_none().then_some(*b)
}

/// May `mac` use `ip`? A bound machine gets its binding (once free) — or, while another MAC still holds it, a free
/// pool IP for now; others get free IPs in the range.
fn allowed(mac: &str, ip: Ipv4Addr, cfg: &Cfg, st: &Store) -> bool {
    if let Some((b, _)) = st.bindings.get(mac) {
        if *b == ip {
            return held_by_other(mac, ip, st).is_none();
        }
        if usable_binding(mac, st).is_some() {
            return false;
        }
    }
    in_range(ip, cfg)
        && ip != cfg.server
        && Some(ip) != cfg.gateway
        && !st.bindings.values().any(|(b, _)| *b == ip)
        && held_by_other(mac, ip, st).is_none()
}

fn pick(mac: &str, cfg: &Cfg, st: &Store) -> Option<Ipv4Addr> {
    if let Some(b) = usable_binding(mac, st) {
        return Some(b);
    }
    if let Some((l, _)) = st.leases.get(mac) {
        if allowed(mac, *l, cfg, st) {
            return Some(*l);
        }
    }
    (u32::from(cfg.start)..=u32::from(cfg.end)).map(Ipv4Addr::from).find(|&ip| allowed(mac, ip, cfg, st))
}

/// Full-mode OFFER/ACK body: IP + network options + boot file.
fn full_reply(req: &Packet, mt: u8, ip: Ipv4Addr, mac: &str, k: Kind, cfg: &Cfg, st: &Store) -> Packet {
    let mut p = reply(req, mt, cfg);
    p.yiaddr = ip;
    p.push(51, cfg.lease_s.to_be_bytes());
    p.push(58, (cfg.lease_s / 2).to_be_bytes());
    p.push(59, (cfg.lease_s / 8 * 7).to_be_bytes());
    p.push(1, cfg.netmask.octets());
    if let Some(gw) = cfg.gateway {
        p.push(3, gw.octets());
    }
    if !cfg.dns.is_empty() {
        p.push(6, cfg.dns.iter().flat_map(|d| d.octets()).collect::<Vec<_>>());
    }
    if let Some((_, Some(h))) = st.bindings.get(mac) {
        p.push(12, h.as_bytes().to_vec());
    }
    if let Some(f) = boot_file(k, cfg) {
        p.file = f;
    }
    if k == Kind::Ipxe {
        // iPXE option 175.176 no-pxedhcp = 1: our offer already carries the boot file → iPXE requests at once
        // instead of waiting DHCP_DISC_PROXY_TIMEOUT_SEC (2 s) for ProxyDHCP offers.
        p.push(175, [0xb0, 1, 1]);
    }
    p
}

/// Decide the answer to one request. Pure: no I/O, so the whole protocol logic is unit-tested.
/// Handle one DHCP packet (the server is always a full DHCP server now: it hands out IPs + boot info).
/// `from`/`port` are unused (kept for the packet-router signature).
pub fn handle(req: &Packet, from: SocketAddrV4, port: u16, cfg: &Cfg, st: &Store) -> Outcome {
    let _ = (from, port);
    let mut out = Outcome { reply: None, lease: LeaseOp::None, log: String::new(), warn: false };
    let Some(mt) = req.opt(53).and_then(|v| v.first().copied()) else {
        return out;
    };
    if req.op != 1 || req.htype != 1 || req.hlen != 6 {
        return out;
    }
    let mac = mac_str(&req.chaddr);
    // Relayed (giaddr set): from another subnet — this server only serves its own segment (pool, gateway and the
    // broadcast reply are all for this LAN).
    if !req.giaddr.is_unspecified() {
        tracing::debug!("dhcp: ignoring relayed packet from {mac} via {}", req.giaddr);
        return out;
    }
    let k = kind(req);
    let want = req.opt_ip(50).or((!req.ciaddr.is_unspecified()).then_some(req.ciaddr));
    // Option 12 is attacker-controlled (any laptop on the LAN). Keep only NetBIOS-safe characters (letters, digits,
    // '-', max 15) so a hostname can never carry markup/quotes into the admin UI, iPXE scripts or logs.
    let host = req.opt(12).and_then(|h| {
        let s: String = String::from_utf8_lossy(h).chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(15).collect();
        (!s.is_empty()).then_some(s)
    });

    let set = |ip: Ipv4Addr, secs: u64| LeaseOp::Set {
        mac: mac.clone(),
        ip: Some(ip),
        hostname: st.bindings.get(&mac).and_then(|(_, h)| h.clone()).or_else(|| host.clone()),
        expires: st.now + secs,
        source: "full",
    };
    match mt {
        DISCOVER => match pick(&mac, cfg, st) {
            Some(ip) => {
                out.log = format!("client {} ({}) DHCP offer {ip} - mac {mac}", who(&mac, st), k.label());
                if let Some((b, _)) = st.bindings.get(&mac).filter(|(b, _)| *b != ip) {
                    let holder = held_by_other(&mac, *b, st).unwrap_or("?");
                    out.log += &format!(" (its static IP {b} is still leased to {holder} — given a pool IP until that lease ends)");
                    out.warn = true;
                }
                out.reply = Some((full_reply(req, OFFER, ip, &mac, k, cfg, st), Dest::Broadcast));
                // An OFFER hold must NOT shorten a longer lease this MAC already has (else a spoofed DISCOVER frees
                // the victim's IP in 60 s → IP conflict). Keep the later expiry.
                let hold = st.now + OFFER_HOLD_S;
                let keep = st.leases.get(&mac).filter(|(l, _)| *l == ip).map(|(_, e)| *e).unwrap_or(0);
                out.lease = set(ip, hold.max(keep) - st.now);
            }
            None => {
                out.log = format!("client {mac}: no free IP in {}-{}", cfg.start, cfg.end);
                out.warn = true;
            }
        },
        REQUEST => {
            if req.opt_ip(54).is_some_and(|s| s != cfg.server) {
                return out; // client took another server's offer
            }
            match want {
                Some(ip) if allowed(&mac, ip, cfg, st) => {
                    out.log = format!("client {} ({}) got IP {ip} - mac {mac}", who(&mac, st), k.label());
                    let dest = if req.ciaddr.is_unspecified() {
                        Dest::Broadcast
                    } else {
                        Dest::Unicast(SocketAddrV4::new(req.ciaddr, 68))
                    };
                    out.reply = Some((full_reply(req, ACK, ip, &mac, k, cfg, st), dest));
                    out.lease = set(ip, cfg.lease_s as u64);
                }
                // NAK only a client we know (bound, or holding/held a lease here). An unknown MAC asking for an IP
                // without naming a server is renewing/rebooting with ANOTHER DHCP server's lease — stay silent
                // (RFC 2131 4.3.2), never knock it off the LAN.
                Some(ip) if st.bindings.contains_key(&mac) || st.leases.contains_key(&mac) => {
                    out.log = format!("client {mac} asked for {ip}: refused (NAK)");
                    out.warn = true;
                    out.reply = Some((reply(req, NAK, cfg), Dest::Broadcast));
                }
                Some(ip) => tracing::debug!("dhcp: {mac} asked for {ip}, no record of it here → silent"),
                None => {}
            }
        }
        DECLINE => {
            // Only honour a DECLINE for OUR server, for an in-range IP this MAC actually holds. Otherwise anyone
            // could park `declined-<ip>` rows over the whole pool. (opt 54 must be us or absent.)
            let ours = req.opt_ip(54).is_none_or(|s| s == cfg.server);
            let leased = |ip| st.leases.get(&mac).map(|(l, _)| *l) == Some(ip);
            let bound = |ip| st.bindings.get(&mac).map(|(b, _)| *b) == Some(ip);
            match req.opt_ip(50) {
                Some(ip) if ours && in_range(ip, cfg) && leased(ip) && !bound(ip) => {
                    out.log = format!("client {mac} declined {ip} (address in use on the LAN): held for one lease");
                    out.warn = true;
                    out.lease = LeaseOp::Set {
                        mac: format!("declined-{ip}"),
                        ip: Some(ip),
                        hostname: None,
                        expires: st.now + cfg.lease_s as u64,
                        source: "full",
                    };
                }
                // A static IP is not parked (the admin chose it) — but the conflict must be visible.
                Some(ip) if ours && (leased(ip) || bound(ip)) => {
                    out.log = format!("client {} declined {ip}: another device on the LAN uses that address", who(&mac, st));
                    out.warn = true;
                }
                _ => {}
            }
        }
        RELEASE => {
            // Only the holder of the IP (ciaddr == its lease) may release it — a spoofed RELEASE mustn't free
            // someone else's lease.
            if st.leases.get(&mac).map(|(l, _)| *l) == Some(req.ciaddr) && !req.ciaddr.is_unspecified() {
                out.log = format!("client {} released its IP - mac {mac}", who(&mac, st));
                out.lease = LeaseOp::Remove(mac.clone());
            }
        }
        INFORM if !req.ciaddr.is_unspecified() => {
            let mut p = full_reply(req, ACK, Ipv4Addr::UNSPECIFIED, &mac, k, cfg, st);
            p.opts.retain(|(c, _)| ![51, 58, 59].contains(c)); // INFORM: no lease
            out.reply = Some((p, Dest::Unicast(SocketAddrV4::new(req.ciaddr, 68))));
        }
        _ => {}
    }
    out
}

fn apply_lease(db: &dyn Db, op: LeaseOp) {
    let r = match op {
        LeaseOp::None => return,
        LeaseOp::Set { mac, ip, hostname, expires, source } => db.put_lease(&Lease {
            mac,
            ip: ip.map(|i| i.to_string()),
            hostname,
            expires: expires as i64,
            source: source.into(),
        }),
        LeaseOp::Remove(mac) => db.delete_lease(&mac),
    };
    if let Err(e) = r {
        tracing::error!("dhcp: lease write failed: {e}");
    }
}

/// UDP socket bound to `addr`, optionally pinned to one interface (SO_BINDTODEVICE) and allowed to broadcast.
pub(crate) fn udp(addr: SocketAddrV4, iface: &str, broadcast: bool) -> std::io::Result<tokio::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    if broadcast {
        s.set_broadcast(true)?;
    }
    // REUSEPORT: on an Apply the new listener can bind :67/:69 while the old one still runs, so a bad config
    // fails the bind WITHOUT first killing the working listeners (start() stops the old ones only after this).
    s.set_reuse_port(true)?;
    if !iface.is_empty() {
        s.bind_device(Some(iface.as_bytes()))?;
    }
    s.bind(&addr.into())?;
    s.set_nonblocking(true)?;
    tokio::net::UdpSocket::from_std(s.into())
}

async fn serve(sock: tokio::net::UdpSocket, port: u16, st: SharedState) {
    let mut buf = [0u8; 1500];
    loop {
        let (n, from) = match sock.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("dhcp :{port} recv: {e}");
                continue;
            }
        };
        let (SocketAddr::V4(from), Some(req)) = (from, parse(&buf[..n])) else {
            continue;
        };
        let Ok(cfg) = Cfg::load(&*st.db) else { continue };
        let out = handle(&req, from, port, &cfg, &Store::load(&*st.db, now()));
        if !out.log.is_empty() {
            if out.warn {
                tracing::warn!("{}", out.log);
            } else {
                tracing::info!("{}", out.log);
            }
        }
        apply_lease(&*st.db, out.lease);
        if let Some((p, dest)) = out.reply {
            let to = match dest {
                Dest::Broadcast => SocketAddrV4::new(Ipv4Addr::BROADCAST, 68),
                Dest::Unicast(a) => a,
            };
            if let Err(e) = sock.send_to(&build(&p), to).await {
                tracing::warn!("dhcp send to {to}: {e}");
            }
        }
    }
}

/// (Re)start the network boot services: TFTP (:69, iPXE) always, the DHCP server (:67) only when it is on
/// (`dhcp_mode` = "full"). With the DHCP server off, the LAN's own DHCP (a router) points PXE clients here with
/// next-server/option 66 + boot file/option 67, and tftp.rs's autoexec.ipxe sends iPXE on to the boot menu.
/// Stops the previous listeners after the new ones are bound (config change from the web).
pub async fn start(st: &SharedState) -> Result<String, String> {
    let cfg = Cfg::load(&*st.db)?;
    let full = st.db.get_config("dhcp_mode", "off") == "full";
    let bind = |port: u16, bcast: bool| {
        udp(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port), &cfg.iface, bcast).map_err(|e| {
            let hint = if e.kind() == std::io::ErrorKind::AddrInUse { " — another DHCP/TFTP server is running" } else { "" };
            format!("bind udp :{port} on '{}': {e}{hint}", cfg.iface)
        })
    };
    // Bind the NEW listeners first (SO_REUSEPORT lets them share the ports with the old ones). Only once all binds
    // succeed do we stop the old listeners — a rejected config never leaves the café with no DHCP/TFTP.
    let dhcp = if full { Some(bind(67, true)?) } else { None };
    let tftp = bind(69, false)?;
    let old: Vec<_> = std::mem::take(&mut *st.net.lock().unwrap());
    for h in old {
        h.abort();
        let _ = h.await;
    }
    let mut handles = vec![tokio::spawn(crate::tftp::serve(tftp, cfg.server, cfg.iface.clone()))];
    handles.extend(dhcp.map(|d| tokio::spawn(serve(d, 67, st.clone()))));
    st.net.lock().unwrap().extend(handles);
    let on = if cfg.iface.is_empty() { "all interfaces" } else { &cfg.iface };
    Ok(if full {
        format!("DHCP {}-{} on {on} (server {}), TFTP :69", cfg.start, cfg.end, cfg.server)
    } else {
        format!(
            "DHCP server off — TFTP :69 on {on}: set the LAN's DHCP to next-server {} + boot file snponly.efi",
            cfg.server
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0x34, 0x5a, 0x60, 0x7b, 0x2b, 0x1d];

    fn cfg() -> Cfg {
        Cfg {
            iface: String::new(),
            server: Ipv4Addr::new(10, 0, 0, 12),
            start: Ipv4Addr::new(10, 0, 0, 100),
            end: Ipv4Addr::new(10, 0, 0, 102),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            gateway: Some(Ipv4Addr::new(10, 0, 0, 1)),
            dns: vec![Ipv4Addr::new(1, 1, 1, 1)],
            lease_s: 3600,
            sb: false,
        }
    }

    fn store() -> Store {
        Store { bindings: HashMap::new(), leases: HashMap::new(), now: 1000 }
    }

    /// UEFI x64 PXE firmware request (option 60 PXEClient, 93 = 7, 97 GUID).
    fn req(mt: u8, extra: &[(u8, &[u8])]) -> Packet {
        let mut chaddr = [0u8; 16];
        chaddr[..6].copy_from_slice(&MAC);
        let mut p = Packet {
            op: 1,
            htype: 1,
            hlen: 6,
            xid: [1, 2, 3, 4],
            flags: [0x80, 0],
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: Ipv4Addr::UNSPECIFIED,
            chaddr,
            file: String::new(),
            opts: vec![],
        };
        p.push(53, [mt]);
        p.push(60, *b"PXEClient:Arch:00007:UNDI:003016");
        p.push(93, [0, 7]);
        p.push(97, [0; 17]);
        for (c, v) in extra {
            p.push(*c, v.to_vec());
        }
        p
    }

    const FROM: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 68);
    const MACS: &str = "34:5a:60:7b:2b:1d";

    #[test]
    fn roundtrip() {
        let mut p = req(DISCOVER, &[(12, b"pc01")]);
        p.file = "snponly.efi".into();
        let b = build(&p);
        assert!(b.len() >= 300);
        assert_eq!(parse(&b).unwrap(), p);
        assert!(parse(&b[..239]).is_none());
    }

    #[test]
    fn full_offer_first_free_ip_and_pxe_bootfile() {
        let out = handle(&req(DISCOVER, &[]), FROM, 67, &cfg(), &store());
        let (p, dest) = out.reply.unwrap();
        assert_eq!(dest, Dest::Broadcast);
        assert_eq!(p.opt(53), Some(&[OFFER][..]));
        assert_eq!(p.yiaddr, Ipv4Addr::new(10, 0, 0, 100));
        assert_eq!((p.siaddr, p.file.as_str()), (Ipv4Addr::new(10, 0, 0, 12), "snponly.efi"));
        assert_eq!(p.opt(3), Some(&[10, 0, 0, 1][..]));
        assert_eq!(p.opt(51), Some(&3600u32.to_be_bytes()[..]));
        assert!(matches!(out.lease, LeaseOp::Set { expires: 1060, source: "full", .. }));
    }

    #[test]
    fn secure_boot_switch_picks_signed_ipxe() {
        let sb = Cfg { sb: true, ..cfg() };
        let (p, _) = handle(&req(DISCOVER, &[]), FROM, 67, &sb, &store()).reply.unwrap();
        assert_eq!(p.file, "sb/snponly-shim.efi", "UEFI PXE → Microsoft-signed iPXE shim");
        // iPXE itself (option 175) still gets the menu URL, whichever iPXE it is.
        let (p, _) = handle(&req(DISCOVER, &[(175, &[1])]), FROM, 67, &sb, &store()).reply.unwrap();
        assert!(p.file.starts_with("http://10.0.0.12/boot.ipxe"));
    }

    #[test]
    fn ipxe_gets_menu_url() {
        let out = handle(&req(DISCOVER, &[(175, &[1])]), FROM, 67, &cfg(), &store());
        let p = out.reply.unwrap().0;
        assert_eq!(p.file, "http://10.0.0.12/boot.ipxe?mac=${net0/mac}&ip=${net0/ip}");
        assert_eq!(p.opt(175), Some(&[0xb0, 1, 1][..])); // no-pxedhcp: don't wait for ProxyDHCP
        let plain = handle(&req(DISCOVER, &[]), FROM, 67, &cfg(), &store()).reply.unwrap().0;
        assert_eq!(plain.opt(175), None);
    }

    #[test]
    fn binding_wins_and_sends_hostname() {
        let mut st = store();
        st.bindings.insert(MACS.into(), (Ipv4Addr::new(10, 0, 0, 50), Some("PC01".into())));
        let (p, _) = handle(&req(DISCOVER, &[]), FROM, 67, &cfg(), &st).reply.unwrap();
        assert_eq!(p.yiaddr, Ipv4Addr::new(10, 0, 0, 50));
        assert_eq!(p.opt(12), Some(&b"PC01"[..]));
    }

    #[test]
    fn skips_ips_in_use() {
        let mut st = store();
        st.leases.insert("aa:aa:aa:aa:aa:aa".into(), (Ipv4Addr::new(10, 0, 0, 100), 2000)); // active
        st.bindings.insert("bb:bb:bb:bb:bb:bb".into(), (Ipv4Addr::new(10, 0, 0, 101), None));
        let (p, _) = handle(&req(DISCOVER, &[]), FROM, 67, &cfg(), &st).reply.unwrap();
        assert_eq!(p.yiaddr, Ipv4Addr::new(10, 0, 0, 102));
        // Expired lease of another machine → its IP is free again.
        st.leases.insert("aa:aa:aa:aa:aa:aa".into(), (Ipv4Addr::new(10, 0, 0, 100), 500));
        let (p, _) = handle(&req(DISCOVER, &[]), FROM, 67, &cfg(), &st).reply.unwrap();
        assert_eq!(p.yiaddr, Ipv4Addr::new(10, 0, 0, 100));
        // Pool full → no offer.
        let mut st = store();
        for (i, m) in ["a", "b", "c"].iter().enumerate() {
            st.leases.insert(m.to_string(), (Ipv4Addr::new(10, 0, 0, 100 + i as u8), 2000));
        }
        assert!(handle(&req(DISCOVER, &[]), FROM, 67, &cfg(), &st).reply.is_none());
    }

    #[test]
    fn decline_release_discover_validated() {
        let ip = Ipv4Addr::new(10, 0, 0, 101); // in the 100..=102 pool
        let held = || {
            let mut st = store();
            st.leases.insert(MACS.into(), (ip, 5000));
            st
        };
        // DECLINE only when this MAC holds the in-range IP (and opt 54 is us or absent).
        assert!(matches!(handle(&req(DECLINE, &[(50, &ip.octets())]), FROM, 67, &cfg(), &store()).lease, LeaseOp::None));
        assert!(matches!(handle(&req(DECLINE, &[(50, &ip.octets())]), FROM, 67, &cfg(), &held()).lease, LeaseOp::Set { .. }));
        let outrange = Ipv4Addr::new(192, 168, 1, 1);
        let mut st = store();
        st.leases.insert(MACS.into(), (outrange, 5000));
        assert!(matches!(handle(&req(DECLINE, &[(50, &outrange.octets())]), FROM, 67, &cfg(), &st).lease, LeaseOp::None));
        // RELEASE only when ciaddr == the held IP.
        let mut r = req(RELEASE, &[]);
        r.ciaddr = ip;
        assert!(matches!(handle(&r, FROM, 67, &cfg(), &held()).lease, LeaseOp::Remove(_)));
        let mut r2 = req(RELEASE, &[]);
        r2.ciaddr = Ipv4Addr::new(10, 0, 0, 200);
        assert!(matches!(handle(&r2, FROM, 67, &cfg(), &held()).lease, LeaseOp::None));
        // DISCOVER must not shorten a longer lease this MAC already has.
        let mut st = store();
        st.leases.insert(MACS.into(), (ip, 9000));
        assert!(matches!(handle(&req(DISCOVER, &[]), FROM, 67, &cfg(), &st).lease, LeaseOp::Set { expires: 9000, .. }));
    }

    #[test]
    fn request_ack_nak_and_other_server() {
        let ok = handle(&req(REQUEST, &[(50, &[10, 0, 0, 101])]), FROM, 67, &cfg(), &store());
        let (p, _) = ok.reply.unwrap();
        assert_eq!((p.opt(53), p.yiaddr), (Some(&[ACK][..]), Ipv4Addr::new(10, 0, 0, 101)));
        assert!(matches!(ok.lease, LeaseOp::Set { expires: 4600, .. }));
        // A client we know (it has a lease here) asking for a wrong IP → NAK.
        let mut known = store();
        known.leases.insert(MACS.into(), (Ipv4Addr::new(10, 0, 0, 100), 5000));
        let bad = handle(&req(REQUEST, &[(50, &[192, 168, 1, 5])]), FROM, 67, &cfg(), &known);
        assert_eq!(bad.reply.unwrap().0.opt(53), Some(&[NAK][..]));
        let other = handle(&req(REQUEST, &[(50, &[10, 0, 0, 101]), (54, &[10, 0, 0, 9])]), FROM, 67, &cfg(), &store());
        assert!(other.reply.is_none());
    }

    /// A REQUEST without option 54 for an IP outside our pool, from a MAC we have no record of = a client renewing
    /// with ANOTHER DHCP server → never NAK it off the LAN.
    #[test]
    fn foreign_client_is_not_naked() {
        let out = handle(&req(REQUEST, &[(50, &[192, 168, 1, 5])]), FROM, 67, &cfg(), &store());
        assert!(out.reply.is_none() && matches!(out.lease, LeaseOp::None));
        let mut renewing = req(REQUEST, &[]);
        renewing.ciaddr = Ipv4Addr::new(192, 168, 1, 5);
        assert!(handle(&renewing, FROM, 67, &cfg(), &store()).reply.is_none());
    }

    /// Static IP still leased to another MAC → the bound machine gets a pool IP for now (warned), not a duplicate.
    #[test]
    fn binding_waits_for_live_lease() {
        let mut st = store();
        let bound = Ipv4Addr::new(10, 0, 0, 50);
        st.bindings.insert(MACS.into(), (bound, Some("PC01".into())));
        st.leases.insert("aa:aa:aa:aa:aa:aa".into(), (bound, 2000)); // live
        let out = handle(&req(DISCOVER, &[]), FROM, 67, &cfg(), &st);
        assert_eq!(out.reply.unwrap().0.yiaddr, Ipv4Addr::new(10, 0, 0, 100));
        assert!(out.warn && out.log.contains("still leased to aa:aa:aa:aa:aa:aa"));
        assert!(!allowed(MACS, bound, &cfg(), &st), "no ACK for the held static IP");
        st.leases.insert("aa:aa:aa:aa:aa:aa".into(), (bound, 500)); // expired
        assert_eq!(handle(&req(DISCOVER, &[]), FROM, 67, &cfg(), &st).reply.unwrap().0.yiaddr, bound);
        assert!(!allowed(MACS, Ipv4Addr::new(10, 0, 0, 100), &cfg(), &st), "once free: only the binding");
    }

    #[test]
    fn decline_of_static_ip_is_logged_not_parked() {
        let mut st = store();
        let bound = Ipv4Addr::new(10, 0, 0, 50);
        st.bindings.insert(MACS.into(), (bound, Some("PC01".into())));
        let out = handle(&req(DECLINE, &[(50, &bound.octets())]), FROM, 67, &cfg(), &st);
        assert!(out.warn && out.log.contains("PC01 declined 10.0.0.50") && matches!(out.lease, LeaseOp::None));
    }

    #[test]
    fn relayed_packets_ignored() {
        let mut r = req(DISCOVER, &[]);
        r.giaddr = Ipv4Addr::new(10, 9, 0, 1);
        let out = handle(&r, FROM, 67, &cfg(), &store());
        assert!(out.reply.is_none() && matches!(out.lease, LeaseOp::None));
    }

    #[test]
    fn lease_parse() {
        assert_eq!((lease_secs("12h"), lease_secs("30m"), lease_secs("1d"), lease_secs("600")), (43200, 1800, 86400, 600));
        assert_eq!(lease_secs("junk"), 43200);
    }
}
