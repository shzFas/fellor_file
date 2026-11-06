//! lockdownd client: framing, sessions with in-place TLS upgrade, services.
//!
//! ```text
//! usbmux Connect(62078) → QueryType → StartSession{HostID,SystemBUID}
//!   → (EnableSessionSSL) TLS on the same socket → GetValue / StartService
//! usbmux Connect(service port) → (EnableServiceSSL) TLS → service protocol
//! ```

pub mod error;
pub mod framing;
pub mod pem;
pub mod tls;

pub use error::map_lockdown_error;
pub use fellor_usbmux::{BoxIo, DeviceIo, PairRecord};
pub use tls::TlsStream;

use fellor_usbmux::plist_util::{get_bool, get_str, get_u64};
use fellor_usbmux::{dict, UsbmuxClient, LOCKDOWN_PORT};
use fellor_vfs::{Result, VfsError};
use plist::{Dictionary, Value};
use rustls::ClientConfig;
use std::sync::Arc;

pub const LABEL: &str = "fellor";
pub const LOCKDOWN_SERVICE_TYPE: &str = "com.apple.mobile.lockdown";

/// Result of `StartService`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    pub name: String,
    pub port: u16,
    pub ssl: bool,
}

/// Result of `StartSession`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: String,
    pub ssl: bool,
}

/// Convert an `Error` key in a reply (if any) into a VfsError.
pub fn check_reply(reply: &Dictionary, ctx: &str) -> Result<()> {
    match reply.get("Error") {
        None => Ok(()),
        Some(Value::String(e)) => Err(map_lockdown_error(e, ctx)),
        Some(other) => Err(VfsError::Protocol(format!("lockdown {ctx}: error {other:?}"))),
    }
}

/// A lockdownd connection. Starts in plaintext; `start_session` may switch
/// it to TLS in place.
pub struct LockdownClient {
    io: Option<BoxIo>,
    session: Option<SessionInfo>,
}

impl LockdownClient {
    pub fn new(io: BoxIo) -> Self {
        Self { io: Some(io), session: None }
    }

    fn io(&mut self) -> Result<&mut BoxIo> {
        self.io.as_mut().ok_or(VfsError::Disconnected)
    }

    pub fn session(&self) -> Option<&SessionInfo> {
        self.session.as_ref()
    }

    /// Send `{Label, Request, ...extra}` and read the reply; `Error` keys are
    /// mapped to VfsError.
    pub async fn request(&mut self, request: &str, extra: Dictionary) -> Result<Dictionary> {
        let mut msg = dict! { "Label" => LABEL, "Request" => request };
        for (k, v) in extra {
            msg.insert(k, v);
        }
        let io = self.io()?;
        framing::write_frame(io, &msg).await?;
        let reply = framing::read_frame(io).await?;
        check_reply(&reply, request)?;
        Ok(reply)
    }

    pub async fn query_type(&mut self) -> Result<String> {
        let r = self.request("QueryType", Dictionary::new()).await?;
        get_str(&r, "Type").map(str::to_owned).ok_or_else(|| VfsError::Protocol("QueryType: no Type".into()))
    }

    /// `GetValue` for one key (or the whole domain if `key` is `None`).
    pub async fn get_value(&mut self, domain: Option<&str>, key: Option<&str>) -> Result<Value> {
        let mut extra = Dictionary::new();
        if let Some(d) = domain {
            extra.insert("Domain".into(), Value::from(d));
        }
        if let Some(k) = key {
            extra.insert("Key".into(), Value::from(k));
        }
        let mut r = self.request("GetValue", extra).await?;
        r.remove("Value").ok_or_else(|| VfsError::NotFound(format!("lockdown value {}", key.unwrap_or("<all>"))))
    }

    /// Convenience: string value from the default domain.
    pub async fn get_string(&mut self, key: &str) -> Result<String> {
        match self.get_value(None, Some(key)).await? {
            Value::String(s) => Ok(s),
            other => Err(VfsError::Protocol(format!("lockdown {key}: not a string: {other:?}"))),
        }
    }

    /// `StartSession`; on `EnableSessionSSL` the connection is upgraded to
    /// TLS using the pair record's host certificate.
    pub async fn start_session(&mut self, pair: &PairRecord, system_buid: &str, tls: Arc<ClientConfig>) -> Result<SessionInfo> {
        let extra = dict! { "HostID" => pair.host_id.as_str(), "SystemBUID" => system_buid };
        let r = self.request("StartSession", extra).await?;
        let session_id = get_str(&r, "SessionID")
            .map(str::to_owned)
            .ok_or_else(|| VfsError::Protocol("StartSession: no SessionID".into()))?;
        let ssl = get_bool(&r, "EnableSessionSSL").unwrap_or(false);
        if ssl {
            let io = self.io.take().ok_or(VfsError::Disconnected)?;
            let tls = TlsStream::connect(io, tls).await?;
            self.io = Some(Box::new(tls));
        }
        let info = SessionInfo { session_id, ssl };
        self.session = Some(info.clone());
        Ok(info)
    }

