/* nvkmstest — boot-time KMS test for LeandrOS's card0, written for the
 * NVIDIA copy-engine scanout (lane nvkms, M8) and harmless anywhere else.
 *
 * Raw DRM ioctls, no libdrm (one C file, built like nvktest). It does what a
 * Wayland compositor's DRM backend does, in order, and checks the pixels that
 * reach the scanout surface through /dev/fb0:
 *
 *   probe    VERSION, SET_MASTER, caps, resources, connector modes, encoder,
 *            planes, IN_FORMATS (format/modifier pairs)
 *   dumb     2 dumb buffers, ADDFB2, SETCRTC, N page flips with events while a
 *            CPU-drawn bar moves; flip rate; scanout pixels verified
 *   gem      (driver "nouveau") the same with 2 nouveau GEM buffers in system
 *            memory, CPU-written, ADDFB2 with the LINEAR modifier: the
 *            copy-engine path
 *   vk       (driver "nouveau" + the NVK ICD) the same with 2 buffers NVK
 *            fills on the GPU (vkCmdFillBuffer), exported as dma-bufs,
 *            imported on card0 (PRIME), ADDFB2 LINEAR: the cosmic-comp path
 *            minus GL
 *   atomic   SET_CLIENT_CAP(ATOMIC), property ids, a modeset commit, N/2
 *            commits with PAGE_FLIP_EVENT (smithay's path), a TEST_ONLY
 *            commit of a mode the connector does not list (refused on NVIDIA)
 *   console  DROP_MASTER + close: the text console must come back (the
 *            scanout no longer shows the last frame)
 *
 * Every subtest ends in exactly one line
 *     nvkmstest <name>: PASS|FAIL|SKIP (<why>)
 * then "---- nvkmstest summary ----" (every result again) and
 *     ==== nvkmstest done: P passed, F failed, S skipped ====
 * Everything is bounded: event waits time out (2 s), a SIGALRM watchdog
 * (default 120 s) prints the summary and exits.
 *
 * Usage: nvkmstest [--frames=N] [--icd=PATH] [--no-vk] [--hold-ms=N]
 * Exit code: number of failures (clamped to 120).
 */

#include <vulkan/vulkan_core.h>
#include <vulkan/vk_icd.h>

#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

/* ───────────────────────────── DRM ABI ───────────────────────────── */

#define DRM_IOWR_(nr, sz) ((3u << 30) | ((unsigned)(sz) << 16) | (0x64u << 8) | (nr))
#define DRM_IOW_(nr, sz)  ((1u << 30) | ((unsigned)(sz) << 16) | (0x64u << 8) | (nr))
#define DRM_IO_(nr)       ((0x64u << 8) | (nr))

struct drm_version_ { int major, minor, patch; size_t name_len; char *name; size_t date_len; char *date; size_t desc_len; char *desc; };
struct drm_get_cap_ { uint64_t capability, value; };
struct drm_set_client_cap_ { uint64_t capability, value; };
struct drm_mode_card_res_ {
    uint64_t fb_id_ptr, crtc_id_ptr, connector_id_ptr, encoder_id_ptr;
    uint32_t count_fbs, count_crtcs, count_connectors, count_encoders;
    uint32_t min_width, max_width, min_height, max_height;
};
struct drm_mode_modeinfo_ {
    uint32_t clock;
    uint16_t hdisplay, hsync_start, hsync_end, htotal, hskew;
    uint16_t vdisplay, vsync_start, vsync_end, vtotal, vscan;
    uint32_t vrefresh, flags, type;
    char name[32];
};
struct drm_mode_get_connector_ {
    uint64_t encoders_ptr, modes_ptr, props_ptr, prop_values_ptr;
    uint32_t count_modes, count_props, count_encoders, encoder_id, connector_id, connector_type, connector_type_id,
             connection, mm_width, mm_height, subpixel, pad;
};
struct drm_mode_get_encoder_ { uint32_t encoder_id, encoder_type, crtc_id, possible_crtcs, possible_clones; };
struct drm_mode_crtc_ {
    uint64_t set_connectors_ptr;
    uint32_t count_connectors, crtc_id, fb_id, x, y, gamma_size, mode_valid;
    struct drm_mode_modeinfo_ mode;
};
struct drm_mode_create_dumb_ { uint32_t height, width, bpp, flags, handle, pitch; uint64_t size; };
struct drm_mode_map_dumb_ { uint32_t handle, pad; uint64_t offset; };
struct drm_mode_destroy_dumb_ { uint32_t handle; };
struct drm_mode_fb_cmd2_ {
    uint32_t fb_id, width, height, pixel_format, flags;
    uint32_t handles[4], pitches[4], offsets[4];
    uint64_t modifier[4];
};
struct drm_mode_crtc_page_flip_ { uint32_t crtc_id, fb_id, flags, reserved; uint64_t user_data; };
struct drm_mode_get_plane_res_ { uint64_t plane_id_ptr; uint32_t count_planes; };
struct drm_mode_get_plane_ { uint32_t plane_id, crtc_id, fb_id, possible_crtcs, gamma_size, count_format_types; uint64_t format_type_ptr; };
struct drm_mode_obj_get_properties_ { uint64_t props_ptr, prop_values_ptr; uint32_t count_props, obj_id, obj_type; };
struct drm_mode_get_property_ { uint64_t values_ptr, enum_blob_ptr; uint32_t prop_id, flags; char name[32]; uint32_t count_values, count_enum_blobs; };
struct drm_mode_get_blob_ { uint32_t blob_id, length; uint64_t data; };
struct drm_mode_create_blob_ { uint64_t data; uint32_t length, blob_id; };
struct drm_mode_destroy_blob_ { uint32_t blob_id; };
struct drm_mode_atomic_ { uint32_t flags, count_objs; uint64_t objs_ptr, count_props_ptr, props_ptr, prop_values_ptr, reserved, user_data; };
struct drm_gem_close_ { uint32_t handle, pad; };
struct drm_prime_handle_ { uint32_t handle, flags; int32_t fd; };
struct drm_event_ { uint32_t type, length; };
struct drm_event_vblank_ { struct drm_event_ base; uint64_t user_data; uint32_t tv_sec, tv_usec, sequence, crtc_id; };
/* nouveau */
struct nv_gem_info_ { uint32_t handle, domain; uint64_t size, offset, map_handle; uint32_t tile_mode, tile_flags; };
struct nv_gem_new_ { struct nv_gem_info_ info; uint32_t channel_hint, align; };

