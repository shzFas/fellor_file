use crate::codes::{op, resp};
use fellor_vfs::VfsError;
use thiserror::Error;

/// Low-level USB transport failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransportError {
    #[error("timeout")]
    Timeout,
    #[error("endpoint stalled")]
    Stall,
    #[error("device disconnected")]
    Disconnected,
    #[error("interface busy (claimed by another program)")]
    Busy,
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("usb: {0}")]
    Other(String),
}

#[derive(Debug, Error)]
pub enum PtpError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    /// Device answered with a non-OK response code.
    #[error("{} -> {} (0x{code:04X})", op::name(*op), resp::name(*code))]
    Response { op: u16, code: u16, params: Vec<u32> },
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("session is not open")]
    NoSession,
    #[error("cancelled")]
    Cancelled,
}

impl PtpError {
    pub fn protocol(msg: impl Into<String>) -> Self {
        PtpError::Protocol(msg.into())
    }

    /// Response code if this is a device response error.
    pub fn response_code(&self) -> Option<u16> {
        match self {
            PtpError::Response { code, .. } => Some(*code),
            _ => None,
        }
    }

    pub fn is_not_supported(&self) -> bool {
        matches!(
            self.response_code(),
            Some(
                resp::OPERATION_NOT_SUPPORTED
                    | resp::PARAMETER_NOT_SUPPORTED
                    | resp::SPECIFICATION_BY_DEPTH_UNSUPPORTED
                    | resp::SPECIFICATION_BY_GROUP_UNSUPPORTED
                    | resp::SPECIFICATION_BY_FORMAT_UNSUPPORTED
                    | resp::OBJECT_PROP_NOT_SUPPORTED
                    | resp::INVALID_OBJECT_PROP_CODE
                    | resp::GROUP_NOT_SUPPORTED
            )
        )
    }
}

pub type PtpResult<T> = Result<T, PtpError>;

impl From<TransportError> for VfsError {
    fn from(e: TransportError) -> Self {
        match e {
            TransportError::Timeout => VfsError::Timeout,
            TransportError::Disconnected => VfsError::Disconnected,
            TransportError::Busy => VfsError::Busy(None),
            TransportError::Unsupported(s) => VfsError::Unsupported(s),
            other => VfsError::Transport(other.to_string()),
        }
    }
}

impl From<PtpError> for VfsError {
    fn from(e: PtpError) -> Self {
        match e {
            PtpError::Transport(t) => t.into(),
            PtpError::Protocol(s) => VfsError::Protocol(s),
            PtpError::NoSession => VfsError::Transport("MTP session is not open".into()),
            PtpError::Cancelled => VfsError::Cancelled,
            PtpError::Response { op, code, .. } => {
                let what = format!("{} ({})", op::name(op), resp::name(code));
                match code {
                    resp::INVALID_OBJECT_HANDLE | resp::INVALID_PARENT_OBJECT => VfsError::NotFound(what),
                    resp::INVALID_STORAGE_ID | resp::STORE_NOT_AVAILABLE => VfsError::Locked,
                    resp::STORE_FULL | resp::OBJECT_TOO_LARGE => VfsError::NoSpace,
                    resp::ACCESS_DENIED | resp::OBJECT_WRITE_PROTECTED | resp::STORE_READ_ONLY => {
                        VfsError::PermissionDenied(what)
                    }
                    resp::OPERATION_NOT_SUPPORTED => VfsError::Unsupported(op::name(op)),
                    resp::DEVICE_BUSY => VfsError::Busy(None),
                    resp::TRANSACTION_CANCELLED => VfsError::Cancelled,
                    resp::SESSION_NOT_OPEN | resp::INCOMPLETE_TRANSFER | resp::INVALID_TRANSACTION_ID => {
                        VfsError::Transport(what)
                    }
                    _ => VfsError::Protocol(what),
                }
            }
        }
    }
}
