// iscsid/scsi.rs — the SCSI commands of a read-only disk (SPC/SBC subset that Linux sd, iPXE and UEFI use). Pure:
// a CDB in, what to answer out; the connection does the actual read.

/// 512-byte blocks, like the LIO fileio backstore the clients were set up with.
pub const BLOCK: u64 = 512;
/// Largest READ the Block Limits page allows (keeps one command within one negotiated burst).
pub const MAX_XFER_BLOCKS: u32 = 512;
/// Largest READ served at all (1 MB): the port is open to the LAN, a READ(16) could otherwise ask for gigabytes of
/// buffer. Initiators that read the Block Limits page stay at MAX_XFER_BLOCKS anyway.
const MAX_READ_BLOCKS: u64 = 2048;

#[derive(Debug, PartialEq)]
pub enum Cmd {
    /// Good status, no data.
    Ok,
    /// Good status with this data.
    Data(Vec<u8>),
    /// Good status with `blocks` blocks read from block `lba`.
    Read { lba: u64, blocks: u32 },
    /// CHECK CONDITION with fixed-format sense (key, ASC, ASCQ).
    Check(u8, u8, u8),
}

const ILLEGAL_REQUEST: u8 = 0x05;
const DATA_PROTECT: u8 = 0x07;

fn be(b: &[u8]) -> u64 {
    b.iter().fold(0, |a, &x| (a << 8) | x as u64)
}

/// Fixed-format sense data (18 bytes).
pub fn sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
    let mut s = vec![0u8; 18];
    s[0] = 0x70;
    s[2] = key;
    s[7] = 10;
    s[12] = asc;
    s[13] = ascq;
    s
}

/// Answer a CDB for LUN 0 of `size` bytes. `id` (the target IQN) names the disk in INQUIRY.
pub fn handle(cdb: &[u8; 16], size: u64, id: &str) -> Cmd {
    let nblocks = size / BLOCK;
    let read = |lba: u64, blocks: u64| match lba.checked_add(blocks) {
        _ if blocks > MAX_READ_BLOCKS => Cmd::Check(ILLEGAL_REQUEST, 0x24, 0x00), // invalid field in CDB
        Some(end) if end <= nblocks => Cmd::Read { lba, blocks: blocks as u32 },
        _ => Cmd::Check(ILLEGAL_REQUEST, 0x21, 0x00), // LBA out of range
    };
    match cdb[0] {
        0x00 | 0x1b | 0x1e | 0x35 | 0x91 | 0x2f | 0xaf => Cmd::Ok, // TUR, START STOP, PREVENT, SYNC CACHE, VERIFY
        0x03 => Cmd::Data(sense(0, 0, 0)),                         // REQUEST SENSE: nothing pending
        0x12 => inquiry(cdb, nblocks, id),
        0x25 => {
            // READ CAPACITY(10): last LBA (0xffffffff = use the 16-byte form) + block length.
            let last = nblocks.saturating_sub(1).min(0xffff_ffff) as u32;
            Cmd::Data([last.to_be_bytes(), (BLOCK as u32).to_be_bytes()].concat())
        }
        0x9e if cdb[1] & 0x1f == 0x10 => {
            let mut d = vec![0u8; 32];
            d[..8].copy_from_slice(&nblocks.saturating_sub(1).to_be_bytes());
            d[8..12].copy_from_slice(&(BLOCK as u32).to_be_bytes());
            Cmd::Data(d)
        }
        0x08 => {
            let blocks = match cdb[4] { 0 => 256, n => n as u64 };
            read(be(&[cdb[1] & 0x1f, cdb[2], cdb[3]]), blocks)
        }
        // No protection information on this disk: RDPROTECT must be 0. DPO/FUA are fine (nothing to bypass).
        0x28 | 0xa8 | 0x88 if cdb[1] >> 5 != 0 => Cmd::Check(ILLEGAL_REQUEST, 0x24, 0x00),
        0x28 => read(be(&cdb[2..6]), be(&cdb[7..9])),
        0xa8 => read(be(&cdb[2..6]), be(&cdb[6..10])),
        0x88 => read(be(&cdb[2..10]), be(&cdb[10..14])),
        // Writes of any kind: the golden is shared and read-only (DATA PROTECT / WRITE PROTECTED).
        0x0a | 0x2a | 0xaa | 0x8a | 0x2e | 0x8e | 0x41 | 0x93 | 0x42 | 0x89 => Cmd::Check(DATA_PROTECT, 0x27, 0x00),
        0xa0 => {
            // REPORT LUNS: just LUN 0.
            let mut d = vec![0u8; 16];
            d[3] = 8;
            Cmd::Data(d)
        }
        0x1a | 0x5a => mode_sense(cdb),
        _ => Cmd::Check(ILLEGAL_REQUEST, 0x20, 0x00), // invalid command operation code
    }
}

