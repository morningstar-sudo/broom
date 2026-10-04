#!/bin/sh
. /run/broom-cache.env
C=/run/broomcache
log(){ echo "$*" >> /run/broom-wb.log; }
# Hide broom's own SSD partitions (writeback + golden cache) from the desktop file manager — internal, and a guest
# browsing/mounting the cache could expose or corrupt the shared golden. Done here (server-injected) so it applies on
# a republish without re-prepping the golden. The overlay root is reset every boot, so re-create it each time.
if [ ! -f /etc/udev/rules.d/99-broom-hide.rules ] 2>/dev/null; then
  cat >/etc/udev/rules.d/99-broom-hide.rules 2>/dev/null <<'UDEV' && {
ENV{ID_FS_LABEL}=="broomwb", ENV{UDISKS_IGNORE}="1"
ENV{ID_FS_LABEL}=="broomcache", ENV{UDISKS_IGNORE}="1"
UDEV
    udevadm control --reload 2>/dev/null || true
    udevadm trigger --subsystem-match=block 2>/dev/null || true
  }
fi
mkdir -p /games && mount --bind $C/games /games && chmod 1777 $C/games
[ "$MODE" = miss ] && [ -b "$GOLDEN" ] && [ -n "$HASH" ] && [ -n "$SIZE" ] || exit 0
# Spread over 0–5 minutes so all clients don't pull the golden at once after Publish; still congested →
# limit bandwidth on the server side.
sleep $(( $(od -An -N2 -tu2 /dev/urandom) % 300 ))
# Whole-file copy (no delta): the old copy goes first — its space is needed, and one sequential write into free
# space keeps the new file unfragmented. (*.chunks: delta manifests of older versions.)
rm -f $C/*.img $C/*.sha256 $C/*.chunks $C/*.tmp
avail=$(df -B1 --output=avail $C | tail -1)
[ "$avail" -gt "$SIZE" ] || { log "cache: not enough SSD space ($avail < $SIZE)"; exit 0; }
log "cache: copying $GOLDEN -> $C/$NAME.img ..."
# Hashed while copying (tee) → the copy is never read back. count_bytes: a zram backstore can be larger than the
# source file (page rounding) → cut exactly SIZE so the hash matches. Short copy (disk full / read error) → size check.
h=$(ionice -c3 nice -n19 dd if="$GOLDEN" bs=4M iflag=count_bytes count="$SIZE" status=none \
  | tee "$C/$NAME.tmp" | sha256sum | cut -d' ' -f1)
if [ "$h" = "$HASH" ] && [ "$(stat -c %s "$C/$NAME.tmp" 2>/dev/null)" = "$SIZE" ]; then
  mv "$C/$NAME.tmp" "$C/$NAME.img" && sync && echo "$HASH" > "$C/$NAME.sha256" && sync
  log "cache: OK — next boot runs from the SSD"
else
  rm -f "$C/$NAME.tmp"; log "cache: copy failed or hash mismatch ($h) — discarded"
fi
