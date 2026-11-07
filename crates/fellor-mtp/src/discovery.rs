//! USB discovery: hotplug watch + periodic rescan, MTP/PTP recognition,
//! per-device connection state machine.

use crate::backend::MtpBackend;
use crate::busy;
use fellor_ptp::usb::{is_ptp_interface, PtpInterface};
use fellor_ptp::{EventSource, TransportError};
use fellor_vfs::{BackendId, DeviceEvent, DeviceEventTx, DeviceKind, DeviceState, VfsError};
use futures::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, info, warn};

/// USB vendor ids of Android phone makers. A device from one of these
/// without an MTP/PTP interface is most likely in "charging only" mode.
pub const ANDROID_VENDOR_IDS: &[(u16, &str)] = &[
    (0x18d1, "Google"),
    (0x04e8, "Samsung"),
    (0x2717, "Xiaomi"),
    (0x2a70, "OnePlus"),
    (0x22b8, "Motorola"),
    (0x12d1, "Huawei"),
    (0x0fce, "Sony"),
    (0x1004, "LG"),
    (0x22d9, "Oppo"),
];

const RESCAN_INTERVAL: Duration = Duration::from_secs(5);
const BUSY_RETRY: Duration = Duration::from_secs(3);
const ERROR_RETRY: Duration = Duration::from_secs(5);
const LOCKED_POLL: Duration = Duration::from_secs(2);

/// Interface summary independent of nusb (unit-testable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfaceDesc {
    pub number: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Candidate {
    /// Has an MTP/PTP interface we can talk to.
    Ptp(PtpInterface),
    /// Known Android vendor without such an interface ("charging only").
    NeedsMtpMode,
}

pub fn android_vendor(vid: u16) -> Option<&'static str> {
    ANDROID_VENDOR_IDS.iter().find(|(v, _)| *v == vid).map(|(_, n)| *n)
}

pub const APPLE_VENDOR_ID: u16 = 0x05ac;

/// Decide whether a USB device is ours.
pub fn classify(vid: u16, device_class: u8, ifaces: &[IfaceDesc]) -> Option<Candidate> {
    // iPhone/iPad also expose a PTP "camera" interface for photo import. They are
    // served with full file access by fellor-ios over usbmuxd, so skip Apple here —
    // otherwise the same phone shows up twice and we steal the PTP interface.
    if vid == APPLE_VENDOR_ID {
        return None;
    }
    if let Some(i) = ifaces.iter().find(|i| is_ptp_interface(i.class, i.subclass, i.protocol, i.name.as_deref())) {
        return Some(Candidate::Ptp(PtpInterface {
            number: i.number,
            class: i.class,
            subclass: i.subclass,
            protocol: i.protocol,
            name: i.name.clone(),
        }));
    }
    android_vendor(vid)?;
    // Same vendors also make SSDs, keyboards, hubs and monitors: those are not phones.
    const NOT_A_PHONE: [u8; 5] = [0x03, 0x08, 0x09, 0x0E, 0x11];
    if NOT_A_PHONE.contains(&device_class) || ifaces.iter().any(|i| NOT_A_PHONE.contains(&i.class)) {
        return None;
    }
    Some(Candidate::NeedsMtpMode)
}

fn ifaces_of(info: &nusb::DeviceInfo) -> Vec<IfaceDesc> {
    info.interfaces()
        .map(|i| IfaceDesc {
            number: i.interface_number(),
            class: i.class(),
            subclass: i.subclass(),
            protocol: i.protocol(),
            name: i.interface_string().map(str::to_owned),
        })
        .collect()
}

pub fn classify_device(info: &nusb::DeviceInfo) -> Option<Candidate> {
    classify(info.vendor_id(), info.class(), &ifaces_of(info))
}

