---
title: Broom — open-source diskless boot server for Windows 11 & Linux
description: Network-boot a room of PCs (PXE + iSCSI) from one golden image, clean on every boot. One static binary. Free alternative to commercial diskless software for internet cafés, gaming centers and school labs.
---

**Broom** network-boots a room of PCs from **one shared golden image**. Every write a user makes lands on the
machine's local SSD and is **wiped at the next boot**, so every machine comes up clean. Clients run
**Windows 11 Pro** or **Linux**. The server is **one static binary** for any x86_64 Linux, with DHCP, TFTP, iPXE,
an iSCSI target and a web admin built in.

Broom is free and open source (Apache-2.0), built for **internet cafés, gaming centers, school labs and
classrooms** of about 10–30 machines.

[Download the latest release](https://github.com/morningstar-sudo/broom/releases/latest) ·
[Source code & full manual](https://github.com/morningstar-sudo/broom) ·
[Hướng dẫn tiếng Việt](bootrom-quan-net.md)

![Windows 11 client network-booted by Broom, with a games disk mounted as G:](images/client-games-disk.png)

## Guides

- [Diskless boot Windows 11 with Broom](windows-11-diskless-boot.md) — how it works and the steps from an empty
  server to a room of clean Windows 11 PCs.
- [FACEIT Anti-Cheat on diskless PCs](faceit-diskless.md) — Secure Boot, TPM 2.0, IOMMU and HVCI on network-booted
  clients.
- [Broom vs CCBoot](broom-vs-ccboot.md) — how Broom differs from commercial diskless software, and where it is still
  behind.
- [Bootrom quán net với Broom](bootrom-quan-net.md) — hướng dẫn dựng phòng máy diskless bằng tiếng Việt.

## What you get

- **Clean on every boot** — Windows boots from a child VHDX on the SSD that is reset each boot; Linux from a
  read-only iSCSI root plus an overlay.
- **One file to deploy** — no dnsmasq, tftpd, targetcli, qemu-img or kernel modules to install.
- **Build the golden in a VM**, upload the VM folder from the browser, and Broom publishes it.
- **Local SSD cache** — each machine keeps its images on its SSD, so a whole room boots without loading the server.
- **Games disks** — install or update a game once on one machine, save, and every machine has it at its next boot.
- **Image versions** — instant snapshot / rollback; export a version back to a VM to edit it.
- **Machine groups** — per-group boot menus, images, games disks and drivers.
- **Secure Boot, TPM 2.0, VBS / HVCI** — [works with FACEIT Anti-Cheat](faceit-diskless.md).

## Quick start

On a Linux server (Debian/Ubuntu or Rocky Linux), as root:

```bash
sudo mkdir -p /opt/bootrom && cd /opt/bootrom
sudo curl -fLO https://github.com/morningstar-sudo/broom/releases/latest/download/bootrom-mgmt
sudo chmod +x bootrom-mgmt
sudo ./bootrom-mgmt
```

Then open `http://<server-ip>/` and follow the
[full manual](https://github.com/morningstar-sudo/broom#quick-start-linux-server-as-root).
