//! How the backend obtains AFC sessions. The real implementation goes
//! through usbmuxd + lockdown; tests plug in an in-memory AFC server.

use async_trait::async_trait;
use fellor_afc::{house_arrest, installation_proxy, AfcClient, AppInfo, AFC_SERVICE};
use fellor_lockdown::Device;
use fellor_vfs::Result;

/// Which AFC namespace a path lives in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AfcTarget {
    /// `com.apple.afc` — `/var/mobile/Media`.
    Media,
    /// house_arrest `VendDocuments` for this bundle id.
    App(String),
}

#[async_trait]
pub trait Connector: Send + Sync + 'static {
    async fn open_afc(&self, target: &AfcTarget) -> Result<AfcClient>;
    /// User apps with File Sharing enabled.
    async fn list_apps(&self) -> Result<Vec<AppInfo>>;
}

/// Real device: every call starts a lockdown session and the service.
pub struct LockdownConnector {
    pub device: Device,
}

#[async_trait]
impl Connector for LockdownConnector {
    async fn open_afc(&self, target: &AfcTarget) -> Result<AfcClient> {
        match target {
            AfcTarget::Media => Ok(AfcClient::new(self.device.start_service(AFC_SERVICE).await?)),
            AfcTarget::App(bundle) => {
                let io = self.device.start_service(house_arrest::SERVICE).await?;
                house_arrest::vend_documents(io, bundle).await
            }
        }
    }

    async fn list_apps(&self) -> Result<Vec<AppInfo>> {
        let io = self.device.start_service(installation_proxy::SERVICE).await?;
        installation_proxy::file_sharing_apps(io).await
    }
}
