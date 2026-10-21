/*
 * pollnarrow — a blocked reader of /dev/netctl or of the console parks on a
 * narrow poll tag, not the broadcast mask (lane pollnarrow).
 *
 * On the laptop both used to park with POLL_TAG_ALL, so every one of the
 * ~15k poll wakes/s in the system also woke leandros-sysbus' netctl-events
 * thread (read of /dev/netctl, ~3k wakes/s) and `login` (console stdin read,
 * ~3.3k wakes/s). See artifacts/notes/zerocopy-2026-10-19.md item 4.
 *
 *   pollnarrow [--input] [-x NAME]...
 *
 * Cases:
 *   netctl_read_narrow   a thread blocked in read(/dev/netctl) parks with a
 *                        mask that is not all ones; 1 s of unrelated AF_UNIX
 *                        and pipe traffic does not wake it (its `pwakes`
 *                        column in /proc/sysstat barely moves); an
 *                        NETCTL_REQUEST_SNAPSHOT on its open wakes it at once
 *   console_read_narrow  the same for a thread blocked in read(0) on the
 *                        console; a `CSI 6 n` written to the console (the
 *                        kernel queues the cursor report as console input)
 *                        wakes it with the reply
 *   console_read_alias   the blocked console read is not woken at all by pipe
 *                        and AF_UNIX traffic over ~200 objects (some keys of
 *                        which share its bloom bits) plus a line-echo child
 *                        on stdin/stdout pipes (lane gettywake)
 *   console_read_input   (--input only, driven by host:pollnarrowtest.py)
 *                        real console input wakes the parked reader: after
 *                        "PN_WANT_KEY" a keyboard key (QMP), after
 *                        "PN_WANT_SERIAL" a UART byte
 *
 * The console cases need fd 0 to be the machine console (the serial shell)
 * and are skipped otherwise; everything is skipped without /proc/sysstat (a
 * host kernel). Output: "<name>: PASS|FAIL ...|SKIP ..." per case and
 * "pollnarrow: N passed, M failed, K skipped". Exit status = failures.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int n_pass, n_fail, n_skip;
static const char *skips[16];
static int n_skips;

static void pass(const char *n) { printf("%s: PASS\n", n); fflush(stdout); n_pass++; }
static void skip(const char *n, const char *why) { printf("%s: SKIP %s\n", n, why); fflush(stdout); n_skip++; }
static void fail(const char *n, const char *fmt, ...) {
    va_list ap;
    printf("%s: FAIL ", n);
    va_start(ap, fmt); vprintf(fmt, ap); va_end(ap);
    printf("\n"); fflush(stdout);
    n_fail++;
}
static int skipped(const char *n) {
    for (int i = 0; i < n_skips; i++) if (!strcmp(skips[i], n)) { skip(n, "(-x)"); return 1; }
    return 0;
}
static long long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}
static void msleep(int ms) { struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L }; nanosleep(&ts, NULL); }

struct sysw { int found; char state; unsigned long long mask, pwakes; };

/* The `w` line of task `tid`:
 * `w tid tgid state sc port listen poll_mask in_sc_ns pwakes` (as in
 * udptest/anytag.c). */
static struct sysw sysstat_w(long tid) {
    static char buf[4 << 20]; /* /proc/sysstat passes 256 KiB on a long-lived system */
    struct sysw r = { 0, 0, 0, 0 };
    int fd = open("/proc/sysstat", O_RDONLY);
    if (fd < 0) return r;
    size_t len = 0;
    for (;;) {
        ssize_t n = read(fd, buf + len, sizeof buf - 1 - len);
        if (n <= 0) break;
        len += (size_t)n;
        if (len >= sizeof buf - 1) break;
    }
    close(fd);
    buf[len] = 0;
    for (char *line = buf; line && *line; ) {
        char *nl = strchr(line, '\n');
        if (nl) *nl = 0;
        long t = 0, tg = 0, sc = 0, lis = 0; unsigned long port = 0;
        char st = 0;
        unsigned long long mask = 0, insc = 0, pw = 0;
        if (line[0] == 'w' && line[1] == ' '
            && sscanf(line, "w %ld %ld %c %ld %lx %ld %llx %llu %llu",
                      &t, &tg, &st, &sc, &port, &lis, &mask, &insc, &pw) == 9
            && t == tid) {
            r.found = 1; r.state = st; r.mask = mask; r.pwakes = pw;
            return r;
        }
        line = nl ? nl + 1 : NULL;
    }
    return r;
}

/* A thread that blocks in read(fd) and collects what it reads. */
struct reader {
    int fd;
    volatile long tid;
    volatile int nread;      /* successful reads so far */
    volatile int stop;
    char buf[512];
    volatile int len;
    pthread_mutex_t mu;
};

