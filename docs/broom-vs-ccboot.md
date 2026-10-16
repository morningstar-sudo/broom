---
title: Broom vs CCBoot — open-source alternative for diskless internet cafés
description: How Broom, a free open-source diskless boot server, compares with CCBoot and other commercial diskless software, and where Broom is still behind.
date: 2026-10-11T23:07:00+07:00
last_modified_at: 2026-10-17
---

[← Broom](index.md)

CCBoot and similar commercial products are the usual way to run an internet café without a hard disk in every PC.
Broom does the same job — every PC boots one shared Windows image and is clean after each reboot — as free,
open-source software with a different design. This page compares the two approaches honestly. Commercial products
change between versions; check their own documentation for current details.

## At a glance

| | Broom | CCBoot / commercial diskless |
|---|---|---|
| License | Apache-2.0, free, source on GitHub | closed source, paid licence |
| Server | any x86_64 Linux, one static binary | Windows |
| Windows clients | Windows 11 Pro | several Windows versions |
| Linux clients | yes (ext4 / LVM goldens) | — |
| Where the client runs | from a VHDX cached on the PC's own SSD | depends on product and setup |
| Reset | every boot | every boot |
| Secure Boot / TPM / HVCI | yes, no MOK enrollment — [works with FACEIT](faceit-diskless.md) | check the product |
| Games | shared games disks, versioned, updated from one PC | product-specific game update tools |
| Remote view / control | built in: live wall of every PC's screen, control in the browser | check the product |
| Client health | CPU, RAM, temperatures, disk SMART / bad sectors per PC | check the product |
| Track record | young project | many years, many sites |

## Where Broom is different

- **Runs on the PC's SSD.** Each PC keeps the golden on its own SSD and boots Windows natively from it. The network
  is only used when a new golden arrives, so the server does not have to serve every disk read of every PC. A room
  can share one modest server.
- **One file on Linux.** No Windows Server licence, no services to install — DHCP, TFTP, iPXE, iSCSI target and web
  admin are all inside one binary.
- **Signed boot chain.** With Secure Boot clients on, the whole network boot is signed, so Secure Boot, TPM 2.0 and
  HVCI stay on — what modern anti-cheats require. No MOK to enroll on any PC.
- **Versions in a click.** Snapshots of the golden are hard links: instant and free until the golden changes. Games
  disks get a new version on every save, and PCs already playing keep the old one until they reboot.
- **The room in one browser tab.** A live wall of every PC's screen, take one over with the mouse and keyboard
  (full-screen games included), and each PC's CPU, RAM, temperature and disk health — no separate remote tool, no
  port open on the PCs.
- **Open.** You can read, audit, fork and fix the code; no licence keys or per-PC fees.

## Where Broom is behind

- **Young project** — fewer sites in production, no commercial support.
- **Windows 11 Pro only** on the Windows side, and **UEFI** PCs only.
- **Each PC needs an SSD** for Windows images (Linux images can run without one).
- **Big rooms not proven yet** — up to ~100 PCs per server by design, given a 10–25 GbE server link, NVMe and enough
  RAM ([sizing](https://github.com/morningstar-sudo/broom#sizing)); the sites running it so far are small rooms.
- **Web admin in English** only.

If those fit your site, Broom gives you a diskless café without licence fees. Start with
[Diskless boot Windows 11 with Broom](windows-11-diskless-boot.md).
