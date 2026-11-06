//! Local filesystem + mounted volumes backend.
//!
//! * `local` — the whole host filesystem rooted at "/".
//! * `vol:<name>` — a mounted volume; VPaths are relative to its mount point.
//!
//! Writes go through a hidden temp file in the destination directory
//! (`.<name>.fellor-tmp-<rand>`), are fsync'ed and then renamed into place
//! without clobbering an existing file.

mod discovery;
mod errors;
mod fsops;
mod thumbs;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use fellor_vfs::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub use discovery::{list_volumes, spawn_discovery_in};
pub use errors::map_io;

/// `local` backend rooted at "/". Volumes under /Volumes get their own
/// backend ids `vol:<name>` rooted at `/Volumes/<name>`.
pub struct LocalBackend {
    id: BackendId,
    root: PathBuf,
    name: String,
    kind: DeviceKind,
    ejectable: bool,
}

impl LocalBackend {
    /// Backend id "local", root "/".
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> BackendRef {
        Arc::new(LocalBackend { id: "local".into(), root: PathBuf::from("/"), name: computer_name(), kind: DeviceKind::Mac, ejectable: false })
    }

    /// Volume backend: id "vol:<name>", root `mount_point`.
    pub fn volume(name: &str, mount_point: PathBuf) -> BackendRef {
        Arc::new(LocalBackend { id: format!("vol:{name}"), root: mount_point, name: name.to_owned(), kind: DeviceKind::Volume, ejectable: true })
    }

    /// A non-ejectable backend with an arbitrary id rooted at `root`
    /// (tests, sandboxes, "pretend device" setups).
    pub fn rooted(id: &str, root: PathBuf) -> BackendRef {
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| id.to_owned());
        Arc::new(LocalBackend { id: id.to_owned(), root, name, kind: DeviceKind::Volume, ejectable: false })
    }

    fn host(&self, p: &VPath) -> Result<PathBuf> {
        let mut out = self.root.clone();
        for c in &p.components {
            if c == ".." || c.contains('/') || c.contains('\0') {
                return Err(VfsError::PermissionDenied(format!("недопустимый компонент пути: {c}")));
            }
            out.push(c);
        }
        Ok(out)
    }

    fn entry_for(&self, path: VPath, host: &Path, meta: &std::fs::Metadata) -> Entry {
        let name = if path.is_root() { root_display_name(&self.root) } else { path.name().to_owned() };
        let ft = meta.file_type();
        let kind = if ft.is_dir() {
            EntryKind::Dir
        } else if ft.is_file() {
            EntryKind::File
        } else if ft.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Special
        };
        // For symlinks, report the target's size/mtime when it resolves.
        let target_meta = if kind == EntryKind::Symlink { std::fs::metadata(host).ok() } else { None };
        let m = target_meta.as_ref().unwrap_or(meta);
        let is_dir_like = m.is_dir();
        Entry {
            mime: if is_dir_like { None } else { mime_from_name(&name) },
            hidden: name.starts_with('.'),
            size: if is_dir_like { None } else { Some(m.len()) },
            mtime: m.modified().ok(),
            ctime: m.created().ok(),
            native: NativeId::Path(host.to_string_lossy().into_owned()),
            name,
            path,
            kind,
        }
    }

    async fn stat_host(&self, path: &VPath, follow: bool) -> Result<Entry> {
        let host = self.host(path)?;
        let meta = if follow { tokio::fs::metadata(&host).await } else { tokio::fs::symlink_metadata(&host).await };
        let meta = meta.map_err(|e| map_io(e, &host))?;
        Ok(self.entry_for(path.clone(), &host, &meta))
    }
}

fn root_display_name(root: &Path) -> String {
    root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into())
}

/// Computer name for the `local` backend ("MacBook Pro Алекса").
fn computer_name() -> String {
    if let Ok(out) = std::process::Command::new("scutil").args(["--get", "ComputerName"]).output() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if out.status.success() && !s.is_empty() {
            return s;
        }
    }
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for buf.len() bytes; gethostname NUL-terminates on success.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let s = String::from_utf8_lossy(&buf[..end]).trim_end_matches(".local").to_owned();
        if !s.is_empty() {
            return s;
        }
    }
    "Mac".into()
}

/// Convert a host path to a `local` VPath and back.
pub fn vpath_from_host(p: &std::path::Path) -> VPath {
    VPath::from_str_path("local", &p.to_string_lossy())
}

/// Inverse of [`vpath_from_host`]: host path for a `local:` VPath
/// (`None` for any other backend).
pub fn host_from_vpath(p: &VPath) -> Option<PathBuf> {
    (p.backend == "local").then(|| PathBuf::from(p.as_unix()))
}

