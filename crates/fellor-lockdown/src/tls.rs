//! TLS for lockdown sessions and SSL services, on top of rustls (ring).
//!
//! * Client certificate = `HostCertificate` + `HostPrivateKey` from the pair
//!   record (PKCS#1 "RSA PRIVATE KEY" usually).
//! * The device presents a self-signed certificate (often X.509 v1, RSA with
//!   SHA-1) that no WebPKI verifier would accept. Trust comes from the pair
//!   record, so the verifier compares it with `DeviceCertificate` and skips
//!   signature checks.
//! * [`TlsStream`] is a small hand-written async adapter (no tokio-rustls).

use crate::pem;
use fellor_usbmux::PairRecord;
use fellor_vfs::{Result, VfsError};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::ResolvesClientCert;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer, ServerName, UnixTime};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};
use std::io::{self, Read, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

fn tls_err(msg: impl std::fmt::Display) -> VfsError {
    VfsError::Protocol(format!("TLS: {msg}"))
}

/// Parse a PEM (or raw DER) certificate.
pub fn load_certificate(pem_or_der: &[u8]) -> Result<CertificateDer<'static>> {
    let b = pem::first_block(pem_or_der, &["CERTIFICATE", "TRUSTED CERTIFICATE"])
        .ok_or_else(|| tls_err("no certificate in pair record"))?;
    Ok(CertificateDer::from(b.der))
}

/// Parse a PEM private key: PKCS#1 (`RSA PRIVATE KEY`), PKCS#8 or SEC1.
pub fn load_private_key(pem_or_der: &[u8]) -> Result<PrivateKeyDer<'static>> {
    let b = pem::first_block(pem_or_der, &["RSA PRIVATE KEY", "PRIVATE KEY", "EC PRIVATE KEY"])
        .ok_or_else(|| tls_err("no private key in pair record"))?;
    Ok(match b.label.as_str() {
        "PRIVATE KEY" => PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(b.der)),
        "EC PRIVATE KEY" => PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(b.der)),
        // "RSA PRIVATE KEY" or raw DER: pair records use PKCS#1.
        _ => PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(b.der)),
    })
}

/// Accepts the device certificate from the pair record.
#[derive(Debug)]
pub struct PairRecordVerifier {
    expected: Option<Vec<u8>>,
    strict: bool,
    schemes: Vec<SignatureScheme>,
}

impl PairRecordVerifier {
    /// `strict`: reject a certificate that differs from the pair record's
    /// `DeviceCertificate`. Non-strict only logs a warning.
    pub fn new(device_certificate: Option<&[u8]>, strict: bool) -> Self {
        let expected = device_certificate.and_then(|c| load_certificate(c).ok()).map(|c| c.as_ref().to_vec());
        Self {
            expected,
            strict,
            schemes: vec![
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::ED25519,
                SignatureScheme::RSA_PKCS1_SHA1,
                SignatureScheme::ECDSA_SHA1_Legacy,
            ],
        }
    }
}

impl ServerCertVerifier for PairRecordVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        match &self.expected {
            Some(exp) if exp.as_slice() == end_entity.as_ref() => {}
            Some(_) if self.strict => {
                return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure));
            }
            Some(_) => tracing::warn!("lockdown TLS: device certificate differs from pair record; accepting"),
            None => tracing::debug!("lockdown TLS: pair record has no DeviceCertificate; accepting"),
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

/// Always presents the host certificate.
#[derive(Debug)]
struct HostCert(Arc<CertifiedKey>);

impl ResolvesClientCert for HostCert {
    fn resolve(&self, _hints: &[&[u8]], _schemes: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
    fn has_certs(&self) -> bool {
        true
    }
}

/// Build a rustls client config from a pair record.
pub fn client_config(pair: &PairRecord) -> Result<Arc<ClientConfig>> {
    client_config_with(pair, false)
}

pub fn client_config_with(pair: &PairRecord, strict: bool) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = load_certificate(&pair.host_certificate)?;
    let key = load_private_key(&pair.host_private_key)?;
    let signing_key = provider.key_provider.load_private_key(key).map_err(|e| tls_err(format!("host private key: {e}")))?;
    // CertifiedKey::new (not from_der): pair-record certs may be X.509 v1,
    // which webpki can't parse for the key-consistency check.
    let certified = Arc::new(CertifiedKey::new(vec![cert], signing_key));
    let verifier = Arc::new(PairRecordVerifier::new(pair.device_certificate.as_deref(), strict));
    let mut cfg = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(tls_err)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_cert_resolver(Arc::new(HostCert(certified)));
    cfg.enable_sni = false;
    Ok(Arc::new(cfg))
}

// ---------------------------------------------------------------------------
// Async adapter
// ---------------------------------------------------------------------------

/// Bridges a tokio stream into the blocking `Read`/`Write` rustls expects,
/// mapping `Pending` to `WouldBlock`.
struct SyncIo<'a, 'b, S> {
    io: &'a mut S,
    cx: &'a mut Context<'b>,
}

