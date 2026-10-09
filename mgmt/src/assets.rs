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
    "static/app.js",
    "static/page-machines.html",
    "static/page-images.html",
    "static/page-network.html",
    "static/page-system.html",
    "static/page-drivers.html",
    "static/page-devices.html",
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
    "scripts/broom-stub.ps1",
);

/// broom-assets.zip of this release: sha256, then its URL (CI). Comments only = local build (embedded assets).
#[cfg_attr(not(feature = "external-assets"), allow(dead_code))]
const PIN: &str = include_str!("../assets.pin");
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
    LOADED.get().and_then(|m| m.get(name).copied()).unwrap_or_else(|| panic!("asset {name} not loaded"))
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
        let (want, url) = parse_pin(PIN).ok_or("release build without assets.pin — CI must write it before building")?;
        let local = crate::home().join("broom-assets.zip");
        let mut how = "kept";
        if crate::hash::file_hash(&local.to_string_lossy()).as_deref() != Some(want) {
            let url = url.ok_or("assets.pin has no URL")?;
            tracing::info!("downloading the web UI / iPXE / scripts of this release: {url}");
            let tmp = local.with_extension("zip.tmp");
            let r = crate::winstage::download(url, &tmp, MAX)
                .and_then(|()| match crate::hash::file_hash(&tmp.to_string_lossy()) {
                    Some(h) if h == want => std::fs::rename(&tmp, &local).map_err(|e| e.to_string()),
                    h => Err(format!("{url}: sha256 {h:?}, this binary expects {want}")),
                });
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
    let mut lines = pin.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
    let sha = lines.next().filter(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))?;
    Some((sha, lines.next()))
}

/// The zip's entries (all of NAMES must be there), leaked: they live as long as the process.
#[cfg_attr(not(feature = "external-assets"), allow(dead_code))]
fn unzip(data: &[u8]) -> Result<HashMap<String, &'static [u8]>, String> {
    use std::io::Read;
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|e| format!("broom-assets.zip: {e}"))?;
    let mut map = HashMap::new();
    for name in NAMES {
        let mut f = z.by_name(name).map_err(|_| format!("broom-assets.zip has no {name} (another release?)"))?;
        let mut v = Vec::new();
        f.read_to_end(&mut v).map_err(|e| format!("broom-assets.zip {name}: {e}"))?;
        map.insert(name.to_string(), &*Box::leak(v.into_boxed_slice()));
    }
    Ok(map)
}

/// `bootrom-mgmt pack-assets <file.zip>` (CI, with the local build): every asset into the zip a release binary pins.
pub fn pack(args: &[String]) -> ! {
    let r = (|| -> Result<String, String> {
        let out = args.get(2).ok_or("usage: bootrom-mgmt pack-assets <file.zip>")?;
        let f = std::fs::File::create(out).map_err(|e| format!("{out}: {e}"))?;
        let mut z = zip::ZipWriter::new(f);
        let opt = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
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
mod tests {
    use super::*;

    /// Every listed asset is there; a zip of them unpacks to the same bytes; the committed pin = local build.
    #[test]
    fn pack_and_unzip() {
        assert!(NAMES.iter().all(|n| !bytes(n).is_empty()));
        assert!(text("scripts/stage.sh").starts_with("#!"));
        let p = std::env::temp_dir().join("broom_test_assets.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
        for n in NAMES {
            z.start_file(*n, zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut z, bytes(n)).unwrap();
        }
        z.finish().unwrap();
        let m = unzip(&std::fs::read(&p).unwrap()).unwrap();
        assert!(NAMES.iter().all(|n| m[*n] == bytes(n)));
        assert!(unzip(b"not a zip").is_err());
        assert_eq!(parse_pin(PIN), None, "committed assets.pin = local build, no pin");
        let _ = std::fs::remove_file(p);
    }
}
