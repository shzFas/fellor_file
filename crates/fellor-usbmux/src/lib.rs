//! Async client for Apple's `usbmuxd` (`/var/run/usbmuxd`).
//!
//! Wire format (all little-endian):
//! `length:u32 | version:u32 (=1) | message:u32 (=8, plist) | tag:u32 | XML plist`
//! where `length` includes the 16-byte header.
//!
//! After a successful `Connect` the socket becomes a raw byte pipe to the
//! requested TCP port on the device; [`UsbmuxClient::connect_device`] hands it
//! back to the caller.

pub mod plist_util;

use fellor_vfs::{Result, VfsError};
use plist::{Dictionary, Value};
use plist_util::{get_dict, get_str, get_u64};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

/// Default usbmuxd socket on macOS / Linux.
pub const USBMUXD_SOCKET: &str = "/var/run/usbmuxd";
/// Override the socket path (useful for tests / socat relays).
pub const USBMUXD_SOCKET_ENV: &str = "FELLOR_USBMUXD_SOCKET";

pub const HEADER_LEN: usize = 16;
pub const PROTOCOL_VERSION: u32 = 1;
pub const MESSAGE_PLIST: u32 = 8;
/// Upper bound for a single usbmux control packet; anything bigger is garbage.
pub const MAX_PACKET: usize = 16 * 1024 * 1024;

/// lockdownd port on the device.
pub const LOCKDOWN_PORT: u16 = 62078;

const PROG_NAME: &str = "fellor";
const CLIENT_VERSION: &str = "fellor-usbmux-0.1";

/// Anything that can serve as a byte stream to a device. Boxed so TLS and
/// plain streams (and in-memory test pipes) are interchangeable.
pub trait DeviceIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + ?Sized> DeviceIo for T {}
pub type BoxIo = Box<dyn DeviceIo>;

// ---------------------------------------------------------------------------
// Result codes
// ---------------------------------------------------------------------------

/// `Result` → `Number` values returned by usbmuxd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultCode {
    Ok,
    BadCommand,
    BadDevice,
    ConnectionRefused,
    Malformed,
    BadVersion,
    Unknown(u64),
}

impl ResultCode {
    pub fn from_number(n: u64) -> Self {
        match n {
            0 => ResultCode::Ok,
            1 => ResultCode::BadCommand,
            2 => ResultCode::BadDevice,
            3 => ResultCode::ConnectionRefused,
            5 => ResultCode::Malformed,
            6 => ResultCode::BadVersion,
            n => ResultCode::Unknown(n),
        }
    }

