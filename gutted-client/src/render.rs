//! Portable 100% Pure-Rust `#![no_std]` + `alloc` Browser Engine & Software Compositor.
//!
//! Zero C dependencies (`0` `cc`, `0` `pkg-config`, `0` `libc`, `0` `relibc` in `no_std`):
//! - [`BrowserCore`]: Embeddable, OS-independent `#![no_std]` + `alloc` browser state
//!   machine, protocol transceiver (`encode_hello`, `ingest_wire_bytes`,
//!   `encode_outbound_messages`), and UI chrome + framebuffer compositor (`render_into`)
//!   that renders into any `&mut [u32]` (`0x00RRGGBB`) slice and returns tight [`DamageRect`]s.
//! - **Linux / Unix Backend (`std`)**: 100% Pure-Rust X11 (`x11rb::rust_connection::RustConnection`)
//!   with partial sub-rectangle `put_image` updates + headless fallback.
//! - **Redox OS Backend (`std`)**: 100% Pure-Rust Orbital (`orbital:` scheme)
//!   window & `audio:` scheme + headless fallback.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use gutted_proto::{caps, CursorShape, Message, ScrollPhase, PROTO_VERSION};

#[cfg(feature = "std")]
use crate::{log_info, Result};
#[cfg(feature = "std")]
use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
#[cfg(feature = "std")]
use std::io::Write;
#[cfg(feature = "std")]
use std::process::{Child, ChildStdin, Command, Stdio};
#[cfg(feature = "std")]
use std::time::{Duration, Instant};

pub const TAB_BAR_H: u32 = 32;
pub const NAV_BAR_H: u32 = 38;
pub const BOOKMARK_BAR_H: u32 = 26;
pub const CHROME_H: u32 = TAB_BAR_H + NAV_BAR_H + BOOKMARK_BAR_H;

/// One full frame handed off from the network stream to [`BrowserCore`].
#[derive(Debug, Clone)]
pub struct GfxFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixels: Vec<u8>,
}

/// A sub-rect update — blit at `(x, y)` inside the existing tab framebuffer.
#[derive(Debug, Clone)]
pub struct GfxSubframe {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub stride: u32,
    pub pixels: Vec<u8>,
}

/// Decoded 16-bit signed little-endian PCM audio chunk queued in [`BrowserCore`] for `#![no_std]` audio drivers.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub pts_us: u64,
    pub sample_rate: u32,
    pub channels: u8,
    pub pcm: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum RenderEvent {
    Frame(GfxFrame),
    Subframe(GfxSubframe),
    TileData {
        hash: u64,
        pixels: Vec<u8>,
    },
    TileRef {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        hash: u64,
    },
    Audio {
        pts_us: u64,
        channels: u8,
        sample_rate: u32,
        data: Vec<u8>,
    },
    CursorShape(CursorShape),
    LoadState(u8),
    InitialUrl(String),
    Title(String),
    UrlChanged(String),
    TabCreated {
        tab_id: u32,
        title: String,
        url: String,
    },
    TabClosed {
        tab_id: u32,
    },
    TabActivated {
        tab_id: u32,
    },
    TabTitle {
        tab_id: u32,
        title: String,
    },
    TabUrl {
        tab_id: u32,
        url: String,
    },
    Quit,
}

impl RenderEvent {
    /// Convert a [`gutted_proto::Message`] from the host into a [`RenderEvent`].
    pub fn from_proto_message(msg: Message) -> Option<Self> {
        match msg {
            Message::Welcome { current_url, .. } => {
                if current_url.is_empty() {
                    None
                } else {
                    Some(Self::InitialUrl(current_url))
                }
            }
            Message::RawFrame {
                width,
                height,
                stride,
                pixels,
                ..
            } => Some(Self::Frame(GfxFrame {
                width: width as u32,
                height: height as u32,
                stride,
                pixels,
            })),
            Message::Subframe {
                x,
                y,
                w,
                h,
                stride,
                pixels,
                ..
            } => Some(Self::Subframe(GfxSubframe {
                x: x as u32,
                y: y as u32,
                w: w as u32,
                h: h as u32,
                stride,
                pixels,
            })),
            Message::TileData { hash, pixels, .. } => Some(Self::TileData { hash, pixels }),
            Message::TileRef {
                x, y, w, h, hash, ..
            } => Some(Self::TileRef {
                x: x as u32,
                y: y as u32,
                w: w as u32,
                h: h as u32,
                hash,
            }),
            Message::AudioFrame {
                pts_us,
                channels,
                sample_rate,
                data,
                ..
            } => Some(Self::Audio {
                pts_us,
                channels,
                sample_rate,
                data,
            }),
            Message::CursorState { shape, .. } => Some(Self::CursorShape(shape)),
            Message::LoadState { state } => Some(Self::LoadState(state)),
            Message::Title { title } => Some(Self::Title(title)),
            Message::UrlChanged { url } => Some(Self::UrlChanged(url)),
            Message::TabCreated { tab_id, title, url } => {
                Some(Self::TabCreated { tab_id, title, url })
            }
            Message::TabClosed { tab_id } => Some(Self::TabClosed { tab_id }),
            Message::TabActivated { tab_id } => Some(Self::TabActivated { tab_id }),
            Message::TabTitle { tab_id, title } => Some(Self::TabTitle { tab_id, title }),
            Message::TabUrl { tab_id, url } => Some(Self::TabUrl { tab_id, url }),
            _ => None,
        }
    }
}

/// Outbound events from [`BrowserCore`] to the network session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Motion {
        x: i32,
        y: i32,
        mods: u32,
    },
    Button {
        x: i32,
        y: i32,
        button: u32,
        pressed: bool,
        mods: u32,
    },
    Key {
        keysym: u32,
        mods: u32,
        pressed: bool,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    Navigate(String),
    Resize {
        w: u16,
        h: u16,
    },
    SetZoom {
        level_milli: u32,
    },
    NavAction {
        action: u8,
    },
    Stop,
    CreateTab {
        tab_id: u32,
        url: String,
    },
    CloseTab {
        tab_id: u32,
    },
    SwitchTab {
        tab_id: u32,
    },
    ClearData {
        clear_cookies: bool,
        clear_cache: bool,
        clear_storage: bool,
    },
}

impl InputEvent {
    /// Convert this [`InputEvent`] into a wire-ready [`gutted_proto::Message`].
    pub fn into_proto_message(self, ts_us: u64) -> Message {
        match self {
            Self::Navigate(url) => Message::Nav { url },
            Self::Resize { w, h } => Message::Resize {
                viewport_w: w,
                viewport_h: h,
                dpr_hundredths: 100,
            },
            Self::SetZoom { level_milli } => Message::SetZoom { level_milli },
            Self::NavAction { action } => Message::NavAction { action },
            Self::Stop => Message::Stop,
            Self::CreateTab { tab_id, url } => Message::CreateTab { tab_id, url },
            Self::CloseTab { tab_id } => Message::CloseTab { tab_id },
            Self::SwitchTab { tab_id } => Message::SwitchTab { tab_id },
            Self::ClearData {
                clear_cookies,
                clear_cache,
                clear_storage,
            } => Message::ClearData {
                clear_cookies,
                clear_cache,
                clear_storage,
            },
            Self::Motion { x, y, mods } => Message::InputPointer {
                ts_us,
                x,
                y,
                modifiers: mods,
            },
            Self::Button {
                x,
                y,
                button,
                pressed,
                mods,
            } => Message::InputButton {
                ts_us,
                x,
                y,
                button,
                pressed,
                modifiers: mods,
            },
            Self::Key {
                keysym,
                mods,
                pressed,
            } => Message::InputKey {
                ts_us,
                keycode: keysym,
                mods,
                down: pressed,
            },
            Self::Scroll { dx, dy } => Message::InputScroll {
                ts_us,
                layer_id: 0,
                dx_units: dx,
                dy_units: dy,
                phase: ScrollPhase::Update,
            },
        }
    }
}

pub const BOOKMARKS: &[(&str, &str)] = &[
    ("YouTube", "https://www.youtube.com"),
    ("HackerNews", "https://news.ycombinator.com"),
    ("Wikipedia", "https://www.wikipedia.org"),
    ("DuckDuckGo", "https://duckduckgo.com"),
    ("Reddit", "https://www.reddit.com"),
    ("GitHub", "https://github.com"),
    ("Rust", "https://www.rust-lang.org"),
    ("Example", "https://example.com"),
];

/// Canonicalize user input: valid URL/scheme passes through, localhost/IP gets http://,
/// search queries go to DuckDuckGo, bare domains get https://.
pub fn canonicalize_url(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return "about:blank".into();
    }
    if s.contains(' ') {
        let query = s.replace(' ', "+");
        return format!("https://duckduckgo.com/?q={query}");
    }
    if s.starts_with("localhost:")
        || s.starts_with("localhost/")
        || s == "localhost"
        || s.starts_with("127.0.0.1:")
        || s.starts_with("127.0.0.1/")
        || s == "127.0.0.1"
    {
        return format!("http://{s}");
    }
    if let Some(colon) = s.find(':') {
        let scheme = &s[..colon];
        let rest = &s[colon + 1..];
        let port_part = rest.split('/').next().unwrap_or("");
        let is_port = !port_part.is_empty() && port_part.chars().all(|c| c.is_ascii_digit());
        let scheme_ok = colon > 0
            && !is_port
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
        if scheme_ok {
            return s.into();
        }
    }
    if !s.contains('.') {
        return format!("https://duckduckgo.com/?q={s}");
    }
    format!("https://{s}")
}

/// `#![no_std]` A/V Synchronization Clock.
#[derive(Debug, Default)]
pub struct AVClock {
    master_audio_pts: u64,
    #[cfg(feature = "std")]
    last_audio_tick: Option<Instant>,
}

