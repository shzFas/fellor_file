//! AFC client over any device stream.

use crate::proto::{self, op, OpenMode, Header, HEADER_LEN};
use crate::status::AfcStatus;
use bytes::Bytes;
use fellor_lockdown::framing::io_err;
use fellor_usbmux::BoxIo;
use fellor_vfs::{Result, VfsError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Largest single `FileRefRead`/`FileRefWrite` we issue.
pub const MAX_IO_CHUNK: usize = 1024 * 1024;
/// How many GetFileInfo requests to keep in flight when stat-ing a directory.
pub const STAT_PIPELINE: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Dir,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
    Unknown,
}

impl FileType {
    pub fn parse(s: &str) -> Self {
        match s {
            "S_IFREG" => FileType::File,
            "S_IFDIR" => FileType::Dir,
            "S_IFLNK" => FileType::Symlink,
            "S_IFCHR" => FileType::CharDevice,
            "S_IFBLK" => FileType::BlockDevice,
            "S_IFIFO" => FileType::Fifo,
            "S_IFSOCK" => FileType::Socket,
            _ => FileType::Unknown,
        }
    }
}

/// `GetFileInfo` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub size: u64,
    pub kind: FileType,
    /// Nanoseconds since the epoch.
    pub mtime_ns: Option<u64>,
    pub birthtime_ns: Option<u64>,
    pub nlink: Option<u64>,
    pub link_target: Option<String>,
    pub raw: Vec<(String, String)>,
}

impl FileInfo {
    pub fn from_kv(kv: Vec<(String, String)>) -> Self {
        let get = |k: &str| kv.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.as_str());
        let num = |k: &str| get(k).and_then(|v| v.trim().parse::<u64>().ok());
        FileInfo {
            size: num("st_size").unwrap_or(0),
            kind: get("st_ifmt").map(FileType::parse).unwrap_or(FileType::Unknown),
            mtime_ns: num("st_mtime"),
            birthtime_ns: num("st_birthtime"),
            nlink: num("st_nlink"),
            link_target: get("LinkTarget").map(str::to_owned),
            raw: kv.clone(),
        }
    }

    pub fn mtime(&self) -> Option<SystemTime> {
        self.mtime_ns.map(ns_to_time)
    }

    pub fn birthtime(&self) -> Option<SystemTime> {
        self.birthtime_ns.map(ns_to_time)
    }
}

pub fn ns_to_time(ns: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(ns)
}

/// Time → nanoseconds since the epoch (clamped at 0).
pub fn time_to_ns(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)).unwrap_or(0)
}

/// `GetDeviceInfo` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceFsInfo {
    pub model: Option<String>,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub block_size: Option<u64>,
    pub raw: Vec<(String, String)>,
}

impl DeviceFsInfo {
    pub fn from_kv(kv: Vec<(String, String)>) -> Self {
        let get = |k: &str| kv.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
        let num = |k: &str| get(k).and_then(|v| v.trim().parse::<u64>().ok());
        DeviceFsInfo { model: get("Model"), total_bytes: num("FSTotalBytes"), free_bytes: num("FSFreeBytes"), block_size: num("FSBlockSize"), raw: kv.clone() }
    }
}

/// Reply to a request: op + payload. Status-errors are already turned into `Err`.
#[derive(Debug)]
struct Reply {
    op: u64,
    payload: Vec<u8>,
}

/// Validate a path we are about to send: no NULs.
fn check_path(p: &str) -> Result<()> {
    if p.contains('\0') {
        return Err(VfsError::Other(format!("invalid path: {p:?}")));
    }
    Ok(())
}

pub struct AfcClient {
    io: BoxIo,
    packet_num: u64,
}

impl std::fmt::Debug for AfcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AfcClient").field("packet_num", &self.packet_num).finish_non_exhaustive()
    }
}

impl AfcClient {
    pub fn new(io: BoxIo) -> Self {
        Self { io, packet_num: 0 }
    }

    async fn send(&mut self, operation: u64, header_payload: &[u8], data: &[u8]) -> Result<u64> {
        let num = self.packet_num;
        self.packet_num = self.packet_num.wrapping_add(1);
        let this_len = (HEADER_LEN + header_payload.len()) as u64;
        let header = Header { entire_len: this_len + data.len() as u64, this_len, packet_num: num, operation };
        let mut head = Vec::with_capacity(HEADER_LEN + header_payload.len());
        head.extend_from_slice(&header.to_bytes());
        head.extend_from_slice(header_payload);
        self.io.write_all(&head).await.map_err(io_err)?;
        if !data.is_empty() {
            self.io.write_all(data).await.map_err(io_err)?;
        }
        self.io.flush().await.map_err(io_err)?;
        Ok(num)
    }

