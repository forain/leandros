/* nvgltest — OpenGL ES through GBM + EGL on the NVIDIA card, the way
 * cosmic-comp (smithay's GLES renderer on EGL/GBM over the DRM node) and
 * Firefox (EGL/GLES, dma-bufs) use it. On this OS that is Mesa's zink
 * (GL on Vulkan) on NVK on the nouveau UAPI layer over OpenRM.
 *
 * Subtests (each prints exactly one "nvgltest <name>: PASS|FAIL|SKIP|TIMEOUT"):
 *   info        DRM identity + caps, the loader's zink prediction
 *               (HAS_VMA_TILEMODE, chipset), GBM backend, EGL (eglinfo-style:
 *               vendor, version, APIs, extensions, EGL device node), a GLES
 *               3.x/2.0 surfaceless context and its GL_RENDERER / GL_VERSION /
 *               extensions (es2_info-style). On a nouveau node the renderer
 *               must be zink (never a software rasterizer).
 *   fbo         clear + red triangle into a 256x256 RGBA8 renderbuffer FBO,
 *               glReadPixels: named pixels, red area 18432 +/- 600, FNV-1a,
 *               ASCII art and a 32x32 PPM (same geometry as nvktest triangle).
 *   gbm_bo      the same triangle rendered into a LINEAR gbm_bo through an
 *               EGLImage (EGL_NATIVE_PIXMAP_KHR); glReadPixels and
 *               gbm_bo_map(READ) must both match the fbo image.
 *   scanout     cosmic-comp's scanout buffer on the NVIDIA card0:
 *               gbm_bo_create_with_modifiers2(XRGB8888, {LINEAR},
 *               SCANOUT|RENDERING), rendered and read back the same way.
 *   gbm_tiled   the same with gbm_bo_create_with_modifiers2() and the
 *               driver's own (block-linear) modifiers from
 *               eglQueryDmaBufModifiersEXT — what a compositor's scanout
 *               buffers are.
 *   prime       export the gbm_bo as a dma-buf (PRIME), re-import it twice:
 *               as an EGLImage (EGL_LINUX_DMA_BUF_EXT, + modifier) sampled by
 *               a full-screen quad into another FBO, and as a gbm_bo
 *               (GBM_BO_IMPORT_FD_MODIFIER) read with gbm_bo_map. Both must
 *               reproduce the image. Also fstat/lseek size and an mmap probe.
 *   fence       EGL_KHR_fence_sync wait; EGL_ANDROID_native_fence_sync:
 *               export a sync-file fd, poll it, re-import it and
 *               eglWaitSyncKHR; DMA_BUF_IOCTL_EXPORT_SYNC_FILE on the shared
 *               bo (implicit sync).
 *   throughput  500 frames of clear + 64 triangles at 1024x1024; ms/frame.
 *
 * Exit: number of failures (<= 120), 126 after a watchdog timeout, 3 when the
 * render node is not a nouveau device (all subtests SKIP; --any-driver runs
 * them anyway, e.g. on virgl in QEMU, to test this program).
 *
 * Usage: nvgltest [--node=/dev/dri/renderD128] [--any-driver]
 *                 [--watchdog-s=60] [--out-dir=/root] [--only=a,b]
 *
 * Built by ports/nvk-tests/build.sh against the GPU Mesa stage (EGL, GLESv2,
 * gbm); every Mesa call here is an upstream API, nothing LeandrOS-specific.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <setjmp.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <gbm.h>

/* ────────────────────────── constants ────────────────────────── */

#define IMG_W 256
#define IMG_H 256
#define IMG_BPP 4
#define IMG_BYTES (IMG_W * IMG_H * IMG_BPP)
#define TRI_AREA_PX 18432
#define TRI_AREA_TOL 600
#define NVKTEST_REF_FNV 0x02C0FDC5u /* nvktest triangle (lavapipe and NVK) */

#ifndef GBM_FORMAT_ABGR8888
#define GBM_FORMAT_ABGR8888 0x34324241
#endif
#ifndef DRM_FORMAT_MOD_INVALID
#define DRM_FORMAT_MOD_INVALID 0x00ffffffffffffffULL
#endif
#ifndef DRM_FORMAT_MOD_LINEAR
#define DRM_FORMAT_MOD_LINEAR 0ULL
#endif
#ifndef GL_NUM_EXTENSIONS
#define GL_NUM_EXTENSIONS 0x821D
#endif
#ifndef GL_MAJOR_VERSION
#define GL_MAJOR_VERSION 0x821B
#define GL_MINOR_VERSION 0x821C
#endif
#ifndef EGL_PLATFORM_GBM_KHR
#define EGL_PLATFORM_GBM_KHR 0x31D7
#endif

/* DRM / nouveau UAPI, enough for the preflight. */
struct drm_version_ {
    int version_major, version_minor, version_patchlevel;
    size_t name_len; char *name;
    size_t date_len; char *date;
    size_t desc_len; char *desc;
};
struct drm_get_cap_ { uint64_t capability, value; };
struct nouveau_getparam_ { uint64_t param, value; };
#define DRM_IOCTL_VERSION_      0xC0406400u
#define DRM_IOCTL_GET_CAP_      0xC010640Cu
#define DRM_IOCTL_NOUVEAU_GETPARAM_ 0xC0106440u
#define DRM_CAP_PRIME_          0x5
#define DRM_CAP_SYNCOBJ_        0x13
#define DRM_CAP_SYNCOBJ_TIMELINE_ 0x14
#define NOUVEAU_GETPARAM_CHIPSET_ID 11
#define NOUVEAU_GETPARAM_HAS_VMA_TILEMODE 20

struct dma_buf_sync_file_ { uint32_t flags; int32_t fd; };
#define DMA_BUF_IOCTL_EXPORT_SYNC_FILE_ 0xC0086202u
#define DMA_BUF_SYNC_READ_  1u
#define DMA_BUF_SYNC_WRITE_ 2u

/* ────────────────────────── reporting ────────────────────────── */

static int g_pass, g_fail, g_skip, g_tmo;
static const char *volatile g_cur = "startup";
static const char *volatile g_step = "startup";
static unsigned g_watchdog_s = 60;
static const char *g_out_dir = "/root";
static const char *g_node = "/dev/dri/renderD128";
static int g_any_driver;
static int g_is_nouveau;
static char g_only[256];

#define RECAP_MAX 16
static char g_recap[RECAP_MAX][300];
static volatile int g_recap_n;

#define INFO(...) do { printf("[INFO] " __VA_ARGS__); printf("\n"); fflush(stdout); } while (0)
#define ERR(...)  do { printf("[ERR ] " __VA_ARGS__); printf("\n"); fflush(stdout); } while (0)

static void sig_puts(const char *s) { ssize_t r = write(1, s, strlen(s)); (void)r; }
static void sig_putu(unsigned v)
{
    char b[16]; int n = 0;
    do { b[n++] = (char)('0' + v % 10); v /= 10; } while (v && n < 15);
    char o[16]; for (int i = 0; i < n; i++) o[i] = b[n - 1 - i]; o[n] = 0;
    sig_puts(o);
}
static void sig_done(int fail, int tmo)
{
    int n = g_recap_n;
    if (n > 0) {
        sig_puts("---- nvgltest summary ----\n");
        for (int i = 0; i < n && i < RECAP_MAX; i++) { sig_puts("  "); sig_puts(g_recap[i]); sig_puts("\n"); }
    }
    sig_puts("==== nvgltest done: "); sig_putu((unsigned)g_pass);
    sig_puts(" passed, "); sig_putu((unsigned)fail);
    sig_puts(" failed, "); sig_putu((unsigned)g_skip);
    sig_puts(" skipped, "); sig_putu((unsigned)tmo);
    sig_puts(" timeout ====\n");
}
static void on_alarm(int sig)
{
    (void)sig;
    sig_puts("\nnvgltest "); sig_puts(g_cur);
    sig_puts(": TIMEOUT (watchdog: no progress for "); sig_putu(g_watchdog_s);
    sig_puts(" s in step '"); sig_puts(g_step); sig_puts("')\n");
    sig_done(g_fail, g_tmo + 1);
    _exit(126);
}
static void on_fatal(int sig)
{
    const char *n = sig == SIGSEGV ? "SIGSEGV" : sig == SIGBUS ? "SIGBUS" :
                    sig == SIGILL ? "SIGILL" : sig == SIGFPE ? "SIGFPE" :
                    sig == SIGABRT ? "SIGABRT" : "signal";
    sig_puts("\nnvgltest "); sig_puts(g_cur);
    sig_puts(": FAIL (crashed with "); sig_puts(n);
    sig_puts(" in step '"); sig_puts(g_step); sig_puts("')\n");
    sig_done(g_fail + 1, g_tmo);
    int rc = g_fail + 1;
    _exit(rc > 120 ? 120 : rc);
}
static void install_signals(void)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = on_alarm;
    sigaction(SIGALRM, &sa, NULL);
    sa.sa_handler = on_fatal;
    sa.sa_flags = (int)SA_RESETHAND;
    sigaction(SIGSEGV, &sa, NULL);
    sigaction(SIGBUS, &sa, NULL);
    sigaction(SIGILL, &sa, NULL);
    sigaction(SIGFPE, &sa, NULL);
    sigaction(SIGABRT, &sa, NULL);
}

