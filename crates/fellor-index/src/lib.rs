//! "Already imported" index for Quick Import (SPEC §3.4).
//!
//! SQLite table `imported(device_id, rel_path, size, mtime, sha_prefix,
//! imported_at, dest)`. A file is "new" when there is no row with the same
//! `(device_id, rel_path, size, mtime)`; mtimes are compared at one-second
//! precision (MTP only has seconds).
use fellor_vfs::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct ImportIndex {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct ImportPlan {
    /// New media files on the device (not yet in the index), sorted by mtime.
    pub new_files: Vec<Entry>,
    pub total_bytes: u64,
    /// Live Photo pairs etc. are included as separate entries.
    pub already_imported: u64,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS imported (
    device_id   TEXT    NOT NULL,
    rel_path    TEXT    NOT NULL,
    size        INTEGER NOT NULL,
    mtime       INTEGER NOT NULL,   -- unix seconds, -1 = unknown
    sha_prefix  TEXT,
    imported_at INTEGER NOT NULL,
    dest        TEXT    NOT NULL,
    PRIMARY KEY (device_id, rel_path, size, mtime)
);
";

fn db_err(e: rusqlite::Error) -> VfsError {
    VfsError::Other(format!("индекс импорта: {e}"))
}

fn secs(t: Option<SystemTime>) -> i64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(-1)
}

/// Device-relative path stored in the index ("/DCIM/100APPLE/IMG_0001.HEIC").
pub fn rel_path(entry: &Entry) -> String {
    entry.path.as_unix()
}

impl ImportIndex {
    /// Default path: ~/Library/Application Support/Fellor/index.db
    /// (parent directories are created).
    pub fn default_path() -> PathBuf {
        let dir = dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("Fellor");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("index.db")
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(VfsError::Io)?;
            }
        }
        let conn = Connection::open(path).map_err(db_err)?;
        Self::init(conn)
    }

    /// Throw-away index (tests, "import everything" mode).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().map_err(db_err)?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(db_err)?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.execute_batch(SCHEMA).map_err(db_err)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn is_imported(&self, device: &str, rel_path: &str, size: u64, mtime: Option<SystemTime>) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let hit: Option<i64> = c
            .query_row(
                "SELECT 1 FROM imported WHERE device_id=?1 AND rel_path=?2 AND size=?3 AND mtime=?4",
                params![device, rel_path, size as i64, secs(mtime)],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        Ok(hit.is_some())
    }

    pub fn mark_imported(&self, device: &str, rel_path: &str, size: u64, mtime: Option<SystemTime>, dest: &str) -> Result<()> {
        let now = secs(Some(SystemTime::now()));
        self.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT OR REPLACE INTO imported (device_id, rel_path, size, mtime, sha_prefix, imported_at, dest)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
                params![device, rel_path, size as i64, secs(mtime), now, dest],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// [`is_imported`](Self::is_imported) for an entry from `plan_import`.
    pub fn is_entry_imported(&self, entry: &Entry) -> Result<bool> {
        self.is_imported(&entry.path.backend, &rel_path(entry), entry.size.unwrap_or(0), entry.mtime)
    }

    /// [`mark_imported`](Self::mark_imported) for an entry from `plan_import`.
    pub fn mark_entry_imported(&self, entry: &Entry, dest: &str) -> Result<()> {
        self.mark_imported(&entry.path.backend, &rel_path(entry), entry.size.unwrap_or(0), entry.mtime, dest)
    }

    /// Number of files recorded for a device.
    pub fn count(&self, device: &str) -> Result<u64> {
        let c = self.conn.lock().unwrap();
        c.query_row("SELECT COUNT(*) FROM imported WHERE device_id=?1", params![device], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
            .map_err(db_err)
    }

    /// Forget everything imported from a device ("import again").
    pub fn forget_device(&self, device: &str) -> Result<u64> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM imported WHERE device_id=?1", params![device]).map(|n| n as u64).map_err(db_err)
    }
}

fn is_media(e: &Entry) -> bool {
    let mime = e.mime.or_else(|| mime_from_name(&e.name));
    mime.is_some_and(|m| m.starts_with("image/") || m.starts_with("video/"))
}

async fn child_dir(backend: &dyn Backend, dir: &VPath, name: &str) -> Option<VPath> {
    let list = backend.list(dir).await.ok()?;
    list.into_iter().find(|e| e.is_dir() && e.name.eq_ignore_ascii_case(name)).map(|e| e.path)
}

