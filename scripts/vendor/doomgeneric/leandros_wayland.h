/*
 * leandros_wayland.h — DOOM as a Wayland client (wl_shm + xdg_shell).
 *
 * Vendored copy: the source of truth is
 * leandros/scripts/vendor/doomgeneric/leandros_wayland.h; build-all.sh copies
 * it into the shared ../doomgeneric checkout. Edit it there, not here.
 *
 * Why this exists. DOOM's only other outputs are the KMS surface (card0's
 * custom 0x1001..0x1006 ioctls) and /dev/fb0. Both put pixels on the display
 * *hardware*, and while a compositor is running the hardware belongs to the
 * compositor: card0's present ioctls are refused with EACCES to anyone but the
 * DRM master, and fb0 writes land in the console surface the compositor is not
 * scanning out. That is Linux's behaviour too — a raw fbdev/KMS program started
 * from a terminal inside a desktop is invisible there. The way a program is
 * seen under a compositor is to be one of its clients, so when the
 * environment names a Wayland display DOOM opens a window on it.
 *
 * The protocol is spoken directly on the socket (no libwayland): DOOM links the
 * small static LeandrOS libc, which has neither libffi nor a dynamic loader,
 * and a fixed 320x200 software surface needs only a handful of requests.
 *
 * Header-only and included by leandros_sdl.c alone, so a Makefile that does not
 * know about this file still builds and links.
 */
#ifndef LEANDROS_WAYLAND_H
#define LEANDROS_WAYLAND_H

#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include "doomgeneric.h"

/* Integer upscale of the 320x200 frame: 960x600 fits a 1280x800 output with
 * the panel, and integer scaling keeps the pixels square and sharp. */
#define WL_SCALE 3
#define WL_W (DOOMGENERIC_RESX * WL_SCALE)
#define WL_H (DOOMGENERIC_RESY * WL_SCALE)
#define WL_STRIDE (WL_W * 4)
#define WL_BUFSZ (WL_STRIDE * WL_H)

/* ── Raw syscalls (the LeandrOS libc has no sockets API) ─────────────────── */

#ifdef __aarch64__
#define WL_NR_SOCKET 198
#define WL_NR_CONNECT 203
#define WL_NR_SENDMSG 211
#define WL_NR_RECVMSG 212
#define WL_NR_MEMFD_CREATE 279
#define WL_NR_FTRUNCATE 46
#define WL_NR_PPOLL 73
#else
#define WL_NR_SOCKET 41
#define WL_NR_CONNECT 42
#define WL_NR_SENDMSG 46
#define WL_NR_RECVMSG 47
#define WL_NR_MEMFD_CREATE 319
#define WL_NR_FTRUNCATE 77
#define WL_NR_PPOLL 271
#endif

