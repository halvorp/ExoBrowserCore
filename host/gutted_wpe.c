// gutted_wpe.c: WPE WebKit backend with native multi-tab support on a single GLib event loop.

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <glib.h>
#include <gio/gio.h>
#include <wayland-server.h>
#include <wpe/wpe.h>
#include <wpe/fdo.h>
#include <wpe/unstable/fdo-shm.h>
#include <wpe/webkit.h>

#include "gutted_wpe.h"

struct instance_t;

typedef struct {
    uint32_t tab_id;
    struct instance_t *inst;
    struct wpe_view_backend_exportable_fdo *exportable;
    WebKitWebView *view;
    int32_t cur_w, cur_h;
} tab_t;

typedef struct instance_t {
    gutted_wpe_callbacks cb;
    void *userdata;
    GMainContext *ctx;
    GMainLoop *loop;
    GHashTable *tabs; // uint32_t tab_id -> tab_t*
    int32_t def_w, def_h;
} instance_t;

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

static const char *k_media_lifecycle_script =
    "(function(){\n"
    "  function stopOtherMedia(activeEl) {\n"
    "    try {\n"
    "      var media = document.querySelectorAll('video, audio');\n"
    "      for (var i = 0; i < media.length; i++) {\n"
    "        var m = media[i];\n"
    "        if (m !== activeEl && !m.paused) {\n"
    "          m.pause();\n"
    "          m.muted = true;\n"
    "        }\n"
    "      }\n"
    "    } catch(e) {}\n"
    "  }\n"
    "\n"
    "  var origPlay = HTMLMediaElement.prototype.play;\n"
    "  HTMLMediaElement.prototype.play = function() {\n"
    "    stopOtherMedia(this);\n"
    "    return origPlay.apply(this, arguments);\n"
    "  };\n"
    "\n"
    "  document.addEventListener('play', function(e) {\n"
    "    if (e.target && (e.target.tagName === 'VIDEO' || e.target.tagName === 'AUDIO')) {\n"
    "      stopOtherMedia(e.target);\n"
    "    }\n"
    "  }, { capture: true, passive: true });\n"
    "\n"
    "  document.addEventListener('playing', function(e) {\n"
    "    if (e.target && (e.target.tagName === 'VIDEO' || e.target.tagName === 'AUDIO')) {\n"
    "      stopOtherMedia(e.target);\n"
    "    }\n"
    "  }, { capture: true, passive: true });\n"
    "\n"
    "  window.addEventListener('popstate', function() { stopOtherMedia(null); }, { passive: true });\n"
    "  window.addEventListener('hashchange', function() { stopOtherMedia(null); }, { passive: true });\n"
    "  document.addEventListener('yt-navigate-start', function() { stopOtherMedia(null); }, { capture: true, passive: true });\n"
    "\n"
    "  var resumeAC = function() {\n"
    "    if (window.AudioContext || window.webkitAudioContext) {\n"
    "      var ac = window.__gt_ac || new (window.AudioContext || window.webkitAudioContext)();\n"
    "      window.__gt_ac = ac;\n"
    "      if (ac.state === 'suspended') ac.resume();\n"
    "    }\n"
    "  };\n"
    "  document.addEventListener('click', resumeAC, { capture: true, passive: true });\n"
    "  document.addEventListener('keydown', resumeAC, { capture: true, passive: true });\n"
    "})();\n";

static void on_shm_buffer(void *data, struct wpe_fdo_shm_exported_buffer *buf) {
    tab_t *tab = (tab_t *)data;
    if (!tab || !tab->inst) return;
    struct wl_shm_buffer *sb = wpe_fdo_shm_exported_buffer_get_shm_buffer(buf);
    int width = wl_shm_buffer_get_width(sb);
    int height = wl_shm_buffer_get_height(sb);
    int stride = wl_shm_buffer_get_stride(sb);
    uint32_t format = wl_shm_buffer_get_format(sb);

    wl_shm_buffer_begin_access(sb);
    const uint8_t *pixels = (const uint8_t *)wl_shm_buffer_get_data(sb);

    if (tab->inst->cb.on_frame && pixels && width > 0 && height > 0) {
        tab->inst->cb.on_frame(tab->inst->userdata, tab->tab_id, pixels, width, height, stride, format);
    }
    wl_shm_buffer_end_access(sb);

    wpe_view_backend_exportable_fdo_dispatch_frame_complete(tab->exportable);
    wpe_view_backend_exportable_fdo_dispatch_release_shm_exported_buffer(tab->exportable, buf);
}

