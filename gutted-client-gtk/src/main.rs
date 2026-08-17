//! gutted-client-gtk: GTK4-based Linux client.
//!
//! Uses GTK4 for real window/text/input so we can prove the design fast
//! on Linux without hand-rolling a wgpu compositor. Same wire (proto +
//! QUIC) as the wgpu client; the host doesn't know which client is talking.

mod net;

use anyhow::Context;
use gtk4::cairo;
use gtk4::gdk;
use gtk4::glib::{self, clone, translate::IntoGlib};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Button, Entry, EventControllerKey, EventControllerMotion,
    EventControllerScroll, EventControllerScrollFlags, GestureClick, HeaderBar, Label, Orientation,
    Picture, Box as GtkBox,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// Canonical framebuffer state kept on the GTK main thread. RawFrame
/// replaces it wholesale; Subframe blits into it. We rebuild a fresh
/// `GdkMemoryTexture` per update because GdkMemoryTexture is immutable.
struct Composite {
    w: u32,
    h: u32,
    stride: u32,
    pixels: Vec<u8>,
}

const APP_ID: &str = "dev.gutted.browser.client";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,quinn=warn".into()),
        )
        .init();

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    let _ = app.run_with_args::<&str>(&[]);
}

/// Prepend `https://` if the user typed a bare domain like "example.com".
/// Passes through anything with an explicit scheme (`about:`, `data:`,
/// `file:`, `http:`, `https:`, etc.). Empty → about:blank.
fn canonicalize_url(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() { return "about:blank".into(); }
    if let Some(colon) = s.find(':') {
        let scheme_ok = colon > 0
            && s[..colon].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            && s[..colon].chars().next().map_or(false, |c| c.is_ascii_alphabetic());
        if scheme_ok { return s.into(); }
    }
    format!("https://{s}")
}

/// A/V Synchronization Clock & Jitter Buffer
#[derive(Debug)]
pub struct AVClock {
    master_audio_pts: u64,
    last_audio_tick: Option<std::time::Instant>,
}

impl AVClock {
    pub fn new() -> Self {
        Self {
            master_audio_pts: 0,
            last_audio_tick: None,
        }
    }

    pub fn on_audio_frame(&mut self, pts_us: u64, duration_us: u64) {
        self.master_audio_pts = pts_us.saturating_add(duration_us);
        self.last_audio_tick = Some(std::time::Instant::now());
    }

    pub fn current_master_pts_us(&self) -> u64 {
        if let Some(tick) = self.last_audio_tick {
            let elapsed = tick.elapsed().as_micros() as u64;
            self.master_audio_pts.saturating_add(elapsed)
        } else {
            0
        }
    }

    /// Video frame presentation decision:
    /// - 0 = Present immediately
    /// - 1 = Hold until next tick (early)
    /// - 2 = Drop late frame (lagging > 40ms)
    pub fn schedule_video(&self, pts_us: u64) -> u8 {
        let master = self.current_master_pts_us();
        if master == 0 {
            return 0; // No audio clock established, present immediately
        }
        if pts_us + 40_000 < master {
            2 // Drop late frame (> 40ms late)
        } else if pts_us > master + 20_000 {
            1 // Hold (early)
        } else {
            0 // In sync (within -40ms .. +20ms)
        }
    }
}

const BOOKMARKS: &[(&str, &str)] = &[
    ("Example", "https://example.com"),
    ("Wikipedia", "https://www.wikipedia.org"),
    ("HackerNews", "https://news.ycombinator.com"),
    ("DuckDuckGo", "https://duckduckgo.com"),
    ("WebKit", "https://webkit.org"),
    ("Rust", "https://www.rust-lang.org"),
    ("QUIC", "https://en.wikipedia.org/wiki/QUIC"),
];

