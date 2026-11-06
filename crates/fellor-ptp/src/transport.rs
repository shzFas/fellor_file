//! Abstract USB transport used by the PTP session. The real implementation is
//! [`crate::usb::NusbTransport`]; tests use [`crate::fake`].

use crate::error::TransportError;
use async_trait::async_trait;
use std::time::Duration;

/// Which bulk pipe a recovery action targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pipe {
    BulkIn,
    BulkOut,
}

/// One PTP-over-USB interface: a bulk IN pipe, a bulk OUT pipe and the
/// class-specific control requests addressed to the interface.
///
/// Semantics mirror USB bulk transfers exactly so that packetisation bugs
/// (missing ZLPs, short packets mid-transfer) are observable in tests:
///
/// * `bulk_out(data)` sends `data` as one transfer. It does **not** append a
///   zero-length packet; an empty `data` *is* a ZLP. A transfer whose length
///   is not a multiple of `max_packet_size` ends with a short packet, which the
///   device treats as the end of the current container.
/// * `bulk_in(len)` performs one IN transfer of at most `len` bytes (`len` is
///   a non-zero multiple of `max_packet_size`). It completes when `len` bytes
///   arrived or after a short packet (a ZLP yields an empty vector).
#[async_trait]
pub trait Transport: Send {
    fn max_packet_size(&self) -> usize;

    async fn bulk_out(&mut self, data: Vec<u8>, timeout: Duration) -> Result<(), TransportError>;

    async fn bulk_in(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, TransportError>;

    async fn clear_halt(&mut self, pipe: Pipe) -> Result<(), TransportError>;

    /// Class-specific, interface-recipient OUT control request.
    async fn class_out(&mut self, request: u8, data: &[u8], timeout: Duration) -> Result<(), TransportError>;

    /// Class-specific, interface-recipient IN control request.
    async fn class_in(&mut self, request: u8, len: u16, timeout: Duration) -> Result<Vec<u8>, TransportError>;
}

/// Interrupt-IN event pipe. Runs independently of the session lock.
#[async_trait]
pub trait EventSource: Send {
    /// Wait for the next raw event container. `Ok(None)` means the pipe is
    /// closed (device gone).
    async fn next_event(&mut self) -> Result<Option<Vec<u8>>, TransportError>;
}
