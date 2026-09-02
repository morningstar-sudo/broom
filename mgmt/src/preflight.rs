// preflight.rs — kiểm gói/dịch vụ trước khi serve.
// Gói thiếu gom thành 1 lệnh apt; misc (file/service/quyền) báo riêng.
use std::path::Path;
use std::process::Command;

/// (binary, gói apt, mục đích) — nguồn chung cho preflight + setup.
pub const BINS: &[(&str, &str, &str)] = &[
    ("dnsmasq", "dnsmasq", "DHCP proxy/full + TFTP + DNS/hostname"),
    ("targetcli", "targetcli-fb", "iSCSI target (golden raw RO shared)"),
    ("qemu-img", "qemu-utils", "convert vmdk → raw img golden"),
    ("virt-copy-out", "libguestfs-tools", "đọc golden (kernel/initrd/UUID) an toàn, xử LVM"),
    ("iscsistart", "open-iscsi", "iSCSI initiator binary (inject vào initrd golden)"),
    ("zfs", "zfsutils-linux", "image store + snapshot/rollback (optional)"),
    ("zpool", "zfsutils-linux", "ZFS pool (optional)"),
    ("ping", "iputils-ping", "giám sát on/off (M7)"),
    ("unzip", "unzip", "giải nén bundle golden (.zip)"),
];

fn has_bin(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn svc_active(name: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

/// Danh sách gói apt còn thiếu (đã khử trùng lặp).
pub fn missing_pkgs() -> Vec<String> {
    let mut pkgs: Vec<String> = Vec::new();
    for (bin, pkg, _why) in BINS {
        if !has_bin(bin) {
            let p = pkg.to_string();
            if !pkgs.contains(&p) {
                pkgs.push(p);
            }
        }
    }
    pkgs
}

/// Kết quả preflight khi fail.
pub struct Report {
    pub pkgs: Vec<String>,
    pub other: Vec<String>,
}

impl Report {
    pub fn install_cmd(&self) -> Option<String> {
        if self.pkgs.is_empty() {
            None
        } else {
            Some(format!("sudo apt install -y {}", self.pkgs.join(" ")))
        }
    }
}

pub fn run() -> Result<(), Report> {
    let pkgs = missing_pkgs();
    let mut other: Vec<String> = Vec::new();

    if !Path::new("/srv/tftp/snponly.efi").exists() {
        other.push("/srv/tftp/snponly.efi → chạy `bootrom-mgmt setup`".into());
    }
    if !svc_active("dnsmasq") {
        other.push("dnsmasq chưa chạy → `bootrom-mgmt setup` (hoặc systemctl enable --now dnsmasq)".into());
    }
    if !is_root() {
        other.push("cần chạy bằng root (targetcli/qemu-img/zram/reload dnsmasq/bind :80)".into());
    }

    if pkgs.is_empty() && other.is_empty() {
        Ok(())
    } else {
        Err(Report { pkgs, other })
    }
}
