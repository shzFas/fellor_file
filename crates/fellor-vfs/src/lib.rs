//! Fellor VFS — the shared contract between protocol backends, the transfer
//! engine and the UI. Nothing above this layer knows whether a file lives on
//! an iPhone, an Android phone or a local disk.
//!
//! THIS CRATE IS THE FROZEN CONTRACT. Do not change signatures without
//! coordinating across all crates.

mod error;
mod path;

pub use error::{Result, VfsError};
pub use path::VPath;

use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;
use std::time::SystemTime;

/// Stable backend key: `"local"`, `"ios:<udid>"`, `"mtp:<serial>"`, `"vol:<volume-name>"`.
pub type BackendId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Special,
}

/// Backend-private identifier carried alongside an entry so backends can
/// avoid re-resolving paths (e.g. MTP object handles).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum NativeId {
    #[default]
    None,
    /// MTP/PTP storage id + object handle.
    Mtp { storage: u32, handle: u32 },
    /// Native path string on the device / host (AFC path, local path).
    Path(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: VPath,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub mtime: Option<SystemTime>,
    pub ctime: Option<SystemTime>,
    /// Lowercase MIME guess (from extension or PTP format code).
    pub mime: Option<&'static str>,
    pub hidden: bool,
    pub native: NativeId,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub read: bool,
    pub write: bool,
    pub delete: bool,
    pub rename: bool,
    /// Rename can move across directories within the backend.
    pub move_within: bool,
    pub mkdir: bool,
    pub set_mtime: bool,
    /// `open_read` honours a non-zero offset (resume).
    pub partial_read: bool,
    pub free_space: bool,
    /// Max concurrent operations the backend tolerates (1 for AFC/MTP sessions).
    pub max_parallel: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DeviceKind {
    Mac,
    Volume,
    IPhone,
    IPad,
    IPod,
    Android,
    Camera,
}

/// Connection state shown in the sidebar. `Ready` means `Backend` calls work;
/// any other state is a user-actionable hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceState {
    Ready,
    /// iOS: no pair record yet — user must tap "Trust".
    NeedsTrust,
    /// iOS: lockdown returned PasswordProtected / Android: zero storages.
    Locked,
    /// Android: phone is in "charging only" USB mode.
    NeedsMtpMode,
    /// USB interface is claimed by another program (name if known).
    Busy { by: Option<String> },
    Connecting,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: BackendId,
    /// User-visible name, e.g. "iPhone Алекса" / "Pixel 8" / "Macintosh HD".
    pub name: String,
    /// Model string, e.g. "iPhone15,2" / "Pixel 8".
    pub model: Option<String>,
    pub os_version: Option<String>,
    pub kind: DeviceKind,
    pub state: DeviceState,
    /// Volumes / USB media that can be ejected.
    pub ejectable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceInfo {
    pub total: u64,
    pub free: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsEvent {
    Added(VPath),
    Removed(VPath),
    Changed(VPath),
    /// Something changed; refresh everything under this path.
    Invalidate(VPath),
}

/// Streaming reader. Returns `Ok(None)` on EOF. Chunks are at most `max` bytes
/// but may be smaller.
#[async_trait]
pub trait ReadSource: Send {
    async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>>;
    /// Total size if known.
    fn size(&self) -> Option<u64>;
}

/// Streaming writer. The file is not visible at its final path until
/// `finish` succeeds (temp-file + rename where the protocol allows).
/// Dropping without `finish` must leave no partial file behind where possible;
/// call `abort` for an explicit async cleanup.
#[async_trait]
pub trait WriteSink: Send {
    async fn write_chunk(&mut self, data: Bytes) -> Result<()>;
    async fn finish(self: Box<Self>) -> Result<Entry>;
    async fn abort(self: Box<Self>) -> Result<()>;
}

#[async_trait]
pub trait Backend: Send + Sync {
    fn id(&self) -> BackendId;
    fn info(&self) -> DeviceInfo;
    fn capabilities(&self) -> Capabilities;

    /// Top-level entries shown when the device is opened (iOS: "Media" + one
    /// dir per File-Sharing app; MTP: one dir per storage; local: "/").
    async fn roots(&self) -> Result<Vec<Entry>>;
    async fn list(&self, path: &VPath) -> Result<Vec<Entry>>;
    async fn stat(&self, path: &VPath) -> Result<Entry>;
    async fn free_space(&self, path: &VPath) -> Result<Option<SpaceInfo>>;

    async fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn ReadSource>>;
    /// `size` is the exact byte count that will be written (MTP needs it up front).
    /// Fails with `AlreadyExists` if `path` exists — conflict policy lives in
    /// the transfer engine, which removes first if it decided to overwrite.
    async fn create_write(&self, path: &VPath, size: u64, mtime: Option<SystemTime>) -> Result<Box<dyn WriteSink>>;

    async fn mkdir(&self, path: &VPath) -> Result<Entry>;
    async fn remove(&self, path: &VPath, recursive: bool) -> Result<()>;
    async fn rename(&self, from: &VPath, to: &VPath) -> Result<Entry>;
    async fn set_mtime(&self, path: &VPath, mtime: SystemTime) -> Result<()>;

    /// Optional change feed.
    fn watch(&self) -> Option<tokio::sync::broadcast::Receiver<FsEvent>> {
        None
    }

    /// Optional small thumbnail (JPEG/PNG bytes) for previews.
    async fn thumbnail(&self, _path: &VPath) -> Result<Option<Bytes>> {
        Ok(None)
    }

    /// Eject/close; backend becomes unusable afterwards.
    async fn eject(&self) -> Result<()> {
        Err(VfsError::Unsupported("eject"))
    }
}

pub type BackendRef = Arc<dyn Backend>;

/// Emitted by every discovery source; merged by `fellor-devices`.
#[derive(Clone)]
pub enum DeviceEvent {
    /// New device (or a device that became usable). The backend may be in a
    /// non-Ready state; check `info().state`.
    Attached(BackendRef),
    /// Same id as before, new state/info (e.g. user tapped Trust).
    Updated(BackendRef),
    Detached(BackendId),
}

impl std::fmt::Debug for DeviceEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceEvent::Attached(b) => write!(f, "Attached({})", b.id()),
            DeviceEvent::Updated(b) => write!(f, "Updated({})", b.id()),
            DeviceEvent::Detached(id) => write!(f, "Detached({id})"),
        }
    }
}

pub type DeviceEventTx = tokio::sync::mpsc::UnboundedSender<DeviceEvent>;

/// Guess a MIME type from a file name's extension.
pub fn mime_from_name(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "heic" | "heif" => "image/heic",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "dng" => "image/x-adobe-dng",
        "mov" => "video/quicktime",
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain",
        "zip" => "application/zip",
        "apk" => "application/vnd.android.package-archive",
        _ => return None,
    })
}

/// Shared map BackendId -> backend, kept up to date by `fellor-devices`
/// and read by the transfer engine and UI.
#[derive(Clone, Default)]
pub struct BackendRegistry {
    inner: Arc<std::sync::RwLock<std::collections::HashMap<BackendId, BackendRef>>>,
}

impl BackendRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&self, b: BackendRef) {
        self.inner.write().unwrap().insert(b.id(), b);
    }
    pub fn remove(&self, id: &str) -> Option<BackendRef> {
        self.inner.write().unwrap().remove(id)
    }
    pub fn get(&self, id: &str) -> Option<BackendRef> {
        self.inner.read().unwrap().get(id).cloned()
    }
    pub fn all(&self) -> Vec<BackendRef> {
        self.inner.read().unwrap().values().cloned().collect()
    }
}