impl AVClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_audio_frame(&mut self, pts_us: u64, duration_us: u64) {
        self.master_audio_pts = pts_us + duration_us;
        #[cfg(feature = "std")]
        {
            self.last_audio_tick = Some(Instant::now());
        }
    }

    pub fn current_master_pts_us(&self) -> u64 {
        #[cfg(feature = "std")]
        {
            match self.last_audio_tick {
                Some(t) => self.master_audio_pts + (t.elapsed().as_micros() as u64),
                None => self.master_audio_pts,
            }
        }
        #[cfg(not(feature = "std"))]
        {
            self.master_audio_pts
        }
    }

    /// 0 = Present Now, 1 = Hold, 2 = Drop (Late)
    pub fn schedule_video(&self, video_pts_us: u64) -> u8 {
        let master = self.current_master_pts_us();
        if master == 0 {
            return 0;
        }
        let drift = (video_pts_us as i64) - (master as i64);
        if drift < -45_000 {
            2
        } else if drift > 15_000 {
            1
        } else {
            0
        }
    }
}

/// Real-time 48kHz stereo PCM audio output engine (`std`).
#[cfg(feature = "std")]
pub struct AudioOutput {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    #[cfg(target_os = "redox")]
    redox_audio: Option<std::fs::File>,
    rate: u32,
    channels: u8,
    pub muted: bool,
    pub last_active: Option<Instant>,
}

#[cfg(feature = "std")]
impl Default for AudioOutput {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "std")]
impl AudioOutput {
    pub fn new() -> Self {
        Self {
            child: None,
            stdin: None,
            #[cfg(target_os = "redox")]
            redox_audio: None,
            rate: 48_000,
            channels: 2,
            muted: false,
            last_active: None,
        }
    }

    fn ensure_sink(&mut self, rate: u32, channels: u8) {
        #[cfg(target_os = "redox")]
        if self.redox_audio.is_some() {
            return;
        }
        if self.stdin.is_some() && self.rate == rate && self.channels == channels {
            return;
        }
        self.close();
        self.rate = rate;
        self.channels = channels;

        #[cfg(target_os = "redox")]
        {
            if let Ok(file) = std::fs::OpenOptions::new().write(true).open("audio:") {
                log_info(
                    "gutted_client::render",
                    "audio output sink initialized backend=\"redox-audio:\"",
                );
                self.redox_audio = Some(file);
                return;
            }
        }

        let rate_s = rate.to_string();
        let ch_s = channels.to_string();
        let candidates: &[(&str, &[&str])] = &[
            (
                "pw-play",
                &[
                    "--rate",
                    &rate_s,
                    "--channels",
                    &ch_s,
                    "--format",
                    "s16",
                    "--latency",
                    "20ms",
                    "-",
                ],
            ),
            (
                "pacat",
                &[
                    "--playback",
                    "--raw",
                    "--rate",
                    &rate_s,
                    "--channels",
                    &ch_s,
                    "--format=s16le",
                    "--latency-msec=20",
                ],
            ),
            (
                "aplay",
                &[
                    "-q",
                    "-t",
                    "raw",
                    "-f",
                    "S16_LE",
                    "-r",
                    &rate_s,
                    "-c",
                    &ch_s,
                    "-",
                ],
            ),
        ];

        for (bin, args) in candidates {
            if let Ok(mut child) = Command::new(bin)
                .args(*args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                if let Some(stdin) = child.stdin.take() {
                    log_info(
                        "gutted_client::render",
                        &format!(
                            "audio output sink initialized backend=\"{bin}\" rate={rate} channels={channels}"
                        ),
                    );
                    self.stdin = Some(stdin);
                    self.child = Some(child);
                    return;
                }
            }
        }
    }

    pub fn write_pcm(&mut self, rate: u32, channels: u8, pcm: &[u8]) {
        self.last_active = Some(Instant::now());
        if self.muted || pcm.is_empty() {
            return;
        }
        self.ensure_sink(rate, channels);
        #[cfg(target_os = "redox")]
        if let Some(f) = self.redox_audio.as_mut() {
            if f.write_all(pcm).is_err() {
                self.redox_audio = None;
            }
            return;
        }
        if let Some(stdin) = self.stdin.as_mut() {
            if stdin.write_all(pcm).is_err() {
                self.close();
            }
        }
    }

    pub fn is_playing(&self) -> bool {
        self.last_active
            .is_some_and(|t| t.elapsed() < Duration::from_millis(600))
    }

    pub fn close(&mut self) {
        self.stdin.take();
        #[cfg(target_os = "redox")]
        {
            self.redox_audio.take();
        }
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[cfg(feature = "std")]
impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Clone, Debug)]
pub struct TabInfo {
    pub id: u32,
    pub title: String,
    pub url: String,
    pub is_loading: bool,
    pub is_secure: bool,
}

#[derive(Clone, Debug)]
pub struct TabBuffer {
    pub w: u32,
    pub h: u32,
    /// `0x00RRGGBB` pixel buffer (matches WPE BGRA8 little-endian layout directly!)
    pub pixels: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Anti-aliased TTF font renderer (`std`) with built-in `#![no_std]` `font8x8` bitmap fallback.
pub struct FontRenderer {
    #[cfg(feature = "std")]
    ttf: std::sync::OnceLock<Option<FontVec>>,
}

impl Default for FontRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl FontRenderer {
    pub fn new() -> Self {
        #[cfg(feature = "std")]
        {
            Self {
                ttf: std::sync::OnceLock::new(),
            }
        }
        #[cfg(not(feature = "std"))]
        {
            Self {}
        }
    }

