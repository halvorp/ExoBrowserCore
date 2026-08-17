//! gutted-proto: wire codec for the gutted-browser link.
//!
//! Doctrine (see memory/wire_schema_decision.md, memory/coralos_constraints.md):
//! hand-rolled, bounds-checked, parse-don't-trust, no_std + alloc, no heavy deps.
//! Every frame is:
//!
//! ```text
//! varint TAG | varint LEN | LEN bytes of payload
//! ```
//!
//! Payloads use fixed little-endian encodings; strings are length-prefixed
//! (varint LEN + UTF-8 bytes, must round-trip cleanly). Unknown tags are
//! rejected — not skipped — because the schema *is* the contract.

#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

// ─── Tags ─────────────────────────────────────────────────────────────────
// One byte space would suffice today but varint tags keep the door open.

pub mod tag {
    pub const HELLO:          u32 = 0x01;
    pub const WELCOME:        u32 = 0x02;
    pub const RESIZE:         u32 = 0x03;
    pub const NAV:            u32 = 0x04;
    /// Client → host: history nav. 0=back, 1=forward, 2=reload.
    pub const NAV_ACTION:     u32 = 0x06;
    /// Client → host: stop page load.
    pub const STOP:           u32 = 0x07;
    pub const HEARTBEAT:      u32 = 0x05;

    pub const INPUT_POINTER:  u32 = 0x10; // motion event
    pub const INPUT_KEY:      u32 = 0x11;
    pub const INPUT_SCROLL:   u32 = 0x12;
    pub const INPUT_BUTTON:   u32 = 0x13; // pointer button press/release
    /// Client → host: set WebKit page zoom (1.0 = 100%). Ctrl+wheel and Ctrl+0.
    pub const SET_ZOOM:       u32 = 0x14;

    pub const CURSOR_STATE:   u32 = 0x30;
    /// Host → client: WebKit navigation load state changed.
    /// 0 = started, 1 = redirected, 2 = committed, 3 = finished
    pub const LOAD_STATE:     u32 = 0x31;
    /// Host → client: page <title> changed. Clients show in window title.
    pub const TITLE:          u32 = 0x32;
    /// Host → client: WebKit committed URL changed (link click, redirect,
    /// pushState). Clients repaint their URL bar.
    pub const URL_CHANGED:    u32 = 0x33;

    /// Phase 1 stopgap: whole-frame ARGB8888 payload on a video uni stream.
    /// Replaced by encoded MoQ video tracks in P1.f proper.
    pub const RAW_FRAME:      u32 = 0x40;
    /// Partial-frame update — pixels for a sub-rect of the framebuffer.
    /// Client blits into its existing texture at the given (x, y).
    pub const SUBFRAME:       u32 = 0x41;
    /// Host → client: compressed tile payload with 64-bit hash.
    pub const TILE_DATA:      u32 = 0x42;
    /// Host → client: blit tile by hash (zero payload; client uses local tile cache).
    pub const TILE_REF:       u32 = 0x43;
    /// Host → client: Opus / PCM audio frame.
    pub const AUDIO_FRAME:    u32 = 0x48;
    /// Host → client: Encoded H.264 / AV1 video track chunk.
    pub const VIDEO_CHUNK:    u32 = 0x49;
    /// Host → client: register immutable asset by 256-bit SHA256 hash.
    pub const ASSET_REGISTER: u32 = 0x50;
    /// Host → client: vector display list commands.
    pub const DRAW_COMMANDS:  u32 = 0x51;

    // Phase 3 scene protocol. Sent on the `scene` uni stream once the
    // WebKit fork exposes Nicosia's layer tree. See phase_plan memory.
    pub const LAYER_ADD:      u32 = 0x60;
    pub const LAYER_UPDATE:   u32 = 0x61;
    pub const LAYER_REMOVE:   u32 = 0x62;
    pub const SCENE_COMMIT:   u32 = 0x63;
}

pub mod audio_codec {
    pub const PCM_S16LE: u8 = 0;
    pub const OPUS:      u8 = 1;
    pub const AAC:       u8 = 2;
}

pub mod video_codec {
    pub const H264: u8 = 0;
    pub const AV1:  u8 = 1;
    pub const VP9:  u8 = 2;
    pub const VP8:  u8 = 3;
}

// ─── Errors ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Input ran out mid-message.
    UnexpectedEof,
    /// A varint exceeded 5 bytes (>32-bit) — caller lied or malicious.
    VarintOverflow,
    /// A declared length exceeded remaining bytes.
    LengthOverflow,
    /// String bytes weren't valid UTF-8.
    InvalidUtf8,
    /// A required field had a value outside the allowed set.
    InvalidValue,
    /// Payload was longer than the message's fields consumed.
    TrailingBytes,
    /// Tag not recognised by this build.
    UnknownTag(u32),
    /// A single message exceeded the configured hard cap.
    MessageTooLarge,
}

pub type Result<T> = core::result::Result<T, Error>;

/// Hard cap per single message (16 MiB). Anything larger is rejected before
/// allocation. Scene/asset chunks that need more use multi-part framing.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

// ─── Varint (LEB128, unsigned) ────────────────────────────────────────────

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