static void *reader_thread(void *p) {
    struct reader *r = p;
    r->tid = syscall(SYS_gettid);
    while (!__atomic_load_n(&r->stop, __ATOMIC_SEQ_CST)) {
        char b[256];
        ssize_t n = read(r->fd, b, sizeof b);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) break;
        pthread_mutex_lock(&r->mu);
        int room = (int)sizeof r->buf - r->len;
        int k = n < room ? (int)n : room;
        memcpy(r->buf + r->len, b, (size_t)k);
        r->len += k;
        pthread_mutex_unlock(&r->mu);
        __atomic_add_fetch(&r->nread, 1, __ATOMIC_SEQ_CST);
    }
    return NULL;
}

static int reader_has(struct reader *r, char c) {
    pthread_mutex_lock(&r->mu);
    int hit = memchr(r->buf, c, (size_t)r->len) != NULL;
    pthread_mutex_unlock(&r->mu);
    return hit;
}

/* Wait (up to 3 s) until the reader is parked: sleeping with a non-zero mask. */
static struct sysw wait_parked(struct reader *r) {
    struct sysw w = { 0, 0, 0, 0 };
    for (int i = 0; i < 300; i++) {
        if (r->tid) { w = sysstat_w(r->tid); if (w.found && w.state == 'S' && w.mask) return w; }
        msleep(10);
    }
    return w;
}

/* ~1 s of poll traffic that has nothing to do with the reader: AF_UNIX round
 * trips (keyed data wakes) and pipe writes/reads. Returns the round trips. */
static long unrelated_traffic(int ms) {
    int us[2], pp[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, us) < 0 || pipe(pp) < 0) return -1;
    long n = 0;
    long long end = now_ms() + ms;
    char c = 'x';
    while (now_ms() < end) {
        for (int i = 0; i < 50; i++, n++) {
            if (write(us[0], &c, 1) != 1 || read(us[1], &c, 1) != 1) { n = -1; goto out; }
            if (write(pp[1], &c, 1) != 1 || read(pp[0], &c, 1) != 1) { n = -1; goto out; }
        }
    }
out:
    close(us[0]); close(us[1]); close(pp[0]); close(pp[1]);
    return n;
}

/* Traffic shaped like pop-launcher's and the WebExtensions process' on the
 * laptop, over MANY objects at once so that some of their keys share a bloom
 * bit with whatever the reader parks on (2 of 64 bits for the console: ~3 %
 * of all pipe / AF_UNIX keys): a forked line-echo child on stdin/stdout pipes
 * (pop-launcher's plugin protocol), plus ALIAS_N pipes and ALIAS_N connected
 * AF_UNIX stream pairs, each written and read in turn, both directions. Before
 * lane gettywake the console read parked with its two bits unrefined, and
 * every keyed wake landing on one of them woke it. Returns the round trips,
 * -1 on an io failure. */
#define ALIAS_N 96
static int al_pp[ALIAS_N][2], al_us[ALIAS_N][2], al_to[2] = { -1, -1 }, al_from[2] = { -1, -1 };
static int al_np, al_nu;
static pid_t al_child = -1;

/* Open the objects and start the echo child. Done outside the measured window:
 * creating and closing them is not the steady traffic this is about (a
 * socketpair's close and the child's SIGCHLD wake by bit or broadcast). */
static int alias_setup(void) {
    al_np = al_nu = 0;
    if (pipe(al_to) < 0) return -1;
    if (pipe(al_from) < 0) { close(al_to[0]); close(al_to[1]); al_to[0] = al_to[1] = -1; return -1; }
    al_child = fork();
    if (al_child == 0) {
        /* The echo plugin: read a request on stdin, answer it on stdout. */
        dup2(al_to[0], 0); dup2(al_from[1], 1);
        close(al_to[0]); close(al_to[1]); close(al_from[0]); close(al_from[1]);
        char b[256];
        for (;;) {
            ssize_t k = read(0, b, sizeof b);
            if (k <= 0) _exit(0);
            if (write(1, b, (size_t)k) != k) _exit(1);
        }
    }
    close(al_to[0]); close(al_from[1]);
    if (al_child < 0) return -1;
    for (; al_np < ALIAS_N; al_np++) if (pipe(al_pp[al_np]) < 0) return -1;
    for (; al_nu < ALIAS_N; al_nu++) if (socketpair(AF_UNIX, SOCK_STREAM, 0, al_us[al_nu]) < 0) return -1;
    return 0;
}

static void alias_teardown(void) {
    for (int i = 0; i < al_np; i++) { close(al_pp[i][0]); close(al_pp[i][1]); }
    for (int i = 0; i < al_nu; i++) { close(al_us[i][0]); close(al_us[i][1]); }
    if (al_to[1] >= 0) close(al_to[1]);
    if (al_from[0] >= 0) close(al_from[0]);
    if (al_child > 0) waitpid(al_child, NULL, 0);
    al_np = al_nu = 0; al_to[1] = al_from[0] = -1; al_child = -1;
}