    #[cfg(feature = "std")]
    fn get_ttf(&self) -> Option<&FontVec> {
        self.ttf
            .get_or_init(|| {
                let font_paths = [
                    "/usr/share/fonts/chromeos/roboto/Roboto-Regular.ttf",
                    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
                    "/usr/share/fonts/TTF/DejaVuSans.ttf",
                    "/ui/fonts/Mono/Fira/Regular.ttf",
                    "/ui/fonts/Sans/Fira/Regular.ttf",
                ];
                for p in font_paths {
                    if let Ok(bytes) = std::fs::read(p) {
                        if let Ok(f) = FontVec::try_from_vec(bytes) {
                            return Some(f);
                        }
                    }
                }
                None
            })
            .as_ref()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_text(
        &self,
        buf: &mut [u32],
        buf_w: u32,
        buf_h: u32,
        x: i32,
        y: i32,
        _size_px: f32,
        color: u32,
        text: &str,
        max_w: i32,
    ) -> i32 {
        #[cfg(feature = "std")]
        if let Some(font) = self.get_ttf() {
            let scale = PxScale::from(_size_px);
            let scaled = font.as_scaled(scale);
            let ascent = scaled.ascent();
            let mut pen_x = x as f32;
            let baseline_y = y as f32 + ascent;
            let limit_x = (x + max_w) as f32;
            let fg_r = (color >> 16) & 0xFF;
            let fg_g = (color >> 8) & 0xFF;
            let fg_b = color & 0xFF;

            for ch in text.chars() {
                let glyph_id = scaled.glyph_id(ch);
                let adv = scaled.h_advance(glyph_id);
                if pen_x + adv > limit_x {
                    break;
                }
                let glyph =
                    glyph_id.with_scale_and_position(scale, ab_glyph::point(pen_x, baseline_y));
                if let Some(outlined) = font.outline_glyph(glyph) {
                    let bounds = outlined.px_bounds();
                    outlined.draw(|gx, gy, cov| {
                        let a = (cov * 255.0) as u32;
                        if a < 12 {
                            return;
                        }
                        let px = bounds.min.x as i32 + gx as i32;
                        let py = bounds.min.y as i32 + gy as i32;
                        if px >= 0 && (px as u32) < buf_w && py >= 0 && (py as u32) < buf_h {
                            let idx = (py as u32 * buf_w + px as u32) as usize;
                            if a >= 245 {
                                buf[idx] = color;
                            } else {
                                let bg = buf[idx];
                                let inv_a = 255 - a;
                                let r = (fg_r * a + ((bg >> 16) & 0xFF) * inv_a) / 255;
                                let g = (fg_g * a + ((bg >> 8) & 0xFF) * inv_a) / 255;
                                let b = (fg_b * a + (bg & 0xFF) * inv_a) / 255;
                                buf[idx] = (r << 16) | (g << 8) | b;
                            }
                        }
                    });
                }
                pen_x += adv;
            }
            return (pen_x - x as f32) as i32;
        }

        use font8x8::UnicodeFonts;
        let mut pen_x = x;
        for ch in text.chars() {
            if pen_x + 8 > x + max_w {
                break;
            }
            let glyph = font8x8::BASIC_FONTS.get(ch).unwrap_or([0; 8]);
            for (row, &bits) in glyph.iter().enumerate() {
                let py = y + 3 + row as i32;
                if py < 0 || (py as u32) >= buf_h {
                    continue;
                }
                for col in 0..8 {
                    if bits & (1 << col) != 0 {
                        let px = pen_x + col;
                        if px >= 0 && (px as u32) < buf_w {
                            buf[(py as u32 * buf_w + px as u32) as usize] = color;
                        }
                    }
                }
            }
            pen_x += 8;
        }
        pen_x - x
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub fn fill_rect(buf: &mut [u32], bw: u32, bh: u32, x: i32, y: i32, w: i32, h: i32, color: u32) {
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = (x + w).max(0).min(bw as i32) as u32;
    let y1 = (y + h).max(0).min(bh as i32) as u32;
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for row in y0..y1 {
        let start = (row * bw + x0) as usize;
        let end = (row * bw + x1) as usize;
        buf[start..end].fill(color);
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub fn fill_rounded_rect(
    buf: &mut [u32],
    bw: u32,
    bh: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
    color: u32,
) {
    if r <= 1 {
        fill_rect(buf, bw, bh, x, y, w, h, color);
        return;
    }
    for dy in 0..h {
        let py = y + dy;
        if py < 0 || (py as u32) >= bh {
            continue;
        }
        let inset = if dy < r {
            let d = r - dy - 1;
            r - ((r * r - d * d).max(0) as u32).isqrt() as i32
        } else if dy >= h - r {
            let d = dy - (h - r);
            r - ((r * r - d * d).max(0) as u32).isqrt() as i32
        } else {
            0
        };
        let x0 = (x + inset).max(0) as u32;
        let x1 = (x + w - inset).max(0).min(bw as i32) as u32;
        if x0 < x1 {
            let row_off = (py as u32 * bw) as usize;
            buf[row_off + x0 as usize..row_off + x1 as usize].fill(color);
        }
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub fn stroke_rounded_rect(
    buf: &mut [u32],
    bw: u32,
    bh: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
) {
    fill_rect(buf, bw, bh, x + 2, y, w - 4, 1, color);
    fill_rect(buf, bw, bh, x + 2, y + h - 1, w - 4, 1, color);
    fill_rect(buf, bw, bh, x, y + 2, 1, h - 4, color);
    fill_rect(buf, bw, bh, x + w - 1, y + 2, 1, h - 4, color);
}

// WPE modifier bit positions
pub const MOD_CTRL: u32 = 1 << 0;
pub const MOD_SHIFT: u32 = 1 << 1;
pub const MOD_ALT: u32 = 1 << 2;
pub const MOD_META: u32 = 1 << 3;
pub const MOD_BUTTON1: u32 = 1 << 20;
pub const MOD_BUTTON2: u32 = 1 << 21;
pub const MOD_BUTTON3: u32 = 1 << 22;

/// Convert a byte slice of little-endian BGRA8 pixels into `0x00RRGGBB` `u32` words.
#[inline(always)]
fn copy_bgra_row_to_u32(src_bgra: &[u8], dst_u32: &mut [u32]) {
    let n = dst_u32.len().min(src_bgra.len() / 4);
    let (chunks, _) = src_bgra[..n * 4].as_chunks::<4>();
    for (px, dst) in chunks.iter().zip(&mut dst_u32[..n]) {
        *dst = u32::from_le_bytes([px[0], px[1], px[2], 0]);
    }
}

/// Portable, OS-independent `#![no_std]` + `alloc` Pure-Rust browser engine & software compositor.
///
/// Can be embedded into any bare-metal kernel, Redox OS Orbital app, or raw
/// framebuffer (`&mut [u32]`).
pub struct BrowserCore {
    pub win_size: (u32, u32),
    pub font: FontRenderer,
    pub outbound_inputs: Vec<InputEvent>,
    #[cfg(feature = "std")]
    pub input_tx: Option<std::sync::mpsc::Sender<InputEvent>>,

    pub tabs: Vec<TabInfo>,
    pub active_tab_id: u32,
    pub next_tab_id: u32,
    pub active_buf: Option<TabBuffer>,
    pub tab_composites: BTreeMap<u32, TabBuffer>,
    pub tile_cache: BTreeMap<u64, Vec<u32>>,

    pub cursor: (i32, i32),
    pub cursor_shape: CursorShape,
    pub window_title: String,
    pub mods: u32,
    pub url_buf: String,
    pub editing: bool,
    pub is_loading: bool,
    pub zoom_milli: u32,
    pub status_toast_text: Option<String>,
    pub status_toast_frames_left: u32,

    pub chrome_dirty: bool,
    pub full_content_dirty: bool,
    pub pending_damage: Vec<DamageRect>,
    pub pending_frames: Vec<GfxFrame>,
    pub pending_subframes: Vec<GfxSubframe>,
    pub pending_tiles: Vec<(u32, u32, u64)>,

    pub audio_muted: bool,
    pub audio_playing_frames_left: u32,
    pub pending_audio: Vec<AudioChunk>,
    #[cfg(feature = "std")]
    pub audio_out: AudioOutput,
    pub av_clock: AVClock,

    #[cfg(feature = "std")]
    pub capture_to: Option<std::path::PathBuf>,
    pub capture_after_frames: u64,
    pub frames_seen: u64,
    pub synth_cursor: Option<(i32, i32)>,
}

impl BrowserCore {
    /// Construct a `#![no_std]` + `alloc` [`BrowserCore`] instance with no OS channels or files.
    pub fn new_no_std(width: u32, height: u32, initial_url: String, editing: bool) -> Self {
        Self {
            win_size: (width, height),
            font: FontRenderer::new(),
            outbound_inputs: Vec::with_capacity(16),
            #[cfg(feature = "std")]
            input_tx: None,
            tabs: vec![TabInfo {
                id: 1,
                title: "New Tab".into(),
                url: initial_url.clone(),
                is_loading: false,
                is_secure: initial_url.starts_with("https://"),
            }],
            active_tab_id: 1,
            next_tab_id: 2,
            active_buf: None,
            tab_composites: BTreeMap::new(),
            tile_cache: BTreeMap::new(),
            cursor: (0, 0),
            cursor_shape: CursorShape::Default,
            window_title: "ExoBrowser".into(),
            mods: 0,
            url_buf: initial_url,
            editing,
            is_loading: false,
            zoom_milli: 1000,
            status_toast_text: None,
            status_toast_frames_left: 0,
            chrome_dirty: true,
            full_content_dirty: true,
            pending_damage: Vec::with_capacity(16),
            pending_frames: Vec::with_capacity(2),
            pending_subframes: Vec::with_capacity(8),
            pending_tiles: Vec::new(),
            audio_muted: false,
            audio_playing_frames_left: 0,
            pending_audio: Vec::new(),
            #[cfg(feature = "std")]
            audio_out: AudioOutput::new(),
            av_clock: AVClock::new(),
            #[cfg(feature = "std")]
            capture_to: None,
            capture_after_frames: 2,
            frames_seen: 0,
            synth_cursor: None,
        }
    }

    /// Construct a [`BrowserCore`] wired to an outbound `std::sync::mpsc::Sender<InputEvent>`.
    #[cfg(feature = "std")]
    pub fn new(
        width: u32,
        height: u32,
        initial_url: String,
        editing: bool,
        input_tx: std::sync::mpsc::Sender<InputEvent>,
    ) -> Self {
        let mut core = Self::new_no_std(width, height, initial_url, editing);
        core.input_tx = Some(input_tx);
        core.capture_to = std::env::var("GBROWSER_SCREENSHOT")
            .ok()
            .map(std::path::PathBuf::from);
        core.capture_after_frames = std::env::var("GBROWSER_SCREENSHOT_AFTER")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2);
        core.synth_cursor = std::env::var("GBROWSER_SYNTH_CURSOR").ok().and_then(|s| {
            let (x, y) = s.split_once(',')?;
            Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
        });
        core
    }

    #[inline]
    fn emit_input(&mut self, ev: InputEvent) {
        #[cfg(feature = "std")]
        if let Some(tx) = &self.input_tx {
            let _ = tx.send(ev.clone());
        }
        self.outbound_inputs.push(ev);
    }

    /// Encode a `Message::Hello` handshake packet for this client's current viewport into `out`.
    pub fn encode_hello(&self, out: &mut Vec<u8>) {
        let content_h = self.win_size.1.saturating_sub(CHROME_H).max(1);
        let hello = Message::Hello {
            proto_version: PROTO_VERSION,
            viewport_w: self.win_size.0.min(u16::MAX as u32) as u16,
            viewport_h: content_h.min(u16::MAX as u32) as u16,
            dpr_hundredths: 100,
            client_name: "gutted-client-pure-rust-nostd".into(),
            capabilities: caps::H264 | caps::OPUS | caps::CLIENT_SCROLL,
        };
        hello.encode(out);
    }

    /// Decode and apply all complete [`gutted_proto::Message`] frames present in `inbuf`,
    /// draining consumed bytes. Usable in `#![no_std]` + `alloc` environments.
    pub fn ingest_wire_bytes(
        &mut self,
        inbuf: &mut Vec<u8>,
    ) -> core::result::Result<usize, gutted_proto::Error> {
        let mut count = 0usize;
        let consumed = {
            let mut cur = inbuf.as_slice();
            while let Some(msg) = Message::decode(&mut cur)? {
                count += 1;
                if let Some(rev) = RenderEvent::from_proto_message(msg) {
                    self.handle_render_event(rev);
                }
            }
            inbuf.len() - cur.len()
        };
        if consumed > 0 {
            inbuf.drain(..consumed);
        }
        Ok(count)
    }

    /// Drain all queued [`InputEvent`]s and encode them as wire [`gutted_proto::Message`]s into `out`.
    pub fn encode_outbound_messages(&mut self, ts_us: u64, out: &mut Vec<u8>) {
        for ev in self.outbound_inputs.drain(..) {
            ev.into_proto_message(ts_us).encode(out);
        }
    }

    /// Drain queued 48kHz stereo S16LE [`AudioChunk`]s (for `#![no_std]` audio drivers).
    pub fn drain_audio(&mut self) -> alloc::vec::Drain<'_, AudioChunk> {
        self.pending_audio.drain(..)
    }

    pub fn on_resize(&mut self, w: u32, h: u32) {
        let w = w.max(64);
        let h = h.max(CHROME_H + 64);
        if self.win_size != (w, h) {
            self.win_size = (w, h);
            self.chrome_dirty = true;
            self.full_content_dirty = true;
            let content_h = h.saturating_sub(CHROME_H).max(1);
            self.emit_input(InputEvent::Resize {
                w: w.min(u16::MAX as u32) as u16,
                h: content_h.min(u16::MAX as u32) as u16,
            });
        }
    }

    pub fn submit_omnibox(&mut self) {
        let url = canonicalize_url(&self.url_buf);
        self.url_buf = url.clone();
        self.editing = false;
        self.is_loading = true;
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == self.active_tab_id) {
            t.url = url.clone();
            t.is_secure = url.starts_with("https://");
            t.is_loading = true;
        }
        self.emit_input(InputEvent::Navigate(url));
        self.chrome_dirty = true;
    }

    /// Process a [`RenderEvent`]. Returns `false` if [`RenderEvent::Quit`] was received.
    pub fn handle_render_event(&mut self, ev: RenderEvent) -> bool {
        match ev {
            RenderEvent::Quit => return false,
            RenderEvent::Frame(f) => {
                if f.width == 0 || f.height == 0 {
                    return true;
                }
                let expected = (f.stride as usize) * (f.height as usize);
                if f.pixels.len() < expected {
                    return true;
                }
                self.pending_frames.clear();
                self.pending_subframes.clear();
                self.pending_tiles.clear();
                self.pending_damage.clear();
                self.pending_frames.push(f);
                self.full_content_dirty = true;
                self.frames_seen += 1;
            }
            RenderEvent::Subframe(s) => {
                if s.w == 0 || s.h == 0 {
                    return true;
                }
                let expected = (s.stride as usize) * (s.h as usize);
                if s.pixels.len() < expected {
                    return true;
                }
                self.pending_damage.push(DamageRect {
                    x: s.x,
                    y: s.y,
                    w: s.w,
                    h: s.h,
                });
                self.pending_subframes.push(s);
                self.frames_seen += 1;
            }
            RenderEvent::TileData { hash, pixels } => {
                if pixels.len() >= 64 * 64 * 4 {
                    let mut words = vec![0u32; 64 * 64];
                    copy_bgra_row_to_u32(&pixels[..64 * 64 * 4], &mut words);
                    self.tile_cache.insert(hash, words);
                }
            }
            RenderEvent::TileRef { x, y, w, h, hash } => {
                if w == 64 && h == 64 && self.tile_cache.contains_key(&hash) {
                    self.pending_tiles.push((x, y, hash));
                    self.pending_damage.push(DamageRect { x, y, w, h });
                }
            }
            RenderEvent::Audio {
                pts_us,
                channels,
                sample_rate,
                data,
            } => {
                let bytes_per_sample = (channels.max(1) as u64) * 2;
                let samples = (data.len() as u64) / bytes_per_sample;
                let dur_us = (samples * 1_000_000) / (sample_rate.max(1) as u64);
                let was_playing = self.audio_playing_frames_left > 0;
                self.audio_playing_frames_left = 30;
                self.av_clock.on_audio_frame(pts_us, dur_us);
                #[cfg(feature = "std")]
                {
                    self.audio_out.muted = self.audio_muted;
                    self.audio_out.write_pcm(sample_rate, channels, &data);
                }
                #[cfg(not(feature = "std"))]
                if !self.audio_muted {
                    self.pending_audio.push(AudioChunk {
                        pts_us,
                        sample_rate,
                        channels,
                        pcm: data,
                    });
                }
                if !was_playing {
                    self.chrome_dirty = true;
                }
            }
            RenderEvent::CursorShape(shape) => {
                self.cursor_shape = shape;
            }
            RenderEvent::LoadState(state) => {
                self.is_loading = state < 3;
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == self.active_tab_id) {
                    t.is_loading = self.is_loading;
                }
                self.chrome_dirty = true;
            }
            RenderEvent::InitialUrl(u) | RenderEvent::UrlChanged(u) => {
                if !self.editing {
                    self.url_buf = u.clone();
                }
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == self.active_tab_id) {
                    t.url = u.clone();
                    t.is_secure = u.starts_with("https://");
                }
                self.chrome_dirty = true;
            }
            RenderEvent::Title(title) => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == self.active_tab_id) {
                    t.title = title.clone();
                }
                self.window_title = if title.is_empty() {
                    "ExoBrowser".into()
                } else {
                    format!("{title} — ExoBrowser")
                };
                self.chrome_dirty = true;
            }
            RenderEvent::TabCreated { tab_id, title, url } => {
                self.next_tab_id = self.next_tab_id.max(tab_id + 1);
                if let Some(existing) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    if !title.is_empty() {
                        existing.title = title;
                    }
                    if !url.is_empty() {
                        existing.is_secure = url.starts_with("https://");
                        existing.url = url;
                    }
                } else {
                    self.tabs.push(TabInfo {
                        id: tab_id,
                        title: if title.is_empty() {
                            "New Tab".into()
                        } else {
                            title
                        },
                        is_secure: url.starts_with("https://"),
                        url,
                        is_loading: false,
                    });
                }
                self.chrome_dirty = true;
            }
            RenderEvent::TabClosed { tab_id } => {
                self.tabs.retain(|t| t.id != tab_id);
                self.tab_composites.remove(&tab_id);
                if self.active_tab_id == tab_id {
                    if let Some(first) = self.tabs.first() {
                        self.active_tab_id = first.id;
                        self.url_buf = first.url.clone();
                        self.active_buf = self.tab_composites.remove(&self.active_tab_id);
                        self.full_content_dirty = true;
                    }
                }
                self.chrome_dirty = true;
            }
            RenderEvent::TabActivated { tab_id } => {
                if self.active_tab_id != tab_id {
                    if let Some(cur) = self.active_buf.take() {
                        self.tab_composites.insert(self.active_tab_id, cur);
                    }
                    self.active_tab_id = tab_id;
                    self.active_buf = self.tab_composites.remove(&tab_id);
                    if let Some(t) = self.tabs.iter().find(|t| t.id == tab_id) {
                        if !self.editing {
                            self.url_buf = t.url.clone();
                        }
                        self.is_loading = t.is_loading;
                    }
                    self.full_content_dirty = true;
                }
                self.chrome_dirty = true;
            }
            RenderEvent::TabTitle { tab_id, title } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    t.title = title;
                    self.chrome_dirty = true;
                }
            }
            RenderEvent::TabUrl { tab_id, url } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    t.is_secure = url.starts_with("https://");
                    t.url = url.clone();
                    if tab_id == self.active_tab_id && !self.editing {
                        self.url_buf = url;
                    }
                    self.chrome_dirty = true;
                }
            }
        }
        true
    }

    pub fn draw_chrome(&self, buf: &mut [u32], bw: u32, bh: u32) {
        let chrome_h = CHROME_H.min(bh) as i32;
        if chrome_h <= 0 {
            return;
        }

        // 1. Tab Bar (y: 0..32)
        fill_rect(buf, bw, bh, 0, 0, bw as i32, TAB_BAR_H as i32, 0x111116);
        let n_tabs = self.tabs.len().max(1) as i32;
        let avail_w = (bw as i32 - 52).max(100);
        let tab_w = (avail_w / n_tabs).clamp(90, 210);
        let mut tx = 6i32;

        for tab in &self.tabs {
            let active = tab.id == self.active_tab_id;
            let bg = if active { 0x1E1E28 } else { 0x16161D };
            fill_rounded_rect(buf, bw, bh, tx, 4, tab_w - 4, TAB_BAR_H as i32 - 4, 6, bg);
            if active {
                fill_rect(buf, bw, bh, tx + 8, 4, tab_w - 20, 2, 0x3B82F6);
            }

            let dot_col = if tab.is_loading {
                0xF59E0B
            } else if tab.is_secure {
                0x10B981
            } else {
                0x6B7280
            };
            fill_rounded_rect(buf, bw, bh, tx + 10, 14, 6, 6, 3, dot_col);

            let title = if tab.title.is_empty() {
                "New Tab"
            } else {
                &tab.title
            };
            let fg = if active { 0xF3F4F6 } else { 0x9CA3AF };
            self.font
                .draw_text(buf, bw, bh, tx + 22, 9, 13.0, fg, title, tab_w - 46);

            if self.tabs.len() > 1 {
                let close_x = tx + tab_w - 22;
                self.font
                    .draw_text(buf, bw, bh, close_x, 9, 13.0, 0x9CA3AF, "x", 12);
            }
            tx += tab_w;
        }

        // '+' New Tab button
        fill_rounded_rect(buf, bw, bh, tx + 2, 6, 26, 22, 5, 0x252532);
        self.font
            .draw_text(buf, bw, bh, tx + 10, 8, 15.0, 0xE5E7EB, "+", 14);

        // 2. Navigation Bar (y: 32..70)
        let ny = TAB_BAR_H as i32;
        fill_rect(buf, bw, bh, 0, ny, bw as i32, NAV_BAR_H as i32, 0x1E1E28);

        let nav_btns = [
            (8, "<"),
            (40, ">"),
            (72, if self.is_loading { "X" } else { "R" }),
            (104, "H"),
        ];
        for (bx, label) in nav_btns {
            fill_rounded_rect(buf, bw, bh, bx, ny + 6, 28, 26, 6, 0x2A2A38);
            self.font
                .draw_text(buf, bw, bh, bx + 9, ny + 10, 13.5, 0xE5E7EB, label, 16);
        }

        let right_w = 216;
        let right_x = (bw as i32 - right_w).max(200);

        // Omnibox pill
        let omni_x = 140;
        let omni_w = (right_x - omni_x - 10).max(80);
        let omni_bg = if self.editing { 0x121218 } else { 0x14141C };
        fill_rounded_rect(buf, bw, bh, omni_x, ny + 5, omni_w, 28, 14, omni_bg);
        let border_col = if self.editing { 0x3B82F6 } else { 0x323244 };
        stroke_rounded_rect(buf, bw, bh, omni_x, ny + 5, omni_w, 28, border_col);

        let is_https = self.url_buf.starts_with("https://");
        let badge_col = if is_https { 0x10B981 } else { 0x9CA3AF };
        let badge_txt = if is_https { "HTTPS" } else { "URL" };
        fill_rounded_rect(buf, bw, bh, omni_x + 6, ny + 9, 44, 20, 10, 0x1E293B);
        self.font.draw_text(
            buf,
            bw,
            bh,
            omni_x + 11,
            ny + 12,
            11.0,
            badge_col,
            badge_txt,
            38,
        );

        let text_x = omni_x + 56;
        let max_text_w = (omni_w - 66).max(20);
        let drawn_w = self.font.draw_text(
            buf,
            bw,
            bh,
            text_x,
            ny + 10,
            13.5,
            0xF3F4F6,
            &self.url_buf,
            max_text_w,
        );
        if self.editing {
            let caret_x = (text_x + drawn_w + 1).min(omni_x + omni_w - 10);
            fill_rect(buf, bw, bh, caret_x, ny + 10, 2, 18, 0x60A5FA);
        }

        // Audio mute/playing button
        let aud_x = right_x;
        let aud_bg = if self.audio_muted {
            0x7F1D1D
        } else if self.audio_playing_frames_left > 0 {
            0x065F46
        } else {
            0x2A2A38
        };
        let aud_txt = if self.audio_muted { "MUTE" } else { "AUD" };
        fill_rounded_rect(buf, bw, bh, aud_x, ny + 6, 42, 26, 6, aud_bg);
        self.font
            .draw_text(buf, bw, bh, aud_x + 6, ny + 11, 11.5, 0xF3F4F6, aud_txt, 34);

        // Zoom controls: [-] [100%] [+]
        let zm_x = right_x + 48;
        fill_rounded_rect(buf, bw, bh, zm_x, ny + 6, 114, 26, 6, 0x2A2A38);
        self.font
            .draw_text(buf, bw, bh, zm_x + 8, ny + 10, 14.0, 0xE5E7EB, "-", 14);
        let pct = format!("{}%", self.zoom_milli / 10);
        self.font
            .draw_text(buf, bw, bh, zm_x + 34, ny + 11, 12.0, 0xD1D5DB, &pct, 48);
        self.font
            .draw_text(buf, bw, bh, zm_x + 94, ny + 10, 14.0, 0xE5E7EB, "+", 14);

        // Clear Data button: [CLR]
        let clr_x = right_x + 168;
        fill_rounded_rect(buf, bw, bh, clr_x, ny + 6, 40, 26, 6, 0x3F232A);
        self.font
            .draw_text(buf, bw, bh, clr_x + 7, ny + 11, 11.5, 0xFCA5A5, "CLR", 30);

        // 3. Bookmarks Bar (y: 70..96)
        let by = (TAB_BAR_H + NAV_BAR_H) as i32;
        fill_rect(
            buf,
            bw,
            bh,
            0,
            by,
            bw as i32,
            BOOKMARK_BAR_H as i32,
            0x181820,
        );
        let mut bkx = 8i32;
        for (idx, (label, _)) in BOOKMARKS.iter().enumerate() {
            let chip_w = (label.len() as i32 * 7) + 20;
            if bkx + chip_w > bw as i32 - 8 {
                break;
            }
            let chip_bg = if idx == 0 { 0x311B1B } else { 0x23232F };
            let chip_fg = if idx == 0 { 0xFCA5A5 } else { 0xD1D5DB };
            fill_rounded_rect(buf, bw, bh, bkx, by + 3, chip_w, 20, 5, chip_bg);
            self.font
                .draw_text(buf, bw, bh, bkx + 8, by + 5, 12.0, chip_fg, label, chip_w - 12);
            bkx += chip_w + 6;
        }

        let div_col = if self.is_loading { 0x3B82F6 } else { 0x2A2A38 };
        fill_rect(buf, bw, bh, 0, CHROME_H as i32 - 1, bw as i32, 1, div_col);
    }

    pub fn on_pointer_move(&mut self, x: i32, y: i32) {
        self.cursor = (x, y);
        if y >= CHROME_H as i32 {
            self.emit_input(InputEvent::Motion {
                x,
                y: y - CHROME_H as i32,
                mods: self.mods,
            });
        }
    }

    pub fn on_mouse_button(&mut self, x: i32, y: i32, button: u32, pressed: bool) {
        let bit = match button {
            1 => MOD_BUTTON1,
            3 => MOD_BUTTON3,
            2 => MOD_BUTTON2,
            _ => 0,
        };
        if pressed {
            self.mods |= bit;
        } else {
            self.mods &= !bit;
        }

        if y < CHROME_H as i32 {
            if pressed {
                self.handle_chrome_click(x, y, button);
            }
            return;
        }

        if pressed && self.editing {
            self.editing = false;
            self.chrome_dirty = true;
        }

        if button != 0 {
            self.emit_input(InputEvent::Button {
                x,
                y: y - CHROME_H as i32,
                button,
                pressed,
                mods: self.mods,
            });
        }
    }

    pub fn on_scroll(&mut self, dx: i32, dy: i32) {
        if self.cursor.1 >= CHROME_H as i32 && (dx != 0 || dy != 0) {
            self.emit_input(InputEvent::Scroll { dx, dy });
        }
    }

    pub fn on_key(&mut self, keysym: u32, pressed: bool, text: Option<&str>) -> bool {
        let ctrl = (self.mods & MOD_CTRL) != 0;
        let alt = (self.mods & MOD_ALT) != 0;

        if pressed {
            if keysym == 0xffc8 {
                return true;
            }
            if keysym == 0xffc2 {
                self.emit_input(InputEvent::NavAction { action: 2 });
                return false;
            }
            if alt && keysym == 0xff51 {
                self.emit_input(InputEvent::NavAction { action: 0 });
                return false;
            }
            if alt && keysym == 0xff53 {
                self.emit_input(InputEvent::NavAction { action: 1 });
                return false;
            }
            if ctrl {
                match keysym {
                    0x006c | 0x004c => {
                        self.editing = true;
                        self.url_buf.clear();
                        self.chrome_dirty = true;
                        return false;
                    }
                    0x0074 | 0x0054 => {
                        let new_id = self.next_tab_id;
                        self.next_tab_id += 1;
                        let url = "https://duckduckgo.com".to_string();
                        if let Some(cur) = self.active_buf.take() {
                            self.tab_composites.insert(self.active_tab_id, cur);
                        }
                        self.tabs.push(TabInfo {
                            id: new_id,
                            title: "New Tab".into(),
                            url: url.clone(),
                            is_loading: true,
                            is_secure: true,
                        });
                        self.active_tab_id = new_id;
                        self.url_buf = url.clone();
                        self.editing = false;
                        self.full_content_dirty = true;
                        self.chrome_dirty = true;
                        self.emit_input(InputEvent::CreateTab { tab_id: new_id, url });
                        return false;
                    }
                    0x0077 | 0x0057 => {
                        if self.tabs.len() > 1 {
                            let id = self.active_tab_id;
                            self.emit_input(InputEvent::CloseTab { tab_id: id });
                            self.tabs.retain(|t| t.id != id);
                            self.tab_composites.remove(&id);
                            if let Some(first) = self.tabs.first() {
                                self.active_tab_id = first.id;
                                self.url_buf = first.url.clone();
                                self.active_buf = self.tab_composites.remove(&self.active_tab_id);
                                self.emit_input(InputEvent::SwitchTab {
                                    tab_id: self.active_tab_id,
                                });
                            }
                            self.full_content_dirty = true;
                            self.chrome_dirty = true;
                        }
                        return false;
                    }
                    0x0072 | 0x0052 => {
                        self.emit_input(InputEvent::NavAction { action: 2 });
                        return false;
                    }
                    0x006d | 0x004d => {
                        self.audio_muted = !self.audio_muted;
                        self.chrome_dirty = true;
                        return false;
                    }
                    0x003d | 0x002b => {
                        self.zoom_milli = (self.zoom_milli + 100).min(3000);
                        self.emit_input(InputEvent::SetZoom {
                            level_milli: self.zoom_milli,
                        });
                        self.chrome_dirty = true;
                        return false;
                    }
                    0x002d => {
                        self.zoom_milli = self.zoom_milli.saturating_sub(100).max(300);
                        self.emit_input(InputEvent::SetZoom {
                            level_milli: self.zoom_milli,
                        });
                        self.chrome_dirty = true;
                        return false;
                    }
                    0x0030 => {
                        self.zoom_milli = 1000;
                        self.emit_input(InputEvent::SetZoom { level_milli: 1000 });
                        self.chrome_dirty = true;
                        return false;
                    }
                    0xff09 => {
                        if self.tabs.len() > 1 {
                            let idx = self
                                .tabs
                                .iter()
                                .position(|t| t.id == self.active_tab_id)
                                .unwrap_or(0);
                            let next_id = self.tabs[(idx + 1) % self.tabs.len()].id;
                            if let Some(cur) = self.active_buf.take() {
                                self.tab_composites.insert(self.active_tab_id, cur);
                            }
                            self.active_tab_id = next_id;
                            self.active_buf = self.tab_composites.remove(&next_id);
                            if let Some(t) = self.tabs.iter().find(|t| t.id == next_id) {
                                self.url_buf = t.url.clone();
                            }
                            self.emit_input(InputEvent::SwitchTab { tab_id: next_id });
                            self.full_content_dirty = true;
                            self.chrome_dirty = true;
                        }
                        return false;
                    }
                    _ => {}
                }
            }

            if self.editing {
                match keysym {
                    0xff0d | 0xff8d => {
                        self.submit_omnibox();
                        return false;
                    }
                    0xff1b => {
                        self.editing = false;
                        if let Some(t) = self.tabs.iter().find(|t| t.id == self.active_tab_id) {
                            self.url_buf = t.url.clone();
                        }
                        self.chrome_dirty = true;
                        return false;
                    }
                    0xff08 => {
                        self.url_buf.pop();
                        self.chrome_dirty = true;
                        return false;
                    }
                    _ => {}
                }
            }
        }

        if self.editing {
            if pressed && !ctrl && !alt {
                if let Some(t) = text {
                    for c in t.chars() {
                        if !c.is_control() {
                            self.url_buf.push(c);
                        }
                    }
                    self.chrome_dirty = true;
                } else if (0x20..=0x7e).contains(&keysym) {
                    if let Some(c) = char::from_u32(keysym) {
                        self.url_buf.push(c);
                        self.chrome_dirty = true;
                    }
                }
            }
            return false;
        }

        if keysym != 0 {
            self.emit_input(InputEvent::Key {
                keysym,
                mods: self.mods,
                pressed,
            });
        }
        false
    }

    fn handle_chrome_click(&mut self, x: i32, y: i32, button: u32) {
        let bw = self.win_size.0 as i32;
        if y < TAB_BAR_H as i32 {
            let n_tabs = self.tabs.len().max(1) as i32;
            let avail_w = (bw - 52).max(100);
            let tab_w = (avail_w / n_tabs).clamp(90, 210);
            let mut tx = 6i32;

            for i in 0..self.tabs.len() {
                let tab_id = self.tabs[i].id;
                if x >= tx && x < tx + tab_w - 4 {
                    let close_x = tx + tab_w - 24;
                    if (button == 2 || (button == 1 && x >= close_x)) && self.tabs.len() > 1 {
                        self.emit_input(InputEvent::CloseTab { tab_id });
                        self.tabs.remove(i);
                        self.tab_composites.remove(&tab_id);
                        if self.active_tab_id == tab_id {
                            if let Some(first) = self.tabs.first() {
                                self.active_tab_id = first.id;
                                self.url_buf = first.url.clone();
                                self.active_buf = self.tab_composites.remove(&self.active_tab_id);
                                self.emit_input(InputEvent::SwitchTab {
                                    tab_id: self.active_tab_id,
                                });
                            }
                            self.full_content_dirty = true;
                        }
                        self.chrome_dirty = true;
                        return;
                    }
                    if self.active_tab_id != tab_id {
                        if let Some(cur) = self.active_buf.take() {
                            self.tab_composites.insert(self.active_tab_id, cur);
                        }
                        self.active_tab_id = tab_id;
                        self.active_buf = self.tab_composites.remove(&tab_id);
                        self.url_buf = self.tabs[i].url.clone();
                        self.editing = false;
                        self.emit_input(InputEvent::SwitchTab { tab_id });
                        self.full_content_dirty = true;
                        self.chrome_dirty = true;
                    }
                    return;
                }
                tx += tab_w;
            }

            if x >= tx && x <= tx + 32 {
                let new_id = self.next_tab_id;
                self.next_tab_id += 1;
                let url = "https://duckduckgo.com".to_string();
                if let Some(cur) = self.active_buf.take() {
                    self.tab_composites.insert(self.active_tab_id, cur);
                }
                self.tabs.push(TabInfo {
                    id: new_id,
                    title: "New Tab".into(),
                    url: url.clone(),
                    is_loading: true,
                    is_secure: true,
                });
                self.active_tab_id = new_id;
                self.url_buf = url.clone();
                self.editing = false;
                self.full_content_dirty = true;
                self.chrome_dirty = true;
                self.emit_input(InputEvent::CreateTab { tab_id: new_id, url });
            }
            return;
        }

        if y < (TAB_BAR_H + NAV_BAR_H) as i32 {
            if (8..36).contains(&x) {
                self.emit_input(InputEvent::NavAction { action: 0 });
                return;
            }
            if (40..68).contains(&x) {
                self.emit_input(InputEvent::NavAction { action: 1 });
                return;
            }
            if (72..100).contains(&x) {
                if self.is_loading {
                    self.emit_input(InputEvent::Stop);
                } else {
                    self.emit_input(InputEvent::NavAction { action: 2 });
                }
                return;
            }
            if (104..132).contains(&x) {
                let home = "https://duckduckgo.com".to_string();
                self.url_buf = home.clone();
                self.editing = false;
                self.emit_input(InputEvent::Navigate(home));
                self.chrome_dirty = true;
                return;
            }

            let right_w = 216;
            let right_x = (bw - right_w).max(200);
            let omni_x = 140;
            let omni_w = (right_x - omni_x - 10).max(80);

            if x >= omni_x && x < omni_x + omni_w {
                if !self.editing {
                    self.editing = true;
                    self.url_buf.clear();
                    self.chrome_dirty = true;
                }
                return;
            }

            if x >= right_x && x < right_x + 42 {
                self.audio_muted = !self.audio_muted;
                self.chrome_dirty = true;
                return;
            }

            let zm_x = right_x + 48;
            if x >= zm_x && x < zm_x + 28 {
                self.zoom_milli = self.zoom_milli.saturating_sub(100).max(300);
                self.emit_input(InputEvent::SetZoom {
                    level_milli: self.zoom_milli,
                });
                self.chrome_dirty = true;
                return;
            }
            if x >= zm_x + 28 && x < zm_x + 86 {
                self.zoom_milli = 1000;
                self.emit_input(InputEvent::SetZoom { level_milli: 1000 });
                self.chrome_dirty = true;
                return;
            }
            if x >= zm_x + 86 && x < zm_x + 114 {
                self.zoom_milli = (self.zoom_milli + 100).min(3000);
                self.emit_input(InputEvent::SetZoom {
                    level_milli: self.zoom_milli,
                });
                self.chrome_dirty = true;
                return;
            }

            let clr_x = right_x + 168;
            if x >= clr_x && x < clr_x + 40 {
                self.emit_input(InputEvent::ClearData {
                    clear_cookies: true,
                    clear_cache: true,
                    clear_storage: true,
                });
                self.status_toast_text = Some("Cleared cookies, cache & storage".into());
                self.status_toast_frames_left = 60;
                self.chrome_dirty = true;
                return;
            }
            return;
        }

        // Bookmarks Bar
        let mut bkx = 8i32;
        for (label, url) in BOOKMARKS {
            let chip_w = (label.len() as i32 * 7) + 20;
            if x >= bkx && x < bkx + chip_w {
                self.url_buf = (*url).to_string();
                self.editing = false;
                self.is_loading = true;
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == self.active_tab_id) {
                    t.url = (*url).to_string();
                    t.is_secure = url.starts_with("https://");
                    t.is_loading = true;
                }
                self.emit_input(InputEvent::Navigate((*url).to_string()));
                self.chrome_dirty = true;
                return;
            }
            bkx += chip_w + 6;
        }
    }

    /// Composite dirty regions into `buffer` (`bw * bh` `u32` pixels in `0x00RRGGBB` format)
    /// and return the list of [`DamageRect`]s that changed in this frame.
    pub fn render_into(&mut self, buffer: &mut [u32]) -> Vec<DamageRect> {
        let (bw, bh) = self.win_size;
        let needed = (bw as usize) * (bh as usize);
        if bw == 0 || bh == 0 || buffer.len() < needed {
            return Vec::new();
        }

        if self.audio_playing_frames_left > 0 {
            self.audio_playing_frames_left -= 1;
            if self.audio_playing_frames_left == 0 {
                self.chrome_dirty = true;
            }
        }

        let mut damage = Vec::new();

        if self.chrome_dirty {
            self.draw_chrome(buffer, bw, bh);
            damage.push(DamageRect {
                x: 0,
                y: 0,
                w: bw,
                h: CHROME_H.min(bh),
            });
            self.chrome_dirty = false;
        }

        let content_h = bh.saturating_sub(CHROME_H);
        if content_h > 0 {
            if let Some(f) = self.pending_frames.pop() {
                self.pending_frames.clear();
                let copy_w = f.width.min(bw) as usize;
                let copy_h = f.height.min(content_h) as usize;
                for row in 0..copy_h {
                    let dst_start = (CHROME_H as usize + row) * bw as usize;
                    let src_start = row * f.stride as usize;
                    copy_bgra_row_to_u32(
                        &f.pixels[src_start..src_start + copy_w * 4],
                        &mut buffer[dst_start..dst_start + copy_w],
                    );
                    if copy_w < bw as usize {
                        buffer[dst_start + copy_w..dst_start + bw as usize].fill(0x0F0F12);
                    }
                }
                if copy_h < content_h as usize {
                    let pad_start = (CHROME_H as usize + copy_h) * bw as usize;
                    buffer[pad_start..needed].fill(0x0F0F12);
                }
                damage.push(DamageRect {
                    x: 0,
                    y: CHROME_H,
                    w: bw,
                    h: content_h,
                });
                self.full_content_dirty = false;
                self.pending_damage.clear();
            } else if self.full_content_dirty {
                if let Some(tb) = &self.active_buf {
                    let copy_w = tb.w.min(bw) as usize;
                    let copy_h = tb.h.min(content_h) as usize;
                    for row in 0..copy_h {
                        let dst_start = (CHROME_H as usize + row) * bw as usize;
                        let src_start = row * tb.w as usize;
                        buffer[dst_start..dst_start + copy_w]
                            .copy_from_slice(&tb.pixels[src_start..src_start + copy_w]);
                        if copy_w < bw as usize {
                            buffer[dst_start + copy_w..dst_start + bw as usize].fill(0x0F0F12);
                        }
                    }
                    if copy_h < content_h as usize {
                        let pad_start = (CHROME_H as usize + copy_h) * bw as usize;
                        buffer[pad_start..needed].fill(0x0F0F12);
                    }
                } else {
                    let start = (CHROME_H * bw) as usize;
                    buffer[start..needed].fill(0x0F0F12);
                }
                damage.push(DamageRect {
                    x: 0,
                    y: CHROME_H,
                    w: bw,
                    h: content_h,
                });
                self.full_content_dirty = false;
                self.pending_damage.clear();
            }
            for s in self.pending_subframes.drain(..) {
                let x0 = s.x.min(bw);
                let y0 = s.y.min(content_h);
                let x1 = (s.x + s.w).min(bw);
                let y1 = (s.y + s.h).min(content_h);
                if x1 > x0 && y1 > y0 {
                    let copy_w = (x1 - x0) as usize;
                    let copy_h = (y1 - y0) as usize;
                    for row in 0..copy_h {
                        let src_off = row * s.stride as usize;
                        let dst_off =
                            (CHROME_H as usize + y0 as usize + row) * bw as usize + x0 as usize;
                        copy_bgra_row_to_u32(
                            &s.pixels[src_off..src_off + copy_w * 4],
                            &mut buffer[dst_off..dst_off + copy_w],
                        );
                    }
                    damage.push(DamageRect {
                        x: x0,
                        y: CHROME_H + y0,
                        w: x1 - x0,
                        h: y1 - y0,
                    });
                }
            }
            for (tx, ty, hash) in self.pending_tiles.drain(..) {
                if let Some(tile) = self.tile_cache.get(&hash) {
                    let x0 = tx.min(bw);
                    let y0 = ty.min(content_h);
                    let x1 = (tx + 64).min(bw);
                    let y1 = (ty + 64).min(content_h);
                    if x1 > x0 && y1 > y0 {
                        let copy_w = (x1 - x0) as usize;
                        let copy_h = (y1 - y0) as usize;
                        for row in 0..copy_h {
                            let dst_off = (CHROME_H as usize + y0 as usize + row) * bw as usize
                                + x0 as usize;
                            let src_off = row * 64;
                            buffer[dst_off..dst_off + copy_w]
                                .copy_from_slice(&tile[src_off..src_off + copy_w]);
                        }
                        damage.push(DamageRect {
                            x: x0,
                            y: CHROME_H + y0,
                            w: x1 - x0,
                            h: y1 - y0,
                        });
                    }
                }
            }
            self.pending_damage.clear();
        }

        if self.status_toast_frames_left > 0 {
            self.status_toast_frames_left -= 1;
            if let Some(msg) = &self.status_toast_text {
                let tw = (msg.len() as i32 * 7) + 24;
                let ty = bh as i32 - 36;
                fill_rounded_rect(buffer, bw, bh, 12, ty, tw, 24, 6, 0x1E293B);
                self.font
                    .draw_text(buffer, bw, bh, 22, ty + 4, 12.5, 0x93C5FD, msg, tw - 16);
                damage.push(DamageRect {
                    x: 12,
                    y: ty.max(0) as u32,
                    w: (tw as u32).min(bw),
                    h: 24,
                });
            }
            if self.status_toast_frames_left == 0 {
                self.full_content_dirty = true;
            }
        }

        if let Some((sx, sy)) = self.synth_cursor {
            fill_rounded_rect(buffer, bw, bh, sx - 4, sy - 4, 9, 9, 4, 0x3B82F6);
        }

        #[cfg(feature = "std")]
        if let Some(path) = self.capture_to.clone() {
            if self.frames_seen >= self.capture_after_frames {
                let _ = write_ppm_u32(&path, bw, bh, &buffer[..needed]);
                log_info(
                    "gutted_client::render",
                    &format!("captured screenshot path={}", path.display()),
                );
                self.capture_to = None;
            }
        }

        damage
    }
}

