// capture.rs — `bootrom-mgmt capture <server>`: runs on a client PC (boot menu [C] → the Windows stage in capture mode
// downloads this binary from /broom-capture), signed in with an owner account typed on the screen, through the web's
// own API. Two ways:
// - Capture: the PC's Windows disk → an image's new golden (upload API). The disk is only read. Sent: the partition
//   tables (first + last MiB), the clusters in use of each NTFS partition ($Bitmap), every other partition whole.
//   Everything else stays a hole in the server's upload, which converts + publishes like an .img.
// - Deploy: an image's whole disk (export.rs: the golden with its boot partitions) → this PC's disk, as real data
//   (no VHDX): Windows boots natively from it — edit it, run the prep again, Capture it back.
use parking_lot::Mutex;
use std::fs::File;
use std::io::{BufRead, Write};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const MIB: u64 = 1 << 20;
const CHUNK: u64 = crate::images::CHUNK_MAX as u64;
/// Chunks read + sent at once (the web upload uses as many).
const THREADS: usize = 4;
/// An NTFS partition above this is likely the PC's whole disk: every client would get a C: that big.
const BIG_PART: u64 = 256 << 30;
/// Left by a Deploy for the stage: "<disk> <ESP partition number>" → a firmware boot entry for the new Windows.
const DEPLOYED: &str = "/run/broom-deploy";

pub fn run(args: &[String]) -> ! {
    let code = match args.get(2) {
        None => {
            eprintln!("usage: bootrom-mgmt capture <server ip>");
            2
        }
        Some(srv) => match tool(srv) {
            Ok(()) => 0,
            Err(e) => {
                println!("\nStopped: {e}");
                1
            }
        },
    };
    std::process::exit(code)
}

fn tool(srv: &str) -> Result<(), String> {
    println!("\nbroom: this PC's disk <-> the server {srv}\n");
    let api = login(srv)?;
    loop {
        let a = ask(
            "\n  1) Capture: this PC's disk -> an image on the server (the disk is only read)\n  \
             2) Deploy: an image on the server -> this PC's disk (the disk is ERASED)\nChoose [1-2]: ",
        )?;
        match a.as_str() {
            "1" => return capture(&api),
            "2" => return deploy(&api),
            _ => {}
        }
    }
}

/// Sign in with an owner account typed here (asked again after a wrong password or a staff account).
fn login(srv: &str) -> Result<Api, String> {
    loop {
        let user = ask("Owner account [admin]: ")?;
        let user = if user.is_empty() {
            "admin".into()
        } else {
            user
        };
        let api = Api::new(srv, user, ask_secret("Password: ")?);
        match api.login() {
            Ok(()) => {}
            Err((401 | 429, m)) => {
                println!("  {m}");
                continue;
            }
            Err((_, m)) => return Err(m),
        }
        let st: Value = parse(&api.req("GET", "/api/auth/status", b"")?)?;
        if st["role"] != "owner" {
            println!("  {} is not an owner account - this needs one", api.user);
            continue;
        }
        return Ok(api);
    }
}

/// One of `n` numbered choices (Enter = the only one).
fn pick(q: &str, n: usize) -> Result<usize, String> {
    loop {
        let a = ask(&format!("{q} [1-{n}]: "))?;
        if a.is_empty() && n == 1 {
            return Ok(0);
        }
        if let Some(i) = a.parse::<usize>().ok().filter(|i| (1..=n).contains(i)) {
            return Ok(i - 1);
        }
    }
}

fn show_disks(disks: &[Disk]) {
    for (i, d) in disks.iter().enumerate() {
        println!(
            "  [{}] {:<14} {:>9}  {}",
            i + 1,
            d.dev,
            size_str(d.size),
            d.model
        );
    }
}

