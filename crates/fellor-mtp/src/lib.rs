//! Android / camera backend over PTP/MTP (protocol in `fellor-ptp`).
//!
//! * [`MtpBackend`] implements `fellor_vfs::Backend`; one directory per
//!   storage at the device root, a lazily-built path→handle cache below.
//! * [`spawn_discovery`] watches USB hotplug and emits `DeviceEvent`s with
//!   `mtp:<serial>` backends in Ready / Locked / NeedsMtpMode / Busy states.

pub mod backend;
pub mod busy;
pub mod discovery;
pub mod tree;

pub use backend::{MtpBackend, Storage};
use fellor_vfs::DeviceEventTx;

/// USB hotplug via nusb; emits Attached/Updated/Detached with `mtp:<serial>` backends.
pub fn spawn_discovery(tx: DeviceEventTx) -> tokio::task::JoinHandle<()> {
    discovery::spawn(tx)
}