/// Watches /Volumes; emits Attached/Detached for removable volumes
/// (skips the boot volume symlink). Also emits Attached for `local` once.
pub fn spawn_discovery(tx: DeviceEventTx) -> tokio::task::JoinHandle<()> {
    spawn_discovery_in(tx, PathBuf::from("/Volumes"), std::time::Duration::from_secs(2), true)
}

#[async_trait]
impl Backend for LocalBackend {
    fn id(&self) -> BackendId {
        self.id.clone()
    }

    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            model: None,
            os_version: None,
            kind: self.kind,
            state: DeviceState::Ready,
            ejectable: self.ejectable,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            read: true,
            write: true,
            delete: true,
            rename: true,
            move_within: true,
            mkdir: true,
            set_mtime: true,
            partial_read: true,
            free_space: true,
            max_parallel: 4,
        }
    }

    async fn roots(&self) -> Result<Vec<Entry>> {
        Ok(vec![self.stat_host(&VPath::root(self.id.clone()), true).await?])
    }

    async fn list(&self, path: &VPath) -> Result<Vec<Entry>> {
        let host = self.host(path)?;
        let mut rd = tokio::fs::read_dir(&host).await.map_err(|e| map_io(e, &host))?;
        let mut out = Vec::new();
        while let Some(de) = rd.next_entry().await.map_err(|e| map_io(e, &host))? {
            let name = de.file_name().to_string_lossy().into_owned();
            let child = de.path();
            let meta = match tokio::fs::symlink_metadata(&child).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::debug!(path = %child.display(), error = %e, "skipping unreadable entry");
                    continue;
                }
            };
            out.push(self.entry_for(path.join(name), &child, &meta));
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn stat(&self, path: &VPath) -> Result<Entry> {
        // Follows symlinks (like `stat(2)`): `/tmp` reports as a directory.
        match self.stat_host(path, true).await {
            Err(VfsError::NotFound(_)) => self.stat_host(path, false).await, // dangling link
            r => r,
        }
    }

    async fn free_space(&self, path: &VPath) -> Result<Option<SpaceInfo>> {
        let mut host = self.host(path)?;
        // Walk up to the nearest existing ancestor (destination may not exist yet).
        while !host.exists() {
            if !host.pop() {
                break;
            }
        }
        let h = host.clone();
        tokio::task::spawn_blocking(move || fsops::statvfs(&h).map(Some)).await.map_err(|e| VfsError::Other(e.to_string()))?
    }

    async fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn ReadSource>> {
        let host = self.host(path)?;
        let mut f = tokio::fs::File::open(&host).await.map_err(|e| map_io(e, &host))?;
        let meta = f.metadata().await.map_err(|e| map_io(e, &host))?;
        if meta.is_dir() {
            return Err(VfsError::IsDir(host.display().to_string()));
        }
        if offset > 0 {
            f.seek(std::io::SeekFrom::Start(offset)).await.map_err(|e| map_io(e, &host))?;
        }
        Ok(Box::new(LocalReader { file: f, size: meta.len(), host }))
    }

    async fn create_write(&self, path: &VPath, _size: u64, mtime: Option<SystemTime>) -> Result<Box<dyn WriteSink>> {
        let host = self.host(path)?;
        if path.is_root() {
            return Err(VfsError::IsDir(host.display().to_string()));
        }
        if tokio::fs::symlink_metadata(&host).await.is_ok() {
            return Err(VfsError::AlreadyExists(host.display().to_string()));
        }
        let parent = host.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
        let tmp = parent.join(format!(".{}.fellor-tmp-{}", path.name(), fsops::rand_suffix()));
        let file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).await.map_err(|e| map_io(e, &host))?;
        Ok(Box::new(LocalWriter {
            file: Some(file),
            tmp: tmp.clone(),
            dst: host,
            vpath: path.clone(),
            mtime,
            backend_root: self.root.clone(),
            backend_id: self.id.clone(),
            done: false,
        }))
    }

    async fn mkdir(&self, path: &VPath) -> Result<Entry> {
        let host = self.host(path)?;
        tokio::fs::create_dir(&host).await.map_err(|e| map_io(e, &host))?;
        self.stat(path).await
    }

    async fn remove(&self, path: &VPath, recursive: bool) -> Result<()> {
        let host = self.host(path)?;
        if path.is_root() {
            return Err(VfsError::PermissionDenied("нельзя удалить корень".into()));
        }
        let meta = tokio::fs::symlink_metadata(&host).await.map_err(|e| map_io(e, &host))?;
        let r = if meta.is_dir() {
            if recursive {
                tokio::fs::remove_dir_all(&host).await
            } else {
                tokio::fs::remove_dir(&host).await
            }
        } else {
            tokio::fs::remove_file(&host).await
        };
        r.map_err(|e| map_io(e, &host))
    }

    async fn rename(&self, from: &VPath, to: &VPath) -> Result<Entry> {
        let a = self.host(from)?;
        let b = self.host(to)?;
        let (a2, b2) = (a.clone(), b.clone());
        tokio::task::spawn_blocking(move || fsops::rename_noclobber(&a2, &b2))
            .await
            .map_err(|e| VfsError::Other(e.to_string()))?
            .map_err(|e| map_io(e, &b))?;
        self.stat(to).await
    }

    async fn set_mtime(&self, path: &VPath, mtime: SystemTime) -> Result<()> {
        let host = self.host(path)?;
        let h = host.clone();
        tokio::task::spawn_blocking(move || std::fs::File::open(&h).and_then(|f| f.set_modified(mtime)))
            .await
            .map_err(|e| VfsError::Other(e.to_string()))?
            .map_err(|e| map_io(e, &host))
    }

    async fn thumbnail(&self, path: &VPath) -> Result<Option<Bytes>> {
        let host = self.host(path)?;
        thumbs::thumbnail(&host).await
    }

    async fn eject(&self) -> Result<()> {
        if !self.ejectable {
            return Err(VfsError::Unsupported("eject"));
        }
        let out = tokio::process::Command::new("diskutil")
            .arg("eject")
            .arg(&self.root)
            .output()
            .await
            .map_err(|e| VfsError::Other(format!("diskutil: {e}")))?;
        if out.status.success() {
            Ok(())
        } else {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            let msg = if msg.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_owned() } else { msg };
            if msg.contains("in use") || msg.contains("busy") || msg.contains("dissent") {
                Err(VfsError::Busy(Some(msg)))
            } else {
                Err(VfsError::Other(format!("не удалось извлечь: {msg}")))
            }
        }
    }
}

