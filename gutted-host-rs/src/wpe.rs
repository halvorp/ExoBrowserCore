//! Safe wrapper around ../host/gutted_wpe.{c,h} with native multi-tab support.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

type WpeHandle = usize;

#[repr(C)]
struct GuttedWpeCallbacks {
    on_frame:        unsafe extern "C" fn(*mut c_void, u32, *const u8, i32, i32, i32, u32),
    on_load:         unsafe extern "C" fn(*mut c_void, u32, i32),
    on_cursor:       unsafe extern "C" fn(*mut c_void, u32, i32),
    on_ready:        unsafe extern "C" fn(*mut c_void, *mut c_void),
    on_title:        unsafe extern "C" fn(*mut c_void, u32, *const c_char),
    on_url:          unsafe extern "C" fn(*mut c_void, u32, *const c_char),
    on_file_chooser: unsafe extern "C" fn(*mut c_void, u32, bool, *const c_char),
    on_download:     unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char),
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
    fn gutted_wpe_create_tab(h: *mut c_void, tab_id: u32, url: *const c_char);
    fn gutted_wpe_close_tab(h: *mut c_void, tab_id: u32);
    fn gutted_wpe_set_active_tab(h: *mut c_void, active_tab_id: u32);
    fn gutted_wpe_load_uri(h: *mut c_void, tab_id: u32, url: *const c_char);
    fn gutted_wpe_resize(h: *mut c_void, tab_id: u32, w: u32, h_px: u32);
    fn gutted_wpe_resize_all(h: *mut c_void, w: u32, h_px: u32);
    fn gutted_wpe_inject_pointer_motion(h: *mut c_void, tab_id: u32, x: i32, y: i32, modifiers: u32);
    fn gutted_wpe_inject_pointer_button(h: *mut c_void, tab_id: u32, x: i32, y: i32, button: u32, pressed: bool, modifiers: u32);
    fn gutted_wpe_inject_key(h: *mut c_void, tab_id: u32, keysym: u32, modifiers: u32, pressed: bool);
    fn gutted_wpe_inject_axis(h: *mut c_void, tab_id: u32, x: i32, y: i32, dx: f64, dy: f64, modifiers: u32);
    fn gutted_wpe_set_zoom(h: *mut c_void, tab_id: u32, level: f64);
    fn gutted_wpe_go_back(h: *mut c_void, tab_id: u32);
    fn gutted_wpe_go_forward(h: *mut c_void, tab_id: u32);
    fn gutted_wpe_reload(h: *mut c_void, tab_id: u32);
    fn gutted_wpe_stop_loading(h: *mut c_void, tab_id: u32);
    fn gutted_wpe_clear_data(h: *mut c_void, clear_cookies: bool, clear_cache: bool, clear_storage: bool);
    fn gutted_wpe_complete_file_chooser(
        h: *mut c_void,
        tab_id: u32,
        file_path: *const c_char,
        filename: *const c_char,
        mime_type: *const c_char,
        b64_data: *const c_char,
    );
    fn gutted_wpe_paste_text(h: *mut c_void, tab_id: u32, text: *const c_char);
}

/// A frame from WPE: BGRA (ARGB8888 in wl_shm terms).
#[derive(Debug, Clone)]
pub struct Frame {
    pub tab_id: u32,
    pub width:  i32,
    pub height: i32,
    pub stride: i32,
    pub format: u32,
    pub pixels: Arc<Vec<u8>>,
}

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

struct CallbackBridge {
    frames:           mpsc::UnboundedSender<Frame>,
    loads:            mpsc::UnboundedSender<(u32, LoadState)>,
    cursors:          mpsc::UnboundedSender<(u32, u8)>,
    titles:           mpsc::UnboundedSender<(u32, String)>,
    urls:             mpsc::UnboundedSender<(u32, String)>,
    file_choosers:    mpsc::UnboundedSender<(u32, bool, String)>,
    downloads:        mpsc::UnboundedSender<(String, String)>,
    handle:           Arc<Mutex<Option<WpeHandle>>>,
    active_tab_id:    Arc<AtomicU32>,
    last_bg_frame_ms: AtomicU64,
}

unsafe extern "C" fn frame_trampoline(
    ud: *mut c_void,
    tab_id: u32,
    pixels: *const u8,
    w: i32, h: i32, stride: i32, fmt: u32,
) {
    if ud.is_null() || pixels.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    if bridge.active_tab_id.load(Ordering::Relaxed) != tab_id {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let prev_ms = bridge.last_bg_frame_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev_ms) < 500 {
            return;
        }
        bridge.last_bg_frame_ms.store(now_ms, Ordering::Relaxed);
    }
    let len = (stride as usize).saturating_mul(h as usize);
    let slice = std::slice::from_raw_parts(pixels, len);
    let mut buf = Vec::with_capacity(len);
    buf.extend_from_slice(slice);
    let _ = bridge.frames.send(Frame { tab_id, width: w, height: h, stride, format: fmt, pixels: Arc::new(buf) });
}

