// Standalone gutted-host driver — thin CLI over libgutted_wpe.a.
// Kept for smoke-testing the WPE glue independently of the Rust host.

#include "gutted_wpe.h"

#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const char *const kDefaultURL =
    "data:text/html,"
    "<style>html,body{margin:0;height:100%;background:linear-gradient(45deg,#08f,#f08);"
    "color:#fff;font:64px sans-serif;display:flex;align-items:center;justify-content:center}</style>"
    "<div id=x>GUTTED 0</div>"
    "<script>let n=0;setInterval(()=>x.textContent='GUTTED '+(++n),16)</script>";

static unsigned g_count = 0;
static const unsigned STOP_AFTER = 60;
// Set by on_ready; picked up by on_frame (STOP_AFTER) and on_sigint.
static gutted_wpe_handle g_h = NULL;

static void on_frame(void *ud, const uint8_t *px, int32_t w, int32_t h,
                     int32_t stride, uint32_t fmt)
{
    (void)ud; (void)px;
    g_count++;
    printf("[frame %4u] %dx%d stride=%d fmt=0x%08x bytes=%d\n",
           g_count, w, h, stride, fmt, stride * h);
    if (g_count >= STOP_AFTER && g_h) gutted_wpe_stop(g_h);
}

static void on_load(void *ud, int32_t s)
{
    (void)ud;
    static const char *names[] = { "started", "redirected", "committed", "finished" };
    if (s >= 0 && (size_t)s < sizeof(names)/sizeof(*names))
        printf("[load ] %s\n", names[s]);
}

static void on_ready(void *ud, gutted_wpe_handle h)
{
    (void)ud;
    g_h = h;
}

static void on_sigint(int sig) { (void)sig; if (g_h) gutted_wpe_stop(g_h); }

int main(int argc, char **argv)
{
    const char *url = (argc > 1) ? argv[1] : kDefaultURL;
    signal(SIGINT, on_sigint);
    gutted_wpe_callbacks cb = {
        .on_frame = on_frame,
        .on_load  = on_load,
        .on_ready = on_ready,
    };
    int rc = gutted_wpe_run(url, 1280, 720, &cb, NULL);
    if (rc != 0) fprintf(stderr, "gutted_wpe_run failed rc=%d\n", rc);
    return rc;
}