static gboolean on_decide_policy(WebKitWebView *v, WebKitPolicyDecision *decision,
                                WebKitPolicyDecisionType type, gpointer user_data)
{
    (void)v; (void)type; (void)user_data;
    WebKitWebsitePolicies *policies = webkit_website_policies_new_with_policies(
        "autoplay", WEBKIT_AUTOPLAY_ALLOW,
        NULL);
    webkit_policy_decision_use_with_policies(decision, policies);
    g_object_unref(policies);
    return TRUE;
}

static void on_load_changed(WebKitWebView *v, WebKitLoadEvent e, gpointer user_data) {
    tab_t *tab = (tab_t *)user_data;
    if (!tab || !tab->inst) return;
    int32_t s = -1;
    switch (e) {
        case WEBKIT_LOAD_STARTED:    s = 0; break;
        case WEBKIT_LOAD_REDIRECTED: s = 1; break;
        case WEBKIT_LOAD_COMMITTED:  s = 2; break;
        case WEBKIT_LOAD_FINISHED:   s = 3; break;
    }
    if (s >= 0 && tab->inst->cb.on_load) {
        tab->inst->cb.on_load(tab->inst->userdata, tab->tab_id, s);
    }

    if (e == WEBKIT_LOAD_COMMITTED || e == WEBKIT_LOAD_FINISHED) {
        const char *uri = webkit_web_view_get_uri(v);
        if (uri && tab->inst->cb.on_url) {
            tab->inst->cb.on_url(tab->inst->userdata, tab->tab_id, uri);
        }
        const char *title = webkit_web_view_get_title(v);
        if (title && tab->inst->cb.on_title) {
            tab->inst->cb.on_title(tab->inst->userdata, tab->tab_id, title);
        }
        const char *audio_unlock_js =
            "(function(){\n"
            "  try {\n"
            "    var resumeAudio = function() {\n"
            "      var media = document.querySelectorAll('video, audio');\n"
            "      for (var i = 0; i < media.length; i++) {\n"
            "        if (media[i].paused && media[i].autoplay) media[i].play().catch(function(){});\n"
            "      }\n"
            "    };\n"
            "    document.addEventListener('click', resumeAudio, { capture: true, passive: true });\n"
            "    document.addEventListener('keydown', resumeAudio, { capture: true, passive: true });\n"
            "  } catch(e) {}\n"
            "})();";
        webkit_web_view_run_javascript(v, audio_unlock_js, NULL, NULL, NULL);
    }
}

static gboolean on_load_failed(WebKitWebView *v, WebKitLoadEvent e,
                              const gchar *failing_uri, GError *error,
                              gpointer user_data)
{
    (void)e; (void)user_data;
    if (error && error->domain == WEBKIT_NETWORK_ERROR &&
        error->code == WEBKIT_NETWORK_ERROR_CANCELLED) {
        return TRUE;
    }
    gchar *html = g_strdup_printf(
        "<!DOCTYPE html><html><head><meta charset='utf-8'>"
        "<title>Load Failed</title>"
        "<style>"
        "body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; "
        "       background: #18181b; color: #f4f4f5; display: flex; align-items: center; justify-content: center; "
        "       height: 100vh; margin: 0; padding: 24px; box-sizing: border-box; }"
        ".card { background: #27272a; border: 1px solid #3f3f46; border-radius: 12px; padding: 32px; max-width: 560px; box-shadow: 0 8px 30px rgba(0,0,0,0.5); }"
        "h1 { margin-top: 0; font-size: 20px; color: #f87171; display: flex; align-items: center; gap: 8px; }"
        ".uri { font-family: monospace; background: #18181b; padding: 8px 12px; border-radius: 6px; word-break: break-all; color: #a1a1aa; font-size: 13px; margin: 16px 0; border: 1px solid #3f3f46; }"
        ".msg { font-size: 14px; line-height: 1.6; color: #d4d4d8; margin-bottom: 24px; }"
        ".btn { background: #3b82f6; color: white; border: none; padding: 10px 20px; border-radius: 6px; font-weight: 500; cursor: pointer; text-decoration: none; display: inline-block; }"
        "</style></head><body>"
        "<div class='card'>"
        "<h1><span>⚠️</span> Page Load Failed</h1>"
        "<div class='uri'>%s</div>"
        "<div class='msg'>%s</div>"
        "<a href='javascript:location.reload()' class='btn'>Try Again</a>"
        "</div></body></html>",
        failing_uri ? failing_uri : "Unknown URL",
        error && error->message ? error->message : "An error occurred while loading the page.");
    webkit_web_view_load_html(v, html, failing_uri);
    g_free(html);
    return TRUE;
}