static void result(const char *name, const char *verdict, const char *fmt, ...)
{
    char why[256];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(why, sizeof(why), fmt, ap);
    va_end(ap);
    char line[sizeof(g_recap[0])];
    snprintf(line, sizeof(line), "nvgltest %s: %s (%s)", name, verdict, why);
    printf("%s\n", line);
    fflush(stdout);
    if (g_recap_n < RECAP_MAX) {
        snprintf(g_recap[g_recap_n], sizeof(g_recap[0]), "%s", line);
        g_recap_n++;
    }
    if (!strcmp(verdict, "PASS")) g_pass++;
    else if (!strcmp(verdict, "SKIP")) g_skip++;
    else g_fail++;
}

/* FNV-1a of a 256x256 RGBA image held in GL row order (row 0 = bottom),
 * hashed top-down like nvktest's Vulkan readback, so the two checksums are
 * directly comparable. */
static uint32_t img_fnv(const uint8_t *px)
{
    uint32_t h = 2166136261u;
    for (int y = IMG_H - 1; y >= 0; y--) {
        const uint8_t *r = px + (size_t)y * IMG_W * IMG_BPP;
        for (int i = 0; i < IMG_W * IMG_BPP; i++) { h ^= r[i]; h *= 16777619u; }
    }
    return h;
}

static uint64_t now_us(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000ull + (uint64_t)ts.tv_nsec / 1000ull;
}

/* Print a long space-separated list wrapped at ~100 columns. */
static void print_wrapped(const char *tag, const char *list)
{
    if (!list) { printf("%s (null)\n", tag); return; }
    char line[128];
    size_t n = 0;
    line[0] = 0;
    const char *p = list;
    while (*p) {
        while (*p == ' ') p++;
        if (!*p) break;
        const char *e = p;
        while (*e && *e != ' ') e++;
        size_t w = (size_t)(e - p);
        if (n && n + 1 + w > 100) { printf("%s %s\n", tag, line); n = 0; line[0] = 0; }
        if (w > 100) w = 100;
        if (n) line[n++] = ' ';
        memcpy(line + n, p, w); n += w; line[n] = 0;
        p = e;
    }
    if (n) printf("%s %s\n", tag, line);
    fflush(stdout);
}

static int has_ext(const char *list, const char *ext)
{
    if (!list) return 0;
    size_t l = strlen(ext);
    const char *p = list;
    while ((p = strstr(p, ext))) {
        if ((p == list || p[-1] == ' ') && (p[l] == 0 || p[l] == ' ')) return 1;
        p += l;
    }
    return 0;
}

/* ────────────────────────── global GL state ────────────────────────── */

static int g_fd = -1;
static struct gbm_device *g_gbm;
static EGLDisplay g_dpy = EGL_NO_DISPLAY;
static EGLContext g_ctx = EGL_NO_CONTEXT;
static const char *g_egl_exts;
static const char *g_gl_exts_es2;   /* glGetString(GL_EXTENSIONS) */
static int g_es_major;
static GLuint g_prog_tri, g_prog_tex;
static uint8_t g_ref[IMG_BYTES];    /* fbo image, GL row order (bottom-up) */
static uint32_t g_ref_fnv;
static int g_have_ref;

static PFNEGLCREATEIMAGEKHRPROC p_eglCreateImageKHR;
static PFNEGLDESTROYIMAGEKHRPROC p_eglDestroyImageKHR;
static PFNGLEGLIMAGETARGETRENDERBUFFERSTORAGEOESPROC p_glEGLImageTargetRenderbufferStorageOES;
static PFNGLEGLIMAGETARGETTEXTURE2DOESPROC p_glEGLImageTargetTexture2DOES;
static PFNEGLQUERYDMABUFMODIFIERSEXTPROC p_eglQueryDmaBufModifiersEXT;
static PFNEGLCREATESYNCKHRPROC p_eglCreateSyncKHR;
static PFNEGLDESTROYSYNCKHRPROC p_eglDestroySyncKHR;
static PFNEGLCLIENTWAITSYNCKHRPROC p_eglClientWaitSyncKHR;
static PFNEGLWAITSYNCKHRPROC p_eglWaitSyncKHR;
static PFNEGLDUPNATIVEFENCEFDANDROIDPROC p_eglDupNativeFenceFDANDROID;
static PFNEGLQUERYDISPLAYATTRIBEXTPROC p_eglQueryDisplayAttribEXT;
static PFNEGLQUERYDEVICESTRINGEXTPROC p_eglQueryDeviceStringEXT;
typedef const GLubyte *(GL_APIENTRYP PFNGLGETSTRINGIPROC_)(GLenum, GLuint);
static PFNGLGETSTRINGIPROC_ p_glGetStringi;

static const char *gl_err_str(GLenum e)
{
    switch (e) {
    case GL_NO_ERROR: return "GL_NO_ERROR";
    case GL_INVALID_ENUM: return "GL_INVALID_ENUM";
    case GL_INVALID_VALUE: return "GL_INVALID_VALUE";
    case GL_INVALID_OPERATION: return "GL_INVALID_OPERATION";
    case GL_OUT_OF_MEMORY: return "GL_OUT_OF_MEMORY";
    case GL_INVALID_FRAMEBUFFER_OPERATION: return "GL_INVALID_FRAMEBUFFER_OPERATION";
    default: return "GL_?";
    }
}

static GLenum gl_check(const char *what)
{
    GLenum e = glGetError(), first = e;
    while (e != GL_NO_ERROR) {
        ERR("%s: %s (0x%x)", what, gl_err_str(e), e);
        e = glGetError();
    }
    return first;
}

/* ────────────────────────── shaders and drawing ────────────────────────── */

static const char *VS_TRI =
    "attribute vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.5, 1.0); }\n";
static const char *FS_TRI =
    "precision mediump float;\n"
    "uniform vec4 color;\n"
    "void main() { gl_FragColor = color; }\n";
static const char *VS_TEX =
    "attribute vec2 pos;\n"
    "varying vec2 uv;\n"
    "void main() { uv = pos * 0.5 + 0.5; gl_Position = vec4(pos, 0.0, 1.0); }\n";
static const char *FS_TEX =
    "precision mediump float;\n"
    "uniform sampler2D tex;\n"
    "varying vec2 uv;\n"
    "void main() { gl_FragColor = texture2D(tex, uv); }\n";

static GLuint compile(GLenum type, const char *src)
{
    GLuint s = glCreateShader(type);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[512] = {0};
        glGetShaderInfoLog(s, sizeof(log) - 1, NULL, log);
        ERR("shader compile failed: %s", log);
        glDeleteShader(s);
        return 0;
    }
    return s;
}

static GLuint link_prog(const char *vs, const char *fs)
{
    GLuint v = compile(GL_VERTEX_SHADER, vs), f = compile(GL_FRAGMENT_SHADER, fs);
    if (!v || !f) return 0;
    GLuint p = glCreateProgram();
    glAttachShader(p, v);
    glAttachShader(p, f);
    glBindAttribLocation(p, 0, "pos");
    glLinkProgram(p);
    GLint ok = 0;
    glGetProgramiv(p, GL_LINK_STATUS, &ok);
    glDeleteShader(v);
    glDeleteShader(f);
    if (!ok) {
        char log[512] = {0};
        glGetProgramInfoLog(p, sizeof(log) - 1, NULL, log);
        ERR("program link failed: %s", log);
        glDeleteProgram(p);
        return 0;
    }
    return p;
}

