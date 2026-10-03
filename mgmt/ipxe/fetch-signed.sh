#!/bin/bash
# fetch-signed.sh — official Secure Boot iPXE (signed by the iPXE project) → mgmt/ipxe/signed/, embedded in the
# mgmt binary next to our own build (boot.rs). Used when "Secure Boot clients" is on (Network page):
#   snponly-shim.efi = the iPXE shim (signed by Microsoft UEFI CA 2011, trusts only the iPXE Secure Boot CA);
#                      it loads snponly.efi from the same TFTP directory by name
#   snponly.efi      = iPXE snponly signed by the iPXE Secure Boot CA
# Our own ipxe-src build can't be signed (Secure Boot exists to stop that), hence the official binaries.
# Pinned release + sha256: rerun with another VERSION/SHA256 to upgrade. UBDL allows redistributing them unmodified.
set -euo pipefail
VERSION=v2.0.0
SHA256=01a526d4cc791fc30362259c609d6c506cc64a7bdff51b9a5eb788354e17eee1
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/ipxeboot.tar.gz" "https://github.com/ipxe/ipxe/releases/download/$VERSION/ipxeboot.tar.gz"
echo "$SHA256  $tmp/ipxeboot.tar.gz" | sha256sum -c -
tar -xzf "$tmp/ipxeboot.tar.gz" -C "$tmp" ipxeboot/x86_64-sb/shimx64.efi ipxeboot/x86_64-sb/snponly.efi
mkdir -p "$here/signed"
cp "$tmp/ipxeboot/x86_64-sb/shimx64.efi" "$here/signed/snponly-shim.efi"
cp "$tmp/ipxeboot/x86_64-sb/snponly.efi" "$here/signed/snponly.efi"
( cd "$here/signed" && { echo "iPXE $VERSION ipxeboot.tar.gz x86_64-sb (sha256 $SHA256)"; sha256sum snponly-shim.efi snponly.efi; } > VERSION )
cat "$here/signed/VERSION"