static gboolean on_load_failed_with_tls_errors(WebKitWebView *v, const gchar *failing_uri,
                                               GTlsCertificate *cert, GTlsCertificateFlags errors,
                                               gpointer user_data)
{
    (void)cert; (void)user_data;
    gchar *html = g_strdup_printf(
        "<!DOCTYPE html><html><head><meta charset='utf-8'>"
        "<title>Security / TLS Error</title>"
        "<style>"
        "body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; "
        "       background: #18181b; color: #f4f4f5; display: flex; align-items: center; justify-content: center; "
        "       height: 100vh; margin: 0; padding: 24px; box-sizing: border-box; }"
        ".card { background: #27272a; border: 1px solid #dc2626; border-radius: 12px; padding: 32px; max-width: 560px; box-shadow: 0 8px 30px rgba(0,0,0,0.5); }"
        "h1 { margin-top: 0; font-size: 20px; color: #f87171; display: flex; align-items: center; gap: 8px; }"
        ".uri { font-family: monospace; background: #18181b; padding: 8px 12px; border-radius: 6px; word-break: break-all; color: #a1a1aa; font-size: 13px; margin: 16px 0; border: 1px solid #3f3f46; }"
        ".msg { font-size: 14px; line-height: 1.6; color: #d4d4d8; margin-bottom: 24px; }"
        ".btn { background: #ef4444; color: white; border: none; padding: 10px 20px; border-radius: 6px; font-weight: 500; cursor: pointer; text-decoration: none; display: inline-block; }"
        "</style></head><body>"
        "<div class='card'>"
        "<h1><span>🔒</span> Insecure Connection (TLS Error)</h1>"
        "<div class='uri'>%s</div>"
        "<div class='msg'>The certificate for this server is invalid, untrusted, or expired (TLS flags: 0x%x).</div>"
        "<a href='javascript:location.reload()' class='btn'>Reload</a>"
        "</div></body></html>",
        failing_uri ? failing_uri : "Unknown URL",
        (unsigned)errors);
    webkit_web_view_load_html(v, html, failing_uri);
    g_free(html);
    return TRUE;
}

static void on_web_process_terminated(WebKitWebView *v, WebKitWebProcessTerminationReason reason, gpointer user_data)
{
    (void)reason; (void)user_data;
    gchar *html = g_strdup(
        "<!DOCTYPE html><html><head><meta charset='utf-8'>"
        "<title>Web Process Terminated</title>"
        "<style>"
        "body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; "
        "       background: #18181b; color: #f4f4f5; display: flex; align-items: center; justify-content: center; "
        "       height: 100vh; margin: 0; padding: 24px; box-sizing: border-box; }"
        ".card { background: #27272a; border: 1px solid #eab308; border-radius: 12px; padding: 32px; max-width: 560px; box-shadow: 0 8px 30px rgba(0,0,0,0.5); }"
        "h1 { margin-top: 0; font-size: 20px; color: #facc15; display: flex; align-items: center; gap: 8px; }"
        ".msg { font-size: 14px; line-height: 1.6; color: #d4d4d8; margin: 16px 0 24px 0; }"
        ".btn { background: #eab308; color: black; border: none; padding: 10px 20px; border-radius: 6px; font-weight: 600; cursor: pointer; text-decoration: none; display: inline-block; }"
        "</style></head><body>"
        "<div class='card'>"
        "<h1><span>💥</span> Tab Process Terminated</h1>"
        "<div class='msg'>The webpage renderer encountered an unexpected error and terminated.</div>"
        "<a href='javascript:location.reload()' class='btn'>Reload Tab</a>"
        "</div></body></html>");
    webkit_web_view_load_html(v, html, NULL);
    g_free(html);
}