fn read_varint(buf: &mut &[u8]) -> Result<u32> {
    let mut result: u32 = 0;
    let mut shift = 0u32;
    for i in 0..5 {
        let byte = *buf.first().ok_or(Error::UnexpectedEof)?;
        *buf = &buf[1..];
        // The 5th byte (i==4) can only legitimately carry 4 payload bits;
        // catch overflow before OR-ing garbage into result.
        if i == 4 && byte & 0xF0 != 0 {
            return Err(Error::VarintOverflow);
        }
        result |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
    Err(Error::VarintOverflow)
}

// ─── Primitive read/write helpers ─────────────────────────────────────────

fn write_u16(out: &mut Vec<u8>, v: u16) { out.extend_from_slice(&v.to_le_bytes()); }
fn write_u32(out: &mut Vec<u8>, v: u32) { out.extend_from_slice(&v.to_le_bytes()); }
fn write_u64(out: &mut Vec<u8>, v: u64) { out.extend_from_slice(&v.to_le_bytes()); }
fn write_i32(out: &mut Vec<u8>, v: i32) { out.extend_from_slice(&v.to_le_bytes()); }
fn write_u8_(out: &mut Vec<u8>, v: u8)  { out.push(v); }

fn read_u16(buf: &mut &[u8]) -> Result<u16> {
    let arr: [u8; 2] = buf.get(..2).ok_or(Error::UnexpectedEof)?.try_into().unwrap();
    *buf = &buf[2..];
    Ok(u16::from_le_bytes(arr))
}
fn read_u32(buf: &mut &[u8]) -> Result<u32> {
    let arr: [u8; 4] = buf.get(..4).ok_or(Error::UnexpectedEof)?.try_into().unwrap();
    *buf = &buf[4..];
    Ok(u32::from_le_bytes(arr))
}
fn read_u64(buf: &mut &[u8]) -> Result<u64> {
    let arr: [u8; 8] = buf.get(..8).ok_or(Error::UnexpectedEof)?.try_into().unwrap();
    *buf = &buf[8..];
    Ok(u64::from_le_bytes(arr))
}
fn read_i32(buf: &mut &[u8]) -> Result<i32> {
    let arr: [u8; 4] = buf.get(..4).ok_or(Error::UnexpectedEof)?.try_into().unwrap();
    *buf = &buf[4..];
    Ok(i32::from_le_bytes(arr))
}
fn read_u8_(buf: &mut &[u8]) -> Result<u8> {
    let b = *buf.first().ok_or(Error::UnexpectedEof)?;
    *buf = &buf[1..];
    Ok(b)
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    write_varint(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

fn read_str(buf: &mut &[u8]) -> Result<String> {
    let len = read_varint(buf)? as usize;
    let bytes = buf.get(..len).ok_or(Error::LengthOverflow)?;
    *buf = &buf[len..];
    core::str::from_utf8(bytes).map(String::from).map_err(|_| Error::InvalidUtf8)
}

// ─── Message enum ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Client → host, first message on ctrl.
    Hello {
        proto_version: u32,
        viewport_w: u16,
        viewport_h: u16,
        /// Device pixel ratio × 100 (e.g. 200 = 2.0). Fixed-point keeps codec integer.
        dpr_hundredths: u16,
        client_name: String,
        capabilities: u32, // bitfield; see caps::*
    },
    /// Host → client, response to Hello.
    Welcome {
        proto_version: u32,
        session_id: u64,
        features: u32,     // bitfield
        cursor_track_id: u32, // >0 = a MoQ track will publish cursor updates
        /// The URL WebKit is currently on. Empty if not yet loaded.
        /// Lets the client's address bar reflect actual state immediately.
        current_url: String,
    },
    Resize {
        viewport_w: u16,
        viewport_h: u16,
        dpr_hundredths: u16,
    },
    Nav {
        url: String,
    },
    /// Client → host: history nav. See `tag::NAV_ACTION` for values.
    NavAction { action: u8 },
    /// Client → host: stop loading page.
    Stop,
    Heartbeat {
        ts_us: u64,
    },
    /// Pointer motion (no button state change). `modifiers` uses the WPE
    /// bitmask from wpe/input.h — keyboard bits 0..3, pointer-held bits
    /// 20..24.
    InputPointer {
        ts_us: u64,
        x: i32,
        y: i32,
        modifiers: u32,
    },
    /// Pointer button press/release. `button` = 1..=5 as WPE expects.
    InputButton {
        ts_us: u64,
        x: i32,
        y: i32,
        button: u32,
        pressed: bool,
        modifiers: u32,
    },
    InputKey {
        ts_us: u64,
        keycode: u32,     // XKB keysym
        mods: u32,        // XKB mod mask
        down: bool,
    },
    /// Client-side composited scroll intent. `layer_id`=0 means root layer.
    InputScroll {
        ts_us: u64,
        layer_id: u32,
        dx_units: i32,    // wheel units (1 unit ≈ 120 in Windows terms)
        dy_units: i32,
        phase: ScrollPhase,
    },
    /// Client → host: change page zoom. `level_milli` = zoom * 1000
    /// (e.g. 1000 = 100%, 1250 = 125%). Host clamps to [250, 5000].
    SetZoom { level_milli: u32 },
    /// Host → client: cursor shape changed. `image_ref` = asset hash, 0 = named.
    CursorState {
        shape: CursorShape,
        hotspot_x: u16,
        hotspot_y: u16,
        image_ref: u64,
    },
    /// Host → client: navigation load state. See `tag::LOAD_STATE` for values.
    LoadState { state: u8 },
    /// Host → client: page <title> changed. Client shows it in window chrome.
    Title { title: String },
    /// Host → client: WebKit committed URL changed. Client refreshes URL bar
    /// (respect user editing state — don't clobber typing).
    UrlChanged { url: String },
    /// Phase 3: introduce a new layer into the scene tree.
    /// `parent`=0 means attach to the root.
    LayerAdd {
        id: u32,
        parent: u32,
        kind: LayerKind,
        size: (u16, u16),
        transform: Transform,
        opacity: u8,          // 0..=255
        content: ContentRef,
    },
    /// Phase 3: mutate an existing layer. `mask` says which optional
    /// fields are meaningful; absent fields keep their prior value.
    LayerUpdate {
        id: u32,
        mask: u32,
        transform: Transform, // valid iff mask & TRANSFORM
        opacity: u8,          // valid iff mask & OPACITY
        size: (u16, u16),     // valid iff mask & SIZE
        content: ContentRef,  // valid iff mask & CONTENT_REF (None disables)
        damage: (u16, u16, u16, u16), // (x,y,w,h) iff mask & DAMAGE
    },
    /// Phase 3: remove a layer (and by convention its subtree).
    LayerRemove { id: u32 },
    /// Phase 3: atomic swap — apply all preceding deltas since the last
    /// COMMIT as one frame. Version number lets the client dedupe/order.
    SceneCommit { version: u64 },
    /// Host → client: sub-rect update to the current framebuffer texture.
    /// `pixels` (uncompressed) is `stride * h` bytes; client blits at
    /// `(x, y)` inside the video texture without touching other pixels.
    Subframe {
        ts_us:   u64,
        x:       u16,
        y:       u16,
        w:       u16,
        h:       u16,
        stride:  u32,
        format:  u32,
        compression: u8,
        pixels:  Vec<u8>,
    },
    /// Host → client: one full ARGB8888 frame on the video uni stream.
    /// Pixels are BGRA in memory (wl_shm ARGB8888) and span `stride * height`
    /// bytes AFTER decompression. The wire may compress the pixel payload;
    /// see `compression`. Consumers accept `stride >= width * 4` (padding).
    RawFrame {
        ts_us:   u64,
        width:   u16,
        height:  u16,
        stride:  u32,
        format:  u32,
        /// 0 = raw bytes, 1 = zstd.
        compression: u8,
        /// UNCOMPRESSED pixel bytes; length must equal stride * height.
        pixels:  Vec<u8>,
    },
    /// Host → client: compressed tile payload with 64-bit hash.
    TileData {
        hash: u64,
        w: u16,
        h: u16,
        stride: u32,
        format: u32,
        compression: u8,
        pixels: Vec<u8>,
    },
    /// Host → client: blit cached tile at (x, y).
    TileRef {
        ts_us: u64,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        hash: u64,
    },
    /// Host → client: register immutable asset by SHA256 hash.
    AssetRegister {
        hash: [u8; 32],
        kind: u8,
        data: Vec<u8>,
    },
    /// Host → client: vector display list commands.
    DrawCommands {
        ts_us: u64,
        layer_id: u32,
        commands: Vec<DrawCommand>,
    },
    /// Host → client: Opus / PCM audio frame.
    AudioFrame {
        pts_us: u64,
        codec: u8,
        channels: u8,
        sample_rate: u32,
        data: Vec<u8>,
    },
    /// Host → client: encoded video track chunk (H.264 / AV1 / VP9).
    VideoChunk {
        pts_us: u64,
        duration_us: u32,
        is_keyframe: bool,
        codec: u8,
        layer_id: u32,
        data: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrawCommand {
    FillRect { x: i32, y: i32, w: u32, h: u32, rgba: u32 },
    StrokeRect { x: i32, y: i32, w: u32, h: u32, rgba: u32, line_width: u16 },
    DrawText { x: i32, y: i32, font_size: u16, rgba: u32, text: String },
    DrawImage { x: i32, y: i32, w: u32, h: u32, asset_hash: [u8; 32] },
    SetClip { x: i32, y: i32, w: u32, h: u32 },
    ClearClip,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollPhase { Begin = 1, Update = 2, End = 3, Momentum = 4 }

impl ScrollPhase {
    fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            1 => Self::Begin, 2 => Self::Update, 3 => Self::End, 4 => Self::Momentum,
            _ => return Err(Error::InvalidValue),
        })
    }
}

/// What a Phase-3 layer holds. `Solid` = flat color rect, useful for
/// scroll containers and root background. `Video` = subscribe to a MoQ
/// track for pixels. `Tile` = raster tiles fetched by hash. `Vector` =
/// display list (font glyphs / Skia ops) also fetched by hash.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    Solid  = 0,
    Video  = 1,
    Tile   = 2,
    Vector = 3,
}
impl LayerKind {
    fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            0 => Self::Solid, 1 => Self::Video, 2 => Self::Tile, 3 => Self::Vector,
            _ => return Err(Error::InvalidValue),
        })
    }
}

