//! lockdownd `Error` strings → [`VfsError`].

use fellor_vfs::VfsError;

/// Map the `Error` value of a lockdown reply. `ctx` is the request name.
pub fn map_lockdown_error(err: &str, ctx: &str) -> VfsError {
    match err {
        "PasswordProtected" | "DeviceLocked" => VfsError::Locked,
        "InvalidHostID" | "InvalidPairRecord" | "PairingDialogResponsePending" | "UserDeniedPairing" | "NoHostKey"
        | "InvalidHostCertificate" | "PairingProhibitedOverThisConnection" | "NotPaired" => VfsError::NeedsTrust,
        "SessionInactive" => VfsError::Transport(format!("lockdown {ctx}: session inactive")),
        "InvalidService" | "ServiceProhibited" | "ServiceLimit" => {
            VfsError::PermissionDenied(format!("lockdown {ctx}: {err}"))
        }
        "MissingValue" | "MissingKey" | "MissingService" => VfsError::NotFound(format!("lockdown {ctx}: {err}")),
        "TimeoutError" => VfsError::Timeout,
        "MuxError" | "ReceiveMessageFailed" | "SendMessageFailed" | "SSLError" => {
            VfsError::Transport(format!("lockdown {ctx}: {err}"))
        }
        other => VfsError::Protocol(format!("lockdown {ctx}: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping() {
        assert!(matches!(map_lockdown_error("PasswordProtected", "x"), VfsError::Locked));
        assert!(matches!(map_lockdown_error("InvalidHostID", "x"), VfsError::NeedsTrust));
        assert!(matches!(map_lockdown_error("InvalidPairRecord", "x"), VfsError::NeedsTrust));
        assert!(matches!(map_lockdown_error("UserDeniedPairing", "x"), VfsError::NeedsTrust));
        assert!(matches!(map_lockdown_error("PairingDialogResponsePending", "x"), VfsError::NeedsTrust));
        assert!(matches!(map_lockdown_error("SessionInactive", "x"), VfsError::Transport(_)));
        assert!(matches!(map_lockdown_error("InvalidService", "x"), VfsError::PermissionDenied(_)));
        assert!(matches!(map_lockdown_error("MissingValue", "x"), VfsError::NotFound(_)));
        assert!(matches!(map_lockdown_error("Wat", "x"), VfsError::Protocol(_)));
    }
}
