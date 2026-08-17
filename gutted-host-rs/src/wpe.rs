//! Safe-ish wrapper around ../host/gutted_wpe.{c,h}.
//!
//! Multi-instance: each `WpeRunner::start` spawns a dedicated thread
//! that runs its own GLib main loop and WebKitWebView. The C library
//! returns a per-instance handle via the `on_ready` callback; the Rust
//! `WpeRunner` owns that handle and every stop/load/resize/inject_* is
//! scoped to it.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Opaque C handle (`instance_t *`), stored as usize so Rust can move it.
type WpeHandle = usize;

#[repr(C)]
struct GuttedWpeCallbacks {
    on_frame:  unsafe extern "C" fn(*mut c_void, *const u8, i32, i32, i32, u32),
    on_load:   unsafe extern "C" fn(*mut c_void, i32),
    on_cursor: unsafe extern "C" fn(*mut c_void, i32),
    on_ready:  unsafe extern "C" fn(*mut c_void, *mut c_void),
    on_title:  unsafe extern "C" fn(*mut c_void, *const c_char),
    on_url:    unsafe extern "C" fn(*mut c_void, *const c_char),
}

extern "C" {
    fn gutted_wpe_run(
        initial_url: *const c_char,
        viewport_w: i32,
        viewport_h: i32,
        cb: *const GuttedWpeCallbacks,
        userdata: *mut c_void,
    ) -> c_int;
    fn gutted_wpe_stop(h: *mut c_void);
    fn gutted_wpe_load_uri(h: *mut c_void, url: *const c_char);
    fn gutted_wpe_resize(h: *mut c_void, w: u32, h_px: u32);
    fn gutted_wpe_inject_pointer_motion(h: *mut c_void, x: i32, y: i32, modifiers: u32);
    fn gutted_wpe_inject_pointer_button(h: *mut c_void, x: i32, y: i32, button: u32, pressed: bool, modifiers: u32);
    fn gutted_wpe_inject_key(h: *mut c_void, keysym: u32, modifiers: u32, pressed: bool);
    fn gutted_wpe_inject_axis(h: *mut c_void, x: i32, y: i32, dx: f64, dy: f64, modifiers: u32);
    fn gutted_wpe_set_zoom(h: *mut c_void, level: f64);
    fn gutted_wpe_go_back(h: *mut c_void);
    fn gutted_wpe_go_forward(h: *mut c_void);
    fn gutted_wpe_reload(h: *mut c_void);
}

/// A frame from WPE: BGRA (ARGB8888 in wl_shm terms).
#[derive(Debug, Clone)]
pub struct Frame {
    pub width:  i32,
    pub height: i32,
    pub stride: i32,
    /// Wayland SHM format code (0 = ARGB8888).
    pub format: u32,
    /// Row-packed pixel bytes, length = stride * height.
    pub pixels: Vec<u8>,
}

/// A WebKit load state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadState { Started, Redirected, Committed, Finished, Unknown }

impl From<i32> for LoadState {
    fn from(v: i32) -> Self {
        match v {
            0 => Self::Started, 1 => Self::Redirected,
            2 => Self::Committed, 3 => Self::Finished,
            _ => Self::Unknown,
        }
    }
}

/// Anything that outlives `gutted_wpe_run` and is referenced by the
/// C callback via its `userdata` pointer.
struct CallbackBridge {
    frames:  mpsc::UnboundedSender<Frame>,
    loads:   mpsc::UnboundedSender<LoadState>,
    cursors: mpsc::UnboundedSender<u8>,
    titles:  mpsc::UnboundedSender<String>,
    urls:    mpsc::UnboundedSender<String>,
    /// C hands us the instance handle via on_ready; store so the
    /// Rust owner can call stop/load/resize/inject_* on it.
    handle:  Arc<Mutex<Option<WpeHandle>>>,
}

unsafe extern "C" fn frame_trampoline(
    ud: *mut c_void,
    pixels: *const u8,
    w: i32, h: i32, stride: i32, fmt: u32,
) {
    if ud.is_null() || pixels.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    let len = (stride as usize).saturating_mul(h as usize);
    let slice = std::slice::from_raw_parts(pixels, len);
    let mut buf = Vec::with_capacity(len);
    buf.extend_from_slice(slice);
    let _ = bridge.frames.send(Frame { width: w, height: h, stride, format: fmt, pixels: buf });
}

unsafe extern "C" fn load_trampoline(ud: *mut c_void, s: i32) {
    if ud.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    let _ = bridge.loads.send(LoadState::from(s));
}