#define IOC_VERSION          DRM_IOWR_(0x00, sizeof(struct drm_version_))
#define IOC_GET_CAP          DRM_IOWR_(0x0C, sizeof(struct drm_get_cap_))
#define IOC_SET_CLIENT_CAP   DRM_IOW_(0x0D, sizeof(struct drm_set_client_cap_))
#define IOC_SET_MASTER       DRM_IO_(0x1E)
#define IOC_DROP_MASTER      DRM_IO_(0x1F)
#define IOC_GEM_CLOSE        DRM_IOW_(0x09, sizeof(struct drm_gem_close_))
#define IOC_PRIME_FD_TO_HANDLE DRM_IOWR_(0x2E, sizeof(struct drm_prime_handle_))
#define IOC_GETRESOURCES     DRM_IOWR_(0xA0, sizeof(struct drm_mode_card_res_))
#define IOC_GETCRTC          DRM_IOWR_(0xA1, sizeof(struct drm_mode_crtc_))
#define IOC_SETCRTC          DRM_IOWR_(0xA2, sizeof(struct drm_mode_crtc_))
#define IOC_GETENCODER       DRM_IOWR_(0xA6, sizeof(struct drm_mode_get_encoder_))
#define IOC_GETCONNECTOR     DRM_IOWR_(0xA7, sizeof(struct drm_mode_get_connector_))
#define IOC_GETPROPERTY      DRM_IOWR_(0xAA, sizeof(struct drm_mode_get_property_))
#define IOC_GETPROPBLOB      DRM_IOWR_(0xAC, sizeof(struct drm_mode_get_blob_))
#define IOC_RMFB             DRM_IOWR_(0xAF, sizeof(uint32_t))
#define IOC_PAGE_FLIP        DRM_IOWR_(0xB0, sizeof(struct drm_mode_crtc_page_flip_))
#define IOC_CREATE_DUMB      DRM_IOWR_(0xB2, sizeof(struct drm_mode_create_dumb_))
#define IOC_MAP_DUMB         DRM_IOWR_(0xB3, sizeof(struct drm_mode_map_dumb_))
#define IOC_DESTROY_DUMB     DRM_IOWR_(0xB4, sizeof(struct drm_mode_destroy_dumb_))
#define IOC_GETPLANERESOURCES DRM_IOWR_(0xB5, sizeof(struct drm_mode_get_plane_res_))
#define IOC_GETPLANE         DRM_IOWR_(0xB6, sizeof(struct drm_mode_get_plane_))
#define IOC_ADDFB2           DRM_IOWR_(0xB8, sizeof(struct drm_mode_fb_cmd2_))
#define IOC_OBJ_GETPROPERTIES DRM_IOWR_(0xB9, sizeof(struct drm_mode_obj_get_properties_))
#define IOC_ATOMIC           DRM_IOWR_(0xBC, sizeof(struct drm_mode_atomic_))
#define IOC_CREATEPROPBLOB   DRM_IOWR_(0xBD, sizeof(struct drm_mode_create_blob_))
#define IOC_DESTROYPROPBLOB  DRM_IOWR_(0xBE, sizeof(struct drm_mode_destroy_blob_))
#define IOC_NV_GEM_NEW       DRM_IOWR_(0x80, sizeof(struct nv_gem_new_))

#define CAP_DUMB_BUFFER        0x1
#define CAP_ADDFB2_MODIFIERS   0x10
#define CLIENT_CAP_UNIVERSAL_PLANES 2
#define CLIENT_CAP_ATOMIC      3
#define FB_MODIFIERS           (1u << 1)
#define PAGE_FLIP_EVENT        0x01
#define ATOMIC_TEST_ONLY       0x0100
#define ATOMIC_ALLOW_MODESET   0x0400
#define EVENT_FLIP_COMPLETE    0x02
#define OBJ_CRTC      0xccccccccu
#define OBJ_CONNECTOR 0xc0c0c0c0u
#define OBJ_PLANE     0xeeeeeeeeu
#define XR24 0x34325258u
#define MOD_LINEAR 0ull
#define NV_DOMAIN_GART 4u
#define NV_DOMAIN_MAPPABLE 8u

/* ───────────────────────────── reporting ───────────────────────────── */

static int g_pass, g_fail, g_skip;
static char g_results[16][240];
static int g_nresults;

