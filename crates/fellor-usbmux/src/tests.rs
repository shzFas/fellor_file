use super::*;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};

const LIST_REPLY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>DeviceList</key>
	<array>
		<dict>
			<key>DeviceID</key><integer>3</integer>
			<key>MessageType</key><string>Attached</string>
			<key>Properties</key>
			<dict>
				<key>ConnectionSpeed</key><integer>480000000</integer>
				<key>ConnectionType</key><string>USB</string>
				<key>DeviceID</key><integer>3</integer>
				<key>LocationID</key><integer>336592896</integer>
				<key>ProductID</key><integer>4776</integer>
				<key>SerialNumber</key><string>00008110-000A1B2C3D4E5F60</string>
			</dict>
		</dict>
		<dict>
			<key>DeviceID</key><integer>7</integer>
			<key>MessageType</key><string>Attached</string>
			<key>Properties</key>
			<dict>
				<key>ConnectionType</key><string>Network</string>
				<key>DeviceID</key><integer>7</integer>
				<key>SerialNumber</key><string>abcdef0123456789abcdef0123456789abcdef01</string>
			</dict>
		</dict>
		<dict><key>Garbage</key><true/></dict>
	</array>
</dict>
</plist>"#;

fn packet(tag: u32, xml: &str) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&((16 + xml.len()) as u32).to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&tag.to_le_bytes());
    v.extend_from_slice(xml.as_bytes());
    v
}

#[test]
fn header_roundtrip_and_layout() {
    let h = Header { length: 0x1234, version: 1, message: 8, tag: 0xdeadbeef };
    let b = h.to_bytes();
    assert_eq!(&b[..], &[0x34, 0x12, 0, 0, 1, 0, 0, 0, 8, 0, 0, 0, 0xef, 0xbe, 0xad, 0xde]);
    assert_eq!(Header::parse(&b), h);
}

#[test]
fn encode_packet_fields() {
    let msg = dict! { "MessageType" => "ListDevices" };
    let p = encode_packet(5, &msg).unwrap();
    let h = Header::parse(p[..16].try_into().unwrap());
    assert_eq!(h.length as usize, p.len());
    assert_eq!(h.version, 1);
    assert_eq!(h.message, 8);
    assert_eq!(h.tag, 5);
    let body = std::str::from_utf8(&p[16..]).unwrap();
    assert!(body.contains("<key>MessageType</key>"));
    assert!(body.contains("<string>ListDevices</string>"));
    let (tag, d) = decode_packet(&p).unwrap();
    assert_eq!(tag, 5);
    assert_eq!(get_str(&d, "MessageType"), Some("ListDevices"));
}

#[test]
fn port_is_byte_swapped() {
    // 62078 = 0xF27E → on the wire as integer 0x7EF2 = 32498 (on LE hosts).
    if cfg!(target_endian = "little") {
        assert_eq!(port_to_wire(62078), 32498);
        assert_eq!(port_to_wire(0x0102), 0x0201);
    }
    assert_eq!(port_to_wire(62078).to_ne_bytes(), 62078u16.to_be_bytes());
}

#[test]
fn decode_rejects_bad_packets() {
    assert!(decode_packet(&[0u8; 4]).is_err());
    // length smaller than header
    let mut p = packet(1, "<plist/>");
    p[0..4].copy_from_slice(&3u32.to_le_bytes());
    assert!(decode_packet(&p).is_err());
    // length larger than data
    let mut p = packet(1, "<plist/>");
    p[0..4].copy_from_slice(&1000u32.to_le_bytes());
    assert!(decode_packet(&p).is_err());
    // huge length
    let mut p = packet(1, "x");
    p[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decode_packet(&p).is_err());
    // wrong message type
    let mut p = packet(1, LIST_REPLY);
    p[8..12].copy_from_slice(&3u32.to_le_bytes());
    assert!(decode_packet(&p).is_err());
    // not a plist
    assert!(decode_packet(&packet(1, "hello")).is_err());
}

#[test]
fn parse_list_devices() {
    let (_, d) = decode_packet(&packet(1, LIST_REPLY)).unwrap();
    let devs = parse_device_list(&d).unwrap();
    assert_eq!(devs.len(), 2);
    assert_eq!(devs[0].device_id, 3);
    assert_eq!(devs[0].udid, "00008110-000A1B2C3D4E5F60");
    assert_eq!(devs[0].connection_type, ConnectionType::Usb);
    assert_eq!(devs[0].product_id, Some(4776));
    assert_eq!(devs[0].location_id, Some(336592896));
    assert_eq!(devs[1].connection_type, ConnectionType::Network);
    assert!(parse_device_list(&Dictionary::new()).is_err());
}