/* nvktest's triangle (Vulkan NDC, y down) mirrored into GL NDC (y up), so the
 * image read back and flipped to top-down rows is the same picture. */
static const GLfloat TRI[] = { 0.0f, 0.75f, 0.75f, -0.75f, -0.75f, -0.75f };
static const GLfloat QUAD[] = { -1, -1, 1, -1, -1, 1, 1, 1 };

/* Clear to blue, draw the red triangle into the bound framebuffer. */
static void draw_triangle(int w, int h)
{
    glViewport(0, 0, w, h);
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_CULL_FACE);
    glDisable(GL_BLEND);
    glClearColor(0.0f, 0.0f, 1.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(g_prog_tri);
    glUniform4f(glGetUniformLocation(g_prog_tri, "color"), 1.0f, 0.0f, 0.0f, 1.0f);
    glBindBuffer(GL_ARRAY_BUFFER, 0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, TRI);
    glEnableVertexAttribArray(0);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glDisableVertexAttribArray(0);
}

static void draw_textured_quad(GLuint tex, int w, int h)
{
    glViewport(0, 0, w, h);
    glClearColor(0.0f, 1.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(g_prog_tex);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, tex);
    glUniform1i(glGetUniformLocation(g_prog_tex, "tex"), 0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, QUAD);
    glEnableVertexAttribArray(0);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glDisableVertexAttribArray(0);
}

static const uint8_t RED[4] = { 0xFF, 0x00, 0x00, 0xFF };
static const uint8_t BLUE[4] = { 0x00, 0x00, 0xFF, 0xFF };

/* px in GL row order (row 0 = bottom). y here is top-down like nvktest. */
static const uint8_t *pix(const uint8_t *px, int x, int y)
{
    return px + ((size_t)(IMG_H - 1 - y) * IMG_W + (size_t)x) * IMG_BPP;
}

static char classify(const uint8_t *p)
{
    if (!memcmp(p, RED, 4)) return '#';
    if (!memcmp(p, BLUE, 4)) return '.';
    return '?';
}

static void print_ascii(const char *tag, const uint8_t *px)
{
    printf("nvgltest %s| +--------------------------------+  (32x16 samples of 256x256; # red, . clear, ? other)\n", tag);
    for (int r = 0; r < 16; r++) {
        char line[33];
        for (int c = 0; c < 32; c++) line[c] = classify(pix(px, c * 8 + 4, r * 16 + 8));
        line[32] = 0;
        printf("nvgltest %s| |%s|\n", tag, line);
    }
    printf("nvgltest %s| +--------------------------------+\n", tag);
    fflush(stdout);
}

static void print_ppm32(const uint8_t *px)
{
    static const char b64[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    uint8_t buf[16 + 32 * 32 * 3];
    int hl = snprintf((char *)buf, 16, "P6\n32 32\n255\n");
    size_t n = (size_t)hl;
    for (int y = 0; y < 32; y++)
        for (int x = 0; x < 32; x++) {
            const uint8_t *p = pix(px, x * 8 + 4, y * 8 + 4);
            buf[n++] = p[0]; buf[n++] = p[1]; buf[n++] = p[2];
        }
    char out[((sizeof(buf) + 2) / 3) * 4 + 1];
    size_t o = 0;
    for (size_t i = 0; i < n; i += 3) {
        uint32_t v = (uint32_t)buf[i] << 16;
        if (i + 1 < n) v |= (uint32_t)buf[i + 1] << 8;
        if (i + 2 < n) v |= buf[i + 2];
        out[o++] = b64[(v >> 18) & 63];
        out[o++] = b64[(v >> 12) & 63];
        out[o++] = i + 1 < n ? b64[(v >> 6) & 63] : '=';
        out[o++] = i + 2 < n ? b64[v & 63] : '=';
    }
    out[o] = 0;
    printf("nvgltest ppm32 begin\n");
    for (size_t i = 0; i < o; i += 76)
        printf("nvgltest ppm32| %.*s\n", (int)(o - i < 76 ? o - i : 76), out + i);
    printf("nvgltest ppm32 end\n");
    fflush(stdout);
}

static void write_ppm(const char *name, const uint8_t *px)
{
    char path[512];
    snprintf(path, sizeof(path), "%s/%s", g_out_dir, name);
    FILE *f = fopen(path, "wb");
    if (!f) return;
    fprintf(f, "P6\n%d %d\n255\n", IMG_W, IMG_H);
    for (int y = 0; y < IMG_H; y++)
        for (int x = 0; x < IMG_W; x++) fwrite(pix(px, x, y), 1, 3, f);
    fclose(f);
    INFO("wrote %s", path);
}

/* Check the triangle image; returns 0 when right, fills why. */
static int check_triangle(const uint8_t *px, uint32_t *sum, char *why, size_t whylen)
{
    static const struct { int x, y; const uint8_t *want; const char *label; } pts[] = {
        { 0, 0, BLUE, "corner_top_left" }, { 255, 0, BLUE, "corner_top_right" },
        { 0, 255, BLUE, "corner_bottom_left" }, { 255, 255, BLUE, "corner_bottom_right" },
        { 128, 10, BLUE, "above_apex" }, { 20, 100, BLUE, "left_of_edge" },
        { 240, 100, BLUE, "right_of_edge" }, { 10, 240, BLUE, "below_base_left" },
        { 245, 240, BLUE, "below_base_right" },
        { 128, 128, RED, "centre" }, { 128, 60, RED, "near_apex" },
        { 60, 210, RED, "near_base_left" }, { 196, 210, RED, "near_base_right" },
    };
    int bad = 0;
    for (size_t i = 0; i < sizeof(pts) / sizeof(pts[0]); i++) {
        const uint8_t *p = pix(px, pts[i].x, pts[i].y);
        if (memcmp(p, pts[i].want, 4)) {
            bad++;
            ERR("pixel %s (%d,%d) = %02x%02x%02x%02x", pts[i].label, pts[i].x, pts[i].y, p[0], p[1], p[2], p[3]);
        }
    }
    uint32_t red = 0, blue = 0, other = 0;
    for (int i = 0; i < IMG_W * IMG_H; i++) {
        const uint8_t *p = px + (size_t)i * 4;
        if (!memcmp(p, RED, 4)) red++;
        else if (!memcmp(p, BLUE, 4)) blue++;
        else other++;
    }
    *sum = img_fnv(px);
    INFO("coverage: red=%u clear=%u other=%u (analytic %d +/- %d), FNV-1a 0x%08X", red, blue, other,
         TRI_AREA_PX, TRI_AREA_TOL, *sum);
    int area_ok = red + TRI_AREA_TOL >= TRI_AREA_PX && red <= TRI_AREA_PX + TRI_AREA_TOL;
    snprintf(why, whylen, "red=%u px, other=%u, %d/13 named pixels wrong, FNV-1a 0x%08X", red, other, bad, *sum);
    return (bad == 0 && area_ok && other == 0) ? 0 : -1;
}

/* Read the bound framebuffer (GL row order). */
static int read_fb(uint8_t *px, int w, int h)
{
    memset(px, 0x5A, (size_t)w * h * 4);
    glPixelStorei(GL_PACK_ALIGNMENT, 4);
    glReadPixels(0, 0, w, h, GL_RGBA, GL_UNSIGNED_BYTE, px);
    return gl_check("glReadPixels") == GL_NO_ERROR ? 0 : -1;
}

/* FBO with a renderbuffer; the renderbuffer is either RGBA8 storage or an
 * EGLImage. */
struct fbo { GLuint fb, rb, tex; };

static int fbo_begin(struct fbo *f, EGLImageKHR img, int w, int h)
{
    memset(f, 0, sizeof(*f));
    glGenFramebuffers(1, &f->fb);
    glBindFramebuffer(GL_FRAMEBUFFER, f->fb);
    glGenRenderbuffers(1, &f->rb);
    glBindRenderbuffer(GL_RENDERBUFFER, f->rb);
    if (img != EGL_NO_IMAGE_KHR) {
        p_glEGLImageTargetRenderbufferStorageOES(GL_RENDERBUFFER, (GLeglImageOES)img);
    } else {
        GLenum fmt = g_es_major >= 3 ? 0x8058 /* GL_RGBA8 */ :
                     (has_ext(g_gl_exts_es2, "GL_OES_rgb8_rgba8") ? 0x8058 : GL_RGBA4);
        glRenderbufferStorage(GL_RENDERBUFFER, fmt, w, h);
    }
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, f->rb);
    GLenum st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    if (gl_check("fbo setup") != GL_NO_ERROR || st != GL_FRAMEBUFFER_COMPLETE) {
        ERR("framebuffer incomplete: 0x%x", st);
        return -1;
    }
    return 0;
}

