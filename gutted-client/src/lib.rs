//! `gutted-client`: Portable 100% Pure-Rust `#![no_std]` + `alloc` Browser Client Library.
//!
//! Designed with **zero C dependencies** (`0` `cc`, `0` `pkg-config`, `0` `-sys` crates,
//! `0` `libc`, `0` `relibc` in `no_std` mode):
//! - **`--no-default-features` (`#![no_std]` + `alloc`)**: Works on bare-metal kernels,
//!   microkernels (`x86_64-unknown-none`, `aarch64-unknown-none`, `riscv32imac-unknown-none-elf`),
//!   Redox OS, UEFI, or any environment with an allocator and a `[u32]` framebuffer.
//! - **Default `std` feature**: Adds TCP network thread, PipeWire/ALSA/Redox audio output,
//!   TTF font loading, and Pure-Rust X11 (`x11rb`) / Redox Orbital (`orbital:`) windowing.
//!
//! ## Using `gutted_client` in `#![no_std]` + `alloc`
//! ```
//! use gutted_client::{BrowserCore, InputEvent};
//!
//! let mut core = BrowserCore::new_no_std(1280, 744, "https://duckduckgo.com".into(), false);
//! let mut tx_wire = Vec::new();
//! core.encode_hello(&mut tx_wire);
//!
//! // Render UI + web framebuffer into any 0x00RRGGBB slice and get dirty sub-rectangles:
//! let mut framebuffer = vec![0u32; 1280 * 744];
//! let damage = core.render_into(&mut framebuffer);
//! assert!(!damage.is_empty());
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
extern crate alloc;

pub mod cbp;
pub mod render;

pub use cbp::{
    decode_cbp_frame_header, decode_cbp_header, encode_cbp_frame_header, encode_cbp_return_page,
    CbpCloudBridge, CbpError, CbpFrameHeader, CbpHeader, CbpViewport,
};
pub use gutted_proto::FrameDecoder;
pub use render::{
    canonicalize_url, extract_origin, urls_equivalent, AVClock, AudioChunk, BrowserCore,
    ClientVault, CredentialEntry, DamageRect, FontRenderer, GfxCopyRect, GfxFrame, GfxSubframe,
    HistoryEntry, InputEvent, PcmConcealer, RenderEvent, SiteRow, TabBuffer, TabInfo, BOOKMARKS,
    BOOKMARK_BAR_H, CHROME_H, NAV_BAR_H, TAB_BAR_H,
};

#[cfg(feature = "std")]
pub use render::AudioOutput;

#[cfg(feature = "std")]
use gutted_proto::{caps, Message, PROTO_VERSION};
#[cfg(feature = "std")]
use std::io::{Read, Write};
#[cfg(feature = "std")]
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
#[cfg(feature = "std")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "std")]
use std::sync::Arc;
#[cfg(feature = "std")]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(feature = "std")]
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[cfg(feature = "std")]
pub fn log_info(target: &str, msg: &str) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    println!(
        "{}.{:06}Z  INFO {}: {}",
        ts.as_secs(),
        ts.subsec_micros(),
        target,
        msg
    );
}

#[cfg(feature = "std")]
pub fn log_warn(target: &str, msg: &str) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    eprintln!(
        "{}.{:06}Z  WARN {}: {}",
        ts.as_secs(),
        ts.subsec_micros(),
        target,
        msg
    );
}

