use super::*;
use fellor_usbmux::plist_util::get_str;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, DistinguishedName, ServerConfig, ServerConnection, SignatureScheme, StreamOwned};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream as StdUnix;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn pair_record(device_cert: &str) -> PairRecord {
    PairRecord {
        host_id: "HOST-1".into(),
        system_buid: Some("BUID-1".into()),
        host_certificate: fixture("host_cert.pem"),
        host_private_key: fixture("host_key.pem"),
        root_certificate: None,
        root_private_key: None,
        device_certificate: Some(fixture(device_cert)),
        escrow_bag: None,
        wifi_mac_address: None,
    }
}

// --- framing ---------------------------------------------------------------

#[test]
fn frame_layout() {
    let f = framing::encode_frame(&dict! { "Request" => "QueryType" }).unwrap();
    let len = u32::from_be_bytes(f[..4].try_into().unwrap()) as usize;
    assert_eq!(len + 4, f.len());
    assert!(std::str::from_utf8(&f[4..]).unwrap().contains("<string>QueryType</string>"));
    let (d, used) = framing::decode_frame(&f).unwrap();
    assert_eq!(used, f.len());
    assert_eq!(get_str(&d, "Request"), Some("QueryType"));
}

#[test]
fn frame_fixture_and_garbage() {
    let xml = br#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>Type</key><string>com.apple.mobile.lockdown</string></dict></plist>"#;
    let mut f = (xml.len() as u32).to_be_bytes().to_vec();
    f.extend_from_slice(xml);
    let (d, _) = framing::decode_frame(&f).unwrap();
    assert_eq!(get_str(&d, "Type"), Some(LOCKDOWN_SERVICE_TYPE));
    // binary plists are accepted too
    let mut bin = Vec::new();
    Value::Dictionary(dict! { "A" => 1u64 }).to_writer_binary(&mut bin).unwrap();
    let mut f2 = (bin.len() as u32).to_be_bytes().to_vec();
    f2.extend_from_slice(&bin);
    assert!(framing::decode_frame(&f2).is_ok());

    assert!(framing::decode_frame(&[0, 0]).is_err());
    assert!(framing::decode_frame(&[0, 0, 0, 0]).is_err());
    assert!(framing::decode_frame(&[0xff, 0xff, 0xff, 0xff, 1]).is_err());
    assert!(framing::decode_frame(&[0, 0, 0, 10, 1, 2]).is_err());
    assert!(framing::decode_frame(&[0, 0, 0, 3, b'a', b'b', b'c']).is_err());
}

#[test]
fn start_service_parsing() {
    let s = parse_start_service(&dict! { "Port" => 49152u64, "EnableServiceSSL" => true, "Service" => "x" }, "x").unwrap();
    assert_eq!(s, ServiceInfo { name: "x".into(), port: 49152, ssl: true });
    let s = parse_start_service(&dict! { "Port" => 1000u64 }, "afc").unwrap();
    assert!(!s.ssl);
    assert_eq!(s.name, "afc");
    assert!(parse_start_service(&dict! { "Port" => 70000u64 }, "x").is_err());
    assert!(parse_start_service(&dict! { "Port" => 0u64 }, "x").is_err());
    assert!(parse_start_service(&Dictionary::new(), "x").is_err());
}

// --- plaintext client over an in-memory pipe -------------------------------

