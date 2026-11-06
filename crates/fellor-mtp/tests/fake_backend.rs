//! MtpBackend end-to-end over the packet-level fake device.

use bytes::Bytes;
use fellor_mtp::MtpBackend;
use fellor_ptp::codes::{event, op};
use fellor_ptp::fake::{FakeConfig, FakeDevice};
use fellor_ptp::EventSource;
use fellor_vfs::{Backend, DeviceKind, DeviceState, EntryKind, FsEvent, NativeId, VPath, VfsError};
use std::time::Duration;

const ID: &str = "mtp:FAKE0001";
const INT: u32 = 0x0001_0001;
const SD: u32 = 0x0002_0001;

struct Fx {
    dev: FakeDevice,
    b: MtpBackend,
    img: Vec<u8>,
    camera: u32,
}

fn p(s: &str) -> VPath {
    VPath::from_str_path(ID, s)
}

async fn setup(cfg: FakeConfig) -> Fx {
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("debug").try_init();
    let cfg = FakeConfig { storages: vec![(INT, "Internal shared storage".into()), (SD, "SD card".into())], ..cfg };
    let dev = FakeDevice::new(cfg);
    let dcim = dev.add_dir(INT, 0, "DCIM");
    let camera = dev.add_dir(INT, dcim, "Camera");
    let img: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
    let h = dev.add_file(INT, camera, "IMG_0001.jpg", &img);
    dev.with(|s| s.objects.get_mut(&h).unwrap().thumb = Some(vec![0xFF, 0xD8, 0xFF, 0xD9]));
    dev.add_file(INT, camera, "VID_0002.mp4", &[1u8; 512 - 12]);
    let dl = dev.add_dir(INT, 0, "Download");
    dev.add_file(INT, dl, "readme.txt", b"hello mtp");
    dev.add_dir(INT, 0, "Music");
    dev.add_file(SD, 0, "sd.jpg", b"sd");
    let events = dev.events().map(|e| Box::new(e) as Box<dyn EventSource>);
    let b = MtpBackend::open_with_transport(ID.into(), Box::new(dev.transport()), events).await.unwrap();
    Fx { dev, b, img, camera }
}

fn assert_clean(dev: &FakeDevice) {
    let v = dev.violations();
    assert!(v.is_empty(), "protocol violations: {v:?}");
}

async fn read_all(b: &MtpBackend, path: &VPath, offset: u64) -> Vec<u8> {
    let mut r = b.open_read(path, offset).await.unwrap();
    let mut out = Vec::new();
    while let Some(c) = r.read_chunk(64 * 1024).await.unwrap() {
        out.extend_from_slice(&c);
    }
    out
}

#[tokio::test]
async fn info_roots_and_listing_via_prop_list() {
    let fx = setup(FakeConfig::default()).await;
    let info = fx.b.info();
    assert_eq!(info.id, ID);
    assert_eq!(info.name, "Pixel 8");
    assert_eq!(info.kind, DeviceKind::Android);
    assert_eq!(info.state, DeviceState::Ready);
    let caps = fx.b.capabilities();
    assert!(caps.read && caps.write && caps.delete && caps.rename && caps.move_within && caps.mkdir && caps.partial_read);
    assert_eq!(caps.max_parallel, 1);

    let roots = fx.b.roots().await.unwrap();
    assert_eq!(roots.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["Internal shared storage", "SD card"]);
    assert_eq!(roots[1].native, NativeId::Mtp { storage: SD, handle: 0 });

    let top = fx.b.list(&p("Internal shared storage")).await.unwrap();
    assert_eq!(top.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["DCIM", "Download", "Music"]);
    assert!(top.iter().all(|e| e.kind == EntryKind::Dir && e.size.is_none()));
    // Root listing spans storages on the wire; we must filter by storage.
    let sd = fx.b.list(&p("SD card")).await.unwrap();
    assert_eq!(sd.len(), 1);
    assert_eq!(sd[0].name, "sd.jpg");

    let cam = fx.b.list(&p("Internal shared storage/DCIM/Camera")).await.unwrap();
    assert_eq!(cam.len(), 2);
    let img = &cam[0];
    assert_eq!(img.name, "IMG_0001.jpg");
    assert_eq!(img.kind, EntryKind::File);
    assert_eq!(img.size, Some(300_000));
    assert_eq!(img.mime, Some("image/jpeg"));
    assert!(img.mtime.is_some());
    assert_eq!(img.path, p("Internal shared storage/DCIM/Camera/IMG_0001.jpg"));
    assert!(matches!(img.native, NativeId::Mtp { storage: INT, .. }));

    assert!(fx.dev.op_log().contains(&op::GET_OBJECT_PROP_LIST));
    assert!(!fx.dev.op_log().contains(&op::GET_OBJECT_HANDLES));
    let st = fx.b.stat(&p("Internal shared storage/Download/readme.txt")).await.unwrap();
    assert_eq!(st.size, Some(9));
    assert!(matches!(fx.b.stat(&p("Internal shared storage/nope")).await, Err(VfsError::NotFound(_))));
    assert!(matches!(fx.b.stat(&p("No such storage")).await, Err(VfsError::NotFound(_))));
    assert!(matches!(fx.b.list(&p("Internal shared storage/Download/readme.txt")).await, Err(VfsError::NotDir(_))));
    let root = fx.b.list(&VPath::root(ID)).await.unwrap();
    assert_eq!(root.len(), 2);
    assert_clean(&fx.dev);
}

