//! web.rs: Built-in HTTP and WebSocket server for gutted-host.
//!
//! Provides a complete, high-performance browser-in-browser experience
//! ("like Neko, but lighter, faster, and multi-tab native").
//!
//! Serves:
//! - `GET /` or `GET /index.html`: Responsive, ultra-modern HTML5/WebGL single-page application.
//! - `GET /ws`: High-speed binary WebSocket stream for video, audio, input, and tab management.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use gutted_proto::{caps, Message, PROTO_VERSION};
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{info, warn};

use crate::HostState;

pub async fn run_web_server(addr: SocketAddr, host_state: HostState) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "ExoBrowser Web Server listening (Open in your browser: http://localhost:{})", addr.port());

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let _ = stream.set_nodelay(true);
                let host_state = host_state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_http_connection(stream, peer, host_state).await {
                        tracing::debug!(%peer, error = %e, "http connection ended");
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "tcp accept error");
            }
        }
    }
}

async fn handle_http_connection(mut stream: TcpStream, peer: SocketAddr, host_state: HostState) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut peek_buf = vec![0u8; 4096];
    let n = stream.peek(&mut peek_buf).await?;
    if n == 0 {
        return Ok(());
    }

    let req_str = String::from_utf8_lossy(&peek_buf[..n]);
    let first_line = req_str.lines().next().unwrap_or("").to_string();
    let is_ws = req_str.to_ascii_lowercase().contains("upgrade: websocket");

    if is_ws && first_line.contains("/ws") {
        let ws_stream = tokio_tungstenite::accept_async(stream).await?;
        handle_websocket(ws_stream, peer, host_state).await?;
        return Ok(());
    }

    // Read full HTTP request header
    let mut discard = vec![0u8; 4096];
    let _ = stream.read(&mut discard).await?;

    let is_favicon = first_line.contains("/favicon.ico");
    let is_get = first_line.starts_with("GET /") || first_line.starts_with("GET ");
    let is_head = first_line.starts_with("HEAD /") || first_line.starts_with("HEAD ");

    if is_favicon {
        let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><text y='.9em' font-size='90'>⚡</text></svg>";
        let header = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: image/svg+xml\r\n\
             Content-Length: {}\r\n\
             Cache-Control: public, max-age=86400\r\n\
             Connection: close\r\n\
             \r\n",
            svg.len()
        );
        stream.write_all(header.as_bytes()).await?;
        if is_get {
            stream.write_all(svg.as_bytes()).await?;
        }
        stream.flush().await?;
    } else if is_get || is_head {
        let html = include_str!("web_app.html");
        let header = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\
             \r\n",
            html.len()
        );
        stream.write_all(header.as_bytes()).await?;
        if is_get {
            stream.write_all(html.as_bytes()).await?;
        }
        stream.flush().await?;
    } else {
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        stream.write_all(response.as_bytes()).await?;
        stream.flush().await?;
    }

    Ok(())
}

