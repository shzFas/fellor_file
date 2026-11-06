//! Per-pane state: location, history, listing, sort, filter and selection.
//! Pure data — drawing lives in `panes.rs`, I/O in `app.rs`.

use fellor_vfs::{Entry, SpaceInfo, VPath};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Date,
}

pub struct Pane {
    pub path: VPath,
    back: Vec<VPath>,
    fwd: Vec<VPath>,
    pub entries: Vec<Entry>,
    /// Indices into `entries` after filtering and sorting.
    pub view: Vec<usize>,
    /// Selected entry names (unique within one directory).
    pub selected: HashSet<String>,
    /// Keyboard cursor, index into `view`.
    pub cursor: Option<usize>,
    anchor: Option<usize>,
    pub filter: String,
    pub sort: SortKey,
    pub ascending: bool,
    pub loading: bool,
    pub error: Option<String>,
    /// Id of the listing request whose answer we are waiting for.
    pub req: u64,
    pub space: Option<SpaceInfo>,
    pub scroll_to_cursor: bool,
    /// Select this name once the next listing arrives (new folder, rename).
    pub pending_select: Option<String>,
    /// The backend is not registered yet (or went away); reload on attach.
    pub waiting: bool,
    /// When going up, put the cursor on the folder we came from.
    came_from: Option<String>,
}

impl Pane {
    pub fn new(path: VPath) -> Self {
        Self {
            path,
            back: Vec::new(),
            fwd: Vec::new(),
            entries: Vec::new(),
            view: Vec::new(),
            selected: HashSet::new(),
            cursor: None,
            anchor: None,
            filter: String::new(),
            sort: SortKey::Name,
            ascending: true,
            loading: true,
            error: None,
            req: 0,
            space: None,
            scroll_to_cursor: false,
            pending_select: None,
            waiting: false,
            came_from: None,
        }
    }

    fn reset_for(&mut self, path: VPath) {
        self.path = path;
        self.entries.clear();
        self.view.clear();
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
        self.filter.clear();
        self.loading = true;
        self.error = None;
        self.space = None;
    }

    /// Go to `path`, recording history. Caller must start a listing.
    pub fn navigate(&mut self, path: VPath) {
        if path == self.path {
            return;
        }
        let old = std::mem::replace(&mut self.path, path.clone());
        self.back.push(old);
        self.fwd.clear();
        self.came_from = None;
        self.reset_for(path);
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }
    pub fn can_forward(&self) -> bool {
        !self.fwd.is_empty()
    }
    pub fn can_up(&self) -> bool {
        !self.path.is_root()
    }

    pub fn back(&mut self) -> bool {
        let Some(p) = self.back.pop() else { return false };
        self.fwd.push(self.path.clone());
        self.came_from = None;
        self.reset_for(p);
        true
    }

    pub fn forward(&mut self) -> bool {
        let Some(p) = self.fwd.pop() else { return false };
        self.back.push(self.path.clone());
        self.came_from = None;
        self.reset_for(p);
        true
    }

    pub fn up(&mut self) -> bool {
        let Some(parent) = self.path.parent() else { return false };
        let from = self.path.name().to_owned();
        self.navigate(parent);
        self.came_from = Some(from);
        true
    }

    /// Install a fresh listing, keeping the selection where names survive.
    pub fn set_entries(&mut self, entries: Vec<Entry>, show_hidden: bool) {
        let keep_cursor_name = self.cursor_entry().map(|e| e.name.clone());
        self.entries = entries;
        self.loading = false;
        self.error = None;
        let names: HashSet<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
        self.selected.retain(|n| names.contains(n.as_str()));
        self.rebuild_view(show_hidden);
        let target = self.pending_select.take().or_else(|| self.came_from.take()).or(keep_cursor_name);
        self.cursor = None;
        if let Some(t) = target {
            if let Some(pos) = self.view.iter().position(|&i| self.entries[i].name == t) {
                self.cursor = Some(pos);
                self.anchor = Some(pos);
                if self.selected.is_empty() {
                    self.selected.insert(t);
                }
                self.scroll_to_cursor = true;
            }
        }
    }