static long aliasing_traffic(int ms) {
    long n = 0;
    long long end = now_ms() + ms;
    char c = 'x';
    static const char req[] = "{\"Search\":\"fire\"}\n";
    char rb[64];
    while (now_ms() < end) {
        if (write(al_to[1], req, sizeof req - 1) != (ssize_t)(sizeof req - 1)) return -1;
        size_t got = 0;
        while (got < sizeof req - 1) {
            ssize_t k = read(al_from[0], rb, sizeof rb);
            if (k <= 0) return -1;
            got += (size_t)k;
        }
        for (int i = 0; i < ALIAS_N; i++, n++) {
            if (write(al_pp[i][1], &c, 1) != 1 || read(al_pp[i][0], &c, 1) != 1) return -1;
            if (write(al_us[i][0], &c, 1) != 1 || read(al_us[i][1], &c, 1) != 1) return -1;
            if (write(al_us[i][1], &c, 1) != 1 || read(al_us[i][0], &c, 1) != 1) return -1;
        }
    }
    return n;
}

/* Common part: parked with a narrow mask, and quiet under `traffic`.
 * `allow` is the number of wake scans tolerated over the window. */
static int check_narrow_with(const char *n, struct reader *r, int nread0, unsigned long long allow,
                             long (*traffic)(int)) {
    struct sysw w = wait_parked(r);
    if (!w.found || w.state != 'S' || !w.mask) {
        fail(n, "reader never parked (found %d state %c mask %llx)", w.found, w.state ? w.state : '?', w.mask);
        return 1;
    }
    if (w.mask == ~0ULL) { fail(n, "parked with the broadcast mask (all ones)"); return 1; }
    long trips = traffic(1000);
    struct sysw w1 = sysstat_w(r->tid);
    unsigned long long dw = w1.pwakes - w.pwakes;
    printf("%s: mask %#llx, %ld round trips, %llu wakes\n", n, w.mask, trips, dw);
    if (trips < 0) { fail(n, "traffic io failed"); return 1; }
    if (r->nread != nread0) {
        /* Something real arrived meanwhile; the count is then not this
         * test's to judge. */
        printf("%s: note: the reader got input during the window\n", n);
        return 0;
    }
    if (!w1.found || dw > allow) {
        fail(n, "unrelated traffic woke the reader %llu times (%ld round trips)", dw, trips);
        return 1;
    }
    return 0;
}

static int check_narrow(const char *n, struct reader *r, int nread0, unsigned long long allow) {
    return check_narrow_with(n, r, nread0, allow, unrelated_traffic);
}

static int wait_nread(struct reader *r, int above, int ms) {
    for (int i = 0; i < ms / 10; i++) { if (r->nread > above) return 1; msleep(10); }
    return r->nread > above;
}

/* ── /dev/netctl ───────────────────────────────────────────────────────── */

#define NETCTL_REQUEST_SNAPSHOT 0x4e06 /* _IO('N', 6) */

static void netctl_read_narrow(void) {
    const char *n = "netctl_read_narrow";
    if (skipped(n)) return;
    int fd = open("/dev/netctl", O_RDONLY);
    if (fd < 0) { skip(n, "(no /dev/netctl)"); return; }
    /* Drain the open's snapshot, so the reader's read blocks. */
    char b[4096];
    for (int i = 0; i < 1000; i++) {
        struct pollfd pf = { fd, POLLIN, 0 };
        if (poll(&pf, 1, 0) != 1 || !(pf.revents & POLLIN)) break;
        if (read(fd, b, sizeof b) <= 0) break;
    }
    struct reader r = { .fd = fd };
    pthread_mutex_init(&r.mu, NULL);
    pthread_t th;
    if (pthread_create(&th, NULL, reader_thread, &r) != 0) { fail(n, "pthread_create"); close(fd); return; }
    /* No deadline backstop on this park: any wake scan is a real wake. */
    int bad = check_narrow(n, &r, 0, 10);
    /* Liveness: a snapshot request on this open queues events and wakes it. */
    int before = r.nread;
    long long t0 = now_ms();
    if (ioctl(fd, NETCTL_REQUEST_SNAPSHOT, 0) != 0) {
        if (!bad) fail(n, "NETCTL_REQUEST_SNAPSHOT: errno %d", errno);
        bad = 1;
    } else if (!wait_nread(&r, before, 2000)) {
        if (!bad) fail(n, "the snapshot did not wake the blocked read within 2 s");
        bad = 1;
    } else {
        printf("%s: woke %lld ms after the snapshot request\n", n, now_ms() - t0);
    }
    __atomic_store_n(&r.stop, 1, __ATOMIC_SEQ_CST);
    /* Release a still-blocked reader. */
    ioctl(fd, NETCTL_REQUEST_SNAPSHOT, 0);
    pthread_join(th, NULL);
    close(fd);
    if (!bad) pass(n);
}