static void out(const char *fmt, ...)
{
    char buf[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    fputs(buf, stdout);
    fputc('\n', stdout);
    fflush(stdout);
}

static void result(const char *name, const char *what, const char *fmt, ...)
{
    char why[200];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(why, sizeof why, fmt, ap);
    va_end(ap);
    if (!strcmp(what, "PASS")) g_pass++;
    else if (!strcmp(what, "SKIP")) g_skip++;
    else g_fail++;
    out("nvkmstest %s: %s (%s)", name, what, why);
    if (g_nresults < 16)
        snprintf(g_results[g_nresults++], sizeof g_results[0], "nvkmstest %s: %s (%s)", name, what, why);
}

static void summary(void)
{
    out("---- nvkmstest summary ----");
    for (int i = 0; i < g_nresults; i++) out("%s", g_results[i]);
    out("==== nvkmstest done: %d passed, %d failed, %d skipped ====", g_pass, g_fail, g_skip);
}

static void on_alarm(int sig)
{
    (void)sig;
    out("nvkmstest watchdog: TIMEOUT (the run took longer than its budget)");
    g_fail++;
    summary();
    _exit(126);
}

static uint64_t now_ns(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
}

static void sleep_ms(unsigned ms)
{
    struct timespec ts = { ms / 1000, (long)(ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

static int xioctl(int fd, unsigned long req, void *arg)
{
    int r;
    do { r = ioctl(fd, req, arg); } while (r == -1 && (errno == EINTR || errno == EAGAIN));
    return r;
}

/* ───────────────────────────── state ───────────────────────────── */

static int g_fd = -1;
static char g_driver[32];
static uint32_t g_crtc, g_conn, g_plane, g_enc;
static struct drm_mode_modeinfo_ g_mode;
static uint32_t g_w, g_h;
static int g_have_mode;
static unsigned g_frames = 120;
static unsigned g_hold_ms = 1500;
static const char *g_icd = "/usr/lib/libvulkan_nouveau.so";
static int g_no_vk;
static uint32_t g_last_color; /* top-band colour of the last presented frame */

/* The test image: a top band (rows 0..15) whose colour encodes the frame, a
 * horizontal bar moving down, a background. XRGB8888 words. */
static uint32_t band_color(unsigned frame) { return 0x00200000u | ((frame * 37u) & 0xffu) << 8 | ((frame * 91u + 40u) & 0xffu); }
static uint32_t bg_color(unsigned frame) { return (frame & 1) ? 0x00102030u : 0x00203010u; }
#define BAR_COLOR 0x00ffd040u
#define BAND_ROWS 16u

static uint32_t bar_y(unsigned frame, uint32_t h) { uint32_t bh = h / 12; return BAND_ROWS + (frame * 7u) % (h - BAND_ROWS - bh); }

static void draw_cpu(uint32_t *px, uint32_t pitch_words, unsigned frame)
{
    uint32_t bh = g_h / 12, by = bar_y(frame, g_h);
    for (uint32_t y = 0; y < g_h; y++) {
        uint32_t c = y < BAND_ROWS ? band_color(frame) : (y >= by && y < by + bh) ? BAR_COLOR : bg_color(frame);
        uint32_t *row = px + (size_t)y * pitch_words;
        for (uint32_t x = 0; x < g_w; x++) row[x] = c;
    }
}

/* ──────────────────────── scanout read-back (/dev/fb0) ──────────────────────── */

static int g_fb0_pitch, g_fb0_w, g_fb0_h;

static int fb0_info(void)
{
    if (g_fb0_pitch) return 0;
    int fd = open("/dev/fb0", O_RDONLY);
    if (fd < 0) return -1;
    uint32_t info[8] = {0};
    int r = ioctl(fd, 0x4600, info); /* FBIOGET_VSCREENINFO, LeandrOS layout: [7] = pitch */
    close(fd);
    if (r < 0 || !info[0] || !info[7]) return -1;
    g_fb0_w = info[0]; g_fb0_h = info[1]; g_fb0_pitch = info[7];
    return 0;
}

/* Read the top `rows` rows of the scanout; returns words read per row or -1. */
static int fb0_read_top(uint32_t *buf, unsigned rows)
{
    if (fb0_info() < 0) return -1;
    int fd = open("/dev/fb0", O_RDONLY);
    if (fd < 0) return -1;
    size_t want = (size_t)rows * g_fb0_pitch, got = 0;
    while (got < want) {
        ssize_t n = read(fd, (char *)buf + got, want - got);
        if (n <= 0) break;
        got += n;
    }
    close(fd);
    return got == want ? g_fb0_pitch / 4 : -1;
}

/* Is the last frame's top band on the scanout? Returns matching sample
 * count out of `*total` (64 samples over rows 2..13). */
static int verify_band(uint32_t color, int *total)
{
    static uint32_t buf[16 * 8192];
    *total = 0;
    int stride = fb0_read_top(buf, BAND_ROWS);
    if (stride < 0) return -1;
    int ok = 0;
    for (int i = 0; i < 64; i++) {
        uint32_t y = 2 + (i % 12), x = (uint32_t)(((uint64_t)i * 997) % (g_w < (uint32_t)g_fb0_w ? g_w : (uint32_t)g_fb0_w));
        if ((buf[(size_t)y * stride + x] & 0xffffff) == (color & 0xffffff)) ok++;
        (*total)++;
    }
    return ok;
}

/* ─────────────────────────── page flips ─────────────────────────── */

/* Wait for the FLIP_COMPLETE event carrying `ud`; 0 ok, -1 timeout/error. */
static int wait_flip(uint64_t ud, unsigned *stale)
{
    uint64_t deadline = now_ns() + 2000000000ull;
    for (;;) {
        int left = (int)((deadline - now_ns()) / 1000000);
        if ((int64_t)(deadline - now_ns()) <= 0) return -1;
        struct pollfd p = { g_fd, POLLIN, 0 };
        int r = poll(&p, 1, left > 0 ? left : 1);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) continue;
        char buf[256];
        ssize_t n = read(g_fd, buf, sizeof buf);
        if (n < 0) { if (errno == EAGAIN || errno == EINTR) continue; return -1; }
        for (ssize_t o = 0; o + (ssize_t)sizeof(struct drm_event_) <= n;) {
            struct drm_event_ *e = (struct drm_event_ *)(buf + o);
            if (e->length < sizeof *e) return -1;
            if (e->type == EVENT_FLIP_COMPLETE && e->length >= sizeof(struct drm_event_vblank_)) {
                struct drm_event_vblank_ *v = (struct drm_event_vblank_ *)e;
                if (v->user_data == ud) return 0;
                (*stale)++;
            }
            o += e->length;
        }
    }
}

typedef void (*draw_fn)(int idx, unsigned frame, void *ctx);

/* SETCRTC to fbs[0], then `frames` flips alternating fbs[0..1], drawing frame
 * f into the back buffer first. Fills *rate (flips/s) and *why on failure. */
static int flip_loop(const char *name, uint32_t fbs[2], draw_fn draw, void *ctx, double *rate, char *why, size_t whylen)
{
    draw(0, 0, ctx);
    struct drm_mode_crtc_ set = {0};
    set.crtc_id = g_crtc;
    set.fb_id = fbs[0];
    set.set_connectors_ptr = (uintptr_t)&g_conn;
    set.count_connectors = 1;
    set.mode = g_mode;
    set.mode_valid = 1;
    if (xioctl(g_fd, IOC_SETCRTC, &set) < 0) {
        snprintf(why, whylen, "SETCRTC fb %u %ux%u: %s", fbs[0], g_w, g_h, strerror(errno));
        return -1;
    }
    g_last_color = band_color(0);
    unsigned stale = 0;
    uint64_t t0 = now_ns(), worst = 0;
    for (unsigned f = 1; f <= g_frames; f++) {
        int back = f & 1;
        draw(back, f, ctx);
        uint64_t t = now_ns();
        struct drm_mode_crtc_page_flip_ pf = { g_crtc, fbs[back], PAGE_FLIP_EVENT, 0, 0x5eed0000ull + f };
        if (xioctl(g_fd, IOC_PAGE_FLIP, &pf) < 0) {
            snprintf(why, whylen, "PAGE_FLIP %u of %u: %s", f, g_frames, strerror(errno));
            return -1;
        }
        if (wait_flip(pf.user_data, &stale) < 0) {
            snprintf(why, whylen, "no FLIP_COMPLETE event for flip %u within 2 s", f);
            return -1;
        }
        uint64_t d = now_ns() - t;
        if (d > worst) worst = d;
        g_last_color = band_color(f);
    }
    double secs = (now_ns() - t0) / 1e9;
    *rate = g_frames / secs;
    out("nvkmstest %s: %u flips in %.2f s = %.1f flips/s (flip+event worst %.1f ms, stale events %u)", name, g_frames, secs,
        *rate, worst / 1e6, stale);
    return 0;
}

static void verify_and_report(const char *name, double rate, uint32_t fbs[2], int nfb_owned)
{
    (void)fbs; (void)nfb_owned;
    int total = 0, ok = verify_band(g_last_color, &total);
    if (ok < 0)
        result(name, "PASS", "%u flips with events, %.1f flips/s; scanout read-back unavailable (/dev/fb0)", g_frames, rate);
    else if (ok * 10 >= total * 9)
        result(name, "PASS", "%u flips with events, %.1f flips/s; scanout shows the last frame (%d/%d samples)", g_frames, rate, ok, total);
    else
        result(name, "FAIL", "%u flips with events, %.1f flips/s, but the scanout does not show the last frame (%d/%d samples of %06x)",
               g_frames, rate, ok, total, g_last_color);
}

static void rmfb(uint32_t id) { if (id) xioctl(g_fd, IOC_RMFB, &id); }

static int addfb(uint32_t handle, uint32_t pitch, uint32_t offset, int with_mod, uint32_t *fb_id)
{
    struct drm_mode_fb_cmd2_ c = {0};
    c.width = g_w; c.height = g_h; c.pixel_format = XR24;
    c.handles[0] = handle; c.pitches[0] = pitch; c.offsets[0] = offset;
    if (with_mod) { c.flags = FB_MODIFIERS; c.modifier[0] = MOD_LINEAR; }
    if (xioctl(g_fd, IOC_ADDFB2, &c) < 0) return -1;
    *fb_id = c.fb_id;
    return 0;
}

/* ─────────────────────────── probe ─────────────────────────── */

static uint64_t get_cap(uint64_t cap)
{
    struct drm_get_cap_ c = { cap, 0 };
    if (xioctl(g_fd, IOC_GET_CAP, &c) < 0) return (uint64_t)-1;
    return c.value;
}

static int prop_name(uint32_t id, char name[32])
{
    struct drm_mode_get_property_ p = {0};
    p.prop_id = id;
    if (xioctl(g_fd, IOC_GETPROPERTY, &p) < 0) return -1;
    memcpy(name, p.name, 32);
    name[31] = 0;
    return 0;
}

/* Property id named `want` on object (id, type), and its value. */
static uint32_t find_prop(uint32_t obj, uint32_t type, const char *want, uint64_t *val)
{
    uint32_t ids[64]; uint64_t vals[64];
    struct drm_mode_obj_get_properties_ g = { (uintptr_t)ids, (uintptr_t)vals, 64, obj, type };
    if (xioctl(g_fd, IOC_OBJ_GETPROPERTIES, &g) < 0) return 0;
    for (uint32_t i = 0; i < g.count_props && i < 64; i++) {
        char name[32];
        if (prop_name(ids[i], name) == 0 && !strcmp(name, want)) { if (val) *val = vals[i]; return ids[i]; }
    }
    return 0;
}

static void probe(void)
{
    char name[32] = {0}, date[32] = {0}, desc[128] = {0};
    struct drm_version_ v = {0};
    v.name = name; v.name_len = sizeof name - 1;
    v.date = date; v.date_len = sizeof date - 1;
    v.desc = desc; v.desc_len = sizeof desc - 1;
    if (xioctl(g_fd, IOC_VERSION, &v) < 0) { result("probe", "FAIL", "VERSION: %s", strerror(errno)); return; }
    snprintf(g_driver, sizeof g_driver, "%s", name);
    int m = xioctl(g_fd, IOC_SET_MASTER, NULL);
    out("nvkmstest probe: driver '%s' %d.%d.%d (%s); SET_MASTER %s", name, v.major, v.minor, v.patch, desc,
        m == 0 ? "ok" : strerror(errno));
    out("nvkmstest probe: caps DUMB_BUFFER=%" PRIu64 " ADDFB2_MODIFIERS=%" PRIu64, get_cap(CAP_DUMB_BUFFER), get_cap(CAP_ADDFB2_MODIFIERS));
    struct drm_set_client_cap_ cc = { CLIENT_CAP_UNIVERSAL_PLANES, 1 };
    xioctl(g_fd, IOC_SET_CLIENT_CAP, &cc);

    uint32_t crtcs[8], conns[8], encs[8];
    struct drm_mode_card_res_ r = {0};
    r.crtc_id_ptr = (uintptr_t)crtcs; r.count_crtcs = 8;
    r.connector_id_ptr = (uintptr_t)conns; r.count_connectors = 8;
    r.encoder_id_ptr = (uintptr_t)encs; r.count_encoders = 8;
    if (xioctl(g_fd, IOC_GETRESOURCES, &r) < 0 || !r.count_crtcs || !r.count_connectors) {
        result("probe", "FAIL", "GETRESOURCES: %s (%u crtcs, %u connectors)", strerror(errno), r.count_crtcs, r.count_connectors);
        return;
    }
    g_crtc = crtcs[0];
    g_conn = conns[0];
    struct drm_mode_modeinfo_ modes[24];
    uint32_t cenc[4];
    struct drm_mode_get_connector_ c = {0};
    c.connector_id = g_conn;
    c.modes_ptr = (uintptr_t)modes; c.count_modes = 24;
    c.encoders_ptr = (uintptr_t)cenc; c.count_encoders = 4;
    if (xioctl(g_fd, IOC_GETCONNECTOR, &c) < 0 || !c.count_modes) {
        result("probe", "FAIL", "GETCONNECTOR %u: %s (%u modes)", g_conn, strerror(errno), c.count_modes);
        return;
    }
    char list[400] = {0};
    size_t L = 0;
    for (uint32_t i = 0; i < c.count_modes && i < 24; i++)
        L += snprintf(list + L, sizeof list - L, "%s%ux%u@%u%s", i ? " " : "", modes[i].hdisplay, modes[i].vdisplay,
                      modes[i].vrefresh, (modes[i].type & (1u << 3)) ? "*" : "");
    g_mode = modes[0];
    for (uint32_t i = 0; i < c.count_modes && i < 24; i++) if (modes[i].type & (1u << 3)) { g_mode = modes[i]; break; }
    g_w = g_mode.hdisplay; g_h = g_mode.vdisplay; g_have_mode = 1;
    out("nvkmstest probe: crtc %u, connector %u (type %u, %s, %ux%u mm), %u mode(s): %s", g_crtc, g_conn, c.connector_type,
        c.connection == 1 ? "connected" : "NOT connected", c.mm_width, c.mm_height, c.count_modes, list);
    struct drm_mode_get_encoder_ e = { c.encoder_id ? c.encoder_id : encs[0], 0, 0, 0, 0 };
    if (xioctl(g_fd, IOC_GETENCODER, &e) == 0) { g_enc = e.encoder_id; }

    uint32_t planes[8];
    struct drm_mode_get_plane_res_ pr = { (uintptr_t)planes, 8 };
    char plist[300] = {0};
    size_t P = 0;
    char infmt[300] = "absent";
    if (xioctl(g_fd, IOC_GETPLANERESOURCES, &pr) == 0) {
        for (uint32_t i = 0; i < pr.count_planes && i < 8; i++) {
            uint32_t fmts[16];
            struct drm_mode_get_plane_ gp = {0};
            gp.plane_id = planes[i]; gp.format_type_ptr = (uintptr_t)fmts; gp.count_format_types = 16;
            if (xioctl(g_fd, IOC_GETPLANE, &gp) < 0) continue;
            uint64_t type = 99;
            find_prop(planes[i], OBJ_PLANE, "type", &type);
            if (type == 1 && !g_plane) g_plane = planes[i];
            P += snprintf(plist + P, sizeof plist - P, "%s%u(%s, %u fmt)", i ? " " : "", planes[i],
                          type == 1 ? "primary" : type == 2 ? "cursor" : "overlay", gp.count_format_types);
            uint64_t blob = 0;
            if (type == 1 && find_prop(planes[i], OBJ_PLANE, "IN_FORMATS", &blob) && blob) {
                static uint8_t b[4096];
                struct drm_mode_get_blob_ gb = { (uint32_t)blob, sizeof b, (uintptr_t)b };
                if (xioctl(g_fd, IOC_GETPROPBLOB, &gb) == 0 && gb.length >= 24 && gb.length <= sizeof b) {
                    uint32_t *h = (uint32_t *)b;
                    size_t Q = 0;
                    for (uint32_t m2 = 0; m2 < h[4]; m2++) {
                        uint8_t *mm2 = b + h[5] + m2 * 24;
                        uint64_t fmask, mod;
                        memcpy(&fmask, mm2, 8);
                        memcpy(&mod, mm2 + 16, 8);
                        for (uint32_t f = 0; f < h[2] && f < 64; f++) {
                            if (!(fmask >> f & 1)) continue;
                            uint32_t fc;
                            memcpy(&fc, b + h[3] + f * 4, 4);
                            Q += snprintf(infmt + Q, sizeof infmt - Q, "%s%.4s/%s", Q ? " " : "", (char *)&fc,
                                          mod == 0 ? "LINEAR" : "0x?");
                            if (Q >= sizeof infmt - 16) break;
                        }
                    }
                    if (!Q) snprintf(infmt, sizeof infmt, "empty");
                }
            }
        }
    }
    out("nvkmstest probe: planes %s; IN_FORMATS %s", plist, infmt);
    if (fb0_info() == 0)
        out("nvkmstest probe: /dev/fb0 %dx%d pitch %d (scanout read-back)", g_fb0_w, g_fb0_h, g_fb0_pitch);
    result("probe", c.connection == 1 ? "PASS" : "FAIL", "'%s', %ux%u mode, %u plane(s), master %s", name, g_w, g_h,
           pr.count_planes, m == 0 ? "ok" : "refused");
}

/* ─────────────────────────── dumb ─────────────────────────── */

struct cpu_ctx { uint32_t *px[2]; uint32_t pitch_words; };

static void draw_cpu_cb(int idx, unsigned frame, void *ctx)
{
    struct cpu_ctx *c = ctx;
    draw_cpu(c->px[idx], c->pitch_words, frame);
}

static void test_dumb(void)
{
    if (!g_have_mode) { result("dumb", "SKIP", "no mode"); return; }
    struct drm_mode_create_dumb_ cd[2];
    uint32_t fbs[2] = {0, 0};
    struct cpu_ctx ctx = {{0}, 0};
    size_t size = 0;
    char why[200] = "";
    int ok = -1;
    memset(cd, 0, sizeof cd);
    for (int i = 0; i < 2; i++) {
        cd[i].width = g_w; cd[i].height = g_h; cd[i].bpp = 32;
        if (xioctl(g_fd, IOC_CREATE_DUMB, &cd[i]) < 0) { snprintf(why, sizeof why, "CREATE_DUMB: %s", strerror(errno)); goto done; }
        struct drm_mode_map_dumb_ md = { cd[i].handle, 0, 0 };
        if (xioctl(g_fd, IOC_MAP_DUMB, &md) < 0) { snprintf(why, sizeof why, "MAP_DUMB: %s", strerror(errno)); goto done; }
        size = cd[i].size;
        void *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, g_fd, md.offset);
        if (p == MAP_FAILED) { snprintf(why, sizeof why, "mmap dumb: %s", strerror(errno)); goto done; }
        ctx.px[i] = p;
        ctx.pitch_words = cd[i].pitch / 4;
        if (addfb(cd[i].handle, cd[i].pitch, 0, 0, &fbs[i]) < 0) { snprintf(why, sizeof why, "ADDFB2 dumb: %s", strerror(errno)); goto done; }
    }
    double rate = 0;
    ok = flip_loop("dumb", fbs, draw_cpu_cb, &ctx, &rate, why, sizeof why);
    if (ok == 0) verify_and_report("dumb", rate, fbs, 2);
done:
    if (ok != 0) result("dumb", "FAIL", "%s", why);
    for (int i = 0; i < 2; i++) {
        rmfb(fbs[i]);
        if (ctx.px[i]) munmap(ctx.px[i], size);
        if (cd[i].handle) { struct drm_mode_destroy_dumb_ dd = { cd[i].handle }; xioctl(g_fd, IOC_DESTROY_DUMB, &dd); }
    }
}

/* ─────────────────────────── gem (nouveau sysmem) ─────────────────────────── */

static void test_gem(void)
{
    if (strcmp(g_driver, "nouveau")) { result("gem", "SKIP", "driver is '%s', not nouveau", g_driver); return; }
    if (!g_have_mode) { result("gem", "SKIP", "no mode"); return; }
    struct nv_gem_new_ g[2];
    uint32_t fbs[2] = {0, 0};
    struct cpu_ctx ctx = {{0}, 0};
    uint32_t pitch = (g_w * 4 + 255) & ~255u;
    uint64_t size = ((uint64_t)pitch * g_h + 0xffff) & ~0xffffull;
    char why[200] = "";
    int ok = -1;
    memset(g, 0, sizeof g);
    for (int i = 0; i < 2; i++) {
        g[i].info.size = size;
        g[i].info.domain = NV_DOMAIN_GART | NV_DOMAIN_MAPPABLE;
        g[i].align = 0x1000;
        if (xioctl(g_fd, IOC_NV_GEM_NEW, &g[i]) < 0) { snprintf(why, sizeof why, "nouveau GEM_NEW %" PRIu64 " KiB: %s", size >> 10, strerror(errno)); goto done; }
        void *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, g_fd, g[i].info.map_handle);
        if (p == MAP_FAILED) { snprintf(why, sizeof why, "mmap GEM: %s", strerror(errno)); goto done; }
        ctx.px[i] = p;
        ctx.pitch_words = pitch / 4;
        if (addfb(g[i].info.handle, pitch, 0, 1, &fbs[i]) < 0) { snprintf(why, sizeof why, "ADDFB2 GEM %#x LINEAR: %s", g[i].info.handle, strerror(errno)); goto done; }
    }
    out("nvkmstest gem: 2 GEM buffers (sysmem, %" PRIu64 " KiB, pitch %u) -> fbs %u %u", size >> 10, pitch, fbs[0], fbs[1]);
    double rate = 0;
    ok = flip_loop("gem", fbs, draw_cpu_cb, &ctx, &rate, why, sizeof why);
    if (ok == 0) verify_and_report("gem", rate, fbs, 2);
done:
    if (ok != 0) result("gem", "FAIL", "%s", why);
    for (int i = 0; i < 2; i++) {
        rmfb(fbs[i]);
        if (ctx.px[i]) munmap(ctx.px[i], size);
        if (g[i].info.handle) { struct drm_gem_close_ gc = { g[i].info.handle, 0 }; xioctl(g_fd, IOC_GEM_CLOSE, &gc); }
    }
}

/* ─────────────────────────── vk (NVK-rendered) ─────────────────────────── */

static PFN_vkGetInstanceProcAddr gipa;
#define VKF(name) static PFN_##name p##name
VKF(vkCreateInstance); VKF(vkEnumeratePhysicalDevices); VKF(vkGetPhysicalDeviceProperties);
VKF(vkGetPhysicalDeviceMemoryProperties); VKF(vkGetPhysicalDeviceQueueFamilyProperties); VKF(vkCreateDevice);
VKF(vkGetDeviceProcAddr); VKF(vkGetDeviceQueue); VKF(vkCreateBuffer); VKF(vkGetBufferMemoryRequirements);
VKF(vkAllocateMemory); VKF(vkBindBufferMemory); VKF(vkGetMemoryFdKHR); VKF(vkCreateCommandPool);
VKF(vkAllocateCommandBuffers); VKF(vkBeginCommandBuffer); VKF(vkEndCommandBuffer); VKF(vkCmdFillBuffer);
VKF(vkQueueSubmit); VKF(vkCreateFence); VKF(vkWaitForFences); VKF(vkResetFences); VKF(vkResetCommandBuffer);
VKF(vkCmdPipelineBarrier);

struct vk_ctx {
    VkDevice dev; VkQueue q; VkCommandBuffer cb; VkFence fence;
    VkBuffer buf[2]; uint32_t pitch; uint64_t size; int failed; char err[160];
};

static void draw_vk_cb(int idx, unsigned frame, void *vctx)
{
    struct vk_ctx *c = vctx;
    if (c->failed) return;
    uint32_t bh = g_h / 12, by = bar_y(frame, g_h);
    pvkResetCommandBuffer(c->cb, 0);
    VkCommandBufferBeginInfo bi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, NULL, VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT, NULL };
    pvkBeginCommandBuffer(c->cb, &bi);
    VkBuffer b = c->buf[idx];
    pvkCmdFillBuffer(c->cb, b, 0, (uint64_t)c->pitch * g_h, bg_color(frame));
    VkMemoryBarrier mb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, NULL, VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_TRANSFER_WRITE_BIT };
    pvkCmdPipelineBarrier(c->cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
    pvkCmdFillBuffer(c->cb, b, 0, (uint64_t)c->pitch * BAND_ROWS, band_color(frame));
    pvkCmdFillBuffer(c->cb, b, (uint64_t)c->pitch * by, (uint64_t)c->pitch * bh, BAR_COLOR);
    pvkEndCommandBuffer(c->cb);
    VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 0, NULL, NULL, 1, &c->cb, 0, NULL };
    pvkResetFences(c->dev, 1, &c->fence);
    VkResult r = pvkQueueSubmit(c->q, 1, &si, c->fence);
    if (r == VK_SUCCESS) r = pvkWaitForFences(c->dev, 1, &c->fence, VK_TRUE, 2000000000ull);
    if (r != VK_SUCCESS) { c->failed = 1; snprintf(c->err, sizeof c->err, "frame %u: submit/wait VkResult %d", frame, r); }
}

