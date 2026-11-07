//! Application state, async plumbing and the per-frame loop.
//!
//! Threading model: a tokio multi-thread runtime is owned by the app; eframe
//! runs on the main thread. Every `Backend` call runs as a tokio task that
//! sends a [`UiMsg`] back through a channel and calls `request_repaint()`.
//! Device and transfer events are forwarded into the same channel and drained
//! at the start of every frame. The UI thread never awaits anything.

use crate::dialogs::{ConflictPrompt, Dialog, PreviewState};
use crate::engine::Engine;
use crate::import::{ImportBatch, ImportDialog};
use crate::pane::Pane;
use crate::settings::{Settings, STORAGE_KEY};
use crate::transfers::Transfers;
use crate::{demo, platform};
use fellor_transfer::{TaskId, TransferEvent, TransferOp, TransferRequest};
use fellor_vfs::*;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::mpsc;

/// Results of background work, delivered to the UI thread.
pub enum UiMsg {
    Device(DeviceEvent),
    Transfer(Box<TransferEvent>),
    Listed { pane: usize, req: u64, result: Result<Vec<Entry>, String> },
    Space { pane: usize, req: u64, space: Option<SpaceInfo> },
    /// A file operation (mkdir / rename / delete / eject…) finished.
    OpDone { errors: Vec<String>, ok: Option<String>, refresh: Vec<VPath>, select: Option<(VPath, String)> },
    Preview { path: VPath, result: Result<Option<egui::ColorImage>, String> },
    /// Fresh info after "Проверить снова".
    DeviceInfo(DeviceInfo),
    Ejected { id: BackendId, result: Result<(), String> },
    ImportPlanned { device: BackendId, result: Result<fellor_index::ImportPlan, String> },
    ImportReady { device: BackendId, groups: Vec<(VPath, Vec<Entry>)>, op: TransferOp, errors: Vec<String> },
    Toast(ToastKind, String),
    /// Hover preview image finished loading.
    HoverImage { key: String, result: Result<Option<egui::ColorImage>, String> },
    /// Download-to-open progress.
    OpenProgress { id: u64, done: u64, total: u64 },
    /// Download-to-open finished; open the local file with its macOS app.
    Opened { id: u64, result: Result<std::path::PathBuf, String> },
}

/// A device file being downloaded so macOS can open/play it.
pub struct OpeningState {
    pub id: u64,
    pub name: String,
    pub done: u64,
    pub total: u64,
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
}

/// The row under the mouse and since when.
pub struct HoverState {
    pub entry: Entry,
    pub since: Instant,
}

/// Small LRU of decoded preview textures, keyed by `media::texture_key`.
#[derive(Default)]
pub struct ThumbCache {
    map: HashMap<String, Option<egui::TextureHandle>>,
    order: VecDeque<String>,
}