static gboolean on_script_dialog(WebKitWebView *v, WebKitScriptDialog *dialog, gpointer user_data)
{
    (void)v; (void)user_data;
    switch (webkit_script_dialog_get_dialog_type(dialog)) {
        case WEBKIT_SCRIPT_DIALOG_ALERT:
            return TRUE;
        case WEBKIT_SCRIPT_DIALOG_CONFIRM:
            webkit_script_dialog_confirm_set_confirmed(dialog, TRUE);
            return TRUE;
        case WEBKIT_SCRIPT_DIALOG_PROMPT:
            webkit_script_dialog_prompt_set_text(dialog, "");
            return TRUE;
        case WEBKIT_SCRIPT_DIALOG_BEFORE_UNLOAD_CONFIRM:
            webkit_script_dialog_confirm_set_confirmed(dialog, TRUE);
            return TRUE;
    }
    return FALSE;
}

static WebKitWebView *on_create_web_view(WebKitWebView *v, WebKitNavigationAction *action, gpointer user_data)
{
    (void)user_data;
    WebKitURIRequest *req = webkit_navigation_action_get_request(action);
    const gchar *uri = req ? webkit_uri_request_get_uri(req) : NULL;
    if (uri) webkit_web_view_load_uri(v, uri);
    return NULL;
}

static void on_mouse_target_changed(WebKitWebView *v, WebKitHitTestResult *hit, guint modifiers, gpointer user_data)
{
    (void)v; (void)modifiers;
    tab_t *tab = (tab_t *)user_data;
    if (!tab || !tab->inst || !tab->inst->cb.on_cursor) return;
    int shape = 0;
    if (webkit_hit_test_result_context_is_link(hit)) {
        shape = 1; // pointer / hand
    } else if (webkit_hit_test_result_context_is_editable(hit)) {
        shape = 2; // I-beam / text
    }
    tab->inst->cb.on_cursor(tab->inst->userdata, tab->tab_id, shape);
}

static void on_notify_title(GObject *obj, GParamSpec *pspec, gpointer user_data) {
    (void)pspec;
    tab_t *tab = (tab_t *)user_data;
    if (!tab || !tab->inst || !tab->inst->cb.on_title) return;
    const char *title = webkit_web_view_get_title(WEBKIT_WEB_VIEW(obj));
    if (title) tab->inst->cb.on_title(tab->inst->userdata, tab->tab_id, title);
}

static void on_notify_uri(GObject *obj, GParamSpec *pspec, gpointer user_data) {
    (void)pspec;
    tab_t *tab = (tab_t *)user_data;
    if (!tab || !tab->inst || !tab->inst->cb.on_url) return;
    const char *uri = webkit_web_view_get_uri(WEBKIT_WEB_VIEW(obj));
    if (uri) tab->inst->cb.on_url(tab->inst->userdata, tab->tab_id, uri);
}

