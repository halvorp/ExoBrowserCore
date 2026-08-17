//! gutted-client-gtk: GTK4-based Linux client.
//!
//! Uses GTK4 for real window/text/input so we can prove the design fast
//! on Linux without hand-rolling a wgpu compositor. Same wire (proto +
//! QUIC) as the wgpu client; the host doesn't know which client is talking.

mod net;

use anyhow::Context;
use gtk4::gdk;
use gtk4::glib::{self, clone};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Entry, EventControllerKey, EventControllerMotion,
    EventControllerScroll, EventControllerScrollFlags, GestureClick, HeaderBar, Orientation,
    Picture,
};
use std::cell::{Cell, RefCell};
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
    // Any known scheme or looks-like-a-scheme up to ':'
    if let Some(colon) = s.find(':') {
        // Only treat as scheme if the part before is scheme-shaped (alpha, +, -, .)
        let scheme_ok = colon > 0
            && s[..colon].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            && s[..colon].chars().next().map_or(false, |c| c.is_ascii_alphabetic());
        if scheme_ok { return s.into(); }
    }
    format!("https://{s}")
}

/// F1..F9 bookmarks — same list as the wgpu client's `render::BOOKMARKS`.
/// Kept locally so both clients can diverge as needed.
/// F1..F9 bookmark URLs. F5 is reserved for reload (empty entry).
const BOOKMARKS: &[&str] = &[
    "https://example.com",
    "https://www.wikipedia.org",
    "https://news.ycombinator.com",
    "about:blank",
    "",  // F5 = reload
    "https://startpage.com",
    "https://duckduckgo.com",
    "https://en.wikipedia.org/wiki/QUIC",
    "https://webkit.org",
];

