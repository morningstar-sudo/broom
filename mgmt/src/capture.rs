// capture.rs — `bootrom-mgmt capture <server>`: runs on a client PC (boot menu [C] → the Windows stage in capture mode
// downloads this binary from /broom-capture) and sends its Windows disk to the server as an image's new golden,
// through the web's own upload API, signed in with an owner account typed on the screen. The disk is only read.
// Sent: the partition tables (first + last MiB), the clusters in use of each NTFS partition ($Bitmap), every other
// partition whole. Everything else stays a hole in the server's upload, which converts + publishes like an .img.
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

pub fn run(args: &[String]) -> ! {
    let code = match args.get(2) {
        None => {
            eprintln!("usage: bootrom-mgmt capture <server ip>");
            2
        }
        Some(srv) => match capture(srv) {
            Ok(()) => 0,
            Err(e) => {
                println!("\nCapture stopped: {e}");
                1
            }
        },
    };
    std::process::exit(code)
}

fn capture(srv: &str) -> Result<(), String> {
    println!("\nbroom Capture: this PC's disk -> a Windows image on {srv}");
    println!("(the disk is only read, never written)\n");
    let disks = disks();
    if disks.is_empty() {
        return Err("no internal disk found".into());
    }
    for (i, d) in disks.iter().enumerate() {
        println!(
            "  [{}] {:<14} {:>9}  {}",
            i + 1,
            d.dev,
            size_str(d.size),
            d.model
        );
    }
    let disk = loop {
        let a = ask(&format!("Disk with Windows [1-{}]: ", disks.len()))?;
        let a = if a.is_empty() && disks.len() == 1 {
            "1".into()
        } else {
            a
        };
        if let Some(d) = a
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| disks.get(i))
        {
            break d;
        }
    };

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
    let api = loop {
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
            println!("  {} is not an owner account - Capture needs one", api.user);
            continue;
        }
        break api;
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
    send(&api, &disk.dev, &name, plan.size, &plan.ranges)?;
    api.req("POST", "/api/images/upload-done", b.as_bytes())?;
    println!("\nAll sent. The server now converts + publishes the image (a few minutes):");
    wait_job(&api, &name)
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
    let total: u64 = list.iter().map(|p| p.1).sum();
    let (next, done, sent) = (AtomicUsize::new(0), AtomicU64::new(0), AtomicU64::new(0));
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
                    let b = &mut buf[..len as usize];
                    let r = f
                        .read_exact_at(b, off)
                        .map_err(|e| format!("read {dev} at {off}: {e}"))
                        .and_then(|()| {
                            // All zeros: already a hole on the server.
                            if b.iter().all(|&x| x == 0) {
                                return Ok(());
                            }
                            sent.fetch_add(len, Relaxed);
                            put(api, name, off, size, b)
                        });
                    match r {
                        Ok(()) => {
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
            let secs = t0.elapsed().as_secs_f64().max(0.1);
            let rate = d as f64 / secs;
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
    if let Some(e) = failed.into_inner() {
        return Err(e);
    }
    println!(
        "  read {} in {}, sent {} (the rest was zeros)",
        size_str(total),
        mins(t0.elapsed().as_secs_f64()),
        size_str(sent.into_inner())
    );
    Ok(())
}

/// One chunk, tried 3 times when the network or the server fails (a refusal is final).
fn put(api: &Api, name: &str, off: u64, total: u64, b: &[u8]) -> Result<(), String> {
    let path =
        format!("/api/images/upload-chunk?name={name}&file=disk.img&offset={off}&total={total}");
    let mut last = String::new();
    for i in 0..3 {
        if i > 0 {
            std::thread::sleep(Duration::from_secs(3));
        }
        match api.call("PUT", &path, b) {
            Ok(_) => return Ok(()),
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

    /// One request → (status, body, the session cookie it set).
    fn raw(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<(u16, String, Option<String>), String> {
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
        let text = r.body_mut().read_to_string().unwrap_or_default();
        Ok((r.status().as_u16(), text, set))
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
            _ => Err((code, format!("login: {}", text.trim()))),
        }
    }

    /// A request in the session (signed in again once when it ran out). Err((status, message)); 0 = no answer.
    fn call(&self, method: &str, path: &str, body: &[u8]) -> Result<String, (u16, String)> {
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
                    return Err((code, format!("{p}: {code} {}", text.trim())));
                }
            }
        }
    }

    fn req(&self, method: &str, path: &str, body: &[u8]) -> Result<String, String> {
        self.call(method, path, body).map_err(|e| e.1)
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
