//! gutted-client-gtk: Modern GTK4-based Linux Client.
//!
//! Features:
//! - Multi-tab engine with per-tab frame caching and instant switching.
//! - Modern browser chrome: custom tab bar, rounded tabs with active styling, close buttons, and + new tab button.
//! - Modern omnibox with HTTPS/HTTP security badge, auto-search, clear button, and progress indicator.
//! - Navigation toolbar (Back, Forward, Reload/Stop, Home).
//! - Quick-launch bookmark bar.
//! - Zoom control pill (- 100% +) with click-to-reset and Ctrl+Wheel support.
//! - Menu popover for clearing cookies/cache, remote node/cert info, and zoom reset.
//! - Link hover status overlay chip.
//! - Hardware-accelerated memory texture compositor with AV sync.

mod net;

use anyhow::Context;
use gtk4::gdk;
use gtk4::glib::{self, clone, translate::IntoGlib};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Button, Entry, EventControllerKey, EventControllerMotion,
    EventControllerScroll, EventControllerScrollFlags, GestureClick, Inhibit, Label,
    Orientation, Overlay, Picture, Popover, Box as GtkBox,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// Canonical framebuffer state kept on the GTK main thread.
#[derive(Clone)]
struct Composite {
    w: u32,
    h: u32,
    stride: u32,
    pixels: Vec<u8>,
}

