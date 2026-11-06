//! PTP session: one transaction at a time, monotonically increasing
//! transaction ids, streaming data phases, Device_Busy retries and
//! cancel/reset recovery.
//!
//! Lock the session with [`Session::lock`] to get a [`SessionGuard`]
//! (an owned guard). Simple operations are methods on the guard; streaming
//! operations ([`DataIn`], [`DataOut`]) consume the guard so the lock is held
//! for the whole data phase and released when the transaction completes.
//! Dropping a stream mid-transfer marks the session as poisoned; the next
//! `lock()` runs the cancel / reset recovery before handing out the guard.

use crate::codes::{class_request, datatype, op, resp};
use crate::container::{encode_command, ContainerType, Header, ParamContainer, Response, HEADER_LEN};
use crate::dataset::{decode_prop_list, encode_prop_list, DeviceInfo, DeviceStatus, ObjectInfo, PropListElement, PropValue, StorageInfo};
use crate::error::{PtpError, PtpResult, TransportError};
use crate::transport::{Pipe, Transport};
use crate::wire::{Reader, Writer};
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::{debug, warn};

pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
pub const DATA_TIMEOUT: Duration = Duration::from_secs(30);
pub const BUSY_RETRY_DELAY: Duration = Duration::from_millis(200);
pub const BUSY_RETRIES: u32 = 10;
/// Largest single bulk transfer we submit.
pub const MAX_TRANSFER: usize = 1024 * 1024;
/// Size of the first IN transfer of a transaction (header + start of data).
const FIRST_TRANSFER: usize = 64 * 1024;

/// Shared handle to a PTP session. Cheap to clone.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Mutex<SessionInner>>,
}

pub type SessionGuard = OwnedMutexGuard<SessionInner>;

pub struct SessionInner {
    transport: Box<dyn Transport>,
    next_tid: u32,
    session_id: Option<u32>,
    /// Transaction id of an interrupted transaction awaiting recovery.
    poisoned: Option<u32>,
    /// Bytes already received that belong to the next container (a device
    /// that omits the ZLP lets one IN transfer run into the response).
    carry: Vec<u8>,
    pub busy_delay: Duration,
    pub busy_retries: u32,
    pub command_timeout: Duration,
    pub data_timeout: Duration,
}

impl Session {
    pub fn new(transport: Box<dyn Transport>) -> Session {
        Session {
            inner: Arc::new(Mutex::new(SessionInner {
                transport,
                next_tid: 1,
                session_id: None,
                poisoned: None,
                carry: Vec::new(),
                busy_delay: BUSY_RETRY_DELAY,
                busy_retries: BUSY_RETRIES,
                command_timeout: COMMAND_TIMEOUT,
                data_timeout: DATA_TIMEOUT,
            })),
        }
    }

    /// Acquire exclusive use of the session, recovering first from any
    /// transaction that was interrupted.
    pub async fn lock(&self) -> SessionGuard {
        let mut g = self.inner.clone().lock_owned().await;
        if g.poisoned.is_some() {
            if let Err(e) = g.recover().await {
                warn!("PTP recovery failed: {e}");
            }
        }
        g
    }
}

fn round_up(n: usize, mps: usize) -> usize {
    n.div_ceil(mps).max(1) * mps
}

impl SessionInner {
    pub fn is_open(&self) -> bool {
        self.session_id.is_some()
    }

    pub fn max_packet_size(&self) -> usize {
        self.transport.max_packet_size().max(1)
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }

    fn alloc_tid(&mut self) -> u32 {
        if self.session_id.is_none() {
            return 0;
        }
        let t = self.next_tid;
        self.next_tid = if t >= 0xFFFF_FFFE { 1 } else { t + 1 };
        t
    }

    // ------------------------------------------------------------- low level

    async fn send_command(&mut self, code: u16, tid: u32, params: &[u32]) -> PtpResult<()> {
        let bytes = encode_command(code, tid, params);
        let t = self.command_timeout;
        self.send_container(bytes, t).await
    }