    /// `StartService` → port and whether the service wants TLS.
    pub async fn start_service(&mut self, name: &str, escrow_bag: Option<&[u8]>) -> Result<ServiceInfo> {
        let mut extra = dict! { "Service" => name };
        if let Some(bag) = escrow_bag {
            extra.insert("EscrowBag".into(), Value::Data(bag.to_vec()));
        }
        let r = self.request("StartService", extra).await?;
        parse_start_service(&r, name)
    }

    pub fn into_inner(self) -> Option<BoxIo> {
        self.io
    }
}

pub fn parse_start_service(r: &Dictionary, name: &str) -> Result<ServiceInfo> {
    let port = get_u64(r, "Port")
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0)
        .ok_or_else(|| VfsError::Protocol(format!("StartService {name}: no valid Port")))?;
    Ok(ServiceInfo {
        name: get_str(r, "Service").unwrap_or(name).to_owned(),
        port,
        ssl: get_bool(r, "EnableServiceSSL").unwrap_or(false),
    })
}

/// Generic plist-over-framing channel (house_arrest, installation_proxy, ...).
pub struct PlistChannel {
    io: BoxIo,
}

impl PlistChannel {
    pub fn new(io: BoxIo) -> Self {
        Self { io }
    }
    pub async fn send(&mut self, msg: &Dictionary) -> Result<()> {
        framing::write_frame(&mut self.io, msg).await
    }
    pub async fn recv(&mut self) -> Result<Dictionary> {
        framing::read_frame(&mut self.io).await
    }
    pub fn into_inner(self) -> BoxIo {
        self.io
    }
}

/// Basic identity values read from lockdown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceValues {
    pub device_name: Option<String>,
    pub product_type: Option<String>,
    pub product_version: Option<String>,
    pub device_class: Option<String>,
    pub unique_device_id: Option<String>,
}

impl DeviceValues {
    pub fn from_dict(d: &Dictionary) -> Self {
        let s = |k: &str| get_str(d, k).map(str::to_owned);
        DeviceValues {
            device_name: s("DeviceName"),
            product_type: s("ProductType"),
            product_version: s("ProductVersion"),
            device_class: s("DeviceClass"),
            unique_device_id: s("UniqueDeviceID"),
        }
    }
}

/// A paired device reachable through usbmuxd. Opening services needs a
/// fresh lockdown session each time (cheap: a few round trips).
#[derive(Clone)]
pub struct Device {
    pub device_id: u32,
    pub udid: String,
    pub pair: Arc<PairRecord>,
    pub system_buid: String,
    tls: Arc<ClientConfig>,
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device").field("device_id", &self.device_id).field("udid", &self.udid).finish_non_exhaustive()
    }
}

impl Device {
    /// Read the pair record from usbmuxd. `NeedsTrust` if there is none.
    pub async fn open(device_id: u32, udid: &str) -> Result<Self> {
        let pair = UsbmuxClient::connect().await?.read_pair_record(udid).await?;
        let system_buid = match &pair.system_buid {
            Some(b) => b.clone(),
            None => UsbmuxClient::connect().await?.read_buid().await?,
        };
        Self::with_pair_record(device_id, udid, pair, system_buid)
    }

    pub fn with_pair_record(device_id: u32, udid: &str, pair: PairRecord, system_buid: String) -> Result<Self> {
        let tls = tls::client_config(&pair)?;
        Ok(Self { device_id, udid: udid.to_owned(), pair: Arc::new(pair), system_buid, tls })
    }

    pub fn tls_config(&self) -> Arc<ClientConfig> {
        self.tls.clone()
    }

    /// Raw usbmux connection to `port` on the device.
    pub async fn connect_port(&self, port: u16) -> Result<BoxIo> {
        let s = UsbmuxClient::connect().await?.connect_device(self.device_id, port).await?;
        Ok(Box::new(s))
    }

    /// Lockdown connection with a started (TLS) session.
    pub async fn lockdown(&self) -> Result<LockdownClient> {
        let mut ld = LockdownClient::new(self.connect_port(LOCKDOWN_PORT).await?);
        let ty = ld.query_type().await?;
        if ty != LOCKDOWN_SERVICE_TYPE {
            tracing::warn!(%ty, "unexpected lockdown QueryType");
        }
        ld.start_session(&self.pair, &self.system_buid, self.tls.clone()).await?;
        Ok(ld)
    }

    /// Identity values (DeviceName, ProductType, ...).
    pub async fn values(&self) -> Result<DeviceValues> {
        let mut ld = self.lockdown().await?;
        match ld.get_value(None, None).await? {
            Value::Dictionary(d) => Ok(DeviceValues::from_dict(&d)),
            _ => Err(VfsError::Protocol("GetValue: not a dictionary".into())),
        }
    }

    /// Connect to a started service, doing TLS if it asked for it.
    pub async fn connect_service(&self, svc: &ServiceInfo) -> Result<BoxIo> {
        let io = self.connect_port(svc.port).await?;
        if svc.ssl {
            Ok(Box::new(TlsStream::connect(io, self.tls.clone()).await?))
        } else {
            Ok(io)
        }
    }

    /// `StartService(name)` + connect.
    pub async fn start_service(&self, name: &str) -> Result<BoxIo> {
        let mut ld = self.lockdown().await?;
        let svc = ld.start_service(name, None).await?;
        drop(ld);
        self.connect_service(&svc).await
    }
}

#[cfg(test)]
mod tests;
