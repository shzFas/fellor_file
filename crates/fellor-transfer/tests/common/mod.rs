//! In-memory fake Backend with fault injection.
#![allow(dead_code)]
use async_trait::async_trait;
use bytes::Bytes;
use fellor_vfs::*;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

#[derive(Clone)]
pub enum Node {
    Dir,
    File { data: Vec<u8>, mtime: SystemTime },
}

type Hook = Box<dyn Fn(&Fake, u64) + Send + Sync>;

pub struct Fake {
    me: Weak<Fake>,
    pub id: String,
    pub caps: Mutex<Capabilities>,
    pub tree: Mutex<BTreeMap<Vec<String>, Node>>,
    /// Each entry: one Transport error when a read reaches this offset.
    pub fail_reads: Mutex<VecDeque<u64>>,
    /// Number of upcoming write_chunk calls that fail with Transport.
    pub fail_writes: Mutex<u32>,
    /// Non-retryable failure for reads of this file name.
    pub deny_name: Mutex<Option<String>>,
    pub disconnected: AtomicBool,
    pub serve_chunk: Mutex<usize>,
    pub delay: Mutex<Duration>,
    pub free: Mutex<Option<u64>>,
    pub active_readers: AtomicUsize,
    pub max_readers: AtomicUsize,
    pub active_writers: AtomicUsize,
    pub max_writers: AtomicUsize,
    pub read_sizes: Mutex<Vec<usize>>,
    pub open_offsets: Mutex<Vec<u64>>,
    pub renames: AtomicUsize,
    /// Called after every served chunk with the new position.
    pub on_read: Mutex<Option<Hook>>,
    /// Called on create_write.
    pub on_create: Mutex<Option<Hook>>,
}

pub fn caps(max_parallel: usize) -> Capabilities {
    Capabilities {
        read: true,
        write: true,
        delete: true,
        rename: true,
        move_within: true,
        mkdir: true,
        set_mtime: true,
        partial_read: true,
        free_space: false,
        max_parallel,
    }
}

fn key(p: &VPath) -> Vec<String> {
    p.components.clone()
}

impl Fake {
    pub fn new(id: &str, max_parallel: usize) -> Arc<Fake> {
        Arc::new_cyclic(|me| {
            let mut tree = BTreeMap::new();
            tree.insert(Vec::new(), Node::Dir);
            Fake {
                me: me.clone(),
                id: id.into(),
                caps: Mutex::new(caps(max_parallel)),
                tree: Mutex::new(tree),
                fail_reads: Mutex::new(VecDeque::new()),
                fail_writes: Mutex::new(0),
                deny_name: Mutex::new(None),
                disconnected: AtomicBool::new(false),
                serve_chunk: Mutex::new(usize::MAX),
                delay: Mutex::new(Duration::ZERO),
                free: Mutex::new(None),
                active_readers: AtomicUsize::new(0),
                max_readers: AtomicUsize::new(0),
                active_writers: AtomicUsize::new(0),
                max_writers: AtomicUsize::new(0),
                read_sizes: Mutex::new(Vec::new()),
                open_offsets: Mutex::new(Vec::new()),
                renames: AtomicUsize::new(0),
                on_read: Mutex::new(None),
                on_create: Mutex::new(None),
            }
        })
    }

    pub fn vp(&self, p: &str) -> VPath {
        VPath::from_str_path(self.id.clone(), p)
    }

    pub fn put_dir(&self, p: &str) {
        let v = self.vp(p);
        let mut t = self.tree.lock().unwrap();
        for i in 0..=v.components.len() {
            t.insert(v.components[..i].to_vec(), Node::Dir);
        }
    }

    pub fn put_file(&self, p: &str, data: &[u8], mtime: SystemTime) {
        let v = self.vp(p);
        if let Some(parent) = v.parent() {
            self.put_dir(&parent.as_unix());
        }
        self.tree.lock().unwrap().insert(key(&v), Node::File { data: data.to_vec(), mtime });
    }

    pub fn get(&self, p: &str) -> Option<Vec<u8>> {
        match self.tree.lock().unwrap().get(&key(&self.vp(p))) {
            Some(Node::File { data, .. }) => Some(data.clone()),
            _ => None,
        }
    }

    pub fn mtime(&self, p: &str) -> Option<SystemTime> {
        match self.tree.lock().unwrap().get(&key(&self.vp(p))) {
            Some(Node::File { mtime, .. }) => Some(*mtime),
            _ => None,
        }
    }

    pub fn exists(&self, p: &str) -> bool {
        self.tree.lock().unwrap().contains_key(&key(&self.vp(p)))
    }

    pub fn paths(&self) -> Vec<String> {
        self.tree.lock().unwrap().keys().map(|k| format!("/{}", k.join("/"))).collect()
    }