struct LocalReader {
    file: tokio::fs::File,
    size: u64,
    host: PathBuf,
}

#[async_trait]
impl ReadSource for LocalReader {
    async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>> {
        let max = max.max(1);
        let mut buf = BytesMut::zeroed(max);
        let mut filled = 0;
        // Fill as much of the buffer as possible (tokio reads are capped at 2 MiB).
        while filled < max {
            let n = self.file.read(&mut buf[filled..]).await.map_err(|e| map_io(e, &self.host))?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            return Ok(None);
        }
        buf.truncate(filled);
        Ok(Some(buf.freeze()))
    }

    fn size(&self) -> Option<u64> {
        Some(self.size)
    }
}

struct LocalWriter {
    file: Option<tokio::fs::File>,
    tmp: PathBuf,
    dst: PathBuf,
    vpath: VPath,
    mtime: Option<SystemTime>,
    backend_root: PathBuf,
    backend_id: BackendId,
    done: bool,
}

#[async_trait]
impl WriteSink for LocalWriter {
    async fn write_chunk(&mut self, data: Bytes) -> Result<()> {
        let f = self.file.as_mut().ok_or_else(|| VfsError::Other("запись уже завершена".into()))?;
        f.write_all(&data).await.map_err(|e| map_io(e, &self.dst))
    }

    async fn finish(mut self: Box<Self>) -> Result<Entry> {
        let mut f = self.file.take().ok_or_else(|| VfsError::Other("запись уже завершена".into()))?;
        f.flush().await.map_err(|e| map_io(e, &self.dst))?;
        let std_file = f.into_std().await;
        let (tmp, dst, mtime) = (self.tmp.clone(), self.dst.clone(), self.mtime);
        let res = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std_file.sync_all()?;
            if let Some(t) = mtime {
                let _ = std_file.set_modified(t);
            }
            drop(std_file);
            fsops::rename_noclobber(&tmp, &dst)
        })
        .await
        .map_err(|e| VfsError::Other(e.to_string()))?;
        if let Err(e) = res {
            let _ = tokio::fs::remove_file(&self.tmp).await;
            self.done = true;
            return Err(map_io(e, &self.dst));
        }
        self.done = true;
        let meta = tokio::fs::symlink_metadata(&self.dst).await.map_err(|e| map_io(e, &self.dst))?;
        let be = LocalBackend { id: self.backend_id.clone(), root: self.backend_root.clone(), name: String::new(), kind: DeviceKind::Volume, ejectable: false };
        Ok(be.entry_for(self.vpath.clone(), &self.dst, &meta))
    }

    async fn abort(mut self: Box<Self>) -> Result<()> {
        drop(self.file.take());
        self.done = true;
        match tokio::fs::remove_file(&self.tmp).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(map_io(e, &self.tmp)),
        }
    }
}

impl Drop for LocalWriter {
    fn drop(&mut self) {
        if !self.done {
            drop(self.file.take());
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

#[cfg(test)]
mod tests;