async fn handle_websocket(
    ws_stream: tokio_tungstenite::WebSocketStream<TcpStream>,
    peer: SocketAddr,
    host_state: HostState,
) -> Result<()> {
    info!(%peer, "WebSocket client connected");
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // ─── Initial Handshake over WebSocket ─────────────────────────────────────
    let (ws_out_tx, mut ws_out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    // Initial snapshot of active tab
    let mut bus_sub = host_state.bus.subscribe();
    if let Some(snap) = host_state.bus.snapshot() {
        let _ = ws_out_tx.send(encode_ws_item(&snap));
    }

    // Current URL & Title
    let cur_url = host_state.current_url().await;
    if !cur_url.is_empty() {
        let mut b = Vec::new();
        Message::UrlChanged { url: cur_url }.encode(&mut b);
        let _ = ws_out_tx.send(b);
    }

    // Active tabs list
    {
        let tabs = host_state.tabs.lock().await;
        for (tid, tab) in tabs.iter() {
            let mut b = Vec::new();
            Message::TabCreated {
                tab_id: *tid,
                title: tab.title.clone(),
                url: tab.url.clone(),
            }
            .encode(&mut b);
            let _ = ws_out_tx.send(b);
        }
    }

    // Task 1: Forward bus messages + replies directly to WebSocket sender with backpressure coalescing
    let host_state_bus = host_state.clone();
    let writer_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(bytes) = ws_out_rx.recv() => {
                    if ws_sender.send(WsMessage::Binary(bytes)).await.is_err() {
                        break;
                    }
                }
                res = bus_sub.recv() => match res {
                    Ok(mut item) => {
                        while let Ok(newer) = bus_sub.try_recv() {
                            match (&*item.msg, &*newer.msg) {
                                (Message::RawFrame { .. } | Message::Subframe { .. }, Message::RawFrame { .. }) => {
                                    item = newer;
                                }
                                (
                                    Message::Subframe { x: x1, y: y1, w: w1, h: h1, .. },
                                    Message::Subframe { x: x2, y: y2, w: w2, h: h2, .. },
                                ) if *x2 <= *x1
                                    && *y2 <= *y1
                                    && (*x2 as u32 + *w2 as u32) >= (*x1 as u32 + *w1 as u32)
                                    && (*y2 as u32 + *h2 as u32) >= (*y1 as u32 + *h1 as u32) =>
                                {
                                    item = newer;
                                }
                                _ => {
                                    if ws_sender.send(WsMessage::Binary(encode_ws_item(&item))).await.is_err() {
                                        return;
                                    }
                                    item = newer;
                                }
                            }
                        }
                        if ws_sender.send(WsMessage::Binary(encode_ws_item(&item))).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(snap_item) = host_state_bus.bus.snapshot() {
                            if ws_sender.send(WsMessage::Binary(encode_ws_item(&snap_item))).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });

    // Task 3: Inbound WebSocket reader loop
    let host_state_clone = host_state.clone();
    let mut last_pointer: (i32, i32) = (0, 0);
    while let Some(msg_result) = ws_receiver.next().await {
        match msg_result {
            Ok(WsMessage::Binary(data)) => {
                let mut cur = &data[..];
                while let Ok(Some(msg)) = Message::decode(&mut cur) {
                    handle_client_msg(msg, &host_state_clone, &ws_out_tx, &mut last_pointer).await;
                }
            }
            Ok(WsMessage::Close(_)) => break,
            Err(e) => {
                tracing::debug!(error = %e, "ws recv error");
                break;
            }
            _ => {}
        }
    }

    writer_task.abort();
    info!(%peer, "WebSocket client disconnected");
    Ok(())
}

fn write_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Encode a `BusItem` for the WebSocket client without cloning pixel buffers.
/// Frame payloads are emitted uncompressed (`compression::RAW`) for zero-overhead
/// HTML5 Canvas blitting in JS.
fn encode_ws_item(item: &crate::BusItem) -> Vec<u8> {
    match &*item.msg {
        Message::RawFrame { ts_us, width, height, stride, format, pixels, .. } => {
            let payload_len = 21 + pixels.len();
            let mut out = Vec::with_capacity(8 + payload_len);
            write_varint(&mut out, gutted_proto::tag::RAW_FRAME);
            write_varint(&mut out, payload_len as u32);
            out.extend_from_slice(&ts_us.to_le_bytes());
            out.extend_from_slice(&width.to_le_bytes());
            out.extend_from_slice(&height.to_le_bytes());
            out.extend_from_slice(&stride.to_le_bytes());
            out.extend_from_slice(&format.to_le_bytes());
            out.push(gutted_proto::compression::RAW);
            out.extend_from_slice(pixels);
            out
        }
        Message::Subframe { ts_us, x, y, w, h, stride, format, pixels, .. } => {
            let payload_len = 25 + pixels.len();
            let mut out = Vec::with_capacity(8 + payload_len);
            write_varint(&mut out, gutted_proto::tag::SUBFRAME);
            write_varint(&mut out, payload_len as u32);
            out.extend_from_slice(&ts_us.to_le_bytes());
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&w.to_le_bytes());
            out.extend_from_slice(&h.to_le_bytes());
            out.extend_from_slice(&stride.to_le_bytes());
            out.extend_from_slice(&format.to_le_bytes());
            out.push(gutted_proto::compression::RAW);
            out.extend_from_slice(pixels);
            out
        }
        _ => item.wire_quic.to_vec(),
    }
}

async fn handle_client_msg(
    msg: Message,
    host_state: &HostState,
    ws_out_tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    last_pointer: &mut (i32, i32),
) {
    match msg {
        Message::Hello { viewport_w, viewport_h, .. } => {
            if viewport_w > 0 && viewport_h > 0 {
                host_state.resize_all(viewport_w as u32, viewport_h as u32).await;
            }
            let welcome = Message::Welcome {
                proto_version: PROTO_VERSION,
                session_id: 0xCAFE_BABE_0000_0001,
                features: caps::CLIENT_SCROLL,
                cursor_track_id: 0,
                current_url: host_state.current_url().await,
            };
            let mut b = Vec::new();
            welcome.encode(&mut b);
            let _ = ws_out_tx.send(b);
        }
        Message::Nav { url } => {
            host_state.load_uri(&url).await;
        }
        Message::Resize { viewport_w, viewport_h, .. } => {
            host_state.resize_all(viewport_w as u32, viewport_h as u32).await;
        }
        Message::SetZoom { level_milli } => {
            let level = (level_milli as f64) / 1000.0;
            host_state.set_zoom(level).await;
        }
        Message::NavAction { action } => {
            match action {
                0 => host_state.go_back().await,
                1 => host_state.go_forward().await,
                2 => host_state.reload().await,
                _ => {}
            }
        }
        Message::Stop => {
            host_state.stop_loading().await;
        }
        Message::CreateTab { tab_id, url } => {
            host_state.create_tab(tab_id, &url, true).await;
            let resp = Message::TabCreated {
                tab_id,
                title: "New Tab".into(),
                url: url.clone(),
            };
            let mut b = Vec::new();
            resp.encode(&mut b);
            let _ = ws_out_tx.send(b);
        }
        Message::CloseTab { tab_id } => {
            host_state.close_tab(tab_id).await;
            let resp = Message::TabClosed { tab_id };
            let mut b = Vec::new();
            resp.encode(&mut b);
            let _ = ws_out_tx.send(b);
        }
        Message::SwitchTab { tab_id } => {
            host_state.switch_tab(tab_id).await;
            let resp = Message::TabActivated { tab_id };
            let mut b = Vec::new();
            resp.encode(&mut b);
            let _ = ws_out_tx.send(b);
        }
        Message::ClearData { clear_cookies, clear_cache, clear_storage } => {
            host_state.clear_data(clear_cookies, clear_cache, clear_storage);
        }
        Message::InputPointer { x, y, modifiers, .. } => {
            *last_pointer = (x, y);
            host_state.inject_pointer_motion(x, y, modifiers).await;
        }
        Message::InputButton { x, y, button, pressed, modifiers, .. } => {
            *last_pointer = (x, y);
            host_state.inject_pointer_button(x, y, button, pressed, modifiers).await;
        }
        Message::InputKey { keycode, mods, down, .. } => {
            host_state.inject_key(keycode, mods, down).await;
        }
        Message::InputScroll { dx_units, dy_units, .. } => {
            let dx = dx_units as f64 * 40.0;
            let dy = dy_units as f64 * 40.0;
            let (px, py) = *last_pointer;
            host_state.inject_axis(px, py, dx, dy, 0).await;
        }
        _ => {}
    }
}