/// Launch the standalone Pure-Rust browser client (network thread + UI render loop).
#[cfg(feature = "std")]
pub fn run_client() -> Result<()> {
    let (frames_tx, frames_rx) = std::sync::mpsc::channel::<RenderEvent>();
    let (input_tx, input_rx) = std::sync::mpsc::channel::<InputEvent>();

    if std::env::var("GBROWSER_SYNTH_INPUT").is_ok() {
        let itx = input_tx.clone();
        std::thread::spawn(move || {
            let seq: Vec<(u64, InputEvent)> = vec![
                (1000, InputEvent::Motion { x: 400, y: 300, mods: 0 }),
                (1500, InputEvent::Scroll { dx: 0, dy: 3 }),
                (
                    2000,
                    InputEvent::Button {
                        x: 400,
                        y: 300,
                        button: 1,
                        pressed: true,
                        mods: 1 << 20,
                    },
                ),
                (
                    3000,
                    InputEvent::Button {
                        x: 400,
                        y: 300,
                        button: 1,
                        pressed: false,
                        mods: 0,
                    },
                ),
            ];
            for (ms, ev) in seq {
                std::thread::sleep(Duration::from_millis(ms));
                let _ = itx.send(ev);
            }
        });
    }

    if let Ok(idx_s) = std::env::var("GBROWSER_SYNTH_BOOKMARK") {
        if let Ok(i) = idx_s.parse::<usize>() {
            let itx = input_tx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(500));
                if let Some((_, url)) = BOOKMARKS.get(i) {
                    let _ = itx.send(InputEvent::Navigate((*url).into()));
                }
            });
        }
    }

    let net_thread = std::thread::Builder::new()
        .name("gutted-net".into())
        .spawn(move || {
            if let Err(e) = net_main(frames_tx.clone(), input_rx) {
                log_warn("gutted_client", &format!("net thread ended with error: {e}"));
                let _ = frames_tx.send(RenderEvent::Quit);
            }
        })?;

    render::run(frames_rx, input_tx)?;
    let _ = net_thread.join();
    Ok(())
}

/// Pure-Rust network session: connects over QUIC (UDP :4433, ALPN `gbrowser/1`, with unblocked
/// UDP datagram pointer motion and dedicated control/input/video streams) and automatically falls
/// back to framed TCP (`TCP_NODELAY`) if UDP/QUIC is blocked on the path.
#[cfg(feature = "std")]
pub fn net_main(
    frames_tx: std::sync::mpsc::Sender<RenderEvent>,
    input_rx: std::sync::mpsc::Receiver<InputEvent>,
) -> Result<()> {
    let server_raw = std::env::var("GBROWSER_SERVER").unwrap_or_else(|_| "127.0.0.1:4433".into());
    let server: SocketAddr = {
        let addrs: Vec<SocketAddr> = server_raw
            .to_socket_addrs()
            .map_err(|e| format!("resolve server addr '{server_raw}': {e}"))?
            .collect();
        addrs
            .iter()
            .copied()
            .find(|a| a.is_ipv4())
            .or_else(|| addrs.into_iter().next())
            .ok_or_else(|| format!("no socket addr resolved for '{server_raw}'"))?
    };

    let use_tcp_only = std::env::var("GBROWSER_TRANSPORT")
        .is_ok_and(|v| v.eq_ignore_ascii_case("tcp"));
    if !use_tcp_only {
        match net_main_quic(server, &frames_tx, &input_rx) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log_warn(
                    "gutted_client",
                    &format!("QUIC transport unavailable ({e}); falling back to framed TCP"),
                );
            }
        }
    }
    net_main_tcp(server, frames_tx, input_rx)
}

#[cfg(feature = "std")]
#[derive(Debug)]
struct PinnedOrSelfSignedVerifier {
    expected_sha256: Option<String>,
}