static void test_vk(void)
{
    if (g_no_vk) { result("vk", "SKIP", "--no-vk"); return; }
    if (strcmp(g_driver, "nouveau")) { result("vk", "SKIP", "driver is '%s', not nouveau", g_driver); return; }
    void *so = dlopen(g_icd, RTLD_NOW | RTLD_LOCAL);
    if (!so) { result("vk", "SKIP", "no ICD %s (%s)", g_icd, dlerror()); return; }
    gipa = (PFN_vkGetInstanceProcAddr)dlsym(so, "vk_icdGetInstanceProcAddr");
    if (!gipa) { result("vk", "SKIP", "ICD has no vk_icdGetInstanceProcAddr"); return; }
    char why[200] = "";
    uint32_t fbs[2] = {0, 0}, handles[2] = {0, 0};
    int fds[2] = {-1, -1};
    struct vk_ctx c;
    memset(&c, 0, sizeof c);
    int ok = -1;
#define GI(inst, n) p##n = (PFN_##n)gipa(inst, #n)
    GI(NULL, vkCreateInstance);
    VkApplicationInfo ai = { VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL, "nvkmstest", 1, NULL, 0, VK_API_VERSION_1_3 };
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL, 0, &ai, 0, NULL, 0, NULL };
    VkInstance inst;
    VkResult r = pvkCreateInstance(&ici, NULL, &inst);
    if (r != VK_SUCCESS) { snprintf(why, sizeof why, "vkCreateInstance: %d", r); goto done; }
    GI(inst, vkEnumeratePhysicalDevices); GI(inst, vkGetPhysicalDeviceProperties); GI(inst, vkGetPhysicalDeviceMemoryProperties);
    GI(inst, vkGetPhysicalDeviceQueueFamilyProperties); GI(inst, vkCreateDevice); GI(inst, vkGetDeviceProcAddr);
    uint32_t n = 1;
    VkPhysicalDevice pd;
    r = pvkEnumeratePhysicalDevices(inst, &n, &pd);
    if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || n == 0) { result("vk", "SKIP", "NVK enumerates no device (%d)", r); return; }
    VkPhysicalDeviceProperties props;
    pvkGetPhysicalDeviceProperties(pd, &props);
    const char *exts[] = { VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME, VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME };
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, NULL, 0, 0, 1, &prio };
    VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, NULL, 0, 1, &qci, 0, NULL, 2, exts, NULL };
    r = pvkCreateDevice(pd, &dci, NULL, &c.dev);
    if (r != VK_SUCCESS) { snprintf(why, sizeof why, "vkCreateDevice (external memory dma-buf): %d", r); goto done; }
