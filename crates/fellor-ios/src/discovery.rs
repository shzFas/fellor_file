//! usbmuxd `Listen` → `DeviceEvent`s.

use crate::backend::IosBackend;
use async_trait::async_trait;
use fellor_usbmux::{BoxIo, ConnectionType, DeviceRecord, Listener, UsbmuxClient, UsbmuxEvent};
use fellor_vfs::{Backend, DeviceEvent, DeviceEventTx, DeviceState, Result, VfsError};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub const POLL_INTERVAL: Duration = Duration::from_secs(3);
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Environment the discovery loop runs against (real usbmuxd or a test fake).
#[async_trait]
pub trait DiscoveryEnv: Send + Sync + 'static {
    async fn listen(&self) -> Result<Listener<BoxIo>>;
    /// Try to build a ready backend for the device.
    async fn probe(&self, rec: &DeviceRecord) -> Result<IosBackend>;
    /// Best-effort placeholder when `probe` failed with `err`.
    async fn placeholder(&self, rec: &DeviceRecord, err: &VfsError) -> IosBackend {
        IosBackend::unavailable(crate::placeholder_info(&rec.udid, state_for_error(err), None))
    }
    fn poll_interval(&self) -> Duration {
        POLL_INTERVAL
    }
}

pub struct UsbmuxEnv;

#[async_trait]
impl DiscoveryEnv for UsbmuxEnv {
    async fn listen(&self) -> Result<Listener<BoxIo>> {
        let c = UsbmuxClient::connect().await?;
        UsbmuxClient::new(Box::new(c.into_inner()) as BoxIo).listen().await
    }

    async fn probe(&self, rec: &DeviceRecord) -> Result<IosBackend> {
        IosBackend::connect(rec.device_id, &rec.udid).await
    }

    async fn placeholder(&self, rec: &DeviceRecord, err: &VfsError) -> IosBackend {
        let values = tokio::time::timeout(Duration::from_secs(5), crate::public_values(rec.device_id)).await.ok().and_then(|r| r.ok());
        IosBackend::unavailable(crate::placeholder_info(&rec.udid, state_for_error(err), values.as_ref()))
    }
}

pub fn state_for_error(e: &VfsError) -> DeviceState {
    match e {
        VfsError::NeedsTrust => DeviceState::NeedsTrust,
        VfsError::Locked => DeviceState::Locked,
        VfsError::Busy(by) => DeviceState::Busy { by: by.clone() },
        e if e.is_retryable() => DeviceState::Connecting,
        e => DeviceState::Error(e.to_string()),
    }
}

/// Keeps probing one device until it is Ready; emits Attached then Updated
/// on every state change.
async fn device_task<E: DiscoveryEnv>(env: Arc<E>, rec: DeviceRecord, tx: DeviceEventTx, nudge: Arc<Notify>, announced: Arc<AtomicBool>) {
    let mut last: Option<DeviceState> = None;
    loop {
        let (backend, ready) = match tokio::time::timeout(PROBE_TIMEOUT, env.probe(&rec)).await.unwrap_or(Err(VfsError::Timeout)) {
            Ok(b) => (b, true),
            Err(e) => {
                tracing::info!(udid = %rec.udid, error = %e, "iOS device not ready yet");
                (env.placeholder(&rec, &e).await, false)
            }
        };
        let state = backend.info().state;
        if last.as_ref() != Some(&state) {
            let b: fellor_vfs::BackendRef = Arc::new(backend);
            let ev = if announced.swap(true, Ordering::SeqCst) { DeviceEvent::Updated(b) } else { DeviceEvent::Attached(b) };
            if tx.send(ev).is_err() {
                return;
            }
            last = Some(state);
        }
        if ready {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(env.poll_interval()) => {}
            _ = nudge.notified() => {}
        }
    }
}

struct Slot {
    udid: String,
    task: JoinHandle<()>,
    nudge: Arc<Notify>,
    announced: Arc<AtomicBool>,
}

fn detach(slot: Slot, tx: &DeviceEventTx) {
    slot.task.abort();
    if slot.announced.load(Ordering::SeqCst) {
        let _ = tx.send(DeviceEvent::Detached(IosBackend::backend_id(&slot.udid)));
    }
}

/// One `Listen` session. Returns when usbmuxd goes away or `tx` closes.
async fn listen_session<E: DiscoveryEnv>(env: &Arc<E>, mut listener: Listener<BoxIo>, tx: &DeviceEventTx) {
    let mut slots: HashMap<u32, Slot> = HashMap::new();
    loop {
        let ev = tokio::select! {
            ev = listener.next_event() => ev,
            _ = tx.closed() => break,
        };
        match ev {
            Ok(UsbmuxEvent::Attached(rec)) => {
                if rec.connection_type != ConnectionType::Usb {
                    tracing::debug!(udid = %rec.udid, "ignoring non-USB iOS device");
                    continue;
                }
                if slots.contains_key(&rec.device_id) {
                    continue;
                }
                // Same phone re-enumerated under a new id: replace silently.
                let dup: Vec<u32> = slots.iter().filter(|(_, s)| s.udid == rec.udid).map(|(k, _)| *k).collect();
                let mut announced = Arc::new(AtomicBool::new(false));
                for k in dup {
                    if let Some(old) = slots.remove(&k) {
                        old.task.abort();
                        announced = old.announced;
                    }
                }
                let nudge = Arc::new(Notify::new());
                let task = tokio::spawn(device_task(env.clone(), rec.clone(), tx.clone(), nudge.clone(), announced.clone()));
                slots.insert(rec.device_id, Slot { udid: rec.udid, task, nudge, announced });
            }
            Ok(UsbmuxEvent::Detached { device_id }) => {
                if let Some(slot) = slots.remove(&device_id) {
                    detach(slot, tx);
                }
            }
            Ok(UsbmuxEvent::Paired { device_id }) => {
                if let Some(slot) = slots.get(&device_id) {
                    slot.nudge.notify_one();
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "usbmuxd listen connection lost");
                break;
            }
        }
    }
    for (_, slot) in slots.drain() {
        detach(slot, tx);
    }
}

pub async fn run<E: DiscoveryEnv>(env: Arc<E>, tx: DeviceEventTx) {
    let mut backoff = Duration::from_secs(1);
    let mut warned = false;
    loop {
        if tx.is_closed() {
            return;
        }
        match env.listen().await {
            Ok(listener) => {
                tracing::info!("listening for iOS devices on usbmuxd");
                backoff = Duration::from_secs(1);
                warned = false;
                listen_session(&env, listener, &tx).await;
            }
            Err(e) => {
                if !warned {
                    tracing::warn!(error = %e, "usbmuxd unavailable; will retry");
                    warned = true;
                } else {
                    tracing::debug!(error = %e, "usbmuxd still unavailable");
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = tx.closed() => return,
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}
