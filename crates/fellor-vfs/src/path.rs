use crate::BackendId;
use std::fmt;

/// Backend-qualified absolute path. Components never contain '/'.
/// Display form: `ios:ABCD/DCIM/100APPLE/IMG_0001.HEIC`, `local:/Users/x`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VPath {
    pub backend: BackendId,
    pub components: Vec<String>,
}

impl VPath {
    pub fn root(backend: impl Into<BackendId>) -> Self {
        Self { backend: backend.into(), components: Vec::new() }
    }

    pub fn new(backend: impl Into<BackendId>, components: Vec<String>) -> Self {
        Self { backend: backend.into(), components }
    }

    /// Build from a '/'-separated string relative to the backend root.
    pub fn from_str_path(backend: impl Into<BackendId>, path: &str) -> Self {
        Self {
            backend: backend.into(),
            components: path.split('/').filter(|c| !c.is_empty() && *c != ".").map(str::to_owned).collect(),
        }
    }

    /// Parse display form `backend:/a/b` (backend ids may contain ':',
    /// e.g. `ios:UDID:/DCIM`). A bare `backend:` means the root.
    pub fn parse(s: &str) -> Option<Self> {
        if let Some(idx) = s.find(":/") {
            let backend = &s[..idx];
            if backend.is_empty() {
                return None;
            }
            return Some(Self::from_str_path(backend, &s[idx + 1..]));
        }
        let backend = s.strip_suffix(':')?;
        (!backend.is_empty()).then(|| Self::root(backend))
    }

    pub fn is_root(&self) -> bool {
        self.components.is_empty()
    }

    pub fn name(&self) -> &str {
        self.components.last().map(String::as_str).unwrap_or("")
    }

    pub fn parent(&self) -> Option<VPath> {
        if self.components.is_empty() {
            return None;
        }
        let mut c = self.components.clone();
        c.pop();
        Some(Self { backend: self.backend.clone(), components: c })
    }

    pub fn join(&self, name: impl Into<String>) -> VPath {
        let mut c = self.components.clone();
        c.push(name.into());
        Self { backend: self.backend.clone(), components: c }
    }

    /// '/'-joined path with leading '/', e.g. "/DCIM/100APPLE".
    pub fn as_unix(&self) -> String {
        format!("/{}", self.components.join("/"))
    }

    pub fn starts_with(&self, other: &VPath) -> bool {
        self.backend == other.backend && self.components.starts_with(&other.components)
    }
}

impl fmt::Display for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.backend, self.as_unix())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let p = VPath::from_str_path("ios:ABCD", "/DCIM/100APPLE/IMG_1.HEIC");
        assert_eq!(p.to_string(), "ios:ABCD:/DCIM/100APPLE/IMG_1.HEIC");
        assert_eq!(p.name(), "IMG_1.HEIC");
        assert_eq!(p.parent().unwrap().as_unix(), "/DCIM/100APPLE");
        let l = VPath::parse("local:/Users/x").unwrap();
        assert_eq!(l.backend, "local");
        assert_eq!(l.components, vec!["Users", "x"]);
        let i = VPath::parse("ios:ABCD:/DCIM").unwrap();
        assert_eq!(i.backend, "ios:ABCD");
        assert_eq!(i.components, vec!["DCIM"]);
    }
}