static tab_t *create_tab_internal(instance_t *inst, uint32_t tab_id, const char *url) {
    if (!inst) return NULL;
    tab_t *tab = g_new0(tab_t, 1);
    tab->tab_id = tab_id;
    tab->inst = inst;
    tab->cur_w = inst->def_w > 0 ? inst->def_w : 1280;
    tab->cur_h = inst->def_h > 0 ? inst->def_h : 720;

    static const struct wpe_view_backend_exportable_fdo_client client_vtable = {
        .export_buffer_resource = NULL,
        .export_dmabuf_resource = NULL,
        .export_shm_buffer      = on_shm_buffer,
    };
    tab->exportable = wpe_view_backend_exportable_fdo_create(
        &client_vtable, tab, tab->cur_w, tab->cur_h);
    if (!tab->exportable) {
        g_free(tab);
        return NULL;
    }

    WebKitSettings *settings = webkit_settings_new();
    webkit_settings_set_enable_write_console_messages_to_stdout(settings, FALSE);
    webkit_settings_set_enable_javascript(settings, TRUE);
    webkit_settings_set_enable_html5_local_storage(settings, TRUE);
    webkit_settings_set_enable_html5_database(settings, TRUE);
    webkit_settings_set_enable_webgl(settings, TRUE);
    webkit_settings_set_enable_smooth_scrolling(settings, FALSE);
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
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15");

    WebKitWebsitePolicies *policies = webkit_website_policies_new_with_policies(
        "autoplay", WEBKIT_AUTOPLAY_ALLOW,
        NULL);

    WebKitUserContentManager *ucm = webkit_user_content_manager_new();
    WebKitUserScript *us = webkit_user_script_new(
        k_media_lifecycle_script,
        WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES,
        WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START,
        NULL, NULL);
    webkit_user_content_manager_add_script(ucm, us);
    webkit_user_script_unref(us);

    WebKitWebViewBackend *vb = webkit_web_view_backend_new(
        wpe_view_backend_exportable_fdo_get_view_backend(tab->exportable),
        NULL, NULL);
    tab->view = WEBKIT_WEB_VIEW(g_object_new(
        WEBKIT_TYPE_WEB_VIEW,
        "backend", vb,
        "settings", settings,
        "website-policies", policies,
        "user-content-manager", ucm,
        NULL));
    g_object_unref(settings);
    g_object_unref(policies);
    g_object_unref(ucm);
    WebKitColor bg_color = { 1.0, 1.0, 1.0, 1.0 };
    webkit_web_view_set_background_color(tab->view, &bg_color);

    g_signal_connect(tab->view, "decide-policy", G_CALLBACK(on_decide_policy), tab);
    g_signal_connect(tab->view, "load-changed", G_CALLBACK(on_load_changed), tab);
    g_signal_connect(tab->view, "load-failed", G_CALLBACK(on_load_failed), tab);
    g_signal_connect(tab->view, "load-failed-with-tls-errors", G_CALLBACK(on_load_failed_with_tls_errors), tab);
    g_signal_connect(tab->view, "web-process-terminated", G_CALLBACK(on_web_process_terminated), tab);
    g_signal_connect(tab->view, "script-dialog", G_CALLBACK(on_script_dialog), tab);
    g_signal_connect(tab->view, "create", G_CALLBACK(on_create_web_view), tab);
    g_signal_connect(tab->view, "mouse-target-changed", G_CALLBACK(on_mouse_target_changed), tab);
    g_signal_connect(tab->view, "notify::title", G_CALLBACK(on_notify_title), tab);
    g_signal_connect(tab->view, "notify::uri",   G_CALLBACK(on_notify_uri),   tab);

    if (url && url[0]) webkit_web_view_load_uri(tab->view, url);

    g_hash_table_insert(inst->tabs, GUINT_TO_POINTER(tab_id), tab);
    return tab;
}

static void free_tab(gpointer p) {
    tab_t *tab = (tab_t *)p;
    if (!tab) return;
    if (tab->exportable) {
        wpe_view_backend_exportable_fdo_destroy(tab->exportable);
        tab->exportable = NULL;
    }
    if (tab->view) {
        g_object_unref(tab->view);
        tab->view = NULL;
    }
    g_free(tab);
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
    inst->def_w     = viewport_w;
    inst->def_h     = viewport_h;
    inst->ctx       = g_main_context_default();
    inst->loop      = g_main_loop_new(inst->ctx, FALSE);
    inst->tabs      = g_hash_table_new_full(g_direct_hash, g_direct_equal, NULL, free_tab);

    if (access("/run/user/1000/pulse/native", F_OK) == 0) {
        setenv("PULSE_SERVER", "unix:/run/user/1000/pulse/native", 0);
    }

    static gsize web_ctx_init = 0;
    if (g_once_init_enter(&web_ctx_init)) {
        WebKitWebContext *web_ctx = webkit_web_context_get_default();
        webkit_web_context_set_sandbox_enabled(web_ctx, FALSE);
        webkit_web_context_set_cache_model(web_ctx, WEBKIT_CACHE_MODEL_WEB_BROWSER);
        WebKitCookieManager *cm = webkit_web_context_get_cookie_manager(web_ctx);
        webkit_cookie_manager_set_accept_policy(cm, WEBKIT_COOKIE_POLICY_ACCEPT_ALWAYS);

        const char *home = g_get_home_dir();
        if (home) {
            gchar *cookie_dir = g_build_filename(home, ".local", "share", "gutted-browser", NULL);
            g_mkdir_with_parents(cookie_dir, 0700);
            gchar *cookie_db = g_build_filename(cookie_dir, "cookies.sqlite", NULL);
            webkit_cookie_manager_set_persistent_storage(cm, cookie_db, WEBKIT_COOKIE_PERSISTENT_STORAGE_SQLITE);
            g_free(cookie_dir);
            g_free(cookie_db);
        }
        g_once_init_leave(&web_ctx_init, 1);
    }

    // Create initial tab (tab_id = 1)
    create_tab_internal(inst, 1, initial_url ? initial_url : "https://duckduckgo.com");

    if (inst->cb.on_ready) inst->cb.on_ready(inst->userdata, (gutted_wpe_handle)inst);

    g_main_loop_run(inst->loop);

    g_hash_table_destroy(inst->tabs);
    g_main_loop_unref(inst->loop);
    g_free(inst);
    return 0;
}

