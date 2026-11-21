//! Native CoralOS Browser Protocol (`CBP 1.0`) Bridge & Adapter (`#![no_std]` + `alloc`).
//!
//! Translates between CoralOS's `coral-browser-protocol` (`CBP 1.0`, `MAGIC = 0x3150_4243`,
//! `FRAME_MAGIC = 0x3146_4243`, `RETURN_MAGIC = 0x3152_4243`) and `CoralCloud` (`gutted_proto::Message`
//! + [`crate::BrowserCore`]).
//!
//! This allows CoralOS's native `Browser.app` (`Ferrum`) to connect to a remote `gutted-host`
//! running on self-hosted `CoralCloud` as its drop-in CBP 1.0 rendering engine:
//! - Accepts CBP 1.0 requests (`OP_OPEN_VIEW` `0x0001` .. `OP_VIDEO_LAYER_ANSWER` `0x0013`),
//!   produces exact 80-byte / 112-byte CBP 1.0 `OP_REPLY` (`0x8000`) packets + correlated
//!   `OP_LIFECYCLE` (`0x4001`), `OP_NAVIGATION` (`0x4002`), `OP_FRAME_READY` (`0x4003`),
//!   `OP_DOCUMENT_STATE` (`0x4004`), `OP_CURSOR` (`0x4005`), and `OP_CLIPBOARD_WRITE` (`0x4006`) events.
//! - Maintains the two CBP 1.0 frame lease buffers (`BUFFER_COUNT = 2`), writing the 4096-byte
//!   `METADATA_BYTES` `FrameHeader` (`FRAME_MAGIC = 0x3146_4243`, `PIXEL_FORMAT_BGRX8_SRGB_OPAQUE = 2`,
//!   64-byte aligned stride, embedded title at `256..768` and URL at `768..3840`) followed by
//!   64-byte-stride-aligned `BGRX8_SRGB_OPAQUE` scanlines at `PIXEL_OFFSET = 4096`.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::render::{BrowserCore, InputEvent, MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT};
use gutted_proto::Message as CloudMessage;

// ─── CBP 1.0 Wire Constants (exact match with coral-browser-protocol) ────────
pub const CBP_MAGIC: u32 = 0x3150_4243; // "CBP1"
pub const CBP_FRAME_MAGIC: u32 = 0x3146_4243; // "CBF1"
pub const CBP_RETURN_MAGIC: u32 = 0x3152_4243; // "CBR1"
pub const CBP_MAJOR: u16 = 1;
pub const CBP_MINOR: u16 = 0;
pub const CBP_HEADER_FLAGS: u16 = 0;

pub const CBP_EP_CAP: usize = 4096;
pub const CBP_HEADER_BYTES: usize = 40;
pub const CBP_VIEWPORT_BYTES: usize = 32;
pub const CBP_REPLY_BODY_BYTES: usize = 40;
pub const CBP_OPEN_REPLY_EXT_BYTES: usize = 32;
pub const CBP_URL_MAX: usize = 3072;
pub const CBP_TITLE_MAX: usize = 512;
pub const CBP_TEXT_MAX: usize = 1024;
pub const CBP_MAX_VIEWS: usize = 16;

pub const CBP_FRAME_HEADER_BYTES: usize = 256;
pub const CBP_METADATA_BYTES: usize = 4096;
pub const CBP_PIXEL_OFFSET: u32 = 4096;
pub const CBP_TITLE_OFFSET: u32 = 256;
pub const CBP_URL_OFFSET: u32 = 768;
pub const CBP_METADATA_RESERVED_OFFSET: usize = 3840;
pub const CBP_RETURN_PAGE_BYTES: usize = 4096;
pub const CBP_PIXEL_FORMAT_BGRX8_SRGB_OPAQUE: u32 = 2;
pub const CBP_BUFFER_COUNT: u32 = 2;
pub const CBP_UNUSED_BUFFER: u32 = u32::MAX;
pub const CBP_STRIDE_ALIGNMENT: u32 = 64;
pub const CBP_Q16_ONE: u32 = 1 << 16;

// Request opcodes (Chrome -> Engine)
pub const OP_OPEN_VIEW: u16 = 0x0001;
pub const OP_NAVIGATE: u16 = 0x0002;
pub const OP_BACK: u16 = 0x0003;
pub const OP_FORWARD: u16 = 0x0004;
pub const OP_RELOAD: u16 = 0x0005;
pub const OP_STOP: u16 = 0x0006;
pub const OP_RESIZE: u16 = 0x0007;
pub const OP_POINTER: u16 = 0x0008;
pub const OP_SCROLL: u16 = 0x0009;
pub const OP_KEY: u16 = 0x000a;
pub const OP_TEXT: u16 = 0x000b;
pub const OP_RELEASE_FRAME: u16 = 0x000c;
pub const OP_CLOSE_VIEW: u16 = 0x000d;
pub const OP_GET_STATUS: u16 = 0x000e;
pub const OP_SCREEN: u16 = 0x000f;
pub const OP_CREATE_VIEW: u16 = 0x0010;
pub const OP_ACTIVATE_VIEW: u16 = 0x0011;
pub const OP_CLIPBOARD_SET: u16 = 0x0012;
pub const OP_VIDEO_LAYER_ANSWER: u16 = 0x0013;

// Event opcodes (Engine -> Chrome)
pub const OP_LIFECYCLE: u16 = 0x4001;
pub const OP_NAVIGATION: u16 = 0x4002;
pub const OP_FRAME_READY: u16 = 0x4003;
pub const OP_DOCUMENT_STATE: u16 = 0x4004;
pub const OP_CURSOR: u16 = 0x4005;
pub const OP_CLIPBOARD_WRITE: u16 = 0x4006;
pub const OP_VIDEO_LAYER: u16 = 0x4007;

// Reply opcode (Engine -> Chrome)
pub const OP_REPLY: u16 = 0x8000;

// CBP Status codes
pub const STATUS_ACCEPTED: u16 = 0;
pub const STATUS_VIEW_BUSY: u16 = 1;
pub const STATUS_NOT_READY: u16 = 2;
pub const STATUS_BAD_ARGUMENT: u16 = 3;
pub const STATUS_OUT_OF_RANGE: u16 = 4;
pub const STATUS_BACKING_UNAVAILABLE: u16 = 5;
pub const STATUS_STALE_INSTANCE: u16 = 6;
pub const STATUS_STALE_VIEW: u16 = 7;
pub const STATUS_STALE_NAVIGATION: u16 = 8;
pub const STATUS_STALE_RESIZE: u16 = 9;
pub const STATUS_STALE_FRAME: u16 = 10;
pub const STATUS_NO_HISTORY: u16 = 11;
pub const STATUS_UNSUPPORTED: u16 = 12;
pub const STATUS_CLOSING: u16 = 13;
pub const STATUS_INTERNAL_ERROR: u16 = 14;
pub const STATUS_BAD_VERSION: u16 = 15;
pub const STATUS_BAD_MESSAGE: u16 = 16;
pub const STATUS_ID_EXHAUSTED: u16 = 17;
pub const STATUS_NO_VIEW: u16 = 18;

// CBP Lifecycle codes
pub const LIFE_STARTING: u16 = 1;
pub const LIFE_READY: u16 = 2;
pub const LIFE_LOADING: u16 = 3;
pub const LIFE_CLOSING: u16 = 4;
pub const LIFE_CLOSED: u16 = 5;
pub const LIFE_FAULTED: u16 = 6;

// CBP Navigation Stage & Failure codes
pub const STAGE_ACCEPTED: u16 = 1;
pub const STAGE_REDIRECT: u16 = 2;
pub const STAGE_RESPONSE: u16 = 3;
pub const STAGE_COMMITTED: u16 = 4;
pub const STAGE_FAILED: u16 = 5;
pub const FAILURE_NONE: u16 = 0;

// CBP Frame kinds
pub const FRAME_BLANK: u32 = 0;
pub const FRAME_SITE: u32 = 1;
pub const FRAME_ERROR: u32 = 2;

// CBP Document flags
pub const DOC_CAN_GO_BACK: u32 = 1;
pub const DOC_CAN_GO_FORWARD: u32 = 2;
pub const DOC_SUBRESOURCE_FAILURE: u32 = 4;
pub const DOC_LOAD_EVENT_FIRED: u32 = 8;

