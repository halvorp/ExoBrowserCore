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
extern crate alloc;

pub mod render;

pub use render::{
    canonicalize_url, AVClock, AudioChunk, BrowserCore, DamageRect, FontRenderer, GfxFrame,
    GfxSubframe, InputEvent, RenderEvent, TabBuffer, TabInfo, BOOKMARKS, BOOKMARK_BAR_H, CHROME_H,
    NAV_BAR_H, TAB_BAR_H,
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

/// Pure-Rust network session over framed TCP (`TCP_NODELAY`).
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

    log_info("gutted_client", &format!("dialing gutted-host server={server}"));
    let t_conn = Instant::now();
    let mut stream = TcpStream::connect_timeout(&server, Duration::from_secs(10))
        .map_err(|e| format!("TCP connect to {server}: {e}"))?;
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
    let mut inbuf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut chunk = vec![0u8; 32 * 1024];
    let welcome = read_next_stream(&mut stream, &mut inbuf, &mut chunk)?
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
        let _ = frames_tx.send(RenderEvent::InitialUrl(current_url));
    }

    if let Ok(url) = std::env::var("GBROWSER_NAV") {
        if !url.is_empty() {
            log_info("gutted_client", &format!("NAV sent (from env) url={url}"));
            let mut nav_buf = Vec::with_capacity(64);
            Message::Nav { url }.encode(&mut nav_buf);
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
            let mut buf = Vec::with_capacity(64);
            while !stop_writer.load(Ordering::Relaxed) {
                let Ok(ev) = input_rx.recv() else {
                    break;
                };
                let ts_us = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros() as u64;
                let msg = ev.into_proto_message(ts_us);
                buf.clear();
                msg.encode(&mut buf);
                if write_stream.write_all(&buf).is_err() {
                    break;
                }
            }
        })?;

    log_info("gutted_client", "video uni-stream accepted");
    let mut frames_full = 0u64;
    let mut frames_sub = 0u64;
    let mut total_bytes = 0u64;
    let mut wire_bytes = inbuf.len() as u64;
    let mut last_report = Instant::now();
    let mut last_wire = 0u64;
    let mut last_frames = 0u64;

    loop {
        {
            let mut cur = inbuf.as_slice();
            loop {
                match Message::decode(&mut cur) {
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
                                "Message::decode error: {e:?} cur_len={} head={:02x?}",
                                cur.len(),
                                &cur[..cur.len().min(32)]
                            ),
                        );
                        break;
                    }
                }
            }
            let consumed = inbuf.len() - cur.len();
            if consumed > 0 {
                inbuf.drain(..consumed);
                if inbuf.is_empty() && inbuf.capacity() > 128 * 1024 {
                    inbuf.shrink_to(64 * 1024);
                }
            }
        }

        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                inbuf.extend_from_slice(&chunk[..n]);
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
                Err(e) => return Err(format!("decode error: {e:?}").into()),
            }
        }
        match recv.read(chunk)? {
            0 => {
                return if inbuf.is_empty() {
                    Ok(None)
                } else {
                    Err("stream ended mid-frame".into())
                }
            }
            n => inbuf.extend_from_slice(&chunk[..n]),
        }
    }
}