#define GD(n) p##n = (PFN_##n)pvkGetDeviceProcAddr(c.dev, #n)
    GD(vkGetDeviceQueue); GD(vkCreateBuffer); GD(vkGetBufferMemoryRequirements); GD(vkAllocateMemory); GD(vkBindBufferMemory);
    GD(vkGetMemoryFdKHR); GD(vkCreateCommandPool); GD(vkAllocateCommandBuffers); GD(vkBeginCommandBuffer);
    GD(vkEndCommandBuffer); GD(vkCmdFillBuffer); GD(vkQueueSubmit); GD(vkCreateFence); GD(vkWaitForFences);
    GD(vkResetFences); GD(vkResetCommandBuffer); GD(vkCmdPipelineBarrier);
    pvkGetDeviceQueue(c.dev, 0, 0, &c.q);
    VkPhysicalDeviceMemoryProperties mp;
    pvkGetPhysicalDeviceMemoryProperties(pd, &mp);
    c.pitch = (g_w * 4 + 255) & ~255u;
    c.size = (uint64_t)c.pitch * g_h;
    const char *where = "?";
    for (int i = 0; i < 2; i++) {
        VkExternalMemoryBufferCreateInfo eb = { VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO, NULL, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
        VkBufferCreateInfo bci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, &eb, 0, c.size, VK_BUFFER_USAGE_TRANSFER_DST_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT, VK_SHARING_MODE_EXCLUSIVE, 0, NULL };
        r = pvkCreateBuffer(c.dev, &bci, NULL, &c.buf[i]);
        if (r != VK_SUCCESS) { snprintf(why, sizeof why, "vkCreateBuffer: %d", r); goto done; }
        VkMemoryRequirements req;
        pvkGetBufferMemoryRequirements(c.dev, c.buf[i], &req);
        int type = -1;
        for (int pass = 0; pass < 2 && type < 0; pass++)
            for (uint32_t t = 0; t < mp.memoryTypeCount; t++) {
                VkMemoryPropertyFlags f = mp.memoryTypes[t].propertyFlags;
                if (!(req.memoryTypeBits & (1u << t))) continue;
                if (pass == 0 && (!(f & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) || (f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT))) continue;
                type = (int)t;
                where = (f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) ? "host-visible" : "VRAM";
                break;
            }
        if (type < 0) { snprintf(why, sizeof why, "no memory type for an exportable buffer"); goto done; }
        VkExportMemoryAllocateInfo ex = { VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO, NULL, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
        VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, &ex, req.size, (uint32_t)type };
        VkDeviceMemory mem;
        r = pvkAllocateMemory(c.dev, &mai, NULL, &mem);
        if (r != VK_SUCCESS) { snprintf(why, sizeof why, "vkAllocateMemory (%s, export dma-buf): %d", where, r); goto done; }
        pvkBindBufferMemory(c.dev, c.buf[i], mem, 0);
        VkMemoryGetFdInfoKHR gfi = { VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR, NULL, mem, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
        r = pvkGetMemoryFdKHR(c.dev, &gfi, &fds[i]);
        if (r != VK_SUCCESS) { snprintf(why, sizeof why, "vkGetMemoryFdKHR(dma-buf): %d", r); goto done; }
        struct drm_prime_handle_ ph = { 0, 0, fds[i] };
        if (xioctl(g_fd, IOC_PRIME_FD_TO_HANDLE, &ph) < 0) { snprintf(why, sizeof why, "PRIME_FD_TO_HANDLE on card0: %s", strerror(errno)); goto done; }
        handles[i] = ph.handle;
        if (addfb(handles[i], c.pitch, 0, 1, &fbs[i]) < 0) { snprintf(why, sizeof why, "ADDFB2 of the NVK buffer (handle %#x, LINEAR): %s", handles[i], strerror(errno)); goto done; }
    }
    VkCommandPoolCreateInfo cpi = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, NULL, VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, 0 };
    VkCommandPool pool;
    pvkCreateCommandPool(c.dev, &cpi, NULL, &pool);
    VkCommandBufferAllocateInfo cai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, NULL, pool, VK_COMMAND_BUFFER_LEVEL_PRIMARY, 1 };
    pvkAllocateCommandBuffers(c.dev, &cai, &c.cb);
    VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, NULL, 0 };
    pvkCreateFence(c.dev, &fci, NULL, &c.fence);
    out("nvkmstest vk: \"%s\": 2 exportable %s buffers (pitch %u), dma-buf -> card0 handles %#x %#x -> fbs %u %u", props.deviceName,
        where, c.pitch, handles[0], handles[1], fbs[0], fbs[1]);
    double rate = 0;
    ok = flip_loop("vk", fbs, draw_vk_cb, &c, &rate, why, sizeof why);
    if (ok == 0 && c.failed) { ok = -1; snprintf(why, sizeof why, "%s", c.err); }
    if (ok == 0) verify_and_report("vk", rate, fbs, 2);
