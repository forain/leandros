/*
 * i915gltest — deterministic 3D test for the real i915 (lane gpuctx).
 *
 * The laptop desktop showed nondeterministic partial 3D results (glyphs
 * missing per redraw, triangular holes) while one big textured quad was
 * fine. This draws what a text renderer draws: many small textured quads
 * from a glyph atlas that is rewritten every frame (glTexSubImage2D),
 * vertex data streamed from client memory (iris u_upload), a uniform that
 * changes per draw (push constants re-emitted), many draws per frame —
 * then glReadPixels and an exact compare with a CPU reference.
 *
 * Exactness: 8x8 quads on integer pixel positions, NEAREST sampling with
 * texcoords on texel edges, no blending: every covered pixel is one atlas
 * texel, so any mismatch is a GPU/driver/kernel bug, not rounding.
 *
 * Cases (each a separate process tree; PASS/FAIL per case):
 *   single  — one process
 *   dual    — two processes at once (forces rcs context switches)
 *   blit    — one process + a render-engine-heavy blit/clear process
 *   comp-*  — a smithay-like compositor + client process (see "comp" below)
 *
 * SKIP when /proc/cmdline has i915.fake (the fake soft CS has no 3D), when
 * there is no DRM node, or when the renderer is not Intel (iris).
 *
 * Usage: i915gltest [frames] [only-case|case-prefix]
 * I915GLTEST_ANY=1 runs on a non-Intel renderer too (test self-check).
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <gbm.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#define W 256
#define H 256
#define CELL 8
#define ATLAS 128                   /* 16x16 cells of 8x8 */
#define NCELL ((ATLAS / CELL) * (ATLAS / CELL))
#define GRID (W / CELL)             /* 32x32 quad positions */
#define DRAWS 32                    /* draw calls per frame */

static int g_frames = 60;

static uint32_t hash3(uint32_t a, uint32_t b, uint32_t c)
{
    uint32_t h = a * 0x9E3779B1u ^ (b + 0x7F4A7C15u) * 0x85EBCA77u ^ (c + 0x165667B1u) * 0xC2B2AE3Du;
    h ^= h >> 15; h *= 0x2C1B3C6Du; h ^= h >> 12; h *= 0x297A2D39u; h ^= h >> 15;
    return h;
}

/* Atlas texel (RGBA8) for frame f, seed s; alpha forced to 255. */
static uint32_t texel(int s, int f, int x, int y)
{
    return hash3((uint32_t)s * 1000003u + (uint32_t)f, (uint32_t)x, (uint32_t)y) | 0xFF000000u;
}

/* Is grid position (gx,gy) drawn in frame f, and with which cell? */
static int quad_cell(int s, int f, int gx, int gy)
{
    uint32_t h = hash3(0xABCD + s, (uint32_t)f, (uint32_t)(gy * GRID + gx));
    if ((h & 7) == 0) return -1;            /* ~1/8 positions left as background */
    return (int)((h >> 8) % NCELL);
}

static const char *VS =
    "attribute vec2 a_pos; attribute vec2 a_uv; varying vec2 v_uv;\n"
    "void main(){ v_uv = a_uv; gl_Position = vec4(a_pos, 0.0, 1.0); }\n";
static const char *FS =
    "precision mediump float; varying vec2 v_uv; uniform sampler2D u_tex; uniform float u_swz;\n"
    "void main(){ vec4 t = texture2D(u_tex, v_uv); gl_FragColor = u_swz > 0.5 ? t.bgra : t; }\n";

static GLuint shader(GLenum type, const char *src)
{
    GLuint s = glCreateShader(type);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[512]; glGetShaderInfoLog(s, sizeof log, NULL, log); fprintf(stderr, "shader: %s\n", log); }
    return s;
}

static int cmdline_has(const char *needle)
{
    char buf[4096];
    int fd = open("/proc/cmdline", O_RDONLY);
    if (fd < 0) return 0;
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n <= 0) return 0;
    buf[n] = 0;
    return strstr(buf, needle) != NULL;
}

struct gl { int fd; struct gbm_device *gbm; EGLDisplay dpy; EGLContext ctx; };