unsafe extern "C" fn cursor_trampoline(ud: *mut c_void, shape: i32) {
    if ud.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    let _ = bridge.cursors.send(shape.clamp(0, 255) as u8);
}

unsafe extern "C" fn ready_trampoline(ud: *mut c_void, handle: *mut c_void) {
    if ud.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    if let Ok(mut slot) = bridge.handle.lock() {
        *slot = Some(handle as WpeHandle);
    }
}

unsafe extern "C" fn title_trampoline(ud: *mut c_void, title: *const c_char) {
    if ud.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    let s = if title.is_null() {
        String::new()
    } else {
        std::ffi::CStr::from_ptr(title).to_string_lossy().into_owned()
    };
    let _ = bridge.titles.send(s);
}

unsafe extern "C" fn url_trampoline(ud: *mut c_void, url: *const c_char) {
    if ud.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    let s = if url.is_null() {
        String::new()
    } else {
        std::ffi::CStr::from_ptr(url).to_string_lossy().into_owned()
    };
    let _ = bridge.urls.send(s);
}

/// One WPE instance, running on its own OS thread. Dropping stops it.
///
/// The `inject_*` and `resize` methods are pub-and-currently-unused —
/// they're the multi-tab-ready per-runner API. Today's ctrl handler
/// still calls the process-scoped compat shims (`wpe::load_uri` etc.)
/// via `install_as_current`. When we route per-tab, the shims go away
/// and these become the only path. Suppressing dead_code for now.
#[allow(dead_code)]
pub struct WpeRunner {
    _bridge: Box<CallbackBridge>,
    thread:  Option<std::thread::JoinHandle<c_int>>,
    handle:  Arc<Mutex<Option<WpeHandle>>>,
    stopped: AtomicBool,
}

impl WpeRunner {
    /// Start WPE. Returns the runner + receivers for frames, load state, and cursor.
    pub fn start(
        initial_url: &str,
        viewport_w: i32,
        viewport_h: i32,
    ) -> (
        Self,
        mpsc::UnboundedReceiver<Frame>,
        mpsc::UnboundedReceiver<LoadState>,
        mpsc::UnboundedReceiver<u8>,
        mpsc::UnboundedReceiver<String>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let (ftx, frx) = mpsc::unbounded_channel();
        let (ltx, lrx) = mpsc::unbounded_channel();
        let (ctx, crx) = mpsc::unbounded_channel();
        let (ttx, trx) = mpsc::unbounded_channel();
        let (utx, urx) = mpsc::unbounded_channel();
        let handle = Arc::new(Mutex::new(None));
        let bridge = Box::new(CallbackBridge {
            frames: ftx, loads: ltx, cursors: ctx, titles: ttx, urls: utx,
            handle: handle.clone(),
        });
        let bridge_addr: usize = (&*bridge as *const CallbackBridge) as usize;

        let url_c = CString::new(initial_url).expect("URL must not contain NUL");
        let thread = std::thread::Builder::new()
            .name("gutted-wpe".into())
            .spawn(move || {
                let cbs = GuttedWpeCallbacks {
                    on_frame:  frame_trampoline,
                    on_load:   load_trampoline,
                    on_cursor: cursor_trampoline,
                    on_ready:  ready_trampoline,
                    on_title:  title_trampoline,
                    on_url:    url_trampoline,
                };
                unsafe {
                    gutted_wpe_run(
                        url_c.as_ptr(),
                        viewport_w, viewport_h,
                        &cbs,
                        bridge_addr as *mut c_void,
                    )
                }
            })
            .expect("spawn wpe thread");

        (
            Self {
                _bridge: bridge,
                thread: Some(thread),
                handle,
                stopped: AtomicBool::new(false),
            },
            frx, lrx, crx, trx, urx,
        )
    }

    /// Handle if the instance has finished setup (may take a few ms
    /// after `start`).
    pub fn h(&self) -> Option<WpeHandle> {
        self.handle.lock().ok().and_then(|g| *g)
    }

