//! Local filesystem + mounted volumes backend. (stub — to be implemented)
use fellor_vfs::*;

/// `local` backend rooted at "/". Volumes under /Volumes get their own
/// backend ids `vol:<name>` rooted at `/Volumes/<name>`.
pub struct LocalBackend;

impl LocalBackend {
    /// Backend id "local", root "/".
    pub fn new() -> BackendRef { todo!() }
    /// Volume backend: id "vol:<name>", root `mount_point`.
    pub fn volume(_name: &str, _mount_point: std::path::PathBuf) -> BackendRef { todo!() }
}

/// Convert a host path to a `local` VPath and back.
pub fn vpath_from_host(p: &std::path::Path) -> VPath { VPath::from_str_path("local", &p.to_string_lossy()) }

/// Watches /Volumes; emits Attached/Detached for removable volumes
/// (skips the boot volume symlink). Also emits Attached for `local` once.
pub fn spawn_discovery(_tx: DeviceEventTx) -> tokio::task::JoinHandle<()> { tokio::spawn(async {}) }
