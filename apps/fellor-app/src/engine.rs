//! One façade over the real `TransferEngine` and the `--demo` engine so the
//! rest of the UI does not care which one is running.

use crate::demo::DemoEngine;
use fellor_transfer::{ConflictId, ConflictResolution, TaskId, TransferEngine, TransferEvent, TransferRequest};
use fellor_vfs::{BackendId, BackendRegistry};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedReceiver;

enum Inner {
    Real(TransferEngine),
    Demo(DemoEngine),
}

pub struct Engine {
    inner: Inner,
    rt: Handle,
}

impl Engine {
    /// Real engine (`fellor_transfer`).
    pub fn real(rt: &Handle, registry: BackendRegistry) -> (Self, UnboundedReceiver<TransferEvent>) {
        let _g = rt.enter();
        let (e, rx) = TransferEngine::new(registry);
        (Self { inner: Inner::Real(e), rt: rt.clone() }, rx)
    }

    pub fn demo(rt: &Handle, registry: BackendRegistry) -> (Self, UnboundedReceiver<TransferEvent>) {
        let _g = rt.enter();
        let (e, rx) = DemoEngine::new(registry);
        (Self { inner: Inner::Demo(e), rt: rt.clone() }, rx)
    }

    pub fn submit(&self, req: TransferRequest) -> TaskId {
        let _g = self.rt.enter();
        match &self.inner {
            Inner::Real(e) => e.submit(req),
            Inner::Demo(e) => e.submit(req),
        }
    }

    pub fn pause(&self, task: TaskId) {
        let _g = self.rt.enter();
        match &self.inner {
            Inner::Real(e) => e.pause(task),
            Inner::Demo(e) => e.pause(task),
        }
    }

    pub fn resume(&self, task: TaskId) {
        let _g = self.rt.enter();
        match &self.inner {
            Inner::Real(e) => e.resume(task),
            Inner::Demo(e) => e.resume(task),
        }
    }

    pub fn cancel(&self, task: TaskId) {
        let _g = self.rt.enter();
        match &self.inner {
            Inner::Real(e) => e.cancel(task),
            Inner::Demo(e) => e.cancel(task),
        }
    }

    pub fn resolve_conflict(&self, task: TaskId, id: ConflictId, res: ConflictResolution, apply_to_all: bool) {
        let _g = self.rt.enter();
        match &self.inner {
            Inner::Real(e) => e.resolve_conflict(task, id, res, apply_to_all),
            Inner::Demo(e) => e.resolve_conflict(task, id, res, apply_to_all),
        }
    }

    pub fn device_attached(&self, id: &BackendId) {
        let _g = self.rt.enter();
        match &self.inner {
            Inner::Real(e) => e.device_attached(id),
            Inner::Demo(e) => e.device_attached(id),
        }
    }
}
