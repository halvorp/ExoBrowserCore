//! Networking half of the GTK client. Runs a tokio runtime on a
//! background std::thread, dials the QUIC host, drains the video uni
//! stream, and pushes decoded frames onto a glib::MainContext channel
//! so the GTK main thread can build a GdkMemoryTexture for each.
//!
//! Ctrl-out direction: any part of the app can push a `Message` into
//! `ctrl_tx` (tokio mpsc) — a dedicated writer task consumes and
//! serializes to the bidi ctrl stream.

use anyhow::{anyhow, Context, Result};
use gutted_proto::{caps, Message, PROTO_VERSION};
use quinn::{ClientConfig, Endpoint};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::{net::SocketAddr, sync::Arc, time::Duration};

const ALPN: &[u8] = b"gbrowser/1";

/// A frame update from the wire. GTK composites `Sub` onto `Full`.
pub enum GtkFrame {
    Full { width: u32, height: u32, stride: u32, pixels: Vec<u8> },
    Sub  { x: u32, y: u32, w: u32, h: u32, stride: u32, pixels: Vec<u8> },
    /// WebKit load state — used by the GTK URL entry to show progress.
    Load(u8),
    /// URL the server was already on when we connected — populate URL bar.
    Url(String),
    /// Page <title> change — GTK sets window title.
    Title(String),
    /// Committed URL change — GTK sets url entry unless user is editing.
    UrlChanged(String),
}

/// Anything the GTK thread wants to send back to the host.
pub enum OutMsg {
    Nav(String),
    Resize { w: u16, h: u16 },
    PointerMotion { x: i32, y: i32, mods: u32 },
    PointerButton { x: i32, y: i32, button: u32, pressed: bool, mods: u32 },
    Scroll { dx: i32, dy: i32 },
    SetZoom { level_milli: u32 },
    /// History nav. 0=back, 1=forward, 2=reload.
    NavAction { action: u8 },
}

/// Public entry point — call from the network std::thread. Blocks until
/// the connection closes or an error. Frames come via `frames_tx`;
/// outbound commands from GTK come via `out_rx`.
pub async fn run(
    server: SocketAddr,
    cert_pin_sha256: Option<Vec<u8>>,
    frames_tx: glib::Sender<GtkFrame>,
    mut out_rx: tokio::sync::mpsc::UnboundedReceiver<OutMsg>,
) -> Result<()> {
    rustls::crypto::ring::default_provider().install_default().ok();
    let client_cfg = make_client_config(cert_pin_sha256)?;
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_cfg);

    tracing::info!(%server, "gtk client dialing");
    let conn = endpoint
        .connect(server, "localhost")?
        .await
        .context("QUIC connect")?;
    tracing::info!(rtt = ?conn.rtt(), "connected");

    let (mut send, mut recv) = conn.open_bi().await.context("open ctrl bi stream")?;

    // HELLO
    let hello = Message::Hello {
        proto_version: PROTO_VERSION,
        viewport_w: 1280, viewport_h: 720,
        dpr_hundredths: 100,
        client_name: "gutted-client-gtk".into(),
        capabilities: caps::H264 | caps::CLIENT_SCROLL,
    };
    let mut buf = Vec::with_capacity(64);
    hello.encode(&mut buf);
    send.write_all(&buf).await?;

    // WELCOME
    let mut inbuf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; 8192];
    let welcome = read_next(&mut recv, &mut inbuf, &mut chunk).await?
        .ok_or_else(|| anyhow!("ctrl stream closed before WELCOME"))?;
    let Message::Welcome { session_id, current_url, .. } = welcome
        else { return Err(anyhow!("expected WELCOME, got {:?}", welcome)); };
    tracing::info!(session = format!("0x{session_id:016x}"), %current_url, "WELCOME");
    if !current_url.is_empty() {
        let _ = frames_tx.send(GtkFrame::Url(current_url));
    }

    // Ctrl-out task: only Nav goes here. Everything else opens the input
    // uni stream so it doesn't share head-of-line with ctrl.
    let (input_open_tx, mut input_open_rx) =
        tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let conn_i = conn.clone();
    let input_writer = tokio::spawn(async move {
        let mut vs: Option<quinn::SendStream> = None;
        while let Some(bytes) = input_open_rx.recv().await {
            if vs.is_none() {
                match conn_i.open_uni().await {
                    Ok(s) => { tracing::info!("input uni-stream opened"); vs = Some(s); }
                    Err(e) => { tracing::warn!(error = %e, "open input stream"); break; }
                }
            }
            if let Some(s) = vs.as_mut() {
                if s.write_all(&bytes).await.is_err() { break; }
            }
        }
    });

    let ctrl_writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            match msg {
                OutMsg::Nav(url) => {
                    let m = Message::Nav { url };
                    let mut b = Vec::with_capacity(64);
                    m.encode(&mut b);
                    if send.write_all(&b).await.is_err() { break; }
                }
                OutMsg::Resize { w, h } => {
                    let m = Message::Resize { viewport_w: w, viewport_h: h, dpr_hundredths: 100 };
                    let mut b = Vec::with_capacity(16);
                    m.encode(&mut b);
                    if send.write_all(&b).await.is_err() { break; }
                }
                OutMsg::SetZoom { level_milli } => {
                    let m = Message::SetZoom { level_milli };
                    let mut b = Vec::with_capacity(8);
                    m.encode(&mut b);
                    if send.write_all(&b).await.is_err() { break; }
                }
                OutMsg::NavAction { action } => {
                    let m = Message::NavAction { action };
                    let mut b = Vec::with_capacity(8);
                    m.encode(&mut b);
                    if send.write_all(&b).await.is_err() { break; }
                }
                other => {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    let ts_us = SystemTime::now().duration_since(UNIX_EPOCH)
                        .unwrap_or_default().as_micros() as u64;
                    let m = match other {
                        OutMsg::PointerMotion { x, y, mods } =>
                            Message::InputPointer { ts_us, x, y, modifiers: mods },
                        OutMsg::PointerButton { x, y, button, pressed, mods } =>
                            Message::InputButton { ts_us, x, y, button, pressed, modifiers: mods },
                        OutMsg::Scroll { dx, dy } =>
                            Message::InputScroll {
                                ts_us, layer_id: 0,
                                dx_units: dx, dy_units: dy,
                                phase: gutted_proto::ScrollPhase::Update,
                            },
                        OutMsg::Nav(_)
                        | OutMsg::Resize { .. }
                        | OutMsg::SetZoom { .. }
                        | OutMsg::NavAction { .. } => unreachable!(),
                    };
                    let mut b = Vec::with_capacity(48);
                    m.encode(&mut b);
                    let _ = input_open_tx.send(b);
                }
            }
        }
    });

    // Video uni-stream: accept + decode + push frames to GTK.
    let conn_v = conn.clone();
    let video = tokio::spawn(async move {
        let mut vs = match conn_v.accept_uni().await {
            Ok(s) => s,
            Err(e) => { tracing::warn!(error = %e, "no video stream"); return; }
        };
        tracing::info!("video uni-stream accepted");
        let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            {
                let mut cur = buf.as_slice();
                while let Ok(Some(msg)) = Message::decode(&mut cur) {
                    match msg {
                        Message::RawFrame { width, height, stride, pixels, .. } => {
                            let _ = frames_tx.send(GtkFrame::Full {
                                width: width as u32, height: height as u32,
                                stride, pixels,
                            });
                        }
                        Message::Subframe { x, y, w, h, stride, pixels, .. } => {
                            let _ = frames_tx.send(GtkFrame::Sub {
                                x: x as u32, y: y as u32,
                                w: w as u32, h: h as u32,
                                stride, pixels,
                            });
                        }
                        Message::LoadState { state } => {
                            let _ = frames_tx.send(GtkFrame::Load(state));
                        }
                        Message::Title { title } => {
                            let _ = frames_tx.send(GtkFrame::Title(title));
                        }
                        Message::UrlChanged { url } => {
                            let _ = frames_tx.send(GtkFrame::UrlChanged(url));
                        }
                        _ => {}
                    }
                }
                let consumed = buf.len() - cur.len();
                buf.drain(..consumed);
            }
            match vs.read(&mut chunk).await {
                Ok(None) => break,
                Ok(Some(n)) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }
    });

    let _ = tokio::join!(ctrl_writer, video, input_writer);
    conn.close(0u32.into(), b"bye");
    endpoint.wait_idle().await;
    Ok(())
}

