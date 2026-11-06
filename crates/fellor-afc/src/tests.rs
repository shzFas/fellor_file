use crate::fake::{self, FakeFs, Faults};
use crate::proto::{self, op, Header};
use crate::*;
use fellor_lockdown::framing;
use fellor_usbmux::dict;
use fellor_vfs::VfsError;
use plist::Value;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// --- packet codec on hand-built bytes ----------------------------------------

#[test]
fn encode_read_dir_bytes() {
    let p = proto::encode(7, op::READ_DIR, &proto::cstr("/DCIM"), &[]);
    #[rustfmt::skip]
    let expected: Vec<u8> = [
        b"CFA6LPAA".as_slice(),
        &[46, 0, 0, 0, 0, 0, 0, 0],      // entire_len = 40 + 6
        &[46, 0, 0, 0, 0, 0, 0, 0],      // this_len
        &[7, 0, 0, 0, 0, 0, 0, 0],       // packet_num
        &[3, 0, 0, 0, 0, 0, 0, 0],       // ReadDir
        b"/DCIM\0",
    ].concat();
    assert_eq!(p, expected);
}

#[test]
fn encode_write_splits_header_payload_and_data() {
    let p = proto::encode(1, op::FILE_REF_WRITE, &5u64.to_le_bytes(), b"hello");
    let h = Header::parse(p[..40].try_into().unwrap()).unwrap();
    assert_eq!(h.this_len, 48);
    assert_eq!(h.entire_len, 53);
    assert_eq!(h.operation, 0x10);
    assert_eq!(&p[48..], b"hello");
}

#[test]
fn decode_status_and_data_fixtures() {
    #[rustfmt::skip]
    let status: Vec<u8> = [
        b"CFA6LPAA".as_slice(),
        &[48, 0, 0, 0, 0, 0, 0, 0], &[48, 0, 0, 0, 0, 0, 0, 0],
        &[9, 0, 0, 0, 0, 0, 0, 0], &[1, 0, 0, 0, 0, 0, 0, 0],
        &[8, 0, 0, 0, 0, 0, 0, 0], // ObjectNotFound
    ].concat();
    let pkt = proto::decode(&status).unwrap();
    assert_eq!(pkt.header.operation, op::STATUS);
    assert_eq!(pkt.header.packet_num, 9);
    assert_eq!(AfcStatus::from_code(proto::u64_at(&pkt.payload, 0).unwrap()), AfcStatus::ObjectNotFound);

    let data = proto::encode(2, op::DATA, &[], b"st_size\x00123\x00st_ifmt\x00S_IFREG\x00st_mtime\x001700000000123456789\x00");
    let pkt = proto::decode(&data).unwrap();
    let fi = FileInfo::from_kv(proto::parse_kv(&pkt.payload));
    assert_eq!(fi.size, 123);
    assert_eq!(fi.kind, FileType::File);
    assert_eq!(fi.mtime_ns, Some(1_700_000_000_123_456_789));
    assert_eq!(time_to_ns(fi.mtime().unwrap()), 1_700_000_000_123_456_789);
    assert_eq!(fi.birthtime_ns, None);
}

