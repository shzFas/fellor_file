//! `IosBackend`: fellor-vfs `Backend` over AFC.
//!
//! VPath scheme (backend id `ios:<udid>`):
//!
//! ```text
//! /                          roots: "Media", "Apps"
//! /Media/<p...>              com.apple.afc  → AFC path "/<p...>"
//! /Apps                      one dir per File-Sharing app (display name)
//! /Apps/<Name>/<p...>        house_arrest VendDocuments → AFC "/Documents/<p...>"
//! ```
//!
//! All AFC traffic goes through one `tokio::sync::Mutex<Session>`
//! (`max_parallel = 1`). A broken connection is dropped and transparently
//! re-opened on the next call; open read/write handles on the old
//! connection fail with a retryable `Transport` error.

use crate::connector::{AfcTarget, Connector};
use async_trait::async_trait;
use bytes::Bytes;
use fellor_afc::{house_arrest, time_to_ns, AfcClient, AppInfo, FileInfo, FileType, OpenMode, MAX_IO_CHUNK};
use fellor_vfs::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Mutex;

pub const MEDIA_ROOT: &str = "Media";
pub const APPS_ROOT: &str = "Apps";
const APP_LIST_TTL: Duration = Duration::from_secs(30);
const MAX_APP_CONNS: usize = 4;
pub const TEMP_PREFIX: &str = ".fellor-tmp-";

static CONN_IDS: AtomicU64 = AtomicU64::new(1);

struct Conn {
    id: u64,
    afc: AfcClient,
}

