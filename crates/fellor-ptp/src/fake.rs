//! In-memory MTP/PTP device simulator implementing [`Transport`] at the USB
//! packet level. It reassembles OUT containers exactly like a device would
//! (short packet / ZLP rules) and packetises its replies into
//! `max_packet_size` packets, so host-side framing bugs fail loudly.
//!
//! Protocol violations the host commits are recorded in
//! [`FakeDevice::violations`]; tests assert that list is empty.

use crate::codes::{class_request, event, format, op, prop, resp};
use crate::container::{encode_params, ContainerType, Header, ParamContainer, HEADER_LEN, LEN_UNKNOWN};
use crate::dataset::{decode_prop_list, encode_prop_list, DeviceInfo, ObjectInfo, PropListElement, PropValue, StorageInfo};
use crate::error::TransportError;
use crate::transport::{EventSource, Pipe, Transport};
use crate::wire::{Reader, Writer};
use async_trait::async_trait;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct FakeConfig {
    pub mps: usize,
    /// Terminate data phases that are a multiple of mps with a ZLP.
    pub send_zlp: bool,
    /// Always use the 0xFFFFFFFF length in data containers (as Android does
    /// for >= 4 GiB objects) to exercise that path with small payloads.
    pub always_ffff_len: bool,
    pub prop_list: bool,
    pub send_prop_list: bool,
    pub partial: bool,
    pub partial64: bool,
    pub move_object: bool,
    pub android: bool,
    /// Report zero storages (screen locked).
    pub locked: bool,
    pub storages: Vec<(u32, String)>,
    pub manufacturer: String,
    pub model: String,
    pub serial: String,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            mps: 512,
            send_zlp: true,
            always_ffff_len: false,
            prop_list: true,
            send_prop_list: true,
            partial: true,
            partial64: true,
            move_object: true,
            android: true,
            locked: false,
            storages: vec![(0x0001_0001, "Internal shared storage".into())],
            manufacturer: "Google".into(),
            model: "Pixel 8".into(),
            serial: "FAKE0001".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FakeObject {
    pub handle: u32,
    pub storage: u32,
    /// 0 = storage root.
    pub parent: u32,
    pub format: u16,
    pub name: String,
    pub data: Vec<u8>,
    /// Reported size override (lets tests claim huge objects without RAM).
    pub reported_size: Option<u64>,
    pub modified: String,
    pub thumb: Option<Vec<u8>>,
}

impl FakeObject {
    pub fn size(&self) -> u64 {
        self.reported_size.unwrap_or(self.data.len() as u64)
    }
}

#[derive(Default)]
struct OutState {
    buf: Vec<u8>,
    expect_zlp: bool,
}

struct PendingSend {
    handle: u32,
    size: u64,
}

pub struct FakeState {
    pub cfg: FakeConfig,
    pub objects: BTreeMap<u32, FakeObject>,
    next_handle: u32,
    session_open: bool,
    last_tid: Option<u32>,
    in_queue: VecDeque<Vec<u8>>,
    out: OutState,
    pending_cmd: Option<ParamContainer>,
    pending_send: Option<PendingSend>,
    /// Remaining commands to answer with Device_Busy.
    pub busy_responses: u32,
    /// Ops executed, in order.
    pub op_log: Vec<u16>,
    pub violations: Vec<String>,
    pub cancels: u32,
    pub resets: u32,
    event_tx: mpsc::UnboundedSender<Vec<u8>>,
}

#[derive(Clone)]
pub struct FakeDevice {
    state: Arc<Mutex<FakeState>>,
    events: Arc<Mutex<Option<mpsc::UnboundedReceiver<Vec<u8>>>>>,
}

impl FakeDevice {
    pub fn new(cfg: FakeConfig) -> FakeDevice {
        let (tx, rx) = mpsc::unbounded_channel();
        FakeDevice {
            state: Arc::new(Mutex::new(FakeState {
                cfg,
                objects: BTreeMap::new(),
                next_handle: 1,
                session_open: false,
                last_tid: None,
                in_queue: VecDeque::new(),
                out: OutState::default(),
                pending_cmd: None,
                pending_send: None,
                busy_responses: 0,
                op_log: Vec::new(),
                violations: Vec::new(),
                cancels: 0,
                resets: 0,
                event_tx: tx,
            })),
            events: Arc::new(Mutex::new(Some(rx))),
        }
    }

    pub fn transport(&self) -> FakeTransport {
        FakeTransport { dev: self.clone() }
    }

    /// The interrupt pipe (can be taken once).
    pub fn events(&self) -> Option<FakeEvents> {
        self.events.lock().unwrap().take().map(|rx| FakeEvents { rx })
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    pub fn add_dir(&self, storage: u32, parent: u32, name: &str) -> u32 {
        self.add(storage, parent, name, format::ASSOCIATION, Vec::new())
    }

    pub fn add_file(&self, storage: u32, parent: u32, name: &str, data: &[u8]) -> u32 {
        self.add(storage, parent, name, format::from_name(name), data.to_vec())
    }

    fn add(&self, storage: u32, parent: u32, name: &str, fmt: u16, data: Vec<u8>) -> u32 {
        self.with(|s| {
            let h = s.alloc_handle();
            s.objects.insert(
                h,
                FakeObject {
                    handle: h,
                    storage,
                    parent,
                    format: fmt,
                    name: name.into(),
                    data,
                    reported_size: None,
                    modified: "20240131T120000".into(),
                    thumb: None,
                },
            );
            h
        })
    }

    pub fn object(&self, handle: u32) -> Option<FakeObject> {
        self.with(|s| s.objects.get(&handle).cloned())
    }

    pub fn find(&self, parent: u32, name: &str) -> Option<FakeObject> {
        self.with(|s| s.objects.values().find(|o| o.parent == parent && o.name == name).cloned())
    }

    pub fn violations(&self) -> Vec<String> {
        self.with(|s| s.violations.clone())
    }

    pub fn op_log(&self) -> Vec<u16> {
        self.with(|s| s.op_log.clone())
    }

    pub fn clear_op_log(&self) {
        self.with(|s| s.op_log.clear())
    }

    /// Simulate a device-side change notification on the interrupt pipe.
    pub fn push_event(&self, code: u16, params: &[u32]) {
        self.with(|s| {
            let _ = s.event_tx.send(encode_params(ContainerType::Event, code, 0xFFFF_FFFF, params));
        })
    }

    /// Device-side creation of a file plus the ObjectAdded event.
    pub fn device_creates_file(&self, storage: u32, parent: u32, name: &str, data: &[u8]) -> u32 {
        let h = self.add_file(storage, parent, name, data);
        self.push_event(event::OBJECT_ADDED, &[h]);
        h
    }
}

impl FakeState {
    fn alloc_handle(&mut self) -> u32 {
        let h = self.next_handle;
        self.next_handle += 1;
        h
    }

    fn violation(&mut self, msg: impl Into<String>) {
        let m = msg.into();
        tracing::error!("fake device: host protocol violation: {m}");
        self.violations.push(m);
    }

    fn queue_message(&mut self, bytes: Vec<u8>) {
        let mps = self.cfg.mps;
        let len = bytes.len();
        for c in bytes.chunks(mps) {
            self.in_queue.push_back(c.to_vec());
        }
        if len % mps == 0 && self.cfg.send_zlp {
            self.in_queue.push_back(Vec::new());
        }
    }

    fn queue_data(&mut self, code: u16, tid: u32, payload: &[u8]) {
        let mut h = Header::data(code, tid, payload.len() as u64);
        if self.cfg.always_ffff_len {
            h.length = LEN_UNKNOWN;
        }
        let mut v = Vec::with_capacity(HEADER_LEN + payload.len());
        v.extend_from_slice(&h.encode());
        v.extend_from_slice(payload);
        self.queue_message(v);
    }

    fn queue_response(&mut self, code: u16, tid: u32, params: &[u32]) {
        self.queue_message(encode_params(ContainerType::Response, code, tid, params));
    }

    fn on_out(&mut self, data: Vec<u8>) {
        let mps = self.cfg.mps;
        if data.is_empty() {
            if self.out.expect_zlp {
                self.out.expect_zlp = false;
                return;
            }
            // ZLP terminates an unknown-length (0xFFFFFFFF) container.
            if self.out.buf.len() >= HEADER_LEN && self.out.buf[0..4] == [0xFF; 4] {
                let c = std::mem::take(&mut self.out.buf);
                self.on_container(c);
                return;
            }
            self.violation("unexpected zero-length packet");
            return;
        }
        if self.out.expect_zlp {
            self.violation("container length was a multiple of wMaxPacketSize but no ZLP followed");
            self.out.expect_zlp = false;
        }
        let short = data.len() % mps != 0;
        self.out.buf.extend_from_slice(&data);
        if self.out.buf.len() < HEADER_LEN {
            if short {
                self.violation("short packet before container header complete");
                self.out.buf.clear();
            }
            return;
        }
        let declared = u32::from_le_bytes(self.out.buf[0..4].try_into().unwrap());
        if declared == LEN_UNKNOWN {
            if short {
                let c = std::mem::take(&mut self.out.buf);
                self.on_container(c);
            }
            return;
        }
        let declared = declared as usize;
        if self.out.buf.len() > declared {
            self.violation(format!("transfer overran container ({} > {declared})", self.out.buf.len()));
            self.out.buf.clear();
            return;
        }
        if self.out.buf.len() == declared {
            let c = std::mem::take(&mut self.out.buf);
            if declared % mps == 0 {
                self.out.expect_zlp = true;
            }
            self.on_container(c);
        } else if short {
            self.violation(format!(
                "short packet terminated the transfer mid-container ({} of {declared} bytes)",
                self.out.buf.len()
            ));
            self.out.buf.clear();
        }
    }

    fn on_container(&mut self, c: Vec<u8>) {
        let h = match Header::decode(&c) {
            Ok(h) => h,
            Err(e) => return self.violation(format!("bad container: {e}")),
        };
        match h.kind {
            ContainerType::Command => {
                let cmd = ParamContainer::decode(&c).unwrap();
                if let Some(cmd) = self.pending_cmd.take() {
                    self.violation(format!("new command while {} awaited its data phase", op::name(cmd.code)));
                }
                // Transaction id discipline.
                if cmd.code == op::OPEN_SESSION || (cmd.code == op::GET_DEVICE_INFO && !self.session_open) {
                    if cmd.tid != 0 {
                        self.violation(format!("{} outside a session must use tid 0, got {}", op::name(cmd.code), cmd.tid));
                    }
                } else if let Some(last) = self.last_tid {
                    if cmd.tid <= last {
                        self.violation(format!("transaction id not increasing: {} after {last}", cmd.tid));
                    }
                }
                if self.session_open && cmd.code != op::OPEN_SESSION {
                    self.last_tid = Some(cmd.tid);
                }
                if matches!(cmd.code, op::SEND_OBJECT_INFO | op::SEND_OBJECT | op::SET_OBJECT_PROP_VALUE | op::SEND_OBJECT_PROP_LIST) {
                    self.pending_cmd = Some(cmd);
                } else {
                    self.execute(cmd, None);
                }
            }
            ContainerType::Data => {
                let Some(cmd) = self.pending_cmd.take() else {
                    return self.violation("data container without a pending command");
                };
                if h.code != cmd.code || h.tid != cmd.tid {
                    self.violation(format!("data container code/tid mismatch: {:04X}/{} vs {:04X}/{}", h.code, h.tid, cmd.code, cmd.tid));
                }
                let payload = c[HEADER_LEN..].to_vec();
                if let Some(pl) = h.payload_len() {
                    if pl != payload.len() as u64 {
                        self.violation("data container length mismatch");
                    }
                }
                self.execute(cmd, Some(payload));
            }
            k => self.violation(format!("host sent a {k:?} container")),
        }
    }

    fn device_info(&self) -> DeviceInfo {
        let mut ops = vec![
            op::GET_DEVICE_INFO,
            op::OPEN_SESSION,
            op::CLOSE_SESSION,
            op::GET_STORAGE_IDS,
            op::GET_STORAGE_INFO,
            op::GET_OBJECT_HANDLES,
            op::GET_OBJECT_INFO,
            op::GET_OBJECT,
            op::GET_THUMB,
            op::DELETE_OBJECT,
            op::SEND_OBJECT_INFO,
            op::SEND_OBJECT,
            op::GET_OBJECT_PROPS_SUPPORTED,
            op::GET_OBJECT_PROP_VALUE,
            op::SET_OBJECT_PROP_VALUE,
        ];
        if self.cfg.partial {
            ops.push(op::GET_PARTIAL_OBJECT);
        }
        if self.cfg.move_object {
            ops.push(op::MOVE_OBJECT);
        }
        if self.cfg.prop_list {
            ops.push(op::GET_OBJECT_PROP_LIST);
        }
        if self.cfg.send_prop_list {
            ops.push(op::SEND_OBJECT_PROP_LIST);
        }
        if self.cfg.partial64 {
            ops.push(op::ANDROID_GET_PARTIAL_OBJECT_64);
        }
        DeviceInfo {
            standard_version: 100,
            vendor_extension_id: if self.cfg.android { 6 } else { 0 },
            vendor_extension_version: 100,
            vendor_extension_desc: if self.cfg.android { "microsoft.com: 1.0; android.com: 1.0; ".into() } else { String::new() },
            functional_mode: 0,
            operations_supported: ops,
            events_supported: vec![event::OBJECT_ADDED, event::OBJECT_REMOVED, event::STORE_ADDED, event::STORE_REMOVED],
            device_properties_supported: vec![],
            capture_formats: vec![],
            playback_formats: vec![format::UNDEFINED, format::ASSOCIATION],
            manufacturer: self.cfg.manufacturer.clone(),
            model: self.cfg.model.clone(),
            device_version: "1.0".into(),
            serial_number: self.cfg.serial.clone(),
        }
    }

    fn prop_value(&self, o: &FakeObject, p: u16) -> Option<PropValue> {
        Some(match p {
            prop::STORAGE_ID => PropValue::U32(o.storage),
            prop::OBJECT_FORMAT => PropValue::U16(o.format),
            prop::PROTECTION_STATUS => PropValue::U16(0),
            prop::OBJECT_SIZE => PropValue::U64(o.size()),
            prop::OBJECT_FILE_NAME => PropValue::Str(o.name.clone()),
            prop::DATE_MODIFIED => PropValue::Str(o.modified.clone()),
            prop::PARENT_OBJECT => PropValue::U32(o.parent),
            prop::PERSISTENT_UID => PropValue::U128(o.handle as u128),
            prop::NAME => PropValue::Str(o.name.clone()),
            _ => return None,
        })
    }

    const ALL_PROPS: [u16; 9] = [
        prop::STORAGE_ID,
        prop::OBJECT_FORMAT,
        prop::PROTECTION_STATUS,
        prop::OBJECT_SIZE,
        prop::OBJECT_FILE_NAME,
        prop::DATE_MODIFIED,
        prop::PARENT_OBJECT,
        prop::PERSISTENT_UID,
        prop::NAME,
    ];

    fn delete_recursive(&mut self, h: u32) {
        let kids: Vec<u32> = self.objects.values().filter(|o| o.parent == h).map(|o| o.handle).collect();
        for k in kids {
            self.delete_recursive(k);
        }
        self.objects.remove(&h);
    }

    fn is_dir(&self, h: u32) -> bool {
        self.objects.get(&h).is_some_and(|o| o.format == format::ASSOCIATION)
    }

    fn execute(&mut self, cmd: ParamContainer, data: Option<Vec<u8>>) {
        self.op_log.push(cmd.code);
        let tid = cmd.tid;
        let p = |i: usize| cmd.param(i);
        if self.busy_responses > 0 && !matches!(cmd.code, op::OPEN_SESSION | op::GET_DEVICE_INFO | op::CLOSE_SESSION) {
            self.busy_responses -= 1;
            return self.queue_response(resp::DEVICE_BUSY, tid, &[]);
        }
        if !self.session_open && !matches!(cmd.code, op::OPEN_SESSION | op::GET_DEVICE_INFO) {
            return self.queue_response(resp::SESSION_NOT_OPEN, tid, &[]);
        }
        let supported = self.device_info().operations_supported;
        if !supported.contains(&cmd.code) {
            return self.queue_response(resp::OPERATION_NOT_SUPPORTED, tid, &[]);
        }
        match cmd.code {
            op::GET_DEVICE_INFO => {
                let d = self.device_info().encode();
                self.queue_data(cmd.code, tid, &d);
                self.queue_response(resp::OK, tid, &[]);
            }
            op::OPEN_SESSION => {
                if self.session_open {
                    return self.queue_response(resp::SESSION_ALREADY_OPEN, tid, &[]);
                }
                self.session_open = true;
                self.last_tid = Some(0);
                self.queue_response(resp::OK, tid, &[]);
            }
            op::CLOSE_SESSION => {
                self.session_open = false;
                self.last_tid = None;
                self.queue_response(resp::OK, tid, &[]);
            }
            op::GET_STORAGE_IDS => {
                let ids: Vec<u32> = if self.cfg.locked { vec![] } else { self.cfg.storages.iter().map(|s| s.0).collect() };
                let mut w = Writer::new();
                w.u32_array(&ids);
                self.queue_data(cmd.code, tid, &w.buf);
                self.queue_response(resp::OK, tid, &[]);
            }
            op::GET_STORAGE_INFO => {
                let Some((_, name)) = self.cfg.storages.iter().find(|s| s.0 == p(0)).cloned() else {
                    return self.queue_response(resp::INVALID_STORAGE_ID, tid, &[]);
                };
                let used: u64 = self.objects.values().filter(|o| o.storage == p(0)).map(|o| o.size()).sum();
                let si = StorageInfo {
                    storage_type: 3,
                    filesystem_type: 2,
                    access_capability: 0,
                    max_capacity: 64 << 30,
                    free_space_bytes: (64u64 << 30) - used,
                    free_space_objects: 0xFFFF_FFFF,
                    description: name,
                    volume_label: String::new(),
                };
                self.queue_data(cmd.code, tid, &si.encode());
                self.queue_response(resp::OK, tid, &[]);
            }
            op::GET_OBJECT_HANDLES => {
                let (storage, parent) = (p(0), p(2));
                let parent = if parent == 0xFFFF_FFFF { 0 } else { parent };
                let hs: Vec<u32> = self
                    .objects
                    .values()
                    .filter(|o| (storage == 0xFFFF_FFFF || o.storage == storage) && o.parent == parent)
                    .map(|o| o.handle)
                    .collect();
                let mut w = Writer::new();
                w.u32_array(&hs);
                self.queue_data(cmd.code, tid, &w.buf);
                self.queue_response(resp::OK, tid, &[]);
            }
            op::GET_OBJECT_INFO => {
                let Some(o) = self.objects.get(&p(0)).cloned() else {
                    return self.queue_response(resp::INVALID_OBJECT_HANDLE, tid, &[]);
                };
                let oi = ObjectInfo {
                    storage_id: o.storage,
                    format: o.format,
                    compressed_size: o.size().min(0xFFFF_FFFF) as u32,
                    parent: o.parent,
                    association_type: if o.format == format::ASSOCIATION { 1 } else { 0 },
                    filename: o.name.clone(),
                    modification_date: o.modified.clone(),
                    ..Default::default()
                };
                self.queue_data(cmd.code, tid, &oi.encode());
                self.queue_response(resp::OK, tid, &[]);
            }
            op::GET_OBJECT => {
                let Some(o) = self.objects.get(&p(0)).cloned() else {
                    return self.queue_response(resp::INVALID_OBJECT_HANDLE, tid, &[]);
                };
                self.queue_data(cmd.code, tid, &o.data);
                self.queue_response(resp::OK, tid, &[]);
            }
            op::GET_PARTIAL_OBJECT | op::ANDROID_GET_PARTIAL_OBJECT_64 => {
                let Some(o) = self.objects.get(&p(0)).cloned() else {
                    return self.queue_response(resp::INVALID_OBJECT_HANDLE, tid, &[]);
                };
                let (off, len) = if cmd.code == op::GET_PARTIAL_OBJECT {
                    (p(1) as u64, p(2) as u64)
                } else {
                    (p(1) as u64 | (p(2) as u64) << 32, p(3) as u64)
                };
                let start = (off as usize).min(o.data.len());
                let end = (off.saturating_add(len) as usize).min(o.data.len());
                let slice = o.data[start..end].to_vec();
                self.queue_data(cmd.code, tid, &slice);
                self.queue_response(resp::OK, tid, &[slice.len() as u32]);
            }
            op::GET_THUMB => match self.objects.get(&p(0)).and_then(|o| o.thumb.clone()) {
                Some(t) => {
                    self.queue_data(cmd.code, tid, &t);
                    self.queue_response(resp::OK, tid, &[]);
                }
                None => self.queue_response(resp::NO_THUMBNAIL_PRESENT, tid, &[]),
            },
            op::DELETE_OBJECT => {
                if !self.objects.contains_key(&p(0)) {
                    return self.queue_response(resp::INVALID_OBJECT_HANDLE, tid, &[]);
                }
                self.delete_recursive(p(0));
                self.queue_response(resp::OK, tid, &[]);
            }
            op::MOVE_OBJECT => {
                let (h, storage, parent) = (p(0), p(1), p(2));
                let parent = if parent == 0xFFFF_FFFF { 0 } else { parent };
                if parent != 0 && !self.is_dir(parent) {
                    return self.queue_response(resp::INVALID_PARENT_OBJECT, tid, &[]);
                }
                let Some(o) = self.objects.get_mut(&h) else {
                    return self.queue_response(resp::INVALID_OBJECT_HANDLE, tid, &[]);
                };
                o.parent = parent;
                o.storage = storage;
                self.queue_response(resp::OK, tid, &[]);
            }
            op::GET_OBJECT_PROPS_SUPPORTED => {
                let mut w = Writer::new();
                w.u16_array(&Self::ALL_PROPS);
                self.queue_data(cmd.code, tid, &w.buf);
                self.queue_response(resp::OK, tid, &[]);
            }
            op::GET_OBJECT_PROP_VALUE => {
                let Some(o) = self.objects.get(&p(0)).cloned() else {
                    return self.queue_response(resp::INVALID_OBJECT_HANDLE, tid, &[]);
                };
                match self.prop_value(&o, p(1) as u16) {
                    Some(v) => {
                        self.queue_data(cmd.code, tid, &v.to_bytes());
                        self.queue_response(resp::OK, tid, &[]);
                    }
                    None => self.queue_response(resp::OBJECT_PROP_NOT_SUPPORTED, tid, &[]),
                }
            }
            op::SET_OBJECT_PROP_VALUE => {
                let data = data.unwrap_or_default();
                let Some(o) = self.objects.get_mut(&p(0)) else {
                    return self.queue_response(resp::INVALID_OBJECT_HANDLE, tid, &[]);
                };
                let mut r = Reader::new(&data);
                let code = match p(1) as u16 {
                    prop::OBJECT_FILE_NAME => match r.string() {
                        Ok(s) if !s.is_empty() => {
                            o.name = s;
                            resp::OK
                        }
                        _ => resp::INVALID_OBJECT_PROP_VALUE,
                    },
                    prop::DATE_MODIFIED => match r.string() {
                        Ok(s) => {
                            o.modified = s;
                            resp::OK
                        }
                        _ => resp::INVALID_OBJECT_PROP_VALUE,
                    },
                    _ => resp::OBJECT_PROP_NOT_SUPPORTED,
                };
                self.queue_response(code, tid, &[]);
            }
            op::GET_OBJECT_PROP_LIST => {
                let (handle, fmt, pcode, _group, depth) = (p(0), p(1), p(2), p(3), p(4));
                let targets: Vec<FakeObject> = match depth {
                    0 if handle == 0xFFFF_FFFF => self.objects.values().cloned().collect(),
                    0 => match self.objects.get(&handle) {
                        Some(o) => vec![o.clone()],
                        None => return self.queue_response(resp::INVALID_OBJECT_HANDLE, tid, &[]),
                    },
                    1 => {
                        if handle != 0 && !self.objects.contains_key(&handle) {
                            return self.queue_response(resp::INVALID_OBJECT_HANDLE, tid, &[]);
                        }
                        self.objects.values().filter(|o| o.parent == handle).cloned().collect()
                    }
                    _ => return self.queue_response(resp::SPECIFICATION_BY_DEPTH_UNSUPPORTED, tid, &[]),
                };
                let mut elems = Vec::new();
                for o in targets.iter().filter(|o| fmt == 0 || o.format as u32 == fmt) {
                    let props: Vec<u16> = if pcode == 0xFFFF_FFFF { Self::ALL_PROPS.to_vec() } else { vec![pcode as u16] };
                    for pc in props {
                        if let Some(v) = self.prop_value(o, pc) {
                            elems.push(PropListElement { handle: o.handle, prop: pc, value: v });
                        }
                    }
                }
                self.queue_data(cmd.code, tid, &encode_prop_list(&elems));
                self.queue_response(resp::OK, tid, &[]);
            }
            op::SEND_OBJECT_INFO | op::SEND_OBJECT_PROP_LIST => {
                let Some(data) = data else {
                    return self.queue_response(resp::INVALID_DATASET, tid, &[]);
                };
                let storage = p(0);
                let parent = if p(1) == 0xFFFF_FFFF { 0 } else { p(1) };
                if parent != 0 && !self.is_dir(parent) {
                    return self.queue_response(resp::INVALID_PARENT_OBJECT, tid, &[]);
                }
                let (name, fmt, size, modified) = if cmd.code == op::SEND_OBJECT_INFO {
                    let Ok(oi) = ObjectInfo::decode(&data) else {
                        return self.queue_response(resp::INVALID_DATASET, tid, &[]);
                    };
                    if oi.compressed_size == 0xFFFF_FFFF {
                        self.violation("SendObjectInfo used for an object >= 4 GiB although SendObjectPropList is available");
                    }
                    (oi.filename, oi.format, oi.compressed_size as u64, oi.modification_date)
                } else {
                    let Ok(props) = decode_prop_list(&data) else {
                        return self.queue_response(resp::INVALID_DATASET, tid, &[]);
                    };
                    let name = props.iter().find(|e| e.prop == prop::OBJECT_FILE_NAME).and_then(|e| e.value.as_str().map(str::to_owned));
                    let Some(name) = name else {
                        return self.queue_response(resp::INVALID_DATASET, tid, &[]);
                    };
                    let modified = props
                        .iter()
                        .find(|e| e.prop == prop::DATE_MODIFIED)
                        .and_then(|e| e.value.as_str().map(str::to_owned))
                        .unwrap_or_default();
                    (name, p(2) as u16, (p(3) as u64) << 32 | p(4) as u64, modified)
                };
                let h = self.alloc_handle();
                self.objects.insert(
                    h,
                    FakeObject {
                        handle: h,
                        storage,
                        parent,
                        format: fmt,
                        name,
                        data: Vec::new(),
                        reported_size: None,
                        modified,
                        thumb: None,
                    },
                );
                self.pending_send = (fmt != format::ASSOCIATION).then_some(PendingSend { handle: h, size });
                self.queue_response(resp::OK, tid, &[storage, if parent == 0 { 0xFFFF_FFFF } else { parent }, h]);
            }
            op::SEND_OBJECT => {
                let data = data.unwrap_or_default();
                let Some(ps) = self.pending_send.take() else {
                    return self.queue_response(resp::NO_VALID_OBJECT_INFO, tid, &[]);
                };
                if ps.size != data.len() as u64 {
                    self.violation(format!("SendObject sent {} bytes, announced {}", data.len(), ps.size));
                    return self.queue_response(resp::INCOMPLETE_TRANSFER, tid, &[]);
                }
                if let Some(o) = self.objects.get_mut(&ps.handle) {
                    o.data = data;
                }
                self.queue_response(resp::OK, tid, &[]);
            }
            _ => self.queue_response(resp::OPERATION_NOT_SUPPORTED, tid, &[]),
        }
    }
}

pub struct FakeTransport {
    dev: FakeDevice,
}

#[async_trait]
impl Transport for FakeTransport {
    fn max_packet_size(&self) -> usize {
        self.dev.with(|s| s.cfg.mps)
    }

    async fn bulk_out(&mut self, data: Vec<u8>, _timeout: Duration) -> Result<(), TransportError> {
        self.dev.with(|s| s.on_out(data));
        Ok(())
    }

    async fn bulk_in(&mut self, len: usize, _timeout: Duration) -> Result<Vec<u8>, TransportError> {
        self.dev.with(|s| {
            let mps = s.cfg.mps;
            if len == 0 || len % mps != 0 {
                s.violation(format!("IN transfer length {len} is not a multiple of wMaxPacketSize {mps}"));
            }
            if s.in_queue.is_empty() {
                return Err(TransportError::Timeout);
            }
            let mut out = Vec::new();
            while out.len() + mps <= len.max(mps) {
                let Some(p) = s.in_queue.pop_front() else { break };
                let short = p.len() < mps;
                out.extend_from_slice(&p);
                if short {
                    break;
                }
            }
            Ok(out)
        })
    }

    async fn clear_halt(&mut self, _pipe: Pipe) -> Result<(), TransportError> {
        Ok(())
    }

    async fn class_out(&mut self, request: u8, data: &[u8], _timeout: Duration) -> Result<(), TransportError> {
        self.dev.with(|s| match request {
            class_request::CANCEL => {
                if data.len() != 6 || data[0..2] != [0x01, 0x40] {
                    s.violation("malformed Cancel request");
                }
                s.cancels += 1;
                s.in_queue.clear();
                s.out = OutState::default();
                s.pending_cmd = None;
                s.pending_send = None;
            }
            class_request::DEVICE_RESET => {
                s.resets += 1;
                s.in_queue.clear();
                s.out = OutState::default();
                s.pending_cmd = None;
                s.pending_send = None;
                s.session_open = false;
                s.last_tid = None;
            }
            _ => s.violation(format!("unknown class request 0x{request:02X}")),
        });
        Ok(())
    }

    async fn class_in(&mut self, request: u8, _len: u16, _timeout: Duration) -> Result<Vec<u8>, TransportError> {
        if request == class_request::GET_DEVICE_STATUS {
            Ok(vec![0x04, 0x00, 0x01, 0x20])
        } else {
            Err(TransportError::Stall)
        }
    }
}

pub struct FakeEvents {
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

#[async_trait]
impl EventSource for FakeEvents {
    async fn next_event(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        Ok(self.rx.recv().await)
    }
}
