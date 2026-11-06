//! AFC status codes (reply op 1) → [`VfsError`].

use fellor_vfs::VfsError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfcStatus {
    Success,
    UnknownError,
    OpHeaderInvalid,
    NoResources,
    ReadError,
    WriteError,
    UnknownPacketType,
    InvalidArg,
    ObjectNotFound,
    ObjectIsDir,
    PermDenied,
    ServiceNotConnected,
    OpTimeout,
    TooMuchData,
    EndOfData,
    OpNotSupported,
    ObjectExists,
    ObjectBusy,
    NoSpaceLeft,
    OpWouldBlock,
    IoError,
    OpInterrupted,
    OpInProgress,
    InternalError,
    MuxError,
    NoMem,
    NotEnoughData,
    DirNotEmpty,
    Other(u64),
}

impl AfcStatus {
    pub fn from_code(c: u64) -> Self {
        use AfcStatus::*;
        match c {
            0 => Success,
            1 => UnknownError,
            2 => OpHeaderInvalid,
            3 => NoResources,
            4 => ReadError,
            5 => WriteError,
            6 => UnknownPacketType,
            7 => InvalidArg,
            8 => ObjectNotFound,
            9 => ObjectIsDir,
            10 => PermDenied,
            11 => ServiceNotConnected,
            12 => OpTimeout,
            13 => TooMuchData,
            14 => EndOfData,
            15 => OpNotSupported,
            16 => ObjectExists,
            17 => ObjectBusy,
            18 => NoSpaceLeft,
            19 => OpWouldBlock,
            20 => IoError,
            21 => OpInterrupted,
            22 => OpInProgress,
            23 => InternalError,
            30 => MuxError,
            31 => NoMem,
            32 => NotEnoughData,
            33 => DirNotEmpty,
            n => Other(n),
        }
    }

    /// Map to a VfsError; `path` gives context for path-related errors.
    pub fn into_error(self, path: &str) -> VfsError {
        use AfcStatus::*;
        let p = || path.to_owned();
        match self {
            Success => VfsError::Other("AFC: success reported as error".into()),
            ObjectNotFound => VfsError::NotFound(p()),
            ObjectIsDir => VfsError::IsDir(p()),
            PermDenied => VfsError::PermissionDenied(p()),
            ObjectExists => VfsError::AlreadyExists(p()),
            DirNotEmpty => VfsError::NotEmpty(p()),
            NoSpaceLeft => VfsError::NoSpace,
            ObjectBusy | OpWouldBlock | OpInProgress => VfsError::Busy(None),
            OpTimeout => VfsError::Timeout,
            ServiceNotConnected => VfsError::Disconnected,
            MuxError | OpInterrupted => VfsError::Transport(format!("AFC {self:?}: {path}")),
            OpNotSupported => VfsError::Unsupported("AFC operation"),
            OpHeaderInvalid | UnknownPacketType | InvalidArg | TooMuchData | NotEnoughData => {
                VfsError::Protocol(format!("AFC {self:?}: {path}"))
            }
            UnknownError | NoResources | ReadError | WriteError | EndOfData | IoError | InternalError | NoMem => {
                VfsError::Other(format!("AFC {self:?}: {path}"))
            }
            Other(n) => VfsError::Other(format!("AFC error {n}: {path}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_map() {
        let e = |c| AfcStatus::from_code(c).into_error("/x");
        assert!(matches!(e(8), VfsError::NotFound(p) if p == "/x"));
        assert!(matches!(e(9), VfsError::IsDir(_)));
        assert!(matches!(e(10), VfsError::PermissionDenied(_)));
        assert!(matches!(e(16), VfsError::AlreadyExists(_)));
        assert!(matches!(e(18), VfsError::NoSpace));
        assert!(matches!(e(33), VfsError::NotEmpty(_)));
        assert!(matches!(e(17), VfsError::Busy(None)));
        assert!(matches!(e(12), VfsError::Timeout));
        assert!(matches!(e(11), VfsError::Disconnected));
        assert!(matches!(e(30), VfsError::Transport(_)));
        assert!(matches!(e(15), VfsError::Unsupported(_)));
        assert!(matches!(e(7), VfsError::Protocol(_)));
        assert!(matches!(e(1), VfsError::Other(_)));
        assert!(matches!(e(999), VfsError::Other(_)));
        assert_eq!(AfcStatus::from_code(0), AfcStatus::Success);
        assert_eq!(AfcStatus::from_code(24), AfcStatus::Other(24));
    }
}