fn capture(api: &Api) -> Result<(), String> {
    let disks = disks();
    if disks.is_empty() {
        return Err("no internal disk found".into());
    }
    show_disks(&disks);
    let disk = &disks[pick("Disk with Windows", disks.len())?];

    println!("\nReading the partitions of {} ...", disk.dev);
    let plan = plan(&disk.dev)?;
    for l in &plan.lines {
        println!("  {l}");
    }
    let total: u64 = plan.ranges.iter().map(|r| r.1).sum();
    println!(
        "To send: {} of the {} disk (free space is skipped)",
        size_str(total),
        size_str(plan.size)
    );
    for w in &plan.warn {
        println!("\nWARNING: {w}");
        if !yes("Capture anyway?")? {
            return Err("stopped by the user".into());
        }
    }

    let name = loop {
        let n = ask("\nImage name on the server (letters, digits, _ or -): ")?;
        if crate::images::valid_name(&n) {
            break n;
        }
        println!("  1-64 letters, digits, _ or -");
    };
    let images: Value = parse(&api.req("GET", "/api/images", b"")?)?;
    let existing = images
        .as_array()
        .and_then(|v| v.iter().find(|i| i["name"] == name.as_str()));
    match existing {
        Some(i) if i["os"] != "windows" => {
            return Err(format!(
                "image {name} is a Linux image - capture again with another name"
            ));
        }
        Some(_) => {
            if !yes(&format!(
                "Image {name} exists: its golden is replaced by this disk (its saved versions stay)."
            ))? {
                return Err("stopped by the user".into());
            }
        }
        None => {
            let b = json!({"name": name, "os": "windows"}).to_string();
            api.req("POST", "/api/images", b.as_bytes())?;
            println!("Image {name} created");
        }
    }
    let b = json!({"name": name}).to_string();
    api.req("POST", "/api/images/upload-start", b.as_bytes())?;
    send(api, &disk.dev, &name, plan.size, &plan.ranges)?;
    api.req("POST", "/api/images/upload-done", b.as_bytes())?;
    println!("\nAll sent. The server now converts + publishes the image (a few minutes):");
    wait_job(api, &name)
}

