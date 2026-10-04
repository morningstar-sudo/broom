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
# Every image this machine uses keeps its own copy (<name>.img + <name>.sha256; the .sha256 mtime = last use).
# Every boot the server says which ones may stay ("name hash" per line): a copy not listed (image deleted or set to not
# use the SSD) or of another version is removed. No answer → keep them all. The copy running now is never touched.
if [ -n "$SRV" ] && list=$(wget -q -T 10 -O - "http://$SRV/api/cache-list" 2>/dev/null); then
  for f in $C/*.sha256; do
    [ -f "$f" ] || continue; n=${f##*/}; n=${n%.sha256}
    [ "$n" = "$NAME" ] && [ "$MODE" = hit ] && continue
    printf '%s\n' "$list" | grep -qxF "$n $(cat "$f")" || { rm -f "$C/$n.img" "$f"; log "cache: $n removed (no longer cached / other version)"; }
  done
fi
# Unfinished copies (no .sha256) + leftovers of older versions (*.chunks: delta manifests).
for f in $C/*.img; do [ -f "${f%.img}.sha256" ] || rm -f "$f"; done
rm -f $C/*.chunks $C/*.tmp
[ "$MODE" = miss ] && [ -b "$GOLDEN" ] && [ -n "$HASH" ] && [ -n "$SIZE" ] || exit 0
# Spread over 0–5 minutes so all clients don't pull the golden at once after Publish; still congested →
# limit bandwidth on the server side.
sleep $(( $(od -An -N2 -tu2 /dev/urandom) % 300 ))
# Whole-file copy (no delta): this image's old copy goes first, then — while the golden doesn't fit — the copy unused
# the longest. One sequential write into free space keeps the new file unfragmented.
rm -f "$C/$NAME.img" "$C/$NAME.sha256"
# Bigger than the whole cache partition → it can never fit: keep the other copies instead of evicting them for nothing.
[ "$(df -B1 --output=size $C | tail -1)" -gt "$SIZE" ] || { log "cache: golden ($SIZE B) larger than the cache partition — not cached"; exit 0; }
while [ "$(df -B1 --output=avail $C | tail -1)" -le "$SIZE" ]; do
  old=$(ls -tr $C/*.sha256 2>/dev/null | head -1); [ -n "$old" ] || break
  n=${old##*/}; n=${n%.sha256}; rm -f "$C/$n.img" "$old"; log "cache: $n removed (unused the longest) to make room"
done
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