/// Discriminant for content_ref. `TrackId` = subscribe to this MoQ track.
/// `AssetHash` = fetch this immutable asset from the client-side asset
/// cache. `Solid` = RGBA8 color; no external reference needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentRef {
    None,
    Solid(u32),      // packed RGBA8
    TrackId(u64),
    AssetHash([u8; 32]),
}

/// Fixed-point 3×2 affine transform matrix, sfixed 24.8 in i32 units.
/// Sending as i32×6 lets us pass sub-pixel updates without allocating a
/// float64 payload. Applied as [ [a c e] [b d f] [0 0 1] ].
pub type Transform = [i32; 6];
pub const TRANSFORM_IDENTITY: Transform = [1 << 8, 0, 0, 1 << 8, 0, 0];

/// Which fields a LAYER_UPDATE actually changed. Bitfield keeps the wire
/// tight — a scroll produces one update with only TRANSFORM set.
#[allow(dead_code)]
pub mod layer_mask {
    pub const TRANSFORM:   u32 = 1 << 0;
    pub const OPACITY:     u32 = 1 << 1;
    pub const SIZE:        u32 = 1 << 2;
    pub const CONTENT_REF: u32 = 1 << 3;
    pub const DAMAGE:      u32 = 1 << 4;
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Default = 0, Pointer = 1, Text = 2, Progress = 3, Wait = 4, Crosshair = 5,
    Move = 6, NotAllowed = 7, Grab = 8, Grabbing = 9,
    ResizeEw = 10, ResizeNs = 11, ResizeNesw = 12, ResizeNwse = 13,
    Custom = 255,
}

impl CursorShape {
    fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            0 => Self::Default, 1 => Self::Pointer, 2 => Self::Text, 3 => Self::Progress,
            4 => Self::Wait, 5 => Self::Crosshair, 6 => Self::Move, 7 => Self::NotAllowed,
            8 => Self::Grab, 9 => Self::Grabbing, 10 => Self::ResizeEw, 11 => Self::ResizeNs,
            12 => Self::ResizeNesw, 13 => Self::ResizeNwse, 255 => Self::Custom,
            _ => return Err(Error::InvalidValue),
        })
    }
}

pub mod caps {
    pub const H264:          u32 = 1 << 0;
    pub const AV1:           u32 = 1 << 1;
    pub const OPUS:          u32 = 1 << 2;
    pub const DMABUF_IMPORT: u32 = 1 << 3;
    pub const CLIENT_SCROLL: u32 = 1 << 4;
}

pub const PROTO_VERSION: u32 = 1;

// ─── Encode ──────────────────────────────────────────────────────────────-