#[test]
fn parse_events() {
    let att = dict! {
        "MessageType" => "Attached",
        "DeviceID" => 9u64,
        "Properties" => Value::Dictionary(dict! { "SerialNumber" => "U1", "ConnectionType" => "USB" }),
    };
    assert_eq!(
        UsbmuxEvent::from_plist(&att),
        Some(UsbmuxEvent::Attached(DeviceRecord {
            device_id: 9,
            udid: "U1".into(),
            connection_type: ConnectionType::Usb,
            product_id: None,
            location_id: None
        }))
    );
    let det = dict! { "MessageType" => "Detached", "DeviceID" => 9u64 };
    assert_eq!(UsbmuxEvent::from_plist(&det), Some(UsbmuxEvent::Detached { device_id: 9 }));
    let paired = dict! { "MessageType" => "Paired", "DeviceID" => 9u64 };
    assert_eq!(UsbmuxEvent::from_plist(&paired), Some(UsbmuxEvent::Paired { device_id: 9 }));
    assert_eq!(UsbmuxEvent::from_plist(&dict! { "MessageType" => "Detached" }), None);
    assert_eq!(UsbmuxEvent::from_plist(&dict! { "MessageType" => "Weird" }), None);
    assert_eq!(UsbmuxEvent::from_plist(&Dictionary::new()), None);
}

pub(crate) fn sample_pair_record_dict() -> Dictionary {
    dict! {
        "HostID" => "HOST-ID-1",
        "SystemBUID" => "BUID-1",
        "HostCertificate" => Value::Data(b"-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n".to_vec()),
        "HostPrivateKey" => Value::Data(b"-----BEGIN RSA PRIVATE KEY-----\nAA==\n-----END RSA PRIVATE KEY-----\n".to_vec()),
        "RootCertificate" => Value::Data(b"root".to_vec()),
        "DeviceCertificate" => Value::Data(b"dev".to_vec()),
        "EscrowBag" => Value::Data(vec![1, 2, 3]),
        "WiFiMACAddress" => "aa:bb:cc:dd:ee:ff",
    }
}

#[test]
fn parse_pair_record() {
    let mut bin = Vec::new();
    Value::Dictionary(sample_pair_record_dict()).to_writer_binary(&mut bin).unwrap();
    let rec = parse_pair_record_reply(&dict! { "PairRecordData" => Value::Data(bin) }).unwrap();
    assert_eq!(rec.host_id, "HOST-ID-1");
    assert_eq!(rec.system_buid.as_deref(), Some("BUID-1"));
    assert_eq!(rec.device_certificate.as_deref(), Some(&b"dev"[..]));
    assert_eq!(rec.escrow_bag.as_deref(), Some(&[1u8, 2, 3][..]));
    assert!(rec.host_private_key.starts_with(b"-----BEGIN RSA"));
    // Debug must not leak key material.
    assert!(!format!("{rec:?}").contains("PRIVATE"));

    // Missing record → NeedsTrust.
    let err = parse_pair_record_reply(&dict! { "MessageType" => "Result", "Number" => 2u64 }).unwrap_err();
    assert!(matches!(err, VfsError::NeedsTrust));
    // Missing HostID → protocol error.
    let mut d = sample_pair_record_dict();
    d.remove("HostID");
    assert!(PairRecord::from_plist(&d).is_err());
    assert!(PairRecord::from_bytes(b"garbage").is_err());
}

#[test]
fn result_codes() {
    assert!(check_result(&dict! { "MessageType" => "Result", "Number" => 0u64 }, "x").is_ok());
    assert!(matches!(check_result(&dict! { "MessageType" => "Result", "Number" => 2u64 }, "x"), Err(VfsError::Disconnected)));
    assert!(matches!(check_result(&dict! { "MessageType" => "Result", "Number" => 3u64 }, "x"), Err(VfsError::Transport(_))));
    assert!(matches!(check_result(&dict! { "MessageType" => "Result", "Number" => 5u64 }, "x"), Err(VfsError::Protocol(_))));
    assert!(check_result(&dict! { "MessageType" => "Result" }, "x").is_err());
    assert!(check_result(&dict! { "Foo" => 1u64 }, "x").is_err());
}

/// Fake usbmuxd: reads one request, checks it, answers.
async fn serve_one(s: &mut DuplexStream, check: impl FnOnce(&Dictionary), reply: Dictionary) {
    let (tag, req) = read_packet(s).await.unwrap();
    check(&req);
    s.write_all(&encode_packet(tag, &reply).unwrap()).await.unwrap();
}