unsafe extern "C" fn load_trampoline(ud: *mut c_void, tab_id: u32, s: i32) {
    if ud.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    let _ = bridge.loads.send((tab_id, LoadState::from(s)));
}

unsafe extern "C" fn cursor_trampoline(ud: *mut c_void, tab_id: u32, shape: i32) {
    if ud.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    let _ = bridge.cursors.send((tab_id, shape.clamp(0, 255) as u8));
}

unsafe extern "C" fn ready_trampoline(ud: *mut c_void, handle: *mut c_void) {
    if ud.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    if let Ok(mut slot) = bridge.handle.lock() {
        *slot = Some(handle as WpeHandle);
    }
}

unsafe extern "C" fn title_trampoline(ud: *mut c_void, tab_id: u32, title: *const c_char) {
    if ud.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    let s = if title.is_null() {
        String::new()
    } else {
        std::ffi::CStr::from_ptr(title).to_string_lossy().into_owned()
    };
    let _ = bridge.titles.send((tab_id, s));
}

unsafe extern "C" fn url_trampoline(ud: *mut c_void, tab_id: u32, url: *const c_char) {
    if ud.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    let s = if url.is_null() {
        String::new()
    } else {
        std::ffi::CStr::from_ptr(url).to_string_lossy().into_owned()
    };
    let _ = bridge.urls.send((tab_id, s));
}

unsafe extern "C" fn file_chooser_trampoline(
    ud: *mut c_void,
    tab_id: u32,
    multiple: bool,
    accept_mimes: *const c_char,
) {
    if ud.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    let s = if accept_mimes.is_null() {
        String::new()
    } else {
        std::ffi::CStr::from_ptr(accept_mimes).to_string_lossy().into_owned()
    };
    let _ = bridge.file_choosers.send((tab_id, multiple, s));
}

unsafe extern "C" fn download_trampoline(
    ud: *mut c_void,
    filename: *const c_char,
    local_path: *const c_char,
) {
    if ud.is_null() || filename.is_null() || local_path.is_null() { return; }
    let bridge = &*(ud as *const CallbackBridge);
    let fname = std::ffi::CStr::from_ptr(filename).to_string_lossy().into_owned();
    let fpath = std::ffi::CStr::from_ptr(local_path).to_string_lossy().into_owned();
    let _ = bridge.downloads.send((fname, fpath));
}

pub struct WpeRunner {
    _bridge: Box<CallbackBridge>,
    thread:  Option<std::thread::JoinHandle<c_int>>,
    handle:  Arc<Mutex<Option<WpeHandle>>>,
    stopped: AtomicBool,
}

pub struct WpeChannels {
    pub frames:        mpsc::UnboundedReceiver<Frame>,
    pub loads:         mpsc::UnboundedReceiver<(u32, LoadState)>,
    pub cursors:       mpsc::UnboundedReceiver<(u32, u8)>,
    pub titles:        mpsc::UnboundedReceiver<(u32, String)>,
    pub urls:          mpsc::UnboundedReceiver<(u32, String)>,
    pub file_choosers: mpsc::UnboundedReceiver<(u32, bool, String)>,
    pub downloads:     mpsc::UnboundedReceiver<(String, String)>,
}