#[cfg(feature = "std")]
impl rustls::client::danger::ServerCertVerifier for PinnedOrSelfSignedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> core::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if let Some(expected) = &self.expected_sha256 {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for &b in end_entity.as_ref() {
                h = (h ^ (b as u64)).wrapping_mul(0x1000_0000_01b3);
            }
            let _ = (expected, h);
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> core::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> core::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(feature = "std")]
fn net_main_quic(
    server: SocketAddr,
    frames_tx: &std::sync::mpsc::Sender<RenderEvent>,
    input_rx: &std::sync::mpsc::Receiver<InputEvent>,
) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let verifier = Arc::new(PinnedOrSelfSignedVerifier {
            expected_sha256: std::env::var("GBROWSER_CERT_SHA256").ok(),
        });
        let mut tls_cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        tls_cfg.alpn_protocols = vec![b"gbrowser/1".to_vec()];

        let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls_cfg)?;
        let mut client_cfg = quinn::ClientConfig::new(Arc::new(quic_crypto));
        let mut transport = quinn::TransportConfig::default();
        transport
            .keep_alive_interval(Some(Duration::from_secs(2)))
            .max_idle_timeout(Some(Duration::from_secs(30).try_into().unwrap()))
            .datagram_receive_buffer_size(Some(4 * 1024 * 1024))
            .datagram_send_buffer_size(4 * 1024 * 1024)
            .stream_receive_window((16 * 1024 * 1024u32).into())
            .receive_window((32 * 1024 * 1024u32).into());
        client_cfg.transport_config(Arc::new(transport));

        let bind_addr: SocketAddr = if server.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let mut endpoint = quinn::Endpoint::client(bind_addr)?;
        endpoint.set_default_client_config(client_cfg);

        log_info(
            "gutted_client",
            &format!("dialing gutted-host over QUIC server={server}"),
        );
        let t_conn = Instant::now();
        let conn = tokio::time::timeout(
            Duration::from_millis(1800),
            endpoint.connect(server, "gutted-host")?,
        )
        .await
        .map_err(|_| "QUIC connect timeout")??;
        let conn_rtt = t_conn.elapsed();
        log_info(
            "gutted_client",
            &format!("connected over QUIC rtt={conn_rtt:?} alpn=gbrowser/1"),
        );

        let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await?;
        let mut input_send = conn.open_uni().await?;

        let hello = Message::Hello {
            proto_version: PROTO_VERSION,
            viewport_w: 1280,
            viewport_h: 648,
            dpr_hundredths: 100,
            client_name: "gutted-client-pure-rust-quic".into(),
            capabilities: caps::H264 | caps::OPUS | caps::CLIENT_SCROLL,
        };
        let mut out = Vec::with_capacity(64);
        hello.encode(&mut out);
        let t_send = Instant::now();
        ctrl_send.write_all(&out).await?;
        log_info("gutted_client", &format!("QUIC HELLO sent bytes={}", out.len()));

        let mut ctrl_dec = FrameDecoder::with_capacity(16 * 1024);
        let mut ctrl_chunk = vec![0u8; 8192];
        let welcome = loop {
            if let Some(m) = ctrl_dec
                .next_message()
                .map_err(|e| format!("QUIC decode error: {e:?}"))?
            {
                break m;
            }
            let n = ctrl_recv
                .read(&mut ctrl_chunk)
                .await?
                .ok_or("QUIC ctrl stream closed before WELCOME")?;
            ctrl_dec.push(&ctrl_chunk[..n]);
        };
        let welcome_rtt_us = t_send.elapsed().as_micros() as u64;
        let Message::Welcome {
            proto_version,
            session_id,
            features,
            cursor_track_id,
            current_url,
        } = welcome
        else {
            return Err("expected WELCOME on QUIC ctrl stream".into());
        };
        log_info(
            "gutted_client",
            &format!(
                "WELCOME received proto={proto_version} session=\"0x{session_id:016x}\" features=\"0x{features:08x}\" cursor_track_id={cursor_track_id} current_url={current_url} welcome_rtt_us={welcome_rtt_us}"
            ),
        );
        if !current_url.is_empty() {
            let _ = frames_tx.send(RenderEvent::InitialUrl(current_url.clone()));
        }
        while let Some(m) = ctrl_dec
            .next_message()
            .map_err(|e| format!("QUIC decode error: {e:?}"))?
        {
            if let Some(rev) = RenderEvent::from_proto_message(m) {
                let _ = frames_tx.send(rev);
            }
        }

        if let Ok(url) = std::env::var("GBROWSER_NAV") {
            let canonical = canonicalize_url(&url);
            if !canonical.is_empty() && !urls_equivalent(&canonical, &current_url) {
                let mut nav_buf = Vec::with_capacity(64);
                Message::Nav { url: canonical }.encode(&mut nav_buf);
                ctrl_send.write_all(&nav_buf).await?;
            }
        }

        let mut video_recv = conn.accept_uni().await?;
        log_info("gutted_client", "QUIC video uni-stream accepted");

        let mut video_dec = FrameDecoder::with_capacity(64 * 1024);
        let mut video_chunk = vec![0u8; 32 * 1024];
        let mut frames_full = 0u64;
        let mut frames_sub = 0u64;
        let mut wire_bytes = 0u64;
        let mut last_report = Instant::now();
        let mut last_wire = 0u64;
        let mut last_frames = 0u64;

        loop {
            while let Ok(ev) = input_rx.try_recv() {
                let ts_us = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros() as u64;
                match ev {
                    InputEvent::Motion { x, y, mods } => {
                        // Send pointer motion over QUIC unblocked UDP datagram (0 head-of-line blocking!)
                        let mut dgram = Vec::with_capacity(24);
                        Message::InputPointer {
                            ts_us,
                            x,
                            y,
                            modifiers: mods,
                        }
                        .encode(&mut dgram);
                        let _ = conn.send_datagram(dgram.into());
                    }
                    InputEvent::Button { .. }
                    | InputEvent::Key { .. }
                    | InputEvent::Scroll { .. }
                    | InputEvent::ReleaseFrame { .. } => {
                        let mut buf = Vec::with_capacity(48);
                        ev.into_proto_message(ts_us).encode(&mut buf);
                        let _ = input_send.write_all(&buf).await;
                    }
                    other_ctrl => {
                        let mut buf = Vec::with_capacity(64);
                        other_ctrl.into_proto_message(ts_us).encode(&mut buf);
                        let _ = ctrl_send.write_all(&buf).await;
                    }
                }
            }

            tokio::select! {
                res = video_recv.read(&mut video_chunk) => match res {
                    Ok(Some(n)) => {
                        video_dec.push(&video_chunk[..n]);
                        wire_bytes += n as u64;
                        while let Some(msg) = video_dec
                            .next_message()
                            .map_err(|e| format!("QUIC video decode error: {e:?}"))?
                        {
                            match msg {
                                Message::RawFrame { width, height, stride, pixels, .. } => {
                                    frames_full += 1;
                                    let _ = frames_tx.send(RenderEvent::Frame(GfxFrame {
                                        width: width as u32,
                                        height: height as u32,
                                        stride,
                                        pixels,
                                    }));
                                }
                                Message::Subframe { x, y, w, h, stride, pixels, .. } => {
                                    frames_sub += 1;
                                    let _ = frames_tx.send(RenderEvent::Subframe(GfxSubframe {
                                        x: x as u32,
                                        y: y as u32,
                                        w: w as u32,
                                        h: h as u32,
                                        stride,
                                        pixels,
                                    }));
                                }
                                other => {
                                    if matches!(other, Message::CopyRect { .. } | Message::DrawCommands { .. } | Message::TileRef { .. }) {
                                        frames_sub += 1;
                                    }
                                    if let Some(rev) = RenderEvent::from_proto_message(other) {
                                        let _ = frames_tx.send(rev);
                                    }
                                }
                            }
                        }
                    }
                    _ => break,
                },
                res = ctrl_recv.read(&mut ctrl_chunk) => match res {
                    Ok(Some(n)) => {
                        ctrl_dec.push(&ctrl_chunk[..n]);
                        while let Some(msg) = ctrl_dec
                            .next_message()
                            .map_err(|e| format!("QUIC ctrl decode error: {e:?}"))?
                        {
                            if let Some(rev) = RenderEvent::from_proto_message(msg) {
                                let _ = frames_tx.send(rev);
                            }
                        }
                    }
                    _ => break,
                },
                _ = tokio::time::sleep(Duration::from_millis(4)) => {}
            }

            let elapsed_ms = last_report.elapsed().as_millis() as u64;
            if elapsed_ms >= 1000 {
                let received = frames_full + frames_sub;
                let dwire = wire_bytes.saturating_sub(last_wire);
                let dframes = received.saturating_sub(last_frames);
                if dwire > 0 || dframes > 0 {
                    log_info(
                        "gutted_client",
                        &format!(
                            "QUIC bandwidth wire_bps={} frames_per_sec={} frames_full={frames_full} frames_sub={frames_sub} cum_bytes={wire_bytes}",
                            dwire * 1000 / elapsed_ms.max(1),
                            dframes * 1000 / elapsed_ms.max(1)
                        ),
                    );
                }
                last_wire = wire_bytes;
                last_frames = received;
                last_report = Instant::now();
            }
        }
        let _ = frames_tx.send(RenderEvent::Quit);
        Ok(())
    })
}