done:
    if (ok != 0) result("vk", "FAIL", "%s", why);
    for (int i = 0; i < 2; i++) {
        rmfb(fbs[i]);
        if (handles[i]) { struct drm_gem_close_ gc = { handles[i], 0 }; xioctl(g_fd, IOC_GEM_CLOSE, &gc); }
        if (fds[i] >= 0) close(fds[i]);
    }
    /* The Vulkan device is left to process exit (no teardown after a failure). */
}

/* ─────────────────────────── atomic ─────────────────────────── */

static void test_atomic(void)
{
    if (!g_have_mode || !g_plane) { result("atomic", "SKIP", "no mode or no primary plane"); return; }
    struct drm_set_client_cap_ cc = { CLIENT_CAP_ATOMIC, 1 };
    if (xioctl(g_fd, IOC_SET_CLIENT_CAP, &cc) < 0) { result("atomic", "SKIP", "SET_CLIENT_CAP(ATOMIC): %s", strerror(errno)); return; }
    uint32_t p_fb = find_prop(g_plane, OBJ_PLANE, "FB_ID", NULL), p_pcrtc = find_prop(g_plane, OBJ_PLANE, "CRTC_ID", NULL);
    uint32_t p_sx = find_prop(g_plane, OBJ_PLANE, "SRC_X", NULL), p_sy = find_prop(g_plane, OBJ_PLANE, "SRC_Y", NULL);
    uint32_t p_sw = find_prop(g_plane, OBJ_PLANE, "SRC_W", NULL), p_sh = find_prop(g_plane, OBJ_PLANE, "SRC_H", NULL);
    uint32_t p_cx = find_prop(g_plane, OBJ_PLANE, "CRTC_X", NULL), p_cy = find_prop(g_plane, OBJ_PLANE, "CRTC_Y", NULL);
    uint32_t p_cw = find_prop(g_plane, OBJ_PLANE, "CRTC_W", NULL), p_ch = find_prop(g_plane, OBJ_PLANE, "CRTC_H", NULL);
    uint32_t p_active = find_prop(g_crtc, OBJ_CRTC, "ACTIVE", NULL), p_mode = find_prop(g_crtc, OBJ_CRTC, "MODE_ID", NULL);
    uint32_t p_ccrtc = find_prop(g_conn, OBJ_CONNECTOR, "CRTC_ID", NULL);
    if (!p_fb || !p_pcrtc || !p_active || !p_mode || !p_ccrtc || !p_sw || !p_cw) {
        result("atomic", "FAIL", "missing properties (FB_ID %u CRTC_ID %u ACTIVE %u MODE_ID %u conn CRTC_ID %u)", p_fb, p_pcrtc, p_active, p_mode, p_ccrtc);
        return;
    }
    /* Buffers: GEM on nouveau (the copy-engine path), dumb otherwise. */
    int gem = !strcmp(g_driver, "nouveau");
    uint32_t handles[2] = {0, 0}, fbs[2] = {0, 0}, pitch = (g_w * 4 + 255) & ~255u;
    uint64_t size = ((uint64_t)pitch * g_h + 0xffff) & ~0xffffull;
    uint32_t *px[2] = {0, 0};
    char why[200] = "";
    int ok = -1;
    uint32_t mode_blob = 0;
    for (int i = 0; i < 2; i++) {
        uint64_t moff;
        if (gem) {
            struct nv_gem_new_ g = {0};
            g.info.size = size; g.info.domain = NV_DOMAIN_GART | NV_DOMAIN_MAPPABLE; g.align = 0x1000;
            if (xioctl(g_fd, IOC_NV_GEM_NEW, &g) < 0) { snprintf(why, sizeof why, "GEM_NEW: %s", strerror(errno)); goto done; }
            handles[i] = g.info.handle; moff = g.info.map_handle;
        } else {
            struct drm_mode_create_dumb_ cd = {0};
            cd.width = g_w; cd.height = g_h; cd.bpp = 32;
            if (xioctl(g_fd, IOC_CREATE_DUMB, &cd) < 0) { snprintf(why, sizeof why, "CREATE_DUMB: %s", strerror(errno)); goto done; }
            struct drm_mode_map_dumb_ md = { cd.handle, 0, 0 };
            xioctl(g_fd, IOC_MAP_DUMB, &md);
            handles[i] = cd.handle; pitch = cd.pitch; size = cd.size; moff = md.offset;
        }
        void *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, g_fd, moff);
        if (p == MAP_FAILED) { snprintf(why, sizeof why, "mmap: %s", strerror(errno)); goto done; }
        px[i] = p;
        if (addfb(handles[i], pitch, 0, gem, &fbs[i]) < 0) { snprintf(why, sizeof why, "ADDFB2: %s", strerror(errno)); goto done; }
    }
    struct drm_mode_create_blob_ cb = { (uintptr_t)&g_mode, sizeof g_mode, 0 };
    if (xioctl(g_fd, IOC_CREATEPROPBLOB, &cb) < 0) { snprintf(why, sizeof why, "CREATEPROPBLOB: %s", strerror(errno)); goto done; }
    mode_blob = cb.blob_id;

    /* A mode the connector does not list: TEST_ONLY must refuse it where
     * the scanout has one fixed mode (NVIDIA); elsewhere it may pass. */
    struct drm_mode_modeinfo_ bogus = g_mode;
    bogus.hdisplay = g_w > 640 ? 640 : g_w / 2; bogus.vdisplay = g_h > 480 ? 480 : g_h / 2;
    struct drm_mode_create_blob_ cb2 = { (uintptr_t)&bogus, sizeof bogus, 0 };
    int bogus_refused = -1;
    if (xioctl(g_fd, IOC_CREATEPROPBLOB, &cb2) == 0) {
        uint32_t objs[1] = { g_crtc }, cnt[1] = { 2 }, ids[2] = { p_active, p_mode };
        uint64_t vals[2] = { 1, cb2.blob_id };
        struct drm_mode_atomic_ a = { ATOMIC_TEST_ONLY | ATOMIC_ALLOW_MODESET, 1, (uintptr_t)objs, (uintptr_t)cnt, (uintptr_t)ids, (uintptr_t)vals, 0, 0 };
        bogus_refused = xioctl(g_fd, IOC_ATOMIC, &a) < 0;
        struct drm_mode_destroy_blob_ db = { cb2.blob_id };
        xioctl(g_fd, IOC_DESTROYPROPBLOB, &db);
    }

    unsigned frames = g_frames / 2, stale = 0;
    uint64_t t0 = 0;
    for (unsigned f = 0; f <= frames; f++) {
        int back = f & 1;
        draw_cpu(px[back], pitch / 4, 1000 + f);
        uint32_t objs[3], cnt[3], ids[16];
        uint64_t vals[16];
        uint32_t no = 0, k = 0;
        if (f == 0) {
            objs[no] = g_conn; cnt[no++] = 1; ids[k] = p_ccrtc; vals[k++] = g_crtc;
            objs[no] = g_crtc; cnt[no++] = 2; ids[k] = p_active; vals[k++] = 1; ids[k] = p_mode; vals[k++] = mode_blob;
            objs[no] = g_plane; cnt[no++] = 10;
            ids[k] = p_fb; vals[k++] = fbs[back]; ids[k] = p_pcrtc; vals[k++] = g_crtc;
            ids[k] = p_sx; vals[k++] = 0; ids[k] = p_sy; vals[k++] = 0;
            ids[k] = p_sw; vals[k++] = (uint64_t)g_w << 16; ids[k] = p_sh; vals[k++] = (uint64_t)g_h << 16;
            ids[k] = p_cx; vals[k++] = 0; ids[k] = p_cy; vals[k++] = 0; ids[k] = p_cw; vals[k++] = g_w; ids[k] = p_ch; vals[k++] = g_h;
        } else {
            objs[no] = g_plane; cnt[no++] = 1; ids[k] = p_fb; vals[k++] = fbs[back];
        }
        uint64_t ud = 0xa70d0000ull + f;
        struct drm_mode_atomic_ a = { (f == 0 ? ATOMIC_ALLOW_MODESET : 0) | PAGE_FLIP_EVENT, no, (uintptr_t)objs,
                                      (uintptr_t)cnt, (uintptr_t)ids, (uintptr_t)vals, 0, ud };
        if (xioctl(g_fd, IOC_ATOMIC, &a) < 0) { snprintf(why, sizeof why, "ATOMIC commit %u%s: %s", f, f == 0 ? " (modeset)" : "", strerror(errno)); goto done; }
        if (wait_flip(ud, &stale) < 0) { snprintf(why, sizeof why, "no event for atomic commit %u within 2 s", f); goto done; }
        g_last_color = band_color(1000 + f);
        if (f == 0) t0 = now_ns();
    }
    {
        double secs = (now_ns() - t0) / 1e9, rate = frames / secs;
        int total = 0, okv = verify_band(g_last_color, &total);
        const char *mode_note = bogus_refused == 1 ? "a non-listed mode is refused" : bogus_refused == 0 ? "a non-listed mode is accepted" : "mode test n/a";
        if (okv >= 0 && okv * 10 < total * 9)
            result("atomic", "FAIL", "%u commits %.1f/s, but the scanout does not show the last frame (%d/%d); %s", frames, rate, okv, total, mode_note);
        else
            result("atomic", "PASS", "%s buffers, modeset + %u flip commits with events, %.1f/s%s; %s", gem ? "GEM" : "dumb", frames, rate,
                   okv >= 0 ? ", scanout verified" : "", mode_note);
        ok = 0;
    }