// CBP Pointer / Button / Key actions
pub const POINTER_MOVE: u16 = 1;
pub const POINTER_DOWN: u16 = 2;
pub const POINTER_UP: u16 = 3;
pub const POINTER_LEAVE: u16 = 4;
pub const BUTTON_NONE: u16 = 0;
pub const BUTTON_LEFT: u16 = 1;
pub const BUTTON_MIDDLE: u16 = 2;
pub const BUTTON_RIGHT: u16 = 3;
pub const KEY_DOWN: u16 = 1;
pub const KEY_UP: u16 = 2;
pub const KEY_REPEAT: u16 = 3;
pub const DISPOSITION_COPIED: u16 = 1;
pub const DISPOSITION_DISCARDED: u16 = 2;
pub const CLIPBOARD_AVAILABLE: u16 = 1;

// CBP Modifiers
pub const CBP_MOD_SHIFT: u16 = 1;
pub const CBP_MOD_CTRL: u16 = 2;
pub const CBP_MOD_ALT: u16 = 4;
pub const CBP_MOD_META: u16 = 8;

/// CBP 1.0 40-byte message header (`i: InstanceId`, `v: ViewId`, `q: RequestId`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CbpHeader {
    pub opcode: u16,
    pub flags: u16,
    pub total_bytes: u32,
    pub instance: u64,
    pub view: u64,
    pub request: u64,
}

/// CBP 1.0 32-byte `Viewport` geometry record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CbpViewport {
    pub resize: u64,
    pub logical_width_q16: u32,
    pub logical_height_q16: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub stride: u32,
    pub scale_q16: u32,
}

impl CbpViewport {
    /// Construct a 1:1 scale (`Q16_ONE`) CBP 1.0 `Viewport` with 64-byte aligned stride.
    pub const fn new_1x(resize: u64, width: u32, height: u32) -> Self {
        let raw_stride = width.saturating_mul(4);
        let rem = raw_stride % CBP_STRIDE_ALIGNMENT;
        let stride = if rem == 0 {
            raw_stride
        } else {
            raw_stride + (CBP_STRIDE_ALIGNMENT - rem)
        };
        Self {
            resize,
            logical_width_q16: width << 16,
            logical_height_q16: height << 16,
            pixel_width: width,
            pixel_height: height,
            stride,
            scale_q16: CBP_Q16_ONE,
        }
    }

    pub const fn pixel_bytes(&self) -> u32 {
        self.stride.saturating_mul(self.pixel_height)
    }
}

/// Decoded CBP 1.0 4096-byte `FrameHeader` (`FRAME_MAGIC = 0x3146_4243`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CbpFrameHeader {
    pub instance: u64,
    pub view: u64,
    pub navigation: u64,
    pub generation: u64,
    pub resize: u64,
    pub buffer: u32,
    pub kind: u32,
    pub published: u64,
    pub viewport: CbpViewport,
    pub region_bytes: u64,
    pub pixel_bytes: u32,
    pub title: String,
    pub url: String,
}

/// Errors returned when decoding or encoding CBP 1.0 packets or frame headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CbpError {
    Truncated,
    BadMagic,
    BadVersion,
    DeclaredLengthMismatch,
    NonzeroReserved,
    UnknownOpcode(u16),
    InvalidUtf8,
    InvalidField(&'static str),
}

