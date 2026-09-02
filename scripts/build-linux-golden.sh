#!/usr/bin/env bash
# build-linux-golden.sh — chạy TRÊN VM Ubuntu Desktop (golden).
# Đóng OS đang chạy thành squashfs image + đẩy sang server bootrom.
#
# Dùng: bash build-linux-golden.sh <server-ip> [ssh-user]
#   vd: bash build-linux-golden.sh 10.0.0.12 ccvi
set -euo pipefail

SERVER="${1:?Usage: build-linux-golden.sh <server-ip> [ssh-user]}"
SSH_USER="${2:-ccvi}"

command -v ltsp >/dev/null || {
    echo "Chưa có ltsp. Cài: sudo apt install -y ltsp"
    exit 1
}

echo "== 1. Đóng golden (ltsp image /) — nén cả OS, hơi lâu =="
sudo ltsp image /

IMG=/srv/ltsp/images/x86_64.img
[ -f "$IMG" ] || { echo "Không thấy $IMG — ltsp image thất bại?"; exit 1; }
ls -lh "$IMG"

echo "== 2. Đẩy image sang server $SERVER (rsync, có resume) =="
rsync -avP "$IMG" "${SSH_USER}@${SERVER}:/tmp/x86_64.img"

cat <<EOF

Xong. Giờ TRÊN SERVER chạy:
  sudo SERVER_IP=$SERVER bash scripts/publish-linux-image.sh
EOF
