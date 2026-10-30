// A PC booting a published Linux golden, step by step as its firmware and iPXE do it: DHCP → TFTP iPXE → (iPXE) DHCP
// → HTTP boot menu → HTTP boot script → kernel/initrd over HTTP → iSCSI login + reads of the golden. A legacy BIOS PC
// gets its own iPXE at the TFTP step; from autoexec.ipxe on both take the same path.
mod common;

use common::clients::{ACK, DISCOVER, Dhcp, Http, Iscsi, OFFER, REQUEST, tftp_get};
use common::*;
use std::net::Ipv4Addr;

const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const MAC_S: &str = "52:54:00:12:34:56";

#[test]
fn linux_client_boots() {
    sandbox("linux_client_boots", || {
        let home = Home::new("boot");
        let _srv = Server::start(&home);
        let h = admin(&home);
        configure_network(&h);
        let g = linux_golden(&home.0);
        let id = publish_golden(&h, "ubu", &g);
        assert_eq!(
            h.post("/api/images/default", &serde_json::json!({"id": id}))
                .status,
            200
        );
        let r = h.post(
            "/api/machines",
            &serde_json::json!({"mac": MAC_S, "hostname": "pc01"}),
        );
        assert_eq!(r.status, 200, "add machine: {}", r.text());

        // 1. UEFI PXE firmware: DISCOVER → OFFER (pool IP, us as next-server, iPXE as boot file) → REQUEST → ACK.
        let dhcp = Dhcp::open();
        let pxe: [(u8, &[u8]); 2] = [(60, b"PXEClient:Arch:00007:UNDI:003016"), (93, &[0, 7])];
        let offer = dhcp.exchange(DISCOVER, MAC, 0x1001, &pxe);
        assert_eq!(offer.msg_type(), OFFER);
        let ip = offer.yiaddr;
        assert!(
            (Ipv4Addr::new(10, 99, 0, 100)..=Ipv4Addr::new(10, 99, 0, 120)).contains(&ip),
            "pool IP: {ip}"
        );
        assert_eq!(offer.siaddr.to_string(), SRV, "next-server");
        assert_eq!(offer.file, "snponly.efi");
        assert_eq!(
            offer.opt(54),
            Some(&[10, 99, 0, 1][..]),
            "server identifier"
        );
        let mut req = pxe.to_vec();
        let (want, sid) = (ip.octets(), [10, 99, 0, 1]);
        req.extend([(50, &want[..]), (54, &sid[..])]);
        let ack = dhcp.exchange(REQUEST, MAC, 0x1002, &req);
        assert_eq!((ack.msg_type(), ack.yiaddr), (ACK, ip));

        // 2. TFTP: iPXE exactly as embedded, then its autoexec script pointing at our boot menu.
        let efi = tftp_get("snponly.efi").unwrap();
        let built =
            std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/ipxe/snponly.efi")).unwrap();
        assert!(
            efi == built,
            "snponly.efi over TFTP: {} bytes, built {}",
            efi.len(),
            built.len()
        );
        let auto = String::from_utf8(tftp_get("autoexec.ipxe").unwrap()).unwrap();
        assert!(
            auto.contains(&format!("chain http://{SRV}/boot.ipxe?")),
            "{auto}"
        );
        assert!(
            tftp_get("../etc/passwd").is_err(),
            "TFTP serves only the iPXE files"
        );
        // A legacy BIOS PC (arch 0) on the same LAN: the BIOS iPXE, which chains the same autoexec.ipxe.
        let bios: [(u8, &[u8]); 2] = [(60, b"PXEClient:Arch:00000:UNDI:002001"), (93, &[0, 0])];
        let offer = dhcp.exchange(DISCOVER, [0x52, 0x54, 0, 0, 0, 0xb1], 0x2001, &bios);
        assert_eq!(
            (offer.msg_type(), offer.file.as_str()),
            (OFFER, "undionly.kpxe")
        );
        let kpxe = tftp_get("undionly.kpxe").unwrap();
        let built =
            std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/ipxe/undionly.kpxe")).unwrap();
        assert!(
            kpxe == built,
            "undionly.kpxe over TFTP: {} bytes, built {}",
            kpxe.len(),
            built.len()
        );

        // 3. iPXE asks DHCP again (option 175) → the boot menu URL as boot file, same IP.
        let ipxe = dhcp.exchange(DISCOVER, MAC, 0x1003, &[(175, &[1])]);
        assert_eq!(ipxe.yiaddr, ip, "same lease for the same MAC");
        assert_eq!(
            ipxe.file,
            format!("http://{SRV}/boot.ipxe?mac=${{net0/mac}}&ip=${{net0/ip}}")
        );

        // 4. Boot menu (no login needed) → default image ubu → its boot script.
        let anon = Http::new();
        let menu = anon.get(&format!("/boot.ipxe?mac={MAC_S}&ip={ip}")).text();
        assert!(menu.starts_with("#!ipxe"), "{menu}");
        assert!(menu.contains("/boot/start?image=ubu&"), "{menu}");
        assert!(
            menu.contains("choose --default"),
            "default image + countdown: {menu}"
        );
        let script = anon
            .get(&format!("/boot/start?image=ubu&mac={MAC_S}&ip={ip}"))
            .text();
        assert!(script.contains(&format!("set broom-srv {SRV}")), "{script}");
        assert!(
            script.contains(&format!("root=UUID={}", g.uuid)),
            "{script}"
        );
        assert!(
            home.log().contains("client pc01 started"),
            "boot logged with the hostname"
        );

        // 5. Kernel as in the golden; initrd = the golden's + broom's hook appended.
        assert!(
            anon.get("/tftp/broom/ubu/vmlinuz").body == g.kernel,
            "kernel"
        );
        let initrd = anon.get("/tftp/broom/ubu/initrd.img").body;
        assert!(
            initrd.len() > g.initrd.len() && initrd[..g.initrd.len()] == g.initrd[..],
            "initrd"
        );

        // 6. iSCSI: the IQN from the boot script, the golden byte for byte (MBR/GPT, ext4 superblock, kernel area).
        let mut s = Iscsi::login(&iqn_of(&script)).expect("iSCSI login");
        assert_eq!(s.capacity(), g.disk.len() as u64);
        for (lba, blocks) in [(0u32, 34u16), (2048, 8), (2048 + 64, 128), (40_000, 64)] {
            let at = lba as usize * 512;
            assert!(
                s.read(lba, blocks) == g.disk[at..at + blocks as usize * 512],
                "LBA {lba}+{blocks}"
            );
        }
        assert_eq!(
            Iscsi::login("iqn.2000-01.nope:x").err().map(|e| e.0),
            Some(2),
            "unknown target refused"
        );
    });
}
