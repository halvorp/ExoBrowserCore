//! gutted-host: Phase 1 skeleton.
//!
//! Owns the QUIC listener that clients (microkernel or Debian test subscriber)
//! connect to. Each accepted connection will eventually get:
//!   - one bidi control stream (HELLO/WELCOME, resize, nav)
//!   - one uni input stream (client -> host: pointer/key/touch)
//!   - one uni scene stream (host -> client: layer tree deltas — Phase 3)
//!   - MoQ tracks for per-layer video/tile content
//!
//! Today: accept, log peer, echo bytes on the first bidi stream. Enough to
//! prove the transport works end-to-end.

mod wpe;

use anyhow::{anyhow, Context, Result};
use gutted_proto::{caps, Message, PROTO_VERSION};
use quinn::{Endpoint, ServerConfig};
use std::{net::SocketAddr, sync::Arc};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, Mutex as AsyncMutex};
use tracing::{error, info, warn};

/// Fan-out for WPE frames.
///
/// `full` = most recent whole-frame `RawFrame` — used as the *initial*
/// state we hand a fresh subscriber, so they always have a base to blit
/// subsequent Subframes onto. It's updated ONLY when a `RawFrame`
/// publishes (Subframes/CursorState leave it alone).
///
/// `tx` broadcasts the full ordered stream of published messages
/// (RawFrame + Subframe + CursorState). Subscribers subscribe first,
/// snapshot `full` second, then process the stream — this ordering
/// avoids missing a message between snapshot and subscribe.
#[derive(Clone)]
struct FrameBus {
    full: Arc<AsyncMutex<Option<Arc<Message>>>>,
    tx:   broadcast::Sender<Arc<Message>>,
}
impl FrameBus {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self { full: Arc::new(AsyncMutex::new(None)), tx }
    }
    async fn publish(&self, msg: Arc<Message>) {
        if matches!(&*msg, Message::RawFrame { .. }) {
            *self.full.lock().await = Some(msg.clone());
        }
        let _ = self.tx.send(msg);
    }
    fn subscribe(&self) -> broadcast::Receiver<Arc<Message>> { self.tx.subscribe() }
    async fn snapshot(&self) -> Option<Arc<Message>> { self.full.lock().await.clone() }
}