impl Message {
    /// Encode as a single wire frame: tag | len | payload.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        // Reserve space by writing tag then a placeholder for length.
        write_varint(out, self.tag());
        // We can't know payload len until it's written; emit into a temp
        // buffer to keep the allocator simple. For a small hot path we
        // could size-estimate — later.
        let mut payload = Vec::with_capacity(64);
        self.encode_payload(&mut payload);
        write_varint(out, payload.len() as u32);
        out.extend_from_slice(&payload);
        debug_assert!(out.len() > start);
    }

    fn tag(&self) -> u32 {
        match self {
            Message::Hello { .. }        => tag::HELLO,
            Message::Welcome { .. }      => tag::WELCOME,
            Message::Resize { .. }       => tag::RESIZE,
            Message::Nav { .. }          => tag::NAV,
            Message::NavAction { .. }    => tag::NAV_ACTION,
            Message::Stop                => tag::STOP,
            Message::Heartbeat { .. }    => tag::HEARTBEAT,
            Message::InputPointer { .. } => tag::INPUT_POINTER,
            Message::InputButton { .. }  => tag::INPUT_BUTTON,
            Message::InputKey { .. }     => tag::INPUT_KEY,
            Message::InputScroll { .. }  => tag::INPUT_SCROLL,
            Message::SetZoom { .. }      => tag::SET_ZOOM,
            Message::CursorState { .. }  => tag::CURSOR_STATE,
            Message::LoadState { .. }    => tag::LOAD_STATE,
            Message::Title { .. }        => tag::TITLE,
            Message::UrlChanged { .. }   => tag::URL_CHANGED,
            Message::RawFrame { .. }     => tag::RAW_FRAME,
            Message::Subframe { .. }     => tag::SUBFRAME,
            Message::TileData { .. }     => tag::TILE_DATA,
            Message::TileRef { .. }      => tag::TILE_REF,
            Message::AssetRegister { .. } => tag::ASSET_REGISTER,
            Message::DrawCommands { .. } => tag::DRAW_COMMANDS,
            Message::AudioFrame { .. }   => tag::AUDIO_FRAME,
            Message::VideoChunk { .. }   => tag::VIDEO_CHUNK,
            Message::LayerAdd { .. }     => tag::LAYER_ADD,
            Message::LayerUpdate { .. }  => tag::LAYER_UPDATE,
            Message::LayerRemove { .. }  => tag::LAYER_REMOVE,
            Message::SceneCommit { .. }  => tag::SCENE_COMMIT,
        }
    }

    fn encode_payload(&self, out: &mut Vec<u8>) {
        match self {
            Message::Hello { proto_version, viewport_w, viewport_h, dpr_hundredths, client_name, capabilities } => {
                write_u32(out, *proto_version);
                write_u16(out, *viewport_w);
                write_u16(out, *viewport_h);
                write_u16(out, *dpr_hundredths);
                write_u32(out, *capabilities);
                write_str(out, client_name);
            }
            Message::Welcome { proto_version, session_id, features, cursor_track_id, current_url } => {
                write_u32(out, *proto_version);
                write_u64(out, *session_id);
                write_u32(out, *features);
                write_u32(out, *cursor_track_id);
                write_str(out, current_url);
            }
            Message::Resize { viewport_w, viewport_h, dpr_hundredths } => {
                write_u16(out, *viewport_w);
                write_u16(out, *viewport_h);
                write_u16(out, *dpr_hundredths);
            }
            Message::Nav { url } => write_str(out, url),
            Message::NavAction { action } => write_u8_(out, *action),
            Message::Stop => {}
            Message::Heartbeat { ts_us } => write_u64(out, *ts_us),
            Message::InputPointer { ts_us, x, y, modifiers } => {
                write_u64(out, *ts_us);
                write_i32(out, *x);
                write_i32(out, *y);
                write_u32(out, *modifiers);
            }
            Message::InputButton { ts_us, x, y, button, pressed, modifiers } => {
                write_u64(out, *ts_us);
                write_i32(out, *x);
                write_i32(out, *y);
                write_u32(out, *button);
                write_u8_(out, if *pressed { 1 } else { 0 });
                write_u32(out, *modifiers);
            }
            Message::InputKey { ts_us, keycode, mods, down } => {
                write_u64(out, *ts_us);
                write_u32(out, *keycode);
                write_u32(out, *mods);
                write_u8_(out, if *down { 1 } else { 0 });
            }
            Message::InputScroll { ts_us, layer_id, dx_units, dy_units, phase } => {
                write_u64(out, *ts_us);
                write_u32(out, *layer_id);
                write_i32(out, *dx_units);
                write_i32(out, *dy_units);
                write_u8_(out, *phase as u8);
            }
            Message::SetZoom { level_milli } => write_u32(out, *level_milli),
            Message::CursorState { shape, hotspot_x, hotspot_y, image_ref } => {
                write_u8_(out, *shape as u8);
                write_u16(out, *hotspot_x);
                write_u16(out, *hotspot_y);
                write_u64(out, *image_ref);
            }
            Message::LoadState { state } => write_u8_(out, *state),
            Message::Title { title }     => write_str(out, title),
            Message::UrlChanged { url }  => write_str(out, url),
            Message::RawFrame { ts_us, width, height, stride, format, compression, pixels } => {
                write_u64(out, *ts_us);
                write_u16(out, *width);
                write_u16(out, *height);
                write_u32(out, *stride);
                write_u32(out, *format);
                write_u8_(out, *compression);
                write_payload(out, *compression, pixels);
            }
            Message::LayerAdd { id, parent, kind, size, transform, opacity, content } => {
                write_u32(out, *id);
                write_u32(out, *parent);
                write_u8_(out, *kind as u8);
                write_u16(out, size.0);
                write_u16(out, size.1);
                for c in transform { write_i32(out, *c); }
                write_u8_(out, *opacity);
                write_content_ref(out, content);
            }
            Message::LayerUpdate { id, mask, transform, opacity, size, content, damage } => {
                write_u32(out, *id);
                write_u32(out, *mask);
                if mask & layer_mask::TRANSFORM != 0 { for c in transform { write_i32(out, *c); } }
                if mask & layer_mask::OPACITY   != 0 { write_u8_(out, *opacity); }
                if mask & layer_mask::SIZE      != 0 { write_u16(out, size.0); write_u16(out, size.1); }
                if mask & layer_mask::CONTENT_REF != 0 { write_content_ref(out, content); }
                if mask & layer_mask::DAMAGE    != 0 {
                    write_u16(out, damage.0); write_u16(out, damage.1);
                    write_u16(out, damage.2); write_u16(out, damage.3);
                }
            }
            Message::LayerRemove { id } => write_u32(out, *id),
            Message::SceneCommit { version } => write_u64(out, *version),
            Message::Subframe { ts_us, x, y, w, h, stride, format, compression, pixels } => {
                write_u64(out, *ts_us);
                write_u16(out, *x);
                write_u16(out, *y);
                write_u16(out, *w);
                write_u16(out, *h);
                write_u32(out, *stride);
                write_u32(out, *format);
                write_u8_(out, *compression);
                write_payload(out, *compression, pixels);
            }
            Message::TileData { hash, w, h, stride, format, compression, pixels } => {
                write_u64(out, *hash);
                write_u16(out, *w);
                write_u16(out, *h);
                write_u32(out, *stride);
                write_u32(out, *format);
                write_u8_(out, *compression);
                write_payload(out, *compression, pixels);
            }
            Message::TileRef { ts_us, x, y, w, h, hash } => {
                write_u64(out, *ts_us);
                write_u16(out, *x);
                write_u16(out, *y);
                write_u16(out, *w);
                write_u16(out, *h);
                write_u64(out, *hash);
            }
            Message::AssetRegister { hash, kind, data } => {
                out.extend_from_slice(hash);
                write_u8_(out, *kind);
                write_varint(out, data.len() as u32);
                out.extend_from_slice(data);
            }
            Message::DrawCommands { ts_us, layer_id, commands } => {
                write_u64(out, *ts_us);
                write_u32(out, *layer_id);
                write_varint(out, commands.len() as u32);
                for cmd in commands { write_draw_cmd(out, cmd); }
            }
            Message::AudioFrame { pts_us, codec, channels, sample_rate, data } => {
                write_u64(out, *pts_us);
                write_u8_(out, *codec);
                write_u8_(out, *channels);
                write_u32(out, *sample_rate);
                write_varint(out, data.len() as u32);
                out.extend_from_slice(data);
            }
            Message::VideoChunk { pts_us, duration_us, is_keyframe, codec, layer_id, data } => {
                write_u64(out, *pts_us);
                write_u32(out, *duration_us);
                write_u8_(out, if *is_keyframe { 1 } else { 0 });
                write_u8_(out, *codec);
                write_u32(out, *layer_id);
                write_varint(out, data.len() as u32);
                out.extend_from_slice(data);
            }
        }
    }

    /// Read exactly one message off `buf`, advancing `buf` past it.
    /// Returns `Ok(None)` if `buf` doesn't yet hold a full frame.
    pub fn decode(buf: &mut &[u8]) -> Result<Option<Self>> {
        let mut probe = *buf;
        let tag = match read_varint(&mut probe) {
            Ok(t) => t,
            Err(Error::UnexpectedEof) => return Ok(None),
            Err(e) => return Err(e),
        };
        let len = match read_varint(&mut probe) {
            Ok(l) => l as usize,
            Err(Error::UnexpectedEof) => return Ok(None),
            Err(e) => return Err(e),
        };
        if len > MAX_MESSAGE_BYTES { return Err(Error::MessageTooLarge); }
        if probe.len() < len { return Ok(None); }
        let payload = &probe[..len];
        let rest    = &probe[len..];

        let msg = Self::decode_payload(tag, payload)?;
        *buf = rest;
        Ok(Some(msg))
    }

    fn decode_payload(tag: u32, mut payload: &[u8]) -> Result<Self> {
        let p = &mut payload;
        let msg = match tag {
            tag::HELLO => {
                let proto_version = read_u32(p)?;
                let viewport_w = read_u16(p)?;
                let viewport_h = read_u16(p)?;
                let dpr_hundredths = read_u16(p)?;
                let capabilities = read_u32(p)?;
                let client_name = read_str(p)?;
                Message::Hello { proto_version, viewport_w, viewport_h, dpr_hundredths, client_name, capabilities }
            }
            tag::WELCOME => Message::Welcome {
                proto_version: read_u32(p)?,
                session_id: read_u64(p)?,
                features: read_u32(p)?,
                cursor_track_id: read_u32(p)?,
                current_url: read_str(p)?,
            },
            tag::RESIZE => Message::Resize {
                viewport_w: read_u16(p)?,
                viewport_h: read_u16(p)?,
                dpr_hundredths: read_u16(p)?,
            },
            tag::NAV => Message::Nav { url: read_str(p)? },
            tag::NAV_ACTION => Message::NavAction { action: read_u8_(p)? },
            tag::STOP => Message::Stop,
            tag::HEARTBEAT => Message::Heartbeat { ts_us: read_u64(p)? },
            tag::INPUT_POINTER => Message::InputPointer {
                ts_us: read_u64(p)?,
                x: read_i32(p)?,
                y: read_i32(p)?,
                modifiers: read_u32(p)?,
            },
            tag::INPUT_BUTTON => Message::InputButton {
                ts_us: read_u64(p)?,
                x: read_i32(p)?,
                y: read_i32(p)?,
                button: read_u32(p)?,
                pressed: read_u8_(p)? != 0,
                modifiers: read_u32(p)?,
            },
            tag::INPUT_KEY => Message::InputKey {
                ts_us: read_u64(p)?,
                keycode: read_u32(p)?,
                mods: read_u32(p)?,
                down: read_u8_(p)? != 0,
            },
            tag::INPUT_SCROLL => Message::InputScroll {
                ts_us: read_u64(p)?,
                layer_id: read_u32(p)?,
                dx_units: read_i32(p)?,
                dy_units: read_i32(p)?,
                phase: ScrollPhase::from_u8(read_u8_(p)?)?,
            },
            tag::SET_ZOOM => Message::SetZoom { level_milli: read_u32(p)? },
            tag::CURSOR_STATE => Message::CursorState {
                shape: CursorShape::from_u8(read_u8_(p)?)?,
                hotspot_x: read_u16(p)?,
                hotspot_y: read_u16(p)?,
                image_ref: read_u64(p)?,
            },
            tag::LOAD_STATE => Message::LoadState { state: read_u8_(p)? },
            tag::TITLE      => Message::Title { title: read_str(p)? },
            tag::URL_CHANGED => Message::UrlChanged { url: read_str(p)? },
            tag::RAW_FRAME => {
                let ts_us  = read_u64(p)?;
                let width  = read_u16(p)?;
                let height = read_u16(p)?;
                let stride = read_u32(p)?;
                let format = read_u32(p)?;
                let compression = read_u8_(p)?;
                let pixels = read_payload(p, compression,
                    (stride as usize).saturating_mul(height as usize))?;
                Message::RawFrame { ts_us, width, height, stride, format, compression, pixels }
            }
            tag::LAYER_ADD => {
                let id       = read_u32(p)?;
                let parent   = read_u32(p)?;
                let kind     = LayerKind::from_u8(read_u8_(p)?)?;
                let size     = (read_u16(p)?, read_u16(p)?);
                let mut transform: Transform = [0; 6];
                for c in transform.iter_mut() { *c = read_i32(p)?; }
                let opacity  = read_u8_(p)?;
                let content  = read_content_ref(p)?;
                Message::LayerAdd { id, parent, kind, size, transform, opacity, content }
            }
            tag::LAYER_UPDATE => {
                let id   = read_u32(p)?;
                let mask = read_u32(p)?;
                let mut transform: Transform = TRANSFORM_IDENTITY;
                if mask & layer_mask::TRANSFORM != 0 { for c in transform.iter_mut() { *c = read_i32(p)?; } }
                let opacity = if mask & layer_mask::OPACITY != 0 { read_u8_(p)? } else { 255 };
                let size    = if mask & layer_mask::SIZE != 0 { (read_u16(p)?, read_u16(p)?) } else { (0, 0) };
                let content = if mask & layer_mask::CONTENT_REF != 0 { read_content_ref(p)? } else { ContentRef::None };
                let damage  = if mask & layer_mask::DAMAGE != 0 {
                    (read_u16(p)?, read_u16(p)?, read_u16(p)?, read_u16(p)?)
                } else { (0, 0, 0, 0) };
                Message::LayerUpdate { id, mask, transform, opacity, size, content, damage }
            }
            tag::LAYER_REMOVE => Message::LayerRemove { id: read_u32(p)? },
            tag::SCENE_COMMIT => Message::SceneCommit { version: read_u64(p)? },
            tag::SUBFRAME => {
                let ts_us  = read_u64(p)?;
                let x      = read_u16(p)?;
                let y      = read_u16(p)?;
                let w      = read_u16(p)?;
                let h      = read_u16(p)?;
                let stride = read_u32(p)?;
                let format = read_u32(p)?;
                let compression = read_u8_(p)?;
                let pixels = read_payload(p, compression,
                    (stride as usize).saturating_mul(h as usize))?;
                Message::Subframe { ts_us, x, y, w, h, stride, format, compression, pixels }
            }
            tag::TILE_DATA => {
                let hash   = read_u64(p)?;
                let w      = read_u16(p)?;
                let h      = read_u16(p)?;
                let stride = read_u32(p)?;
                let format = read_u32(p)?;
                let compression = read_u8_(p)?;
                let pixels = read_payload(p, compression, (stride as usize).saturating_mul(h as usize))?;
                Message::TileData { hash, w, h, stride, format, compression, pixels }
            }
            tag::TILE_REF => {
                let ts_us = read_u64(p)?;
                let x     = read_u16(p)?;
                let y     = read_u16(p)?;
                let w     = read_u16(p)?;
                let h     = read_u16(p)?;
                let hash  = read_u64(p)?;
                Message::TileRef { ts_us, x, y, w, h, hash }
            }
            tag::ASSET_REGISTER => {
                let bytes = p.get(..32).ok_or(Error::UnexpectedEof)?;
                let mut hash = [0u8; 32];
                hash.copy_from_slice(bytes);
                *p = &p[32..];
                let kind = read_u8_(p)?;
                let len = read_varint(p)? as usize;
                let data = p.get(..len).ok_or(Error::LengthOverflow)?.to_vec();
                *p = &p[len..];
                Message::AssetRegister { hash, kind, data }
            }
            tag::DRAW_COMMANDS => {
                let ts_us    = read_u64(p)?;
                let layer_id = read_u32(p)?;
                let count    = read_varint(p)? as usize;
                let mut commands = Vec::with_capacity(count);
                for _ in 0..count { commands.push(read_draw_cmd(p)?); }
                Message::DrawCommands { ts_us, layer_id, commands }
            }
            tag::AUDIO_FRAME => {
                let pts_us      = read_u64(p)?;
                let codec       = read_u8_(p)?;
                let channels    = read_u8_(p)?;
                let sample_rate = read_u32(p)?;
                let len         = read_varint(p)? as usize;
                let data        = p.get(..len).ok_or(Error::LengthOverflow)?.to_vec();
                *p = &p[len..];
                Message::AudioFrame { pts_us, codec, channels, sample_rate, data }
            }
            tag::VIDEO_CHUNK => {
                let pts_us      = read_u64(p)?;
                let duration_us = read_u32(p)?;
                let is_keyframe = read_u8_(p)? != 0;
                let codec       = read_u8_(p)?;
                let layer_id    = read_u32(p)?;
                let len         = read_varint(p)? as usize;
                let data        = p.get(..len).ok_or(Error::LengthOverflow)?.to_vec();
                *p = &p[len..];
                Message::VideoChunk { pts_us, duration_us, is_keyframe, codec, layer_id, data }
            }
            other => return Err(Error::UnknownTag(other)),
        };
        if !p.is_empty() { return Err(Error::TrailingBytes); }
        Ok(msg)
    }
}