// ─── Thread-Safe Operations (Marshalled to WPE Loop) ──────────────────────

typedef struct { instance_t *inst; } idle_stop_t;
static gboolean idle_stop(gpointer p) {
    idle_stop_t *s = p;
    if (s->inst && s->inst->loop) g_main_loop_quit(s->inst->loop);
    g_free(s);
    return G_SOURCE_REMOVE;
}

void gutted_wpe_stop(gutted_wpe_handle h) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_stop_t *s = g_new0(idle_stop_t, 1);
    s->inst = inst;
    g_main_context_invoke_full(inst->ctx, G_PRIORITY_HIGH, idle_stop, s, NULL);
}

typedef struct { instance_t *inst; uint32_t tab_id; char *url; } idle_tab_op_t;

static gboolean idle_create_tab(gpointer p) {
    idle_tab_op_t *o = p;
    if (o->inst && !g_hash_table_lookup(o->inst->tabs, GUINT_TO_POINTER(o->tab_id))) {
        create_tab_internal(o->inst, o->tab_id, o->url);
    }
    g_free(o->url);
    g_free(o);
    return G_SOURCE_REMOVE;
}

void gutted_wpe_create_tab(gutted_wpe_handle h, uint32_t tab_id, const char *url) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_tab_op_t *o = g_new0(idle_tab_op_t, 1);
    o->inst = inst;
    o->tab_id = tab_id;
    o->url = g_strdup(url);
    g_main_context_invoke(inst->ctx, idle_create_tab, o);
}

static gboolean idle_close_tab(gpointer p) {
    idle_tab_op_t *o = p;
    if (o->inst) {
        g_hash_table_remove(o->inst->tabs, GUINT_TO_POINTER(o->tab_id));
    }
    g_free(o);
    return G_SOURCE_REMOVE;
}

void gutted_wpe_close_tab(gutted_wpe_handle h, uint32_t tab_id) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_tab_op_t *o = g_new0(idle_tab_op_t, 1);
    o->inst = inst;
    o->tab_id = tab_id;
    g_main_context_invoke(inst->ctx, idle_close_tab, o);
}

static gboolean idle_load_uri(gpointer p) {
    idle_tab_op_t *o = p;
    if (o->inst) {
        tab_t *tab = g_hash_table_lookup(o->inst->tabs, GUINT_TO_POINTER(o->tab_id));
        if (tab && tab->view && o->url) {
            webkit_web_view_load_uri(tab->view, o->url);
        }
    }
    g_free(o->url);
    g_free(o);
    return G_SOURCE_REMOVE;
}

void gutted_wpe_load_uri(gutted_wpe_handle h, uint32_t tab_id, const char *url) {
    instance_t *inst = (instance_t *)h;
    if (!inst || !url) return;
    idle_tab_op_t *o = g_new0(idle_tab_op_t, 1);
    o->inst = inst;
    o->tab_id = tab_id;
    o->url = g_strdup(url);
    g_main_context_invoke(inst->ctx, idle_load_uri, o);
}

