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
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
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
        let (tx, _) = broadcast::channel(16);
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
    let pin_hex = hex_sha256(&cert_der);
    println!("GBROWSER_CERT_SHA256={pin_hex}");
    info!(GBROWSER_CERT_SHA256 = %pin_hex, "cert pin (pass to client)");

    let endpoint = Endpoint::server(server_cfg, addr).context("bind quic endpoint")?;
    info!(listen = %addr, "gutted-host listening");

    let bus = FrameBus::new();
    let initial_url = std::env::var("GBROWSER_URL").unwrap_or_else(|_| "https://example.com".into());
    let host_state = HostState::new(bus.clone(), &initial_url).await;

    while let Some(incoming) = endpoint.accept().await {
        let host_state = host_state.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    if let Err(e) = handle_connection(conn, host_state).await {
                        warn!(error = %e, "connection ended with error");
                    }
                }
                Err(e) => error!(error = %e, "handshake failed"),
            }
        });
    }
    Ok(())
}

struct TabEntry {
    url: String,
    title: String,
    last_frame: Option<wpe::Frame>,
}

#[derive(Clone)]
struct HostState {
    bus: FrameBus,
    runner: Arc<wpe::WpeRunner>,
    tabs: Arc<AsyncMutex<HashMap<u32, TabEntry>>>,
    active_tab_id: Arc<AtomicU32>,
    viewport_w: Arc<AtomicU32>,
    viewport_h: Arc<AtomicU32>,
}