// ─── ContentRef codec ────────────────────────────────────────────────────

fn write_content_ref(out: &mut Vec<u8>, r: &ContentRef) {
    match r {
        ContentRef::None            => out.push(0),
        ContentRef::Solid(rgba)     => { out.push(1); write_u32(out, *rgba); }
        ContentRef::TrackId(t)      => { out.push(2); write_u64(out, *t); }
        ContentRef::AssetHash(h)    => { out.push(3); out.extend_from_slice(h); }
    }
}
fn read_content_ref(p: &mut &[u8]) -> Result<ContentRef> {
    Ok(match read_u8_(p)? {
        0 => ContentRef::None,
        1 => ContentRef::Solid(read_u32(p)?),
        2 => ContentRef::TrackId(read_u64(p)?),
        3 => {
            let bytes = p.get(..32).ok_or(Error::UnexpectedEof)?;
            let mut h = [0u8; 32];
            h.copy_from_slice(bytes);
            *p = &p[32..];
            ContentRef::AssetHash(h)
        }
        _ => return Err(Error::InvalidValue),
    })
}

// ─── DrawCommand codec ───────────────────────────────────────────────────

fn write_draw_cmd(out: &mut Vec<u8>, cmd: &DrawCommand) {
    match cmd {
        DrawCommand::FillRect { x, y, w, h, rgba } => {
            out.push(1);
            write_i32(out, *x); write_i32(out, *y);
            write_u32(out, *w); write_u32(out, *h);
            write_u32(out, *rgba);
        }
        DrawCommand::StrokeRect { x, y, w, h, rgba, line_width } => {
            out.push(2);
            write_i32(out, *x); write_i32(out, *y);
            write_u32(out, *w); write_u32(out, *h);
            write_u32(out, *rgba); write_u16(out, *line_width);
        }
        DrawCommand::DrawText { x, y, font_size, rgba, text } => {
            out.push(3);
            write_i32(out, *x); write_i32(out, *y);
            write_u16(out, *font_size); write_u32(out, *rgba);
            write_str(out, text);
        }
        DrawCommand::DrawImage { x, y, w, h, asset_hash } => {
            out.push(4);
            write_i32(out, *x); write_i32(out, *y);
            write_u32(out, *w); write_u32(out, *h);
            out.extend_from_slice(asset_hash);
        }
        DrawCommand::SetClip { x, y, w, h } => {
            out.push(5);
            write_i32(out, *x); write_i32(out, *y);
            write_u32(out, *w); write_u32(out, *h);
        }
        DrawCommand::ClearClip => out.push(6),
    }
}