const ALPN: &[u8] = b"gbrowser/1";
const DEFAULT_ADDR: &str = "0.0.0.0:4433";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,quinn=warn".into()),
        )
        .init();

    // Bring rustls' process-wide crypto provider up before quinn touches it.
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let addr: SocketAddr = std::env::var("GBROWSER_LISTEN")
        .unwrap_or_else(|_| DEFAULT_ADDR.into())
        .parse()
        .context("parse listen addr")?;

    let (server_cfg, cert_der) = make_self_signed_config()?;
    let endpoint = Endpoint::server(server_cfg, addr).context("bind quic endpoint")?;

    info!(?addr, "gutted-host listening (QUIC/1)");
    let pin = hex_sha256(&cert_der);
    println!("GBROWSER_CERT_SHA256={pin}");
    info!(cert_sha256 = %pin, "self-signed cert fingerprint (pin this on the client)");

    let bus = FrameBus::new();
    // Tracks the currently-loaded URL so late-joining clients can see it
    // in WELCOME. Updated on GBROWSER_URL boot and on every Nav request.
    let current_url: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));

    if let Ok(url) = std::env::var("GBROWSER_URL") {
        *current_url.lock().unwrap() = url.clone();
        let bus_for_wpe = bus.clone();
        let current_url_for_wpe = current_url.clone();
        tokio::spawn(async move {
            let (mut runner, mut frames, mut loads, mut cursors, mut titles, mut urls) = wpe::WpeRunner::start(&url, 1280, 720);
            // Publish this runner's handle to the process-current slot so
            // the compat `wpe::load_uri/resize/inject_*` calls in the ctrl
            // handlers keep working. Small poll — on_ready fires quickly.
            for _ in 0..200u32 {
                runner.install_as_current();
                if runner.h().is_some() { break; }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let mut n = 0u64;
            let mut last: Option<wpe::Frame> = None;
            loop {
                tokio::select! {
                    Some(f) = frames.recv() => {
                        n += 1;
                        let ts_us = SystemTime::now().duration_since(UNIX_EPOCH)
                            .unwrap_or_default().as_micros() as u64;
                        let same_geom = last.as_ref().map_or(false, |l|
                            l.width == f.width && l.height == f.height && l.stride == f.stride);
                        let msg: Option<Arc<Message>> = if !same_geom {
                            // First frame, or dims changed → full frame.
                            Some(Arc::new(Message::RawFrame {
                                ts_us,
                                width:  f.width  as u16, height: f.height as u16,
                                stride: f.stride as u32, format: f.format,
                                compression: 1, pixels: f.pixels.clone(),
                            }))
                        } else {
                            let prev = last.as_ref().unwrap();
                            let regions = diff_regions(&prev.pixels, &f.pixels, f.width, f.height, f.stride);
                            if regions.is_empty() {
                                None
                            } else {
                                // If a single region covers ≥60% of the pixels, fall
                                // back to a full RawFrame — cheaper to compress once.
                                let total_px: u64 = regions.iter().map(|(_, _, w, h)| (*w as u64) * (*h as u64)).sum();
                                let full_px = (f.width as u64) * (f.height as u64);
                                if regions.len() == 1 && total_px * 5 >= full_px * 3 {
                                    Some(Arc::new(Message::RawFrame {
                                        ts_us,
                                        width:  f.width  as u16, height: f.height as u16,
                                        stride: f.stride as u32, format: f.format,
                                        compression: 1, pixels: f.pixels.clone(),
                                    }))
                                } else {
                                    // Publish each region separately.
                                    for (x, y, w, h) in &regions {
                                        let sub = extract_subrect(&f.pixels, f.stride, *x, *y, *w, *h);
                                        let m = Arc::new(Message::Subframe {
                                            ts_us, x: *x, y: *y, w: *w, h: *h,
                                            stride: (*w as u32) * 4, format: f.format,
                                            compression: 1, pixels: sub,
                                        });
                                        bus_for_wpe.publish(m).await;
                                    }
                                    if n <= 5 || n % 30 == 0 {
                                        info!(frame = n, regions = regions.len(), "WPE frame → bus (SUB×N)");
                                    }
                                    // We already published; signal to outer scope: no more work.
                                    Some(Arc::new(Message::Heartbeat { ts_us: 0 })) // sentinel — filtered below
                                }
                            }
                        };
                        last = Some(f);
                        if let Some(m) = msg {
                            match &*m {
                                Message::Heartbeat { .. } => {} // sentinel, already published
                                Message::RawFrame { .. } => {
                                    bus_for_wpe.publish(m).await;
                                    if n <= 5 || n % 30 == 0 { info!(frame = n, kind = "FULL", "WPE frame → bus"); }
                                }
                                Message::Subframe { .. } => {
                                    // Legacy single-region path unused now; keep as fallback.
                                    bus_for_wpe.publish(m).await;
                                }
                                _ => {}
                            }
                        } else if n <= 5 || n % 30 == 0 {
                            info!(frame = n, kind = "SKIP", "WPE frame identical");
                        }
                    }
                    Some(s) = loads.recv() => {
                        info!(?s, "load state");
                        let state = match s {
                            wpe::LoadState::Started    => 0u8,
                            wpe::LoadState::Redirected => 1u8,
                            wpe::LoadState::Committed  => 2u8,
                            wpe::LoadState::Finished   => 3u8,
                            wpe::LoadState::Unknown    => 255u8,
                        };
                        bus_for_wpe.publish(Arc::new(Message::LoadState { state })).await;
                    }
                    Some(title) = titles.recv() => {
                        info!(%title, "page title");
                        bus_for_wpe.publish(Arc::new(Message::Title { title })).await;
                    }
                    Some(new_url) = urls.recv() => {
                        info!(url = %new_url, "committed uri");
                        if let Ok(mut cu) = current_url_for_wpe.lock() { *cu = new_url.clone(); }
                        bus_for_wpe.publish(Arc::new(Message::UrlChanged { url: new_url })).await;
                    }
                    Some(shape_id) = cursors.recv() => {
                        use gutted_proto::CursorShape;
                        let shape = match shape_id {
                            1 => CursorShape::Pointer,
                            2 => CursorShape::Text,
                            _ => CursorShape::Default,
                        };
                        info!(?shape, "cursor shape");
                        let msg = Arc::new(Message::CursorState {
                            shape, hotspot_x: 0, hotspot_y: 0, image_ref: 0,
                        });
                        bus_for_wpe.publish(msg).await;
                    }
                    else => break,
                }
            }
            let rc = runner.stop();
            info!(rc, "WPE thread exited");
        });
    }

    while let Some(incoming) = endpoint.accept().await {
        let bus = bus.clone();
        let current_url = current_url.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    if let Err(e) = handle_connection(conn, bus, current_url).await {
                        warn!(error = %e, "connection ended with error");
                    }
                }
                Err(e) => error!(error = %e, "handshake failed"),
            }
        });
    }
    Ok(())
}

