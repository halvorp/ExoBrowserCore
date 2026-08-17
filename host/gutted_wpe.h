// gutted_wpe: C library exposing the WPE two-process seam behind a
// callback-shaped API so Rust can drive it. Doctrine: keep the GLib +
// WebKit + libwpe glue in C where it's tested; ship pixel data + input
// intents across a plain C boundary.
//
// Multi-instance: each call to gutted_wpe_run creates its own WebKitWebView
// (and its own WPEWebProcess child). The returned handle scopes every
// thread-safe op (stop/load/resize/inject) to a specific tab. Callers
// keep their own map of handles; there is no global registry.

#ifndef GUTTED_WPE_H
#define GUTTED_WPE_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/// Opaque handle to a running WPE view. Only valid between the
/// on_ready callback firing and gutted_wpe_run returning. Thread-safe
/// to pass; the C library serialises via GLib main context.
typedef void *gutted_wpe_handle;

/// Called for every rendered frame from WPEWebProcess.
/// `pixels` is BGRA (WL_SHM_FORMAT_ARGB8888 in memory order).
/// Pointer is only valid for the duration of the call — copy anything you keep.
typedef void (*gutted_wpe_frame_cb)(
    void *userdata,
    const uint8_t *pixels,
    int32_t width,
    int32_t height,
    int32_t stride,
    uint32_t wl_shm_format);

/// Called when WebKit's load state changes for the top-level view.
/// `state`: 0=started, 1=redirected, 2=committed, 3=finished
typedef void (*gutted_wpe_load_cb)(void *userdata, int32_t state);

/// Called when the cursor shape should change (mouse-target-changed).
/// `shape_id`: 0 = default, 1 = pointer, 2 = text.
typedef void (*gutted_wpe_cursor_cb)(void *userdata, int32_t shape_id);

/// Called when the page's <title> changes. `title` is UTF-8, may be NULL.
typedef void (*gutted_wpe_title_cb)(void *userdata, const char *title);

/// Called when WebKit's committed URL changes (link clicks, redirects,
/// history.pushState, etc.). `url` is UTF-8, may be NULL.
typedef void (*gutted_wpe_url_cb)(void *userdata, const char *url);

/// Fired once, on the WPE thread, as soon as the instance is fully
/// set up and just before the GLib main loop starts. Gives the caller
/// a handle they can use from any thread for stop/load/resize/inject_*.
typedef void (*gutted_wpe_ready_cb)(void *userdata, gutted_wpe_handle handle);

/// Callbacks passed to gutted_wpe_run. `on_frame` is required, others optional.
typedef struct {
    gutted_wpe_frame_cb  on_frame;
    gutted_wpe_load_cb   on_load;   // may be NULL
    gutted_wpe_cursor_cb on_cursor; // may be NULL
    gutted_wpe_ready_cb  on_ready;  // may be NULL, but you almost certainly want it
    gutted_wpe_title_cb  on_title;  // may be NULL
    gutted_wpe_url_cb    on_url;    // may be NULL
} gutted_wpe_callbacks;

/// Blocking: initialises libwpe/WPE-fdo, creates a WebKitWebView, loads
/// `initial_url`, and runs a GLib main loop until gutted_wpe_stop(handle).
/// Must be called on a dedicated thread (WPE uses TLS + GLib). Multiple
/// instances may coexist, one per calling thread.
/// Returns 0 on clean shutdown, non-zero on init failure.
int gutted_wpe_run(
    const char *initial_url,
    int32_t viewport_w,
    int32_t viewport_h,
    const gutted_wpe_callbacks *cb,
    void *userdata);

/// Ask the identified instance to exit. Thread-safe. No-op if handle is NULL.
void gutted_wpe_stop(gutted_wpe_handle h);

/// Navigate the identified view to a new URL. Thread-safe. No-op on NULL.
void gutted_wpe_load_uri(gutted_wpe_handle h, const char *url);

/// Tell WebKit this view's viewport size changed. Thread-safe.
void gutted_wpe_resize(gutted_wpe_handle h, uint32_t w, uint32_t h_px);

/// Input injection. All thread-safe; each marshals to the GLib main
/// context. `modifiers` is the WPE bitmask (see wpe/input.h).
void gutted_wpe_inject_pointer_motion(gutted_wpe_handle h,
                                      int32_t x, int32_t y, uint32_t modifiers);
void gutted_wpe_inject_pointer_button(gutted_wpe_handle h,
                                      int32_t x, int32_t y,
                                      uint32_t button, bool pressed,
                                      uint32_t modifiers);
void gutted_wpe_inject_key(gutted_wpe_handle h,
                           uint32_t keysym, uint32_t modifiers, bool pressed);

/// 2-axis smooth wheel/trackpad scroll. dx/dy are pixel deltas.
void gutted_wpe_inject_axis(gutted_wpe_handle h,
                            int32_t x, int32_t y,
                            double dx, double dy,
                            uint32_t modifiers);

/// Set WebKit page zoom level (1.0 = 100%). Thread-safe; clamped to [0.25, 5.0].
void gutted_wpe_set_zoom(gutted_wpe_handle h, double level);

/// History navigation. Thread-safe; no-op on NULL handle. Reload preserves
/// scroll position and session state (unlike load_uri with the same URL).
void gutted_wpe_go_back(gutted_wpe_handle h);
void gutted_wpe_go_forward(gutted_wpe_handle h);
void gutted_wpe_reload(gutted_wpe_handle h);

#ifdef __cplusplus
}
#endif

#endif
