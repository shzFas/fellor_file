//! `--demo` mode: in-memory fake backends (a fake Mac home tree, a fake
//! iPhone with DCIM photos, a couple of devices in hint states, a fake SD
//! card) and a tiny transfer engine that speaks the same `TransferEvent`
//! protocol as `fellor_transfer::TransferEngine`.
//!
//! Nothing here is used by the real code path.

use async_trait::async_trait;
use bytes::Bytes;
use fellor_transfer::{
    ConflictId, ConflictPolicy, ConflictResolution, TaskId, TaskOutcome, TransferEvent, TransferOp,
    TransferRequest,
};
use fellor_vfs::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{mpsc, oneshot, watch};

// ───────────────────────────── in-memory backend ─────────────────────────────

#[derive(Clone)]
struct Node {
    kind: EntryKind,
    size: u64,
    mtime: SystemTime,
}

type Tree = Arc<Mutex<BTreeMap<Vec<String>, Node>>>;

pub struct MemBackend {
    id: BackendId,
    info: Mutex<DeviceInfo>,
    nodes: Tree,
    total: u64,
    /// State the device switches to when the user presses "Проверить снова".
    fix_on_recheck: Mutex<Option<DeviceState>>,
}

impl MemBackend {
    fn new(info: DeviceInfo, total: u64) -> Self {
        let mut map = BTreeMap::new();
        map.insert(Vec::new(), Node { kind: EntryKind::Dir, size: 0, mtime: SystemTime::now() });
        Self {
            id: info.id.clone(),
            info: Mutex::new(info),
            nodes: Arc::new(Mutex::new(map)),
            total,
            fix_on_recheck: Mutex::new(None),
        }
    }

    fn add_dir(&self, path: &str) {
        let mut nodes = self.nodes.lock().unwrap();
        let comps: Vec<String> = path.split('/').filter(|c| !c.is_empty()).map(str::to_owned).collect();
        for i in 1..=comps.len() {
            nodes
                .entry(comps[..i].to_vec())
                .or_insert(Node { kind: EntryKind::Dir, size: 0, mtime: SystemTime::now() });
        }
    }

    fn add_file(&self, path: &str, size: u64, mtime: SystemTime) {
        let comps: Vec<String> = path.split('/').filter(|c| !c.is_empty()).map(str::to_owned).collect();
        if let Some((_, parent)) = comps.split_last() {
            self.add_dir(&parent.join("/"));
        }
        self.nodes.lock().unwrap().insert(comps, Node { kind: EntryKind::File, size, mtime });
    }

    fn ready(&self) -> Result<()> {
        match self.info.lock().unwrap().state.clone() {
            DeviceState::Ready => Ok(()),
            DeviceState::NeedsTrust => Err(VfsError::NeedsTrust),
            DeviceState::Locked => Err(VfsError::Locked),
            DeviceState::Busy { by } => Err(VfsError::Busy(by)),
            DeviceState::NeedsMtpMode => Err(VfsError::Other("включите режим «Передача файлов»".into())),
            DeviceState::Connecting => Err(VfsError::Other("подключение…".into())),
            DeviceState::Error(e) => Err(VfsError::Other(e)),
        }
    }

    fn entry(&self, comps: &[String], n: &Node) -> Entry {
        let name = comps.last().cloned().unwrap_or_default();
        Entry {
            mime: if n.kind == EntryKind::File { mime_from_name(&name) } else { None },
            hidden: name.starts_with('.'),
            name,
            path: VPath::new(self.id.clone(), comps.to_vec()),
            kind: n.kind,
            size: (n.kind == EntryKind::File).then_some(n.size),
            mtime: Some(n.mtime),
            ctime: Some(n.mtime),
            native: NativeId::None,
        }
    }

    fn not_found(p: &VPath) -> VfsError {
        VfsError::NotFound(p.as_unix())
    }
}

struct ZeroReader {
    remaining: u64,
    size: u64,
}

