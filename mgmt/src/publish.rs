// publish.rs — sau khi upload golden, tự xử lý để image boot được.
// Golden = raw disk (từ vmdk). Serve iSCSI RO shared (disk hoặc zram) + build kernel/initrd
// (overlay.rs) → boot_script iPXE nạp kernel+initrd + attach iSCSI + overlay SSD.
// (LTSP cũ: install_ltsp_bundle/publish_linux còn dưới — gỡ ở B6 sau PoC.)
use rusqlite::params;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::{db, images_dir, SharedState};

/// Di chuyển file (rename, fallback copy nếu khác filesystem).
fn mv(src: &Path, dst: &Path) -> Result<(), String> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
    let _ = std::fs::remove_file(src);
    Ok(())
}

/// sha256 file (so version cho cache SSD). None nếu lỗi.
fn file_hash(path: &str) -> Option<String> {
    let out = Command::new("sha256sum").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(|s| s.to_string())
}

/// Tính + lưu hash image vào DB + sidecar `<path>.sha256` (cho initrd cache đọc qua NFS).
fn save_hash(st: &SharedState, name: &str, path: &str) {
    if let Some(h) = file_hash(path) {
        let _ = std::fs::write(format!("{path}.sha256"), &h);
        let c = st.db.lock().unwrap();
        let _ = c.execute("UPDATE images SET hash=?1 WHERE name=?2", params![h, name]);
    }
}

/// Cmdline LTSP → boot_script (dùng chung cho linux publish + bundle).
fn ltsp_boot_script(ip: &str, name: &str) -> String {
    format!(
        "set cmdline root=/dev/nfs nfsroot={ip}:/srv/ltsp ltsp.image=images/{name}.img loop.max_part=9 BOOTIF=01-${{mac:hexhyp}}\n\
         kernel http://{ip}/tftp/ltsp/{name}/vmlinuz initrd=ltsp.img initrd=initrd.img ${{cmdline}}\n\
         initrd http://{ip}/tftp/ltsp/ltsp.img\n\
         initrd http://{ip}/tftp/ltsp/{name}/initrd.img\n\
         boot"
    )
}

/// Nhận bundle zip (từ VM desktop: x86_64.img + vmlinuz + initrd.img), giải nén,
/// đặt đúng chỗ LTSP, sinh ltsp.img (config server) + nfs + boot_script. Chạy blocking.
pub fn install_ltsp_bundle(st: &SharedState, name: &str, zip_path: &str) -> Result<String, String> {
    let exdir = format!("/tmp/bootrom-ltsp-{name}");
    let _ = std::fs::remove_dir_all(&exdir);
    std::fs::create_dir_all(&exdir).map_err(|e| e.to_string())?;
    run("unzip", &["-o", zip_path, "-d", &exdir])?;

    // squashfs → images/<name>/image.img + hardlink /srv/ltsp/images/<name>.img
    let idir = images_dir().join(name);
    std::fs::create_dir_all(&idir).map_err(|e| e.to_string())?;
    let img_dst = idir.join("image.img");
    let src_img = Path::new(&exdir).join("x86_64.img");
    if !src_img.exists() {
        return Err("zip thiếu x86_64.img".into());
    }
    mv(&src_img, &img_dst)?;
    std::fs::create_dir_all("/srv/ltsp/images").map_err(|e| e.to_string())?;
    let ltsp_img = format!("/srv/ltsp/images/{name}.img");
    let _ = std::fs::remove_file(&ltsp_img);
    let img_abs = std::fs::canonicalize(&img_dst).map_err(|e| e.to_string())?;
    if std::fs::hard_link(&img_abs, &ltsp_img).is_err() {
        std::fs::copy(&img_abs, &ltsp_img).map_err(|e| e.to_string())?;
    }

    // kernel/initrd → /srv/tftp/ltsp/<name>/
    let tftp = format!("/srv/tftp/ltsp/{name}");
    std::fs::create_dir_all(&tftp).map_err(|e| e.to_string())?;
    for f in ["vmlinuz", "initrd.img"] {
        let s = Path::new(&exdir).join(f);
        if !s.exists() {
            return Err(format!("zip thiếu {f}"));
        }
        mv(&s, &Path::new(&tftp).join(f))?;
    }
    let _ = std::fs::remove_dir_all(&exdir);

    // ltsp.conf café + patch cache + ltsp initrd (ltsp::apply) → ltsp.img server + nfs.
    {
        let c = st.db.lock().unwrap();
        crate::ltsp::apply(&c)?;
    }
    run("ltsp", &["nfs"])?;

    let ip = {
        let c = st.db.lock().unwrap();
        db::get_config(&c, "dhcp_server_ip", "")
    };
    if ip.is_empty() {
        return Err("dhcp_server_ip trống — chạy `setup` trước".into());
    }
    let bs = ltsp_boot_script(&ip, name);
    {
        let c = st.db.lock().unwrap();
        c.execute("UPDATE images SET boot_script=?1 WHERE name=?2", params![bs, name])
            .map_err(|e| e.to_string())?;
    }
    save_hash(st, name, &ltsp_img);
    Ok(format!("Bundle LTSP xử lý xong — image '{name}' publish + boot_script sẵn sàng"))
}

