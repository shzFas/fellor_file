//! `MtpBackend`: fellor_vfs::Backend over a PTP/MTP session.

use crate::tree::{DirKey, Node, Tree};
use async_trait::async_trait;
use bytes::Bytes;
use fellor_ptp::codes::{association, event as ev, format, op, prop, resp, ROOT_PARENT};
use fellor_ptp::dataset::ObjectProps;
use fellor_ptp::wire::{format_datetime, parse_datetime};
use fellor_ptp::{DataIn, DataOut, Event, EventSource, ObjectInfo, PropListElement, PropValue, PtpError, Session, StorageInfo, Transport};
use fellor_vfs::{
    mime_from_name, Backend, BackendId, Capabilities, DeviceEvent, DeviceEventTx, DeviceInfo, DeviceKind, DeviceState, Entry, EntryKind, FsEvent,
    NativeId, ReadSource, Result, SpaceInfo, VPath, VfsError, WriteSink,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::SystemTime;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

/// Session id we open (any non-zero value).
pub const SESSION_ID: u32 = 1;
/// Max bytes requested per GetPartialObject(64) transaction.
const PARTIAL_REQUEST: u64 = 256 * 1024 * 1024;
const FOUR_GIB: u64 = 0xFFFF_FFFF;

#[derive(Debug, Clone)]
pub struct Storage {
    pub id: u32,
    /// Unique, '/'-free display name used as the first path component.
    pub name: String,
    pub info: StorageInfo,
}

pub(crate) struct Conn {
    session: Session,
    dev: fellor_ptp::DeviceInfo,
    storages: RwLock<Vec<Storage>>,
    tree: Mutex<Tree>,
    prop_list_ok: AtomicBool,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        for t in self.tasks.get_mut().unwrap().drain(..) {
            t.abort();
        }
    }
}

impl Conn {
    fn supports(&self, code: u16) -> bool {
        self.dev.supports_op(code)
    }

    fn storage_by_name(&self, name: &str) -> Option<Storage> {
        self.storages.read().unwrap().iter().find(|s| s.name == name).cloned()
    }

    fn storage_name(&self, id: u32) -> Option<String> {
        self.storages.read().unwrap().iter().find(|s| s.id == id).map(|s| s.name.clone())
    }
}

pub(crate) struct Inner {
    id: BackendId,
    info: RwLock<DeviceInfo>,
    conn: RwLock<Option<Arc<Conn>>>,
    events: broadcast::Sender<FsEvent>,
    /// Where to announce our own state changes (discovery channel).
    device_tx: Mutex<Option<DeviceEventTx>>,
}

/// Android phone or PTP camera. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct MtpBackend {
    inner: Arc<Inner>,
}

/// Where a path points.
#[derive(Debug, Clone)]
enum Target {
    /// The virtual device root listing the storages.
    Root,
    Storage(Storage),
    Object { storage: u32, handle: u32, node: Node },
}

fn dir_param(parent: u32) -> u32 {
    if parent == 0 {
        ROOT_PARENT
    } else {
        parent
    }
}

fn sanitize(name: &str) -> String {
    name.replace('/', "∕")
}

impl MtpBackend {
    /// A backend without a session yet (state Connecting / NeedsMtpMode / Busy ...).
    pub fn new(id: BackendId, name: String, kind: DeviceKind, state: DeviceState) -> MtpBackend {
        let (events, _) = broadcast::channel(256);
        MtpBackend {
            inner: Arc::new(Inner {
                id: id.clone(),
                info: RwLock::new(DeviceInfo { id, name, model: None, os_version: None, kind, state, ejectable: false }),
                conn: RwLock::new(None),
                events,
                device_tx: Mutex::new(None),
            }),
        }
    }

    /// Test / tooling helper: wrap a transport, open the session and load storages.
    pub async fn open_with_transport(id: BackendId, transport: Box<dyn Transport>, events: Option<Box<dyn EventSource>>) -> Result<MtpBackend> {
        let b = MtpBackend::new(id, "MTP".into(), DeviceKind::Android, DeviceState::Connecting);
        b.attach(transport, events).await?;
        Ok(b)
    }

    pub fn as_ref(&self) -> fellor_vfs::BackendRef {
        Arc::new(self.clone())
    }

    /// Report state changes caused by device events (StoreAdded → Ready)
    /// on this channel as `DeviceEvent::Updated`.
    pub fn set_device_tx(&self, tx: DeviceEventTx) {
        *self.inner.device_tx.lock().unwrap() = Some(tx);
    }

    pub fn state(&self) -> DeviceState {
        self.inner.info.read().unwrap().state.clone()
    }

    /// Set the state; returns true if it changed.
    pub fn set_state(&self, state: DeviceState) -> bool {
        let mut i = self.inner.info.write().unwrap();
        if i.state == state {
            return false;
        }
        i.state = state;
        true
    }

    pub fn set_name(&self, name: String) {
        self.inner.info.write().unwrap().name = name;
    }

    pub fn storages(&self) -> Vec<Storage> {
        self.inner.conn.read().unwrap().as_ref().map(|c| c.storages.read().unwrap().clone()).unwrap_or_default()
    }

