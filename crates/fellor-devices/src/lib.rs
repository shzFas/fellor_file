//! Merges all discovery sources into one event stream and keeps a
//! `BackendRegistry` up to date.
use fellor_vfs::{BackendRegistry, DeviceEvent};
use tokio::sync::mpsc;

/// Start every discovery source. Must be called inside a tokio runtime.
/// The returned receiver sees every event *after* the registry was updated.
pub fn start(registry: BackendRegistry) -> mpsc::UnboundedReceiver<DeviceEvent> {
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
    let (tx, rx) = mpsc::unbounded_channel();
    fellor_local::spawn_discovery(raw_tx.clone());
    fellor_ios::spawn_discovery(raw_tx.clone());
    fellor_mtp::spawn_discovery(raw_tx);
    tokio::spawn(async move {
        while let Some(ev) = raw_rx.recv().await {
            match &ev {
                DeviceEvent::Attached(b) | DeviceEvent::Updated(b) => registry.insert(b.clone()),
                DeviceEvent::Detached(id) => {
                    registry.remove(id);
                }
            }
            tracing::info!(?ev, "device event");
            if tx.send(ev).is_err() {
                break;
            }
        }
    });
    rx
}