/* ── console ───────────────────────────────────────────────────────────── */

/* Wait up to `ms` for byte `c` in the reader's buffer, printing `marker`
 * (the host's cue) first when non-NULL. */
static int console_wait_byte(struct reader *r, const char *marker, char c, int ms) {
    if (marker) { printf("%s\n", marker); fflush(stdout); }
    for (int i = 0; i < ms / 10; i++) { if (reader_has(r, c)) return 1; msleep(10); }
    return reader_has(r, c);
}

static void console_cases(int with_input) {
    const char *n = "console_read_narrow";
    const char *ni = "console_read_input";
    if (skipped(n)) { if (with_input) skip(ni, "(console_read_narrow skipped)"); return; }
    /* The console, not a pty or a file: a tty that is not /dev/pts/N. */
    char *tn = isatty(0) ? ttyname(0) : NULL;
    if (!isatty(0) || (tn && !strncmp(tn, "/dev/pts/", 9))) {
        skip(n, "(fd 0 is not the console)");
        if (with_input) skip(ni, "(fd 0 is not the console)");
        return;
    }
    struct reader r = { .fd = 0 };
    pthread_mutex_init(&r.mu, NULL);
    pthread_t th;
    if (pthread_create(&th, NULL, reader_thread, &r) != 0) { fail(n, "pthread_create"); return; }
    /* The park keeps its 10 ms deadline as a backstop; deadline expiries are
     * not wake scans, so a narrow park sees ~0 of them here. Broadcast saw
     * one per traffic wake: thousands. */
    int bad = check_narrow(n, &r, 0, 20);
    /* Liveness through the synthetic-input ring: the kernel answers a cursor
     * position query written to the console by queueing ESC [ r ; c R. */
    int before = r.nread;
    long long t0 = now_ms();
    if (write(1, "\033[6n", 4) != 4) {
        if (!bad) fail(n, "write CSI 6 n: errno %d", errno);
        bad = 1;
    } else if (!wait_nread(&r, before, 2000) || !console_wait_byte(&r, NULL, 'R', 1000)) {
        if (!bad) fail(n, "the cursor report did not wake the blocked console read within 3 s");
        bad = 1;
    } else {
        printf("%s: woke %lld ms after CSI 6 n\n", n, now_ms() - t0);
    }
    if (!bad) pass(n);

    /* The getty case of lane gettywake: pipe and AF_UNIX traffic over enough
     * objects that some of their keys share the park's bloom bits must not
     * wake the blocked console read at all (deadline expiries are not wake
     * scans). Real input still does: console_read_input below. */
    const char *na = "console_read_alias";
    if (!skipped(na)) {
        if (alias_setup() < 0) fail(na, "setup: errno %d", errno);
        else if (!check_narrow_with(na, &r, r.nread, 0, aliasing_traffic)) pass(na);
        alias_teardown();
    }

    if (with_input && !skipped(ni)) {
        int ibad = 0;
        struct sysw w = wait_parked(&r);
        if (!w.found || w.mask == ~0ULL) { fail(ni, "reader not parked narrow (mask %llx)", w.mask); ibad = 1; }
        long long t1 = now_ms();
        if (!ibad && !console_wait_byte(&r, "PN_WANT_KEY", 'k', 15000)) {
            fail(ni, "a keyboard 'k' did not reach the blocked console read within 15 s"); ibad = 1;
        } else if (!ibad) {
            printf("%s: key arrived after %lld ms\n", ni, now_ms() - t1);
        }
        long long t2 = now_ms();
        if (!ibad && !console_wait_byte(&r, "PN_WANT_SERIAL", 's', 15000)) {
            fail(ni, "a serial 's' did not reach the blocked console read within 15 s"); ibad = 1;
        } else if (!ibad) {
            printf("%s: serial byte arrived after %lld ms\n", ni, now_ms() - t2);
        }
        if (!ibad) pass(ni);
    }

    /* Release the reader: it is parked in read(0) and only console input
     * ends that, so ask for one more cursor report. */
    __atomic_store_n(&r.stop, 1, __ATOMIC_SEQ_CST);
    int last = r.nread;
    for (int i = 0; i < 5 && r.nread == last; i++) {
        (void)!write(1, "\033[6n", 4);
        wait_nread(&r, last, 1000);
    }
    pthread_join(th, NULL);
}

