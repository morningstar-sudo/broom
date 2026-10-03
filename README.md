# Broom — Diskless Boot System

Network-boot a room of PCs from **one shared golden image**. Every user write goes to the local
SSD and is **wiped on every boot**, so every machine comes up clean. Built for small sites
(≈10–30 machines, e.g. an internet café or a lab). Supports **Linux** and **Windows 11 Pro**.

Everything is **one static binary** — no dnsmasq, tftpd, targetcli or extra services to install:

- **Web admin + boot files** over HTTP
- **DHCP server** (hands out IPs + PXE boot info) — toggle on/off
- **TFTP** (serves the embedded iPXE)
- **iSCSI** targets straight through the kernel (LIO via configfs)
- **Image versions** — snapshot / rollback, deduplicated in 4 MB chunks (no ZFS)
- **Linux goldens read in-process** — ext4 + LVM, no libguestfs

Publishing shells out to a few stable tools (`qemu-img`, `hivex`, `initramfs-tools`, `ntfs-3g`,
`sfdisk`, …); the first run installs them automatically.

## How it works

1. Build a golden **in a VM** (Linux or Windows 11 Pro).
2. Upload the VM folder to the web admin — the server extracts the disk, builds the boot files and
   serves it.
3. Clients PXE-boot the golden read-only; their writes land on the local SSD and reset each boot.
   - **Linux:** RO iSCSI root + `overlayroot` (writeback on the SSD, reset every boot).
   - **Windows:** `golden.vhdx` cached on the SSD, a child VHDX that resets every boot.

## Quick start (Debian/Ubuntu server, as root)