#[cfg(feature = "std")]
fn write_ppm_u32(path: &std::path::Path, w: u32, h: u32, pixels: &[u32]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    write!(f, "P6\n{w} {h}\n255\n")?;
    let mut row_rgb = Vec::with_capacity(w as usize * 3);
    for y in 0..h as usize {
        row_rgb.clear();
        let row = &pixels[y * w as usize..(y + 1) * w as usize];
        for &px in row {
            row_rgb.push(((px >> 16) & 0xFF) as u8);
            row_rgb.push(((px >> 8) & 0xFF) as u8);
            row_rgb.push((px & 0xFF) as u8);
        }
        f.write_all(&row_rgb)?;
    }
    Ok(())
}

/// Run the platform UI event loop (`std`: Pure-Rust X11 on Linux/Unix, Orbital on Redox OS,
/// or headless framebuffer fallback).
#[cfg(feature = "std")]
pub fn run(
    frames_rx: std::sync::mpsc::Receiver<RenderEvent>,
    input_tx: std::sync::mpsc::Sender<InputEvent>,
) -> Result<()> {
    let synth_url = std::env::var("GBROWSER_SYNTH_URL").ok();
    let editing = synth_url.is_some();
    let initial_url = synth_url.unwrap_or_else(|| "https://duckduckgo.com".into());
    let mut core = BrowserCore::new(1280, 744, initial_url, editing, input_tx);

    #[cfg(all(unix, not(target_os = "redox")))]
    {
        if std::env::var("DISPLAY").is_ok()
            && std::env::var("GBROWSER_HEADLESS").is_err()
            && std::env::var("GBROWSER_HOLD_SECS").is_err()
        {
            if let Ok(()) = run_x11_loop(&mut core, &frames_rx) {
                return Ok(());
            }
        }
    }

    #[cfg(target_os = "redox")]
    {
        if let Ok(()) = run_redox_orbital_loop(&mut core, &frames_rx) {
            return Ok(());
        }
    }

    run_headless_loop(&mut core, &frames_rx)
}