fn read_draw_cmd(p: &mut &[u8]) -> Result<DrawCommand> {
    Ok(match read_u8_(p)? {
        1 => DrawCommand::FillRect {
            x: read_i32(p)?, y: read_i32(p)?,
            w: read_u32(p)?, h: read_u32(p)?,
            rgba: read_u32(p)?,
        },
        2 => DrawCommand::StrokeRect {
            x: read_i32(p)?, y: read_i32(p)?,
            w: read_u32(p)?, h: read_u32(p)?,
            rgba: read_u32(p)?, line_width: read_u16(p)?,
        },
        3 => DrawCommand::DrawText {
            x: read_i32(p)?, y: read_i32(p)?,
            font_size: read_u16(p)?, rgba: read_u32(p)?,
            text: read_str(p)?,
        },
        4 => {
            let x = read_i32(p)?; let y = read_i32(p)?;
            let w = read_u32(p)?; let h = read_u32(p)?;
            let bytes = p.get(..32).ok_or(Error::UnexpectedEof)?;
            let mut asset_hash = [0u8; 32];
            asset_hash.copy_from_slice(bytes);
            *p = &p[32..];
            DrawCommand::DrawImage { x, y, w, h, asset_hash }
        }
        5 => DrawCommand::SetClip {
            x: read_i32(p)?, y: read_i32(p)?,
            w: read_u32(p)?, h: read_u32(p)?,
        },
        6 => DrawCommand::ClearClip,
        _ => return Err(Error::InvalidValue),
    })
}

// ─── Pixel payload encode/decode (shared by RawFrame + Subframe) ─────────

pub mod compression {
    pub const RAW: u8 = 0;
    pub const ZSTD: u8 = 1;
    pub const ZSTD_DELTA: u8 = 2;
}

#[cfg(feature = "std")]
fn write_payload(out: &mut Vec<u8>, compression: u8, pixels: &[u8]) {
    match compression {
        0 => out.extend_from_slice(pixels),
        1 => out.extend_from_slice(&zstd_encode(pixels)),
        2 => {
            let filtered = spatial_filter_sub(pixels);
            out.extend_from_slice(&zstd_encode(&filtered));
        }
        other => panic!("gutted-proto: unknown compression {other} on encode"),
    }
}
#[cfg(not(feature = "std"))]
fn write_payload(out: &mut Vec<u8>, compression: u8, pixels: &[u8]) {
    if compression != 0 {
        panic!("gutted-proto: compression {compression} requires std feature");
    }
    out.extend_from_slice(pixels);
}

fn read_payload(p: &mut &[u8], compression: u8, expected_len: usize) -> Result<Vec<u8>> {
    let wire = p.to_vec();
    *p = &[];
    let pixels = match compression {
        0 => wire,
        #[cfg(feature = "std")]
        1 => zstd_decode(&wire, expected_len)?,
        #[cfg(feature = "std")]
        2 => {
            let filtered = zstd_decode(&wire, expected_len)?;
            spatial_unfilter_sub(&filtered)
        }
        _ => return Err(Error::InvalidValue),
    };
    if pixels.len() != expected_len { return Err(Error::InvalidValue); }
    Ok(pixels)
}

fn spatial_filter_sub(pixels: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; pixels.len()];
    let n = pixels.len() / 4;
    if n == 0 { return out; }
    out[0..4].copy_from_slice(&pixels[0..4]);
    for i in 1..n {
        let prev = &pixels[(i - 1) * 4 .. (i - 1) * 4 + 4];
        let curr = &pixels[i * 4 .. i * 4 + 4];
        out[i * 4]     = curr[0].wrapping_sub(prev[0]);
        out[i * 4 + 1] = curr[1].wrapping_sub(prev[1]);
        out[i * 4 + 2] = curr[2].wrapping_sub(prev[2]);
        out[i * 4 + 3] = curr[3].wrapping_sub(prev[3]);
    }
    out
}

fn spatial_unfilter_sub(filtered: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; filtered.len()];
    let n = filtered.len() / 4;
    if n == 0 { return out; }
    out[0..4].copy_from_slice(&filtered[0..4]);
    for i in 1..n {
        let prev0 = out[(i - 1) * 4];
        let prev1 = out[(i - 1) * 4 + 1];
        let prev2 = out[(i - 1) * 4 + 2];
        let prev3 = out[(i - 1) * 4 + 3];

        let curr = &filtered[i * 4 .. i * 4 + 4];
        out[i * 4]     = curr[0].wrapping_add(prev0);
        out[i * 4 + 1] = curr[1].wrapping_add(prev1);
        out[i * 4 + 2] = curr[2].wrapping_add(prev2);
        out[i * 4 + 3] = curr[3].wrapping_add(prev3);
    }
    out
}

// ─── zstd helpers (std-only) ─────────────────────────────────────────────

#[cfg(feature = "std")]
fn zstd_encode(bytes: &[u8]) -> Vec<u8> {
    use std::cell::RefCell;
    thread_local! {
        static COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> = RefCell::new(None);
    }
    COMPRESSOR.with(|c| {
        let mut borrow = c.borrow_mut();
        if borrow.is_none() {
            *borrow = zstd::bulk::Compressor::new(1).ok();
        }
        if let Some(comp) = borrow.as_mut() {
            if let Ok(res) = comp.compress(bytes) {
                return res;
            }
        }
        zstd::encode_all(bytes, 1).expect("zstd encode never fails on valid input")
    })
}

