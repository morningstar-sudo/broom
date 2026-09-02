// ltsp.rs — sinh /etc/ltsp/ltsp.conf (café user + autologin + home + SSD + cache) +
// patch initrd cho cache SSD. Gọi trước `ltsp initrd`.
use rusqlite::Connection;
use std::process::Command;

use crate::db;

/// base64 chuẩn (không newline) — cho PASSWORDS_x của LTSP.
fn b64(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        out.push(T[(b[0] >> 2) as usize] as char);
        out.push(T[(((b[0] & 0x3) << 4) | (b[1] >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 { T[(((b[1] & 0xf) << 2) | (b[2] >> 6)) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(b[2] & 0x3f) as usize] as char } else { '=' });
    }
    out
}

/// Ghi /etc/ltsp/ltsp.conf từ config DB.
pub fn write_conf(conn: &Connection) -> Result<(), String> {
    let user = db::get_config(conn, "ltsp_user", "khach");
    let pass = db::get_config(conn, "ltsp_password", "123456");
    let ssd_dev = db::get_config(conn, "ltsp_ssd_dev", "auto");
    let ssd_mount = db::get_config(conn, "ltsp_ssd_mount", "/games");
    let img_cache = db::get_config(conn, "ltsp_image_cache", "off");
    let sudo = db::get_config(conn, "ltsp_user_sudo", "1");
    let groups = if sudo == "1" {
        "audio,video,plugdev,netdev,sudo"
    } else {
        "audio,video,plugdev,netdev"
    };
    // LTSP PASSWORDS_x86_64 = user/base64(PLAINTEXT). ltsp init base64-decode → ghi /etc/shadow.
    // (man ltsp.conf). base64(crypt) SAI → password thật thành chuỗi $6$. Dùng plaintext.
    let pb64 = b64(pass.as_bytes());
    // Cleanup user: XOÁ HẾT user image uid>=1000 (ccvi server-inject + khach golden + mọi user)
    // rồi tạo lại DUY NHẤT café user fresh (home skel). Eval inline của POST_INIT không chạy
    // loop/redirect → viết script THẬT ra file rồi `sh file` (loop chạy bình thường). base64 để
    // tránh quote hell (script có nháy/space/newline). Chạy SYNC (không setsid) → xong trước gdm.
    let cleanup = format!(
        "while IFS=: read -r u x uid rest; do case $uid in \"\"|*[!0-9]*) continue;; esac; if [ \"$uid\" -ge 1000 ] && [ \"$uid\" -lt 65534 ] && [ \"$u\" != \"{user}\" ]; then userdel -f \"$u\" 2>/dev/null; fi; done < /etc/passwd\n\
id \"{user}\" >/dev/null 2>&1 || useradd -u 1000 -m -s /bin/bash \"{user}\"\n\
gpasswd -d \"{user}\" sudo 2>/dev/null\n\
usermod -aG {groups} \"{user}\" 2>/dev/null\n\
echo \"{user}:{pass}\" | chpasswd\n\
if [ ! -d /home/{user} ]; then cp -a /etc/skel /home/{user}; fi\n\
mkdir -p /home/{user}/.config\n\
echo yes > /home/{user}/.config/gnome-initial-setup-done\n\
chown -R {user}:{user} /home/{user}\n\
sed -i /ltsp/s/^/#/ /etc/pam.d/common-auth /etc/pam.d/common-session /etc/pam.d/common-account /etc/pam.d/common-password 2>/dev/null\n\
mkdir -p /etc/gdm3\n\
printf '[daemon]\\nAutomaticLoginEnable=true\\nAutomaticLogin={user}\\n' > /etc/gdm3/custom.conf\n"
    );
    let cleanup_b64 = b64(cleanup.as_bytes());

    // Cache SSD: initrd đọc IMAGE_CACHE_DEV; SSD chia p1=cache (initrd), p2=games (OS).
    let cache_line = if img_cache == "ssd" {
        format!("IMAGE_CACHE_DEV={ssd_dev}\n")
    } else {
        String::new()
    };
    // POST_INIT SSD: cache=ssd → 2 partition (p1 cache / p2 games); off → 1 (games).
    // $disk / ${disk} literal (build ngoài format lớn nên không cần escape thêm).
    // Resolve ổ: "auto" = ổ vật lý đầu tiên (/sys/block, loại loop/ram/dm/sr); hoặc /dev/path.
    // p = "p" nếu tên ổ kết thúc bằng số (nvme0n1p1); rỗng cho sda1.
    // KHÔNG dùng nháy kép bên trong (POST_INIT đã bọc "..."); var đĩa không có space nên an toàn.
    let resolve = format!("DEV={ssd_dev}; if [ $DEV = auto ]; then for d in /sys/block/*; do n=${{d##*/}}; case $n in loop*|ram*|dm-*|sr*|nbd*|md*) continue;; esac; disk=/dev/$n; break; done; else disk=$DEV; fi; case $disk in *[0-9]) p=p;; *) p=;; esac");
    // partprobe + wait node vì init-ltsp chưa có udev (parted xong node chưa hiện ngay).
    let setup = if img_cache == "ssd" {
        format!("{resolve}; case $disk in /dev/*) if [ -b ${{disk}} ] && ! blkid ${{disk}}${{p}}2 >/dev/null 2>&1; then parted -s $disk mklabel gpt mkpart cache ext4 1MiB 30% mkpart games ext4 30% 100%; partprobe $disk; i=0; while [ ! -b ${{disk}}${{p}}2 ] && [ $i -lt 20 ]; do sleep 1; i=$((i+1)); done; mkfs.ext4 -qF ${{disk}}${{p}}1; mkfs.ext4 -qF ${{disk}}${{p}}2; fi; mkdir -p {ssd_mount}; mount ${{disk}}${{p}}2 {ssd_mount}; chmod 777 {ssd_mount};; esac")
    } else {
        format!("{resolve}; case $disk in /dev/*) if [ -b ${{disk}} ] && ! blkid ${{disk}}${{p}}1 >/dev/null 2>&1; then parted -s $disk mklabel gpt mkpart primary ext4 1MiB 100%; partprobe $disk; i=0; while [ ! -b ${{disk}}${{p}}1 ] && [ $i -lt 20 ]; do sleep 1; i=$((i+1)); done; mkfs.ext4 -qF ${{disk}}${{p}}1; fi; mkdir -p {ssd_mount}; mount ${{disk}}${{p}}1 {ssd_mount}; chmod 777 {ssd_mount};; esac")
    };
    // Ghi setup ra /run/bssd.sh + `setsid ... </dev/null &` → TÁCH HẲN phiên (init-ltsp KHÔNG
    // chờ) + stdin EOF (prompt không block). Boot không thể treo vì SSD. /run luôn có (tmpfs).
    let post_init_ssd = format!("printf %s '{setup}' > /run/bssd.sh; setsid sh /run/bssd.sh </dev/null >/dev/null 2>&1 & true");

    let mut conf = format!(
        r#"# Auto-generated bởi bootrom-mgmt (ltsp.rs). ĐỪNG sửa tay.
[server]

[clients]
{cache_line}AUTOLOGIN={user}
PASSWORDS_x86_64="{user}/{pb64}"

# SSD local writeback.
POST_INIT_SSD="{post_init_ssd}"

# ZZUSER (chạy CUỐI, sau LTSP inject): decode script base64 → sh (loop xoá HẾT user image
# uid>=1000, tạo lại café {user} fresh + pass + groups + skip gnome). base64 tránh quote hell.
POST_INIT_ZZUSER="echo {cleanup_b64} | base64 -d > /run/buser.sh; sh /run/buser.sh; true"
"#
    );

    // Per-machine hostname: LTSP [mac] sections từ bảng máy.
    if let Ok(mut stmt) = conn
        .prepare("SELECT mac,hostname FROM machines WHERE hostname IS NOT NULL AND hostname != ''")
    {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            for (mac, hostname) in rows.flatten() {
                conf.push_str(&format!("\n[{}]\nHOSTNAME={hostname}\n", mac.to_lowercase()));
            }
        }
    }

    std::fs::create_dir_all("/etc/ltsp").map_err(|e| e.to_string())?;
    std::fs::write("/etc/ltsp/ltsp.conf", conf).map_err(|e| format!("ghi ltsp.conf: {e}"))?;
    Ok(())
}

/// Khối shell chèn vào 55-initrd-bottom.sh: cache squashfs xuống SSD p1, so hash sidecar.
const INITRD_CACHE_BLOCK: &str = r#"        # --- BROOM SSD CACHE (bootrom-mgmt) ---
        _bdev="$IMAGE_CACHE_DEV"
        if [ "$_bdev" = auto ]; then for d in /sys/block/*; do n=${d##*/}; case "$n" in loop*|ram*|dm-*|sr*|nbd*|md*) continue;; esac; _bdev=/dev/$n; break; done; fi
        case "$_bdev" in *[0-9]) _bp=p;; *) _bp=;; esac
        if [ -n "$_bdev" ] && [ "$IMAGE_TO_RAM" != "1" ] && [ -b "${_bdev}${_bp}1" ]; then
            _bimg=${img_src%%,*}; _brest=${img_src#$_bimg}; _bname=${_bimg##*/}
            _bcache=/run/initramfs/ltsp-cache
            mkdir -p "$_bcache"
            _blog() { echo "BROOM SSD cache: $*"; echo "BROOM SSD cache: $*" > /dev/kmsg 2>/dev/null; }
            if mount "${_bdev}${_bp}1" "$_bcache" 2>/dev/null; then
                _bsh=$(cat "${_bimg}.sha256" 2>/dev/null)
                _bch=$(cat "$_bcache/${_bname}.sha256" 2>/dev/null)
                if [ -n "$_bsh" ] && [ "$_bsh" = "$_bch" ] && [ -f "$_bcache/$_bname" ]; then
                    _blog "HIT $_bsh"
                else
                    _blog "MISS (server=$_bsh cache=$_bch) -> caching golden to SSD"
                    if cp "$_bimg" "$_bcache/$_bname"; then printf '%s' "$_bsh" > "$_bcache/${_bname}.sha256"; _blog "cached OK"; else _blog "cache copy FAILED, dung NFS"; fi
                fi
                if [ -f "$_bcache/$_bname" ]; then
                    re umount "$rootmnt"
                    img_src="$_bcache/$_bname$_brest"
                fi
            fi
        fi
        # --- /BROOM SSD CACHE ---
"#;

/// Chèn khối cache vào script LTSP. Xoá block cũ (giữa markers) rồi chèn block mới
/// → luôn dùng bản mới nhất, idempotent.
fn patch_initrd() -> Result<(), String> {
    let path = "/usr/share/ltsp/client/initrd-bottom/55-initrd-bottom.sh";
    let mut content = std::fs::read_to_string(path).map_err(|e| format!("đọc {path}: {e}"))?;

    // Gỡ block cũ nếu có.
    let start = "        # --- BROOM SSD CACHE (bootrom-mgmt) ---";
    let end = "        # --- /BROOM SSD CACHE ---";
    if let (Some(s), Some(e)) = (content.find(start), content.find(end)) {
        let e_end = content[e..].find('\n').map(|n| e + n + 1).unwrap_or(content.len());
        content.replace_range(s..e_end, "");
    }

    let anchor = "    elif [ -d \"$rootmnt/proc\" ]; then";
    if !content.contains(anchor) {
        return Err("không thấy anchor trong 55-initrd-bottom.sh (LTSP đổi bản?) — tắt cache".into());
    }
    content = content.replacen(anchor, &format!("{INITRD_CACHE_BLOCK}{anchor}"), 1);
    std::fs::write(path, content).map_err(|e| format!("ghi {path}: {e}"))?;
    Ok(())
}

/// Ghi ltsp.conf + patch initrd (nếu cache) + `ltsp initrd`.
pub fn apply(conn: &Connection) -> Result<(), String> {
    write_conf(conn)?;
    if db::get_config(conn, "ltsp_image_cache", "off") == "ssd" {
        patch_initrd()?;
    }
    let ok = Command::new("ltsp")
        .arg("initrd")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err("ltsp initrd lỗi".into())
    }
}