/* 0 ok, 1 skip (reason printed), -1 error */
static int gl_up(struct gl *g, const char *who)
{
    const char *nodes[] = { "/dev/dri/renderD128", "/dev/dri/card0" };
    g->fd = -1;
    for (unsigned i = 0; i < 2 && g->fd < 0; i++) g->fd = open(nodes[i], O_RDWR | O_CLOEXEC);
    if (g->fd < 0) { printf("i915gltest[%s]: SKIP no DRM node\n", who); return 1; }
    g->gbm = gbm_create_device(g->fd);
    if (!g->gbm) { printf("i915gltest[%s]: gbm_create_device failed\n", who); return -1; }
    PFNEGLGETPLATFORMDISPLAYEXTPROC gpd = (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    g->dpy = gpd ? gpd(EGL_PLATFORM_GBM_KHR, g->gbm, NULL) : eglGetDisplay((EGLNativeDisplayType)g->gbm);
    EGLint maj, min;
    if (g->dpy == EGL_NO_DISPLAY || !eglInitialize(g->dpy, &maj, &min)) { printf("i915gltest[%s]: eglInitialize failed\n", who); return -1; }
    eglBindAPI(EGL_OPENGL_ES_API);
    static const EGLint cattr[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    g->ctx = eglCreateContext(g->dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
    if (g->ctx == EGL_NO_CONTEXT || !eglMakeCurrent(g->dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g->ctx)) {
        printf("i915gltest[%s]: surfaceless context failed (0x%x)\n", who, eglGetError()); return -1;
    }
    const char *r = (const char *)glGetString(GL_RENDERER);
    if ((!r || !strstr(r, "Intel")) && !getenv("I915GLTEST_ANY")) { printf("i915gltest[%s]: SKIP renderer is '%s', not Intel\n", who, r ? r : "?"); return 1; }
    return 0;
}

/* The text-like scene for `frames` frames; returns mismatched pixels (or -1). */
static long run_scene(int seed, const char *who)
{
    struct gl g;
    int r = gl_up(&g, who);
    if (r) return r > 0 ? -2 : -1;
    GLuint prog = glCreateProgram();
    glAttachShader(prog, shader(GL_VERTEX_SHADER, VS));
    glAttachShader(prog, shader(GL_FRAGMENT_SHADER, FS));
    glBindAttribLocation(prog, 0, "a_pos");
    glBindAttribLocation(prog, 1, "a_uv");
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) { printf("i915gltest[%s]: link failed\n", who); return -1; }
    glUseProgram(prog);
    GLint u_swz = glGetUniformLocation(prog, "u_swz");
    glUniform1i(glGetUniformLocation(prog, "u_tex"), 0);

    GLuint atlas, rt, fbo;
    glGenTextures(1, &atlas);
    glBindTexture(GL_TEXTURE_2D, atlas);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, ATLAS, ATLAS, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glGenTextures(1, &rt);
    glBindTexture(GL_TEXTURE_2D, rt);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, W, H, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, rt, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) { printf("i915gltest[%s]: FBO incomplete\n", who); return -1; }
    glBindTexture(GL_TEXTURE_2D, atlas);
    glViewport(0, 0, W, H);
    glEnableVertexAttribArray(0);
    glEnableVertexAttribArray(1);

    uint32_t *atl = malloc(ATLAS * ATLAS * 4), *ref = malloc(W * H * 4), *got = malloc(W * H * 4);
    float *vtx = malloc(GRID * GRID * 6 * 4 * sizeof(float));
    long bad_total = 0;
    int bad_frames = 0;
    const uint32_t clear = 0xFF204060u; /* ABGR in memory = RGBA(0x60,0x40,0x20,0xFF) */
    for (int f = 0; f < g_frames; f++) {
        /* Rewrite the atlas: half the frames all of it, else 16 rows of cells. */
        for (int y = 0; y < ATLAS; y++)
            for (int x = 0; x < ATLAS; x++) atl[y * ATLAS + x] = texel(seed, f, x, y);
        if (f & 1) {
            glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, ATLAS, ATLAS, GL_RGBA, GL_UNSIGNED_BYTE, atl);
        } else {
            for (int cy = 0; cy < ATLAS / CELL; cy++)   /* one call per cell row, like a glyph cache */
                glTexSubImage2D(GL_TEXTURE_2D, 0, 0, cy * CELL, ATLAS, CELL, GL_RGBA, GL_UNSIGNED_BYTE, atl + cy * CELL * ATLAS);
        }
        glClearColor(0x60 / 255.0f, 0x40 / 255.0f, 0x20 / 255.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        for (int i = 0; i < W * H; i++) ref[i] = clear;
        /* Quads, split into DRAWS draw calls with alternating swizzle. */
        int nq = 0;
        int qdraw[GRID * GRID];
        for (int gy = 0; gy < GRID; gy++)
            for (int gx = 0; gx < GRID; gx++) {
                int c = quad_cell(seed, f, gx, gy);
                if (c < 0) continue;
                int d = (gy * GRID + gx) * DRAWS / (GRID * GRID);
                int swz = d & 1;
                int cu = (c % (ATLAS / CELL)) * CELL, cv = (c / (ATLAS / CELL)) * CELL;
                float x0 = gx * CELL * 2.0f / W - 1, x1 = (gx + 1) * CELL * 2.0f / W - 1;
                float y0 = gy * CELL * 2.0f / H - 1, y1 = (gy + 1) * CELL * 2.0f / H - 1;
                float u0 = (float)cu / ATLAS, u1 = (float)(cu + CELL) / ATLAS;
                float v0 = (float)cv / ATLAS, v1 = (float)(cv + CELL) / ATLAS;
                float q[24] = { x0, y0, u0, v0, x1, y0, u1, v0, x1, y1, u1, v1,
                                x0, y0, u0, v0, x1, y1, u1, v1, x0, y1, u0, v1 };
                memcpy(vtx + nq * 24, q, sizeof q);
                qdraw[nq++] = d;
                for (int py = 0; py < CELL; py++)
                    for (int px = 0; px < CELL; px++) {
                        uint32_t t = atl[(cv + py) * ATLAS + cu + px];
                        if (swz) t = (t & 0xFF00FF00u) | ((t >> 16) & 0xFF) | ((t & 0xFF) << 16);
                        ref[(gy * CELL + py) * W + gx * CELL + px] = t;
                    }
            }
        /* Client-memory vertex arrays: iris streams them per draw. */
        for (int s = 0; s < nq;) {
            int e = s;
            while (e < nq && qdraw[e] == qdraw[s]) e++;
            glUniform1f(u_swz, (float)(qdraw[s] & 1));
            glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 16, vtx + s * 24);
            glVertexAttribPointer(1, 2, GL_FLOAT, GL_FALSE, 16, vtx + s * 24 + 2);
            glDrawArrays(GL_TRIANGLES, 0, (e - s) * 6);
            s = e;
        }
        glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, got);
        long bad = 0;
        int first = -1;
        for (int i = 0; i < W * H; i++)
            if (got[i] != ref[i]) { if (first < 0) first = i; bad++; }
        if (bad) {
            bad_frames++;
            if (bad_frames <= 3)
                printf("i915gltest[%s]: frame %d: %ld/%d pixels differ; first (%d,%d) got %08x want %08x\n",
                       who, f, bad, W * H, first % W, first / W, got[first], ref[first]);
        }
        bad_total += bad;
    }
    if (bad_frames) printf("i915gltest[%s]: %d/%d frames bad, %ld pixels total\n", who, bad_frames, g_frames, bad_total);
    glFinish();
    eglMakeCurrent(g.dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglTerminate(g.dpy);
    return bad_total;
}

/* Render-engine pressure: big clears and framebuffer copies until killed. */
static void blit_hog(void)
{
    struct gl g;
    if (gl_up(&g, "hog")) _exit(0);
    GLuint tex[2], fbo[2];
    glGenTextures(2, tex);
    glGenFramebuffers(2, fbo);
    for (int i = 0; i < 2; i++) {
        glBindTexture(GL_TEXTURE_2D, tex[i]);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 1024, 1024, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
        glBindFramebuffer(GL_FRAMEBUFFER, fbo[i]);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex[i], 0);
    }
    for (unsigned n = 0;; n++) {
        glBindFramebuffer(GL_FRAMEBUFFER, fbo[n & 1]);
        glClearColor((n & 255) / 255.0f, 0.5f, 0.25f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glBindTexture(GL_TEXTURE_2D, tex[(n + 1) & 1]);
        glCopyTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 0, 0, 1024, 1024);
        if ((n & 7) == 7) glFinish();
    }
}

/* Run `nproc` scene processes (+ a hog); 0 pass, 1 fail, 2 skip. */
static int run_case(const char *name, int nproc, int hog)
{
    pid_t hp = -1, pids[4];
    if (hog) { hp = fork(); if (hp == 0) { blit_hog(); _exit(0); } }
    for (int i = 0; i < nproc; i++) {
        pids[i] = fork();
        if (pids[i] == 0) {
            char who[32];
            snprintf(who, sizeof who, "%s.%d", name, i);
            long bad = run_scene(i + 1, who);
            _exit(bad == -2 ? 2 : bad == 0 ? 0 : 1);
        }
    }
    int res = 0, skips = 0;
    for (int i = 0; i < nproc; i++) {
        int st = 0;
        waitpid(pids[i], &st, 0);
        int rc = WIFEXITED(st) ? WEXITSTATUS(st) : 1;
        if (rc == 2) skips++; else if (rc) res = 1;
    }
    if (hp > 0) { kill(hp, SIGKILL); waitpid(hp, NULL, 0); }
    if (skips == nproc) { printf("i915gltest: case %s: SKIP\n", name); return 2; }
    printf("i915gltest: case %s: %s (%d process%s x %d frames%s)\n", name, res ? "FAIL" : "PASS",
           nproc, nproc > 1 ? "es" : "", g_frames, hog ? ", + rcs blit hog" : "");
    return res;
}


/* ───────────────────── comp: a smithay-like compositor ─────────────────────
 *
 * Replicates what cosmic-comp (smithay GlesRenderer) does on the laptop,
 * where the scanout showed cursor trails, no wallpaper and no greeter:
 *   - render target: 2 GBM BOs (XRGB8888, SCANOUT|RENDERING, LINEAR, or
 *     with_modifiers {LINEAR}), imported as EGLImage dma-bufs and bound as
 *     renderbuffer FBOs; optionally ADDFB2'd on card0 so the kernel's
 *     uncached-scanout path (i915.kms_uncached) applies;
 *   - buffer-age damage (age 2): per damage rect glScissor + glClear, then
 *     the elements with premultiplied blending (ONE, ONE_MINUS_SRC_ALPHA):
 *     wallpaper (client dma-buf, rendered once), greeter (client dma-buf,
 *     new content every frame), cursor (glTexImage2D texture, moves);
 *   - the client is ANOTHER process: iris through a gbm_surface with the
 *     requested modifier (eglSwapBuffers + lock_front_buffer, no glFinish),
 *     buffers passed over a unix socket (SCM_RIGHTS), cached by the
 *     compositor per buffer like smithay caches per wl_buffer, released back
 *     after the compositor's glFlush (implicit sync in both directions).
 * Every frame is read back three ways and compared with a CPU reference:
 *   mmapE — dma-buf mmap + DMA_BUF_IOCTL_SYNC right after glFlush (no finish)
 *   gl    — glReadPixels
 *   mmapL — dma-buf mmap again after glFinish
 *   other — mmap of the OTHER target: must still hold frame f-1
 * Mismatches are split by where they are: inside this frame's damage
 * (cursor / greeter / wallpaper-only pixels) or outside it (stale content
 * left from 2 frames ago: the trail signature).
 */
