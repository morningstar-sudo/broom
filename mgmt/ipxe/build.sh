#!/bin/bash
# Build iPXE (UEFI snponly.efi) → mgmt/ipxe/snponly.efi; this file is EMBEDDED in the mgmt binary (boot.rs) and
# served from memory by the built-in TFTP server (tftp.rs). Neither ipxe-src/ nor snponly.efi is in git.
# Source = upstream iPXE at the commit in IPXE_COMMIT + broom's changes in patches/ (menu layout, short banner).
# ipxe-src/ is a temporary git checkout: fetched + patched here (needs git + network), deleted again after the build
# (~200 MB with objects) unless KEEP_IPXE_SRC=1.
# To change iPXE: KEEP_IPXE_SRC=1 bash mgmt/ipxe/build.sh, edit + `git commit` inside ipxe-src/, then regenerate the
# patches and rerun this script (with KEEP_IPXE_SRC=1 while still editing):
#   rm mgmt/ipxe/patches/*.patch
#   git -C mgmt/ipxe/ipxe-src format-patch --no-signature "$(cat mgmt/ipxe/IPXE_COMMIT)" -o ../patches
# Newer iPXE: change IPXE_COMMIT, rerun, fix any patch that no longer applies.
# Runs on the dev machine (WSL/Linux with git gcc make perl liblzma-dev).
# iPXE: GPLv2 + UBDL — https://github.com/ipxe/ipxe
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
src="$here/ipxe-src"
commit=$(tr -d '[:space:]' < "$here/IPXE_COMMIT")
stamp="$commit $(cat "$here"/patches/*.patch | sha256sum | cut -d' ' -f1)"
if [ "$(cat "$src/.git/broom-stamp" 2>/dev/null)" != "$stamp" ]; then
  # Never throw away work in progress: uncommitted edits must be committed + exported to patches/ first.
  if [ -d "$src/.git" ] && [ -n "$(git -C "$src" status --porcelain)" ]; then
    echo "ipxe-src has uncommitted changes — commit them and regenerate patches/ (see the top of $0)" >&2
    exit 1
  fi
  echo "== iPXE: upstream $commit + $(ls "$here"/patches/*.patch | wc -l) patch(es) → ipxe-src/"
  rm -rf "$src"
  git init -q "$src"
  git -C "$src" config core.autocrlf false
  git -C "$src" fetch -q --depth 1 https://github.com/ipxe/ipxe "$commit"
  git -C "$src" checkout -q FETCH_HEAD
  git -C "$src" -c user.name=morningstar-sudo -c user.email=mornngstr1@gmail.com am -q "$here"/patches/*.patch
  echo "$stamp" > "$src/.git/broom-stamp"
fi
make -C "$src/src" -j"$(nproc)" bin-x86_64-efi/snponly.efi
cp "$src/src/bin-x86_64-efi/snponly.efi" "$here/snponly.efi"
echo "OK: $here/snponly.efi ($(stat -c %s "$here/snponly.efi") bytes)"
if [ "${KEEP_IPXE_SRC:-0}" != 1 ] && [ -z "$(git -C "$src" status --porcelain)" ]; then
  rm -rf "$src"   # regenerable from IPXE_COMMIT + patches/; kept when it holds uncommitted edits
fi
