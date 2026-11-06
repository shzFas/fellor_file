use thiserror::Error;

pub type Result<T, E = VfsError> = std::result::Result<T, E>;

/// Errors carry enough meaning for the UI to show a human hint and for the
/// transfer engine to decide whether to retry.
#[derive(Debug, Error)]
pub enum VfsError {
    #[error("не найдено: {0}")]
    NotFound(String),
    #[error("уже существует: {0}")]
    AlreadyExists(String),
    #[error("нет доступа: {0}")]
    PermissionDenied(String),
    #[error("недостаточно места")]
    NoSpace,
    #[error("это папка: {0}")]
    IsDir(String),
    #[error("это не папка: {0}")]
    NotDir(String),
    #[error("папка не пуста: {0}")]
    NotEmpty(String),
    #[error("операция не поддерживается: {0}")]
    Unsupported(&'static str),
    #[error("устройство отключено")]
    Disconnected,
    #[error("устройство заблокировано — разблокируйте его")]
    Locked,
    #[error("нажмите «Доверять этому компьютеру» на устройстве")]
    NeedsTrust,
    #[error("устройство занято другой программой{}", .0.as_ref().map(|s| format!(": {s}")).unwrap_or_default())]
    Busy(Option<String>),
    #[error("отменено")]
    Cancelled,
    #[error("таймаут")]
    Timeout,
    /// Transport-level failure (USB stall, socket reset) — retryable.
    #[error("ошибка связи: {0}")]
    Transport(String),
    /// Protocol violation / unexpected reply.
    #[error("ошибка протокола: {0}")]
    Protocol(String),
    #[error("ошибка ввода-вывода: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl VfsError {
    /// True for errors the transfer engine should retry after reconnecting.
    pub fn is_retryable(&self) -> bool {
        matches!(self, VfsError::Transport(_) | VfsError::Timeout | VfsError::Disconnected | VfsError::Busy(_))
    }
}
