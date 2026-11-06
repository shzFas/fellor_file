//! AFC wire format.
//!
//! ```text
//! 0   magic        "CFA6LPAA" (8 bytes)
//! 8   entire_len   u64 LE  header + header payload + data
//! 16  this_len     u64 LE  header + header payload
//! 24  packet_num   u64 LE  echoed by the device in the reply
//! 32  operation    u64 LE
//! 40  header payload (this_len - 40), then data (entire_len - this_len)
//! ```

use fellor_vfs::{Result, VfsError};

pub const MAGIC: &[u8; 8] = b"CFA6LPAA";
pub const HEADER_LEN: usize = 40;
/// Refuse replies larger than this (a 1 MiB read + slack, big directory listings).
pub const MAX_PACKET: u64 = 64 * 1024 * 1024;

/// Operation codes (cross-checked with libimobiledevice's afc.h; SPEC values are correct).
pub mod op {
    pub const STATUS: u64 = 0x01;
    pub const DATA: u64 = 0x02;
    pub const READ_DIR: u64 = 0x03;
    pub const READ_FILE: u64 = 0x04;
    pub const WRITE_FILE: u64 = 0x05;
    pub const WRITE_PART: u64 = 0x06;
    pub const TRUNCATE: u64 = 0x07;
    pub const REMOVE_PATH: u64 = 0x08;
    pub const MAKE_DIR: u64 = 0x09;
    pub const GET_FILE_INFO: u64 = 0x0A;
    pub const GET_DEVICE_INFO: u64 = 0x0B;
    pub const WRITE_FILE_ATOMIC: u64 = 0x0C;
    pub const FILE_REF_OPEN: u64 = 0x0D;
    pub const FILE_REF_OPEN_RESULT: u64 = 0x0E;
    pub const FILE_REF_READ: u64 = 0x0F;
    pub const FILE_REF_WRITE: u64 = 0x10;
    pub const FILE_REF_SEEK: u64 = 0x11;
    pub const FILE_REF_TELL: u64 = 0x12;
    pub const FILE_REF_TELL_RESULT: u64 = 0x13;
    pub const FILE_REF_CLOSE: u64 = 0x14;
    pub const FILE_REF_SET_SIZE: u64 = 0x15;
    pub const GET_CONNECTION_INFO: u64 = 0x16;
    pub const SET_CONNECTION_OPTIONS: u64 = 0x17;
    pub const RENAME_PATH: u64 = 0x18;
    pub const SET_FS_BLOCK_SIZE: u64 = 0x19;
    pub const SET_SOCKET_BLOCK_SIZE: u64 = 0x1A;
    pub const FILE_REF_LOCK: u64 = 0x1B;
    pub const MAKE_LINK: u64 = 0x1C;
    pub const GET_FILE_HASH: u64 = 0x1D;
    pub const SET_FILE_MOD_TIME: u64 = 0x1E;
    pub const GET_FILE_HASH_RANGE: u64 = 0x1F;
    pub const FILE_REF_SET_IMMUTABLE_HINT: u64 = 0x20;
    pub const GET_SIZE_OF_PATH_CONTENTS: u64 = 0x21;
    pub const REMOVE_PATH_AND_CONTENTS: u64 = 0x22;
    pub const DIR_OPEN: u64 = 0x23;
    pub const DIR_READ: u64 = 0x24;
    pub const DIR_CLOSE: u64 = 0x25;
}

/// `FileRefOpen` modes (fopen-style).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum OpenMode {
    /// "r"  O_RDONLY
    ReadOnly = 1,
    /// "r+" O_RDWR
    ReadWrite = 2,
    /// "w"  O_WRONLY | O_CREAT | O_TRUNC
    WriteOnly = 3,
    /// "w+" O_RDWR | O_CREAT | O_TRUNC
    WriteRead = 4,
    /// "a"  O_WRONLY | O_APPEND | O_CREAT
    Append = 5,
    /// "a+" O_RDWR | O_APPEND | O_CREAT
    ReadAppend = 6,
}

/// lseek whence.
pub const SEEK_SET: u64 = 0;
pub const SEEK_CUR: u64 = 1;
pub const SEEK_END: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub entire_len: u64,
    pub this_len: u64,
    pub packet_num: u64,
    pub operation: u64,
}

impl Header {
    pub fn to_bytes(self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[..8].copy_from_slice(MAGIC);
        b[8..16].copy_from_slice(&self.entire_len.to_le_bytes());
        b[16..24].copy_from_slice(&self.this_len.to_le_bytes());
        b[24..32].copy_from_slice(&self.packet_num.to_le_bytes());
        b[32..40].copy_from_slice(&self.operation.to_le_bytes());
        b
    }

    /// Parse and sanity-check a header.
    pub fn parse(b: &[u8; HEADER_LEN]) -> Result<Self> {
        if &b[..8] != MAGIC {
            return Err(VfsError::Protocol("AFC: bad magic".into()));
        }
        let u = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap_or([0; 8]));
        let h = Header { entire_len: u(8), this_len: u(16), packet_num: u(24), operation: u(32) };
        if h.entire_len < HEADER_LEN as u64 || h.entire_len > MAX_PACKET {
            return Err(VfsError::Protocol(format!("AFC: bad packet length {}", h.entire_len)));
        }
        if h.this_len < HEADER_LEN as u64 || h.this_len > h.entire_len {
            return Err(VfsError::Protocol(format!("AFC: bad header length {}", h.this_len)));
        }
        Ok(h)
    }
}

/// Encode a request: header + header payload + data.
pub fn encode(packet_num: u64, operation: u64, header_payload: &[u8], data: &[u8]) -> Vec<u8> {
    let this_len = (HEADER_LEN + header_payload.len()) as u64;
    let entire_len = this_len + data.len() as u64;
    let mut out = Vec::with_capacity(entire_len as usize);
    out.extend_from_slice(&Header { entire_len, this_len, packet_num, operation }.to_bytes());
    out.extend_from_slice(header_payload);
    out.extend_from_slice(data);
    out
}

/// A decoded packet. `payload` is header payload + data concatenated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub header: Header,
    pub payload: Vec<u8>,
}

/// Decode a full packet from memory (tests, replay).
pub fn decode(b: &[u8]) -> Result<Packet> {
    let hb: &[u8; HEADER_LEN] = b
        .get(..HEADER_LEN)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| VfsError::Protocol("AFC: short packet".into()))?;
    let header = Header::parse(hb)?;
    let payload = b
        .get(HEADER_LEN..header.entire_len as usize)
        .ok_or_else(|| VfsError::Protocol("AFC: truncated packet".into()))?
        .to_vec();
    Ok(Packet { header, payload })
}

/// NUL-terminated string.
pub fn cstr(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 1);
    v.extend_from_slice(s.as_bytes());
    v.push(0);
    v
}

/// Split a `a\0b\0c\0` list. Empty trailing segment is dropped.
pub fn split_strings(payload: &[u8]) -> Vec<String> {
    let mut v: Vec<String> = payload.split(|&b| b == 0).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    if v.last().is_some_and(String::is_empty) {
        v.pop();
    }
    v
}

/// Parse a `key\0value\0key\0value\0` dictionary.
pub fn parse_kv(payload: &[u8]) -> Vec<(String, String)> {
    let items = split_strings(payload);
    items.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect()
}

pub fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    b.get(off..off + 8).and_then(|s| s.try_into().ok()).map(u64::from_le_bytes)
}