static void fbo_end(struct fbo *f)
{
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    if (f->rb) glDeleteRenderbuffers(1, &f->rb);
    if (f->fb) glDeleteFramebuffers(1, &f->fb);
    if (f->tex) glDeleteTextures(1, &f->tex);
    memset(f, 0, sizeof(*f));
}

/* ────────────────────────── subtest: info ────────────────────────── */

static int is_software(const char *r)
{
    static const char *const sw[] = { "softpipe", "llvmpipe", "swrast", "lavapipe", "Software Rasterizer" };
    for (size_t i = 0; i < sizeof(sw) / sizeof(sw[0]); i++)
        if (r && strstr(r, sw[i])) return 1;
    return 0;
}

static int drm_cap(int fd, uint64_t cap, uint64_t *v)
{
    struct drm_get_cap_ c = { cap, 0 };
    if (ioctl(fd, DRM_IOCTL_GET_CAP_, &c) != 0) return -errno;
    *v = c.value;
    return 0;
}

static int nouveau_param(int fd, uint64_t param, uint64_t *v)
{
    struct nouveau_getparam_ g = { param, 0 };
    if (ioctl(fd, DRM_IOCTL_NOUVEAU_GETPARAM_, &g) != 0) return -errno;
    *v = g.value;
    return 0;
}

#define LOADPROC(var, name) var = (__typeof__(var))eglGetProcAddress(name)

static int t_info(void)
{
    char why[200];
    g_step = "open";
    g_fd = open(g_node, O_RDWR | O_CLOEXEC);
    if (g_fd < 0) {
        result("info", g_any_driver ? "FAIL" : "SKIP", "cannot open %s: %s", g_node, strerror(errno));
        return g_any_driver ? -1 : -2;
    }
    g_step = "DRM_IOCTL_VERSION";
    char name[64] = {0}, date[64] = {0}, desc[128] = {0};
    struct drm_version_ v;
    memset(&v, 0, sizeof(v));
    v.name = name; v.name_len = sizeof(name) - 1;
    v.date = date; v.date_len = sizeof(date) - 1;
    v.desc = desc; v.desc_len = sizeof(desc) - 1;
    if (ioctl(g_fd, DRM_IOCTL_VERSION_, &v) != 0) { name[0] = 0; }
    INFO("node %s: driver '%s' %d.%d.%d (%s, %s)", g_node, name, v.version_major, v.version_minor,
         v.version_patchlevel, date, desc);
    uint64_t c_prime = 0, c_so = 0, c_tl = 0;
    drm_cap(g_fd, DRM_CAP_PRIME_, &c_prime);
    drm_cap(g_fd, DRM_CAP_SYNCOBJ_, &c_so);
    drm_cap(g_fd, DRM_CAP_SYNCOBJ_TIMELINE_, &c_tl);
    INFO("caps: PRIME=0x%llx SYNCOBJ=%llu SYNCOBJ_TIMELINE=%llu", (unsigned long long)c_prime,
         (unsigned long long)c_so, (unsigned long long)c_tl);
    g_is_nouveau = !strcmp(name, "nouveau");
    if (g_is_nouveau) {
        uint64_t tile = 0, chip = 0;
        int rt = nouveau_param(g_fd, NOUVEAU_GETPARAM_HAS_VMA_TILEMODE, &tile);
        int rc = nouveau_param(g_fd, NOUVEAU_GETPARAM_CHIPSET_ID, &chip);
        INFO("nouveau: HAS_VMA_TILEMODE=%llu (rc %d), CHIPSET_ID=0x%llx (rc %d) -> Mesa's loader picks %s",
             (unsigned long long)tile, rt, (unsigned long long)chip, rc,
             (rt == 0 && tile == 1 && rc == 0 && chip >= 0x160) ? "zink (nouveau_zink_predicate)" :
             "nouveau GL (not built here; GBM/EGL then fall back to an inferred zink)");
    } else if (!g_any_driver) {
        result("info", "SKIP", "render node driver is '%s', not nouveau: no NVIDIA GPU here", name);
        return -2;
    }
    const char *env[] = { "MESA_LOADER_DRIVER_OVERRIDE", "GALLIUM_DRIVER", "NOUVEAU_USE_ZINK",
                          "VK_ICD_FILENAMES", "LIBGL_ALWAYS_SOFTWARE", "MESA_SHADER_CACHE_DISABLE" };
    for (size_t i = 0; i < sizeof(env) / sizeof(env[0]); i++)
        printf("[ENV ] %s=%s\n", env[i], getenv(env[i]) ? getenv(env[i]) : "(unset)");

    g_step = "gbm_create_device";
    g_gbm = gbm_create_device(g_fd);
    if (!g_gbm) { result("info", "FAIL", "gbm_create_device(%s) failed", g_node); return -1; }
    INFO("GBM backend: %s", gbm_device_get_backend_name(g_gbm));

    g_step = "eglGetPlatformDisplay";
    const char *cext = eglQueryString(EGL_NO_DISPLAY, EGL_EXTENSIONS);
    print_wrapped("[EGL ] client extensions:", cext);
    PFNEGLGETPLATFORMDISPLAYEXTPROC gpd;
    LOADPROC(gpd, "eglGetPlatformDisplayEXT");
    g_dpy = gpd ? gpd(EGL_PLATFORM_GBM_KHR, g_gbm, NULL) : eglGetDisplay((EGLNativeDisplayType)g_gbm);
    EGLint maj = 0, min = 0;
    g_step = "eglInitialize";
    if (g_dpy == EGL_NO_DISPLAY || !eglInitialize(g_dpy, &maj, &min)) {
        result("info", "FAIL", "eglInitialize on GBM: 0x%x", eglGetError());
        return -1;
    }
    g_egl_exts = eglQueryString(g_dpy, EGL_EXTENSIONS);
    printf("[EGL ] EGL %d.%d vendor '%s' version '%s'\n", maj, min, eglQueryString(g_dpy, EGL_VENDOR),
           eglQueryString(g_dpy, EGL_VERSION));
    printf("[EGL ] client APIs: %s\n", eglQueryString(g_dpy, EGL_CLIENT_APIS));
    print_wrapped("[EGL ] display extensions:", g_egl_exts);

    LOADPROC(p_eglQueryDisplayAttribEXT, "eglQueryDisplayAttribEXT");
    LOADPROC(p_eglQueryDeviceStringEXT, "eglQueryDeviceStringEXT");
    if (p_eglQueryDisplayAttribEXT && p_eglQueryDeviceStringEXT) {
        EGLAttrib dev = 0;
        if (p_eglQueryDisplayAttribEXT(g_dpy, EGL_DEVICE_EXT, &dev) && dev) {
            EGLDeviceEXT d = (EGLDeviceEXT)dev;
            const char *dext = p_eglQueryDeviceStringEXT(d, EGL_EXTENSIONS);
            printf("[EGL ] device extensions: %s\n", dext ? dext : "(none)");
#ifdef EGL_DRM_DEVICE_FILE_EXT
            if (has_ext(dext, "EGL_EXT_device_drm"))
                printf("[EGL ] device DRM node: %s\n", p_eglQueryDeviceStringEXT(d, EGL_DRM_DEVICE_FILE_EXT));
#endif
#ifdef EGL_DRM_RENDER_NODE_FILE_EXT
            if (has_ext(dext, "EGL_EXT_device_drm_render_node"))
                printf("[EGL ] device render node: %s\n", p_eglQueryDeviceStringEXT(d, EGL_DRM_RENDER_NODE_FILE_EXT));
#endif
        }
    }

    g_step = "eglCreateContext";
    if (!eglBindAPI(EGL_OPENGL_ES_API)) { result("info", "FAIL", "eglBindAPI(GLES): 0x%x", eglGetError()); return -1; }
    if (!has_ext(g_egl_exts, "EGL_KHR_no_config_context") && !has_ext(g_egl_exts, "EGL_MESA_configless_context"))
        ERR("EGL has no no-config context extension (smithay needs it)");
    static const EGLint vers[][2] = { { 3, 2 }, { 3, 0 }, { 2, 0 } };
    for (size_t i = 0; i < 3 && g_ctx == EGL_NO_CONTEXT; i++) {
        EGLint a[] = { EGL_CONTEXT_MAJOR_VERSION, vers[i][0], EGL_CONTEXT_MINOR_VERSION, vers[i][1], EGL_NONE };
        g_ctx = eglCreateContext(g_dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, a);
        if (g_ctx == EGL_NO_CONTEXT) INFO("GLES %d.%d context: 0x%x", vers[i][0], vers[i][1], eglGetError());
    }
    if (g_ctx == EGL_NO_CONTEXT) { result("info", "FAIL", "no GLES context could be created"); return -1; }
    g_step = "eglMakeCurrent";
    if (!eglMakeCurrent(g_dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g_ctx)) {
        result("info", "FAIL", "eglMakeCurrent(surfaceless): 0x%x", eglGetError());
        return -1;
    }
    const char *vendor = (const char *)glGetString(GL_VENDOR);
    const char *renderer = (const char *)glGetString(GL_RENDERER);
    const char *version = (const char *)glGetString(GL_VERSION);
    const char *slv = (const char *)glGetString(GL_SHADING_LANGUAGE_VERSION);
    printf("[GL  ] GL_VENDOR: %s\n[GL  ] GL_RENDERER: %s\n[GL  ] GL_VERSION: %s\n[GL  ] GL_SHADING_LANGUAGE_VERSION: %s\n",
           vendor ? vendor : "(null)", renderer ? renderer : "(null)", version ? version : "(null)", slv ? slv : "(null)");
    int esmaj = 0, esmin = 0;
    if (version) sscanf(version, "OpenGL ES %d.%d", &esmaj, &esmin);
    g_es_major = esmaj;
    g_gl_exts_es2 = (const char *)glGetString(GL_EXTENSIONS);
    GLint next = 0;
    if (esmaj >= 3) {
        glGetIntegerv(GL_NUM_EXTENSIONS, &next);
        LOADPROC(p_glGetStringi, "glGetStringi");
    }
    print_wrapped("[GL  ] extensions:", g_gl_exts_es2);
    GLint maxtex = 0, maxrb = 0;
    glGetIntegerv(GL_MAX_TEXTURE_SIZE, &maxtex);
    glGetIntegerv(GL_MAX_RENDERBUFFER_SIZE, &maxrb);
    INFO("GL_MAX_TEXTURE_SIZE=%d GL_MAX_RENDERBUFFER_SIZE=%d GL_NUM_EXTENSIONS=%d", maxtex, maxrb, next);
    while (glGetError() != GL_NO_ERROR) {}

    LOADPROC(p_eglCreateImageKHR, "eglCreateImageKHR");
    LOADPROC(p_eglDestroyImageKHR, "eglDestroyImageKHR");
    LOADPROC(p_glEGLImageTargetRenderbufferStorageOES, "glEGLImageTargetRenderbufferStorageOES");
    LOADPROC(p_glEGLImageTargetTexture2DOES, "glEGLImageTargetTexture2DOES");
    LOADPROC(p_eglQueryDmaBufModifiersEXT, "eglQueryDmaBufModifiersEXT");
    LOADPROC(p_eglCreateSyncKHR, "eglCreateSyncKHR");
    LOADPROC(p_eglDestroySyncKHR, "eglDestroySyncKHR");
    LOADPROC(p_eglClientWaitSyncKHR, "eglClientWaitSyncKHR");
    LOADPROC(p_eglWaitSyncKHR, "eglWaitSyncKHR");
    LOADPROC(p_eglDupNativeFenceFDANDROID, "eglDupNativeFenceFDANDROID");

    g_step = "shaders";
    g_prog_tri = link_prog(VS_TRI, FS_TRI);
    g_prog_tex = link_prog(VS_TEX, FS_TEX);
    if (!g_prog_tri || !g_prog_tex) { result("info", "FAIL", "shader programs failed to build"); return -1; }

    char rshort[96];
    snprintf(rshort, sizeof(rshort), "%s", renderer ? renderer : "(null)");
    if (!renderer || is_software(renderer)) {
        result("info", "FAIL", "software renderer '%s': GPU rendering only", rshort);
        return -1;
    }
    if (g_is_nouveau && !strstr(renderer, "zink")) {
        result("info", "FAIL", "renderer '%s' on nouveau is not zink", rshort);
        return -1;
    }
    snprintf(why, sizeof(why), "GL_RENDERER=\"%s\", OpenGL ES %d.%d", rshort, esmaj, esmin);
    if (g_is_nouveau && !strstr(renderer, "NVK")) INFO("note: renderer string has no 'NVK'");
    result("info", "PASS", "%s", why);
    return 0;
}

