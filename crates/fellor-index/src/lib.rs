//! "Already imported" index for Quick Import. (stub — public API is frozen)
use fellor_vfs::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct ImportIndex;

#[derive(Debug, Clone)]
pub struct ImportPlan {
    /// New media files on the device (not yet in the index), sorted by mtime.
    pub new_files: Vec<Entry>,
    pub total_bytes: u64,
    /// Live Photo pairs etc. are included as separate entries.
    pub already_imported: u64,
}

impl ImportIndex {
    /// Default path: ~/Library/Application Support/Fellor/index.db
    pub fn default_path() -> PathBuf { todo!() }
    pub fn open(_path: &Path) -> Result<Self> { todo!() }
    pub fn is_imported(&self, _device: &str, _rel_path: &str, _size: u64, _mtime: Option<std::time::SystemTime>) -> Result<bool> { todo!() }
    pub fn mark_imported(&self, _device: &str, _rel_path: &str, _size: u64, _mtime: Option<std::time::SystemTime>, _dest: &str) -> Result<()> { todo!() }
}

/// Walk the device's media roots (iOS: /DCIM under Media root; MTP: */DCIM;
/// volumes: /DCIM) and return media files not yet imported.
pub async fn plan_import(_backend: Arc<dyn Backend>, _index: &ImportIndex) -> Result<ImportPlan> { todo!() }

/// Destination folder for a file given a template like "{date}" -> "2026-11-07".
pub fn dest_subdir(_template: &str, _entry: &Entry) -> String { todo!() }