fn read_u16(b: &[u8], off: usize) -> Result<u16, CbpError> {
    let s = b.get(off..off + 2).ok_or(CbpError::Truncated)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32(b: &[u8], off: usize) -> Result<u32, CbpError> {
    let s = b.get(off..off + 4).ok_or(CbpError::Truncated)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_i32(b: &[u8], off: usize) -> Result<i32, CbpError> {
    let s = b.get(off..off + 4).ok_or(CbpError::Truncated)?;
    Ok(i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_u64(b: &[u8], off: usize) -> Result<u64, CbpError> {
    let s = b.get(off..off + 8).ok_or(CbpError::Truncated)?;
    Ok(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

fn read_viewport(b: &[u8], off: usize) -> Result<CbpViewport, CbpError> {
    if b.len() < off + CBP_VIEWPORT_BYTES {
        return Err(CbpError::Truncated);
    }
    Ok(CbpViewport {
        resize: read_u64(b, off)?,
        logical_width_q16: read_u32(b, off + 8)?,
        logical_height_q16: read_u32(b, off + 12)?,
        pixel_width: read_u32(b, off + 16)?,
        pixel_height: read_u32(b, off + 20)?,
        stride: read_u32(b, off + 24)?,
        scale_q16: read_u32(b, off + 28)?,
    })
}

fn write_header(out: &mut Vec<u8>, opcode: u16, body_len: usize, i: u64, v: u64, q: u64) {
    let total = (CBP_HEADER_BYTES + body_len) as u32;
    out.extend_from_slice(&CBP_MAGIC.to_le_bytes());
    out.extend_from_slice(&CBP_MAJOR.to_le_bytes());
    out.extend_from_slice(&CBP_MINOR.to_le_bytes());
    out.extend_from_slice(&opcode.to_le_bytes());
    out.extend_from_slice(&CBP_HEADER_FLAGS.to_le_bytes());
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&i.to_le_bytes());
    out.extend_from_slice(&v.to_le_bytes());
    out.extend_from_slice(&q.to_le_bytes());
}

/// Decode the 40-byte CBP 1.0 header and verify magic, version, and declared length.
pub fn decode_cbp_header(packet: &[u8]) -> Result<CbpHeader, CbpError> {
    if packet.len() < CBP_HEADER_BYTES {
        return Err(CbpError::Truncated);
    }
    if read_u32(packet, 0)? != CBP_MAGIC {
        return Err(CbpError::BadMagic);
    }
    let major = read_u16(packet, 4)?;
    let minor = read_u16(packet, 6)?;
    if major != CBP_MAJOR || minor != CBP_MINOR {
        return Err(CbpError::BadVersion);
    }
    let opcode = read_u16(packet, 8)?;
    let flags = read_u16(packet, 10)?;
    if flags != CBP_HEADER_FLAGS {
        return Err(CbpError::NonzeroReserved);
    }
    let total_bytes = read_u32(packet, 12)?;
    if total_bytes as usize != packet.len() {
        return Err(CbpError::DeclaredLengthMismatch);
    }
    Ok(CbpHeader {
        opcode,
        flags,
        total_bytes,
        instance: read_u64(packet, 16)?,
        view: read_u64(packet, 24)?,
        request: read_u64(packet, 32)?,
    })
}

/// Encode a 4096-byte CBP 1.0 `FrameHeader` (`FRAME_MAGIC = 0x3146_4243`) into `out[..4096]`.
pub fn encode_cbp_frame_header(
    h: &CbpFrameHeader,
    out: &mut [u8; CBP_METADATA_BYTES],
) -> Result<(), CbpError> {
    if h.title.len() > CBP_TITLE_MAX || h.url.len() > CBP_URL_MAX {
        return Err(CbpError::InvalidField("frame_header.string_len"));
    }
    if h.kind == FRAME_BLANK && (!h.url.is_empty() || !h.title.is_empty() || h.navigation != 0) {
        return Err(CbpError::InvalidField("frame_header.blank_rule"));
    }
    if (h.kind == FRAME_SITE || h.kind == FRAME_ERROR) && (h.url.is_empty() || h.navigation == 0) {
        return Err(CbpError::InvalidField("frame_header.site_rule"));
    }

    out.fill(0);
    out[0..4].copy_from_slice(&CBP_FRAME_MAGIC.to_le_bytes());
    out[4..6].copy_from_slice(&CBP_MAJOR.to_le_bytes());
    out[6..8].copy_from_slice(&CBP_MINOR.to_le_bytes());
    out[8..12].copy_from_slice(&(CBP_FRAME_HEADER_BYTES as u32).to_le_bytes());
    out[12..16].copy_from_slice(&CBP_PIXEL_FORMAT_BGRX8_SRGB_OPAQUE.to_le_bytes());
    out[16..24].copy_from_slice(&h.instance.to_le_bytes());
    out[24..32].copy_from_slice(&h.view.to_le_bytes());
    out[32..40].copy_from_slice(&h.navigation.to_le_bytes());
    out[40..48].copy_from_slice(&h.generation.to_le_bytes());
    out[48..56].copy_from_slice(&h.resize.to_le_bytes());
    out[56..60].copy_from_slice(&h.buffer.to_le_bytes());
    out[60..64].copy_from_slice(&h.kind.to_le_bytes());
    out[64..72].copy_from_slice(&h.published.to_le_bytes());
    // [72..128) is reserved0 (zeroed)
    out[128..132].copy_from_slice(&h.viewport.logical_width_q16.to_le_bytes());
    out[132..136].copy_from_slice(&h.viewport.logical_height_q16.to_le_bytes());
    out[136..140].copy_from_slice(&h.viewport.pixel_width.to_le_bytes());
    out[140..144].copy_from_slice(&h.viewport.pixel_height.to_le_bytes());
    out[144..148].copy_from_slice(&h.viewport.stride.to_le_bytes());
    out[148..152].copy_from_slice(&h.viewport.scale_q16.to_le_bytes());
    out[152..160].copy_from_slice(&h.region_bytes.to_le_bytes());
    out[160..164].copy_from_slice(&CBP_PIXEL_OFFSET.to_le_bytes());
    out[164..168].copy_from_slice(&h.pixel_bytes.to_le_bytes());
    out[168..172].copy_from_slice(&CBP_TITLE_OFFSET.to_le_bytes());
    out[172..176].copy_from_slice(&(h.title.len() as u32).to_le_bytes());
    out[176..180].copy_from_slice(&CBP_URL_OFFSET.to_le_bytes());
    out[180..184].copy_from_slice(&(h.url.len() as u32).to_le_bytes());
    // [184..256) is reserved1 (zeroed)
    let t_start = CBP_TITLE_OFFSET as usize;
    out[t_start..t_start + h.title.len()].copy_from_slice(h.title.as_bytes());
    let u_start = CBP_URL_OFFSET as usize;
    out[u_start..u_start + h.url.len()].copy_from_slice(h.url.as_bytes());
    Ok(())
}

/// Decode and validate a 4096-byte CBP 1.0 `FrameHeader` (`FRAME_MAGIC = 0x3146_4243`).
pub fn decode_cbp_frame_header(
    snapshot: &[u8; CBP_METADATA_BYTES],
) -> Result<CbpFrameHeader, CbpError> {
    if read_u32(snapshot, 0)? != CBP_FRAME_MAGIC {
        return Err(CbpError::BadMagic);
    }
    if read_u16(snapshot, 4)? != CBP_MAJOR || read_u16(snapshot, 6)? != CBP_MINOR {
        return Err(CbpError::BadVersion);
    }
    if read_u32(snapshot, 8)? != CBP_FRAME_HEADER_BYTES as u32 {
        return Err(CbpError::InvalidField("header_bytes"));
    }
    if read_u32(snapshot, 12)? != CBP_PIXEL_FORMAT_BGRX8_SRGB_OPAQUE {
        return Err(CbpError::InvalidField("pixel_format"));
    }
    let instance = read_u64(snapshot, 16)?;
    let view = read_u64(snapshot, 24)?;
    let navigation = read_u64(snapshot, 32)?;
    let generation = read_u64(snapshot, 40)?;
    let resize = read_u64(snapshot, 48)?;
    let buffer = read_u32(snapshot, 56)?;
    let kind = read_u32(snapshot, 60)?;
    let published = read_u64(snapshot, 64)?;
    if snapshot[72..128].iter().any(|&b| b != 0) {
        return Err(CbpError::NonzeroReserved);
    }
    let viewport = CbpViewport {
        resize,
        logical_width_q16: read_u32(snapshot, 128)?,
        logical_height_q16: read_u32(snapshot, 132)?,
        pixel_width: read_u32(snapshot, 136)?,
        pixel_height: read_u32(snapshot, 140)?,
        stride: read_u32(snapshot, 144)?,
        scale_q16: read_u32(snapshot, 148)?,
    };
    let region_bytes = read_u64(snapshot, 152)?;
    if read_u32(snapshot, 160)? != CBP_PIXEL_OFFSET {
        return Err(CbpError::InvalidField("pixel_offset"));
    }
    let pixel_bytes = read_u32(snapshot, 164)?;
    if read_u32(snapshot, 168)? != CBP_TITLE_OFFSET {
        return Err(CbpError::InvalidField("title_offset"));
    }
    let title_len = read_u32(snapshot, 172)? as usize;
    if read_u32(snapshot, 176)? != CBP_URL_OFFSET {
        return Err(CbpError::InvalidField("url_offset"));
    }
    let url_len = read_u32(snapshot, 180)? as usize;
    if snapshot[184..256].iter().any(|&b| b != 0) {
        return Err(CbpError::NonzeroReserved);
    }
    if title_len > CBP_TITLE_MAX || url_len > CBP_URL_MAX {
        return Err(CbpError::InvalidField("string_len"));
    }
    let t_start = CBP_TITLE_OFFSET as usize;
    let title = core::str::from_utf8(&snapshot[t_start..t_start + title_len])
        .map_err(|_| CbpError::InvalidUtf8)?;
    let u_start = CBP_URL_OFFSET as usize;
    let url = core::str::from_utf8(&snapshot[u_start..u_start + url_len])
        .map_err(|_| CbpError::InvalidUtf8)?;
    if snapshot[CBP_METADATA_RESERVED_OFFSET..]
        .iter()
        .any(|&b| b != 0)
    {
        return Err(CbpError::NonzeroReserved);
    }
    Ok(CbpFrameHeader {
        instance,
        view,
        navigation,
        generation,
        resize,
        buffer,
        kind,
        published,
        viewport,
        region_bytes,
        pixel_bytes,
        title: String::from(title),
        url: String::from(url),
    })
}

/// Encode a 4096-byte CBP 1.0 `ReturnPage` (`RETURN_MAGIC = 0x3152_4243`).
pub fn encode_cbp_return_page(
    instance: u64,
    view: u64,
    released: [u64; 2],
    ready: u64,
    out: &mut [u8; CBP_RETURN_PAGE_BYTES],
) {
    out.fill(0);
    out[0..4].copy_from_slice(&CBP_RETURN_MAGIC.to_le_bytes());
    out[4..6].copy_from_slice(&CBP_MAJOR.to_le_bytes());
    out[6..8].copy_from_slice(&CBP_MINOR.to_le_bytes());
    out[8..12].copy_from_slice(&(CBP_RETURN_PAGE_BYTES as u32).to_le_bytes());
    out[12..16].copy_from_slice(&CBP_BUFFER_COUNT.to_le_bytes());
    out[16..24].copy_from_slice(&instance.to_le_bytes());
    out[24..32].copy_from_slice(&view.to_le_bytes());
    out[64..72].copy_from_slice(&released[0].to_le_bytes());
    out[128..136].copy_from_slice(&released[1].to_le_bytes());
    out[192..200].copy_from_slice(&ready.to_le_bytes());
}

#[derive(Debug, Clone)]
struct CbpViewState {
    pub tab_id: u32,
    pub lifecycle: u16,
    pub active_n: u64,
    pub viewport: CbpViewport,
    pub title: String,
    pub url: String,
    pub doc_flags: u32,
}

/// Map a USB HID keyboard usage ID (`0x07` page) to an X11 keysym understood by `BrowserCore` & WPE.
pub fn hid_usage_to_keysym(usage: u16, shift: bool) -> u32 {
    match usage {
        0x04..=0x1d => {
            let off = (usage - 0x04) as u32;
            if shift {
                (b'A' as u32) + off
            } else {
                (b'a' as u32) + off
            }
        }
        0x1e..=0x26 => (b'1' as u32) + (usage - 0x1e) as u32,
        0x27 => b'0' as u32,
        0x28 => 0xff0d, // Return / Enter
        0x29 => 0xff1b, // Escape
        0x2a => 0xff08, // BackSpace
        0x2b => 0xff09, // Tab
        0x2c => 0x0020, // Space
        0x4f => 0xff53, // Right
        0x50 => 0xff51, // Left
        0x51 => 0xff54, // Down
        0x52 => 0xff52, // Up
        _ => usage as u32,
    }
}

/// Map `gutted-proto`'s `CursorState` shape byte (`0..=13`) to a CBP 1.0 `CursorShape` (`1..=35`).
pub fn cloud_cursor_to_cbp(shape: u8) -> u16 {
    match shape {
        0 => 1,  // CURSOR_DEFAULT
        1 => 2,  // CURSOR_POINTER
        2 => 3,  // CURSOR_TEXT
        3 => 5,  // CURSOR_WAIT
        4 => 6,  // CURSOR_PROGRESS
        5 => 8,  // CURSOR_CROSSHAIR
        6 => 9,  // CURSOR_MOVE
        7 => 11, // CURSOR_NOT_ALLOWED
        8 => 17, // CURSOR_GRAB
        9 => 18, // CURSOR_GRABBING
        10 => 30, // CURSOR_EW_RESIZE
        11 => 31, // CURSOR_NS_RESIZE
        12 => 32, // CURSOR_NESW_RESIZE
        13 => 33, // CURSOR_NWSE_RESIZE
        _ => 1,
    }
}

/// `#![no_std]` + `alloc` CoralOS CBP 1.0 ↔ `CoralCloud` (`gutted-host`) Bridge.
///
/// Acts as a CBP 1.0 engine endpoint for CoralOS's `Browser.app` (`Ferrum`) while delegating
/// all heavy HTML/CSS/JS/WebGL/Video rasterization to a remote `gutted-host` over `gutted-proto`.
pub struct CbpCloudBridge {
    pub instance_id: u64,
    pub active_view_id: u64,
    next_view_id: u64,
    next_nav_id: u64,
    next_generation: u64,
    next_buffer_slot: u32,
    pub capacity: u64,
    pub region_bytes: u64,
    views: BTreeMap<u64, CbpViewState>,
    /// Double-buffered CBP 1.0 shared-memory frame regions (`[buffer 0, buffer 1]`).
    /// Each region begins with the 4096-byte `METADATA_BYTES` `FrameHeader` (`FRAME_MAGIC`)
    /// followed by `viewport.stride * viewport.pixel_height` bytes of `BGRX8_SRGB_OPAQUE` pixels.
    pub frame_buffers: [Vec<u8>; 2],
    /// Currently leased generation per buffer slot (`0` if free).
    pub buffer_lease_gen: [u64; 2],
    /// 4096-byte CBP 1.0 `ReturnPage` (`RETURN_MAGIC`).
    pub return_page: [u8; CBP_RETURN_PAGE_BYTES],
    /// Underlying `#![no_std]` `BrowserCore` (configured in chromeless mode `builtin_chrome = false`
    /// because CoralOS `Browser.app` draws its own Sandstone chrome).
    pub core: BrowserCore,
    /// Outbound CBP 1.0 wire packets (`OP_REPLY` and `OP_*` events) queued for `Browser.app`.
    cbp_outbox: Vec<Vec<u8>>,
    /// Scratch `[u32]` composite surface for `BrowserCore::render_into`.
    scratch_fb: Vec<u32>,
}

impl CbpCloudBridge {
    /// Create a new CBP 1.0 ↔ `CoralCloud` bridge for `instance_id`.
    pub fn new(instance_id: u64) -> Self {
        let instance_id = instance_id.max(1);
        let mut core = BrowserCore::new_no_std(1280, 720, "about:blank".into(), false);
        core.builtin_chrome = false;
        Self {
            instance_id,
            active_view_id: 0,
            next_view_id: 1,
            next_nav_id: 1,
            next_generation: 1,
            next_buffer_slot: 0,
            capacity: 0,
            region_bytes: 0,
            views: BTreeMap::new(),
            frame_buffers: [Vec::new(), Vec::new()],
            buffer_lease_gen: [0, 0],
            return_page: [0u8; CBP_RETURN_PAGE_BYTES],
            core,
            cbp_outbox: Vec::new(),
            scratch_fb: vec![0u32; 1280 * 720],
        }
    }

    /// Drain all queued CBP 1.0 wire packets (`OP_REPLY` + `OP_*` events) ready to send to `Browser.app`.
    pub fn drain_cbp_packets(&mut self) -> Vec<Vec<u8>> {
        core::mem::take(&mut self.cbp_outbox)
    }

    /// Encode all pending outbound `CoralCloud` (`gutted_proto::Message`) packets into `out`.
    pub fn drain_cloud_wire(&mut self, ts_us: u64, out: &mut Vec<u8>) {
        self.core.encode_outbound_messages(ts_us, out);
    }

    /// Read the 4096-byte CBP 1.0 `FrameHeader` currently written in `buffer_slot` (`0` or `1`).
    pub fn read_frame_header(&self, buffer_slot: usize) -> Result<CbpFrameHeader, CbpError> {
        let buf = self
            .frame_buffers
            .get(buffer_slot)
            .ok_or(CbpError::InvalidField("buffer_slot"))?;
        if buf.len() < CBP_METADATA_BYTES {
            return Err(CbpError::Truncated);
        }
        let mut meta = [0u8; CBP_METADATA_BYTES];
        meta.copy_from_slice(&buf[..CBP_METADATA_BYTES]);
        decode_cbp_frame_header(&meta)
    }

    #[allow(clippy::too_many_arguments)]
    fn push_reply(
        &mut self,
        view: u64,
        req_id: u64,
        req_opcode: u16,
        status: u16,
        n: u64,
        r: u64,
        g: u64,
        b: u32,
        lifecycle: u16,
        open_ext: Option<(u64, u64)>,
    ) {
        let body_len = if open_ext.is_some() {
            CBP_REPLY_BODY_BYTES + CBP_OPEN_REPLY_EXT_BYTES
        } else {
            CBP_REPLY_BODY_BYTES
        };
        let mut pkt = Vec::with_capacity(CBP_HEADER_BYTES + body_len);
        write_header(&mut pkt, OP_REPLY, body_len, self.instance_id, view, req_id);
        pkt.extend_from_slice(&req_opcode.to_le_bytes());
        pkt.extend_from_slice(&status.to_le_bytes());
        pkt.extend_from_slice(&0u32.to_le_bytes());
        pkt.extend_from_slice(&n.to_le_bytes());
        pkt.extend_from_slice(&r.to_le_bytes());
        pkt.extend_from_slice(&g.to_le_bytes());
        pkt.extend_from_slice(&b.to_le_bytes());
        pkt.extend_from_slice(&lifecycle.to_le_bytes());
        pkt.extend_from_slice(&0u16.to_le_bytes());
        if let Some((cap, reg_bytes)) = open_ext {
            pkt.extend_from_slice(&cap.to_le_bytes());
            pkt.extend_from_slice(&reg_bytes.to_le_bytes());
            pkt.extend_from_slice(&0u32.to_le_bytes()); // buffer0_slot
            pkt.extend_from_slice(&1u32.to_le_bytes()); // buffer1_slot
            pkt.extend_from_slice(&2u32.to_le_bytes()); // return_slot
            pkt.extend_from_slice(&CBP_BUFFER_COUNT.to_le_bytes());
        }
        self.cbp_outbox.push(pkt);
    }

    fn push_lifecycle_event(&mut self, view: u64, state: u16, reason: u16, active_n: u64, active_r: u64) {
        let mut pkt = Vec::with_capacity(CBP_HEADER_BYTES + 24);
        write_header(&mut pkt, OP_LIFECYCLE, 24, self.instance_id, view, 0);
        pkt.extend_from_slice(&state.to_le_bytes());
        pkt.extend_from_slice(&reason.to_le_bytes());
        pkt.extend_from_slice(&0u32.to_le_bytes());
        pkt.extend_from_slice(&active_n.to_le_bytes());
        pkt.extend_from_slice(&active_r.to_le_bytes());
        self.cbp_outbox.push(pkt);
    }

    #[allow(clippy::too_many_arguments)]
    fn push_navigation_event(
        &mut self,
        view: u64,
        req_id: u64,
        n: u64,
        stage: u16,
        failure: u16,
        http_status: u16,
        flags: u16,
        url: &str,
    ) {
        let url_bytes = url.as_bytes();
        let body_len = 24 + url_bytes.len();
        let mut pkt = Vec::with_capacity(CBP_HEADER_BYTES + body_len);
        write_header(&mut pkt, OP_NAVIGATION, body_len, self.instance_id, view, req_id);
        pkt.extend_from_slice(&n.to_le_bytes());
        pkt.extend_from_slice(&stage.to_le_bytes());
        pkt.extend_from_slice(&failure.to_le_bytes());
        pkt.extend_from_slice(&http_status.to_le_bytes());
        pkt.extend_from_slice(&flags.to_le_bytes());
        pkt.extend_from_slice(&0u32.to_le_bytes()); // redirect_hops
        pkt.extend_from_slice(&(url_bytes.len() as u16).to_le_bytes());
        pkt.extend_from_slice(&0u16.to_le_bytes());
        pkt.extend_from_slice(url_bytes);
        self.cbp_outbox.push(pkt);
    }

    fn push_document_state_event(&mut self, view: u64, n: u64, flags: u32) {
        if n == 0 {
            return;
        }
        let mut pkt = Vec::with_capacity(CBP_HEADER_BYTES + 16);
        write_header(&mut pkt, OP_DOCUMENT_STATE, 16, self.instance_id, view, 0);
        pkt.extend_from_slice(&n.to_le_bytes());
        pkt.extend_from_slice(&flags.to_le_bytes());
        pkt.extend_from_slice(&0u32.to_le_bytes());
        self.cbp_outbox.push(pkt);
    }

    fn push_frame_ready_event(&mut self, view: u64, b: u32, kind: u32, n: u64, g: u64, r: u64) {
        let mut pkt = Vec::with_capacity(CBP_HEADER_BYTES + 32);
        write_header(&mut pkt, OP_FRAME_READY, 32, self.instance_id, view, 0);
        pkt.extend_from_slice(&b.to_le_bytes());
        pkt.extend_from_slice(&kind.to_le_bytes());
        pkt.extend_from_slice(&n.to_le_bytes());
        pkt.extend_from_slice(&g.to_le_bytes());
        pkt.extend_from_slice(&r.to_le_bytes());
        self.cbp_outbox.push(pkt);
    }

    fn push_cursor_event(&mut self, view: u64, shape: u16) {
        let mut pkt = Vec::with_capacity(CBP_HEADER_BYTES + 8);
        write_header(&mut pkt, OP_CURSOR, 8, self.instance_id, view, 0);
        pkt.extend_from_slice(&shape.to_le_bytes());
        pkt.extend_from_slice(&0u16.to_le_bytes());
        pkt.extend_from_slice(&0u32.to_le_bytes());
        self.cbp_outbox.push(pkt);
    }

    fn push_clipboard_write_event(&mut self, view: u64, text: &str) {
        if text.is_empty() || text.len() > CBP_URL_MAX {
            return;
        }
        let bytes = text.as_bytes();
        let body_len = 8 + bytes.len();
        let mut pkt = Vec::with_capacity(CBP_HEADER_BYTES + body_len);
        write_header(&mut pkt, OP_CLIPBOARD_WRITE, body_len, self.instance_id, view, 0);
        pkt.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        pkt.extend_from_slice(&0u16.to_le_bytes()); // flags = 0 on WRITE
        pkt.extend_from_slice(&0u32.to_le_bytes());
        pkt.extend_from_slice(bytes);
        self.cbp_outbox.push(pkt);
    }

    /// Handle a raw CBP 1.0 request packet from CoralOS `Browser.app` (`Ferrum`).
    pub fn handle_cbp_request(&mut self, packet: &[u8]) -> Result<(), CbpError> {
        let hdr = decode_cbp_header(packet)?;
        if hdr.request == 0 {
            return Err(CbpError::InvalidField("request.q"));
        }
        let body = &packet[CBP_HEADER_BYTES..];

        match hdr.opcode {
            OP_OPEN_VIEW => {
                if hdr.instance != 0 || hdr.view != 0 || body.len() != CBP_VIEWPORT_BYTES + 8 {
                    return Err(CbpError::InvalidField("open_view.header_or_len"));
                }
                let vp = read_viewport(body, 0)?;
                let capacity = read_u64(body, CBP_VIEWPORT_BYTES)?;
                let needed_px = (vp.stride as u64).saturating_mul(vp.pixel_height as u64);
                let cap = capacity.max(needed_px).max(4096);
                let region_bytes = (CBP_METADATA_BYTES as u64) + cap;

                let view_id = self.next_view_id;
                self.next_view_id += 1;
                self.active_view_id = view_id;
                self.capacity = cap;
                self.region_bytes = region_bytes;

                let alloc_len = (CBP_METADATA_BYTES + (needed_px as usize)).max(CBP_METADATA_BYTES);
                self.frame_buffers[0].resize(alloc_len, 0);
                self.frame_buffers[1].resize(alloc_len, 0);
                encode_cbp_return_page(
                    self.instance_id,
                    view_id,
                    [0, 0],
                    view_id,
                    &mut self.return_page,
                );

                self.views.insert(
                    view_id,
                    CbpViewState {
                        tab_id: 1,
                        lifecycle: LIFE_READY,
                        active_n: 0,
                        viewport: vp,
                        title: String::new(),
                        url: String::new(),
                        doc_flags: 0,
                    },
                );

                let w = vp.pixel_width.max(1);
                let h = vp.pixel_height.max(1);
                self.scratch_fb.resize((w * h) as usize, 0);
                self.core.on_resize(w, h);

                self.push_reply(
                    view_id,
                    hdr.request,
                    OP_OPEN_VIEW,
                    STATUS_ACCEPTED,
                    0,
                    vp.resize,
                    0,
                    CBP_UNUSED_BUFFER,
                    LIFE_STARTING,
                    Some((cap, region_bytes)),
                );
                self.push_lifecycle_event(view_id, LIFE_READY, STATUS_ACCEPTED, 0, vp.resize);
            }
            OP_CREATE_VIEW => {
                if body.len() != CBP_VIEWPORT_BYTES {
                    return Err(CbpError::InvalidField("create_view.len"));
                }
                let vp = read_viewport(body, 0)?;
                if self.views.len() >= CBP_MAX_VIEWS {
                    self.push_reply(
                        hdr.view,
                        hdr.request,
                        OP_CREATE_VIEW,
                        STATUS_ID_EXHAUSTED,
                        0,
                        vp.resize,
                        0,
                        CBP_UNUSED_BUFFER,
                        LIFE_READY,
                        None,
                    );
                    return Ok(());
                }
                let new_view_id = self.next_view_id;
                self.next_view_id += 1;
                let tab_id = new_view_id as u32;
                self.views.insert(
                    new_view_id,
                    CbpViewState {
                        tab_id,
                        lifecycle: LIFE_READY,
                        active_n: 0,
                        viewport: vp,
                        title: String::new(),
                        url: String::new(),
                        doc_flags: 0,
                    },
                );
                self.core.outbound_inputs.push(InputEvent::CreateTab {
                    tab_id,
                    url: String::from("about:blank"),
                });
                self.push_reply(
                    new_view_id,
                    hdr.request,
                    OP_CREATE_VIEW,
                    STATUS_ACCEPTED,
                    0,
                    vp.resize,
                    0,
                    CBP_UNUSED_BUFFER,
                    LIFE_STARTING,
                    None,
                );
                self.push_lifecycle_event(new_view_id, LIFE_READY, STATUS_ACCEPTED, 0, vp.resize);
            }
            OP_ACTIVATE_VIEW => {
                if body.len() != CBP_VIEWPORT_BYTES {
                    return Err(CbpError::InvalidField("activate_view.len"));
                }
                let vp = read_viewport(body, 0)?;
                let Some(v) = self.views.get_mut(&hdr.view) else {
                    self.push_reply(
                        hdr.view,
                        hdr.request,
                        OP_ACTIVATE_VIEW,
                        STATUS_STALE_VIEW,
                        0,
                        0,
                        0,
                        CBP_UNUSED_BUFFER,
                        LIFE_CLOSED,
                        None,
                    );
                    return Ok(());
                };
                v.viewport = vp;
                let active_n = v.active_n;
                let lifecycle = v.lifecycle;
                let tab_id = v.tab_id;
                self.active_view_id = hdr.view;
                self.core
                    .outbound_inputs
                    .push(InputEvent::SwitchTab { tab_id });
                self.core
                    .on_resize(vp.pixel_width.max(1), vp.pixel_height.max(1));
                self.push_reply(
                    hdr.view,
                    hdr.request,
                    OP_ACTIVATE_VIEW,
                    STATUS_ACCEPTED,
                    active_n,
                    vp.resize,
                    0,
                    CBP_UNUSED_BUFFER,
                    lifecycle,
                    None,
                );
            }
            OP_CLOSE_VIEW => {
                let Some(v) = self.views.remove(&hdr.view) else {
                    self.push_reply(
                        hdr.view,
                        hdr.request,
                        OP_CLOSE_VIEW,
                        STATUS_STALE_VIEW,
                        0,
                        0,
                        0,
                        CBP_UNUSED_BUFFER,
                        LIFE_CLOSED,
                        None,
                    );
                    return Ok(());
                };
                self.core
                    .outbound_inputs
                    .push(InputEvent::CloseTab { tab_id: v.tab_id });
                self.push_reply(
                    hdr.view,
                    hdr.request,
                    OP_CLOSE_VIEW,
                    STATUS_ACCEPTED,
                    v.active_n,
                    v.viewport.resize,
                    0,
                    CBP_UNUSED_BUFFER,
                    LIFE_CLOSING,
                    None,
                );
                self.push_lifecycle_event(
                    hdr.view,
                    LIFE_CLOSED,
                    STATUS_ACCEPTED,
                    v.active_n,
                    v.viewport.resize,
                );
            }
            OP_NAVIGATE => {
                if body.len() < 8 {
                    return Err(CbpError::Truncated);
                }
                let url_len = read_u16(body, 0)? as usize;
                if read_u16(body, 2)? != 0 || read_u32(body, 4)? != 0 || body.len() != 8 + url_len {
                    return Err(CbpError::InvalidField("navigate.body"));
                }
                let url =
                    core::str::from_utf8(&body[8..8 + url_len]).map_err(|_| CbpError::InvalidUtf8)?;
                let Some(v) = self.views.get_mut(&hdr.view) else {
                    self.push_reply(
                        hdr.view,
                        hdr.request,
                        OP_NAVIGATE,
                        STATUS_STALE_VIEW,
                        0,
                        0,
                        0,
                        CBP_UNUSED_BUFFER,
                        LIFE_CLOSED,
                        None,
                    );
                    return Ok(());
                };
                let nav_id = self.next_nav_id;
                self.next_nav_id += 1;
                v.active_n = nav_id;
                v.url = String::from(url);
                v.lifecycle = LIFE_LOADING;
                let active_r = v.viewport.resize;

                self.core
                    .outbound_inputs
                    .push(InputEvent::Navigate(String::from(url)));
                self.push_reply(
                    hdr.view,
                    hdr.request,
                    OP_NAVIGATE,
                    STATUS_ACCEPTED,
                    nav_id,
                    0,
                    0,
                    CBP_UNUSED_BUFFER,
                    LIFE_LOADING,
                    None,
                );
                self.push_navigation_event(
                    hdr.view,
                    hdr.request,
                    nav_id,
                    STAGE_ACCEPTED,
                    FAILURE_NONE,
                    0,
                    0,
                    url,
                );
                self.push_lifecycle_event(hdr.view, LIFE_LOADING, STATUS_ACCEPTED, nav_id, active_r);
            }
            OP_BACK | OP_FORWARD | OP_RELOAD | OP_STOP => {
                if body.len() != 8 {
                    return Err(CbpError::InvalidField("history.len"));
                }
                let expected_n = read_u64(body, 0)?;
                let Some(v) = self.views.get_mut(&hdr.view) else {
                    return Ok(());
                };
                if expected_n != v.active_n {
                    let lifecycle = v.lifecycle;
                    self.push_reply(
                        hdr.view,
                        hdr.request,
                        hdr.opcode,
                        STATUS_STALE_NAVIGATION,
                        0,
                        0,
                        0,
                        CBP_UNUSED_BUFFER,
                        lifecycle,
                        None,
                    );
                    return Ok(());
                }
                if hdr.opcode == OP_STOP {
                    v.lifecycle = LIFE_READY;
                    let cur_n = v.active_n;
                    self.core.outbound_inputs.push(InputEvent::Stop);
                    self.push_reply(
                        hdr.view,
                        hdr.request,
                        OP_STOP,
                        STATUS_ACCEPTED,
                        cur_n,
                        0,
                        0,
                        CBP_UNUSED_BUFFER,
                        LIFE_READY,
                        None,
                    );
                } else {
                    let action = match hdr.opcode {
                        OP_BACK => 0,
                        OP_FORWARD => 1,
                        _ => 2,
                    };
                    let nav_id = self.next_nav_id;
                    self.next_nav_id += 1;
                    v.active_n = nav_id;
                    v.lifecycle = LIFE_LOADING;
                    self.core
                        .outbound_inputs
                        .push(InputEvent::NavAction { action });
                    self.push_reply(
                        hdr.view,
                        hdr.request,
                        hdr.opcode,
                        STATUS_ACCEPTED,
                        nav_id,
                        0,
                        0,
                        CBP_UNUSED_BUFFER,
                        LIFE_LOADING,
                        None,
                    );
                }
            }
            OP_RESIZE => {
                if body.len() != CBP_VIEWPORT_BYTES {
                    return Err(CbpError::InvalidField("resize.len"));
                }
                let vp = read_viewport(body, 0)?;
                let Some(v) = self.views.get_mut(&hdr.view) else {
                    return Ok(());
                };
                v.viewport = vp;
                let lifecycle = v.lifecycle;
                let needed_px = (vp.stride as usize).saturating_mul(vp.pixel_height as usize);
                let alloc_len = CBP_METADATA_BYTES + needed_px;
                if self.frame_buffers[0].len() < alloc_len {
                    self.frame_buffers[0].resize(alloc_len, 0);
                    self.frame_buffers[1].resize(alloc_len, 0);
                }
                let w = vp.pixel_width.max(1);
                let h = vp.pixel_height.max(1);
                self.scratch_fb.resize((w * h) as usize, 0);
                self.core.on_resize(w, h);
                self.push_reply(
                    hdr.view,
                    hdr.request,
                    OP_RESIZE,
                    STATUS_ACCEPTED,
                    0,
                    vp.resize,
                    0,
                    CBP_UNUSED_BUFFER,
                    lifecycle,
                    None,
                );
            }
            OP_POINTER => {
                if body.len() != 32 {
                    return Err(CbpError::InvalidField("pointer.len"));
                }
                let n = read_u64(body, 0)?;
                let r = read_u64(body, 8)?;
                let x_q16 = read_i32(body, 16)?;
                let y_q16 = read_i32(body, 20)?;
                let action = read_u16(body, 24)?;
                let button = read_u16(body, 26)?;
                let cbp_mods = read_u16(body, 30)?;
                let Some(v) = self.views.get(&hdr.view) else {
                    return Ok(());
                };
                let lifecycle = v.lifecycle;
                let px_x = x_q16 >> 16;
                let px_y = y_q16 >> 16;
                let mods = Self::translate_mods(cbp_mods);
                self.core.mods = mods;
                match action {
                    POINTER_MOVE | POINTER_LEAVE => {
                        self.core.on_pointer_move(px_x, px_y);
                    }
                    POINTER_DOWN | POINTER_UP => {
                        let btn = match button {
                            BUTTON_LEFT => 1,
                            BUTTON_MIDDLE => 2,
                            BUTTON_RIGHT => 3,
                            _ => 1,
                        };
                        self.core
                            .on_mouse_button(px_x, px_y, btn, action == POINTER_DOWN);
                    }
                    _ => {}
                }
                self.push_reply(
                    hdr.view,
                    hdr.request,
                    OP_POINTER,
                    STATUS_ACCEPTED,
                    n,
                    r,
                    0,
                    CBP_UNUSED_BUFFER,
                    lifecycle,
                    None,
                );
            }
            OP_SCROLL => {
                if body.len() != 36 {
                    return Err(CbpError::InvalidField("scroll.len"));
                }
                let n = read_u64(body, 0)?;
                let r = read_u64(body, 8)?;
                let dx_q16 = read_i32(body, 24)?;
                let dy_q16 = read_i32(body, 28)?;
                let Some(v) = self.views.get(&hdr.view) else {
                    return Ok(());
                };
                let lifecycle = v.lifecycle;
                self.core.on_scroll(dx_q16 >> 16, dy_q16 >> 16);
                self.push_reply(
                    hdr.view,
                    hdr.request,
                    OP_SCROLL,
                    STATUS_ACCEPTED,
                    n,
                    r,
                    0,
                    CBP_UNUSED_BUFFER,
                    lifecycle,
                    None,
                );
            }
            OP_KEY => {
                if body.len() != 24 {
                    return Err(CbpError::InvalidField("key.len"));
                }
                let n = read_u64(body, 0)?;
                let r = read_u64(body, 8)?;
                let usage = read_u16(body, 16)?;
                let action = read_u16(body, 18)?;
                let cbp_mods = read_u16(body, 20)?;
                let Some(v) = self.views.get(&hdr.view) else {
                    return Ok(());
                };
                let lifecycle = v.lifecycle;
                let mods = Self::translate_mods(cbp_mods);
                self.core.mods = mods;
                let keysym = hid_usage_to_keysym(usage, (cbp_mods & CBP_MOD_SHIFT) != 0);
                self.core.on_key(keysym, action != KEY_UP, None);
                self.push_reply(
                    hdr.view,
                    hdr.request,
                    OP_KEY,
                    STATUS_ACCEPTED,
                    n,
                    r,
                    0,
                    CBP_UNUSED_BUFFER,
                    lifecycle,
                    None,
                );
            }
            OP_TEXT => {
                if body.len() < 20 {
                    return Err(CbpError::Truncated);
                }
                let n = read_u64(body, 0)?;
                let r = read_u64(body, 8)?;
                let text_len = read_u16(body, 16)? as usize;
                if body.len() != 20 + text_len {
                    return Err(CbpError::InvalidField("text.len"));
                }
                let text = core::str::from_utf8(&body[20..20 + text_len])
                    .map_err(|_| CbpError::InvalidUtf8)?;
                let Some(v) = self.views.get(&hdr.view) else {
                    return Ok(());
                };
                let lifecycle = v.lifecycle;
                for ch in text.chars() {
                    let mut buf = [0u8; 4];
                    let s = ch.encode_utf8(&mut buf);
                    self.core.on_key(ch as u32, true, Some(s));
                }
                self.push_reply(
                    hdr.view,
                    hdr.request,
                    OP_TEXT,
                    STATUS_ACCEPTED,
                    n,
                    r,
                    0,
                    CBP_UNUSED_BUFFER,
                    lifecycle,
                    None,
                );
            }
            OP_CLIPBOARD_SET => {
                if body.len() < 8 {
                    return Err(CbpError::Truncated);
                }
                let text_len = read_u16(body, 0)? as usize;
                let flags = read_u16(body, 2)?;
                if body.len() != 8 + text_len {
                    return Err(CbpError::InvalidField("clipboard_set.len"));
                }
                let text = core::str::from_utf8(&body[8..8 + text_len])
                    .map_err(|_| CbpError::InvalidUtf8)?;
                let Some(v) = self.views.get(&hdr.view) else {
                    return Ok(());
                };
                let lifecycle = v.lifecycle;
                let active_n = v.active_n;
                let active_r = v.viewport.resize;
                if (flags & CLIPBOARD_AVAILABLE) != 0 {
                    self.core.clipboard = String::from(text);
                    self.core
                        .outbound_inputs
                        .push(InputEvent::Clipboard(String::from(text)));
                } else {
                    self.core.clipboard.clear();
                }
                self.push_reply(
                    hdr.view,
                    hdr.request,
                    OP_CLIPBOARD_SET,
                    STATUS_ACCEPTED,
                    active_n,
                    active_r,
                    0,
                    CBP_UNUSED_BUFFER,
                    lifecycle,
                    None,
                );
            }
            OP_RELEASE_FRAME => {
                if body.len() != 32 {
                    return Err(CbpError::InvalidField("release_frame.len"));
                }
                let b = read_u32(body, 0)?;
                let disposition = read_u16(body, 4)?;
                let n = read_u64(body, 8)?;
                let g = read_u64(body, 16)?;
                let r = read_u64(body, 24)?;
                let Some(v) = self.views.get(&hdr.view) else {
                    return Ok(());
                };
                let lifecycle = v.lifecycle;
                let tab_id = v.tab_id;
                if (b as usize) < 2 && self.buffer_lease_gen[b as usize] == g {
                    self.buffer_lease_gen[b as usize] = 0;
                }
                self.core.outbound_inputs.push(InputEvent::ReleaseFrame {
                    view_id: tab_id,
                    nav_id: n,
                    resize_id: r,
                    generation: g,
                    disposition: disposition as u8,
                });
                self.push_reply(
                    hdr.view,
                    hdr.request,
                    OP_RELEASE_FRAME,
                    STATUS_ACCEPTED,
                    n,
                    r,
                    g,
                    b,
                    lifecycle,
                    None,
                );
            }
            OP_GET_STATUS | OP_SCREEN | OP_VIDEO_LAYER_ANSWER => {
                let Some(v) = self.views.get(&hdr.view) else {
                    return Ok(());
                };
                let (n, r) = if hdr.opcode == OP_SCREEN {
                    (0, 0)
                } else {
                    (v.active_n, v.viewport.resize)
                };
                let lifecycle = v.lifecycle;
                self.push_reply(
                    hdr.view,
                    hdr.request,
                    hdr.opcode,
                    STATUS_ACCEPTED,
                    n,
                    r,
                    0,
                    CBP_UNUSED_BUFFER,
                    lifecycle,
                    None,
                );
            }
            other => return Err(CbpError::UnknownOpcode(other)),
        }
        Ok(())
    }

    fn translate_mods(cbp_mods: u16) -> u32 {
        let mut m = 0u32;
        if (cbp_mods & CBP_MOD_CTRL) != 0 {
            m |= MOD_CTRL;
        }
        if (cbp_mods & CBP_MOD_SHIFT) != 0 {
            m |= MOD_SHIFT;
        }
        if (cbp_mods & CBP_MOD_ALT) != 0 {
            m |= MOD_ALT;
        }
        if (cbp_mods & CBP_MOD_META) != 0 {
            m |= MOD_META;
        }
        m
    }

    /// Ingest a chunk of `CoralCloud` (`gutted_proto::Message`) wire bytes, update the active
    /// view's composited framebuffer, and publish CBP 1.0 `FrameHeader` + `OP_FRAME_READY` /
    /// `OP_NAVIGATION` / `OP_DOCUMENT_STATE` / `OP_CURSOR` / `OP_CLIPBOARD_WRITE` events.
    pub fn ingest_cloud_wire(&mut self, mut chunk: &[u8]) -> Result<usize, gutted_proto::Error> {
        let mut count = 0usize;
        let mut frame_updated = false;
        while let Some(msg) = CloudMessage::decode(&mut chunk)? {
            count += 1;
            match &msg {
                CloudMessage::RawFrame { .. }
                | CloudMessage::Subframe { .. }
                | CloudMessage::CopyRect { .. }
                | CloudMessage::TileRef { .. } => {
                    frame_updated = true;
                }
                CloudMessage::Title { title } => {
                    if let Some(v) = self.views.get_mut(&self.active_view_id) {
                        v.title = title.clone();
                    }
                }
                CloudMessage::UrlChanged { url } => {
                    let view_id = self.active_view_id;
                    if let Some(v) = self.views.get_mut(&view_id) {
                        if v.active_n == 0 {
                            v.active_n = self.next_nav_id;
                            self.next_nav_id += 1;
                        }
                        v.url = url.clone();
                        let nav_id = v.active_n;
                        self.push_navigation_event(
                            view_id,
                            0,
                            nav_id,
                            STAGE_COMMITTED,
                            FAILURE_NONE,
                            200,
                            0,
                            url,
                        );
                    }
                }
                CloudMessage::LoadState { state } => {
                    let view_id = self.active_view_id;
                    if let Some(v) = self.views.get_mut(&view_id) {
                        if *state == 3 {
                            v.lifecycle = LIFE_READY;
                            v.doc_flags |= DOC_LOAD_EVENT_FIRED;
                            let nav_id = v.active_n;
                            let active_r = v.viewport.resize;
                            let doc_flags = v.doc_flags;
                            self.push_document_state_event(view_id, nav_id, doc_flags);
                            self.push_lifecycle_event(
                                view_id,
                                LIFE_READY,
                                STATUS_ACCEPTED,
                                nav_id,
                                active_r,
                            );
                        }
                    }
                }
                CloudMessage::CursorState { shape, .. } => {
                    let view_id = self.active_view_id;
                    if view_id != 0 {
                        self.push_cursor_event(view_id, cloud_cursor_to_cbp(*shape as u8));
                    }
                }
                CloudMessage::Clipboard { text } => {
                    let view_id = self.active_view_id;
                    if view_id != 0 && !text.is_empty() {
                        self.push_clipboard_write_event(view_id, text);
                    }
                }
                _ => {}
            }

            let mut single = Vec::new();
            msg.encode(&mut single);
            let _ = self.core.ingest_wire_chunk(&single);
        }

        if frame_updated && self.active_view_id != 0 {
            self.publish_active_view_frame();
        }
        Ok(count)
    }

    fn publish_active_view_frame(&mut self) {
        let view_id = self.active_view_id;
        let Some(v) = self.views.get(&view_id).cloned() else {
            return;
        };
        let w = v.viewport.pixel_width.max(1);
        let h = v.viewport.pixel_height.max(1);
        let needed_u32 = (w * h) as usize;
        if self.scratch_fb.len() < needed_u32 {
            self.scratch_fb.resize(needed_u32, 0);
        }
        let _ = self.core.render_into(&mut self.scratch_fb[..needed_u32]);

        let slot = (self.next_buffer_slot % CBP_BUFFER_COUNT) as usize;
        self.next_buffer_slot = (self.next_buffer_slot + 1) % CBP_BUFFER_COUNT;
        let gen = self.next_generation;
        self.next_generation += 1;
        self.buffer_lease_gen[slot] = gen;

        let (kind, nav_id, title, url) = if v.active_n == 0 || v.url.is_empty() {
            (FRAME_BLANK, 0, String::new(), String::new())
        } else {
            let mut safe_title = v.title.clone();
            if safe_title.len() > CBP_TITLE_MAX {
                safe_title.truncate(CBP_TITLE_MAX);
            }
            (FRAME_SITE, v.active_n, safe_title, v.url.clone())
        };

        let pixel_bytes = v.viewport.pixel_bytes();
        let fh = CbpFrameHeader {
            instance: self.instance_id,
            view: view_id,
            navigation: nav_id,
            generation: gen,
            resize: v.viewport.resize,
            buffer: slot as u32,
            kind,
            published: gen,
            viewport: v.viewport,
            region_bytes: self.region_bytes,
            pixel_bytes,
            title,
            url,
        };

        let total_region_len = CBP_METADATA_BYTES + (pixel_bytes as usize);
        if self.frame_buffers[slot].len() < total_region_len {
            self.frame_buffers[slot].resize(total_region_len, 0);
        }
        let mut meta = [0u8; CBP_METADATA_BYTES];
        if encode_cbp_frame_header(&fh, &mut meta).is_ok() {
            self.frame_buffers[slot][..CBP_METADATA_BYTES].copy_from_slice(&meta);
            let stride = v.viewport.stride as usize;
            let row_bytes = (w as usize) * 4;
            let px_dst = &mut self.frame_buffers[slot][CBP_METADATA_BYTES..total_region_len];
            for y in 0..(h as usize) {
                let dst_row = &mut px_dst[y * stride..y * stride + row_bytes];
                let src_row = &self.scratch_fb[y * (w as usize)..(y + 1) * (w as usize)];
                let (dst_chunks, _) = dst_row.as_chunks_mut::<4>();
                for (px_u32, dst_chunk) in src_row.iter().zip(dst_chunks) {
                    *dst_chunk = (*px_u32 | 0xFF00_0000).to_le_bytes();
                }
            }
            self.push_frame_ready_event(view_id, slot as u32, kind, nav_id, gen, v.viewport.resize);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_open_view_req(q: u64, vp: CbpViewport, capacity: u64) -> Vec<u8> {
        let mut pkt = Vec::with_capacity(CBP_HEADER_BYTES + CBP_VIEWPORT_BYTES + 8);
        write_header(&mut pkt, OP_OPEN_VIEW, CBP_VIEWPORT_BYTES + 8, 0, 0, q);
        pkt.extend_from_slice(&vp.resize.to_le_bytes());
        pkt.extend_from_slice(&vp.logical_width_q16.to_le_bytes());
        pkt.extend_from_slice(&vp.logical_height_q16.to_le_bytes());
        pkt.extend_from_slice(&vp.pixel_width.to_le_bytes());
        pkt.extend_from_slice(&vp.pixel_height.to_le_bytes());
        pkt.extend_from_slice(&vp.stride.to_le_bytes());
        pkt.extend_from_slice(&vp.scale_q16.to_le_bytes());
        pkt.extend_from_slice(&capacity.to_le_bytes());
        pkt
    }

    fn build_navigate_req(i: u64, v: u64, q: u64, url: &str) -> Vec<u8> {
        let u = url.as_bytes();
        let mut pkt = Vec::with_capacity(CBP_HEADER_BYTES + 8 + u.len());
        write_header(&mut pkt, OP_NAVIGATE, 8 + u.len(), i, v, q);
        pkt.extend_from_slice(&(u.len() as u16).to_le_bytes());
        pkt.extend_from_slice(&0u16.to_le_bytes());
        pkt.extend_from_slice(&0u32.to_le_bytes());
        pkt.extend_from_slice(u);
        pkt
    }

    #[allow(clippy::too_many_arguments)]
    fn build_release_frame_req(
        i: u64,
        v: u64,
        q: u64,
        b: u32,
        disp: u16,
        n: u64,
        g: u64,
        r: u64,
    ) -> Vec<u8> {
        let mut pkt = Vec::with_capacity(CBP_HEADER_BYTES + 32);
        write_header(&mut pkt, OP_RELEASE_FRAME, 32, i, v, q);
        pkt.extend_from_slice(&b.to_le_bytes());
        pkt.extend_from_slice(&disp.to_le_bytes());
        pkt.extend_from_slice(&0u16.to_le_bytes());
        pkt.extend_from_slice(&n.to_le_bytes());
        pkt.extend_from_slice(&g.to_le_bytes());
        pkt.extend_from_slice(&r.to_le_bytes());
        pkt
    }

    #[test]
    fn cbp_bridge_open_navigate_frame_publication_and_release_roundtrip() {
        let mut bridge = CbpCloudBridge::new(42);
        let vp = CbpViewport::new_1x(1, 64, 64);
        assert_eq!(vp.stride, 256); // 64 * 4 = 256 (multiple of 64)

        // 1. OpenView request from CoralOS Browser.app
        let open_req = build_open_view_req(1, vp, 64 * 1024);
        bridge.handle_cbp_request(&open_req).unwrap();
        let pkts = bridge.drain_cbp_packets();
        assert_eq!(pkts.len(), 2); // OP_REPLY(OpenView) + OP_LIFECYCLE(Ready)
        let rep_hdr = decode_cbp_header(&pkts[0]).unwrap();
        assert_eq!(rep_hdr.opcode, OP_REPLY);
        assert_eq!(rep_hdr.instance, 42);
        assert_eq!(rep_hdr.view, 1);
        assert_eq!(rep_hdr.request, 1);
        let life_hdr = decode_cbp_header(&pkts[1]).unwrap();
        assert_eq!(life_hdr.opcode, OP_LIFECYCLE);

        // 2. Navigate request -> CoralCloud Message::Nav
        let nav_req = build_navigate_req(42, 1, 2, "https://coralos.org/ferrum");
        bridge.handle_cbp_request(&nav_req).unwrap();
        let nav_pkts = bridge.drain_cbp_packets();
        assert_eq!(nav_pkts.len(), 3); // OP_REPLY + OP_NAVIGATION(Accepted) + OP_LIFECYCLE(Loading)

        let mut cloud_out = Vec::new();
        bridge.drain_cloud_wire(1000, &mut cloud_out);
        let mut cur = cloud_out.as_slice();
        let mut saw_cloud_nav = false;
        while let Some(m) = CloudMessage::decode(&mut cur).unwrap() {
            if let CloudMessage::Nav { url } = m {
                assert_eq!(url, "https://coralos.org/ferrum");
                saw_cloud_nav = true;
            }
        }
        assert!(saw_cloud_nav);

        // 3. CoralCloud sends Title, UrlChanged, RawFrame (64x64), and LoadState(Finished)
        let mut cloud_in = Vec::new();
        CloudMessage::Title {
            title: "CoralOS Ferrum".into(),
        }
        .encode(&mut cloud_in);
        CloudMessage::UrlChanged {
            url: "https://coralos.org/ferrum".into(),
        }
        .encode(&mut cloud_in);
        CloudMessage::RawFrame {
            ts_us: 2000,
            width: 64,
            height: 64,
            stride: 256,
            format: 0,
            compression: gutted_proto::compression::RAW,
            pixels: [0x44u8, 0x33, 0x22, 0xFF].repeat(64 * 64),
        }
        .encode(&mut cloud_in);
        CloudMessage::LoadState { state: 3 }.encode(&mut cloud_in);

        bridge.ingest_cloud_wire(&cloud_in).unwrap();
        let ev_pkts = bridge.drain_cbp_packets();
        // Expect OP_NAVIGATION(Committed), OP_DOCUMENT_STATE, OP_LIFECYCLE(Ready), and OP_FRAME_READY
        assert!(ev_pkts
            .iter()
            .any(|p| decode_cbp_header(p).unwrap().opcode == OP_FRAME_READY));

        // 4. Verify the 4096-byte CBP 1.0 FrameHeader & BGRX8_SRGB_OPAQUE pixels in buffer 0
        let fh = bridge.read_frame_header(0).unwrap();
        assert_eq!(fh.instance, 42);
        assert_eq!(fh.view, 1);
        assert_eq!(fh.navigation, 1);
        assert_eq!(fh.generation, 1);
        assert_eq!(fh.resize, 1);
        assert_eq!(fh.kind, FRAME_SITE);
        assert_eq!(fh.title, "CoralOS Ferrum");
        assert_eq!(fh.url, "https://coralos.org/ferrum");
        assert_eq!(
            &bridge.frame_buffers[0][CBP_METADATA_BYTES..CBP_METADATA_BYTES + 4],
            &[0x44, 0x33, 0x22, 0xFF]
        );

        // 5. ReleaseFrame from CoralOS Browser.app frees the lease and forwards ReleaseFrame to CoralCloud
        let rel_req = build_release_frame_req(42, 1, 3, 0, DISPOSITION_COPIED, 1, 1, 1);
        bridge.handle_cbp_request(&rel_req).unwrap();
        assert_eq!(bridge.buffer_lease_gen[0], 0);
        let mut rel_cloud = Vec::new();
        bridge.drain_cloud_wire(3000, &mut rel_cloud);
        let mut rcur = rel_cloud.as_slice();
        let mut saw_release = false;
        while let Some(m) = CloudMessage::decode(&mut rcur).unwrap() {
            if let CloudMessage::ReleaseFrame {
                generation,
                disposition,
                ..
            } = m
            {
                assert_eq!(generation, 1);
                assert_eq!(disposition, 1);
                saw_release = true;
            }
        }
        assert!(saw_release);
    }
}