async fn handle_connection(
    conn: quinn::Connection,
    bus: FrameBus,
    current_url: Arc<std::sync::Mutex<String>>,
) -> Result<()> {
    let peer = conn.remote_address();
    info!(%peer, "client connected");

    loop {
        tokio::select! {
            biresult = conn.accept_bi() => {
                let (send, recv) = biresult?;
                let bus = bus.clone();
                let conn2 = conn.clone();
                let current_url = current_url.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_ctrl_stream(send, recv, conn2, bus, current_url).await {
                        warn!(error = %e, "ctrl stream error");
                    }
                });
            }
            uniresult = conn.accept_uni() => {
                let recv = uniresult?;
                tokio::spawn(async move {
                    if let Err(e) = handle_input_stream(recv).await {
                        warn!(error = %e, "input stream error");
                    }
                });
            }
            _ = conn.closed() => {
                info!(%peer, reason = ?conn.close_reason(), "client disconnected");
                return Ok(());
            }
        }
    }
}

/// Uni stream client→host: input events → wpe::inject_*.
async fn handle_input_stream(mut recv: quinn::RecvStream) -> Result<()> {
    let mut inbuf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; 8192];
    let mut n_events = 0u64;
    // Track last-known pointer position so scroll events can carry a
    // cursor coord (WebKit uses it to hit-test the target scrollable
    // element). Without this, wheel events land at (0,0) and only the
    // top-left element scrolls.
    let mut last_pointer: (i32, i32) = (0, 0);
    info!("input uni-stream open");
    loop {
        loop {
            let mut cur = inbuf.as_slice();
            match Message::decode(&mut cur) {
                Ok(Some(msg)) => {
                    let consumed = inbuf.len() - cur.len();
                    inbuf.drain(..consumed);
                    n_events += 1;
                    match msg {
                        Message::InputPointer { x, y, modifiers, .. } => {
                            last_pointer = (x, y);
                            wpe::inject_pointer_motion(x, y, modifiers);
                        }
                        Message::InputButton { x, y, button, pressed, modifiers, .. } => {
                            last_pointer = (x, y);
                            wpe::inject_pointer_button(x, y, button, pressed, modifiers);
                            info!(x, y, button, pressed, "button injected");
                        }
                        Message::InputKey { keycode, mods, down, .. } => {
                            wpe::inject_key(keycode, mods, down);
                            info!(keycode = format!("0x{keycode:x}"), down, "key injected");
                        }
                        Message::InputScroll { dx_units, dy_units, .. } => {
                            // Wheel units → pixel deltas; browsers convention
                            // ≈ 40px per notch. WPE Y+ = content up = scroll down.
                            let dx = dx_units as f64 * 40.0;
                            let dy = dy_units as f64 * 40.0;
                            let (px, py) = last_pointer;
                            wpe::inject_axis(px, py, dx, dy, 0);
                            info!(px, py, dx, dy, "axis injected");
                        }
                        other => warn!(?other, "unexpected input msg"),
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(anyhow!("input decode: {:?}", e)),
            }
        }
        match recv.read(&mut chunk).await {
            Ok(None) => break,
            Ok(Some(n)) => inbuf.extend_from_slice(&chunk[..n]),
            Err(quinn::ReadError::ConnectionLost(_)) if inbuf.is_empty() => break,
            Err(e) => return Err(e.into()),
        }
    }
    info!(events = n_events, "input stream closed");
    Ok(())
}

/// The bidi ctrl stream. Today: read HELLO, answer with WELCOME, spawn a
/// video uni-stream publisher, then stream framed messages either
/// direction until the client hangs up.
async fn handle_ctrl_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    conn: quinn::Connection,
    bus: FrameBus,
    current_url: Arc<std::sync::Mutex<String>>,
) -> Result<()> {
    let mut inbuf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; 8192];

    // ── Handshake: expect HELLO first ────────────────────────────────────
    let hello = read_next(&mut recv, &mut inbuf, &mut chunk).await?
        .ok_or_else(|| anyhow!("ctrl stream closed before HELLO"))?;
    let Message::Hello { proto_version, viewport_w, viewport_h, dpr_hundredths, client_name, capabilities } = hello
        else { return Err(anyhow!("first ctrl message was not HELLO: {:?}", hello)); };
    if proto_version != PROTO_VERSION {
        return Err(anyhow!("proto version mismatch: client={proto_version} host={PROTO_VERSION}"));
    }
    info!(
        client = %client_name, viewport = format!("{viewport_w}x{viewport_h}"),
        dpr = format!("{:.2}", dpr_hundredths as f32 / 100.0),
        caps = format!("0x{capabilities:08x}"),
        "HELLO received",
    );

    // ── Reply with WELCOME ───────────────────────────────────────────────
    let session_id: u64 = rand64();
    let welcome = Message::Welcome {
        proto_version: PROTO_VERSION,
        session_id,
        features: capabilities & (caps::H264 | caps::CLIENT_SCROLL), // intersection
        cursor_track_id: 0, // no cursor track yet — Phase 2
        current_url: current_url.lock().map(|g| g.clone()).unwrap_or_default(),
    };
    let mut out = Vec::with_capacity(64);
    welcome.encode(&mut out);
    send.write_all(&out).await?;
    info!(session_id = format!("0x{session_id:016x}"), "WELCOME sent");

    // Spawn the video uni-stream publisher.
    let video_conn = conn.clone();
    let bus_for_video = bus.clone();
    // Subscribe FIRST (marker set), then snapshot full — order matters
    // so a publish between these two ops isn't lost.
    let mut stream_rx = bus.subscribe();
    tokio::spawn(async move {
        let mut vs = match video_conn.open_uni().await {
            Ok(s) => s,
            Err(e) => { warn!(error = %e, "open_uni video stream"); return; }
        };
        info!("video uni-stream open");

        async fn write_frame(vs: &mut quinn::SendStream, msg: &Message) -> (Result<()>, usize) {
            let mut buf = match msg {
                Message::RawFrame { pixels, .. } => Vec::with_capacity(32 + pixels.len()),
                Message::Subframe { pixels, .. } => Vec::with_capacity(32 + pixels.len()),
                _ => Vec::with_capacity(64),
            };
            msg.encode(&mut buf);
            let r = vs.write_all(&buf).await.map_err(|e| anyhow!(e));
            (r, buf.len())
        }

        // Initial state: send the last full RawFrame (never a SUBFRAME —
        // that would leave the client with no base to blit onto).
        let mut published = 0u64;
        let mut cum_bytes: u64 = 0;
        let mut cum_full: u64 = 0;
        let mut cum_sub: u64 = 0;
        let snap = bus_for_video.snapshot().await;
        info!(snap_has_full = snap.is_some(), "video task: initial snapshot");
        if let Some(m) = snap {
            let (res, n) = write_frame(&mut vs, &m).await;
            match res {
                Ok(()) => {
                    published += 1;
                    cum_bytes += n as u64; cum_full += n as u64;
                    info!(frame = published, bytes = n, "published RAW_FRAME (snapshot)");
                }
                Err(e) => warn!(error = %e, "snapshot-frame write"),
            }
        }

        loop {
            tokio::select! {
                r = stream_rx.recv() => match r {
                    Ok(m) => {
                        // If we already delivered this exact Arc as snapshot,
                        // skip (avoids duplicate FULL at start).
                        let (res, n) = write_frame(&mut vs, &m).await;
                        if let Err(e) = res { warn!(error = %e, "video write"); break; }
                        published += 1;
                        cum_bytes += n as u64;
                        match &*m {
                            Message::RawFrame { .. } => cum_full += n as u64,
                            Message::Subframe { .. } => cum_sub  += n as u64,
                            _ => {}
                        }
                        if published <= 5 || published % 30 == 0 {
                            let kind = match &*m {
                                Message::Subframe { .. }    => "SUB",
                                Message::RawFrame { .. }    => "FULL",
                                Message::CursorState { .. } => "CUR",
                                _                           => "OTH",
                            };
                            info!(frame = published, kind, bytes = n, cum_bytes, "published");
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(dropped = n, "video subscriber lagged — will fall behind; resync via next FULL");
                    }
                    Err(broadcast::error::RecvError::Closed)    => break,
                },
                _ = video_conn.closed() => break,
            }
        }
        let _ = vs.finish();
        info!(published, cum_bytes, cum_full, cum_sub, "VIDEO STREAM CLOSED (bandwidth report)");
    });

    // ── Post-handshake message loop ──────────────────────────────────────
    while let Some(msg) = read_next(&mut recv, &mut inbuf, &mut chunk).await? {
        match msg {
            Message::Heartbeat { ts_us } => {
                let mut buf = Vec::with_capacity(16);
                Message::Heartbeat { ts_us }.encode(&mut buf);
                send.write_all(&buf).await?;
            }
            Message::Nav { url } => {
                info!(%url, "NAV request");
                if let Ok(mut cu) = current_url.lock() { *cu = url.clone(); }
                wpe::load_uri(&url);
            }
            Message::Resize { viewport_w, viewport_h, .. } => {
                info!(w = viewport_w, h = viewport_h, "RESIZE request");
                wpe::resize(viewport_w as u32, viewport_h as u32);
            }
            Message::SetZoom { level_milli } => {
                let level = (level_milli as f64) / 1000.0;
                info!(level, "SET_ZOOM request");
                wpe::set_zoom(level);
            }
            Message::NavAction { action } => {
                match action {
                    0 => { info!("NAV_ACTION back");    wpe::go_back(); }
                    1 => { info!("NAV_ACTION forward"); wpe::go_forward(); }
                    2 => { info!("NAV_ACTION reload");  wpe::reload(); }
                    _ => info!(action, "NAV_ACTION unknown"),
                }
            }
            other => info!(?other, "ctrl msg (unhandled yet)"),
        }
    }
    info!("ctrl stream closed");
    Ok(())
}