impl HostState {
    async fn new(bus: FrameBus, initial_url: &str) -> Self {
        let (runner, mut frames, mut loads, mut cursors, mut titles, mut urls) =
            wpe::WpeRunner::start(initial_url, 1280, 648);
        let runner = Arc::new(runner);

        for _ in 0..200 {
            if runner.h().is_some() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let tabs = Arc::new(AsyncMutex::new(HashMap::new()));
        {
            let mut t = tabs.lock().await;
            t.insert(1, TabEntry {
                url: initial_url.to_string(),
                title: "New Tab".to_string(),
                last_frame: None,
            });
        }

        let active_tab_id = Arc::new(AtomicU32::new(1));
        let viewport_w = Arc::new(AtomicU32::new(1280));
        let viewport_h = Arc::new(AtomicU32::new(648));

        let bus_clone = bus.clone();
        let active_atomic = active_tab_id.clone();
        let tabs_ref = tabs.clone();

        tokio::spawn(async move {
            let mut last_per_tab: HashMap<u32, wpe::Frame> = HashMap::new();
            loop {
                tokio::select! {
                    Some(first_f) = frames.recv() => {
                        let mut batch: HashMap<u32, wpe::Frame> = HashMap::new();
                        batch.insert(first_f.tab_id, first_f);
                        while let Ok(newer) = frames.try_recv() {
                            batch.insert(newer.tab_id, newer);
                        }

                        for (this_tab_id, f) in batch {
                            info!(tab_id = this_tab_id, w = f.width, h = f.height, "WPE frame received in Rust");
                            {
                                let mut t = tabs_ref.lock().await;
                                if let Some(tab) = t.get_mut(&this_tab_id) {
                                    tab.last_frame = Some(f.clone());
                                }
                            }

                            let active = active_atomic.load(Ordering::Relaxed);
                            if active == this_tab_id {
                                let ts_us = SystemTime::now().duration_since(UNIX_EPOCH)
                                    .unwrap_or_default().as_micros() as u64;
                                let last = last_per_tab.get(&this_tab_id);
                                let same_geom = last.is_some_and(|l|
                                    l.width == f.width && l.height == f.height && l.stride == f.stride);
                                let msg: Option<Arc<Message>> = if !same_geom {
                                    Some(Arc::new(Message::RawFrame {
                                        ts_us,
                                        width: f.width as u16, height: f.height as u16,
                                        stride: f.stride as u32, format: f.format,
                                        compression: gutted_proto::compression::ZSTD_DELTA,
                                        pixels: f.pixels.clone(),
                                    }))
                                } else {
                                    let prev = last.unwrap();
                                    let regions = diff_regions(&prev.pixels, &f.pixels, f.width, f.height, f.stride);
                                    if regions.is_empty() {
                                        None
                                    } else {
                                        let total_px: u64 = regions.iter().map(|(_, _, w, h)| (*w as u64) * (*h as u64)).sum();
                                        let full_px = (f.width as u64) * (f.height as u64);
                                        if total_px * 20 >= full_px * 7 || regions.len() > 3 {
                                            Some(Arc::new(Message::RawFrame {
                                                ts_us,
                                                width: f.width as u16, height: f.height as u16,
                                                stride: f.stride as u32, format: f.format,
                                                compression: gutted_proto::compression::ZSTD_DELTA,
                                                pixels: f.pixels.clone(),
                                            }))
                                        } else {
                                            for (x, y, w, h) in &regions {
                                                let sub = extract_subrect(&f.pixels, f.stride, *x, *y, *w, *h);
                                                let m = Arc::new(Message::Subframe {
                                                    ts_us, x: *x, y: *y, w: *w, h: *h,
                                                    stride: (*w as u32) * 4, format: f.format,
                                                    compression: gutted_proto::compression::ZSTD_DELTA,
                                                    pixels: sub,
                                                });
                                                bus_clone.publish(m).await;
                                            }
                                            Some(Arc::new(Message::Heartbeat { ts_us: 0 }))
                                        }
                                    }
                                };
                                last_per_tab.insert(this_tab_id, f);
                                if let Some(m) = msg {
                                    if !matches!(&*m, Message::Heartbeat { .. }) {
                                        bus_clone.publish(m).await;
                                    }
                                }
                            } else {
                                last_per_tab.insert(this_tab_id, f);
                            }
                        }
                    }
                    Some((this_tab_id, s)) = loads.recv() => {
                        let state = match s {
                            wpe::LoadState::Started    => { last_per_tab.remove(&this_tab_id); 0u8 },
                            wpe::LoadState::Redirected => 1u8,
                            wpe::LoadState::Committed  => { last_per_tab.remove(&this_tab_id); 2u8 },
                            wpe::LoadState::Finished   => 3u8,
                            wpe::LoadState::Unknown    => 255u8,
                        };
                        if active_atomic.load(Ordering::Relaxed) == this_tab_id {
                            bus_clone.publish(Arc::new(Message::LoadState { state })).await;
                        }
                    }
                    Some((this_tab_id, title)) = titles.recv() => {
                        {
                            let mut t = tabs_ref.lock().await;
                            if let Some(tab) = t.get_mut(&this_tab_id) {
                                tab.title = title.clone();
                            }
                        }
                        bus_clone.publish(Arc::new(Message::TabTitle { tab_id: this_tab_id, title: title.clone() })).await;
                        if active_atomic.load(Ordering::Relaxed) == this_tab_id {
                            bus_clone.publish(Arc::new(Message::Title { title })).await;
                        }
                    }
                    Some((this_tab_id, new_url)) = urls.recv() => {
                        last_per_tab.remove(&this_tab_id);
                        {
                            let mut t = tabs_ref.lock().await;
                            if let Some(tab) = t.get_mut(&this_tab_id) {
                                tab.url = new_url.clone();
                            }
                        }
                        bus_clone.publish(Arc::new(Message::TabUrl { tab_id: this_tab_id, url: new_url.clone() })).await;
                        if active_atomic.load(Ordering::Relaxed) == this_tab_id {
                            bus_clone.publish(Arc::new(Message::UrlChanged { url: new_url })).await;
                        }
                    }
                    Some((this_tab_id, shape_id)) = cursors.recv() => {
                        if active_atomic.load(Ordering::Relaxed) == this_tab_id {
                            use gutted_proto::CursorShape;
                            let shape = match shape_id {
                                1 => CursorShape::Pointer,
                                2 => CursorShape::Text,
                                _ => CursorShape::Default,
                            };
                            bus_clone.publish(Arc::new(Message::CursorState { shape, hotspot_x: 0, hotspot_y: 0, image_ref: 0 })).await;
                        }
                    }
                    else => break,
                }
            }
        });

        Self {
            bus,
            runner,
            tabs,
            active_tab_id,
            viewport_w,
            viewport_h,
        }
    }

    async fn create_tab(&self, tab_id: u32, url: &str, make_active: bool) {
        {
            let mut tabs = self.tabs.lock().await;
            tabs.insert(tab_id, TabEntry {
                url: url.to_string(),
                title: "New Tab".to_string(),
                last_frame: None,
            });
        }
        self.runner.create_tab(tab_id, url);
        if make_active {
            self.switch_tab(tab_id).await;
        }
    }

    async fn switch_tab(&self, tab_id: u32) {
        self.active_tab_id.store(tab_id, Ordering::Relaxed);
        let tabs = self.tabs.lock().await;
        if let Some(tab) = tabs.get(&tab_id) {
            if let Some(f) = &tab.last_frame {
                let ts_us = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as u64;
                let msg = Arc::new(Message::RawFrame {
                    ts_us,
                    width: f.width as u16, height: f.height as u16,
                    stride: f.stride as u32, format: f.format,
                    compression: gutted_proto::compression::ZSTD_DELTA,
                    pixels: f.pixels.clone(),
                });
                self.bus.publish(msg).await;
            }
            self.bus.publish(Arc::new(Message::Title { title: tab.title.clone() })).await;
            self.bus.publish(Arc::new(Message::UrlChanged { url: tab.url.clone() })).await;
            self.bus.publish(Arc::new(Message::TabActivated { tab_id })).await;
        }
    }

    async fn close_tab(&self, tab_id: u32) {
        {
            let mut tabs = self.tabs.lock().await;
            tabs.remove(&tab_id);
        }
        self.runner.close_tab(tab_id);
    }

    async fn load_uri(&self, url: &str) {
        let active = self.active_tab_id.load(Ordering::Relaxed);
        self.runner.load_uri(active, url);
    }

    async fn resize_all(&self, w: u32, h: u32) {
        self.viewport_w.store(w, Ordering::Relaxed);
        self.viewport_h.store(h, Ordering::Relaxed);
        self.runner.resize_all(w, h);
    }

    async fn inject_pointer_motion(&self, x: i32, y: i32, modifiers: u32) {
        let active = self.active_tab_id.load(Ordering::Relaxed);
        self.runner.inject_pointer_motion(active, x, y, modifiers);
    }

    async fn inject_pointer_button(&self, x: i32, y: i32, button: u32, pressed: bool, modifiers: u32) {
        let active = self.active_tab_id.load(Ordering::Relaxed);
        self.runner.inject_pointer_button(active, x, y, button, pressed, modifiers);
    }

    async fn inject_key(&self, keycode: u32, mods: u32, down: bool) {
        let active = self.active_tab_id.load(Ordering::Relaxed);
        self.runner.inject_key(active, keycode, mods, down);
    }

    async fn inject_axis(&self, x: i32, y: i32, dx: f64, dy: f64, modifiers: u32) {
        let active = self.active_tab_id.load(Ordering::Relaxed);
        self.runner.inject_axis(active, x, y, dx, dy, modifiers);
    }

    async fn set_zoom(&self, level: f64) {
        let active = self.active_tab_id.load(Ordering::Relaxed);
        self.runner.set_zoom(active, level);
    }

    async fn go_back(&self) {
        let active = self.active_tab_id.load(Ordering::Relaxed);
        self.runner.go_back(active);
    }

    async fn go_forward(&self) {
        let active = self.active_tab_id.load(Ordering::Relaxed);
        self.runner.go_forward(active);
    }

    async fn reload(&self) {
        let active = self.active_tab_id.load(Ordering::Relaxed);
        self.runner.reload(active);
    }

    async fn stop_loading(&self) {
        let active = self.active_tab_id.load(Ordering::Relaxed);
        self.runner.stop_loading(active);
    }

    fn clear_data(&self, cookies: bool, cache: bool, storage: bool) {
        self.runner.clear_data(cookies, cache, storage);
    }

    async fn current_url(&self) -> String {
        let active = self.active_tab_id.load(Ordering::Relaxed);
        let tabs = self.tabs.lock().await;
        tabs.get(&active).map(|t| t.url.clone()).unwrap_or_default()
    }
}

async fn handle_connection(
    conn: quinn::Connection,
    host_state: HostState,
) -> Result<()> {
    let peer = conn.remote_address();
    info!(%peer, "client connected");

    let dgram_conn = conn.clone();
    let host_state_dgram = host_state.clone();
    tokio::spawn(async move {
        while let Ok(data) = dgram_conn.read_datagram().await {
            let mut cur = &data[..];
            if let Ok(Some(Message::InputPointer { x, y, modifiers, .. })) = Message::decode(&mut cur) {
                host_state_dgram.inject_pointer_motion(x, y, modifiers).await;
            }
        }
    });

    loop {
        tokio::select! {
            biresult = conn.accept_bi() => {
                let (send, recv) = biresult?;
                let host_state = host_state.clone();
                let conn2 = conn.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_ctrl_stream(send, recv, conn2, host_state).await {
                        warn!(error = %e, "ctrl stream error");
                    }
                });
            }
            uniresult = conn.accept_uni() => {
                let recv = uniresult?;
                let host_state = host_state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_input_stream(recv, host_state).await {
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
async fn handle_input_stream(mut recv: quinn::RecvStream, host_state: HostState) -> Result<()> {
    let mut inbuf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; 8192];
    let mut n_events = 0u64;
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
                            host_state.inject_pointer_motion(x, y, modifiers).await;
                        }
                        Message::InputButton { x, y, button, pressed, modifiers, .. } => {
                            last_pointer = (x, y);
                            host_state.inject_pointer_button(x, y, button, pressed, modifiers).await;
                            info!(x, y, button, pressed, "button injected");
                        }
                        Message::InputKey { keycode, mods, down, .. } => {
                            host_state.inject_key(keycode, mods, down).await;
                            info!(keycode = format!("0x{keycode:x}"), down, "key injected");
                        }
                        Message::InputScroll { dx_units, dy_units, .. } => {
                            let dx = dx_units as f64 * 40.0;
                            let dy = dy_units as f64 * 40.0;
                            let (px, py) = last_pointer;
                            host_state.inject_axis(px, py, dx, dy, 0).await;
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
    host_state: HostState,
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
    if viewport_w > 0 && viewport_h > 0 {
        host_state.resize_all(viewport_w as u32, viewport_h as u32).await;
    }

    // ── Reply with WELCOME ───────────────────────────────────────────────
    let session_id: u64 = rand64();
    let welcome = Message::Welcome {
        proto_version: PROTO_VERSION,
        session_id,
        features: capabilities & (caps::H264 | caps::CLIENT_SCROLL), // intersection
        cursor_track_id: 0, // no cursor track yet — Phase 2
        current_url: host_state.current_url().await,
    };
    let mut out = Vec::with_capacity(64);
    welcome.encode(&mut out);
    send.write_all(&out).await?;
    info!(session_id = format!("0x{session_id:016x}"), "WELCOME sent");

    // Spawn the video uni-stream publisher.
    let video_conn = conn.clone();
    let bus_for_video = host_state.bus.clone();
    let mut stream_rx = host_state.bus.subscribe();
    tokio::spawn(async move {
        let mut vs = match video_conn.open_uni().await {
            Ok(s) => s,
            Err(e) => { warn!(error = %e, "open_uni video stream"); return; }
        };
        let _ = vs.set_priority((-1i32).into());   // lower than default (0)
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

        // Initial state: send the last full RawFrame
        let mut published = 0u64;
        let mut cum_bytes: u64 = 0;
        let mut cum_full: u64 = 0;
        let mut cum_sub: u64 = 0;
        if let Some(snap_m) = bus_for_video.snapshot().await {
            let (res, snap_len) = write_frame(&mut vs, &snap_m).await;
            if let Ok(()) = res {
                published += 1;
                cum_bytes += snap_len as u64; cum_full += snap_len as u64;
                info!(bytes = snap_len, "initial FULL frame snapshot delivered to new subscriber");
            }
        }

        loop {
            tokio::select! {
                res = stream_rx.recv() => match res {
                    Ok(m) => {
                        let (res, n) = write_frame(&mut vs, &m).await;
                        if let Err(e) = res {
                            warn!(error = %e, "video write");
                            break;
                        }
                        published += 1;
                        cum_bytes += n as u64;
                        match &*m {
                            Message::RawFrame { .. } => cum_full += n as u64,
                            Message::Subframe { .. } => cum_sub  += n as u64,
                            _ => {}
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(dropped = n, "video subscriber lagged — resyncing via immediate FULL snapshot");
                        if let Some(snap_m) = bus_for_video.snapshot().await {
                            let (res, snap_len) = write_frame(&mut vs, &snap_m).await;
                            if let Ok(()) = res {
                                published += 1;
                                cum_bytes += snap_len as u64; cum_full += snap_len as u64;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = video_conn.closed() => break,
            }
        }
        let _ = vs.finish();
        info!(published, cum_bytes, cum_full, cum_sub, "VIDEO STREAM CLOSED");
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
                host_state.load_uri(&url).await;
            }
            Message::Resize { viewport_w, viewport_h, .. } => {
                info!(w = viewport_w, h = viewport_h, "RESIZE request");
                host_state.resize_all(viewport_w as u32, viewport_h as u32).await;
            }
            Message::SetZoom { level_milli } => {
                let level = (level_milli as f64) / 1000.0;
                info!(level, "SET_ZOOM request");
                host_state.set_zoom(level).await;
            }
            Message::NavAction { action } => {
                match action {
                    0 => { info!("NAV_ACTION back");    host_state.go_back().await; }
                    1 => { info!("NAV_ACTION forward"); host_state.go_forward().await; }
                    2 => { info!("NAV_ACTION reload");  host_state.reload().await; }
                    _ => info!(action, "NAV_ACTION unknown"),
                }
            }
            Message::Stop => {
                info!("STOP request");
                host_state.stop_loading().await;
            }
            Message::ClearData { clear_cookies, clear_cache, clear_storage } => {
                info!(clear_cookies, clear_cache, clear_storage, "CLEAR_DATA request");
                host_state.clear_data(clear_cookies, clear_cache, clear_storage);
            }
            Message::CreateTab { tab_id, url } => {
                info!(tab_id, %url, "CREATE_TAB request");
                host_state.create_tab(tab_id, &url, true).await;
                let resp = Message::TabCreated {
                    tab_id,
                    title: "New Tab".into(),
                    url: url.clone(),
                };
                let mut buf = Vec::with_capacity(64);
                resp.encode(&mut buf);
                let _ = send.write_all(&buf).await;
            }
            Message::CloseTab { tab_id } => {
                info!(tab_id, "CLOSE_TAB request");
                host_state.close_tab(tab_id).await;
                let resp = Message::TabClosed { tab_id };
                let mut buf = Vec::with_capacity(16);
                resp.encode(&mut buf);
                let _ = send.write_all(&buf).await;
            }
            Message::SwitchTab { tab_id } => {
                info!(tab_id, "SWITCH_TAB request");
                host_state.switch_tab(tab_id).await;
                let resp = Message::TabActivated { tab_id };
                let mut buf = Vec::with_capacity(16);
                resp.encode(&mut buf);
                let _ = send.write_all(&buf).await;
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
        .keep_alive_interval(Some(std::time::Duration::from_secs(5)))
        .datagram_receive_buffer_size(Some(1024 * 1024))
        .datagram_send_buffer_size(1024 * 1024)
        .stream_receive_window((1024 * 1024u32).into())
        .receive_window((2 * 1024 * 1024u32).into())
        .send_window(2 * 1024 * 1024);
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
    let mut dirty_count = 0usize;
    let mut first_dirty = h;
    let mut last_dirty = 0usize;

    for y in 0..h {
        let ra = &a[y * s .. y * s + row_bytes];
        let rb = &b[y * s .. y * s + row_bytes];
        let is_d = ra != rb;
        dirty[y] = is_d;
        if is_d {
            dirty_count += 1;
            if first_dirty == h { first_dirty = y; }
            last_dirty = y;
        }
    }
    if dirty_count == 0 {
        return Vec::new();
    }

    // Fast-path: if >25% of the screen rows changed (e.g. video or scrolling),
    // emit a single bounding subrect across the full width to prevent multi-region tearing.
    if dirty_count * 4 >= h {
        return vec![(0, first_dirty as u16, w as u16, (last_dirty - first_dirty + 1) as u16)];
    }

    // 2) coalesce into runs; tolerate up to `gap` clean rows so we don't
    //    over-fragment when a shape has interior whitespace lines.
    let gap: usize = 4;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut y = 0;
    while y < h {
        if !dirty[y] { y += 1; continue; }
        let start = y;
        let mut end = y;
        y += 1;
        while y < h {
            if dirty[y] {
                end = y;
                y += 1;
            } else {
                let look = (1..=gap).find(|&k| y + k < h && dirty[y + k]);
                if let Some(k) = look {
                    y += k;
                } else {
                    break;
                }
            }
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
            let ra_u32 = align_slice_u32(ra);
            let rb_u32 = align_slice_u32(rb);
            if let (Some(sa), Some(sb)) = (ra_u32, rb_u32) {
                if let Some(lx) = sa.iter().zip(sb.iter()).position(|(pa, pb)| pa != pb) {
                    left = left.min(lx);
                    let rx = sa.iter().zip(sb.iter()).rposition(|(pa, pb)| pa != pb).unwrap_or(lx);
                    right = right.max(rx);
                }
            } else {
                if let Some(lx) = ra.chunks_exact(4).zip(rb.chunks_exact(4)).position(|(px_a, px_b)| px_a != px_b) {
                    left = left.min(lx);
                    let rx = ra.chunks_exact(4).zip(rb.chunks_exact(4)).rposition(|(px_a, px_b)| px_a != px_b).unwrap_or(lx);
                    right = right.max(rx);
                }
            }
        }
        if right >= left {
            out.push((left as u16, top as u16,
                      (right - left + 1) as u16, (bot - top + 1) as u16));
        }
    }
    out
}

#[inline(always)]
fn align_slice_u32(s: &[u8]) -> Option<&[u32]> {
    if (s.as_ptr() as usize) % std::mem::align_of::<u32>() == 0 && s.len() % 4 == 0 {
        Some(unsafe { std::slice::from_raw_parts(s.as_ptr() as *const u32, s.len() / 4) })
    } else {
        None
    }
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