#[test]
fn decode_rejects_garbage() {
    assert!(proto::decode(b"short").is_err());
    let mut p = proto::encode(0, op::DATA, &[], b"abc");
    p[0] = b'X';
    assert!(proto::decode(&p).is_err());
    let mut p = proto::encode(0, op::DATA, &[], b"abc");
    p[8..16].copy_from_slice(&10u64.to_le_bytes()); // entire < header
    assert!(proto::decode(&p).is_err());
    let mut p = proto::encode(0, op::DATA, &[], b"abc");
    p[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(proto::decode(&p).is_err());
    let mut p = proto::encode(0, op::DATA, &[], b"abc");
    p[16..24].copy_from_slice(&100u64.to_le_bytes()); // this > entire
    assert!(proto::decode(&p).is_err());
    let mut p = proto::encode(0, op::DATA, &[], b"abc");
    p[8..16].copy_from_slice(&100u64.to_le_bytes()); // truncated
    p[16..24].copy_from_slice(&40u64.to_le_bytes());
    assert!(proto::decode(&p).is_err());
}

#[test]
fn string_lists() {
    assert_eq!(proto::split_strings(b".\0..\0a b\0\xd0\xb6\0"), vec![".", "..", "a b", "ж"]);
    assert!(proto::split_strings(b"").is_empty());
    assert_eq!(proto::parse_kv(b"a\0b\0c\0"), vec![("a".to_string(), "b".to_string())]);
    let di = DeviceFsInfo::from_kv(proto::parse_kv(b"Model\0iPhone15,2\0FSTotalBytes\x00128\0FSFreeBytes\x0064\0FSBlockSize\x004096\0"));
    assert_eq!((di.total_bytes, di.free_bytes, di.block_size), (Some(128), Some(64), Some(4096)));
    assert_eq!(FileType::parse("S_IFLNK"), FileType::Symlink);
    assert_eq!(FileType::parse("?"), FileType::Unknown);
}

// --- client against the in-memory server ------------------------------------

fn client(fs: &Arc<std::sync::Mutex<FakeFs>>) -> AfcClient {
    AfcClient::new(fake::spawn(fs.clone(), Arc::new(Faults::default())))
}

#[tokio::test]
async fn dir_ops_and_errors() {
    let fs = FakeFs::shared();
    {
        let mut f = fs.lock().unwrap();
        f.put_file("/DCIM/100APPLE/IMG_0001.HEIC", b"heic", 42);
        f.put_file("/DCIM/100APPLE/IMG_0001.MOV", b"mov!", 43);
    }
    let mut c = client(&fs);
    let mut names = c.read_dir("/DCIM/100APPLE").await.unwrap();
    names.sort();
    assert_eq!(names, vec!["IMG_0001.HEIC", "IMG_0001.MOV"]);
    assert!(matches!(c.read_dir("/nope").await, Err(VfsError::NotFound(p)) if p == "/nope"));

    let fi = c.file_info("/DCIM/100APPLE/IMG_0001.HEIC").await.unwrap();
    assert_eq!((fi.size, fi.kind, fi.mtime_ns), (4, FileType::File, Some(42)));
    assert_eq!(c.file_info("/DCIM").await.unwrap().kind, FileType::Dir);

    let paths = vec!["/DCIM/100APPLE/IMG_0001.HEIC".to_string(), "/missing".to_string(), "/DCIM/100APPLE/IMG_0001.MOV".to_string()];
    let many = c.file_info_many(&paths).await.unwrap();
    assert_eq!(many.len(), 3);
    assert_eq!(many[0].as_ref().unwrap().mtime_ns, Some(42));
    assert!(matches!(many[1], Err(VfsError::NotFound(_))));
    assert_eq!(many[2].as_ref().unwrap().mtime_ns, Some(43));
    // pipelining > window size
    let big: Vec<String> = (0..100).map(|i| if i % 2 == 0 { "/DCIM".into() } else { format!("/x{i}") }).collect();
    let r = c.file_info_many(&big).await.unwrap();
    assert_eq!(r.iter().filter(|x| x.is_ok()).count(), 50);

    c.make_dir("/Downloads/a/b").await.unwrap();
    assert_eq!(c.file_info("/Downloads/a/b").await.unwrap().kind, FileType::Dir);
    assert!(matches!(c.remove_path("/Downloads/a").await, Err(VfsError::NotEmpty(_))));
    c.remove_path_and_contents("/Downloads/a").await.unwrap();
    assert!(matches!(c.file_info("/Downloads/a/b").await, Err(VfsError::NotFound(_))));
    assert!(matches!(c.remove_path("/Downloads/a").await, Err(VfsError::NotFound(_))));

    c.rename_path("/DCIM/100APPLE", "/DCIM/101APPLE").await.unwrap();
    assert!(c.file_info("/DCIM/101APPLE/IMG_0001.MOV").await.is_ok());
    assert!(matches!(c.rename_path("/zzz", "/y").await, Err(VfsError::NotFound(_))));

    c.set_file_time("/DCIM/101APPLE/IMG_0001.MOV", 1_600_000_000_000_000_000).await.unwrap();
    assert_eq!(c.file_info("/DCIM/101APPLE/IMG_0001.MOV").await.unwrap().mtime_ns, Some(1_600_000_000_000_000_000));

    let di = c.device_info().await.unwrap();
    assert_eq!(di.total_bytes, Some(64 << 30));
    assert_eq!(di.free_bytes, Some(10 << 30));
    assert!(c.read_dir("bad\0path").await.is_err());
}

#[tokio::test]
async fn file_io_roundtrip() {
    let fs = FakeFs::shared();
    let mut c = client(&fs);
    let data: Vec<u8> = (0..(2 * MAX_IO_CHUNK + 1234)).map(|i| (i % 253) as u8).collect();
    let h = c.file_open("/Downloads.bin", OpenMode::WriteOnly).await.unwrap();
    c.file_write(h, &data).await.unwrap();
    assert_eq!(c.file_tell(h).await.unwrap(), data.len() as u64);
    c.file_close(h).await.unwrap();
    assert_eq!(fs.lock().unwrap().file("/Downloads.bin").unwrap(), &data[..]);
    // write was split into 3 packets
    assert_eq!(fs.lock().unwrap().ops.iter().filter(|o| **o == op::FILE_REF_WRITE).count(), 3);

    let h = c.file_open("/Downloads.bin", OpenMode::ReadOnly).await.unwrap();
    c.file_seek(h, 1000, proto::SEEK_SET).await.unwrap();
    let mut back = Vec::new();
    loop {
        let chunk = c.file_read(h, 4 << 20).await.unwrap();
        assert!(chunk.len() <= MAX_IO_CHUNK);
        if chunk.is_empty() {
            break;
        }
        back.extend_from_slice(&chunk);
    }
    assert_eq!(back, &data[1000..]);
    c.file_close(h).await.unwrap();
    assert!(c.file_close(h).await.is_err());
    assert!(matches!(c.file_open("/nope/x", OpenMode::ReadOnly).await, Err(VfsError::NotFound(_))));
    assert!(matches!(c.file_open("/", OpenMode::ReadOnly).await, Err(VfsError::IsDir(_))));

    fs.lock().unwrap().free_bytes = 0;
    let h = c.file_open("/full", OpenMode::WriteOnly).await.unwrap();
    assert!(matches!(c.file_write(h, b"x").await, Err(VfsError::NoSpace)));
}

#[tokio::test]
async fn disconnect_mid_request_is_retryable() {
    let fs = FakeFs::shared();
    let faults = Arc::new(Faults::default());
    let mut c = AfcClient::new(fake::spawn(fs, faults.clone()));
    c.read_dir("/").await.unwrap();
    faults.drop_next.store(true, std::sync::atomic::Ordering::SeqCst);
    let e = c.read_dir("/").await.unwrap_err();
    assert!(e.is_retryable(), "{e:?}");
}

#[tokio::test]
async fn mismatched_packet_number_is_protocol_error() {
    let (a, mut b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let mut hb = [0u8; 40];
        b.read_exact(&mut hb).await.unwrap();
        let h = Header::parse(&hb).unwrap();
        let mut rest = vec![0u8; (h.entire_len - 40) as usize];
        b.read_exact(&mut rest).await.unwrap();
        b.write_all(&proto::encode(h.packet_num + 5, op::DATA, &[], b"x\0")).await.unwrap();
    });
    let mut c = AfcClient::new(Box::new(a));
    assert!(matches!(c.read_dir("/").await, Err(VfsError::Protocol(_))));
}

