use super::*;
use std::time::{Duration, UNIX_EPOCH};

fn be(dir: &Path) -> BackendRef {
    LocalBackend::volume("T", dir.to_path_buf())
}

fn vp(p: &str) -> VPath {
    VPath::from_str_path("vol:T", p)
}

async fn write(b: &BackendRef, p: &str, data: &[u8], mtime: Option<SystemTime>) -> Result<Entry> {
    let mut s = b.create_write(&vp(p), data.len() as u64, mtime).await?;
    s.write_chunk(Bytes::copy_from_slice(data)).await?;
    s.finish().await
}

fn tmp_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|d| d.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("fellor-tmp"))
        .collect()
}

#[tokio::test]
async fn info_and_ids() {
    let d = tempfile::tempdir().unwrap();
    let b = be(d.path());
    assert_eq!(b.id(), "vol:T");
    let i = b.info();
    assert_eq!(i.kind, DeviceKind::Volume);
    assert!(i.ejectable);
    assert_eq!(b.capabilities().max_parallel, 4);
    let l = LocalBackend::new();
    assert_eq!(l.id(), "local");
    assert_eq!(l.info().kind, DeviceKind::Mac);
    assert!(!l.info().ejectable);
    let roots = l.roots().await.unwrap();
    assert_eq!(roots.len(), 1);
    assert!(roots[0].path.is_root());
    assert!(roots[0].is_dir());
}

#[tokio::test]
async fn write_list_stat_read() {
    let d = tempfile::tempdir().unwrap();
    let b = be(d.path());
    let mt = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let e = write(&b, "/a.jpg", b"hello world", Some(mt)).await.unwrap();
    assert_eq!(e.size, Some(11));
    assert_eq!(e.mime, Some("image/jpeg"));
    assert_eq!(e.mtime, Some(mt));
    b.mkdir(&vp("/sub")).await.unwrap();
    write(&b, "/.hidden", b"x", None).await.unwrap();
    let list = b.list(&VPath::root("vol:T")).await.unwrap();
    let names: Vec<_> = list.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec![".hidden", "a.jpg", "sub"]);
    assert!(list[0].hidden);
    assert!(list[2].is_dir());
    assert_eq!(list[1].path, vp("/a.jpg"));
    assert!(tmp_files(d.path()).is_empty());

    let mut r = b.open_read(&vp("/a.jpg"), 6).await.unwrap();
    assert_eq!(r.size(), Some(11));
    let mut got = Vec::new();
    while let Some(c) = r.read_chunk(3).await.unwrap() {
        assert!(c.len() <= 3);
        got.extend_from_slice(&c);
    }
    assert_eq!(got, b"world");
}

#[tokio::test]
async fn create_write_no_clobber_and_cleanup() {
    let d = tempfile::tempdir().unwrap();
    let b = be(d.path());
    write(&b, "/x.txt", b"one", None).await.unwrap();
    assert!(matches!(b.create_write(&vp("/x.txt"), 3, None).await, Err(VfsError::AlreadyExists(_))));

    // Target appears while writing → finish fails, original untouched, temp removed.
    let mut s = b.create_write(&vp("/y.txt"), 3, None).await.unwrap();
    s.write_chunk(Bytes::from_static(b"new")).await.unwrap();
    std::fs::write(d.path().join("y.txt"), b"other").unwrap();
    assert!(matches!(s.finish().await, Err(VfsError::AlreadyExists(_))));
    assert_eq!(std::fs::read(d.path().join("y.txt")).unwrap(), b"other");
    assert!(tmp_files(d.path()).is_empty());

    // abort removes temp
    let mut s = b.create_write(&vp("/z.txt"), 3, None).await.unwrap();
    s.write_chunk(Bytes::from_static(b"abc")).await.unwrap();
    assert_eq!(tmp_files(d.path()).len(), 1);
    s.abort().await.unwrap();
    assert!(tmp_files(d.path()).is_empty());
    assert!(!d.path().join("z.txt").exists());

    // drop removes temp
    let mut s = b.create_write(&vp("/w.txt"), 3, None).await.unwrap();
    s.write_chunk(Bytes::from_static(b"abc")).await.unwrap();
    drop(s);
    assert!(tmp_files(d.path()).is_empty());
    assert!(!d.path().join("w.txt").exists());

    // missing parent
    assert!(matches!(b.create_write(&vp("/nope/a"), 1, None).await, Err(VfsError::NotFound(_))));
}

