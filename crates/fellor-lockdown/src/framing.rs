//! Lockdown-style plist framing: `length:u32 big-endian | XML plist`.
//! Also used by house_arrest, installation_proxy and friends.

use fellor_usbmux::plist_util;
use fellor_vfs::{Result, VfsError};
use plist::Dictionary;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Hard cap on a single frame; protects against garbage lengths.
pub const MAX_FRAME: usize = 32 * 1024 * 1024;

pub fn encode_frame(msg: &Dictionary) -> Result<Vec<u8>> {
    let body = plist_util::to_xml(msg).map_err(|e| VfsError::Protocol(format!("plist encode: {e}")))?;
    let len = u32::try_from(body.len()).map_err(|_| VfsError::Protocol("frame too large".into()))?;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode a complete in-memory frame; returns the message and bytes consumed.
pub fn decode_frame(b: &[u8]) -> Result<(Dictionary, usize)> {
    let len_bytes: [u8; 4] = b
        .get(..4)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| VfsError::Protocol("frame shorter than length prefix".into()))?;
    let len = check_len(u32::from_be_bytes(len_bytes))?;
    let body = b.get(4..4 + len).ok_or_else(|| VfsError::Protocol("frame truncated".into()))?;
    Ok((parse_body(body)?, 4 + len))
}

fn check_len(len: u32) -> Result<usize> {
    let len = len as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(VfsError::Protocol(format!("bad plist frame length {len}")));
    }
    Ok(len)
}

fn parse_body(body: &[u8]) -> Result<Dictionary> {
    plist_util::from_bytes(body).map_err(|e| VfsError::Protocol(format!("plist frame: {e}")))
}

pub async fn write_frame<W: AsyncWrite + Unpin + ?Sized>(w: &mut W, msg: &Dictionary) -> Result<()> {
    let buf = encode_frame(msg)?;
    w.write_all(&buf).await.map_err(io_err)?;
    w.flush().await.map_err(io_err)?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin + ?Sized>(r: &mut R) -> Result<Dictionary> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.map_err(io_err)?;
    let len = check_len(u32::from_be_bytes(len))?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.map_err(io_err)?;
    parse_body(&body)
}

/// Map I/O errors to VFS errors: EOF / reset means the device went away.
pub fn io_err(e: std::io::Error) -> VfsError {
    use std::io::ErrorKind::*;
    match e.kind() {
        UnexpectedEof | BrokenPipe | ConnectionReset | ConnectionAborted | NotConnected => VfsError::Disconnected,
        TimedOut => VfsError::Timeout,
        InvalidData => VfsError::Protocol(e.to_string()),
        _ => VfsError::Transport(e.to_string()),
    }
}
