//! In-memory AFC server for tests (feature `fake`). Speaks the real wire
//! protocol over `tokio::io::duplex`, so the client code under test is the
//! production code.

use crate::proto::{self, op, Header, HEADER_LEN};
use fellor_usbmux::BoxIo;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Dir { mtime_ns: u64 },
    File { data: Vec<u8>, mtime_ns: u64 },
}

/// Paths are normalized absolute strings ("/", "/DCIM", "/DCIM/a.jpg").
#[derive(Debug, Default)]
pub struct FakeFs {
    pub nodes: BTreeMap<String, Node>,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Log of operation codes received.
    pub ops: Vec<u64>,
}

pub fn norm(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    format!("/{}", parts.join("/"))
}

fn parent(p: &str) -> String {
    match p.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => p[..i].into(),
    }
}

impl FakeFs {
    pub fn new() -> Self {
        let mut fs = FakeFs { total_bytes: 64 << 30, free_bytes: 10 << 30, ..Default::default() };
        fs.nodes.insert("/".into(), Node::Dir { mtime_ns: 0 });
        fs
    }

    pub fn shared() -> Arc<Mutex<FakeFs>> {
        Arc::new(Mutex::new(FakeFs::new()))
    }

    pub fn mkdir_p(&mut self, p: &str) {
        let p = norm(p);
        let mut cur = String::new();
        for c in p.split('/').filter(|c| !c.is_empty()) {
            cur.push('/');
            cur.push_str(c);
            self.nodes.entry(cur.clone()).or_insert(Node::Dir { mtime_ns: 0 });
        }
    }

    pub fn put_file(&mut self, p: &str, data: &[u8], mtime_ns: u64) {
        let p = norm(p);
        self.mkdir_p(&parent(&p));
        self.nodes.insert(p, Node::File { data: data.to_vec(), mtime_ns });
    }

    pub fn file(&self, p: &str) -> Option<&[u8]> {
        match self.nodes.get(&norm(p)) {
            Some(Node::File { data, .. }) => Some(data),
            _ => None,
        }
    }

    pub fn children(&self, p: &str) -> Vec<String> {
        let p = norm(p);
        let prefix = if p == "/" { "/".to_string() } else { format!("{p}/") };
        self.nodes
            .keys()
            .filter(|k| k.starts_with(&prefix) && k.len() > prefix.len() && !k[prefix.len()..].contains('/'))
            .map(|k| k[prefix.len()..].to_string())
            .collect()
    }
}

/// Knobs to make the fake misbehave.
#[derive(Debug, Default)]
pub struct Faults {
    /// Close the connection when the next request arrives.
    pub drop_next: AtomicBool,
    /// Number of connections served so far.
    pub connections: AtomicUsize,
}

struct Handle {
    path: String,
    pos: usize,
}

/// Spawn a server on an in-memory pipe; returns the client end.
pub fn spawn(fs: Arc<Mutex<FakeFs>>, faults: Arc<Faults>) -> BoxIo {
    let (client, server) = tokio::io::duplex(4 << 20);
    faults.connections.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(serve(server, fs, faults));
    Box::new(client)
}

async fn read_req<S: AsyncRead + Unpin>(s: &mut S) -> Option<(Header, Vec<u8>, Vec<u8>)> {
    let mut hb = [0u8; HEADER_LEN];
    s.read_exact(&mut hb).await.ok()?;
    let h = Header::parse(&hb).ok()?;
    let mut hp = vec![0u8; h.this_len as usize - HEADER_LEN];
    s.read_exact(&mut hp).await.ok()?;
    let mut data = vec![0u8; (h.entire_len - h.this_len) as usize];
    s.read_exact(&mut data).await.ok()?;
    Some((h, hp, data))
}

fn status(code: u64) -> (u64, Vec<u8>) {
    (op::STATUS, code.to_le_bytes().to_vec())
}

fn kv(pairs: &[(&str, String)]) -> Vec<u8> {
    let mut v = Vec::new();
    for (k, val) in pairs {
        v.extend_from_slice(&proto::cstr(k));
        v.extend_from_slice(&proto::cstr(val));
    }
    v
}