#define CW 256
#define CH 256
#define GS 128                      /* greeter size, at (GX,GY) */
#define GX 64
#define GY 64
#define CUR 16
#define NTGT 2                      /* cosmic-comp ADDFBs 2 on the laptop (boot 10) */
#define FMT_ARGB 0x34325241u
#define FMT_XRGB 0x34325258u
#define MOD_LINEAR 0ull
#define MOD_Y ((1ull << 56) | 2)
#define MOD_YCCS ((1ull << 56) | 4)
#define MOD_INVALID 0x00ffffffffffffffull
#define DMA_BUF_IOCTL_SYNC_ 0x40086200u
#define DRM_IOCTL_PRIME_FD_TO_HANDLE_ 0xC00C642Eu
#define DRM_IOCTL_MODE_ADDFB2_ 0xC06864B8u
#define DRM_IOCTL_MODE_RMFB_ 0xC00464AFu

struct fb_cmd2 { uint32_t fb_id, width, height, pixel_format, flags, handles[4], pitches[4], offsets[4]; uint64_t modifier[4]; };
struct prime_h { uint32_t handle, flags; int32_t fd; };
struct cmsg_buf { int kind, id, frame, nplanes; uint32_t fourcc, w, h, off[4], str[4]; uint64_t mod; };

static const char *mod_name(uint64_t m)
{
    return m == MOD_LINEAR ? "LINEAR" : m == MOD_Y ? "Y_TILED" : m == MOD_YCCS ? "Y_TILED_CCS" : m == MOD_INVALID ? "INVALID" : "other";
}

static void wall_px(int x, int y, uint8_t *p)
{
    uint32_t h = hash3((uint32_t)x / 32, (uint32_t)y / 32, 4242);
    p[0] = h & 0xFF; p[1] = (h >> 8) & 0xFF; p[2] = (h >> 16) & 0xFF; p[3] = 0xFF;
}

/* Greeter layout of frame f: premultiplied bg (alpha 0xC0), 4 opaque rects,
 * one premultiplied (alpha 0x80) quad drawn by a shader. Memory coords. */
struct grect { int x, y, w, h; uint8_t c[4]; };
static void greet_layout(int f, struct grect *r /* [6] */)
{
    uint32_t h = hash3((uint32_t)f, 1, 31);
    r[0] = (struct grect){ 0, 0, GS, GS, { (uint8_t)(h % 0xC0), (uint8_t)((h >> 8) % 0xC0), (uint8_t)((h >> 16) % 0xC0), 0xC0 } };
    for (int k = 1; k <= 5; k++) {
        uint32_t a = hash3((uint32_t)f, (uint32_t)k, 77), c = hash3((uint32_t)f, (uint32_t)k, 99);
        r[k].x = a % 96; r[k].y = (a >> 8) % 96; r[k].w = 8 + (a >> 16) % 32; r[k].h = 8 + (a >> 24) % 32;
        if (k < 5) { r[k].c[0] = c; r[k].c[1] = c >> 8; r[k].c[2] = c >> 16; r[k].c[3] = 0xFF; }
        else { r[k].c[0] = (c & 0xFF) % 0x80; r[k].c[1] = ((c >> 8) & 0xFF) % 0x80; r[k].c[2] = ((c >> 16) & 0xFF) % 0x80; r[k].c[3] = 0x80; }
    }
}
static void greet_px(const struct grect *r, int x, int y, uint8_t *p)
{
    for (int k = 5; k >= 0; k--)
        if (x >= r[k].x && x < r[k].x + r[k].w && y >= r[k].y && y < r[k].y + r[k].h) { memcpy(p, r[k].c, 4); return; }
}
static void cur_px(int x, int y, uint8_t *p)
{
    if (x == 0 || y == 0 || x == CUR - 1 || y == CUR - 1) { p[0] = p[1] = p[2] = p[3] = 0xFF; return; }
    if ((x + y) & 4) { p[0] = 0x40; p[1] = 0x20; p[2] = 0x10; p[3] = 0x80; return; }
    p[0] = p[1] = p[2] = p[3] = 0;
}
static void cur_pos(int f, int *x, int *y) { *x = 4 + (f * 13) % (CW - CUR - 8); *y = 4 + (f * 29) % (CH - CUR - 8); }
static uint8_t blend1(uint8_t s, uint8_t d, uint8_t a)
{
    int v = (s * 255 + d * (255 - a) + 127) / 255;
    return (uint8_t)(v > 255 ? 255 : v);
}

/* CPU reference of frame f: RGB per pixel. */
static void comp_ref(int f, uint8_t *ref)
{
    struct grect g[6];
    greet_layout(f, g);
    int cx, cy;
    cur_pos(f, &cx, &cy);
    for (int y = 0; y < CH; y++)
        for (int x = 0; x < CW; x++) {
            uint8_t d[4], s[4];
            wall_px(x, y, d);
            if (x >= GX && x < GX + GS && y >= GY && y < GY + GS) {
                greet_px(g, x - GX, y - GY, s);
                for (int c = 0; c < 3; c++) d[c] = blend1(s[c], d[c], s[3]);
            }
            if (x >= cx && x < cx + CUR && y >= cy && y < cy + CUR) {
                cur_px(x - cx, y - cy, s);
                for (int c = 0; c < 3; c++) d[c] = blend1(s[c], d[c], s[3]);
            }
            memcpy(ref + (y * CW + x) * 3, d, 3);
        }
}

static int send_buf(int sk, const struct cmsg_buf *m, const int *fds, int nfds)
{
    struct iovec iov = { (void *)m, sizeof *m };
    char cbuf[CMSG_SPACE(4 * sizeof(int))];
    struct msghdr mh;
    memset(&mh, 0, sizeof mh);
    mh.msg_iov = &iov; mh.msg_iovlen = 1;
    if (nfds) {
        memset(cbuf, 0, sizeof cbuf);
        mh.msg_control = cbuf; mh.msg_controllen = CMSG_SPACE(nfds * sizeof(int));
        struct cmsghdr *c = CMSG_FIRSTHDR(&mh);
        c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(nfds * sizeof(int));
        memcpy(CMSG_DATA(c), fds, nfds * sizeof(int));
    }
    return sendmsg(sk, &mh, 0) == (ssize_t)sizeof *m ? 0 : -1;
}
static int recv_buf(int sk, struct cmsg_buf *m, int *fds)
{
    struct iovec iov = { m, sizeof *m };
    char cbuf[CMSG_SPACE(4 * sizeof(int))];
    struct msghdr mh;
    memset(&mh, 0, sizeof mh);
    mh.msg_iov = &iov; mh.msg_iovlen = 1; mh.msg_control = cbuf; mh.msg_controllen = sizeof cbuf;
    m->kind = -1;
    if (recvmsg(sk, &mh, MSG_WAITALL) != (ssize_t)sizeof *m) return -1;
    int n = 0;
    struct cmsghdr *c = CMSG_FIRSTHDR(&mh);   /* the sender attaches at most one */
    if (c && c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) {
            n = (int)((c->cmsg_len - CMSG_LEN(0)) / sizeof(int));
            memcpy(fds, CMSG_DATA(c), n * sizeof(int));
        }
    return n;
}

