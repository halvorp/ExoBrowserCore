//! gutted-client: Phase 1 skeleton subscriber.
//!
//! Today: dial gutted-host over QUIC, complete handshake, open a bidi stream,
//! send a hello, read the echo back. Proves the transport works both ways.
//!
//! Cert trust: for now we pin by SHA-256 fingerprint passed via
//! GBROWSER_CERT_SHA256 (hex). No CA. Later: cert baked into image, or SPKI pin.

mod render;

use anyhow::{anyhow, Context, Result};
use gutted_proto::{caps, Message, PROTO_VERSION};
use quinn::{ClientConfig, Endpoint};
use render::{GfxFrame, GfxSubframe, RenderEvent};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tracing::info;

const ALPN: &[u8] = b"gbrowser/1";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,quinn=warn".into()),
        )
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    // Networking runs on a background thread with its own tokio runtime.
    // Winit owns the main thread (required on macOS/Wayland).
    let (frames_tx, frames_rx) = std::sync::mpsc::channel::<RenderEvent>();
    let (input_tx, input_rx)   = std::sync::mpsc::channel::<render::InputEvent>();

    // Test hook: feed canned input events so we can verify the round-trip
    // headlessly. Fires 3 events at 1/2/3 s after start.
    if std::env::var("GBROWSER_SYNTH_INPUT").is_ok() {
        let itx = input_tx.clone();
        std::thread::spawn(move || {
            let seq: Vec<(u64, render::InputEvent)> = vec![
                (1000, render::InputEvent::Motion { x: 400, y: 300, mods: 0 }),
                (1500, render::InputEvent::Scroll { dx: 0, dy: 3 }),
                (2000, render::InputEvent::Button { x: 400, y: 300, button: 1, pressed: true,  mods: 1 << 20 }),
                (3000, render::InputEvent::Button { x: 400, y: 300, button: 1, pressed: false, mods: 0 }),
            ];
            for (ms, ev) in seq {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                let _ = itx.send(ev);
            }
        });
    }
    // Standalone bookmark-key synth for headless testing of F1..F9.
    if let Ok(idx_s) = std::env::var("GBROWSER_SYNTH_BOOKMARK") {
        if let Ok(i) = idx_s.parse::<usize>() {
            let itx = input_tx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if let Some((_, url)) = render::BOOKMARKS.get(i) {
                    let _ = itx.send(render::InputEvent::Navigate((*url).into()));
                }
            });
        }
    }

    let net_thread = std::thread::Builder::new()
        .name("gutted-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all().build().expect("tokio rt");
            if let Err(e) = rt.block_on(net_main(frames_tx.clone(), input_rx)) {
                tracing::error!(error = %e, "net thread ended with error");
                let _ = frames_tx.send(RenderEvent::Quit);
            }
        })?;

    render::run(frames_rx, input_tx)?;
    let _ = net_thread.join();
    Ok(())
}

