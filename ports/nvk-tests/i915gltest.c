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
 *
 * SKIP when /proc/cmdline has i915.fake (the fake soft CS has no 3D), when
 * there is no DRM node, or when the renderer is not Intel (iris).
 *
 * Usage: i915gltest [frames] [only-case]
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <gbm.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
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
    if (!r || !strstr(r, "Intel")) { printf("i915gltest[%s]: SKIP renderer is '%s', not Intel\n", who, r ? r : "?"); return 1; }
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
    struct { const char *n; int p, h; } cases[] = { { "single", 1, 0 }, { "dual", 2, 0 }, { "blit", 1, 1 } };
    for (unsigned i = 0; i < 3; i++) {
        if (only && strcmp(only, cases[i].n)) continue;
        int r = run_case(cases[i].n, cases[i].p, cases[i].h);
        if (r == 1) fails++;
        if (r == 2) skips++;
    }
    printf("--- i915gltest done, failures = %d%s ---\n", fails, skips ? " (some cases skipped)" : "");
    return fails;
}