static long wl_sys(long n, long a0, long a1, long a2, long a3, long a4) {
    long ret;
#ifdef __aarch64__
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a0;
    register long x1 __asm__("x1") = a1;
    register long x2 __asm__("x2") = a2;
    register long x3 __asm__("x3") = a3;
    register long x4 __asm__("x4") = a4;
    __asm__ __volatile__("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3), "r"(x4) : "memory");
    ret = x0;
#else
    register long r10 __asm__("r10") = a3;
    register long r8 __asm__("r8") = a4;
    __asm__ __volatile__("syscall" : "=a"(ret)
                         : "a"(n), "D"(a0), "S"(a1), "d"(a2), "r"(r10), "r"(r8)
                         : "rcx", "r11", "memory");
#endif
    return ret;
}

struct wl_iov { void *base; size_t len; };
struct wl_msghdr {
    void *name; uint32_t namelen; uint32_t _pad0;
    struct wl_iov *iov; size_t iovlen;
    void *control; size_t controllen;
    int flags; int _pad1;
};
struct wl_cmsg { size_t len; int level; int type; };
#define WL_SOL_SOCKET 1
#define WL_SCM_RIGHTS 1
#define WL_CMSG_HDR ((sizeof(struct wl_cmsg) + 7) & ~(size_t)7)

/* ── Environment ─────────────────────────────────────────────────────────── */

/* Weak, so a libc without `environ` (an older tree building this shared
 * checkout) links and simply never finds a display. */
extern char **environ __attribute__((weak));

static const char *wl_getenv(const char *name) {
    if (&environ == 0 || environ == 0) return 0;
    size_t n = strlen(name);
    for (char **e = environ; *e; e++)
        if (strncmp(*e, name, n) == 0 && (*e)[n] == '=') return *e + n + 1;
    return 0;
}

/* ── Connection state ────────────────────────────────────────────────────── */

static struct {
    int fd;
    int active;            /* window is up; DOOM presents here */
    int closed;            /* toplevel closed or connection lost */
    uint32_t next_id;
    uint8_t out[4096]; size_t out_len;
    int out_fd;            /* one fd to pass with the next flush, or -1 */
    uint8_t in[8192]; size_t in_len;
    /* globals */
    uint32_t compositor_name, compositor_ver, shm_name, wm_name, seat_name, seat_ver;
    /* objects */
    uint32_t registry, sync_cb, compositor, shm, wm, seat, keyboard;
    uint32_t surface, xdg_surface, toplevel, pool, buffer[2], frame_cb;
    int sync_done, configured, buffer_busy[2], seat_caps;
    uint8_t *pixels;       /* both buffers, back to back */
    /* key events for SDL_PollEvent: (pressed << 16) | evdev code */
    uint32_t keyq[128]; unsigned keyq_r, keyq_w;
    uint8_t held[256];
} wl = { .fd = -1, .out_fd = -1 };

/* ── Request marshalling ─────────────────────────────────────────────────── */

static uint32_t wl_new_id(void) { return wl.next_id++; }

static int wl_flush(void) {
    size_t off = 0;
    while (off < wl.out_len) {
        struct wl_iov iov = { wl.out + off, wl.out_len - off };
        uint8_t cbuf[WL_CMSG_HDR + 8];
        struct wl_msghdr m;
        memset(&m, 0, sizeof m);
        m.iov = &iov; m.iovlen = 1;
        if (wl.out_fd >= 0) {
            struct wl_cmsg *c = (struct wl_cmsg *)cbuf;
            memset(cbuf, 0, sizeof cbuf);
            c->len = WL_CMSG_HDR + sizeof(int);
            c->level = WL_SOL_SOCKET; c->type = WL_SCM_RIGHTS;
            memcpy(cbuf + WL_CMSG_HDR, &wl.out_fd, sizeof(int));
            m.control = cbuf; m.controllen = WL_CMSG_HDR + 8;
        }
        long r = wl_sys(WL_NR_SENDMSG, wl.fd, (long)&m, 0x4000 /* MSG_NOSIGNAL */, 0, 0);
        if (r <= 0) { wl.closed = 1; wl.out_len = 0; return -1; }
        wl.out_fd = -1;
        off += (size_t)r;
    }
    wl.out_len = 0;
    return 0;
}

/* Message builder: begin, append args, end (patches the size). */
static size_t wl_msg_start;
static void wl_begin(uint32_t obj, uint32_t op) {
    if (wl.out_len + 256 > sizeof wl.out) wl_flush();
    wl_msg_start = wl.out_len;
    memcpy(wl.out + wl.out_len, &obj, 4);
    memcpy(wl.out + wl.out_len + 4, &op, 4);
    wl.out_len += 8;
}
static void wl_u32(uint32_t v) { memcpy(wl.out + wl.out_len, &v, 4); wl.out_len += 4; }
static void wl_str(const char *s) {
    uint32_t n = (uint32_t)strlen(s) + 1;
    wl_u32(n);
    memcpy(wl.out + wl.out_len, s, n);
    uint32_t pad = (n + 3) & ~3u;
    memset(wl.out + wl.out_len + n, 0, pad - n);
    wl.out_len += pad;
}
static void wl_end(void) {
    uint32_t size = (uint32_t)(wl.out_len - wl_msg_start), op;
    memcpy(&op, wl.out + wl_msg_start + 4, 4);
    op = (size << 16) | (op & 0xffff);
    memcpy(wl.out + wl_msg_start + 4, &op, 4);
}

static void wl_bind(uint32_t name, const char *iface, uint32_t ver, uint32_t id) {
    wl_begin(wl.registry, 0); wl_u32(name); wl_str(iface); wl_u32(ver); wl_u32(id); wl_end();
}

/* ── Event dispatch ──────────────────────────────────────────────────────── */

static void wl_key(uint32_t code, int pressed) {
    if (code < sizeof wl.held) {
        if (wl.held[code] == pressed) return;
        wl.held[code] = (uint8_t)pressed;
    }
    if (wl.keyq_w - wl.keyq_r >= 128) return;
    wl.keyq[wl.keyq_w++ % 128] = ((uint32_t)pressed << 16) | (code & 0xffff);
}

static void wl_event(uint32_t obj, uint32_t op, const uint8_t *a, uint32_t len) {
    uint32_t v0 = 0, v1 = 0, v2 = 0, v3 = 0;
    if (len >= 4) memcpy(&v0, a, 4);
    if (len >= 8) memcpy(&v1, a + 4, 4);
    if (len >= 12) memcpy(&v2, a + 8, 4);
    if (len >= 16) memcpy(&v3, a + 12, 4);

    if (obj == 1) {                                   /* wl_display */
        if (op == 0) {                                /* error */
            /* error(object, code, message): v2 is the string's length + NUL */
            size_t n = (len > 12 && v2 > 1 && v2 <= len - 12) ? v2 - 1 : 0;
            write(2, "[doom] wayland protocol error: ", 31);
            write(2, (const char *)a + 12, n);
            write(2, "\n", 1);
            wl.closed = 1;
        }
        return;
    }
    if (obj == wl.registry && op == 0) {              /* global(name, iface, ver) */
        uint32_t slen = v1;
        const char *iface = (const char *)a + 8;
        uint32_t ver;
        if (8 + ((slen + 3) & ~3u) + 4 > len) return;
        memcpy(&ver, a + 8 + ((slen + 3) & ~3u), 4);
        if (!strcmp(iface, "wl_compositor")) { wl.compositor_name = v0; wl.compositor_ver = ver; }
        else if (!strcmp(iface, "wl_shm")) wl.shm_name = v0;
        else if (!strcmp(iface, "xdg_wm_base")) wl.wm_name = v0;
        else if (!strcmp(iface, "wl_seat") && !wl.seat_name) { wl.seat_name = v0; wl.seat_ver = ver; }
        return;
    }
    if (obj == wl.sync_cb && op == 0) { wl.sync_done = 1; return; }
    if (obj == wl.frame_cb && op == 0) { wl.frame_cb = 0; return; }
    if (obj == wl.wm && op == 0) {                    /* ping -> pong */
        wl_begin(wl.wm, 3); wl_u32(v0); wl_end();
        return;
    }
    if (obj == wl.xdg_surface && op == 0) {           /* configure -> ack */
        wl_begin(wl.xdg_surface, 4); wl_u32(v0); wl_end();
        wl.configured = 1;
        return;
    }
    if (obj == wl.toplevel && op == 1) { wl.closed = 1; return; }   /* close */
    if (obj == wl.buffer[0] && op == 0) { wl.buffer_busy[0] = 0; return; }
    if (obj == wl.buffer[1] && op == 0) { wl.buffer_busy[1] = 0; return; }
    if (obj == wl.seat && op == 0) {                  /* capabilities */
        wl.seat_caps = (int)v0;
        if ((v0 & 2) && !wl.keyboard) {
            wl.keyboard = wl_new_id();
            wl_begin(wl.seat, 1); wl_u32(wl.keyboard); wl_end();
        }
        return;
    }
    if (obj == wl.keyboard && wl.keyboard) {
        if (op == 3) wl_key(v2, v3 == 1);             /* key(serial, time, key, state) */
        else if (op == 2) {                           /* leave: nothing stays held */
            for (uint32_t k = 0; k < sizeof wl.held; k++) if (wl.held[k]) wl_key(k, 0);
        }
        return;
    }
}

/* Read whatever is available (blocking only if `wait_ms` > 0) and dispatch it.
 * Returns -1 once the connection is gone. */
static int wl_dispatch(int wait_ms) {
    struct { int fd; short events, revents; } pfd = { wl.fd, 1, 0 };
    struct { long sec, nsec; } ts = { wait_ms / 1000, (long)(wait_ms % 1000) * 1000000L };
    long pr = wl_sys(WL_NR_PPOLL, (long)&pfd, 1, (long)&ts, 0, 8);
    if (pr < 0) return 0;
    if (pr == 0) return 0;
    if (pfd.revents & 0x18 && !(pfd.revents & 1)) { wl.closed = 1; return -1; } /* HUP/ERR */

    struct wl_iov iov = { wl.in + wl.in_len, sizeof wl.in - wl.in_len };
    uint8_t cbuf[WL_CMSG_HDR + 8 * sizeof(int)];
    struct wl_msghdr m;
    memset(&m, 0, sizeof m);
    memset(cbuf, 0, sizeof cbuf);
    m.iov = &iov; m.iovlen = 1;
    m.control = cbuf; m.controllen = sizeof cbuf;
    long r = wl_sys(WL_NR_RECVMSG, wl.fd, (long)&m, 0, 0, 0);
    if (r <= 0) { wl.closed = 1; return -1; }
    /* The only fd any event we handle carries is the keyboard keymap, which
     * DOOM never reads (it maps evdev codes itself). Close what arrives. */
    if (m.controllen >= WL_CMSG_HDR) {
        struct wl_cmsg *c = (struct wl_cmsg *)cbuf;
        if (c->level == WL_SOL_SOCKET && c->type == WL_SCM_RIGHTS && c->len > WL_CMSG_HDR) {
            size_t nfd = (c->len - WL_CMSG_HDR) / sizeof(int);
            for (size_t i = 0; i < nfd; i++) {
                int f; memcpy(&f, cbuf + WL_CMSG_HDR + i * sizeof(int), sizeof f);
                close(f);
            }
        }
    }
    wl.in_len += (size_t)r;

    size_t off = 0;
    while (wl.in_len - off >= 8) {
        uint32_t obj, opsz;
        memcpy(&obj, wl.in + off, 4);
        memcpy(&opsz, wl.in + off + 4, 4);
        uint32_t size = opsz >> 16;
        if (size < 8) { wl.closed = 1; return -1; }
        if (wl.in_len - off < size) break;
        wl_event(obj, opsz & 0xffff, wl.in + off + 8, size - 8);
        off += size;
    }
    memmove(wl.in, wl.in + off, wl.in_len - off);
    wl.in_len -= off;
    if (wl.out_len) wl_flush();
    return wl.closed ? -1 : 0;
}

/* Dispatch until `*flag` is set, up to ~5 s. */
static int wl_wait(int *flag) {
    for (int i = 0; i < 100 && !*flag && !wl.closed; i++) wl_dispatch(50);
    return *flag ? 0 : -1;
}

static void wl_roundtrip(void) {
    wl.sync_done = 0;
    wl.sync_cb = wl_new_id();
    wl_begin(1, 0); wl_u32(wl.sync_cb); wl_end();
    wl_flush();
    wl_wait(&wl.sync_done);
}

/* ── Public API (used by leandros_sdl.c) ─────────────────────────────────── */

static void wl_teardown(void) {
    if (wl.fd >= 0) close(wl.fd);
    wl.fd = -1;
    wl.active = 0;
}

/* Open a 960x600 window on $WAYLAND_DISPLAY. Returns 1 on success, 0 when there
 * is no display to use (DOOM then falls back to KMS / fbdev). */
static int wl_open(void) {
    const char *disp = wl_getenv("WAYLAND_DISPLAY");
    if (!disp || !*disp) return 0;
    const char *dir = wl_getenv("XDG_RUNTIME_DIR");

    struct { uint16_t family; char path[108]; } addr;
    memset(&addr, 0, sizeof addr);
    addr.family = 1; /* AF_UNIX */
    size_t dl = dir ? strlen(dir) : 0, nl = strlen(disp);
    if (disp[0] == '/') {
        if (nl >= sizeof addr.path) return 0;
        memcpy(addr.path, disp, nl);
    } else {
        if (!dir || dl + 1 + nl >= sizeof addr.path) return 0;
        memcpy(addr.path, dir, dl);
        addr.path[dl] = '/';
        memcpy(addr.path + dl + 1, disp, nl);
    }

    long fd = wl_sys(WL_NR_SOCKET, 1 /* AF_UNIX */, 1 /* SOCK_STREAM */, 0, 0, 0);
    if (fd < 0) return 0;
    wl.fd = (int)fd;
    if (wl_sys(WL_NR_CONNECT, fd, (long)&addr, sizeof addr, 0, 0) < 0) {
        write(2, "[doom] cannot connect to the Wayland display, using the console\n", 64);
        wl_teardown();
        return 0;
    }
    wl.next_id = 2;

    wl.registry = wl_new_id();
    wl_begin(1, 1); wl_u32(wl.registry); wl_end();
    wl_roundtrip();
    if (!wl.sync_done || !wl.compositor_name || !wl.shm_name || !wl.wm_name) {
        write(2, "[doom] Wayland display lacks wl_compositor/wl_shm/xdg_wm_base\n", 63);
        wl_teardown();
        return 0;
    }

    wl.compositor = wl_new_id();
    wl_bind(wl.compositor_name, "wl_compositor", wl.compositor_ver < 4 ? wl.compositor_ver : 4, wl.compositor);
    wl.shm = wl_new_id();
    wl_bind(wl.shm_name, "wl_shm", 1, wl.shm);
    wl.wm = wl_new_id();
    wl_bind(wl.wm_name, "xdg_wm_base", 1, wl.wm);
    if (wl.seat_name) {
        wl.seat = wl_new_id();
        wl_bind(wl.seat_name, "wl_seat", wl.seat_ver < 4 ? wl.seat_ver : 4, wl.seat);
    }

    wl.surface = wl_new_id();
    wl_begin(wl.compositor, 0); wl_u32(wl.surface); wl_end();
    wl.xdg_surface = wl_new_id();
    wl_begin(wl.wm, 2); wl_u32(wl.xdg_surface); wl_u32(wl.surface); wl_end();
    wl.toplevel = wl_new_id();
    wl_begin(wl.xdg_surface, 1); wl_u32(wl.toplevel); wl_end();
    wl_begin(wl.toplevel, 2); wl_str("DOOM"); wl_end();
    wl_begin(wl.toplevel, 3); wl_str("doom"); wl_end();
    /* A fixed-size game surface: ask a tiling layout not to stretch it. */
    wl_begin(wl.toplevel, 7); wl_u32(WL_W); wl_u32(WL_H); wl_end();   /* set_max_size */
    wl_begin(wl.toplevel, 8); wl_u32(WL_W); wl_u32(WL_H); wl_end();   /* set_min_size */
    wl_begin(wl.surface, 6); wl_end();                                /* commit */
    wl_flush();
    if (wl_wait(&wl.configured) < 0) {
        write(2, "[doom] the compositor never configured the window\n", 50);
        wl_teardown();
        return 0;
    }

    /* Two buffers in one shared-memory pool. */
    long mfd = wl_sys(WL_NR_MEMFD_CREATE, (long)"doom-shm", 1 /* MFD_CLOEXEC */, 0, 0, 0);
    if (mfd < 0 || wl_sys(WL_NR_FTRUNCATE, mfd, 2L * WL_BUFSZ, 0, 0, 0) < 0) {
        if (mfd >= 0) close((int)mfd);
        wl_teardown();
        return 0;
    }
    void *map = mmap(0, 2 * WL_BUFSZ, 3 /* RW */, 1 /* MAP_SHARED */, (int)mfd, 0);
    if (map == (void *)-1 || map == 0) { close((int)mfd); wl_teardown(); return 0; }
    wl.pixels = (uint8_t *)map;
    memset(wl.pixels, 0, 2 * WL_BUFSZ);

    wl.pool = wl_new_id();
    wl_begin(wl.shm, 0); wl_u32(wl.pool); wl_u32(2 * WL_BUFSZ); wl_end();  /* fd travels out of band */
    wl.out_fd = (int)mfd;
    wl_flush();
    close((int)mfd);
    for (int i = 0; i < 2; i++) {
        wl.buffer[i] = wl_new_id();
        wl_begin(wl.pool, 0);
        wl_u32(wl.buffer[i]); wl_u32((uint32_t)(i * WL_BUFSZ));
        wl_u32(WL_W); wl_u32(WL_H); wl_u32(WL_STRIDE); wl_u32(1 /* XRGB8888 */);
        wl_end();
    }
    wl_flush();
    wl_roundtrip();
    if (wl.closed) { wl_teardown(); return 0; }

    wl.active = 1;
    write(1, "[doom] drawing in a Wayland window\n", 35);
    return 1;
}

/* Present one 320x200 XRGB frame. A frame is dropped rather than queued while
 * the compositor has not asked for the next one, or both buffers are still in
 * its hands: DOOM's game clock does not depend on it, and the next frame
 * carries everything this one would have. */
static void wl_present(const uint32_t *src) {
    wl_dispatch(0);
    if (!wl.active || wl.closed || wl.frame_cb) return;
    int b = !wl.buffer_busy[0] ? 0 : !wl.buffer_busy[1] ? 1 : -1;
    if (b < 0) return;

    uint32_t *dst = (uint32_t *)(wl.pixels + (size_t)b * WL_BUFSZ);
    for (int y = 0; y < DOOMGENERIC_RESY; y++) {
        uint32_t *row = dst + (size_t)y * WL_SCALE * WL_W;
        const uint32_t *s = src + (size_t)y * DOOMGENERIC_RESX;
        for (int x = 0; x < DOOMGENERIC_RESX; x++) {
            uint32_t p = s[x];
            for (int k = 0; k < WL_SCALE; k++) row[x * WL_SCALE + k] = p;
        }
        for (int k = 1; k < WL_SCALE; k++) memcpy(row + (size_t)k * WL_W, row, WL_STRIDE);
    }

    wl_begin(wl.surface, 1); wl_u32(wl.buffer[b]); wl_u32(0); wl_u32(0); wl_end();  /* attach */
    wl_begin(wl.surface, 2); wl_u32(0); wl_u32(0); wl_u32(WL_W); wl_u32(WL_H); wl_end(); /* damage */
    wl.frame_cb = wl_new_id();
    wl_begin(wl.surface, 3); wl_u32(wl.frame_cb); wl_end();                         /* frame */
    wl_begin(wl.surface, 6); wl_end();                                              /* commit */
    wl.buffer_busy[b] = 1;
    wl_flush();
}

/* Next key event as (pressed << 16) | evdev code; -1 when none, -2 when the
 * window was closed or the compositor went away. */
static int wl_next_key(void) {
    wl_dispatch(0);
    if (wl.keyq_r != wl.keyq_w) return (int)wl.keyq[wl.keyq_r++ % 128];
    return wl.closed ? -2 : -1;
}

#endif /* LEANDROS_WAYLAND_H */