    /// Ask the WPE thread to exit and wait for it.
    pub fn stop(&mut self) -> c_int {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            if let Some(h) = self.h() {
                unsafe { gutted_wpe_stop(h as *mut c_void) };
            }
        }
        self.thread.take().map(|t| t.join().unwrap_or(-99)).unwrap_or(0)
    }

    #[allow(dead_code)]
    pub fn load_uri(&self, url: &str) {
        if let (Some(h), Ok(c)) = (self.h(), CString::new(url)) {
            unsafe { gutted_wpe_load_uri(h as *mut c_void, c.as_ptr()) };
        }
    }

    #[allow(dead_code)]
    pub fn resize(&self, w: u32, h_px: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_resize(h as *mut c_void, w, h_px) };
        }
    }

    #[allow(dead_code)]
    pub fn inject_pointer_motion(&self, x: i32, y: i32, modifiers: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_inject_pointer_motion(h as *mut c_void, x, y, modifiers) };
        }
    }
    #[allow(dead_code)]
    pub fn inject_pointer_button(&self, x: i32, y: i32, button: u32, pressed: bool, modifiers: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_inject_pointer_button(h as *mut c_void, x, y, button, pressed, modifiers) };
        }
    }
    #[allow(dead_code)]
    pub fn inject_key(&self, keysym: u32, modifiers: u32, pressed: bool) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_inject_key(h as *mut c_void, keysym, modifiers, pressed) };
        }
    }
    #[allow(dead_code)]
    pub fn inject_axis(&self, x: i32, y: i32, dx: f64, dy: f64, modifiers: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_inject_axis(h as *mut c_void, x, y, dx, dy, modifiers) };
        }
    }
    #[allow(dead_code)]
    pub fn set_zoom(&self, level: f64) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_set_zoom(h as *mut c_void, level) };
        }
    }
}

impl Drop for WpeRunner {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// The bridge lives in a Box; only the raw pointer crosses threads.
unsafe impl Send for WpeRunner {}

#[allow(dead_code)]
const _WL_SHM_FORMAT_ARGB8888: c_uint = 0;

// ─── Single-instance compat shim ─────────────────────────────────────────
//
// Callers today assume one WPE view per process (the "current tab").
// Multi-tab will route via a HashMap<TabId, WpeRunner>; for now, the
// last-started `WpeRunner` publishes its handle into this global so
// legacy `wpe::load_uri` etc. still work without threading a runner
// through every ctrl handler. Delete when tabs land.

use std::sync::OnceLock;

static CURRENT: OnceLock<Mutex<Option<WpeHandle>>> = OnceLock::new();
fn current() -> &'static Mutex<Option<WpeHandle>> {
    CURRENT.get_or_init(|| Mutex::new(None))
}

impl WpeRunner {
    /// Publish this instance's handle as the process-current one. Call
    /// once the on_ready callback has fired (i.e. `h()` is Some).
    pub fn install_as_current(&self) {
        if let Some(h) = self.h() {
            if let Ok(mut slot) = current().lock() { *slot = Some(h); }
        }
    }
}

fn with_current<F: FnOnce(WpeHandle)>(f: F) {
    if let Some(h) = current().lock().ok().and_then(|g| *g) { f(h); }
}

/// Compat: navigate the current tab. No-op if no runner installed.
pub fn load_uri(url: &str) {
    let Ok(c) = CString::new(url) else { return; };
    with_current(|h| unsafe { gutted_wpe_load_uri(h as *mut c_void, c.as_ptr()) });
}
/// Compat: resize the current tab.
pub fn resize(w: u32, h_px: u32) {
    with_current(|h| unsafe { gutted_wpe_resize(h as *mut c_void, w, h_px) });
}
pub fn inject_pointer_motion(x: i32, y: i32, modifiers: u32) {
    with_current(|h| unsafe { gutted_wpe_inject_pointer_motion(h as *mut c_void, x, y, modifiers) });
}
pub fn inject_pointer_button(x: i32, y: i32, button: u32, pressed: bool, modifiers: u32) {
    with_current(|h| unsafe {
        gutted_wpe_inject_pointer_button(h as *mut c_void, x, y, button, pressed, modifiers)
    });
}
pub fn inject_key(keysym: u32, modifiers: u32, pressed: bool) {
    with_current(|h| unsafe { gutted_wpe_inject_key(h as *mut c_void, keysym, modifiers, pressed) });
}
pub fn inject_axis(x: i32, y: i32, dx: f64, dy: f64, modifiers: u32) {
    with_current(|h| unsafe { gutted_wpe_inject_axis(h as *mut c_void, x, y, dx, dy, modifiers) });
}
pub fn set_zoom(level: f64) {
    with_current(|h| unsafe { gutted_wpe_set_zoom(h as *mut c_void, level) });
}
pub fn go_back()    { with_current(|h| unsafe { gutted_wpe_go_back(h as *mut c_void) }); }
pub fn go_forward() { with_current(|h| unsafe { gutted_wpe_go_forward(h as *mut c_void) }); }
pub fn reload()     { with_current(|h| unsafe { gutted_wpe_reload(h as *mut c_void) }); }
