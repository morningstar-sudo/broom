#!/bin/sh
PREREQ=""
prereqs(){ echo "$PREREQ"; }
case $1 in prereqs) prereqs; exit 0;; esac
. /usr/share/initramfs-tools/hook-functions
for b in __TOOLS__; do
  p=$(command -v $b) && copy_exec "$p" /broom/bin/$b
done
manual_add_modules ntfs3 vfat nls_cp437 nls_iso8859_1 nls_utf8 efivarfs
