// publish.rs — after a golden is uploaded, process it automatically so the image can boot.
// Linux: golden raw (from vmdk) → shared RO iSCSI (disk or zram) + kernel/initrd (overlay.rs)
// → iPXE boot_script loads kernel+initrd + attaches iSCSI + SSD overlay.
// Windows: winstage.rs (native VHDX boot from the client SSD).
use rusqlite::params;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::{db, images_dir, SharedState};

/// Publish job progress: each step shows "⏳ <step>..." on the web + is timed; the ✓ result includes
/// a per-step timing table → you see right away where it is slow.
pub(crate) struct Steps<'a> {
    st: &'a SharedState,
    name: String,
    cur: Option<(String, std::time::Instant)>,
    done: Vec<String>,
}

impl<'a> Steps<'a> {
    pub fn new(st: &'a SharedState, name: &str) -> Self {
        Steps { st, name: name.to_string(), cur: None, done: Vec::new() }
    }
    /// Finish the running step (record its time), start a new one.
    pub fn go(&mut self, label: &str) {
        self.end();
        eprintln!("[publish {}] {label}...", self.name);
        self.st.jobs.lock().unwrap().insert(self.name.clone(), format!("⏳ {label}..."));
        self.cur = Some((label.to_string(), std::time::Instant::now()));
    }
    fn end(&mut self) {
        if let Some((l, t)) = self.cur.take() {
            let s = t.elapsed().as_secs();
            self.done.push(if s >= 60 { format!("{l} {}m{:02}s", s / 60, s % 60) } else { format!("{l} {s}s") });
        }
    }
    /// "step1 12s, step2 3m05s, ..."
    pub fn summary(mut self) -> String {
        self.end();
        self.done.join(", ")
    }
}

/// Move a file (rename, fall back to copy across filesystems).
fn mv(src: &Path, dst: &Path) -> Result<(), String> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
    let _ = std::fs::remove_file(src);
    Ok(())
}

/// sha256 of a file (clients compare it to detect a golden change). None on error.
pub(crate) fn file_hash(path: &str) -> Option<String> {
    let out = Command::new("sha256sum").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(|s| s.to_string())
}

/// Normalize the uploaded source into raw `dest` (image.img). Blocking.
/// src: "raw" (already an img → rename) | "vmdk" (qemu-img convert) | "zip" (unzip → vmdk/img → convert).
pub fn prepare_golden(src: &str, uploaded: &Path, dest: &Path) -> Result<(), String> {
    match src {
        "raw" => {
            // uploaded == <dest>.uploading → rename to image.img.
            mv(uploaded, dest)
        }
        "vmdk" => {
            // -m 16: 16 parallel I/O coroutines (default 8); -W: out-of-order writes (sparse raw target).
            run("qemu-img", &["convert", "-m", "16", "-W", "-O", "raw",
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
            // Pick the vmdk to convert: a .vmx in the zip names the disk the VM ACTUALLY uses (even with a
            // branching snapshot tree) → preferred. No .vmx: one file → use it; several → pick_vmdk.
            // qemu-img reads extents/parents from the same directory.
            let from_vmx = walk(&exdir)
                .into_iter()
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("vmx"))
                .find_map(|vmx| {
                    let disk = vmx_disk(&std::fs::read_to_string(&vmx).ok()?)?;
                    let p = vmx.with_file_name(disk);
                    p.exists().then_some(p)
                });
            let chosen_vmdk = if from_vmx.is_some() {
                from_vmx
            } else if vmdks.len() == 1 {
                Some(vmdks.remove(0))
            } else {
                pick_vmdk(&vmdks)
            };
            if let Some(v) = &chosen_vmdk {
                eprintln!("[golden] zip: convert {}", v.display());
            }
            let out = if let Some(v) = chosen_vmdk {
                run("qemu-img", &["convert", "-m", "16", "-W", "-O", "raw",
                    &v.to_string_lossy(), &dest.to_string_lossy()])?;
                Ok(())
            } else if let Some(r) = raw {
                mv(&r, dest)
            } else if !vmdks.is_empty() {
                Err("zip has several .vmdk files but no descriptor file — export the VM as a single monolithic vmdk and upload again".into())
            } else {
                Err("zip contains no .vmdk/.img/.raw".into())
            };
            let _ = std::fs::remove_dir_all(&exdir);
            let _ = std::fs::remove_file(uploaded);
            out
        }
        other => Err(format!("invalid src: {other}")),
    }
}

/// Head of a vmdk file (text descriptor, or the descriptor embedded at sector 1 of a monolithicSparse).
fn vmdk_head(p: &Path) -> String {
    use std::io::Read;
    let mut buf = [0u8; 8192];
    let n = std::fs::File::open(p).and_then(|mut f| f.read(&mut buf)).unwrap_or(0);
    String::from_utf8_lossy(&buf[..n]).to_string()
}

/// vmdk descriptor = text file (contains "# Disk DescriptorFile" / "createType") pointing to extents.
/// Otherwise = binary extent (monolithicSparse starts with magic "KDMV") — can't be converted on its own.
fn is_vmdk_descriptor(p: &Path) -> bool {
    let head = vmdk_head(p);
    head.contains("# Disk DescriptorFile") || head.contains("createType")
}

/// First disk the VM uses according to the .vmx: line `<bus>N:M.fileName = "x.vmdk"` (skip CD/ISO).
fn vmx_disk(vmx: &str) -> Option<String> {
    vmx.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        let v = v.trim().trim_matches('"');
        (k.trim().ends_with(".fileName") && v.to_ascii_lowercase().ends_with(".vmdk"))
            .then(|| v.rsplit(['\\', '/']).next().unwrap_or(v).to_string())
    })
}