    pub fn device_info(&self) -> Option<fellor_ptp::DeviceInfo> {
        self.inner.conn.read().unwrap().as_ref().map(|c| c.dev.clone())
    }

    pub fn session(&self) -> Option<Session> {
        self.inner.conn.read().unwrap().as_ref().map(|c| c.session.clone())
    }

    /// Drop the session (device unplugged or interface lost).
    pub fn detach(&self, state: DeviceState) {
        *self.inner.conn.write().unwrap() = None;
        self.set_state(state);
    }

    /// Open the PTP session over `transport`: GetDeviceInfo, OpenSession,
    /// storages. Returns the resulting state (Ready or Locked).
    pub async fn attach(&self, transport: Box<dyn Transport>, events: Option<Box<dyn EventSource>>) -> Result<DeviceState> {
        let session = Session::new(transport);
        let dev = {
            let mut g = session.lock().await;
            let dev = g.get_device_info().await?;
            g.open_session(SESSION_ID).await?;
            dev
        };
        info!(model = %dev.model, ext = %dev.vendor_extension_desc, ops = dev.operations_supported.len(), "MTP session open");
        {
            let mut i = self.inner.info.write().unwrap();
            let model = dev.model.trim();
            if !model.is_empty() {
                i.name = model.to_owned();
                i.model = Some(model.to_owned());
            }
            if !dev.device_version.trim().is_empty() {
                i.os_version = Some(dev.device_version.trim().to_owned());
            }
            i.kind = if dev.is_mtp() || dev.has_android_extension() { DeviceKind::Android } else { DeviceKind::Camera };
        }
        let conn = Arc::new(Conn {
            session,
            prop_list_ok: AtomicBool::new(dev.supports_op(op::GET_OBJECT_PROP_LIST)),
            dev,
            storages: RwLock::new(Vec::new()),
            tree: Mutex::new(Tree::default()),
            tasks: Mutex::new(Vec::new()),
        });
        *self.inner.conn.write().unwrap() = Some(conn.clone());
        if let Some(src) = events {
            self.start_event_pump(&conn, src);
        }
        Ok(self.refresh_storages().await)
    }

    /// Re-read storage ids/infos. Zero storages (or only ids with logical
    /// part 0) means the phone is locked. Returns the new state.
    pub async fn refresh_storages(&self) -> DeviceState {
        let Some(conn) = self.inner.conn.read().unwrap().clone() else {
            return self.state();
        };
        let res: Result<Vec<Storage>> = async {
            let mut g = conn.session.lock().await;
            let ids = g.get_storage_ids().await?;
            let mut out: Vec<Storage> = Vec::new();
            for id in ids.into_iter().filter(|id| id & 0xFFFF != 0) {
                let info = match g.get_storage_info(id).await {
                    Ok(i) => i,
                    Err(e) if e.response_code() == Some(resp::STORE_NOT_AVAILABLE) => continue,
                    Err(e) => {
                        warn!("GetStorageInfo(0x{id:08X}) failed: {e}");
                        StorageInfo::default()
                    }
                };
                let base = sanitize(info.display_name().unwrap_or(if out.is_empty() { "Internal storage" } else { "Storage" }));
                let mut name = base.clone();
                let mut n = 2;
                while out.iter().any(|s| s.name == name) {
                    name = format!("{base} ({n})");
                    n += 1;
                }
                out.push(Storage { id, name, info });
            }
            Ok(out)
        }
        .await;
        let state = match res {
            Ok(list) => {
                let changed = {
                    let mut cur = conn.storages.write().unwrap();
                    let old_ids: Vec<u32> = cur.iter().map(|s| s.id).collect();
                    let new_ids: Vec<u32> = list.iter().map(|s| s.id).collect();
                    if old_ids != new_ids {
                        let mut tree = conn.tree.lock().unwrap();
                        for gone in old_ids.iter().filter(|i| !new_ids.contains(i)) {
                            tree.drop_storage(*gone);
                        }
                    }
                    let changed = old_ids != new_ids;
                    *cur = list;
                    changed
                };
                if changed {
                    let _ = self.inner.events.send(FsEvent::Invalidate(VPath::root(self.inner.id.clone())));
                }
                if conn.storages.read().unwrap().is_empty() {
                    DeviceState::Locked
                } else {
                    DeviceState::Ready
                }
            }
            Err(VfsError::Disconnected) => DeviceState::Error("устройство отключено".into()),
            Err(e) => {
                warn!("GetStorageIDs failed: {e}");
                DeviceState::Locked
            }
        };
        self.set_state(state.clone());
        state
    }

