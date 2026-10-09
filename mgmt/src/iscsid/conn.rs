// iscsid/conn.rs — one iSCSI connection (RFC 7143 subset that iPXE sanhook and open-iscsi use): login without
// authentication or digests (ERL 0, one connection per session), then SCSI commands served CONCURRENTLY (an initiator
// queues up to dozens; one at a time would cut it to queue depth 1), NOP, SendTargets, task management, logout.
// The reader parses PDUs and spawns a task per command; one writer task numbers and sends every reply.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::sync::{mpsc, Notify, Semaphore};

use super::{scsi, Lun};

/// Data segment we accept / send at most per PDU.
pub const MRDSL: usize = 262_144;
/// Commands in flight per connection (also the CmdSN window we advertise).
const WINDOW: u32 = 64;
/// Commands in flight in the whole daemon: bounds the read buffers (≤ 1 MB each) whatever the connection count.
static IN_FLIGHT: Semaphore = Semaphore::const_new(512);
/// A data segment bigger than this ends the connection (we never ask for data; login/text are small).
const MAX_IN: usize = 1 << 20;

fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}
fn pad(n: usize) -> usize {
    (4 - n % 4) % 4
}

pub struct Pdu {
    pub bhs: [u8; 48],
    pub data: Vec<u8>,
}

pub async fn read_pdu(r: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Pdu> {
    let mut bhs = [0u8; 48];
    r.read_exact(&mut bhs).await?;
    let ahs = bhs[4] as usize * 4;
    let dlen = (bhs[5] as usize) << 16 | (bhs[6] as usize) << 8 | bhs[7] as usize;
    if dlen > MAX_IN {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "data segment too big"));
    }
    let mut data = vec![0u8; ahs + dlen + pad(dlen)];
    r.read_exact(&mut data).await?;
    data.drain(..ahs);
    data.truncate(dlen);
    Ok(Pdu { bhs, data })
}

/// A PDU to send: `data[range]` as its data segment. `status`: gets the next StatSN.
pub struct Out {
    pub bhs: [u8; 48],
    pub data: Arc<Vec<u8>>,
    pub range: (usize, usize),
    pub status: bool,
}

impl Out {
    fn new(bhs: [u8; 48], data: Vec<u8>, status: bool) -> Out {
        let n = data.len();
        Out { bhs, data: Arc::new(data), range: (0, n), status }
    }
}

/// Sends in order, numbering StatSN and stamping ExpCmdSN/MaxCmdSN (same offsets in every target PDU).
pub async fn writer(w: impl AsyncWrite + Unpin, mut rx: mpsc::Receiver<Out>, exp: Arc<AtomicU32>) {
    let mut w = BufWriter::with_capacity(MRDSL + 64, w);
    let mut statsn: u32 = 1;
    while let Some(mut o) = rx.recv().await {
        loop {
            if o.status {
                put32(&mut o.bhs, 24, statsn);
                statsn = statsn.wrapping_add(1);
            }
            let e = exp.load(Ordering::Relaxed);
            put32(&mut o.bhs, 28, e);
            put32(&mut o.bhs, 32, e.wrapping_add(WINDOW - 1));
            let d = &o.data[o.range.0..o.range.1];
            o.bhs[5..8].copy_from_slice(&(d.len() as u32).to_be_bytes()[1..]);
            // A client that stops reading for 30 s is dropped: its queued replies would hold the daemon's read slots.
            let send = async {
                w.write_all(&o.bhs).await?;
                w.write_all(d).await?;
                w.write_all(&[0u8; 3][..pad(d.len())]).await
            };
            if !matches!(tokio::time::timeout(Duration::from_secs(30), send).await, Ok(Ok(()))) {
                return;
            }
            match rx.try_recv() {
                Ok(next) => o = next,
                Err(_) => break,
            }
        }
        if !matches!(tokio::time::timeout(Duration::from_secs(30), w.flush()).await, Ok(Ok(()))) {
            return;
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), w.flush()).await;
}