typedef struct { instance_t *inst; uint32_t tab_id; uint32_t w, h; bool all; } idle_resize_t;
static gboolean idle_resize(gpointer p) {
    idle_resize_t *r = p;
    if (r->inst) {
        r->inst->def_w = r->w;
        r->inst->def_h = r->h;
        if (r->all) {
            GHashTableIter iter;
            gpointer key, value;
            g_hash_table_iter_init(&iter, r->inst->tabs);
            while (g_hash_table_iter_next(&iter, &key, &value)) {
                tab_t *tab = (tab_t *)value;
                if (tab && tab->exportable) {
                    tab->cur_w = r->w;
                    tab->cur_h = r->h;
                    struct wpe_view_backend *vb =
                        wpe_view_backend_exportable_fdo_get_view_backend(tab->exportable);
                    wpe_view_backend_dispatch_set_size(vb, r->w, r->h);
                }
            }
        } else {
            tab_t *tab = g_hash_table_lookup(r->inst->tabs, GUINT_TO_POINTER(r->tab_id));
            if (tab && tab->exportable) {
                tab->cur_w = r->w;
                tab->cur_h = r->h;
                struct wpe_view_backend *vb =
                    wpe_view_backend_exportable_fdo_get_view_backend(tab->exportable);
                wpe_view_backend_dispatch_set_size(vb, r->w, r->h);
            }
        }
    }
    g_free(r);
    return G_SOURCE_REMOVE;
}

void gutted_wpe_resize(gutted_wpe_handle h, uint32_t tab_id, uint32_t w, uint32_t h_px) {
    instance_t *inst = (instance_t *)h;
    if (!inst || w == 0 || h_px == 0) return;
    idle_resize_t *r = g_new0(idle_resize_t, 1);
    r->inst = inst; r->tab_id = tab_id; r->w = w; r->h = h_px; r->all = false;
    g_main_context_invoke(inst->ctx, idle_resize, r);
}

void gutted_wpe_resize_all(gutted_wpe_handle h, uint32_t w, uint32_t h_px) {
    instance_t *inst = (instance_t *)h;
    if (!inst || w == 0 || h_px == 0) return;
    idle_resize_t *r = g_new0(idle_resize_t, 1);
    r->inst = inst; r->w = w; r->h = h_px; r->all = true;
    g_main_context_invoke(inst->ctx, idle_resize, r);
}

// ─── Input Dispatch ──────────────────────────────────────────────────────

typedef struct {
    instance_t *inst;
    uint32_t    tab_id;
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
    if (!e->inst) { g_free(e); return G_SOURCE_REMOVE; }
    tab_t *tab = g_hash_table_lookup(e->inst->tabs, GUINT_TO_POINTER(e->tab_id));
    if (!tab || !tab->exportable) { g_free(e); return G_SOURCE_REMOVE; }
    struct wpe_view_backend *vb =
        wpe_view_backend_exportable_fdo_get_view_backend(tab->exportable);
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

void gutted_wpe_inject_pointer_motion(gutted_wpe_handle h, uint32_t tab_id, int32_t x, int32_t y, uint32_t modifiers) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_input_t *e = g_new0(idle_input_t, 1);
    e->inst = inst; e->tab_id = tab_id; e->kind = 0; e->x = x; e->y = y; e->modifiers = modifiers;
    g_main_context_invoke(inst->ctx, idle_input, e);
}

void gutted_wpe_inject_pointer_button(gutted_wpe_handle h, uint32_t tab_id, int32_t x, int32_t y, uint32_t button, bool pressed, uint32_t modifiers) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_input_t *e = g_new0(idle_input_t, 1);
    e->inst = inst; e->tab_id = tab_id; e->kind = 1; e->x = x; e->y = y; e->button = button; e->pressed = pressed; e->modifiers = modifiers;
    g_main_context_invoke(inst->ctx, idle_input, e);
}

void gutted_wpe_inject_key(gutted_wpe_handle h, uint32_t tab_id, uint32_t keysym, uint32_t modifiers, bool pressed) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_input_t *e = g_new0(idle_input_t, 1);
    e->inst = inst; e->tab_id = tab_id; e->kind = 2; e->keysym = keysym; e->modifiers = modifiers; e->pressed = pressed;
    g_main_context_invoke(inst->ctx, idle_input, e);
}

void gutted_wpe_inject_axis(gutted_wpe_handle h, uint32_t tab_id, int32_t x, int32_t y, double dx, double dy, uint32_t modifiers) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_input_t *e = g_new0(idle_input_t, 1);
    e->inst = inst; e->tab_id = tab_id; e->kind = 3; e->x = x; e->y = y; e->dx = dx; e->dy = dy; e->modifiers = modifiers;
    g_main_context_invoke(inst->ctx, idle_input, e);
}