/* ────────────────────────── subtest: fbo ────────────────────────── */

static int t_fbo(void)
{
    struct fbo f;
    char why[200];
    g_step = "fbo setup";
    if (fbo_begin(&f, EGL_NO_IMAGE_KHR, IMG_W, IMG_H)) { fbo_end(&f); result("fbo", "FAIL", "RGBA8 FBO incomplete"); return -1; }
    g_step = "draw";
    uint64_t t0 = now_us();
    draw_triangle(IMG_W, IMG_H);
    glFinish();
    uint64_t t1 = now_us();
    g_step = "glReadPixels";
    int rr = read_fb(g_ref, IMG_W, IMG_H);
    fbo_end(&f);
    if (rr) { result("fbo", "FAIL", "glReadPixels failed"); return -1; }
    uint32_t sum = 0;
    int bad = check_triangle(g_ref, &sum, why, sizeof(why));
    print_ascii("img", g_ref);
    print_ppm32(g_ref);
    write_ppm("nvgltest-triangle.ppm", g_ref);
    g_ref_fnv = sum;
    g_have_ref = 1;
    INFO("first draw + glFinish: %.2f ms; %s nvktest's reference 0x%08X", (double)(t1 - t0) / 1000.0,
         sum == NVKTEST_REF_FNV ? "IDENTICAL to" : "differs from", NVKTEST_REF_FNV);
    result("fbo", bad ? "FAIL" : "PASS", "%s", why);
    return bad;
}

/* ────────────────────────── gbm_bo helpers ────────────────────────── */

static int compare_map(struct gbm_bo *bo, const char *what)
{
    uint32_t stride = 0;
    void *map_data = NULL;
    g_step = "gbm_bo_map";
    void *p = gbm_bo_map(bo, 0, 0, IMG_W, IMG_H, GBM_BO_TRANSFER_READ, &stride, &map_data);
    if (!p) { ERR("%s: gbm_bo_map(READ) failed: %s", what, strerror(errno)); return -1; }
    /* XRGB8888 / ARGB8888 are B,G,R,X in memory; the reference is R,G,B,A. */
    uint32_t fmt = gbm_bo_get_format(bo);
    int bgrx = fmt == GBM_FORMAT_XRGB8888 || fmt == GBM_FORMAT_ARGB8888;
    int diff_rows = 0, first = -1;
    for (int y = 0; y < IMG_H; y++) {
        const uint8_t *m = (const uint8_t *)p + (size_t)y * stride;
        const uint8_t *r = g_ref + (size_t)y * IMG_W * 4;
        int bad;
        if (!bgrx) {
            bad = memcmp(m, r, IMG_W * 4) != 0;
        } else {
            bad = 0;
            for (int x = 0; x < IMG_W && !bad; x++)
                bad = m[4 * x] != r[4 * x + 2] || m[4 * x + 1] != r[4 * x + 1] || m[4 * x + 2] != r[4 * x];
        }
        if (bad) {
            if (first < 0) first = y;
            diff_rows++;
        }
    }
    gbm_bo_unmap(bo, map_data);
    INFO("%s: gbm_bo_map stride %u: %d/%d rows differ from the fbo image%s", what, stride, diff_rows, IMG_H,
         first >= 0 ? " (first listed below)" : "");
    if (first >= 0) ERR("%s: first differing row %d", what, first);
    return diff_rows ? -1 : 0;
}