#[tokio::test]
async fn read_at_eof_status_reply_is_empty() {
    let (a, mut b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        for code in [0u64, 14] {
            let mut hb = [0u8; 40];
            b.read_exact(&mut hb).await.unwrap();
            let h = Header::parse(&hb).unwrap();
            let mut rest = vec![0u8; (h.entire_len - 40) as usize];
            b.read_exact(&mut rest).await.unwrap();
            assert_eq!(h.operation, op::FILE_REF_READ);
            b.write_all(&proto::encode(h.packet_num, op::STATUS, &code.to_le_bytes(), &[])).await.unwrap();
        }
    });
    let mut c = AfcClient::new(Box::new(a));
    assert!(c.file_read(1, 10).await.unwrap().is_empty());
    assert!(c.file_read(1, 10).await.unwrap().is_empty());
}

// --- house_arrest / installation_proxy --------------------------------------

#[tokio::test]
async fn house_arrest_vend_then_afc() {
    let (a, mut b) = tokio::io::duplex(1 << 16);
    let fs = FakeFs::shared();
    fs.lock().unwrap().put_file("/Documents/notes.txt", b"hi", 1);
    let fs2 = fs.clone();
    tokio::spawn(async move {
        let req = framing::read_frame(&mut b).await.unwrap();
        assert_eq!(req.get("Command").and_then(Value::as_string), Some("VendDocuments"));
        assert_eq!(req.get("Identifier").and_then(Value::as_string), Some("com.example.app"));
        framing::write_frame(&mut b, &dict! { "Status" => "Complete" }).await.unwrap();
        fake::serve(b, fs2, Arc::new(Faults::default())).await;
    });
    let mut afc = house_arrest::vend_documents(Box::new(a), "com.example.app").await.unwrap();
    assert_eq!(afc.read_dir(house_arrest::DOCUMENTS).await.unwrap(), vec!["notes.txt"]);
}