static PFNEGLCREATEIMAGEKHRPROC p_create_image;
static PFNEGLDESTROYIMAGEKHRPROC p_destroy_image;
static PFNGLEGLIMAGETARGETTEXTURE2DOESPROC p_img_tex;
static PFNGLEGLIMAGETARGETRENDERBUFFERSTORAGEOESPROC p_img_rb;
static PFNEGLQUERYDMABUFMODIFIERSEXTPROC p_query_mods;

static void egl_procs(void)
{
    p_create_image = (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    p_destroy_image = (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    p_img_tex = (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    p_img_rb = (PFNGLEGLIMAGETARGETRENDERBUFFERSTORAGEOESPROC)eglGetProcAddress("glEGLImageTargetRenderbufferStorageOES");
    p_query_mods = (PFNEGLQUERYDMABUFMODIFIERSEXTPROC)eglGetProcAddress("eglQueryDmaBufModifiersEXT");
}

/* EGL_EXT_image_dma_buf_import(_modifiers), as smithay's import_dmabuf. */
static EGLImageKHR import_dmabuf(EGLDisplay dpy, const struct cmsg_buf *m, const int *fds)
{
    EGLint a[64], n = 0;
    a[n++] = EGL_WIDTH; a[n++] = (EGLint)m->w; a[n++] = EGL_HEIGHT; a[n++] = (EGLint)m->h;
    a[n++] = EGL_LINUX_DRM_FOURCC_EXT; a[n++] = (EGLint)m->fourcc;
    static const EGLint pa[4][5] = {
        { EGL_DMA_BUF_PLANE0_FD_EXT, EGL_DMA_BUF_PLANE0_OFFSET_EXT, EGL_DMA_BUF_PLANE0_PITCH_EXT, EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT },
        { EGL_DMA_BUF_PLANE1_FD_EXT, EGL_DMA_BUF_PLANE1_OFFSET_EXT, EGL_DMA_BUF_PLANE1_PITCH_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT },
        { EGL_DMA_BUF_PLANE2_FD_EXT, EGL_DMA_BUF_PLANE2_OFFSET_EXT, EGL_DMA_BUF_PLANE2_PITCH_EXT, EGL_DMA_BUF_PLANE2_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE2_MODIFIER_HI_EXT },
        { EGL_DMA_BUF_PLANE3_FD_EXT, EGL_DMA_BUF_PLANE3_OFFSET_EXT, EGL_DMA_BUF_PLANE3_PITCH_EXT, EGL_DMA_BUF_PLANE3_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE3_MODIFIER_HI_EXT },
    };
    for (int p = 0; p < m->nplanes && p < 4; p++) {
        a[n++] = pa[p][0]; a[n++] = fds[p]; a[n++] = pa[p][1]; a[n++] = (EGLint)m->off[p];
        a[n++] = pa[p][2]; a[n++] = (EGLint)m->str[p];
        if (m->mod != MOD_INVALID) {
            a[n++] = pa[p][3]; a[n++] = (EGLint)(m->mod & 0xFFFFFFFFu);
            a[n++] = pa[p][4]; a[n++] = (EGLint)(m->mod >> 32);
        }
    }
    a[n++] = EGL_NONE;
    return p_create_image(dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, a);
}

static int bo_desc(struct gbm_bo *bo, struct cmsg_buf *m, int *fds)
{
    m->w = gbm_bo_get_width(bo); m->h = gbm_bo_get_height(bo); m->fourcc = gbm_bo_get_format(bo);
    m->mod = gbm_bo_get_modifier(bo);
    m->nplanes = gbm_bo_get_plane_count(bo);
    for (int p = 0; p < m->nplanes && p < 4; p++) {
        fds[p] = gbm_bo_get_fd_for_plane(bo, p);
        m->off[p] = gbm_bo_get_offset(bo, p);
        m->str[p] = gbm_bo_get_stride_for_plane(bo, p);
        if (fds[p] < 0) return -1;
    }
    return 0;
}

static const char *FS_TEX = "precision mediump float; varying vec2 v_uv; uniform sampler2D u_tex;\n"
    "void main(){ gl_FragColor = texture2D(u_tex, v_uv); }\n";
static const char *FS_COL = "precision mediump float; varying vec2 v_uv; uniform vec4 u_col;\n"
    "void main(){ gl_FragColor = u_col; }\n";

static GLuint mkprog(const char *fs)
{
    GLuint p = glCreateProgram();
    glAttachShader(p, shader(GL_VERTEX_SHADER, VS));
    glAttachShader(p, shader(GL_FRAGMENT_SHADER, fs));
    glBindAttribLocation(p, 0, "a_pos");
    glBindAttribLocation(p, 1, "a_uv");
    glLinkProgram(p);
    return p;
}

/* Quad of pixel rect (x,y,w,h) in a (vw,vh) viewport, uv 0..1 (v=0 at
 * memory row y). `flip`: window-system framebuffer (memory row 0 is GL y
 * vh-1). The vertex array lives in client memory (static: iris copies it
 * at draw time, but keep it alive anyway). */
static void quad(int x, int y, int w, int h, int vw, int vh, int flip)
{
    static float q[24];
    float gy0 = flip ? (float)(vh - y - h) : (float)y, gy1 = gy0 + h;
    float x0 = x * 2.0f / vw - 1, x1 = (x + w) * 2.0f / vw - 1;
    float y0 = gy0 * 2.0f / vh - 1, y1 = gy1 * 2.0f / vh - 1;
    float v0 = flip ? 1.0f : 0.0f, v1 = flip ? 0.0f : 1.0f;
    float t[24] = { x0, y0, 0, v0, x1, y0, 1, v0, x1, y1, 1, v1, x0, y0, 0, v0, x1, y1, 1, v1, x0, y1, 0, v1 };
    memcpy(q, t, sizeof t);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 16, q);
    glVertexAttribPointer(1, 2, GL_FLOAT, GL_FALSE, 16, q + 2);
    glDrawArrays(GL_TRIANGLES, 0, 6);
}

static void client_fail(int sk, int kind)
{
    struct cmsg_buf m;
    memset(&m, 0, sizeof m);
    m.kind = kind;
    send_buf(sk, &m, NULL, 0);
    _exit(0);
}

/* ── the client process (a Wayland client's EGL path, minus Wayland) ── */
static void comp_client(int sk, uint64_t want)
{
    struct gl g;
    struct cmsg_buf m;
    memset(&m, 0, sizeof m);
    int r = gl_up(&g, "comp.client");
    if (r) client_fail(sk, r > 0 ? -2 : -1);
    egl_procs();
    EGLuint64KHR mods[64];
    EGLBoolean ext[64];
    EGLint nm = 0;
    if (p_query_mods) p_query_mods(g.dpy, (EGLint)FMT_ARGB, 64, mods, ext, &nm);
    int have = 0;
    char list[512] = "";
    for (int i = 0; i < nm; i++) {
        if (mods[i] == want && !ext[i]) have = 1;
        size_t l = strlen(list);
        snprintf(list + l, sizeof list - l, "%s%s%s", i ? "," : "", mod_name(mods[i]), ext[i] ? "(ext)" : "");
    }
    printf("i915gltest[comp.client]: ARGB8888 modifiers: %s\n", nm ? list : "(none reported)");
    if (!have && !(want == MOD_LINEAR && nm == 0)) {
        printf("i915gltest[comp.client]: %s not offered\n", mod_name(want));
        client_fail(sk, -2);
    }
    PFNEGLCREATEPLATFORMWINDOWSURFACEEXTPROC mkwin = (PFNEGLCREATEPLATFORMWINDOWSURFACEEXTPROC)eglGetProcAddress("eglCreatePlatformWindowSurfaceEXT");
    static const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_WINDOW_BIT, EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
                                 EGL_ALPHA_SIZE, 8, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_NONE };
    EGLConfig cfgs[64], cfg = NULL;
    EGLint nc = 0;
    eglChooseConfig(g.dpy, ca, cfgs, 64, &nc);
    for (int i = 0; i < nc; i++) {
        EGLint vid = 0;
        eglGetConfigAttrib(g.dpy, cfgs[i], EGL_NATIVE_VISUAL_ID, &vid);
        if ((uint32_t)vid == FMT_ARGB) { cfg = cfgs[i]; break; }
    }
    struct gbm_surface *gs[2];
    EGLSurface es[2];
    int dim[2] = { CW, GS };
    for (int i = 0; i < 2; i++) {
        gs[i] = nm ? gbm_surface_create_with_modifiers2(g.gbm, dim[i], dim[i], FMT_ARGB, &want, 1, GBM_BO_USE_RENDERING)
                   : gbm_surface_create(g.gbm, dim[i], dim[i], FMT_ARGB, GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);
        es[i] = (cfg && gs[i] && mkwin) ? mkwin(g.dpy, cfg, gs[i], NULL) : EGL_NO_SURFACE;
        if (es[i] == EGL_NO_SURFACE) {
            printf("i915gltest[comp.client]: window surface %d failed (cfg %p gbm_surface %p egl 0x%x)\n", i, (void *)cfg, (void *)gs[i], eglGetError());
            client_fail(sk, -1);
        }
    }
    GLuint pcol = mkprog(FS_COL);
    GLint u_col = glGetUniformLocation(pcol, "u_col");
    int fds[4];
    /* Wallpaper: once, 64 scissored clears of 32x32 cells. */
    eglMakeCurrent(g.dpy, es[0], es[0], g.ctx);
    glViewport(0, 0, CW, CH);
    glEnable(GL_SCISSOR_TEST);
    for (int cy = 0; cy < CH / 32; cy++)
        for (int cx = 0; cx < CW / 32; cx++) {
            uint8_t p[4];
            wall_px(cx * 32, cy * 32, p);
            glScissor(cx * 32, CH - cy * 32 - 32, 32, 32);
            glClearColor(p[0] / 255.0f, p[1] / 255.0f, p[2] / 255.0f, 1.0f);
            glClear(GL_COLOR_BUFFER_BIT);
        }
    eglSwapBuffers(g.dpy, es[0]);
    struct gbm_bo *wall = gbm_surface_lock_front_buffer(gs[0]);
    if (!wall || bo_desc(wall, &m, fds)) client_fail(sk, -1);
    m.kind = 0; m.id = 0; m.frame = -1;
    printf("i915gltest[comp.client]: wallpaper %ux%u %s planes=%d stride=%u\n", m.w, m.h, mod_name(m.mod), m.nplanes, m.str[0]);
    send_buf(sk, &m, fds, m.nplanes);
    for (int p = 0; p < m.nplanes; p++) close(fds[p]);
    /* Greeter: new content every frame. */
    eglMakeCurrent(g.dpy, es[1], es[1], g.ctx);
    glViewport(0, 0, GS, GS);
    glEnableVertexAttribArray(0);
    glEnableVertexAttribArray(1);
    struct gbm_bo *prev = NULL, *known[8] = { 0 };
    for (int f = 0; f < g_frames; f++) {
        struct grect gr[6];
        greet_layout(f, gr);
        glEnable(GL_SCISSOR_TEST);
        for (int k = 0; k < 5; k++) {
            glScissor(gr[k].x, GS - gr[k].y - gr[k].h, gr[k].w, gr[k].h);
            glClearColor(gr[k].c[0] / 255.0f, gr[k].c[1] / 255.0f, gr[k].c[2] / 255.0f, gr[k].c[3] / 255.0f);
            glClear(GL_COLOR_BUFFER_BIT);
        }
        glDisable(GL_SCISSOR_TEST);
        glUseProgram(pcol);
        glUniform4f(u_col, gr[5].c[0] / 255.0f, gr[5].c[1] / 255.0f, gr[5].c[2] / 255.0f, gr[5].c[3] / 255.0f);
        quad(gr[5].x, gr[5].y, gr[5].w, gr[5].h, GS, GS, 1);
        eglSwapBuffers(g.dpy, es[1]);
        struct gbm_bo *bo = gbm_surface_lock_front_buffer(gs[1]);
        if (!bo) client_fail(sk, -1);
        int id = -1, nfd = 0;
        for (int i = 0; i < 8; i++) if (known[i] == bo) id = i;
        if (id < 0) {
            for (int i = 0; i < 8 && id < 0; i++) if (!known[i]) { known[i] = bo; id = i; }
            if (id < 0 || bo_desc(bo, &m, fds)) client_fail(sk, -1);
            nfd = m.nplanes;
        }
        m.kind = 1; m.id = id + 1; m.frame = f;
        send_buf(sk, &m, fds, nfd);
        for (int p = 0; p < nfd; p++) close(fds[p]);
        char ack;
        if (read(sk, &ack, 1) != 1) _exit(1);   /* compositor flushed frame f: done with f-1's buffer */
        if (prev) gbm_surface_release_buffer(gs[1], prev);
        prev = bo;
    }
    _exit(0);
}

struct tgt { struct gbm_bo *bo; int fd; uint32_t stride, fb; GLuint rb, fbo; uint8_t *map; size_t len; EGLImageKHR img; };

static void sync_cpu(int fd, int end)
{
    uint64_t fl = 1 /* READ */ | (end ? 4 : 0);
    ioctl(fd, DMA_BUF_IOCTL_SYNC_, &fl);
}

/* Count mismatches (tolerance 2 per channel, RGB) by region. */
struct where { long dmg_cur, dmg_greet, dmg_wall, stale, total; int fx, fy; uint8_t got[3], want[3]; };
static void classify(const uint8_t *got, int bgr, const uint8_t *ref, const uint8_t *dmg, int f, struct where *w)
{
    int cx, cy;
    cur_pos(f, &cx, &cy);
    memset(w, 0, sizeof *w);
    w->fx = -1;
    for (int y = 0; y < CH; y++)
        for (int x = 0; x < CW; x++) {
            const uint8_t *s = got + (y * CW + x) * 4, *r = ref + (y * CW + x) * 3;
            uint8_t g3[3] = { bgr ? s[2] : s[0], s[1], bgr ? s[0] : s[2] };
            int bad = 0;
            for (int c = 0; c < 3; c++) if (abs((int)g3[c] - (int)r[c]) > 2) bad = 1;
            if (!bad) continue;
            w->total++;
            if (!dmg[y * CW + x]) w->stale++;
            else if (x >= cx && x < cx + CUR && y >= cy && y < cy + CUR) w->dmg_cur++;
            else if (x >= GX && x < GX + GS && y >= GY && y < GY + GS) w->dmg_greet++;
            else w->dmg_wall++;
            if (w->fx < 0) { w->fx = x; w->fy = y; memcpy(w->got, g3, 3); memcpy(w->want, r, 3); }
        }
}

/* One render target: GBM BO -> dma-buf -> EGLImage -> renderbuffer FBO
 * (+ mmap, + ADDFB2 on card0). 0 ok, 1 fail, 2 skip. */
static int tgt_make(struct gl *g, struct tgt *T, const char *who, int tmode, int addfb, int card)
{
    memset(T, 0, sizeof *T);
    uint64_t lin = MOD_LINEAR;
    T->bo = tmode ? gbm_bo_create_with_modifiers2(g->gbm, CW, CH, FMT_XRGB, &lin, 1, GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING)
                    : gbm_bo_create(g->gbm, CW, CH, FMT_XRGB, GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);
    struct cmsg_buf d;
    memset(&d, 0, sizeof d);
    int fds[4];
    if (!T->bo && tmode) { printf("i915gltest[%s]: SKIP gbm_bo_create_with_modifiers2({LINEAR}) failed (no modifier support)\n", who); return 2; }
    if (!T->bo || bo_desc(T->bo, &d, fds)) { printf("i915gltest[%s]: target BO failed\n", who); return 1; }
    if (tmode == 0) d.mod = MOD_INVALID;    /* a plain gbm_bo_create: implicit modifier */
    T->fd = fds[0]; T->stride = d.str[0]; T->len = (size_t)d.str[0] * CH;
    EGLImageKHR img = import_dmabuf(g->dpy, &d, fds);
    T->img = img;
    if (img == EGL_NO_IMAGE_KHR) { printf("i915gltest[%s]: target import failed 0x%x\n", who, eglGetError()); return 1; }
    glGenRenderbuffers(1, &T->rb);
    glBindRenderbuffer(GL_RENDERBUFFER, T->rb);
    p_img_rb(GL_RENDERBUFFER, img);
    glGenFramebuffers(1, &T->fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, T->fbo);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, T->rb);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) { printf("i915gltest[%s]: target FBO incomplete\n", who); return 1; }
    T->map = mmap(NULL, T->len, PROT_READ, MAP_SHARED, T->fd, 0);
    if (T->map == MAP_FAILED) { T->map = NULL; printf("i915gltest[%s]: target mmap failed (%d)\n", who, errno); }
    if (addfb) {
        struct prime_h ph = { 0, 0, T->fd };
        struct fb_cmd2 fc;
        memset(&fc, 0, sizeof fc);
        if (card >= 0 && ioctl(card, DRM_IOCTL_PRIME_FD_TO_HANDLE_, &ph) == 0) {
            fc.width = CW; fc.height = CH; fc.pixel_format = FMT_XRGB; fc.handles[0] = ph.handle; fc.pitches[0] = T->stride;
            if (tmode) { fc.flags = 2; fc.modifier[0] = MOD_LINEAR; }
            if (ioctl(card, DRM_IOCTL_MODE_ADDFB2_, &fc) == 0) T->fb = fc.fb_id;
        }
        if (!T->fb) { printf("i915gltest[%s]: SKIP ADDFB2 on card0 failed (errno %d)\n", who, errno); return 2; }
    }
    return 0;
}

