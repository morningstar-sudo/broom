// assets.rs — the files the binary serves or hands out: web UI, iPXE (TFTP), boot / prep scripts.
// Local build: embedded (include_bytes!). Release build (feature `external-assets`, CI): not in the binary — they come
// as broom-assets.zip of the same release, pinned by sha256 in assets.pin (written by CI, like stage.pin). The first
// start downloads it and keeps it next to the binary; later starts (and offline servers: copy it there) use that file.
// Without it the server does not start: it would hand clients no iPXE.
use std::collections::HashMap;
use std::sync::OnceLock;

macro_rules! assets {
    ($($p:literal),* $(,)?) => {
        /// Every asset, by its path under mgmt/.
        const NAMES: &[&str] = &[$($p),*];
        #[cfg(not(feature = "external-assets"))]
        const EMBEDDED: &[&[u8]] = &[$(include_bytes!(concat!("../", $p))),*];
    };
}
assets!(
    "static/index.html",
    "static/login.html",
    "static/login.js",
    "static/crypto.js",
    "static/app.js",
    "static/app.css",
    "static/bootstrap.min.css",
    "static/bootstrap.bundle.min.js",
    "static/chart.umd.min.js",
    "static/icons.svg",
    "static/page-machines.html",
    "static/page-images.html",
    "static/page-network.html",
    "static/page-system.html",
    "static/page-drivers.html",
    "static/page-devices.html",
    "static/page-groups.html",
    "static/page-benchmark.html",
    "static/page-remote.html",
    "static/page-server.html",
    "static/page-games.html",
    "static/page-windows.html",
    "static/page-logs.html",
    "ipxe/snponly.efi",
    "ipxe/signed/snponly-shim.efi",
    "ipxe/signed/snponly.efi",
    "scripts/linux-iscsi-hook.sh",
    "scripts/linux-cache.sh",
    "scripts/prep-linux.sh",
    "scripts/stage-hook.sh",
    "scripts/stage.sh",
    "scripts/prep-win.ps1",
    "scripts/broom-done.ps1",
    "scripts/broom-bootorder.ps1",
    "scripts/broom-games.ps1",
    "scripts/broom-watch.ps1",
    "scripts/broom-stub.ps1",
    "scripts/broom-fw.cs",
    "scripts/broom-vnc.cs",
);

/// Assets only a release has: built by CI on another OS and added to its broom-assets.zip (a local build has none).
/// broom-fw.dll / broom-vnc.exe: broom-fw.cs / broom-vnc.cs compiled on a Windows runner, so the clients never run the
/// C# compiler for them.
const OPTIONAL: &[&str] = &["scripts/broom-fw.dll", "scripts/broom-vnc.exe"];

/// An asset of OPTIONAL: Some in a release whose zip has it.
pub fn optional(name: &str) -> Option<&'static [u8]> {
    debug_assert!(OPTIONAL.contains(&name));
    LOADED.get().and_then(|m| m.get(name).copied())
}

/// broom-assets.zip of this release: sha256, then its URL. Comments only = local build (embedded assets). CI appends the
/// release's pins to the built binary instead (pin()).
#[cfg_attr(not(feature = "external-assets"), allow(dead_code))]
const PIN: &str = include_str!("../assets.pin");

const TRAILER: &[u8] = b"\n#broom-pins\n";

/// Pins appended by CI to the release binary after it is built, so the binary compiles while the bundles are made: the
/// file ends with "\n#broom-pins\n" and one "<name> <sha256> <url>" line per bundle → name → "sha256\nurl".
pub(crate) fn parse_trailer(tail: &[u8]) -> HashMap<String, String> {
    let Some(at) = tail.windows(TRAILER.len()).rposition(|w| w == TRAILER) else {
        return HashMap::new();
    };
    String::from_utf8_lossy(&tail[at + TRAILER.len()..])
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (name, sha, url) = (f.next()?, f.next()?, f.next()?);
            (sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| (name.to_string(), format!("{sha}\n{url}")))
        })
        .collect()
}

/// The pin of bundle `name` ("stage" | "assets") as pin-file text: the one appended to this binary, else `embedded`
/// (its pin file, compiled in). The binary's tail is read once.
pub fn pin(name: &str, embedded: &'static str) -> &'static str {
    use std::io::{Read, Seek, SeekFrom};
    static PINS: OnceLock<HashMap<String, String>> = OnceLock::new();
    PINS.get_or_init(|| {
        let mut tail = Vec::new();
        if let Ok(mut f) = std::fs::File::open("/proc/self/exe") {
            let _ = f
                .seek(SeekFrom::End(-4096))
                .or_else(|_| f.seek(SeekFrom::Start(0)));
            let _ = f.read_to_end(&mut tail);
        }
        parse_trailer(&tail)
    })
    .get(name)
    .map_or(embedded, String::as_str)
}
/// Largest download accepted (the assets are ~2 MB).
#[cfg_attr(not(feature = "external-assets"), allow(dead_code))]
const MAX: u64 = 64 << 20;

static LOADED: OnceLock<HashMap<String, &'static [u8]>> = OnceLock::new();

/// An asset's bytes. Panics on a name not in the list (a bug, caught by the tests) or before init() in a release build.
pub fn bytes(name: &str) -> &'static [u8] {
    #[cfg(not(feature = "external-assets"))]
    if let Some(i) = NAMES.iter().position(|n| *n == name) {
        return EMBEDDED[i];
    }
    LOADED
        .get()
        .and_then(|m| m.get(name).copied())
        .unwrap_or_else(|| panic!("asset {name} not loaded"))
}