/// The CURRENT descriptor among the vmdk files (split + snapshots): the descriptor that is not the parent
/// (`parentFileNameHint`) of any other descriptor = top of the snapshot chain. Not by name:
/// "win.vmdk" > "win-000001.vmdk" as strings, yet 000001 is the newest state.
fn pick_vmdk(vmdks: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    let desc: Vec<_> = vmdks.iter().filter(|p| is_vmdk_descriptor(p)).collect();
    let parents: Vec<String> = desc
        .iter()
        .filter_map(|p| {
            let h = vmdk_head(p);
            let v = h.split("parentFileNameHint=\"").nth(1)?.split('"').next()?.to_string();
            Some(v.rsplit(['\\', '/']).next().unwrap_or(&v).to_ascii_lowercase())
        })
        .collect();
    let mut top: Vec<_> = desc
        .into_iter()
        .filter(|p| {
            let n = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            !parents.contains(&n)
        })
        .cloned()
        .collect();
    top.sort();
    top.pop()
}

/// Does the img fit in RAM for zram: needs avail >= img + reserve. avail=0 (unreadable) → allow.
fn zram_fits(img: u64, avail: u64, reserve: u64) -> bool {
    avail == 0 || img.saturating_add(reserve) <= avail
}

/// Available RAM (bytes) from /proc/meminfo MemAvailable. 0 if unreadable (check skipped).
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

/// Walk the files in dir (recursive). ponytail: enough for a golden zip with a few files.
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
            "{bin} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Publish an image according to its os. Blocking (called from spawn_blocking).