#[async_trait]
impl ReadSource for ZeroReader {
    async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let n = (max as u64).min(self.remaining);
        self.remaining -= n;
        Ok(Some(Bytes::from(vec![0u8; n as usize])))
    }
    fn size(&self) -> Option<u64> {
        Some(self.size)
    }
}

struct MemSink {
    backend: BackendId,
    nodes: Tree,
    path: Vec<String>,
    written: u64,
    mtime: Option<SystemTime>,
}

#[async_trait]
impl WriteSink for MemSink {
    async fn write_chunk(&mut self, data: Bytes) -> Result<()> {
        self.written += data.len() as u64;
        Ok(())
    }
    async fn finish(self: Box<Self>) -> Result<Entry> {
        let node = Node { kind: EntryKind::File, size: self.written, mtime: self.mtime.unwrap_or_else(SystemTime::now) };
        self.nodes.lock().unwrap().insert(self.path.clone(), node.clone());
        let name = self.path.last().cloned().unwrap_or_default();
        Ok(Entry {
            mime: mime_from_name(&name),
            hidden: name.starts_with('.'),
            name,
            path: VPath::new(self.backend.clone(), self.path.clone()),
            kind: EntryKind::File,
            size: Some(node.size),
            mtime: Some(node.mtime),
            ctime: Some(node.mtime),
            native: NativeId::None,
        })
    }
    async fn abort(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Backend for MemBackend {
    fn id(&self) -> BackendId {
        self.id.clone()
    }
    fn info(&self) -> DeviceInfo {
        self.info.lock().unwrap().clone()
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
            partial_read: false,
            free_space: true,
            max_parallel: 1,
        }
    }

    async fn roots(&self) -> Result<Vec<Entry>> {
        // "Проверить снова" lands here: maybe the user fixed the device.
        let fix = self.fix_on_recheck.lock().unwrap().take();
        if let Some(st) = fix {
            tokio::time::sleep(Duration::from_millis(400)).await;
            self.info.lock().unwrap().state = st;
        }
        self.list(&VPath::root(self.id.clone())).await
    }

    async fn list(&self, path: &VPath) -> Result<Vec<Entry>> {
        self.ready()?;
        tokio::time::sleep(Duration::from_millis(60)).await;
        let nodes = self.nodes.lock().unwrap();
        match nodes.get(&path.components) {
            None => return Err(Self::not_found(path)),
            Some(n) if n.kind != EntryKind::Dir => return Err(VfsError::NotDir(path.as_unix())),
            _ => {}
        }
        let depth = path.components.len() + 1;
        Ok(nodes
            .iter()
            .filter(|(k, _)| k.len() == depth && k.starts_with(&path.components))
            .map(|(k, n)| self.entry(k, n))
            .collect())
    }

    async fn stat(&self, path: &VPath) -> Result<Entry> {
        self.ready()?;
        let nodes = self.nodes.lock().unwrap();
        nodes.get(&path.components).map(|n| self.entry(&path.components, n)).ok_or_else(|| Self::not_found(path))
    }

    async fn free_space(&self, _path: &VPath) -> Result<Option<SpaceInfo>> {
        self.ready()?;
        let used: u64 = self.nodes.lock().unwrap().values().map(|n| n.size).sum();
        Ok(Some(SpaceInfo { total: self.total, free: self.total.saturating_sub(used) }))
    }

    async fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn ReadSource>> {
        let e = self.stat(path).await?;
        if e.is_dir() {
            return Err(VfsError::IsDir(path.as_unix()));
        }
        let size = e.size.unwrap_or(0);
        Ok(Box::new(ZeroReader { remaining: size.saturating_sub(offset), size }))
    }

    async fn create_write(&self, path: &VPath, _size: u64, mtime: Option<SystemTime>) -> Result<Box<dyn WriteSink>> {
        self.ready()?;
        let nodes = self.nodes.lock().unwrap();
        if nodes.contains_key(&path.components) {
            return Err(VfsError::AlreadyExists(path.as_unix()));
        }
        let parent = path.parent().ok_or_else(|| VfsError::PermissionDenied("/".into()))?;
        if !nodes.get(&parent.components).is_some_and(|n| n.kind == EntryKind::Dir) {
            return Err(Self::not_found(&parent));
        }
        Ok(Box::new(MemSink {
            backend: self.id.clone(),
            nodes: self.nodes.clone(),
            path: path.components.clone(),
            written: 0,
            mtime,
        }))
    }

    async fn mkdir(&self, path: &VPath) -> Result<Entry> {
        self.ready()?;
        let mut nodes = self.nodes.lock().unwrap();
        if nodes.contains_key(&path.components) {
            return Err(VfsError::AlreadyExists(path.as_unix()));
        }
        let parent = path.parent().ok_or_else(|| VfsError::AlreadyExists("/".into()))?;
        if !nodes.contains_key(&parent.components) {
            return Err(Self::not_found(&parent));
        }
        let n = Node { kind: EntryKind::Dir, size: 0, mtime: SystemTime::now() };
        nodes.insert(path.components.clone(), n.clone());
        Ok(self.entry(&path.components, &n))
    }

    async fn remove(&self, path: &VPath, recursive: bool) -> Result<()> {
        self.ready()?;
        if path.is_root() {
            return Err(VfsError::PermissionDenied("/".into()));
        }
        let mut nodes = self.nodes.lock().unwrap();
        if !nodes.contains_key(&path.components) {
            return Err(Self::not_found(path));
        }
        let children: Vec<Vec<String>> = nodes
            .keys()
            .filter(|k| k.len() > path.components.len() && k.starts_with(&path.components))
            .cloned()
            .collect();
        if !children.is_empty() && !recursive {
            return Err(VfsError::NotEmpty(path.as_unix()));
        }
        for k in children {
            nodes.remove(&k);
        }
        nodes.remove(&path.components);
        Ok(())
    }

    async fn rename(&self, from: &VPath, to: &VPath) -> Result<Entry> {
        self.ready()?;
        let mut nodes = self.nodes.lock().unwrap();
        if nodes.contains_key(&to.components) {
            return Err(VfsError::AlreadyExists(to.as_unix()));
        }
        let keys: Vec<Vec<String>> = nodes.keys().filter(|k| k.starts_with(&from.components)).cloned().collect();
        if keys.is_empty() {
            return Err(Self::not_found(from));
        }
        for k in keys {
            let n = nodes.remove(&k).unwrap();
            let mut nk = to.components.clone();
            nk.extend_from_slice(&k[from.components.len()..]);
            nodes.insert(nk, n);
        }
        let n = nodes[&to.components].clone();
        Ok(self.entry(&to.components, &n))
    }

    async fn set_mtime(&self, path: &VPath, mtime: SystemTime) -> Result<()> {
        let mut nodes = self.nodes.lock().unwrap();
        let n = nodes.get_mut(&path.components).ok_or_else(|| Self::not_found(path))?;
        n.mtime = mtime;
        Ok(())
    }

    async fn thumbnail(&self, path: &VPath) -> Result<Option<Bytes>> {
        let e = self.stat(path).await?;
        if !e.mime.is_some_and(|m| m.starts_with("image/")) {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(Some(fake_photo(&e.name)))
    }

    async fn eject(&self) -> Result<()> {
        if self.info.lock().unwrap().ejectable {
            Ok(())
        } else {
            Err(VfsError::Unsupported("eject"))
        }
    }
}

/// A colourful gradient PNG unique to the file name — stands in for a photo.
fn fake_photo(name: &str) -> Bytes {
    let h = name.bytes().fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    let (w, ht) = (480u32, 360u32);
    let base = [(h & 0xff) as f32, ((h >> 8) & 0xff) as f32, ((h >> 16) & 0xff) as f32];
    let img = image::RgbImage::from_fn(w, ht, |x, y| {
        let fx = x as f32 / w as f32;
        let fy = y as f32 / ht as f32;
        let sun = {
            let (dx, dy) = (fx - 0.7, fy - 0.3);
            (1.0 - (dx * dx + dy * dy).sqrt() * 4.0).max(0.0)
        };
        let horizon = if fy > 0.65 { 0.45 } else { 1.0 };
        let px = |c: f32, k: f32| ((c * (1.0 - fy * 0.6) * horizon + 255.0 * sun * k).min(255.0)) as u8;
        image::Rgb([px(base[0], 1.0), px(base[1], 0.9), px((base[2] + 255.0 * fx) / 2.0, 0.5)])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    let _ = img.write_to(&mut out, image::ImageFormat::Png);
    Bytes::from(out.into_inner())
}

fn days_ago(d: u64, minutes: u64) -> SystemTime {
    SystemTime::now() - Duration::from_secs(d * 86_400 + minutes * 60)
}

fn info(id: &str, name: &str, kind: DeviceKind, state: DeviceState, model: Option<&str>, ejectable: bool) -> DeviceInfo {
    DeviceInfo {
        id: id.into(),
        name: name.into(),
        model: model.map(Into::into),
        os_version: None,
        kind,
        state,
        ejectable,
    }
}

/// Fake `local` backend whose tree mirrors the real home folder layout so the
/// sidebar favourites resolve.
fn fake_local() -> MemBackend {
    let b = MemBackend::new(
        info("local", "Мак", DeviceKind::Mac, DeviceState::Ready, None, false),
        494_000_000_000,
    );
    let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_else(|| "/Users/demo".into());
    for d in ["Applications", "Library", "System", "Volumes"] {
        b.add_dir(d);
    }
    for d in ["Desktop", "Documents", "Downloads", "Pictures", "Movies", "Music", ".config"] {
        b.add_dir(&format!("{home}/{d}"));
    }
    b.add_file(&format!("{home}/.zshrc"), 1_200, days_ago(40, 0));
    b.add_file(&format!("{home}/Desktop/Скриншот 2026-11-05.png"), 2_400_000, days_ago(2, 30));
    b.add_file(&format!("{home}/Desktop/заметки.txt"), 4_800, days_ago(1, 3));
    b.add_file(&format!("{home}/Documents/Договор аренды.pdf"), 812_000, days_ago(90, 0));
    b.add_file(&format!("{home}/Documents/Бюджет 2026.xlsx"), 64_000, days_ago(12, 0));
    b.add_dir(&format!("{home}/Documents/Проекты"));
    b.add_file(&format!("{home}/Downloads/fellor-0.1.0.dmg"), 18_300_000, days_ago(0, 50));
    b.add_file(&format!("{home}/Downloads/report.zip"), 145_000_000, days_ago(3, 0));
    b.add_file(&format!("{home}/Movies/Отпуск 2025.mp4"), 1_850_000_000, days_ago(120, 0));
    for (i, d) in [(1u64, 200u64), (2, 201), (3, 202)] {
        b.add_file(&format!("{home}/Pictures/2026/IMG_0{d}.JPG"), 2_100_000 + i * 310_000, days_ago(30 + i, 0));
    }
    b.add_dir(&format!("{home}/Pictures/Fellor Import"));
    b
}

fn fake_iphone() -> MemBackend {
    let b = MemBackend::new(
        info("ios:DEMO00008110", "iPhone (демо)", DeviceKind::IPhone, DeviceState::Ready, Some("iPhone15,2"), false),
        128_000_000_000,
    );
    let mut n = 401u32;
    for day in (0..6u64).rev() {
        for k in 0..(4 + day * 2) {
            let size = 1_800_000 + ((n as u64 * 7919) % 2_400_000);
            let m = days_ago(day, 600 - k * 7);
            let folder = if n < 430 { "100APPLE" } else { "101APPLE" };
            b.add_file(&format!("DCIM/{folder}/IMG_0{n}.HEIC"), size, m);
            if k % 3 == 0 {
                b.add_file(&format!("DCIM/{folder}/IMG_0{n}.MOV"), size * 2 + 900_000, m);
            }
            if k % 5 == 1 {
                b.add_file(&format!("DCIM/{folder}/IMG_0{}.JPG", n), size / 2, m);
            }
            n += 1;
        }
    }
    b.add_dir("Downloads");
    b.add_file("Downloads/Посадочный.pdf", 210_000, days_ago(4, 0));
    b.add_dir("Books");
    b
}

fn fake_pixel() -> MemBackend {
    let b = MemBackend::new(
        info("mtp:DEMO-PIXEL8", "Pixel 8 (демо)", DeviceKind::Android, DeviceState::NeedsMtpMode, Some("Pixel 8"), false),
        256_000_000_000,
    );
    for i in 0..12u64 {
        b.add_file(
            &format!("Внутренняя память/DCIM/Camera/PXL_20261106_{:06}.jpg", 101500 + i * 37),
            3_000_000 + i * 110_000,
            days_ago(1, i * 11),
        );
    }
    b.add_dir("Внутренняя память/Download");
    *b.fix_on_recheck.lock().unwrap() = Some(DeviceState::Ready);
    b
}

fn fake_ipad() -> MemBackend {
    let b = MemBackend::new(
        info("ios:DEMO-IPAD", "iPad (демо)", DeviceKind::IPad, DeviceState::NeedsTrust, Some("iPad13,4"), false),
        64_000_000_000,
    );
    b.add_file("DCIM/100APPLE/IMG_0001.HEIC", 2_000_000, days_ago(9, 0));
    *b.fix_on_recheck.lock().unwrap() = Some(DeviceState::Ready);
    b
}

fn fake_sd() -> MemBackend {
    let b = MemBackend::new(
        info("vol:SD 64G", "SD 64G", DeviceKind::Volume, DeviceState::Ready, None, true),
        64_000_000_000,
    );
    for i in 1..=9u64 {
        b.add_file(&format!("DCIM/100MSDCF/DSC0{:04}.JPG", 1000 + i), 6_000_000 + i * 230_000, days_ago(14, i * 3));
    }
    b
}

/// Register all fake backends and return a device-event stream that looks
/// like the one from `fellor_devices::start`.
pub fn start(registry: BackendRegistry) -> mpsc::UnboundedReceiver<DeviceEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    let backends: Vec<BackendRef> = vec![
        Arc::new(fake_local()),
        Arc::new(fake_iphone()),
        Arc::new(fake_pixel()),
        Arc::new(fake_ipad()),
        Arc::new(fake_sd()),
    ];
    for b in backends {
        registry.insert(b.clone());
        let _ = tx.send(DeviceEvent::Attached(b));
    }
    // Keep the sender alive for the whole session (the stream never "ends").
    std::mem::forget(tx);
    rx
}

// ───────────────────────────── tiny transfer engine ─────────────────────────────

struct TaskCtl {
    paused: watch::Sender<bool>,
    cancelled: Arc<AtomicBool>,
    conflicts: HashMap<ConflictId, oneshot::Sender<(ConflictResolution, bool)>>,
}

type Tasks = Arc<Mutex<HashMap<TaskId, TaskCtl>>>;

/// Sequential copier over `Backend` calls with a simulated ~40 МБ/с link.
pub struct DemoEngine {
    registry: BackendRegistry,
    tx: mpsc::UnboundedSender<TransferEvent>,
    next_task: AtomicU64,
    next_conflict: Arc<AtomicU64>,
    tasks: Tasks,
}

impl DemoEngine {
    pub fn new(registry: BackendRegistry) -> (Self, mpsc::UnboundedReceiver<TransferEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let e = Self {
            registry,
            tx,
            next_task: AtomicU64::new(1),
            next_conflict: Arc::new(AtomicU64::new(1)),
            tasks: Arc::default(),
        };
        (e, rx)
    }

    pub fn submit(&self, req: TransferRequest) -> TaskId {
        let id = self.next_task.fetch_add(1, Ordering::Relaxed);
        let (ptx, prx) = watch::channel(false);
        let cancelled = Arc::new(AtomicBool::new(false));
        self.tasks.lock().unwrap().insert(id, TaskCtl { paused: ptx, cancelled: cancelled.clone(), conflicts: HashMap::new() });
        let job = Job {
            id,
            req,
            registry: self.registry.clone(),
            tx: self.tx.clone(),
            tasks: self.tasks.clone(),
            next_conflict: self.next_conflict.clone(),
            paused: prx,
            cancelled,
            apply_all: None,
        };
        tokio::spawn(job.run());
        id
    }

    pub fn pause(&self, task: TaskId) {
        if let Some(t) = self.tasks.lock().unwrap().get(&task) {
            let _ = t.paused.send(true);
            let _ = self.tx.send(TransferEvent::Paused { task });
        }
    }

    pub fn resume(&self, task: TaskId) {
        if let Some(t) = self.tasks.lock().unwrap().get(&task) {
            let _ = t.paused.send(false);
            let _ = self.tx.send(TransferEvent::Resumed { task });
        }
    }

    pub fn cancel(&self, task: TaskId) {
        if let Some(t) = self.tasks.lock().unwrap().get_mut(&task) {
            t.cancelled.store(true, Ordering::Relaxed);
            let _ = t.paused.send(false);
            t.conflicts.clear(); // dropping the senders wakes waiters
        }
    }

    pub fn resolve_conflict(&self, task: TaskId, id: ConflictId, res: ConflictResolution, apply_to_all: bool) {
        if let Some(t) = self.tasks.lock().unwrap().get_mut(&task) {
            if let Some(s) = t.conflicts.remove(&id) {
                let _ = s.send((res, apply_to_all));
            }
        }
    }

    pub fn device_attached(&self, _id: &BackendId) {}
}

struct Job {
    id: TaskId,
    req: TransferRequest,
    registry: BackendRegistry,
    tx: mpsc::UnboundedSender<TransferEvent>,
    tasks: Tasks,
    next_conflict: Arc<AtomicU64>,
    paused: watch::Receiver<bool>,
    cancelled: Arc<AtomicBool>,
    apply_all: Option<ConflictResolution>,
}

struct FileJob {
    src: Entry,
    dst: VPath,
}

struct Progress {
    done_files: u64,
    done_bytes: u64,
    total_files: u64,
    total_bytes: u64,
    started: Instant,
    last_emit: Instant,
}

impl Job {
    fn emit(&self, ev: TransferEvent) {
        let _ = self.tx.send(ev);
    }

    fn label(&self) -> String {
        let verb = if self.req.op == TransferOp::Move { "Перемещение" } else { "Копирование" };
        match self.req.sources.as_slice() {
            [one] => format!("{verb}: {} → {}", one.name(), self.req.dest_dir.name()),
            many => format!("{verb}: {} объектов → {}", many.len(), self.req.dest_dir.name()),
        }
    }

    async fn run(mut self) {
        self.emit(TransferEvent::Queued { task: self.id, label: self.label() });
        let outcome = self.run_inner().await;
        let failed = match &outcome {
            Ok(f) => *f,
            Err(_) => 0,
        };
        let outcome = match outcome {
            Ok(_) if self.cancelled.load(Ordering::Relaxed) => TaskOutcome::Cancelled,
            Ok(_) => TaskOutcome::Done,
            Err(VfsError::Cancelled) => TaskOutcome::Cancelled,
            Err(e) => TaskOutcome::Failed(e.to_string()),
        };
        self.tasks.lock().unwrap().remove(&self.id);
        self.emit(TransferEvent::Finished { task: self.id, outcome, failed_files: failed });
    }

    fn backend(&self, id: &str) -> Result<BackendRef> {
        self.registry.get(id).ok_or(VfsError::Disconnected)
    }

    async fn run_inner(&mut self) -> Result<u64> {
        let src_backend = self.backend(&self.req.sources.first().ok_or(VfsError::Other("пусто".into()))?.backend)?;
        let dst_backend = self.backend(&self.req.dest_dir.backend)?;

        // Plan.
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        let mut top_dirs = Vec::new();
        for s in self.req.sources.clone() {
            let e = src_backend.stat(&s).await?;
            let dst = self.req.dest_dir.join(e.name.clone());
            if e.is_dir() {
                if dst.starts_with(&s) {
                    return Err(VfsError::Other("нельзя копировать папку внутрь самой себя".into()));
                }
                top_dirs.push(s.clone());
                let mut stack = vec![(s, dst)];
                while let Some((sd, dd)) = stack.pop() {
                    dirs.push(dd.clone());
                    for c in src_backend.list(&sd).await? {
                        let cd = dd.join(c.name.clone());
                        if c.is_dir() {
                            stack.push((c.path.clone(), cd));
                        } else {
                            files.push(FileJob { src: c, dst: cd });
                        }
                    }
                }
            } else {
                files.push(FileJob { src: e, dst });
            }
        }
        let total_bytes = files.iter().map(|f| f.src.size.unwrap_or(0)).sum();
        let mut p = Progress {
            done_files: 0,
            done_bytes: 0,
            total_files: files.len() as u64,
            total_bytes,
            started: Instant::now(),
            last_emit: Instant::now(),
        };
        self.emit(TransferEvent::Planned { task: self.id, total_files: p.total_files, total_bytes });

        for d in &dirs {
            match dst_backend.mkdir(d).await {
                Ok(_) | Err(VfsError::AlreadyExists(_)) => {}
                Err(e) => return Err(e),
            }
        }

        let mut failed = 0;
        for f in files {
            if self.cancelled.load(Ordering::Relaxed) {
                return Err(VfsError::Cancelled);
            }
            match self.copy_one(&src_backend, &dst_backend, &f, &mut p).await {
                Ok(()) => {}
                Err(VfsError::Cancelled) => return Err(VfsError::Cancelled),
                Err(e) => {
                    failed += 1;
                    self.emit(TransferEvent::FileFailed { task: self.id, path: f.src.path.clone(), error: e.to_string() });
                }
            }
            p.done_files += 1;
        }
        self.progress(&mut p, None, true);
        if self.req.op == TransferOp::Move && failed == 0 {
            for d in top_dirs {
                let _ = src_backend.remove(&d, true).await;
            }
        }
        Ok(failed)
    }

    fn progress(&self, p: &mut Progress, current: Option<&VPath>, force: bool) {
        if !force && p.last_emit.elapsed() < Duration::from_millis(100) {
            return;
        }
        p.last_emit = Instant::now();
        let secs = p.started.elapsed().as_secs_f64().max(0.001);
        self.emit(TransferEvent::Progress {
            task: self.id,
            done_files: p.done_files,
            done_bytes: p.done_bytes,
            total_files: p.total_files,
            total_bytes: p.total_bytes,
            bytes_per_sec: p.done_bytes as f64 / secs,
            current: current.cloned(),
        });
    }

    async fn wait_unpaused(&mut self) -> Result<()> {
        while *self.paused.borrow() {
            if self.paused.changed().await.is_err() {
                break;
            }
        }
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(VfsError::Cancelled);
        }
        Ok(())
    }

    async fn ask(&mut self, src: &Entry, dst: &Entry) -> Result<ConflictResolution> {
        if let Some(r) = self.apply_all {
            return Ok(r);
        }
        let id = self.next_conflict.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Some(t) = self.tasks.lock().unwrap().get_mut(&self.id) {
            t.conflicts.insert(id, tx);
        }
        self.emit(TransferEvent::Conflict { task: self.id, id, src: src.clone(), dst: dst.clone() });
        let (res, all) = rx.await.map_err(|_| VfsError::Cancelled)?;
        if all {
            self.apply_all = Some(res);
        }
        Ok(res)
    }

    async fn copy_one(&mut self, sb: &BackendRef, db: &BackendRef, f: &FileJob, p: &mut Progress) -> Result<()> {
        let size = f.src.size.unwrap_or(0);
        let mut dst = f.dst.clone();
        if let Ok(existing) = db.stat(&dst).await {
            let res = match self.req.policy {
                ConflictPolicy::Ask => self.ask(&f.src, &existing).await?,
                ConflictPolicy::Skip => ConflictResolution::Skip,
                ConflictPolicy::Overwrite => ConflictResolution::Overwrite,
                ConflictPolicy::Rename => ConflictResolution::Rename,
                ConflictPolicy::KeepNewer => ConflictResolution::KeepNewer,
                ConflictPolicy::SkipIfSameSize if existing.size == f.src.size => ConflictResolution::Skip,
                ConflictPolicy::SkipIfSameSize => ConflictResolution::Overwrite,
            };
            let overwrite = match res {
                ConflictResolution::Skip => false,
                ConflictResolution::Overwrite => true,
                ConflictResolution::KeepNewer => f.src.mtime > existing.mtime,
                ConflictResolution::Rename => {
                    dst = free_name(db, &dst).await;
                    true
                }
            };
            if !overwrite {
                p.done_bytes += size;
                return Ok(());
            }
            if res != ConflictResolution::Rename {
                db.remove(&dst, true).await?;
            }
        }
        let mut reader = sb.open_read(&f.src.path, 0).await?;
        let mut sink = db.create_write(&dst, size, f.src.mtime).await?;
        loop {
            if let Err(e) = self.wait_unpaused().await {
                let _ = sink.abort().await;
                return Err(e);
            }
            match reader.read_chunk(1 << 20).await? {
                None => break,
                Some(chunk) => {
                    let n = chunk.len() as u64;
                    sink.write_chunk(chunk).await?;
                    p.done_bytes += n;
                    tokio::time::sleep(Duration::from_millis(25)).await; // ~40 МБ/с
                    self.progress(p, Some(&f.src.path), false);
                }
            }
        }
        sink.finish().await?;
        if self.req.op == TransferOp::Move {
            sb.remove(&f.src.path, false).await?;
        }
        Ok(())
    }
}

async fn free_name(b: &BackendRef, p: &VPath) -> VPath {
    let name = p.name().to_owned();
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_owned(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    let parent = p.parent().unwrap_or_else(|| p.clone());
    for i in 2.. {
        let cand = parent.join(format!("{stem} ({i}){ext}"));
        if b.stat(&cand).await.is_err() {
            return cand;
        }
    }
    unreachable!()
}

/// Demo stand-in for `fellor_index::dest_subdir` (which is not usable in a
/// worktree where the index crate is still a stub).
pub fn dest_subdir(template: &str, entry: &Entry) -> String {
    let t = entry.mtime.or(entry.ctime).unwrap_or_else(SystemTime::now);
    let dt: chrono::DateTime<chrono::Local> = t.into();
    template
        .replace("{date}", &dt.format("%Y-%m-%d").to_string())
        .replace("{year}", &dt.format("%Y").to_string())
        .replace("{month}", &dt.format("%m").to_string())
        .replace("{day}", &dt.format("%d").to_string())
}

/// Demo stand-in for `fellor_index::plan_import`: every media file under
/// DCIM that is not in the in-memory "imported" set.
pub async fn plan_import(
    backend: BackendRef,
    imported: &Mutex<std::collections::HashSet<(String, String)>>,
) -> Result<fellor_index::ImportPlan> {
    let id = backend.id();
    let mut stack = vec![];
    for r in backend.roots().await? {
        if r.is_dir() {
            stack.push(r.path);
        }
    }
    let mut new_files = Vec::new();
    let mut already = 0;
    while let Some(d) = stack.pop() {
        for e in backend.list(&d).await? {
            if e.is_dir() {
                stack.push(e.path);
            } else if e.path.components.iter().any(|c| c == "DCIM")
                && e.mime.is_some_and(|m| m.starts_with("image/") || m.starts_with("video/"))
            {
                if imported.lock().unwrap().contains(&(id.clone(), e.path.as_unix())) {
                    already += 1;
                } else {
                    new_files.push(e);
                }
            }
        }
    }
    new_files.sort_by_key(|e| e.mtime);
    let total_bytes = new_files.iter().filter_map(|e| e.size).sum();
    Ok(fellor_index::ImportPlan { new_files, total_bytes, already_imported: already })
}