/// A text asset (UTF-8 by construction: they are this repo's own files).
pub fn text(name: &str) -> &'static str {
    std::str::from_utf8(bytes(name)).unwrap_or_default()
}

/// Release build: load broom-assets.zip next to the binary, downloading it first if missing or of another release.
/// Local build: nothing to do. Blocking (a download).
pub fn init() -> Result<String, String> {
    #[cfg(not(feature = "external-assets"))]
    return Ok("assets embedded (local build)".into());
    #[cfg(feature = "external-assets")]
    {
        let (want, url) = parse_pin(pin("assets", PIN))
            .ok_or("release build without an assets pin — CI appends it to the binary")?;
        let local = crate::home().join("broom-assets.zip");
        let mut how = "kept";
        if crate::hash::file_hash(&local.to_string_lossy()).as_deref() != Some(want) {
            let url = url.ok_or("assets.pin has no URL")?;
            tracing::info!("downloading the web UI / iPXE / scripts of this release: {url}");
            let tmp = local.with_extension("zip.tmp");
            let r =
                crate::winstage::download(url, &tmp, MAX).and_then(
                    |()| match crate::hash::file_hash(&tmp.to_string_lossy()) {
                        Some(h) if h == want => {
                            std::fs::rename(&tmp, &local).map_err(|e| e.to_string())
                        }
                        h => Err(format!("{url}: sha256 {h:?}, this binary expects {want}")),
                    },
                );
            if let Err(e) = r {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!(
                    "{e} — no internet on this server? download broom-assets.zip of this release elsewhere and copy it to {}",
                    local.display()
                ));
            }
            how = "downloaded";
        }
        let map = unzip(&std::fs::read(&local).map_err(|e| format!("{}: {e}", local.display()))?)?;
        let _ = LOADED.set(map);
        Ok(format!("assets {how} ({})", local.display()))
    }
}

/// (sha256, URL) from assets.pin; None without a valid sha256.
#[cfg_attr(not(feature = "external-assets"), allow(dead_code))]
fn parse_pin(pin: &str) -> Option<(&str, Option<&str>)> {
    let mut lines = pin
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let sha = lines
        .next()
        .filter(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))?;
    Some((sha, lines.next()))
}

/// The zip's entries (all of NAMES must be there), leaked: they live as long as the process.
#[cfg_attr(not(feature = "external-assets"), allow(dead_code))]
fn unzip(data: &[u8]) -> Result<HashMap<String, &'static [u8]>, String> {
    use std::io::Read;
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(data))
        .map_err(|e| format!("broom-assets.zip: {e}"))?;
    let mut map = HashMap::new();
    for name in NAMES {
        let mut f = z
            .by_name(name)
            .map_err(|_| format!("broom-assets.zip has no {name} (another release?)"))?;
        let mut v = Vec::new();
        f.read_to_end(&mut v)
            .map_err(|e| format!("broom-assets.zip {name}: {e}"))?;
        map.insert(name.to_string(), &*Box::leak(v.into_boxed_slice()));
    }
    for name in OPTIONAL {
        let Ok(mut f) = z.by_name(name) else { continue };
        let mut v = Vec::new();
        f.read_to_end(&mut v)
            .map_err(|e| format!("broom-assets.zip {name}: {e}"))?;
        map.insert(name.to_string(), &*Box::leak(v.into_boxed_slice()));
    }
    Ok(map)
}

/// `bootrom-mgmt pins` (CI, after appending them): the stage / assets sha256 this binary will accept, one per line
/// ("-" = none). Exits 1 unless both are there.
pub fn print_pins() -> ! {
    let mut all = true;
    for (name, embedded) in [("stage", crate::winstage::STAGE_PIN), ("assets", PIN)] {
        let sha = parse_pin(pin(name, embedded)).map(|(s, _)| s);
        all &= sha.is_some();
        println!("{name} {}", sha.unwrap_or("-"));
    }
    std::process::exit(if all { 0 } else { 1 })
}

/// `bootrom-mgmt pack-assets <file.zip>` (CI, with the local build): every asset into the zip a release binary pins.
pub fn pack(args: &[String]) -> ! {
    let r = (|| -> Result<String, String> {
        let out = args
            .get(2)
            .ok_or("usage: bootrom-mgmt pack-assets <file.zip>")?;
        let f = std::fs::File::create(out).map_err(|e| format!("{out}: {e}"))?;
        let mut z = zip::ZipWriter::new(f);
        let opt = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for name in NAMES {
            z.start_file(*name, opt).map_err(|e| e.to_string())?;
            std::io::Write::write_all(&mut z, bytes(name)).map_err(|e| e.to_string())?;
        }
        z.finish().map_err(|e| e.to_string())?;
        crate::hash::file_hash(out).ok_or_else(|| "sha256 of the zip failed".into())
    })();
    match r {
        Ok(sha) => {
            println!("{sha}");
            std::process::exit(0)
        }
        Err(e) => {
            eprintln!("pack-assets: {e}");
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/assets.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/unit/webui.rs"]
mod webui_tests;
