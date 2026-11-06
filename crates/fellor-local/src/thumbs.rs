use bytes::Bytes;
use fellor_vfs::{mime_from_name, Result};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

const DIRECT_LIMIT: u64 = 8 * 1024 * 1024;

fn cache_dir() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("Fellor").join("thumbs"))
}

/// JPEG/PNG under 8 MB are returned as-is; anything else image-like
/// (HEIC, DNG, big JPEGs, ...) goes through `sips -s format jpeg -Z 256`,
/// cached in ~/Library/Caches/Fellor/thumbs keyed by (path, mtime, size).
pub async fn thumbnail(host: &Path) -> Result<Option<Bytes>> {
    let name = host.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Some(mime) = mime_from_name(&name) else { return Ok(None) };
    if !mime.starts_with("image/") {
        return Ok(None);
    }
    let meta = match tokio::fs::metadata(host).await {
        Ok(m) if m.is_file() => m,
        _ => return Ok(None),
    };
    if (mime == "image/jpeg" || mime == "image/png") && meta.len() < DIRECT_LIMIT {
        return Ok(tokio::fs::read(host).await.ok().map(Bytes::from));
    }
    let Some(dir) = cache_dir() else { return Ok(None) };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    host.hash(&mut h);
    meta.len().hash(&mut h);
    meta.modified().ok().hash(&mut h);
    let out = dir.join(format!("{:016x}.jpg", h.finish()));
    if let Ok(b) = tokio::fs::read(&out).await {
        return Ok(Some(Bytes::from(b)));
    }
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        return Ok(None);
    }
    let status = tokio::process::Command::new("sips")
        .args(["-s", "format", "jpeg", "-Z", "256"])
        .arg(host)
        .arg("--out")
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    match status {
        Ok(s) if s.success() => Ok(tokio::fs::read(&out).await.ok().map(Bytes::from)),
        _ => {
            let _ = tokio::fs::remove_file(&out).await;
            Ok(None)
        }
    }
}