impl Conn {
    fn new(afc: AfcClient) -> Self {
        Conn { id: CONN_IDS.fetch_add(1, Ordering::Relaxed), afc }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDir {
    /// Directory name shown under /Apps (sanitized, unique).
    pub name: String,
    pub bundle_id: String,
}

/// Turn app infos into unique, '/'-free directory names.
pub fn app_dirs(apps: &[AppInfo]) -> Vec<AppDir> {
    let clean = |s: &str| {
        let c: String = s.chars().map(|ch| if ch == '/' || ch == '\0' { '_' } else { ch }).collect();
        let c = c.trim().to_string();
        if c.is_empty() || c == "." || c == ".." {
            None
        } else {
            Some(c)
        }
    };
    let mut counts: HashMap<String, usize> = HashMap::new();
    let base: Vec<String> = apps.iter().map(|a| clean(&a.display_name).or_else(|| clean(&a.bundle_id)).unwrap_or_else(|| "App".into())).collect();
    for b in &base {
        *counts.entry(b.to_lowercase()).or_default() += 1;
    }
    apps.iter()
        .zip(base)
        .map(|(a, b)| {
            let name = if counts.get(&b.to_lowercase()).copied().unwrap_or(0) > 1 {
                clean(&format!("{b} ({})", a.bundle_id)).unwrap_or(b)
            } else {
                b
            };
            AppDir { name, bundle_id: a.bundle_id.clone() }
        })
        .collect()
}

#[derive(Default)]
struct Session {
    media: Option<Conn>,
    apps: HashMap<String, Conn>,
    app_list: Option<(Instant, Arc<Vec<AppDir>>)>,
}

impl Session {
    async fn conn(&mut self, connector: &dyn Connector, target: &AfcTarget) -> Result<&mut Conn> {
        match target {
            AfcTarget::Media => {
                if self.media.is_none() {
                    self.media = Some(Conn::new(connector.open_afc(target).await?));
                }
                self.media.as_mut().ok_or(VfsError::Disconnected)
            }
            AfcTarget::App(bundle) => {
                if !self.apps.contains_key(bundle) {
                    if self.apps.len() >= MAX_APP_CONNS {
                        self.apps.clear();
                    }
                    let c = Conn::new(connector.open_afc(target).await?);
                    self.apps.insert(bundle.clone(), c);
                }
                self.apps.get_mut(bundle).ok_or(VfsError::Disconnected)
            }
        }
    }

    /// The connection a handle was opened on, if it is still alive.
    fn existing(&mut self, target: &AfcTarget, conn_id: u64) -> Result<&mut AfcClient> {
        let c = match target {
            AfcTarget::Media => self.media.as_mut(),
            AfcTarget::App(b) => self.apps.get_mut(b),
        };
        match c {
            Some(c) if c.id == conn_id => Ok(&mut c.afc),
            _ => Err(VfsError::Transport("AFC connection was reset".into())),
        }
    }

    fn invalidate(&mut self, target: &AfcTarget) {
        match target {
            AfcTarget::Media => self.media = None,
            AfcTarget::App(b) => {
                self.apps.remove(b);
            }
        }
    }
}

/// Errors after which the AFC stream can't be trusted any more.
fn is_broken(e: &VfsError) -> bool {
    matches!(e, VfsError::Transport(_) | VfsError::Disconnected | VfsError::Timeout | VfsError::Protocol(_) | VfsError::Io(_))
}

struct Inner {
    id: BackendId,
    info: DeviceInfo,
    connector: Option<Arc<dyn Connector>>,
    session: Mutex<Session>,
}

impl Inner {
    fn connector(&self) -> Result<Arc<dyn Connector>> {
        self.connector.clone().ok_or_else(|| state_error(&self.info.state))
    }
}

fn state_error(state: &DeviceState) -> VfsError {
    match state {
        DeviceState::NeedsTrust => VfsError::NeedsTrust,
        DeviceState::Locked => VfsError::Locked,
        DeviceState::Busy { by } => VfsError::Busy(by.clone()),
        DeviceState::Error(m) => VfsError::Other(m.clone()),
        DeviceState::Connecting => VfsError::Busy(None),
        _ => VfsError::Disconnected,
    }
}

/// Run `$body` with `$c: &mut AfcClient` for `$target`, reconnecting once on
/// a broken connection. Evaluates to `Result<T>`.
macro_rules! with_afc {
    ($inner:expr, $target:expr, |$c:ident| $body:expr) => {{
        let inner: &Inner = &$inner;
        let target: &AfcTarget = $target;
        let connector = inner.connector()?;
        let mut session = inner.session.lock().await;
        let mut attempt = 0u32;
        loop {
            let res = match session.conn(&*connector, target).await {
                Ok(conn) => {
                    #[allow(unused_mut)]
                    let mut $c = &mut conn.afc;
                    $body.await
                }
                Err(e) => Err(e),
            };
            match res {
                Err(e) if is_broken(&e) => {
                    session.invalidate(target);
                    attempt += 1;
                    if attempt < 2 {
                        tracing::debug!(error = %e, "AFC connection broken; reconnecting");
                        continue;
                    }
                    break Err(e);
                }
                r => break r,
            }
        }
    }};
}

/// A resolved VPath.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Loc {
    Root,
    AppsDir,
    Afc {
        target: AfcTarget,
        path: String,
        /// The sandbox root itself (/Media or /Apps/<Name>): can't be removed/renamed.
        is_root: bool,
    },
}

fn validate_components(c: &[String]) -> Result<()> {
    for s in c {
        if s.is_empty() || s == "." || s == ".." || s.contains('/') || s.contains('\0') {
            return Err(VfsError::Other(format!("недопустимое имя: {s:?}")));
        }
    }
    Ok(())
}

fn afc_join(base: &str, rest: &[String]) -> String {
    let mut p = base.trim_end_matches('/').to_string();
    for c in rest {
        p.push('/');
        p.push_str(c);
    }
    if p.is_empty() {
        p.push('/');
    }
    p
}

fn afc_parent(p: &str) -> String {
    match p.rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((a, _)) => a.into(),
    }
}

fn dir_entry(path: VPath, name: &str, native: NativeId) -> Entry {
    Entry {
        name: name.to_string(),
        path,
        kind: EntryKind::Dir,
        size: None,
        mtime: None,
        ctime: None,
        mime: None,
        hidden: false,
        native,
    }
}

