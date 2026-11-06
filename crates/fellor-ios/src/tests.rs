use crate::backend::TEMP_PREFIX;
use crate::discovery::{self, DiscoveryEnv};
use crate::*;
use async_trait::async_trait;
use bytes::Bytes;
use fellor_afc::fake::{self, FakeFs, Faults};
use fellor_afc::{AfcClient, AppInfo};
use fellor_usbmux::{dict, BoxIo, DeviceRecord, Listener, UsbmuxClient};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

const UDID: &str = "00008110-000A1B2C3D4E5F60";

struct FakeConnector {
    media: Arc<Mutex<FakeFs>>,
    apps: HashMap<String, Arc<Mutex<FakeFs>>>,
    infos: Vec<AppInfo>,
    faults: Arc<Faults>,
    list_calls: AtomicUsize,
}

#[async_trait]
impl Connector for FakeConnector {
    async fn open_afc(&self, target: &AfcTarget) -> Result<AfcClient> {
        let fs = match target {
            AfcTarget::Media => self.media.clone(),
            AfcTarget::App(b) => self.apps.get(b).cloned().ok_or_else(|| VfsError::NotFound(b.clone()))?,
        };
        Ok(AfcClient::new(fake::spawn(fs, self.faults.clone())))
    }
    async fn list_apps(&self) -> Result<Vec<AppInfo>> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.infos.clone())
    }
}

struct Fixture {
    backend: IosBackend,
    media: Arc<Mutex<FakeFs>>,
    vlc: Arc<Mutex<FakeFs>>,
    faults: Arc<Faults>,
}

fn fixture() -> Fixture {
    let media = FakeFs::shared();
    {
        let mut m = media.lock().unwrap();
        m.put_file("/DCIM/100APPLE/IMG_0001.HEIC", b"heic-bytes", 1_700_000_000_000_000_000);
        m.put_file("/DCIM/100APPLE/IMG_0001.MOV", &vec![7u8; 3 * 1024 * 1024 + 5], 1_700_000_000_000_000_000);
        m.put_file("/DCIM/.hidden", b"", 0);
        m.mkdir_p("/Downloads");
    }
    let vlc = FakeFs::shared();
    vlc.lock().unwrap().put_file("/Documents/movie.mkv", b"mkv", 5);
    let docs = FakeFs::shared();
    docs.lock().unwrap().mkdir_p("/Documents");
    let faults = Arc::new(Faults::default());
    let connector = Arc::new(FakeConnector {
        media: media.clone(),
        apps: HashMap::from([("org.videolan.vlc".to_string(), vlc.clone()), ("com.readdle.docs".to_string(), docs)]),
        infos: vec![
            AppInfo { bundle_id: "org.videolan.vlc".into(), display_name: "VLC".into(), file_sharing: true },
            AppInfo { bundle_id: "com.readdle.docs".into(), display_name: "Documents".into(), file_sharing: true },
        ],
        faults: faults.clone(),
        list_calls: AtomicUsize::new(0),
    });
    let values = fellor_lockdown::DeviceValues {
        device_name: Some("iPhone Алекса".into()),
        product_type: Some("iPhone15,2".into()),
        product_version: Some("18.1".into()),
        device_class: Some("iPhone".into()),
        unique_device_id: Some(UDID.into()),
    };
    let backend = IosBackend::new(device_info(UDID, &values, DeviceState::Ready), connector);
    Fixture { backend, media, vlc, faults }
}

fn vp(path: &str) -> VPath {
    VPath::from_str_path(IosBackend::backend_id(UDID), path)
}

async fn read_all(src: &mut Box<dyn ReadSource>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(c) = src.read_chunk(1 << 20).await.unwrap() {
        assert!(c.len() <= 1 << 20);
        out.extend_from_slice(&c);
    }
    out
}

fn temp_files(fs: &Arc<Mutex<FakeFs>>) -> Vec<String> {
    fs.lock().unwrap().nodes.keys().filter(|k| k.contains(TEMP_PREFIX)).cloned().collect()
}