impl ThumbCache {
    const CAP: usize = 300;
    pub fn get(&self, k: &str) -> Option<&Option<egui::TextureHandle>> {
        self.map.get(k)
    }
    pub fn insert(&mut self, k: String, v: Option<egui::TextureHandle>) {
        if self.map.insert(k.clone(), v).is_none() {
            self.order.push_back(k);
            while self.order.len() > Self::CAP {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
}

pub struct Toast {
    pub kind: ToastKind,
    pub text: String,
    pub born: Instant,
}

pub struct FellorApp {
    /// Keeps the runtime alive for the app's lifetime.
    _runtime: Runtime,
    pub rt: Handle,
    pub demo: bool,
    pub ctx: egui::Context,
    pub registry: BackendRegistry,
    pub tx: mpsc::UnboundedSender<UiMsg>,
    rx: mpsc::UnboundedReceiver<UiMsg>,
    /// Every known backend except the Mac itself (`DeviceKind::Mac`).
    pub devices: BTreeMap<BackendId, DeviceInfo>,
    /// Backends currently being re-checked ("Проверить снова").
    pub rechecking: HashSet<BackendId>,
    pub panes: [Pane; 2],
    pub active: usize,
    pub engine: Engine,
    pub transfers: Transfers,
    pub settings: Settings,
    pub dialog: Option<Dialog>,
    pub conflicts: VecDeque<ConflictPrompt>,
    pub toasts: Vec<Toast>,
    pub preview: Option<PreviewState>,
    pub opening: Option<OpeningState>,
    pub thumbs: ThumbCache,
    pub hover: Option<HoverState>,
    /// Set by the table each frame a file row is hovered.
    pub hover_seen: bool,
    /// Texture key currently loading for the hover preview (one at a time).
    pub hover_loading: Option<String>,
    /// Dev screenshots only: pretend the mouse rests on this row at this point.
    pub fake_hover: Option<(Entry, egui::Pos2)>,
    pub import: Option<ImportDialog>,
    pub import_tasks: HashMap<TaskId, ImportBatch>,
    /// `--demo` stand-in for the SQLite import index.
    pub demo_imported: Arc<Mutex<HashSet<(String, String)>>>,
    /// ⌘C buffer: sources copied from a pane, pasted with ⌘V into the other.
    pub clipboard: Option<(usize, Vec<VPath>)>,
    pub settings_open: bool,
    pub help_open: bool,
    pub focus_filter: Option<usize>,
    pub devshot: Option<crate::devshot::DevShot>,
    next_req: u64,
}

pub fn home_vpath() -> VPath {
    dirs::home_dir().map(|h| fellor_local::vpath_from_host(&h)).unwrap_or_else(|| VPath::root("local"))
}

/// Backends whose root is a plain directory (list it) as opposed to devices
/// whose top level comes from `roots()`.
fn is_fs_backend(id: &str) -> bool {
    id == "local" || id.starts_with("vol:")
}

impl FellorApp {
    pub fn new(cc: &eframe::CreationContext<'_>, runtime: Runtime, demo_mode: bool) -> Self {
        let ctx = cc.egui_ctx.clone();
        ctx.options_mut(|o| o.theme_preference = egui::ThemePreference::System);
        crate::style::apply(&ctx);

        let settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, STORAGE_KEY)).unwrap_or_default();
        let rt = runtime.handle().clone();
        let registry = BackendRegistry::new();
        let (tx, rx) = mpsc::unbounded_channel();

        // Device discovery + transfer engine. The demo path never touches the
        // real crates' stubs.
        let (dev_rx, (engine, tr_rx)) = {
            let _g = rt.enter();
            if demo_mode {
                (demo::start(registry.clone()), Engine::demo(&rt, registry.clone()))
            } else {
                (fellor_devices::start(registry.clone()), Engine::real(&rt, registry.clone()))
            }
        };
        forward(&rt, &ctx, &tx, dev_rx, UiMsg::Device);
        forward(&rt, &ctx, &tx, tr_rx, |e| UiMsg::Transfer(Box::new(e)));

        let start = |i: usize, fallback: VPath| {
            settings.last_paths[i]
                .as_deref()
                .filter(|p| std::path::Path::new(p).is_dir() || demo_mode)
                .map(|p| fellor_local::vpath_from_host(std::path::Path::new(p)))
                .unwrap_or(fallback)
        };
        let left = start(0, home_vpath());
        let right = start(
            1,
            dirs::picture_dir().map(|p| fellor_local::vpath_from_host(&p)).unwrap_or_else(home_vpath),
        );

        let mut app = Self {
            _runtime: runtime,
            rt,
            demo: demo_mode,
            ctx,
            registry,
            tx,
            rx,
            devices: BTreeMap::new(),
            rechecking: HashSet::new(),
            panes: [Pane::new(left), Pane::new(right)],
            active: 0,
            engine,
            transfers: Transfers::default(),
            settings,
            dialog: None,
            conflicts: VecDeque::new(),
            toasts: Vec::new(),
            preview: None,
            opening: None,
            thumbs: ThumbCache::default(),
            hover: None,
            hover_seen: false,
            hover_loading: None,
            fake_hover: None,
            import: None,
            import_tasks: HashMap::new(),
            demo_imported: Arc::default(),
            clipboard: None,
            settings_open: false,
            help_open: false,
            focus_filter: None,
            devshot: if demo_mode { crate::devshot::DevShot::from_env() } else { None },
            next_req: 0,
        };
        std::thread::spawn(crate::media::prune_cache);
        app.load_pane(0);
        app.load_pane(1);
        app
    }

    // ───────────────────────────── async helpers ─────────────────────────────

    /// Run `fut` on the runtime; its result is delivered as a `UiMsg`.
    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = UiMsg> + Send + 'static,
    {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let handle = self.rt.spawn(fut);
        // A panicking backend must not take the UI down or leave it waiting
        // silently: report it as an error toast.
        self.rt.spawn(async move {
            let msg = match handle.await {
                Ok(m) => m,
                Err(e) => UiMsg::Toast(ToastKind::Error, format!("Внутренняя ошибка: {e}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        let text = text.into();
        if kind == ToastKind::Error {
            tracing::warn!("{text}");
        }
        self.toasts.push(Toast { kind, text, born: Instant::now() });
    }

    pub fn backend(&self, id: &str) -> Option<BackendRef> {
        self.registry.get(id)
    }

    /// User-visible name of a backend ("Мак", "iPhone Алекса").
    pub fn backend_label(&self, id: &str) -> String {
        if id == "local" {
            return "Мак".into();
        }
        self.devices.get(id).map(|d| d.name.clone()).unwrap_or_else(|| id.to_owned())
    }

    // ───────────────────────────── panes ─────────────────────────────

    pub fn load_pane(&mut self, i: usize) {
        self.next_req += 1;
        let req = self.next_req;
        let pane = &mut self.panes[i];
        pane.req = req;
        pane.loading = true;
        let path = pane.path.clone();
        let Some(b) = self.registry.get(&path.backend) else {
            pane.waiting = true;
            return;
        };
        pane.waiting = false;
        let b2 = b.clone();
        let p2 = path.clone();
        self.spawn(async move {
            let result = if path.is_root() && !is_fs_backend(&path.backend) { b.roots().await } else { b.list(&path).await };
            UiMsg::Listed { pane: i, req, result: result.map_err(|e| e.to_string()) }
        });
        self.spawn(async move {
            let space = b2.free_space(&p2).await.ok().flatten();
            UiMsg::Space { pane: i, req, space }
        });
    }

    pub fn navigate(&mut self, i: usize, path: VPath) {
        if self.panes[i].path == path {
            self.load_pane(i);
            return;
        }
        self.panes[i].navigate(path);
        self.load_pane(i);
    }

    pub fn go_back(&mut self, i: usize) {
        if self.panes[i].back() {
            self.load_pane(i);
        }
    }
    pub fn go_forward(&mut self, i: usize) {
        if self.panes[i].forward() {
            self.load_pane(i);
        }
    }
    pub fn go_up(&mut self, i: usize) {
        if self.panes[i].up() {
            self.load_pane(i);
        }
    }

    /// Reload panes that display any of `dirs`.
    pub fn refresh_dirs(&mut self, dirs: &[VPath]) {
        for i in 0..2 {
            if dirs.iter().any(|d| *d == self.panes[i].path) {
                self.load_pane(i);
            }
        }
    }

    /// Enter on an entry / double click.
    pub fn open_entry(&mut self, i: usize, e: Entry) {
        if e.is_dir() {
            self.navigate(i, e.path);
        } else if let Some(host) = platform::host_path(&e.path) {
            if self.demo {
                self.open_preview(e);
            } else if let Err(err) = platform::open(&host) {
                self.toast(ToastKind::Error, format!("Не удалось открыть: {err}"));
            }
        } else if self.demo {
            self.open_preview(e);
        } else {
            self.open_with_app(e);
        }
    }

    /// Open a file with its default macOS app (QuickTime for video, Preview
    /// for photos…). Device files are downloaded into the cache first.
    pub fn open_with_app(&mut self, e: Entry) {
        if e.is_dir() {
            return;
        }
        if let Some(host) = crate::media::local_copy(&e) {
            if let Err(err) = platform::open(&host) {
                self.toast(ToastKind::Error, format!("Не удалось открыть: {err}"));
            }
            return;
        }
        if let Some(o) = &self.opening {
            o.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let Some(b) = self.backend(&e.path.backend) else { return };
        self.next_req += 1;
        let id = self.next_req;
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.opening = Some(OpeningState { id, name: e.name.clone(), done: 0, total: e.size.unwrap_or(0), cancel: cancel.clone() });
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let mut last = Instant::now();
        self.spawn(async move {
            let result = crate::media::fetch_local(b, &e, cancel, move |done, total| {
                if last.elapsed() >= Duration::from_millis(100) {
                    last = Instant::now();
                    let _ = tx.send(UiMsg::OpenProgress { id, done, total });
                    ctx.request_repaint();
                }
            })
            .await;
            UiMsg::Opened { id, result }
        });
    }

    pub fn reveal_in_finder(&mut self, path: &VPath) {
        match platform::host_path(path) {
            Some(h) if !self.demo => {
                if let Err(e) = platform::reveal(&h) {
                    self.toast(ToastKind::Error, format!("Finder: {e}"));
                }
            }
            Some(h) => self.toast(ToastKind::Info, format!("Демо: open -R {}", h.display())),
            None => self.toast(ToastKind::Info, "Доступно только для файлов на Маке"),
        }
    }

    // ───────────────────────────── actions ─────────────────────────────

    /// F5 / F6: copy or move the active pane's selection into the other pane.
    pub fn transfer_to_other(&mut self, op: TransferOp) {
        let src = self.active;
        let sources = self.panes[src].selection_paths();
        self.transfer(sources, 1 - src, op);
    }

    pub fn transfer(&mut self, sources: Vec<VPath>, dest_pane: usize, op: TransferOp) {
        if sources.is_empty() {
            self.toast(ToastKind::Info, "Ничего не выбрано");
            return;
        }
        let dest_dir = self.panes[dest_pane].path.clone();
        if sources.iter().any(|s| s.parent().as_ref() == Some(&dest_dir)) {
            self.toast(ToastKind::Info, "Источник и назначение совпадают");
            return;
        }
        if sources.iter().any(|s| dest_dir.starts_with(s)) {
            self.toast(ToastKind::Error, "Нельзя копировать папку внутрь самой себя");
            return;
        }
        if self.registry.get(&dest_dir.backend).is_none() {
            self.toast(ToastKind::Error, "Панель назначения недоступна");
            return;
        }
        let req = TransferRequest { sources: sources.clone(), dest_dir: dest_dir.clone(), op, policy: self.settings.conflict_policy.policy() };
        let label = transfer_label(op, &sources, &dest_dir);
        let id = self.engine.submit(req);
        let mut refresh = vec![dest_dir];
        if op == TransferOp::Move {
            refresh.extend(sources.iter().filter_map(VPath::parent));
        }
        self.transfers.add(id, label, refresh);
        self.settings.transfers_open = true;
    }

    pub fn copy_to_clipboard(&mut self) {
        let paths = self.panes[self.active].selection_paths();
        if paths.is_empty() {
            return;
        }
        self.toast(ToastKind::Info, format!("Скопировано в буфер: {} — ⌘V в другой панели", paths.len()));
        self.clipboard = Some((self.active, paths));
    }

    pub fn paste(&mut self) {
        match self.clipboard.clone() {
            Some((from, paths)) if from != self.active => {
                self.transfer(paths, self.active, TransferOp::Copy);
            }
            Some(_) => self.toast(ToastKind::Info, "Вставка — в противоположной панели"),
            None => {}
        }
    }

    pub fn ask_new_folder(&mut self) {
        let i = self.active;
        self.dialog = Some(Dialog::NewFolder { pane: i, name: "Новая папка".into(), focus: true });
    }

    pub fn ask_rename(&mut self) {
        let i = self.active;
        let sel = self.panes[i].selection();
        match sel.as_slice() {
            [one] => {
                let e = (*one).clone();
                self.dialog = Some(Dialog::Rename { pane: i, name: e.name.clone(), entry: e, focus: true });
            }
            [] => self.toast(ToastKind::Info, "Выберите файл для переименования"),
            _ => self.toast(ToastKind::Info, "Переименовать можно только один объект"),
        }
    }

    pub fn ask_delete(&mut self) {
        let i = self.active;
        let items: Vec<Entry> = self.panes[i].selection().into_iter().cloned().collect();
        if items.is_empty() {
            self.toast(ToastKind::Info, "Ничего не выбрано");
            return;
        }
        self.dialog = Some(Dialog::Delete { pane: i, items });
    }

    pub fn do_mkdir(&mut self, i: usize, name: String) {
        let dir = self.panes[i].path.clone();
        let Some(b) = self.backend(&dir.backend) else { return };
        let path = dir.join(name.clone());
        self.spawn(async move {
            match b.mkdir(&path).await {
                Ok(_) => UiMsg::OpDone { errors: vec![], ok: None, refresh: vec![dir.clone()], select: Some((dir, name)) },
                Err(e) => UiMsg::OpDone { errors: vec![format!("Не удалось создать папку: {e}")], ok: None, refresh: vec![], select: None },
            }
        });
    }

    pub fn do_rename(&mut self, entry: Entry, new_name: String) {
        let Some(dir) = entry.path.parent() else { return };
        let Some(b) = self.backend(&dir.backend) else { return };
        let to = dir.join(new_name.clone());
        self.spawn(async move {
            match b.rename(&entry.path, &to).await {
                Ok(_) => UiMsg::OpDone { errors: vec![], ok: None, refresh: vec![dir.clone()], select: Some((dir, new_name)) },
                Err(e) => UiMsg::OpDone { errors: vec![format!("Не удалось переименовать: {e}")], ok: None, refresh: vec![], select: None },
            }
        });
    }

    pub fn do_delete(&mut self, items: Vec<Entry>) {
        let Some(first) = items.first() else { return };
        let Some(b) = self.backend(&first.path.backend) else { return };
        let dirs: Vec<VPath> = items.iter().filter_map(|e| e.path.parent()).collect::<HashSet<_>>().into_iter().collect();
        self.spawn(async move {
            let mut errors = Vec::new();
            let mut n = 0;
            for e in &items {
                match b.remove(&e.path, e.is_dir()).await {
                    Ok(()) => n += 1,
                    Err(err) => errors.push(format!("{}: {err}", e.name)),
                }
            }
            UiMsg::OpDone { errors, ok: Some(format!("Удалено: {n}")), refresh: dirs, select: None }
        });
    }

    pub fn eject(&mut self, id: BackendId) {
        let Some(b) = self.backend(&id) else { return };
        self.spawn(async move {
            let result = b.eject().await.map_err(|e| e.to_string());
            UiMsg::Ejected { id, result }
        });
    }

    /// ⌘E: eject whatever the active pane shows, if it is ejectable.
    pub fn eject_active(&mut self) {
        let id = self.panes[self.active].path.backend.clone();
        if self.devices.get(&id).is_some_and(|d| d.ejectable) {
            self.eject(id);
        } else {
            self.toast(ToastKind::Info, "Этот диск нельзя извлечь");
        }
    }

    /// "Проверить снова": poke the backend and pick up its new state.
    pub fn recheck(&mut self, id: BackendId) {
        let Some(b) = self.backend(&id) else { return };
        self.rechecking.insert(id);
        self.spawn(async move {
            let _ = b.roots().await;
            UiMsg::DeviceInfo(b.info())
        });
    }

    pub fn open_device(&mut self, id: &str) {
        let i = self.active;
        self.navigate(i, VPath::root(id));
    }

    pub fn open_preview(&mut self, entry: Entry) {
        if entry.is_dir() {
            return;
        }
        let Some(b) = self.backend(&entry.path.backend) else { return };
        let path = entry.path.clone();
        self.preview = Some(PreviewState::new(entry.clone()));
        self.spawn(async move {
            let result = crate::media::load_image(b, entry, 2560, crate::media::Fetch::ImagesAndSmallVideos).await;
            UiMsg::Preview { path, result }
        });
    }

    /// Start loading the hover preview for `e` unless cached or busy.
    fn request_hover_image(&mut self, e: &Entry, max_px: u32) {
        let key = crate::media::texture_key(e, max_px);
        if self.thumbs.get(&key).is_some() || self.hover_loading.is_some() {
            return;
        }
        let Some(b) = self.backend(&e.path.backend) else { return };
        self.hover_loading = Some(key.clone());
        let e = e.clone();
        self.spawn(async move {
            let result = crate::media::load_image(b, e, max_px, crate::media::Fetch::Images).await;
            UiMsg::HoverImage { key, result }
        });
    }

    /// Floating preview next to the mouse for the hovered file row.
    fn hover_preview_ui(&mut self, ctx: &egui::Context) {
        if let Some((e, _)) = &self.fake_hover {
            self.hover_seen = true;
            if self.hover.as_ref().map(|h| &h.entry.path) != Some(&e.path) {
                self.hover = Some(HoverState { entry: e.clone(), since: Instant::now() - Duration::from_secs(1) });
            }
        }
        let blocked = self.dialog.is_some() || self.import.is_some() || !self.conflicts.is_empty() || self.settings_open || self.help_open || self.preview.is_some();
        if !self.hover_seen || blocked || !self.settings.hover_preview || ctx.input(|i| i.pointer.any_down()) {
            self.hover = None;
            return;
        }
        let Some(h) = &self.hover else { return };
        let delay = Duration::from_millis(self.settings.hover_delay_ms as u64);
        if h.since.elapsed() < delay {
            ctx.request_repaint_after(delay - h.since.elapsed());
            return;
        }
        let e = h.entry.clone();
        let side = self.settings.hover_size.clamp(160.0, 900.0);
        let max_px = (side * ctx.pixels_per_point()).round() as u32;
        self.request_hover_image(&e, max_px);
        let key = crate::media::texture_key(&e, max_px);
        let tex = self.thumbs.get(&key).cloned();
        let loading = tex.is_none();
        let Some(pos) = self.fake_hover.as_ref().map(|f| f.1).or_else(|| ctx.pointer_hover_pos()) else { return };
        let screen = ctx.content_rect();
        // Place to the right of the cursor, or to the left if it doesn't fit.
        let w = side + 16.0;
        let x = if pos.x + 24.0 + w < screen.right() { pos.x + 24.0 } else { (pos.x - 24.0 - w).max(screen.left()) };
        let y = (pos.y - side / 2.0).clamp(screen.top(), (screen.bottom() - side - 60.0).max(screen.top()));
        egui::Area::new(egui::Id::unique("hover-preview"))
            .order(egui::Order::Tooltip)
            .fixed_pos(egui::pos2(x, y))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(side);
                    match tex {
                        Some(Some(t)) => {
                            let sz = t.size_vec2() / ctx.pixels_per_point();
                            let k = (side / sz.x).min(side / sz.y).min(1.0);
                            ui.add(egui::Image::new(&t).fit_to_exact_size(sz * k));
                        }
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Загрузка превью…");
                            });
                        }
                        Some(None) => {
                            let hint = if crate::media::is_video(&e) { "🎞 Enter — воспроизвести" } else { "Превью недоступно" };
                            ui.label(egui::RichText::new(hint).weak());
                        }
                    }
                    ui.label(egui::RichText::new(&e.name).small().strong());
                    ui.label(
                        egui::RichText::new(format!(
                            "{} · {}",
                            e.size.map(crate::fmt::size).unwrap_or_else(|| "—".into()),
                            crate::fmt::date(e.mtime)
                        ))
                        .small()
                        .weak(),
                    );
                });
            });
        if loading {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Progress strip while a device file is downloaded for opening.
    fn opening_ui(&mut self, ctx: &egui::Context) {
        let Some(o) = &self.opening else { return };
        let mut cancel = false;
        egui::Area::new(egui::Id::unique("opening"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -48.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(380.0);
                    ui.label(format!("Открываю «{}»…", o.name));
                    let frac = if o.total > 0 { o.done as f32 / o.total as f32 } else { 0.0 };
                    ui.add(egui::ProgressBar::new(frac).show_percentage().desired_width(360.0));
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{} из {}", crate::fmt::size(o.done), crate::fmt::size(o.total)))
                                .small()
                                .weak(),
                        );
                        if ui.button("Отмена").clicked() {
                            cancel = true;
                        }
                    });
                });
            });
        if cancel {
            o.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            self.opening = None;
        }
    }

    // ───────────────────────────── event handling ─────────────────────────────

    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
    }

    fn handle(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::Device(ev) => self.on_device(ev),
            UiMsg::Transfer(ev) => self.on_transfer(*ev),
            UiMsg::Listed { pane, req, result } => {
                let show_hidden = self.settings.show_hidden;
                let p = &mut self.panes[pane];
                if p.req != req {
                    return;
                }
                match result {
                    Ok(entries) => p.set_entries(entries, show_hidden),
                    Err(e) => p.set_error(e),
                }
            }
            UiMsg::Space { pane, req, space } => {
                if self.panes[pane].req == req {
                    self.panes[pane].space = space;
                }
            }
            UiMsg::OpDone { errors, ok, refresh, select } => {
                if let Some((dir, name)) = select {
                    for p in &mut self.panes {
                        if p.path == dir {
                            p.selected.clear();
                            p.pending_select = Some(name.clone());
                        }
                    }
                }
                for e in errors {
                    self.toast(ToastKind::Error, e);
                }
                if let Some(ok) = ok {
                    self.toast(ToastKind::Info, ok);
                }
                self.refresh_dirs(&refresh);
            }
            UiMsg::Preview { path, result } => {
                if let Some(p) = &mut self.preview {
                    if p.entry.path == path {
                        p.set_result(&self.ctx, result);
                    }
                }
            }
            UiMsg::HoverImage { key, result } => {
                if self.hover_loading.as_deref() == Some(&key) {
                    self.hover_loading = None;
                }
                let tex = match result {
                    Ok(Some(img)) => Some(self.ctx.load_texture(format!("hover:{key}"), img, egui::TextureOptions::LINEAR)),
                    Ok(None) => None,
                    Err(e) => {
                        tracing::debug!("hover preview failed: {e}");
                        None
                    }
                };
                self.thumbs.insert(key, tex);
            }
            UiMsg::OpenProgress { id, done, total } => {
                if let Some(o) = &mut self.opening {
                    if o.id == id {
                        o.done = done;
                        o.total = total;
                    }
                }
            }
            UiMsg::Opened { id, result } => {
                if self.opening.as_ref().is_some_and(|o| o.id == id) {
                    self.opening = None;
                    match result {
                        Ok(p) => {
                            if let Err(err) = platform::open(&p) {
                                self.toast(ToastKind::Error, format!("Не удалось открыть: {err}"));
                            }
                        }
                        Err(e) => self.toast(ToastKind::Error, format!("Не удалось открыть: {e}")),
                    }
                }
            }
            UiMsg::DeviceInfo(info) => {
                self.rechecking.remove(&info.id);
                let ready = info.state == DeviceState::Ready;
                let id = info.id.clone();
                if info.kind != DeviceKind::Mac {
                    self.devices.insert(id.clone(), info);
                }
                if ready {
                    self.engine.device_attached(&id);
                    self.reload_backend_panes(&id);
                } else {
                    self.toast(ToastKind::Info, "Состояние не изменилось");
                }
            }
            UiMsg::Ejected { id, result } => match result {
                Ok(()) => {
                    let name = self.backend_label(&id);
                    self.toast(ToastKind::Info, format!("«{name}» извлечён"));
                    self.forget_device(&id);
                }
                Err(e) => self.toast(ToastKind::Error, format!("Не удалось извлечь: {e}")),
            },
            UiMsg::ImportPlanned { device, result } => self.on_import_planned(device, result),
            UiMsg::ImportReady { device, groups, op, errors } => self.on_import_ready(device, groups, op, errors),
            UiMsg::Toast(kind, text) => self.toast(kind, text),
        }
    }

    fn reload_backend_panes(&mut self, id: &str) {
        for i in 0..2 {
            let p = &self.panes[i];
            if p.path.backend == id && (p.waiting || p.error.is_some()) {
                self.load_pane(i);
            }
        }
    }

    /// Device gone: drop it and move panes that showed it back home.
    fn forget_device(&mut self, id: &str) {
        self.devices.remove(id);
        if !self.demo {
            // fellor-devices removes it on Detached; after a successful eject
            // the backend is unusable anyway.
            self.registry.remove(id);
        }
        for i in 0..2 {
            if self.panes[i].path.backend == id {
                self.navigate(i, home_vpath());
            }
        }
    }

    fn on_device(&mut self, ev: DeviceEvent) {
        match ev {
            DeviceEvent::Attached(b) => {
                let info = b.info();
                let id = info.id.clone();
                if info.kind != DeviceKind::Mac {
                    let focused = self.ctx.input(|i| i.viewport().focused).unwrap_or(true);
                    if !focused && info.kind != DeviceKind::Volume {
                        platform::notify("Fellor File", &format!("{} подключён — {}", kind_word(info.kind), info.name));
                    }
                    self.devices.insert(id.clone(), info);
                }
                self.engine.device_attached(&id);
                self.reload_backend_panes(&id);
            }
            DeviceEvent::Updated(b) => {
                let info = b.info();
                let id = info.id.clone();
                let ready = info.state == DeviceState::Ready;
                if info.kind != DeviceKind::Mac {
                    self.devices.insert(id.clone(), info);
                }
                if ready {
                    self.engine.device_attached(&id);
                    self.reload_backend_panes(&id);
                }
            }
            DeviceEvent::Detached(id) => {
                if let Some(d) = self.devices.get(&id) {
                    let name = d.name.clone();
                    self.toast(ToastKind::Info, format!("«{name}» отключён"));
                }
                self.devices.remove(&id);
                for i in 0..2 {
                    if self.panes[i].path.backend == id {
                        self.navigate(i, home_vpath());
                    }
                }
            }
        }
    }

    fn on_transfer(&mut self, ev: TransferEvent) {
        match &ev {
            TransferEvent::Conflict { task, id, src, dst } => {
                self.conflicts.push_back(ConflictPrompt { task: *task, id: *id, src: src.clone(), dst: dst.clone(), apply_all: false });
            }
            TransferEvent::FileFailed { task, path, .. } => {
                if let Some(b) = self.import_tasks.get_mut(task) {
                    b.failed.insert(path.clone());
                }
            }
            TransferEvent::WaitingForDevice { backend, .. } => {
                let name = self.backend_label(backend);
                self.toast(ToastKind::Info, format!("Ожидание устройства «{name}» — подключите его снова"));
            }
            _ => {}
        }
        if let TransferEvent::Finished { task, outcome, failed_files } = &ev {
            let is_import = self.import_tasks.contains_key(task);
            let label = self.transfers.get(*task).map(|t| t.label.clone()).unwrap_or_default();
            match outcome {
                fellor_transfer::TaskOutcome::Done if *failed_files == 0 => {
                    if !is_import {
                        self.toast(ToastKind::Info, format!("Готово: {label}"));
                    }
                }
                fellor_transfer::TaskOutcome::Done => {
                    self.toast(ToastKind::Error, format!("{label}: не удалось {}", crate::fmt::files(*failed_files)))
                }
                fellor_transfer::TaskOutcome::Cancelled => self.toast(ToastKind::Info, format!("Отменено: {label}")),
                fellor_transfer::TaskOutcome::Failed(e) => self.toast(ToastKind::Error, format!("{label}: {e}")),
            }
            // Conflicts of a finished task can no longer be answered.
            self.conflicts.retain(|c| c.task != *task);
            if let Some(batch) = self.import_tasks.remove(task) {
                if matches!(outcome, fellor_transfer::TaskOutcome::Done) {
                    self.mark_imported(batch);
                }
                if self.import_tasks.is_empty() {
                    self.toast(ToastKind::Info, "Импорт завершён");
                }
            }
        }
        let refresh = self.transfers.apply(&ev);
        self.refresh_dirs(&refresh);
    }

    // ───────────────────────────── keyboard ─────────────────────────────

    fn shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, Modifiers as M};
        let cmd_shift = M::COMMAND | M::SHIFT;
        let blocked = self.dialog.is_some() || self.import.is_some() || !self.conflicts.is_empty() || self.settings_open || self.help_open;

        // Global shortcuts that work even while typing in the filter field.
        let pressed = |m: M, k: Key| ctx.input_mut(|i| i.consume_key(m, k));
        if !blocked {
            if pressed(M::COMMAND, Key::Comma) {
                self.settings_open = true;
                return;
            }
            if pressed(M::COMMAND, Key::F) {
                self.focus_filter = Some(self.active);
                return;
            }
            if pressed(M::COMMAND, Key::I) {
                self.start_import();
                return;
            }
            if pressed(M::COMMAND, Key::R) {
                self.load_pane(self.active);
                return;
            }
        }
        if blocked || ctx.text_edit_focused() {
            if self.preview.is_some() && !blocked && pressed(M::NONE, Key::Space) {
                self.preview = None;
            }
            return;
        }
        let i = self.active;
        if pressed(cmd_shift, Key::N) || pressed(M::NONE, Key::F7) {
            self.ask_new_folder();
        } else if pressed(cmd_shift, Key::Period) {
            self.settings.show_hidden = !self.settings.show_hidden;
            let sh = self.settings.show_hidden;
            self.panes.iter_mut().for_each(|p| p.rebuild_view(sh));
        } else if pressed(M::COMMAND, Key::Backspace) || pressed(M::NONE, Key::F8) || pressed(M::NONE, Key::Delete) {
            self.ask_delete();
        } else if pressed(M::COMMAND, Key::ArrowUp) || pressed(M::NONE, Key::Backspace) {
            self.go_up(i);
        } else if pressed(M::COMMAND, Key::ArrowDown) || pressed(M::NONE, Key::Enter) {
            if let Some(e) = self.panes[i].cursor_entry().cloned() {
                self.open_entry(i, e);
            }
        } else if pressed(M::COMMAND, Key::OpenBracket) || pressed(M::COMMAND, Key::ArrowLeft) {
            self.go_back(i);
        } else if pressed(M::COMMAND, Key::CloseBracket) || pressed(M::COMMAND, Key::ArrowRight) {
            self.go_forward(i);
        } else if pressed(M::NONE, Key::F5) {
            self.transfer_to_other(TransferOp::Copy);
        } else if pressed(M::NONE, Key::F6) {
            self.transfer_to_other(TransferOp::Move);
        } else if pressed(M::NONE, Key::F2) || pressed(M::SHIFT, Key::Enter) {
            self.ask_rename();
        } else if pressed(M::COMMAND, Key::E) {
            self.eject_active();
        } else if pressed(M::COMMAND, Key::A) {
            self.panes[i].select_all();
        } else if pressed(M::COMMAND, Key::C) {
            self.copy_to_clipboard();
        } else if pressed(M::COMMAND, Key::V) {
            self.paste();
        } else if pressed(M::NONE, Key::Tab) {
            self.active = 1 - i;
            self.preview = None;
        } else if pressed(M::NONE, Key::Space) {
            if self.preview.is_some() {
                self.preview = None;
            } else if let Some(e) = self.panes[i].cursor_entry().cloned() {
                self.open_preview(e);
            }
        } else if pressed(M::NONE, Key::Escape) {
            if self.preview.is_some() {
                self.preview = None;
            } else {
                self.panes[i].selected.clear();
            }
        } else {
            let shift = ctx.input(|inp| inp.modifiers.shift);
            let page = 15;
            for (k, d) in [(Key::ArrowDown, 1isize), (Key::ArrowUp, -1), (Key::PageDown, page), (Key::PageUp, -page)] {
                if ctx.input_mut(|inp| inp.consume_key(M::NONE, k)) {
                    self.panes[i].move_cursor(d, shift);
                    if let Some(p) = &self.preview {
                        let cur = self.panes[i].cursor_entry().cloned();
                        if cur.as_ref().is_some_and(|c| c.path != p.entry.path) {
                            self.open_preview(cur.unwrap());
                        }
                    }
                }
            }
            if pressed(M::NONE, Key::Home) {
                self.panes[i].move_cursor(-(isize::MAX / 2), shift);
            } else if pressed(M::NONE, Key::End) {
                self.panes[i].move_cursor(isize::MAX / 2, shift);
            }
        }
    }

    fn show_toasts(&mut self, ctx: &egui::Context) {
        const TTL: Duration = Duration::from_secs(5);
        self.toasts.retain(|t| t.born.elapsed() < TTL);
        if self.toasts.len() > 5 {
            let drop = self.toasts.len() - 5;
            self.toasts.drain(..drop);
        }
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::unique("toasts"))
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 52.0))
            .order(egui::Order::Tooltip)
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_max_width(360.0);
                for t in &self.toasts {
                    let (fill, stroke) = match t.kind {
                        ToastKind::Info => (ui.visuals().window_fill, ui.visuals().widgets.noninteractive.bg_stroke),
                        ToastKind::Error => (
                            ui.visuals().error_fg_color.gamma_multiply(0.18),
                            egui::Stroke::new(1.0, ui.visuals().error_fg_color),
                        ),
                    };
                    egui::Frame::popup(ui.style()).fill(fill).stroke(stroke).show(ui, |ui| {
                        ui.label(&t.text);
                    });
                    ui.add_space(6.0);
                }
            });
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