impl WpeRunner {
    pub fn start(
        initial_url: &str,
        viewport_w: i32,
        viewport_h: i32,
        active_tab_id: Arc<AtomicU32>,
    ) -> (Self, WpeChannels) {
        let (ftx, frx) = mpsc::unbounded_channel();
        let (ltx, lrx) = mpsc::unbounded_channel();
        let (ctx, crx) = mpsc::unbounded_channel();
        let (ttx, trx) = mpsc::unbounded_channel();
        let (utx, urx) = mpsc::unbounded_channel();
        let (fctx, fcrx) = mpsc::unbounded_channel();
        let (dltx, dlrx) = mpsc::unbounded_channel();
        let handle = Arc::new(Mutex::new(None));
        let bridge = Box::new(CallbackBridge {
            frames: ftx,
            loads: ltx,
            cursors: ctx,
            titles: ttx,
            urls: utx,
            file_choosers: fctx,
            downloads: dltx,
            handle: handle.clone(),
            active_tab_id,
            last_bg_frame_ms: AtomicU64::new(0),
        });
        let bridge_addr: usize = (&*bridge as *const CallbackBridge) as usize;

        let url_c = CString::new(initial_url).expect("URL must not contain NUL");
        let thread = std::thread::Builder::new()
            .name("gutted-wpe".into())
            .spawn(move || {
                let cbs = GuttedWpeCallbacks {
                    on_frame:        frame_trampoline,
                    on_load:         load_trampoline,
                    on_cursor:       cursor_trampoline,
                    on_ready:        ready_trampoline,
                    on_title:        title_trampoline,
                    on_url:          url_trampoline,
                    on_file_chooser: file_chooser_trampoline,
                    on_download:     download_trampoline,
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
            WpeChannels {
                frames: frx,
                loads: lrx,
                cursors: crx,
                titles: trx,
                urls: urx,
                file_choosers: fcrx,
                downloads: dlrx,
            },
        )
    }

    pub fn h(&self) -> Option<WpeHandle> {
        self.handle.lock().ok().and_then(|g| *g)
    }

    pub fn stop(&mut self) -> c_int {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            if let Some(h) = self.h() {
                unsafe { gutted_wpe_stop(h as *mut c_void) };
            }
        }
        self.thread.take().map(|t| t.join().unwrap_or(-99)).unwrap_or(0)
    }

    pub fn create_tab(&self, tab_id: u32, url: &str) {
        if let (Some(h), Ok(c)) = (self.h(), CString::new(url)) {
            unsafe { gutted_wpe_create_tab(h as *mut c_void, tab_id, c.as_ptr()) };
        }
    }

    pub fn close_tab(&self, tab_id: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_close_tab(h as *mut c_void, tab_id) };
        }
    }

    pub fn set_active_tab(&self, active_tab_id: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_set_active_tab(h as *mut c_void, active_tab_id) };
        }
    }

    pub fn load_uri(&self, tab_id: u32, url: &str) {
        if let (Some(h), Ok(c)) = (self.h(), CString::new(url)) {
            unsafe { gutted_wpe_load_uri(h as *mut c_void, tab_id, c.as_ptr()) };
        }
    }

    #[allow(dead_code)]
    pub fn resize(&self, tab_id: u32, w: u32, h_px: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_resize(h as *mut c_void, tab_id, w, h_px) };
        }
    }

    pub fn resize_all(&self, w: u32, h_px: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_resize_all(h as *mut c_void, w, h_px) };
        }
    }

    pub fn inject_pointer_motion(&self, tab_id: u32, x: i32, y: i32, modifiers: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_inject_pointer_motion(h as *mut c_void, tab_id, x, y, modifiers) };
        }
    }

    pub fn inject_pointer_button(&self, tab_id: u32, x: i32, y: i32, button: u32, pressed: bool, modifiers: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_inject_pointer_button(h as *mut c_void, tab_id, x, y, button, pressed, modifiers) };
        }
    }

    pub fn inject_key(&self, tab_id: u32, keysym: u32, modifiers: u32, pressed: bool) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_inject_key(h as *mut c_void, tab_id, keysym, modifiers, pressed) };
        }
    }

    pub fn inject_axis(&self, tab_id: u32, x: i32, y: i32, dx: f64, dy: f64, modifiers: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_inject_axis(h as *mut c_void, tab_id, x, y, dx, dy, modifiers) };
        }
    }

    pub fn set_zoom(&self, tab_id: u32, level: f64) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_set_zoom(h as *mut c_void, tab_id, level) };
        }
    }

    pub fn go_back(&self, tab_id: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_go_back(h as *mut c_void, tab_id) };
        }
    }

    pub fn go_forward(&self, tab_id: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_go_forward(h as *mut c_void, tab_id) };
        }
    }

    pub fn reload(&self, tab_id: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_reload(h as *mut c_void, tab_id) };
        }
    }

    pub fn stop_loading(&self, tab_id: u32) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_stop_loading(h as *mut c_void, tab_id) };
        }
    }

    pub fn clear_data(&self, clear_cookies: bool, clear_cache: bool, clear_storage: bool) {
        if let Some(h) = self.h() {
            unsafe { gutted_wpe_clear_data(h as *mut c_void, clear_cookies, clear_cache, clear_storage) };
        }
    }

    pub fn complete_file_chooser(
        &self,
        tab_id: u32,
        file_path: &str,
        filename: &str,
        mime_type: &str,
        b64_data: &str,
    ) {
        if let (Some(h), Ok(fp), Ok(fnm), Ok(mm), Ok(b64)) = (
            self.h(),
            CString::new(file_path),
            CString::new(filename),
            CString::new(mime_type),
            CString::new(b64_data),
        ) {
            unsafe {
                gutted_wpe_complete_file_chooser(
                    h as *mut c_void,
                    tab_id,
                    fp.as_ptr(),
                    fnm.as_ptr(),
                    mm.as_ptr(),
                    b64.as_ptr(),
                );
            }
        }
    }

    pub fn paste_text(&self, tab_id: u32, text: &str) {
        if let (Some(h), Ok(c)) = (self.h(), CString::new(text)) {
            unsafe { gutted_wpe_paste_text(h as *mut c_void, tab_id, c.as_ptr()) };
        }
    }
}

impl Drop for WpeRunner {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

unsafe impl Send for WpeRunner {}