async fn net_main(
    frames_tx: std::sync::mpsc::Sender<RenderEvent>,
    input_rx: std::sync::mpsc::Receiver<render::InputEvent>,
) -> Result<()> {

    let server: SocketAddr = std::env::var("GBROWSER_SERVER")
        .unwrap_or_else(|_| "127.0.0.1:4433".into())
        .parse()
        .context("parse server addr")?;

    let expected_pin = std::env::var("GBROWSER_CERT_SHA256").ok().map(|s| {
        hex::decode(s.trim()).expect("GBROWSER_CERT_SHA256 must be hex")
    });

    let client_cfg = make_client_config(expected_pin)?;

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_cfg);

    info!(%server, "dialing gutted-host");
    let conn = endpoint
        .connect(server, "localhost")?
        .await
        .context("QUIC connect")?;
    info!(
        rtt = ?conn.rtt(),
        alpn = %String::from_utf8_lossy(&conn.handshake_data().unwrap().downcast::<quinn::crypto::rustls::HandshakeData>().unwrap().protocol.unwrap_or_default()),
        "connected"
    );

    let (mut send, mut recv) = conn.open_bi().await.context("open ctrl bi stream")?;

    // ── HELLO ────────────────────────────────────────────────────────────
    let hello = Message::Hello {
        proto_version: PROTO_VERSION,
        viewport_w: 1280, viewport_h: 720,
        dpr_hundredths: 100,
        client_name: "gutted-client/debian-arm64".into(),
        capabilities: caps::H264 | caps::CLIENT_SCROLL | caps::DMABUF_IMPORT,
    };
    let mut out = Vec::with_capacity(64);
    hello.encode(&mut out);
    let t_send = Instant::now();
    send.write_all(&out).await?;
    info!(bytes = out.len(), "HELLO sent");

    // ── WELCOME ──────────────────────────────────────────────────────────
    let mut inbuf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; 8192];
    let welcome = read_next(&mut recv, &mut inbuf, &mut chunk).await?
        .ok_or_else(|| anyhow!("ctrl stream closed before WELCOME"))?;
    let dt_hello_to_welcome = t_send.elapsed();
    let Message::Welcome { proto_version, session_id, features, cursor_track_id, current_url } = welcome
        else { return Err(anyhow!("expected WELCOME, got {:?}", welcome)); };
    info!(
        proto = proto_version, session = format!("0x{session_id:016x}"),
        features = format!("0x{features:08x}"), cursor_track_id,
        %current_url,
        dt_us = dt_hello_to_welcome.as_micros() as u64,
        "WELCOME received",
    );
    if !current_url.is_empty() {
        let _ = frames_tx.send(RenderEvent::InitialUrl(current_url));
    }

    // ── Send a couple of heartbeats to prove full-duplex framing works ───
    for i in 0..3 {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as u64;
        let t = Instant::now();
        let mut buf = Vec::new();
        Message::Heartbeat { ts_us: ts }.encode(&mut buf);
        send.write_all(&buf).await?;
        let echoed = read_next(&mut recv, &mut inbuf, &mut chunk).await?
            .ok_or_else(|| anyhow!("no heartbeat echo"))?;
        let rtt = t.elapsed();
        match echoed {
            Message::Heartbeat { ts_us } if ts_us == ts => {
                info!(i, rtt_us = rtt.as_micros() as u64, "heartbeat RTT");
            }
            other => return Err(anyhow!("bad heartbeat echo: {:?}", other)),
        }
    }

    info!(quinn_rtt = ?conn.rtt(), "protocol end-to-end verified");

    // Ctrl-send task: any part of the client can push a Message into
    // ctrl_tx and it lands on the bidi ctrl stream, ordered.
    let (ctrl_tx, mut ctrl_rx) = tokio::sync::mpsc::channel::<Message>(32);
    let ctrl_send_task = tokio::spawn(async move {
        while let Some(msg) = ctrl_rx.recv().await {
            let mut buf = Vec::with_capacity(64);
            msg.encode(&mut buf);
            if send.write_all(&buf).await.is_err() { break; }
        }
    });

    // Client-initiated navigation. GBROWSER_NAV=url is the headless-test
    // stand-in for the future address bar; F1..F9 bookmarks handled below.
    if let Ok(url) = std::env::var("GBROWSER_NAV") {
        info!(%url, "NAV sent (from env)");
        ctrl_tx.send(Message::Nav { url }).await?;
    }

    // Accept the video uni stream and hand frames to the renderer.
    let video_task = {
        let conn = conn.clone();
        let frames_tx = frames_tx.clone();
        tokio::spawn(async move {
            let mut vs = match conn.accept_uni().await {
                Ok(s) => s,
                Err(e) => { tracing::warn!(error = %e, "no video stream"); return; }
            };
            tracing::info!("video uni-stream accepted");
            let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
            let mut chunk = vec![0u8; 64 * 1024];
            let mut received = 0u64;
            let mut total_bytes: u64 = 0;
            // Wire-byte accounting for per-second bandwidth reports.
            // wire_bytes = bytes off the QUIC socket (compressed); total_bytes
            // above is decompressed pixel volume. Both interesting.
            let mut wire_bytes: u64 = 0;
            let mut last_report = std::time::Instant::now();
            let mut last_wire: u64 = 0;
            let mut last_frames: u64 = 0;
            loop {
                {
                    let mut cur = buf.as_slice();
                    while let Ok(Some(msg)) = Message::decode(&mut cur) {
                        received += 1;
                        if let Message::RawFrame { ts_us, width, height, stride, format, compression: _, pixels } = msg {
                            let now_us = SystemTime::now().duration_since(UNIX_EPOCH)
                                .unwrap_or_default().as_micros() as u64;
                            let one_way_us = now_us.saturating_sub(ts_us);
                            total_bytes += pixels.len() as u64;
                            if received <= 3 || received % 30 == 0 {
                                tracing::info!(
                                    frame = received,
                                    size = format!("{width}x{height}"),
                                    stride, fmt = format!("0x{format:08x}"),
                                    bytes = pixels.len(),
                                    one_way_us,
                                    "RAW_FRAME received",
                                );
                            }
                            let _ = frames_tx.send(RenderEvent::Frame(GfxFrame {
                                width: width as u32, height: height as u32,
                                stride, pixels,
                            }));
                        } else if let Message::Subframe { ts_us, x, y, w, h, stride, format: _, compression: _, pixels } = msg {
                            let now_us = SystemTime::now().duration_since(UNIX_EPOCH)
                                .unwrap_or_default().as_micros() as u64;
                            let one_way_us = now_us.saturating_sub(ts_us);
                            total_bytes += pixels.len() as u64;
                            if received <= 5 || received % 30 == 0 {
                                tracing::info!(
                                    frame = received,
                                    at = format!("({x},{y})"),
                                    size = format!("{w}x{h}"),
                                    bytes = pixels.len(),
                                    one_way_us,
                                    "SUBFRAME received",
                                );
                            }
                            let _ = frames_tx.send(RenderEvent::Subframe(GfxSubframe {
                                x: x as u32, y: y as u32,
                                w: w as u32, h: h as u32,
                                stride, pixels,
                            }));
                        } else if let Message::CursorState { shape, .. } = msg {
                            tracing::info!(?shape, "cursor shape from server");
                            let _ = frames_tx.send(RenderEvent::CursorShape(shape as u8));
                        } else if let Message::LoadState { state } = msg {
                            tracing::info!(state, "load state from server");
                            let _ = frames_tx.send(RenderEvent::LoadState(state));
                        } else if let Message::Title { title } = msg {
                            tracing::info!(%title, "title from server");
                            let _ = frames_tx.send(RenderEvent::Title(title));
                        } else if let Message::UrlChanged { url } = msg {
                            tracing::info!(%url, "url changed from server");
                            let _ = frames_tx.send(RenderEvent::UrlChanged(url));
                        }
                    }
                    let consumed = buf.len() - cur.len();
                    buf.drain(..consumed);
                }
                // Race the read against a 1-second report timer so we get
                // periodic bandwidth logs even during quiet stretches.
                let mut deadline = tokio::time::interval(std::time::Duration::from_secs(1));
                deadline.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                deadline.tick().await; // first tick fires immediately; discard
                tokio::select! {
                    r = vs.read(&mut chunk) => match r {
                        Ok(None) => break,
                        Ok(Some(n)) => {
                            buf.extend_from_slice(&chunk[..n]);
                            wire_bytes += n as u64;
                        }
                        Err(_) => break,
                    },
                    _ = deadline.tick() => {
                        // Timer fired — fall through to the periodic report below.
                    }
                }
                let elapsed_ms = last_report.elapsed().as_millis() as u64;
                if elapsed_ms >= 1000 {
                    let dwire   = wire_bytes.saturating_sub(last_wire);
                    let dframes = received.saturating_sub(last_frames);
                    if dwire > 0 || dframes > 0 {
                        tracing::info!(
                            wire_bps  = (dwire * 1000 / elapsed_ms.max(1)),
                            frames_per_sec = (dframes * 1000 / elapsed_ms.max(1)),
                            wire_total = wire_bytes,
                            "bandwidth",
                        );
                    }
                    last_wire = wire_bytes;
                    last_frames = received;
                    last_report = std::time::Instant::now();
                }
            }
            tracing::info!(
                frames = received, total_bytes, wire_bytes,
                "video stream ended",
            );
        })
    };

    // Input publisher: pull events from the winit thread's std::mpsc and
    // route each event: InputEvent::Navigate → ctrl (via ctrl_tx), everything
    // else → input uni stream.
    let input_task = {
        let conn = conn.clone();
        let ctrl_tx = ctrl_tx.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let rt_handle = tokio::runtime::Handle::current();
            let mut send: Option<quinn::SendStream> = None;
            let rt_open = rt_handle.clone();
            let open_stream = || -> Result<quinn::SendStream> {
                let conn = conn.clone();
                let s = rt_open.block_on(async move { conn.open_uni().await })?;
                Ok(s)
            };
            while let Ok(ev) = input_rx.recv() {
                // Client-side navigation and viewport resize go on ctrl,
                // not the input stream (both are session-level control).
                if let render::InputEvent::Navigate(url) = ev {
                    tracing::info!(%url, "NAV sent (bookmark key)");
                    let _ = rt_handle.block_on(ctrl_tx.send(Message::Nav { url }));
                    continue;
                }
                if let render::InputEvent::Resize { w, h } = ev {
                    tracing::info!(w, h, "RESIZE sent");
                    let _ = rt_handle.block_on(ctrl_tx.send(Message::Resize {
                        viewport_w: w, viewport_h: h, dpr_hundredths: 100,
                    }));
                    continue;
                }
                if let render::InputEvent::SetZoom { level_milli } = ev {
                    tracing::info!(level_milli, "SET_ZOOM sent");
                    let _ = rt_handle.block_on(ctrl_tx.send(Message::SetZoom { level_milli }));
                    continue;
                }
                if let render::InputEvent::NavAction { action } = ev {
                    tracing::info!(action, "NAV_ACTION sent");
                    let _ = rt_handle.block_on(ctrl_tx.send(Message::NavAction { action }));
                    continue;
                }
                let ts_us = SystemTime::now().duration_since(UNIX_EPOCH)
                    .unwrap_or_default().as_micros() as u64;
                let msg = match ev {
                    render::InputEvent::Motion { x, y, mods } =>
                        Message::InputPointer { ts_us, x, y, modifiers: mods },
                    render::InputEvent::Button { x, y, button, pressed, mods } =>
                        Message::InputButton { ts_us, x, y, button, pressed, modifiers: mods },
                    render::InputEvent::Key { keysym, mods, pressed } =>
                        Message::InputKey { ts_us, keycode: keysym, mods, down: pressed },
                    render::InputEvent::Scroll { dx, dy } =>
                        Message::InputScroll {
                            ts_us, layer_id: 0,
                            dx_units: dx, dy_units: dy,
                            phase: gutted_proto::ScrollPhase::Update,
                        },
                    render::InputEvent::Navigate(_)
                    | render::InputEvent::Resize { .. }
                    | render::InputEvent::SetZoom { .. }
                    | render::InputEvent::NavAction { .. } => unreachable!(),
                };
                let mut buf = Vec::with_capacity(48);
                msg.encode(&mut buf);
                if send.is_none() {
                    match open_stream() {
                        Ok(s) => { tracing::info!("input uni-stream opened"); send = Some(s); }
                        Err(e) => { tracing::warn!(error = %e, "open input stream"); return Err(e); }
                    }
                }
                let s = send.as_mut().unwrap();
                if let Err(e) = rt_handle.block_on(s.write_all(&buf)) {
                    tracing::warn!(error = %e, "input write"); break;
                }
            }
            Ok(())
        })
    };

    // In wgpu mode: keep the connection alive; the window owns lifetime.
    // If GBROWSER_HOLD_SECS is set (headless test), close after N seconds.
    if let Some(hold) = std::env::var("GBROWSER_HOLD_SECS").ok().and_then(|s| s.parse::<u64>().ok()) {
        tokio::time::sleep(std::time::Duration::from_secs(hold)).await;
        conn.close(0u32.into(), b"bye");
    } else {
        video_task.await.ok();
    }
    input_task.abort();
    ctrl_send_task.abort();
    endpoint.wait_idle().await;
    let _ = frames_tx.send(RenderEvent::Quit);
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
                Err(e) => return Err(anyhow!("decode error: {:?}", e)),
            }
        }
        match recv.read(chunk).await? {
            None => return if inbuf.is_empty() { Ok(None) }
                     else { Err(anyhow!("stream ended mid-frame")) },
            Some(n) => inbuf.extend_from_slice(&chunk[..n]),
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
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(end_entity.as_ref());
        if digest.as_slice() == self.sha256.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "cert pin mismatch: expected {}, got {}",
                hex::encode(&self.sha256),
                hex::encode(digest)
            )))
        }
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error>
    { Ok(HandshakeSignatureValid::assertion()) }
    fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error>
    { Ok(HandshakeSignatureValid::assertion()) }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PSS_SHA256, SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ED25519,
        ]
    }
}

#[derive(Debug)]
struct InsecureVerifier;
impl ServerCertVerifier for InsecureVerifier {
    fn verify_server_cert(
        &self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>],
        _: &ServerName<'_>, _: &[u8], _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> { Ok(ServerCertVerified::assertion()) }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error>
    { Ok(HandshakeSignatureValid::assertion()) }
    fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error>
    { Ok(HandshakeSignatureValid::assertion()) }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PSS_SHA256, SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ED25519,
        ]
    }
}
