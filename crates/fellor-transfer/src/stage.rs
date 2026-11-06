//! Local temp-file endpoints used to stage device → device copies.
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use fellor_vfs::*;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

fn io(e: std::io::Error) -> VfsError {
    match e.kind() {
        std::io::ErrorKind::StorageFull => VfsError::NoSpace,
        _ => VfsError::Io(e),
    }
}

pub(crate) struct TempReader {
    file: tokio::fs::File,
    size: u64,
}

impl TempReader {
    pub async fn open(p: &Path, offset: u64) -> Result<Self> {
        let mut file = tokio::fs::File::open(p).await.map_err(io)?;
        let size = file.metadata().await.map_err(io)?.len();
        if offset > 0 {
            file.seek(std::io::SeekFrom::Start(offset)).await.map_err(io)?;
        }
        Ok(Self { file, size })
    }
}

#[async_trait]
impl ReadSource for TempReader {
    async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>> {
        let mut buf = BytesMut::zeroed(max.max(1));
        let mut filled = 0;
        while filled < buf.len() {
            let n = self.file.read(&mut buf[filled..]).await.map_err(io)?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            return Ok(None);
        }
        buf.truncate(filled);
        Ok(Some(buf.freeze()))
    }

    fn size(&self) -> Option<u64> {
        Some(self.size)
    }
}

/// Plain truncating writer; the staged file is removed by the caller.
pub(crate) struct TempWriter {
    file: tokio::fs::File,
    path: PathBuf,
    written: u64,
}

impl TempWriter {
    pub async fn create(p: &Path) -> Result<Self> {
        let file = tokio::fs::File::create(p).await.map_err(io)?;
        Ok(Self { file, path: p.to_path_buf(), written: 0 })
    }
}

#[async_trait]
impl WriteSink for TempWriter {
    async fn write_chunk(&mut self, data: Bytes) -> Result<()> {
        self.written += data.len() as u64;
        self.file.write_all(&data).await.map_err(io)
    }

    async fn finish(mut self: Box<Self>) -> Result<Entry> {
        self.file.flush().await.map_err(io)?;
        let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Entry {
            path: VPath::from_str_path("local", &self.path.to_string_lossy()),
            name,
            kind: EntryKind::File,
            size: Some(self.written),
            mtime: None,
            ctime: None,
            mime: None,
            hidden: true,
            native: NativeId::None,
        })
    }

    async fn abort(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

/// Removes the staged file when dropped.
pub(crate) struct StageGuard(pub PathBuf);

impl Drop for StageGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