#[tokio::test]
async fn listing_falls_back_without_prop_list() {
    let fx = setup(FakeConfig { prop_list: false, ..Default::default() }).await;
    let cam = fx.b.list(&p("Internal shared storage/DCIM/Camera")).await.unwrap();
    assert_eq!(cam.iter().map(|e| (e.name.as_str(), e.size)).collect::<Vec<_>>(), [("IMG_0001.jpg", Some(300_000)), ("VID_0002.mp4", Some(500))]);
    let log = fx.dev.op_log();
    assert!(!log.contains(&op::GET_OBJECT_PROP_LIST));
    assert!(log.contains(&op::GET_OBJECT_HANDLES) && log.contains(&op::GET_OBJECT_INFO));
    assert_clean(&fx.dev);
}

#[tokio::test]
async fn read_whole_and_resume() {
    let fx = setup(FakeConfig::default()).await;
    let path = p("Internal shared storage/DCIM/Camera/IMG_0001.jpg");
    let r = fx.b.open_read(&path, 0).await.unwrap();
    assert_eq!(r.size(), Some(300_000));
    drop(r); // dropped before reading: session must recover
    assert_eq!(read_all(&fx.b, &path, 0).await, fx.img);
    assert_eq!(read_all(&fx.b, &path, 123_457).await, &fx.img[123_457..]);
    assert!(fx.dev.op_log().contains(&op::ANDROID_GET_PARTIAL_OBJECT_64));
    // Exactly at EOF: empty stream, no transaction.
    assert!(read_all(&fx.b, &path, 300_000).await.is_empty());
    // A container that is an exact multiple of wMaxPacketSize (ZLP case).
    let v = read_all(&fx.b, &p("Internal shared storage/DCIM/Camera/VID_0002.mp4"), 0).await;
    assert_eq!(v, vec![1u8; 500]);
    assert!(matches!(fx.b.open_read(&p("Internal shared storage/DCIM"), 0).await, Err(VfsError::IsDir(_))));
    assert_clean(&fx.dev);
}

#[tokio::test]
async fn resume_with_32bit_partial_only_and_without_partial() {
    let fx = setup(FakeConfig { partial64: false, ..Default::default() }).await;
    let path = p("Internal shared storage/DCIM/Camera/IMG_0001.jpg");
    assert_eq!(read_all(&fx.b, &path, 1000).await, &fx.img[1000..]);
    assert!(fx.dev.op_log().contains(&op::GET_PARTIAL_OBJECT));

    let fx = setup(FakeConfig { partial64: false, partial: false, ..Default::default() }).await;
    assert!(!fx.b.capabilities().partial_read);
    assert!(matches!(fx.b.open_read(&path, 10).await, Err(VfsError::Unsupported(_))));
    assert_eq!(read_all(&fx.b, &path, 0).await, fx.img);
}

