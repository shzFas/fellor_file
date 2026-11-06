//! `com.apple.mobile.installation_proxy` — `Browse` installed apps.

use fellor_lockdown::framing::{read_frame, write_frame};
use fellor_usbmux::plist_util::{get_bool, get_str};
use fellor_usbmux::{dict, BoxIo};
use fellor_vfs::{Result, VfsError};
use plist::{Dictionary, Value};

pub const SERVICE: &str = "com.apple.mobile.installation_proxy";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    pub bundle_id: String,
    pub display_name: String,
    pub file_sharing: bool,
}

impl AppInfo {
    pub fn from_dict(d: &Dictionary) -> Option<Self> {
        let bundle_id = get_str(d, "CFBundleIdentifier")?.to_owned();
        let display_name = get_str(d, "CFBundleDisplayName")
            .or_else(|| get_str(d, "CFBundleName"))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&bundle_id)
            .to_owned();
        Some(AppInfo { bundle_id, display_name, file_sharing: get_bool(d, "UIFileSharingEnabled").unwrap_or(false) })
    }
}

pub fn browse_request(application_type: Option<&str>, attributes: &[&str]) -> Dictionary {
    let mut opts = Dictionary::new();
    if let Some(t) = application_type {
        opts.insert("ApplicationType".into(), Value::from(t));
    }
    if !attributes.is_empty() {
        opts.insert("ReturnAttributes".into(), Value::Array(attributes.iter().map(|a| Value::from(*a)).collect()));
    }
    dict! { "Command" => "Browse", "ClientOptions" => Value::Dictionary(opts) }
}

/// Run `Browse` and collect all `CurrentList` chunks until `Complete`.
pub async fn browse(io: &mut BoxIo, application_type: Option<&str>, attributes: &[&str]) -> Result<Vec<Dictionary>> {
    write_frame(io, &browse_request(application_type, attributes)).await?;
    let mut apps = Vec::new();
    for _ in 0..100_000 {
        let msg = read_frame(io).await?;
        if let Some(err) = get_str(&msg, "Error") {
            let desc = get_str(&msg, "ErrorDescription").unwrap_or("");
            return Err(VfsError::Protocol(format!("installation_proxy: {err} {desc}")));
        }
        if let Some(list) = msg.get("CurrentList").and_then(Value::as_array) {
            apps.extend(list.iter().filter_map(Value::as_dictionary).cloned());
        }
        if get_str(&msg, "Status") == Some("Complete") {
            return Ok(apps);
        }
    }
    Err(VfsError::Protocol("installation_proxy: Browse never completed".into()))
}

/// User apps with iTunes/Finder File Sharing, sorted by display name.
pub async fn file_sharing_apps(mut io: BoxIo) -> Result<Vec<AppInfo>> {
    let raw = browse(&mut io, Some("User"), &["CFBundleIdentifier", "CFBundleDisplayName", "CFBundleName", "UIFileSharingEnabled"]).await?;
    let mut apps: Vec<AppInfo> = raw.iter().filter_map(AppInfo::from_dict).filter(|a| a.file_sharing).collect();
    apps.sort_by_key(|a| a.display_name.to_lowercase());
    Ok(apps)
}
