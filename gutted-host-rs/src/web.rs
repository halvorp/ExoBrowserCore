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

    let is_get = first_line.starts_with("GET /") || first_line.starts_with("GET ");
    let is_head = first_line.starts_with("HEAD /") || first_line.starts_with("HEAD ");

    if is_get || is_head {
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

    let mut bus_sub = host_state.bus.subscribe();

    // ─── Initial Handshake over WebSocket ─────────────────────────────────────
    let (ws_out_tx, mut ws_out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    // Initial snapshot of active tab
    if let Some(snap) = host_state.bus.snapshot().await {
        let raw_snap = convert_to_raw_frame(&snap);
        let mut buf = Vec::new();
        raw_snap.encode(&mut buf);
        let _ = ws_out_tx.send(buf);
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

    // Task 1: Forward bus messages to WebSocket client
    let ws_out_tx_clone = ws_out_tx.clone();
    let forward_task = tokio::spawn(async move {
        while let Ok(msg) = bus_sub.recv().await {
            let msg_to_send = convert_to_raw_frame(&msg);
            let mut buf = Vec::new();
            msg_to_send.encode(&mut buf);
            if ws_out_tx_clone.send(buf).is_err() {
                break;
            }
        }
    });

    // Task 2: WebSocket writer loop
    let writer_task = tokio::spawn(async move {
        while let Some(bytes) = ws_out_rx.recv().await {
            if ws_sender.send(WsMessage::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
    });

    // Task 3: Inbound WebSocket reader loop
    let host_state_clone = host_state.clone();
    while let Some(msg_result) = ws_receiver.next().await {
        match msg_result {
            Ok(WsMessage::Binary(data)) => {
                let mut cur = &data[..];
                while let Ok(Some(msg)) = Message::decode(&mut cur) {
                    handle_client_msg(msg, &host_state_clone, &ws_out_tx).await;
                }
            }
            Ok(WsMessage::Ping(p)) => {
                let _ = ws_out_tx.send(p.to_vec());
            }
            Ok(WsMessage::Close(_)) => break,
            Err(e) => {
                tracing::debug!(error = %e, "ws recv error");
                break;
            }
            _ => {}
        }
    }

    forward_task.abort();
    writer_task.abort();
    info!(%peer, "WebSocket client disconnected");
    Ok(())
}

/// Convert ZSTD-compressed frames to uncompressed RAW frames for fast HTML5 Canvas rendering.
fn convert_to_raw_frame(m: &Message) -> Message {
    match m {
        Message::RawFrame { ts_us, width, height, stride, format, pixels, .. } => {
            Message::RawFrame {
                ts_us: *ts_us,
                width: *width,
                height: *height,
                stride: *stride,
                format: *format,
                compression: gutted_proto::compression::RAW,
                pixels: pixels.clone(),
            }
        }
        Message::Subframe { ts_us, x, y, w, h, stride, format, pixels, .. } => {
            Message::Subframe {
                ts_us: *ts_us,
                x: *x,
                y: *y,
                w: *w,
                h: *h,
                stride: *stride,
                format: *format,
                compression: gutted_proto::compression::RAW,
                pixels: pixels.clone(),
            }
        }
        other => other.clone(),
    }
}

async fn handle_client_msg(
    msg: Message,
    host_state: &HostState,
    ws_out_tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
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
            host_state.inject_pointer_motion(x, y, modifiers).await;
        }
        Message::InputButton { x, y, button, pressed, modifiers, .. } => {
            host_state.inject_pointer_button(x, y, button, pressed, modifiers).await;
        }
        Message::InputKey { keycode, mods, down, .. } => {
            host_state.inject_key(keycode, mods, down).await;
        }
        Message::InputScroll { dx_units, dy_units, .. } => {
            let dx = dx_units as f64 * 40.0;
            let dy = dy_units as f64 * 40.0;
            host_state.inject_axis(0, 0, dx, dy, 0).await;
        }
        _ => {}
    }
}
