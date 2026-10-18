// dhcp.rs — built-in DHCP server for UEFI PXE + iPXE, on only when `dhcp_mode` = "full" (Network page; off by default:
// a router usually owns the LAN, then it points PXE clients at our TFTP — see start()). When on, it is the LAN's DHCP
// server: hands out IPs (Machines-table binding > previous lease > first free in range) + gateway/DNS + the boot file.
// Optional behaviours (Rapid Commit, iPXE no-ProxyDHCP wait, authoritative NAK, hostname) are Network-page checkboxes
// (DhcpOpts). No ProxyDHCP, relay or DHCPv6. Every packet reads config/bindings/leases from the DB → edits apply at
// once; start() rebinds the sockets. Leases live in the `leases` table.
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use crate::SharedState;
use crate::db::{Db, Lease};
use crate::now_secs as now;

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
        self.opts
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, v)| v.as_slice())
    }
    fn opt_ip(&self, code: u8) -> Option<Ipv4Addr> {
        self.opt(code)
            .filter(|v| v.len() == 4)
            .map(|v| Ipv4Addr::new(v[0], v[1], v[2], v[3]))
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
    for (o, ip) in [
        (12, p.ciaddr),
        (16, p.yiaddr),
        (20, p.siaddr),
        (24, p.giaddr),
    ] {
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
    chaddr[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
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
    /// Optional behaviours (checkboxes on the Network page).
    pub opts: DhcpOpts,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DhcpOpts {
    /// RFC 4039: a DISCOVER asking for it (option 80) gets the ACK at once, no OFFER/REQUEST round. Off by default
    /// (the RFC wants it explicitly enabled); iPXE/UEFI PXE never ask, some Linux clients (systemd-networkd) do.
    pub rapid_commit: bool,
    /// Option 175.176 no-pxedhcp: iPXE takes our answer at once instead of waiting 2 s for ProxyDHCP offers.
    pub ipxe_fast: bool,
    /// NAK a known client asking for an IP it may not have (it gets a new one at once). Off → stay silent.
    pub authoritative: bool,
    /// Option 12: the hostname from the Machines table.
    pub send_hostname: bool,
}

impl Default for DhcpOpts {
    fn default() -> Self {
        DhcpOpts {
            rapid_commit: false,
            ipxe_fast: true,
            authoritative: true,
            send_hostname: true,
        }
    }
}

impl DhcpOpts {
    /// (config key, default) of each option, as stored by the Network page.
    pub const KEYS: [(&'static str, &'static str); 4] = [
        ("dhcp_rapid_commit", "0"),
        ("dhcp_ipxe_fast", "1"),
        ("dhcp_authoritative", "1"),
        ("dhcp_send_hostname", "1"),
    ];

    fn load(db: &dyn Db) -> Self {
        let on = |i: usize| db.get_config(Self::KEYS[i].0, Self::KEYS[i].1) == "1";
        DhcpOpts {
            rapid_commit: on(0),
            ipxe_fast: on(1),
            authoritative: on(2),
            send_hostname: on(3),
        }
    }
}

/// "12h" / "30m" / "1d" / "90s" / "3600" → seconds; None = not a lease time (settings.rs refuses it).
pub(crate) fn parse_lease(s: &str) -> Option<u32> {
    let s = s.trim();
    let (n, mul) = match s.chars().last() {
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('d') => (&s[..s.len() - 1], 86400),
        Some('s') => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    n.parse::<u32>()
        .ok()
        .filter(|&v| v > 0)
        .map(|v| v.saturating_mul(mul))
}

/// The lease time in seconds, 12h when unset / invalid.
fn lease_secs(s: &str) -> u32 {
    parse_lease(s).unwrap_or(43200)
}

impl Cfg {
    pub fn load(db: &dyn Db) -> Result<Cfg, String> {
        let g = |k: &str, d: &str| db.get_config(k, d);
        let ip = |k: &str| g(k, "").trim().parse::<Ipv4Addr>().ok();
        let server =
            ip("dhcp_server_ip").ok_or("dhcp_server_ip is not set — run setup (start as root)")?;
        // Range default: .100–.200 of the subnet.
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
            // The built-in DNS server on (dns.rs): it is the clients' only DNS — a second one would answer what it
            // refuses. Off: the Network page's list.
            dns: if g("dns_mode", "off") == "on" {
                vec![server]
            } else {
                g("dhcp_dns", "")
                    .split([',', ' '])
                    .filter_map(|s| s.trim().parse().ok())
                    .collect()
            },
            lease_s: lease_secs(&g("dhcp_lease", "12h")),
            sb: g("ipxe_signed", "0") == "1",
            opts: DhcpOpts::load(db),
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
        Store {
            bindings,
            leases,
            now,
        }
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
    Set {
        mac: String,
        ip: Option<Ipv4Addr>,
        hostname: Option<String>,
        expires: u64,
        source: &'static str,
    },
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
    st.bindings
        .get(mac)
        .and_then(|(_, h)| h.clone())
        .unwrap_or_else(|| mac.to_string())
}

fn kind(req: &Packet) -> Kind {
    let ipxe = req.opt(175).is_some()
        || req
            .opt(77)
            .is_some_and(|v| v.windows(4).any(|w| w == b"iPXE"));
    if ipxe {
        return Kind::Ipxe;
    }
    let pxe = req.opt(60).is_some_and(|v| v.starts_with(b"PXEClient"));
    let arch = req
        .opt(93)
        .filter(|v| v.len() >= 2)
        .map(|v| u16::from_be_bytes([v[0], v[1]]));
    match (pxe, arch) {
        (true, Some(7 | 9)) => Kind::PxeEfi,
        (true, _) => Kind::PxeOther,
        _ => Kind::Plain,
    }
}

/// Boot file for the client: iPXE → our dynamic menu URL; UEFI PXE firmware → snponly.efi over TFTP.
fn boot_file(k: Kind, cfg: &Cfg) -> Option<String> {
    match k {
        Kind::Ipxe => Some(format!(
            "http://{}/boot.ipxe?mac=${{net0/mac}}&ip=${{net0/ip}}",
            cfg.server
        )),
        // Secure Boot: the iPXE shim (Microsoft-signed) then loads sb/snponly.efi (iPXE-signed) by name itself.
        Kind::PxeEfi => Some(
            if cfg.sb {
                "sb/snponly-shim.efi"
            } else {
                "snponly.efi"
            }
            .into(),
        ),
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
    st.leases
        .iter()
        .find(|(m, (l, exp))| m.as_str() != mac && *l == ip && *exp > st.now)
        .map(|(m, _)| m.as_str())
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
    if let Some((l, _)) = st.leases.get(mac)
        && allowed(mac, *l, cfg, st)
    {
        return Some(*l);
    }
    (u32::from(cfg.start)..=u32::from(cfg.end))
        .map(Ipv4Addr::from)
        .find(|&ip| allowed(mac, ip, cfg, st))
}

/// Full-mode OFFER/ACK body: IP + network options + boot file.
fn full_reply(
    req: &Packet,
    mt: u8,
    ip: Ipv4Addr,
    mac: &str,
    k: Kind,
    cfg: &Cfg,
    st: &Store,
) -> Packet {
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
        p.push(
            6,
            cfg.dns.iter().flat_map(|d| d.octets()).collect::<Vec<_>>(),
        );
    }
    if let Some((_, Some(h))) = st.bindings.get(mac).filter(|_| cfg.opts.send_hostname) {
        p.push(12, h.as_bytes().to_vec());
    }
    if let Some(f) = boot_file(k, cfg) {
        p.file = f;
    }
    if k == Kind::Ipxe && cfg.opts.ipxe_fast {
        // iPXE option 175.176 no-pxedhcp = 1 (checkbox "iPXE: don't wait for ProxyDHCP"): our offer already carries the
        // boot file → iPXE requests at once instead of waiting DHCP_DISC_PROXY_TIMEOUT_SEC (2 s) for ProxyDHCP offers.
        p.push(175, [0xb0, 1, 1]);
    }
    p
}

/// Decide the answer to one request. Pure (no sockets, no DB; debug logging only), so the protocol logic is unit-tested.
/// Handle one DHCP packet: hand out IPs + boot info.
pub fn handle(req: &Packet, cfg: &Cfg, st: &Store) -> Outcome {
    let mut out = Outcome {
        reply: None,
        lease: LeaseOp::None,
        log: String::new(),
        warn: false,
    };
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
        tracing::debug!(
            "dhcp: ignoring relayed packet from {mac} via {}",
            req.giaddr
        );
        return out;
    }
    let k = kind(req);
    let want = req
        .opt_ip(50)
        .or((!req.ciaddr.is_unspecified()).then_some(req.ciaddr));
    // Option 12 is attacker-controlled (any laptop on the LAN). Keep only NetBIOS-safe characters (letters, digits,
    // '-', max 15) so a hostname can never carry markup/quotes into the admin UI, iPXE scripts or logs.
    let host = req.opt(12).and_then(|h| {
        let s: String = String::from_utf8_lossy(h)
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(15)
            .collect();
        (!s.is_empty()).then_some(s)
    });

    let set = |ip: Ipv4Addr, secs: u64| LeaseOp::Set {
        mac: mac.clone(),
        ip: Some(ip),
        hostname: st
            .bindings
            .get(&mac)
            .and_then(|(_, h)| h.clone())
            .or_else(|| host.clone()),
        expires: st.now + secs,
        source: "full",
    };
    match mt {
        DISCOVER => match pick(&mac, cfg, st) {
            Some(ip) => {
                out.log = format!(
                    "client {} ({}) DHCP offer {ip} - mac {mac}",
                    who(&mac, st),
                    k.label()
                );
                if let Some((b, _)) = st.bindings.get(&mac).filter(|(b, _)| *b != ip) {
                    let holder = held_by_other(&mac, *b, st).unwrap_or("?");
                    out.log += &format!(
                        " (its static IP {b} is still leased to {holder} — given a pool IP until that lease ends)"
                    );
                    out.warn = true;
                }
                if cfg.opts.rapid_commit && req.opt(80).is_some() {
                    // Rapid Commit: the client asked to skip OFFER/REQUEST → straight ACK (with option 80) + full lease.
                    out.log = out.log.replacen("DHCP offer", "got IP", 1) + " (rapid commit)";
                    let mut p = full_reply(req, ACK, ip, &mac, k, cfg, st);
                    p.push(80, Vec::new());
                    out.reply = Some((p, Dest::Broadcast));
                    out.lease = set(ip, cfg.lease_s as u64);
                    return out;
                }
                out.reply = Some((
                    full_reply(req, OFFER, ip, &mac, k, cfg, st),
                    Dest::Broadcast,
                ));
                // An OFFER hold must NOT shorten a longer lease this MAC already has (else a spoofed DISCOVER frees
                // the victim's IP in 60 s → IP conflict). Keep the later expiry.
                let hold = st.now + OFFER_HOLD_S;
                let keep = st
                    .leases
                    .get(&mac)
                    .filter(|(l, _)| *l == ip)
                    .map(|(_, e)| *e)
                    .unwrap_or(0);
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
                    out.log = format!(
                        "client {} ({}) got IP {ip} - mac {mac}",
                        who(&mac, st),
                        k.label()
                    );
                    let dest = if req.ciaddr.is_unspecified() {
                        Dest::Broadcast
                    } else {
                        Dest::Unicast(SocketAddrV4::new(req.ciaddr, 68))
                    };
                    out.reply = Some((full_reply(req, ACK, ip, &mac, k, cfg, st), dest));
                    out.lease = set(ip, cfg.lease_s as u64);
                }
                // NAK only a client we know (bound, or holding/held a lease here), and only when authoritative. An
                // unknown MAC asking for an IP without naming a server is renewing/rebooting with ANOTHER DHCP
                // server's lease — stay silent (RFC 2131 4.3.2), never knock it off the LAN.
                Some(ip)
                    if cfg.opts.authoritative
                        && (st.bindings.contains_key(&mac) || st.leases.contains_key(&mac)) =>
                {
                    out.log = format!("client {mac} asked for {ip}: refused (NAK)");
                    out.warn = true;
                    out.reply = Some((reply(req, NAK, cfg), Dest::Broadcast));
                }
                Some(ip) => {
                    tracing::debug!("dhcp: {mac} asked for {ip}, not ours to give → silent")
                }
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
                    out.log = format!(
                        "client {mac} declined {ip} (address in use on the LAN): held for one lease"
                    );
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
                    out.log = format!(
                        "client {} declined {ip}: another device on the LAN uses that address",
                        who(&mac, st)
                    );
                    out.warn = true;
                }
                _ => {}
            }
        }
        RELEASE => {
            // Only the holder of the IP (ciaddr == its lease) may release it — a spoofed RELEASE mustn't free
            // someone else's lease.
            if st.leases.get(&mac).map(|(l, _)| *l) == Some(req.ciaddr)
                && !req.ciaddr.is_unspecified()
            {
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
        LeaseOp::Set {
            mac,
            ip,
            hostname,
            expires,
            source,
        } => db.put_lease(&Lease {
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
pub(crate) fn udp(
    addr: SocketAddrV4,
    iface: &str,
    broadcast: bool,
) -> std::io::Result<tokio::net::UdpSocket> {
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
        let (SocketAddr::V4(_), Some(req)) = (from, parse(&buf[..n])) else {
            continue;
        };
        // The SQLite reads/writes of one packet run on a blocking thread, not on a runtime worker: a DHCP flood then
        // queues here instead of stalling the web, TFTP and every other task.
        let st2 = st.clone();
        let reply = tokio::task::spawn_blocking(move || {
            let cfg = Cfg::load(&*st2.db).ok()?;
            let out = handle(&req, &cfg, &Store::load(&*st2.db, now()));
            if !out.log.is_empty() {
                if out.warn {
                    tracing::warn!("{}", out.log);
                } else {
                    tracing::info!("{}", out.log);
                }
            }
            apply_lease(&*st2.db, out.lease);
            out.reply
        })
        .await
        .ok()
        .flatten();
        if let Some((p, dest)) = reply {
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
        udp(
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port),
            &cfg.iface,
            bcast,
        )
        .map_err(|e| {
            let hint = if e.kind() == std::io::ErrorKind::AddrInUse {
                " — another DHCP/TFTP server is running"
            } else {
                ""
            };
            format!("bind udp :{port} on '{}': {e}{hint}", cfg.iface)
        })
    };
    // Bind the NEW listeners first (SO_REUSEPORT lets them share the ports with the old ones). Only once all binds
    // succeed do we stop the old listeners — a rejected config never leaves the café with no DHCP/TFTP.
    let tftp = bind(69, false)?;
    let dhcp = match full.then(|| bind(67, true)).transpose() {
        Ok(d) => d,
        Err(e) => {
            // Nothing running yet (server start): TFTP alone still serves iPXE to a LAN whose router hands out the
            // PXE options — better than no network boot at all. On an Apply the old listeners simply stay.
            let mut net = st.net.lock();
            if net.is_empty() {
                net.push(tokio::spawn(crate::tftp::serve(
                    tftp,
                    cfg.server,
                    cfg.iface.clone(),
                )));
                return Err(format!("{e} — DHCP server NOT running, TFTP :69 is"));
            }
            return Err(e);
        }
    };
    let mut handles = vec![tokio::spawn(crate::tftp::serve(
        tftp,
        cfg.server,
        cfg.iface.clone(),
    ))];
    handles.extend(dhcp.map(|d| tokio::spawn(serve(d, 67, st.clone()))));
    // Swap in ONE step: whatever was running (also the listeners of an Apply that ran at the same time) is stopped,
    // so two Applies can never leave an extra listener behind with an old config.
    let old = std::mem::replace(&mut *st.net.lock(), handles);
    for h in old {
        h.abort();
        let _ = h.await;
    }
    let on = if cfg.iface.is_empty() {
        "all interfaces"
    } else {
        &cfg.iface
    };
    // The DNS server follows (its settings, the server IP, the resolvers list).
    match crate::dns::restart(st).await {
        Ok(m) if crate::dns::active() => tracing::info!("{m}"),
        Ok(_) => {}
        Err(e) => tracing::error!("{e}"),
    }
    Ok(if full {
        format!(
            "DHCP {}-{} on {on} (server {}), TFTP :69",
            cfg.start, cfg.end, cfg.server
        )
    } else {
        format!(
            "DHCP server off — TFTP :69 on {on}: set the LAN's DHCP to next-server {} + boot file snponly.efi",
            cfg.server
        )
    })
}

#[cfg(test)]
#[path = "../tests/unit/dhcp.rs"]
mod tests;
