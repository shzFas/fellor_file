use fellor_vfs::VfsError;
use std::io::ErrorKind;
use std::path::Path;

/// Map a host I/O error to a [`VfsError`] with a human-readable context.
///
/// macOS TCC (privacy) denials surface as `EPERM` on folders such as
/// ~/Desktop, ~/Documents, ~/Downloads, ~/Pictures or removable volumes; we
/// add a hint telling the user where to grant access.
pub fn map_io(e: std::io::Error, path: &Path) -> VfsError {
    let p = path.display().to_string();
    if let Some(code) = e.raw_os_error() {
        match code {
            libc::EPERM => {
                return VfsError::PermissionDenied(format!(
                    "{p} — macOS не разрешает доступ. Откройте Системные настройки › Конфиденциальность и безопасность › \
                     Файлы и папки (или «Доступ к диску») и разрешите доступ для Fellor"
                ))
            }
            libc::EACCES => return VfsError::PermissionDenied(p),
            libc::ENOSPC | libc::EDQUOT => return VfsError::NoSpace,
            libc::ENOTEMPTY => return VfsError::NotEmpty(p),
            libc::EEXIST => return VfsError::AlreadyExists(p),
            libc::ENOENT => return VfsError::NotFound(p),
            libc::EISDIR => return VfsError::IsDir(p),
            libc::ENOTDIR => return VfsError::NotDir(p),
            libc::ENXIO | libc::ENODEV => return VfsError::Disconnected,
            libc::EBUSY => return VfsError::Busy(None),
            libc::ETIMEDOUT => return VfsError::Timeout,
            libc::EROFS => return VfsError::PermissionDenied(format!("{p} — том доступен только для чтения")),
            libc::ENAMETOOLONG => return VfsError::Other(format!("слишком длинное имя: {p}")),
            _ => {}
        }
    }
    match e.kind() {
        ErrorKind::NotFound => VfsError::NotFound(p),
        ErrorKind::PermissionDenied => VfsError::PermissionDenied(p),
        ErrorKind::AlreadyExists => VfsError::AlreadyExists(p),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => VfsError::NoSpace,
        ErrorKind::DirectoryNotEmpty => VfsError::NotEmpty(p),
        ErrorKind::IsADirectory => VfsError::IsDir(p),
        ErrorKind::NotADirectory => VfsError::NotDir(p),
        ErrorKind::TimedOut => VfsError::Timeout,
        ErrorKind::ReadOnlyFilesystem => VfsError::PermissionDenied(format!("{p} — том доступен только для чтения")),
        _ => VfsError::Io(e),
    }
}
