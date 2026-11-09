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
mod web;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use gutted_proto::{caps, Message, PROTO_VERSION};
use quinn::{Endpoint, ServerConfig};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, Mutex as AsyncMutex};
use tracing::{error, info, warn};

#[derive(Clone, Copy, Debug)]
pub enum BusMeta {
    RawFrame,
    Subframe { x: u16, y: u16, w: u16, h: u16 },
    Other,
}

#[derive(Clone)]
pub struct BusItem {
    pub meta: BusMeta,
    pub wire_quic: Bytes,
}

/// Fan-out for WPE frames.
///
/// Pre-encodes each published message once into `wire_quic` so multiple QUIC,
/// TCP, and WebSocket subscribers write directly without re-running spatial
/// filtering or LZ4 compression, and without retaining multi-megabyte raw
/// pixel buffers in the broadcast ring.
#[derive(Clone)]
pub struct FrameBus {
    last_frames: Arc<StdMutex<HashMap<u32, wpe::Frame>>>,
    active_tab_id: Arc<AtomicU32>,
    tx: broadcast::Sender<BusItem>,
}

impl FrameBus {
    fn new(
        last_frames: Arc<StdMutex<HashMap<u32, wpe::Frame>>>,
        active_tab_id: Arc<AtomicU32>,
    ) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self { last_frames, active_tab_id, tx }
    }

    fn encode_msg(msg: &Message) -> BusItem {
        let meta = match msg {
            Message::RawFrame { .. } => BusMeta::RawFrame,
            Message::Subframe { x, y, w, h, .. } => BusMeta::Subframe {
                x: *x,
                y: *y,
                w: *w,
                h: *h,
            },
            _ => BusMeta::Other,
        };
        let mut buf = Vec::with_capacity(64);
        msg.encode(&mut buf);
        BusItem {
            meta,
            wire_quic: Bytes::from(buf),
        }
    }

    fn publish(&self, msg: Arc<Message>) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        let _ = self.tx.send(Self::encode_msg(&msg));
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_raw_frame(
        &self,
        ts_us: u64,
        width: u16,
        height: u16,
        stride: u32,
        format: u32,
        compression: u8,
        pixels: &[u8],
    ) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        let mut buf = Vec::with_capacity(64 + pixels.len() / 8);
        Message::encode_raw_frame_from_slice(
            &mut buf,
            ts_us,
            width,
            height,
            stride,
            format,
            compression,
            pixels,
        );
        let _ = self.tx.send(BusItem {
            meta: BusMeta::RawFrame,
            wire_quic: Bytes::from(buf),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_subframe(
        &self,
        ts_us: u64,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        stride: u32,
        format: u32,
        compression: u8,
        pixels: &[u8],
    ) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        let mut buf = Vec::with_capacity(64 + pixels.len() / 4);
        Message::encode_subframe_from_slice(
            &mut buf,
            ts_us,
            x,
            y,
            w,
            h,
            stride,
            format,
            compression,
            pixels,
        );
        let _ = self.tx.send(BusItem {
            meta: BusMeta::Subframe { x, y, w, h },
            wire_quic: Bytes::from(buf),
        });
    }

    #[inline]
    pub fn has_subscribers(&self) -> bool {
        self.tx.receiver_count() > 0
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusItem> {
        self.tx.subscribe()
    }

    pub fn snapshot(&self) -> Option<BusItem> {
        let active = self.active_tab_id.load(Ordering::Relaxed);
        let f = self.last_frames.lock().ok()?.get(&active)?.clone();
        let ts_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        let mut buf = Vec::with_capacity(64 + f.pixels.len() / 8);
        Message::encode_raw_frame_from_slice(
            &mut buf,
            ts_us,
            f.width as u16,
            f.height as u16,
            f.stride as u32,
            f.format,
            gutted_proto::compression::ZSTD,
            &f.pixels,
        );
        Some(BusItem {
            meta: BusMeta::RawFrame,
            wire_quic: Bytes::from(buf),
        })
    }
}

/// Drain all immediately available items from `rx` and coalesce superseded
/// `RawFrame`/`Subframe` updates across the entire batch (even when `AudioFrame`
/// or control messages are interleaved between video frames).
pub fn coalesce_bus_batch(
    first: BusItem,
    rx: &mut broadcast::Receiver<BusItem>,
) -> Vec<BusItem> {
    let mut batch = Vec::with_capacity(8);
    batch.push(first);
    while let Ok(newer) = rx.try_recv() {
        match newer.meta {
            BusMeta::RawFrame => {
                batch.retain(|old| !matches!(old.meta, BusMeta::RawFrame | BusMeta::Subframe { .. }));
            }
            BusMeta::Subframe {
                x: x2,
                y: y2,
                w: w2,
                h: h2,
            } => {
                let nx0 = x2 as u32;
                let ny0 = y2 as u32;
                let nx1 = nx0 + (w2 as u32);
                let ny1 = ny0 + (h2 as u32);
                batch.retain(|old| match old.meta {
                    BusMeta::Subframe {
                        x: x1,
                        y: y1,
                        w: w1,
                        h: h1,
                    } => {
                        let ox0 = x1 as u32;
                        let oy0 = y1 as u32;
                        let ox1 = ox0 + (w1 as u32);
                        let oy1 = oy0 + (h1 as u32);
                        !(nx0 <= ox0 && ny0 <= oy0 && nx1 >= ox1 && ny1 >= oy1)
                    }
                    _ => true,
                });
            }
            BusMeta::Other => {}
        }
        batch.push(newer);
    }
    batch
}

const ALPN: &[u8] = b"gbrowser/1";
const DEFAULT_ADDR: &str = "0.0.0.0:4433";
const DEFAULT_HTTP_ADDR: &str = "0.0.0.0:8080";

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

    let endpoint = Endpoint::server(server_cfg, addr).context("bind quic endpoint")?;
    let tcp_listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("bind tcp listener")?;

    let initial_url = std::env::var("GBROWSER_URL").unwrap_or_else(|_| "https://example.com".into());
    let host_state = HostState::new(&initial_url).await;

    // Spawn built-in Web Client Server (HTTP + WebSocket)
    let http_addr_raw = std::env::var("GBROWSER_HTTP_LISTEN")
        .or_else(|_| std::env::var("GBROWSER_WEB_LISTEN"))
        .unwrap_or_else(|_| DEFAULT_HTTP_ADDR.into());
    let http_addr: SocketAddr = http_addr_raw.parse().context("parse http listen addr")?;
    let host_state_web = host_state.clone();
    tokio::spawn(async move {
        if let Err(e) = web::run_web_server(http_addr, host_state_web).await {
            error!(error = %e, "web server failed");
        }
    });

    // Spawn C-free Pure-Rust Framed TCP listener on the same port (TCP :4433 alongside UDP :4433 QUIC)
    info!(listen = %addr, "gutted-host TCP stream listener ready (Redox/relibc & C-free clients)");
    let host_state_tcp = host_state.clone();
    tokio::spawn(async move {
        while let Ok((stream, peer)) = tcp_listener.accept().await {
            let hs = host_state_tcp.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_tcp_connection(stream, peer, hs).await {
                    warn!(%peer, error = %e, "tcp connection ended with error");
                }
            });
        }
    });

    // Print readiness token AFTER WPE WebKit, QUIC, and TCP listeners are bound and ready
    println!("GBROWSER_CERT_SHA256={pin_hex}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    info!(GBROWSER_CERT_SHA256 = %pin_hex, listen = %addr, "gutted-host listening");

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
}

