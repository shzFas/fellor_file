//! Transfer engine: planner, scheduler, copier (SPEC §3.3).
//!
//! * **Planner** walks the sources recursively via `Backend::list`, computes
//!   totals and checks free space up front.
//! * **Scheduler** runs up to `min(max_parallel)` of the involved backends
//!   per task, and additionally caps every backend globally (across tasks) at
//!   its own `max_parallel`. Device → device copies are staged through a
//!   local temp file.
//! * **Copier** streams in chunks (1 MiB iOS, 4 MiB MTP, 8 MiB local), emits
//!   throttled progress, retries transport errors (resuming with
//!   `open_read(offset)` when possible), waits for devices that disappear,
//!   and sets the source mtime on the copy.
//!
//! `done_files` in progress events counts every *processed* file: copied,
//! skipped and failed ones, so the bar always reaches 100%.

mod names;
mod progress;
mod stage;
mod task;

use fellor_vfs::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch, Notify, Semaphore};
use tokio_util::sync::CancellationToken;

pub use names::{chunk_size_for, is_device_backend, numbered_name};

pub type TaskId = u64;
pub type ConflictId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferOp {
    Copy,
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    Ask,
    Skip,
    Overwrite,
    Rename,
    KeepNewer,
    SkipIfSameSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    Skip,
    Overwrite,
    Rename,
    KeepNewer,
}

#[derive(Debug, Clone)]
pub struct TransferRequest {
    /// Files and/or directories (copied recursively).
    pub sources: Vec<VPath>,
    /// Destination directory; items land at `dest_dir/<name>`.
    /// Created (with parents) if it doesn't exist.
    pub dest_dir: VPath,
    pub op: TransferOp,
    pub policy: ConflictPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutcome {
    Done,
    Cancelled,
    Failed(String),
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // frozen public API; Conflict carries two Entries
pub enum TransferEvent {
    Queued { task: TaskId, label: String },
    /// Planning finished: totals known.
    Planned { task: TaskId, total_files: u64, total_bytes: u64 },
    Progress { task: TaskId, done_files: u64, done_bytes: u64, total_files: u64, total_bytes: u64, bytes_per_sec: f64, current: Option<VPath> },
    /// Only with ConflictPolicy::Ask; task is paused until `resolve_conflict`.
    Conflict { task: TaskId, id: ConflictId, src: Entry, dst: Entry },
    FileFailed { task: TaskId, path: VPath, error: String },
    Paused { task: TaskId },
    Resumed { task: TaskId },
    /// Device disappeared; task resumes automatically when it is back.
    WaitingForDevice { task: TaskId, backend: BackendId },
    Finished { task: TaskId, outcome: TaskOutcome, failed_files: u64 },
}

/// Tunables. `Default` matches production behaviour; tests shorten the delays.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Retries per file for `VfsError::is_retryable()` errors.
    pub max_retries: u32,
    /// Base backoff between retries (doubles each attempt).
    pub retry_backoff: Duration,
    /// How often a task waiting for a device re-checks the registry.
    pub device_poll: Duration,
    /// Minimum interval between `Progress` events per task.
    pub progress_interval: Duration,
    /// Where device → device copies are staged.
    pub temp_dir: PathBuf,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            retry_backoff: Duration::from_millis(500),
            device_poll: Duration::from_secs(2),
            progress_interval: Duration::from_millis(100),
            temp_dir: std::env::temp_dir(),
        }
    }
}

pub(crate) struct TaskCtl {
    pub cancel: CancellationToken,
    pub pause: watch::Sender<bool>,
    pub conflicts: Mutex<HashMap<ConflictId, oneshot::Sender<(ConflictResolution, bool)>>>,
}

pub(crate) struct Inner {
    pub registry: BackendRegistry,
    pub tx: mpsc::UnboundedSender<TransferEvent>,
    pub cfg: EngineConfig,
    pub next_task: AtomicU64,
    pub next_conflict: AtomicU64,
    pub tasks: Mutex<HashMap<TaskId, Arc<TaskCtl>>>,
    pub device_notify: Notify,
    /// Global per-backend concurrency limits.
    pub slots: Mutex<HashMap<BackendId, Arc<Semaphore>>>,
}