fn entry_from_info(path: VPath, afc_path: &str, info: &FileInfo) -> Entry {
    let name = path.name().to_string();
    let kind = match info.kind {
        FileType::File => EntryKind::File,
        FileType::Dir => EntryKind::Dir,
        FileType::Symlink => EntryKind::Symlink,
        _ => EntryKind::Special,
    };
    Entry {
        hidden: name.starts_with('.'),
        mime: if kind == EntryKind::File { mime_from_name(&name) } else { None },
        size: if kind == EntryKind::Dir { None } else { Some(info.size) },
        mtime: info.mtime(),
        ctime: info.birthtime(),
        kind,
        name,
        path,
        native: NativeId::Path(afc_path.to_string()),
    }
}

/// Random-ish temp name without a RNG crate.
fn temp_name() -> String {
    use std::hash::{BuildHasher, Hasher};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    h.write_u128(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u32(std::process::id());
    format!("{TEMP_PREFIX}{:016x}", h.finish())
}

/// iOS device backend. Cheap to clone.
#[derive(Clone)]
pub struct IosBackend {
    inner: Arc<Inner>,
}

impl IosBackend {
    /// A ready backend.
    pub fn new(info: DeviceInfo, connector: Arc<dyn Connector>) -> Self {
        Self { inner: Arc::new(Inner { id: info.id.clone(), info, connector: Some(connector), session: Mutex::new(Session::default()) }) }
    }

    /// Ready backend with an already-open media AFC session.
    pub fn with_media_session(info: DeviceInfo, connector: Arc<dyn Connector>, media: AfcClient) -> Self {
        let b = Self::new(info, connector);
        if let Ok(mut s) = b.inner.session.try_lock() {
            s.media = Some(Conn::new(media));
        }
        b
    }

    /// A placeholder for a device that isn't usable yet (NeedsTrust/Locked/…).
    /// Every operation fails with the matching error.
    pub fn unavailable(info: DeviceInfo) -> Self {
        Self { inner: Arc::new(Inner { id: info.id.clone(), info, connector: None, session: Mutex::new(Session::default()) }) }
    }

    pub fn backend_id(udid: &str) -> BackendId {
        format!("ios:{udid}")
    }

    fn vpath(&self, comps: Vec<String>) -> VPath {
        VPath::new(self.inner.id.clone(), comps)
    }

    fn check_backend(&self, p: &VPath) -> Result<()> {
        if p.backend != self.inner.id {
            return Err(VfsError::NotFound(format!("{p} (другое устройство)")));
        }
        validate_components(&p.components)
    }

    async fn app_list(&self, force: bool) -> Result<Arc<Vec<AppDir>>> {
        let connector = self.inner.connector()?;
        let mut s = self.inner.session.lock().await;
        if !force {
            if let Some((t, list)) = &s.app_list {
                if t.elapsed() < APP_LIST_TTL {
                    return Ok(list.clone());
                }
            }
        }
        let apps = connector.list_apps().await?;
        let list = Arc::new(app_dirs(&apps));
        s.app_list = Some((Instant::now(), list.clone()));
        Ok(list)
    }

    async fn find_app(&self, name: &str) -> Result<AppDir> {
        for force in [false, true] {
            if let Some(a) = self.app_list(force).await?.iter().find(|a| a.name == name) {
                return Ok(a.clone());
            }
        }
        Err(VfsError::NotFound(format!("{APPS_ROOT}/{name}")))
    }

    async fn resolve(&self, p: &VPath) -> Result<Loc> {
        self.check_backend(p)?;
        let c = &p.components;
        let Some(first) = c.first() else { return Ok(Loc::Root) };
        match first.as_str() {
            MEDIA_ROOT => Ok(Loc::Afc { target: AfcTarget::Media, path: afc_join("/", &c[1..]), is_root: c.len() == 1 }),
            APPS_ROOT => {
                let Some(app_name) = c.get(1) else { return Ok(Loc::AppsDir) };
                let app = self.find_app(app_name).await?;
                Ok(Loc::Afc { target: AfcTarget::App(app.bundle_id), path: afc_join(house_arrest::DOCUMENTS, &c[2..]), is_root: c.len() == 2 })
            }
            _ => Err(VfsError::NotFound(p.to_string())),
        }
    }

    /// Resolve to an AFC location that may be modified (not a root).
    async fn resolve_mutable(&self, p: &VPath) -> Result<(AfcTarget, String)> {
        match self.resolve(p).await? {
            Loc::Afc { target, path, is_root: false } => Ok((target, path)),
            _ => Err(VfsError::PermissionDenied(p.to_string())),
        }
    }

    fn root_entries(&self) -> Vec<Entry> {
        vec![
            dir_entry(self.vpath(vec![MEDIA_ROOT.into()]), MEDIA_ROOT, NativeId::Path("/".into())),
            dir_entry(self.vpath(vec![APPS_ROOT.into()]), APPS_ROOT, NativeId::None),
        ]
    }

    async fn stat_afc(&self, target: &AfcTarget, afc_path: &str, vpath: VPath) -> Result<Entry> {
        let info = with_afc!(self.inner, target, |c| c.file_info(afc_path))?;
        Ok(entry_from_info(vpath, afc_path, &info))
    }
}

#[async_trait]
impl Backend for IosBackend {
    fn id(&self) -> BackendId {
        self.inner.id.clone()
    }

    fn info(&self) -> DeviceInfo {
        self.inner.info.clone()
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
            max_parallel: 1,
        }
    }

    async fn roots(&self) -> Result<Vec<Entry>> {
        self.inner.connector()?;
        Ok(self.root_entries())
    }

    async fn list(&self, path: &VPath) -> Result<Vec<Entry>> {
        match self.resolve(path).await? {
            Loc::Root => self.roots().await,
            Loc::AppsDir => {
                let apps = self.app_list(false).await?;
                Ok(apps
                    .iter()
                    .map(|a| dir_entry(path.join(a.name.clone()), &a.name, NativeId::Path(format!("{}:{}", a.bundle_id, house_arrest::DOCUMENTS))))
                    .collect())
            }
            Loc::Afc { target, path: afc_path, .. } => {
                let (names, infos) = with_afc!(self.inner, &target, |c| async {
                    let mut names = c.read_dir(&afc_path).await?;
                    names.sort();
                    let full: Vec<String> = names.iter().map(|n| afc_join(&afc_path, std::slice::from_ref(n))).collect();
                    let infos = c.file_info_many(&full).await?;
                    Ok::<_, VfsError>((names, infos))
                })?;
                let mut out = Vec::with_capacity(names.len());
                for (name, info) in names.into_iter().zip(infos) {
                    let child_afc = afc_join(&afc_path, std::slice::from_ref(&name));
                    match info {
                        Ok(i) => out.push(entry_from_info(path.join(name), &child_afc, &i)),
                        // Vanished between ReadDir and GetFileInfo, or unreadable: skip.
                        Err(e) => tracing::debug!(path = %child_afc, error = %e, "skipping entry"),
                    }
                }
                Ok(out)
            }
        }
    }

    async fn stat(&self, path: &VPath) -> Result<Entry> {
        match self.resolve(path).await? {
            Loc::Root => {
                self.inner.connector()?;
                Ok(dir_entry(path.clone(), &self.inner.info.name, NativeId::None))
            }
            Loc::AppsDir => Ok(dir_entry(path.clone(), APPS_ROOT, NativeId::None)),
            Loc::Afc { target, path: afc_path, is_root } => {
                if is_root {
                    // Synthetic, but verify the sandbox is reachable.
                    let mut e = self.stat_afc(&target, &afc_path, path.clone()).await?;
                    e.name = path.name().to_string();
                    e.hidden = false;
                    return Ok(e);
                }
                self.stat_afc(&target, &afc_path, path.clone()).await
            }
        }
    }

    async fn free_space(&self, path: &VPath) -> Result<Option<SpaceInfo>> {
        self.check_backend(path)?;
        let di = with_afc!(self.inner, &AfcTarget::Media, |c| c.device_info())?;
        Ok(match (di.total_bytes, di.free_bytes) {
            (Some(total), Some(free)) => Some(SpaceInfo { total, free }),
            _ => None,
        })
    }

    async fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn ReadSource>> {
        let (target, afc_path) = match self.resolve(path).await? {
            Loc::Afc { target, path, is_root: false } => (target, path),
            _ => return Err(VfsError::IsDir(path.to_string())),
        };
        let (handle, size, conn_id) = {
            let connector = self.inner.connector()?;
            let mut session = self.inner.session.lock().await;
            let mut attempt = 0;
            loop {
                let res = async {
                    let conn = session.conn(&*connector, &target).await?;
                    let conn_id = conn.id;
                    let c = &mut conn.afc;
                    let info = c.file_info(&afc_path).await?;
                    if info.kind == FileType::Dir {
                        return Err(VfsError::IsDir(path.to_string()));
                    }
                    let h = c.file_open(&afc_path, OpenMode::ReadOnly).await?;
                    if offset > 0 {
                        let off = i64::try_from(offset).map_err(|_| VfsError::Other("offset too large".into()))?;
                        if let Err(e) = c.file_seek(h, off, fellor_afc::proto::SEEK_SET).await {
                            let _ = c.file_close(h).await;
                            return Err(e);
                        }
                    }
                    Ok((h, info.size, conn_id))
                }
                .await;
                match res {
                    Err(e) if is_broken(&e) && attempt == 0 => {
                        session.invalidate(&target);
                        attempt += 1;
                    }
                    Err(e) => {
                        if is_broken(&e) {
                            session.invalidate(&target);
                        }
                        return Err(e);
                    }
                    Ok(v) => break v,
                }
            }
        };
        Ok(Box::new(AfcReadSource { inner: self.inner.clone(), target, conn_id, handle: Some(handle), size }))
    }

    async fn create_write(&self, path: &VPath, size: u64, mtime: Option<SystemTime>) -> Result<Box<dyn WriteSink>> {
        let (target, final_afc) = self.resolve_mutable(path).await?;
        let dir = afc_parent(&final_afc);
        let tmp = afc_join(&dir, &[temp_name()]);
        let connector = self.inner.connector()?;
        let mut session = self.inner.session.lock().await;
        let mut attempt = 0;
        let (handle, conn_id) = loop {
            let res = async {
                let conn = session.conn(&*connector, &target).await?;
                let id = conn.id;
                let c = &mut conn.afc;
                match c.file_info(&final_afc).await {
                    Ok(_) => return Err(VfsError::AlreadyExists(path.to_string())),
                    Err(VfsError::NotFound(_)) => {}
                    Err(e) => return Err(e),
                }
                match c.file_info(&dir).await {
                    Ok(i) if i.kind == FileType::Dir => {}
                    Ok(_) => return Err(VfsError::NotDir(dir.clone())),
                    Err(e) => return Err(e),
                }
                let h = c.file_open(&tmp, OpenMode::WriteOnly).await?;
                Ok((h, id))
            }
            .await;
            match res {
                Err(e) if is_broken(&e) && attempt == 0 => {
                    session.invalidate(&target);
                    attempt += 1;
                }
                Err(e) => {
                    if is_broken(&e) {
                        session.invalidate(&target);
                    }
                    return Err(e);
                }
                Ok(v) => break v,
            }
        };
        drop(session);
        Ok(Box::new(AfcWriteSink {
            inner: self.inner.clone(),
            target,
            conn_id,
            handle: Some(handle),
            tmp,
            final_afc,
            final_vpath: path.clone(),
            mtime,
            expected: size,
            written: 0,
            done: false,
        }))
    }

    async fn mkdir(&self, path: &VPath) -> Result<Entry> {
        let (target, afc_path) = self.resolve_mutable(path).await?;
        let info = with_afc!(self.inner, &target, |c| async {
            match c.file_info(&afc_path).await {
                Ok(_) => return Err(VfsError::AlreadyExists(path.to_string())),
                Err(VfsError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
            c.make_dir(&afc_path).await?;
            c.file_info(&afc_path).await
        })?;
        Ok(entry_from_info(path.clone(), &afc_path, &info))
    }

    async fn remove(&self, path: &VPath, recursive: bool) -> Result<()> {
        let (target, afc_path) = self.resolve_mutable(path).await?;
        with_afc!(self.inner, &target, |c| async {
            if recursive {
                c.remove_path_and_contents(&afc_path).await
            } else {
                c.remove_path(&afc_path).await
            }
        })
    }

    async fn rename(&self, from: &VPath, to: &VPath) -> Result<Entry> {
        let (t_from, p_from) = self.resolve_mutable(from).await?;
        let (t_to, p_to) = self.resolve_mutable(to).await?;
        if t_from != t_to {
            return Err(VfsError::Unsupported("перемещение между Media и папками приложений"));
        }
        if p_to == p_from || p_to.starts_with(&format!("{p_from}/")) {
            return Err(VfsError::Other("нельзя переместить папку внутрь самой себя".into()));
        }
        let info = with_afc!(self.inner, &t_from, |c| async {
            c.file_info(&p_from).await?;
            match c.file_info(&p_to).await {
                Ok(_) => return Err(VfsError::AlreadyExists(to.to_string())),
                Err(VfsError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
            c.rename_path(&p_from, &p_to).await?;
            c.file_info(&p_to).await
        })?;
        Ok(entry_from_info(to.clone(), &p_to, &info))
    }

    async fn set_mtime(&self, path: &VPath, mtime: SystemTime) -> Result<()> {
        let (target, afc_path) = self.resolve_mutable(path).await?;
        with_afc!(self.inner, &target, |c| c.set_file_time(&afc_path, time_to_ns(mtime)))
    }
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

struct AfcReadSource {
    inner: Arc<Inner>,
    target: AfcTarget,
    conn_id: u64,
    handle: Option<u64>,
    size: u64,
}

#[async_trait]
impl ReadSource for AfcReadSource {
    async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>> {
        let Some(h) = self.handle else { return Ok(None) };
        let mut s = self.inner.session.lock().await;
        let c = s.existing(&self.target, self.conn_id)?;
        match c.file_read(h, max.clamp(1, MAX_IO_CHUNK)).await {
            Ok(b) if b.is_empty() => {
                let _ = c.file_close(h).await;
                self.handle = None;
                Ok(None)
            }
            Ok(b) => Ok(Some(b)),
            Err(e) => {
                if is_broken(&e) {
                    s.invalidate(&self.target);
                    self.handle = None;
                }
                Err(e)
            }
        }
    }

    fn size(&self) -> Option<u64> {
        Some(self.size)
    }
}

impl Drop for AfcReadSource {
    fn drop(&mut self) {
        let Some(h) = self.handle.take() else { return };
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let inner = self.inner.clone();
        let target = self.target.clone();
        let conn_id = self.conn_id;
        rt.spawn(async move {
            let mut s = inner.session.lock().await;
            if let Ok(c) = s.existing(&target, conn_id) {
                if let Err(e) = c.file_close(h).await {
                    if is_broken(&e) {
                        s.invalidate(&target);
                    }
                }
            }
        });
    }
}

struct AfcWriteSink {
    inner: Arc<Inner>,
    target: AfcTarget,
    conn_id: u64,
    handle: Option<u64>,
    tmp: String,
    final_afc: String,
    final_vpath: VPath,
    mtime: Option<SystemTime>,
    expected: u64,
    written: u64,
    done: bool,
}

/// Close the handle (if its connection is alive) and delete the temp file
/// (reconnecting if necessary). Best effort.
async fn cleanup_temp(inner: Arc<Inner>, target: AfcTarget, conn_id: u64, handle: Option<u64>, tmp: String) {
    let mut s = inner.session.lock().await;
    if let Some(h) = handle {
        if let Ok(c) = s.existing(&target, conn_id) {
            if let Err(e) = c.file_close(h).await {
                if is_broken(&e) {
                    s.invalidate(&target);
                }
            }
        }
    }
    let Ok(connector) = inner.connector() else { return };
    match s.conn(&*connector, &target).await {
        Ok(conn) => match conn.afc.remove_path(&tmp).await {
            Ok(()) | Err(VfsError::NotFound(_)) => {}
            Err(e) => {
                tracing::warn!(path = %tmp, error = %e, "could not remove temp file");
                if is_broken(&e) {
                    s.invalidate(&target);
                }
            }
        },
        Err(e) => tracing::warn!(path = %tmp, error = %e, "could not reconnect to remove temp file"),
    }
}

#[async_trait]
impl WriteSink for AfcWriteSink {
    async fn write_chunk(&mut self, data: Bytes) -> Result<()> {
        let Some(h) = self.handle else { return Err(VfsError::Other("запись уже завершена".into())) };
        let mut s = self.inner.session.lock().await;
        let c = s.existing(&self.target, self.conn_id)?;
        match c.file_write(h, &data).await {
            Ok(()) => {
                self.written += data.len() as u64;
                Ok(())
            }
            Err(e) => {
                if is_broken(&e) {
                    s.invalidate(&self.target);
                    self.handle = None;
                }
                Err(e)
            }
        }
    }

    async fn finish(mut self: Box<Self>) -> Result<Entry> {
        if self.written != self.expected {
            tracing::warn!(expected = self.expected, written = self.written, path = %self.final_vpath, "size mismatch on finish");
        }
        let res = async {
            let mut s = self.inner.session.lock().await;
            let c = s.existing(&self.target, self.conn_id)?;
            if let Some(h) = self.handle.take() {
                c.file_close(h).await?;
            }
            match c.file_info(&self.final_afc).await {
                Ok(_) => return Err(VfsError::AlreadyExists(self.final_vpath.to_string())),
                Err(VfsError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
            c.rename_path(&self.tmp, &self.final_afc).await?;
            if let Some(t) = self.mtime {
                if let Err(e) = c.set_file_time(&self.final_afc, time_to_ns(t)).await {
                    tracing::warn!(error = %e, "could not set mtime");
                }
            }
            c.file_info(&self.final_afc).await
        }
        .await;
        match res {
            Ok(info) => {
                self.done = true;
                Ok(entry_from_info(self.final_vpath.clone(), &self.final_afc, &info))
            }
            Err(e) => {
                if is_broken(&e) {
                    self.inner.session.lock().await.invalidate(&self.target);
                }
                self.done = true;
                cleanup_temp(self.inner.clone(), self.target.clone(), self.conn_id, self.handle.take(), self.tmp.clone()).await;
                Err(e)
            }
        }
    }

    async fn abort(mut self: Box<Self>) -> Result<()> {
        self.done = true;
        cleanup_temp(self.inner.clone(), self.target.clone(), self.conn_id, self.handle.take(), self.tmp.clone()).await;
        Ok(())
    }
}

impl Drop for AfcWriteSink {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        rt.spawn(cleanup_temp(self.inner.clone(), self.target.clone(), self.conn_id, self.handle.take(), self.tmp.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn afc_paths() {
        assert_eq!(afc_join("/", &[]), "/");
        assert_eq!(afc_join("/", &["DCIM".into(), "a.jpg".into()]), "/DCIM/a.jpg");
        assert_eq!(afc_join("/Documents", &[]), "/Documents");
        assert_eq!(afc_join("/Documents", &["x".into()]), "/Documents/x");
        assert_eq!(afc_parent("/DCIM/a.jpg"), "/DCIM");
        assert_eq!(afc_parent("/a.jpg"), "/");
        assert!(validate_components(&["..".into()]).is_err());
        assert!(validate_components(&["a/b".into()]).is_err());
        assert!(validate_components(&["".into()]).is_err());
        assert!(validate_components(&["ok name.jpg".into()]).is_ok());
    }

    #[test]
    fn temp_names_are_unique_and_hidden() {
        let a = temp_name();
        let b = temp_name();
        assert_ne!(a, b);
        assert!(a.starts_with(TEMP_PREFIX));
    }

    #[test]
    fn app_dir_names() {
        let app = |id: &str, n: &str| AppInfo { bundle_id: id.into(), display_name: n.into(), file_sharing: true };
        let dirs = app_dirs(&[app("a.vlc", "VLC"), app("b.x", "Docs/PDF"), app("c.1", "Same"), app("c.2", "same"), app("d", "  "), app("e", "..")]);
        let names: Vec<_> = dirs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["VLC", "Docs_PDF", "Same (c.1)", "same (c.2)", "d", "e"]);
    }
}