#[tokio::test]
async fn mkdir_remove_rename_mtime() {
    let d = tempfile::tempdir().unwrap();
    let b = be(d.path());
    b.mkdir(&vp("/d")).await.unwrap();
    assert!(matches!(b.mkdir(&vp("/d")).await, Err(VfsError::AlreadyExists(_))));
    write(&b, "/d/f", b"1", None).await.unwrap();
    assert!(matches!(b.remove(&vp("/d"), false).await, Err(VfsError::NotEmpty(_))));
    let e = b.rename(&vp("/d/f"), &vp("/g")).await.unwrap();
    assert_eq!(e.name, "g");
    write(&b, "/h", b"2", None).await.unwrap();
    assert!(matches!(b.rename(&vp("/g"), &vp("/h")).await, Err(VfsError::AlreadyExists(_))));
    let mt = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    b.set_mtime(&vp("/h"), mt).await.unwrap();
    assert_eq!(b.stat(&vp("/h")).await.unwrap().mtime, Some(mt));
    b.remove(&vp("/d"), false).await.unwrap();
    write(&b, "/e/../x", b"", None).await.unwrap_err();
    b.mkdir(&vp("/r")).await.unwrap();
    b.mkdir(&vp("/r/s")).await.unwrap();
    write(&b, "/r/s/t", b"1", None).await.unwrap();
    b.remove(&vp("/r"), true).await.unwrap();
    assert!(matches!(b.stat(&vp("/r")).await, Err(VfsError::NotFound(_))));
    assert!(matches!(b.list(&vp("/h")).await, Err(VfsError::NotDir(_))));
}

#[tokio::test]
async fn free_space_reports() {
    let d = tempfile::tempdir().unwrap();
    let b = be(d.path());
    let s = b.free_space(&vp("/does/not/exist")).await.unwrap().unwrap();
    assert!(s.total > 0 && s.free <= s.total);
}

#[tokio::test]
async fn thumbnail_png_direct() {
    let d = tempfile::tempdir().unwrap();
    let b = be(d.path());
    write(&b, "/p.png", b"\x89PNGfake", None).await.unwrap();
    assert_eq!(b.thumbnail(&vp("/p.png")).await.unwrap().unwrap().as_ref(), b"\x89PNGfake");
    write(&b, "/t.txt", b"x", None).await.unwrap();
    assert!(b.thumbnail(&vp("/t.txt")).await.unwrap().is_none());
}

#[test]
fn error_mapping() {
    let p = Path::new("/x");
    assert!(matches!(map_io(std::io::Error::from_raw_os_error(libc::EPERM), p), VfsError::PermissionDenied(m) if m.contains("Конфиденциальность")));
    assert!(matches!(map_io(std::io::Error::from_raw_os_error(libc::ENOSPC), p), VfsError::NoSpace));
    assert!(matches!(map_io(std::io::Error::from_raw_os_error(libc::ENOTEMPTY), p), VfsError::NotEmpty(_)));
    assert!(matches!(map_io(std::io::ErrorKind::NotFound.into(), p), VfsError::NotFound(_)));
}

#[test]
fn volumes_listing_skips_boot_symlink() {
    // /Volumes/Macintosh HD is a symlink to "/" and must never show up.
    for (_, p) in list_volumes(Path::new("/Volumes")) {
        assert_ne!(std::fs::canonicalize(&p).unwrap(), Path::new("/"));
    }
}

#[tokio::test]
async fn discovery_attach_detach() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir(d.path().join("USB")).unwrap();
    std::os::unix::fs::symlink("/", d.path().join("Boot")).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let h = spawn_discovery_in(tx, d.path().to_path_buf(), Duration::from_millis(50), false);
    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    match ev {
        DeviceEvent::Attached(b) => {
            assert_eq!(b.id(), "vol:USB");
            assert_eq!(b.info().name, "USB");
        }
        e => panic!("unexpected {e:?}"),
    }
    std::fs::remove_dir(d.path().join("USB")).unwrap();
    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    assert!(matches!(ev, DeviceEvent::Detached(id) if id == "vol:USB"));
    h.abort();
}

#[test]
fn host_roundtrip() {
    let v = vpath_from_host(Path::new("/Users/x/a b"));
    assert_eq!(v.backend, "local");
    assert_eq!(host_from_vpath(&v).unwrap(), PathBuf::from("/Users/x/a b"));
}