#[cfg(feature = "std")]
fn run_headless_loop(
    core: &mut BrowserCore,
    frames_rx: &std::sync::mpsc::Receiver<RenderEvent>,
) -> Result<()> {
    let want_screenshot = core.capture_to.is_some();
    let mut framebuffer: Vec<u32> = if want_screenshot {
        let mut fb = vec![0x0F0F12u32; (core.win_size.0 * core.win_size.1) as usize];
        let _ = core.render_into(&mut fb);
        fb
    } else {
        Vec::new()
    };

    while let Ok(ev) = frames_rx.recv() {
        if !core.handle_render_event(ev) {
            break;
        }
        while let Ok(ev2) = frames_rx.try_recv() {
            if !core.handle_render_event(ev2) {
                return Ok(());
            }
        }
        if core.capture_to.is_some() {
            let needed = (core.win_size.0 * core.win_size.1) as usize;
            if framebuffer.len() != needed {
                framebuffer.resize(needed, 0x0F0F12);
            }
            let _ = core.render_into(&mut framebuffer);
            if core.capture_to.is_none() {
                framebuffer = Vec::new();
            }
        } else {
            core.pending_frames.clear();
            core.pending_subframes.clear();
            core.pending_tiles.clear();
            core.pending_damage.clear();
            core.full_content_dirty = false;
            core.chrome_dirty = false;
        }
    }
    Ok(())
}