/* Tear a target down the way smithay drops a swapchain slot (RMFB, image,
 * FBO, gbm_bo_destroy): iris caches the BO and hands its handle/VA out again. */
static void tgt_free(struct tgt *T, int card)
{
    if (T->fb) ioctl(card, DRM_IOCTL_MODE_RMFB_, &T->fb);
    glDeleteFramebuffers(1, &T->fbo);
    glDeleteRenderbuffers(1, &T->rb);
    if (T->img != EGL_NO_IMAGE_KHR && p_destroy_image) p_destroy_image(eglGetCurrentDisplay(), T->img);
    if (T->map) munmap(T->map, T->len);
    close(T->fd);
    gbm_bo_destroy(T->bo);
    memset(T, 0, sizeof *T);
}

/* Count pixels of a CPU view (stride bytes per row, B,G,R,X in memory when
 * bgr, else R,G,B,A) equal to rgb; report the first other one. */
static long px_match(const uint8_t *p, size_t stride, int bgr, uint32_t rgb, uint32_t *first_bad)
{
    long ok = 0;
    *first_bad = 0xffffffffu;
    for (int y = 0; y < CH; y++)
        for (int x = 0; x < CW; x++) {
            const uint8_t *s = p + (size_t)y * stride + x * 4;
            uint32_t v = bgr ? (uint32_t)s[2] << 16 | s[1] << 8 | s[0] : (uint32_t)s[0] << 16 | s[1] << 8 | s[2];
            if (v == rgb) ok++;
            else if (*first_bad == 0xffffffffu) *first_bad = v;
        }
    return ok;
}