/// Chuẩn hoá nguồn upload thành raw `dest` (image.img). Blocking.
/// src: "raw" (đã là img → rename) | "vmdk" (qemu-img convert) | "zip" (giải nén → vmdk/img → convert).
pub fn prepare_golden(src: &str, uploaded: &Path, dest: &Path) -> Result<(), String> {
    match src {
        "raw" => {
            // uploaded == <dest>.uploading → đổi tên thành image.img.
            mv(uploaded, dest)
        }
        "vmdk" => {
            run("qemu-img", &["convert", "-O", "raw",
                &uploaded.to_string_lossy(), &dest.to_string_lossy()])?;
            let _ = std::fs::remove_file(uploaded);
            Ok(())
        }
        "zip" => {
            let exdir = uploaded.with_file_name("unzip");
            let _ = std::fs::remove_dir_all(&exdir);
            std::fs::create_dir_all(&exdir).map_err(|e| e.to_string())?;
            run("unzip", &["-o", &uploaded.to_string_lossy(), "-d", &exdir.to_string_lossy()])?;
            // Gom .vmdk + .img/.raw.
            let mut vmdks = Vec::new();
            let mut raw = None;
            for e in walk(&exdir) {
                match e.extension().and_then(|s| s.to_str()) {
                    Some("vmdk") => vmdks.push(e),
                    Some("img") | Some("raw") => { if raw.is_none() { raw = Some(e); } }
                    _ => {}
                }
            }
            // Chọn vmdk để convert: 1 file → dùng luôn (monolithic). Nhiều file (split/snapshot)
            // → chọn DESCRIPTOR (file text, không phải extent nhị phân); nhiều descriptor
            // (snapshot) → tên lớn nhất = trạng thái hiện tại. qemu-img đọc extent cùng thư mục.
            let chosen_vmdk = if vmdks.len() == 1 {
                Some(vmdks.remove(0))
            } else if !vmdks.is_empty() {
                let mut desc: Vec<_> = vmdks.iter().filter(|p| is_vmdk_descriptor(p)).cloned().collect();
                desc.sort();
                desc.pop()
            } else {
                None
            };
            let out = if let Some(v) = chosen_vmdk {
                run("qemu-img", &["convert", "-O", "raw",
                    &v.to_string_lossy(), &dest.to_string_lossy()])?;
                Ok(())
            } else if let Some(r) = raw {
                mv(&r, dest)
            } else if !vmdks.is_empty() {
                Err("zip có nhiều .vmdk nhưng không thấy file descriptor — xuất VM thành 1 vmdk monolithic rồi up lại".into())
            } else {
                Err("zip không có .vmdk/.img/.raw".into())
            };
            let _ = std::fs::remove_dir_all(&exdir);
            let _ = std::fs::remove_file(uploaded);
            out
        }
        other => Err(format!("src không hợp lệ: {other}")),
    }
}