/* ── ppoll vs epoll_wait parks (lane ppollcost) ──────────────────────────
 * relibc's poll() moved from a per-call epoll instance to the kernel's ppoll
 * and Firefox's kernel time rose ~16 %, with ALL-mask wakes 34-181/s ->
 * 706/s. A thread parked in ppoll on a set GLib/NSPR-like fds must park as
 * narrow as the same set parked in epoll_wait: not all ones, and not woken
 * by unrelated AF_UNIX / pipe traffic. */
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/timerfd.h>
#include <netinet/in.h>

enum { PW_PPOLL, PW_EPOLL };
#define PW_MAXFD 8
struct pwaiter {
    int mode, nfd, fds[PW_MAXFD];
    short ev[PW_MAXFD];
    volatile long tid;
    volatile int wakes_ret; /* calls that returned */
    volatile int stop;
};

static void *pwaiter_thread(void *p) {
    struct pwaiter *w = p;
    w->tid = syscall(SYS_gettid);
    while (!__atomic_load_n(&w->stop, __ATOMIC_SEQ_CST)) {
        if (w->mode == PW_PPOLL) {
            struct pollfd pf[PW_MAXFD];
            for (int i = 0; i < w->nfd; i++) { pf[i].fd = w->fds[i]; pf[i].events = w->ev[i]; pf[i].revents = 0; }
            long r = syscall(SYS_ppoll, pf, (long)w->nfd, NULL, NULL, 8L);
            if (r < 0 && errno != EINTR) break;
        } else {
            int ep = epoll_create1(EPOLL_CLOEXEC);
            for (int i = 0; i < w->nfd; i++) {
                struct epoll_event e = { .events = (unsigned)w->ev[i], .data.u32 = (unsigned)i };
                epoll_ctl(ep, EPOLL_CTL_ADD, w->fds[i], &e);
            }
            struct epoll_event out[PW_MAXFD];
            int r = epoll_wait(ep, out, PW_MAXFD, -1);
            close(ep);
            if (r < 0 && errno != EINTR) break;
        }
        __atomic_add_fetch(&w->wakes_ret, 1, __ATOMIC_SEQ_CST);
        char b[64];
        for (int i = 0; i < w->nfd; i++) (void)!read(w->fds[i], b, sizeof b); /* stop flag wake drains */
    }
    return NULL;
}

/* One fd set, as `kind`: 0 = AF_UNIX stream, 1 = mixed (eventfd, pipe,
 * AF_UNIX stream + dgram, UDP, timerfd). Peers stay open in `keep`. */
static int pw_build(struct pwaiter *w, int kind, int *keep, int *nkeep) {
    int sv[2], pp[2];
    w->nfd = 0; *nkeep = 0;
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, sv) < 0) return -1;
    w->fds[w->nfd] = sv[0]; w->ev[w->nfd++] = POLLIN; keep[(*nkeep)++] = sv[1];
    if (kind == 0) return 0;
    int e = eventfd(0, EFD_NONBLOCK);
    if (e >= 0) { w->fds[w->nfd] = e; w->ev[w->nfd++] = POLLIN; }
    if (pipe2(pp, O_NONBLOCK) == 0) { w->fds[w->nfd] = pp[0]; w->ev[w->nfd++] = POLLIN; keep[(*nkeep)++] = pp[1]; }
    if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK, 0, sv) == 0) {
        w->fds[w->nfd] = sv[0]; w->ev[w->nfd++] = POLLIN; keep[(*nkeep)++] = sv[1];
    }
    int u = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    if (u >= 0) {
        struct sockaddr_in a = { .sin_family = AF_INET, .sin_addr.s_addr = htonl(0x7f000001) };
        if (bind(u, (struct sockaddr *)&a, sizeof a) == 0) { w->fds[w->nfd] = u; w->ev[w->nfd++] = POLLIN; }
        else close(u);
    }
    int t = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    if (t >= 0) { w->fds[w->nfd] = t; w->ev[w->nfd++] = POLLIN; }
    return 0;
}