done:
    if (ok != 0) result("atomic", "FAIL", "%s", why);
    if (mode_blob) { struct drm_mode_destroy_blob_ db = { mode_blob }; xioctl(g_fd, IOC_DESTROYPROPBLOB, &db); }
    for (int i = 0; i < 2; i++) {
        rmfb(fbs[i]);
        if (px[i]) munmap(px[i], size);
        if (handles[i]) { struct drm_gem_close_ gc = { handles[i], 0 }; xioctl(g_fd, IOC_GEM_CLOSE, &gc); }
    }
}

/* ─────────────────────────── console ─────────────────────────── */

static void test_console(void)
{
    uint32_t last = g_last_color;
    int total = 0, before = verify_band(last, &total);
    int d = xioctl(g_fd, IOC_DROP_MASTER, NULL);
    int de = errno;
    close(g_fd);
    g_fd = -1;
    out("nvkmstest console: DROP_MASTER %s, card0 closed; waiting for the text console", d == 0 ? "ok" : strerror(de));
    sleep_ms(1000);
    out("nvkmstest console: if you can read this line on the monitor, the text console is back");
    sleep_ms(300);
    int after = verify_band(last, &total);
    if (after < 0)
        result("console", "PASS", "DROP_MASTER %s; card0 closed (scanout read-back unavailable: check the screen)", d == 0 ? "ok" : "refused");
    else if (after * 10 <= total)
        result("console", "PASS", "DROP_MASTER %s; the last frame is gone from the scanout (%d -> %d of %d samples): console repainted",
               d == 0 ? "ok" : "refused", before, after, total);
    else
        result("console", "FAIL", "the scanout still shows the last frame %d/%d samples after DROP_MASTER + close", after, total);
}