fn inquiry(cdb: &[u8; 16], nblocks: u64, id: &str) -> Cmd {
    let pad = |s: &str, n: usize| format!("{s:<n$}").into_bytes()[..n].to_vec();
    if cdb[1] & 1 == 0 {
        if cdb[2] != 0 {
            return Cmd::Check(ILLEGAL_REQUEST, 0x24, 0x00);
        }
        // Standard data: direct-access disk, SPC-4, command queuing; version descriptors SAM-5, iSCSI, SPC-4, SBC-3.
        let mut d = vec![0x00, 0x00, 0x06, 0x02, 69, 0, 0, 0x02];
        d.extend(pad("BROOM", 8));
        d.extend(pad("golden", 16));
        d.extend(pad("1.0", 4));
        d.resize(58, 0);
        for v in [0x00a0u16, 0x0960, 0x0460, 0x04c0] {
            d.extend(v.to_be_bytes());
        }
        d.resize(74, 0);
        return Cmd::Data(d);
    }
    let serial = blake3::hash(id.as_bytes()).to_hex()[..16].to_string();
    let page = |code: u8, body: Vec<u8>| {
        let mut d = vec![0x00, code];
        d.extend((body.len() as u16).to_be_bytes());
        d.extend(body);
        Cmd::Data(d)
    };
    match cdb[2] {
        0x00 => page(0x00, vec![0x00, 0x80, 0x83, 0xb0]),
        0x80 => page(0x80, serial.into_bytes()),
        0x83 => {
            // One T10 vendor-ID designator (ASCII): vendor + a stable id of the target.
            let des = [pad("BROOM", 8), serial.into_bytes()].concat();
            let mut b = vec![0x02, 0x01, 0x00, des.len() as u8];
            b.extend(des);
            page(0x83, b)
        }
        0xb0 => {
            // Block Limits: max / optimal transfer length.
            let mut b = vec![0u8; 0x3c];
            b[4..8].copy_from_slice(&MAX_XFER_BLOCKS.to_be_bytes());
            b[8..12].copy_from_slice(&(MAX_XFER_BLOCKS.min(nblocks.max(1) as u32)).to_be_bytes());
            page(0xb0, b)
        }
        _ => Cmd::Check(ILLEGAL_REQUEST, 0x24, 0x00),
    }
}