#[test]
fn info_and_capabilities() {
    let f = fixture();
    let info = f.backend.info();
    assert_eq!(f.backend.id(), format!("ios:{UDID}"));
    assert_eq!(info.name, "iPhone Алекса");
    assert_eq!(info.model.as_deref(), Some("iPhone15,2"));
    assert_eq!(info.os_version.as_deref(), Some("18.1"));
    assert_eq!(info.kind, DeviceKind::IPhone);
    assert_eq!(info.state, DeviceState::Ready);
    let caps = f.backend.capabilities();
    assert_eq!(caps.max_parallel, 1);
    assert!(caps.partial_read && caps.write && caps.rename && caps.set_mtime);
    assert_eq!(device_kind(Some("iPad")), DeviceKind::IPad);
    assert_eq!(device_kind(Some("iPod")), DeviceKind::IPod);
    assert_eq!(device_kind(None), DeviceKind::IPhone);
    assert_eq!(device_info("u", &Default::default(), DeviceState::NeedsTrust).name, "iPhone");
}

#[tokio::test]
async fn roots_and_listing() {
    let f = fixture();
    let roots = f.backend.roots().await.unwrap();
    assert_eq!(roots.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["Media", "Apps"]);
    assert_eq!(f.backend.list(&vp("/")).await.unwrap().len(), 2);

    let media = f.backend.list(&vp("/Media")).await.unwrap();
    let names: Vec<_> = media.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["DCIM", "Downloads"]);
    assert!(media.iter().all(|e| e.kind == EntryKind::Dir && e.size.is_none()));

    let dcim = f.backend.list(&vp("/Media/DCIM")).await.unwrap();
    let hidden = dcim.iter().find(|e| e.name == ".hidden").unwrap();
    assert!(hidden.hidden);

    let photos = f.backend.list(&vp("/Media/DCIM/100APPLE")).await.unwrap();
    assert_eq!(photos.len(), 2);
    let heic = &photos[0];
    assert_eq!(heic.name, "IMG_0001.HEIC");
    assert_eq!(heic.path, vp("/Media/DCIM/100APPLE/IMG_0001.HEIC"));
    assert_eq!(heic.kind, EntryKind::File);
    assert_eq!(heic.size, Some(10));
    assert_eq!(heic.mime, Some("image/heic"));
    assert_eq!(heic.mtime, Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000)));
    assert_eq!(heic.native, NativeId::Path("/DCIM/100APPLE/IMG_0001.HEIC".into()));
    assert_eq!(photos[1].mime, Some("video/quicktime"));

    let st = f.backend.stat(&vp("/Media/DCIM/100APPLE/IMG_0001.MOV")).await.unwrap();
    assert_eq!(st.size, Some(3 * 1024 * 1024 + 5));
    assert_eq!(f.backend.stat(&vp("/")).await.unwrap().name, "iPhone Алекса");
    assert_eq!(f.backend.stat(&vp("/Media")).await.unwrap().kind, EntryKind::Dir);
    assert!(matches!(f.backend.stat(&vp("/Media/nope")).await, Err(VfsError::NotFound(_))));
    assert!(matches!(f.backend.list(&vp("/Bogus")).await, Err(VfsError::NotFound(_))));
    assert!(matches!(f.backend.list(&VPath::from_str_path("ios:other", "/Media")).await, Err(VfsError::NotFound(_))));
    assert!(f.backend.list(&VPath::new(f.backend.id(), vec!["Media".into(), "..".into()])).await.is_err());

    let space = f.backend.free_space(&vp("/Media")).await.unwrap().unwrap();
    assert_eq!(space, SpaceInfo { total: 64 << 30, free: 10 << 30 });
}