#[tokio::test]
async fn unknown_length_header_reads() {
    let fx = setup(FakeConfig { always_ffff_len: true, ..Default::default() }).await;
    let path = p("Internal shared storage/DCIM/Camera/IMG_0001.jpg");
    assert_eq!(read_all(&fx.b, &path, 0).await, fx.img);
    assert_eq!(read_all(&fx.b, &path, 5).await, &fx.img[5..]);
    assert_clean(&fx.dev);
}

#[tokio::test]
async fn write_read_back_and_conflict() {
    let fx = setup(FakeConfig::default()).await;
    let path = p("Internal shared storage/Download/new.bin");
    for size in [0usize, 1, 512 - 12, 512, 1024 * 1024 - 12, 2_500_000] {
        let data: Vec<u8> = (0..size).map(|i| (i * 31 % 256) as u8).collect();
        let mut w = fx.b.create_write(&path, size as u64, None).await.unwrap();
        for c in data.chunks(700_001) {
            w.write_chunk(Bytes::copy_from_slice(c)).await.unwrap();
        }
        let e = w.finish().await.unwrap();
        assert_eq!(e.name, "new.bin");
        assert_eq!(e.size, Some(size as u64));
        let NativeId::Mtp { handle, .. } = e.native else { panic!() };
        assert!(fx.dev.object(handle).unwrap().data == data, "size {size}");
        assert_eq!(read_all(&fx.b, &path, 0).await.len(), size);
        // Exists now.
        assert!(matches!(fx.b.create_write(&path, 1, None).await, Err(VfsError::AlreadyExists(_))));
        fx.b.remove(&path, false).await.unwrap();
        assert!(fx.dev.object(handle).is_none());
    }
    // Root of the device is virtual: cannot create files there.
    assert!(matches!(fx.b.create_write(&p("x.txt"), 1, None).await, Err(VfsError::PermissionDenied(_))));
    // Parent missing.
    assert!(matches!(fx.b.create_write(&p("Internal shared storage/nope/x"), 1, None).await, Err(VfsError::NotFound(_))));
    assert_clean(&fx.dev);
}

#[tokio::test]
async fn upload_into_storage_root_uses_root_parent() {
    let fx = setup(FakeConfig::default()).await;
    let path = p("SD card/top.txt");
    let mut w = fx.b.create_write(&path, 3, None).await.unwrap();
    w.write_chunk(Bytes::from_static(b"abc")).await.unwrap();
    w.finish().await.unwrap();
    let o = fx.dev.find(0, "top.txt").unwrap();
    assert_eq!(o.storage, SD);
    let sd = fx.b.list(&p("SD card")).await.unwrap();
    assert!(sd.iter().any(|e| e.name == "top.txt"));
    assert_clean(&fx.dev);
}

#[tokio::test]
async fn aborted_and_dropped_writes_leave_nothing() {
    let fx = setup(FakeConfig::default()).await;
    let path = p("Internal shared storage/Download/partial.bin");
    let mut w = fx.b.create_write(&path, 1_000_000, None).await.unwrap();
    w.write_chunk(Bytes::from(vec![7u8; 300_000])).await.unwrap();
    w.abort().await.unwrap();
    assert!(fx.dev.find(fx.dev.find(0, "Download").unwrap().handle, "partial.bin").is_none());
    assert_eq!(fx.dev.with(|s| s.cancels), 1);

    let mut w = fx.b.create_write(&path, 1_000_000, None).await.unwrap();
    w.write_chunk(Bytes::from(vec![7u8; 10])).await.unwrap();
    drop(w);
    // Cleanup runs on a spawned task; it needs the session lock (which recovers first).
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if fx.dev.find(fx.dev.find(0, "Download").unwrap().handle, "partial.bin").is_none() {
            break;
        }
    }
    assert!(fx.dev.find(fx.dev.find(0, "Download").unwrap().handle, "partial.bin").is_none());
    // Session still usable.
    assert_eq!(fx.b.list(&p("Internal shared storage/Download")).await.unwrap().len(), 1);
    // Writing more than announced fails.
    let mut w = fx.b.create_write(&path, 2, None).await.unwrap();
    assert!(w.write_chunk(Bytes::from_static(b"abc")).await.is_err());
    w.abort().await.unwrap();
}