/// Logged-in sessions by (initiator name, ISID): a new login with the same ones replaces the old session (the
/// initiator lost it, e.g. a network blip) — the old connection is told to close instead of waiting for TCP to time out.
static ACTIVE: LazyLock<Mutex<HashMap<(String, [u8; 6]), Arc<Notify>>>> = LazyLock::new(Default::default);
static TSIH: AtomicU16 = AtomicU16::new(1);

struct Login {
    lun: Option<Arc<Lun>>, // None = discovery session
    their_mrdsl: usize,
    key: (String, [u8; 6]),
}

/// "k=v\0k=v\0" → pairs.
fn keys(d: &[u8]) -> Vec<(String, String)> {
    d.split(|&b| b == 0)
        .filter_map(|kv| {
            let s = String::from_utf8_lossy(kv);
            s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// Our answer to one offered login key (None = declarative / handled elsewhere, nothing to answer).
fn answer(k: &str, v: &str) -> Option<String> {
    let num = |max: u64| v.parse::<u64>().map_or(max, |n| n.min(max)).to_string();
    Some(match k {
        "InitiatorName" | "TargetName" | "SessionType" | "InitiatorAlias" | "MaxRecvDataSegmentLength" => return None,
        "AuthMethod" => if v.split(',').any(|m| m == "None") { "None".into() } else { "Reject".into() },
        "HeaderDigest" | "DataDigest" => if v.split(',').any(|m| m == "None") { "None".into() } else { "Reject".into() },
        "MaxConnections" | "MaxOutstandingR2T" => "1".into(),
        "ErrorRecoveryLevel" | "DefaultTime2Retain" => "0".into(),
        "InitialR2T" | "DataPDUInOrder" | "DataSequenceInOrder" => "Yes".into(),
        "ImmediateData" | "IFMarker" | "OFMarker" => "No".into(),
        "MaxBurstLength" | "FirstBurstLength" => num(MRDSL as u64),
        "DefaultTime2Wait" => num(3600),
        "IFMarkInt" | "OFMarkInt" => "Irrelevant".into(),
        _ => "NotUnderstood".into(),
    })
}

/// Login phase. Ok(None) = refused (response already queued) or the connection ended.
async fn login(r: &mut (impl AsyncRead + Unpin), tx: &mpsc::Sender<Out>, exp: &AtomicU32) -> std::io::Result<Option<Login>> {
    let (mut name, mut target, mut kind, mut their_mrdsl) = (String::new(), None::<String>, "Normal".to_string(), 8192);
    let (mut sent_tpgt, mut sent_mrdsl, mut pending) = (false, false, Vec::new());
    loop {
        let p = read_pdu(r).await?;
        if p.bhs[0] & 0x3f != 0x03 {
            return Ok(None); // anything but a login before full feature phase
        }
        let (flags, cmdsn) = (p.bhs[1], get32(&p.bhs, 24));
        let (transit, cont, csg, nsg) = (flags & 0x80 != 0, flags & 0x40 != 0, (flags >> 2) & 3, flags & 3);
        exp.store(cmdsn, Ordering::Relaxed); // login is immediate: ExpCmdSN = its CmdSN
        let mut rsp = [0u8; 48];
        rsp[0] = 0x23;
        rsp[8..14].copy_from_slice(&p.bhs[8..14]); // ISID
        rsp[16..20].copy_from_slice(&p.bhs[16..20]); // ITT
        pending.extend_from_slice(&p.data);
        if cont {
            rsp[1] = csg << 2; // more key text follows: acknowledge, answer once it's complete
            tx.send(Out::new(rsp, Vec::new(), true)).await.ok();
            continue;
        }
        let mut out = Vec::new();
        let mut fail = None;
        for (k, v) in keys(&std::mem::take(&mut pending)) {
            match k.as_str() {
                "InitiatorName" => name = v.clone(),
                "TargetName" => target = Some(v.clone()),
                "SessionType" => kind = v.clone(),
                "MaxRecvDataSegmentLength" => their_mrdsl = v.parse::<usize>().unwrap_or(8192).clamp(512, MRDSL),
                _ => {}
            }
            if let Some(a) = answer(&k, &v) {
                if k == "AuthMethod" && a == "Reject" {
                    fail = Some((0x02, 0x01)); // authentication failure: we only do None
                }
                out.push(format!("{k}={a}"));
            }
        }
        let lun = if kind == "Discovery" { None } else { target.as_deref().and_then(super::lookup) };
        if fail.is_none() && kind != "Discovery" && lun.is_none() {
            fail = Some((0x02, 0x03)); // target not found
        }
        if name.is_empty() && fail.is_none() {
            fail = Some((0x02, 0x07)); // missing InitiatorName
        }
        if let Some((class, detail)) = fail {
            rsp[1] = csg << 2;
            rsp[36] = class;
            rsp[37] = detail;
            tx.send(Out::new(rsp, Vec::new(), true)).await.ok();
            return Ok(None);
        }
        if kind == "Normal" && !sent_tpgt {
            out.push("TargetPortalGroupTag=1".into());
            sent_tpgt = true;
        }
        if (csg == 1 || (transit && nsg == 3)) && !sent_mrdsl {
            out.push(format!("MaxRecvDataSegmentLength={MRDSL}"));
            sent_mrdsl = true;
        }
        let done = transit && nsg == 3;
        rsp[1] = if transit { 0x80 | csg << 2 | nsg } else { csg << 2 };
        if done {
            rsp[14..16].copy_from_slice(&TSIH.fetch_add(1, Ordering::Relaxed).max(1).to_be_bytes());
        }
        let text: Vec<u8> = out.iter().flat_map(|s| s.bytes().chain([0])).collect();
        tx.send(Out::new(rsp, text, true)).await.ok();
        if done {
            let isid: [u8; 6] = p.bhs[8..14].try_into().unwrap();
            return Ok(Some(Login { lun, their_mrdsl, key: (name, isid) }));
        }
    }
}

/// Counts the session on its LUN for as long as the connection lives (image_in_use / gc read it).
struct Counted(Arc<Lun>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.sessions.fetch_sub(1, Ordering::Relaxed);
    }
}

pub async fn serve(sock: tokio::net::TcpStream) {
    let local = sock.local_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    let (mut r, w) = sock.into_split();
    let (tx, rx) = mpsc::channel::<Out>(256);
    let exp = Arc::new(AtomicU32::new(0));
    let wt = tokio::spawn(writer(w, rx, exp.clone()));
    // A connection that never finishes logging in is dropped (slow-loris on a public port).
    let Ok(Ok(Some(l))) = tokio::time::timeout(Duration::from_secs(15), login(&mut r, &tx, &exp)).await else {
        drop(tx);
        let _ = wt.await;
        return;
    };
    let kick = Arc::new(Notify::new());
    let _counted = l.lun.clone().map(|lun| {
        lun.sessions.fetch_add(1, Ordering::Relaxed);
        if let Some(old) = ACTIVE.lock().unwrap().insert(l.key.clone(), kick.clone()) {
            old.notify_one();
        }
        Counted(lun)
    });
    full_feature(&mut r, &tx, &exp, &l, &kick, &local).await;
    if l.lun.is_some() {
        let mut a = ACTIVE.lock().unwrap();
        if a.get(&l.key).is_some_and(|k| Arc::ptr_eq(k, &kick)) {
            a.remove(&l.key);
        }
    }
    drop(tx);
    let _ = wt.await;
}

async fn full_feature(
    r: &mut (impl AsyncRead + Unpin),
    tx: &mpsc::Sender<Out>,
    exp: &AtomicU32,
    l: &Login,
    kick: &Notify,
    local: &str,
) {
    let slots = Arc::new(Semaphore::new(WINDOW as usize));
    loop {
        if tx.is_closed() {
            return; // the writer gave up on this client
        }
        let p = tokio::select! {
            p = read_pdu(r) => match p { Ok(p) => p, Err(_) => return },
            _ = kick.notified() => return, // replaced by a new login of the same session
        };
        let (op, immediate) = (p.bhs[0] & 0x3f, p.bhs[0] & 0x40 != 0);
        let cmdsn = get32(&p.bhs, 24);
        if !immediate && op != 0x05 {
            // Next expected CmdSN (serial-number arithmetic: never moves backwards).
            let e = exp.load(Ordering::Relaxed);
            if cmdsn.wrapping_sub(e) < 1 << 31 {
                exp.store(cmdsn.wrapping_add(1), Ordering::Relaxed);
            }
        }
        let itt = get32(&p.bhs, 16);
        let mut rsp = [0u8; 48];
        rsp[16..20].copy_from_slice(&p.bhs[16..20]);
        match op {
            0x00 if itt != 0xffff_ffff => {
                // NOP-Out ping → NOP-In echo.
                rsp[0] = 0x20;
                rsp[1] = 0x80;
                rsp[8..16].copy_from_slice(&p.bhs[8..16]);
                put32(&mut rsp, 20, 0xffff_ffff);
                tx.send(Out::new(rsp, p.data, true)).await.ok();
            }
            0x00 | 0x05 => {} // NOP-Out answering ours / unsolicited Data-Out: nothing to do
            0x01 => {
                let Some(lun) = l.lun.clone() else { return reject(tx, &p.bhs, 0x05).await };
                let Ok(permit) = slots.clone().acquire_owned().await else { return };
                let Ok(global) = IN_FLIGHT.acquire().await else { return };
                let (tx, mrdsl) = (tx.clone(), l.their_mrdsl);
                tokio::spawn(async move {
                    // Both slots held until the data is handed to the writer: buffers waiting to be sent count too
                    // (the writer drops a client that stops reading, which frees them).
                    for o in command(&p.bhs, lun, mrdsl).await {
                        if tx.send(o).await.is_err() {
                            break;
                        }
                    }
                    drop((permit, global));
                });
            }
            0x02 => {
                // Task management: commands finish quickly, so every function is "complete".
                rsp[0] = 0x22;
                rsp[1] = 0x80;
                tx.send(Out::new(rsp, Vec::new(), true)).await.ok();
            }
            0x04 => {
                // Text: SendTargets (discovery) → every target at this portal.
                rsp[0] = 0x24;
                rsp[1] = 0x80;
                put32(&mut rsp, 20, 0xffff_ffff);
                let mut text = Vec::new();
                if keys(&p.data).iter().any(|(k, _)| k == "SendTargets") {
                    for iqn in super::iqns() {
                        text.extend(format!("TargetName={iqn}\0TargetAddress={local}:3260,1\0").bytes());
                    }
                }
                tx.send(Out::new(rsp, text, true)).await.ok();
            }
            0x06 => {
                rsp[0] = 0x26;
                rsp[1] = 0x80;
                tx.send(Out::new(rsp, Vec::new(), true)).await.ok();
                return;
            }
            _ => return reject(tx, &p.bhs, 0x04).await, // protocol error (SNACK at ERL 0, unknown opcode)
        }
    }
}

async fn reject(tx: &mpsc::Sender<Out>, bhs: &[u8; 48], reason: u8) {
    let mut rsp = [0u8; 48];
    rsp[0] = 0x3f;
    rsp[1] = 0x80;
    rsp[2] = reason;
    put32(&mut rsp, 16, 0xffff_ffff);
    tx.send(Out::new(rsp, bhs.to_vec(), true)).await.ok();
}

/// Run one SCSI command → the PDUs answering it (Data-In split to the initiator's segment size, status in the last
/// one; or a SCSI Response).
pub async fn command(bhs: &[u8; 48], lun: Arc<Lun>, mrdsl: usize) -> Vec<Out> {
    let cdb: [u8; 16] = bhs[32..48].try_into().unwrap();
    let edtl = get32(bhs, 20) as usize;
    let cmd = if bhs[8..16] != [0u8; 8] {
        // Only LUN 0 exists.
        match cdb[0] {
            0x12 => scsi::Cmd::Data(vec![0x7f, 0, 0x06, 0x02, 0, 0, 0, 0]), // peripheral qualifier: no LUN here
            0xa0 => scsi::handle(&cdb, lun.size, &lun.iqn),
            _ => scsi::Cmd::Check(0x05, 0x25, 0x00),
        }
    } else {
        scsi::handle(&cdb, lun.size, &lun.iqn)
    };
    let cmd = match cmd {
        scsi::Cmd::Read { lba, blocks } => {
            let (off, len) = (lba * scsi::BLOCK, blocks as usize * scsi::BLOCK as usize);
            let r = match lun.read_now(off, len) {
                Some(buf) => Ok(Ok(buf)),
                None => {
                    let l = lun.clone();
                    tokio::task::spawn_blocking(move || {
                        let mut buf = vec![0u8; len];
                        l.read_at(off, &mut buf).map(|_| buf)
                    })
                    .await
                }
            };
            match r {
                Ok(Ok(buf)) => scsi::Cmd::Data(buf),
                Ok(Err(e)) => {
                    tracing::warn!("iscsid: read {} @{off}+{len}: {e}", lun.iqn);
                    scsi::Cmd::Check(0x03, 0x11, 0x00) // medium error, unrecovered read error
                }
                Err(_) => scsi::Cmd::Check(0x04, 0x44, 0x00),
            }
        }
        c => c,
    };
    let mut base = [0u8; 48];
    base[16..20].copy_from_slice(&bhs[16..20]); // ITT
    let response = |status: u8, sense: Vec<u8>, sent: usize| {
        let mut b = base;
        b[0] = 0x21;
        b[1] = 0x80;
        b[3] = status;
        if sent < edtl {
            b[1] |= 0x02; // underflow
            put32(&mut b, 44, (edtl - sent) as u32);
        }
        Out::new(b, sense, true)
    };
    match cmd {
        scsi::Cmd::Ok | scsi::Cmd::Read { .. } => vec![response(0, Vec::new(), 0)],
        scsi::Cmd::Check(k, a, q) => {
            let s = scsi::sense(k, a, q);
            let mut d = (s.len() as u16).to_be_bytes().to_vec();
            d.extend(s);
            vec![response(0x02, d, 0)]
        }
        scsi::Cmd::Data(mut d) => {
            d.truncate(edtl); // the allocation length the initiator gave (INQUIRY / MODE SENSE …)
            if d.is_empty() {
                return vec![response(0, Vec::new(), 0)];
            }
            let n = d.len();
            let data = Arc::new(d);
            let mut v = Vec::new();
            for (sn, at) in (0..n).step_by(mrdsl).enumerate() {
                let end = (at + mrdsl).min(n);
                let mut b = base;
                b[0] = 0x25;
                b[8..16].copy_from_slice(&bhs[8..16]);
                put32(&mut b, 20, 0xffff_ffff);
                put32(&mut b, 36, sn as u32);
                put32(&mut b, 40, at as u32);
                let last = end == n;
                if last {
                    b[1] = 0x80 | 0x01; // final + status in this PDU (GOOD)
                    if n < edtl {
                        b[1] |= 0x02;
                        put32(&mut b, 44, (edtl - n) as u32);
                    }
                }
                v.push(Out { bhs: b, data: data.clone(), range: (at, end), status: last });
            }
            v
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_keys_answered() {
        assert_eq!(answer("HeaderDigest", "CRC32C,None").as_deref(), Some("None"));
        assert_eq!(answer("DataDigest", "CRC32C").as_deref(), Some("Reject"));
        assert_eq!(answer("AuthMethod", "CHAP,None").as_deref(), Some("None"));
        assert_eq!(answer("MaxBurstLength", "16776192").as_deref(), Some("262144"));
        assert_eq!(answer("MaxBurstLength", "65536").as_deref(), Some("65536"));
        assert_eq!(answer("ImmediateData", "Yes").as_deref(), Some("No"));
        assert_eq!(answer("X-com.example", "1").as_deref(), Some("NotUnderstood"));
        assert_eq!(answer("MaxRecvDataSegmentLength", "8192"), None, "declarative");
        assert_eq!(keys(b"A=1\0B=x=y\0\0"), [("A".into(), "1".into()), ("B".into(), "x=y".into())]);
    }
}