#[tokio::test]
async fn apps_and_house_arrest() {
    let f = fixture();
    let apps = f.backend.list(&vp("/Apps")).await.unwrap();
    assert_eq!(apps.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["VLC", "Documents"]);
    let vlc = f.backend.list(&vp("/Apps/VLC")).await.unwrap();
    assert_eq!(vlc.len(), 1);
    assert_eq!(vlc[0].name, "movie.mkv");
    assert_eq!(vlc[0].native, NativeId::Path("/Documents/movie.mkv".into()));
    assert_eq!(vlc[0].mime, Some("video/x-matroska"));
    assert_eq!(f.backend.stat(&vp("/Apps/VLC")).await.unwrap().name, "VLC");
    assert!(matches!(f.backend.list(&vp("/Apps/Nope")).await, Err(VfsError::NotFound(_))));

    let mut w = f.backend.create_write(&vp("/Apps/VLC/new.mp4"), 3, None).await.unwrap();
    w.write_chunk(Bytes::from_static(b"abc")).await.unwrap();
    w.finish().await.unwrap();
    assert_eq!(f.vlc.lock().unwrap().file("/Documents/new.mp4"), Some(&b"abc"[..]));

    // Cross-sandbox rename isn't possible; neither is touching sandbox roots.
    assert!(matches!(f.backend.rename(&vp("/Apps/VLC/new.mp4"), &vp("/Media/new.mp4")).await, Err(VfsError::Unsupported(_))));
    assert!(matches!(f.backend.remove(&vp("/Apps/VLC"), true).await, Err(VfsError::PermissionDenied(_))));
    assert!(matches!(f.backend.remove(&vp("/Media"), true).await, Err(VfsError::PermissionDenied(_))));
    assert!(matches!(f.backend.mkdir(&vp("/Apps/X")).await, Err(VfsError::NotFound(_) | VfsError::PermissionDenied(_))));
}

#[tokio::test]
async fn read_with_offset() {
    let f = fixture();
    let p = vp("/Media/DCIM/100APPLE/IMG_0001.MOV");
    let mut src = f.backend.open_read(&p, 0).await.unwrap();
    assert_eq!(src.size(), Some(3 * 1024 * 1024 + 5));
    let all = read_all(&mut src).await;
    assert_eq!(all.len(), 3 * 1024 * 1024 + 5);
    assert!(src.read_chunk(10).await.unwrap().is_none());

    let mut src = f.backend.open_read(&p, 3 * 1024 * 1024).await.unwrap();
    assert_eq!(read_all(&mut src).await, vec![7u8; 5]);

    let mut src = f.backend.open_read(&vp("/Media/DCIM/100APPLE/IMG_0001.HEIC"), 5).await.unwrap();
    assert_eq!(src.read_chunk(3).await.unwrap().unwrap(), Bytes::from_static(b"byt"));
    drop(src); // closes the handle in the background

    assert!(matches!(f.backend.open_read(&vp("/Media/DCIM"), 0).await, Err(VfsError::IsDir(_))));
    assert!(matches!(f.backend.open_read(&vp("/Media"), 0).await, Err(VfsError::IsDir(_))));
    assert!(matches!(f.backend.open_read(&vp("/Media/none"), 0).await, Err(VfsError::NotFound(_))));
}

#[tokio::test]
async fn write_is_atomic_and_sets_mtime() {
    let f = fixture();
    let p = vp("/Media/Downloads/report.pdf");
    let data: Vec<u8> = (0..2_500_000).map(|i| (i % 251) as u8).collect();
    let mtime = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    let mut w = f.backend.create_write(&p, data.len() as u64, Some(mtime)).await.unwrap();
    for chunk in data.chunks(1 << 20) {
        w.write_chunk(Bytes::copy_from_slice(chunk)).await.unwrap();
    }
    // Not visible at the final path yet; temp is in the same dir.
    assert!(f.media.lock().unwrap().file("/Downloads/report.pdf").is_none());
    let tmps = temp_files(&f.media);
    assert_eq!(tmps.len(), 1);
    assert!(tmps[0].starts_with("/Downloads/.fellor-tmp-"));

    let e = w.finish().await.unwrap();
    assert_eq!(e.path, p);
    assert_eq!(e.size, Some(data.len() as u64));
    assert_eq!(e.mtime, Some(mtime));
    assert_eq!(f.media.lock().unwrap().file("/Downloads/report.pdf").unwrap(), &data[..]);
    assert!(temp_files(&f.media).is_empty());

    // Existing target → AlreadyExists up front.
    assert!(matches!(f.backend.create_write(&p, 1, None).await, Err(VfsError::AlreadyExists(_))));
    // Missing parent → NotFound.
    assert!(matches!(f.backend.create_write(&vp("/Media/nodir/x"), 1, None).await, Err(VfsError::NotFound(_))));
    // Parent is a file → NotDir.
    assert!(matches!(f.backend.create_write(&vp("/Media/Downloads/report.pdf/x"), 1, None).await, Err(VfsError::NotDir(_))));
}