#[tokio::test]
async fn large_upload_uses_send_object_prop_list() {
    let fx = setup(FakeConfig::default()).await;
    let path = p("Internal shared storage/Download/huge.mkv");
    let w = fx.b.create_write(&path, 5 << 30, Some(std::time::SystemTime::now())).await.unwrap();
    assert!(fx.dev.op_log().contains(&op::SEND_OBJECT_PROP_LIST));
    assert!(!fx.dev.op_log().contains(&op::SEND_OBJECT_INFO));
    w.abort().await.unwrap();
    assert!(fx.dev.find(fx.dev.find(0, "Download").unwrap().handle, "huge.mkv").is_none());
    assert_clean(&fx.dev);
}

#[tokio::test]
async fn mkdir_rename_move_remove() {
    let fx = setup(FakeConfig::default()).await;
    let e = fx.b.mkdir(&p("Internal shared storage/Download/Sub")).await.unwrap();
    assert_eq!(e.kind, EntryKind::Dir);
    assert!(matches!(fx.b.mkdir(&p("Internal shared storage/Download/Sub")).await, Err(VfsError::AlreadyExists(_))));
    assert!(fx.b.list(&p("Internal shared storage/Download/Sub")).await.unwrap().is_empty());

    // Rename in place (ObjectFileName).
    let r = fx.b.rename(&p("Internal shared storage/Download/readme.txt"), &p("Internal shared storage/Download/README.md")).await.unwrap();
    assert_eq!(r.name, "README.md");
    assert!(fx.dev.op_log().contains(&op::SET_OBJECT_PROP_VALUE));
    let names: Vec<String> = fx.b.list(&p("Internal shared storage/Download")).await.unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["README.md", "Sub"]);

    // Move into Sub with a new name (MoveObject + rename).
    let m = fx.b.rename(&p("Internal shared storage/Download/README.md"), &p("Internal shared storage/Download/Sub/r.txt")).await.unwrap();
    assert_eq!(m.path, p("Internal shared storage/Download/Sub/r.txt"));
    assert!(fx.dev.op_log().contains(&op::MOVE_OBJECT));
    assert_eq!(read_all(&fx.b, &p("Internal shared storage/Download/Sub/r.txt"), 0).await, b"hello mtp");
    // Move to the storage root.
    fx.b.rename(&p("Internal shared storage/Download/Sub/r.txt"), &p("Internal shared storage/r.txt")).await.unwrap();
    assert_eq!(fx.dev.find(0, "r.txt").unwrap().storage, INT);
    // Target exists.
    assert!(matches!(
        fx.b.rename(&p("Internal shared storage/r.txt"), &p("Internal shared storage/Music")).await,
        Err(VfsError::AlreadyExists(_))
    ));

    // Non-recursive remove of a non-empty dir is refused; recursive works.
    assert!(matches!(fx.b.remove(&p("Internal shared storage/DCIM"), false).await, Err(VfsError::NotEmpty(_))));
    fx.b.remove(&p("Internal shared storage/DCIM"), true).await.unwrap();
    assert!(fx.dev.object(fx.camera).is_none());
    assert!(matches!(fx.b.stat(&p("Internal shared storage/DCIM/Camera")).await, Err(VfsError::NotFound(_))));
    fx.b.remove(&p("Internal shared storage/Music"), false).await.unwrap();
    assert!(matches!(fx.b.remove(&p("Internal shared storage"), true).await, Err(VfsError::PermissionDenied(_))));
    assert_clean(&fx.dev);
}

#[tokio::test]
async fn move_unsupported_without_move_object() {
    let fx = setup(FakeConfig { move_object: false, ..Default::default() }).await;
    assert!(!fx.b.capabilities().move_within);
    let r = fx.b.rename(&p("Internal shared storage/Download/readme.txt"), &p("Internal shared storage/Music/readme.txt")).await;
    assert!(matches!(r, Err(VfsError::Unsupported(_))));
}