/// `mtp:<usb serial>` or `mtp:<vid>:<pid>:<bus>:<addr>` without a serial.
pub fn backend_id(info: &nusb::DeviceInfo) -> BackendId {
    match info.serial_number().map(str::trim) {
        Some(s) if !s.is_empty() => format!("mtp:{s}"),
        _ => format!("mtp:{:04x}:{:04x}:{}:{}", info.vendor_id(), info.product_id(), bus_of(info), info.device_address()),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn bus_of(info: &nusb::DeviceInfo) -> String {
    info.bus_id().to_owned()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn bus_of(_info: &nusb::DeviceInfo) -> String {
    "0".into()
}

fn initial_kind(info: &nusb::DeviceInfo, c: &Candidate) -> DeviceKind {
    match c {
        Candidate::NeedsMtpMode => DeviceKind::Android,
        Candidate::Ptp(i) if i.is_named_mtp() || android_vendor(info.vendor_id()).is_some() => DeviceKind::Android,
        Candidate::Ptp(_) => DeviceKind::Camera,
    }
}

fn initial_name(info: &nusb::DeviceInfo, kind: DeviceKind) -> String {
    if let Some(p) = info.product_string().map(str::trim).filter(|p| !p.is_empty()) {
        return p.to_owned();
    }
    match (android_vendor(info.vendor_id()), kind) {
        (Some(v), _) => format!("{v} Android"),
        (None, DeviceKind::Camera) => "Камера".into(),
        _ => "Android".into(),
    }
}

struct Tracked {
    id: BackendId,
    backend: MtpBackend,
    candidate: Candidate,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

pub fn spawn(tx: DeviceEventTx) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(tx))
}

async fn run(tx: DeviceEventTx) {
    // Subscribe before enumerating so nothing slips between the two.
    let mut watch = match nusb::watch_devices() {
        Ok(w) => Some(w),
        Err(e) => {
            warn!("USB hotplug unavailable ({e}); falling back to polling");
            None
        }
    };
    let mut known: HashMap<nusb::DeviceId, Tracked> = HashMap::new();
    rescan(&mut known, &tx).await;
    let mut tick = tokio::time::interval(RESCAN_INTERVAL);
    tick.tick().await;
    loop {
        if tx.is_closed() {
            return;
        }
        tokio::select! {
            ev = async {
                match watch.as_mut() {
                    Some(w) => w.next().await,
                    None => std::future::pending().await,
                }
            } => match ev {
                Some(nusb::hotplug::HotplugEvent::Connected(info)) => consider(&mut known, info, &tx),
                Some(nusb::hotplug::HotplugEvent::Disconnected(id)) => {
                    if let Some(t) = known.remove(&id) {
                        detached(t, &tx);
                    }
                }
                None => watch = None,
            },
            _ = tick.tick() => rescan(&mut known, &tx).await,
        }
    }
}

fn detached(t: Tracked, tx: &DeviceEventTx) {
    info!(id = %t.id, "MTP device detached");
    t.backend.detach(DeviceState::Error("устройство отключено".into()));
    let _ = tx.send(DeviceEvent::Detached(t.id.clone()));
}

async fn rescan(known: &mut HashMap<nusb::DeviceId, Tracked>, tx: &DeviceEventTx) {
    let devices: Vec<nusb::DeviceInfo> = match nusb::list_devices().await {
        Ok(it) => it.collect(),
        Err(e) => {
            warn!("USB enumeration failed: {e}");
            return;
        }
    };
    let present: std::collections::HashSet<nusb::DeviceId> = devices.iter().map(|d| d.id()).collect();
    let gone: Vec<nusb::DeviceId> = known.keys().filter(|k| !present.contains(k)).copied().collect();
    for id in gone {
        if let Some(t) = known.remove(&id) {
            detached(t, tx);
        }
    }
    for d in devices {
        consider(known, d, tx);
    }
}

fn consider(known: &mut HashMap<nusb::DeviceId, Tracked>, info: nusb::DeviceInfo, tx: &DeviceEventTx) {
    let dev_id = info.id();
    let Some(candidate) = classify_device(&info) else {
        // A tracked "charging only" phone that changed into something else.
        if let Some(t) = known.remove(&dev_id) {
            detached(t, tx);
        }
        return;
    };
    if let Some(t) = known.get_mut(&dev_id) {
        if t.candidate == candidate {
            return;
        }
        // Same device node, new interfaces (mode switch without re-enumeration).
        debug!(id = %t.id, ?candidate, "USB interfaces changed");
        if let Some(old) = t.task.take() {
            old.abort();
        }
        t.candidate = candidate.clone();
        start(t, info, tx);
        let _ = tx.send(DeviceEvent::Updated(t.backend.as_ref()));
        return;
    }
    let id = backend_id(&info);
    let kind = initial_kind(&info, &candidate);
    let backend = MtpBackend::new(id.clone(), initial_name(&info, kind), kind, DeviceState::Connecting);
    backend.set_device_tx(tx.clone());
    info!(%id, vid = format_args!("{:04x}", info.vendor_id()), pid = format_args!("{:04x}", info.product_id()), ?candidate, "MTP/PTP candidate");
    let mut t = Tracked { id, backend, candidate, task: None };
    start(&mut t, info, tx);
    let _ = tx.send(DeviceEvent::Attached(t.backend.as_ref()));
    known.insert(dev_id, t);
}

fn start(t: &mut Tracked, info: nusb::DeviceInfo, tx: &DeviceEventTx) {
    match &t.candidate {
        Candidate::NeedsMtpMode => {
            t.backend.detach(DeviceState::NeedsMtpMode);
        }
        Candidate::Ptp(iface) => {
            t.backend.set_state(DeviceState::Connecting);
            t.task = Some(tokio::spawn(device_task(info, iface.clone(), t.backend.clone(), tx.clone())));
        }
    }
}

async fn device_task(info: nusb::DeviceInfo, iface: PtpInterface, backend: MtpBackend, tx: DeviceEventTx) {
    let updated = |b: &MtpBackend| {
        let _ = tx.send(DeviceEvent::Updated(b.as_ref()));
    };
    loop {
        match fellor_ptp::usb::open(&info, iface.number).await {
            Err(TransportError::Busy) => {
                let by = busy::detect_holder(iface.is_still_image_class()).await;
                debug!(?by, "MTP interface busy");
                if backend.set_state(DeviceState::Busy { by }) {
                    updated(&backend);
                }
                tokio::time::sleep(BUSY_RETRY).await;
            }
            Err(TransportError::Disconnected) => return,
            Err(e) => {
                warn!("cannot open MTP interface: {e}");
                if backend.set_state(DeviceState::Error(format!("USB: {e}"))) {
                    updated(&backend);
                }
                tokio::time::sleep(ERROR_RETRY).await;
            }
            Ok((transport, events)) => {
                let events = events.map(|e| Box::new(e) as Box<dyn EventSource>);
                match backend.attach(Box::new(transport), events).await {
                    Ok(state) => {
                        info!(id = %backend.info_id(), ?state, "MTP device ready");
                        updated(&backend);
                        break;
                    }
                    Err(VfsError::Disconnected) => return,
                    Err(e) => {
                        warn!("MTP session setup failed: {e}");
                        backend.detach(DeviceState::Error(e.to_string()));
                        updated(&backend);
                        tokio::time::sleep(ERROR_RETRY).await;
                    }
                }
            }
        }
    }
    // Supervise: a locked phone reports zero storages until unlocked.
    loop {
        tokio::time::sleep(LOCKED_POLL).await;
        if backend.state() == DeviceState::Locked {
            let st = backend.refresh_storages().await;
            if st != DeviceState::Locked {
                updated(&backend);
            }
        }
    }
}

impl MtpBackend {
    fn info_id(&self) -> BackendId {
        fellor_vfs::Backend::id(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i(number: u8, class: u8, subclass: u8, protocol: u8, name: Option<&str>) -> IfaceDesc {
        IfaceDesc { number, class, subclass, protocol, name: name.map(str::to_owned) }
    }

    #[test]
    fn android_mtp_mode_vendor_interface() {
        // Pixel in "File transfer": vendor-specific + "MTP" string, plus ADB.
        let c = classify(0x18d1, 0, &[i(0, 0xFF, 0xFF, 0, Some("MTP")), i(1, 0xFF, 0x42, 1, Some("ADB Interface"))]);
        assert!(matches!(c, Some(Candidate::Ptp(p)) if p.number == 0 && p.is_named_mtp() && !p.is_still_image_class()));
    }

    #[test]
    fn camera_still_image_class() {
        let c = classify(0x04a9, 0, &[i(0, 0x06, 0x01, 0x01, None)]);
        assert!(matches!(c, Some(Candidate::Ptp(p)) if p.is_still_image_class()));
    }

    #[test]
    fn charging_only_android() {
        assert_eq!(classify(0x04e8, 0, &[]), Some(Candidate::NeedsMtpMode));
        assert_eq!(classify(0x18d1, 0, &[i(0, 0xFF, 0x42, 1, Some("ADB Interface"))]), Some(Candidate::NeedsMtpMode));
    }

    #[test]
    fn same_vendor_non_phones_ignored() {
        // Samsung T7 SSD (mass storage), Samsung keyboard (HID), Google hub.
        assert_eq!(classify(0x04e8, 0, &[i(0, 0x08, 0x06, 0x50, None)]), None);
        assert_eq!(classify(0x04e8, 0, &[i(0, 0x03, 0x01, 0x01, None)]), None);
        assert_eq!(classify(0x18d1, 0x09, &[]), None);
        // Unknown vendor without PTP interface.
        assert_eq!(classify(0x05ac, 0, &[i(0, 0xFF, 0xFE, 2, None)]), None);
        // iPhone's PTP interface is left to fellor-ios.
        assert_eq!(classify(0x05ac, 0, &[i(0, 0x06, 0x01, 0x01, None)]), None);
        // Vendor-specific interface not named MTP.
        assert_eq!(classify(0x1234, 0, &[i(0, 0xFF, 0xFF, 0, Some("Debug"))]), None);
    }
}