static void pw_case(const char *n, int mode, int kind, unsigned long long allow) {
    if (skipped(n)) { skip(n, "(-x)"); return; }
    struct pwaiter w; memset(&w, 0, sizeof w); w.mode = mode;
    int keep[PW_MAXFD], nkeep = 0;
    if (pw_build(&w, kind, keep, &nkeep) < 0) { fail(n, "setup failed"); return; }
    pthread_t th;
    if (pthread_create(&th, NULL, pwaiter_thread, &w) != 0) { fail(n, "pthread_create"); return; }
    struct sysw s0 = { 0, 0, 0, 0 };
    for (int i = 0; i < 300; i++) {
        if (w.tid) { s0 = sysstat_w(w.tid); if (s0.found && s0.state == 'S' && s0.mask) break; }
        msleep(10);
    }
    int ret0 = w.wakes_ret;
    int bad = 0;
    if (!s0.found || s0.state != 'S' || !s0.mask) {
        fail(n, "waiter never parked (found %d state %c mask %llx)", s0.found, s0.state ? s0.state : '?', s0.mask);
        bad = 1;
    } else {
        long trips = unrelated_traffic(1000);
        struct sysw s1 = sysstat_w(w.tid);
        unsigned long long dw = s1.pwakes - s0.pwakes;
        printf("%s: nfd %d mask %#llx, %ld round trips, %llu wakes, %d returns\n",
               n, w.nfd, s0.mask, trips, dw, w.wakes_ret - ret0);
        if (s0.mask == ~0ULL) { fail(n, "parked with the broadcast mask (all ones)"); bad = 1; }
        else if (trips < 0) { fail(n, "traffic io failed"); bad = 1; }
        else if (w.wakes_ret != ret0) { fail(n, "returned %d times with nothing ready", w.wakes_ret - ret0); bad = 1; }
        else if (dw > allow) { fail(n, "unrelated traffic woke the waiter %llu times (%ld round trips)", dw, trips); bad = 1; }
    }
    /* Wake it for real: the AF_UNIX peer. */
    __atomic_store_n(&w.stop, 1, __ATOMIC_SEQ_CST);
    char c = 'q';
    (void)!write(keep[0], &c, 1);
    long long end = now_ms() + 3000;
    while (w.wakes_ret == ret0 && now_ms() < end) msleep(5);
    if (w.wakes_ret == ret0) {
        if (!bad) fail(n, "a write to its socket did not wake it");
        /* leak the thread rather than hang */
    } else {
        pthread_join(th, NULL);
        if (!bad) pass(n);
    }
    for (int i = 0; i < w.nfd; i++) close(w.fds[i]);
    for (int i = 0; i < nkeep; i++) close(keep[i]);
}

/* ---- ppoll vs unrelated epoll_ctl churn (lane ppollcost2) ----
 * A ppoll ping-pong on the mixed fd set while another thread loops
 * EPOLL_CTL_MOD on an epoll instance the ppoll set does not contain. Every
 * successful epoll_ctl bumps a global generation; if ppoll treats that as
 * "my interest set may have changed", each park is cancelled and the whole
 * set re-probed. Probes per ppoll call (from /proc/wakestat "# pollx") must
 * stay near the no-churn baseline. */
static int wakestat_po(unsigned long long *calls, unsigned long long *probes) {
    int fd = open("/proc/wakestat", O_RDONLY);
    if (fd < 0) return -1;
    static char buf[16384];
    int n = (int)read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n <= 0) return -1;
    buf[n] = 0;
    char *l = strstr(buf, "# pollx calls ");
    if (!l || sscanf(l, "# pollx calls %llu probes %llu", calls, probes) != 2) return -1;
    return 0;
}