/* ─────────────────────────── main ─────────────────────────── */

int main(int argc, char **argv)
{
    unsigned budget = 120;
    for (int i = 1; i < argc; i++) {
        if (!strncmp(argv[i], "--frames=", 9)) g_frames = (unsigned)atoi(argv[i] + 9);
        else if (!strncmp(argv[i], "--icd=", 6)) g_icd = argv[i] + 6;
        else if (!strcmp(argv[i], "--no-vk")) g_no_vk = 1;
        else if (!strncmp(argv[i], "--hold-ms=", 10)) g_hold_ms = (unsigned)atoi(argv[i] + 10);
        else if (!strncmp(argv[i], "--budget=", 9)) budget = (unsigned)atoi(argv[i] + 9);
        else { out("usage: nvkmstest [--frames=N] [--icd=PATH] [--no-vk] [--hold-ms=N] [--budget=S]"); return 2; }
    }
    if (g_frames < 2) g_frames = 2;
    setvbuf(stdout, NULL, _IOLBF, 0);
    signal(SIGALRM, on_alarm);
    alarm(budget);
    out("==== nvkmstest: start (frames %u) ====", g_frames);
    g_fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
    if (g_fd < 0) {
        result("probe", "FAIL", "open /dev/dri/card0: %s", strerror(errno));
        summary();
        return 1;
    }
    probe();
    test_dumb();
    test_gem();
    test_vk();
    test_atomic();
    sleep_ms(g_hold_ms);
    test_console();
    summary();
    return g_fail > 120 ? 120 : g_fail;
}
