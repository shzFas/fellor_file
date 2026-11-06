//! Android / camera backend over PTP/MTP. (stub)
use fellor_vfs::*;

/// USB hotplug via nusb; emits Attached/Updated/Detached with `mtp:<serial>` backends.
pub fn spawn_discovery(_tx: DeviceEventTx) -> tokio::task::JoinHandle<()> { tokio::spawn(async {}) }
