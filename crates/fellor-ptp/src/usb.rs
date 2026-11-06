//! Real USB transport over `nusb` (IOKit on macOS).

use crate::error::TransportError;
use crate::transport::{EventSource, Pipe, Transport};
use async_trait::async_trait;
use nusb::descriptors::TransferType;
use nusb::transfer::{Buffer, Bulk, ControlIn, ControlOut, ControlType, In, Interrupt, Out, Recipient, TransferError};
use nusb::{Endpoint, Interface};
use std::time::Duration;

/// Still Image class, Still Image Capture subclass, PIMA 15740 protocol.
pub const PTP_CLASS: (u8, u8, u8) = (0x06, 0x01, 0x01);

/// How an interface was recognised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtpInterface {
    pub number: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub name: Option<String>,
}

impl PtpInterface {
    /// The interface uses the standard Still Image class (cameras, and
    /// Android in both MTP and PTP modes).
    pub fn is_still_image_class(&self) -> bool {
        (self.class, self.subclass, self.protocol) == PTP_CLASS
    }

    /// Interface string says MTP (Android names its interface "MTP").
    pub fn is_named_mtp(&self) -> bool {
        self.name.as_deref().is_some_and(|n| n.trim().eq_ignore_ascii_case("mtp") || n.contains("MTP"))
    }
}

/// Classify an interface descriptor triple + string.
pub fn is_ptp_interface(class: u8, subclass: u8, protocol: u8, name: Option<&str>) -> bool {
    if (class, subclass, protocol) == PTP_CLASS {
        return true;
    }
    class == 0xFF && name.is_some_and(|n| n.contains("MTP"))
}

/// Find the PTP/MTP interface of an enumerated device, if any.
pub fn find_ptp_interface(info: &nusb::DeviceInfo) -> Option<PtpInterface> {
    info.interfaces()
        .find(|i| is_ptp_interface(i.class(), i.subclass(), i.protocol(), i.interface_string()))
        .map(|i| PtpInterface {
            number: i.interface_number(),
            class: i.class(),
            subclass: i.subclass(),
            protocol: i.protocol(),
            name: i.interface_string().map(str::to_owned),
        })
}

fn map_transfer(e: TransferError) -> TransportError {
    match e {
        TransferError::Cancelled => TransportError::Timeout,
        TransferError::Stall => TransportError::Stall,
        TransferError::Disconnected => TransportError::Disconnected,
        other => TransportError::Other(other.to_string()),
    }
}

fn map_err(e: nusb::Error) -> TransportError {
    match e.kind() {
        nusb::ErrorKind::Busy => TransportError::Busy,
        nusb::ErrorKind::Disconnected => TransportError::Disconnected,
        _ => TransportError::Other(e.to_string()),
    }
}

pub struct NusbTransport {
    iface: Interface,
    number: u8,
    bulk_in: Endpoint<Bulk, In>,
    bulk_out: Endpoint<Bulk, Out>,
    mps: usize,
}

pub struct NusbEvents {
    ep: Endpoint<Interrupt, In>,
    mps: usize,
    partial: Vec<u8>,
}

/// Open the device and claim its PTP interface. Fails with
/// [`TransportError::Busy`] when another program holds the interface
/// (Android File Transfer, OpenMTP, ptpcamerad, ...).
pub async fn open(info: &nusb::DeviceInfo, number: u8) -> Result<(NusbTransport, Option<NusbEvents>), TransportError> {
    let device = info.open().await.map_err(map_err)?;
    let iface = device.claim_interface(number).await.map_err(map_err)?;
    let desc = iface.descriptor().ok_or_else(|| TransportError::Other("no interface descriptor".into()))?;
    let (mut bin, mut bout, mut irq) = (None, None, None);
    for ep in desc.endpoints() {
        let addr = ep.address();
        let dir_in = addr & 0x80 != 0;
        match (ep.transfer_type(), dir_in) {
            (TransferType::Bulk, true) if bin.is_none() => bin = Some(addr),
            (TransferType::Bulk, false) if bout.is_none() => bout = Some(addr),
            (TransferType::Interrupt, true) if irq.is_none() => irq = Some(addr),
            _ => {}
        }
    }
    let (Some(bin), Some(bout)) = (bin, bout) else {
        return Err(TransportError::Other("PTP interface lacks bulk endpoints".into()));
    };
    let bulk_in = iface.endpoint::<Bulk, In>(bin).map_err(map_err)?;
    let bulk_out = iface.endpoint::<Bulk, Out>(bout).map_err(map_err)?;
    let mps = bulk_in.max_packet_size().max(8);
    let events = match irq {
        Some(a) => match iface.endpoint::<Interrupt, In>(a) {
            Ok(ep) => {
                let mps = ep.max_packet_size().max(8);
                Some(NusbEvents { ep, mps, partial: Vec::new() })
            }
            Err(e) => {
                tracing::warn!("cannot open interrupt endpoint: {e}");
                None
            }
        },
        None => None,
    };
    Ok((NusbTransport { iface, number, bulk_in, bulk_out, mps }, events))
}