static void describe_bo(const char *what, struct gbm_bo *bo)
{
    uint64_t mod = gbm_bo_get_modifier(bo);
    INFO("%s: %ux%u format 0x%08x modifier 0x%016llx planes %d stride %u offset %u handle %u", what,
         gbm_bo_get_width(bo), gbm_bo_get_height(bo), gbm_bo_get_format(bo), (unsigned long long)mod,
         gbm_bo_get_plane_count(bo), gbm_bo_get_stride(bo), gbm_bo_get_offset(bo, 0), gbm_bo_get_handle(bo).u32);
}

/* Render the triangle into bo through an EGLImage; returns 0 when the image
 * matches the fbo reference (glReadPixels and gbm_bo_map). */
static int render_into_bo(const char *name, struct gbm_bo *bo, char *why, size_t whylen)
{
    g_step = "eglCreateImage(NATIVE_PIXMAP)";
    EGLImageKHR img = p_eglCreateImageKHR(g_dpy, EGL_NO_CONTEXT, EGL_NATIVE_PIXMAP_KHR, (EGLClientBuffer)bo, NULL);
    if (img == EGL_NO_IMAGE_KHR) { snprintf(why, whylen, "eglCreateImageKHR(NATIVE_PIXMAP): 0x%x", eglGetError()); return -1; }
    struct fbo f;
    int rc = -1;
    if (fbo_begin(&f, img, IMG_W, IMG_H) == 0) {
        g_step = "draw into bo";
        draw_triangle(IMG_W, IMG_H);
        glFinish();
        static uint8_t px[IMG_BYTES];
        g_step = "glReadPixels(bo)";
        if (read_fb(px, IMG_W, IMG_H) == 0) {
            uint32_t sum = img_fnv(px);
            int same = sum == g_ref_fnv;
            int mapped = compare_map(bo, name);
            snprintf(why, whylen, "glReadPixels FNV-1a 0x%08X %s fbo; gbm_bo_map %s", sum,
                     same ? "==" : "!=", mapped == 0 ? "matches" : "DIFFERS/failed");
            rc = (same && mapped == 0) ? 0 : -1;
        } else {
            snprintf(why, whylen, "glReadPixels from the bo failed");
        }
    } else {
        snprintf(why, whylen, "FBO on the bo's EGLImage incomplete");
    }
    fbo_end(&f);
    p_eglDestroyImageKHR(g_dpy, img);
    return rc;
}

static struct gbm_bo *g_bo;     /* the linear bo, kept for prime/fence */

static int need_ref(const char *name)
{
    if (g_have_ref && p_eglCreateImageKHR && p_glEGLImageTargetRenderbufferStorageOES) return 0;
    result(name, "SKIP", g_have_ref ? "EGLImage entry points missing" : "no fbo reference image");
    return -1;
}

static int t_gbm_bo(void)
{
    char why[200];
    if (need_ref("gbm_bo")) return 0;
    g_step = "gbm_bo_create";
    g_bo = gbm_bo_create(g_gbm, IMG_W, IMG_H, GBM_FORMAT_ABGR8888, GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);
    if (!g_bo) { result("gbm_bo", "FAIL", "gbm_bo_create(LINEAR|RENDERING) failed: %s", strerror(errno)); return -1; }
    describe_bo("gbm_bo", g_bo);
    int rc = render_into_bo("gbm_bo", g_bo, why, sizeof(why));
    result("gbm_bo", rc ? "FAIL" : "PASS", "%s", why);
    return rc;
}

/* What cosmic-comp's smithay DRM backend asks for on the NVIDIA card0: KMS
 * there advertises only XR24/AR24 x LINEAR in IN_FORMATS, so the scanout
 * buffers are gbm_bo_create_with_modifiers2(XRGB8888, {LINEAR},
 * SCANOUT | RENDERING). zink turns that into an NVK DRM-modifier image with
 * the LINEAR modifier, which NVK renders through a tiled shadow (it needs
 * VK_EXT_image_drm_format_modifier, i.e. HAS_VMA_TILEMODE=1). */
static int t_scanout(void)
{
    char why[200];
    if (need_ref("scanout")) return 0;
    const uint64_t lin[1] = { DRM_FORMAT_MOD_LINEAR };
    g_step = "gbm_bo_create_with_modifiers2(XRGB8888, LINEAR, SCANOUT)";
    struct gbm_bo *bo = gbm_bo_create_with_modifiers2(g_gbm, IMG_W, IMG_H, GBM_FORMAT_XRGB8888, lin, 1,
                                                      GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING);
    if (!bo) {
        result("scanout", "FAIL", "gbm_bo_create_with_modifiers2(XRGB8888, {LINEAR}, SCANOUT|RENDERING): %s", strerror(errno));
        return -1;
    }
    describe_bo("scanout", bo);
    uint64_t mod = gbm_bo_get_modifier(bo);
    int rc = render_into_bo("scanout", bo, why, sizeof(why));
    int fd = gbm_bo_get_fd(bo);
    char w2[240];
    snprintf(w2, sizeof(w2), "XRGB8888 modifier 0x%llx stride %u, dma-buf %s; %s", (unsigned long long)mod,
             gbm_bo_get_stride(bo), fd >= 0 ? "ok" : "FAILED", why);
    if (fd >= 0) close(fd); else rc = -1;
    if (mod != DRM_FORMAT_MOD_LINEAR) rc = -1;
    gbm_bo_destroy(bo);
    result("scanout", rc ? "FAIL" : "PASS", "%s", w2);
    return rc;
}

static int t_gbm_tiled(void)
{
    char why[200];
    if (need_ref("gbm_tiled")) return 0;
    if (!p_eglQueryDmaBufModifiersEXT || !has_ext(g_egl_exts, "EGL_EXT_image_dma_buf_import_modifiers")) {
        result("gbm_tiled", "SKIP", "EGL_EXT_image_dma_buf_import_modifiers not advertised");
        return 0;
    }
    EGLuint64KHR mods[64];
    EGLBoolean ext_only[64];
    EGLint n = 0;
    g_step = "eglQueryDmaBufModifiersEXT";
    if (!p_eglQueryDmaBufModifiersEXT(g_dpy, GBM_FORMAT_ABGR8888, 64, mods, ext_only, &n) || n <= 0) {
        result("gbm_tiled", "SKIP", "no modifiers for ABGR8888 (n=%d, err 0x%x)", n, eglGetError());
        return 0;
    }
    uint64_t use[64];
    int nu = 0;
    for (int i = 0; i < n; i++) {
        INFO("modifier[%d] = 0x%016llx%s", i, (unsigned long long)mods[i], ext_only[i] ? " (external only)" : "");
        if (!ext_only[i] && mods[i] != DRM_FORMAT_MOD_LINEAR) use[nu++] = mods[i];
    }
    if (nu == 0) { result("gbm_tiled", "SKIP", "only linear / external-only modifiers"); return 0; }
    g_step = "gbm_bo_create_with_modifiers2";
    struct gbm_bo *bo = gbm_bo_create_with_modifiers2(g_gbm, IMG_W, IMG_H, GBM_FORMAT_ABGR8888, use, (unsigned)nu,
                                                      GBM_BO_USE_RENDERING);
    if (!bo) { result("gbm_tiled", "FAIL", "gbm_bo_create_with_modifiers2(%d modifiers): %s", nu, strerror(errno)); return -1; }
    describe_bo("gbm_tiled", bo);
    uint64_t mod = gbm_bo_get_modifier(bo);
    int rc = render_into_bo("gbm_tiled", bo, why, sizeof(why));
    char w2[220];
    snprintf(w2, sizeof(w2), "modifier 0x%llx; %s", (unsigned long long)mod, why);
    gbm_bo_destroy(bo);
    result("gbm_tiled", rc ? "FAIL" : "PASS", "%s", w2);
    return rc;
}

/* ────────────────────────── subtest: prime ────────────────────────── */