    fn check(&self) -> Result<()> {
        if self.disconnected.load(Ordering::SeqCst) {
            Err(VfsError::Disconnected)
        } else {
            Ok(())
        }
    }

    fn entry(&self, k: &[String], n: &Node) -> Entry {
        let path = VPath::new(self.id.clone(), k.to_vec());
        let name = path.name().to_owned();
        match n {
            Node::Dir => Entry { name, path, kind: EntryKind::Dir, size: None, mtime: None, ctime: None, mime: None, hidden: false, native: NativeId::None },
            Node::File { data, mtime } => Entry {
                mime: mime_from_name(&name),
                hidden: name.starts_with('.'),
                name,
                path,
                kind: EntryKind::File,
                size: Some(data.len() as u64),
                mtime: Some(*mtime),
                ctime: None,
                native: NativeId::None,
            },
        }
    }

    fn bump(cur: &AtomicUsize, max: &AtomicUsize) {
        let n = cur.fetch_add(1, Ordering::SeqCst) + 1;
        max.fetch_max(n, Ordering::SeqCst);
    }
}

#[async_trait]
impl Backend for Fake {
    fn id(&self) -> BackendId {
        self.id.clone()
    }
    fn info(&self) -> DeviceInfo {
        DeviceInfo { id: self.id.clone(), name: self.id.clone(), model: None, os_version: None, kind: DeviceKind::Android, state: DeviceState::Ready, ejectable: false }
    }
    fn capabilities(&self) -> Capabilities {
        *self.caps.lock().unwrap()
    }
    async fn roots(&self) -> Result<Vec<Entry>> {
        self.check()?;
        Ok(vec![self.entry(&[], &Node::Dir)])
    }
    async fn list(&self, path: &VPath) -> Result<Vec<Entry>> {
        self.check()?;
        let t = self.tree.lock().unwrap();
        match t.get(&key(path)) {
            Some(Node::Dir) => {}
            Some(_) => return Err(VfsError::NotDir(path.to_string())),
            None => return Err(VfsError::NotFound(path.to_string())),
        }
        let n = path.components.len();
        Ok(t.iter().filter(|(k, _)| k.len() == n + 1 && k.starts_with(&path.components)).map(|(k, v)| self.entry(k, v)).collect())
    }
    async fn stat(&self, path: &VPath) -> Result<Entry> {
        self.check()?;
        let t = self.tree.lock().unwrap();
        t.get(&key(path)).map(|n| self.entry(&path.components, n)).ok_or_else(|| VfsError::NotFound(path.to_string()))
    }
    async fn free_space(&self, _path: &VPath) -> Result<Option<SpaceInfo>> {
        Ok(self.free.lock().unwrap().map(|f| SpaceInfo { total: f * 2, free: f }))
    }
    async fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn ReadSource>> {
        self.check()?;
        if self.deny_name.lock().unwrap().as_deref() == Some(path.name()) {
            return Err(VfsError::PermissionDenied(path.to_string()));
        }
        let data = match self.tree.lock().unwrap().get(&key(path)) {
            Some(Node::File { data, .. }) => data.clone(),
            Some(Node::Dir) => return Err(VfsError::IsDir(path.to_string())),
            None => return Err(VfsError::NotFound(path.to_string())),
        };
        self.open_offsets.lock().unwrap().push(offset);
        Fake::bump(&self.active_readers, &self.max_readers);
        Ok(Box::new(FakeReader { fake: self.me.upgrade().unwrap(), data, pos: offset as usize }))
    }
    async fn create_write(&self, path: &VPath, _size: u64, mtime: Option<SystemTime>) -> Result<Box<dyn WriteSink>> {
        self.check()?;
        if let Some(h) = self.on_create.lock().unwrap().as_ref() {
            h(self, 0);
        }
        let t = self.tree.lock().unwrap();
        if t.contains_key(&key(path)) {
            return Err(VfsError::AlreadyExists(path.to_string()));
        }
        match path.parent().and_then(|p| t.get(&key(&p)).cloned()) {
            Some(Node::Dir) => {}
            _ => return Err(VfsError::NotFound(path.to_string())),
        }
        drop(t);
        Fake::bump(&self.active_writers, &self.max_writers);
        Ok(Box::new(FakeWriter { fake: self.me.upgrade().unwrap(), path: path.clone(), buf: Vec::new(), mtime: mtime.unwrap_or(SystemTime::UNIX_EPOCH), open: true }))
    }
    async fn mkdir(&self, path: &VPath) -> Result<Entry> {
        self.check()?;
        let mut t = self.tree.lock().unwrap();
        if t.contains_key(&key(path)) {
            return Err(VfsError::AlreadyExists(path.to_string()));
        }
        t.insert(key(path), Node::Dir);
        Ok(self.entry(&path.components, &Node::Dir))
    }
    async fn remove(&self, path: &VPath, recursive: bool) -> Result<()> {
        self.check()?;
        let mut t = self.tree.lock().unwrap();
        let k = key(path);
        let has_children = t.keys().any(|c| c.len() > k.len() && c.starts_with(&k));
        if !t.contains_key(&k) {
            return Err(VfsError::NotFound(path.to_string()));
        }
        if has_children && !recursive {
            return Err(VfsError::NotEmpty(path.to_string()));
        }
        t.retain(|c, _| !c.starts_with(&k));
        Ok(())
    }
    async fn rename(&self, from: &VPath, to: &VPath) -> Result<Entry> {
        self.check()?;
        self.renames.fetch_add(1, Ordering::SeqCst);
        let mut t = self.tree.lock().unwrap();
        if t.contains_key(&key(to)) {
            return Err(VfsError::AlreadyExists(to.to_string()));
        }
        let fk = key(from);
        let moved: Vec<_> = t.iter().filter(|(k, _)| k.starts_with(&fk)).map(|(k, v)| (k.clone(), v.clone())).collect();
        if moved.is_empty() {
            return Err(VfsError::NotFound(from.to_string()));
        }
        t.retain(|k, _| !k.starts_with(&fk));
        for (k, v) in moved {
            let mut nk = to.components.clone();
            nk.extend_from_slice(&k[fk.len()..]);
            t.insert(nk, v);
        }
        let n = t.get(&key(to)).unwrap().clone();
        Ok(self.entry(&to.components, &n))
    }
    async fn set_mtime(&self, path: &VPath, m: SystemTime) -> Result<()> {
        self.check()?;
        if let Some(Node::File { mtime, .. }) = self.tree.lock().unwrap().get_mut(&key(path)) {
            *mtime = m;
        }
        Ok(())
    }
}