fn deploy(api: &Api) -> Result<(), String> {
    let images: Value = parse(&api.req("GET", "/api/images", b"")?)?;
    let images: Vec<&Value> = images
        .as_array()
        .map(|v| {
            v.iter()
                .filter(|i| i["os"] == "windows" && i["size"].is_u64())
                .collect()
        })
        .unwrap_or_default();
    if images.is_empty() {
        return Err("no Windows image with a golden on the server".into());
    }
    for (i, img) in images.iter().enumerate() {
        println!(
            "  [{}] {:<24} {:>9}",
            i + 1,
            img["name"].as_str().unwrap_or(""),
            size_str(img["size"].as_u64().unwrap_or(0))
        );
    }
    let name = images[pick("Image to deploy", images.len())?]["name"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let map: Value =
        parse(&api.req("GET", &format!("/api/images/deploy-map?name={name}"), b"")?)?;
    let total = map["total"].as_u64().ok_or("server answer: no size")?;
    let stamp = map["stamp"].as_str().unwrap_or("").to_string();
    let ranges: Vec<(u64, u64)> = map["ranges"]
        .as_array()
        .map(|v| {
            v.iter()
                .filter_map(|r| Some((r[0].as_u64()?, r[1].as_u64()?)))
                .map(|(s, e)| (s, e - s))
                .collect()
        })
        .unwrap_or_default();
    if map["firmware"] == "bios" && std::path::Path::new("/sys/firmware/efi").exists() {
        println!(
            "\nWARNING: {name} boots with BIOS (MBR), this PC started in UEFI mode: it won't boot it as it is."
        );
        if !yes("Deploy anyway?")? {
            return Err("stopped by the user".into());
        }
    }

    println!("\n{name} needs a disk of at least {}:", size_str(total));
    let disks: Vec<Disk> = disks().into_iter().filter(|d| d.size >= total).collect();
    if disks.is_empty() {
        return Err(format!("no disk in this PC holds {}", size_str(total)));
    }
    show_disks(&disks);
    let disk = &disks[pick("Disk to write it to", disks.len())?];
    let parts = crate::disk::read(&disk.dev)
        .ok()
        .flatten()
        .map_or(0, |t| t.parts.len());
    println!(
        "\n{} has {parts} partition(s) now - ALL of it is erased.",
        disk.dev
    );
    if broom_ssd(&disk.dev) {
        println!(
            "It is this PC's Broom SSD: its cached images and Windows go; the stage sets it up again at a later PXE boot."
        );
    }
    let short = disk.dev.rsplit('/').next().unwrap_or(&disk.dev).to_string();
    if ask(&format!("Type {short} to erase it and write {name}: "))? != short {
        return Err("stopped by the user".into());
    }

    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&disk.dev)
        .map_err(|e| format!("{}: {e}", disk.dev))?;
    discard(&f, disk.size);
    let list = pieces(&ranges, CHUNK);
    let t0 = Instant::now();
    run_pieces(&list, |off, b| {
        let path = format!(
            "/api/images/deploy-chunk?name={name}&stamp={stamp}&offset={off}&len={}",
            b.len()
        );
        let data = retry(|| api.call("GET", &path, b""))?;
        if data.len() != b.len() {
            return Err(format!("short answer at {off}"));
        }
        f.write_all_at(&data, off)
            .map_err(|e| format!("write {} at {off}: {e}", disk.dev))?;
        Ok(true)
    })?;
    // What the image has as holes must read back as zeros (empty FAT / $Bitmap areas, the rest of the GPT array):
    // TRIM does not promise that, so they are zeroed for real.
    println!("  zeroing the rest of the image's {}", size_str(total));
    let mut at = 0;
    for &(s, len) in ranges.iter().chain([(total, 0)].iter()) {
        if s > at {
            zero(&f, at, s - at).map_err(|e| format!("{}: zero at {at}: {e}", disk.dev))?;
        }
        at = at.max(s + len);
    }
    f.sync_all().map_err(|e| format!("{}: {e}", disk.dev))?;
    drop(f);
    if crate::disk::move_backup_gpt(&disk.dev)? {
        println!(
            "  partition table moved to the end of the bigger disk (extend C: in Windows if needed)"
        );
    }
    println!(
        "  written {} in {}",
        size_str(list.iter().map(|p| p.1).sum()),
        mins(t0.elapsed().as_secs_f64())
    );
    // The firmware needs a boot entry on this disk's EFI partition: the stage makes it (efibootmgr).
    let esp = crate::disk::read(&disk.dev)?.and_then(|t| {
        t.parts
            .iter()
            .position(|p| p.kind.eq_ignore_ascii_case(crate::disk::ESP) || p.kind == "ef")
    });
    if let (Some(i), true) = (esp, disk.dev.starts_with("/dev/")) {
        let _ = std::fs::write(DEPLOYED, format!("{short} {}\n", i + 1));
    }
    println!(
        "\n{name} is on {}. Next: it starts Windows from this disk (setup runs once).",
        disk.dev
    );
    println!(
        "Edit it, run the prep command again (it shuts down), then PXE-boot and Capture it back."
    );
    Ok(())
}

/// The disk holds broom's partitions (BROOMEFI / BROOMWIN, by their GPT names in sysfs).
fn broom_ssd(dev: &str) -> bool {
    let n = dev.rsplit('/').next().unwrap_or(dev);
    std::fs::read_dir(format!("/sys/block/{n}"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(n))
        .any(|e| {
            std::fs::read_to_string(e.path().join("uevent")).is_ok_and(|u| {
                u.lines()
                    .any(|l| l == "PARTNAME=BROOMEFI" || l == "PARTNAME=BROOMWIN")
            })
        })
}

/// Zeros over [off, off+len): the disk does it itself (BLKZEROOUT: write-zeroes / unmap on SSDs, fast); a file gets
/// a hole; else written.
fn zero(f: &File, off: u64, len: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    const BLKZEROOUT: libc::c_ulong = 0x127f; // _IO(0x12, 127)
    let range = [off, len];
    // SAFETY: BLKZEROOUT reads two u64 (start, length) from the pointer.
    if unsafe { libc::ioctl(f.as_raw_fd(), BLKZEROOUT as _, range.as_ptr()) } == 0 {
        return Ok(());
    }
    // SAFETY: fallocate on an open fd, no memory involved.
    let punch = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    if unsafe { libc::fallocate(f.as_raw_fd(), punch, off as i64, len as i64) } == 0 {
        return Ok(());
    }
    let buf = vec![0u8; CHUNK as usize];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(CHUNK) as usize;
        f.write_all_at(&buf[..n], off + done)?;
        done += n as u64;
    }
    Ok(())
}