// ─── Tab Controls ────────────────────────────────────────────────────────

typedef struct { instance_t *inst; uint32_t tab_id; double level; int action; } idle_ctrl_t;
static gboolean idle_tab_ctrl(gpointer p) {
    idle_ctrl_t *c = p;
    if (c->inst) {
        tab_t *tab = g_hash_table_lookup(c->inst->tabs, GUINT_TO_POINTER(c->tab_id));
        if (tab && tab->view) {
            switch (c->action) {
                case 0: webkit_web_view_go_back(tab->view); break;
                case 1: webkit_web_view_go_forward(tab->view); break;
                case 2: webkit_web_view_reload(tab->view); break;
                case 3: webkit_web_view_stop_loading(tab->view); break;
                case 4: webkit_web_view_set_zoom_level(tab->view, c->level); break;
            }
        }
    }
    g_free(c);
    return G_SOURCE_REMOVE;
}

void gutted_wpe_set_zoom(gutted_wpe_handle h, uint32_t tab_id, double level) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_ctrl_t *c = g_new0(idle_ctrl_t, 1);
    c->inst = inst; c->tab_id = tab_id; c->level = level; c->action = 4;
    g_main_context_invoke(inst->ctx, idle_tab_ctrl, c);
}

void gutted_wpe_go_back(gutted_wpe_handle h, uint32_t tab_id) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_ctrl_t *c = g_new0(idle_ctrl_t, 1);
    c->inst = inst; c->tab_id = tab_id; c->action = 0;
    g_main_context_invoke(inst->ctx, idle_tab_ctrl, c);
}

void gutted_wpe_go_forward(gutted_wpe_handle h, uint32_t tab_id) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_ctrl_t *c = g_new0(idle_ctrl_t, 1);
    c->inst = inst; c->tab_id = tab_id; c->action = 1;
    g_main_context_invoke(inst->ctx, idle_tab_ctrl, c);
}

void gutted_wpe_reload(gutted_wpe_handle h, uint32_t tab_id) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_ctrl_t *c = g_new0(idle_ctrl_t, 1);
    c->inst = inst; c->tab_id = tab_id; c->action = 2;
    g_main_context_invoke(inst->ctx, idle_tab_ctrl, c);
}

void gutted_wpe_stop_loading(gutted_wpe_handle h, uint32_t tab_id) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_ctrl_t *c = g_new0(idle_ctrl_t, 1);
    c->inst = inst; c->tab_id = tab_id; c->action = 3;
    g_main_context_invoke(inst->ctx, idle_tab_ctrl, c);
}

typedef struct { instance_t *inst; bool cookies, cache, storage; } idle_clear_t;
static gboolean idle_clear(gpointer p) {
    idle_clear_t *c = p;
    WebKitWebContext *web_ctx = webkit_web_context_get_default();
    WebKitWebsiteDataManager *dm = webkit_web_context_get_website_data_manager(web_ctx);
    WebKitWebsiteDataTypes types = 0;
    if (c->cookies) types |= WEBKIT_WEBSITE_DATA_COOKIES;
    if (c->cache)   types |= WEBKIT_WEBSITE_DATA_DISK_CACHE | WEBKIT_WEBSITE_DATA_MEMORY_CACHE;
    if (c->storage) types |= WEBKIT_WEBSITE_DATA_LOCAL_STORAGE | WEBKIT_WEBSITE_DATA_INDEXEDDB_DATABASES;

    if (types != 0 && dm) {
        webkit_website_data_manager_clear(dm, types, 0, NULL, NULL, NULL);
    }
    g_free(c);
    return G_SOURCE_REMOVE;
}

void gutted_wpe_clear_data(gutted_wpe_handle h, bool clear_cookies, bool clear_cache, bool clear_storage) {
    instance_t *inst = (instance_t *)h;
    if (!inst) return;
    idle_clear_t *c = g_new0(idle_clear_t, 1);
    c->inst = inst; c->cookies = clear_cookies; c->cache = clear_cache; c->storage = clear_storage;
    g_main_context_invoke(inst->ctx, idle_clear, c);
}
