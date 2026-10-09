// goldenram.rs — what the stages download as tftp/broom-win/<name>/golden.vhdx. It is not a file: the VHDX is made
// on the fly from image.img (vhdx::Virtual — header in memory, payload read straight from the raw disk), so a publish
// never writes a 50 GB copy. Cache mode RAM: the same bytes held compressed in RAM (iscsid::ramimg, the same 16 KB
// zstd blocks as the Linux RAM targets) — after a publish a whole room fetches the new golden at once, 30 streams at
// 30 offsets would keep the disk seeking for an hour; from RAM it is never touched.
// The golden being served keeps image.img open: a new upload / rollback renamed over it changes nothing until the
// publish that installs the new golden. An image published by an older version (a real golden.vhdx file, no
// golden.id) is served from that file until it is published again.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use axum::body::Body;
use axum::extract::{Path, Request};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::iscsid::ramimg::RamImage;
use crate::SharedState;

/// The golden an image serves right now.
pub struct Golden {
    /// hash::stamp of image.img when it was built (saved as golden.id): the same id → the same bytes and sha256.
    pub id: String,
    pub vhdx: Arc<crate::vhdx::Virtual>,
    ram: Option<Arc<RamImage>>,
}

impl Golden {
    fn size(&self) -> u64 {
        self.vhdx.len()
    }
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        match &self.ram {
            Some(r) => r.read_at(off, buf),
            None => self.vhdx.read_at(off, buf),
        }
    }
}

static SERVED: LazyLock<Mutex<HashMap<String, Arc<Golden>>>> = LazyLock::new(Default::default);

fn served(name: &str) -> Option<Arc<Golden>> {
    SERVED.lock().unwrap().get(name).cloned()
}

/// Id of the golden an image serves (None: none built by this version yet).
pub fn served_id(name: &str) -> Option<String> {
    served(name).map(|g| g.id.clone())
}

/// Serve this golden from now on (publish, start); no RAM copy yet (set_ram).
pub fn install(name: &str, id: &str, vhdx: crate::vhdx::Virtual) {
    SERVED.lock().unwrap().insert(name.to_string(), Arc::new(Golden { id: id.to_string(), vhdx: Arc::new(vhdx), ram: None }));
}

/// Make the served golden's RAM copy follow the cache mode: `ram` → loaded (kept if there), else dropped. Blocking
/// (~a minute for 12 GB). Err = refused (RAM) or failed: the caller goes back to disk.
pub fn set_ram(st: &SharedState, name: &str, ram: bool) -> Result<(), String> {
    let g = served(name).ok_or("golden not built yet")?;
    if ram == g.ram.is_some() {
        return Ok(());
    }
    let copy = if ram {
        let v = g.vhdx.clone();
        let blocks = v.len().div_ceil(crate::iscsid::ramimg::BLOCK as u64);
        Some(Arc::new(RamImage::load_with(name, v.len(), blocks, crate::publish::ram_reserve(st), |off, buf| v.read_at(off, buf))?))
    } else {
        None
    };
    let mut s = SERVED.lock().unwrap();
    // Only onto the golden it was made from (jobs of an image never overlap, so it is still that one).
    if s.get(name).is_some_and(|cur| cur.id == g.id) {
        s.insert(name.to_string(), Arc::new(Golden { id: g.id.clone(), vhdx: g.vhdx.clone(), ram: copy }));
    }
    Ok(())
}

/// The image is gone.
pub fn forget(name: &str) {
    SERVED.lock().unwrap().remove(name);
}

fn dir(name: &str) -> PathBuf {
    crate::tftp_dir().join("broom-win").join(name)
}

/// At start: the golden of every published Windows image, made again from image.img (same id → same bytes, so no
/// client downloads again), plus its RAM copy when set to RAM. image.img changed since (an upload whose publish did not
/// finish) → not served until published.
pub fn restore_all(st: &SharedState) {
    for img in st.db.images().unwrap_or_default() {
        if img.os != "windows" || img.boot_script.is_none() {
            continue;
        }
        let raw = crate::images_dir().join(&img.name).join("image.img");
        let Some(id) = crate::hash::stamp(&raw) else { continue };
        if std::fs::read_to_string(dir(&img.name).join("golden.id")).ok().as_deref().map(str::trim) != Some(id.as_str()) {
            continue; // published by an older version (golden.vhdx file) or image.img changed since
        }
        match crate::vhdx::Virtual::open(&raw, &id) {
            Ok(v) => install(&img.name, &id, v),
            Err(e) => {
                tracing::error!("image {}: golden not served: {e}", img.name);
                continue;
            }
        }
        if img.cache_mode == "zram" {
            match set_ram(st, &img.name, true) {
                Ok(()) => tracing::info!("image {}: golden.vhdx in RAM", img.name),
                Err(e) => {
                    tracing::warn!("image {}: golden not loaded into RAM ({e}) → cache_mode=disk", img.name);
                    let _ = st.db.set_cache_mode(img.id, "disk");
                }
            }
        }
    }
}