    pub fn set_error(&mut self, err: String) {
        self.loading = false;
        self.error = Some(err);
        self.entries.clear();
        self.view.clear();
        self.selected.clear();
        self.cursor = None;
    }

    pub fn rebuild_view(&mut self, show_hidden: bool) {
        let needle = self.filter.to_lowercase();
        let mut view: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                (show_hidden || !(e.hidden || e.name.starts_with('.')))
                    && (needle.is_empty() || e.name.to_lowercase().contains(&needle))
            })
            .collect();
        let entries = &self.entries;
        let (key, asc) = (self.sort, self.ascending);
        view.sort_by(|&a, &b| {
            let (ea, eb) = (&entries[a], &entries[b]);
            // Directories always first, regardless of direction.
            eb.is_dir().cmp(&ea.is_dir()).then_with(|| {
                let ord = match key {
                    SortKey::Name => natural_cmp(&ea.name, &eb.name),
                    SortKey::Size => ea.size.cmp(&eb.size).then_with(|| natural_cmp(&ea.name, &eb.name)),
                    SortKey::Date => ea.mtime.cmp(&eb.mtime).then_with(|| natural_cmp(&ea.name, &eb.name)),
                };
                if asc {
                    ord
                } else {
                    ord.reverse()
                }
            })
        });
        // Hidden-by-filter entries leave the selection.
        let visible: HashSet<&str> = view.iter().map(|&i| self.entries[i].name.as_str()).collect();
        self.selected.retain(|n| visible.contains(n.as_str()));
        self.view = view;
        if let Some(c) = self.cursor {
            if c >= self.view.len() {
                self.cursor = self.view.len().checked_sub(1);
            }
        }
    }

    pub fn set_sort(&mut self, key: SortKey, show_hidden: bool) {
        if self.sort == key {
            self.ascending = !self.ascending;
        } else {
            self.sort = key;
            self.ascending = true;
        }
        let cur = self.cursor_entry().map(|e| e.name.clone());
        self.rebuild_view(show_hidden);
        self.cursor = cur.and_then(|n| self.view.iter().position(|&i| self.entries[i].name == n));
    }

    pub fn entry_at(&self, view_idx: usize) -> Option<&Entry> {
        self.view.get(view_idx).map(|&i| &self.entries[i])
    }

    pub fn cursor_entry(&self) -> Option<&Entry> {
        self.cursor.and_then(|c| self.entry_at(c))
    }

    pub fn is_selected(&self, view_idx: usize) -> bool {
        self.entry_at(view_idx).is_some_and(|e| self.selected.contains(&e.name))
    }

    /// Mouse click with modifiers (`command` = ⌘, `shift` = ⇧).
    pub fn click(&mut self, view_idx: usize, command: bool, shift: bool) {
        let Some(name) = self.entry_at(view_idx).map(|e| e.name.clone()) else { return };
        if shift {
            let a = self.anchor.unwrap_or(view_idx);
            let (lo, hi) = if a <= view_idx { (a, view_idx) } else { (view_idx, a) };
            if !command {
                self.selected.clear();
            }
            for i in lo..=hi {
                if let Some(e) = self.entry_at(i) {
                    self.selected.insert(e.name.clone());
                }
            }
        } else if command {
            if !self.selected.remove(&name) {
                self.selected.insert(name);
            }
            self.anchor = Some(view_idx);
        } else {
            self.selected.clear();
            self.selected.insert(name);
            self.anchor = Some(view_idx);
        }
        self.cursor = Some(view_idx);
    }

    /// Make sure the item under a right-click is part of the selection.
    pub fn ensure_selected(&mut self, view_idx: usize) {
        if !self.is_selected(view_idx) {
            self.click(view_idx, false, false);
        }
    }

    /// Keyboard ↑/↓ (delta = ±1, or a page).
    pub fn move_cursor(&mut self, delta: isize, shift: bool) {
        if self.view.is_empty() {
            return;
        }
        let last = self.view.len() as isize - 1;
        let next = match self.cursor {
            None => {
                if delta > 0 {
                    0
                } else {
                    last
                }
            }
            Some(c) => (c as isize + delta).clamp(0, last),
        } as usize;
        if shift {
            if self.anchor.is_none() {
                self.anchor = self.cursor.or(Some(next));
            }
            let a = self.anchor.unwrap_or(next);
            let (lo, hi) = if a <= next { (a, next) } else { (next, a) };
            self.selected.clear();
            for i in lo..=hi {
                if let Some(e) = self.entry_at(i) {
                    self.selected.insert(e.name.clone());
                }
            }
            self.cursor = Some(next);
        } else {
            self.click(next, false, false);
        }
        self.scroll_to_cursor = true;
    }

    pub fn select_all(&mut self) {
        self.selected = self.view.iter().map(|&i| self.entries[i].name.clone()).collect();
    }

    /// Selected entries in view order.
    pub fn selection(&self) -> Vec<&Entry> {
        self.view.iter().map(|&i| &self.entries[i]).filter(|e| self.selected.contains(&e.name)).collect()
    }

    pub fn selection_paths(&self) -> Vec<VPath> {
        self.selection().into_iter().map(|e| e.path.clone()).collect()
    }

    /// (files, dirs, total bytes of files) over the visible listing.
    pub fn totals(&self) -> (u64, u64, u64) {
        let mut t = (0, 0, 0);
        for &i in &self.view {
            let e = &self.entries[i];
            if e.is_dir() {
                t.1 += 1;
            } else {
                t.0 += 1;
                t.2 += e.size.unwrap_or(0);
            }
        }
        t
    }

    pub fn selection_bytes(&self) -> u64 {
        self.selection().iter().filter_map(|e| e.size).sum()
    }
}

