// Minimal clients written from the protocols, not from broom's code: what a PC's firmware, iPXE and the admin's browser
// send. Each panics on anything a real client would choke on.
use super::{CLI_IF, SRV};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------- HTTP/1.1

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One request per connection (Connection: close), session cookie kept by hand.
#[derive(Default)]
pub struct Http {
    pub cookie: Option<String>,
    /// The session key of the sign-in handshake (new passwords are sealed with it).
    pub sk: Option<[u8; 32]>,
}

impl Http {
    pub fn new() -> Http {
        Http::default()
    }

    pub fn get(&self, path: &str) -> Resp {
        self.request("GET", path, &[], b"")
    }

    pub fn post(&self, path: &str, body: &serde_json::Value) -> Resp {
        self.request(
            "POST",
            path,
            &[("Content-Type", "application/json")],
            body.to_string().as_bytes(),
        )
    }

    pub fn put(&self, path: &str, body: &[u8]) -> Resp {
        self.request(
            "PUT",
            path,
            &[("Content-Type", "application/octet-stream")],
            body,
        )
    }

    /// Keep the session from a Set-Cookie answer.
    pub fn take_cookie(&mut self, r: &Resp) {
        let c = r.header("set-cookie").expect("Set-Cookie");
        self.cookie = Some(c.split(';').next().unwrap().to_string());
    }

    pub fn request(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
        let mut s =
            TcpStream::connect((SRV, 80)).unwrap_or_else(|e| panic!("connect {SRV}:80: {e}"));
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            req += &format!("Host: {SRV}\r\n");
        }
        if let Some(c) = &self.cookie {
            req += &format!("Cookie: {c}\r\n");
        }
        for (k, v) in headers {
            req += &format!("{k}: {v}\r\n");
        }
        req += "\r\n";
        s.write_all(req.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).unwrap();
        parse_response(&raw)
    }
}

fn parse_response(raw: &[u8]) -> Resp {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("HTTP header end");
    let head = String::from_utf8_lossy(&raw[..end]);
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let mut body = raw[end + 4..].to_vec();
    let chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")
    });
    if chunked {
        body = dechunk(&body);
    }
    Resp {
        status,
        headers,
        body,
    }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let eol = b
            .windows(2)
            .position(|w| w == b"\r\n")
            .expect("chunk size line");
        let size = usize::from_str_radix(
            String::from_utf8_lossy(&b[..eol])
                .split(';')
                .next()
                .unwrap()
                .trim(),
            16,
        )
        .expect("chunk size");
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&b[eol + 2..eol + 2 + size]);
        b = &b[eol + 2 + size + 2..];
    }
}

// ---------------------------------------------------------------- DHCP (RFC 2131)

pub const DISCOVER: u8 = 1;
pub const OFFER: u8 = 2;
pub const REQUEST: u8 = 3;
pub const ACK: u8 = 5;

pub struct DhcpReply {
    pub yiaddr: Ipv4Addr,
    pub siaddr: Ipv4Addr,
    pub file: String,
    pub opts: Vec<(u8, Vec<u8>)>,
}

impl DhcpReply {
    pub fn opt(&self, code: u8) -> Option<&[u8]> {
        self.opts
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, v)| v.as_slice())
    }

    pub fn msg_type(&self) -> u8 {
        self.opt(53).expect("option 53")[0]
    }
}

/// Client port 68 on cli0, broadcasting like a NIC without an address yet.
pub struct Dhcp {
    sock: UdpSocket,
}