fn cstr_at(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, fs: Arc<Mutex<FakeFs>>, faults: Arc<Faults>) {
    let mut handles: HashMap<u64, Handle> = HashMap::new();
    let mut next_handle = 1u64;
    while let Some((h, hp, data)) = read_req(&mut s).await {
        if faults.drop_next.swap(false, Ordering::SeqCst) {
            return;
        }
        let (rop, payload) = {
            let mut fs = fs.lock().unwrap();
            fs.ops.push(h.operation);
            handle(&mut fs, &mut handles, &mut next_handle, h.operation, &hp, &data)
        };
        let pkt = proto::encode(h.packet_num, rop, &[], &payload);
        if s.write_all(&pkt).await.is_err() {
            return;
        }
    }
}

fn handle(fs: &mut FakeFs, handles: &mut HashMap<u64, Handle>, next: &mut u64, operation: u64, hp: &[u8], data: &[u8]) -> (u64, Vec<u8>) {
    let u = |off| proto::u64_at(hp, off).unwrap_or(0);
    match operation {
        op::READ_DIR => {
            let p = norm(&cstr_at(hp));
            match fs.nodes.get(&p) {
                Some(Node::Dir { .. }) => {
                    let mut out = proto::cstr(".");
                    out.extend_from_slice(&proto::cstr(".."));
                    for c in fs.children(&p) {
                        out.extend_from_slice(&proto::cstr(&c));
                    }
                    (op::DATA, out)
                }
                Some(_) => status(7),
                None => status(8),
            }
        }
        op::GET_FILE_INFO => match fs.nodes.get(&norm(&cstr_at(hp))) {
            Some(Node::Dir { mtime_ns }) => (
                op::DATA,
                kv(&[("st_size", "96".into()), ("st_nlink", "3".into()), ("st_ifmt", "S_IFDIR".into()), ("st_mtime", mtime_ns.to_string()), ("st_birthtime", mtime_ns.to_string())]),
            ),
            Some(Node::File { data, mtime_ns }) => (
                op::DATA,
                kv(&[
                    ("st_size", data.len().to_string()),
                    ("st_blocks", "8".into()),
                    ("st_nlink", "1".into()),
                    ("st_ifmt", "S_IFREG".into()),
                    ("st_mtime", mtime_ns.to_string()),
                    ("st_birthtime", "1000000000".into()),
                ]),
            ),
            None => status(8),
        },
        op::GET_DEVICE_INFO => (
            op::DATA,
            kv(&[("Model", "iPhone15,2".into()), ("FSTotalBytes", fs.total_bytes.to_string()), ("FSFreeBytes", fs.free_bytes.to_string()), ("FSBlockSize", "4096".into())]),
        ),
        op::MAKE_DIR => {
            let p = norm(&cstr_at(hp));
            match fs.nodes.get(&p) {
                Some(Node::File { .. }) => status(16),
                _ => {
                    fs.mkdir_p(&p);
                    status(0)
                }
            }
        }
        op::REMOVE_PATH => {
            let p = norm(&cstr_at(hp));
            match fs.nodes.get(&p) {
                None => status(8),
                Some(Node::Dir { .. }) if !fs.children(&p).is_empty() => status(33),
                Some(_) => {
                    fs.nodes.remove(&p);
                    status(0)
                }
            }
        }
        op::REMOVE_PATH_AND_CONTENTS => {
            let p = norm(&cstr_at(hp));
            if !fs.nodes.contains_key(&p) {
                return status(8);
            }
            let prefix = format!("{p}/");
            fs.nodes.retain(|k, _| k != &p && !k.starts_with(&prefix));
            status(0)
        }
        op::RENAME_PATH => {
            let from = norm(&cstr_at(hp));
            let rest = hp.iter().position(|&c| c == 0).map(|i| &hp[i + 1..]).unwrap_or(&[]);
            let to = norm(&cstr_at(rest));
            if !fs.nodes.contains_key(&from) {
                return status(8);
            }
            if !matches!(fs.nodes.get(&parent(&to)), Some(Node::Dir { .. })) {
                return status(8);
            }
            let prefix = format!("{from}/");
            let moved: Vec<(String, Node)> =
                fs.nodes.iter().filter(|(k, _)| **k == from || k.starts_with(&prefix)).map(|(k, v)| (k.clone(), v.clone())).collect();
            for (k, _) in &moved {
                fs.nodes.remove(k);
            }
            for (k, v) in moved {
                fs.nodes.insert(format!("{to}{}", &k[from.len()..]), v);
            }
            status(0)
        }
        op::SET_FILE_MOD_TIME => {
            let t = u(0);
            let p = norm(&cstr_at(hp.get(8..).unwrap_or(&[])));
            match fs.nodes.get_mut(&p) {
                Some(Node::File { mtime_ns, .. }) | Some(Node::Dir { mtime_ns }) => {
                    *mtime_ns = t;
                    status(0)
                }
                None => status(8),
            }
        }
        op::FILE_REF_OPEN => {
            let mode = u(0);
            let p = norm(&cstr_at(hp.get(8..).unwrap_or(&[])));
            match (mode, fs.nodes.get(&p)) {
                (_, Some(Node::Dir { .. })) => return status(9),
                (1 | 2, None) => return status(8),
                (3..=6, None) => {
                    if !matches!(fs.nodes.get(&parent(&p)), Some(Node::Dir { .. })) {
                        return status(8);
                    }
                    fs.nodes.insert(p.clone(), Node::File { data: Vec::new(), mtime_ns: 5 });
                }
                (3 | 4, Some(_)) => {
                    fs.nodes.insert(p.clone(), Node::File { data: Vec::new(), mtime_ns: 5 });
                }
                _ => {}
            }
            let hnd = *next;
            *next += 1;
            handles.insert(hnd, Handle { path: p, pos: 0 });
            (op::FILE_REF_OPEN_RESULT, hnd.to_le_bytes().to_vec())
        }
        op::FILE_REF_READ => {
            let Some(hd) = handles.get_mut(&u(0)) else { return status(7) };
            let len = u(8) as usize;
            match fs.nodes.get(&hd.path) {
                Some(Node::File { data, .. }) => {
                    let start = hd.pos.min(data.len());
                    let end = (start + len).min(data.len());
                    hd.pos = end;
                    (op::DATA, data[start..end].to_vec())
                }
                _ => status(8),
            }
        }
        op::FILE_REF_WRITE => {
            let Some(hd) = handles.get_mut(&u(0)) else { return status(7) };
            match fs.nodes.get_mut(&hd.path) {
                Some(Node::File { data: fdata, .. }) => {
                    if fs.free_bytes < data.len() as u64 {
                        return status(18);
                    }
                    let end = hd.pos + data.len();
                    if fdata.len() < end {
                        fdata.resize(end, 0);
                    }
                    fdata[hd.pos..end].copy_from_slice(data);
                    hd.pos = end;
                    status(0)
                }
                _ => status(8),
            }
        }
        op::FILE_REF_SEEK => {
            let Some(hd) = handles.get_mut(&u(0)) else { return status(7) };
            let whence = u(8);
            let off = u(16) as i64;
            let len = match fs.nodes.get(&hd.path) {
                Some(Node::File { data, .. }) => data.len() as i64,
                _ => 0,
            };
            let base = match whence {
                0 => 0,
                1 => hd.pos as i64,
                _ => len,
            };
            hd.pos = (base + off).max(0) as usize;
            status(0)
        }
        op::FILE_REF_TELL => match handles.get(&u(0)) {
            Some(hd) => (op::FILE_REF_TELL_RESULT, (hd.pos as u64).to_le_bytes().to_vec()),
            None => status(7),
        },
        op::FILE_REF_CLOSE => match handles.remove(&u(0)) {
            Some(_) => status(0),
            None => status(7),
        },
        _ => status(15),
    }
}
