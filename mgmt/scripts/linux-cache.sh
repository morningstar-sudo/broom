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
# Only keep the cache of the running image (other images + partial files go).
for f in $C/*.img; do [ "$f" = "$C/$NAME.img" ] || rm -f "$f" "${f%.img}.sha256" "${f%.img}.chunks"; done
rm -f $C/*.tmp
# Delta: an older copy of THIS image + its manifest → patch it in place (raw = fixed positions): only chunks whose
# sha256 changed (server's golden.chunks) are read from the attached iSCSI golden, checked, written. Cache invalid
# while patching (re-running is idempotent). Any problem → the full copy below.
M=/run/broom-golden.chunks; T=/run/broom-chunk
if [ -f "$C/$NAME.img" ] && [ -f "$C/$NAME.chunks" ] && [ -n "$SRV" ] \
   && wget -q -O $M "http://$SRV/tftp/broom/$NAME/golden.chunks" && [ "$(sed -n '1s/^size //p' $M)" = "$SIZE" ]; then
  rm -f "$C/$NAME.sha256"
  awk 'NR==FNR { if (FNR>1) o[FNR-2]=$1; next } FNR>1 && o[FNR-2]!=$1 { print FNR-2, $1 }' "$C/$NAME.chunks" $M > /run/broom-diff
  truncate -s "$SIZE" "$C/$NAME.img"
  n=0; bad=""
  while read i h; do
    off=$((i * 4194304)); len=$((SIZE - off)); [ $len -gt 4194304 ] && len=4194304
    if [ "$h" = zero ]; then
      dd if=/dev/zero of="$C/$NAME.img" bs=4M iflag=count_bytes oflag=seek_bytes seek=$off count=$len conv=notrunc status=none
    else
      dd if="$GOLDEN" of=$T bs=4M iflag=skip_bytes,count_bytes skip=$off count=$len status=none
      [ "$(sha256sum $T | cut -c1-64)" = "$h" ] || { bad=$i; break; }
      dd if=$T of="$C/$NAME.img" bs=4M oflag=seek_bytes seek=$off conv=notrunc status=none
    fi
    n=$((n + 1))
  done < /run/broom-diff
  rm -f $T
  if [ -z "$bad" ]; then
    cp $M "$C/$NAME.chunks" && sync && echo "$HASH" > "$C/$NAME.sha256" && sync
    log "cache: delta OK — $n chunks ($((n * 4)) MB) patched, next boot runs from the SSD"
    exit 0
  fi
  log "cache: delta chunk $bad does not match the manifest → full copy"
fi
rm -f $C/*.img $C/*.sha256 $C/*.chunks
avail=$(df -B1 --output=avail $C | tail -1)
[ "$avail" -gt "$SIZE" ] || { log "cache: not enough SSD space ($avail < $SIZE)"; exit 0; }
log "cache: copying $GOLDEN -> $C/$NAME.img ..."
# count_bytes: a zram backstore can be larger than the source file (page rounding) → cut exactly SIZE so the hash matches.
ionice -c3 nice -n19 dd if="$GOLDEN" of="$C/$NAME.tmp" bs=4M iflag=count_bytes count="$SIZE" status=none \
  || { log "cache: dd failed"; exit 0; }
h=$(ionice -c3 nice -n19 sha256sum "$C/$NAME.tmp" | cut -d' ' -f1)
if [ "$h" = "$HASH" ]; then
  mv "$C/$NAME.tmp" "$C/$NAME.img" && sync && echo "$HASH" > "$C/$NAME.sha256" && sync
  # Manifest of this copy → the next golden update is a delta. No manifest (old server) → next one is full again.
  [ -n "$SRV" ] && wget -q -O "$C/$NAME.chunks" "http://$SRV/tftp/broom/$NAME/golden.chunks" || rm -f "$C/$NAME.chunks"
  log "cache: OK — next boot runs from the SSD"
else
  rm -f "$C/$NAME.tmp"; log "cache: hash mismatch ($h) — discarded"
fi
