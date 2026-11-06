/// `"IMG_0001.HEIC", 2` → `"IMG_0001 (2).HEIC"`; `"README", 3` → `"README (3)"`;
/// dotfiles keep their leading dot: `".env", 2` → `".env (2)"`.
pub fn numbered_name(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => format!("{} ({n}){}", &name[..i], &name[i..]),
        _ => format!("{name} ({n})"),
    }
}

/// Copy chunk size for a backend: 1 MiB AFC, 4 MiB MTP, 8 MiB local/volumes.
pub fn chunk_size_for(backend: &str) -> usize {
    if backend.starts_with("ios:") {
        1 << 20
    } else if backend.starts_with("mtp:") {
        4 << 20
    } else {
        8 << 20
    }
}

/// Devices are everything except the host FS and mounted volumes; a copy
/// between two devices is staged through a local temp file.
pub fn is_device_backend(backend: &str) -> bool {
    !(backend == "local" || backend.starts_with("vol:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbering() {
        assert_eq!(numbered_name("IMG_0001.HEIC", 2), "IMG_0001 (2).HEIC");
        assert_eq!(numbered_name("a.tar.gz", 3), "a.tar (3).gz");
        assert_eq!(numbered_name("README", 2), "README (2)");
        assert_eq!(numbered_name(".env", 2), ".env (2)");
        assert_eq!(numbered_name("trailing.", 2), "trailing. (2)");
    }

    #[test]
    fn chunks() {
        assert_eq!(chunk_size_for("ios:abc"), 1 << 20);
        assert_eq!(chunk_size_for("mtp:abc"), 4 << 20);
        assert_eq!(chunk_size_for("local"), 8 << 20);
        assert!(!is_device_backend("vol:USB"));
        assert!(is_device_backend("ios:x"));
    }
}
