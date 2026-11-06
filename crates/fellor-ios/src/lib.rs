//! iOS backend over usbmuxd / lockdown / AFC / house_arrest. (stub)
use fellor_vfs::*;

/// Listens to usbmuxd; emits Attached/Updated/Detached with `ios:<udid>` backends.
/// If usbmuxd is unavailable, logs and retries every few seconds.
pub fn spawn_discovery(_tx: DeviceEventTx) -> tokio::task::JoinHandle<()> { tokio::spawn(async {}) }
