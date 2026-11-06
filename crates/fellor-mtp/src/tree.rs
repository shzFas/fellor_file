//! Path → object-handle cache. MTP addresses objects by handle and parent
//! handle only; we mirror the parts of the hierarchy the user has looked at.
//!
//! A directory is identified by `(storage, handle)` where handle 0 is the
//! storage root. A directory whose listing is cached has an entry in
//! `dirs`; absent means "not listed yet / invalidated".

use std::collections::{BTreeMap, HashMap};
use std::time::SystemTime;

pub type DirKey = (u32, u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub storage: u32,
    /// Parent handle, 0 = storage root.
    pub parent: u32,
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub mtime: Option<SystemTime>,
    pub ctime: Option<SystemTime>,
    pub format: u16,
    pub hidden: bool,
}

#[derive(Debug, Default)]
pub struct Tree {
    nodes: HashMap<u32, Node>,
    dirs: HashMap<DirKey, BTreeMap<String, u32>>,
}

impl Tree {
    pub fn node(&self, handle: u32) -> Option<&Node> {
        self.nodes.get(&handle)
    }

    pub fn is_listed(&self, dir: DirKey) -> bool {
        self.dirs.contains_key(&dir)
    }

    pub fn lookup(&self, dir: DirKey, name: &str) -> Option<u32> {
        self.dirs.get(&dir)?.get(name).copied()
    }

    /// Handles in a listed directory, sorted by name.
    pub fn children(&self, dir: DirKey) -> Option<Vec<u32>> {
        Some(self.dirs.get(&dir)?.values().copied().collect())
    }

    /// Replace a directory's listing. Objects that disappeared are dropped
    /// together with their cached subtrees. Duplicate names (MTP allows
    /// them) are disambiguated with a ` (handle)` suffix so every object
    /// stays addressable by path.
    pub fn set_listing(&mut self, dir: DirKey, items: Vec<(u32, Node)>) -> Vec<(u32, String)> {
        let old: Vec<u32> = self.dirs.get(&dir).map(|m| m.values().copied().collect()).unwrap_or_default();
        let new_handles: std::collections::HashSet<u32> = items.iter().map(|(h, _)| *h).collect();
        for h in old {
            if !new_handles.contains(&h) {
                self.remove(h);
            }
        }
        let mut map = BTreeMap::new();
        let mut names = Vec::with_capacity(items.len());
        for (h, mut n) in items {
            if map.contains_key(&n.name) {
                n.name = format!("{} ({h})", n.name);
            }
            // A directory that changed into a file (or moved) loses its cache.
            if let Some(prev) = self.nodes.get(&h) {
                if prev.is_dir && !n.is_dir {
                    self.drop_subtree((prev.storage, h));
                }
            }
            map.insert(n.name.clone(), h);
            names.push((h, n.name.clone()));
            self.nodes.insert(h, n);
        }
        self.dirs.insert(dir, map);
        names
    }

    /// Add one object (after our own upload/mkdir or an ObjectAdded event).
    /// Returns the (possibly disambiguated) name.
    pub fn insert(&mut self, handle: u32, mut node: Node) -> String {
        let dir = (node.storage, node.parent);
        if let Some(m) = self.dirs.get_mut(&dir) {
            if m.get(&node.name).is_some_and(|&h| h != handle) {
                node.name = format!("{} ({handle})", node.name);
            }
            m.insert(node.name.clone(), handle);
        }
        let name = node.name.clone();
        self.nodes.insert(handle, node);
        name
    }

    /// Forget an object and anything cached below it.
    pub fn remove(&mut self, handle: u32) -> Option<Node> {
        let n = self.nodes.remove(&handle)?;
        if let Some(m) = self.dirs.get_mut(&(n.storage, n.parent)) {
            if m.get(&n.name) == Some(&handle) {
                m.remove(&n.name);
            }
        }
        if n.is_dir {
            self.drop_subtree((n.storage, handle));
        }
        Some(n)
    }

    fn drop_subtree(&mut self, dir: DirKey) {
        if let Some(m) = self.dirs.remove(&dir) {
            for h in m.into_values() {
                if let Some(child) = self.nodes.remove(&h) {
                    if child.is_dir {
                        self.drop_subtree((child.storage, h));
                    }
                }
            }
        }
    }

    pub fn rename(&mut self, handle: u32, new_name: &str) {
        let Some(n) = self.nodes.get_mut(&handle) else { return };
        let dir = (n.storage, n.parent);
        let old = std::mem::replace(&mut n.name, new_name.to_owned());
        if let Some(m) = self.dirs.get_mut(&dir) {
            if m.get(&old) == Some(&handle) {
                m.remove(&old);
            }
            m.insert(new_name.to_owned(), handle);
        }
    }