#[async_trait]
impl Transport for NusbTransport {
    fn max_packet_size(&self) -> usize {
        self.mps
    }

    async fn bulk_out(&mut self, data: Vec<u8>, timeout: Duration) -> Result<(), TransportError> {
        self.bulk_out.submit(Buffer::from(data));
        let c = match tokio::time::timeout(timeout, self.bulk_out.next_complete()).await {
            Ok(c) => c,
            Err(_) => {
                self.bulk_out.cancel_all();
                let _ = self.bulk_out.next_complete().await;
                return Err(TransportError::Timeout);
            }
        };
        c.status.map_err(map_transfer)
    }

    async fn bulk_in(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, TransportError> {
        let len = len.div_ceil(self.mps).max(1) * self.mps;
        self.bulk_in.submit(Buffer::new(len));
        let c = match tokio::time::timeout(timeout, self.bulk_in.next_complete()).await {
            Ok(c) => c,
            Err(_) => {
                self.bulk_in.cancel_all();
                let c = self.bulk_in.next_complete().await;
                // The transfer may have completed while we were cancelling.
                if c.status.is_ok() {
                    return Ok(c.buffer.into_vec());
                }
                return Err(TransportError::Timeout);
            }
        };
        c.status.map_err(map_transfer)?;
        Ok(c.buffer.into_vec())
    }

    async fn clear_halt(&mut self, pipe: Pipe) -> Result<(), TransportError> {
        match pipe {
            Pipe::BulkIn => self.bulk_in.clear_halt().await.map_err(map_err),
            Pipe::BulkOut => self.bulk_out.clear_halt().await.map_err(map_err),
        }
    }

    async fn class_out(&mut self, request: u8, data: &[u8], timeout: Duration) -> Result<(), TransportError> {
        self.iface
            .control_out(
                ControlOut {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request,
                    value: 0,
                    index: self.number as u16,
                    data,
                },
                timeout,
            )
            .await
            .map_err(map_transfer)
    }

    async fn class_in(&mut self, request: u8, len: u16, timeout: Duration) -> Result<Vec<u8>, TransportError> {
        self.iface
            .control_in(
                ControlIn {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request,
                    value: 0,
                    index: self.number as u16,
                    length: len,
                },
                timeout,
            )
            .await
            .map_err(map_transfer)
    }
}

#[async_trait]
impl EventSource for NusbEvents {
    async fn next_event(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        loop {
            // Keep exactly one transfer in flight; next_complete is
            // cancel-safe so a dropped future does not lose the transfer.
            if self.ep.pending() == 0 {
                self.ep.submit(Buffer::new(self.mps));
            }
            let c = self.ep.next_complete().await;
            match c.status {
                Ok(()) => {}
                Err(TransferError::Disconnected) => return Ok(None),
                Err(TransferError::Stall) => {
                    let _ = self.ep.clear_halt().await;
                    continue;
                }
                Err(e) => return Err(map_transfer(e)),
            }
            let b = c.buffer.into_vec();
            self.partial.extend_from_slice(&b);
            // Containers longer than one packet continue until a short packet
            // or the declared length.
            let declared = (self.partial.len() >= 4).then(|| u32::from_le_bytes(self.partial[0..4].try_into().unwrap()) as usize);
            let complete = b.len() < self.mps || declared.is_some_and(|l| self.partial.len() >= l);
            if complete {
                let ev = std::mem::take(&mut self.partial);
                if ev.is_empty() {
                    continue;
                }
                return Ok(Some(ev));
            }
        }
    }
}