/// Attach custom stylesheet for modern GTK loading bar and quick launch bar.
fn install_css(display: &gdk::Display) {
    let css = gtk4::CssProvider::new();
    css.load_from_data(
        "entry.loading { \
            box-shadow: inset 0 -3px 0 0 #3b82f6; \
        } \
        .bookmark-chip { \
            border-radius: 12px; \
            padding: 2px 10px; \
            font-size: 12px; \
            margin: 2px; \
        }"
    );
    gtk4::style_context_add_provider_for_display(
        display,
        &css,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn build_ui(app: &Application) {
    let server: std::net::SocketAddr = std::env::var("GBROWSER_SERVER")
        .unwrap_or_else(|_| "127.0.0.1:4433".into())
        .parse()
        .expect("parse server");
    let cert_pin = std::env::var("GBROWSER_CERT_SHA256")
        .ok()
        .map(|s| hex::decode(s.trim()).context("cert pin hex").expect("hex"));

    // GTK-thread → GTK-thread channel for frames from the net side.
    let (frame_tx, frame_rx) = glib::MainContext::channel::<net::GtkFrame>(glib::PRIORITY_DEFAULT);
    // GTK-thread → net-thread channel for outbound commands.
    let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel::<net::OutMsg>();

    // --- HeaderBar UI --------------------------------------------------
    let back_btn = Button::builder().label("◀").tooltip_text("Back (Alt+Left)").build();
    let fwd_btn  = Button::builder().label("▶").tooltip_text("Forward (Alt+Right)").build();
    let reload_btn = Button::builder().label("↻").tooltip_text("Reload (F5)").build();
    let is_loading = Rc::new(Cell::new(false));

    let url_entry = Entry::builder()
        .placeholder_text("Search or enter web address...")
        .hexpand(true)
        .build();

    let zoom_milli: Rc<Cell<u32>> = Rc::new(Cell::new(1000));
    let zoom_out_btn = Button::builder().label("-").tooltip_text("Zoom Out (Ctrl+-)").build();
    let zoom_label = Label::new(Some("100%"));
    let zoom_in_btn = Button::builder().label("+").tooltip_text("Zoom In (Ctrl++)").build();

    let zoom_box = GtkBox::new(Orientation::Horizontal, 2);
    zoom_box.append(&zoom_out_btn);
    zoom_box.append(&zoom_label);
    zoom_box.append(&zoom_in_btn);

    let header = HeaderBar::new();
    header.pack_start(&back_btn);
    header.pack_start(&fwd_btn);
    header.pack_start(&reload_btn);
    header.set_title_widget(Some(&url_entry));
    header.pack_end(&zoom_box);

    // --- Bookmarks Bar UI ----------------------------------------------
    let bookmarks_box = GtkBox::new(Orientation::Horizontal, 4);
    bookmarks_box.set_margin_start(6);
    bookmarks_box.set_margin_end(6);
    bookmarks_box.set_margin_top(2);
    bookmarks_box.set_margin_bottom(2);

    for &(label, url) in BOOKMARKS {
        let chip = Button::builder().label(label).build();
        chip.add_css_class("bookmark-chip");
        let out_tx = out_tx.clone();
        let url_entry = url_entry.clone();
        let target_url = url.to_string();
        chip.connect_clicked(move |_| {
            url_entry.set_text(&target_url);
            let _ = out_tx.send(net::OutMsg::Nav(target_url.clone()));
        });
        bookmarks_box.append(&chip);
    }

    // --- Header button handlers ---
    {
        let out_tx = out_tx.clone();
        back_btn.connect_clicked(move |_| {
            let _ = out_tx.send(net::OutMsg::NavAction { action: 0 });
        });
    }
    {
        let out_tx = out_tx.clone();
        fwd_btn.connect_clicked(move |_| {
            let _ = out_tx.send(net::OutMsg::NavAction { action: 1 });
        });
    }
    {
        let out_tx = out_tx.clone();
        let is_loading = is_loading.clone();
        reload_btn.connect_clicked(move |_| {
            if is_loading.get() {
                let _ = out_tx.send(net::OutMsg::Stop);
            } else {
                let _ = out_tx.send(net::OutMsg::NavAction { action: 2 });
            }
        });
    }

    let update_zoom_ui = clone!(@strong zoom_milli, @weak zoom_label => move || {
        zoom_label.set_text(&format!("{}%", zoom_milli.get() / 10));
    });

    {
        let out_tx = out_tx.clone();
        let zoom_milli = zoom_milli.clone();
        let update_zoom_ui = update_zoom_ui.clone();
        zoom_out_btn.connect_clicked(move |_| {
            let next = (zoom_milli.get() as i32 - 100).clamp(250, 5000) as u32;
            if next != zoom_milli.get() {
                zoom_milli.set(next);
                update_zoom_ui();
                let _ = out_tx.send(net::OutMsg::SetZoom { level_milli: next });
            }
        });
    }
    {
        let out_tx = out_tx.clone();
        let zoom_milli = zoom_milli.clone();
        let update_zoom_ui = update_zoom_ui.clone();
        zoom_in_btn.connect_clicked(move |_| {
            let next = (zoom_milli.get() as i32 + 100).clamp(250, 5000) as u32;
            if next != zoom_milli.get() {
                zoom_milli.set(next);
                update_zoom_ui();
                let _ = out_tx.send(net::OutMsg::SetZoom { level_milli: next });
            }
        });
    }

    let picture = Picture::new();
    picture.set_can_shrink(true);
    picture.set_vexpand(true);
    picture.set_hexpand(true);
    picture.set_can_target(true);
    picture.set_focusable(true);

    let cursor_pos: Rc<Cell<(i32, i32)>> = Rc::new(Cell::new((0, 0)));

    fn picture_origin(picture: &Picture) -> (f64, f64) {
        picture
            .translate_coordinates(&picture.root().unwrap(), 0.0, 0.0)
            .map(|(x, y)| (x, y))
            .unwrap_or((0.0, 0.0))
    }

    // Pointer motion.
    let motion = EventControllerMotion::new();
    {
        let out_tx = out_tx.clone();
        let cursor_pos = cursor_pos.clone();
        let picture = picture.clone();
        let last_sent = std::rc::Rc::new(std::cell::Cell::new(std::time::Instant::now()));
        motion.connect_motion(move |_, x, y| {
            let (ox, oy) = picture_origin(&picture);
            let ix = (x - ox) as i32;
            let iy = (y - oy) as i32;
            if ix < 0 || iy < 0 { return; }
            if cursor_pos.get() == (ix, iy) { return; }
            cursor_pos.set((ix, iy));
            let now = std::time::Instant::now();
            if now.duration_since(last_sent.get()).as_millis() >= 8 {
                last_sent.set(now);
                let _ = out_tx.send(net::OutMsg::PointerMotion { x: ix, y: iy, mods: 0 });
            }
        });
    }

    // Mouse buttons.
    let click = GestureClick::builder().button(0).build();
    {
        let out_tx_p = out_tx.clone();
        let cursor_p = cursor_pos.clone();
        let picture_p = picture.clone();
        click.connect_pressed(move |gesture, _n, x, y| {
            match gesture.current_button() {
                8 => {
                    let _ = out_tx_p.send(net::OutMsg::NavAction { action: 0 });
                    return;
                }
                9 => {
                    let _ = out_tx_p.send(net::OutMsg::NavAction { action: 1 });
                    return;
                }
                _ => {}
            }
            let btn = match gesture.current_button() {
                1 => 1, 2 => 2, 3 => 3, _ => return,
            };
            let mods_bit = 1u32 << (20 + (btn - 1));
            let (ox, oy) = picture_origin(&picture_p);
            let ix = (x - ox) as i32;
            let iy = (y - oy) as i32;
            if ix < 0 || iy < 0 { return; }
            cursor_p.set((ix, iy));
            picture_p.grab_focus();
            let _ = out_tx_p.send(net::OutMsg::PointerButton {
                x: ix, y: iy, button: btn as u32, pressed: true, mods: mods_bit,
            });
        });
        let out_tx_r = out_tx.clone();
        let cursor_r = cursor_pos.clone();
        click.connect_released(move |gesture, _n, _x, _y| {
            let btn = match gesture.current_button() {
                1 => 1, 2 => 2, 3 => 3, _ => return,
            };
            let (cx, cy) = cursor_r.get();
            let _ = out_tx_r.send(net::OutMsg::PointerButton {
                x: cx, y: cy, button: btn as u32, pressed: false, mods: 0,
            });
        });
    }

    // Scroll wheel / trackpad. Ctrl+wheel = zoom; plain wheel = scroll.
    let scroll = EventControllerScroll::new(EventControllerScrollFlags::BOTH_AXES);
    {
        let out_tx = out_tx.clone();
        let zoom_milli = zoom_milli.clone();
        let update_zoom_ui = update_zoom_ui.clone();
        let acc_dx = Rc::new(Cell::new(0.0f64));
        let acc_dy = Rc::new(Cell::new(0.0f64));
        scroll.connect_scroll(move |ctrl, dx, dy| {
            let ctrl_down = ctrl.current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK);
            if ctrl_down {
                let step: i32 = if dy < 0.0 { 125 } else if dy > 0.0 { -125 } else { 0 };
                if step != 0 {
                    let next = (zoom_milli.get() as i32 + step).clamp(250, 5000) as u32;
                    if next != zoom_milli.get() {
                        zoom_milli.set(next);
                        update_zoom_ui();
                        let _ = out_tx.send(net::OutMsg::SetZoom { level_milli: next });
                    }
                }
                return gtk4::Inhibit(true);
            }
            let acc_x = acc_dx.get() + dx;
            let acc_y = acc_dy.get() + dy;

            let send_x = if acc_x > 0.0 { acc_x.ceil() as i32 } else { acc_x.floor() as i32 };
            let send_y = if acc_y > 0.0 { acc_y.ceil() as i32 } else { acc_y.floor() as i32 };

            if send_x != 0 || send_y != 0 {
                acc_dx.set(acc_x - send_x as f64);
                acc_dy.set(acc_y - send_y as f64);
                let _ = out_tx.send(net::OutMsg::Scroll { dx: send_x, dy: send_y });
            } else {
                acc_dx.set(acc_x);
                acc_dy.set(acc_y);
            }
            gtk4::Inhibit(false)
        });
    }

    let vbox = GtkBox::new(Orientation::Vertical, 0);
    vbox.append(&header);
    vbox.append(&bookmarks_box);
    vbox.append(&picture);

    let win = ApplicationWindow::builder()
        .application(app)
        .default_width(1280)
        .default_height(768)
        .title("gutted-browser (GTK)")
        .child(&vbox)
        .build();

    win.add_controller(motion);
    win.add_controller(click);
    win.add_controller(scroll);

    // Keyboard controller: forward keyboard events to WPE backend when typing on web view,
    // while catching browser shortcuts (Ctrl+L, Alt+Left/Right, F5, Esc, Zoom).
    let key = EventControllerKey::new();
    {
        let out_tx_press = out_tx.clone();
        let entry = url_entry.clone();
        let zoom_milli = zoom_milli.clone();
        let update_zoom_ui = update_zoom_ui.clone();
        key.connect_key_pressed(move |_, keyval, _keycode, state| {
            use gtk4::gdk::Key;

            // Shortcut: Ctrl+0 -> reset zoom
            if state.contains(gdk::ModifierType::CONTROL_MASK) && keyval == Key::_0 {
                if zoom_milli.get() != 1000 {
                    zoom_milli.set(1000);
                    update_zoom_ui();
                    let _ = out_tx_press.send(net::OutMsg::SetZoom { level_milli: 1000 });
                }
                return gtk4::Inhibit(true);
            }
            // Shortcut: Ctrl+= / Ctrl+- -> zoom
            if state.contains(gdk::ModifierType::CONTROL_MASK) {
                let step: i32 = match keyval {
                    Key::equal | Key::plus | Key::KP_Add => 125,
                    Key::minus | Key::KP_Subtract => -125,
                    _ => 0,
                };
                if step != 0 {
                    let next = (zoom_milli.get() as i32 + step).clamp(250, 5000) as u32;
                    if next != zoom_milli.get() {
                        zoom_milli.set(next);
                        update_zoom_ui();
                        let _ = out_tx_press.send(net::OutMsg::SetZoom { level_milli: next });
                    }
                    return gtk4::Inhibit(true);
                }
            }
            // Shortcut: Alt+Left / Alt+Right -> history back/forward
            if state.contains(gdk::ModifierType::ALT_MASK) {
                if keyval == Key::Left {
                    let _ = out_tx_press.send(net::OutMsg::NavAction { action: 0 });
                    return gtk4::Inhibit(true);
                }
                if keyval == Key::Right {
                    let _ = out_tx_press.send(net::OutMsg::NavAction { action: 1 });
                    return gtk4::Inhibit(true);
                }
            }
            // Shortcut: Ctrl+L -> focus URL bar
            if state.contains(gdk::ModifierType::CONTROL_MASK) && keyval == Key::l {
                entry.grab_focus();
                entry.select_region(0, -1);
                return gtk4::Inhibit(true);
            }
            // Shortcut: F5 -> reload
            if keyval == Key::F5 {
                let _ = out_tx_press.send(net::OutMsg::NavAction { action: 2 });
                return gtk4::Inhibit(true);
            }
            // Shortcut: Escape -> stop load
            if keyval == Key::Escape {
                let _ = out_tx_press.send(net::OutMsg::Stop);
                return gtk4::Inhibit(true);
            }

            // If URL entry has focus, let GTK entry handle typing
            if entry.has_focus() {
                return gtk4::Inhibit(false);
            }

            // Forward text/key events to WPE backend
            let mut mods = 0u32;
            if state.contains(gdk::ModifierType::SHIFT_MASK)   { mods |= 1 << 0; }
            if state.contains(gdk::ModifierType::LOCK_MASK)    { mods |= 1 << 1; }
            if state.contains(gdk::ModifierType::CONTROL_MASK) { mods |= 1 << 2; }
            if state.contains(gdk::ModifierType::ALT_MASK)     { mods |= 1 << 3; }

            let _ = out_tx_press.send(net::OutMsg::Key {
                keycode: keyval.into_glib(),
                mods,
                down: true,
            });
            gtk4::Inhibit(false)
        });

        let out_tx_rel = out_tx.clone();
        let entry_rel = url_entry.clone();
        key.connect_key_released(move |_, keyval, _keycode, state| {
            if entry_rel.has_focus() {
                return;
            }
            let mut mods = 0u32;
            if state.contains(gdk::ModifierType::SHIFT_MASK)   { mods |= 1 << 0; }
            if state.contains(gdk::ModifierType::LOCK_MASK)    { mods |= 1 << 1; }
            if state.contains(gdk::ModifierType::CONTROL_MASK) { mods |= 1 << 2; }
            if state.contains(gdk::ModifierType::ALT_MASK)     { mods |= 1 << 3; }

            let _ = out_tx_rel.send(net::OutMsg::Key {
                keycode: keyval.into_glib(),
                mods,
                down: false,
            });
        });
    }
    win.add_controller(key);

    // Notify WPE backend on viewport resize
    {
        let last = Rc::new(Cell::new((1280i32, 648i32)));
        let out_tx = out_tx.clone();
        let picture = picture.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            let w = picture.width();
            let h = picture.height();
            if w > 0 && h > 0 && (w, h) != last.get() {
                last.set((w, h));
                let _ = out_tx.send(net::OutMsg::Resize { w: w as u16, h: h as u16 });
            }
            glib::Continue(true)
        });
    }

    // URL entry activate -> navigate
    let out_tx_url = out_tx.clone();
    url_entry.connect_activate(move |e| {
        let raw = e.text().to_string();
        let url = canonicalize_url(&raw);
        if url != raw { e.set_text(&url); }
        let _ = out_tx_url.send(net::OutMsg::Nav(url));
    });

    let composite: Rc<RefCell<Option<Composite>>> = Rc::new(RefCell::new(None));
    let tile_cache: Rc<RefCell<HashMap<u64, Vec<u8>>>> = Rc::new(RefCell::new(HashMap::new()));
    let asset_store: Rc<RefCell<HashMap<[u8; 32], Vec<u8>>>> = Rc::new(RefCell::new(HashMap::new()));
    let av_clock: Rc<RefCell<AVClock>> = Rc::new(RefCell::new(AVClock::new()));

    let url_entry_for_load = url_entry.clone();
    let url_entry_for_url  = url_entry.clone();
    let reload_btn_for_load = reload_btn.clone();
    let is_loading_for_load = is_loading.clone();
    let win_for_cursor = win.clone();
    let dirty: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    frame_rx.attach(None, clone!(
        @weak picture,
        @strong composite,
        @strong tile_cache,
        @strong asset_store,
        @strong av_clock,
        @strong dirty
    => @default-return glib::Continue(false), move |f| {
        {
            let mut cell = composite.borrow_mut();
            match f {
                net::GtkFrame::Load(s) => {
                    let ctx = url_entry_for_load.style_context();
                    if s < 3 {
                        ctx.add_class("loading");
                        reload_btn_for_load.set_label("✕");
                        is_loading_for_load.set(true);
                    } else {
                        ctx.remove_class("loading");
                        reload_btn_for_load.set_label("↻");
                        is_loading_for_load.set(false);
                    }
                    return glib::Continue(true);
                }
                net::GtkFrame::Url(u) => {
                    if !url_entry_for_url.has_focus() {
                        url_entry_for_url.set_text(&u);
                    }
                    return glib::Continue(true);
                }
                net::GtkFrame::Title(t) => {
                    let full = if t.is_empty() { "gutted-browser (GTK)".into() }
                               else { format!("{t} — gutted-browser (GTK)") };
                    win_for_cursor.set_title(Some(&full));
                    return glib::Continue(true);
                }
                net::GtkFrame::UrlChanged(u) => {
                    if !url_entry_for_url.has_focus() {
                        url_entry_for_url.set_text(&u);
                    }
                    return glib::Continue(true);
                }
                net::GtkFrame::Cursor(shape) => {
                    use gutted_proto::CursorShape;
                    let cursor_name = match shape {
                        CursorShape::Pointer    => "pointer",
                        CursorShape::Text       => "text",
                        CursorShape::Crosshair  => "crosshair",
                        CursorShape::Move       => "move",
                        CursorShape::NotAllowed => "not-allowed",
                        CursorShape::Grab       => "grab",
                        CursorShape::Grabbing   => "grabbing",
                        CursorShape::Wait       => "wait",
                        CursorShape::Progress   => "progress",
                        CursorShape::ResizeEw   => "ew-resize",
                        CursorShape::ResizeNs   => "ns-resize",
                        CursorShape::ResizeNesw => "nesw-resize",
                        CursorShape::ResizeNwse => "nwse-resize",
                        _                       => "default",
                    };
                    if let Some(_disp) = gdk::Display::default() {
                        let cursor = gdk::Cursor::from_name(cursor_name, None);
                        win_for_cursor.set_cursor(cursor.as_ref());
                    }
                    return glib::Continue(true);
                }
                net::GtkFrame::TileData { hash, _w: _, _h: _, _stride: _, pixels } => {
                    tile_cache.borrow_mut().insert(hash, pixels);
                    return glib::Continue(true);
                }
                net::GtkFrame::AssetRegister { hash, _kind: _, data } => {
                    asset_store.borrow_mut().insert(hash, data);
                    return glib::Continue(true);
                }
                net::GtkFrame::DrawCommands { _layer_id: _, commands } => {
                    if let Some(c) = cell.as_mut() {
                        let all_fill_rect = commands.iter().all(|cmd| matches!(cmd, gutted_proto::DrawCommand::FillRect { .. }));
                        if all_fill_rect {
                            for cmd in commands {
                                if let gutted_proto::DrawCommand::FillRect { x, y, w, h, rgba } = cmd {
                                    let r = ((rgba >> 24) & 0xFF) as u8;
                                    let g = ((rgba >> 16) & 0xFF) as u8;
                                    let b = ((rgba >> 8) & 0xFF) as u8;
                                    let a = (rgba & 0xFF) as u8;
                                    let a_val = if a == 0 { 0xFF } else { a };
                                    for row in 0..h as usize {
                                        let dst_y = y.max(0) as usize + row;
                                        if dst_y >= c.h as usize { break; }
                                        let dst_off = dst_y * c.stride as usize + (x.max(0) as usize) * 4;
                                        let fill_w = (w as usize).min(c.w as usize - x.max(0) as usize);
                                        for px in c.pixels[dst_off .. dst_off + fill_w * 4].chunks_exact_mut(4) {
                                            px[0] = b; px[1] = g; px[2] = r; px[3] = a_val;
                                        }
                                    }
                                }
                            }
                        } else {
                            let surface = unsafe {
                                cairo::ImageSurface::create_for_data_unsafe(
                                    c.pixels.as_mut_ptr(),
                                    cairo::Format::ARgb32,
                                    c.w as i32,
                                    c.h as i32,
                                    c.stride as i32,
                                ).ok()
                            };
                            if let Some(surf) = surface {
                                if let Ok(cr) = cairo::Context::new(&surf) {
                                    for cmd in commands {
                                        match cmd {
                                            gutted_proto::DrawCommand::FillRect { x, y, w, h, rgba } => {
                                                let r = ((rgba >> 24) & 0xFF) as f64 / 255.0;
                                                let g = ((rgba >> 16) & 0xFF) as f64 / 255.0;
                                                let b = ((rgba >> 8) & 0xFF) as f64 / 255.0;
                                                let a = (rgba & 0xFF) as f64 / 255.0;
                                                cr.set_source_rgba(r, g, b, a);
                                                cr.rectangle(x as f64, y as f64, w as f64, h as f64);
                                                let _ = cr.fill();
                                            }
                                            gutted_proto::DrawCommand::StrokeRect { x, y, w, h, rgba, line_width } => {
                                                let r = ((rgba >> 24) & 0xFF) as f64 / 255.0;
                                                let g = ((rgba >> 16) & 0xFF) as f64 / 255.0;
                                                let b = ((rgba >> 8) & 0xFF) as f64 / 255.0;
                                                let a = (rgba & 0xFF) as f64 / 255.0;
                                                cr.set_source_rgba(r, g, b, a);
                                                cr.set_line_width(line_width as f64);
                                                cr.rectangle(x as f64, y as f64, w as f64, h as f64);
                                                let _ = cr.stroke();
                                            }
                                            gutted_proto::DrawCommand::DrawText { x, y, font_size, rgba, text } => {
                                                let r = ((rgba >> 24) & 0xFF) as f64 / 255.0;
                                                let g = ((rgba >> 16) & 0xFF) as f64 / 255.0;
                                                let b = ((rgba >> 8) & 0xFF) as f64 / 255.0;
                                                let a = (rgba & 0xFF) as f64 / 255.0;
                                                cr.set_source_rgba(r, g, b, a);
                                                cr.set_font_size(font_size as f64);
                                                cr.move_to(x as f64, y as f64);
                                                let _ = cr.show_text(&text);
                                            }
                                            gutted_proto::DrawCommand::SetClip { x, y, w, h } => {
                                                cr.rectangle(x as f64, y as f64, w as f64, h as f64);
                                                cr.clip();
                                            }
                                            gutted_proto::DrawCommand::ClearClip => {
                                                cr.reset_clip();
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                net::GtkFrame::TileRef { x, y, w, h, hash } => {
                    if let Some(c) = cell.as_mut() {
                        if let Some(pixels) = tile_cache.borrow().get(&hash) {
                            if x + w <= c.w && y + h <= c.h {
                                for row in 0..h as usize {
                                    let src_off = row * (w as usize) * 4;
                                    let dst_off = (y as usize + row) * c.stride as usize + x as usize * 4;
                                    c.pixels[dst_off .. dst_off + (w as usize) * 4]
                                        .copy_from_slice(&pixels[src_off .. src_off + (w as usize) * 4]);
                                }
                            }
                        }
                    }
                }
                net::GtkFrame::Audio { pts_us, codec: _, channels: _, sample_rate: _, data: _ } => {
                    av_clock.borrow_mut().on_audio_frame(pts_us, 20_000);
                    return glib::Continue(true);
                }
                net::GtkFrame::VideoChunk { pts_us, duration_us: _, is_keyframe: _, codec: _, layer_id: _, data: _ } => {
                    let decision = av_clock.borrow().schedule_video(pts_us);
                    if decision == 2 {
                        // Late frame dropped (> 40ms behind master audio clock)
                        return glib::Continue(true);
                    }
                    // In-sync video chunk accepted for presentation
                    return glib::Continue(true);
                }
                net::GtkFrame::Full { width, height, stride, mut pixels } => {
                    for chunk in pixels.chunks_exact_mut(4) {
                        if chunk[3] == 0 { chunk[3] = 0xFF; }
                    }
                    *cell = Some(Composite { w: width, h: height, stride, pixels });
                }
                net::GtkFrame::Sub { x, y, w, h, stride, pixels } => {
                    if let Some(c) = cell.as_mut() {
                        if x + w > c.w || y + h > c.h {
                            return glib::Continue(true);
                        }
                        let row_len = w as usize * 4;
                        for row in 0..h as usize {
                            let src_off = row * stride as usize;
                            let dst_off = (y as usize + row) * c.stride as usize + x as usize * 4;
                            let dst_slice = &mut c.pixels[dst_off .. dst_off + row_len];
                            dst_slice.copy_from_slice(&pixels[src_off .. src_off + row_len]);
                            for px in dst_slice.chunks_exact_mut(4) {
                                if px[3] == 0 { px[3] = 0xFF; }
                            }
                        }
                    } else {
                        return glib::Continue(true);
                    }
                }
            }
        }
        // Schedule single texture rebuild per main-loop tick
        if !dirty.replace(true) {
            let composite = composite.clone();
            let dirty = dirty.clone();
            let picture = picture.clone();
            glib::idle_add_local_once(move || {
                let cell = composite.borrow();
                if let Some(c) = cell.as_ref() {
                    let bytes = glib::Bytes::from(&c.pixels[..]);
                    let tex = gdk::MemoryTexture::new(
                        c.w as i32, c.h as i32,
                        gdk::MemoryFormat::B8g8r8a8,
                        &bytes,
                        c.stride as usize,
                    );
                    picture.set_paintable(Some(&tex));

                    if let Ok(shot_path) = std::env::var("GBROWSER_SCREENSHOT") {
                        let shot_after: u64 = std::env::var("GBROWSER_SCREENSHOT_AFTER")
                            .ok().and_then(|s| s.parse().ok()).unwrap_or(2);
                        static SHOT_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                        let cnt = SHOT_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        if cnt == shot_after {
                            let _ = save_ppm(&shot_path, c.w, c.h, c.stride, &c.pixels);
                            tracing::info!(path = %shot_path, "GTK screenshot captured");
                        }
                    }
                }
                dirty.set(false);
            });
        }
        glib::Continue(true)
    }));

    if let Ok(hold_str) = std::env::var("GBROWSER_HOLD_SECS") {
        if let Ok(secs) = hold_str.parse::<u64>() {
            let app_clone = app.clone();
            let composite = composite.clone();
            glib::timeout_add_local_once(std::time::Duration::from_secs(secs), move || {
                if let Ok(shot_path) = std::env::var("GBROWSER_SCREENSHOT") {
                    let cell = composite.borrow();
                    if let Some(c) = cell.as_ref() {
                        let _ = save_ppm(&shot_path, c.w, c.h, c.stride, &c.pixels);
                    }
                }
                app_clone.quit();
            });
        }
    }

    // Spawn network thread
    std::thread::Builder::new()
        .name("gutted-gtk-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all().build().expect("tokio rt");
            if let Err(e) = rt.block_on(net::run(server, cert_pin, frame_tx, out_rx)) {
                tracing::error!(error = %e, "net thread ended with error");
            }
        }).expect("spawn net");

    if let Some(disp) = gdk::Display::default() { install_css(&disp); }
    win.present();
}

fn save_ppm(path: &str, w: u32, h: u32, stride: u32, bgra: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    write!(f, "P6\n{} {}\n255\n", w, h)?;
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h as usize {
        let row_start = y * stride as usize;
        for x in 0..w as usize {
            let px = row_start + x * 4;
            if px + 2 < bgra.len() {
                rgb.push(bgra[px + 2]); // R
                rgb.push(bgra[px + 1]); // G
                rgb.push(bgra[px + 0]); // B
            }
        }
    }
    f.write_all(&rgb)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av_clock_synchronization_logic() {
        let mut clock = AVClock::new();
        assert_eq!(clock.schedule_video(100_000), 0); // No audio master clock yet -> present

        // Audio starts at PTS 1,000,000 with 20ms duration
        clock.on_audio_frame(1_000_000, 20_000);
        let master = clock.current_master_pts_us();
        assert!(master >= 1_020_000);

        // Video frame on-time (within -40ms .. +20ms of master)
        assert_eq!(clock.schedule_video(master), 0); // PresentNow

        // Video frame too early (> 20ms ahead of master)
        assert_eq!(clock.schedule_video(master + 50_000), 1); // Hold

        // Video frame too late (> 40ms behind master)
        assert_eq!(clock.schedule_video(master.saturating_sub(60_000)), 2); // DropLate
    }
}