#[tokio::test]
async fn plain_requests_and_errors() {
    let (a, mut b) = tokio::io::duplex(1 << 16);
    let server = tokio::spawn(async move {
        let req = framing::read_frame(&mut b).await.unwrap();
        assert_eq!(get_str(&req, "Request"), Some("QueryType"));
        assert_eq!(get_str(&req, "Label"), Some(LABEL));
        framing::write_frame(&mut b, &dict! { "Request" => "QueryType", "Type" => LOCKDOWN_SERVICE_TYPE }).await.unwrap();

        let req = framing::read_frame(&mut b).await.unwrap();
        assert_eq!(get_str(&req, "Key"), Some("DeviceName"));
        framing::write_frame(&mut b, &dict! { "Key" => "DeviceName", "Value" => "Phone" }).await.unwrap();

        let _ = framing::read_frame(&mut b).await.unwrap();
        framing::write_frame(&mut b, &dict! { "Request" => "GetValue", "Error" => "MissingValue" }).await.unwrap();

        let req = framing::read_frame(&mut b).await.unwrap();
        assert_eq!(get_str(&req, "HostID"), Some("HOST-1"));
        assert_eq!(get_str(&req, "SystemBUID"), Some("BUID-1"));
        framing::write_frame(&mut b, &dict! { "Request" => "StartSession", "Error" => "InvalidHostID" }).await.unwrap();

        let _ = framing::read_frame(&mut b).await.unwrap();
        framing::write_frame(&mut b, &dict! { "Error" => "PasswordProtected" }).await.unwrap();

        let _ = framing::read_frame(&mut b).await.unwrap();
        framing::write_frame(&mut b, &dict! { "SessionID" => "S", "EnableSessionSSL" => false }).await.unwrap();
    });
    let pair = pair_record("device_cert.pem");
    let cfg = tls::client_config(&pair).unwrap();
    let mut ld = LockdownClient::new(Box::new(a));
    assert_eq!(ld.query_type().await.unwrap(), LOCKDOWN_SERVICE_TYPE);
    assert_eq!(ld.get_string("DeviceName").await.unwrap(), "Phone");
    assert!(matches!(ld.get_value(None, Some("Nope")).await, Err(VfsError::NotFound(_))));
    assert!(matches!(ld.start_session(&pair, "BUID-1", cfg.clone()).await, Err(VfsError::NeedsTrust)));
    assert!(matches!(ld.start_session(&pair, "BUID-1", cfg.clone()).await, Err(VfsError::Locked)));
    let s = ld.start_session(&pair, "BUID-1", cfg).await.unwrap();
    assert_eq!(s, SessionInfo { session_id: "S".into(), ssl: false });
    server.await.unwrap();
    // server gone → Disconnected
    assert!(matches!(ld.query_type().await, Err(VfsError::Disconnected)));
}

// --- TLS -------------------------------------------------------------------

#[test]
fn pem_keys_load() {
    let pair = pair_record("device_cert.pem");
    assert!(matches!(tls::load_private_key(&pair.host_private_key).unwrap(), rustls::pki_types::PrivateKeyDer::Pkcs1(_)));
    assert!(tls::load_certificate(&pair.host_certificate).is_ok());
    assert!(tls::load_certificate(b"nope").is_err());
    let mut bad = pair.clone();
    bad.host_private_key = b"-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n".to_vec();
    assert!(tls::client_config(&bad).is_err());
}

#[derive(Debug)]
struct TestClientVerifier {
    seen: Arc<Mutex<Option<Vec<u8>>>>,
}