/// One byte range of `Range: bytes=...` → (first, last) inclusive. None = no / multi / unsupported range → whole file.
/// Err = not satisfiable (416).
fn range(h: Option<&HeaderValue>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = h.and_then(|v| v.to_str().ok()).and_then(|s| s.strip_prefix("bytes=")) else { return Ok(None) };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((a, b)) = spec.trim().split_once('-') else { return Ok(None) };
    let (first, last) = match (a.parse::<u64>().ok(), b.parse::<u64>().ok()) {
        (Some(a), Some(b)) => (a, b.min(size.saturating_sub(1))),
        (Some(a), None) if b.is_empty() => (a, size.saturating_sub(1)),
        (None, Some(n)) if a.is_empty() && n > 0 => (size.saturating_sub(n), size.saturating_sub(1)),
        _ => return Ok(None),
    };
    if first >= size || first > last {
        return Err(());
    }
    Ok(Some((first, last)))
}

/// GET /tftp/broom-win/{name}/golden.vhdx — the served golden (RAM copy or made from image.img); an image published
/// by an older version: its golden.vhdx file.
pub async fn get(Path(name): Path<String>, req: Request) -> Response {
    let Some(g) = crate::images::valid_name(&name).then(|| served(&name)).flatten() else {
        use tower::ServiceExt;
        return match tower_http::services::ServeFile::new(dir(&name).join("golden.vhdx")).oneshot(req).await {
            Ok(r) => r.into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    };
    serve(g, req.headers())
}

fn serve(img: Arc<Golden>, headers: &HeaderMap) -> Response {
    let size = img.size();
    let (status, first, last) = match range(headers.get(header::RANGE), size) {
        Ok(Some((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b),
        Ok(None) => (StatusCode::OK, 0, size.saturating_sub(1)),
        Err(()) => {
            return (StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{size}"))]).into_response();
        }
    };
    let len = if size == 0 { 0 } else { last - first + 1 };
    // 1 MB at a time, read (disk) / unpacked (RAM) off the async threads.
    let body = futures_util::stream::unfold(first, move |at| {
        let img = img.clone();
        async move {
            let end = first + len;
            if at >= end {
                return None;
            }
            let n = (end - at).min(1 << 20) as usize;
            let chunk = tokio::task::spawn_blocking(move || {
                let mut buf = vec![0u8; n];
                img.read_at(at, &mut buf).map(|_| buf)
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
            .map_err(std::io::Error::other);
            Some((chunk, at + n as u64))
        }
    });
    let mut r = Response::new(Body::from_stream(body));
    *r.status_mut() = status;
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if status == StatusCode::PARTIAL_CONTENT {
        h.insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes {first}-{last}/{size}")).unwrap());
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let r = |s: &str| range(Some(&HeaderValue::from_str(s).unwrap()), 1000);
        assert_eq!(range(None, 1000), Ok(None));
        assert_eq!(r("bytes=100-"), Ok(Some((100, 999))), "wget -c resume");
        assert_eq!(r("bytes=0-99"), Ok(Some((0, 99))));
        assert_eq!(r("bytes=900-5000"), Ok(Some((900, 999))), "end clamped");
        assert_eq!(r("bytes=-10"), Ok(Some((990, 999))), "suffix");
        assert_eq!(r("bytes=1000-"), Err(()), "past the end → 416");
        assert_eq!(r("bytes=5-1"), Err(()));
        assert_eq!(r("bytes=0-1,5-6"), Ok(None), "multi-range → whole file");
        assert_eq!(r("items=0-1"), Ok(None));
    }

    /// The route coexists with the /tftp file service (axum would panic on a conflicting route at start).
    #[test]
    fn route_beside_serve_dir() {
        let _: axum::Router = axum::Router::new()
            .route("/tftp/broom-win/{name}/golden.vhdx", axum::routing::get(get))
            .nest_service("/tftp", tower_http::services::ServeDir::new("."));
    }
}