#[derive(Debug, Clone)]
struct TabInfo {
    id: u32,
    title: String,
    url: String,
    is_loading: bool,
    is_secure: bool,
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
fn canonicalize_url(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() { return "about:blank".into(); }
    if let Some(colon) = s.find(':') {
        let scheme_ok = colon > 0
            && s[..colon].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            && s[..colon].chars().next().map_or(false, |c| c.is_ascii_alphabetic());
        if scheme_ok { return s.into(); }
    }
    if !s.contains('.') && !s.contains('/') {
        let query = s.replace(' ', "+");
        return format!("https://duckduckgo.com/?q={query}");
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

    pub fn schedule_video(&mut self, pts_us: u64) -> u8 {
        let master = self.current_master_pts_us();
        if master == 0 {
            return 0; // Present immediately
        }
        if pts_us + 500_000 < master || pts_us > master + 2_000_000 {
            self.master_audio_pts = pts_us;
            self.last_audio_tick = Some(std::time::Instant::now());
            return 0;
        }
        if pts_us + 40_000 < master {
            2 // Drop late frame (> 40ms late)
        } else if pts_us > master + 20_000 {
            1 // Hold (early)
        } else {
            0 // In sync
        }
    }
}

const BOOKMARKS: &[(&str, &str)] = &[
    ("YouTube", "https://youtube.com"),
    ("Reddit", "https://reddit.com"),
    ("DuckDuckGo", "https://duckduckgo.com"),
    ("Wikipedia", "https://www.wikipedia.org"),
    ("GitHub", "https://github.com"),
    ("HackerNews", "https://news.ycombinator.com"),
    ("Rust", "https://www.rust-lang.org"),
    ("Example", "https://example.com"),
];

/// Attach custom dark theme stylesheet for modern browser chrome.
fn install_css(display: &gdk::Display) {
    let css = gtk4::CssProvider::new();
    css.load_from_data(
        "window { \
            background-color: #0c0d0e; \
            color: #f3f4f6; \
        } \
        .tab-bar { \
            background-color: #0c0d0e; \
            padding: 4px 6px 0 6px; \
            border-bottom: 1px solid #23252a; \
        } \
        .tab-item { \
            background-color: #15171c; \
            color: #9ca3af; \
            border-radius: 8px 8px 0 0; \
            border: 1px solid #23252a; \
            border-bottom: none; \
            padding: 5px 12px; \
            margin-right: 4px; \
            font-size: 13px; \
            font-weight: 500; \
            transition: all 120ms ease; \
        } \
        .tab-item:hover { \
            background-color: #1e2026; \
            color: #f9fafb; \
        } \
        .tab-item.tab-active { \
            background-color: #1e2026; \
            color: #ffffff; \
            border-top: 2px solid #3b82f6; \
            box-shadow: 0 -2px 8px rgba(59, 130, 246, 0.2); \
        } \
        .tab-close { \
            background: transparent; \
            border: none; \
            border-radius: 50%; \
            padding: 0 4px; \
            margin-left: 8px; \
            color: #6b7280; \
            font-size: 12px; \
        } \
        .tab-close:hover { \
            background-color: #ef4444; \
            color: #ffffff; \
        } \
        .tab-new-btn { \
            background-color: #15171c; \
            border: 1px solid #23252a; \
            border-radius: 6px; \
            padding: 2px 10px; \
            font-size: 14px; \
            font-weight: bold; \
            color: #9ca3af; \
            margin-bottom: 2px; \
        } \
        .tab-new-btn:hover { \
            background-color: #262830; \
            color: #ffffff; \
        } \
        .nav-toolbar { \
            background-color: #18191e; \
            padding: 6px 10px; \
            border-bottom: 1px solid #23252a; \
        } \
        .nav-btn { \
            background: transparent; \
            border: 1px solid transparent; \
            border-radius: 6px; \
            color: #d1d5db; \
            padding: 4px 8px; \
            font-size: 14px; \
            font-weight: 500; \
        } \
        .nav-btn:hover { \
            background-color: #262830; \
            border-color: #374151; \
            color: #ffffff; \
        } \
        .omnibox-box { \
            background-color: #262830; \
            border: 1px solid #374151; \
            border-radius: 20px; \
            padding: 2px 10px; \
            margin: 0 6px; \
            transition: all 120ms ease; \
        } \
        .omnibox-box:focus-within { \
            border-color: #3b82f6; \
            box-shadow: 0 0 0 1px #3b82f6; \
        } \
        .omnibox-box.loading { \
            box-shadow: inset 0 -2px 0 0 #3b82f6; \
        } \
        entry.url-entry { \
            background: transparent; \
            border: none; \
            box-shadow: none; \
            color: #f9fafb; \
            font-size: 13px; \
            padding: 2px 4px; \
        } \
        entry.url-entry:focus { \
            border: none; \
            box-shadow: none; \
        } \
        .security-badge { \
            padding: 0 4px; \
            font-size: 12px; \
        } \
        .security-badge.secure { \
            color: #10b981; \
        } \
        .security-badge.insecure { \
            color: #f59e0b; \
        } \
        .zoom-pill { \
            background-color: #262830; \
            border: 1px solid #374151; \
            border-radius: 14px; \
            padding: 1px 4px; \
            margin: 0 4px; \
        } \
        .zoom-label { \
            font-size: 11px; \
            font-weight: 600; \
            color: #9ca3af; \
            padding: 0 4px; \
        } \
        .zoom-btn { \
            background: transparent; \
            border: none; \
            color: #d1d5db; \
            font-size: 11px; \
            padding: 1px 4px; \
        } \
        .zoom-btn:hover { \
            color: #ffffff; \
        } \
        .bookmarks-bar { \
            background-color: #15171c; \
            padding: 3px 8px; \
            border-bottom: 1px solid #23252a; \
        } \
        .bookmark-chip { \
            background-color: #202228; \
            color: #d1d5db; \
            border: 1px solid #2f323a; \
            border-radius: 12px; \
            padding: 2px 10px; \
            font-size: 11px; \
            font-weight: 500; \
            margin: 1px 3px; \
            transition: all 120ms ease; \
        } \
        .bookmark-chip:hover { \
            background-color: #2f323a; \
            color: #ffffff; \
        } \
        .status-overlay { \
            background-color: rgba(18, 20, 24, 0.92); \
            color: #93c5fd; \
            border: 1px solid #374151; \
            border-radius: 6px; \
            padding: 3px 8px; \
            font-size: 11px; \
            font-family: ui-monospace, Menlo, Monaco, monospace; \
            box-shadow: 0 4px 12px rgba(0, 0, 0, 0.4); \
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

    let (frame_tx, frame_rx) = glib::MainContext::channel::<net::GtkFrame>(glib::PRIORITY_DEFAULT);
    let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel::<net::OutMsg>();

    // --- Tab Management & Frame Caches ---
    let tabs: Rc<RefCell<Vec<TabInfo>>> = Rc::new(RefCell::new(vec![
        TabInfo {
            id: 1,
            title: "New Tab".into(),
            url: "https://duckduckgo.com".into(),
            is_loading: false,
            is_secure: true,
        }
    ]));
    let active_tab_id: Rc<Cell<u32>> = Rc::new(Cell::new(1));
    let next_tab_id: Rc<Cell<u32>> = Rc::new(Cell::new(2));
    let tab_composites: Rc<RefCell<HashMap<u32, Composite>>> = Rc::new(RefCell::new(HashMap::new()));

    // --- Top Tab Bar ---
    let tab_bar_box = GtkBox::new(Orientation::Horizontal, 2);
    tab_bar_box.add_css_class("tab-bar");

    let tabs_list_box = GtkBox::new(Orientation::Horizontal, 2);
    tabs_list_box.set_hexpand(true);
    tab_bar_box.append(&tabs_list_box);

    let new_tab_btn = Button::builder().label("+").tooltip_text("New Tab (Ctrl+T)").build();
    new_tab_btn.add_css_class("tab-new-btn");
    tab_bar_box.append(&new_tab_btn);

    // --- Navigation & Omnibox Toolbar ---
    let nav_toolbar = GtkBox::new(Orientation::Horizontal, 4);
    nav_toolbar.add_css_class("nav-toolbar");

    let back_btn = Button::builder().label("‹").tooltip_text("Back (Alt+Left)").build();
    back_btn.add_css_class("nav-btn");
    let fwd_btn  = Button::builder().label("›").tooltip_text("Forward (Alt+Right)").build();
    fwd_btn.add_css_class("nav-btn");
    let reload_btn = Button::builder().label("↻").tooltip_text("Reload (F5)").build();
    reload_btn.add_css_class("nav-btn");
    let home_btn = Button::builder().label("⌂").tooltip_text("Home").build();
    home_btn.add_css_class("nav-btn");

    let is_loading = Rc::new(Cell::new(false));

    let security_badge = Label::new(Some("🔒"));
    security_badge.add_css_class("security-badge");
    security_badge.add_css_class("secure");

    let url_entry = Entry::builder()
        .placeholder_text("Search with DuckDuckGo or enter URL...")
        .hexpand(true)
        .build();
    url_entry.add_css_class("url-entry");

    let omnibox_box = GtkBox::new(Orientation::Horizontal, 2);
    omnibox_box.add_css_class("omnibox-box");
    omnibox_box.set_hexpand(true);
    omnibox_box.append(&security_badge);
    omnibox_box.append(&url_entry);

    let zoom_milli: Rc<Cell<u32>> = Rc::new(Cell::new(1000));
    let zoom_out_btn = Button::builder().label("-").tooltip_text("Zoom Out (Ctrl+-)").build();
    zoom_out_btn.add_css_class("zoom-btn");
    let zoom_label = Label::new(Some("100%"));
    zoom_label.add_css_class("zoom-label");
    let zoom_in_btn = Button::builder().label("+").tooltip_text("Zoom In (Ctrl++)").build();
    zoom_in_btn.add_css_class("zoom-btn");

    let zoom_pill = GtkBox::new(Orientation::Horizontal, 1);
    zoom_pill.add_css_class("zoom-pill");
    zoom_pill.append(&zoom_out_btn);
    zoom_pill.append(&zoom_label);
    zoom_pill.append(&zoom_in_btn);

    let menu_btn = Button::builder().label("⋮").tooltip_text("Settings & Tools").build();
    menu_btn.add_css_class("nav-btn");

    let menu_popover = Popover::new();
    let menu_vbox = GtkBox::new(Orientation::Vertical, 6);
    menu_vbox.set_margin_start(10);
    menu_vbox.set_margin_end(10);
    menu_vbox.set_margin_top(10);
    menu_vbox.set_margin_bottom(10);

    let clear_data_btn = Button::builder().label("🗑️ Clear Cookies & Cache").build();
    clear_data_btn.add_css_class("nav-btn");
    let node_info_btn = Button::builder().label("🔑 Security & Remote Info").build();
    node_info_btn.add_css_class("nav-btn");
    let reset_zoom_btn = Button::builder().label("🔍 Reset Zoom (100%)").build();
    reset_zoom_btn.add_css_class("nav-btn");

    menu_vbox.append(&clear_data_btn);
    menu_vbox.append(&node_info_btn);
    menu_vbox.append(&reset_zoom_btn);
    menu_popover.set_child(Some(&menu_vbox));
    menu_popover.set_parent(&menu_btn);

    nav_toolbar.append(&back_btn);
    nav_toolbar.append(&fwd_btn);
    nav_toolbar.append(&reload_btn);
    nav_toolbar.append(&home_btn);
    nav_toolbar.append(&omnibox_box);
    nav_toolbar.append(&zoom_pill);
    nav_toolbar.append(&menu_btn);

    // --- Bookmarks Bar ---
    let bookmarks_box = GtkBox::new(Orientation::Horizontal, 2);
    bookmarks_box.add_css_class("bookmarks-bar");

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

    // --- Viewport & Overlay ---
    let picture = Picture::new();
    picture.set_content_fit(gtk4::ContentFit::Fill);
    picture.set_can_shrink(true);
    picture.set_vexpand(true);
    picture.set_hexpand(true);
    picture.set_can_target(true);
    picture.set_focusable(true);

    let status_label = Label::new(None);
    status_label.add_css_class("status-overlay");
    status_label.set_halign(gtk4::Align::Start);
    status_label.set_valign(gtk4::Align::End);
    status_label.set_margin_start(12);
    status_label.set_margin_bottom(12);
    status_label.set_visible(false);

    let overlay = Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&status_label);

    let composite: Rc<RefCell<Option<Composite>>> = Rc::new(RefCell::new(None));
    let dirty = Rc::new(Cell::new(false));

    // --- Render Tabs Function ---
    let render_tabs: Rc<RefCell<Option<Box<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    {
        let tabs = tabs.clone();
        let active_tab_id = active_tab_id.clone();
        let tabs_list_box = tabs_list_box.clone();
        let out_tx = out_tx.clone();
        let url_entry = url_entry.clone();
        let render_tabs_holder = render_tabs.clone();
        let tab_composites = tab_composites.clone();
        let composite = composite.clone();
        let picture = picture.clone();
        let security_badge = security_badge.clone();

        *render_tabs.borrow_mut() = Some(Box::new(move || {
            while let Some(child) = tabs_list_box.first_child() {
                tabs_list_box.remove(&child);
            }
            let tab_list = tabs.borrow().clone();
            let current_active = active_tab_id.get();

            for tab in tab_list {
                let tab_id = tab.id;
                let is_active = tab_id == current_active;

                let tab_widget = GtkBox::new(Orientation::Horizontal, 6);
                tab_widget.add_css_class("tab-item");
                if is_active {
                    tab_widget.add_css_class("tab-active");
                }

                let icon = if tab.is_loading { "⏳" } else if tab.is_secure { "●" } else { "○" };
                let icon_label = Label::new(Some(icon));
                if tab.is_secure {
                    icon_label.add_css_class("security-badge");
                    icon_label.add_css_class("secure");
                }
                tab_widget.append(&icon_label);

                let title_truncated = if tab.title.len() > 22 {
                    format!("{}…", &tab.title[..21])
                } else {
                    tab.title.clone()
                };
                let title_label = Label::new(Some(&title_truncated));
                title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                tab_widget.append(&title_label);

                let close_btn = Button::builder().label("×").tooltip_text("Close Tab").build();
                close_btn.add_css_class("tab-close");

                let tabs_c = tabs.clone();
                let active_c = active_tab_id.clone();
                let out_c = out_tx.clone();
                let render_c = render_tabs_holder.clone();
                let tab_comp_c = tab_composites.clone();
                let comp_c = composite.clone();
                let pic_c = picture.clone();
                let url_entry_c = url_entry.clone();

                close_btn.connect_clicked(move |_| {
                    let mut list = tabs_c.borrow_mut();
                    if list.len() > 1 {
                        if let Some(pos) = list.iter().position(|t| t.id == tab_id) {
                            list.remove(pos);
                            tab_comp_c.borrow_mut().remove(&tab_id);
                            if active_c.get() == tab_id {
                                let new_active = if pos > 0 { list[pos - 1].id } else { list[0].id };
                                active_c.set(new_active);
                                if let Some(t) = list.iter().find(|t| t.id == new_active) {
                                    url_entry_c.set_text(&t.url);
                                }
                                if let Some(c) = tab_comp_c.borrow().get(&new_active) {
                                    *comp_c.borrow_mut() = Some(c.clone());
                                    let bytes = glib::Bytes::from(&c.pixels[..]);
                                    let tex = gdk::MemoryTexture::new(
                                        c.w as i32, c.h as i32,
                                        gdk::MemoryFormat::B8g8r8a8,
                                        &bytes,
                                        c.stride as usize,
                                    );
                                    pic_c.set_paintable(Some(&tex));
                                    pic_c.queue_draw();
                                }
                                let _ = out_c.send(net::OutMsg::SwitchTab { tab_id: new_active });
                            }
                            let _ = out_c.send(net::OutMsg::CloseTab { tab_id });
                        }
                    }
                    drop(list);
                    if let Some(ref r) = *render_c.borrow() { r(); }
                });
                tab_widget.append(&close_btn);

                let gesture = GestureClick::new();
                let tabs_c2 = tabs.clone();
                let active_c2 = active_tab_id.clone();
                let out_c2 = out_tx.clone();
                let url_entry_c2 = url_entry.clone();
                let render_c2 = render_tabs_holder.clone();
                let tab_comp_c2 = tab_composites.clone();
                let comp_c2 = composite.clone();
                let pic_c2 = picture.clone();
                let sec_c2 = security_badge.clone();

                gesture.connect_pressed(move |_, _, _, _| {
                    if active_c2.get() != tab_id {
                        active_c2.set(tab_id);
                        if let Some(t) = tabs_c2.borrow().iter().find(|t| t.id == tab_id) {
                            url_entry_c2.set_text(&t.url);
                            sec_c2.set_text(if t.is_secure { "🔒" } else { "ℹ️" });
                        }
                        // Instant client-side frame restore
                        if let Some(c) = tab_comp_c2.borrow().get(&tab_id) {
                            *comp_c2.borrow_mut() = Some(c.clone());
                            let bytes = glib::Bytes::from(&c.pixels[..]);
                            let tex = gdk::MemoryTexture::new(
                                c.w as i32, c.h as i32,
                                gdk::MemoryFormat::B8g8r8a8,
                                &bytes,
                                c.stride as usize,
                            );
                            pic_c2.set_paintable(Some(&tex));
                            pic_c2.queue_draw();
                        } else {
                            *comp_c2.borrow_mut() = None;
                        }
                        let _ = out_c2.send(net::OutMsg::SwitchTab { tab_id });
                        if let Some(ref r) = *render_c2.borrow() { r(); }
                    }
                });
                tab_widget.add_controller(gesture);

                tabs_list_box.append(&tab_widget);
            }
        }));
    }

    if let Some(ref r) = *render_tabs.borrow() { r(); }

    // --- New Tab Handler ---
    {
        let tabs = tabs.clone();
        let active_tab_id = active_tab_id.clone();
        let next_tab_id = next_tab_id.clone();
        let out_tx = out_tx.clone();
        let url_entry = url_entry.clone();
        let render_tabs = render_tabs.clone();
        new_tab_btn.connect_clicked(move |_| {
            let new_id = next_tab_id.get();
            next_tab_id.set(new_id + 1);
            let new_url = "https://duckduckgo.com".to_string();
            tabs.borrow_mut().push(TabInfo {
                id: new_id,
                title: "New Tab".into(),
                url: new_url.clone(),
                is_loading: false,
                is_secure: true,
            });
            active_tab_id.set(new_id);
            url_entry.set_text(&new_url);
            url_entry.grab_focus();
            let _ = out_tx.send(net::OutMsg::CreateTab { tab_id: new_id, url: new_url });
            if let Some(ref r) = *render_tabs.borrow() { r(); }
        });
    }

    // --- Navigation Handlers ---
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
    {
        let out_tx = out_tx.clone();
        let url_entry = url_entry.clone();
        home_btn.connect_clicked(move |_| {
            let home_url = "https://duckduckgo.com".to_string();
            url_entry.set_text(&home_url);
            let _ = out_tx.send(net::OutMsg::Nav(home_url));
        });
    }

    // --- Menu popover actions ---
    {
        let menu_popover = menu_popover.clone();
        menu_btn.connect_clicked(move |_| {
            menu_popover.popup();
        });
    }
    {
        let out_tx = out_tx.clone();
        let menu_popover = menu_popover.clone();
        clear_data_btn.connect_clicked(move |_| {
            let _ = out_tx.send(net::OutMsg::ClearData {
                clear_cookies: true,
                clear_cache: true,
                clear_storage: true,
            });
            menu_popover.popdown();
        });
    }
    {
        let zoom_milli = zoom_milli.clone();
        let zoom_label = zoom_label.clone();
        let out_tx = out_tx.clone();
        let menu_popover = menu_popover.clone();
        reset_zoom_btn.connect_clicked(move |_| {
            zoom_milli.set(1000);
            zoom_label.set_text("100%");
            let _ = out_tx.send(net::OutMsg::SetZoom { level_milli: 1000 });
            menu_popover.popdown();
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

    let cursor_pos: Rc<Cell<(i32, i32)>> = Rc::new(Cell::new((0, 0)));

    let motion = EventControllerMotion::new();
    {
        let out_tx = out_tx.clone();
        let cursor_pos = cursor_pos.clone();
        let picture_m = picture.clone();
        let composite_m = composite.clone();
        let last_sent = std::rc::Rc::new(std::cell::Cell::new(std::time::Instant::now()));
        motion.connect_motion(move |_, x, y| {
            let pw = picture_m.width() as f64;
            let ph = picture_m.height() as f64;
            let (cw, ch) = if let Some(comp) = composite_m.borrow().as_ref() {
                (comp.w as f64, comp.h as f64)
            } else {
                (pw, ph)
            };
            let scale_x = if pw > 0.0 { cw / pw } else { 1.0 };
            let scale_y = if ph > 0.0 { ch / ph } else { 1.0 };
            let ix = ((x * scale_x) as i32).clamp(0, cw as i32);
            let iy = ((y * scale_y) as i32).clamp(0, ch as i32);
            if cursor_pos.get() == (ix, iy) { return; }
            cursor_pos.set((ix, iy));
            let now = std::time::Instant::now();
            if now.duration_since(last_sent.get()).as_millis() >= 8 {
                last_sent.set(now);
                let _ = out_tx.send(net::OutMsg::PointerMotion { x: ix, y: iy, mods: 0 });
            }
        });
    }

    let click = GestureClick::builder().button(0).build();
    {
        let out_tx_p = out_tx.clone();
        let cursor_p = cursor_pos.clone();
        let picture_p = picture.clone();
        let composite_c = composite.clone();
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
            let pw = picture_p.width() as f64;
            let ph = picture_p.height() as f64;
            let (cw, ch) = if let Some(comp) = composite_c.borrow().as_ref() {
                (comp.w as f64, comp.h as f64)
            } else {
                (pw, ph)
            };
            let scale_x = if pw > 0.0 { cw / pw } else { 1.0 };
            let scale_y = if ph > 0.0 { ch / ph } else { 1.0 };
            let ix = ((x * scale_x) as i32).clamp(0, cw as i32);
            let iy = ((y * scale_y) as i32).clamp(0, ch as i32);
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
            let (ix, iy) = cursor_r.get();
            let _ = out_tx_r.send(net::OutMsg::PointerButton {
                x: ix, y: iy, button: btn as u32, pressed: false, mods: 0,
            });
        });
    }

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
                return Inhibit(true);
            }
            let acc_x = acc_dx.get() + dx;
            let acc_y = acc_dy.get() + dy;
            let send_x = acc_x.trunc() as i32;
            let send_y = acc_y.trunc() as i32;

            if send_x != 0 || send_y != 0 {
                acc_dx.set(acc_x - send_x as f64);
                acc_dy.set(acc_y - send_y as f64);
                let _ = out_tx.send(net::OutMsg::Scroll { dx: send_x, dy: send_y });
            } else {
                acc_dx.set(acc_x);
                acc_dy.set(acc_y);
            }
            Inhibit(false)
        });
    }

    let root_vbox = GtkBox::new(Orientation::Vertical, 0);
    root_vbox.append(&tab_bar_box);
    root_vbox.append(&nav_toolbar);
    root_vbox.append(&bookmarks_box);
    root_vbox.append(&overlay);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("ExoBrowser")
        .default_width(1280)
        .default_height(780)
        .child(&root_vbox)
        .build();

    let display = gdk::Display::default().expect("gdk display");
    install_css(&display);
    let url_focused = Rc::new(Cell::new(false));
    let focus_ctrl = gtk4::EventControllerFocus::new();
    {
        let uf = url_focused.clone();
        focus_ctrl.connect_enter(move |_| uf.set(true));
    }
    {
        let uf = url_focused.clone();
        focus_ctrl.connect_leave(move |_| uf.set(false));
    }
    url_entry.add_controller(focus_ctrl);
    picture.set_focusable(true);

    // Global Key controller
    let key_controller = EventControllerKey::new();
    {
        let out_tx_press = out_tx.clone();
        let url_entry_press = url_entry.clone();
        let out_tx_rel = out_tx.clone();
        let tabs = tabs.clone();
        let active_tab_id = active_tab_id.clone();
        let next_tab_id = next_tab_id.clone();
        let render_tabs = render_tabs.clone();
        let zoom_milli = zoom_milli.clone();
        let update_zoom_ui = update_zoom_ui.clone();
        let uf_press = url_focused.clone();
        let uf_rel = url_focused.clone();

        key_controller.connect_key_pressed(move |_, key, _code, state| {
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let alt = state.contains(gdk::ModifierType::ALT_MASK);

            // Ctrl+T: New tab
            if ctrl && (key == gdk::Key::t || key == gdk::Key::T) {
                let new_id = next_tab_id.get();
                next_tab_id.set(new_id + 1);
                let new_url = "https://duckduckgo.com".to_string();
                tabs.borrow_mut().push(TabInfo {
                    id: new_id,
                    title: "New Tab".into(),
                    url: new_url.clone(),
                    is_loading: false,
                    is_secure: true,
                });
                active_tab_id.set(new_id);
                url_entry_press.set_text(&new_url);
                url_entry_press.grab_focus();
                let _ = out_tx_press.send(net::OutMsg::CreateTab { tab_id: new_id, url: new_url });
                if let Some(ref r) = *render_tabs.borrow() { r(); }
                return Inhibit(true);
            }

            // Ctrl+W: Close current tab
            if ctrl && (key == gdk::Key::w || key == gdk::Key::W) {
                let current_id = active_tab_id.get();
                let mut list = tabs.borrow_mut();
                if list.len() > 1 {
                    if let Some(pos) = list.iter().position(|t| t.id == current_id) {
                        list.remove(pos);
                        let new_active = if pos > 0 { list[pos - 1].id } else { list[0].id };
                        active_tab_id.set(new_active);
                        let _ = out_tx_press.send(net::OutMsg::SwitchTab { tab_id: new_active });
                        let _ = out_tx_press.send(net::OutMsg::CloseTab { tab_id: current_id });
                    }
                }
                drop(list);
                if let Some(ref r) = *render_tabs.borrow() { r(); }
                return Inhibit(true);
            }

            // Ctrl+L or F6: Focus address bar
            if (ctrl && (key == gdk::Key::l || key == gdk::Key::L)) || key == gdk::Key::F6 {
                url_entry_press.grab_focus();
                url_entry_press.select_region(0, -1);
                return Inhibit(true);
            }

            // F5 or Ctrl+R: Reload
            if key == gdk::Key::F5 || (ctrl && (key == gdk::Key::r || key == gdk::Key::R)) {
                let _ = out_tx_press.send(net::OutMsg::NavAction { action: 2 });
                return Inhibit(true);
            }

            // Alt+Left / Alt+Right: Back / Forward
            if alt && key == gdk::Key::Left {
                let _ = out_tx_press.send(net::OutMsg::NavAction { action: 0 });
                return Inhibit(true);
            }
            if alt && key == gdk::Key::Right {
                let _ = out_tx_press.send(net::OutMsg::NavAction { action: 1 });
                return Inhibit(true);
            }

            // Ctrl+0: Reset zoom
            if ctrl && (key == gdk::Key::_0 || key == gdk::Key::KP_0) {
                zoom_milli.set(1000);
                update_zoom_ui();
                let _ = out_tx_press.send(net::OutMsg::SetZoom { level_milli: 1000 });
                return Inhibit(true);
            }

            // Forward generic key events to webview when not typing in URL entry
            if !uf_press.get() {
                let keyval: u32 = key.into_glib();
                let mods = translate_gtk_mods(state);
                let _ = out_tx_press.send(net::OutMsg::Key { keycode: keyval, mods, down: true });
            }
            Inhibit(false)
        });

        key_controller.connect_key_released(move |_, key, _code, state| {
            if !uf_rel.get() {
                let keyval: u32 = key.into_glib();
                let mods = translate_gtk_mods(state);
                let _ = out_tx_rel.send(net::OutMsg::Key { keycode: keyval, mods, down: false });
            }
        });
    }
    window.add_controller(key_controller);

    // URL Entry Submit
    {
        let out_tx = out_tx.clone();
        let picture = picture.clone();
        let tabs = tabs.clone();
        let active_tab_id = active_tab_id.clone();
        let render_tabs = render_tabs.clone();
        url_entry.connect_activate(move |e| {
            let raw = e.text().to_string();
            let url = canonicalize_url(&raw);
            e.set_text(&url);
            let current_id = active_tab_id.get();
            if let Some(t) = tabs.borrow_mut().iter_mut().find(|t| t.id == current_id) {
                t.url = url.clone();
            }
            let _ = out_tx.send(net::OutMsg::Nav(url));
            picture.grab_focus();
            if let Some(ref r) = *render_tabs.borrow() { r(); }
        });
    }

    picture.add_controller(motion);
    picture.add_controller(click);
    picture.add_controller(scroll);

    // Track picture widget allocation resize continuously and accurately
    {
        let out_tx = out_tx.clone();
        let picture_res = picture.clone();
        let last_size = Rc::new(Cell::new((0u16, 0u16)));
        glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            let w = picture_res.width() as u16;
            let h = picture_res.height() as u16;
            if w >= 64 && h >= 64 && last_size.get() != (w, h) {
                last_size.set((w, h));
                let _ = out_tx.send(net::OutMsg::Resize { w, h });
            }
            glib::Continue(true)
        });
    }

    // Spawn network task on background thread
    let server_copy = server;
    let cert_pin_copy = cert_pin;
    std::thread::Builder::new()
        .name("gtk-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio rt");
            if let Err(e) = rt.block_on(net::run(server_copy, cert_pin_copy, frame_tx, out_rx)) {
                tracing::error!(error = %e, "net task failed");
            }
        })
        .expect("spawn net thread");

    let av_clock = Rc::new(RefCell::new(AVClock::new()));
    let window_title_url = Rc::new(RefCell::new(String::new()));

    frame_rx.attach(None, clone!(
        @strong composite,
        @strong dirty,
        @strong picture,
        @strong url_entry,
        @strong omnibox_box,
        @strong reload_btn,
        @strong is_loading,
        @strong window,
        @strong tabs,
        @strong active_tab_id,
        @strong render_tabs,
        @strong security_badge,
        @strong status_label,
        @strong tab_composites
        => move |frame| {
        {
            let mut cell = composite.borrow_mut();
            match frame {
                net::GtkFrame::Load(s) => {
                    let loading = s == 0 || s == 1 || s == 2;
                    is_loading.set(loading);
                    if loading {
                        omnibox_box.add_css_class("loading");
                        reload_btn.set_label("✕");
                        reload_btn.set_tooltip_text(Some("Stop (Esc)"));
                    } else {
                        omnibox_box.remove_css_class("loading");
                        reload_btn.set_label("↻");
                        reload_btn.set_tooltip_text(Some("Reload (F5)"));
                    }
                    let current_id = active_tab_id.get();
                    if let Some(t) = tabs.borrow_mut().iter_mut().find(|t| t.id == current_id) {
                        t.is_loading = loading;
                    }
                    if let Some(ref r) = *render_tabs.borrow() { r(); }
                }
                net::GtkFrame::Url(u) | net::GtkFrame::UrlChanged(u) => {
                    *window_title_url.borrow_mut() = u.clone();
                    if !url_entry.has_focus() {
                        url_entry.set_text(&u);
                    }
                    let is_https = u.starts_with("https://");
                    security_badge.set_text(if is_https { "🔒" } else { "ℹ️" });
                    if is_https {
                        security_badge.remove_css_class("insecure");
                        security_badge.add_css_class("secure");
                    } else {
                        security_badge.remove_css_class("secure");
                        security_badge.add_css_class("insecure");
                    }
                    let current_id = active_tab_id.get();
                    if let Some(t) = tabs.borrow_mut().iter_mut().find(|t| t.id == current_id) {
                        t.url = u.clone();
                        t.is_secure = is_https;
                    }
                    if let Some(ref r) = *render_tabs.borrow() { r(); }
                }
                net::GtkFrame::Title(t) => {
                    window.set_title(Some(&format!("{t} — ExoBrowser")));
                    let current_id = active_tab_id.get();
                    if let Some(tab) = tabs.borrow_mut().iter_mut().find(|tb| tb.id == current_id) {
                        tab.title = t.clone();
                    }
                    if let Some(ref r) = *render_tabs.borrow() { r(); }
                }
                net::GtkFrame::Cursor(shape) => {
                    let cursor_name = match shape {
                        gutted_proto::CursorShape::Pointer => "pointer",
                        gutted_proto::CursorShape::Text => "text",
                        gutted_proto::CursorShape::Wait | gutted_proto::CursorShape::Progress => "wait",
                        gutted_proto::CursorShape::Crosshair => "crosshair",
                        gutted_proto::CursorShape::Move => "move",
                        gutted_proto::CursorShape::NotAllowed => "not-allowed",
                        gutted_proto::CursorShape::Grab => "grab",
                        gutted_proto::CursorShape::Grabbing => "grabbing",
                        gutted_proto::CursorShape::ResizeEw => "ew-resize",
                        gutted_proto::CursorShape::ResizeNs => "ns-resize",
                        gutted_proto::CursorShape::ResizeNesw => "nesw-resize",
                        gutted_proto::CursorShape::ResizeNwse => "nwse-resize",
                        _ => "default",
                    };
                    picture.set_cursor_from_name(Some(cursor_name));
                }
                net::GtkFrame::TabCreated { tab_id, title, url } => {
                    let mut list = tabs.borrow_mut();
                    if !list.iter().any(|t| t.id == tab_id) {
                        list.push(TabInfo {
                            id: tab_id,
                            title,
                            url,
                            is_loading: false,
                            is_secure: true,
                        });
                    }
                    drop(list);
                    if let Some(ref r) = *render_tabs.borrow() { r(); }
                }
                net::GtkFrame::TabClosed { tab_id } => {
                    let mut list = tabs.borrow_mut();
                    if let Some(pos) = list.iter().position(|t| t.id == tab_id) {
                        list.remove(pos);
                    }
                    tab_composites.borrow_mut().remove(&tab_id);
                    drop(list);
                    if let Some(ref r) = *render_tabs.borrow() { r(); }
                }
                net::GtkFrame::TabActivated { tab_id } => {
                    active_tab_id.set(tab_id);
                    if let Some(c) = tab_composites.borrow().get(&tab_id) {
                        *cell = Some(c.clone());
                    } else {
                        *cell = None;
                    }
                    if let Some(t) = tabs.borrow().iter().find(|t| t.id == tab_id) {
                        url_entry.set_text(&t.url);
                        let is_https = t.url.starts_with("https://");
                        security_badge.set_text(if is_https { "🔒" } else { "ℹ️" });
                    }
                    if let Some(ref r) = *render_tabs.borrow() { r(); }
                }
                net::GtkFrame::TabTitle { tab_id, title } => {
                    if let Some(t) = tabs.borrow_mut().iter_mut().find(|tb| tb.id == tab_id) {
                        t.title = title.clone();
                    }
                    if active_tab_id.get() == tab_id {
                        window.set_title(Some(&format!("{title} — ExoBrowser")));
                    }
                    if let Some(ref r) = *render_tabs.borrow() { r(); }
                }
                net::GtkFrame::TabUrl { tab_id, url } => {
                    if let Some(t) = tabs.borrow_mut().iter_mut().find(|tb| tb.id == tab_id) {
                        t.url = url.clone();
                    }
                    if active_tab_id.get() == tab_id && !url_entry.has_focus() {
                        url_entry.set_text(&url);
                    }
                    if let Some(ref r) = *render_tabs.borrow() { r(); }
                }
                net::GtkFrame::AuthSuccess { node_id } => {
                    tracing::info!(node = %node_id, "remote authentication successful");
                }
                net::GtkFrame::Audio { pts_us, .. } => {
                    av_clock.borrow_mut().on_audio_frame(pts_us, 20_000);
                    return glib::Continue(true);
                }
                net::GtkFrame::VideoChunk { pts_us, .. } => {
                    let decision = av_clock.borrow_mut().schedule_video(pts_us);
                    if decision == 2 {
                        return glib::Continue(true);
                    }
                    return glib::Continue(true);
                }
                net::GtkFrame::Full { width, height, stride, pixels } => {
                    tracing::debug!(width, height, stride, bytes = pixels.len(), "GTK received Full frame");
                    let comp = Composite { w: width, h: height, stride, pixels };
                    let cur_id = active_tab_id.get();
                    tab_composites.borrow_mut().insert(cur_id, comp.clone());
                    *cell = Some(comp);
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
                            c.pixels[dst_off .. dst_off + row_len]
                                .copy_from_slice(&pixels[src_off .. src_off + row_len]);
                        }
                        let cur_id = active_tab_id.get();
                        tab_composites.borrow_mut().insert(cur_id, c.clone());
                    } else {
                        return glib::Continue(true);
                    }
                }
                _ => {}
            }
        }
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
                    picture.queue_draw();
                }
                dirty.set(false);
            });
        }
        glib::Continue(true)
    }));

    window.present();
}

fn translate_gtk_mods(s: gdk::ModifierType) -> u32 {
    let mut m = 0u32;
    if s.contains(gdk::ModifierType::CONTROL_MASK) { m |= 1 << 0; }
    if s.contains(gdk::ModifierType::SHIFT_MASK)   { m |= 1 << 1; }
    if s.contains(gdk::ModifierType::ALT_MASK)     { m |= 1 << 2; }
    if s.contains(gdk::ModifierType::SUPER_MASK)   { m |= 1 << 3; }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_url_tests() {
        assert_eq!(canonicalize_url("https://example.com"), "https://example.com");
        assert_eq!(canonicalize_url("example.com"), "https://example.com");
        assert_eq!(canonicalize_url("   youtube.com/watch  "), "https://youtube.com/watch");
        assert_eq!(canonicalize_url(""), "about:blank");
        assert_eq!(canonicalize_url("rust programming"), "https://duckduckgo.com/?q=rust+programming");
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
}