#[tokio::test]
async fn client_list_devices_skips_unrelated_packets() {
    let (a, mut b) = duplex(1 << 16);
    let server = tokio::spawn(async move {
        let (tag, req) = read_packet(&mut b).await.unwrap();
        assert_eq!(get_str(&req, "MessageType"), Some("ListDevices"));
        assert_eq!(get_str(&req, "ProgName"), Some("fellor"));
        // a stray notification first
        b.write_all(&packet(0, "<plist version=\"1.0\"><dict><key>MessageType</key><string>Paired</string></dict></plist>"))
            .await
            .unwrap();
        b.write_all(&packet(tag, LIST_REPLY)).await.unwrap();
    });
    let mut c = UsbmuxClient::new(a);
    let devs = c.list_devices().await.unwrap();
    assert_eq!(devs.len(), 2);
    server.await.unwrap();
}

#[tokio::test]
async fn client_connect_becomes_raw_stream() {
    let (a, mut b) = duplex(1 << 16);
    let server = tokio::spawn(async move {
        serve_one(
            &mut b,
            |req| {
                assert_eq!(get_str(req, "MessageType"), Some("Connect"));
                assert_eq!(get_u64(req, "DeviceID"), Some(3));
                assert_eq!(get_u64(req, "PortNumber"), Some(port_to_wire(62078) as u64));
            },
            dict! { "MessageType" => "Result", "Number" => 0u64 },
        )
        .await;
        // now raw
        let mut buf = [0u8; 4];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        b.write_all(b"pong").await.unwrap();
    });
    let mut raw = UsbmuxClient::new(a).connect_device(3, LOCKDOWN_PORT).await.unwrap();
    raw.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    raw.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"pong");
    server.await.unwrap();
}

#[tokio::test]
async fn client_connect_refused() {
    let (a, mut b) = duplex(1 << 16);
    tokio::spawn(async move {
        serve_one(&mut b, |_| {}, dict! { "MessageType" => "Result", "Number" => 3u64 }).await;
    });
    let err = UsbmuxClient::new(a).connect_device(3, 1234).await.err().unwrap();
    assert!(err.is_retryable(), "{err:?}");
}

#[tokio::test]
async fn client_pair_record_and_buid() {
    let (a, mut b) = duplex(1 << 16);
    tokio::spawn(async move {
        let mut bin = Vec::new();
        Value::Dictionary(sample_pair_record_dict()).to_writer_xml(&mut bin).unwrap();
        serve_one(
            &mut b,
            |req| {
                assert_eq!(get_str(req, "MessageType"), Some("ReadPairRecord"));
                assert_eq!(get_str(req, "PairRecordID"), Some("U1"));
            },
            dict! { "PairRecordData" => Value::Data(bin) },
        )
        .await;
        serve_one(&mut b, |_| {}, dict! { "MessageType" => "Result", "Number" => 2u64 }).await;
        serve_one(&mut b, |req| assert_eq!(get_str(req, "MessageType"), Some("ReadBUID")), dict! { "BUID" => "B-1" }).await;
    });
    let mut c = UsbmuxClient::new(a);
    assert_eq!(c.read_pair_record("U1").await.unwrap().host_id, "HOST-ID-1");
    assert!(matches!(c.read_pair_record("U2").await, Err(VfsError::NeedsTrust)));
    assert_eq!(c.read_buid().await.unwrap(), "B-1");
}

#[tokio::test]
async fn listener_events_and_eof() {
    let (a, mut b) = duplex(1 << 16);
    tokio::spawn(async move {
        serve_one(&mut b, |req| assert_eq!(get_str(req, "MessageType"), Some("Listen")), dict! { "MessageType" => "Result", "Number" => 0u64 }).await;
        let att = dict! {
            "MessageType" => "Attached", "DeviceID" => 4u64,
            "Properties" => Value::Dictionary(dict! { "SerialNumber" => "U4", "ConnectionType" => "USB" }),
        };
        b.write_all(&encode_packet(0, &dict! { "MessageType" => "Unknown" }).unwrap()).await.unwrap();
        b.write_all(&encode_packet(0, &att).unwrap()).await.unwrap();
        b.write_all(&encode_packet(0, &dict! { "MessageType" => "Detached", "DeviceID" => 4u64 }).unwrap()).await.unwrap();
        // drop → EOF
    });
    let mut l = UsbmuxClient::new(a).listen().await.unwrap();
    assert!(matches!(l.next_event().await.unwrap(), UsbmuxEvent::Attached(r) if r.udid == "U4"));
    assert_eq!(l.next_event().await.unwrap(), UsbmuxEvent::Detached { device_id: 4 });
    assert!(matches!(l.next_event().await, Err(VfsError::Disconnected)));
}

#[tokio::test]
async fn garbage_from_server_is_an_error_not_a_panic() {
    let (a, mut b) = duplex(1 << 16);
    tokio::spawn(async move {
        let _ = read_packet(&mut b).await;
        b.write_all(&[0xff; 64]).await.unwrap();
    });
    let mut c = UsbmuxClient::new(a);
    assert!(c.list_devices().await.is_err());
}
