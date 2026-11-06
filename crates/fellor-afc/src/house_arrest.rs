//! `com.apple.mobile.house_arrest`: vend an app's sandbox, then speak AFC
//! on the same stream.

use crate::AfcClient;
use fellor_lockdown::framing::{read_frame, write_frame};
use fellor_usbmux::plist_util::get_str;
use fellor_usbmux::{dict, BoxIo};
use fellor_vfs::{Result, VfsError};
use plist::Dictionary;

pub const SERVICE: &str = "com.apple.mobile.house_arrest";
/// Root of the vended documents inside the AFC namespace.
pub const DOCUMENTS: &str = "/Documents";

pub fn check_vend_reply(reply: &Dictionary, bundle_id: &str) -> Result<()> {
    if let Some(err) = get_str(reply, "Error") {
        return Err(match err {
            "ApplicationLookupFailed" | "InstallationLookupFailed" => VfsError::NotFound(format!("app {bundle_id}")),
            "PasswordProtected" => VfsError::Locked,
            "InstallationIsNotDeveloperSigned" | "AccessDenied" => VfsError::PermissionDenied(format!("app {bundle_id}: {err}")),
            other => VfsError::Protocol(format!("house_arrest {bundle_id}: {other}")),
        });
    }
    match get_str(reply, "Status") {
        Some("Complete") => Ok(()),
        other => Err(VfsError::Protocol(format!("house_arrest {bundle_id}: unexpected status {other:?}"))),
    }
}

async fn vend(mut io: BoxIo, command: &str, bundle_id: &str) -> Result<AfcClient> {
    write_frame(&mut io, &dict! { "Command" => command, "Identifier" => bundle_id }).await?;
    let reply = read_frame(&mut io).await?;
    check_vend_reply(&reply, bundle_id)?;
    Ok(AfcClient::new(io))
}

/// `VendDocuments` — works for apps with `UIFileSharingEnabled`.
/// The returned AFC root is the app container; use [`DOCUMENTS`].
pub async fn vend_documents(io: BoxIo, bundle_id: &str) -> Result<AfcClient> {
    vend(io, "VendDocuments", bundle_id).await
}

/// `VendContainer` — whole container; only for developer-signed apps on modern iOS.
pub async fn vend_container(io: BoxIo, bundle_id: &str) -> Result<AfcClient> {
    vend(io, "VendContainer", bundle_id).await
}