async fn read_next(
    recv: &mut quinn::RecvStream,
    inbuf: &mut Vec<u8>,
    chunk: &mut [u8],
) -> Result<Option<Message>> {
    loop {
        {
            let mut cur = inbuf.as_slice();
            match Message::decode(&mut cur) {
                Ok(Some(msg)) => {
                    let consumed = inbuf.len() - cur.len();
                    inbuf.drain(..consumed);
                    return Ok(Some(msg));
                }
                Ok(None) => {}
                Err(e) => return Err(anyhow!("decode: {:?}", e)),
            }
        }
        match recv.read(chunk).await {
            Ok(None) => return if inbuf.is_empty() { Ok(None) } else { Err(anyhow!("mid-frame EOF")) },
            Ok(Some(n)) => inbuf.extend_from_slice(&chunk[..n]),
            Err(quinn::ReadError::ConnectionLost(_)) if inbuf.is_empty() => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
}

fn make_client_config(cert_pin_sha256: Option<Vec<u8>>) -> Result<ClientConfig> {
    let verifier: Arc<dyn ServerCertVerifier> = match cert_pin_sha256 {
        Some(pin) => Arc::new(PinnedCertVerifier { sha256: pin }),
        None => Arc::new(InsecureVerifier),
    };
    let mut rustls_cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    rustls_cfg.alpn_protocols = vec![ALPN.to_vec()];
    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_cfg)?;
    let mut cfg = ClientConfig::new(Arc::new(quic_crypto));
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    cfg.transport_config(Arc::new(t));
    Ok(cfg)
}

#[derive(Debug)]
struct PinnedCertVerifier { sha256: Vec<u8> }
impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self, end_entity: &CertificateDer<'_>, _: &[CertificateDer<'_>],
        _: &ServerName<'_>, _: &[u8], _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(end_entity.as_ref());
        if digest.as_slice() == self.sha256.as_slice() { Ok(ServerCertVerified::assertion()) }
        else { Err(rustls::Error::General(format!("pin mismatch: expected {}, got {}", hex::encode(&self.sha256), hex::encode(digest)))) }
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error> { Ok(HandshakeSignatureValid::assertion()) }
    fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error> { Ok(HandshakeSignatureValid::assertion()) }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::RSA_PSS_SHA256, SignatureScheme::ECDSA_NISTP256_SHA256, SignatureScheme::ED25519]
    }
}

#[derive(Debug)]
struct InsecureVerifier;
impl ServerCertVerifier for InsecureVerifier {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>],
        _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error>
    { Ok(ServerCertVerified::assertion()) }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error> { Ok(HandshakeSignatureValid::assertion()) }
    fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error> { Ok(HandshakeSignatureValid::assertion()) }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::RSA_PSS_SHA256, SignatureScheme::ECDSA_NISTP256_SHA256, SignatureScheme::ED25519]
    }
}