    /// Send a complete container (command or small data container), split
    /// into mps-aligned transfers, terminated by a ZLP when its length is a
    /// multiple of the max packet size.
    async fn send_container(&mut self, bytes: Vec<u8>, timeout: Duration) -> PtpResult<()> {
        let mps = self.max_packet_size();
        let chunk = (MAX_TRANSFER / mps).max(1) * mps;
        let needs_zlp = bytes.len() % mps == 0;
        if bytes.len() <= chunk {
            self.transport.bulk_out(bytes, timeout).await?;
        } else {
            for c in bytes.chunks(chunk) {
                self.transport.bulk_out(c.to_vec(), timeout).await?;
            }
        }
        if needs_zlp {
            self.transport.bulk_out(Vec::new(), timeout).await?;
        }
        Ok(())
    }

    /// Read the first transfer of the next container, skipping ZLPs left
    /// over from a previous data phase. Returns the transfer bytes and
    /// whether it was a short transfer (container complete).
    async fn read_first(&mut self, req: usize, timeout: Duration) -> PtpResult<(Vec<u8>, bool)> {
        let mps = self.max_packet_size();
        let req = round_up(req, mps);
        if !self.carry.is_empty() {
            return Ok((std::mem::take(&mut self.carry), true));
        }
        for _ in 0..4 {
            let buf = self.transport.bulk_in(req, timeout).await?;
            if buf.is_empty() {
                continue; // ZLP terminating the previous data phase
            }
            let short = buf.len() < req;
            return Ok((buf, short));
        }
        Err(PtpError::protocol("only zero-length packets received"))
    }

    async fn read_response(&mut self, op_code: u16, tid: u32, timeout: Duration) -> PtpResult<Response> {
        for _ in 0..8 {
            let (buf, short) = self.read_first(self.max_packet_size(), timeout).await?;
            let h = Header::decode(&buf)?;
            match h.kind {
                ContainerType::Response if h.tid == tid => return ParamContainer::decode(&buf),
                ContainerType::Response => {
                    warn!(op = op::name(op_code), expected = tid, got = h.tid, "stale response skipped");
                }
                _ => {
                    warn!(op = op::name(op_code), kind = ?h.kind, "unexpected container while waiting for response");
                    if !short {
                        self.drain_until_short(timeout).await?;
                    }
                }
            }
        }
        Err(PtpError::protocol(format!("no response for {}", op::name(op_code))))
    }

    async fn drain_until_short(&mut self, timeout: Duration) -> PtpResult<()> {
        let mps = self.max_packet_size();
        let req = round_up(MAX_TRANSFER, mps);
        loop {
            let b = self.transport.bulk_in(req, timeout).await?;
            if b.len() < req {
                return Ok(());
            }
        }
    }

    fn check(op_code: u16, r: Response) -> PtpResult<Response> {
        if r.code == resp::OK {
            Ok(r)
        } else {
            Err(PtpError::Response { op: op_code, code: r.code, params: r.params })
        }
    }

    /// One transaction with an optional outgoing data phase and a buffered
    /// incoming data phase. Retries on Device_Busy.
    pub async fn transact(&mut self, op_code: u16, params: &[u32], out: Option<&[u8]>) -> PtpResult<(Vec<u8>, Response)> {
        let mut attempt = 0;
        loop {
            let tid = self.alloc_tid();
            match self.transact_once(op_code, tid, params, out).await {
                Ok((_, r)) if r.code == resp::DEVICE_BUSY && attempt < self.busy_retries => {
                    attempt += 1;
                    debug!(op = op::name(op_code), attempt, "Device_Busy, retrying");
                    tokio::time::sleep(self.busy_delay).await;
                }
                Ok((data, r)) => return Ok((data, Self::check(op_code, r)?)),
                Err(e) => {
                    if matches!(e, PtpError::Transport(_) | PtpError::Protocol(_)) {
                        self.poisoned = Some(tid);
                    }
                    return Err(e);
                }
            }
        }
    }

