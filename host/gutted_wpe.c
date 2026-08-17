// gutted_wpe: WPE + WebKit glue as a small C library. Multi-instance.
//
// Each gutted_wpe_run() creates one WebKitWebView (which spawns its own
// WPEWebProcess) and runs a GLib main loop on the calling thread. The
// caller receives an opaque `gutted_wpe_handle` via the on_ready
// callback and scopes stop/load/resize/inject_* to that handle.
//
// Process-scoped one-shots (wpe_loader_init + wpe_fdo_initialize_shm)
// are guarded with g_once_init_enter so N instances can coexist.
// Each instance owns a *new* GMainContext pushed as thread-default so
// GLib idle sources land on the right loop.

#include "gutted_wpe.h"

#include <wpe/wpe.h>
#include <wpe/fdo.h>
#include <wpe/unstable/fdo-shm.h>
#include <wpe/webkit.h>
#include <wayland-server.h>
#include <glib.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    struct wpe_view_backend_exportable_fdo *exportable;
    WebKitWebView                          *view;
    GMainLoop                              *loop;
    GMainContext                           *ctx;
    gutted_wpe_callbacks                    cb;
    void                                   *userdata;
    unsigned                                frame_count;
    uint32_t                                cur_w;
    uint32_t                                cur_h;
} instance_t;

static void on_shm_buffer(void *data, struct wpe_fdo_shm_exported_buffer *exported)
{
    instance_t *inst = data;
    struct wl_shm_buffer *shm = wpe_fdo_shm_exported_buffer_get_shm_buffer(exported);

    int32_t  w      = wl_shm_buffer_get_width(shm);
    int32_t  h      = wl_shm_buffer_get_height(shm);
    int32_t  stride = wl_shm_buffer_get_stride(shm);
    uint32_t fmt    = wl_shm_buffer_get_format(shm);

    wl_shm_buffer_begin_access(shm);
    const uint8_t *pixels = wl_shm_buffer_get_data(shm);

    inst->frame_count++;
    if (inst->cb.on_frame) {
        inst->cb.on_frame(inst->userdata, pixels, w, h, stride, fmt);
    }

    wl_shm_buffer_end_access(shm);

    wpe_view_backend_exportable_fdo_dispatch_frame_complete(inst->exportable);
    wpe_view_backend_exportable_fdo_dispatch_release_shm_exported_buffer(inst->exportable, exported);
}

static void on_mouse_target_changed(WebKitWebView *v, WebKitHitTestResult *r,
                                    guint modifiers, gpointer user_data)
{
    (void)v; (void)modifiers;
    instance_t *inst = user_data;
    if (!inst->cb.on_cursor) return;
    int32_t shape = 0;
    if (webkit_hit_test_result_context_is_link(r))          shape = 1;
    else if (webkit_hit_test_result_context_is_editable(r)) shape = 2;
    inst->cb.on_cursor(inst->userdata, shape);
}

static void on_notify_title(WebKitWebView *v, GParamSpec *pspec, gpointer user_data)
{
    (void)pspec;
    instance_t *inst = user_data;
    if (!inst->cb.on_title) return;
    const gchar *title = webkit_web_view_get_title(v);
    inst->cb.on_title(inst->userdata, title);
}

static void on_notify_uri(WebKitWebView *v, GParamSpec *pspec, gpointer user_data)
{
    (void)pspec;
    instance_t *inst = user_data;
    if (!inst->cb.on_url) return;
    const gchar *uri = webkit_web_view_get_uri(v);
    inst->cb.on_url(inst->userdata, uri);
}

static void on_load_changed(WebKitWebView *v, WebKitLoadEvent e, gpointer user_data)
{
    (void)v;
    instance_t *inst = user_data;
    if (e == WEBKIT_LOAD_FINISHED) {
        const char *nudge_ms = g_getenv("GBROWSER_NUDGE_MS");
        if (nudge_ms && nudge_ms[0]) {
            int ms = atoi(nudge_ms);
            if (ms < 1) ms = 16;
            char *js = g_strdup_printf(
                "(function(){"
                "  if (window.__gt_nudge) clearInterval(window.__gt_nudge);"
                "  var el = document.getElementById('__gt_nudge_el');"
                "  if (!el) {"
                "    el = document.createElement('div');"
                "    el.id = '__gt_nudge_el';"
                "    el.style.cssText = 'position:fixed;bottom:0;right:0;width:6px;height:6px;pointer-events:none;z-index:2147483647';"
                "    document.body && document.body.appendChild(el);"
                "  }"
                "  window.__gt_nudge = setInterval(function(){"
                "    var t = Date.now();"
                "    el.style.background = 'hsl(' + (t %% 360) + ',80%%,50%%)';"
                "  }, %d);"
                "})();",
                ms);
            webkit_web_view_run_javascript(inst->view, js, NULL, NULL, NULL);
            g_free(js);
        }
    }
    if (!inst->cb.on_load) return;
    int32_t s = -1;
    switch (e) {
        case WEBKIT_LOAD_STARTED:    s = 0; break;
        case WEBKIT_LOAD_REDIRECTED: s = 1; break;
        case WEBKIT_LOAD_COMMITTED:  s = 2; break;
        case WEBKIT_LOAD_FINISHED:   s = 3; break;
    }
    inst->cb.on_load(inst->userdata, s);
}

