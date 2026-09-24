# Broom — Diskless Boot System

Network OS boot for sites with 10–30 machines: one shared golden image, every user write goes to
the local SSD and is **reset on every boot**. Supports **Linux** and **Windows 11 Pro**.

## Layout
```
broom/
├── plan.md          # architecture + phases + verification (source of truth)
├── progress.md      # progress log + real test results
├── rule.md          # working rules
├── docs/
│   └── phase-w-windows.md   # guide + checklist for the Windows golden
└── mgmt/            # mgmt app (Rust/axum, one binary, web UI embedded)
    ├── src/         # boot menu, images+publish, overlay (Linux), winstage+vhdx (Windows),
    │                # machines, monitor/WOL, dnsmasq, setup/preflight
    ├── static/      # web admin (embedded in the binary)
    ├── ipxe/        # iPXE (GPLv2/UBDL): source in ipxe-src/ (upstream commit in IPXE_COMMIT, edited
    │                # in place) → `bash mgmt/ipxe/build.sh` → snponly.efi embedded in the binary
    └── dist/        # prebuilt binary (Linux x86_64)
```

## Build
One step: **double-click `build.cmd`** on Windows (runs in WSL `Ubuntu-24.04`; override with `BROOM_WSL`),
or `./build.sh` on Linux/WSL. Builds iPXE only when `mgmt/ipxe/ipxe-src` changed (`--ipxe` forces it),
then the mgmt release binary + unit tests (`--no-test` skips them) → `mgmt/dist/bootrom-mgmt`.
Needs cargo, gcc, make, perl, liblzma-dev.

## Adding a golden
A golden is always built in a **VM**, then its `.vmdk` (or a zip of the whole VM folder) is uploaded
to the web admin — the server handles the rest.

- **Linux:** inside the Ubuntu VM `curl -fsSL http://<server>/broom-prep | sudo bash` → power off the
  VM → upload (OS = linux, cache disk/zram). Clients boot via RO iSCSI + overlayroot, writes go to the
  SSD, reset every boot.
- **Windows 11 Pro:** inside the VM (Audit Mode) `irm http://<server>/broom-prep-win | iex` → the VM
  syspreps + powers off by itself → upload (OS = windows). Clients: golden.vhdx cached on the SSD,
  child VHDX reset every boot. Details: `docs/phase-w-windows.md`.

**Client machines:** UEFI, Secure Boot OFF, **PXE first in the boot order** (required for the
reset on every boot).

## Quickstart (Debian/Ubuntu server, run as root)
⚠ Deploy into a **fixed directory** (e.g. `/opt/bootrom`), do NOT run from `/tmp` — images +
`bootrom.db` live next to the binary.
```bash
sudo mkdir -p /opt/bootrom && cd /opt/bootrom
sudo cp <path>/bootrom-mgmt .
# First run: preflight FAILS → runs setup BY ITSELF (detect network, install packages, snponly.efi, dnsmasq) then serves.
sudo ./bootrom-mgmt --mode full      # full DHCP; without --mode full = proxyDHCP
```
Web admin: `http://<server-ip>/` — images, DHCP, machines, monitoring, guest user.
(`BOOTROM_IMAGES_DIR=/srv/bootrom/images` to store images elsewhere.)

Detailed status: `progress.md`.