    /// Read one reply. Outer `Err` = transport/protocol failure (stream is
    /// unusable); inner `Err` = AFC status error for this request.
    async fn recv(&mut self, expect_num: u64) -> Result<std::result::Result<Reply, AfcStatus>> {
        let mut hb = [0u8; HEADER_LEN];
        self.io.read_exact(&mut hb).await.map_err(io_err)?;
        let h = Header::parse(&hb)?;
        let mut payload = vec![0u8; (h.entire_len as usize).saturating_sub(HEADER_LEN)];
        self.io.read_exact(&mut payload).await.map_err(io_err)?;
        if h.packet_num != expect_num {
            return Err(VfsError::Protocol(format!("AFC: reply for packet {} while waiting for {expect_num}", h.packet_num)));
        }
        if h.operation == op::STATUS {
            let code = proto::u64_at(&payload, 0).ok_or_else(|| VfsError::Protocol("AFC: short status".into()))?;
            return Ok(match AfcStatus::from_code(code) {
                AfcStatus::Success => Ok(Reply { op: op::STATUS, payload: Vec::new() }),
                st => Err(st),
            });
        }
        Ok(Ok(Reply { op: h.operation, payload }))
    }

    async fn call_raw(&mut self, operation: u64, header_payload: &[u8], data: &[u8]) -> Result<std::result::Result<Reply, AfcStatus>> {
        let n = self.send(operation, header_payload, data).await?;
        self.recv(n).await
    }

    async fn call(&mut self, operation: u64, header_payload: &[u8], data: &[u8], ctx: &str) -> Result<Reply> {
        self.call_raw(operation, header_payload, data).await?.map_err(|st| st.into_error(ctx))
    }

    async fn call_data(&mut self, operation: u64, header_payload: &[u8], ctx: &str) -> Result<Vec<u8>> {
        let r = self.call(operation, header_payload, &[], ctx).await?;
        match r.op {
            op::DATA => Ok(r.payload),
            other => Err(VfsError::Protocol(format!("AFC: expected DATA, got op {other}"))),
        }
    }

    async fn call_status(&mut self, operation: u64, header_payload: &[u8], ctx: &str) -> Result<()> {
        self.call(operation, header_payload, &[], ctx).await.map(|_| ())
    }

    /// Directory entries, without "." and "..".
    pub async fn read_dir(&mut self, path: &str) -> Result<Vec<String>> {
        check_path(path)?;
        let p = self.call_data(op::READ_DIR, &proto::cstr(path), path).await?;
        Ok(proto::split_strings(&p).into_iter().filter(|n| !n.is_empty() && n != "." && n != "..").collect())
    }

    pub async fn file_info(&mut self, path: &str) -> Result<FileInfo> {
        check_path(path)?;
        let p = self.call_data(op::GET_FILE_INFO, &proto::cstr(path), path).await?;
        Ok(FileInfo::from_kv(proto::parse_kv(&p)))
    }

    /// `file_info` for many paths with pipelining. Outer `Err` only on
    /// transport failure; per-path errors are returned inline.
    pub async fn file_info_many(&mut self, paths: &[String]) -> Result<Vec<Result<FileInfo>>> {
        let mut out = Vec::with_capacity(paths.len());
        for window in paths.chunks(STAT_PIPELINE) {
            let mut nums = Vec::with_capacity(window.len());
            for p in window {
                check_path(p)?;
                nums.push(self.send(op::GET_FILE_INFO, &proto::cstr(p), &[]).await?);
            }
            for (p, n) in window.iter().zip(nums) {
                out.push(self.recv(n).await?.map_err(|st| st.into_error(p)).and_then(|r| match r.op {
                    op::DATA => Ok(FileInfo::from_kv(proto::parse_kv(&r.payload))),
                    other => Err(VfsError::Protocol(format!("AFC: expected DATA, got op {other}"))),
                }));
            }
        }
        Ok(out)
    }

    pub async fn device_info(&mut self) -> Result<DeviceFsInfo> {
        let p = self.call_data(op::GET_DEVICE_INFO, &[], "device info").await?;
        Ok(DeviceFsInfo::from_kv(proto::parse_kv(&p)))
    }