/// TRIM the whole disk before writing: the old data is gone, and what is not written reads back as zeros (the next
/// Capture skips it). Not a block device / no TRIM: nothing.
fn discard(f: &File, len: u64) {
    use std::os::fd::AsRawFd;
    const BLKDISCARD: libc::c_ulong = 0x1277; // _IO(0x12, 119)
    let range = [0u64, len];
    // SAFETY: BLKDISCARD reads two u64 (start, length) from the pointer; any error is ignored.
    unsafe { libc::ioctl(f.as_raw_fd(), BLKDISCARD as _, range.as_ptr()) };
}

/// A disk of this machine.
struct Disk {
    dev: String,
    size: u64,
    model: String,
}

/// Internal disks, like the stage's own scan: not removable, not on USB, size > 0. BROOM_CAPTURE_DISK (tests): a disk
/// image file instead.
fn disks() -> Vec<Disk> {
    if let Ok(p) = std::env::var("BROOM_CAPTURE_DISK") {
        let size = File::open(&p)
            .and_then(|f| crate::disk::size(&f))
            .unwrap_or(0);
        return vec![Disk {
            dev: p,
            size,
            model: "(file)".into(),
        }];
    }
    let mut v = Vec::new();
    for e in std::fs::read_dir("/sys/block")
        .into_iter()
        .flatten()
        .flatten()
    {
        let n = e.file_name().to_string_lossy().into_owned();
        if ["loop", "ram", "dm-", "sr", "nbd", "md", "fd", "zram"]
            .iter()
            .any(|p| n.starts_with(p))
        {
            continue;
        }
        let d = e.path();
        let rd = |f: &str| {
            std::fs::read_to_string(d.join(f))
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        let usb = std::fs::canonicalize(&d).is_ok_and(|p| p.to_string_lossy().contains("/usb"));
        let size = rd("size").parse::<u64>().unwrap_or(0) * 512;
        if rd("removable") == "1" || usb || size == 0 {
            continue;
        }
        v.push(Disk {
            dev: format!("/dev/{n}"),
            size,
            model: rd("device/model"),
        });
    }
    v.sort_by(|a, b| a.dev.cmp(&b.dev));
    v
}

/// What will be read of a disk.
struct Plan {
    size: u64,
    /// (offset, length), whole MiB, sorted, not touching.
    ranges: Vec<(u64, u64)>,
    /// One line per partition, for the screen.
    lines: Vec<String>,
    /// Things the user must confirm.
    warn: Vec<String>,
}

fn plan(dev: &str) -> Result<Plan, String> {
    let f = File::open(dev).map_err(|e| format!("{dev}: {e}"))?;
    let size = crate::disk::size(&f).map_err(|e| format!("{dev}: {e}"))?;
    if size > crate::images::MAX_UPLOAD {
        return Err(format!(
            "the disk is {}: above the server's limit",
            size_str(size)
        ));
    }
    let t =
        crate::disk::read(dev)?.ok_or("no partition table on this disk (not a Windows disk)")?;
    let mut want = vec![(0, MIB), (size.saturating_sub(MIB), MIB)];
    let (mut lines, mut warn) = (Vec::new(), Vec::new());
    for (i, p) in t.parts.iter().enumerate() {
        let name = format!("partition {}", i + 1);
        let mut bs = [0u8; 512];
        f.read_exact_at(&mut bs, p.start)
            .map_err(|e| format!("{dev}: read {name}: {e}"))?;
        match fs_kind(&bs) {
            "BitLocker" => {
                return Err(format!(
                    "{name} is encrypted (BitLocker): in Windows run 'manage-bde -off C:', wait until it is fully \
                     decrypted, then capture again"
                ));
            }
            "NTFS" => {
                let mut v = crate::ntfsread::Vol::open(dev, (p.start, p.size))
                    .map_err(|e| format!("{name}: {e}"))?;
                if v.head("hiberfil.sys", 4)?
                    .is_some_and(|h| h.eq_ignore_ascii_case(b"hibr"))
                {
                    warn.push(format!(
                        "{name}: Windows is hibernated (Fast Startup) - part of it is only in RAM. Start Windows, \
                         then shut it down with 'shutdown /s /t 0' (or run the prep script, it turns hibernation off)."
                    ));
                } else if v.is_dirty()? {
                    warn.push(format!(
                        "{name} was not shut down cleanly (power cut or crash): files may be half-written"
                    ));
                }
                let used = v.used_ranges()?;
                let n: u64 = used.iter().map(|r| r.1).sum();
                want.extend(used.iter().map(|&(o, l)| (p.start + o, l)));
                // The backup boot sector sits after the last cluster.
                want.push((p.start + p.size.saturating_sub(MIB), MIB.min(p.size)));
                lines.push(format!(
                    "{name}: NTFS {}, {} in use",
                    size_str(p.size),
                    size_str(n)
                ));
                if p.size > BIG_PART {
                    warn.push(format!(
                        "{name} is {}: every client gets a C: that big (it needs room for its changes on the \
                         client SSD). Shrink C: in Disk Management first if that is too much.",
                        size_str(p.size)
                    ));
                }
            }
            what => {
                want.push((p.start, p.size));
                lines.push(format!("{name}: {what} {}, read whole", size_str(p.size)));
            }
        }
    }
    Ok(Plan {
        size,
        ranges: merge(want, size),
        lines,
        warn,
    })
}

/// What a partition's first sector says it holds.
pub(crate) fn fs_kind(bs: &[u8]) -> &'static str {
    match &bs[3..11] {
        b"NTFS    " => "NTFS",
        b"-FVE-FS-" => "BitLocker",
        _ if &bs[82..87] == b"FAT32" || &bs[54..57] == b"FAT" => "FAT",
        _ => "other",
    }
}

