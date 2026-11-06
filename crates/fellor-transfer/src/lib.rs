//! Transfer engine: planner, scheduler, copier. (stub — public API is frozen)
use fellor_vfs::*;
use tokio::sync::mpsc;

pub type TaskId = u64;
pub type ConflictId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferOp { Copy, Move }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy { Ask, Skip, Overwrite, Rename, KeepNewer, SkipIfSameSize }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution { Skip, Overwrite, Rename, KeepNewer }

#[derive(Debug, Clone)]
pub struct TransferRequest {
    /// Files and/or directories (copied recursively).
    pub sources: Vec<VPath>,
    /// Destination directory; items land at `dest_dir/<name>`.
    pub dest_dir: VPath,
    pub op: TransferOp,
    pub policy: ConflictPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutcome { Done, Cancelled, Failed(String) }

#[derive(Debug, Clone)]
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

pub struct TransferEngine;

impl TransferEngine {
    /// Must be called inside a tokio runtime.
    pub fn new(_registry: BackendRegistry) -> (Self, mpsc::UnboundedReceiver<TransferEvent>) { todo!() }
    pub fn submit(&self, _req: TransferRequest) -> TaskId { todo!() }
    pub fn pause(&self, _task: TaskId) { todo!() }
    pub fn resume(&self, _task: TaskId) { todo!() }
    pub fn cancel(&self, _task: TaskId) { todo!() }
    pub fn resolve_conflict(&self, _task: TaskId, _id: ConflictId, _res: ConflictResolution, _apply_to_all: bool) { todo!() }
    /// Notify that a backend (re)appeared so waiting tasks can continue.
    pub fn device_attached(&self, _id: &BackendId) { todo!() }
}