static int t_prime(void)
{
    if (!g_bo) { result("prime", "SKIP", "no gbm_bo (gbm_bo failed or was not run)"); return 0; }
    if (!p_glEGLImageTargetTexture2DOES || !has_ext(g_egl_exts, "EGL_EXT_image_dma_buf_import")) {
        result("prime", "SKIP", "EGL_EXT_image_dma_buf_import not advertised");
        return 0;
    }
    g_step = "gbm_bo_get_fd (PRIME export)";
    int fd = gbm_bo_get_fd(g_bo);
    if (fd < 0) { result("prime", "FAIL", "gbm_bo_get_fd (PRIME_HANDLE_TO_FD) failed: %s", strerror(errno)); return -1; }
    struct stat st;
    long long fsz = fstat(fd, &st) == 0 ? (long long)st.st_size : -1;
    off_t lsz = lseek(fd, 0, SEEK_END);
    lseek(fd, 0, SEEK_SET);
    uint32_t stride = gbm_bo_get_stride(g_bo);
    uint64_t mod = gbm_bo_get_modifier(g_bo);
    INFO("dma-buf fd %d: fstat size %lld, lseek(SEEK_END) %lld, stride %u, modifier 0x%llx", fd, fsz,
         (long long)lsz, stride, (unsigned long long)mod);
    void *mp = mmap(NULL, (size_t)stride * IMG_H, PROT_READ, MAP_SHARED, fd, 0);
    if (mp == MAP_FAILED) INFO("mmap of the dma-buf: failed (%s) - expected for a VRAM buffer (token export)", strerror(errno));
    else {
        int same = !memcmp(mp, g_ref, IMG_W * 4);
        INFO("mmap of the dma-buf: ok, first row %s the fbo image", same ? "matches" : "differs from");
        munmap(mp, (size_t)stride * IMG_H);
    }

    /* (a) EGLImage from the dma-buf, sampled into another FBO. */
    g_step = "eglCreateImage(LINUX_DMA_BUF)";
    EGLint attrs[32];
    int n = 0;
    attrs[n++] = EGL_WIDTH; attrs[n++] = IMG_W;
    attrs[n++] = EGL_HEIGHT; attrs[n++] = IMG_H;
    attrs[n++] = EGL_LINUX_DRM_FOURCC_EXT; attrs[n++] = GBM_FORMAT_ABGR8888;
    attrs[n++] = EGL_DMA_BUF_PLANE0_FD_EXT; attrs[n++] = fd;
    attrs[n++] = EGL_DMA_BUF_PLANE0_OFFSET_EXT; attrs[n++] = (EGLint)gbm_bo_get_offset(g_bo, 0);
    attrs[n++] = EGL_DMA_BUF_PLANE0_PITCH_EXT; attrs[n++] = (EGLint)stride;
    if (mod != DRM_FORMAT_MOD_INVALID && has_ext(g_egl_exts, "EGL_EXT_image_dma_buf_import_modifiers")) {
        attrs[n++] = EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT; attrs[n++] = (EGLint)(mod & 0xffffffffu);
        attrs[n++] = EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT; attrs[n++] = (EGLint)(mod >> 32);
    }
    attrs[n++] = EGL_NONE;
    EGLImageKHR img = p_eglCreateImageKHR(g_dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
    int egl_ok = 0;
    uint32_t sum = 0;
    if (img == EGL_NO_IMAGE_KHR) {
        ERR("eglCreateImageKHR(LINUX_DMA_BUF): 0x%x", eglGetError());
    } else {
        GLuint tex;
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        p_glEGLImageTargetTexture2DOES(GL_TEXTURE_2D, (GLeglImageOES)img);
        if (gl_check("glEGLImageTargetTexture2DOES") == GL_NO_ERROR) {
            struct fbo f;
            if (fbo_begin(&f, EGL_NO_IMAGE_KHR, IMG_W, IMG_H) == 0) {
                g_step = "sample the imported dma-buf";
                draw_textured_quad(tex, IMG_W, IMG_H);
                glFinish();
                static uint8_t px[IMG_BYTES];
                if (read_fb(px, IMG_W, IMG_H) == 0) {
                    sum = img_fnv(px);
                    egl_ok = sum == g_ref_fnv;
                    if (!egl_ok) print_ascii("prime", px);
                }
            }
            fbo_end(&f);
        }
        glDeleteTextures(1, &tex);
        p_eglDestroyImageKHR(g_dpy, img);
    }
    INFO("EGLImage from the dma-buf, sampled: FNV-1a 0x%08X (%s)", sum, egl_ok ? "matches" : "DIFFERS");

    /* (b) gbm_bo_import of the same fd, read with gbm_bo_map. */
    g_step = "gbm_bo_import(FD_MODIFIER)";
    struct gbm_import_fd_modifier_data d;
    memset(&d, 0, sizeof(d));
    d.width = IMG_W; d.height = IMG_H; d.format = GBM_FORMAT_ABGR8888;
    d.num_fds = 1; d.fds[0] = fd; d.strides[0] = (int)stride; d.offsets[0] = (int)gbm_bo_get_offset(g_bo, 0);
    d.modifier = mod;
    struct gbm_bo *imp = gbm_bo_import(g_gbm, GBM_BO_IMPORT_FD_MODIFIER, &d, GBM_BO_USE_RENDERING);
    int gbm_ok = 0;
    if (!imp) ERR("gbm_bo_import(FD_MODIFIER) failed: %s", strerror(errno));
    else {
        describe_bo("imported", imp);
        gbm_ok = compare_map(imp, "imported") == 0;
        gbm_bo_destroy(imp);
    }
    close(fd);
    int ok = egl_ok && gbm_ok;
    result("prime", ok ? "PASS" : "FAIL", "dma-buf %lld B; EGLImage re-import %s; gbm_bo_import+map %s",
           (long long)lsz, egl_ok ? "matches" : "FAILS", gbm_ok ? "matches" : "FAILS");
    return ok ? 0 : -1;
}

/* ────────────────────────── subtest: fence ────────────────────────── */

static int poll_fd(int fd, int ms)
{
    struct pollfd p = { .fd = fd, .events = POLLIN };
    int r = poll(&p, 1, ms);
    return r == 1 && (p.revents & POLLIN);
}

static int t_fence(void)
{
    if (!p_eglCreateSyncKHR || !p_eglClientWaitSyncKHR) { result("fence", "SKIP", "EGL_KHR_fence_sync entry points missing"); return 0; }
    int fails = 0;
    char why[220];
    why[0] = 0;
    struct fbo f;
    if (fbo_begin(&f, EGL_NO_IMAGE_KHR, IMG_W, IMG_H)) { fbo_end(&f); result("fence", "FAIL", "FBO incomplete"); return -1; }

    /* (a) EGL_KHR_fence_sync */
    g_step = "EGL_KHR_fence_sync";
    draw_triangle(IMG_W, IMG_H);
    EGLSyncKHR s = p_eglCreateSyncKHR(g_dpy, EGL_SYNC_FENCE_KHR, NULL);
    EGLint wr = s ? p_eglClientWaitSyncKHR(g_dpy, s, EGL_SYNC_FLUSH_COMMANDS_BIT_KHR, 5000000000ull) : 0;
    if (s) p_eglDestroySyncKHR(g_dpy, s);
    int fence_ok = wr == EGL_CONDITION_SATISFIED_KHR;
    if (!fence_ok) fails++;
    INFO("EGL_KHR_fence_sync: create %s, client wait -> 0x%x", s ? "ok" : "FAILED", wr);
    snprintf(why + strlen(why), sizeof(why) - strlen(why), "fence_sync %s", fence_ok ? "ok" : "FAIL");

    /* (b) EGL_ANDROID_native_fence_sync: sync file out, poll, back in. */
    g_step = "EGL_ANDROID_native_fence_sync";
    if (has_ext(g_egl_exts, "EGL_ANDROID_native_fence_sync") && p_eglDupNativeFenceFDANDROID && p_eglWaitSyncKHR) {
        draw_triangle(IMG_W, IMG_H);
        EGLint na[] = { EGL_SYNC_NATIVE_FENCE_FD_ANDROID, EGL_NO_NATIVE_FENCE_FD_ANDROID, EGL_NONE };
        EGLSyncKHR ns = p_eglCreateSyncKHR(g_dpy, EGL_SYNC_NATIVE_FENCE_ANDROID, na);
        glFlush();
        int sfd = ns ? p_eglDupNativeFenceFDANDROID(g_dpy, ns) : -1;
        uint64_t t0 = now_us();
        int ready_now = sfd >= 0 && poll_fd(sfd, 0);
        int ready = sfd >= 0 && (ready_now || poll_fd(sfd, 5000));
        uint64_t t1 = now_us();
        INFO("native fence: sync file fd %d, %s at export, readable after %.2f ms", sfd,
             ready_now ? "already signalled" : "pending", (double)(t1 - t0) / 1000.0);
        int imp_ok = 0;
        if (ready) {
            EGLint ia[] = { EGL_SYNC_NATIVE_FENCE_FD_ANDROID, dup(sfd), EGL_NONE };
            EGLSyncKHR is = p_eglCreateSyncKHR(g_dpy, EGL_SYNC_NATIVE_FENCE_ANDROID, ia);
            if (is) {
                imp_ok = p_eglWaitSyncKHR(g_dpy, is, 0) == EGL_TRUE;
                draw_triangle(IMG_W, IMG_H);
                glFinish();
                p_eglDestroySyncKHR(g_dpy, is);
            } else {
                ERR("eglCreateSyncKHR(NATIVE_FENCE, fd): 0x%x", eglGetError());
                close(ia[1]);
            }
        }
        if (ns) p_eglDestroySyncKHR(g_dpy, ns);
        if (sfd >= 0) close(sfd);
        int nf_ok = sfd >= 0 && ready && imp_ok;
        if (!nf_ok) fails++;
        snprintf(why + strlen(why), sizeof(why) - strlen(why), ", native_fence %s",
                 nf_ok ? "ok" : (sfd < 0 ? "EXPORT FAIL" : !ready ? "POLL FAIL" : "IMPORT FAIL"));
    } else {
        INFO("EGL_ANDROID_native_fence_sync not advertised");
        if (g_is_nouveau) fails++;
        snprintf(why + strlen(why), sizeof(why) - strlen(why), ", native_fence %s",
                 g_is_nouveau ? "MISSING" : "n/a");
    }
    fbo_end(&f);

    /* (c) implicit sync: the shared bo's fences as a sync file. */
    g_step = "DMA_BUF_IOCTL_EXPORT_SYNC_FILE";
    if (g_bo) {
        int dfd = gbm_bo_get_fd(g_bo);
        struct dma_buf_sync_file_ x = { DMA_BUF_SYNC_READ_ | DMA_BUF_SYNC_WRITE_, -1 };
        int r = dfd >= 0 ? ioctl(dfd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE_, &x) : -1;
        int e = errno;
        if (r == 0 && x.fd >= 0) {
            int ok = poll_fd(x.fd, 5000);
            INFO("dma-buf EXPORT_SYNC_FILE: fd %d, readable %s", x.fd, ok ? "yes" : "NO (5 s)");
            if (!ok) fails++;
            close(x.fd);
            snprintf(why + strlen(why), sizeof(why) - strlen(why), ", dmabuf sync_file %s", ok ? "ok" : "FAIL");
        } else {
            INFO("dma-buf EXPORT_SYNC_FILE: %s", strerror(e));
            if (g_is_nouveau) fails++;
            snprintf(why + strlen(why), sizeof(why) - strlen(why), ", dmabuf sync_file %s",
                     g_is_nouveau ? strerror(e) : "n/a");
        }
        if (dfd >= 0) close(dfd);
    }
    result("fence", fails ? "FAIL" : "PASS", "%s", why);
    return fails ? -1 : 0;
}

/* ────────────────────────── subtest: throughput ────────────────────────── */

static int t_throughput(void)
{
    const int W = 1024, H = 1024, FRAMES = 500, TRIS = 64;
    struct fbo f;
    memset(&f, 0, sizeof(f));
    glGenFramebuffers(1, &f.fb);
    glBindFramebuffer(GL_FRAMEBUFFER, f.fb);
    glGenRenderbuffers(1, &f.rb);
    glBindRenderbuffer(GL_RENDERBUFFER, f.rb);
    glRenderbufferStorage(GL_RENDERBUFFER, g_es_major >= 3 ? 0x8058 : GL_RGBA4, W, H);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, f.rb);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fbo_end(&f);
        result("throughput", "FAIL", "1024x1024 FBO incomplete");
        return -1;
    }
    g_step = "throughput frames";
    glUseProgram(g_prog_tri);
    GLint col = glGetUniformLocation(g_prog_tri, "color");
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, TRI);
    glEnableVertexAttribArray(0);
    glViewport(0, 0, W, H);
    uint64_t t0 = now_us();
    for (int i = 0; i < FRAMES; i++) {
        glClearColor((float)(i & 255) / 255.0f, 0.0f, 0.2f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        for (int t = 0; t < TRIS; t++) {
            glUniform4f(col, (float)t / TRIS, 1.0f - (float)t / TRIS, 0.5f, 1.0f);
            glDrawArrays(GL_TRIANGLES, 0, 3);
        }
        if ((i & 31) == 31) glFlush();
    }
    glFinish();
    uint64_t t1 = now_us();
    glDisableVertexAttribArray(0);
    GLenum e = gl_check("throughput");
    fbo_end(&f);
    double ms = (double)(t1 - t0) / 1000.0;
    result("throughput", e == GL_NO_ERROR ? "PASS" : "FAIL", "%d frames x %d tris at %dx%d in %.0f ms: %.2f ms/frame, %.0f fps",
           FRAMES, TRIS, W, H, ms, ms / FRAMES, FRAMES * 1000.0 / (ms > 0 ? ms : 1));
    return e == GL_NO_ERROR ? 0 : -1;
}