/// DCIM directories of a device.
pub async fn media_roots(backend: &dyn Backend) -> Result<Vec<VPath>> {
    let id = backend.id();
    let mut found = Vec::new();
    if id.starts_with("ios:") {
        let roots = backend.roots().await?;
        if let Some(media) = roots.iter().find(|r| r.name.eq_ignore_ascii_case("Media")) {
            if let Some(d) = child_dir(backend, &media.path, "DCIM").await {
                found.push(d);
            }
        }
    } else if id.starts_with("mtp:") {
        for r in backend.roots().await? {
            if let Some(d) = child_dir(backend, &r.path, "DCIM").await {
                found.push(d);
            }
        }
    } else if id.starts_with("vol:") {
        if let Some(d) = child_dir(backend, &VPath::root(id.clone()), "DCIM").await {
            found.push(d);
        }
    }
    if found.is_empty() {
        // Fallback: any "DCIM" dir up to depth 2 under every root.
        for r in backend.roots().await? {
            if r.name.eq_ignore_ascii_case("DCIM") && r.is_dir() {
                found.push(r.path.clone());
                continue;
            }
            let Ok(level1) = backend.list(&r.path).await else { continue };
            for e in level1.iter().filter(|e| e.is_dir()) {
                if e.name.eq_ignore_ascii_case("DCIM") {
                    found.push(e.path.clone());
                } else if let Some(d) = child_dir(backend, &e.path, "DCIM").await {
                    found.push(d);
                }
            }
        }
        found.sort();
        found.dedup();
    }
    Ok(found)
}

/// Walk the device's media roots (iOS: /DCIM under Media root; MTP: */DCIM;
/// volumes: /DCIM) and return media files not yet imported.
pub async fn plan_import(backend: Arc<dyn Backend>, index: &ImportIndex) -> Result<ImportPlan> {
    let roots = media_roots(backend.as_ref()).await?;
    let mut new_files = Vec::new();
    let mut already = 0u64;
    let mut stack = roots;
    while let Some(dir) = stack.pop() {
        let entries = match backend.list(&dir).await {
            Ok(v) => v,
            Err(e) if e.is_retryable() => return Err(e),
            Err(e) => {
                tracing::warn!(%dir, error = %e, "skipping unreadable folder");
                continue;
            }
        };
        for e in entries {
            if e.hidden || e.name.starts_with('.') {
                continue;
            }
            match e.kind {
                EntryKind::Dir => stack.push(e.path),
                EntryKind::File if is_media(&e) => {
                    if index.is_entry_imported(&e)? {
                        already += 1;
                    } else {
                        new_files.push(e);
                    }
                }
                _ => {}
            }
        }
    }
    new_files.sort_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.path.cmp(&b.path)));
    let total_bytes = new_files.iter().map(|e| e.size.unwrap_or(0)).sum();
    Ok(ImportPlan { new_files, total_bytes, already_imported: already })
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c == '/' || c == ':' || c.is_control() { '-' } else { c }).collect::<String>().trim().to_owned()
}

/// Destination folder for a file given a template like "{date}" -> "2026-11-07".
/// Placeholders: `{date}` (YYYY-MM-DD), `{year}`, `{month}` (MM), `{day}` (DD),
/// `{device}` (empty here — see [`dest_subdir_with_device`]). Dates use the
/// file's mtime (falling back to ctime) in local time; unknown → "unknown".
/// Empty path segments are dropped, so "{device}/{date}" → "2026-11-07".
pub fn dest_subdir(template: &str, entry: &Entry) -> String {
    dest_subdir_with_device(template, entry, "")
}

/// [`dest_subdir`] with a device name for `{device}`.
pub fn dest_subdir_with_device(template: &str, entry: &Entry, device: &str) -> String {
    let (date, year, month, day) = match entry.mtime.or(entry.ctime) {
        Some(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            (dt.format("%Y-%m-%d").to_string(), dt.format("%Y").to_string(), dt.format("%m").to_string(), dt.format("%d").to_string())
        }
        None => ("unknown".into(), "unknown".into(), "unknown".into(), "unknown".into()),
    };
    let s = template
        .replace("{date}", &date)
        .replace("{year}", &year)
        .replace("{month}", &month)
        .replace("{day}", &day)
        .replace("{device}", &sanitize(device));
    s.split('/').map(str::trim).filter(|c| !c.is_empty() && *c != "." && *c != "..").collect::<Vec<_>>().join("/")
}

/// Group the plan's files by destination sub-directory (in order of first
/// appearance, i.e. oldest first). Callers turn each group into one
/// `TransferRequest { sources, dest_dir: dest_root/subdir, .. }`.
pub fn group_by_subdir(plan: &ImportPlan, template: &str) -> Vec<(String, Vec<VPath>)> {
    group_by_subdir_with_device(plan, template, "")
}

/// [`group_by_subdir`] with a device name for `{device}`.
pub fn group_by_subdir_with_device(plan: &ImportPlan, template: &str, device: &str) -> Vec<(String, Vec<VPath>)> {
    let mut out: Vec<(String, Vec<VPath>)> = Vec::new();
    let mut pos: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for e in &plan.new_files {
        let sub = dest_subdir_with_device(template, e, device);
        match pos.get(&sub) {
            Some(&i) => out[i].1.push(e.path.clone()),
            None => {
                pos.insert(sub.clone(), out.len());
                out.push((sub, vec![e.path.clone()]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