    async fn transact_once(&mut self, op_code: u16, tid: u32, params: &[u32], out: Option<&[u8]>) -> PtpResult<(Vec<u8>, Response)> {
        self.send_command(op_code, tid, params).await?;
        if let Some(payload) = out {
            let mut c = Vec::with_capacity(HEADER_LEN + payload.len());
            c.extend_from_slice(&Header::data(op_code, tid, payload.len() as u64).encode());
            c.extend_from_slice(payload);
            let t = self.data_timeout;
            self.send_container(c, t).await?;
        }
        let ct = self.command_timeout;
        let (first, short) = self.read_first(FIRST_TRANSFER, ct).await?;
        let h = Header::decode(&first)?;
        match h.kind {
            ContainerType::Response => {
                if h.tid != tid {
                    return Err(PtpError::protocol(format!("response tid {} != {tid}", h.tid)));
                }
                Ok((Vec::new(), ParamContainer::decode(&first)?))
            }
            ContainerType::Data => {
                if h.tid != tid {
                    return Err(PtpError::protocol(format!("data tid {} != {tid}", h.tid)));
                }
                let data = self.collect_data(h, first, short).await?;
                let dt = self.data_timeout;
                let r = self.read_response(op_code, tid, dt).await?;
                Ok((data, r))
            }
            k => Err(PtpError::protocol(format!("unexpected {k:?} container"))),
        }
    }

    /// Buffer a whole (small) incoming data phase.
    async fn collect_data(&mut self, h: Header, first: Vec<u8>, mut short: bool) -> PtpResult<Vec<u8>> {
        let mps = self.max_packet_size();
        let total = h.payload_len();
        let mut data = first[HEADER_LEN.min(first.len())..].to_vec();
        loop {
            if let Some(t) = total {
                if data.len() as u64 >= t {
                    self.carry = data.split_off(t as usize);
                    return Ok(data);
                }
            }
            if short {
                if let Some(t) = total {
                    return Err(PtpError::protocol(format!("data phase ended early: {} of {t} bytes", data.len())));
                }
                return Ok(data);
            }
            let want = match total {
                Some(t) => (t - data.len() as u64).min(MAX_TRANSFER as u64) as usize,
                None => MAX_TRANSFER,
            };
            let req = round_up(want, mps);
            let b = self.transport.bulk_in(req, self.data_timeout).await?;
            short = b.len() < req;
            data.extend_from_slice(&b);
        }
    }

    // -------------------------------------------------------------- recovery

    /// PTP still-image class recovery: Cancel request → drain → poll
    /// GetDeviceStatus → clear halts; escalate to Device Reset and reopen
    /// the session if the device does not settle.
    pub async fn recover(&mut self) -> PtpResult<()> {
        let tid = self.poisoned.take().unwrap_or(0);
        self.carry.clear();
        warn!(tid, "recovering PTP session after interrupted transaction");
        let mut w = Writer::new();
        w.u16(0x4001).u32(tid);
        let cancel = self.transport.class_out(class_request::CANCEL, &w.buf, self.command_timeout).await;
        if let Err(TransportError::Disconnected) = cancel {
            return Err(TransportError::Disconnected.into());
        }
        // Drain whatever the device still had queued for us.
        let mps = self.max_packet_size();
        for _ in 0..64 {
            match self.transport.bulk_in(round_up(MAX_TRANSFER, mps), Duration::from_millis(300)).await {
                Ok(b) if !b.is_empty() => continue,
                Ok(_) => continue,
                Err(TransportError::Stall) => {
                    let _ = self.transport.clear_halt(Pipe::BulkIn).await;
                    break;
                }
                Err(_) => break,
            }
        }
        if self.wait_status_ok().await {
            return Ok(());
        }
        warn!("device did not settle after cancel; issuing Device Reset");
        let _ = self.transport.class_out(class_request::DEVICE_RESET, &[], self.command_timeout).await;
        let _ = self.transport.clear_halt(Pipe::BulkIn).await;
        let _ = self.transport.clear_halt(Pipe::BulkOut).await;
        if let Some(id) = self.session_id.take() {
            self.open_session(id).await?;
        }
        Ok(())
    }