#[tokio::test]
async fn write_conflict_at_finish_abort_and_drop_clean_up() {
    let f = fixture();
    let p = vp("/Media/Downloads/a.txt");
    let mut w = f.backend.create_write(&p, 2, None).await.unwrap();
    w.write_chunk(Bytes::from_static(b"hi")).await.unwrap();
    f.media.lock().unwrap().put_file("/Downloads/a.txt", b"other", 0);
    assert!(matches!(w.finish().await, Err(VfsError::AlreadyExists(_))));
    assert_eq!(f.media.lock().unwrap().file("/Downloads/a.txt"), Some(&b"other"[..]));
    assert!(temp_files(&f.media).is_empty());

    let w = f.backend.create_write(&vp("/Media/Downloads/b.txt"), 2, None).await.unwrap();
    assert_eq!(temp_files(&f.media).len(), 1);
    w.abort().await.unwrap();
    assert!(temp_files(&f.media).is_empty());
    assert!(f.media.lock().unwrap().file("/Downloads/b.txt").is_none());

    let mut w = f.backend.create_write(&vp("/Media/Downloads/c.txt"), 2, None).await.unwrap();
    w.write_chunk(Bytes::from_static(b"zz")).await.unwrap();
    drop(w);
    for _ in 0..100 {
        if temp_files(&f.media).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(temp_files(&f.media).is_empty(), "dropped sink must remove its temp file");
}

#[tokio::test]
async fn mkdir_remove_rename_set_mtime() {
    let f = fixture();
    let d = f.backend.mkdir(&vp("/Media/Downloads/New")).await.unwrap();
    assert_eq!(d.kind, EntryKind::Dir);
    assert_eq!(d.name, "New");
    assert!(matches!(f.backend.mkdir(&vp("/Media/Downloads/New")).await, Err(VfsError::AlreadyExists(_))));

    let mut w = f.backend.create_write(&vp("/Media/Downloads/New/x.jpg"), 1, None).await.unwrap();
    w.write_chunk(Bytes::from_static(b"x")).await.unwrap();
    w.finish().await.unwrap();

    assert!(matches!(f.backend.remove(&vp("/Media/Downloads/New"), false).await, Err(VfsError::NotEmpty(_))));
    let r = f.backend.rename(&vp("/Media/Downloads/New/x.jpg"), &vp("/Media/Downloads/y.jpg")).await.unwrap();
    assert_eq!(r.name, "y.jpg");
    assert_eq!(r.mime, Some("image/jpeg"));
    assert!(matches!(f.backend.rename(&vp("/Media/Downloads/y.jpg"), &vp("/Media/DCIM")).await, Err(VfsError::AlreadyExists(_))));
    assert!(matches!(f.backend.rename(&vp("/Media/Downloads/zz"), &vp("/Media/q")).await, Err(VfsError::NotFound(_))));
    assert!(f.backend.rename(&vp("/Media/Downloads"), &vp("/Media/Downloads/inner")).await.is_err());
    f.backend.remove(&vp("/Media/Downloads/New"), false).await.unwrap();

    let t = UNIX_EPOCH + Duration::from_secs(1_234_567_890);
    f.backend.set_mtime(&vp("/Media/Downloads/y.jpg"), t).await.unwrap();
    assert_eq!(f.backend.stat(&vp("/Media/Downloads/y.jpg")).await.unwrap().mtime, Some(t));

    f.backend.remove(&vp("/Media/DCIM"), true).await.unwrap();
    assert!(matches!(f.backend.stat(&vp("/Media/DCIM/100APPLE")).await, Err(VfsError::NotFound(_))));
}

#[tokio::test]
async fn reconnects_after_transport_failure() {
    let f = fixture();
    f.backend.list(&vp("/Media")).await.unwrap();
    assert_eq!(f.faults.connections.load(Ordering::SeqCst), 1);
    let mut src = f.backend.open_read(&vp("/Media/DCIM/100APPLE/IMG_0001.MOV"), 0).await.unwrap();
    assert!(src.read_chunk(1024).await.unwrap().is_some());

    // The device drops the connection: the next call reconnects transparently.
    f.faults.drop_next.store(true, Ordering::SeqCst);
    assert_eq!(f.backend.list(&vp("/Media")).await.unwrap().len(), 2);
    assert_eq!(f.faults.connections.load(Ordering::SeqCst), 2);

    // The open handle belonged to the old connection: retryable error.
    let e = src.read_chunk(1024).await.unwrap_err();
    assert!(e.is_retryable(), "{e:?}");
    // Resume via offset works on the new connection.
    let mut src = f.backend.open_read(&vp("/Media/DCIM/100APPLE/IMG_0001.MOV"), 1024).await.unwrap();
    assert_eq!(read_all(&mut src).await.len(), 3 * 1024 * 1024 + 5 - 1024);

    // A write interrupted by a drop fails retryably; the next attempt cleans up.
    let mut w = f.backend.create_write(&vp("/Media/Downloads/z.bin"), 4, None).await.unwrap();
    f.faults.drop_next.store(true, Ordering::SeqCst);
    assert!(w.write_chunk(Bytes::from_static(b"zzzz")).await.unwrap_err().is_retryable());
    w.abort().await.unwrap();
    assert!(temp_files(&f.media).is_empty());
}

#[tokio::test]
async fn unavailable_backend_reports_state() {
    for (state, check) in [
        (DeviceState::NeedsTrust, (|e: &VfsError| matches!(e, VfsError::NeedsTrust)) as fn(&VfsError) -> bool),
        (DeviceState::Locked, |e: &VfsError| matches!(e, VfsError::Locked)),
    ] {
        let b = IosBackend::unavailable(device_info(UDID, &Default::default(), state.clone()));
        assert_eq!(b.info().state, state);
        assert!(check(&b.roots().await.unwrap_err()));
        assert!(check(&b.list(&vp("/Media")).await.unwrap_err()));
        assert!(check(&b.open_read(&vp("/Media/a"), 0).await.err().unwrap()));
        assert!(check(&b.free_space(&vp("/")).await.unwrap_err()));
    }
}

// --- discovery -------------------------------------------------------------

struct FakeEnv {
    /// One server stream per `listen()` call.
    listeners: Mutex<Vec<tokio::io::DuplexStream>>,
    probes: AtomicUsize,
    /// Probe results, in order; the last one repeats.
    script: Vec<fn() -> Result<()>>,
    listen_calls: AtomicUsize,
}

#[async_trait]
impl DiscoveryEnv for FakeEnv {
    async fn listen(&self) -> Result<Listener<BoxIo>> {
        self.listen_calls.fetch_add(1, Ordering::SeqCst);
        let s = self.listeners.lock().unwrap().pop();
        match s {
            Some(s) => UsbmuxClient::new(Box::new(s) as BoxIo).listen().await,
            None => Err(VfsError::Transport("no usbmuxd".into())),
        }
    }
    async fn probe(&self, rec: &DeviceRecord) -> Result<IosBackend> {
        let n = self.probes.fetch_add(1, Ordering::SeqCst);
        let step = self.script[n.min(self.script.len() - 1)];
        step()?;
        Ok(fixture_for(&rec.udid))
    }
    fn poll_interval(&self) -> Duration {
        Duration::from_millis(10)
    }
}

/// Only `info()` matters for discovery tests.
fn fixture_for(udid: &str) -> IosBackend {
    IosBackend::unavailable(device_info(udid, &Default::default(), DeviceState::Ready))
}

/// Server half of a fake usbmuxd Listen connection.
async fn fake_usbmuxd(mut s: tokio::io::DuplexStream, events: Vec<plist::Dictionary>, close_after: bool) {
    use tokio::io::AsyncWriteExt;
    let (tag, _req) = fellor_usbmux::read_packet(&mut s).await.unwrap();
    s.write_all(&fellor_usbmux::encode_packet(tag, &dict! { "MessageType" => "Result", "Number" => 0u64 }).unwrap()).await.unwrap();
    for e in events {
        s.write_all(&fellor_usbmux::encode_packet(0, &e).unwrap()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    if !close_after {
        std::future::pending::<()>().await;
    }
}

fn attached(id: u64, udid: &str, conn: &str) -> plist::Dictionary {
    dict! {
        "MessageType" => "Attached", "DeviceID" => id,
        "Properties" => plist::Value::Dictionary(dict! { "SerialNumber" => udid, "ConnectionType" => conn, "DeviceID" => id }),
    }
}

async fn next_event(rx: &mut tokio::sync::mpsc::UnboundedReceiver<DeviceEvent>) -> DeviceEvent {
    tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("timed out waiting for event").expect("channel closed")
}

#[tokio::test]
async fn discovery_needs_trust_then_ready_then_detached() {
    let (client, server) = tokio::io::duplex(1 << 16);
    let env = Arc::new(FakeEnv {
        listeners: Mutex::new(vec![client]),
        probes: AtomicUsize::new(0),
        script: vec![|| Err(VfsError::NeedsTrust), || Err(VfsError::NeedsTrust), || Err(VfsError::Locked), || Ok(())],
        listen_calls: AtomicUsize::new(0),
    });
    tokio::spawn(fake_usbmuxd(
        server,
        vec![
            attached(5, "NETWORK-UDID", "Network"),
            attached(3, UDID, "USB"),
            dict! { "MessageType" => "Paired", "DeviceID" => 3u64 },
        ],
        false,
    ));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let h = tokio::spawn(discovery::run(env.clone(), tx));

    let DeviceEvent::Attached(b) = next_event(&mut rx).await else { panic!("expected Attached") };
    assert_eq!(b.id(), format!("ios:{UDID}"));
    assert_eq!(b.info().state, DeviceState::NeedsTrust);
    // NeedsTrust twice → only one event; then Locked → Updated; then Ready → Updated.
    let DeviceEvent::Updated(b) = next_event(&mut rx).await else { panic!("expected Updated") };
    assert_eq!(b.info().state, DeviceState::Locked);
    let DeviceEvent::Updated(b) = next_event(&mut rx).await else { panic!("expected Updated") };
    assert_eq!(b.info().state, DeviceState::Ready);
    assert_eq!(env.probes.load(Ordering::SeqCst), 4);
    h.abort();
}

#[tokio::test]
async fn discovery_detach_and_usbmuxd_restart() {
    let (c1, s1) = tokio::io::duplex(1 << 16);
    let (c2, s2) = tokio::io::duplex(1 << 16);
    let env = Arc::new(FakeEnv {
        // popped from the back: c1 first, then c2
        listeners: Mutex::new(vec![c2, c1]),
        probes: AtomicUsize::new(0),
        script: vec![|| Ok(())],
        listen_calls: AtomicUsize::new(0),
    });
    tokio::spawn(fake_usbmuxd(s1, vec![attached(3, UDID, "USB"), dict! { "MessageType" => "Detached", "DeviceID" => 3u64 }, attached(4, UDID, "USB")], true));
    tokio::spawn(fake_usbmuxd(s2, vec![attached(9, UDID, "USB")], false));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let h = tokio::spawn(discovery::run(env.clone(), tx));

    assert!(matches!(next_event(&mut rx).await, DeviceEvent::Attached(b) if b.info().state == DeviceState::Ready));
    assert!(matches!(next_event(&mut rx).await, DeviceEvent::Detached(id) if id == format!("ios:{UDID}")));
    assert!(matches!(next_event(&mut rx).await, DeviceEvent::Attached(_)));
    // usbmuxd connection closes → everything is detached, then we reconnect.
    assert!(matches!(next_event(&mut rx).await, DeviceEvent::Detached(_)));
    assert!(matches!(next_event(&mut rx).await, DeviceEvent::Attached(_)));
    assert_eq!(env.listen_calls.load(Ordering::SeqCst), 2);
    h.abort();
}

#[tokio::test]
async fn discovery_stops_when_receiver_dropped() {
    let env = Arc::new(FakeEnv { listeners: Mutex::new(vec![]), probes: AtomicUsize::new(0), script: vec![|| Ok(())], listen_calls: AtomicUsize::new(0) });
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let h = tokio::spawn(discovery::run(env, tx));
    drop(rx);
    tokio::time::timeout(Duration::from_secs(5), h).await.expect("discovery should exit").unwrap();
}

#[test]
fn state_mapping() {
    assert_eq!(discovery::state_for_error(&VfsError::NeedsTrust), DeviceState::NeedsTrust);
    assert_eq!(discovery::state_for_error(&VfsError::Locked), DeviceState::Locked);
    assert_eq!(discovery::state_for_error(&VfsError::Disconnected), DeviceState::Connecting);
    assert!(matches!(discovery::state_for_error(&VfsError::Protocol("x".into())), DeviceState::Error(_)));
}