pub fn run_publish(st: &SharedState, name: &str, steps: &mut Steps) -> Result<String, String> {
    let (id, os): (i64, String) = {
        let c = st.db.lock().unwrap();
        c.query_row("SELECT id, os FROM images WHERE name=?1", [name], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(|e| format!("image '{name}' not found in DB: {e}"))?
    };
    match os.as_str() {
        "linux" => {
            steps.go("publish linux (kernel/initrd + iSCSI)");
            publish_iscsi(st, id, name)
        }
        "windows" => crate::winstage::publish(st, id, name, steps),
        other => Err(format!("invalid os: {other} (linux|windows)")),
    }
}

/// Run a batch of targetcli commands via stdin.
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
    } // close stdin → targetcli processes and exits
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Golden Linux: build kernel/initrd (overlay.rs) + serve iSCSI RO shared (disk|zram) +
/// iPXE boot_script (loads kernel/initrd, the initrd attaches iSCSI + SSD overlay itself). Blocking.
fn publish_iscsi(st: &SharedState, id: i64, name: &str) -> Result<String, String> {
    let img = images_dir().join(name).join("image.img");
    let img_abs = std::fs::canonicalize(&img)
        .map_err(|e| format!("no golden raw yet ({}): {e}", img.display()))?;

    // 1. Extract kernel + initrd from the golden → /srv/tftp/broom/<name>/, read the root UUID.
    let root_uuid = crate::overlay::build_boot(&img_abs, name)?;

    // 2. cache_mode (images column): disk → serve the file directly; zram → load the img into /dev/zramN.
    // zram fails (RAM overflow / error) → fall back to disk BY ITSELF (DB updated) so the image always boots.
    let want = {
        let c = st.db.lock().unwrap();
        c.query_row("SELECT cache_mode FROM images WHERE id=?1", [id], |r| r.get::<_, String>(0))
            .unwrap_or_else(|_| "disk".into())
    };
    let (cache_mode, backing) = if want == "zram" {
        match ensure_zram(st, name, &img_abs) {
            Ok(dev) => ("zram".to_string(), dev),
            Err(e) => {
                eprintln!("[zram] '{name}': {e} → falling back to cache_mode=disk");
                let c = st.db.lock().unwrap();
                let _ = c.execute("UPDATE images SET cache_mode='disk' WHERE id=?1", [id]);
                ("disk".to_string(), img_abs.to_string_lossy().to_string())
            }
        }
    } else {
        ("disk".to_string(), img_abs.to_string_lossy().to_string())
    };

    // 3. Shared RO iSCSI target. Idempotent: remove the old one first.
    let iqn = {
        let c = st.db.lock().unwrap();
        format!("{}:{name}", db::get_config(&c, "iqn_base", "iqn.2026-01.local.broom"))
    };
    let _ = targetcli_script(&format!("cd /iscsi\ndelete {iqn}\nexit\n"));
    // ponytail: target named by the old fixed IQN (before iqn_base) — remove it too; drop this line later.
    let _ = targetcli_script(&format!("cd /iscsi\ndelete iqn.2026-08.net.tiem:{name}\nexit\n"));
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
    targetcli_script(&script).map_err(|e| format!("targetcli create target: {e}"))?;

    // 4. iPXE boot_script. The initrd reads broom.iscsi / broom.ssd from the cmdline.
    let ip = {
        let c = st.db.lock().unwrap();
        db::get_config(&c, "dhcp_server_ip", "")
    };
    if ip.is_empty() {
        return Err("dhcp_server_ip is empty — run `setup` first".into());
    }
    // sanhook = iPXE attaches iSCSI via iBFT (does not boot the LUN); initrd open-iscsi reads the iBFT →
    // /dev/sda golden RO → root=UUID mounted RO; overlayroot (baked into the golden) overlays it onto the
    // SSD writeback (reset every boot). ip=dhcp gives the initrd a network.
    // overlayroot on the CMDLINE (takes precedence over the conf file) → root RO + upper on the SSD LABEL broomwb.
    // `quiet` left out so overlayroot/broom logs are visible during the PoC.
    // broom.name/hash/size: the initrd hook compares the hash with the SSD cache copy (match → boot from the SSD,
    // skip iSCSI; mismatch → iSCSI + background copy). Hash computed FIRST to embed it in the cmdline.
    let img_str = img_abs.to_string_lossy().to_string();
    let hash = file_hash(&img_str).ok_or("sha256sum golden failed")?;
    let size = std::fs::metadata(&img_abs).map_err(|e| e.to_string())?.len();
    let bs = format!(
        "sanhook iscsi:{ip}::::{iqn} || shell\n\
         kernel http://{ip}/tftp/broom/{name}/vmlinuz initrd=initrd.img ip=dhcp root=UUID={root_uuid} ro fsck.mode=skip overlayroot=device:dev=/dev/disk/by-label/broomwb,recurse=0 broom.name={name} broom.hash={hash} broom.size={size}\n\
         initrd http://{ip}/tftp/broom/{name}/initrd.img\n\
         boot"
    );
    {
        let c = st.db.lock().unwrap();
        c.execute("UPDATE images SET boot_script=?1, hash=?2 WHERE id=?3", params![bs, hash, id])
            .map_err(|e| e.to_string())?;
    }
    Ok(format!(
        "Publish OK — golden '{name}' iSCSI RO ({cache_mode}) + kernel/initrd + boot_script overlay"
    ))
}