/* CPU -> GPU -> CPU probe of one target, without any draw: separates "GPU
 * writes to the target are dropped" from "readback is broken".
 *   fresh     : a new dma-buf mmap of the just-created BO (expect zeros)
 *   clear>mmap: glClear(112233) + glFinish, then a NEW dma-buf mmap
 *   clear>gl  : glReadPixels of the same
 *   cpu>gl    : the CPU fills the BO (445566) through a RW dma-buf mmap,
 *               then glReadPixels
 *   cpu>mmapR : the read-only target mapping made at creation sees it too */
static void tgt_probe(struct tgt *T, const char *who, int idx)
{
    uint8_t *got = malloc(CW * CH * 4);
    uint32_t fb;
    long n;
    uint8_t *m = mmap(NULL, T->len, PROT_READ, MAP_SHARED, T->fd, 0);
    if (m != MAP_FAILED) {
        sync_cpu(T->fd, 0);
        n = px_match(m, T->stride, 1, 0, &fb);
        sync_cpu(T->fd, 1);
        munmap(m, T->len);
        printf("i915gltest[%s]: probe t%d fresh: %ld/%d px == 000000 (first other %06x)\n", who, idx, n, CW * CH, fb);
    } else printf("i915gltest[%s]: probe t%d fresh: mmap failed (%d)\n", who, idx, errno);
    glBindFramebuffer(GL_FRAMEBUFFER, T->fbo);
    glViewport(0, 0, CW, CH);
    glDisable(GL_SCISSOR_TEST);
    glDisable(GL_BLEND);
    glClearColor(0x11 / 255.0f, 0x22 / 255.0f, 0x33 / 255.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glFinish();
    m = mmap(NULL, T->len, PROT_READ, MAP_SHARED, T->fd, 0);
    if (m != MAP_FAILED) {
        sync_cpu(T->fd, 0);
        n = px_match(m, T->stride, 1, 0x112233, &fb);
        sync_cpu(T->fd, 1);
        munmap(m, T->len);
        printf("i915gltest[%s]: probe t%d clear>mmap: %ld/%d px == 112233 (first other %06x)\n", who, idx, n, CW * CH, fb);
    }
    glReadPixels(0, 0, CW, CH, GL_RGBA, GL_UNSIGNED_BYTE, got);
    n = px_match(got, CW * 4, 0, 0x112233, &fb);
    printf("i915gltest[%s]: probe t%d clear>gl: %ld/%d px == 112233 (first other %06x, glGetError 0x%x)\n", who, idx, n, CW * CH, fb, glGetError());
    m = mmap(NULL, T->len, PROT_READ | PROT_WRITE, MAP_SHARED, T->fd, 0);
    if (m != MAP_FAILED) {
        uint64_t fl = 2 /* WRITE */;
        ioctl(T->fd, DMA_BUF_IOCTL_SYNC_, &fl);
        for (int y = 0; y < CH; y++)
            for (int x = 0; x < CW; x++) { uint8_t *s = m + (size_t)y * T->stride + x * 4; s[0] = 0x66; s[1] = 0x55; s[2] = 0x44; s[3] = 0xff; }
        fl = 2 | 4;
        ioctl(T->fd, DMA_BUF_IOCTL_SYNC_, &fl);
        munmap(m, T->len);
        glReadPixels(0, 0, CW, CH, GL_RGBA, GL_UNSIGNED_BYTE, got);
        n = px_match(got, CW * 4, 0, 0x445566, &fb);
        printf("i915gltest[%s]: probe t%d cpu>gl: %ld/%d px == 445566 (first other %06x)\n", who, idx, n, CW * CH, fb);
        if (T->map) {
            n = px_match(T->map, T->stride, 1, 0x445566, &fb);
            printf("i915gltest[%s]: probe t%d cpu>mmapR: %ld/%d px == 445566 (first other %06x)\n", who, idx, n, CW * CH, fb);
        }
    } else printf("i915gltest[%s]: probe t%d cpu>gl: RW mmap failed (%d)\n", who, idx, errno);
    free(got);
}

/* The compositor process. tmode: 0 gbm_bo_create(LINEAR), 1 with_modifiers{LINEAR}. */
static int comp_server(int sk, const char *who, int tmode, int addfb, int churn)
{
    struct gl g;
    int r = gl_up(&g, who);
    if (r) return r > 0 ? 2 : 1;
    egl_procs();
    if (!p_create_image || !p_img_rb || !p_img_tex) { printf("i915gltest[%s]: no EGL dma-buf import\n", who); return 1; }
    int card = addfb ? open("/dev/dri/card0", O_RDWR | O_CLOEXEC) : -1;
    struct tgt t[NTGT];
    int born[NTGT];
    for (int i = 0; i < NTGT; i++) {
        int rc = tgt_make(&g, &t[i], who, tmode, addfb, card);
        if (rc) return rc;
        born[i] = 0;
    }
    printf("i915gltest[%s]: target %dx XRGB8888 %s stride %u%s\n", who, NTGT, tmode ? "with_modifiers{LINEAR}" : "gbm_bo_create(LINEAR)",
           t[0].stride, addfb ? ", ADDFB2'd on card0 (kms_uncached path)" : "");
    if (!strcmp(who, "comp-lin-first"))
        for (int i = 0; i < NTGT; i++) tgt_probe(&t[i], who, i);
    GLuint ptex = mkprog(FS_TEX);
    glUseProgram(ptex);
    glUniform1i(glGetUniformLocation(ptex, "u_tex"), 0);
    glEnableVertexAttribArray(0);
    glEnableVertexAttribArray(1);
    GLuint cur;
    uint8_t cbits[CUR * CUR * 4];
    for (int y = 0; y < CUR; y++) for (int x = 0; x < CUR; x++) cur_px(x, y, cbits + (y * CUR + x) * 4);
    glGenTextures(1, &cur);
    glBindTexture(GL_TEXTURE_2D, cur);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, CUR, CUR, 0, GL_RGBA, GL_UNSIGNED_BYTE, cbits);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    GLuint ctex[9] = { 0 };   /* [0] wallpaper, [1..8] greeter buffers by id */
    struct cmsg_buf m;
    int fds[4], res = 0, badf[4] = { 0, 0, 0, 0 };
    long badp[4] = { 0, 0, 0, 0 };
    const char *rbn[4] = { "mmapE", "gl", "mmapL", "other" };
    uint8_t *ref = malloc(CW * CH * 3), *dmg = malloc(CW * CH), *got = malloc(CW * CH * 4);
    uint8_t *ref_prev = malloc(CW * CH * 3), *all = malloc(CW * CH);
    memset(all, 1, CW * CH);
    for (int f = -1; f < g_frames; f++) {
        int n = recv_buf(sk, &m, fds);
        if (n < 0 || m.kind < 0) {
            if (m.kind == -2) { printf("i915gltest[%s]: SKIP (client)\n", who); return 2; }
            printf("i915gltest[%s]: client failed at frame %d\n", who, f); return 1;
        }
        if ((f < 0) != (m.kind == 0) || (f >= 0 && m.frame != f)) { printf("i915gltest[%s]: protocol error\n", who); return 1; }
        int slot = m.kind == 0 ? 0 : m.id;
        if (slot < 0 || slot > 8) { printf("i915gltest[%s]: bad buffer id %d\n", who, slot); return 1; }
        if (n > 0) {
            EGLImageKHR img = import_dmabuf(g.dpy, &m, fds);
            for (int p = 0; p < n; p++) close(fds[p]);
            if (img == EGL_NO_IMAGE_KHR) { printf("i915gltest[%s]: client buffer import (%s, %d planes) failed 0x%x\n", who, mod_name(m.mod), m.nplanes, eglGetError()); return 1; }
            if (!ctex[slot]) glGenTextures(1, &ctex[slot]);
            glBindTexture(GL_TEXTURE_2D, ctex[slot]);
            p_img_tex(GL_TEXTURE_2D, img);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            printf("i915gltest[%s]: imported client buffer %d: %s planes=%d stride=%u%s\n", who, slot, mod_name(m.mod), m.nplanes, m.str[0],
                   m.nplanes > 1 ? " (+aux plane)" : "");
        }
        if (f < 0) continue;
        if (!ctex[slot]) { printf("i915gltest[%s]: unknown buffer id %d\n", who, slot); return 1; }
        /* churn: every 8 frames drop both targets and allocate new ones
         * (same size: iris's BO cache recycles handles and VAs). */
        if (churn && f && f % 8 == 0)
            for (int i = 0; i < NTGT; i++) {
                tgt_free(&t[i], card);
                int rc = tgt_make(&g, &t[i], who, tmode, addfb, card);
                if (rc) return rc;
                born[i] = f;
            }
        struct tgt *T = &t[f % NTGT];
        /* Damage for buffer age NTGT: cursor at f..f-NTGT, the greeter; full when new. */
        int rects[6][4], nr = 0;
        if (f - born[f % NTGT] < NTGT) { rects[0][0] = 0; rects[0][1] = 0; rects[0][2] = CW; rects[0][3] = CH; nr = 1; }
        else {
            for (int k = 0; k <= NTGT; k++) { cur_pos(f - k, &rects[nr][0], &rects[nr][1]); rects[nr][2] = rects[nr][3] = CUR; nr++; }
            rects[nr][0] = GX; rects[nr][1] = GY; rects[nr][2] = rects[nr][3] = GS; nr++;
        }
        memset(dmg, 0, CW * CH);
        for (int k = 0; k < nr; k++)
            for (int y = rects[k][1]; y < rects[k][1] + rects[k][3]; y++) memset(dmg + y * CW + rects[k][0], 1, rects[k][2]);
        glBindFramebuffer(GL_FRAMEBUFFER, T->fbo);
        glViewport(0, 0, CW, CH);
        glEnable(GL_SCISSOR_TEST);
        glEnable(GL_BLEND);
        glBlendFunc(GL_ONE, GL_ONE_MINUS_SRC_ALPHA);
        int cx, cy;
        cur_pos(f, &cx, &cy);
        for (int k = 0; k < nr; k++) {
            glScissor(rects[k][0], rects[k][1], rects[k][2], rects[k][3]);
            glClearColor(0x27 / 255.0f, 0x29 / 255.0f, 0x2a / 255.0f, 1.0f);
            glClear(GL_COLOR_BUFFER_BIT);
            glBindTexture(GL_TEXTURE_2D, ctex[0]);
            quad(0, 0, CW, CH, CW, CH, 0);
            glBindTexture(GL_TEXTURE_2D, ctex[slot]);
            quad(GX, GY, GS, GS, CW, CH, 0);
            glBindTexture(GL_TEXTURE_2D, cur);
            quad(cx, cy, CUR, CUR, CW, CH, 0);
        }
        glDisable(GL_SCISSOR_TEST);
        glFlush();
        char ack = 1;
        if (write(sk, &ack, 1) != 1) return 1;
        if (f) memcpy(ref_prev, ref, CW * CH * 3);
        comp_ref(f, ref);
        /* "other": after frame f is finished, the OTHER target must still
         * hold frame f-1 (a render landing in the wrong BO shows here). */
        struct tgt *O = &t[(f + 1) % NTGT];
        int other_ok = f >= 1 && born[(f + 1) % NTGT] <= f - 1 && O->map;
        for (int rb = 0; rb < 4; rb++) {
            if (rb == 1) glReadPixels(0, 0, CW, CH, GL_RGBA, GL_UNSIGNED_BYTE, got);
            else if (rb == 3) {
                if (!other_ok) continue;
                sync_cpu(O->fd, 0);
                for (int y = 0; y < CH; y++) memcpy(got + y * CW * 4, O->map + (size_t)y * O->stride, CW * 4);
                sync_cpu(O->fd, 1);
            } else {
                if (!T->map) continue;
                if (rb == 2) glFinish();
                sync_cpu(T->fd, 0);
                for (int y = 0; y < CH; y++) memcpy(got + y * CW * 4, T->map + (size_t)y * T->stride, CW * 4);
                sync_cpu(T->fd, 1);
            }
            struct where w;
            if (rb == 3) classify(got, 1, ref_prev, all, f - 1, &w);
            else classify(got, rb != 1, ref, dmg, f, &w);
            if (!w.total) continue;
            badp[rb] += w.total;
            if (badf[rb]++ < 3)
                printf("i915gltest[%s]: frame %d %s: %ld px bad: damage{cursor %ld greeter %ld wallpaper %ld} undamaged(stale) %ld; first (%d,%d) got %02x%02x%02x want %02x%02x%02x\n",
                       who, f, rbn[rb], w.total, w.dmg_cur, w.dmg_greet, w.dmg_wall, w.stale, w.fx, w.fy,
                       w.got[0], w.got[1], w.got[2], w.want[0], w.want[1], w.want[2]);
        }
    }
    for (int rb = 0; rb < 4; rb++)
        if (badf[rb]) {
            /* mmapE alone is informational: a CPU read racing the GPU
             * (DMA_BUF_IOCTL_SYNC not waiting for the fence). */
            if (rb) res = 1;
            else printf("i915gltest[%s]: (mmapE not counted as a failure: DMA_BUF_IOCTL_SYNC returned before the GPU finished)\n", who);
            printf("i915gltest[%s]: %s: %d/%d frames bad, %ld px\n", who, rbn[rb], badf[rb], g_frames, badp[rb]);
        }
    glFinish();
    for (int i = 0; i < NTGT; i++) tgt_free(&t[i], card);
    return res;
}