Deploy into a **fixed directory** (images and the database live next to the binary — don't run from `/tmp`).

```bash
sudo mkdir -p /opt/bootrom && cd /opt/bootrom
sudo cp <path>/bootrom-mgmt .
# First run with no network config: setup detects the network + installs packages, then serves.
# Any old dnsmasq / tftpd-hpa / targetcli restore service is stopped and disabled automatically.
sudo ./bootrom-mgmt
```

Then open `http://<server-ip>/`:

- **First open sets the admin password** (the LAN is shared — do it now). Every admin action needs a login.
- Turn the **DHCP server on** and pick the interface + address range, or leave it **off** if another
  DHCP server owns the LAN.
- Client/boot endpoints (`/boot*`, `/tftp`, license and driver/chunk fetch) stay open — a PXE client can't log in.

**Client machines:** UEFI, Secure Boot **off**, **PXE first** in the boot order (required for the reset-on-boot).

**Secure Boot (test):** tick **Secure Boot clients** on the Network page. Clients then get the official iPXE signed
by the iPXE project (`mgmt/ipxe/signed/`, via its Microsoft-signed shim) instead of broom's own build, and the Ubuntu
kernels boot through Ubuntu's Microsoft-signed shim (`apt install shim-signed` on the server). The Windows stage runs
the server's own kernel, which must be ≥ 6.x: shim rejects the 5.15 kernel ("Relocation section is invalid"), so on
Ubuntu 22.04 install `linux-generic-hwe-22.04`, reboot, then publish again. On the clients: enable
Secure Boot and the "Microsoft 3rd-party UEFI CA" (often off on Secured-core PCs). The menu footer (Host / IP / MAC)
then shows as lines under the images. Untick to go back to broom's own iPXE. TPM 2.0, VBS / Memory integrity (HVCI)
and IOMMU work on the clients too — broom-prep-win turns on VBS + HVCI in the golden (runs where the client has VT-x);
drivers in the golden must be HVCI-compatible (e.g. the VMware `e1000` NIC driver is blocked — use `e1000e`).

## Adding a golden

Build the golden in a VM, power it off, then upload the whole VM folder (only `.vmx` / `.vmdk` are
sent, in parallel 8 MB chunks with retry) — or a single `.vmdk` / `.img` / `.zip`.

- **Linux** — inside the VM:
  ```bash
  curl -fsSL http://<server>/broom-prep | sudo bash
  ```
  Power off → upload (OS = Linux, cache `disk` or `zram`).

- **Windows 11 Pro** — inside the VM in Audit Mode (PowerShell as Admin):
  ```powershell
  irm http://<server>/broom-prep-win | iex
  ```
  The VM syspreps and powers off by itself → upload (OS = Windows).

## Versions, export

- **Versions** (Images → Versions): snapshot / rollback the golden (deduplicated 4 MB chunks). **→ New image** turns a
  version into a separate image on the list (only its manifest is copied — no extra disk space) and publishes it.
- **Export** (per image, or per version): builds a VMware VM (`<name>.vmx` + `<name>.vmdk`) to edit the golden again —
  download both into one folder, open the `.vmx`, edit, run broom-prep again, upload. Needs free space on the server
  ≈ the golden size. Windows: works for images uploaded with this version or later (publish trims the boot partitions
  from the golden; they are now kept aside for the export).

## Data & configuration

Everything lives next to the binary:

```
/opt/bootrom/
├── bootrom-mgmt     # the binary
├── bootrom.db       # config, images, machines, leases (SQLite)
├── images/<name>/   # image.img = golden (raw, sparse)
├── storage/         # image versions (dedup chunks + manifests)
├── tftp/            # boot files (kernel/initrd, golden.vhdx, stage) — served over HTTP /tftp + TFTP
└── work/            # scratch for publish steps
```

Environment overrides: `BOOTROM_HOME` (the whole tree), `BOOTROM_IMAGES_DIR` / `BOOTROM_STORAGE_DIR`
(e.g. a bigger disk), `BOOTROM_DB=sqlite:///path/bootrom.db`.

Logs go to stdout (client boots, DHCP/TFTP, publish steps, config changes); warnings and errors to
stderr. `RUST_LOG=debug` also logs every external command. Under systemd: `journalctl -u <service> -f`.

## Security

The web admin and every `/api/*` route sit behind a login (argon2 password, signed session cookie) plus
a `Host`-header check. A few properties are inherent to diskless boot on a shared LAN and are handled by
**network setup**, not the binary:

- **Segment the boot LAN.** The iSCSI portal (`:3260`) and the golden over HTTP are readable by any host
  on that segment — that is how diskless clients read their root. Put the workstations on their own VLAN,
  away from guest Wi-Fi / unknown laptops, and don't bake real secrets into a golden.
- **License keys** are handed to a machine over plain HTTP once, when its base is built — fine on a
  trusted, segmented LAN.
- **Update `disk`-cache images off-hours.** A `disk` image serves one shared golden file and refuses to
  publish or roll back while clients are connected (they would read changed bytes). Use `zram` cache to
  update live: each publish gets a fresh RAM copy + target, and the old one is kept until its clients drop.

## Build

- **Windows:** double-click `build.cmd` (runs in WSL `Ubuntu-24.04`; override with `BROOM_WSL`).
- **Linux / WSL:** `./build.sh`.

Builds iPXE only when `mgmt/ipxe/ipxe-src` changed (`--ipxe` forces it), then the release binary + unit
tests (`--no-test` to skip; `--live` also runs the root-only LIO/zram/ping/LVM tests). Output:
`mgmt/dist/bootrom-mgmt`. Don't build with sudo.

Requirements: **Rust stable via [rustup](https://rustup.rs)** + `musl-tools` (the binary is built static
with musl → runs on any x86_64 Linux, no glibc version issues; edition 2024, toolchain pinned in
`mgmt/rust-toolchain.toml`), plus `gcc`, `make`, `perl`, `liblzma-dev`.

CI (`.github/workflows/build.yml`) runs `./build.sh --ipxe` on every push / PR and publishes
`bootrom-mgmt`, `snponly.efi` and `SHA256SUMS`; pushes to `main` are tagged `v<version>-<short-sha>`
and released.

## Repository layout

```
broom/
├── build.sh / build.cmd     # one-step build (Linux/WSL, or Windows→WSL)
└── mgmt/                     # the app (Rust / axum, single binary, web UI embedded)
    ├── src/                  # boot menu, images + publish, overlay (Linux), winstage + vhdx (Windows),
    │   │                     # dhcp / tftp / iscsi, machines, monitor + WOL, setup / preflight, auth
    │   └── db/               # storage: `Db` trait + SQLite (swap in another driver by adding a file)
    ├── static/               # web admin (embedded in the binary)
    └── ipxe/ipxe-src/        # vendored iPXE (GPLv2 / UBDL) → build.sh → snponly.efi embedded in the binary
```

## License

Broom is licensed under the **Apache License 2.0** — see [LICENSE](LICENSE) and [NOTICE](NOTICE).

It bundles **iPXE** (`mgmt/ipxe/ipxe-src/`), which is © the iPXE authors and licensed **GPLv2 (with the
UBDL exception)** — see that directory. iPXE is not covered by the Apache license; the two apply to their
respective parts of the repository.
