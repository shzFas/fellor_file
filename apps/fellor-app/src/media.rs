//! Media helpers: download device files into a local cache (so macOS apps can
//! open/play them), and build preview images for any format macOS knows
//! (HEIC, MOV/MP4, PDF, RAW…) through Quick Look (`qlmanage`).

use crate::platform;
use fellor_vfs::{BackendRef, Entry};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Images larger than this are not auto-downloaded from a device just to preview.
pub const MAX_IMAGE_FETCH: u64 = 80_000_000;
/// Videos up to this size are downloaded for a poster frame in the big preview.
pub const MAX_VIDEO_FETCH_FOR_PREVIEW: u64 = 150_000_000;

pub fn cache_root() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("Fellor")
}

fn entry_key(e: &Entry) -> u64 {
    let mut h = DefaultHasher::new();
    e.path.to_string().hash(&mut h);
    e.size.hash(&mut h);
    e.mtime.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs()).hash(&mut h);
    h.finish()
}

/// Stable cache key for textures (includes size/mtime so edits invalidate it).
pub fn texture_key(e: &Entry, max_px: u32) -> String {
    format!("{:016x}@{max_px}", entry_key(e))
}

/// Where a device file is cached locally. Keeps the original file name so
/// apps show the right title and pick the right handler.
fn cache_file(e: &Entry) -> PathBuf {
    cache_root().join("open").join(format!("{:016x}", entry_key(e))).join(&e.name)
}

/// Local path for an entry if one exists without downloading: host files,
/// and device files already in the cache.
pub fn local_copy(e: &Entry) -> Option<PathBuf> {
    if let Some(p) = platform::host_path(&e.path) {
        return Some(p);
    }
    let p = cache_file(e);
    let len = std::fs::metadata(&p).ok()?.len();
    (e.size.is_none() || e.size == Some(len)).then_some(p)
}

pub fn is_image(e: &Entry) -> bool {
    e.mime.is_some_and(|m| m.starts_with("image/"))
}
pub fn is_video(e: &Entry) -> bool {
    e.mime.is_some_and(|m| m.starts_with("video/"))
}
fn rust_decodable(e: &Entry) -> bool {
    matches!(e.mime, Some("image/jpeg" | "image/png" | "image/gif" | "image/webp"))
}

/// Download `e` from its backend into the cache (or return the host path).
/// `progress(done, total)` is called per chunk; `cancel` aborts.
pub async fn fetch_local(
    b: BackendRef,
    e: &Entry,
    cancel: Arc<AtomicBool>,
    mut progress: impl FnMut(u64, u64) + Send,
) -> Result<PathBuf, String> {
    if let Some(p) = local_copy(e) {
        return Ok(p);
    }
    let dst = cache_file(e);
    let dir = dst.parent().expect("cache file has a parent").to_path_buf();
    tokio::fs::create_dir_all(&dir).await.map_err(|err| err.to_string())?;
    let part = dir.join(format!(".{}.part", e.name));
    let total = e.size.unwrap_or(0);
    let mut r = b.open_read(&e.path, 0).await.map_err(|err| err.to_string())?;
    let mut f = tokio::fs::File::create(&part).await.map_err(|err| err.to_string())?;
    let mut done = 0u64;
    let res: Result<(), String> = async {
        use tokio::io::AsyncWriteExt;
        while let Some(chunk) = r.read_chunk(1 << 20).await.map_err(|err| err.to_string())? {
            if cancel.load(Ordering::Relaxed) {
                return Err("отменено".into());
            }
            f.write_all(&chunk).await.map_err(|err| err.to_string())?;
            done += chunk.len() as u64;
            progress(done, total.max(done));
        }
        f.flush().await.map_err(|err| err.to_string())?;
        Ok(())
    }
    .await;
    drop(f);
    if let Err(err) = res {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(err);
    }
    if let Some(t) = e.mtime {
        if let Ok(f) = std::fs::File::options().write(true).open(&part) {
            let _ = f.set_modified(t);
        }
    }
    tokio::fs::rename(&part, &dst).await.map_err(|err| err.to_string())?;
    Ok(dst)
}

/// Quick Look thumbnail as PNG bytes (works for HEIC, video, PDF, RAW…).
/// Blocking: call from `spawn_blocking`.
fn quicklook_png(src: &Path, max_px: u32) -> Option<Vec<u8>> {
    let out = cache_root().join("ql").join(format!("{:x}", rand_suffix()));
    std::fs::create_dir_all(&out).ok()?;
    let status = Command::new("/usr/bin/qlmanage")
        .args(["-t", "-x", "-s", &max_px.to_string(), "-o"])
        .arg(&out)
        .arg(src)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    let png = std::fs::read_dir(&out).ok()?.flatten().map(|d| d.path()).find(|p| p.extension().is_some_and(|x| x == "png"));
    let bytes = if status.success() { png.and_then(|p| std::fs::read(p).ok()) } else { None };
    let _ = std::fs::remove_dir_all(&out);
    bytes
}