/// Process-scoped one-shots: load libWPEBackend-fdo + init the SHM
/// transport. Safe to call from multiple threads; runs exactly once.
static gsize g_wpe_init_once = 0;
static gboolean g_wpe_init_ok = FALSE;
static gboolean wpe_process_init(void) {
    if (g_once_init_enter(&g_wpe_init_once)) {
        wpe_loader_init("libWPEBackend-fdo-1.0.so");
        g_wpe_init_ok = wpe_fdo_initialize_shm() ? TRUE : FALSE;
        g_once_init_leave(&g_wpe_init_once, 1);
    }
    return g_wpe_init_ok;
}

int gutted_wpe_run(
    const char *initial_url,
    int32_t viewport_w,
    int32_t viewport_h,
    const gutted_wpe_callbacks *cb,
    void *userdata)
{
    if (!cb || !cb->on_frame) return -1;
    if (!wpe_process_init())  return -2;

    instance_t *inst = g_new0(instance_t, 1);
    inst->cb        = *cb;
    inst->userdata  = userdata;
    inst->cur_w     = viewport_w;
    inst->cur_h     = viewport_h;
    inst->ctx       = g_main_context_default();
    inst->loop      = g_main_loop_new(inst->ctx, FALSE);

    static const struct wpe_view_backend_exportable_fdo_client client_vtable = {
        .export_buffer_resource = NULL,
        .export_dmabuf_resource = NULL,
        .export_shm_buffer      = on_shm_buffer,
    };
    inst->exportable = wpe_view_backend_exportable_fdo_create(
        &client_vtable, inst, viewport_w, viewport_h);
    if (!inst->exportable) {
        g_main_loop_unref(inst->loop);
        g_free(inst);
        return -3;
    }

    WebKitWebContext *web_ctx = webkit_web_context_get_default();
    webkit_web_context_set_cache_model(web_ctx, WEBKIT_CACHE_MODEL_WEB_BROWSER);
    WebKitCookieManager *cm = webkit_web_context_get_cookie_manager(web_ctx);
    webkit_cookie_manager_set_accept_policy(cm, WEBKIT_COOKIE_POLICY_ACCEPT_ALWAYS);

    WebKitSettings *settings = webkit_settings_new();
    webkit_settings_set_enable_write_console_messages_to_stdout(settings, FALSE);
    webkit_settings_set_enable_javascript(settings, TRUE);
    webkit_settings_set_enable_html5_local_storage(settings, TRUE);
    webkit_settings_set_enable_html5_database(settings, TRUE);
    webkit_settings_set_enable_webgl(settings, TRUE);
    webkit_settings_set_enable_smooth_scrolling(settings, TRUE);
    webkit_settings_set_enable_developer_extras(settings, TRUE);
    webkit_settings_set_enable_resizable_text_areas(settings, TRUE);
    webkit_settings_set_enable_page_cache(settings, TRUE);
    webkit_settings_set_enable_site_specific_quirks(settings, TRUE);
    webkit_settings_set_enable_media(settings, TRUE);
    webkit_settings_set_enable_media_stream(settings, TRUE);
    webkit_settings_set_enable_mediasource(settings, TRUE);
    webkit_settings_set_enable_encrypted_media(settings, TRUE);
    webkit_settings_set_enable_webaudio(settings, TRUE);
    webkit_settings_set_media_playback_allows_inline(settings, TRUE);
    webkit_settings_set_media_playback_requires_user_gesture(settings, FALSE);
    webkit_settings_set_user_agent(settings,
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36");

    WebKitWebViewBackend *vb = webkit_web_view_backend_new(
        wpe_view_backend_exportable_fdo_get_view_backend(inst->exportable),
        NULL, NULL);
    inst->view = WEBKIT_WEB_VIEW(g_object_new(
        WEBKIT_TYPE_WEB_VIEW,
        "backend", vb,
        "settings", settings,
        NULL));
    g_object_unref(settings);
    WebKitColor bg_color = { 1.0, 1.0, 1.0, 1.0 };
    webkit_web_view_set_background_color(inst->view, &bg_color);

    g_signal_connect(inst->view, "load-changed", G_CALLBACK(on_load_changed), inst);
    g_signal_connect(inst->view, "mouse-target-changed", G_CALLBACK(on_mouse_target_changed), inst);
    g_signal_connect(inst->view, "notify::title", G_CALLBACK(on_notify_title), inst);
    g_signal_connect(inst->view, "notify::uri",   G_CALLBACK(on_notify_uri),   inst);

    if (initial_url) webkit_web_view_load_uri(inst->view, initial_url);

    // Hand the handle to the caller before we block on the main loop.
    if (inst->cb.on_ready) inst->cb.on_ready(inst->userdata, (gutted_wpe_handle)inst);

    g_main_loop_run(inst->loop);

    wpe_view_backend_exportable_fdo_destroy(inst->exportable);
    g_object_unref(inst->view);
    g_main_loop_unref(inst->loop);
    g_free(inst);
    return 0;
}

// ─── Thread-safe hooks marshalled onto the instance's GLib context ──────

typedef struct { instance_t *inst; } idle_stop_t;
static gboolean idle_stop(gpointer p) {
    idle_stop_t *s = p;
    if (s->inst && s->inst->loop) g_main_loop_quit(s->inst->loop);
    g_free(s);
    return G_SOURCE_REMOVE;
}

void gutted_wpe_stop(gutted_wpe_handle h)
{
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_stop_t *s = g_new0(idle_stop_t, 1);
    s->inst = inst;
    g_main_context_invoke_full(inst->ctx, G_PRIORITY_HIGH, idle_stop, s, NULL);
}

typedef struct { instance_t *inst; char *url; } idle_load_t;
static gboolean idle_load(gpointer p) {
    idle_load_t *l = p;
    if (l->inst && l->inst->view && l->url)
        webkit_web_view_load_uri(l->inst->view, l->url);
    g_free(l->url);
    g_free(l);
    return G_SOURCE_REMOVE;
}

void gutted_wpe_load_uri(gutted_wpe_handle h, const char *url)
{
    instance_t *inst = (instance_t *)h;
    if (!inst || !url) return;
    idle_load_t *l = g_new0(idle_load_t, 1);
    l->inst = inst;
    l->url  = g_strdup(url);
    g_main_context_invoke(inst->ctx, idle_load, l);
}

typedef struct { instance_t *inst; uint32_t w, h; } idle_resize_t;
static gboolean idle_resize(gpointer p) {
    idle_resize_t *r = p;
    if (r->inst && r->inst->exportable) {
        if (r->inst->cur_w != r->w || r->inst->cur_h != r->h) {
            r->inst->cur_w = r->w;
            r->inst->cur_h = r->h;
            struct wpe_view_backend *vb =
                wpe_view_backend_exportable_fdo_get_view_backend(r->inst->exportable);
            wpe_view_backend_dispatch_set_size(vb, r->w, r->h);
        }
    }
    g_free(r);
    return G_SOURCE_REMOVE;
}

void gutted_wpe_resize(gutted_wpe_handle h, uint32_t w, uint32_t h_px)
{
    instance_t *inst = (instance_t *)h;
    if (!inst || w == 0 || h_px == 0) return;
    idle_resize_t *r = g_new0(idle_resize_t, 1);
    r->inst = inst; r->w = w; r->h = h_px;
    g_main_context_invoke(inst->ctx, idle_resize, r);
}

// ─── Input dispatch, marshalled onto the instance's GLib context ─────────
// (wpe/input.h and wpe/view-backend.h are already pulled by wpe/wpe.h above.)

typedef struct {
    instance_t *inst;
    int32_t     x, y;
    uint32_t    button;
    uint32_t    modifiers;
    uint32_t    keysym;
    double      dx, dy;
    bool        pressed;
    int         kind; // 0=motion, 1=button, 2=key, 3=axis
} idle_input_t;

static gboolean idle_input(gpointer p) {
    idle_input_t *e = p;
    if (!e->inst || !e->inst->exportable) { g_free(e); return G_SOURCE_REMOVE; }
    struct wpe_view_backend *vb =
        wpe_view_backend_exportable_fdo_get_view_backend(e->inst->exportable);
    uint32_t now_ms = (uint32_t)(g_get_monotonic_time() / 1000);
    switch (e->kind) {
        case 0: {
            struct wpe_input_pointer_event ev = {
                .type = wpe_input_pointer_event_type_motion,
                .time = now_ms, .x = e->x, .y = e->y,
                .button = 0, .state = 0, .modifiers = e->modifiers,
            };
            wpe_view_backend_dispatch_pointer_event(vb, &ev);
            break;
        }
        case 1: {
            struct wpe_input_pointer_event ev = {
                .type = wpe_input_pointer_event_type_button,
                .time = now_ms, .x = e->x, .y = e->y,
                .button = e->button, .state = e->pressed ? 1 : 0,
                .modifiers = e->modifiers,
            };
            wpe_view_backend_dispatch_pointer_event(vb, &ev);
            break;
        }
        case 2: {
            struct wpe_input_keyboard_event ev = {
                .time = now_ms, .key_code = e->keysym,
                .hardware_key_code = 0, .pressed = e->pressed,
                .modifiers = e->modifiers,
            };
            wpe_view_backend_dispatch_keyboard_event(vb, &ev);
            break;
        }
        case 3: {
            struct wpe_input_axis_2d_event ev2 = {
                .base = {
                    .type = (enum wpe_input_axis_event_type)
                        (wpe_input_axis_event_type_motion_smooth
                         | wpe_input_axis_event_type_mask_2d),
                    .time = now_ms, .x = e->x, .y = e->y,
                    .axis = 0, .value = 0,
                    .modifiers = e->modifiers,
                },
                .x_axis = e->dx, .y_axis = e->dy,
            };
            wpe_view_backend_dispatch_axis_event(vb, &ev2.base);
            break;
        }
    }
    g_free(e);
    return G_SOURCE_REMOVE;
}

static void enqueue_input(instance_t *inst, idle_input_t *e) {
    if (!inst) { g_free(e); return; }
    e->inst = inst;
    g_main_context_invoke(inst->ctx, idle_input, e);
}

void gutted_wpe_inject_pointer_motion(gutted_wpe_handle h,
                                      int32_t x, int32_t y, uint32_t modifiers) {
    idle_input_t *e = g_new0(idle_input_t, 1);
    e->kind = 0; e->x = x; e->y = y; e->modifiers = modifiers;
    enqueue_input((instance_t *)h, e);
}

void gutted_wpe_inject_pointer_button(gutted_wpe_handle h,
                                      int32_t x, int32_t y,
                                      uint32_t button, bool pressed,
                                      uint32_t modifiers) {
    idle_input_t *e = g_new0(idle_input_t, 1);
    e->kind = 1; e->x = x; e->y = y; e->button = button;
    e->pressed = pressed; e->modifiers = modifiers;
    enqueue_input((instance_t *)h, e);
}

void gutted_wpe_inject_key(gutted_wpe_handle h,
                           uint32_t keysym, uint32_t modifiers, bool pressed) {
    idle_input_t *e = g_new0(idle_input_t, 1);
    e->kind = 2; e->keysym = keysym; e->modifiers = modifiers; e->pressed = pressed;
    enqueue_input((instance_t *)h, e);
}

void gutted_wpe_inject_axis(gutted_wpe_handle h,
                            int32_t x, int32_t y,
                            double dx, double dy,
                            uint32_t modifiers) {
    idle_input_t *e = g_new0(idle_input_t, 1);
    e->kind = 3; e->x = x; e->y = y;
    e->dx = dx; e->dy = dy; e->modifiers = modifiers;
    enqueue_input((instance_t *)h, e);
}

typedef struct { instance_t *inst; double level; } idle_zoom_t;

static gboolean idle_zoom(gpointer p) {
    idle_zoom_t *z = p;
    if (z->inst && z->inst->view) {
        webkit_web_view_set_zoom_level(z->inst->view, z->level);
    }
    g_free(z);
    return G_SOURCE_REMOVE;
}

void gutted_wpe_set_zoom(gutted_wpe_handle h, double level) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    if (level < 0.25) level = 0.25;
    if (level > 5.0)  level = 5.0;
    idle_zoom_t *z = g_new0(idle_zoom_t, 1);
    z->inst = inst; z->level = level;
    g_main_context_invoke(inst->ctx, idle_zoom, z);
}

typedef struct { instance_t *inst; int action; } idle_nav_t;

static gboolean idle_nav(gpointer p) {
    idle_nav_t *n = p;
    if (n->inst && n->inst->view) {
        switch (n->action) {
            case 0: webkit_web_view_go_back(n->inst->view);      break;
            case 1: webkit_web_view_go_forward(n->inst->view);   break;
            case 2: webkit_web_view_reload(n->inst->view);       break;
            case 3: webkit_web_view_stop_loading(n->inst->view); break;
        }
    }
    g_free(n);
    return G_SOURCE_REMOVE;
}

static void enqueue_nav(gutted_wpe_handle h, int action) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_nav_t *n = g_new0(idle_nav_t, 1);
    n->inst = inst; n->action = action;
    g_main_context_invoke(inst->ctx, idle_nav, n);
}

void gutted_wpe_go_back(gutted_wpe_handle h)    { enqueue_nav(h, 0); }
void gutted_wpe_go_forward(gutted_wpe_handle h) { enqueue_nav(h, 1); }
void gutted_wpe_reload(gutted_wpe_handle h)     { enqueue_nav(h, 2); }
void gutted_wpe_stop_loading(gutted_wpe_handle h){ enqueue_nav(h, 3); }