    /// Poll GetDeviceStatus until OK. Returns false if the device stays busy.
    async fn wait_status_ok(&mut self) -> bool {
        for _ in 0..20 {
            match self.get_device_status().await {
                Ok(st) if st.code == resp::OK => return true,
                Ok(st) => {
                    // Params list endpoints that are halted.
                    for ep in &st.params {
                        let pipe = if ep & 0x80 != 0 { Pipe::BulkIn } else { Pipe::BulkOut };
                        let _ = self.transport.clear_halt(pipe).await;
                    }
                    if st.code != resp::DEVICE_BUSY {
                        // e.g. Transaction_Cancelled: halts cleared, device is idle.
                        return true;
                    }
                }
                Err(_) => {
                    // Request unsupported/stalled: clear both pipes and hope.
                    let _ = self.transport.clear_halt(Pipe::BulkIn).await;
                    let _ = self.transport.clear_halt(Pipe::BulkOut).await;
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    pub async fn get_device_status(&mut self) -> PtpResult<DeviceStatus> {
        let b = self.transport.class_in(class_request::GET_DEVICE_STATUS, 32, self.command_timeout).await?;
        DeviceStatus::decode(&b)
    }

    // ------------------------------------------------------------ operations

    pub async fn get_device_info(&mut self) -> PtpResult<DeviceInfo> {
        let (d, _) = self.transact(op::GET_DEVICE_INFO, &[], None).await?;
        DeviceInfo::decode(&d)
    }

    /// OpenSession. `Session_Already_Open` (left over from a previous host
    /// process) is treated as success.
    pub async fn open_session(&mut self, id: u32) -> PtpResult<()> {
        self.session_id = None;
        match self.transact(op::OPEN_SESSION, &[id], None).await {
            Ok(_) => {}
            Err(PtpError::Response { code: resp::SESSION_ALREADY_OPEN, .. }) => {
                debug!("session already open; reusing");
            }
            Err(e) => return Err(e),
        }
        self.session_id = Some(id);
        self.next_tid = 1;
        Ok(())
    }

    pub async fn close_session(&mut self) -> PtpResult<()> {
        if self.session_id.is_none() {
            return Ok(());
        }
        let r = self.transact(op::CLOSE_SESSION, &[], None).await;
        self.session_id = None;
        r.map(|_| ())
    }

    pub async fn get_storage_ids(&mut self) -> PtpResult<Vec<u32>> {
        let (d, _) = self.transact(op::GET_STORAGE_IDS, &[], None).await?;
        Reader::new(&d).u32_array()
    }

    pub async fn get_storage_info(&mut self, storage: u32) -> PtpResult<StorageInfo> {
        let (d, _) = self.transact(op::GET_STORAGE_INFO, &[storage], None).await?;
        StorageInfo::decode(&d)
    }

    /// `parent`: [`crate::codes::ROOT_PARENT`] for the storage root.
    pub async fn get_object_handles(&mut self, storage: u32, format: u16, parent: u32) -> PtpResult<Vec<u32>> {
        let (d, _) = self.transact(op::GET_OBJECT_HANDLES, &[storage, format as u32, parent], None).await?;
        Reader::new(&d).u32_array()
    }

    pub async fn get_object_info(&mut self, handle: u32) -> PtpResult<ObjectInfo> {
        let (d, _) = self.transact(op::GET_OBJECT_INFO, &[handle], None).await?;
        ObjectInfo::decode(&d)
    }

    /// GetObjectPropList(handle, format, prop, group, depth).
    /// Listing a folder: `handle = folder` (0 for root), `depth = 1`.
    pub async fn get_object_prop_list(&mut self, handle: u32, format: u32, prop: u32, group: u32, depth: u32) -> PtpResult<Vec<PropListElement>> {
        let (d, _) = self.transact(op::GET_OBJECT_PROP_LIST, &[handle, format, prop, group, depth], None).await?;
        decode_prop_list(&d)
    }

    pub async fn get_object_props_supported(&mut self, format: u16) -> PtpResult<Vec<u16>> {
        let (d, _) = self.transact(op::GET_OBJECT_PROPS_SUPPORTED, &[format as u32], None).await?;
        Reader::new(&d).u16_array()
    }

    pub async fn get_object_prop_value_raw(&mut self, handle: u32, prop: u16) -> PtpResult<Vec<u8>> {
        let (d, _) = self.transact(op::GET_OBJECT_PROP_VALUE, &[handle, prop as u32], None).await?;
        Ok(d)
    }

    pub async fn get_object_prop_value(&mut self, handle: u32, prop: u16, dtype: u16) -> PtpResult<PropValue> {
        let d = self.get_object_prop_value_raw(handle, prop).await?;
        PropValue::decode(&mut Reader::new(&d), dtype)
    }

    pub async fn set_object_prop_value(&mut self, handle: u32, prop: u16, value: &PropValue) -> PtpResult<()> {
        let bytes = value.to_bytes();
        self.transact(op::SET_OBJECT_PROP_VALUE, &[handle, prop as u32], Some(&bytes)).await?;
        Ok(())
    }

    /// Rename via ObjectFileName (0xDC07).
    pub async fn set_object_filename(&mut self, handle: u32, name: &str) -> PtpResult<()> {
        self.set_object_prop_value(handle, crate::codes::prop::OBJECT_FILE_NAME, &PropValue::Str(name.into())).await
    }

    pub async fn delete_object(&mut self, handle: u32) -> PtpResult<()> {
        self.transact(op::DELETE_OBJECT, &[handle, 0], None).await?;
        Ok(())
    }

    pub async fn move_object(&mut self, handle: u32, storage: u32, parent: u32) -> PtpResult<()> {
        self.transact(op::MOVE_OBJECT, &[handle, storage, parent], None).await?;
        Ok(())
    }

    pub async fn get_thumb(&mut self, handle: u32) -> PtpResult<Vec<u8>> {
        let (d, _) = self.transact(op::GET_THUMB, &[handle], None).await?;
        Ok(d)
    }

    /// SendObjectInfo → (storage, parent, new handle). Must be followed by
    /// SendObject on the same guard.
    pub async fn send_object_info(&mut self, storage: u32, parent: u32, info: &ObjectInfo) -> PtpResult<(u32, u32, u32)> {
        let bytes = info.encode();
        let (_, r) = self.transact(op::SEND_OBJECT_INFO, &[storage, parent], Some(&bytes)).await?;
        Ok((r.param(0), r.param(1), r.param(2)))
    }

    /// SendObjectPropList (MTP) with a 64-bit size → (storage, parent, handle).
    /// `props` must not contain StorageID/ObjectFormat/ObjectSize/ParentObject
    /// (they are operation parameters); ObjectFileName is required.
    pub async fn send_object_prop_list(&mut self, storage: u32, parent: u32, format: u16, size: u64, props: &[PropListElement]) -> PtpResult<(u32, u32, u32)> {
        let bytes = encode_prop_list(props);
        let params = [storage, parent, format as u32, (size >> 32) as u32, size as u32];
        let (_, r) = self.transact(op::SEND_OBJECT_PROP_LIST, &params, Some(&bytes)).await?;
        Ok((r.param(0), r.param(1), r.param(2)))
    }

    /// Fetch a property list value helper used for >4 GiB sizes.
    pub async fn get_object_size64(&mut self, handle: u32) -> PtpResult<u64> {
        let v = self.get_object_prop_value(handle, crate::codes::prop::OBJECT_SIZE, datatype::UINT64).await?;
        v.as_u64().ok_or_else(|| PtpError::protocol("ObjectSize not an integer"))
    }
}

// ------------------------------------------------------------ streaming IN

/// Streaming incoming data phase. Holds the session lock until the
/// transaction completes (EOF) or the stream is dropped.
pub struct DataIn {
    guard: Option<SessionGuard>,
    op: u16,
    tid: u32,
    /// Payload length, if known (header or caller-supplied).
    total: Option<u64>,
    received: u64,
    pending: Bytes,
    data_done: bool,
    response: Option<Response>,
}

impl DataIn {
    /// Start a transaction whose data phase is streamed. `expected_len` is
    /// used when the device sends the 0xFFFFFFFF length (objects >= 4 GiB).
    pub async fn start(mut guard: SessionGuard, op_code: u16, params: &[u32], expected_len: Option<u64>) -> PtpResult<DataIn> {
        let mut attempt = 0;
        loop {
            let tid = guard.alloc_tid();
            let res: PtpResult<Option<DataIn>> = async {
                guard.send_command(op_code, tid, params).await?;
                let ct = guard.command_timeout;
                let (first, short) = guard.read_first(FIRST_TRANSFER, ct).await?;
                let h = Header::decode(&first)?;
                if h.tid != tid {
                    return Err(PtpError::protocol(format!("tid {} != {tid}", h.tid)));
                }
                match h.kind {
                    ContainerType::Response => {
                        let r = ParamContainer::decode(&first)?;
                        if r.code == resp::DEVICE_BUSY && attempt < guard.busy_retries {
                            return Ok(None);
                        }
                        let r = SessionInner::check(op_code, r)?;
                        // OK without a data phase: empty object.
                        Ok(Some(DataIn { guard: None, op: op_code, tid, total: Some(0), received: 0, pending: Bytes::new(), data_done: true, response: Some(r) }))
                    }
                    ContainerType::Data => {
                        let total = h.payload_len().or(expected_len);
                        let mut payload = Bytes::from(first).slice(HEADER_LEN..);
                        if let Some(t) = total {
                            if payload.len() as u64 > t {
                                guard.carry = payload.split_off(t as usize).to_vec();
                            }
                        }
                        let received = payload.len() as u64;
                        let data_done = short || total.is_some_and(|t| received >= t);
                        if short && total.is_some_and(|t| received < t) {
                            return Err(PtpError::protocol("data phase ended early"));
                        }
                        Ok(Some(DataIn { guard: None, op: op_code, tid, total, received, pending: payload, data_done, response: None }))
                    }
                    k => Err(PtpError::protocol(format!("unexpected {k:?} container"))),
                }
            }
            .await;
            match res {
                Ok(None) => {
                    attempt += 1;
                    tokio::time::sleep(guard.busy_delay).await;
                }
                Ok(Some(mut d)) => {
                    if d.response.is_none() {
                        d.guard = Some(guard);
                    }
                    return Ok(d);
                }
                Err(e) => {
                    if matches!(e, PtpError::Transport(_) | PtpError::Protocol(_)) {
                        guard.poisoned = Some(tid);
                    }
                    return Err(e);
                }
            }
        }
    }

    /// GetObject (0x1009).
    pub async fn get_object(guard: SessionGuard, handle: u32, size: Option<u64>) -> PtpResult<DataIn> {
        Self::start(guard, op::GET_OBJECT, &[handle], size).await
    }

    /// GetPartialObject (0x101B) — 32-bit offset and length.
    pub async fn get_partial_object(guard: SessionGuard, handle: u32, offset: u32, len: u32) -> PtpResult<DataIn> {
        Self::start(guard, op::GET_PARTIAL_OBJECT, &[handle, offset, len], Some(len as u64)).await
    }

    /// Android GetPartialObject64 (0x95C1) — 64-bit offset, 32-bit length.
    pub async fn get_partial_object_64(guard: SessionGuard, handle: u32, offset: u64, len: u32) -> PtpResult<DataIn> {
        Self::start(guard, op::ANDROID_GET_PARTIAL_OBJECT_64, &[handle, offset as u32, (offset >> 32) as u32, len], Some(len as u64)).await
    }

    pub fn total_len(&self) -> Option<u64> {
        self.total
    }

    pub fn response(&self) -> Option<&Response> {
        self.response.as_ref()
    }

    fn fail(&mut self, e: PtpError) -> PtpError {
        if let Some(g) = self.guard.as_mut() {
            g.poisoned = Some(self.tid);
        }
        self.guard = None;
        e
    }

    /// Next chunk of at most `max` bytes, `None` at end (the response has
    /// then been read and the session lock released).
    pub async fn read_chunk(&mut self, max: usize) -> PtpResult<Option<Bytes>> {
        let max = max.max(1);
        loop {
            if !self.pending.is_empty() {
                let n = self.pending.len().min(max);
                return Ok(Some(self.pending.split_to(n)));
            }
            if self.response.is_some() {
                return Ok(None);
            }
            let Some(g) = self.guard.as_mut() else {
                return Err(PtpError::NoSession);
            };
            if self.data_done {
                let dt = g.data_timeout;
                let r = match g.read_response(self.op, self.tid, dt).await {
                    Ok(r) => r,
                    Err(e) => return Err(self.fail(e)),
                };
                self.guard = None; // transaction complete: release the lock
                let r = SessionInner::check(self.op, r)?;
                self.response = Some(r);
                return Ok(None);
            }
            let mps = g.max_packet_size();
            let want = match self.total {
                Some(t) => (t - self.received).min(max.min(MAX_TRANSFER) as u64) as usize,
                None => max.min(MAX_TRANSFER),
            };
            if want == 0 {
                self.data_done = true;
                continue;
            }
            let req = round_up(want, mps);
            let dt = g.data_timeout;
            let mut b = match g.transport.bulk_in(req, dt).await {
                Ok(b) => b,
                Err(e) => return Err(self.fail(e.into())),
            };
            let short = b.len() < req;
            if let Some(t) = self.total {
                let left = t - self.received;
                if b.len() as u64 > left {
                    // Next container (response) arrived in the same transfer.
                    g.carry = b.split_off(left as usize);
                }
            }
            self.received += b.len() as u64;
            if short || self.total.is_some_and(|t| self.received >= t) {
                self.data_done = true;
                if let Some(t) = self.total {
                    if self.received < t {
                        let e = PtpError::protocol(format!("data phase ended early: {} of {t} bytes", self.received));
                        // The device finished its data phase; read the response so the session stays in sync.
                        let g = self.guard.as_mut().unwrap();
                        let dt = g.data_timeout;
                        if g.read_response(self.op, self.tid, dt).await.is_ok() {
                            self.guard = None;
                            return Err(e);
                        }
                        return Err(self.fail(e));
                    }
                }
            }
            if !b.is_empty() {
                self.pending = Bytes::from(b);
            }
        }
    }

    /// Read the remaining data into memory.
    pub async fn read_to_end(mut self) -> PtpResult<Vec<u8>> {
        let mut out = Vec::with_capacity(self.total.unwrap_or(0).min(64 << 20) as usize);
        while let Some(c) = self.read_chunk(MAX_TRANSFER).await? {
            out.extend_from_slice(&c);
        }
        Ok(out)
    }
}

impl Drop for DataIn {
    fn drop(&mut self) {
        if let Some(g) = self.guard.as_mut() {
            debug!(tid = self.tid, "DataIn dropped mid-transfer; session will be recovered");
            g.poisoned = Some(self.tid);
        }
    }
}

// ----------------------------------------------------------- streaming OUT

/// Streaming outgoing data phase (SendObject). Holds the session lock.
pub struct DataOut {
    guard: Option<SessionGuard>,
    op: u16,
    tid: u32,
    total: u64,
    accepted: u64,
    buf: Vec<u8>,
    chunk: usize,
    mps: usize,
    response: Option<Response>,
}

impl DataOut {
    /// Send the command and prepare a data container of exactly `total`
    /// payload bytes.
    pub async fn start(mut guard: SessionGuard, op_code: u16, params: &[u32], total: u64) -> PtpResult<DataOut> {
        let tid = guard.alloc_tid();
        if let Err(e) = guard.send_command(op_code, tid, params).await {
            guard.poisoned = Some(tid);
            return Err(e);
        }
        let mps = guard.max_packet_size();
        let chunk = (MAX_TRANSFER / mps).max(1) * mps;
        let mut buf = Vec::with_capacity(chunk);
        buf.extend_from_slice(&Header::data(op_code, tid, total).encode());
        Ok(DataOut { guard: Some(guard), op: op_code, tid, total, accepted: 0, buf, chunk, mps, response: None })
    }

    /// SendObject (0x100D) for the object announced by the preceding
    /// SendObjectInfo / SendObjectPropList on this guard.
    pub async fn send_object(guard: SessionGuard, size: u64) -> PtpResult<DataOut> {
        Self::start(guard, op::SEND_OBJECT, &[], size).await
    }

    pub fn written(&self) -> u64 {
        self.accepted
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    fn fail(&mut self, e: PtpError) -> PtpError {
        if let Some(g) = self.guard.as_mut() {
            g.poisoned = Some(self.tid);
        }
        self.guard = None;
        e
    }

    pub async fn write(&mut self, mut data: &[u8]) -> PtpResult<()> {
        if self.guard.is_none() {
            return Err(PtpError::NoSession);
        }
        if self.accepted + data.len() as u64 > self.total {
            return Err(self.fail(PtpError::protocol(format!(
                "write exceeds declared size {} (already {}, +{})",
                self.total,
                self.accepted,
                data.len()
            ))));
        }
        self.accepted += data.len() as u64;
        while !data.is_empty() {
            let room = self.chunk - self.buf.len();
            let n = room.min(data.len());
            self.buf.extend_from_slice(&data[..n]);
            data = &data[n..];
            if self.buf.len() == self.chunk {
                let full = std::mem::replace(&mut self.buf, Vec::with_capacity(self.chunk));
                let g = self.guard.as_mut().unwrap();
                let dt = g.data_timeout;
                if let Err(e) = g.transport.bulk_out(full, dt).await {
                    return Err(self.fail(e.into()));
                }
            }
        }
        Ok(())
    }

    /// Flush the tail (short packet or ZLP) and read the response. Returns
    /// the response and the guard (so callers can chain operations).
    pub async fn finish(mut self) -> PtpResult<(Response, SessionGuard)> {
        if self.accepted != self.total {
            let e = PtpError::protocol(format!("finish after {} of {} bytes", self.accepted, self.total));
            return Err(self.fail(e));
        }
        let tail = std::mem::take(&mut self.buf);
        let container_len = self.total + HEADER_LEN as u64;
        let needs_zlp = container_len % self.mps as u64 == 0;
        let g = self.guard.as_mut().ok_or(PtpError::NoSession)?;
        let dt = g.data_timeout;
        let res: PtpResult<Response> = async {
            if !tail.is_empty() {
                g.transport.bulk_out(tail, dt).await?;
            }
            if needs_zlp {
                g.transport.bulk_out(Vec::new(), dt).await?;
            }
            g.read_response(self.op, self.tid, dt).await
        }
        .await;
        match res {
            Ok(r) => {
                let g = self.guard.take().unwrap();
                let r = SessionInner::check(self.op, r)?;
                self.response = Some(r.clone());
                Ok((r, g))
            }
            Err(e) => Err(self.fail(e)),
        }
    }

    /// Abandon the transfer: cancel the transaction now (instead of on the
    /// next lock) and return the recovered guard. `None` if the stream had
    /// already failed and released the lock (the next `lock()` recovers).
    pub async fn abort(mut self) -> Option<SessionGuard> {
        let mut g = self.guard.take()?;
        g.poisoned = Some(self.tid);
        if let Err(e) = g.recover().await {
            warn!("recovery after abort failed: {e}");
        }
        Some(g)
    }
}

impl Drop for DataOut {
    fn drop(&mut self) {
        if let Some(g) = self.guard.as_mut() {
            debug!(tid = self.tid, "DataOut dropped mid-transfer; session will be recovered");
            g.poisoned = Some(self.tid);
        }
    }
}