impl ClientCertVerifier for TestClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(&self, ee: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: UnixTime) -> std::result::Result<ClientCertVerified, rustls::Error> {
        *self.seen.lock().unwrap() = Some(ee.as_ref().to_vec());
        Ok(ClientCertVerified::assertion())
    }
    // Really verify the client's signatures: proves the PKCS#1 host key signs correctly.
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &rustls::crypto::ring::default_provider().signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &rustls::crypto::ring::default_provider().signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug)]
struct FixedServerCert(Arc<CertifiedKey>);
impl ResolvesServerCert for FixedServerCert {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

/// A "device" TLS server config: self-signed X.509 v1 SHA-1 cert, requires a client cert.
type SeenCert = Arc<Mutex<Option<Vec<u8>>>>;

fn device_server_config(versions: &[&'static rustls::SupportedProtocolVersion]) -> (Arc<ServerConfig>, SeenCert) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = tls::load_certificate(&fixture("device_cert.pem")).unwrap();
    let key = provider.key_provider.load_private_key(tls::load_private_key(&fixture("device_key.pem")).unwrap()).unwrap();
    let seen = Arc::new(Mutex::new(None));
    let cfg = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .unwrap()
        .with_client_cert_verifier(Arc::new(TestClientVerifier { seen: seen.clone() }))
        .with_cert_resolver(Arc::new(FixedServerCert(Arc::new(CertifiedKey::new(vec![cert], key)))));
    (Arc::new(cfg), seen)
}

fn sync_read_frame(r: &mut impl Read) -> Dictionary {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).unwrap();
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    r.read_exact(&mut body).unwrap();
    fellor_usbmux::plist_util::from_bytes(&body).unwrap()
}

fn sync_write_frame(w: &mut impl Write, d: &Dictionary) {
    w.write_all(&framing::encode_frame(d).unwrap()).unwrap();
    w.flush().unwrap();
}

fn tokio_side(s: StdUnix) -> BoxIo {
    s.set_nonblocking(true).unwrap();
    Box::new(tokio::net::UnixStream::from_std(s).unwrap())
}

async fn tls_echo_roundtrip(versions: &[&'static rustls::SupportedProtocolVersion], expect: rustls::ProtocolVersion) {
    let (c, s) = StdUnix::pair().unwrap();
    let (cfg, seen) = device_server_config(versions);
    let server = std::thread::spawn(move || {
        let conn = ServerConnection::new(cfg).unwrap();
        let mut tls = StreamOwned::new(conn, s);
        // echo 1 MiB + a bit, in odd-sized pieces
        let mut buf = vec![0u8; (1 << 20) + 7];
        tls.read_exact(&mut buf).unwrap();
        tls.write_all(&buf).unwrap();
        tls.flush().unwrap();
        tls.conn.send_close_notify();
        let _ = tls.flush();
    });
    let pair = pair_record("device_cert.pem");
    let mut t = TlsStream::connect(tokio_side(c), tls::client_config(&pair).unwrap()).await.unwrap();
    assert_eq!(t.protocol_version(), Some(expect));
    let data: Vec<u8> = (0..(1usize << 20) + 7).map(|i| (i * 31 % 251) as u8).collect();
    let (mut rd, mut wr) = tokio::io::split(&mut t);
    let send = async {
        wr.write_all(&data).await.unwrap();
        wr.flush().await.unwrap();
    };
    let mut back = Vec::new();
    let recv = async {
        rd.read_to_end(&mut back).await.unwrap();
    };
    tokio::join!(send, recv);
    assert_eq!(back, data);
    server.join().unwrap();
    let host_der = tls::load_certificate(&pair.host_certificate).unwrap();
    assert_eq!(seen.lock().unwrap().as_deref(), Some(host_der.as_ref()), "server must receive the host certificate");
}

#[tokio::test]
async fn tls12_with_v1_sha1_device_cert_and_pkcs1_client_key() {
    tls_echo_roundtrip(&[&rustls::version::TLS12], rustls::ProtocolVersion::TLSv1_2).await;
}

#[tokio::test]
async fn tls13_roundtrip() {
    tls_echo_roundtrip(&[&rustls::version::TLS13], rustls::ProtocolVersion::TLSv1_3).await;
}

#[tokio::test]
async fn device_cert_mismatch_strict_vs_permissive() {
    for strict in [true, false] {
        let (c, s) = StdUnix::pair().unwrap();
        let (cfg, _) = device_server_config(&[&rustls::version::TLS12]);
        let server = std::thread::spawn(move || {
            let mut conn = ServerConnection::new(cfg).unwrap();
            let mut s = s;
            let _ = conn.complete_io(&mut s);
        });
        let pair = pair_record("other_cert.pem");
        let res = TlsStream::connect(tokio_side(c), tls::client_config_with(&pair, strict).unwrap()).await;
        assert_eq!(res.is_err(), strict, "strict={strict}");
        drop(res);
        server.join().unwrap();
    }
}

#[tokio::test]
async fn handshake_against_closed_peer_fails_cleanly() {
    let (c, s) = StdUnix::pair().unwrap();
    drop(s);
    let pair = pair_record("device_cert.pem");
    let err = TlsStream::connect(tokio_side(c), tls::client_config(&pair).unwrap()).await.unwrap_err();
    assert!(err.is_retryable() || matches!(err, VfsError::Protocol(_)), "{err:?}");
}

#[tokio::test]
async fn handshake_against_garbage_fails_cleanly() {
    let (c, s) = StdUnix::pair().unwrap();
    let t = std::thread::spawn(move || {
        let mut s = s;
        let mut b = [0u8; 512];
        let _ = s.read(&mut b);
        let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
    });
    let pair = pair_record("device_cert.pem");
    assert!(TlsStream::connect(tokio_side(c), tls::client_config(&pair).unwrap()).await.is_err());
    t.join().unwrap();
}

/// Full lockdown flow: plaintext QueryType/StartSession, TLS upgrade in
/// place, then GetValue/StartService over TLS.
#[tokio::test]
async fn lockdown_session_with_tls_upgrade() {
    let (c, s) = StdUnix::pair().unwrap();
    let (cfg, seen) = device_server_config(&[&rustls::version::TLS12, &rustls::version::TLS13]);
    let server = std::thread::spawn(move || {
        let mut s = s;
        let req = sync_read_frame(&mut s);
        assert_eq!(get_str(&req, "Request"), Some("QueryType"));
        sync_write_frame(&mut s, &dict! { "Request" => "QueryType", "Type" => LOCKDOWN_SERVICE_TYPE });
        let req = sync_read_frame(&mut s);
        assert_eq!(get_str(&req, "Request"), Some("StartSession"));
        assert_eq!(get_str(&req, "HostID"), Some("HOST-1"));
        sync_write_frame(&mut s, &dict! { "Request" => "StartSession", "SessionID" => "SESS", "EnableSessionSSL" => true });

        let mut tls = StreamOwned::new(ServerConnection::new(cfg).unwrap(), s);
        let req = sync_read_frame(&mut tls);
        assert_eq!(get_str(&req, "Request"), Some("GetValue"));
        assert!(req.get("Key").is_none());
        let all = dict! { "DeviceName" => "Alex’s iPhone", "ProductType" => "iPhone15,2", "ProductVersion" => "18.1", "DeviceClass" => "iPhone" };
        sync_write_frame(&mut tls, &dict! { "Request" => "GetValue", "Value" => Value::Dictionary(all) });
        let req = sync_read_frame(&mut tls);
        assert_eq!(get_str(&req, "Service"), Some("com.apple.mobile.house_arrest"));
        sync_write_frame(&mut tls, &dict! { "Request" => "StartService", "Service" => "com.apple.mobile.house_arrest", "Port" => 50123u64, "EnableServiceSSL" => true });
        let _ = sync_read_frame(&mut tls);
        sync_write_frame(&mut tls, &dict! { "Request" => "StartService", "Error" => "InvalidService" });
    });
    let pair = pair_record("device_cert.pem");
    let mut ld = LockdownClient::new(tokio_side(c));
    assert_eq!(ld.query_type().await.unwrap(), LOCKDOWN_SERVICE_TYPE);
    let sess = ld.start_session(&pair, "BUID-1", tls::client_config(&pair).unwrap()).await.unwrap();
    assert!(sess.ssl);
    assert_eq!(sess.session_id, "SESS");
    let vals = match ld.get_value(None, None).await.unwrap() {
        Value::Dictionary(d) => DeviceValues::from_dict(&d),
        v => panic!("{v:?}"),
    };
    assert_eq!(vals.device_name.as_deref(), Some("Alex’s iPhone"));
    assert_eq!(vals.product_type.as_deref(), Some("iPhone15,2"));
    assert_eq!(vals.device_class.as_deref(), Some("iPhone"));
    let svc = ld.start_service("com.apple.mobile.house_arrest", None).await.unwrap();
    assert_eq!(svc.port, 50123);
    assert!(svc.ssl);
    assert!(matches!(ld.start_service("bogus", None).await, Err(VfsError::PermissionDenied(_))));
    server.join().unwrap();
    assert!(seen.lock().unwrap().is_some());
}