#[derive(Clone)]
struct HostState {
    bus: FrameBus,
    runner: Arc<wpe::WpeRunner>,
    tabs: Arc<AsyncMutex<HashMap<u32, TabEntry>>>,
    last_frames: Arc<StdMutex<HashMap<u32, wpe::Frame>>>,
    active_tab_id: Arc<AtomicU32>,
    viewport_w: Arc<AtomicU32>,
    viewport_h: Arc<AtomicU32>,
}

impl HostState {
    async fn new(initial_url: &str) -> Self {
        init_pulse_null_sink();
        let active_tab_id = Arc::new(AtomicU32::new(1));
        let (
            runner,
            wpe::WpeChannels {
                mut frames,
                mut loads,
                mut cursors,
                mut titles,
                mut urls,
                mut file_choosers,
                mut downloads,
            },
        ) = wpe::WpeRunner::start(initial_url, 1280, 648, active_tab_id.clone());
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
            });
        }

        let last_frames = Arc::new(StdMutex::new(HashMap::new()));
        let viewport_w = Arc::new(AtomicU32::new(1280));
        let viewport_h = Arc::new(AtomicU32::new(648));
        let bus = FrameBus::new(last_frames.clone(), active_tab_id.clone());
        spawn_audio_capture(bus.clone());

        let bus_clone = bus.clone();
        let active_atomic = active_tab_id.clone();
        let tabs_ref = tabs.clone();
        let last_frames_ref = last_frames.clone();

        tokio::spawn(async move {
            let mut force_full: HashSet<u32> = HashSet::new();
            let mut q2_dirty_since: HashMap<u32, std::time::Instant> = HashMap::new();
            let mut refine_tick = tokio::time::interval(std::time::Duration::from_millis(60));
            refine_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = refine_tick.tick(), if !q2_dirty_since.is_empty() => {
                        let active = active_atomic.load(Ordering::Relaxed);
                        if let Some(&since) = q2_dirty_since.get(&active) {
                            if since.elapsed() >= std::time::Duration::from_millis(140) {
                                q2_dirty_since.remove(&active);
                                if bus_clone.has_subscribers() {
                                    if let Some(f) = last_frames_ref.lock().ok().and_then(|g| g.get(&active).cloned()) {
                                        let ts_us = SystemTime::now().duration_since(UNIX_EPOCH)
                                            .unwrap_or_default().as_micros() as u64;
                                        bus_clone.publish_raw_frame(
                                            ts_us,
                                            f.width as u16,
                                            f.height as u16,
                                            f.stride as u32,
                                            f.format,
                                            gutted_proto::compression::ZSTD,
                                            &f.pixels,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Some(first_f) = frames.recv() => {
                        let mut batch: HashMap<u32, wpe::Frame> = HashMap::new();
                        batch.insert(first_f.tab_id, first_f);
                        while let Ok(newer) = frames.try_recv() {
                            batch.insert(newer.tab_id, newer);
                        }

                        for (this_tab_id, f) in batch {
                            let active = active_atomic.load(Ordering::Relaxed);
                            if active == this_tab_id && bus_clone.has_subscribers() {
                                let ts_us = SystemTime::now().duration_since(UNIX_EPOCH)
                                    .unwrap_or_default().as_micros() as u64;
                                let must_full = force_full.remove(&this_tab_id);
                                let prev = last_frames_ref.lock().ok().and_then(|g| g.get(&this_tab_id).cloned());
                                let same_geom = !must_full && prev.as_ref().is_some_and(|l|
                                    l.width == f.width && l.height == f.height && l.stride == f.stride);

                                if !same_geom {
                                    q2_dirty_since.remove(&this_tab_id);
                                    bus_clone.publish_raw_frame(
                                        ts_us,
                                        f.width as u16,
                                        f.height as u16,
                                        f.stride as u32,
                                        f.format,
                                        gutted_proto::compression::ZSTD,
                                        &f.pixels,
                                    );
                                    if let Ok(mut g) = last_frames_ref.lock() {
                                        g.insert(this_tab_id, f);
                                    }
                                } else {
                                    let prev_f = prev.unwrap();
                                    let regions = diff_regions(&prev_f.pixels, &f.pixels, f.width, f.height, f.stride);
                                    if !regions.is_empty() {
                                        let total_px: u64 = regions.iter().map(|(_, _, w, h)| (*w as u64) * (*h as u64)).sum();
                                        let full_px = (f.width as u64) * (f.height as u64);
                                        if total_px * 10 >= full_px * 8 || regions.len() > 16 {
                                            let comp = if f.stride == f.width * 4 {
                                                q2_dirty_since.insert(this_tab_id, std::time::Instant::now());
                                                gutted_proto::compression::LZ4_DELTA_Q2
                                            } else {
                                                q2_dirty_since.remove(&this_tab_id);
                                                gutted_proto::compression::ZSTD
                                            };
                                            bus_clone.publish_raw_frame(
                                                ts_us,
                                                f.width as u16,
                                                f.height as u16,
                                                f.stride as u32,
                                                f.format,
                                                comp,
                                                &f.pixels,
                                            );
                                            if let Ok(mut g) = last_frames_ref.lock() {
                                                g.insert(this_tab_id, f);
                                            }
                                        } else {
                                            let fmt = f.format;
                                            let stride = f.stride;
                                            let s_usize = stride as usize;
                                            for &(x, y, w, h) in &regions {
                                                let area = (w as usize) * (h as usize);
                                                let comp = if area >= 96_000 {
                                                    q2_dirty_since.insert(this_tab_id, std::time::Instant::now());
                                                    gutted_proto::compression::LZ4_DELTA_Q2
                                                } else if area >= 32_000 {
                                                    gutted_proto::compression::ZSTD
                                                } else {
                                                    gutted_proto::compression::ZSTD_DELTA
                                                };
                                                if x == 0 && (w as i32) * 4 == stride {
                                                    let start = (y as usize) * s_usize;
                                                    let end = start + (h as usize) * s_usize;
                                                    bus_clone.publish_subframe(
                                                        ts_us,
                                                        x,
                                                        y,
                                                        w,
                                                        h,
                                                        (w as u32) * 4,
                                                        fmt,
                                                        comp,
                                                        &f.pixels[start..end],
                                                    );
                                                } else {
                                                    let sub = extract_subrect(&f.pixels, stride, x, y, w, h);
                                                    bus_clone.publish_subframe(
                                                        ts_us,
                                                        x,
                                                        y,
                                                        w,
                                                        h,
                                                        (w as u32) * 4,
                                                        fmt,
                                                        comp,
                                                        &sub,
                                                    );
                                                }
                                            }
                                            if let Ok(mut g) = last_frames_ref.lock() {
                                                g.insert(this_tab_id, f);
                                            }
                                        }
                                    } else if let Ok(mut g) = last_frames_ref.lock() {
                                        g.insert(this_tab_id, f);
                                    }
                                }
                            } else if let Ok(mut g) = last_frames_ref.lock() {
                                g.insert(this_tab_id, f);
                            }
                        }
                    }
                    Some((this_tab_id, s)) = loads.recv() => {
                        let state = match s {
                            wpe::LoadState::Started    => { force_full.insert(this_tab_id); 0u8 },
                            wpe::LoadState::Redirected => 1u8,
                            wpe::LoadState::Committed  => { force_full.insert(this_tab_id); 2u8 },
                            wpe::LoadState::Finished   => 3u8,
                            wpe::LoadState::Unknown    => 255u8,
                        };
                        if active_atomic.load(Ordering::Relaxed) == this_tab_id {
                            bus_clone.publish(Arc::new(Message::LoadState { state }));
                        }
                    }
                    Some((this_tab_id, title)) = titles.recv() => {
                        {
                            let mut t = tabs_ref.lock().await;
                            if let Some(tab) = t.get_mut(&this_tab_id) {
                                tab.title = title.clone();
                            }
                        }
                        bus_clone.publish(Arc::new(Message::TabTitle { tab_id: this_tab_id, title: title.clone() }));
                        if active_atomic.load(Ordering::Relaxed) == this_tab_id {
                            bus_clone.publish(Arc::new(Message::Title { title }));
                        }
                    }
                    Some((this_tab_id, new_url)) = urls.recv() => {
                        force_full.insert(this_tab_id);
                        {
                            let mut t = tabs_ref.lock().await;
                            if let Some(tab) = t.get_mut(&this_tab_id) {
                                tab.url = new_url.clone();
                            }
                        }
                        bus_clone.publish(Arc::new(Message::TabUrl { tab_id: this_tab_id, url: new_url.clone() }));
                        if active_atomic.load(Ordering::Relaxed) == this_tab_id {
                            bus_clone.publish(Arc::new(Message::UrlChanged { url: new_url }));
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
                            bus_clone.publish(Arc::new(Message::CursorState { shape, hotspot_x: 0, hotspot_y: 0, image_ref: 0 }));
                        }
                    }
                    Some((this_tab_id, multiple, accept_mimes)) = file_choosers.recv() => {
                        info!(tab_id = this_tab_id, multiple, %accept_mimes, "WPE requested file chooser");
                        bus_clone.publish(Arc::new(Message::FileChooserPrompt {
                            tab_id: this_tab_id,
                            multiple,
                            accept_mimes,
                        }));
                    }
                    Some((filename, local_path)) = downloads.recv() => {
                        if let Ok(meta) = std::fs::metadata(&local_path) {
                            if meta.len() <= 12 * 1024 * 1024 {
                                if let Ok(data) = std::fs::read(&local_path) {
                                    info!(%filename, bytes = data.len(), "delivering finished download to clients");
                                    bus_clone.publish(Arc::new(Message::FileDownload { filename, data }));
                                }
                            } else {
                                warn!(%filename, bytes = meta.len(), "download exceeds 12 MiB wire limit; saved on host at {}", local_path);
                            }
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
            last_frames,
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
            });
        }
        self.runner.create_tab(tab_id, url);
        self.bus.publish(Arc::new(Message::TabCreated {
            tab_id,
            title: "New Tab".to_string(),
            url: url.to_string(),
        }));
        if make_active {
            self.switch_tab(tab_id).await;
        } else {
            let active = self.active_tab_id.load(Ordering::Relaxed);
            self.runner.set_active_tab(active);
        }
    }

    async fn switch_tab(&self, tab_id: u32) {
        self.active_tab_id.store(tab_id, Ordering::Relaxed);
        self.runner.set_active_tab(tab_id);
        let tabs = self.tabs.lock().await;
        if let Some(tab) = tabs.get(&tab_id) {
            if let Some(f) = self.last_frames.lock().ok().and_then(|g| g.get(&tab_id).cloned()) {
                let ts_us = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as u64;
                self.bus.publish_raw_frame(
                    ts_us,
                    f.width as u16,
                    f.height as u16,
                    f.stride as u32,
                    f.format,
                    gutted_proto::compression::ZSTD,
                    &f.pixels,
                );
            }
            self.bus.publish(Arc::new(Message::Title { title: tab.title.clone() }));
            self.bus.publish(Arc::new(Message::UrlChanged { url: tab.url.clone() }));
            self.bus.publish(Arc::new(Message::TabActivated { tab_id }));
        }
    }

    async fn close_tab(&self, tab_id: u32) {
        {
            let mut tabs = self.tabs.lock().await;
            tabs.remove(&tab_id);
        }
        if let Ok(mut g) = self.last_frames.lock() {
            g.remove(&tab_id);
        }
        self.runner.close_tab(tab_id);
        self.bus.publish(Arc::new(Message::TabClosed { tab_id }));
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

    fn paste_text(&self, text: &str) {
        let active = self.active_tab_id.load(Ordering::Relaxed);
        self.runner.paste_text(active, text);
    }

    fn handle_file_upload(&self, tab_id: u32, filename: &str, mime_type: &str, data: &[u8]) {
        let target_tab = if tab_id == 0 {
            self.active_tab_id.load(Ordering::Relaxed)
        } else {
            tab_id
        };
        let safe_name: String = filename
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
            .collect();
        let safe_name = if safe_name.is_empty() {
            "upload.bin".to_string()
        } else {
            safe_name
        };
        let dir = "/tmp/gutted-uploads";
        let _ = std::fs::create_dir_all(dir);
        let local_path = format!("{dir}/{target_tab}_{safe_name}");
        if let Err(e) = std::fs::write(&local_path, data) {
            warn!(error = %e, %local_path, "failed to write uploaded file");
            return;
        }
        let b64 = if data.len() <= 2 * 1024 * 1024 {
            base64_encode(data)
        } else {
            String::new()
        };
        info!(tab_id = target_tab, %safe_name, bytes = data.len(), "completing file upload");
        self.runner
            .complete_file_chooser(target_tab, &local_path, &safe_name, mime_type, &b64);
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
        {
            let mut cur = inbuf.as_slice();
            loop {
                match Message::decode(&mut cur) {
                    Ok(Some(msg)) => {
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
                            Message::Clipboard { text } => {
                                host_state.paste_text(&text);
                            }
                            Message::FileUpload { tab_id, filename, mime_type, data } => {
                                host_state.handle_file_upload(tab_id, &filename, &mime_type, &data);
                            }
                            other => warn!(?other, "unexpected input msg"),
                        }
                    }
                    Ok(None) => break,
                    Err(e) => return Err(anyhow!("input decode: {:?}", e)),
                }
            }
            let consumed = inbuf.len() - cur.len();
            if consumed > 0 {
                inbuf.drain(..consumed);
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
    let _ = send.set_priority(20); // High priority for control & navigation messages
    let mut out = Vec::with_capacity(128);
    welcome.encode(&mut out);
    {
        let tabs = host_state.tabs.lock().await;
        let mut sorted: Vec<_> = tabs.iter().collect();
        sorted.sort_by_key(|(id, _)| **id);
        for (&tid, tab) in sorted {
            Message::TabCreated {
                tab_id: tid,
                title: tab.title.clone(),
                url: tab.url.clone(),
            }
            .encode(&mut out);
        }
        let active = host_state.active_tab_id.load(Ordering::Relaxed);
        Message::TabActivated { tab_id: active }.encode(&mut out);
    }
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
        let _ = vs.set_priority(10);   // high priority for smooth video stream
        info!("video uni-stream open");

        // Initial state: send the last full RawFrame
        let mut published = 0u64;
        let mut cum_bytes: u64 = 0;
        let mut cum_full: u64 = 0;
        let mut cum_sub: u64 = 0;
        if let Some(snap_item) = bus_for_video.snapshot() {
            let snap_len = snap_item.wire_quic.len();
            if vs.write_all(&snap_item.wire_quic).await.is_ok() {
                published += 1;
                cum_bytes += snap_len as u64; cum_full += snap_len as u64;
                info!(bytes = snap_len, "initial FULL frame snapshot delivered to new subscriber");
            }
        }

        loop {
            tokio::select! {
                res = stream_rx.recv() => match res {
                    Ok(first) => {
                        let batch = coalesce_bus_batch(first, &mut stream_rx);
                        for item in batch {
                            let n = item.wire_quic.len();
                            if let Err(e) = vs.write_all(&item.wire_quic).await {
                                warn!(error = %e, "video write");
                                return;
                            }
                            published += 1;
                            cum_bytes += n as u64;
                            match item.meta {
                                BusMeta::RawFrame => cum_full += n as u64,
                                BusMeta::Subframe { .. } => cum_sub += n as u64,
                                BusMeta::Other => {}
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(dropped = n, "video subscriber lagged — resyncing via immediate FULL snapshot");
                        if let Some(snap_item) = bus_for_video.snapshot() {
                            let snap_len = snap_item.wire_quic.len();
                            if vs.write_all(&snap_item.wire_quic).await.is_ok() {
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
            Message::Clipboard { text } => {
                host_state.paste_text(&text);
            }
            Message::FileUpload { tab_id, filename, mime_type, data } => {
                host_state.handle_file_upload(tab_id, &filename, &mime_type, &data);
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

async fn read_next_tcp(
    recv: &mut tokio::net::tcp::OwnedReadHalf,
    inbuf: &mut Vec<u8>,
    chunk: &mut [u8],
) -> Result<Option<Message>> {
    use tokio::io::AsyncReadExt;
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
            Ok(0) => {
                return if inbuf.is_empty() {
                    Ok(None)
                } else {
                    Err(anyhow!("tcp stream ended mid-frame ({} bytes)", inbuf.len()))
                };
            }
            Ok(n) => inbuf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset && inbuf.is_empty() => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

async fn handle_tcp_connection(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    host_state: HostState,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let _ = stream.set_nodelay(true);
    info!(%peer, "TCP client connected");

    let (mut recv, mut send) = stream.into_split();
    let mut inbuf = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; 8192];

    let hello = read_next_tcp(&mut recv, &mut inbuf, &mut chunk)
        .await?
        .ok_or_else(|| anyhow!("tcp stream closed before HELLO"))?;
    let Message::Hello {
        proto_version,
        viewport_w,
        viewport_h,
        dpr_hundredths,
        client_name,
        capabilities,
    } = hello
    else {
        return Err(anyhow!("first tcp message was not HELLO: {:?}", hello));
    };
    if proto_version != PROTO_VERSION {
        return Err(anyhow!(
            "proto version mismatch: client={proto_version} host={PROTO_VERSION}"
        ));
    }
    info!(
        %peer,
        client = %client_name,
        viewport = format!("{viewport_w}x{viewport_h}"),
        dpr = format!("{:.2}", dpr_hundredths as f32 / 100.0),
        caps = format!("0x{capabilities:08x}"),
        "TCP HELLO received",
    );
    if viewport_w > 0 && viewport_h > 0 {
        host_state.resize_all(viewport_w as u32, viewport_h as u32).await;
    }

    let session_id: u64 = rand64();
    let welcome = Message::Welcome {
        proto_version: PROTO_VERSION,
        session_id,
        features: capabilities & (caps::H264 | caps::CLIENT_SCROLL),
        cursor_track_id: 0,
        current_url: host_state.current_url().await,
    };
    let mut out = Vec::with_capacity(128);
    welcome.encode(&mut out);
    {
        let tabs = host_state.tabs.lock().await;
        let mut sorted: Vec<_> = tabs.iter().collect();
        sorted.sort_by_key(|(id, _)| **id);
        for (&tid, tab) in sorted {
            Message::TabCreated {
                tab_id: tid,
                title: tab.title.clone(),
                url: tab.url.clone(),
            }
            .encode(&mut out);
        }
        let active = host_state.active_tab_id.load(Ordering::Relaxed);
        Message::TabActivated { tab_id: active }.encode(&mut out);
    }
    send.write_all(&out).await?;

    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    let bus_for_video = host_state.bus.clone();
    let mut stream_rx = host_state.bus.subscribe();
    let writer_task = tokio::spawn(async move {
        if let Some(snap_item) = bus_for_video.snapshot() {
            if send.write_all(&snap_item.wire_quic).await.is_err() {
                return;
            }
        }
        loop {
            tokio::select! {
                Some(b) = reply_rx.recv() => {
                    if send.write_all(&b).await.is_err() {
                        break;
                    }
                }
                res = stream_rx.recv() => match res {
                    Ok(first) => {
                        let batch = coalesce_bus_batch(first, &mut stream_rx);
                        for item in batch {
                            if send.write_all(&item.wire_quic).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(snap_item) = bus_for_video.snapshot() {
                            if send.write_all(&snap_item.wire_quic).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });

    let mut last_pointer: (i32, i32) = (0, 0);
    while let Some(msg) = read_next_tcp(&mut recv, &mut inbuf, &mut chunk).await? {
        match msg {
            Message::Heartbeat { ts_us } => {
                let mut buf = Vec::with_capacity(16);
                Message::Heartbeat { ts_us }.encode(&mut buf);
                let _ = reply_tx.send(Bytes::from(buf)).await;
            }
            Message::Nav { url } => {
                info!(%url, "NAV request (tcp)");
                host_state.load_uri(&url).await;
            }
            Message::Resize { viewport_w, viewport_h, .. } => {
                host_state.resize_all(viewport_w as u32, viewport_h as u32).await;
            }
            Message::SetZoom { level_milli } => {
                host_state.set_zoom((level_milli as f64) / 1000.0).await;
            }
            Message::NavAction { action } => match action {
                0 => host_state.go_back().await,
                1 => host_state.go_forward().await,
                2 => host_state.reload().await,
                _ => {}
            },
            Message::Stop => {
                host_state.stop_loading().await;
            }
            Message::ClearData { clear_cookies, clear_cache, clear_storage } => {
                host_state.clear_data(clear_cookies, clear_cache, clear_storage);
            }
            Message::CreateTab { tab_id, url } => {
                host_state.create_tab(tab_id, &url, true).await;
                let mut buf = Vec::with_capacity(64);
                Message::TabCreated {
                    tab_id,
                    title: "New Tab".into(),
                    url,
                }
                .encode(&mut buf);
                let _ = reply_tx.send(Bytes::from(buf)).await;
            }
            Message::CloseTab { tab_id } => {
                host_state.close_tab(tab_id).await;
                let mut buf = Vec::with_capacity(16);
                Message::TabClosed { tab_id }.encode(&mut buf);
                let _ = reply_tx.send(Bytes::from(buf)).await;
            }
            Message::SwitchTab { tab_id } => {
                host_state.switch_tab(tab_id).await;
                let mut buf = Vec::with_capacity(16);
                Message::TabActivated { tab_id }.encode(&mut buf);
                let _ = reply_tx.send(Bytes::from(buf)).await;
            }
            Message::InputPointer { x, y, modifiers, .. } => {
                last_pointer = (x, y);
                host_state.inject_pointer_motion(x, y, modifiers).await;
            }
            Message::InputButton { x, y, button, pressed, modifiers, .. } => {
                last_pointer = (x, y);
                host_state.inject_pointer_button(x, y, button, pressed, modifiers).await;
            }
            Message::InputKey { keycode, mods, down, .. } => {
                host_state.inject_key(keycode, mods, down).await;
            }
            Message::InputScroll { dx_units, dy_units, .. } => {
                let (px, py) = last_pointer;
                host_state
                    .inject_axis(px, py, dx_units as f64 * 40.0, dy_units as f64 * 40.0, 0)
                    .await;
            }
            Message::Clipboard { text } => {
                host_state.paste_text(&text);
            }
            Message::FileUpload { tab_id, filename, mime_type, data } => {
                host_state.handle_file_upload(tab_id, &filename, &mime_type, &data);
            }
            _ => {}
        }
    }
    writer_task.abort();
    info!(%peer, "TCP client disconnected");
    Ok(())
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
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

    // Aggressive defaults for LAN / Internet. Tune later.
    let mut transport = quinn::TransportConfig::default();
    transport
        .max_concurrent_bidi_streams(128u32.into())
        .max_concurrent_uni_streams(128u32.into())
        .keep_alive_interval(Some(std::time::Duration::from_secs(2)))
        .max_idle_timeout(Some(std::time::Duration::from_secs(30).try_into().unwrap()))
        .datagram_receive_buffer_size(Some(4 * 1024 * 1024))
        .datagram_send_buffer_size(4 * 1024 * 1024)
        .stream_receive_window((16 * 1024 * 1024u32).into())
        .receive_window((32 * 1024 * 1024u32).into())
        .send_window(32 * 1024 * 1024);
    server_cfg.transport_config(Arc::new(transport));

    Ok((server_cfg, cert_der))
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[inline]
fn first_diff_u32(a: &[u32], b: &[u32]) -> Option<usize> {
    let n = a.len().min(b.len());
    let mut i = 0usize;
    while i + 8 <= n {
        if a[i..i + 8] != b[i..i + 8] {
            for j in i..i + 8 {
                if a[j] != b[j] {
                    return Some(j);
                }
            }
        }
        i += 8;
    }
    while i < n {
        if a[i] != b[i] {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[inline]
fn last_diff_u32(a: &[u32], b: &[u32]) -> Option<usize> {
    let mut i = a.len().min(b.len());
    while i >= 8 {
        if a[i - 8..i] != b[i - 8..i] {
            for j in (i - 8..i).rev() {
                if a[j] != b[j] {
                    return Some(j);
                }
            }
        }
        i -= 8;
    }
    while i > 0 {
        i -= 1;
        if a[i] != b[i] {
            return Some(i);
        }
    }
    None
}

/// Update horizontal dirty bounds `[left, right]` for a known-dirty row `(ra, rb)`.
/// Only scans outside the existing `[left, right]` interval once initialized!
#[inline]
fn update_row_bounds(ra: &[u8], rb: &[u8], w: usize, left: &mut usize, right: &mut usize) {
    if *left == 0 && *right + 1 >= w {
        return;
    }
    if let (Some(sa), Some(sb)) = (align_slice_u32(ra), align_slice_u32(rb)) {
        if *left > *right {
            if let Some(lx) = first_diff_u32(sa, sb) {
                *left = lx;
                *right = last_diff_u32(&sa[lx + 1..], &sb[lx + 1..])
                    .map_or(lx, |rx| lx + 1 + rx);
            }
        } else {
            if *left > 0 {
                if let Some(lx) = first_diff_u32(&sa[..*left], &sb[..*left]) {
                    *left = lx;
                }
            }
            if *right + 1 < w {
                if let Some(rx) = last_diff_u32(&sa[*right + 1..], &sb[*right + 1..]) {
                    *right = *right + 1 + rx;
                }
            }
        }
    } else {
        let ca = ra.as_chunks::<4>().0;
        let cb = rb.as_chunks::<4>().0;
        if *left > *right {
            if let Some(lx) = ca.iter().zip(cb.iter()).position(|(pa, pb)| pa != pb) {
                *left = lx;
                *right = ca[lx + 1..]
                    .iter()
                    .zip(cb[lx + 1..].iter())
                    .rposition(|(pa, pb)| pa != pb)
                    .map_or(lx, |rx| lx + 1 + rx);
            }
        } else {
            if *left > 0 {
                if let Some(lx) = ca[..*left].iter().zip(cb[..*left].iter()).position(|(pa, pb)| pa != pb) {
                    *left = lx;
                }
            }
            if *right + 1 < w {
                if let Some(rx) = ca[*right + 1..].iter().zip(cb[*right + 1..].iter()).rposition(|(pa, pb)| pa != pb) {
                    *right = *right + 1 + rx;
                }
            }
        }
    }
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

    for (y, is_dirty_slot) in dirty.iter_mut().enumerate().take(h) {
        let ra = &a[y * s .. y * s + row_bytes];
        let rb = &b[y * s .. y * s + row_bytes];
        let is_d = ra != rb;
        *is_dirty_slot = is_d;
        if is_d {
            dirty_count += 1;
            if first_dirty == h { first_dirty = y; }
            last_dirty = y;
        }
    }
    if dirty_count == 0 {
        return Vec::new();
    }

    // 2) coalesce into runs; tolerate up to `gap` clean rows so we don't
    //    over-fragment when a shape has interior whitespace lines.
    let gap: usize = 8;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut y = first_dirty;
    while y <= last_dirty {
        if !dirty[y] { y += 1; continue; }
        let start = y;
        let mut end = y;
        y += 1;
        while y <= last_dirty {
            if dirty[y] {
                end = y;
                y += 1;
            } else {
                let look = (1..=gap).find(|&k| y + k <= last_dirty && dirty[y + k]);
                if let Some(k) = look {
                    y += k;
                } else {
                    break;
                }
            }
        }
        runs.push((start, end));
    }

    // Fast-path: if >50% of screen rows changed or >8 disjoint runs,
    // compute a single tight horizontal bounding box across all dirty rows.
    if dirty_count * 2 >= h || runs.len() > 8 {
        let mut min_left = w;
        let mut max_right = 0usize;
        for (yy, &is_d) in dirty.iter().enumerate().take(last_dirty + 1).skip(first_dirty) {
            if !is_d { continue; }
            let ra = &a[yy * s .. yy * s + row_bytes];
            let rb = &b[yy * s .. yy * s + row_bytes];
            update_row_bounds(ra, rb, w, &mut min_left, &mut max_right);
            if min_left == 0 && max_right + 1 >= w {
                break;
            }
        }
        if max_right >= min_left {
            return vec![(min_left as u16, first_dirty as u16, (max_right - min_left + 1) as u16, (last_dirty - first_dirty + 1) as u16)];
        } else {
            return vec![(0, first_dirty as u16, w as u16, (last_dirty - first_dirty + 1) as u16)];
        }
    }
    // 3) for each run, compute col bbox.
    let mut out = Vec::with_capacity(runs.len());
    for (top, bot) in runs {
        let mut left  = w;
        let mut right = 0usize;
        for (yy, &is_d) in dirty.iter().enumerate().take(bot + 1).skip(top) {
            if !is_d { continue; }
            let ra = &a[yy * s .. yy * s + row_bytes];
            let rb = &b[yy * s .. yy * s + row_bytes];
            update_row_bounds(ra, rb, w, &mut left, &mut right);
            if left == 0 && right + 1 >= w {
                break;
            }
        }
        if right >= left {
            out.push((left as u16, top as u16,
                      (right - left + 1) as u16, (bot - top + 1) as u16));
        }
    }
    out
}

fn init_pulse_null_sink() {
    use std::process::{Command, Stdio};
    let pactl_ok = Command::new("pactl")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !pactl_ok {
        let _ = Command::new("pulseaudio")
            .args([
                "--daemonize=yes",
                "--exit-idle-time=-1",
                "--disallow-exit",
                "-n",
                "-L",
                "module-null-sink sink_name=gutted_sink sink_properties=device.description=GuttedBrowser",
                "-L",
                "module-native-protocol-unix auth-anonymous=1",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else {
        let sinks = Command::new("pactl")
            .args(["list", "short", "sinks"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        if !sinks.contains("gutted_sink") {
            let _ = Command::new("pactl")
                .args([
                    "load-module",
                    "module-null-sink",
                    "sink_name=gutted_sink",
                    "sink_properties=device.description=GuttedBrowser",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    std::env::set_var("PULSE_SINK", "gutted_sink");
}

fn spawn_audio_capture(bus: FrameBus) {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let _ = std::thread::Builder::new()
        .name("gutted-audio".into())
        .spawn(move || {
            let Ok(mut child) = Command::new("parec")
                .args([
                    "--device=gutted_sink.monitor",
                    "--format=s16le",
                    "--rate=48000",
                    "--channels=2",
                    "--raw",
                    "--latency-msec=10",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            else {
                return;
            };
            let Some(mut stdout) = child.stdout.take() else { return; };
            // 10ms of 48kHz stereo S16LE = 480 frames * 4 bytes = 1920 bytes
            let mut buf = [0u8; 1920];
            loop {
                if stdout.read_exact(&mut buf).is_err() {
                    break;
                }
                if !bus.has_subscribers() {
                    continue;
                }
                // Skip silent/dither-only chunks (<= 2 LSBs) so idle pages consume 0 audio bandwidth
                let (samples, _) = buf.as_chunks::<2>();
                if samples
                    .iter()
                    .all(|&s| (-2..=2).contains(&i16::from_le_bytes(s)))
                {
                    continue;
                }
                let pts_us = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros() as u64;
                bus.publish(Arc::new(Message::AudioFrame {
                    pts_us,
                    codec: 0, // PCM S16LE
                    channels: 2,
                    sample_rate: 48_000,
                    data: buf.to_vec(),
                }));
            }
            let _ = child.kill();
        });
}

#[inline(always)]
fn align_slice_u32(s: &[u8]) -> Option<&[u32]> {
    if (s.as_ptr() as usize).is_multiple_of(std::mem::align_of::<u32>()) && s.len().is_multiple_of(4) {
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
        for chunk in px.as_chunks_mut::<4>().0 { chunk.copy_from_slice(&fill); }
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