/// Case-insensitive comparison that orders "IMG_2" before "IMG_10".
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    bi.next();
                }
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fellor_vfs::{EntryKind, NativeId};

    fn e(name: &str, dir: bool, size: u64) -> Entry {
        Entry {
            name: name.into(),
            path: VPath::from_str_path("local", &format!("/t/{name}")),
            kind: if dir { EntryKind::Dir } else { EntryKind::File },
            size: (!dir).then_some(size),
            mtime: None,
            ctime: None,
            mime: None,
            hidden: name.starts_with('.'),
            native: NativeId::None,
        }
    }

    #[test]
    fn natural() {
        assert_eq!(natural_cmp("IMG_2", "IMG_10"), std::cmp::Ordering::Less);
        assert_eq!(natural_cmp("a", "B"), std::cmp::Ordering::Less);
    }

    #[test]
    fn dirs_first_and_selection() {
        let mut p = Pane::new(VPath::from_str_path("local", "/t"));
        p.set_entries(vec![e("b.txt", false, 5), e("Zdir", true, 0), e(".hid", false, 1), e("a.txt", false, 9)], false);
        let names: Vec<_> = p.view.iter().map(|&i| p.entries[i].name.as_str()).collect();
        assert_eq!(names, ["Zdir", "a.txt", "b.txt"]);
        p.click(1, false, false);
        p.click(2, false, true);
        assert_eq!(p.selection().len(), 2);
        assert_eq!(p.selection_bytes(), 14);
        p.click(0, true, false);
        assert_eq!(p.selection().len(), 3);
        p.set_sort(SortKey::Size, false);
        p.set_sort(SortKey::Size, false); // descending
        let names: Vec<_> = p.view.iter().map(|&i| p.entries[i].name.as_str()).collect();
        assert_eq!(names, ["Zdir", "a.txt", "b.txt"]);
    }

    #[test]
    fn history() {
        let mut p = Pane::new(VPath::from_str_path("local", "/a"));
        p.navigate(VPath::from_str_path("local", "/a/b"));
        assert!(p.up());
        assert_eq!(p.path.as_unix(), "/a");
        assert!(p.back());
        assert_eq!(p.path.as_unix(), "/a/b");
        assert!(p.forward());
        assert_eq!(p.path.as_unix(), "/a");
    }
}