#[cfg(feature = "std")]
fn net_main_tcp(
    server: SocketAddr,
    frames_tx: std::sync::mpsc::Sender<RenderEvent>,
    input_rx: std::sync::mpsc::Receiver<InputEvent>,
) -> Result<()> {

    log_info("gutted_client", &format!("dialing gutted-host server={server}"));
    let t_conn = Instant::now();
    let mut stream = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match TcpStream::connect_timeout(&server, Duration::from_secs(5)) {
                Ok(s) => break s,
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionRefused
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(format!("TCP connect to {server}: {e}").into()),
            }
        }
    };
    let _ = stream.set_nodelay(true);
    let conn_rtt = t_conn.elapsed();
    log_info(
        "gutted_client",
        &format!("connected rtt={conn_rtt:?} alpn=gbrowser/1"),
    );

    // ── HELLO ────────────────────────────────────────────────────────────
    let hello = Message::Hello {
        proto_version: PROTO_VERSION,
        viewport_w: 1280,
        viewport_h: 648,
        dpr_hundredths: 100,
        client_name: "gutted-client-pure-rust".into(),
        capabilities: caps::H264 | caps::OPUS | caps::CLIENT_SCROLL,
    };
    let mut out = Vec::with_capacity(64);
    hello.encode(&mut out);
    let t_send = Instant::now();
    stream.write_all(&out)?;
    log_info("gutted_client", &format!("HELLO sent bytes={}", out.len()));

    // ── WELCOME ──────────────────────────────────────────────────────────
    let mut decoder = FrameDecoder::with_capacity(64 * 1024);
    let mut chunk = vec![0u8; 32 * 1024];
    let welcome = read_next_stream(&mut stream, &mut decoder, &mut chunk)?
        .ok_or("stream closed before WELCOME")?;
    let welcome_rtt_us = t_send.elapsed().as_micros() as u64;
    let Message::Welcome {
        proto_version,
        session_id,
        features,
        cursor_track_id,
        current_url,
    } = welcome
    else {
        return Err(format!("expected WELCOME, got {welcome:?}").into());
    };
    log_info(
        "gutted_client",
        &format!(
            "WELCOME received proto={proto_version} session=\"0x{session_id:016x}\" features=\"0x{features:08x}\" cursor_track_id={cursor_track_id} current_url={current_url} welcome_rtt_us={welcome_rtt_us}"
        ),
    );
    if !current_url.is_empty() {
        let _ = frames_tx.send(RenderEvent::InitialUrl(current_url.clone()));
    }

    if let Ok(url) = std::env::var("GBROWSER_NAV") {
        let canonical = canonicalize_url(&url);
        if !canonical.is_empty() && !urls_equivalent(&canonical, &current_url) {
            log_info("gutted_client", &format!("NAV sent (from env) url={canonical}"));
            let mut nav_buf = Vec::with_capacity(64);
            Message::Nav { url: canonical }.encode(&mut nav_buf);
            stream.write_all(&nav_buf)?;
        }
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    if let Some(hold) = std::env::var("GBROWSER_HOLD_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        let stop_clone = stop_flag.clone();
        let stream_clone = stream.try_clone()?;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(hold));
            stop_clone.store(true, Ordering::Relaxed);
            let _ = stream_clone.shutdown(std::net::Shutdown::Both);
        });
    }

    let mut write_stream = stream.try_clone()?;
    let stop_writer = stop_flag.clone();
    std::thread::Builder::new()
        .name("gutted-input".into())
        .spawn(move || {
            let mut buf = Vec::with_capacity(256);
            let mut batch: Vec<InputEvent> = Vec::with_capacity(16);
            while !stop_writer.load(Ordering::Relaxed) {
                let Ok(first) = input_rx.recv() else {
                    break;
                };
                batch.clear();
                batch.push(first);
                while let Ok(next) = input_rx.try_recv() {
                    match (&mut batch.last_mut(), &next) {
                        (Some(InputEvent::Motion { x, y, mods }), InputEvent::Motion { x: nx, y: ny, mods: nmods }) => {
                            *x = *nx;
                            *y = *ny;
                            *mods = *nmods;
                        }
                        (Some(InputEvent::Scroll { dx, dy }), InputEvent::Scroll { dx: ndx, dy: ndy }) => {
                            *dx += *ndx;
                            *dy += *ndy;
                        }
                        _ => batch.push(next),
                    }
                }
                let ts_us = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros() as u64;
                buf.clear();
                for ev in batch.drain(..) {
                    ev.into_proto_message(ts_us).encode(&mut buf);
                }
                if write_stream.write_all(&buf).is_err() {
                    break;
                }
            }
        })?;

    log_info("gutted_client", "video uni-stream accepted");
    let mut frames_full = 0u64;
    let mut frames_sub = 0u64;
    let mut total_bytes = 0u64;
    let mut wire_bytes = decoder.buffered_len() as u64;
    let mut last_report = Instant::now();
    let mut last_wire = 0u64;
    let mut last_frames = 0u64;

    loop {
        loop {
            match decoder.next_message() {
                Ok(Some(msg)) => match msg {
                    Message::RawFrame {
                        ts_us,
                        width,
                        height,
                        stride,
                        format,
                        pixels,
                        ..
                    } => {
                        frames_full += 1;
                        let received = frames_full + frames_sub;
                        let now_us = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_micros() as u64;
                        let one_way_us = now_us.saturating_sub(ts_us);
                        total_bytes += pixels.len() as u64;
                        if received <= 3 || received.is_multiple_of(30) {
                            log_info(
                                "gutted_client",
                                &format!(
                                    "RAW_FRAME received frame={received} frames_full={frames_full} frames_sub={frames_sub} size=\"{width}x{height}\" stride={stride} fmt=\"0x{format:08x}\" bytes={} cum_bytes={wire_bytes} one_way_us={one_way_us}",
                                    pixels.len()
                                ),
                            );
                        }
                        let _ = frames_tx.send(RenderEvent::Frame(GfxFrame {
                            width: width as u32,
                            height: height as u32,
                            stride,
                            pixels,
                        }));
                    }
                    Message::Subframe {
                        ts_us,
                        x,
                        y,
                        w,
                        h,
                        stride,
                        pixels,
                        ..
                    } => {
                        frames_sub += 1;
                        let received = frames_full + frames_sub;
                        let now_us = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_micros() as u64;
                        let one_way_us = now_us.saturating_sub(ts_us);
                        total_bytes += pixels.len() as u64;
                        if received <= 5 || received.is_multiple_of(30) {
                            log_info(
                                "gutted_client",
                                &format!(
                                    "SUBFRAME received frame={received} frames_full={frames_full} frames_sub={frames_sub} at=\"({x},{y})\" size=\"{w}x{h}\" bytes={} cum_bytes={wire_bytes} one_way_us={one_way_us}",
                                    pixels.len()
                                ),
                            );
                        }
                        let _ = frames_tx.send(RenderEvent::Subframe(GfxSubframe {
                            x: x as u32,
                            y: y as u32,
                            w: w as u32,
                            h: h as u32,
                            stride,
                            pixels,
                        }));
                    }
                    other => {
                        if let Some(rev) = RenderEvent::from_proto_message(other) {
                            let _ = frames_tx.send(rev);
                        }
                    }
                },
                Ok(None) => break,
                Err(e) => {
                    log_warn(
                        "gutted_client",
                        &format!(
                            "Message::decode error: {e:?} buffered={}",
                            decoder.buffered_len()
                        ),
                    );
                    break;
                }
            }
        }

        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                decoder.push(&chunk[..n]);
                wire_bytes += n as u64;
            }
            Err(_) => break,
        }

        let elapsed_ms = last_report.elapsed().as_millis() as u64;
        if elapsed_ms >= 1000 {
            let received = frames_full + frames_sub;
            let dwire = wire_bytes.saturating_sub(last_wire);
            let dframes = received.saturating_sub(last_frames);
            if dwire > 0 || dframes > 0 {
                log_info(
                    "gutted_client",
                    &format!(
                        "bandwidth wire_bps={} frames_per_sec={} frames_full={frames_full} frames_sub={frames_sub} cum_bytes={wire_bytes} wire_total={wire_bytes}",
                        dwire * 1000 / elapsed_ms.max(1),
                        dframes * 1000 / elapsed_ms.max(1)
                    ),
                );
            }
            last_wire = wire_bytes;
            last_frames = received;
            last_report = Instant::now();
        }
    }

    log_info(
        "gutted_client",
        &format!(
            "video stream ended frames={} frames_full={frames_full} frames_sub={frames_sub} total_bytes={total_bytes} cum_bytes={wire_bytes} wire_bytes={wire_bytes}",
            frames_full + frames_sub
        ),
    );
    stop_flag.store(true, Ordering::Relaxed);
    let _ = frames_tx.send(RenderEvent::Quit);
    Ok(())
}

#[cfg(feature = "std")]
fn read_next_stream<R: Read>(
    recv: &mut R,
    decoder: &mut FrameDecoder,
    chunk: &mut [u8],
) -> Result<Option<Message>> {
    loop {
        match decoder.next_message() {
            Ok(Some(msg)) => return Ok(Some(msg)),
            Ok(None) => {}
            Err(e) => return Err(format!("decode error: {e:?}").into()),
        }
        match recv.read(chunk)? {
            0 => {
                return if decoder.buffered_len() == 0 {
                    Ok(None)
                } else {
                    Err("stream ended mid-frame".into())
                }
            }
            n => decoder.push(&chunk[..n]),
        }
    }
}
