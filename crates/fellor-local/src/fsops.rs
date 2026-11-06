use fellor_vfs::{SpaceInfo, VfsError};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

fn cstr(p: &Path) -> std::io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))
}

/// Random-ish suffix for temp names (pid + time + counter; no RNG dep needed).
pub fn rand_suffix() -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let mut x = t ^ (u64::from(std::process::id()) << 32) ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    format!("{:012x}", x & 0xffff_ffff_ffff)
}

/// Rename `from` → `to`, failing with `AlreadyExists` if `to` exists.
/// Uses `renamex_np(RENAME_EXCL)` on macOS (atomic); falls back to
/// check-then-rename on filesystems that don't support it (FAT/exFAT/SMB).
pub fn rename_noclobber(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let (a, b) = (cstr(from)?, cstr(to)?);
        // SAFETY: both pointers are valid NUL-terminated strings for the call.
        let rc = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_EXCL) };
        if rc == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ENOTSUP) | Some(libc::EINVAL) | Some(libc::ENOSYS) => {}
            _ => return Err(err),
        }
    }
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(std::io::Error::from_raw_os_error(libc::EEXIST));
    }
    std::fs::rename(from, to)
}

pub fn statvfs(p: &Path) -> Result<SpaceInfo, VfsError> {
    let c = cstr(p).map_err(VfsError::Io)?;
    // SAFETY: statvfs is a plain C struct; all-zero is a valid bit pattern.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c is a valid C string, st is a valid out-pointer.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(crate::map_io(std::io::Error::last_os_error(), p));
    }
    let frsize = if st.f_frsize > 0 { st.f_frsize as u64 } else { st.f_bsize as u64 };
    Ok(SpaceInfo { total: st.f_blocks as u64 * frsize, free: st.f_bavail as u64 * frsize })
}