struct pp_ping { struct pwaiter w; int peer; volatile int stop; volatile long rounds; };
static void *pp_ponger(void *p) {
    struct pp_ping *g = p;
    char b[64];
    while (!__atomic_load_n(&g->stop, __ATOMIC_SEQ_CST)) {
        struct pollfd pf[PW_MAXFD];
        for (int i = 0; i < g->w.nfd; i++) { pf[i].fd = g->w.fds[i]; pf[i].events = g->w.ev[i]; pf[i].revents = 0; }
        long r = syscall(SYS_ppoll, pf, (long)g->w.nfd, NULL, NULL, 8L);
        if (r < 0 && errno != EINTR) break;
        if (r > 0 && (pf[0].revents & POLLIN)) {
            if (read(g->w.fds[0], b, sizeof b) > 0) (void)!write(g->w.fds[0], "p", 1);
        }
    }
    return NULL;
}
struct ctl_churn { int ep, fd; volatile int stop; volatile long n; };
static void *ctl_churner(void *p) {
    struct ctl_churn *c = p;
    unsigned k = 0;
    while (!__atomic_load_n(&c->stop, __ATOMIC_SEQ_CST)) {
        struct epoll_event e = { .events = (k++ & 1) ? EPOLLIN : (EPOLLIN | EPOLLET) };
        if (epoll_ctl(c->ep, EPOLL_CTL_MOD, c->fd, &e) == 0) c->n++;
    }
    return NULL;
}
/* One measurement: `ms` of ping-pong; returns probes per ppoll call x100. */
static long pp_measure(struct pp_ping *g, int ms, long *rounds, int *ok) {
    unsigned long long c0, p0, c1, p1;
    *ok = 0;
    if (wakestat_po(&c0, &p0) < 0) return -1;
    long long end = now_ms() + ms;
    long n = 0;
    char b[8];
    while (now_ms() < end) {
        if (write(g->peer, "x", 1) != 1) return -1;
        if (read(g->peer, b, 1) != 1) return -1; /* blocking: the peer is not polled */
        n++;
    }
    if (wakestat_po(&c1, &p1) < 0) return -1;
    *rounds = n;
    if (c1 <= c0) return -1;
    *ok = 1;
    return (long)((p1 - p0) * 100 / (c1 - c0));
}
static void ppoll_ctl_churn_case(void) {
    const char *n = "ppoll_ctl_churn";
    if (skipped(n)) { skip(n, "(-x)"); return; }
    unsigned long long a, b;
    if (wakestat_po(&a, &b) < 0) { skip(n, "(no /proc/wakestat pollx line)"); return; }
    struct pp_ping g; memset(&g, 0, sizeof g);
    int keep[PW_MAXFD], nkeep = 0;
    if (pw_build(&g.w, 1, keep, &nkeep) < 0) { fail(n, "setup failed"); return; }
    g.peer = keep[0];
    int fl = fcntl(g.peer, F_GETFL); fcntl(g.peer, F_SETFL, fl & ~O_NONBLOCK);
    struct ctl_churn c = { .ep = epoll_create1(EPOLL_CLOEXEC), .fd = eventfd(0, EFD_NONBLOCK) };
    struct epoll_event e = { .events = EPOLLIN };
    if (c.ep < 0 || c.fd < 0 || epoll_ctl(c.ep, EPOLL_CTL_ADD, c.fd, &e) < 0) { fail(n, "epoll setup"); return; }
    pthread_t pt, ct;
    if (pthread_create(&pt, NULL, pp_ponger, &g) != 0) { fail(n, "pthread_create"); return; }
    long r0 = 0, r1 = 0; int ok0, ok1;
    msleep(50);
    long base = pp_measure(&g, 700, &r0, &ok0);
    if (pthread_create(&ct, NULL, ctl_churner, &c) != 0) { fail(n, "pthread_create"); return; }
    msleep(20);
    long churn = pp_measure(&g, 700, &r1, &ok1);
    __atomic_store_n(&c.stop, 1, __ATOMIC_SEQ_CST);
    pthread_join(ct, NULL);
    __atomic_store_n(&g.stop, 1, __ATOMIC_SEQ_CST);
    (void)!write(g.peer, "q", 1);
    pthread_join(pt, NULL);
    printf("%s: nfd %d, baseline %ld.%02ld probes/call (%ld rounds), with %ld epoll_ctl MODs %ld.%02ld probes/call (%ld rounds)\n",
           n, g.w.nfd, base / 100, base % 100, r0, c.n, churn / 100, churn % 100, r1);
    if (!ok0 || !ok1 || base < 0 || churn < 0) fail(n, "measurement failed");
    else if (c.n < 1000) fail(n, "churn thread made only %ld MODs", c.n);
    else if (churn > base * 11 / 10 + 50) fail(n, "unrelated epoll_ctl churn raised ppoll probes/call %ld.%02ld -> %ld.%02ld",
                                                base / 100, base % 100, churn / 100, churn % 100);
    else pass(n);
    close(c.ep); close(c.fd);
    for (int i = 0; i < g.w.nfd; i++) close(g.w.fds[i]);
    for (int i = 0; i < nkeep; i++) close(keep[i]);
}

/* ---- per-call cost: ppoll vs per-call epoll vs persistent epoll (lane ppollcost3) ----
 * The same ping-pong as ppoll_ctl_churn, the ponger waiting in (0) raw
 * SYS_ppoll, (1) the old relibc poll() shape: epoll_create1 + ADD per fd +
 * epoll_wait + close per call, (2) one persistent epoll. Prints us per round
 * trip and probes per ppoll call; informational (never fails on timing). */