#[cfg(feature = "std")]
fn zstd_decode(bytes: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    use std::cell::RefCell;
    thread_local! {
        static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> = RefCell::new(None);
    }
    DECOMPRESSOR.with(|d| {
        let mut borrow = d.borrow_mut();
        if borrow.is_none() {
            *borrow = zstd::bulk::Decompressor::new().ok();
        }
        if let Some(decomp) = borrow.as_mut() {
            let mut out = vec![0u8; expected_len];
            if decomp.decompress_to_buffer(bytes, &mut out).is_ok() {
                return Ok(out);
            }
        }
        zstd::bulk::decompress(bytes, expected_len).map_err(|_| Error::InvalidValue)
    })
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn roundtrip(m: Message) {
        let mut buf = Vec::new();
        m.encode(&mut buf);
        let mut s = buf.as_slice();
        let out = Message::decode(&mut s).unwrap().unwrap();
        assert_eq!(m, out, "roundtrip mismatch");
        assert!(s.is_empty(), "trailing bytes after decode");
    }

    #[test]
    fn hello_welcome_roundtrip() {
        roundtrip(Message::Hello {
            proto_version: PROTO_VERSION,
            viewport_w: 1920, viewport_h: 1080,
            dpr_hundredths: 200,
            client_name: "gutted-client/debian".into(),
            capabilities: caps::H264 | caps::CLIENT_SCROLL,
        });
        roundtrip(Message::Welcome {
            proto_version: PROTO_VERSION,
            session_id: 0xDEADBEEF_CAFEBABE,
            features: caps::H264,
            cursor_track_id: 42,
            current_url: "about:blank".into(),
        });
        roundtrip(Message::Welcome {
            proto_version: PROTO_VERSION,
            session_id: 1,
            features: 0,
            cursor_track_id: 0,
            current_url: "".into(),
        });
    }

    #[test]
    fn input_and_scroll_roundtrip() {
        roundtrip(Message::InputPointer {
            ts_us: 1_234_567_890,
            x: -50, y: 720,
            modifiers: 1 << 20, // pointer_button1 held
        });
        roundtrip(Message::InputButton {
            ts_us: 42, x: 100, y: 200, button: 1, pressed: true, modifiers: 0,
        });
        roundtrip(Message::InputKey {
            ts_us: 999, keycode: 0xFF0D, mods: 0b1010, down: true,
        });
        roundtrip(Message::InputScroll {
            ts_us: 1_000, layer_id: 0,
            dx_units: 3, dy_units: -12,
            phase: ScrollPhase::Update,
        });
        roundtrip(Message::CursorState {
            shape: CursorShape::Pointer,
            hotspot_x: 4, hotspot_y: 4,
            image_ref: 0,
        });
        roundtrip(Message::SetZoom { level_milli: 1250 });
        roundtrip(Message::UrlChanged { url: "https://example.com/after".into() });
        roundtrip(Message::NavAction { action: 0 });
        roundtrip(Message::NavAction { action: 2 });
    }

    #[test]
    fn nav_and_resize_roundtrip() {
        roundtrip(Message::Nav { url: "https://example.com/a?b=c#d".into() });
        roundtrip(Message::Resize { viewport_w: 640, viewport_h: 480, dpr_hundredths: 100 });
        roundtrip(Message::Heartbeat { ts_us: 0 });
        roundtrip(Message::Heartbeat { ts_us: u64::MAX });
    }

    #[test]
    fn stream_of_messages() {
        // Multiple messages back-to-back in one buffer must decode in order.
        let mut buf = Vec::new();
        let msgs = vec![
            Message::Heartbeat { ts_us: 1 },
            Message::InputPointer { ts_us: 2, x: 10, y: 20, modifiers: 0 },
            Message::Nav { url: "about:blank".into() },
        ];
        for m in &msgs { m.encode(&mut buf); }
        let mut s = buf.as_slice();
        let mut out = Vec::new();
        while let Some(m) = Message::decode(&mut s).unwrap() { out.push(m); }
        assert_eq!(msgs, out);
        assert!(s.is_empty());
    }

    #[test]
    fn partial_frame_returns_none() {
        let m = Message::Nav { url: "https://x.example".into() };
        let mut full = Vec::new();
        m.encode(&mut full);
        // Every truncation should return Ok(None), never partial success.
        for cut in 0..full.len() {
            let mut s = &full[..cut];
            assert_eq!(Message::decode(&mut s).unwrap(), None,
                "cut={cut} should be incomplete but decoded");
        }
        let mut s = full.as_slice();
        assert!(Message::decode(&mut s).unwrap().is_some());
    }

    #[test]
    fn unknown_tag_is_refused_not_skipped() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 0xFFFF); // unreserved tag
        write_varint(&mut buf, 0);      // len=0
        let mut s = buf.as_slice();
        match Message::decode(&mut s) {
            Err(Error::UnknownTag(0xFFFF)) => {}
            other => panic!("expected UnknownTag, got {other:?}"),
        }
    }

    #[test]
    fn trailing_bytes_in_payload_is_refused() {
        // Craft a Heartbeat with an extra byte.
        let mut buf = Vec::new();
        write_varint(&mut buf, tag::HEARTBEAT);
        write_varint(&mut buf, 9); // one byte too many
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.push(0xAA);
        let mut s = buf.as_slice();
        assert_eq!(Message::decode(&mut s), Err(Error::TrailingBytes));
    }

    #[test]
    fn short_payload_is_length_overflow() {
        // Nav declared len=100 but only 5 bytes follow.
        let mut buf = Vec::new();
        write_varint(&mut buf, tag::NAV);
        write_varint(&mut buf, 100);
        buf.extend_from_slice(b"abcde");
        let mut s = buf.as_slice();
        // Full frame not yet present, so decode returns None (waiting for more bytes).
        assert_eq!(Message::decode(&mut s), Ok(None));
    }

    #[test]
    fn oversized_message_refused_before_alloc() {
        let mut buf = Vec::new();
        write_varint(&mut buf, tag::NAV);
        write_varint(&mut buf, (MAX_MESSAGE_BYTES as u32) + 1);
        let mut s = buf.as_slice();
        assert_eq!(Message::decode(&mut s), Err(Error::MessageTooLarge));
    }

    #[test]
    fn varint_overflow_is_caught() {
        // 5 bytes with the high nibble of the 5th byte set — invalid u32.
        let bad = [0xFFu8, 0xFF, 0xFF, 0xFF, 0x10];
        let mut s = &bad[..];
        assert_eq!(read_varint(&mut s), Err(Error::VarintOverflow));
    }

    #[test]
    fn invalid_utf8_is_refused() {
        let mut buf = Vec::new();
        write_varint(&mut buf, tag::NAV);
        // varint-len for a 3-byte "utf-8" that's actually 0xFF 0xFF 0xFF
        write_varint(&mut buf, 4);
        write_varint(&mut buf, 3); // inner str length
        buf.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
        let mut s = buf.as_slice();
        assert_eq!(Message::decode(&mut s), Err(Error::InvalidUtf8));
    }

    #[test]
    fn raw_frame_roundtrip_uncompressed() {
        let mut px = Vec::with_capacity(64 * 2);
        for i in 0..(64 * 2) { px.push((i & 0xff) as u8); }
        roundtrip(Message::RawFrame {
            ts_us: 1_000_000, width: 16, height: 2, stride: 64, format: 0,
            compression: 0, pixels: px,
        });
    }

    #[test]
    fn raw_frame_roundtrip_zstd() {
        // Big-ish repetitive payload — 128×32 rows of grey — so zstd earns.
        let (w, h) = (128u16, 32u16);
        let stride = (w as u32) * 4;
        let mut px = vec![0u8; (stride as usize) * (h as usize)];
        for chunk in px.chunks_exact_mut(4) { chunk.copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xFF]); }
        let m = Message::RawFrame {
            ts_us: 42, width: w, height: h, stride, format: 0,
            compression: 1, pixels: px.clone(),
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        // The wire-side should be much smaller than raw pixels + header.
        assert!(buf.len() < px.len() / 10, "zstd should shrink flat content ≥10× (got {} vs raw {})", buf.len(), px.len());
        let mut s = buf.as_slice();
        let out = Message::decode(&mut s).unwrap().unwrap();
        assert_eq!(m, out);
        assert!(s.is_empty());
    }

    #[test]
    fn phase3_scene_roundtrip() {
        roundtrip(Message::LayerAdd {
            id: 42, parent: 0, kind: LayerKind::Video,
            size: (1280, 720),
            transform: TRANSFORM_IDENTITY,
            opacity: 255,
            content: ContentRef::TrackId(0xDEADBEEFCAFEBABE),
        });
        roundtrip(Message::LayerAdd {
            id: 100, parent: 42, kind: LayerKind::Solid,
            size: (200, 30), transform: [256, 0, 0, 256, 40, 60],
            opacity: 200,
            content: ContentRef::Solid(0xFF00FF80),
        });
        roundtrip(Message::LayerUpdate {
            id: 42, mask: layer_mask::TRANSFORM,
            transform: [256, 0, 0, 256, 0, -240],
            opacity: 255, size: (0, 0), content: ContentRef::None, damage: (0,0,0,0),
        });
        roundtrip(Message::LayerUpdate {
            id: 100, mask: layer_mask::OPACITY | layer_mask::DAMAGE,
            transform: TRANSFORM_IDENTITY,
            opacity: 128, size: (0, 0), content: ContentRef::None,
            damage: (10, 20, 50, 25),
        });
        roundtrip(Message::LayerRemove { id: 100 });
        roundtrip(Message::SceneCommit { version: 1234567 });
    }

    #[test]
    fn scene_delta_bandwidth_is_tiny() {
        // A scroll of layer 42 down 240 px = one LayerUpdate with only
        // the transform mask + one SceneCommit. Wire cost sanity check.
        let scroll = Message::LayerUpdate {
            id: 42, mask: layer_mask::TRANSFORM,
            transform: [256, 0, 0, 256, 0, -240],
            opacity: 255, size: (0, 0), content: ContentRef::None, damage: (0,0,0,0),
        };
        let commit = Message::SceneCommit { version: 42 };
        let mut buf = Vec::new();
        scroll.encode(&mut buf);
        commit.encode(&mut buf);
        assert!(buf.len() < 64, "scroll+commit should be <64 B, got {}", buf.len());
    }

    #[test]
    fn asset_hash_content_ref() {
        let h = [0xAAu8; 32];
        roundtrip(Message::LayerAdd {
            id: 7, parent: 0, kind: LayerKind::Tile,
            size: (256, 256), transform: TRANSFORM_IDENTITY, opacity: 255,
            content: ContentRef::AssetHash(h),
        });
    }

    #[test]
    fn subframe_roundtrip_zstd() {
        let (w, h) = (64u16, 32u16);
        let stride = (w as u32) * 4;
        let mut px = vec![0u8; (stride as usize) * (h as usize)];
        for (i, chunk) in px.chunks_exact_mut(4).enumerate() {
            chunk.copy_from_slice(&[(i as u8), 0x80, 0x40, 0xFF]);
        }
        roundtrip(Message::Subframe {
            ts_us: 42, x: 100, y: 200, w, h, stride, format: 0,
            compression: 1, pixels: px.clone(),
        });
        roundtrip(Message::Subframe {
            ts_us: 42, x: 100, y: 200, w, h, stride, format: 0,
            compression: 2, pixels: px,
        });
    }

    #[test]
    fn spatial_delta_compresses_ui_better() {
        let (w, h) = (128u16, 64u16);
        let stride = (w as u32) * 4;
        // Simulate text / UI pattern: runs of identical background with occasional content
        let mut px = vec![0xFFu8; (stride as usize) * (h as usize)];
        for row in 10..20 {
            for col in 20..40 {
                let idx = row * (stride as usize) + col * 4;
                px[idx..idx+4].copy_from_slice(&[0x10, 0x20, 0x30, 0xFF]);
            }
        }
        let m1 = Message::Subframe {
            ts_us: 1, x: 0, y: 0, w, h, stride, format: 0,
            compression: 1, pixels: px.clone(),
        };
        let m2 = Message::Subframe {
            ts_us: 1, x: 0, y: 0, w, h, stride, format: 0,
            compression: 2, pixels: px,
        };
        let mut b1 = Vec::new(); m1.encode(&mut b1);
        let mut b2 = Vec::new(); m2.encode(&mut b2);
        assert!(b2.len() < b1.len(), "ZSTD_DELTA ({} B) should be smaller than standard ZSTD ({} B)", b2.len(), b1.len());
    }

    #[test]
    fn audio_and_video_roundtrip() {
        let opus_data = vec![0xFC, 0xFF, 0xFE, 0x01, 0x02, 0x03, 0x04];
        roundtrip(Message::AudioFrame {
            pts_us: 123456789,
            codec: audio_codec::OPUS,
            channels: 2,
            sample_rate: 48000,
            data: opus_data,
        });

        let h264_nal = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1E];
        roundtrip(Message::VideoChunk {
            pts_us: 987654321,
            duration_us: 16666,
            is_keyframe: true,
            codec: video_codec::H264,
            layer_id: 1,
            data: h264_nal,
        });
    }

    #[test]
    fn raw_frame_stride_mismatch_is_refused() {
        // stride=8 * height=3 = 24 bytes expected, but supply 25 (uncompressed).
        let mut buf = Vec::new();
        write_varint(&mut buf, tag::RAW_FRAME);
        // header = 8+2+2+4+4+1 = 21 bytes; plus 25 pixel bytes = 46.
        write_varint(&mut buf, 21 + 25);
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&4u16.to_le_bytes());
        buf.extend_from_slice(&3u16.to_le_bytes());
        buf.extend_from_slice(&8u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.push(0); // compression = raw
        buf.extend_from_slice(&[0xAA; 25]);
        let mut s = buf.as_slice();
        assert_eq!(Message::decode(&mut s), Err(Error::InvalidValue));
    }

    #[test]
    fn invalid_scroll_phase_is_refused() {
        let mut buf = Vec::new();
        Message::InputScroll {
            ts_us: 1, layer_id: 0, dx_units: 0, dy_units: 0, phase: ScrollPhase::Update,
        }.encode(&mut buf);
        // Corrupt the phase byte (last byte of payload).
        let last = buf.len() - 1;
        buf[last] = 99;
        let mut s = buf.as_slice();
        assert_eq!(Message::decode(&mut s), Err(Error::InvalidValue));
    }

    #[test]
    fn stop_and_tile_roundtrips() {
        roundtrip(Message::Stop);
        let px = vec![0x12u8; 64 * 64 * 4];
        roundtrip(Message::TileData {
            hash: 0x123456789ABCDEF0,
            w: 64, h: 64, stride: 256, format: 0,
            compression: 1, pixels: px,
        });
        roundtrip(Message::TileRef {
            ts_us: 99999, x: 128, y: 256, w: 64, h: 64,
            hash: 0x123456789ABCDEF0,
        });
    }

    #[test]
    fn vector_and_asset_roundtrip() {
        let hash = [0x55u8; 32];
        roundtrip(Message::AssetRegister {
            hash,
            kind: 0,
            data: vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
        });
        roundtrip(Message::DrawCommands {
            ts_us: 1234567,
            layer_id: 1,
            commands: vec![
                DrawCommand::SetClip { x: 0, y: 0, w: 800, h: 600 },
                DrawCommand::FillRect { x: 10, y: 20, w: 100, h: 50, rgba: 0xFF0000FF },
                DrawCommand::StrokeRect { x: 120, y: 20, w: 100, h: 50, rgba: 0x00FF00FF, line_width: 2 },
                DrawCommand::DrawText { x: 10, y: 100, font_size: 16, rgba: 0x000000FF, text: String::from("Hello QUIC") },
                DrawCommand::DrawImage { x: 10, y: 150, w: 64, h: 64, asset_hash: hash },
                DrawCommand::ClearClip,
            ],
        });
    }
}