/* ────────────────────────── main ────────────────────────── */

static int selected(const char *name)
{
    if (!g_only[0]) return 1;
    char buf[sizeof(g_only)];
    snprintf(buf, sizeof(buf), "%s", g_only);
    for (char *t = strtok(buf, ","); t; t = strtok(NULL, ","))
        if (!strcmp(t, name)) return 1;
    return 0;
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    for (int i = 1; i < argc; i++) {
        const char *a = argv[i];
        if (!strncmp(a, "--node=", 7)) g_node = a + 7;
        else if (!strcmp(a, "--any-driver")) g_any_driver = 1;
        else if (!strncmp(a, "--watchdog-s=", 13)) g_watchdog_s = (unsigned)atoi(a + 13);
        else if (!strncmp(a, "--out-dir=", 10)) g_out_dir = a + 10;
        else if (!strncmp(a, "--only=", 7)) snprintf(g_only, sizeof(g_only), "%s", a + 7);
        else if (!strcmp(a, "-h") || !strcmp(a, "--help")) {
            printf("usage: nvgltest [--node=PATH] [--any-driver] [--watchdog-s=N] [--out-dir=DIR] [--only=a,b]\n"
                   "subtests: info fbo gbm_bo scanout gbm_tiled prime fence throughput\n");
            return 0;
        } else { fprintf(stderr, "nvgltest: unknown argument %s\n", a); return 2; }
    }
    install_signals();
    printf("==== nvgltest: OpenGL ES via GBM/EGL (zink on NVK) ====\n");
    INFO("node=%s watchdog=%u s out-dir=%s%s", g_node, g_watchdog_s, g_out_dir, g_any_driver ? " any-driver" : "");

    static const struct { const char *name; int (*fn)(void); } tests[] = {
        { "fbo", t_fbo }, { "gbm_bo", t_gbm_bo }, { "scanout", t_scanout }, { "gbm_tiled", t_gbm_tiled },
        { "prime", t_prime }, { "fence", t_fence }, { "throughput", t_throughput },
    };
    g_cur = "info";
    alarm(g_watchdog_s);
    int ir = t_info();
    alarm(0);
    for (size_t i = 0; i < sizeof(tests) / sizeof(tests[0]); i++) {
        if (ir != 0) {
            if (!selected(tests[i].name)) continue;
            if (ir == -2) result(tests[i].name, "SKIP", "no nouveau render node");
            else result(tests[i].name, "SKIP", "info failed");
            continue;
        }
        if (!selected(tests[i].name)) continue;
        g_cur = tests[i].name;
        g_step = "start";
        alarm(g_watchdog_s);
        tests[i].fn();
        alarm(0);
        while (glGetError() != GL_NO_ERROR) {}
    }
    if (g_bo) gbm_bo_destroy(g_bo);
    sig_done(g_fail, g_tmo);
    if (ir == -2) return 3;
    return g_fail > 120 ? 120 : g_fail;
}