// ─── 100% Pure-Rust X11 Backend (`x11rb`, zero C deps) ───────────────────

#[cfg(all(feature = "std", unix, not(target_os = "redox")))]
fn run_x11_loop(
    core: &mut BrowserCore,
    frames_rx: &std::sync::mpsc::Receiver<RenderEvent>,
) -> Result<()> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        AtomEnum, ChangeWindowAttributesAux, ClientMessageData, ClientMessageEvent,
        ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, KeyButMask, PropMode,
        WindowClass,
    };
    use x11rb::protocol::Event;
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;

    let (conn, screen_num) = RustConnection::connect(None)?;
    let setup = conn.setup();
    let min_keycode = setup.min_keycode;
    let max_keycode = setup.max_keycode;
    let screen = &setup.roots[screen_num];
    let root = screen.root;
    let depth = screen.root_depth;
    let visual = screen.root_visual;

    let keymap_reply = conn
        .get_keyboard_mapping(min_keycode, max_keycode - min_keycode + 1)?
        .reply()?;
    let keysyms_per_keycode = keymap_reply.keysyms_per_keycode as usize;
    let keysyms = keymap_reply.keysyms;

    let win_id = conn.generate_id()?;
    let gc_id = conn.generate_id()?;
    let (bw, bh) = core.win_size;

    conn.create_window(
        depth,
        win_id,
        root,
        0,
        0,
        bw as u16,
        bh as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new()
            .background_pixel(0x0F0F12)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::KEY_PRESS
                    | EventMask::KEY_RELEASE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION,
            ),
    )?;

    conn.change_property8(
        PropMode::REPLACE,
        win_id,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"ExoBrowser",
    )?;

    let wm_protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
    let wm_delete_window = conn.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;
    let net_wm_state = conn.intern_atom(false, b"_NET_WM_STATE")?.reply()?.atom;
    let net_wm_state_fullscreen = conn
        .intern_atom(false, b"_NET_WM_STATE_FULLSCREEN")?
        .reply()?
        .atom;
    conn.change_property32(
        PropMode::REPLACE,
        win_id,
        wm_protocols,
        AtomEnum::ATOM,
        &[wm_delete_window],
    )?;

    conn.create_gc(gc_id, win_id, &CreateGCAux::new())?;
    conn.map_window(win_id)?;
    conn.flush()?;

    let mut framebuffer = vec![0x0F0F12u32; (bw * bh) as usize];
    let mut scratch_bytes: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut last_title = core.window_title.clone();

    let initial_damage = core.render_into(&mut framebuffer);
    present_x11_damage(
        &conn,
        win_id,
        gc_id,
        depth,
        core.win_size.0,
        &framebuffer,
        &initial_damage,
        &mut scratch_bytes,
    )?;

    loop {
        let mut dirty_from_x11 = false;
        while let Some(ev) = conn.poll_for_event()? {
            match ev {
                Event::Expose(_) => {
                    core.chrome_dirty = true;
                    core.full_content_dirty = true;
                    dirty_from_x11 = true;
                }
                Event::ConfigureNotify(cfg) => {
                    let nw = cfg.width as u32;
                    let nh = cfg.height as u32;
                    if (nw, nh) != core.win_size && nw > 0 && nh > 0 {
                        core.on_resize(nw, nh);
                        framebuffer.resize((core.win_size.0 * core.win_size.1) as usize, 0x0F0F12);
                        dirty_from_x11 = true;
                    }
                }
                Event::MotionNotify(m) => {
                    update_x11_mods(core, m.state);
                    core.on_pointer_move(m.event_x as i32, m.event_y as i32);
                }
                Event::ButtonPress(b) => {
                    update_x11_mods(core, b.state);
                    match b.detail {
                        1..=3 => {
                            core.on_mouse_button(
                                b.event_x as i32,
                                b.event_y as i32,
                                b.detail as u32,
                                true,
                            );
                            dirty_from_x11 = true;
                        }
                        4 => core.on_scroll(0, -3),
                        5 => core.on_scroll(0, 3),
                        6 => core.on_scroll(-3, 0),
                        7 => core.on_scroll(3, 0),
                        _ => {}
                    }
                }
                Event::ButtonRelease(b) => {
                    update_x11_mods(core, b.state);
                    if (1..=3).contains(&b.detail) {
                        core.on_mouse_button(
                            b.event_x as i32,
                            b.event_y as i32,
                            b.detail as u32,
                            false,
                        );
                    }
                }
                Event::KeyPress(k) | Event::KeyRelease(k) => {
                    let pressed = matches!(ev, Event::KeyPress(_));
                    update_x11_mods(core, k.state);
                    let shift = k.state.contains(KeyButMask::SHIFT);
                    let keysym = lookup_x11_keysym(
                        k.detail,
                        min_keycode,
                        keysyms_per_keycode,
                        &keysyms,
                        shift,
                    );
                    update_keysym_modifier(core, keysym, pressed);
                    if core.on_key(keysym, pressed, None) {
                        let data = ClientMessageData::from([
                            2u32,
                            net_wm_state_fullscreen,
                            0,
                            1,
                            0,
                        ]);
                        let cm = ClientMessageEvent {
                            response_type: x11rb::protocol::xproto::CLIENT_MESSAGE_EVENT,
                            format: 32,
                            sequence: 0,
                            window: win_id,
                            type_: net_wm_state,
                            data,
                        };
                        let _ = conn.send_event(
                            false,
                            root,
                            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                            cm,
                        );
                        let _ = conn.flush();
                    }
                    dirty_from_x11 = true;
                }
                Event::ClientMessage(cm) if cm.data.as_data32()[0] == wm_delete_window => {
                    return Ok(());
                }
                _ => {}
            }
        }

        let mut got_net = false;
        match frames_rx.recv_timeout(Duration::from_millis(4)) {
            Ok(ev) => {
                if !core.handle_render_event(ev) {
                    return Ok(());
                }
                got_net = true;
                while let Ok(ev2) = frames_rx.try_recv() {
                    if !core.handle_render_event(ev2) {
                        return Ok(());
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }

        if got_net
            || dirty_from_x11
            || core.chrome_dirty
            || core.full_content_dirty
            || !core.pending_damage.is_empty()
        {
            if core.window_title != last_title {
                last_title = core.window_title.clone();
                let _ = conn.change_property8(
                    PropMode::REPLACE,
                    win_id,
                    AtomEnum::WM_NAME,
                    AtomEnum::STRING,
                    last_title.as_bytes(),
                );
            }
            let _ = ChangeWindowAttributesAux::default();
            let damage = core.render_into(&mut framebuffer);
            if !damage.is_empty() {
                present_x11_damage(
                    &conn,
                    win_id,
                    gc_id,
                    depth,
                    core.win_size.0,
                    &framebuffer,
                    &damage,
                    &mut scratch_bytes,
                )?;
            }
        }
    }
}

#[cfg(all(feature = "std", unix, not(target_os = "redox")))]
#[allow(clippy::too_many_arguments)]
fn present_x11_damage<C: x11rb::connection::Connection>(
    conn: &C,
    win_id: u32,
    gc_id: u32,
    depth: u8,
    bw: u32,
    framebuffer: &[u32],
    damage: &[DamageRect],
    scratch_bytes: &mut Vec<u8>,
) -> Result<()> {
    use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};

    for d in damage {
        if d.w == 0 || d.h == 0 {
            continue;
        }
        let row_bytes = (d.w as usize) * 4;
        let max_rows_per_chunk = (48 * 1024 / row_bytes.max(4)).max(1) as u32;
        let mut ry = 0u32;
        while ry < d.h {
            let chunk_h = (d.h - ry).min(max_rows_per_chunk);
            scratch_bytes.clear();
            scratch_bytes.reserve(row_bytes * chunk_h as usize);
            for r in 0..chunk_h {
                let py = (d.y + ry + r) as usize;
                let row_start = py * bw as usize + d.x as usize;
                let row_slice = &framebuffer[row_start..row_start + d.w as usize];
                for &px in row_slice {
                    scratch_bytes.extend_from_slice(&px.to_le_bytes());
                }
            }
            conn.put_image(
                ImageFormat::Z_PIXMAP,
                win_id,
                gc_id,
                d.w as u16,
                chunk_h as u16,
                d.x as i16,
                (d.y + ry) as i16,
                0,
                depth,
                scratch_bytes,
            )?;
            ry += chunk_h;
        }
    }
    conn.flush()?;
    Ok(())
}

#[cfg(all(feature = "std", unix, not(target_os = "redox")))]
fn update_x11_mods(core: &mut BrowserCore, state: x11rb::protocol::xproto::KeyButMask) {
    use x11rb::protocol::xproto::KeyButMask;
    let mut m = core.mods & (MOD_BUTTON1 | MOD_BUTTON2 | MOD_BUTTON3);
    if state.contains(KeyButMask::SHIFT) {
        m |= MOD_SHIFT;
    }
    if state.contains(KeyButMask::CONTROL) {
        m |= MOD_CTRL;
    }
    if state.contains(KeyButMask::MOD1) {
        m |= MOD_ALT;
    }
    if state.contains(KeyButMask::MOD4) {
        m |= MOD_META;
    }
    core.mods = m;
}

#[cfg(all(feature = "std", unix, not(target_os = "redox")))]
fn lookup_x11_keysym(
    keycode: u8,
    min_keycode: u8,
    keysyms_per_keycode: usize,
    keysyms: &[u32],
    shift: bool,
) -> u32 {
    if keycode < min_keycode || keysyms_per_keycode == 0 {
        return 0;
    }
    let idx = (keycode - min_keycode) as usize * keysyms_per_keycode;
    let col = usize::from(shift && keysyms_per_keycode > 1);
    let sym = keysyms.get(idx + col).copied().unwrap_or(0);
    if sym != 0 {
        sym
    } else {
        keysyms.get(idx).copied().unwrap_or(0)
    }
}

#[cfg(all(feature = "std", unix, not(target_os = "redox")))]
fn update_keysym_modifier(core: &mut BrowserCore, keysym: u32, pressed: bool) {
    let bit = match keysym {
        0xffe1 | 0xffe2 => MOD_SHIFT,
        0xffe3 | 0xffe4 => MOD_CTRL,
        0xffe9 | 0xffea => MOD_ALT,
        0xffeb | 0xffec => MOD_META,
        _ => return,
    };
    if pressed {
        core.mods |= bit;
    } else {
        core.mods &= !bit;
    }
}

// ─── 100% Pure-Rust Redox OS Orbital Backend (`std`) ─────────────────────

#[cfg(all(feature = "std", target_os = "redox"))]
fn run_redox_orbital_loop(
    core: &mut BrowserCore,
    frames_rx: &std::sync::mpsc::Receiver<RenderEvent>,
) -> Result<()> {
    use std::io::Read;
    let (bw, bh) = core.win_size;
    let path = format!("orbital:r/-1/-1/{bw}/{bh}/ExoBrowser");
    let mut win_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?;

    let (orb_tx, orb_rx) = std::sync::mpsc::channel::<(i64, i64, i64)>();
    if let Ok(mut ev_file) = win_file.try_clone() {
        std::thread::spawn(move || {
            let mut pkt = [0u8; 24];
            while ev_file.read_exact(&mut pkt).is_ok() {
                let code = i64::from_ne_bytes(pkt[0..8].try_into().unwrap());
                let a = i64::from_ne_bytes(pkt[8..16].try_into().unwrap());
                let b = i64::from_ne_bytes(pkt[16..24].try_into().unwrap());
                if orb_tx.send((code, a, b)).is_err() {
                    break;
                }
            }
        });
    }

    let mut framebuffer = vec![0x0F0F12u32; (bw * bh) as usize];
    let mut raw_bytes = vec![0u8; (bw * bh * 4) as usize];
    let mut prev_buttons = 0i64;

    let present_orbital =
        |file: &mut std::fs::File, fb: &[u32], raw: &mut [u8]| -> std::io::Result<()> {
            for (i, &px) in fb.iter().enumerate() {
                raw[i * 4..i * 4 + 4].copy_from_slice(&(px | 0xFF00_0000).to_le_bytes());
            }
            file.write_all(raw)?;
            file.sync_data()
        };

    let _ = core.render_into(&mut framebuffer);
    present_orbital(&mut win_file, &framebuffer, &mut raw_bytes)?;

    loop {
        while let Ok((code, a, b)) = orb_rx.try_recv() {
            match code {
                1 => {
                    let ch = (a & 0xFFFF) as u32;
                    let pressed = (b & 0x8000_0000) != 0 || b > 0;
                    let keysym = match ch {
                        8 => 0xff08,
                        9 => 0xff09,
                        10 | 13 => 0xff0d,
                        27 => 0xff1b,
                        c => c,
                    };
                    let _ = core.on_key(keysym, pressed, None);
                }
                2 => {
                    core.on_pointer_move(a as i32, b as i32);
                }
                3 => {
                    for (bit, btn) in [(1i64, 1u32), (2i64, 2u32), (4i64, 3u32)] {
                        let was = (prev_buttons & bit) != 0;
                        let now = (a & bit) != 0;
                        if was != now {
                            core.on_mouse_button(core.cursor.0, core.cursor.1, btn, now);
                        }
                    }
                    prev_buttons = a;
                }
                4 => {
                    core.on_scroll(a as i32, -(b as i32) * 3);
                }
                5 => {
                    let nw = (a.max(64)) as u32;
                    let nh = (b.max(160)) as u32;
                    core.on_resize(nw, nh);
                    framebuffer.resize((nw * nh) as usize, 0x0F0F12);
                    raw_bytes.resize((nw * nh * 4) as usize, 0);
                }
                6 => return Ok(()),
                _ => {}
            }
        }

        match frames_rx.recv_timeout(Duration::from_millis(8)) {
            Ok(ev) => {
                if !core.handle_render_event(ev) {
                    return Ok(());
                }
                while let Ok(ev2) = frames_rx.try_recv() {
                    if !core.handle_render_event(ev2) {
                        return Ok(());
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }

        let damage = core.render_into(&mut framebuffer);
        if !damage.is_empty() {
            present_orbital(&mut win_file, &framebuffer, &mut raw_bytes)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_url_tests() {
        assert_eq!(canonicalize_url("https://example.com"), "https://example.com");
        assert_eq!(canonicalize_url("example.com"), "https://example.com");
        assert_eq!(canonicalize_url("http://x"), "http://x");
        assert_eq!(canonicalize_url("data:text/html,x"), "data:text/html,x");
        assert_eq!(canonicalize_url("about:blank"), "about:blank");
        assert_eq!(canonicalize_url(""), "about:blank");
        assert_eq!(canonicalize_url("  foo.com  "), "https://foo.com");
        assert_eq!(
            canonicalize_url("rust programming"),
            "https://duckduckgo.com/?q=rust+programming"
        );
        assert_eq!(canonicalize_url("localhost:8080"), "http://localhost:8080");
        assert_eq!(canonicalize_url("127.0.0.1:4433"), "http://127.0.0.1:4433");
    }

    #[test]
    fn av_clock_synchronization_logic() {
        let mut clock = AVClock::new();
        assert_eq!(clock.schedule_video(100_000), 0);

        clock.on_audio_frame(1_000_000, 20_000);
        let master = clock.current_master_pts_us();
        assert!(master >= 1_020_000);

        assert_eq!(clock.schedule_video(master), 0);
        assert_eq!(clock.schedule_video(master + 50_000), 1);
        assert_eq!(clock.schedule_video(master.saturating_sub(60_000)), 2);
    }

    #[test]
    fn browser_core_no_std_wire_and_damage() {
        let mut core = BrowserCore::new_no_std(640, 480, "https://example.com".into(), false);
        let mut fb = vec![0u32; 640 * 480];
        let damage = core.render_into(&mut fb);
        assert!(!damage.is_empty());

        // Second render with no changes should produce zero damage rects
        let damage2 = core.render_into(&mut fb);
        assert!(damage2.is_empty());

        // Simulate receiving a compressed RawFrame over the wire in #![no_std] mode
        let mut wire = Vec::new();
        Message::RawFrame {
            ts_us: 1000,
            width: 64,
            height: 64,
            stride: 256,
            format: 0,
            compression: gutted_proto::compression::ZSTD_DELTA,
            pixels: vec![0x80; 64 * 64 * 4],
        }
        .encode(&mut wire);
        let decoded = core.ingest_wire_bytes(&mut wire).unwrap();
        assert_eq!(decoded, 1);
        assert!(wire.is_empty());
        let damage3 = core.render_into(&mut fb);
        assert!(!damage3.is_empty());
    }
}
