// gutted_wpe: C library exposing WPE WebKit with native multi-tab support
// running on a single GLib event loop on a dedicated thread.

#ifndef GUTTED_WPE_H
#define GUTTED_WPE_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef void *gutted_wpe_handle;

typedef void (*gutted_wpe_frame_cb)(
    void *userdata,
    uint32_t tab_id,
    const uint8_t *pixels,
    int32_t width,
    int32_t height,
    int32_t stride,
    uint32_t wl_shm_format);

typedef void (*gutted_wpe_load_cb)(void *userdata, uint32_t tab_id, int32_t state);
typedef void (*gutted_wpe_cursor_cb)(void *userdata, uint32_t tab_id, int32_t shape_id);
typedef void (*gutted_wpe_title_cb)(void *userdata, uint32_t tab_id, const char *title);
typedef void (*gutted_wpe_url_cb)(void *userdata, uint32_t tab_id, const char *url);
typedef void (*gutted_wpe_ready_cb)(void *userdata, gutted_wpe_handle handle);

typedef struct {
    gutted_wpe_frame_cb  on_frame;
    gutted_wpe_load_cb   on_load;
    gutted_wpe_cursor_cb on_cursor;
    gutted_wpe_ready_cb  on_ready;
    gutted_wpe_title_cb  on_title;
    gutted_wpe_url_cb    on_url;
} gutted_wpe_callbacks;

int gutted_wpe_run(
    const char *initial_url,
    int32_t viewport_w,
    int32_t viewport_h,
    const gutted_wpe_callbacks *cb,
    void *userdata);

void gutted_wpe_stop(gutted_wpe_handle h);

void gutted_wpe_create_tab(gutted_wpe_handle h, uint32_t tab_id, const char *url);
void gutted_wpe_close_tab(gutted_wpe_handle h, uint32_t tab_id);
void gutted_wpe_load_uri(gutted_wpe_handle h, uint32_t tab_id, const char *url);
void gutted_wpe_resize(gutted_wpe_handle h, uint32_t tab_id, uint32_t w, uint32_t h_px);
void gutted_wpe_resize_all(gutted_wpe_handle h, uint32_t w, uint32_t h_px);

void gutted_wpe_inject_pointer_motion(gutted_wpe_handle h, uint32_t tab_id, int32_t x, int32_t y, uint32_t modifiers);
void gutted_wpe_inject_pointer_button(gutted_wpe_handle h, uint32_t tab_id, int32_t x, int32_t y, uint32_t button, bool pressed, uint32_t modifiers);
void gutted_wpe_inject_key(gutted_wpe_handle h, uint32_t tab_id, uint32_t keysym, uint32_t modifiers, bool pressed);
void gutted_wpe_inject_axis(gutted_wpe_handle h, uint32_t tab_id, int32_t x, int32_t y, double dx, double dy, uint32_t modifiers);

void gutted_wpe_set_zoom(gutted_wpe_handle h, uint32_t tab_id, double level);
void gutted_wpe_go_back(gutted_wpe_handle h, uint32_t tab_id);
void gutted_wpe_go_forward(gutted_wpe_handle h, uint32_t tab_id);
void gutted_wpe_reload(gutted_wpe_handle h, uint32_t tab_id);
void gutted_wpe_stop_loading(gutted_wpe_handle h, uint32_t tab_id);
void gutted_wpe_clear_data(gutted_wpe_handle h, bool clear_cookies, bool clear_cache, bool clear_storage);

#ifdef __cplusplus
}
#endif

#endif