/// Attach a small stylesheet so `.loading` on the URL entry shows an
/// amber underline while a page is loading.
fn install_css(display: &gdk::Display) {
    let css = gtk4::CssProvider::new();
    css.load_from_data(
        "entry.loading { \
            box-shadow: inset 0 -2px 0 0 #ffa726; \
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

    // --- UI ------------------------------------------------------------
    let url_entry = Entry::builder()
        .placeholder_text("Enter a URL, press Enter to navigate")
        .hexpand(true)
        .build();
    let header = HeaderBar::new();
    header.set_title_widget(Some(&url_entry));

    let picture = Picture::new();
    picture.set_can_shrink(true);
    picture.set_vexpand(true);
    picture.set_hexpand(true);
    // Picture is content-only by default; make it receive pointer events.
    picture.set_can_target(true);
    picture.set_focusable(true);

    // Track the last pointer position — button events include coords but
    // we still like a single "current" for symmetry with the wgpu client.
    // Also track allocation to translate ROOT (window) coords → picture coords,
    // which is what WebKit expects.
    let cursor: Rc<Cell<(i32, i32)>> = Rc::new(Cell::new((0, 0)));

    // Attach event controllers to the WINDOW so we catch everything and can
    // translate coordinates ourselves (Picture-attached controllers were
    // silently not firing — likely because Picture consumes its own paintable
    // hit-test in a way that swallows motion). One place to reason about.
    // We'll compute picture-relative coords by subtracting the picture's
    // (x, y) translation inside the window.
    fn picture_origin(picture: &Picture) -> (f64, f64) {
        // translate_coordinates from picture → root gives us the offset.
        picture
            .translate_coordinates(&picture.root().unwrap(), 0.0, 0.0)
            .map(|(x, y)| (x, y))
            .unwrap_or((0.0, 0.0))
    }

    // Pointer motion.
    let motion = EventControllerMotion::new();
    {
        let out_tx = out_tx.clone();
        let cursor = cursor.clone();
        let picture = picture.clone();
        motion.connect_motion(move |_, x, y| {
            let (ox, oy) = picture_origin(&picture);
            let ix = (x - ox) as i32;
            let iy = (y - oy) as i32;
            if ix < 0 || iy < 0 { return; }
            cursor.set((ix, iy));
            let _ = out_tx.send(net::OutMsg::PointerMotion { x: ix, y: iy, mods: 0 });
        });
    }

    // Mouse buttons.
    let click = GestureClick::builder().button(0).build(); // any button
    {
        let out_tx_p = out_tx.clone();
        let cursor_p = cursor.clone();
        let picture_p = picture.clone();
        click.connect_pressed(move |gesture, _n, x, y| {
            // Mouse Back(8) / Forward(9) → history nav; don't forward as button.
            match gesture.current_button() {
                8 => {
                    tracing::info!("GTK mouse-back → NAV_ACTION 0");
                    let _ = out_tx_p.send(net::OutMsg::NavAction { action: 0 });
                    return;
                }
                9 => {
                    tracing::info!("GTK mouse-fwd → NAV_ACTION 1");
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
            tracing::info!(x = ix, y = iy, button = btn, "GTK press → send");
            let _ = out_tx_p.send(net::OutMsg::PointerButton {
                x: ix, y: iy, button: btn as u32, pressed: true, mods: mods_bit,
            });
        });
        let out_tx_r = out_tx.clone();
        let cursor_r = cursor.clone();
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

    // Current page zoom, per-mille (1000 = 100%). Shared with Ctrl+wheel + Ctrl+0.
    let zoom_milli: Rc<Cell<u32>> = Rc::new(Cell::new(1000));

    // Scroll wheel / trackpad. Ctrl+wheel = zoom; plain wheel = scroll.
    let scroll = EventControllerScroll::new(EventControllerScrollFlags::BOTH_AXES);
    {
        let out_tx = out_tx.clone();
        let zoom_milli = zoom_milli.clone();
        scroll.connect_scroll(move |ctrl, dx, dy| {
            let ctrl_down = ctrl.current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK);
            if ctrl_down {
                let step: i32 = if dy < 0.0 { 125 } else if dy > 0.0 { -125 } else { 0 };
                if step != 0 {
                    let next = (zoom_milli.get() as i32 + step).clamp(250, 5000) as u32;
                    if next != zoom_milli.get() {
                        zoom_milli.set(next);
                        tracing::info!(level_milli = next, "GTK zoom → send");
                        let _ = out_tx.send(net::OutMsg::SetZoom { level_milli: next });
                    }
                }
                return gtk4::Inhibit(true);
            }
            tracing::info!(dx, dy, "GTK scroll → send");
            let _ = out_tx.send(net::OutMsg::Scroll {
                dx: dx.round() as i32, dy: dy.round() as i32,
            });
            gtk4::Inhibit(false)
        });
    }

    let vbox = gtk4::Box::new(Orientation::Vertical, 0);
    vbox.append(&header);
    vbox.append(&picture);

    let win = ApplicationWindow::builder()
        .application(app)
        .default_width(1280)
        .default_height(744)
        .title("gutted-browser (GTK)")
        .child(&vbox)
        .build();

    // Attach controllers on the WINDOW (they were silently dropped on Picture).
    win.add_controller(motion);
    win.add_controller(click);
    win.add_controller(scroll);

    // Keyboard: Ctrl+L focuses URL entry, F1..F9 = bookmark nav.
    let key = EventControllerKey::new();
    {
        let out_tx = out_tx.clone();
        let entry = url_entry.clone();
        let zoom_milli = zoom_milli.clone();
        key.connect_key_pressed(move |_, keyval, _keycode, state| {
            use gtk4::gdk::Key;
            // Ctrl+0 → reset zoom to 100%.
            if state.contains(gdk::ModifierType::CONTROL_MASK) && keyval == Key::_0 {
                if zoom_milli.get() != 1000 {
                    zoom_milli.set(1000);
                    let _ = out_tx.send(net::OutMsg::SetZoom { level_milli: 1000 });
                    tracing::info!("GTK zoom reset");
                }
                return gtk4::Inhibit(true);
            }
            // Ctrl+= / Ctrl++ / Ctrl+- → zoom step.
            if state.contains(gdk::ModifierType::CONTROL_MASK) {
                let step: i32 = match keyval {
                    Key::equal | Key::plus | Key::KP_Add       =>  125,
                    Key::minus | Key::KP_Subtract              => -125,
                    _ => 0,
                };
                if step != 0 {
                    let next = (zoom_milli.get() as i32 + step).clamp(250, 5000) as u32;
                    if next != zoom_milli.get() {
                        zoom_milli.set(next);
                        tracing::info!(level_milli = next, "GTK zoom key");
                        let _ = out_tx.send(net::OutMsg::SetZoom { level_milli: next });
                    }
                    return gtk4::Inhibit(true);
                }
            }
            // Alt+Left/Right → history back/forward.
            if state.contains(gdk::ModifierType::ALT_MASK) {
                if keyval == Key::Left {
                    tracing::info!("GTK Alt+Left → NAV_ACTION 0");
                    let _ = out_tx.send(net::OutMsg::NavAction { action: 0 });
                    return gtk4::Inhibit(true);
                }
                if keyval == Key::Right {
                    tracing::info!("GTK Alt+Right → NAV_ACTION 1");
                    let _ = out_tx.send(net::OutMsg::NavAction { action: 1 });
                    return gtk4::Inhibit(true);
                }
            }
            // Ctrl+L → focus the URL bar (real-browser reflex).
            if state.contains(gdk::ModifierType::CONTROL_MASK) && keyval == Key::l {
                entry.grab_focus();
                entry.select_region(0, -1);
                return gtk4::Inhibit(true);
            }
            // F1..F9 → hop to a bookmarked URL.
            let idx: Option<usize> = match keyval {
                Key::F1 => Some(0), Key::F2 => Some(1), Key::F3 => Some(2),
                Key::F4 => Some(3), Key::F5 => Some(4), Key::F6 => Some(5),
                Key::F7 => Some(6), Key::F8 => Some(7), Key::F9 => Some(8),
                _ => None,
            };
            if let Some(i) = idx {
                if i == 4 {
                    // F5 = real WebKit reload (preserves scroll + session state).
                    tracing::info!("GTK F5 → NAV_ACTION reload");
                    let _ = out_tx.send(net::OutMsg::NavAction { action: 2 });
                } else if let Some(url) = BOOKMARKS.get(i) {
                    if !url.is_empty() {
                        entry.set_text(url);
                        let _ = out_tx.send(net::OutMsg::Nav((*url).into()));
                        tracing::info!(url = *url, key = ?keyval, "bookmark nav");
                    }
                }
                return gtk4::Inhibit(true);
            }
            gtk4::Inhibit(false)
        });
    }
    win.add_controller(key);

    // Send Resize whenever the picture's allocated size changes. Poll on
    // a low-frequency timer (100 ms) — GTK4 doesn't expose a clean
    // "notify me on size-allocate" signal for widgets, but polling is
    // free and small deltas won't spam the host.
    {
        let last = Rc::new(Cell::new((0i32, 0i32)));
        let out_tx = out_tx.clone();
        let picture = picture.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            let w = picture.width();
            let h = picture.height();
            if w > 0 && h > 0 && (w, h) != last.get() {
                last.set((w, h));
                tracing::info!(w, h, "GTK picture size → Resize");
                let _ = out_tx.send(net::OutMsg::Resize { w: w as u16, h: h as u16 });
            }
            glib::Continue(true)
        });
    }

    // URL entry → NAV on the ctrl stream.
    let out_tx_url = out_tx.clone();
    url_entry.connect_activate(move |e| {
        let raw = e.text().to_string();
        let url = canonicalize_url(&raw);
        if url != raw { e.set_text(&url); }
        tracing::info!(%url, "NAV entered");
        let _ = out_tx_url.send(net::OutMsg::Nav(url));
    });

    // Persistent composite buffer — RawFrame swaps it, Subframe blits into it.
    let composite: Rc<RefCell<Option<Composite>>> = Rc::new(RefCell::new(None));

    // Frame receiver → apply into composite immediately (fast: memcpy),
    // then schedule ONE texture rebuild per main-loop tick. WPE bursts
    // subframes at ~60Hz during real page loads; without coalescing we'd
    // rebuild the 3.6MB GdkMemoryTexture N times per burst and block the
    // UI thread. With coalescing: one rebuild covers all queued subframes.
    let url_entry_for_load = url_entry.clone();
    let url_entry_for_url  = url_entry.clone();
    let dirty: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    frame_rx.attach(None, clone!(
        @weak picture,
        @strong composite,
        @strong dirty
    => @default-return glib::Continue(false), move |f| {
        {
            let mut cell = composite.borrow_mut();
            match f {
                net::GtkFrame::Load(s) => {
                    let ctx = url_entry_for_load.style_context();
                    if s < 3 { ctx.add_class("loading"); } else { ctx.remove_class("loading"); }
                    // Load state doesn't dirty the framebuffer.
                    return glib::Continue(true);
                }
                net::GtkFrame::Url(u) => {
                    // Only overwrite if the user isn't editing right now,
                    // else we'd clobber their typing.
                    if !url_entry_for_url.has_focus() {
                        url_entry_for_url.set_text(&u);
                    }
                    return glib::Continue(true);
                }
                net::GtkFrame::Title(t) => {
                    if let Some(root) = url_entry_for_url.root().and_downcast_ref::<gtk4::Window>() {
                        let full = if t.is_empty() { "gutted-browser (GTK)".into() }
                                   else { format!("{t} — gutted-browser (GTK)") };
                        root.set_title(Some(&full));
                    }
                    return glib::Continue(true);
                }
                net::GtkFrame::UrlChanged(u) => {
                    if !url_entry_for_url.has_focus() {
                        url_entry_for_url.set_text(&u);
                    }
                    return glib::Continue(true);
                }
                net::GtkFrame::Full { width, height, stride, pixels } => {
                    *cell = Some(Composite { w: width, h: height, stride, pixels });
                }
                net::GtkFrame::Sub { x, y, w, h, stride, pixels } => {
                    if let Some(c) = cell.as_mut() {
                        if x + w > c.w || y + h > c.h {
                            tracing::warn!(x, y, w, h, cw = c.w, ch = c.h, "SUBFRAME OOB, dropped");
                            return glib::Continue(true);
                        }
                        for row in 0..h as usize {
                            let src_off = row * stride as usize;
                            let dst_off = (y as usize + row) * c.stride as usize + x as usize * 4;
                            c.pixels[dst_off .. dst_off + (w as usize) * 4]
                                .copy_from_slice(&pixels[src_off .. src_off + (w as usize) * 4]);
                        }
                    } else {
                        tracing::warn!("SUBFRAME arrived before any RawFrame — no base composite; dropping");
                        return glib::Continue(true);
                    }
                }
            }
        }
        // Schedule a single rebuild per tick.
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
                }
                dirty.set(false);
            });
        }
        glib::Continue(true)
    }));

    // Net thread: dedicated OS thread hosting a tokio runtime.
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
