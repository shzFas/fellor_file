//! PTP (ISO 15740) + MTP 1.1 protocol over USB bulk, written from the specs.
//!
//! Layers:
//! * [`wire`], [`container`], [`dataset`] — byte-level codecs.
//! * [`transport`] — the USB abstraction ([`Transport`], [`EventSource`]).
//! * [`session`] — transactions, streaming data phases, retries, recovery.
//! * [`usb`] — the real transport over `nusb`.
//! * `fake` (feature `fake` or tests) — an in-memory device simulator.

// Workspace MSRV is 1.85: `usize::is_multiple_of` (1.87) and `as_chunks`
// (1.88) are not available there.
#![allow(unknown_lints, clippy::manual_is_multiple_of, clippy::chunks_exact_to_as_chunks)]

pub mod codes;
pub mod container;
pub mod dataset;
pub mod error;
pub mod session;
pub mod transport;
pub mod usb;
pub mod wire;

#[cfg(any(test, feature = "fake"))]
pub mod fake;

#[cfg(test)]
mod session_tests;

pub use dataset::{DeviceInfo, Event, ObjectInfo, ObjectProps, PropListElement, PropValue, StorageInfo};
pub use error::{PtpError, PtpResult, TransportError};
pub use session::{DataIn, DataOut, Session, SessionGuard, SessionInner};
pub use transport::{EventSource, Pipe, Transport};
