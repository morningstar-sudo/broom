#!/bin/bash
# Build iPXE (UEFI snponly.efi) from the source in this repo (ipxe-src/) → mgmt/ipxe/snponly.efi; this file is
# EMBEDDED in the mgmt binary (boot.rs) and written to /srv/tftp at runtime.
# ipxe-src = upstream iPXE at commit IPXE_COMMIT, edited in place (menu_ui.c layout, banner, no autoexec) +
# config in src/config/local/. To change the UI: edit ipxe-src directly, then rerun this script.
# Runs on the dev machine (WSL/Linux with gcc make perl liblzma-dev).
# iPXE: GPLv2 + UBDL — https://github.com/ipxe/ipxe
set -e
here=$(cd "$(dirname "$0")" && pwd)
make -C "$here/ipxe-src/src" -j"$(nproc)" bin-x86_64-efi/snponly.efi
cp "$here/ipxe-src/src/bin-x86_64-efi/snponly.efi" "$here/snponly.efi"
echo "OK: $here/snponly.efi ($(stat -c %s "$here/snponly.efi") bytes)"