struct FakeReader {
    fake: Arc<Fake>,
    data: Vec<u8>,
    pos: usize,
}

impl Drop for FakeReader {
    fn drop(&mut self) {
        self.fake.active_readers.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl ReadSource for FakeReader {
    async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>> {
        let d = *self.fake.delay.lock().unwrap();
        if !d.is_zero() {
            tokio::time::sleep(d).await;
        }
        self.fake.check()?;
        self.fake.read_sizes.lock().unwrap().push(max);
        {
            let mut fr = self.fake.fail_reads.lock().unwrap();
            if fr.front().is_some_and(|&t| self.pos as u64 >= t) {
                fr.pop_front();
                return Err(VfsError::Transport("injected read stall".into()));
            }
        }
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        let n = max.min(*self.fake.serve_chunk.lock().unwrap()).min(self.data.len() - self.pos);
        let out = Bytes::copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        if let Some(h) = self.fake.on_read.lock().unwrap().as_ref() {
            h(&self.fake, self.pos as u64);
        }
        Ok(Some(out))
    }
    fn size(&self) -> Option<u64> {
        Some(self.data.len() as u64)
    }
}

struct FakeWriter {
    fake: Arc<Fake>,
    path: VPath,
    buf: Vec<u8>,
    mtime: SystemTime,
    open: bool,
}

impl FakeWriter {
    fn close(&mut self) {
        if std::mem::replace(&mut self.open, false) {
            self.fake.active_writers.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Drop for FakeWriter {
    fn drop(&mut self) {
        self.close();
    }
}

#[async_trait]
impl WriteSink for FakeWriter {
    async fn write_chunk(&mut self, data: Bytes) -> Result<()> {
        let d = *self.fake.delay.lock().unwrap();
        if !d.is_zero() {
            tokio::time::sleep(d).await;
        }
        self.fake.check()?;
        {
            let mut fw = self.fake.fail_writes.lock().unwrap();
            if *fw > 0 {
                *fw -= 1;
                return Err(VfsError::Transport("injected write stall".into()));
            }
        }
        self.buf.extend_from_slice(&data);
        Ok(())
    }
    async fn finish(mut self: Box<Self>) -> Result<Entry> {
        self.fake.check()?;
        self.close();
        let mut t = self.fake.tree.lock().unwrap();
        if t.contains_key(&key(&self.path)) {
            return Err(VfsError::AlreadyExists(self.path.to_string()));
        }
        let node = Node::File { data: std::mem::take(&mut self.buf), mtime: self.mtime };
        t.insert(key(&self.path), node.clone());
        Ok(self.fake.entry(&self.path.components, &node))
    }
    async fn abort(mut self: Box<Self>) -> Result<()> {
        self.close();
        Ok(())
    }
}