fn kind_word(k: DeviceKind) -> &'static str {
    match k {
        DeviceKind::IPhone => "iPhone",
        DeviceKind::IPad => "iPad",
        DeviceKind::IPod => "iPod",
        DeviceKind::Android => "Android-телефон",
        DeviceKind::Camera => "Камера",
        DeviceKind::Volume => "Диск",
        DeviceKind::Mac => "Мак",
    }
}

pub fn transfer_label(op: TransferOp, sources: &[VPath], dest: &VPath) -> String {
    let verb = if op == TransferOp::Move { "Перемещение" } else { "Копирование" };
    let dest_name = if dest.is_root() { dest.backend.clone() } else { dest.name().to_owned() };
    match sources {
        [one] => format!("{verb}: {} → {dest_name}", one.name()),
        many => format!("{verb}: {} объектов → {dest_name}", many.len()),
    }
}

/// Forward a channel into the UI channel, waking the UI on every message.
fn forward<T: Send + 'static>(
    rt: &Handle,
    ctx: &egui::Context,
    tx: &mpsc::UnboundedSender<UiMsg>,
    mut rx: mpsc::UnboundedReceiver<T>,
    wrap: fn(T) -> UiMsg,
) {
    let (ctx, tx) = (ctx.clone(), tx.clone());
    rt.spawn(async move {
        while let Some(m) = rx.recv().await {
            if tx.send(wrap(m)).is_err() {
                break;
            }
            ctx.request_repaint();
        }
    });
}

impl eframe::App for FellorApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.devshot.is_some() {
            // Dev screenshots must work even when the window is occluded.
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain();
        self.devshot_tick(&ctx);
        self.shortcuts(&ctx);

        self.toolbar(ui);
        self.transfers_panel(ui);
        self.sidebar(ui);
        self.hover_seen = false;
        egui::CentralPanel::default().show(ui, |ui| self.panes_ui(ui));

        self.dialogs(&ctx);
        self.hover_preview_ui(&ctx);
        self.opening_ui(&ctx);
        self.show_toasts(&ctx);
        if self.transfers.any_active() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        for i in 0..2 {
            let p = &self.panes[i].path;
            if p.backend == "local" {
                self.settings.last_paths[i] = Some(p.as_unix());
            }
        }
        eframe::set_value(storage, STORAGE_KEY, &self.settings);
    }
}