impl Dhcp {
    pub fn open() -> Dhcp {
        use socket2::{Domain, Protocol, Socket, Type};
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        s.set_broadcast(true).unwrap();
        s.bind_device(Some(CLI_IF.as_bytes())).unwrap();
        s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 68)).into())
            .unwrap();
        let sock: UdpSocket = s.into();
        sock.set_read_timeout(Some(TIMEOUT)).unwrap();
        Dhcp { sock }
    }

    /// Broadcast one message, wait for the server's answer to the same xid.
    pub fn exchange(&self, mt: u8, mac: [u8; 6], xid: u32, opts: &[(u8, &[u8])]) -> DhcpReply {
        let mut p = vec![0u8; 236];
        p[..3].copy_from_slice(&[1, 1, 6]); // BOOTREQUEST, Ethernet, 6-byte MAC
        p[4..8].copy_from_slice(&xid.to_be_bytes());
        p[10] = 0x80; // broadcast flag: no IP to unicast to yet
        p[28..34].copy_from_slice(&mac);
        p.extend([99, 130, 83, 99, 53, 1, mt]);
        for (c, v) in opts {
            p.push(*c);
            p.push(v.len() as u8);
            p.extend_from_slice(v);
        }
        p.push(255);
        p.resize(p.len().max(300), 0);
        self.sock.send_to(&p, (Ipv4Addr::BROADCAST, 67)).unwrap();
        let mut buf = [0u8; 1500];
        loop {
            let n = self
                .sock
                .recv(&mut buf)
                .unwrap_or_else(|e| panic!("no DHCP answer to type {mt}: {e}"));
            let b = &buf[..n];
            if n < 240
                || b[0] != 2
                || b[4..8] != xid.to_be_bytes()
                || b[236..240] != [99, 130, 83, 99]
            {
                continue;
            }
            let ip = |o: usize| Ipv4Addr::new(b[o], b[o + 1], b[o + 2], b[o + 3]);
            let mut opts = Vec::new();
            let mut i = 240;
            while i < n && b[i] != 255 {
                if b[i] == 0 {
                    i += 1;
                    continue;
                }
                let len = b[i + 1] as usize;
                opts.push((b[i], b[i + 2..i + 2 + len].to_vec()));
                i += 2 + len;
            }
            let file_end = b[108..236].iter().position(|&c| c == 0).unwrap_or(128);
            return DhcpReply {
                yiaddr: ip(16),
                siaddr: ip(20),
                file: String::from_utf8_lossy(&b[108..108 + file_end]).into_owned(),
                opts,
            };
        }
    }
}

// ---------------------------------------------------------------- TFTP (RFC 1350 + blksize)

/// Download `file` from the server's TFTP with 1468-byte blocks, as UEFI PXE does. Err = the server's ERROR message.
pub fn tftp_get(file: &str) -> Result<Vec<u8>, String> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    sock.set_read_timeout(Some(TIMEOUT)).unwrap();
    let mut rrq = vec![0, 1];
    for s in [file, "octet", "blksize", "1468"] {
        rrq.extend(s.as_bytes());
        rrq.push(0);
    }
    sock.send_to(&rrq, (SRV, 69)).unwrap();
    let (mut blksize, mut next, mut out) = (512usize, 1u16, Vec::new());
    let mut buf = [0u8; 2048];
    loop {
        let (n, peer) = sock
            .recv_from(&mut buf)
            .map_err(|e| format!("TFTP timeout: {e}"))?;
        let b = &buf[..n];
        match u16::from_be_bytes([b[0], b[1]]) {
            6 => {
                // OACK: the transfer's own port answers; take the agreed block size, ACK block 0.
                let f: Vec<_> = b[2..]
                    .split(|&c| c == 0)
                    .map(|s| String::from_utf8_lossy(s).to_lowercase())
                    .collect();
                if let Some(i) = f.iter().position(|k| k == "blksize") {
                    blksize = f[i + 1].parse().unwrap();
                }
                sock.send_to(&[0, 4, 0, 0], peer).unwrap();
            }
            3 => {
                let block = u16::from_be_bytes([b[2], b[3]]);
                if block == next {
                    out.extend_from_slice(&b[4..]);
                    next = next.wrapping_add(1);
                }
                sock.send_to(&[0, 4, b[2], b[3]], peer).unwrap();
                if block == next.wrapping_sub(1) && n - 4 < blksize {
                    return Ok(out);
                }
            }
            5 => return Err(String::from_utf8_lossy(&b[4..n.saturating_sub(1)]).into_owned()),
            op => return Err(format!("unexpected TFTP opcode {op}")),
        }
    }
}

// ---------------------------------------------------------------- iSCSI (RFC 7143), read-only initiator

pub struct Iscsi {
    s: TcpStream,
    itt: u32,
    cmdsn: u32,
}

fn read_pdu(s: &mut TcpStream) -> ([u8; 48], Vec<u8>) {
    let mut bhs = [0u8; 48];
    s.read_exact(&mut bhs).expect("iSCSI PDU header");
    let ahs = bhs[4] as usize * 4;
    let dlen = u32::from_be_bytes([0, bhs[5], bhs[6], bhs[7]]) as usize;
    let mut rest = vec![0u8; ahs + dlen.div_ceil(4) * 4];
    s.read_exact(&mut rest).expect("iSCSI PDU data");
    (bhs, rest[ahs..ahs + dlen].to_vec())
}