/// Byte ranges (offset, length) → widened to whole MiB, sorted, joined where they touch or overlap, cut at `size`.
pub(crate) fn merge(r: Vec<(u64, u64)>, size: u64) -> Vec<(u64, u64)> {
    let mut spans: Vec<(u64, u64)> = r
        .into_iter()
        .filter(|r| r.1 > 0)
        .map(|(o, l)| {
            let end = o.saturating_add(l).div_ceil(MIB).saturating_mul(MIB);
            (o / MIB * MIB, end.min(size))
        })
        .filter(|s| s.0 < s.1)
        .collect();
    spans.sort_unstable();
    let mut v: Vec<(u64, u64)> = Vec::new();
    for (s, e) in spans {
        match v.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => v.push((s, e)),
        }
    }
    v.into_iter().map(|(s, e)| (s, e - s)).collect()
}

/// Ranges cut into upload chunks of at most `max` bytes.
pub(crate) fn pieces(r: &[(u64, u64)], max: u64) -> Vec<(u64, u64)> {
    r.iter()
        .flat_map(|&(o, l)| (0..l.div_ceil(max)).map(move |i| (o + i * max, max.min(l - i * max))))
        .collect()
}

/// Read every range of `dev` and upload it at its own offset of the server's disk.img (`size` bytes, the rest holes).
fn send(api: &Api, dev: &str, name: &str, size: u64, ranges: &[(u64, u64)]) -> Result<(), String> {
    let f = File::open(dev).map_err(|e| format!("{dev}: {e}"))?;
    let list = pieces(ranges, CHUNK);
    let t0 = Instant::now();
    let sent = run_pieces(&list, |off, b| {
        f.read_exact_at(b, off)
            .map_err(|e| format!("read {dev} at {off}: {e}"))?;
        // All zeros: already a hole on the server.
        if b.iter().all(|&x| x == 0) {
            return Ok(false);
        }
        let path =
            format!("/api/images/upload-chunk?name={name}&file=disk.img&offset={off}&total={size}");
        retry(|| api.call("PUT", &path, b))?;
        Ok(true)
    })?;
    println!(
        "  read {} in {}, sent {} (the rest was zeros)",
        size_str(list.iter().map(|p| p.1).sum()),
        mins(t0.elapsed().as_secs_f64()),
        size_str(sent)
    );
    Ok(())
}

