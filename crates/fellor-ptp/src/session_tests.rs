//! Session-level tests: byte-exact scripted exchanges and the fake device.

use crate::codes::{op, resp, ROOT_PARENT};
use crate::container::{Header, HEADER_LEN};
use crate::dataset::ObjectInfo;
use crate::error::{PtpError, TransportError};
use crate::fake::{FakeConfig, FakeDevice};
use crate::session::{DataIn, DataOut, Session};
use crate::transport::{Pipe, Transport};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Transport that replays canned IN transfers and records OUT transfers.
#[derive(Clone, Default)]
struct Script {
    ins: Arc<Mutex<VecDeque<Vec<u8>>>>,
    outs: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[async_trait]
impl Transport for Script {
    fn max_packet_size(&self) -> usize {
        512
    }
    async fn bulk_out(&mut self, data: Vec<u8>, _t: Duration) -> Result<(), TransportError> {
        self.outs.lock().unwrap().push(data);
        Ok(())
    }
    async fn bulk_in(&mut self, _len: usize, _t: Duration) -> Result<Vec<u8>, TransportError> {
        self.ins.lock().unwrap().pop_front().ok_or(TransportError::Timeout)
    }
    async fn clear_halt(&mut self, _p: Pipe) -> Result<(), TransportError> {
        Ok(())
    }
    async fn class_out(&mut self, _r: u8, _d: &[u8], _t: Duration) -> Result<(), TransportError> {
        Ok(())
    }
    async fn class_in(&mut self, _r: u8, _l: u16, _t: Duration) -> Result<Vec<u8>, TransportError> {
        Ok(vec![4, 0, 1, 0x20])
    }
}

#[tokio::test]
async fn scripted_open_session_and_storage_ids_bytes() {
    let s = Script::default();
    {
        let mut ins = s.ins.lock().unwrap();
        // OpenSession OK, tid 0
        ins.push_back(vec![0x0C, 0, 0, 0, 0x03, 0, 0x01, 0x20, 0, 0, 0, 0]);
        // GetStorageIDs data: [0x00010001, 0x00020001], tid 1
        ins.push_back(vec![
            0x18, 0, 0, 0, 0x02, 0, 0x04, 0x10, 0x01, 0, 0, 0, 0x02, 0, 0, 0, 0x01, 0, 0x01, 0, 0x01, 0, 0x02, 0,
        ]);
        ins.push_back(vec![0x0C, 0, 0, 0, 0x03, 0, 0x01, 0x20, 0x01, 0, 0, 0]);
    }
    let sess = Session::new(Box::new(s.clone()));
    let mut g = sess.lock().await;
    g.open_session(1).await.unwrap();
    assert_eq!(g.get_storage_ids().await.unwrap(), vec![0x0001_0001, 0x0002_0001]);
    let outs = s.outs.lock().unwrap();
    assert_eq!(outs[0], [0x10, 0, 0, 0, 0x01, 0, 0x02, 0x10, 0, 0, 0, 0, 0x01, 0, 0, 0]);
    assert_eq!(outs[1], [0x0C, 0, 0, 0, 0x01, 0, 0x04, 0x10, 0x01, 0, 0, 0]);
}

#[tokio::test]
async fn scripted_error_response_maps_to_error() {
    let s = Script::default();
    s.ins.lock().unwrap().push_back(vec![0x0C, 0, 0, 0, 0x03, 0, 0x09, 0x20, 0, 0, 0, 0]);
    let sess = Session::new(Box::new(s));
    let mut g = sess.lock().await;
    let e = g.get_object_info(77).await.unwrap_err();
    assert_eq!(e.response_code(), Some(resp::INVALID_OBJECT_HANDLE));
    assert!(matches!(fellor_vfs::VfsError::from(e), fellor_vfs::VfsError::NotFound(_)));
}

#[tokio::test]
async fn scripted_skips_leading_zlp_before_response() {
    let s = Script::default();
    {
        let mut ins = s.ins.lock().unwrap();
        ins.push_back(vec![]); // stray ZLP from previous phase
        ins.push_back(vec![0x0C, 0, 0, 0, 0x03, 0, 0x01, 0x20, 0, 0, 0, 0]);
    }
    let sess = Session::new(Box::new(s));
    sess.lock().await.open_session(1).await.unwrap();
}

async fn open(dev: &FakeDevice) -> Session {
    let sess = Session::new(Box::new(dev.transport()));
    {
        let mut g = sess.lock().await;
        g.busy_delay = Duration::from_millis(1);
        let di = g.get_device_info().await.unwrap();
        assert!(di.is_mtp());
        g.open_session(1).await.unwrap();
    }
    sess
}

#[tokio::test]
async fn fake_basic_ops_and_tid_discipline() {
    let dev = FakeDevice::new(FakeConfig::default());
    let st = 0x0001_0001;
    let dcim = dev.add_dir(st, 0, "DCIM");
    dev.add_file(st, dcim, "a.jpg", b"hello");
    let sess = open(&dev).await;
    let mut g = sess.lock().await;
    assert_eq!(g.get_storage_ids().await.unwrap(), vec![st]);
    assert_eq!(g.get_storage_info(st).await.unwrap().description, "Internal shared storage");
    let root = g.get_object_handles(st, 0, ROOT_PARENT).await.unwrap();
    assert_eq!(root, vec![dcim]);
    let kids = g.get_object_handles(st, 0, dcim).await.unwrap();
    let oi = g.get_object_info(kids[0]).await.unwrap();
    assert_eq!(oi.filename, "a.jpg");
    assert_eq!(oi.compressed_size, 5);
    let pl = g.get_object_prop_list(dcim, 0, 0xFFFF_FFFF, 0, 1).await.unwrap();
    assert!(pl.iter().any(|e| e.value.as_str() == Some("a.jpg")));
    drop(g);
    assert!(dev.violations().is_empty(), "{:?}", dev.violations());
}

async fn roundtrip_read(cfg: FakeConfig, size: usize, expected: Option<u64>) {
    let dev = FakeDevice::new(cfg);
    let data: Vec<u8> = (0..size).map(|i| (i * 7 + 3) as u8).collect();
    let h = dev.add_file(0x0001_0001, 0, "f.bin", &data);
    let sess = open(&dev).await;
    let g = sess.lock().await;
    let mut s = DataIn::get_object(g, h, expected).await.unwrap();
    let mut got = Vec::new();
    while let Some(c) = s.read_chunk(100_000).await.unwrap() {
        assert!(c.len() <= 100_000);
        got.extend_from_slice(&c);
    }
    assert_eq!(got.len(), data.len());
    assert!(got == data);
    // Lock was released at EOF; the session continues in sync.
    let mut g = sess.lock().await;
    assert_eq!(g.get_storage_ids().await.unwrap().len(), 1);
    drop(g);
    drop(s);
    assert!(dev.violations().is_empty(), "{:?}", dev.violations());
}

#[tokio::test]
async fn read_sizes_around_packet_boundaries() {
    // container = 12 + size; multiples of 512 trigger a ZLP from the device.
    for size in [0, 1, 500, 512 - 12, 512, 1024 - 12, 64 * 1024 - 12, 64 * 1024, 300_000, 1024 * 1024 + 500 - 12, 2 * 1024 * 1024] {
        roundtrip_read(FakeConfig::default(), size, None).await;
    }
}

#[tokio::test]
async fn read_zlp_multiple_without_device_zlp() {
    // Some devices omit the ZLP; with a known length we must not wait for one.
    let cfg = FakeConfig { send_zlp: false, ..Default::default() };
    for size in [512 - 12, 4096 - 12, 200_000 - 12] {
        roundtrip_read(cfg.clone(), size, None).await;
    }
}

#[tokio::test]
async fn read_unknown_length_ffffffff() {
    // Header length 0xFFFFFFFF (>4 GiB on Android): read to the short packet / ZLP...
    let cfg = FakeConfig { always_ffff_len: true, ..Default::default() };
    for size in [0, 10, 512 - 12, 512, 70_000, 1024 * 1024 - 12, 1024 * 1024 + 1] {
        roundtrip_read(cfg.clone(), size, None).await;
    }
    // ...or up to the size known from ObjectSize (0xDC04).
    for size in [10, 512 - 12, 333_333] {
        roundtrip_read(cfg.clone(), size, Some(size as u64)).await;
    }
    // Known size + no ZLP from the device.
    let cfg = FakeConfig { always_ffff_len: true, send_zlp: false, ..Default::default() };
    roundtrip_read(cfg, 1024 - 12, Some(1024 - 12)).await;
}

async fn roundtrip_write(mps: usize, size: usize, chunk: usize) {
    let dev = FakeDevice::new(FakeConfig { mps, ..Default::default() });
    let sess = open(&dev).await;
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let mut g = sess.lock().await;
    let info = ObjectInfo { format: 0x3000, compressed_size: size as u32, filename: "up.bin".into(), ..Default::default() };
    let (_, _, h) = g.send_object_info(0x0001_0001, ROOT_PARENT, &info).await.unwrap();
    let mut w = DataOut::send_object(g, size as u64).await.unwrap();
    for c in data.chunks(chunk.max(1)) {
        w.write(c).await.unwrap();
    }
    let (r, _g) = w.finish().await.unwrap();
    assert_eq!(r.code, resp::OK);
    assert!(dev.object(h).unwrap().data == data, "size {size}");
    assert!(dev.violations().is_empty(), "size {size} mps {mps}: {:?}", dev.violations());
}

#[tokio::test]
async fn write_sizes_around_packet_boundaries_zlp() {
    for mps in [64, 512, 1024] {
        for size in [0, 1, mps - 12, mps - 11, mps, 2 * mps - 12, 1024 * 1024 - 12, 1024 * 1024, 1024 * 1024 + mps - 12, 3 * 1024 * 1024 + 7] {
            roundtrip_write(mps, size, 100_003).await;
        }
    }
    // Tiny chunks exercise the alignment carry.
    roundtrip_write(512, 5000 - 12, 1).await;
    roundtrip_write(512, 70_000, 511).await;
}

#[tokio::test]
async fn write_more_than_declared_is_rejected() {
    let dev = FakeDevice::new(FakeConfig::default());
    let sess = open(&dev).await;
    let mut g = sess.lock().await;
    let info = ObjectInfo { format: 0x3000, compressed_size: 4, filename: "x".into(), ..Default::default() };
    g.send_object_info(0x0001_0001, ROOT_PARENT, &info).await.unwrap();
    let mut w = DataOut::send_object(g, 4).await.unwrap();
    assert!(w.write(b"12345").await.is_err());
    drop(w);
    // Recovers on next lock.
    let mut g = sess.lock().await;
    assert_eq!(g.get_storage_ids().await.unwrap().len(), 1);
    assert_eq!(dev.with(|s| s.cancels), 1);
}

#[tokio::test]
async fn send_object_prop_list_64bit_size_params() {
    let dev = FakeDevice::new(FakeConfig::default());
    let sess = open(&dev).await;
    let mut g = sess.lock().await;
    let props = vec![crate::dataset::PropListElement {
        handle: 0,
        prop: crate::codes::prop::OBJECT_FILE_NAME,
        value: crate::dataset::PropValue::Str("big.mkv".into()),
    }];
    // Announce 5 GiB; the fake checks the high/low split.
    let (_, _, h) = g.send_object_prop_list(0x0001_0001, ROOT_PARENT, 0x3000, 5 << 30, &props).await.unwrap();
    assert_eq!(dev.object(h).unwrap().name, "big.mkv");
    // Abort instead of streaming 5 GiB: cancel + recover.
    let w = DataOut::send_object(g, 5 << 30).await.unwrap();
    let mut g = w.abort().await.unwrap();
    assert_eq!(dev.with(|s| s.cancels), 1);
    g.delete_object(h).await.unwrap();
    assert!(dev.object(h).is_none());
}

#[test]
fn data_out_header_for_large_object_is_ffffffff() {
    let h = Header::data(op::SEND_OBJECT, 9, 5 << 30);
    assert_eq!(&h.encode()[0..4], &[0xFF; 4]);
    assert_eq!(HEADER_LEN, 12);
}

#[tokio::test]
async fn busy_is_retried() {
    let dev = FakeDevice::new(FakeConfig::default());
    let sess = open(&dev).await;
    dev.with(|s| s.busy_responses = 3);
    let mut g = sess.lock().await;
    assert_eq!(g.get_storage_ids().await.unwrap().len(), 1);
    drop(g);
    assert_eq!(dev.op_log().iter().filter(|&&o| o == op::GET_STORAGE_IDS).count(), 4);
    // Also for streaming reads.
    let h = dev.add_file(0x0001_0001, 0, "b", b"abc");
    dev.with(|s| s.busy_responses = 2);
    let s = DataIn::get_object(sess.lock().await, h, None).await.unwrap();
    assert_eq!(s.read_to_end().await.unwrap(), b"abc");
    assert!(dev.violations().is_empty(), "{:?}", dev.violations());
}

#[tokio::test]
async fn busy_gives_up_after_ten_retries() {
    let dev = FakeDevice::new(FakeConfig::default());
    let sess = open(&dev).await;
    dev.with(|s| s.busy_responses = 100);
    let e = sess.lock().await.get_storage_ids().await.unwrap_err();
    assert_eq!(e.response_code(), Some(resp::DEVICE_BUSY));
    assert_eq!(dev.op_log().iter().filter(|&&o| o == op::GET_STORAGE_IDS).count(), 11);
}

#[tokio::test]
async fn dropped_read_is_cancelled_and_session_recovers() {
    let dev = FakeDevice::new(FakeConfig::default());
    let h = dev.add_file(0x0001_0001, 0, "big", &vec![9u8; 3 * 1024 * 1024]);
    let sess = open(&dev).await;
    let mut s = DataIn::get_object(sess.lock().await, h, None).await.unwrap();
    let first = s.read_chunk(4096).await.unwrap().unwrap();
    assert_eq!(first.len(), 4096);
    drop(s);
    let mut g = sess.lock().await;
    assert!(!g.is_poisoned());
    assert_eq!(dev.with(|s| s.cancels), 1);
    assert_eq!(g.get_storage_ids().await.unwrap().len(), 1);
    drop(g);
    assert!(dev.violations().is_empty(), "{:?}", dev.violations());
}

#[tokio::test]
async fn partial_reads() {
    let dev = FakeDevice::new(FakeConfig::default());
    let data: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
    let h = dev.add_file(0x0001_0001, 0, "p", &data);
    let sess = open(&dev).await;
    let s = DataIn::get_partial_object(sess.lock().await, h, 100, 50).await.unwrap();
    assert_eq!(s.read_to_end().await.unwrap(), &data[100..150]);
    let s = DataIn::get_partial_object_64(sess.lock().await, h, 9_000, 5_000).await.unwrap();
    assert_eq!(s.read_to_end().await.unwrap(), &data[9_000..]);
    let log = dev.op_log();
    assert!(log.contains(&op::GET_PARTIAL_OBJECT) && log.contains(&op::ANDROID_GET_PARTIAL_OBJECT_64));
    assert!(dev.violations().is_empty(), "{:?}", dev.violations());
}

#[tokio::test]
async fn prop_value_get_set_and_move() {
    let dev = FakeDevice::new(FakeConfig::default());
    let st = 0x0001_0001;
    let d = dev.add_dir(st, 0, "Download");
    let f = dev.add_file(st, 0, "old.txt", b"x");
    let sess = open(&dev).await;
    let mut g = sess.lock().await;
    g.set_object_filename(f, "new.txt").await.unwrap();
    let v = g.get_object_prop_value(f, crate::codes::prop::OBJECT_FILE_NAME, crate::codes::datatype::STR).await.unwrap();
    assert_eq!(v.as_str(), Some("new.txt"));
    assert_eq!(g.get_object_size64(f).await.unwrap(), 1);
    g.move_object(f, st, d).await.unwrap();
    assert_eq!(dev.object(f).unwrap().parent, d);
    let props = g.get_object_props_supported(0x3000).await.unwrap();
    assert!(props.contains(&crate::codes::prop::OBJECT_SIZE));
    let e = g.get_thumb(f).await.unwrap_err();
    assert_eq!(e.response_code(), Some(resp::NO_THUMBNAIL_PRESENT));
    drop(g);
    assert!(dev.violations().is_empty(), "{:?}", dev.violations());
}

#[tokio::test]
async fn session_not_open_error() {
    let dev = FakeDevice::new(FakeConfig::default());
    let sess = Session::new(Box::new(dev.transport()));
    let e = sess.lock().await.get_storage_ids().await.unwrap_err();
    assert!(matches!(e, PtpError::Response { code: resp::SESSION_NOT_OPEN, .. }));
}

#[tokio::test]
async fn already_open_session_is_accepted() {
    let dev = FakeDevice::new(FakeConfig::default());
    let sess = open(&dev).await;
    // A second host process / restart: OpenSession again.
    let s2 = Session::new(Box::new(dev.transport()));
    s2.lock().await.open_session(1).await.unwrap();
    drop(sess);
}
