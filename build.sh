#!/bin/bash
# build.sh — build the whole project in one go (Linux / WSL):
#   1. iPXE snponly.efi   (upstream + mgmt/ipxe/patches; only when a patch, IPXE_COMMIT or ipxe-src changed, or --ipxe)
#   2. mgmt release binary (embeds snponly.efi + web UI + scripts) + unit tests (skip with --no-test);
#      --live also runs the root-only live tests (NTFS via ntfs-3g / ping / LVM, asks for sudo)
#   3. copy to mgmt/dist/bootrom-mgmt  → deploy that single file to the server
#   --dev: a debug build instead (not static, not for a server): CI only runs it to make the bundles
# Windows: double-click build.cmd (runs this script in WSL).
# Needs: Rust stable via rustup (https://rustup.rs), musl-tools, git, gcc, make, perl, liblzma-dev (iPXE).
# Output: a STATIC binary (musl) → runs on any x86_64 Linux server. Cargo output → ~/broom-target
# (override with CARGO_TARGET_DIR) — building on /mnt/* is slow and would litter the repo.
set -euo pipefail
cd "$(dirname "$0")"
# Under sudo HOME=/root → no rustup (falls back to an old distro cargo) + root-owned files in the repo.
# Building needs no root → run again as the calling user.
if [ "$(id -u)" = 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
  echo "== build.sh does not need sudo → running as $SUDO_USER"
  exec sudo -u "$SUDO_USER" -H bash "$0" "$@"
fi
# rustup's stable toolchain (mgmt/rust-toolchain.toml), also from non-login shells like build.cmd's.
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
root=$(pwd)

force_ipxe=0; test=1; live=0; dev=0
for a in "$@"; do
  case "$a" in
    --ipxe) force_ipxe=1 ;;
    --no-test) test=0 ;;
    --live) live=1 ;;
    --dev) dev=1 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "unknown option: $a (see --help)" >&2; exit 2 ;;
  esac
done

missing=""
for t in cargo git gcc make perl; do command -v "$t" >/dev/null || missing="$missing $t"; done
[ -z "$missing" ] || { echo "missing tools:$missing" >&2; exit 1; }

# 1. iPXE — rebuild only if the patches, the pinned commit or a source file is newer than the embedded binary.
efi=mgmt/ipxe/snponly.efi
# (ipxe-src/ only exists while someone edits the patches — see mgmt/ipxe/build.sh.)
if [ "$force_ipxe" = 1 ] || [ ! -f "$efi" ] \
   || [ -n "$(find mgmt/ipxe/IPXE_COMMIT mgmt/ipxe/patches mgmt/ipxe/ipxe-src/src -newer "$efi" -type f ! -path '*/bin*' -print -quit 2>/dev/null)" ]; then
  echo "== [1/3] iPXE"
  bash mgmt/ipxe/build.sh
else
  echo "== [1/3] iPXE unchanged, skip (--ipxe to force)"
fi

# 2. mgmt
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/broom-target}"
# The binary's version: the build time in UTC (CI passes the release tag) → a new build is never served from cache.
export BROOM_VERSION="${BROOM_VERSION:-$(date -u +%Y-%m-%dT%H-%M-%SZ)}"
cd "$root/mgmt"
# Static musl binary: no glibc dependency (a glibc build from a new distro fails on older servers with
# "GLIBC_2.xx not found"). Needs musl-tools (musl-gcc) for the bundled SQLite C code.
target=x86_64-unknown-linux-musl
command -v musl-gcc >/dev/null || { echo "missing musl-gcc: sudo apt install musl-tools" >&2; exit 1; }
# assets.pin holds a pin (CI's release build) → web UI / iPXE / scripts come from broom-assets.zip, not embedded.
features=""
grep -qE '^[0-9a-f]{64}$' assets.pin && features="--features external-assets"
if [ "$dev" = 1 ]; then
  # Same host target + profile as cargo test below → its dependencies are built once for both.
  echo "== [2/3] mgmt (cargo build, debug → $CARGO_TARGET_DIR) — for CI's bundle steps, not for a server"
  cargo build $features
  bin="$CARGO_TARGET_DIR/debug/bootrom-mgmt"
else
  echo "== [2/3] mgmt (cargo build --release --target $target $features → $CARGO_TARGET_DIR)"
  cargo build --release --target $target $features
  bin="$CARGO_TARGET_DIR/$target/release/bootrom-mgmt"
fi
if [ "$test" = 1 ]; then
  echo "== cargo test"
  cargo test
fi
if [ "$live" = 1 ]; then
  # Built as the user (rustup), only the test binary runs as root. They mount a scratch NTFS image (ntfs-3g),
  # make a loop/LVM VG "brtest" → run on a build box / WSL, not a busy server.
  echo "== live tests (root: NTFS, ping, LVM)"
  tbin=$(cargo test --bins --no-run 2>&1 | sed -n 's/.*Executable .*(\(.*\)).*/\1/p' | head -1)
  [ -n "$tbin" ] || { echo "live tests: test binary not found" >&2; exit 1; }
  sudo "$tbin" --ignored live
fi

# 3. dist
if [ "$dev" = 0 ] && readelf -l "$bin" | grep -q INTERP; then
  echo "ERROR: $bin is dynamically linked (expected static musl)" >&2; exit 1
fi
mkdir -p dist
cp "$bin" dist/bootrom-mgmt
echo "== [3/3] OK: mgmt/dist/bootrom-mgmt $BROOM_VERSION ($(du -h dist/bootrom-mgmt | cut -f1))"