impl<S: AsyncRead + Unpin> Read for SyncIo<'_, '_, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut rb = ReadBuf::new(buf);
        match Pin::new(&mut *self.io).poll_read(self.cx, &mut rb) {
            Poll::Ready(Ok(())) => Ok(rb.filled().len()),
            Poll::Ready(Err(e)) => Err(e),
            Poll::Pending => Err(io::ErrorKind::WouldBlock.into()),
        }
    }
}

impl<S: AsyncWrite + Unpin> Write for SyncIo<'_, '_, S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match Pin::new(&mut *self.io).poll_write(self.cx, buf) {
            Poll::Ready(r) => r,
            Poll::Pending => Err(io::ErrorKind::WouldBlock.into()),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match Pin::new(&mut *self.io).poll_flush(self.cx) {
            Poll::Ready(r) => r,
            Poll::Pending => Err(io::ErrorKind::WouldBlock.into()),
        }
    }
}

/// A TLS client stream over any tokio byte stream.
pub struct TlsStream<S> {
    io: S,
    conn: ClientConnection,
    eof: bool,
    close_sent: bool,
}

impl<S> std::fmt::Debug for TlsStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsStream").field("eof", &self.eof).finish_non_exhaustive()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> TlsStream<S> {
    /// Perform the TLS handshake over `io` (in-place upgrade).
    pub async fn connect(io: S, config: Arc<ClientConfig>) -> Result<Self> {
        let name = ServerName::IpAddress(std::net::IpAddr::from([127, 0, 0, 1]).into());
        let conn = ClientConnection::new(config, name).map_err(tls_err)?;
        let mut s = TlsStream { io, conn, eof: false, close_sent: false };
        std::future::poll_fn(|cx| s.poll_handshake(cx)).await.map_err(|e| match e.kind() {
            io::ErrorKind::InvalidData => tls_err(e),
            _ => crate::framing::io_err(e),
        })?;
        tracing::debug!(version = ?s.conn.protocol_version(), suite = ?s.conn.negotiated_cipher_suite(), "TLS established");
        Ok(s)
    }

    pub fn get_ref(&self) -> &S {
        &self.io
    }

    pub fn protocol_version(&self) -> Option<rustls::ProtocolVersion> {
        self.conn.protocol_version()
    }

    fn poll_write_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.conn.wants_write() {
            let mut sio = SyncIo { io: &mut self.io, cx };
            match self.conn.write_tls(&mut sio) {
                Ok(0) => return Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Poll::Pending,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
        Poll::Ready(Ok(()))
    }

    /// Read ciphertext and process it. `Ok(false)` = EOF from the peer.
    fn poll_read_tls(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<bool>> {
        let mut sio = SyncIo { io: &mut self.io, cx };
        let n = match self.conn.read_tls(&mut sio) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Poll::Pending,
            Err(e) => return Poll::Ready(Err(e)),
        };
        if let Err(err) = self.conn.process_new_packets() {
            // Best effort: push the alert out before failing.
            let mut sio = SyncIo { io: &mut self.io, cx };
            let _ = self.conn.write_tls(&mut sio);
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::InvalidData, err)));
        }
        Poll::Ready(Ok(n > 0))
    }

    fn poll_handshake(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            ready!(self.poll_write_pending(cx))?;
            ready!(Pin::new(&mut self.io).poll_flush(cx))?;
            if !self.conn.is_handshaking() {
                return Poll::Ready(Ok(()));
            }
            if !ready!(self.poll_read_tls(cx))? {
                return Poll::Ready(Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed during TLS handshake")));
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for TlsStream<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            match this.conn.reader().read(buf.initialize_unfilled()) {
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                // Peer closed TCP without close_notify: treat as EOF.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Poll::Ready(Ok(())),
                Err(e) => return Poll::Ready(Err(e)),
            }
            if this.eof {
                return Poll::Ready(Ok(()));
            }
            // Flush anything rustls wants to send (alerts, key updates); a
            // blocked writer must not stall reading.
            if let Poll::Ready(Err(e)) = this.poll_write_pending(cx) {
                return Poll::Ready(Err(e));
            }
            if !ready!(this.poll_read_tls(cx))? {
                this.eof = true;
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for TlsStream<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        for _ in 0..2 {
            let n = this.conn.writer().write(buf)?;
            if n > 0 {
                if let Poll::Ready(Err(e)) = this.poll_write_pending(cx) {
                    return Poll::Ready(Err(e));
                }
                return Poll::Ready(Ok(n));
            }
            // rustls' send buffer is full: drain it and retry.
            ready!(this.poll_write_pending(cx))?;
        }
        Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.conn.writer().flush()?;
        ready!(this.poll_write_pending(cx))?;
        Pin::new(&mut this.io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.close_sent {
            this.conn.send_close_notify();
            this.close_sent = true;
        }
        ready!(this.poll_write_pending(cx))?;
        Pin::new(&mut this.io).poll_shutdown(cx)
    }
}