#[tokio::test]
async fn device_events_update_cache_and_watchers() {
    let fx = setup(FakeConfig::default()).await;
    let mut rx = fx.b.watch().unwrap();
    // Populate the cache for Camera.
    assert_eq!(fx.b.list(&p("Internal shared storage/DCIM/Camera")).await.unwrap().len(), 2);
    let h = fx.dev.device_creates_file(INT, fx.camera, "IMG_0003.jpg", b"new photo");
    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    assert_eq!(ev, FsEvent::Added(p("Internal shared storage/DCIM/Camera/IMG_0003.jpg")));
    // Resolves from cache without relisting.
    fx.dev.clear_op_log();
    assert_eq!(fx.b.stat(&p("Internal shared storage/DCIM/Camera/IMG_0003.jpg")).await.unwrap().size, Some(9));
    assert!(fx.dev.op_log().is_empty());

    fx.dev.with(|s| s.objects.remove(&h));
    fx.dev.push_event(event::OBJECT_REMOVED, &[h]);
    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    assert_eq!(ev, FsEvent::Removed(p("Internal shared storage/DCIM/Camera/IMG_0003.jpg")));
    assert!(matches!(fx.b.stat(&p("Internal shared storage/DCIM/Camera/IMG_0003.jpg")).await, Err(VfsError::NotFound(_))));

    // Storage removed (SD card ejected).
    fx.dev.with(|s| s.cfg.storages.retain(|s| s.0 != SD));
    fx.dev.push_event(event::STORE_REMOVED, &[SD]);
    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    assert!(matches!(ev, FsEvent::Invalidate(_)));
    assert_eq!(fx.b.roots().await.unwrap().len(), 1);
}

#[tokio::test]
async fn locked_phone_then_unlocked() {
    let fx = setup(FakeConfig { locked: true, ..Default::default() }).await;
    assert_eq!(fx.b.state(), DeviceState::Locked);
    assert!(matches!(fx.b.roots().await, Err(VfsError::Locked)));
    fx.dev.with(|s| s.cfg.locked = false);
    assert_eq!(fx.b.refresh_storages().await, DeviceState::Ready);
    assert_eq!(fx.b.roots().await.unwrap().len(), 2);
}

#[tokio::test]
async fn camera_kind_thumbnail_and_space() {
    let fx = setup(FakeConfig { android: false, model: "Canon EOS R6".into(), ..Default::default() }).await;
    assert_eq!(fx.b.info().kind, DeviceKind::Camera);
    assert_eq!(fx.b.info().name, "Canon EOS R6");
    let t = fx.b.thumbnail(&p("Internal shared storage/DCIM/Camera/IMG_0001.jpg")).await.unwrap();
    assert_eq!(t.as_deref(), Some(&[0xFF, 0xD8, 0xFF, 0xD9][..]));
    assert_eq!(fx.b.thumbnail(&p("Internal shared storage/Download/readme.txt")).await.unwrap(), None);
    let sp = fx.b.free_space(&p("Internal shared storage")).await.unwrap().unwrap();
    assert_eq!(sp.total, 64 << 30);
    assert!(sp.free < sp.total);
    assert_clean(&fx.dev);
}

#[tokio::test]
async fn busy_device_is_retried_transparently() {
    let fx = setup(FakeConfig::default()).await;
    fx.dev.with(|s| s.busy_responses = 3);
    assert_eq!(fx.b.list(&p("Internal shared storage/Download")).await.unwrap().len(), 1);
}

#[tokio::test]
async fn placeholder_states_map_to_errors() {
    let b = MtpBackend::new(ID.into(), "Pixel".into(), DeviceKind::Android, DeviceState::NeedsMtpMode);
    assert!(matches!(b.roots().await, Err(VfsError::Other(_))));
    b.set_state(DeviceState::Busy { by: Some("OpenMTP".into()) });
    assert!(matches!(b.list(&VPath::root(ID)).await, Err(VfsError::Busy(Some(_)))));
    assert_eq!(b.capabilities().max_parallel, 1);
    assert!(!b.capabilities().read);
}