    pub async fn make_dir(&mut self, path: &str) -> Result<()> {
        check_path(path)?;
        self.call_status(op::MAKE_DIR, &proto::cstr(path), path).await
    }

    pub async fn remove_path(&mut self, path: &str) -> Result<()> {
        check_path(path)?;
        self.call_status(op::REMOVE_PATH, &proto::cstr(path), path).await
    }

    pub async fn remove_path_and_contents(&mut self, path: &str) -> Result<()> {
        check_path(path)?;
        self.call_status(op::REMOVE_PATH_AND_CONTENTS, &proto::cstr(path), path).await
    }

    pub async fn rename_path(&mut self, from: &str, to: &str) -> Result<()> {
        check_path(from)?;
        check_path(to)?;
        let mut hp = proto::cstr(from);
        hp.extend_from_slice(&proto::cstr(to));
        self.call_status(op::RENAME_PATH, &hp, from).await
    }

    /// `SetFileModTime` (op 0x1E): mtime in ns since the epoch.
    pub async fn set_file_time(&mut self, path: &str, mtime_ns: u64) -> Result<()> {
        check_path(path)?;
        let mut hp = mtime_ns.to_le_bytes().to_vec();
        hp.extend_from_slice(&proto::cstr(path));
        self.call_status(op::SET_FILE_MOD_TIME, &hp, path).await
    }

    pub async fn file_open(&mut self, path: &str, mode: OpenMode) -> Result<u64> {
        check_path(path)?;
        let mut hp = (mode as u64).to_le_bytes().to_vec();
        hp.extend_from_slice(&proto::cstr(path));
        let r = self.call(op::FILE_REF_OPEN, &hp, &[], path).await?;
        match r.op {
            op::FILE_REF_OPEN_RESULT => proto::u64_at(&r.payload, 0).ok_or_else(|| VfsError::Protocol("AFC: short open result".into())),
            other => Err(VfsError::Protocol(format!("AFC: expected open result, got op {other}"))),
        }
    }

    /// Read up to `len` bytes (capped at [`MAX_IO_CHUNK`]). Empty = EOF.
    pub async fn file_read(&mut self, handle: u64, len: usize) -> Result<Bytes> {
        let len = len.clamp(1, MAX_IO_CHUNK) as u64;
        let mut hp = handle.to_le_bytes().to_vec();
        hp.extend_from_slice(&len.to_le_bytes());
        match self.call_raw(op::FILE_REF_READ, &hp, &[]).await? {
            Ok(r) if r.op == op::DATA => Ok(Bytes::from(r.payload)),
            // Some devices answer a read at EOF with a success status or EndOfData.
            Ok(r) if r.op == op::STATUS => Ok(Bytes::new()),
            Ok(r) => Err(VfsError::Protocol(format!("AFC: expected DATA, got op {}", r.op))),
            Err(AfcStatus::EndOfData) => Ok(Bytes::new()),
            Err(st) => Err(st.into_error("read")),
        }
    }

    /// Write all of `data`, splitting into [`MAX_IO_CHUNK`] packets.
    pub async fn file_write(&mut self, handle: u64, data: &[u8]) -> Result<()> {
        let hp = handle.to_le_bytes();
        for chunk in data.chunks(MAX_IO_CHUNK) {
            self.call(op::FILE_REF_WRITE, &hp, chunk, "write").await?;
        }
        Ok(())
    }

    pub async fn file_seek(&mut self, handle: u64, offset: i64, whence: u64) -> Result<()> {
        let mut hp = handle.to_le_bytes().to_vec();
        hp.extend_from_slice(&whence.to_le_bytes());
        hp.extend_from_slice(&offset.to_le_bytes());
        self.call_status(op::FILE_REF_SEEK, &hp, "seek").await
    }

    pub async fn file_tell(&mut self, handle: u64) -> Result<u64> {
        let r = self.call(op::FILE_REF_TELL, &handle.to_le_bytes(), &[], "tell").await?;
        match r.op {
            op::FILE_REF_TELL_RESULT => proto::u64_at(&r.payload, 0).ok_or_else(|| VfsError::Protocol("AFC: short tell".into())),
            other => Err(VfsError::Protocol(format!("AFC: expected tell result, got op {other}"))),
        }
    }

    pub async fn file_close(&mut self, handle: u64) -> Result<()> {
        self.call_status(op::FILE_REF_CLOSE, &handle.to_le_bytes(), "close").await
    }

    pub fn into_inner(self) -> BoxIo {
        self.io
    }
}