/// Read the next framed message from `recv`. Accumulates into `inbuf`
/// until a full frame is available. Returns Ok(None) on clean EOF or
/// when the peer closes the whole connection between frames.
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
        match recv.read(chunk).await {
            Ok(None) => {
                return if inbuf.is_empty() { Ok(None) }
                       else { Err(anyhow!("stream ended mid-frame ({} bytes)", inbuf.len())) };
            }
            Ok(Some(n)) => inbuf.extend_from_slice(&chunk[..n]),
            // Between-frame connection loss is a clean end-of-stream, not an error.
            Err(quinn::ReadError::ConnectionLost(_)) if inbuf.is_empty() => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
}

fn rand64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
    // splitmix64 for a bit of dispersion
    let mut z = n.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

fn make_self_signed_config() -> Result<(ServerConfig, Vec<u8>)> {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into(), "gutted-host".into()])?;
    let cert_der = cert.cert.der().to_vec();
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());

    let mut rustls_cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(cert_der.clone())],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key_der),
        )?;
    rustls_cfg.alpn_protocols = vec![ALPN.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_cfg)?;
    let mut server_cfg = ServerConfig::with_crypto(Arc::new(quic_crypto));

    // Aggressive defaults for LAN. Tune later.
    let mut transport = quinn::TransportConfig::default();
    transport
        .max_concurrent_bidi_streams(64u32.into())
        .max_concurrent_uni_streams(64u32.into())
        .keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    server_cfg.transport_config(Arc::new(transport));

    Ok((server_cfg, cert_der))
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Split a frame diff into up to N tight sub-rects by grouping changed
/// rows into contiguous runs (gap-tolerant up to `gap` clean rows).
/// Empty vec = no differences.
fn diff_regions(
    a: &[u8], b: &[u8],
    width: i32, height: i32, stride: i32,
) -> Vec<(u16, u16, u16, u16)> {
    let w = width  as usize;
    let h = height as usize;
    let s = stride as usize;
    let row_bytes = w * 4;

    // 1) which rows changed?
    let mut dirty = vec![false; h];
    for y in 0..h {
        let ra = &a[y * s .. y * s + row_bytes];
        let rb = &b[y * s .. y * s + row_bytes];
        dirty[y] = ra != rb;
    }
    // 2) coalesce into runs; tolerate up to `gap` clean rows so we don't
    //    over-fragment when a shape has interior whitespace lines.
    let gap: usize = 8;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut y = 0;
    while y < h {
        if !dirty[y] { y += 1; continue; }
        let start = y;
        let mut end = y;
        y += 1;
        while y < h {
            if dirty[y] { end = y; y += 1; continue; }
            // look ahead `gap` rows for another dirty
            let look = (1..=gap).find(|k| y + k - 1 < h && dirty[y + k - 1]);
            if let Some(k) = look { y += k; } else { break; }
        }
        runs.push((start, end));
    }
    // 3) for each run, compute col bbox.
    let mut out = Vec::with_capacity(runs.len());
    for (top, bot) in runs {
        let mut left  = w;
        let mut right = 0usize;
        for yy in top..=bot {
            let ra = &a[yy * s .. yy * s + row_bytes];
            let rb = &b[yy * s .. yy * s + row_bytes];
            let mut lx = w;
            for x in 0..w { if ra[x*4..x*4+4] != rb[x*4..x*4+4] { lx = x; break; } }
            let mut rx = 0usize;
            for x in (0..w).rev() { if ra[x*4..x*4+4] != rb[x*4..x*4+4] { rx = x; break; } }
            if lx < w { left = left.min(lx); }
            if rx > 0 { right = right.max(rx); }
        }
        if right >= left {
            out.push((left as u16, top as u16,
                      (right - left + 1) as u16, (bot - top + 1) as u16));
        }
    }
    out
}