struct pc_ping { struct pp_ping g; int mode; };
static void *pc_ponger(void *p) {
    struct pc_ping *c = p; struct pp_ping *g = &c->g;
    char b[64];
    int pep = -1;
    if (c->mode == 2) {
        pep = epoll_create1(EPOLL_CLOEXEC);
        for (int i = 0; i < g->w.nfd; i++) {
            struct epoll_event e = { .events = (unsigned)g->w.ev[i], .data.u32 = (unsigned)i };
            epoll_ctl(pep, EPOLL_CTL_ADD, g->w.fds[i], &e);
        }
    }
    while (!__atomic_load_n(&g->stop, __ATOMIC_SEQ_CST)) {
        int in0 = 0;
        if (c->mode == 0) {
            struct pollfd pf[PW_MAXFD];
            for (int i = 0; i < g->w.nfd; i++) { pf[i].fd = g->w.fds[i]; pf[i].events = g->w.ev[i]; pf[i].revents = 0; }
            long r = syscall(SYS_ppoll, pf, (long)g->w.nfd, NULL, NULL, 8L);
            if (r < 0 && errno != EINTR) break;
            in0 = r > 0 && (pf[0].revents & POLLIN);
        } else {
            int ep = pep;
            if (c->mode == 1) {
                ep = epoll_create1(EPOLL_CLOEXEC);
                for (int i = 0; i < g->w.nfd; i++) {
                    struct epoll_event e = { .events = (unsigned)g->w.ev[i], .data.u32 = (unsigned)i };
                    epoll_ctl(ep, EPOLL_CTL_ADD, g->w.fds[i], &e);
                }
            }
            struct epoll_event ev[PW_MAXFD];
            int r = epoll_wait(ep, ev, PW_MAXFD, -1);
            for (int k = 0; k < r; k++) if (ev[k].data.u32 == 0 && (ev[k].events & EPOLLIN)) in0 = 1;
            if (c->mode == 1) close(ep);
            if (r < 0 && errno != EINTR) break;
        }
        if (in0 && read(g->w.fds[0], b, sizeof b) > 0) (void)!write(g->w.fds[0], "p", 1);
    }
    if (pep >= 0) close(pep);
    return NULL;
}
static void ppoll_cost_case(void) {
    const char *n = "ppoll_cost";
    static const char *mn[3] = { "ppoll", "epoll_percall", "epoll_persist" };
    if (skipped(n)) { skip(n, "(-x)"); return; }
    unsigned long long a, b;
    if (wakestat_po(&a, &b) < 0) { skip(n, "(no /proc/wakestat pollx line)"); return; }
    for (int m = 0; m < 3; m++) {
        struct pc_ping c; memset(&c, 0, sizeof c); c.mode = m;
        int keep[PW_MAXFD], nkeep = 0;
        if (pw_build(&c.g.w, 1, keep, &nkeep) < 0) { fail(n, "setup failed"); return; }
        c.g.peer = keep[0];
        int fl = fcntl(c.g.peer, F_GETFL); fcntl(c.g.peer, F_SETFL, fl & ~O_NONBLOCK);
        pthread_t pt;
        if (pthread_create(&pt, NULL, pc_ponger, &c) != 0) { fail(n, "pthread_create"); return; }
        msleep(50);
        long best = 0, pr = -1; int ok = 0;
        for (int rep = 0; rep < 3; rep++) {
            long r = 0; int o;
            long q = pp_measure(&c.g, 500, &r, &o);
            if (r > best) { best = r; if (o) { pr = q; ok = 1; } }
        }
        __atomic_store_n(&c.g.stop, 1, __ATOMIC_SEQ_CST);
        (void)!write(c.g.peer, "q", 1);
        pthread_join(pt, NULL);
        long ns = best > 0 ? 500000000L / best : -1;
        if (m == 0) printf("%s: %s nfd %d, %ld.%02ld us/round trip (%ld rounds/500 ms), %ld.%02ld probes/call\n",
                           n, mn[m], c.g.w.nfd, ns / 1000, (ns % 1000) / 10, best, ok ? pr / 100 : -1, ok ? pr % 100 : 0);
        else printf("%s: %s nfd %d, %ld.%02ld us/round trip (%ld rounds/500 ms)\n",
                    n, mn[m], c.g.w.nfd, ns / 1000, (ns % 1000) / 10, best);
        for (int i = 0; i < c.g.w.nfd; i++) close(c.g.w.fds[i]);
        for (int i = 0; i < nkeep; i++) close(keep[i]);
    }
    pass(n);
}

static void ppoll_cases(void) {
    pw_case("epoll_park_unix",  PW_EPOLL, 0, 5);
    pw_case("ppoll_park_unix",  PW_PPOLL, 0, 5);
    pw_case("epoll_park_mixed", PW_EPOLL, 1, 5);
    pw_case("ppoll_park_mixed", PW_PPOLL, 1, 5);
    ppoll_ctl_churn_case();
    ppoll_cost_case();
}

int main(int argc, char **argv) {
    int with_input = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--input")) with_input = 1;
        else if (!strcmp(argv[i], "-x") && i + 1 < argc && n_skips < 16) skips[n_skips++] = argv[++i];
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    int sfd = open("/proc/sysstat", O_RDONLY);
    if (sfd < 0) {
        skip("netctl_read_narrow", "(no /proc/sysstat)");
        skip("console_read_narrow", "(no /proc/sysstat)");
    } else {
        close(sfd);
        netctl_read_narrow();
        console_cases(with_input);
        ppoll_cases();
    }
    printf("pollnarrow: %d passed, %d failed, %d skipped\n", n_pass, n_fail, n_skip);
    return n_fail > 125 ? 125 : n_fail;
}