/// vmdk descriptor = file text (chứa "# Disk DescriptorFile" / "createType") trỏ tới extent.
/// Ngược lại = extent nhị phân (monolithicSparse mở đầu magic "KDMV") — không convert riêng được.
fn is_vmdk_descriptor(p: &Path) -> bool {
    use std::io::Read;
    let mut buf = [0u8; 2048];
    let n = std::fs::File::open(p).and_then(|mut f| f.read(&mut buf)).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    head.contains("# Disk DescriptorFile") || head.contains("createType")
}

/// img có vừa RAM cho zram không: cần avail >= img + reserve. avail=0 (không đọc được) → cho qua.
fn zram_fits(img: u64, avail: u64, reserve: u64) -> bool {
    avail == 0 || img.saturating_add(reserve) <= avail
}

/// RAM khả dụng (bytes) từ /proc/meminfo MemAvailable. 0 nếu không đọc được (bỏ qua check).
fn mem_available_bytes() -> u64 {
    let s = match std::fs::read_to_string("/proc/meminfo") {
        Ok(s) => s,
        Err(_) => return 0,
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            // "MemAvailable:   12345678 kB"
            if let Some(kb) = rest.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()) {
                return kb * 1024;
            }
        }
    }
    0
}

/// Duyệt file trong dir (đệ quy). ponytail: đủ cho zip golden vài file.
fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn run(bin: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("{bin}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{bin} {} lỗi: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Publish image theo os. Chạy blocking (gọi từ spawn_blocking).
pub fn run_publish(st: &SharedState, name: &str) -> Result<String, String> {
    let (id, os): (i64, String) = {
        let c = st.db.lock().unwrap();
        c.query_row("SELECT id, os FROM images WHERE name=?1", [name], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(|e| format!("image '{name}' không có trong DB: {e}"))?
    };
    match os.as_str() {
        "linux" => publish_iscsi(st, id, name),
        other => Err(format!("os không hợp lệ: {other} (chỉ hỗ trợ linux)")),
    }
}

/// Chạy 1 loạt lệnh targetcli qua stdin.
fn targetcli_script(script: &str) -> Result<(), String> {
    let mut child = Command::new("targetcli")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("targetcli: {e}"))?;
    {
        let mut si = child.stdin.take().ok_or("stdin")?;
        si.write_all(script.as_bytes()).map_err(|e| e.to_string())?;
    } // đóng stdin → targetcli xử lý rồi thoát
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Golden Linux: build kernel/initrd (overlay.rs) + serve iSCSI RO shared (disk|zram) +
/// boot_script iPXE (nạp kernel/initrd, initrd tự attach iSCSI + overlay SSD). Blocking.
fn publish_iscsi(st: &SharedState, id: i64, name: &str) -> Result<String, String> {
    let img = images_dir().join(name).join("image.img");
    let img_abs = std::fs::canonicalize(&img)
        .map_err(|e| format!("chưa có golden raw ({}): {e}", img.display()))?;

    // 1. Trích kernel + initrd từ golden → /srv/tftp/broom/<name>/, đọc UUID root.
    let root_uuid = crate::overlay::build_boot(&img_abs, name)?;

    // 2. cache_mode (cột images): disk → serve thẳng file; zram → nạp img vào /dev/zramN.
    // zram fail (tràn RAM / lỗi) → TỰ hạ về disk (cập nhật DB) để image luôn boot được.
    let want = {
        let c = st.db.lock().unwrap();
        c.query_row("SELECT cache_mode FROM images WHERE id=?1", [id], |r| r.get::<_, String>(0))
            .unwrap_or_else(|_| "disk".into())
    };
    let (cache_mode, backing) = if want == "zram" {
        match ensure_zram(st, name, &img_abs) {
            Ok(dev) => ("zram".to_string(), dev),
            Err(e) => {
                eprintln!("[zram] '{name}': {e} → hạ về cache_mode=disk");
                let c = st.db.lock().unwrap();
                let _ = c.execute("UPDATE images SET cache_mode='disk' WHERE id=?1", [id]);
                ("disk".to_string(), img_abs.to_string_lossy().to_string())
            }
        }
    } else {
        ("disk".to_string(), img_abs.to_string_lossy().to_string())
    };

    // 3. iSCSI target RO shared. Idempotent: gỡ cũ trước.
    let iqn = format!("iqn.2026-08.net.tiem:{name}");
    let _ = targetcli_script(&format!("cd /iscsi\ndelete {iqn}\nexit\n"));
    let _ = targetcli_script(&format!("cd /backstores/fileio\ndelete {name}\nexit\n"));
    let _ = targetcli_script(&format!("cd /backstores/block\ndelete {name}\nexit\n"));
    // zram = block device → backstore block; disk = file → backstore fileio.
    let backstore = if cache_mode == "zram" {
        format!("/backstores/block create {name} {backing}\n")
    } else {
        format!("/backstores/fileio create {name} {backing}\n")
    };
    let bs_path = if cache_mode == "zram" {
        format!("/backstores/block/{name}")
    } else {
        format!("/backstores/fileio/{name}")
    };
    let script = format!(
        "{backstore}\
         /iscsi create {iqn}\n\
         /iscsi/{iqn}/tpg1/luns create {bs_path}\n\
         /iscsi/{iqn}/tpg1 set attribute authentication=0 generate_node_acls=1 demo_mode_write_protect=1\n\
         saveconfig\nexit\n"
    );
    targetcli_script(&script).map_err(|e| format!("targetcli tạo target: {e}"))?;

    // 4. boot_script iPXE. initrd đọc broom.iscsi / broom.ssd từ cmdline.
    let ip = {
        let c = st.db.lock().unwrap();
        db::get_config(&c, "dhcp_server_ip", "")
    };
    if ip.is_empty() {
        return Err("dhcp_server_ip trống — chạy `setup` trước".into());
    }
    // sanhook = iPXE attach iSCSI qua iBFT (không boot LUN); initrd open-iscsi đọc iBFT →
    // /dev/sda golden RO → root=UUID mount RO; overlayroot (bake trong golden) overlay lên
    // writeback SSD (reset mỗi boot). ip=dhcp cho initrd có mạng.
    // overlayroot trên CMDLINE (ưu tiên hơn file conf) → root RO + upper trên SSD LABEL broomwb.
    // Bỏ `quiet` để thấy log overlayroot/broom khi PoC.
    let bs = format!(
        "sanhook iscsi:{ip}::::{iqn} || shell\n\
         kernel http://{ip}/tftp/broom/{name}/vmlinuz initrd=initrd.img ip=dhcp root=UUID={root_uuid} ro fsck.mode=skip overlayroot=device:dev=/dev/disk/by-label/broomwb,recurse=0\n\
         initrd http://{ip}/tftp/broom/{name}/initrd.img\n\
         boot"
    );
    {
        let c = st.db.lock().unwrap();
        c.execute("UPDATE images SET boot_script=?1 WHERE id=?2", params![bs, id])
            .map_err(|e| e.to_string())?;
    }
    save_hash(st, name, &img_abs.to_string_lossy());
    Ok(format!(
        "Publish OK — golden '{name}' iSCSI RO ({cache_mode}) + kernel/initrd + boot_script overlay"
    ))
}

/// Nạp golden img vào 1 zram device (nén zstd), trả /dev/zramN. Lưu map vào DB config.
/// Reset device cũ của image (nếu có) trước khi tạo mới.
fn ensure_zram(st: &SharedState, name: &str, img: &Path) -> Result<String, String> {
    // Reset device cũ nếu image từng ở zram.
    let old = {
        let c = st.db.lock().unwrap();
        db::get_config(&c, &format!("zram_dev:{name}"), "")
    };
    if !old.is_empty() {
        let _ = Command::new("zramctl").args(["--reset", &old]).status();
    }
    // module zram (zramctl --find cần nó); bỏ qua lỗi nếu đã load/builtin.
    let _ = Command::new("modprobe").arg("zram").status();
    let size = std::fs::metadata(img).map_err(|e| e.to_string())?.len();

    // VALIDATE tràn RAM: zram nén nhưng worst-case (data không nén được) = full img size.
    // Đòi MemAvailable còn dư > img size + reserve (chừa cho OS + iSCSI serving). device cũ
    // đã reset ở trên nên RAM nó đã trả lại; MemAvailable cũng phản ánh zram image KHÁC đang giữ.
    // reserve chỉnh qua config zram_reserve_mb (web mục Cấu hình).
    let reserve = {
        let c = st.db.lock().unwrap();
        db::get_config(&c, "zram_reserve_mb", "2048").parse::<u64>().unwrap_or(2048) * 1024 * 1024
    };
    let avail = mem_available_bytes();
    if !zram_fits(size, avail, reserve) {
        let gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
        return Err(format!(
            "zram sẽ tràn RAM: img {:.1}GB + reserve {:.1}GB > RAM khả dụng {:.1}GB. \
             Dùng cache_mode=disk, giảm reserve, hoặc tăng RAM server.",
            gb(size), gb(reserve), gb(avail)
        ));
    }

    // zramctl --find --size <bytes> --algorithm zstd → in /dev/zramN.
    let out = Command::new("zramctl")
        .args(["--find", "--size", &size.to_string(), "--algorithm", "zstd"])
        .output()
        .map_err(|e| format!("zramctl: {e} (cần modprobe zram?)"))?;
    if !out.status.success() {
        return Err(format!("zramctl --find: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if dev.is_empty() {
        return Err("zramctl không trả device".into());
    }
    // Nạp raw img vào block device zram.
    let ddout = Command::new("dd")
        .arg(format!("if={}", img.display()))
        .arg(format!("of={dev}"))
        .arg("bs=4M")
        .output()
        .map_err(|e| format!("dd → zram: {e}"))?;
    if !ddout.status.success() {
        let _ = Command::new("zramctl").args(["--reset", &dev]).status();
        return Err(format!("dd → zram: {}", String::from_utf8_lossy(&ddout.stderr).trim()));
    }
    {
        let c = st.db.lock().unwrap();
        let _ = db::set_config(&c, &format!("zram_dev:{name}"), &dev);
    }
    Ok(dev)
}

/// Dựng lại zram cho mọi image cache_mode=zram (gọi lúc mgmt start — zram mất khi reboot server).
pub fn repopulate_zram(st: &SharedState) {
    let names: Vec<String> = {
        let c = st.db.lock().unwrap();
        let mut stmt = match c.prepare("SELECT name FROM images WHERE cache_mode='zram'") {
            Ok(s) => s,
            Err(_) => return,
        };
        let rows = stmt.query_map([], |r| r.get::<_, String>(0));
        match rows {
            Ok(rs) => rs.flatten().collect(),
            Err(_) => return,
        }
    };
    for name in names {
        // Device cũ trong DB đã chết sau reboot → xoá map rồi publish lại (tạo zram mới + re-target).
        {
            let c = st.db.lock().unwrap();
            let _ = db::set_config(&c, &format!("zram_dev:{name}"), "");
        }
        // publish_iscsi tự hạ zram→disk nếu tràn RAM/lỗi → image luôn boot được.
        match publish_iscsi_by_name(st, &name) {
            Ok(msg) => eprintln!("[zram] repopulate '{name}': {msg}"),
            Err(e) => eprintln!("[zram] repopulate '{name}' lỗi: {e}"),
        }
    }
}

/// publish_iscsi theo name (tra id) — cho repopulate.
fn publish_iscsi_by_name(st: &SharedState, name: &str) -> Result<String, String> {
    let id: i64 = {
        let c = st.db.lock().unwrap();
        c.query_row("SELECT id FROM images WHERE name=?1", [name], |r| r.get(0))
            .map_err(|e| e.to_string())?
    };
    publish_iscsi(st, id, name)
}

// LTSP cũ — không còn gọi từ run_publish (đã chuyển iSCSI). Giữ tới B6 (gỡ sau PoC).
#[allow(dead_code)]
fn publish_linux(st: &SharedState, id: i64, name: &str) -> Result<String, String> {
    // Nguồn: <images_dir>/<name>/image.img → cần đường tuyệt đối cho hardlink.
    let src = images_dir().join(name).join("image.img");
    let src_abs = std::fs::canonicalize(&src)
        .map_err(|e| format!("chưa upload image ({}): {e}", src.display()))?;

    // LTSP đòi image ở /srv/ltsp/images/<name>.img. Hardlink (khỏi copy GB); cross-device thì copy.
    std::fs::create_dir_all("/srv/ltsp/images").map_err(|e| e.to_string())?;
    let dst = format!("/srv/ltsp/images/{name}.img");
    let _ = std::fs::remove_file(&dst);
    if std::fs::hard_link(&src_abs, &dst).is_err() {
        std::fs::copy(&src_abs, &dst).map_err(|e| format!("đưa image vào {dst}: {e}"))?;
    }

    // kernel; rồi ltsp.conf café + patch cache + ltsp initrd (ltsp::apply); rồi nfs.
    run("ltsp", &["kernel", &dst])?;
    {
        let c = st.db.lock().unwrap();
        crate::ltsp::apply(&c)?;
    }
    run("ltsp", &["nfs"])?;

    // boot_script (cmdline LTSP). ${mac:hexhyp}/${cmdline} để nguyên cho iPXE.
    let ip = {
        let c = st.db.lock().unwrap();
        db::get_config(&c, "dhcp_server_ip", "")
    };
    if ip.is_empty() {
        return Err("dhcp_server_ip trống — chạy `setup` trước để có IP server".into());
    }
    let bs = ltsp_boot_script(&ip, name);
    {
        let c = st.db.lock().unwrap();
        c.execute("UPDATE images SET boot_script=?1 WHERE id=?2", params![bs, id])
            .map_err(|e| e.to_string())?;
    }
    save_hash(st, name, &dst);
    Ok(format!(
        "Linux publish OK — kernel/initrd/nfs + boot_script đã set (image '{name}')"
    ))
}

#[cfg(test)]
mod tests {
    use super::{is_vmdk_descriptor, zram_fits};
    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn vmdk_descriptor_detect() {
        let dir = std::env::temp_dir();
        // descriptor = text.
        let d = dir.join("broom_test_desc.vmdk");
        std::fs::write(&d, "# Disk DescriptorFile\nversion=1\ncreateType=\"twoGbMaxExtentSparse\"\n").unwrap();
        assert!(is_vmdk_descriptor(&d));
        // extent = nhị phân (magic KDMV) → không phải descriptor.
        let e = dir.join("broom_test_ext.vmdk");
        std::fs::write(&e, b"KDMV\x01\x00\x00\x00binarygarbage").unwrap();
        assert!(!is_vmdk_descriptor(&e));
        let _ = std::fs::remove_file(&d);
        let _ = std::fs::remove_file(&e);
    }

    #[test]
    fn zram_fits_logic() {
        let reserve = 2 * GB;
        // img 4GB + 2GB reserve <= 8GB avail → vừa.
        assert!(zram_fits(4 * GB, 8 * GB, reserve));
        // img 40GB + 2GB > 16GB avail → tràn.
        assert!(!zram_fits(40 * GB, 16 * GB, reserve));
        // đúng biên: 6GB + 2GB == 8GB → vừa.
        assert!(zram_fits(6 * GB, 8 * GB, reserve));
        // hơn biên 1 byte → tràn.
        assert!(!zram_fits(6 * GB + 1, 8 * GB, reserve));
        // avail=0 (không đọc được meminfo) → luôn cho qua.
        assert!(zram_fits(999 * GB, 0, reserve));
    }
}