fn rand_suffix() -> u64 {
    let mut h = DefaultHasher::new();
    std::time::Instant::now().hash(&mut h);
    std::thread::current().id().hash(&mut h);
    h.finish()
}

fn decode(bytes: &[u8], max_px: u32) -> Result<egui::ColorImage, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("не удалось декодировать: {e}"))?;
    let img = if img.width() > max_px || img.height() > max_px { img.thumbnail(max_px, max_px) } else { img };
    let rgba = img.to_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}

/// Image for a file on disk: decode directly when we can, else Quick Look.
async fn image_from_file(path: PathBuf, direct: bool, max_px: u32) -> Result<Option<egui::ColorImage>, String> {
    tokio::task::spawn_blocking(move || {
        if direct {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(img) = decode(&bytes, max_px) {
                    return Ok(Some(img));
                }
            }
        }
        match quicklook_png(&path, max_px) {
            Some(png) => decode(&png, max_px).map(Some),
            None => Ok(None),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// How eager a preview load may be about pulling bytes off a device.
#[derive(Clone, Copy)]
pub enum Fetch {
    /// Hover: images only (they are small), never videos.
    Images,
    /// Big preview window: images, and videos up to MAX_VIDEO_FETCH_FOR_PREVIEW.
    ImagesAndSmallVideos,
}

/// Preview image for any entry. `Ok(None)` means "no preview possible
/// without downloading more than allowed".
pub async fn load_image(b: BackendRef, e: Entry, max_px: u32, fetch: Fetch) -> Result<Option<egui::ColorImage>, String> {
    if let Some(p) = local_copy(&e) {
        return image_from_file(p, rust_decodable(&e), max_px).await;
    }
    // Backend thumbnails (MTP GetThumb) are small — fine for hover, and the
    // only option for videos we won't download.
    let thumb = b.thumbnail(&e.path).await.ok().flatten();
    let size = e.size.unwrap_or(u64::MAX);
    let want_full = (is_image(&e) && size <= MAX_IMAGE_FETCH)
        || (is_video(&e) && matches!(fetch, Fetch::ImagesAndSmallVideos) && size <= MAX_VIDEO_FETCH_FOR_PREVIEW);
    if want_full && (thumb.is_none() || max_px > 400) {
        let full = match fetch_local(b, &e, Arc::new(AtomicBool::new(false)), |_, _| {}).await {
            Ok(p) => image_from_file(p, rust_decodable(&e), max_px).await,
            Err(err) => Err(err),
        };
        match (full, &thumb) {
            (Ok(Some(img)), _) => return Ok(Some(img)),
            // Full file unreadable/undecodable: fall back to the device thumbnail.
            (_, Some(_)) => {}
            (other, None) => return other,
        }
    }
    match thumb {
        Some(bytes) => tokio::task::spawn_blocking(move || decode(&bytes, max_px).map(Some)).await.map_err(|e| e.to_string())?,
        None => Ok(None),
    }
}

/// Remove cached device files older than a few days (best effort, at startup).
pub fn prune_cache() {
    let dir = cache_root().join("open");
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let cutoff = Duration::from_secs(3 * 24 * 3600);
    for d in rd.flatten() {
        let old = d.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > cutoff);
        if old {
            let _ = std::fs::remove_dir_all(d.path());
        }
    }
    let _ = std::fs::remove_dir_all(cache_root().join("ql"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use fellor_vfs::VPath;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap()
    }

    /// A non-host backend (like an iPhone) must be downloaded into the cache,
    /// keep the original name, and be reused on the second call.
    #[test]
    fn device_file_is_cached_for_opening() {
        let dir = std::env::temp_dir().join(format!("fellor-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("clip.mov"), &data).unwrap();
        rt().block_on(async {
            let b = fellor_local::LocalBackend::rooted("ios:TEST", dir.clone());
            let e = b.stat(&VPath::from_str_path("ios:TEST", "/clip.mov")).await.unwrap();
            assert!(local_copy(&e).is_none());
            let mut calls = 0;
            let p = fetch_local(b.clone(), &e, Arc::new(AtomicBool::new(false)), |_, _| calls += 1).await.unwrap();
            assert!(calls > 0);
            assert_eq!(p.file_name().unwrap(), "clip.mov");
            assert_eq!(std::fs::read(&p).unwrap(), data);
            assert_eq!(local_copy(&e).as_deref(), Some(p.as_path()));
            let _ = std::fs::remove_dir_all(p.parent().unwrap());
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// HEIC is not decodable by the `image` crate; Quick Look must handle it.
    #[test]
    fn heic_preview_via_quicklook() {
        let Some(heic) = std::fs::read_dir("/System/Library/Desktop Pictures")
            .ok()
            .and_then(|rd| rd.flatten().map(|d| d.path()).find(|p| p.extension().is_some_and(|x| x == "heic")))
        else {
            return;
        };
        let img = rt().block_on(image_from_file(heic, false, 512)).unwrap().expect("quicklook thumbnail");
        assert!(img.size[0] <= 512 && img.size[1] <= 512 && img.size[0] > 0);
    }
}
