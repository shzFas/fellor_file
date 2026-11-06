//! Command-line path syntax → VPath.
//!
//! * `~/x`, `/x`, `./x`, `x`          → `local` (or `vol:<name>` under /Volumes/<name>)
//! * `ios:/DCIM`, `mtp:/Internal…`    → first backend whose id starts with `ios:` / `mtp:`
//! * `ios:<udid>:/DCIM`, `vol:USB:/x` → that exact backend (a unique id prefix works too)
//! * `local:/Users/x`                 → explicit local path
use fellor_vfs::{BackendRegistry, VPath};
use std::path::{Component, Path, PathBuf};

const PREFIXES: [&str; 4] = ["ios:", "mtp:", "vol:", "local:"];

/// A parsed argument before the backend is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathArg {
    /// Backend hint ("ios", "ios:UDID", "vol:USB", "local") + path.
    Device(VPath),
    Host(PathBuf),
}

pub fn parse(arg: &str) -> PathArg {
    if PREFIXES.iter().any(|p| arg.starts_with(p)) {
        let v = VPath::parse(arg).unwrap_or_else(|| VPath::root(arg));
        if v.backend == "local" {
            return PathArg::Host(PathBuf::from(v.as_unix()));
        }
        return PathArg::Device(v);
    }
    PathArg::Host(host_abs(arg))
}

/// Expand `~`, make absolute against the cwd and normalise `.`/`..` lexically.
pub fn host_abs(arg: &str) -> PathBuf {
    let p = if arg == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
    } else if let Some(rest) = arg.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")).join(rest)
    } else {
        PathBuf::from(arg)
    };
    let p = if p.is_absolute() { p } else { std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(p) };
    normalize(&p)
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for c in p.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    out
}

/// Backend id for a hint: exact match, else the first id (sorted) that
/// starts with `hint:` or `hint`.
pub fn resolve_backend(reg: &BackendRegistry, hint: &str) -> Option<String> {
    if reg.get(hint).is_some() {
        return Some(hint.to_owned());
    }
    let mut ids: Vec<String> = reg.all().iter().map(|b| b.id()).collect();
    ids.sort();
    let colon = format!("{hint}:");
    ids.iter().find(|id| id.starts_with(&colon)).or_else(|| ids.iter().find(|id| id.starts_with(hint))).cloned()
}

/// Resolve a parsed argument against the registry (None = backend not present).
pub fn resolve(reg: &BackendRegistry, arg: &PathArg) -> Option<VPath> {
    match arg {
        PathArg::Device(v) => resolve_backend(reg, &v.backend).map(|id| VPath::new(id, v.components.clone())),
        PathArg::Host(p) => {
            // /Volumes/<name>/… → vol:<name> when that volume backend exists.
            let v = fellor_local::vpath_from_host(p);
            if v.components.len() >= 2 && v.components[0] == "Volumes" {
                let id = format!("vol:{}", v.components[1]);
                if reg.get(&id).is_some() {
                    return Some(VPath::new(id, v.components[2..].to_vec()));
                }
            }
            reg.get("local").map(|_| v)
        }
    }
}

/// Backend hint an argument needs (for waiting on discovery).
pub fn hint(arg: &PathArg) -> String {
    match arg {
        PathArg::Device(v) => v.backend.clone(),
        PathArg::Host(_) => "local".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing() {
        assert_eq!(parse("ios:/DCIM/100APPLE"), PathArg::Device(VPath::from_str_path("ios", "/DCIM/100APPLE")));
        assert_eq!(parse("ios:ABCD:/DCIM"), PathArg::Device(VPath::from_str_path("ios:ABCD", "/DCIM")));
        assert_eq!(parse("mtp:"), PathArg::Device(VPath::root("mtp")));
        assert_eq!(parse("vol:USB"), PathArg::Device(VPath::root("vol:USB")));
        assert_eq!(parse("local:/Users/x"), PathArg::Host(PathBuf::from("/Users/x")));
        assert_eq!(parse("/a/b/../c/./d"), PathArg::Host(PathBuf::from("/a/c/d")));
        let home = dirs::home_dir().unwrap();
        assert_eq!(parse("~/Pictures"), PathArg::Host(home.join("Pictures")));
        assert_eq!(parse("rel"), PathArg::Host(std::env::current_dir().unwrap().join("rel")));
    }

    #[test]
    fn resolving() {
        let reg = BackendRegistry::new();
        reg.insert(fellor_local::LocalBackend::new());
        reg.insert(fellor_local::LocalBackend::rooted("ios:ZZZZ", "/tmp".into()));
        reg.insert(fellor_local::LocalBackend::rooted("ios:AAAA", "/tmp".into()));
        reg.insert(fellor_local::LocalBackend::volume("USB", "/Volumes/USB".into()));
        assert_eq!(resolve(&reg, &parse("ios:/DCIM")).unwrap().to_string(), "ios:AAAA:/DCIM");
        assert_eq!(resolve(&reg, &parse("ios:ZZ:/DCIM")).unwrap().to_string(), "ios:ZZZZ:/DCIM");
        assert!(resolve(&reg, &parse("mtp:/x")).is_none());
        assert_eq!(resolve(&reg, &parse("/Volumes/USB/DCIM")).unwrap().to_string(), "vol:USB:/DCIM");
        assert_eq!(resolve(&reg, &parse("/Users")).unwrap().to_string(), "local:/Users");
    }
}