/// Load the golden img into a zram device (zstd compressed), return /dev/zramN. Map stored in DB config.
/// Reset the image's old device (if any) before creating a new one.
fn ensure_zram(st: &SharedState, name: &str, img: &Path) -> Result<String, String> {
    // Reset the old device if the image was on zram before.
    let old = {
        let c = st.db.lock().unwrap();
        db::get_config(&c, &format!("zram_dev:{name}"), "")
    };
    if !old.is_empty() {
        let _ = Command::new("zramctl").args(["--reset", &old]).status();
    }
    // zram module (zramctl --find needs it); ignore errors if already loaded/builtin.
    let _ = Command::new("modprobe").arg("zram").status();
    let size = std::fs::metadata(img).map_err(|e| e.to_string())?.len();

    // VALIDATE RAM overflow: zram compresses but worst case (incompressible data) = full img size.
    // Require MemAvailable > img size + reserve (kept for the OS + iSCSI serving). The old device
    // was reset above so its RAM is returned; MemAvailable also reflects OTHER zram images being held.
    // reserve is set via the zram_reserve_mb config (web System page).
    let reserve = {
        let c = st.db.lock().unwrap();
        db::get_config(&c, "zram_reserve_mb", "2048").parse::<u64>().unwrap_or(2048) * 1024 * 1024
    };
    let avail = mem_available_bytes();
    if !zram_fits(size, avail, reserve) {
        let gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
        return Err(format!(
            "zram would overflow RAM: img {:.1}GB + reserve {:.1}GB > available RAM {:.1}GB. \
             Use cache_mode=disk, lower the reserve, or add RAM to the server.",
            gb(size), gb(reserve), gb(avail)
        ));
    }

    // zramctl --find --size <bytes> --algorithm zstd → in /dev/zramN.
    let out = Command::new("zramctl")
        .args(["--find", "--size", &size.to_string(), "--algorithm", "zstd"])
        .output()
        .map_err(|e| format!("zramctl: {e} (modprobe zram needed?)"))?;
    if !out.status.success() {
        return Err(format!("zramctl --find: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if dev.is_empty() {
        return Err("zramctl returned no device".into());
    }
    // Load the raw img into the zram block device.
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

/// Rebuild zram for every image with cache_mode=zram (called at mgmt start — zram is lost on server reboot).
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
        // The old device in the DB died with the reboot → clear the map then publish again (new zram + re-target).
        {
            let c = st.db.lock().unwrap();
            let _ = db::set_config(&c, &format!("zram_dev:{name}"), "");
        }
        // publish_iscsi falls back zram→disk by itself on RAM overflow/error → the image always boots.
        match publish_iscsi_by_name(st, &name) {
            Ok(msg) => eprintln!("[zram] repopulate '{name}': {msg}"),
            Err(e) => eprintln!("[zram] repopulate '{name}' failed: {e}"),
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

#[cfg(test)]
mod tests {
    use super::{is_vmdk_descriptor, pick_vmdk, vmx_disk, zram_fits};

    /// .vmx: take the disk in use (current snapshot), skip CD/ISO.
    #[test]
    fn vmx_current_disk() {
        let vmx = "displayName = \"win\"\n\
                   sata0:1.fileName = \"D:\\\\iso\\\\win11.iso\"\n\
                   nvme0:0.fileName = \"win-000003.vmdk\"\n\
                   nvme0:0.present = \"TRUE\"\n";
        assert_eq!(vmx_disk(vmx).unwrap(), "win-000003.vmdk");
        assert_eq!(vmx_disk("sata0:1.fileName = \"auto detect\"\n"), None);
    }

    /// VM with snapshots: pick the top of the chain (000002), not the base file whose name sorts "higher".
    #[test]
    fn pick_vmdk_snapshot_top() {
        let d = std::env::temp_dir().join("broom_test_snap");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let w = |n: &str, parent: &str| {
            let hint = if parent.is_empty() { String::new() } else { format!("parentFileNameHint=\"C:\\VMs\\win\\{parent}\"\n") };
            std::fs::write(d.join(n), format!("# Disk DescriptorFile\ncreateType=\"twoGbMaxExtentSparse\"\n{hint}")).unwrap();
            d.join(n)
        };
        let v = vec![
            w("win.vmdk", ""),
            w("win-000001.vmdk", "win.vmdk"),
            w("win-000002.vmdk", "win-000001.vmdk"),
        ];
        let mut all = v.clone();
        std::fs::write(d.join("win-s001.vmdk"), b"KDMV\x01binary").unwrap();
        all.push(d.join("win-s001.vmdk"));
        assert_eq!(pick_vmdk(&all).unwrap(), d.join("win-000002.vmdk"));
        assert_eq!(pick_vmdk(&v[..2]).unwrap(), d.join("win-000001.vmdk"));
        assert_eq!(pick_vmdk(&v[..1]).unwrap(), d.join("win.vmdk"));
        let _ = std::fs::remove_dir_all(&d);
    }
    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn vmdk_descriptor_detect() {
        let dir = std::env::temp_dir();
        // descriptor = text.
        let d = dir.join("broom_test_desc.vmdk");
        std::fs::write(&d, "# Disk DescriptorFile\nversion=1\ncreateType=\"twoGbMaxExtentSparse\"\n").unwrap();
        assert!(is_vmdk_descriptor(&d));
        // extent = binary (magic KDMV) → not a descriptor.
        let e = dir.join("broom_test_ext.vmdk");
        std::fs::write(&e, b"KDMV\x01\x00\x00\x00binarygarbage").unwrap();
        assert!(!is_vmdk_descriptor(&e));
        let _ = std::fs::remove_file(&d);
        let _ = std::fs::remove_file(&e);
    }

    #[test]
    fn zram_fits_logic() {
        let reserve = 2 * GB;
        // img 4GB + 2GB reserve <= 8GB avail → fits.
        assert!(zram_fits(4 * GB, 8 * GB, reserve));
        // img 40GB + 2GB > 16GB avail → overflows.
        assert!(!zram_fits(40 * GB, 16 * GB, reserve));
        // exactly at the limit: 6GB + 2GB == 8GB → fits.
        assert!(zram_fits(6 * GB, 8 * GB, reserve));
        // 1 byte over the limit → overflows.
        assert!(!zram_fits(6 * GB + 1, 8 * GB, reserve));
        // avail=0 (meminfo unreadable) → always allowed.
        assert!(zram_fits(999 * GB, 0, reserve));
    }
}