impl Iscsi {
    /// Log in to `target` on port 3260, straight to full feature phase. Err((status class, detail)) when refused.
    pub fn login(target: &str) -> Result<Iscsi, (u8, u8)> {
        let s =
            TcpStream::connect((SRV, 3260)).unwrap_or_else(|e| panic!("connect {SRV}:3260: {e}"));
        Iscsi::login_on(s, target)
    }

    /// Logged in, or None (nothing listening yet, or refused).
    pub fn try_login(target: &str) -> Option<Iscsi> {
        Iscsi::login_on(TcpStream::connect((SRV, 3260)).ok()?, target).ok()
    }

    fn login_on(mut s: TcpStream, target: &str) -> Result<Iscsi, (u8, u8)> {
        s.set_read_timeout(Some(TIMEOUT)).unwrap();
        let keys = format!(
            "InitiatorName=iqn.2024-01.test:it\0TargetName={target}\0SessionType=Normal\0\
             MaxRecvDataSegmentLength=65536\0HeaderDigest=None\0DataDigest=None\0"
        );
        let mut p = vec![0u8; 48];
        p[0] = 0x43; // Login Request, immediate
        p[1] = 0x80 | 1 << 2 | 3; // Transit, operational → full feature
        p[5..8].copy_from_slice(&(keys.len() as u32).to_be_bytes()[1..]);
        // ISID: one per session — a login repeating the name + ISID replaces that session (the target kicks the old one).
        static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(1);
        let n = NEXT
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_be_bytes();
        p[8..14].copy_from_slice(&[0x80, 0, 0, 0xab, n[0], n[1]]);
        p[16..20].copy_from_slice(&1u32.to_be_bytes()); // ITT
        p.extend(keys.as_bytes());
        p.resize(p.len().div_ceil(4) * 4, 0);
        s.write_all(&p).unwrap();
        let (bhs, _) = read_pdu(&mut s);
        assert_eq!(bhs[0] & 0x3f, 0x23, "Login Response");
        if bhs[36] != 0 {
            return Err((bhs[36], bhs[37]));
        }
        Ok(Iscsi {
            s,
            itt: 1,
            cmdsn: 1,
        })
    }

    /// One SCSI command that reads up to `len` bytes → the data (status GOOD checked).
    fn data_in(&mut self, cdb: &[u8], len: u32) -> Vec<u8> {
        self.itt += 1;
        let mut p = vec![0u8; 48];
        p[0] = 0x01; // SCSI Command
        p[1] = 0xc0; // Final + Read
        p[16..20].copy_from_slice(&self.itt.to_be_bytes());
        p[20..24].copy_from_slice(&len.to_be_bytes());
        p[24..28].copy_from_slice(&self.cmdsn.to_be_bytes());
        p[32..32 + cdb.len()].copy_from_slice(cdb);
        self.cmdsn += 1;
        self.s.write_all(&p).unwrap();
        let mut out = vec![0u8; len as usize];
        loop {
            let (bhs, data) = read_pdu(&mut self.s);
            match bhs[0] & 0x3f {
                0x25 => {
                    let at = u32::from_be_bytes(bhs[40..44].try_into().unwrap()) as usize;
                    out[at..at + data.len()].copy_from_slice(&data);
                    if bhs[1] & 0x01 != 0 {
                        assert_eq!(bhs[3], 0, "SCSI status GOOD");
                        return out;
                    }
                }
                0x21 => {
                    assert_eq!(bhs[3], 0, "SCSI status GOOD");
                    return out;
                }
                op => panic!("unexpected iSCSI opcode {op:#x}"),
            }
        }
    }

    /// READ(10) of `blocks` 512-byte blocks at `lba`.
    pub fn read(&mut self, lba: u32, blocks: u16) -> Vec<u8> {
        let mut cdb = [0u8; 10];
        cdb[0] = 0x28;
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&blocks.to_be_bytes());
        self.data_in(&cdb, blocks as u32 * 512)
    }

    /// READ CAPACITY(10) → disk size in bytes.
    pub fn capacity(&mut self) -> u64 {
        let d = self.data_in(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8);
        let last = u32::from_be_bytes(d[..4].try_into().unwrap()) as u64;
        let bs = u32::from_be_bytes(d[4..8].try_into().unwrap()) as u64;
        (last + 1) * bs
    }
}