/// MODE SENSE(6)/(10): write-protected (WP + the control page's SWP), DPO/FUA accepted, no block descriptors; the
/// caching page (read cache on, no write cache).
fn mode_sense(cdb: &[u8; 16]) -> Cmd {
    const WP_DPOFUA: u8 = 0x80 | 0x10;
    let caching = {
        let mut p = vec![0u8; 20];
        p[0] = 0x08;
        p[1] = 0x12;
        p
    };
    let pages = match cdb[2] & 0x3f {
        0x08 | 0x3f => caching,
        0x0a => {
            let mut p = vec![0u8; 12];
            p[0] = 0x0a;
            p[1] = 0x0a;
            p[4] = 0x08; // SWP
            p
        }
        _ => return Cmd::Check(ILLEGAL_REQUEST, 0x24, 0x00),
    };
    let d = if cdb[0] == 0x1a {
        [vec![(3 + pages.len()) as u8, 0, WP_DPOFUA, 0], pages].concat()
    } else {
        let len = (6 + pages.len()) as u16;
        [len.to_be_bytes().to_vec(), vec![0, WP_DPOFUA, 0, 0, 0, 0], pages].concat()
    };
    Cmd::Data(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cdb(b: &[u8]) -> [u8; 16] {
        let mut c = [0u8; 16];
        c[..b.len()].copy_from_slice(b);
        c
    }
    const SIZE: u64 = 1000 * 512 + 100; // a tail shorter than a block is not exposed (like LIO)

    #[test]
    fn capacity_and_reads() {
        assert_eq!(handle(&cdb(&[0x25]), SIZE, "t"), Cmd::Data(vec![0, 0, 3, 231, 0, 0, 2, 0]));
        let Cmd::Data(d) = handle(&cdb(&[0x9e, 0x10]), SIZE, "t") else { panic!() };
        assert_eq!((be(&d[..8]), be(&d[8..12])), (999, 512));
        assert_eq!(handle(&cdb(&[0x28, 0, 0, 0, 0, 10, 0, 0, 4]), SIZE, "t"), Cmd::Read { lba: 10, blocks: 4 });
        assert_eq!(handle(&cdb(&[0x88, 0, 0, 0, 0, 0, 0, 0, 3, 0xe6, 0, 0, 0, 2]), SIZE, "t"), Cmd::Read { lba: 998, blocks: 2 });
        assert_eq!(handle(&cdb(&[0x88, 0, 0, 0, 0, 0, 0, 0, 3, 0xe7, 0, 0, 0, 2]), SIZE, "t"), Cmd::Check(5, 0x21, 0), "past the end");
        assert_eq!(handle(&cdb(&[0x08, 0, 0, 5, 0]), SIZE, "t"), Cmd::Read { lba: 5, blocks: 256 }, "READ(6): 0 = 256");
        let big = 1u64 << 40;
        assert_eq!(handle(&cdb(&[0x88, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08, 1]), big, "t"), Cmd::Check(5, 0x24, 0), "> 1 MB in one READ");
        assert_eq!(handle(&cdb(&[0x88, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 1]), SIZE, "t"), Cmd::Check(5, 0x21, 0), "no overflow");
    }

    #[test]
    fn read_only_and_unknown() {
        for w in [0x2a, 0x8a, 0x0a, 0x42, 0x93] {
            assert_eq!(handle(&cdb(&[w]), SIZE, "t"), Cmd::Check(7, 0x27, 0), "write {w:#x}");
        }
        assert_eq!(handle(&cdb(&[0xee]), SIZE, "t"), Cmd::Check(5, 0x20, 0));
        let Cmd::Data(m) = handle(&cdb(&[0x1a, 0, 0x3f, 0, 255]), SIZE, "t") else { panic!() };
        assert_eq!((m[0] as usize + 1, m[2] & 0x80), (m.len(), 0x80), "length + write-protect bit");
        let Cmd::Data(m) = handle(&cdb(&[0x5a, 0, 0x08]), SIZE, "t") else { panic!() };
        assert_eq!((be(&m[..2]) as usize + 2, m[3] & 0x80, m[8]), (m.len(), 0x80, 0x08));
        assert_eq!(handle(&cdb(&[0x28, 0x20, 0, 0, 0, 0, 0, 0, 1]), SIZE, "t"), Cmd::Check(5, 0x24, 0), "RDPROTECT");
        assert_eq!(handle(&cdb(&[0x28, 0x18, 0, 0, 0, 0, 0, 0, 1]), SIZE, "t"), Cmd::Read { lba: 0, blocks: 1 }, "DPO+FUA");
    }

    #[test]
    fn inquiry_pages() {
        let Cmd::Data(d) = handle(&cdb(&[0x12, 0, 0, 0, 96]), SIZE, "t") else { panic!() };
        assert_eq!((d.len(), d[0], d[4] as usize + 5), (74, 0, 74));
        assert_eq!(be(&d[64..66]), 0x04c0, "claims SBC-3 (its Block Limits page length)");
        let Cmd::Data(p) = handle(&cdb(&[0x12, 1, 0x00]), SIZE, "t") else { panic!() };
        assert_eq!(&p[4..], &[0x00, 0x80, 0x83, 0xb0]);
        let Cmd::Data(a) = handle(&cdb(&[0x12, 1, 0x83]), SIZE, "iqn.a") else { panic!() };
        let Cmd::Data(b) = handle(&cdb(&[0x12, 1, 0x83]), SIZE, "iqn.b") else { panic!() };
        assert!(a != b && be(&a[2..4]) as usize + 4 == a.len(), "distinct id per target");
        let Cmd::Data(l) = handle(&cdb(&[0x12, 1, 0xb0]), SIZE, "t") else { panic!() };
        assert_eq!(be(&l[8..12]), MAX_XFER_BLOCKS as u64);
        assert_eq!(handle(&cdb(&[0x12, 1, 0x99]), SIZE, "t"), Cmd::Check(5, 0x24, 0));
    }
}