static int run_comp(const char *name, int tmode, uint64_t cmod, int addfb, int churn)
{
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) return 1;
    pid_t cp = fork();
    if (cp == 0) { close(sv[0]); comp_client(sv[1], cmod); }
    close(sv[1]);
    pid_t sp = fork();
    if (sp == 0) _exit(comp_server(sv[0], name, tmode, addfb, churn));
    close(sv[0]);
    int st = 0, rc;
    waitpid(sp, &st, 0);
    rc = WIFEXITED(st) ? WEXITSTATUS(st) : 1;
    kill(cp, SIGKILL);
    waitpid(cp, NULL, 0);
    printf("i915gltest: case %s: %s (target %s%s%s, client %s, %d frames)\n", name, rc == 2 ? "SKIP" : rc ? "FAIL" : "PASS",
           tmode ? "mods{LINEAR}" : "LINEAR", addfb ? "+ADDFB2" : "", churn ? "+churn/8" : "", mod_name(cmod), g_frames);
    return rc;
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc > 1 && atoi(argv[1]) > 0) g_frames = atoi(argv[1]);
    const char *only = argc > 2 ? argv[2] : NULL;
    printf("--- i915gltest: deterministic 3D (glyph atlas + streamed quads, %d frames) ---\n", g_frames);
    if (cmdline_has("i915.fake")) {
        printf("i915gltest: SKIP (i915.fake: the soft CS runs no 3D)\n--- i915gltest done, failures = 0 ---\n");
        return 0;
    }
    int fails = 0, skips = 0;
    /* comp-lin once more BEFORE the blit case's SIGKILLed hog: a pass here
     * with a fail after it points at state a killed process leaves behind
     * (boot 13: execution-core LRCs keyed by the recycled open_id). */
    if (!only) {
        int r = run_comp("comp-lin-first", 0, MOD_LINEAR, 0, 0);
        if (r == 1) fails++;
        if (r == 2) skips++;
    }
    struct { const char *n; int p, h; } cases[] = { { "single", 1, 0 }, { "dual", 2, 0 }, { "blit", 1, 1 } };
    for (unsigned i = 0; i < 3; i++) {
        if (only && strcmp(only, cases[i].n)) continue;
        int r = run_case(cases[i].n, cases[i].p, cases[i].h);
        if (r == 1) fails++;
        if (r == 2) skips++;
    }
    /* comp: smithay-like compositor + client process (see above). */
    struct { const char *n; int tmode; uint64_t cmod; int addfb, churn; } comp[] = {
        { "comp-lin", 0, MOD_LINEAR, 0, 0 },   /* all linear: damage clears + blending only */
        { "comp-y", 0, MOD_Y, 0, 0 },          /* client Y-tiled (what iris picks without CCS) */
        { "comp-ccs", 0, MOD_YCCS, 0, 0 },     /* client Y-tiled + CCS aux plane */
        { "comp-mod", 1, MOD_Y, 0, 0 },        /* target via with_modifiers{LINEAR} */
        { "comp-fb", 0, MOD_Y, 1, 0 },         /* targets ADDFB2'd: uncached scanout PTEs */
        { "comp-fbccs", 1, MOD_YCCS, 1, 0 },   /* uncached targets + CCS client */
        { "comp-churn", 0, MOD_LINEAR, 0, 1 }, /* targets re-created every 8 frames (handle/VA reuse) */
        { "comp-fbchurn", 1, MOD_Y, 1, 1 },    /* the same, ADDFB2'd/RMFB'd each time */
    };
    for (unsigned i = 0; i < sizeof comp / sizeof comp[0]; i++) {
        if (only && strncmp(only, comp[i].n, strlen(only))) continue;
        int r = run_comp(comp[i].n, comp[i].tmode, comp[i].cmod, comp[i].addfb, comp[i].churn);
        if (r == 1) fails++;
        if (r == 2) skips++;
    }
    printf("--- i915gltest done, failures = %d%s ---\n", fails, skips ? " (some cases skipped)" : "");
    return fails;
}