/// `work(offset, buffer of that piece's length)` over every piece, THREADS at a time, with a progress line; the first
/// error stops the rest. Ok(bytes of the pieces `work` said it moved).
fn run_pieces(
    list: &[(u64, u64)],
    work: impl Fn(u64, &mut [u8]) -> Result<bool, String> + Sync,
) -> Result<u64, String> {
    let total: u64 = list.iter().map(|p| p.1).sum();
    let (next, done, moved) = (AtomicUsize::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let stop = AtomicBool::new(false);
    let failed = Mutex::new(None::<String>);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut buf = vec![0u8; CHUNK as usize];
                while !stop.load(Relaxed) {
                    let Some(&(off, len)) = list.get(next.fetch_add(1, Relaxed)) else {
                        break;
                    };
                    match work(off, &mut buf[..len as usize]) {
                        Ok(m) => {
                            if m {
                                moved.fetch_add(len, Relaxed);
                            }
                            done.fetch_add(len, Relaxed);
                        }
                        Err(e) => {
                            failed.lock().get_or_insert(e);
                            stop.store(true, Relaxed);
                        }
                    }
                }
            });
        }
        loop {
            let d = done.load(Relaxed);
            let rate = d as f64 / t0.elapsed().as_secs_f64().max(0.1);
            let left = if rate > 0.0 {
                format!("{} left", mins((total - d) as f64 / rate))
            } else {
                String::new()
            };
            print!(
                "\r  {} / {}  {:.0} MB/s  {left}      ",
                size_str(d),
                size_str(total),
                rate / 1e6
            );
            let _ = std::io::stdout().flush();
            if d >= total || stop.load(Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    println!();
    match failed.into_inner() {
        Some(e) => Err(e),
        None => Ok(moved.into_inner()),
    }
}

/// A request tried 3 times when the network or the server fails (a refusal is final).
fn retry<T>(f: impl Fn() -> Result<T, (u16, String)>) -> Result<T, String> {
    let mut last = String::new();
    for i in 0..3 {
        if i > 0 {
            std::thread::sleep(Duration::from_secs(3));
        }
        match f() {
            Ok(v) => return Ok(v),
            Err((c, e)) if c == 0 || c >= 500 => last = e,
            Err((_, e)) => return Err(e),
        }
    }
    Err(last)
}

/// Follow the server's convert + publish job to its end.
fn wait_job(api: &Api, name: &str) -> Result<(), String> {
    let (mut last, mut errors) = (String::new(), 0);
    loop {
        std::thread::sleep(Duration::from_secs(3));
        let s = match api.req("GET", &format!("/api/images/job?name={name}"), b"") {
            Ok(t) => {
                errors = 0;
                parse(&t)?["status"].as_str().unwrap_or("").to_string()
            }
            Err(e) if errors < 20 => {
                errors += 1;
                println!("  ({e})");
                continue;
            }
            Err(e) => {
                return Err(format!("{e} - the server keeps going: see the Images page"));
            }
        };
        if s != last {
            println!("  {}", ascii(&s));
            last = s.clone();
        }
        if s.starts_with('✓') {
            println!("\nImage {name} is ready.");
            return Ok(());
        }
        if s.starts_with('✗') {
            return Err(format!(
                "the server could not publish {name} (see the Images page)"
            ));
        }
    }
}

/// The server's web API, signed in as one account.
struct Api {
    agent: ureq::Agent,
    base: String,
    user: String,
    pass: String,
    cookie: Mutex<String>,
}

impl Api {
    fn new(srv: &str, user: String, pass: String) -> Api {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(300)))
            .build()
            .into();
        Api {
            agent,
            base: format!("http://{srv}"),
            user,
            pass,
            cookie: Mutex::new(String::new()),
        }
    }

    /// One request → (status, body, the session cookie it set). Bodies up to 32 MB (a 16 MB Deploy chunk).
    fn raw(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>, Option<String>), String> {
        let url = format!("{}{path}", self.base);
        let ck = self.cookie.lock().clone();
        let r = match method {
            "GET" => self.agent.get(&url).header("Cookie", ck.as_str()).call(),
            "PUT" => self
                .agent
                .put(&url)
                .header("Cookie", ck.as_str())
                .send(body),
            _ => self
                .agent
                .post(&url)
                .header("Cookie", ck.as_str())
                .header("Content-Type", "application/json")
                .send(body),
        };
        let mut r = r.map_err(|e| format!("{url}: {e}"))?;
        let set = r
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .map(str::to_string);
        let body = r
            .body_mut()
            .with_config()
            .limit(32 << 20)
            .read_to_vec()
            .map_err(|e| format!("{url}: {e}"))?;
        Ok((r.status().as_u16(), body, set))
    }

    /// Start a session. Err((status, message)); status 0 = no answer.
    fn login(&self) -> Result<(), (u16, String)> {
        let b = json!({"user": self.user, "password": self.pass}).to_string();
        let (code, text, set) = self
            .raw("POST", "/api/auth/login", b.as_bytes())
            .map_err(|e| (0, e))?;
        match (code, set) {
            (200, Some(c)) => {
                *self.cookie.lock() = c;
                Ok(())
            }
            _ => Err((
                code,
                format!("login: {}", String::from_utf8_lossy(&text).trim()),
            )),
        }
    }

    /// A request in the session (signed in again once when it ran out). Err((status, message)); 0 = no answer.
    fn call(&self, method: &str, path: &str, body: &[u8]) -> Result<Vec<u8>, (u16, String)> {
        let mut again = true;
        loop {
            let (code, text, _) = self.raw(method, path, body).map_err(|e| (0, e))?;
            match code {
                200 => return Ok(text),
                401 if again => {
                    again = false;
                    self.login()?;
                }
                _ => {
                    let p = path.split('?').next().unwrap_or(path);
                    let text = String::from_utf8_lossy(&text);
                    return Err((code, format!("{p}: {code} {}", text.trim())));
                }
            }
        }
    }

    fn req(&self, method: &str, path: &str, body: &[u8]) -> Result<String, String> {
        self.call(method, path, body)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .map_err(|e| e.1)
    }
}

