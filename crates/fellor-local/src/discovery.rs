use crate::LocalBackend;
use fellor_vfs::{DeviceEvent, DeviceEventTx};
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Real mount points directly under `dir` as `(display name, mount path)`.
/// Skips symlinks (the boot volume link `Macintosh HD -> /`), plain folders
/// that live on the same device as `dir` (stale mount-point leftovers) and
/// hidden entries.
pub fn list_volumes(dir: &Path) -> Vec<(String, PathBuf)> {
    list_volumes_impl(dir, true)
}

fn list_volumes_impl(dir: &Path, require_mount: bool) -> Vec<(String, PathBuf)> {
    let Ok(parent_meta) = std::fs::metadata(dir) else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for de in rd.flatten() {
        let name = de.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = de.path();
        let Ok(lmeta) = std::fs::symlink_metadata(&path) else { continue };
        if lmeta.file_type().is_symlink() || !lmeta.is_dir() {
            continue;
        }
        if require_mount && lmeta.dev() == parent_meta.dev() {
            continue; // not a mount point
        }
        if std::fs::canonicalize(&path).map(|c| c == Path::new("/")).unwrap_or(false) {
            continue;
        }
        out.push((name, path));
    }
    out.sort();
    out
}

/// Discovery loop over an arbitrary volumes directory.
///
/// With `real_mounts_only == true` (production, `/Volumes`) only real mount
/// points are reported; tests pass `false` to treat every sub-directory of a
/// temp dir as a volume. Emits `Attached(local)` first when `emit_local` is set.
pub fn spawn_discovery_in(tx: DeviceEventTx, dir: PathBuf, interval: Duration, real_mounts_only: bool) -> tokio::task::JoinHandle<()> {
    let emit_local = real_mounts_only;
    tokio::spawn(async move {
        if emit_local && tx.send(DeviceEvent::Attached(LocalBackend::new())).is_err() {
            return;
        }
        let mut known: BTreeMap<String, PathBuf> = BTreeMap::new();
        loop {
            let d = dir.clone();
            let now: BTreeMap<String, PathBuf> = tokio::task::spawn_blocking(move || list_volumes_impl(&d, real_mounts_only))
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();
            for (name, path) in &now {
                if known.get(name) != Some(path) {
                    if known.contains_key(name) {
                        let _ = tx.send(DeviceEvent::Detached(format!("vol:{name}")));
                    }
                    if tx.send(DeviceEvent::Attached(LocalBackend::volume(name, path.clone()))).is_err() {
                        return;
                    }
                }
            }
            for name in known.keys() {
                if !now.contains_key(name) && tx.send(DeviceEvent::Detached(format!("vol:{name}"))).is_err() {
                    return;
                }
            }
            known = now;
            tokio::time::sleep(interval).await;
        }
    })
}