    fn start_event_pump(&self, conn: &Arc<Conn>, mut src: Box<dyn EventSource>) {
        // Reader: drain the interrupt pipe continuously so the device never
        // stalls on a full event queue, even while a transfer holds the lock.
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
        let reader = tokio::spawn(async move {
            loop {
                match src.next_event().await {
                    Ok(Some(raw)) => match Event::decode(&raw) {
                        Ok(e) => {
                            if tx.send(e).is_err() {
                                break;
                            }
                        }
                        Err(e) => debug!("bad event container: {e}"),
                    },
                    Ok(None) | Err(fellor_ptp::TransportError::Disconnected) => break,
                    Err(e) => {
                        debug!("event pipe error: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
        });
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        let conn_weak = Arc::downgrade(conn);
        let handler = tokio::spawn(async move {
            while let Some(e) = rx.recv().await {
                let (Some(inner), Some(conn)) = (weak.upgrade(), conn_weak.upgrade()) else { break };
                MtpBackend { inner }.handle_event(&conn, e).await;
            }
        });
        conn.tasks.lock().unwrap().extend([reader, handler]);
    }

    async fn handle_event(&self, conn: &Arc<Conn>, e: Event) {
        debug!(code = format_args!("0x{:04X}", e.code), params = ?e.params, "MTP event");
        let h = e.param(0);
        let root = VPath::root(self.inner.id.clone());
        match e.code {
            ev::OBJECT_ADDED => {
                if conn.tree.lock().unwrap().node(h).is_some() {
                    return; // our own upload
                }
                let info = conn.session.lock().await.get_object_info(h).await;
                match info {
                    Ok(oi) => {
                        let node = node_from_info(&oi);
                        let dir = (node.storage, node.parent);
                        let listed = conn.tree.lock().unwrap().is_listed(dir);
                        let parent_known = node.parent == 0 || conn.tree.lock().unwrap().node(node.parent).is_some();
                        if listed && parent_known {
                            conn.tree.lock().unwrap().insert(h, node);
                            if let Some(p) = self.path_of(conn, h) {
                                let _ = self.inner.events.send(FsEvent::Added(p));
                            }
                        } else if let Some(p) = self.dir_path(conn, dir) {
                            let _ = self.inner.events.send(FsEvent::Invalidate(p));
                        }
                    }
                    Err(_) => {
                        let _ = self.inner.events.send(FsEvent::Invalidate(root));
                    }
                }
            }
            ev::OBJECT_REMOVED => {
                let path = self.path_of(conn, h);
                if conn.tree.lock().unwrap().remove(h).is_some() {
                    if let Some(p) = path {
                        let _ = self.inner.events.send(FsEvent::Removed(p));
                    }
                }
            }
            ev::OBJECT_INFO_CHANGED | ev::OBJECT_PROP_CHANGED => {
                let path = self.path_of(conn, h);
                let mut t = conn.tree.lock().unwrap();
                if let Some(n) = t.node(h).cloned() {
                    t.invalidate_dir((n.storage, n.parent));
                    drop(t);
                    if let Some(p) = path {
                        let _ = self.inner.events.send(FsEvent::Changed(p));
                    }
                }
            }
            ev::STORE_ADDED | ev::STORE_REMOVED | ev::STORAGE_INFO_CHANGED => {
                let before = self.state();
                let after = self.refresh_storages().await;
                let _ = self.inner.events.send(FsEvent::Invalidate(root));
                if before != after {
                    self.announce();
                }
            }
            ev::DEVICE_RESET | ev::DEVICE_INFO_CHANGED | ev::UNREPORTED_STATUS => {
                conn.tree.lock().unwrap().clear();
                let _ = self.inner.events.send(FsEvent::Invalidate(root));
            }
            _ => {}
        }
    }

    fn announce(&self) {
        if let Some(tx) = self.inner.device_tx.lock().unwrap().as_ref() {
            let _ = tx.send(DeviceEvent::Updated(self.as_ref()));
        }
    }

    // ------------------------------------------------------------ helpers

    fn conn(&self) -> Result<Arc<Conn>> {
        let state = self.state();
        match state {
            DeviceState::Ready => {}
            DeviceState::Locked => return Err(VfsError::Locked),
            DeviceState::Busy { by } => return Err(VfsError::Busy(by)),
            DeviceState::NeedsMtpMode => return Err(VfsError::Other("переключите USB-режим телефона на «Передача файлов»".into())),
            DeviceState::NeedsTrust => return Err(VfsError::NeedsTrust),
            DeviceState::Connecting => return Err(VfsError::Transport("подключение…".into())),
            DeviceState::Error(_) => return Err(VfsError::Disconnected),
        }
        self.inner.conn.read().unwrap().clone().ok_or(VfsError::Disconnected)
    }

    fn path_of(&self, conn: &Conn, handle: u32) -> Option<VPath> {
        let t = conn.tree.lock().unwrap();
        let n = t.node(handle)?;
        let mut comps = vec![conn.storage_name(n.storage)?];
        comps.extend(t.components_of(handle)?);
        Some(VPath::new(self.inner.id.clone(), comps))
    }

    fn dir_path(&self, conn: &Conn, dir: DirKey) -> Option<VPath> {
        if dir.1 == 0 {
            return Some(VPath::new(self.inner.id.clone(), vec![conn.storage_name(dir.0)?]));
        }
        self.path_of(conn, dir.1)
    }

    fn entry_for(&self, path: VPath, handle: u32, storage: u32, n: &Node) -> Entry {
        let mime = if n.is_dir { None } else { mime_from_name(&n.name).or_else(|| format::mime(n.format)) };
        Entry {
            name: n.name.clone(),
            path,
            kind: if n.is_dir { EntryKind::Dir } else { EntryKind::File },
            size: if n.is_dir { None } else { n.size },
            mtime: n.mtime,
            ctime: n.ctime,
            mime,
            hidden: n.hidden || n.name.starts_with('.'),
            native: NativeId::Mtp { storage, handle },
        }
    }

    fn storage_entry(&self, s: &Storage) -> Entry {
        Entry {
            name: s.name.clone(),
            path: VPath::new(self.inner.id.clone(), vec![s.name.clone()]),
            kind: EntryKind::Dir,
            size: None,
            mtime: None,
            ctime: None,
            mime: None,
            hidden: false,
            native: NativeId::Mtp { storage: s.id, handle: 0 },
        }
    }

    /// List a directory on the device and cache it. Returns (handle, node)
    /// pairs with final (disambiguated) names.
    async fn fetch_dir(&self, conn: &Conn, dir: DirKey) -> Result<Vec<(u32, Node)>> {
        let (storage, parent) = dir;
        let mut items: Option<Vec<(u32, Node)>> = None;
        if conn.prop_list_ok.load(Ordering::Relaxed) {
            let r = conn.session.lock().await.get_object_prop_list(parent, 0, prop::ALL, 0, 1).await;
            match r {
                Ok(elems) => items = Some(self.nodes_from_props(conn, dir, elems).await?),
                Err(e @ PtpError::Response { .. }) if e.response_code() != Some(resp::INVALID_OBJECT_HANDLE) => {
                    warn!("GetObjectPropList failed ({e}); falling back to GetObjectHandles");
                    conn.prop_list_ok.store(false, Ordering::Relaxed);
                }
                Err(e) => return Err(e.into()),
            }
        }
        let items = match items {
            Some(i) => i,
            None => {
                let mut g = conn.session.lock().await;
                let handles = g.get_object_handles(storage, 0, dir_param(parent)).await?;
                let mut out = Vec::with_capacity(handles.len());
                for h in handles {
                    match g.get_object_info(h).await {
                        Ok(oi) => {
                            let mut n = node_from_info(&oi);
                            if oi.compressed_size == 0xFFFF_FFFF && !n.is_dir && conn.supports(op::GET_OBJECT_PROP_VALUE) {
                                n.size = g.get_object_size64(h).await.ok();
                            }
                            n.storage = storage;
                            n.parent = parent;
                            out.push((h, n));
                        }
                        Err(e) if e.response_code() == Some(resp::INVALID_OBJECT_HANDLE) => {} // deleted meanwhile
                        Err(e) => return Err(e.into()),
                    }
                }
                out
            }
        };
        let names = conn.tree.lock().unwrap().set_listing(dir, items.clone());
        Ok(items.into_iter().zip(names).map(|((h, mut n), (_, name))| {
            n.name = name;
            (h, n)
        }).collect())
    }

    async fn nodes_from_props(&self, conn: &Conn, dir: DirKey, elems: Vec<PropListElement>) -> Result<Vec<(u32, Node)>> {
        let (storage, parent) = dir;
        let mut out = Vec::new();
        for o in ObjectProps::group(&elems) {
            if o.storage_id.is_some_and(|s| s != storage) {
                continue; // root listing spans all storages
            }
            let p = o.parent.map(|p| if p == ROOT_PARENT { 0 } else { p });
            if p.is_some_and(|p| p != parent) {
                continue;
            }
            let mut node = match (&o.filename, o.format) {
                (Some(name), Some(fmt)) => Node {
                    storage,
                    parent,
                    name: sanitize(name),
                    is_dir: fmt == format::ASSOCIATION,
                    size: o.size,
                    mtime: o.date_modified.as_deref().and_then(parse_datetime),
                    ctime: o.date_created.as_deref().and_then(parse_datetime),
                    format: fmt,
                    hidden: o.hidden,
                },
                _ => {
                    // Incomplete prop set: ask for the ObjectInfo.
                    let oi = conn.session.lock().await.get_object_info(o.handle).await?;
                    let mut n = node_from_info(&oi);
                    n.storage = storage;
                    n.parent = parent;
                    n
                }
            };
            if node.is_dir {
                node.size = None;
            }
            out.push((o.handle, node));
        }
        Ok(out)
    }

    async fn ensure_listed(&self, conn: &Conn, dir: DirKey) -> Result<()> {
        if !conn.tree.lock().unwrap().is_listed(dir) {
            self.fetch_dir(conn, dir).await?;
        }
        Ok(())
    }

    async fn resolve(&self, conn: &Conn, path: &VPath) -> Result<Target> {
        let Some((first, rest)) = path.components.split_first() else {
            return Ok(Target::Root);
        };
        let storage = conn.storage_by_name(first).ok_or_else(|| VfsError::NotFound(path.to_string()))?;
        if rest.is_empty() {
            return Ok(Target::Storage(storage));
        }
        let mut dir: DirKey = (storage.id, 0);
        let mut found: Option<(u32, Node)> = None;
        for (i, comp) in rest.iter().enumerate() {
            if let Some((_, n)) = &found {
                if !n.is_dir {
                    return Err(VfsError::NotDir(path.to_string()));
                }
            }
            let was_cached = conn.tree.lock().unwrap().is_listed(dir);
            self.ensure_listed(conn, dir).await?;
            let mut hit = conn.tree.lock().unwrap().lookup(dir, comp);
            if hit.is_none() && was_cached && i + 1 == rest.len() {
                // Might have been created on the device since we listed.
                self.fetch_dir(conn, dir).await?;
                hit = conn.tree.lock().unwrap().lookup(dir, comp);
            }
            let h = hit.ok_or_else(|| VfsError::NotFound(path.to_string()))?;
            let n = conn.tree.lock().unwrap().node(h).cloned().ok_or_else(|| VfsError::NotFound(path.to_string()))?;
            dir = (storage.id, h);
            found = Some((h, n));
        }
        let (handle, node) = found.unwrap();
        Ok(Target::Object { storage: storage.id, handle, node })
    }

    /// Resolve the directory that will contain `path`.
    async fn resolve_parent_dir(&self, conn: &Conn, path: &VPath) -> Result<(DirKey, Storage)> {
        let parent = path.parent().ok_or_else(|| VfsError::PermissionDenied(path.to_string()))?;
        match self.resolve(conn, &parent).await? {
            Target::Root => Err(VfsError::PermissionDenied(format!("{path}: создавать можно только внутри хранилища"))),
            Target::Storage(s) => Ok(((s.id, 0), s)),
            Target::Object { storage, handle, node } => {
                if !node.is_dir {
                    return Err(VfsError::NotDir(parent.to_string()));
                }
                let s = conn.storages.read().unwrap().iter().find(|s| s.id == storage).cloned().ok_or(VfsError::NotFound(parent.to_string()))?;
                Ok(((storage, handle), s))
            }
        }
    }

    fn check_name(path: &VPath) -> Result<String> {
        let name = path.name();
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(VfsError::Other(format!("недопустимое имя: {name:?}")));
        }
        Ok(name.to_owned())
    }

    async fn ensure_absent(&self, conn: &Conn, dir: DirKey, name: &str, path: &VPath) -> Result<()> {
        self.ensure_listed(conn, dir).await?;
        if conn.tree.lock().unwrap().lookup(dir, name).is_some() {
            return Err(VfsError::AlreadyExists(path.to_string()));
        }
        Ok(())
    }

    fn writable(s: &Storage, path: &VPath) -> Result<()> {
        if s.info.read_only() {
            return Err(VfsError::PermissionDenied(format!("{path}: хранилище только для чтения")));
        }
        Ok(())
    }
}

fn node_from_info(oi: &ObjectInfo) -> Node {
    let is_dir = oi.is_dir();
    Node {
        storage: oi.storage_id,
        parent: if oi.parent == ROOT_PARENT { 0 } else { oi.parent },
        name: sanitize(&oi.filename),
        is_dir,
        size: if is_dir || oi.compressed_size == 0xFFFF_FFFF { None } else { Some(oi.compressed_size as u64) },
        mtime: oi.mtime(),
        ctime: oi.ctime(),
        format: oi.format,
        hidden: false,
    }
}

#[async_trait]
impl Backend for MtpBackend {
    fn id(&self) -> BackendId {
        self.inner.id.clone()
    }

    fn info(&self) -> DeviceInfo {
        self.inner.info.read().unwrap().clone()
    }

    fn capabilities(&self) -> Capabilities {
        let Some(c) = self.inner.conn.read().unwrap().clone() else {
            return Capabilities { max_parallel: 1, ..Default::default() };
        };
        let any_rw = c.storages.read().unwrap().iter().any(|s| !s.info.read_only());
        let can_send = c.supports(op::SEND_OBJECT_INFO) && c.supports(op::SEND_OBJECT);
        Capabilities {
            read: c.supports(op::GET_OBJECT),
            write: any_rw && can_send,
            delete: any_rw && c.supports(op::DELETE_OBJECT),
            rename: any_rw && c.supports(op::SET_OBJECT_PROP_VALUE),
            move_within: any_rw && c.supports(op::MOVE_OBJECT),
            mkdir: any_rw && c.supports(op::SEND_OBJECT_INFO),
            // Android ignores DateModified writes; best effort only.
            set_mtime: false,
            partial_read: c.supports(op::ANDROID_GET_PARTIAL_OBJECT_64) || c.supports(op::GET_PARTIAL_OBJECT),
            free_space: true,
            max_parallel: 1,
        }
    }

    async fn roots(&self) -> Result<Vec<Entry>> {
        let conn = self.conn()?;
        let storages = conn.storages.read().unwrap().clone();
        Ok(storages.iter().map(|s| self.storage_entry(s)).collect())
    }

    async fn list(&self, path: &VPath) -> Result<Vec<Entry>> {
        let conn = self.conn()?;
        let dir = match self.resolve(&conn, path).await? {
            Target::Root => return self.roots().await,
            Target::Storage(s) => (s.id, 0),
            Target::Object { storage, handle, node } => {
                if !node.is_dir {
                    return Err(VfsError::NotDir(path.to_string()));
                }
                (storage, handle)
            }
        };
        // Listing is always fresh from the device (the cache serves path resolution).
        let items = self.fetch_dir(&conn, dir).await?;
        let mut out: Vec<Entry> = items.iter().map(|(h, n)| self.entry_for(path.join(n.name.clone()), *h, dir.0, n)).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn stat(&self, path: &VPath) -> Result<Entry> {
        let conn = self.conn()?;
        match self.resolve(&conn, path).await? {
            Target::Root => Ok(Entry {
                name: self.info().name,
                path: path.clone(),
                kind: EntryKind::Dir,
                size: None,
                mtime: None,
                ctime: None,
                mime: None,
                hidden: false,
                native: NativeId::None,
            }),
            Target::Storage(s) => Ok(self.storage_entry(&s)),
            Target::Object { storage, handle, node } => Ok(self.entry_for(path.clone(), handle, storage, &node)),
        }
    }

    async fn free_space(&self, path: &VPath) -> Result<Option<SpaceInfo>> {
        let conn = self.conn()?;
        let ids: Vec<u32> = match path.components.first() {
            Some(name) => vec![conn.storage_by_name(name).ok_or_else(|| VfsError::NotFound(path.to_string()))?.id],
            None => conn.storages.read().unwrap().iter().map(|s| s.id).collect(),
        };
        let mut total = SpaceInfo { total: 0, free: 0 };
        let mut g = conn.session.lock().await;
        for id in ids {
            let si = g.get_storage_info(id).await?;
            total.total += si.max_capacity;
            total.free += si.free_space_bytes;
        }
        Ok(Some(total))
    }

    async fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn ReadSource>> {
        let conn = self.conn()?;
        let Target::Object { handle, node, .. } = self.resolve(&conn, path).await? else {
            return Err(VfsError::IsDir(path.to_string()));
        };
        if node.is_dir {
            return Err(VfsError::IsDir(path.to_string()));
        }
        let mut size = node.size;
        if size.is_none() && conn.supports(op::GET_OBJECT_PROP_VALUE) {
            size = conn.session.lock().await.get_object_size64(handle).await.ok();
        }
        let mode = if offset == 0 {
            ReadMode::Whole
        } else if conn.supports(op::ANDROID_GET_PARTIAL_OBJECT_64) {
            ReadMode::Partial64
        } else if conn.supports(op::GET_PARTIAL_OBJECT) && offset < FOUR_GIB {
            ReadMode::Partial32
        } else {
            return Err(VfsError::Unsupported("partial_read"));
        };
        if size.is_some_and(|s| offset > s) {
            return Err(VfsError::Other(format!("offset {offset} beyond end of {path}")));
        }
        let mut r = MtpReader { session: conn.session.clone(), handle, size, pos: offset, mode, cur: None, cur_req: 0, cur_got: 0, eof: false };
        if size == Some(offset) {
            r.eof = true;
        } else if mode == ReadMode::Whole {
            // Start the transfer now so errors (e.g. deleted object) surface here.
            r.cur = Some(DataIn::get_object(conn.session.lock().await, handle, size).await?);
        }
        Ok(Box::new(r))
    }

    async fn create_write(&self, path: &VPath, size: u64, mtime: Option<SystemTime>) -> Result<Box<dyn WriteSink>> {
        let conn = self.conn()?;
        let name = Self::check_name(path)?;
        let (dir, storage) = self.resolve_parent_dir(&conn, path).await?;
        Self::writable(&storage, path)?;
        self.ensure_absent(&conn, dir, &name, path).await?;
        let fmt = format::from_name(&name);
        let mut g = conn.session.lock().await;
        let (_, _, handle) = if size >= FOUR_GIB && conn.supports(op::SEND_OBJECT_PROP_LIST) {
            let mut props = vec![PropListElement { handle: 0, prop: prop::OBJECT_FILE_NAME, value: PropValue::Str(name.clone()) }];
            if let Some(t) = mtime {
                props.push(PropListElement { handle: 0, prop: prop::DATE_MODIFIED, value: PropValue::Str(format_datetime(t)) });
            }
            g.send_object_prop_list(dir.0, dir_param(dir.1), fmt, size, &props).await?
        } else {
            let info = ObjectInfo {
                storage_id: dir.0,
                format: fmt,
                compressed_size: size.min(FOUR_GIB) as u32,
                parent: dir.1,
                filename: name.clone(),
                modification_date: mtime.map(format_datetime).unwrap_or_default(),
                ..Default::default()
            };
            g.send_object_info(dir.0, dir_param(dir.1), &info).await?
        };
        let out = match DataOut::send_object(g, size).await {
            Ok(o) => o,
            Err(e) => {
                let _ = conn.session.lock().await.delete_object(handle).await;
                return Err(e.into());
            }
        };
        Ok(Box::new(MtpWriter {
            backend: self.clone(),
            conn,
            out: Some(out),
            handle,
            dir,
            name,
            size,
            mtime,
            path: path.clone(),
            fmt,
        }))
    }

    async fn mkdir(&self, path: &VPath) -> Result<Entry> {
        let conn = self.conn()?;
        let name = Self::check_name(path)?;
        let (dir, storage) = self.resolve_parent_dir(&conn, path).await?;
        Self::writable(&storage, path)?;
        self.ensure_absent(&conn, dir, &name, path).await?;
        let info = ObjectInfo {
            storage_id: dir.0,
            format: format::ASSOCIATION,
            association_type: association::GENERIC_FOLDER,
            parent: dir.1,
            filename: name.clone(),
            ..Default::default()
        };
        let (_, _, handle) = conn.session.lock().await.send_object_info(dir.0, dir_param(dir.1), &info).await?;
        let node = Node { storage: dir.0, parent: dir.1, name, is_dir: true, size: None, mtime: Some(SystemTime::now()), ctime: None, format: format::ASSOCIATION, hidden: false };
        let name = {
            let mut t = conn.tree.lock().unwrap();
            let n = t.insert(handle, node.clone());
            t.set_listing((dir.0, handle), vec![]);
            n
        };
        let path = path.parent().unwrap().join(name.clone());
        let _ = self.inner.events.send(FsEvent::Added(path.clone()));
        Ok(self.entry_for(path, handle, dir.0, &Node { name, ..node }))
    }

    async fn remove(&self, path: &VPath, recursive: bool) -> Result<()> {
        let conn = self.conn()?;
        let Target::Object { storage, handle, node } = self.resolve(&conn, path).await? else {
            return Err(VfsError::PermissionDenied(format!("{path}: хранилище нельзя удалить")));
        };
        if let Some(s) = conn.storages.read().unwrap().iter().find(|s| s.id == storage) {
            if s.info.access_capability == 1 {
                return Err(VfsError::PermissionDenied(path.to_string()));
            }
        }
        if node.is_dir && !recursive {
            let kids = self.fetch_dir(&conn, (storage, handle)).await?;
            if !kids.is_empty() {
                return Err(VfsError::NotEmpty(path.to_string()));
            }
        }
        // DeleteObject on an association deletes its contents too (PTP 10.5.11).
        match conn.session.lock().await.delete_object(handle).await {
            Ok(()) => {}
            Err(e) if e.response_code() == Some(resp::PARTIAL_DELETION) => {
                conn.tree.lock().unwrap().invalidate_dir((storage, handle));
                return Err(VfsError::Other(format!("{path}: удалено частично")));
            }
            Err(e) => return Err(e.into()),
        }
        conn.tree.lock().unwrap().remove(handle);
        let _ = self.inner.events.send(FsEvent::Removed(path.clone()));
        Ok(())
    }

    async fn rename(&self, from: &VPath, to: &VPath) -> Result<Entry> {
        let conn = self.conn()?;
        let Target::Object { storage, handle, node } = self.resolve(&conn, from).await? else {
            return Err(VfsError::PermissionDenied(format!("{from}: хранилище нельзя переименовать")));
        };
        let new_name = Self::check_name(to)?;
        let (dest_dir, dest_storage) = self.resolve_parent_dir(&conn, to).await?;
        Self::writable(&dest_storage, to)?;
        let same_dir = dest_dir == (storage, node.parent);
        if same_dir && new_name == node.name {
            return Ok(self.entry_for(from.clone(), handle, storage, &node));
        }
        self.ensure_absent(&conn, dest_dir, &new_name, to).await?;
        if !same_dir {
            if !conn.supports(op::MOVE_OBJECT) {
                return Err(VfsError::Unsupported("move_within"));
            }
            // MoveObject: parent 0 = storage root.
            conn.session.lock().await.move_object(handle, dest_dir.0, dest_dir.1).await?;
            conn.tree.lock().unwrap().reparent(handle, dest_dir.0, dest_dir.1);
        }
        if new_name != node.name {
            conn.session.lock().await.set_object_filename(handle, &new_name).await?;
            conn.tree.lock().unwrap().rename(handle, &new_name);
        }
        let n = conn.tree.lock().unwrap().node(handle).cloned().unwrap_or(Node { name: new_name.clone(), storage: dest_dir.0, parent: dest_dir.1, ..node });
        let _ = self.inner.events.send(FsEvent::Removed(from.clone()));
        let _ = self.inner.events.send(FsEvent::Added(to.clone()));
        Ok(self.entry_for(to.clone(), handle, dest_dir.0, &n))
    }

    async fn set_mtime(&self, path: &VPath, mtime: SystemTime) -> Result<()> {
        let conn = self.conn()?;
        let Target::Object { handle, .. } = self.resolve(&conn, path).await? else {
            return Err(VfsError::Unsupported("set_mtime"));
        };
        let v = PropValue::Str(format_datetime(mtime));
        match conn.session.lock().await.set_object_prop_value(handle, prop::DATE_MODIFIED, &v).await {
            Ok(()) => {
                let t = conn.tree.lock().unwrap().node(handle).cloned();
                if let Some(mut n) = t {
                    n.mtime = Some(mtime);
                    conn.tree.lock().unwrap().insert(handle, n);
                }
                Ok(())
            }
            Err(e) if e.response_code().is_some() => Err(VfsError::Unsupported("set_mtime")),
            Err(e) => Err(e.into()),
        }
    }

    fn watch(&self) -> Option<broadcast::Receiver<FsEvent>> {
        Some(self.inner.events.subscribe())
    }

    async fn thumbnail(&self, path: &VPath) -> Result<Option<Bytes>> {
        let conn = self.conn()?;
        if !conn.supports(op::GET_THUMB) {
            return Ok(None);
        }
        let Target::Object { handle, node, .. } = self.resolve(&conn, path).await? else {
            return Ok(None);
        };
        if node.is_dir {
            return Ok(None);
        }
        match conn.session.lock().await.get_thumb(handle).await {
            Ok(b) if !b.is_empty() => Ok(Some(Bytes::from(b))),
            Ok(_) => Ok(None),
            Err(e) if e.response_code().is_some() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn eject(&self) -> Result<()> {
        let conn = self.inner.conn.read().unwrap().clone();
        if let Some(c) = conn {
            let _ = c.session.lock().await.close_session().await;
        }
        self.detach(DeviceState::Error("отключено".into()));
        Ok(())
    }
}

// ---------------------------------------------------------------- reading

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadMode {
    Whole,
    Partial64,
    Partial32,
}

struct MtpReader {
    session: Session,
    handle: u32,
    size: Option<u64>,
    pos: u64,
    mode: ReadMode,
    cur: Option<DataIn>,
    cur_req: u64,
    cur_got: u64,
    eof: bool,
}

#[async_trait]
impl ReadSource for MtpReader {
    async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>> {
        loop {
            if let Some(cur) = self.cur.as_mut() {
                match cur.read_chunk(max).await? {
                    Some(b) => {
                        self.pos += b.len() as u64;
                        self.cur_got += b.len() as u64;
                        return Ok(Some(b));
                    }
                    None => {
                        self.cur = None;
                        let reached_size = self.size.is_some_and(|s| self.pos >= s);
                        if self.mode == ReadMode::Whole || reached_size || self.cur_got < self.cur_req {
                            self.eof = true;
                        }
                    }
                }
            }
            if self.eof {
                return Ok(None);
            }
            // Next partial request.
            let remaining = self.size.map(|s| s - self.pos).unwrap_or(PARTIAL_REQUEST);
            let mut len = remaining.min(PARTIAL_REQUEST);
            let g = self.session.lock().await;
            let stream = match self.mode {
                ReadMode::Partial64 | ReadMode::Whole => DataIn::get_partial_object_64(g, self.handle, self.pos, len as u32).await?,
                ReadMode::Partial32 => {
                    if self.pos >= FOUR_GIB {
                        return Err(VfsError::Unsupported("partial_read beyond 4 GiB"));
                    }
                    len = len.min(FOUR_GIB - self.pos);
                    DataIn::get_partial_object(g, self.handle, self.pos as u32, len as u32).await?
                }
            };
            self.cur_req = len;
            self.cur_got = 0;
            self.cur = Some(stream);
        }
    }

    fn size(&self) -> Option<u64> {
        self.size
    }
}

// ---------------------------------------------------------------- writing

struct MtpWriter {
    backend: MtpBackend,
    conn: Arc<Conn>,
    out: Option<DataOut>,
    handle: u32,
    dir: DirKey,
    name: String,
    size: u64,
    mtime: Option<SystemTime>,
    path: VPath,
    fmt: u16,
}

impl MtpWriter {
    /// Remove the partially-sent object (MTP has no temp+rename).
    async fn delete_partial(conn: &Conn, handle: u32) {
        if let Err(e) = conn.session.lock().await.delete_object(handle).await {
            warn!("could not delete partial object 0x{handle:08X}: {e}");
        }
    }
}

#[async_trait]
impl WriteSink for MtpWriter {
    async fn write_chunk(&mut self, data: Bytes) -> Result<()> {
        let out = self.out.as_mut().ok_or(VfsError::Other("запись уже завершена".into()))?;
        out.write(&data).await.map_err(Into::into)
    }

    async fn finish(mut self: Box<Self>) -> Result<Entry> {
        let out = self.out.take().ok_or(VfsError::Other("запись уже завершена".into()))?;
        match out.finish().await {
            Ok((_, g)) => drop(g),
            Err(e) => {
                Self::delete_partial(&self.conn, self.handle).await;
                return Err(e.into());
            }
        }
        let node = Node {
            storage: self.dir.0,
            parent: self.dir.1,
            name: self.name.clone(),
            is_dir: false,
            size: Some(self.size),
            mtime: self.mtime.or(Some(SystemTime::now())),
            ctime: None,
            format: self.fmt,
            hidden: false,
        };
        let name = self.conn.tree.lock().unwrap().insert(self.handle, node.clone());
        let path = if name == self.name { self.path.clone() } else { self.path.parent().unwrap().join(name.clone()) };
        let _ = self.backend.inner.events.send(FsEvent::Added(path.clone()));
        Ok(self.backend.entry_for(path, self.handle, self.dir.0, &Node { name, ..node }))
    }

    async fn abort(mut self: Box<Self>) -> Result<()> {
        if let Some(out) = self.out.take() {
            let mut g = match out.abort().await {
                Some(g) => g,
                None => self.conn.session.lock().await,
            };
            if let Err(e) = g.delete_object(self.handle).await {
                warn!("could not delete partial object 0x{:08X}: {e}", self.handle);
            }
        }
        Ok(())
    }
}

impl Drop for MtpWriter {
    fn drop(&mut self) {
        if let Some(out) = self.out.take() {
            // Dropping the stream poisons the session; the next lock cancels
            // the transaction. Then delete what the device already created.
            drop(out);
            let conn = self.conn.clone();
            let handle = self.handle;
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move { MtpWriter::delete_partial(&conn, handle).await });
            }
        }
    }
}
