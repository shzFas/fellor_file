//! Apple File Conduit (AFC) client, plus house_arrest and installation_proxy.

pub mod client;
pub mod house_arrest;
pub mod installation_proxy;
pub mod proto;
pub mod status;

#[cfg(any(test, feature = "fake"))]
pub mod fake;

pub use client::{time_to_ns, ns_to_time, AfcClient, DeviceFsInfo, FileInfo, FileType, MAX_IO_CHUNK};
pub use installation_proxy::AppInfo;
pub use proto::OpenMode;
pub use status::AfcStatus;

/// lockdown service name for the media AFC (`/var/mobile/Media`).
pub const AFC_SERVICE: &str = "com.apple.afc";

#[cfg(test)]
mod tests;