#[test]
fn house_arrest_errors() {
    assert!(house_arrest::check_vend_reply(&dict! { "Status" => "Complete" }, "x").is_ok());
    assert!(matches!(house_arrest::check_vend_reply(&dict! { "Error" => "ApplicationLookupFailed" }, "x"), Err(VfsError::NotFound(_))));
    assert!(matches!(house_arrest::check_vend_reply(&dict! { "Error" => "InstallationIsNotDeveloperSigned" }, "x"), Err(VfsError::PermissionDenied(_))));
    assert!(house_arrest::check_vend_reply(&dict! {}, "x").is_err());
}

#[tokio::test]
async fn installation_proxy_browse_chunks() {
    let (a, mut b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let req = framing::read_frame(&mut b).await.unwrap();
        assert_eq!(req.get("Command").and_then(Value::as_string), Some("Browse"));
        let opts = req.get("ClientOptions").and_then(Value::as_dictionary).unwrap();
        assert_eq!(opts.get("ApplicationType").and_then(Value::as_string), Some("User"));
        assert!(opts.get("ReturnAttributes").and_then(Value::as_array).unwrap().len() >= 3);
        let app = |id: &str, name: &str, fs: bool| Value::Dictionary(dict! { "CFBundleIdentifier" => id, "CFBundleDisplayName" => name, "UIFileSharingEnabled" => fs });
        framing::write_frame(&mut b, &dict! { "Status" => "BrowsingApplications", "CurrentList" => Value::Array(vec![app("com.v.vlc", "VLC", true), app("com.x.nofs", "NoFS", false)]) }).await.unwrap();
        framing::write_frame(&mut b, &dict! { "Status" => "BrowsingApplications", "CurrentList" => Value::Array(vec![app("com.a.docs", "documents", true), Value::Dictionary(dict! { "CFBundleIdentifier" => "com.noname", "UIFileSharingEnabled" => "YES" })]) }).await.unwrap();
        framing::write_frame(&mut b, &dict! { "Status" => "Complete" }).await.unwrap();
    });
    let apps = installation_proxy::file_sharing_apps(Box::new(a)).await.unwrap();
    let names: Vec<_> = apps.iter().map(|a| a.display_name.as_str()).collect();
    assert_eq!(names, vec!["com.noname", "documents", "VLC"]);
    assert!(apps.iter().all(|a| a.file_sharing));
}

#[tokio::test]
async fn installation_proxy_error() {
    let (a, mut b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let _ = framing::read_frame(&mut b).await.unwrap();
        framing::write_frame(&mut b, &dict! { "Error" => "InvalidArgument", "ErrorDescription" => "nope" }).await.unwrap();
    });
    assert!(installation_proxy::file_sharing_apps(Box::new(a)).await.is_err());
}