fn extract_subrect(pixels: &[u8], stride: i32, x: u16, y: u16, w: u16, h: u16) -> Vec<u8> {
    let s = stride as usize;
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for yy in 0..h as usize {
        let row_start = (y as usize + yy) * s + x as usize * 4;
        out.extend_from_slice(&pixels[row_start .. row_start + w as usize * 4]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(w: usize, h: usize, fill: [u8; 4]) -> (Vec<u8>, i32) {
        let stride = w * 4;
        let mut px = vec![0u8; stride * h];
        for chunk in px.chunks_exact_mut(4) { chunk.copy_from_slice(&fill); }
        (px, stride as i32)
    }

    /// Two separated dirty bands (top + bottom, wide clean middle) → 2 regions.
    #[test]
    fn diff_regions_splits_by_row_runs() {
        let (mut a, stride) = make_frame(100, 100, [0xFF, 0xFF, 0xFF, 0xFF]);
        let mut b = a.clone();
        // Paint a red block at rows 5..15, cols 10..40
        for y in 5..15 {
            for x in 10..40 {
                let i = y * (stride as usize) + x * 4;
                b[i..i+4].copy_from_slice(&[0x00, 0x00, 0xFF, 0xFF]);
            }
        }
        // Paint a blue block at rows 80..90, cols 50..70 (well separated from the first)
        for y in 80..90 {
            for x in 50..70 {
                let i = y * (stride as usize) + x * 4;
                b[i..i+4].copy_from_slice(&[0xFF, 0x00, 0x00, 0xFF]);
            }
        }
        let _ = &mut a; // silence warn
        let regions = diff_regions(&a, &b, 100, 100, stride);
        assert_eq!(regions.len(), 2, "expected 2 disjoint regions, got {regions:?}");
        // Verify each region tightly bounds one of the painted blocks.
        let (r0, r1) = if regions[0].1 < regions[1].1 { (regions[0], regions[1]) } else { (regions[1], regions[0]) };
        assert_eq!(r0, (10, 5, 30, 10), "top region should be (10,5,30,10) got {r0:?}");
        assert_eq!(r1, (50, 80, 20, 10), "bottom region should be (50,80,20,10) got {r1:?}");
    }

    /// Identical frames → no regions.
    #[test]
    fn diff_regions_empty_for_identical() {
        let (a, stride) = make_frame(32, 32, [0xAA; 4]);
        let b = a.clone();
        assert!(diff_regions(&a, &b, 32, 32, stride).is_empty());
    }

    /// A single tight change → one region with exact bounds.
    #[test]
    fn diff_regions_single_tight_bbox() {
        let (a, stride) = make_frame(64, 64, [0; 4]);
        let mut b = a.clone();
        for y in 10..12 {
            for x in 20..25 {
                let i = y * (stride as usize) + x * 4;
                b[i..i+4].copy_from_slice(&[0xFF; 4]);
            }
        }
        let regions = diff_regions(&a, &b, 64, 64, stride);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0], (20, 10, 5, 2));
    }
}
