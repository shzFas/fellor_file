//! iOS backend over usbmuxd / lockdown / AFC / house_arrest.

pub mod backend;
pub mod connector;
pub mod discovery;

pub use backend::{IosBackend, APPS_ROOT, MEDIA_ROOT};
pub use connector::{AfcTarget, Connector, LockdownConnector};

use fellor_lockdown::{Device, DeviceValues, LockdownClient};
use fellor_usbmux::{UsbmuxClient, LOCKDOWN_PORT};
use fellor_vfs::*;
use std::sync::Arc;

/// Listens to usbmuxd; emits Attached/Updated/Detached with `ios:<udid>` backends.
/// If usbmuxd is unavailable, logs and retries every few seconds.
pub fn spawn_discovery(tx: DeviceEventTx) -> tokio::task::JoinHandle<()> {
    tokio::spawn(discovery::run(Arc::new(discovery::UsbmuxEnv), tx))
}

pub fn device_kind(class: Option<&str>) -> DeviceKind {
    match class {
        Some("iPad") => DeviceKind::IPad,
        Some("iPod") => DeviceKind::IPod,
        _ => DeviceKind::IPhone,
    }
}

pub fn device_info(udid: &str, values: &DeviceValues, state: DeviceState) -> DeviceInfo {
    let kind = device_kind(values.device_class.as_deref());
    let fallback = match kind {
        DeviceKind::IPad => "iPad",
        DeviceKind::IPod => "iPod",
        _ => "iPhone",
    };
    DeviceInfo {
        id: IosBackend::backend_id(udid),
        name: values.device_name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| fallback.to_string()),
        model: values.product_type.clone(),
        os_version: values.product_version.clone(),
        kind,
        state,
        ejectable: false,
    }
}

pub(crate) fn placeholder_info(udid: &str, state: DeviceState, values: Option<&DeviceValues>) -> DeviceInfo {
    device_info(udid, values.unwrap_or(&DeviceValues::default()), state)
}

/// Values lockdown hands out without a session (pre-trust): usually
/// DeviceClass, ProductType, ProductVersion, sometimes DeviceName.
pub async fn public_values(device_id: u32) -> Result<DeviceValues> {
    let io = UsbmuxClient::connect().await?.connect_device(device_id, LOCKDOWN_PORT).await?;
    let mut ld = LockdownClient::new(Box::new(io));
    match ld.get_value(None, None).await? {
        plist::Value::Dictionary(d) => Ok(DeviceValues::from_dict(&d)),
        _ => Err(VfsError::Protocol("GetValue: not a dictionary".into())),
    }
}

impl IosBackend {
    /// Connect to a USB device: pair record → lockdown session → media AFC.
    /// Errors: `NeedsTrust` (no/invalid pair record), `Locked`, transport errors.
    pub async fn connect(device_id: u32, udid: &str) -> Result<Self> {
        let device = Device::open(device_id, udid).await?;
        let values = device.values().await?;
        let connector = Arc::new(LockdownConnector { device });
        let media = connector.open_afc(&AfcTarget::Media).await?;
        let info = device_info(udid, &values, DeviceState::Ready);
        Ok(IosBackend::with_media_session(info, connector, media))
    }
}

#[cfg(test)]
mod tests;
