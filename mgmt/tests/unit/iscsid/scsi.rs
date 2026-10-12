use super::*;

fn cdb(b: &[u8]) -> [u8; 16] {
    let mut c = [0u8; 16];
    c[..b.len()].copy_from_slice(b);
    c
}
const SIZE: u64 = 1000 * 512 + 100; // a tail shorter than a block is not exposed (like LIO)

#[test]
fn capacity_and_reads() {
    assert_eq!(
        handle(&cdb(&[0x25]), SIZE, "t", false),
        Cmd::Data(vec![0, 0, 3, 231, 0, 0, 2, 0])
    );
    let Cmd::Data(d) = handle(&cdb(&[0x9e, 0x10]), SIZE, "t", false) else {
        panic!()
    };
    assert_eq!((be(&d[..8]), be(&d[8..12])), (999, 512));
    assert_eq!(
        handle(&cdb(&[0x28, 0, 0, 0, 0, 10, 0, 0, 4]), SIZE, "t", false),
        Cmd::Read { lba: 10, blocks: 4 }
    );
    assert_eq!(
        handle(
            &cdb(&[0x88, 0, 0, 0, 0, 0, 0, 0, 3, 0xe6, 0, 0, 0, 2]),
            SIZE,
            "t",
            false
        ),
        Cmd::Read {
            lba: 998,
            blocks: 2
        }
    );
    assert_eq!(
        handle(
            &cdb(&[0x88, 0, 0, 0, 0, 0, 0, 0, 3, 0xe7, 0, 0, 0, 2]),
            SIZE,
            "t",
            false
        ),
        Cmd::Check(5, 0x21, 0),
        "past the end"
    );
    assert_eq!(
        handle(&cdb(&[0x08, 0, 0, 5, 0]), SIZE, "t", false),
        Cmd::Read {
            lba: 5,
            blocks: 256
        },
        "READ(6): 0 = 256"
    );
    let big = 1u64 << 40;
    assert_eq!(
        handle(
            &cdb(&[0x88, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08, 1]),
            big,
            "t",
            false
        ),
        Cmd::Check(5, 0x24, 0),
        "> 1 MB in one READ"
    );
    assert_eq!(
        handle(
            &cdb(&[
                0x88, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 1
            ]),
            SIZE,
            "t",
            false
        ),
        Cmd::Check(5, 0x21, 0),
        "no overflow"
    );
}

#[test]
fn read_only_and_unknown() {
    for w in [0x2a, 0x8a, 0x0a, 0x42, 0x93] {
        assert_eq!(
            handle(&cdb(&[w]), SIZE, "t", false),
            Cmd::Check(7, 0x27, 0),
            "write {w:#x}"
        );
    }
    assert_eq!(
        handle(&cdb(&[0xee]), SIZE, "t", false),
        Cmd::Check(5, 0x20, 0)
    );
    let Cmd::Data(m) = handle(&cdb(&[0x1a, 0, 0x3f, 0, 255]), SIZE, "t", false) else {
        panic!()
    };
    assert_eq!(
        (m[0] as usize + 1, m[2] & 0x80),
        (m.len(), 0x80),
        "length + write-protect bit"
    );
    let Cmd::Data(m) = handle(&cdb(&[0x5a, 0, 0x08]), SIZE, "t", false) else {
        panic!()
    };
    assert_eq!(
        (be(&m[..2]) as usize + 2, m[3] & 0x80, m[8]),
        (m.len(), 0x80, 0x08)
    );
    assert_eq!(
        handle(&cdb(&[0x28, 0x20, 0, 0, 0, 0, 0, 0, 1]), SIZE, "t", false),
        Cmd::Check(5, 0x24, 0),
        "RDPROTECT"
    );
    assert_eq!(
        handle(&cdb(&[0x28, 0x18, 0, 0, 0, 0, 0, 0, 1]), SIZE, "t", false),
        Cmd::Read { lba: 0, blocks: 1 },
        "DPO+FUA"
    );
}

#[test]
fn writable_session() {
    let h = |c: &[u8]| handle(&cdb(c), SIZE, "t", true);
    assert_eq!(
        h(&[0x2a, 0, 0, 0, 0, 10, 0, 0, 4]),
        Cmd::Write { lba: 10, blocks: 4 }
    );
    assert_eq!(
        h(&[0x8a, 0, 0, 0, 0, 0, 0, 0, 3, 0xe6, 0, 0, 0, 2]),
        Cmd::Write {
            lba: 998,
            blocks: 2
        }
    );
    assert_eq!(
        h(&[0x8a, 0, 0, 0, 0, 0, 0, 0, 3, 0xe7, 0, 0, 0, 2]),
        Cmd::Check(5, 0x21, 0),
        "past the end"
    );
    assert_eq!(
        h(&[0x2a, 0x20, 0, 0, 0, 0, 0, 0, 1]),
        Cmd::Check(5, 0x24, 0),
        "WRPROTECT"
    );
    assert_eq!(h(&[0x35]), Cmd::Sync);
    assert_eq!(h(&[0x42]), Cmd::Check(5, 0x20, 0), "UNMAP not offered");
    let Cmd::Data(m) = h(&[0x5a, 0, 0x08]) else {
        panic!()
    };
    assert_eq!(
        (m[3], m[8], m[10] & 0x04),
        (0, 0x08, 0x04),
        "no WP / FUA, write cache on"
    );
}

#[test]
fn inquiry_pages() {
    let Cmd::Data(d) = handle(&cdb(&[0x12, 0, 0, 0, 96]), SIZE, "t", false) else {
        panic!()
    };
    assert_eq!((d.len(), d[0], d[4] as usize + 5), (74, 0, 74));
    assert_eq!(
        be(&d[64..66]),
        0x04c0,
        "claims SBC-3 (its Block Limits page length)"
    );
    let Cmd::Data(p) = handle(&cdb(&[0x12, 1, 0x00]), SIZE, "t", false) else {
        panic!()
    };
    assert_eq!(&p[4..], &[0x00, 0x80, 0x83, 0xb0]);
    let Cmd::Data(a) = handle(&cdb(&[0x12, 1, 0x83]), SIZE, "iqn.a", false) else {
        panic!()
    };
    let Cmd::Data(b) = handle(&cdb(&[0x12, 1, 0x83]), SIZE, "iqn.b", false) else {
        panic!()
    };
    assert!(
        a != b && be(&a[2..4]) as usize + 4 == a.len(),
        "distinct id per target"
    );
    let Cmd::Data(l) = handle(&cdb(&[0x12, 1, 0xb0]), SIZE, "t", false) else {
        panic!()
    };
    assert_eq!(be(&l[8..12]), MAX_XFER_BLOCKS as u64);
    assert_eq!(
        handle(&cdb(&[0x12, 1, 0x99]), SIZE, "t", false),
        Cmd::Check(5, 0x24, 0)
    );
}
