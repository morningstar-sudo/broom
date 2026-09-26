# Broom — Diskless Boot System

Network OS boot for sites with 10–30 machines: one shared golden image, every user write goes to
the local SSD and is **reset on every boot**. Supports **Linux** and **Windows 11 Pro**.

One binary does it all — no distro services: HTTP (web admin + boot files), **DHCP** (full or
proxyDHCP, PXE boot server on 4011), **TFTP**, and **iSCSI** targets (kernel LIO configured
directly through configfs). Linux goldens are read in-process (ext4 + LVM, no libguestfs); image
**versions** (snapshot/rollback, dedup 4 MB chunks) are built in (no ZFS). Publishing still uses a few
stable distro tools (qemu-img, hivex, initramfs-tools, ntfs-3g, sfdisk…); preflight installs them.

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
    │                # dhcp + tftp + iscsi (built-in services), machines, monitor/WOL, setup/preflight
    │   └── db/      # storage driver: `Db` trait + SQLite (db/sqlite.rs); other databases = new driver file
    ├── static/      # web admin (embedded in the binary)
    ├── ipxe/        # iPXE (GPLv2/UBDL): source in ipxe-src/ (upstream commit in IPXE_COMMIT, edited
    │                # in place) → `bash mgmt/ipxe/build.sh` → snponly.efi embedded in the binary
    └── dist/        # prebuilt binary (Linux x86_64)
```

## Build
One step: **double-click `build.cmd`** on Windows (runs in WSL `Ubuntu-24.04`; override with `BROOM_WSL`),
or `./build.sh` on Linux/WSL. Builds iPXE only when `mgmt/ipxe/ipxe-src` changed (`--ipxe` forces it),
then the mgmt release binary + unit tests (`--no-test` skips them; `--live` also runs the root-only
LIO/zram/ping/LVM tests, asking for sudo) → `mgmt/dist/bootrom-mgmt`. Don't run it with sudo.

CI: `.github/workflows/build.yml` runs the same `./build.sh --ipxe` (iPXE from source + binary + unit tests) on
every push / pull request and keeps `bootrom-mgmt`, `snponly.efi` and `SHA256SUMS` as artifacts. Every push
to `main` also tags the commit `v<version>-<short sha>` (version from `mgmt/Cargo.toml`, e.g. `v0.3.0-a1b2c3d`)
and publishes a GitHub Release with those files.
Needs Rust **stable via rustup** (`curl https://sh.rustup.rs -sSf | sh`) + `musl-tools` (the binary is
built **static with musl** → runs on any x86_64 Linux, no glibc version issue; `mgmt/rust-toolchain.toml`
pins the stable channel, edition 2024), gcc, make, perl, liblzma-dev.

## Adding a golden
A golden is always built in a **VM**, then the web admin uploads the **whole VM folder** (no zip; only
.vmx/.vmdk are sent, in parallel 8 MB chunks with retry) or a single .vmdk/.img/.zip — the server
handles the rest.

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
# First run: network not configured → setup runs BY ITSELF (detect network, install packages) then serves.
# Older installs: dnsmasq / tftpd-hpa / targetcli's restore service are stopped + disabled automatically.
sudo ./bootrom-mgmt --mode full      # full DHCP; without --mode full = proxyDHCP
```
Web admin: `http://<server-ip>/` — images, DHCP, machines (on/off), guest user.

All data lives next to the binary (wherever it is started from):
```
/opt/bootrom/
├── bootrom-mgmt     # the binary
├── bootrom.db       # config, images, machines, leases
├── images/<name>/   # image.img = golden (raw, sparse)
├── storage/         # image versions (dedup chunks + manifests)
├── tftp/            # boot files served over HTTP /tftp + TFTP (kernel/initrd, golden.vhdx, stage)
└── work/            # scratch for publish steps
```
Overrides: `BOOTROM_HOME` (the whole tree), `BOOTROM_IMAGES_DIR` / `BOOTROM_STORAGE_DIR` (e.g. a bigger
disk), `BOOTROM_DB=sqlite:///path/bootrom.db`. Upgrading from an older version: the first start moves
`bootrom.db` / `images` / `storage` from the current directory and `/srv/tftp/broom*` next to the binary.

Logs: events on stdout (`client PC01 started - mac … - ip … - hostname … - image win11`, DHCP/TFTP,
publish steps, config changes), warnings/errors on stderr. The on/off ping is never logged.
`RUST_LOG=debug` also shows every external command; under systemd: `journalctl -u <service> -f`.

Detailed status: `progress.md`.
