#!/usr/bin/env bash
# publish-linux-image.sh — chạy TRÊN server bootrom (root).
# Nhận golden image → sinh kernel/initrd/nfs → set boot_script vào mgmt app.
#
# Dùng: sudo SERVER_IP=10.0.0.12 bash publish-linux-image.sh [img-name] [src-file]
# Env:  SERVER_IP (bắt buộc), IMG_ID (mặc định 1), MGMT (mặc định http://localhost)
set -euo pipefail

SERVER_IP="${SERVER_IP:?Cần SERVER_IP, vd: SERVER_IP=10.0.0.12}"
IMG_NAME="${1:-x86_64}"
SRC="${2:-/tmp/${IMG_NAME}.img}"
IMG_ID="${IMG_ID:-1}"
MGMT="${MGMT:-http://localhost}"
DST="/srv/ltsp/images/${IMG_NAME}.img"

[ "$(id -u)" = 0 ] || { echo "Cần root"; exit 1; }

if [ -f "$SRC" ] && [ "$SRC" != "$DST" ]; then
    echo "== Đưa image vào $DST =="
    mkdir -p /srv/ltsp/images
    mv "$SRC" "$DST"
fi
[ -f "$DST" ] || { echo "Không thấy image $DST"; exit 1; }
ls -lh "$DST"

echo "== ltsp kernel / initrd / nfs =="
ltsp kernel "$DST"
ltsp initrd
ltsp nfs

echo "== set boot_script cho image id=$IMG_ID (mgmt $MGMT) =="
# \${mac:hexhyp} và \${cmdline} phải tới iPXE nguyên văn; \\n = newline trong JSON.
BS="set cmdline root=/dev/nfs nfsroot=${SERVER_IP}:/srv/ltsp ltsp.image=images/${IMG_NAME}.img loop.max_part=9 BOOTIF=01-\${mac:hexhyp}\\nkernel http://${SERVER_IP}/tftp/ltsp/${IMG_NAME}/vmlinuz initrd=ltsp.img initrd=initrd.img \${cmdline}\\ninitrd http://${SERVER_IP}/tftp/ltsp/ltsp.img\\ninitrd http://${SERVER_IP}/tftp/ltsp/${IMG_NAME}/initrd.img\\nboot"

curl -fsS -X POST "${MGMT}/api/images/boot-script" \
    -H 'content-type: application/json' \
    -d "{\"id\":${IMG_ID},\"boot_script\":\"${BS}\"}" \
    && echo " → boot_script đã set"

echo "Xong. Boot client để kiểm."