fn parse(t: &str) -> Result<Value, String> {
    serde_json::from_str(t).map_err(|e| format!("server answer: {e}"))
}

/// One answer line from the console. Its end (no more input) stops the capture.
fn ask(q: &str) -> Result<String, String> {
    print!("{q}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    match std::io::stdin().lock().read_line(&mut s) {
        Ok(0) | Err(_) => Err("no more input".into()),
        Ok(_) => Ok(s.trim().to_string()),
    }
}

fn yes(q: &str) -> Result<bool, String> {
    Ok(ask(&format!("{q} Type yes to go on: "))?.eq_ignore_ascii_case("yes"))
}

/// An answer not shown while typed (echo off when the input is a terminal).
fn ask_secret(q: &str) -> Result<String, String> {
    // SAFETY: tcgetattr / tcsetattr on stdin with a termios it filled in.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    let tty = unsafe { libc::tcgetattr(0, &mut t) } == 0;
    if tty {
        let mut quiet = t;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) };
    }
    let a = ask(q);
    if tty {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        println!();
    }
    a
}

/// The console font has no symbols: the server's job lines without them.
fn ascii(s: &str) -> String {
    s.replace('→', "->")
        .chars()
        .filter(char::is_ascii)
        .collect::<String>()
        .trim()
        .to_string()
}

pub(crate) fn size_str(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.1} GB", b as f64 / (1u64 << 30) as f64)
    } else {
        format!("{} MB", b >> 20)
    }
}

fn mins(secs: f64) -> String {
    if secs < 90.0 {
        format!("{secs:.0} s")
    } else {
        format!("{:.0} min", secs / 60.0)
    }
}

#[cfg(test)]
#[path = "../tests/unit/capture.rs"]
mod tests;
