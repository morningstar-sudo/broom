#!/bin/bash
# build.sh — build the whole project in one go (Linux / WSL):
#   1. iPXE snponly.efi   (only when mgmt/ipxe/ipxe-src changed since the last build, or --ipxe)
#   2. mgmt release binary (embeds snponly.efi + web UI) + unit tests (skip with --no-test)
#   3. copy to mgmt/dist/bootrom-mgmt  → deploy that single file to the server
# Windows: double-click build.cmd (runs this script in WSL).
# Needs: cargo, gcc, make, perl, liblzma-dev (iPXE). Cargo output goes to ~/broom-target
# (override with CARGO_TARGET_DIR) — building on /mnt/* is slow and would litter the repo.
set -euo pipefail
cd "$(dirname "$0")"
root=$(pwd)

force_ipxe=0; test=1
for a in "$@"; do
  case "$a" in
    --ipxe) force_ipxe=1 ;;
    --no-test) test=0 ;;
    -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
    *) echo "unknown option: $a (see --help)" >&2; exit 2 ;;
  esac
done

missing=""
for t in cargo gcc make perl; do command -v "$t" >/dev/null || missing="$missing $t"; done
[ -z "$missing" ] || { echo "missing tools:$missing" >&2; exit 1; }

# 1. iPXE — rebuild only if a source file is newer than the embedded binary.
efi=mgmt/ipxe/snponly.efi
if [ "$force_ipxe" = 1 ] || [ ! -f "$efi" ] \
   || [ -n "$(find mgmt/ipxe/ipxe-src/src -newer "$efi" -type f ! -path '*/bin*' -print -quit)" ]; then
  echo "== [1/3] iPXE"
  bash mgmt/ipxe/build.sh
else
  echo "== [1/3] iPXE unchanged, skip (--ipxe to force)"
fi

# 2. mgmt
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/broom-target}"
cd "$root/mgmt"
echo "== [2/3] mgmt (cargo build --release → $CARGO_TARGET_DIR)"
cargo build --release
if [ "$test" = 1 ]; then
  echo "== cargo test"
  cargo test
fi

# 3. dist
cp "$CARGO_TARGET_DIR/release/bootrom-mgmt" dist/bootrom-mgmt
v=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
echo "== [3/3] OK: mgmt/dist/bootrom-mgmt v$v ($(du -h dist/bootrom-mgmt | cut -f1))"