    /// Re-parent an object (MoveObject). Its own cached listing stays valid
    /// within the same storage; across storages it is dropped.
    pub fn reparent(&mut self, handle: u32, storage: u32, parent: u32) {
        let Some(mut n) = self.nodes.remove(&handle) else { return };
        if let Some(m) = self.dirs.get_mut(&(n.storage, n.parent)) {
            if m.get(&n.name) == Some(&handle) {
                m.remove(&n.name);
            }
        }
        if n.is_dir && n.storage != storage {
            self.drop_subtree((n.storage, handle));
        }
        n.storage = storage;
        n.parent = parent;
        self.insert(handle, n);
    }

    /// Mark a directory as needing a fresh listing (its nodes stay cached
    /// so paths keep resolving until the next listing replaces them).
    pub fn invalidate_dir(&mut self, dir: DirKey) {
        self.dirs.remove(&dir);
    }

    pub fn drop_storage(&mut self, storage: u32) {
        self.nodes.retain(|_, n| n.storage != storage);
        self.dirs.retain(|k, _| k.0 != storage);
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
        self.dirs.clear();
    }

    /// Path components below the storage root, e.g. `["DCIM", "Camera"]`.
    pub fn components_of(&self, handle: u32) -> Option<Vec<String>> {
        let mut out = Vec::new();
        let mut h = handle;
        for _ in 0..4096 {
            let n = self.nodes.get(&h)?;
            out.push(n.name.clone());
            if n.parent == 0 {
                out.reverse();
                return Some(out);
            }
            h = n.parent;
        }
        None // cycle guard
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(storage: u32, parent: u32, name: &str, is_dir: bool) -> Node {
        Node { storage, parent, name: name.into(), is_dir, size: None, mtime: None, ctime: None, format: 0, hidden: false }
    }

    #[test]
    fn listing_lookup_and_paths() {
        let mut t = Tree::default();
        t.set_listing((1, 0), vec![(10, n(1, 0, "DCIM", true)), (11, n(1, 0, "a.txt", false))]);
        t.set_listing((1, 10), vec![(20, n(1, 10, "Camera", true))]);
        t.set_listing((1, 20), vec![(30, n(1, 20, "IMG.jpg", false))]);
        assert_eq!(t.lookup((1, 0), "DCIM"), Some(10));
        assert_eq!(t.components_of(30).unwrap(), vec!["DCIM", "Camera", "IMG.jpg"]);
        assert!(t.is_listed((1, 20)));
        // Relisting the root without DCIM drops the whole subtree.
        t.set_listing((1, 0), vec![(11, n(1, 0, "a.txt", false))]);
        assert!(t.node(30).is_none());
        assert!(!t.is_listed((1, 20)));
        assert!(!t.is_listed((1, 10)));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn duplicate_names_disambiguated() {
        let mut t = Tree::default();
        let names = t.set_listing((1, 0), vec![(5, n(1, 0, "x", false)), (6, n(1, 0, "x", false))]);
        assert_eq!(names[1].1, "x (6)");
        assert_eq!(t.lookup((1, 0), "x (6)"), Some(6));
        assert_eq!(t.insert(7, n(1, 0, "x", false)), "x (7)");
    }

    #[test]
    fn rename_reparent_remove() {
        let mut t = Tree::default();
        t.set_listing((1, 0), vec![(10, n(1, 0, "A", true)), (11, n(1, 0, "f", false))]);
        t.set_listing((1, 10), vec![]);
        t.rename(11, "g");
        assert_eq!(t.lookup((1, 0), "g"), Some(11));
        assert_eq!(t.lookup((1, 0), "f"), None);
        t.reparent(11, 1, 10);
        assert_eq!(t.lookup((1, 10), "g"), Some(11));
        assert_eq!(t.lookup((1, 0), "g"), None);
        assert_eq!(t.components_of(11).unwrap(), vec!["A", "g"]);
        t.remove(10);
        assert!(t.node(11).is_none());
        assert_eq!(t.lookup((1, 0), "A"), None);
    }

    #[test]
    fn insert_into_unlisted_dir_keeps_node_only() {
        let mut t = Tree::default();
        t.insert(3, n(2, 0, "z", false));
        assert!(!t.is_listed((2, 0)));
        assert!(t.node(3).is_some());
        t.drop_storage(2);
        assert!(t.is_empty());
    }
}
