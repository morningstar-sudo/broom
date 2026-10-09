// goldenram.rs — the RAM cache mode of Windows images: golden.vhdx held compressed in RAM (iscsid::ramimg, the same
// 16 KB zstd blocks as the Linux RAM targets) and downloaded by the stages from there. After a publish a whole room
// fetches the new golden at once: 30 streams at 30 different offsets keep an HDD seeking for an hour; from RAM the
// disk is never touched. The file on disk stays the source: served from it while the copy loads, when the copy is
// stale (golden rebuilt) or when the image is set back to disk.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use axum::body::Body;
use axum::extract::{Path, Request};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::iscsid::ramimg::RamImage;
use crate::SharedState;

/// image name → (hash::stamp of golden.vhdx it was loaded from, the copy).
static COPIES: LazyLock<Mutex<HashMap<String, (String, Arc<RamImage>)>>> = LazyLock::new(Default::default);

fn golden(name: &str) -> PathBuf {
    crate::tftp_dir().join("broom-win").join(name).join("golden.vhdx")
}

/// Make the RAM copy of an image's golden follow its cache mode: `ram` → loaded (kept if already from this very
/// file), else dropped. Blocking (~a minute for 12 GB). Err = refused (RAM) or failed: the caller goes back to disk.
pub fn sync(st: &SharedState, name: &str, ram: bool) -> Result<(), String> {
    let p = golden(name);
    let stamp = crate::hash::stamp(&p);
    let mut copies = COPIES.lock().unwrap();
    if !ram {
        copies.remove(name);
        return Ok(());
    }
    let stamp = stamp.ok_or_else(|| format!("{}: no golden", p.display()))?;
    if copies.get(name).is_some_and(|(s, _)| *s == stamp) {
        return Ok(());
    }
    copies.remove(name); // the old copy's RAM back before checking room for the new one
    drop(copies);
    let img = RamImage::load(&p.to_string_lossy(), crate::publish::ram_reserve(st))?;
    COPIES.lock().unwrap().insert(name.to_string(), (stamp, Arc::new(img)));
    Ok(())
}

/// The image is gone.
pub fn forget(name: &str) {
    COPIES.lock().unwrap().remove(name);
}

/// RAM copies for every published Windows image set to RAM (at start: they live in this process). An image that no
/// longer fits goes back to disk.
pub fn load_all(st: &SharedState) {
    for img in st.db.images().unwrap_or_default() {
        if img.os != "windows" || img.cache_mode != "zram" || img.boot_script.is_none() {
            continue;
        }
        match sync(st, &img.name, true) {
            Ok(()) => tracing::info!("image {}: golden.vhdx in RAM", img.name),
            Err(e) => {
                tracing::warn!("image {}: golden not loaded into RAM ({e}) → cache_mode=disk", img.name);
                let _ = st.db.set_cache_mode(img.id, "disk");
            }
        }
    }
}

/// The copy, if it is of the file on disk right now (a publish renamed a new golden over it → the file wins).
fn copy_of(name: &str) -> Option<Arc<RamImage>> {
    let stamp = crate::hash::stamp(&golden(name))?;
    COPIES.lock().unwrap().get(name).filter(|(s, _)| *s == stamp).map(|(_, c)| c.clone())
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

/// GET /tftp/broom-win/{name}/golden.vhdx — from RAM when the image has a current copy, else the file.
pub async fn get(Path(name): Path<String>, req: Request) -> Response {
    let Some(img) = crate::images::valid_name(&name).then(|| copy_of(&name)).flatten() else {
        use tower::ServiceExt;
        return match tower_http::services::ServeFile::new(golden(&name)).oneshot(req).await {
            Ok(r) => r.into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    };
    from_ram(img, req.headers())
}

fn from_ram(img: Arc<RamImage>, headers: &HeaderMap) -> Response {
    let size = img.size();
    let (status, first, last) = match range(headers.get(header::RANGE), size) {
        Ok(Some((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b),
        Ok(None) => (StatusCode::OK, 0, size.saturating_sub(1)),
        Err(()) => {
            return (StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{size}"))]).into_response();
        }
    };
    let len = if size == 0 { 0 } else { last - first + 1 };
    // 1 MB at a time, unpacked off the async threads.
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