    pub fn into_error(self, ctx: &str) -> VfsError {
        match self {
            ResultCode::Ok => VfsError::Other(format!("usbmuxd {ctx}: ok")),
            ResultCode::BadCommand => VfsError::Protocol(format!("usbmuxd {ctx}: bad command")),
            ResultCode::BadDevice => VfsError::Disconnected,
            ResultCode::ConnectionRefused => VfsError::Transport(format!("usbmuxd {ctx}: connection refused by device")),
            ResultCode::Malformed => VfsError::Protocol(format!("usbmuxd {ctx}: malformed request")),
            ResultCode::BadVersion => VfsError::Protocol(format!("usbmuxd {ctx}: protocol version mismatch")),
            ResultCode::Unknown(n) => VfsError::Protocol(format!("usbmuxd {ctx}: error {n}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Packet codec
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub length: u32,
    pub version: u32,
    pub message: u32,
    pub tag: u32,
}

impl Header {
    pub fn parse(b: &[u8; HEADER_LEN]) -> Self {
        let w = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Header { length: w(0), version: w(4), message: w(8), tag: w(12) }
    }

    pub fn to_bytes(self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&self.length.to_le_bytes());
        out[4..8].copy_from_slice(&self.version.to_le_bytes());
        out[8..12].copy_from_slice(&self.message.to_le_bytes());
        out[12..16].copy_from_slice(&self.tag.to_le_bytes());
        out
    }
}

/// Encode a plist message into a full usbmux packet.
pub fn encode_packet(tag: u32, msg: &Dictionary) -> Result<Vec<u8>> {
    let body = plist_util::to_xml(msg).map_err(|e| VfsError::Protocol(format!("plist encode: {e}")))?;
    let length = u32::try_from(HEADER_LEN + body.len()).map_err(|_| VfsError::Protocol("usbmux packet too large".into()))?;
    let mut out = Vec::with_capacity(length as usize);
    out.extend_from_slice(&Header { length, version: PROTOCOL_VERSION, message: MESSAGE_PLIST, tag }.to_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode a complete packet held in memory. Returns (tag, plist).
pub fn decode_packet(b: &[u8]) -> Result<(u32, Dictionary)> {
    let hdr: &[u8; HEADER_LEN] = b
        .get(..HEADER_LEN)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| VfsError::Protocol("usbmux packet shorter than header".into()))?;
    let h = Header::parse(hdr);
    let len = validate_header(&h)?;
    let body = b.get(HEADER_LEN..len).ok_or_else(|| VfsError::Protocol("usbmux packet truncated".into()))?;
    Ok((h.tag, decode_body(&h, body)?))
}

fn validate_header(h: &Header) -> Result<usize> {
    let len = h.length as usize;
    if !(HEADER_LEN..=MAX_PACKET).contains(&len) {
        return Err(VfsError::Protocol(format!("usbmux bad packet length {len}")));
    }
    Ok(len)
}

fn decode_body(h: &Header, body: &[u8]) -> Result<Dictionary> {
    if h.message != MESSAGE_PLIST {
        return Err(VfsError::Protocol(format!("usbmux unexpected message type {}", h.message)));
    }
    plist_util::from_bytes(body).map_err(|e| VfsError::Protocol(format!("usbmux plist: {e}")))
}

/// Read one packet from a stream. Returns (tag, plist).
pub async fn read_packet<R: AsyncRead + Unpin + ?Sized>(r: &mut R) -> Result<(u32, Dictionary)> {
    let mut hdr = [0u8; HEADER_LEN];
    r.read_exact(&mut hdr).await.map_err(io_to_transport)?;
    let h = Header::parse(&hdr);
    let len = validate_header(&h)?;
    let mut body = vec![0u8; len - HEADER_LEN];
    r.read_exact(&mut body).await.map_err(io_to_transport)?;
    Ok((h.tag, decode_body(&h, &body)?))
}

pub(crate) fn io_to_transport(e: std::io::Error) -> VfsError {
    use std::io::ErrorKind::*;
    match e.kind() {
        UnexpectedEof | BrokenPipe | ConnectionReset | ConnectionAborted | NotConnected => VfsError::Disconnected,
        _ => VfsError::Transport(e.to_string()),
    }
}

/// The `PortNumber` field carries the port in network byte order packed into
/// a host-order u16 (i.e. `htons(port)`).
pub fn port_to_wire(port: u16) -> u16 {
    u16::from_ne_bytes(port.to_be_bytes())
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionType {
    Usb,
    Network,
    Other(String),
}

impl ConnectionType {
    fn parse(s: &str) -> Self {
        match s {
            "USB" => ConnectionType::Usb,
            "Network" => ConnectionType::Network,
            o => ConnectionType::Other(o.to_owned()),
        }
    }
}

/// One device as reported by `ListDevices` / `Attached`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    pub device_id: u32,
    /// The UDID ("SerialNumber" in usbmuxd terms).
    pub udid: String,
    pub connection_type: ConnectionType,
    pub product_id: Option<u32>,
    pub location_id: Option<u32>,
}

impl DeviceRecord {
    /// Parse from an `Attached` message or a `DeviceList` element.
    pub fn from_plist(d: &Dictionary) -> Option<Self> {
        let props = get_dict(d, "Properties").unwrap_or(d);
        let device_id = get_u64(d, "DeviceID").or_else(|| get_u64(props, "DeviceID"))?;
        let udid = get_str(props, "SerialNumber").or_else(|| get_str(props, "UDID"))?.to_owned();
        let connection_type = get_str(props, "ConnectionType").map(ConnectionType::parse).unwrap_or(ConnectionType::Usb);
        Some(DeviceRecord {
            device_id: u32::try_from(device_id).ok()?,
            udid,
            connection_type,
            product_id: get_u64(props, "ProductID").and_then(|v| u32::try_from(v).ok()),
            location_id: get_u64(props, "LocationID").and_then(|v| u32::try_from(v).ok()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsbmuxEvent {
    Attached(DeviceRecord),
    Detached { device_id: u32 },
    /// The user tapped "Trust" (a pair record now exists).
    Paired { device_id: u32 },
}

impl UsbmuxEvent {
    pub fn from_plist(d: &Dictionary) -> Option<Self> {
        let id = || get_u64(d, "DeviceID").and_then(|v| u32::try_from(v).ok());
        match get_str(d, "MessageType")? {
            "Attached" => DeviceRecord::from_plist(d).map(UsbmuxEvent::Attached),
            "Detached" => Some(UsbmuxEvent::Detached { device_id: id()? }),
            "Paired" => Some(UsbmuxEvent::Paired { device_id: id()? }),
            _ => None,
        }
    }
}

/// Pair record as stored by macOS in `/var/db/lockdown/<udid>.plist`.
/// Certificates and keys are PEM blobs.
#[derive(Clone, PartialEq, Eq)]
pub struct PairRecord {
    pub host_id: String,
    pub system_buid: Option<String>,
    pub host_certificate: Vec<u8>,
    pub host_private_key: Vec<u8>,
    pub root_certificate: Option<Vec<u8>>,
    pub root_private_key: Option<Vec<u8>>,
    pub device_certificate: Option<Vec<u8>>,
    pub escrow_bag: Option<Vec<u8>>,
    pub wifi_mac_address: Option<String>,
}

impl std::fmt::Debug for PairRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material.
        f.debug_struct("PairRecord")
            .field("host_id", &self.host_id)
            .field("system_buid", &self.system_buid)
            .field("has_device_certificate", &self.device_certificate.is_some())
            .field("has_escrow_bag", &self.escrow_bag.is_some())
            .finish_non_exhaustive()
    }
}

impl PairRecord {
    pub fn from_plist(d: &Dictionary) -> Result<Self> {
        use plist_util::get_data;
        let missing = |k: &str| VfsError::Protocol(format!("pair record is missing {k}"));
        Ok(PairRecord {
            host_id: get_str(d, "HostID").ok_or_else(|| missing("HostID"))?.to_owned(),
            system_buid: get_str(d, "SystemBUID").map(str::to_owned),
            host_certificate: get_data(d, "HostCertificate").ok_or_else(|| missing("HostCertificate"))?,
            host_private_key: get_data(d, "HostPrivateKey").ok_or_else(|| missing("HostPrivateKey"))?,
            root_certificate: get_data(d, "RootCertificate"),
            root_private_key: get_data(d, "RootPrivateKey"),
            device_certificate: get_data(d, "DeviceCertificate"),
            escrow_bag: get_data(d, "EscrowBag"),
            wifi_mac_address: get_str(d, "WiFiMACAddress").map(str::to_owned),
        })
    }

    /// Parse from raw plist bytes (XML or binary).
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        let d = plist_util::from_bytes(b).map_err(|e| VfsError::Protocol(format!("pair record plist: {e}")))?;
        Self::from_plist(&d)
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Resolve the socket path (env override or the default).
pub fn socket_path() -> PathBuf {
    std::env::var_os(USBMUXD_SOCKET_ENV).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(USBMUXD_SOCKET))
}

/// A control connection to usbmuxd. Each `Connect`/`Listen` consumes it.
pub struct UsbmuxClient<S = UnixStream> {
    stream: S,
    tag: u32,
}

impl UsbmuxClient<UnixStream> {
    /// Connect to the system usbmuxd socket.
    pub async fn connect() -> Result<Self> {
        Self::connect_path(&socket_path()).await
    }

    pub async fn connect_path(path: &Path) -> Result<Self> {
        let s = UnixStream::connect(path)
            .await
            .map_err(|e| VfsError::Transport(format!("usbmuxd ({}): {e}", path.display())))?;
        Ok(Self::new(s))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> UsbmuxClient<S> {
    pub fn new(stream: S) -> Self {
        Self { stream, tag: 0 }
    }

    pub fn into_inner(self) -> S {
        self.stream
    }

    fn base_request(message_type: &str) -> Dictionary {
        dict! {
            "MessageType" => message_type,
            "ProgName" => PROG_NAME,
            "ClientVersionString" => CLIENT_VERSION,
            "kLibUSBMuxVersion" => 3u64,
        }
    }

    /// Send a request and wait for the reply with the matching tag.
    pub async fn request(&mut self, msg: Dictionary) -> Result<Dictionary> {
        self.tag = self.tag.wrapping_add(1).max(1);
        let tag = self.tag;
        let pkt = encode_packet(tag, &msg)?;
        self.stream.write_all(&pkt).await.map_err(io_to_transport)?;
        self.stream.flush().await.map_err(io_to_transport)?;
        // Skip stray notifications (tag 0) that may interleave.
        for _ in 0..64 {
            let (rtag, reply) = read_packet(&mut self.stream).await?;
            if rtag == tag {
                return Ok(reply);
            }
            tracing::debug!(rtag, tag, "usbmux: skipping unrelated packet");
        }
        Err(VfsError::Protocol("usbmuxd: no reply with matching tag".into()))
    }

    /// Send a request whose reply is a plain `Result`.
    async fn request_result(&mut self, msg: Dictionary, ctx: &str) -> Result<()> {
        let reply = self.request(msg).await?;
        check_result(&reply, ctx)
    }

    pub async fn list_devices(&mut self) -> Result<Vec<DeviceRecord>> {
        let reply = self.request(Self::base_request("ListDevices")).await?;
        parse_device_list(&reply)
    }

    /// Read the pair record for `udid`. Fails with `NeedsTrust` if none exists.
    pub async fn read_pair_record(&mut self, udid: &str) -> Result<PairRecord> {
        let mut msg = Self::base_request("ReadPairRecord");
        msg.insert("PairRecordID".into(), Value::from(udid));
        let reply = self.request(msg).await?;
        parse_pair_record_reply(&reply)
    }

    pub async fn read_buid(&mut self) -> Result<String> {
        let reply = self.request(Self::base_request("ReadBUID")).await?;
        if let Some(b) = get_str(&reply, "BUID") {
            return Ok(b.to_owned());
        }
        check_result(&reply, "ReadBUID")?;
        Err(VfsError::Protocol("usbmuxd ReadBUID: no BUID".into()))
    }

    /// Ask usbmuxd to connect to `port` on the device. On success the
    /// underlying stream is a raw pipe to that port.
    pub async fn connect_device(mut self, device_id: u32, port: u16) -> Result<S> {
        let mut msg = Self::base_request("Connect");
        msg.insert("DeviceID".into(), Value::from(device_id as u64));
        msg.insert("PortNumber".into(), Value::from(port_to_wire(port) as u64));
        self.request_result(msg, "Connect").await?;
        Ok(self.stream)
    }

    /// Subscribe to attach/detach notifications.
    pub async fn listen(mut self) -> Result<Listener<S>> {
        self.request_result(Self::base_request("Listen"), "Listen").await?;
        Ok(Listener { stream: self.stream })
    }
}

pub fn check_result(reply: &Dictionary, ctx: &str) -> Result<()> {
    if get_str(reply, "MessageType") != Some("Result") && reply.get("Number").is_none() {
        return Err(VfsError::Protocol(format!("usbmuxd {ctx}: unexpected reply")));
    }
    match get_u64(reply, "Number").map(ResultCode::from_number) {
        Some(ResultCode::Ok) => Ok(()),
        Some(code) => Err(code.into_error(ctx)),
        None => Err(VfsError::Protocol(format!("usbmuxd {ctx}: Result without Number"))),
    }
}

pub fn parse_device_list(reply: &Dictionary) -> Result<Vec<DeviceRecord>> {
    let list = reply
        .get("DeviceList")
        .and_then(Value::as_array)
        .ok_or_else(|| VfsError::Protocol("usbmuxd ListDevices: no DeviceList".into()))?;
    Ok(list.iter().filter_map(Value::as_dictionary).filter_map(DeviceRecord::from_plist).collect())
}

pub fn parse_pair_record_reply(reply: &Dictionary) -> Result<PairRecord> {
    match reply.get("PairRecordData") {
        Some(Value::Data(b)) => PairRecord::from_bytes(b),
        Some(_) => Err(VfsError::Protocol("PairRecordData is not data".into())),
        None => {
            // usbmuxd answers with a non-zero Result when there is no record.
            tracing::debug!(?reply, "no pair record");
            Err(VfsError::NeedsTrust)
        }
    }
}

/// Stream of attach/detach events from a `Listen` connection.
pub struct Listener<S = UnixStream> {
    stream: S,
}

impl<S: AsyncRead + Unpin> Listener<S> {
    /// Next event. Unknown message types are skipped. An error means the
    /// usbmuxd connection is gone.
    pub async fn next_event(&mut self) -> Result<UsbmuxEvent> {
        loop {
            let (_tag, msg) = read_packet(&mut self.stream).await?;
            match UsbmuxEvent::from_plist(&msg) {
                Some(ev) => return Ok(ev),
                None => tracing::debug!(?msg, "usbmux: ignoring message"),
            }
        }
    }
}

#[cfg(test)]
mod tests;