impl Inner {
    pub fn emit(&self, ev: TransferEvent) {
        let _ = self.tx.send(ev);
    }

    pub fn slot(&self, be: &BackendRef) -> Arc<Semaphore> {
        let mut m = self.slots.lock().unwrap();
        m.entry(be.id()).or_insert_with(|| Arc::new(Semaphore::new(be.capabilities().max_parallel.max(1)))).clone()
    }
}

pub struct TransferEngine {
    inner: Arc<Inner>,
    rt: tokio::runtime::Handle,
}

impl TransferEngine {
    /// Must be called inside a tokio runtime.
    pub fn new(registry: BackendRegistry) -> (Self, mpsc::UnboundedReceiver<TransferEvent>) {
        Self::with_config(registry, EngineConfig::default())
    }

    /// Like [`TransferEngine::new`] with explicit tunables.
    pub fn with_config(registry: BackendRegistry, cfg: EngineConfig) -> (Self, mpsc::UnboundedReceiver<TransferEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            registry,
            tx,
            cfg,
            next_task: AtomicU64::new(1),
            next_conflict: AtomicU64::new(1),
            tasks: Mutex::new(HashMap::new()),
            device_notify: Notify::new(),
            slots: Mutex::new(HashMap::new()),
        });
        (Self { inner, rt: tokio::runtime::Handle::current() }, rx)
    }

    /// Queue a request; it starts immediately (subject to per-backend limits).
    /// Safe to call from outside the runtime (e.g. the GUI thread).
    pub fn submit(&self, req: TransferRequest) -> TaskId {
        let id = self.inner.next_task.fetch_add(1, Ordering::Relaxed);
        let (pause, _) = watch::channel(false);
        let ctl = Arc::new(TaskCtl { cancel: CancellationToken::new(), pause, conflicts: Mutex::new(HashMap::new()) });
        self.inner.tasks.lock().unwrap().insert(id, ctl.clone());
        self.inner.emit(TransferEvent::Queued { task: id, label: task::label(&req) });
        let inner = self.inner.clone();
        self.rt.spawn(async move { task::run(inner, id, ctl, req).await });
        id
    }

    pub fn pause(&self, task: TaskId) {
        if let Some(ctl) = self.ctl(task) {
            if ctl.pause.send_if_modified(|p| !std::mem::replace(p, true)) {
                self.inner.emit(TransferEvent::Paused { task });
            }
        }
    }

    pub fn resume(&self, task: TaskId) {
        if let Some(ctl) = self.ctl(task) {
            if ctl.pause.send_if_modified(|p| std::mem::replace(p, false)) {
                self.inner.emit(TransferEvent::Resumed { task });
            }
        }
    }

    pub fn cancel(&self, task: TaskId) {
        if let Some(ctl) = self.ctl(task) {
            ctl.cancel.cancel();
        }
    }

    pub fn resolve_conflict(&self, task: TaskId, id: ConflictId, res: ConflictResolution, apply_to_all: bool) {
        if let Some(ctl) = self.ctl(task) {
            if let Some(tx) = ctl.conflicts.lock().unwrap().remove(&id) {
                let _ = tx.send((res, apply_to_all));
            }
        }
    }

    /// Notify that a backend (re)appeared so waiting tasks can continue.
    pub fn device_attached(&self, _id: &BackendId) {
        self.inner.device_notify.notify_waiters();
    }

    /// Ids of tasks that have not finished yet.
    pub fn active_tasks(&self) -> Vec<TaskId> {
        let mut v: Vec<_> = self.inner.tasks.lock().unwrap().keys().copied().collect();
        v.sort_unstable();
        v
    }

    fn ctl(&self, task: TaskId) -> Option<Arc<TaskCtl>> {
        self.inner.tasks.lock().unwrap().get(&task).cloned()
    }
}
