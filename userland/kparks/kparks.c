/*
 * kparks — kernel poll parks that used to take the broadcast mask now park
 * narrow (lane kparks, artifacts/notes/broadcast-audit-2026-10-19.md lane 4).
 *
 *   kparks [--input] [-x NAME]...
 *
 * Cases:
 *   console_poll_narrow  a thread in poll({0, POLLIN}, 1, -1) and one in
 *                        epoll_wait on fd 0 (the machine console) both park
 *                        with a mask that is not all ones; 1 s of unrelated
 *                        AF_UNIX and pipe traffic wakes neither (`pwakes` in
 *                        /proc/sysstat stays ~0); a `CSI 6 n` written to the
 *                        console (the kernel queues the cursor report as
 *                        console input) wakes both
 *   console_poll_input   (--input only, driven by host:kparkstest.py) real
 *                        console input wakes both parked waiters: after
 *                        "KP_WANT_KEY" a keyboard key (QMP), after
 *                        "KP_WANT_SERIAL" a UART byte
 *   empty_poll_sleep     poll(NULL, 0, 100), poll() of only negative fds and
 *                        select(0, ...) each sleep ~100 ms and take 0 poll
 *                        wakes during unrelated traffic; the park's mask is
 *                        0 (deadline and signals only), and a signal still
 *                        ends poll(NULL, 0, -1) with EINTR
 *   vt_waitactive        a thread in VT_WAITACTIVE(2) parks narrow, unrelated
 *                        traffic does not wake it, and a VT_ACTIVATE(2) from
 *                        another process returns it within 30 ms; then the
 *                        same back to the original VT
 *
 * The console cases need fd 0 to be the machine console (the serial shell)
 * and are skipped otherwise; everything is skipped without /proc/sysstat.
 * Output: "<name>: PASS|FAIL ...|SKIP ..." per case and
 * "kparks: N passed, M failed, K skipped". Exit status = failures.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/select.h>
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
 * `w tid tgid state sc port listen poll_mask in_sc_ns pwakes`. */
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

/* Wait (up to 3 s) until `*tid` is sleeping with a non-zero mask. */
static struct sysw wait_parked(volatile long *tid) {
    struct sysw w = { 0, 0, 0, 0 };
    for (int i = 0; i < 300; i++) {
        if (*tid) { w = sysstat_w(*tid); if (w.found && w.state == 'S' && w.mask) return w; }
        msleep(10);
    }
    return w;
}

/* `ms` of poll traffic that has nothing to do with the waiters: AF_UNIX round
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

/* ── console in poll and epoll ─────────────────────────────────────────── */

/* A thread that waits for fd 0 to become readable, once per round, without
 * reading it: the main thread consumes the input, then opens the next round. */
struct waiter {
    int use_epoll;
    int ep;
    volatile long tid;
    volatile int round;       /* set by main: wait once per value */
    volatile int woke_round;  /* last round whose wait returned ready */
    volatile long long woke_ms;
    volatile int stop;
    volatile int err;
};

static void *waiter_thread(void *p) {
    struct waiter *w = p;
    w->tid = syscall(SYS_gettid);
    int done = 0;
    while (!__atomic_load_n(&w->stop, __ATOMIC_SEQ_CST)) {
        int r = __atomic_load_n(&w->round, __ATOMIC_SEQ_CST);
        if (r == done) { msleep(1); continue; }
        int got;
        if (w->use_epoll) {
            struct epoll_event ev;
            got = epoll_wait(w->ep, &ev, 1, -1);
        } else {
            struct pollfd pf = { 0, POLLIN, 0 };
            got = poll(&pf, 1, -1);
            if (got == 1 && !(pf.revents & POLLIN)) { w->err = pf.revents; got = 0; }
        }
        if (got < 0 && errno == EINTR) continue;
        if (got != 1) { if (!w->err) w->err = got < 0 ? errno : -1; break; }
        w->woke_ms = now_ms();
        __atomic_store_n(&w->woke_round, r, __ATOMIC_SEQ_CST);
        done = r;
    }
    return NULL;
}

/* Read console bytes until `c` (up to 3 s); input is known to be ready. */
static int drain_until(char c) {
    long long end = now_ms() + 3000;
    while (now_ms() < end) {
        struct pollfd pf = { 0, POLLIN, 0 };
        if (poll(&pf, 1, 100) != 1) continue;
        char b[64];
        ssize_t n = read(0, b, sizeof b);
        if (n > 0 && memchr(b, c, (size_t)n)) return 1;
    }
    return 0;
}

/* Wait until both waiters returned for `round` (up to `ms`). */
static int both_woke(struct waiter *w, int round, int ms) {
    for (int i = 0; i < ms; i++) {
        if (w[0].woke_round == round && w[1].woke_round == round) return 1;
        if (w[0].err || w[1].err) return 0;
        msleep(1);
    }
    return 0;
}

static const char *wname(struct waiter *w) { return w->use_epoll ? "epoll" : "poll"; }

/* One stimulus round: both waiters parked narrow, the stimulus, both woken,
 * the input consumed. `quiet` also runs the unrelated-traffic window first. */
static int console_round(const char *n, struct waiter *w, int round, int quiet,
                         const char *marker, const char *seq, char last, int ms) {
    for (int i = 0; i < 2; i++) __atomic_store_n(&w[i].round, round, __ATOMIC_SEQ_CST);
    struct sysw p[2];
    for (int i = 0; i < 2; i++) {
        p[i] = wait_parked(&w[i].tid);
        if (!p[i].found || p[i].state != 'S' || !p[i].mask) {
            fail(n, "%s waiter never parked (found %d state %c mask %llx, err %d)", wname(&w[i]),
                 p[i].found, p[i].state ? p[i].state : '?', p[i].mask, w[i].err);
            return 1;
        }
        if (p[i].mask == ~0ULL) { fail(n, "%s waiter parked with the broadcast mask", wname(&w[i])); return 1; }
    }
    if (quiet) {
        long trips = unrelated_traffic(1000);
        for (int i = 0; i < 2; i++) {
            struct sysw q = sysstat_w(w[i].tid);
            unsigned long long dw = q.pwakes - p[i].pwakes;
            printf("%s: %s mask %#llx, %ld round trips, %llu wakes\n", n, wname(&w[i]), p[i].mask, trips, dw);
            if (trips < 0) { fail(n, "traffic io failed"); return 1; }
            if (w[i].woke_round == round) {
                printf("%s: note: console input arrived during the window\n", n);
                continue;
            }
            if (!q.found || dw > 10) {
                fail(n, "unrelated traffic woke the %s waiter %llu times", wname(&w[i]), dw);
                return 1;
            }
        }
    }
    long long t0 = now_ms();
    if (marker) { printf("%s\n", marker); fflush(stdout); }
    if (seq && write(1, seq, strlen(seq)) != (ssize_t)strlen(seq)) { fail(n, "write: errno %d", errno); return 1; }
    if (!both_woke(w, round, ms)) {
        fail(n, "%s did not wake both waiters within %d ms (poll round %d err %d, epoll round %d err %d)",
             marker ? marker : "the cursor report", ms, w[0].woke_round, w[0].err, w[1].woke_round, w[1].err);
        drain_until(last);
        return 1;
    }
    printf("%s: %s woke poll after %lld ms, epoll after %lld ms\n", n, marker ? marker : "CSI 6 n",
           w[0].woke_ms - t0, w[1].woke_ms - t0);
    if (!drain_until(last)) { fail(n, "the input ('%c') was not readable after the wake", last); return 1; }
    return 0;
}

static void console_cases(int with_input) {
    const char *n = "console_poll_narrow";
    const char *ni = "console_poll_input";
    if (skipped(n)) { if (with_input) skip(ni, "(console_poll_narrow skipped)"); return; }
    char *tn = isatty(0) ? ttyname(0) : NULL;
    if (!isatty(0) || (tn && !strncmp(tn, "/dev/pts/", 9))) {
        skip(n, "(fd 0 is not the console)");
        if (with_input) skip(ni, "(fd 0 is not the console)");
        return;
    }
    struct waiter w[2];
    memset(w, 0, sizeof w);
    w[1].use_epoll = 1;
    w[1].ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = { .events = EPOLLIN, .data.u32 = 0 };
    if (w[1].ep < 0 || epoll_ctl(w[1].ep, EPOLL_CTL_ADD, 0, &ev) != 0) {
        fail(n, "epoll on fd 0: errno %d", errno);
        if (with_input) skip(ni, "(no epoll)");
        return;
    }
    pthread_t th[2];
    for (int i = 0; i < 2; i++)
        if (pthread_create(&th[i], NULL, waiter_thread, &w[i]) != 0) { fail(n, "pthread_create"); return; }
    int round = 1;
    int bad = console_round(n, w, round++, 1, NULL, "\033[6n", 'R', 2000);
    if (!bad) pass(n);

    if (with_input && !skipped(ni)) {
        int ibad = console_round(ni, w, round++, 0, "KP_WANT_KEY", NULL, 'k', 15000);
        if (!ibad) ibad = console_round(ni, w, round++, 0, "KP_WANT_SERIAL", NULL, 's', 15000);
        if (!ibad) pass(ni);
    }

    /* Release the waiters: stop, then one more cursor report. */
    for (int i = 0; i < 2; i++) __atomic_store_n(&w[i].stop, 1, __ATOMIC_SEQ_CST);
    for (int i = 0; i < 2; i++) __atomic_store_n(&w[i].round, round, __ATOMIC_SEQ_CST);
    (void)!write(1, "\033[6n", 4);
    for (int i = 0; i < 2; i++) pthread_join(th[i], NULL);
    drain_until('R');
    close(w[1].ep);
}

/* ── empty poll / select sets ──────────────────────────────────────────── */

enum { SLEEP_POLL_NULL, SLEEP_POLL_NEG, SLEEP_SELECT0, SLEEP_KINDS };
static const char *sleep_names[SLEEP_KINDS] = { "poll(NULL,0,100)", "poll(negative fds,100)", "select(0,100ms)" };

struct sleeper {
    volatile long tid;
    volatile int kind;
    volatile int go;          /* rounds requested */
    volatile int done;        /* rounds finished */
    volatile long long ms[SLEEP_KINDS];
    volatile int bad_rc;
    volatile int intr;        /* poll(NULL,0,-1) round: 1 = got EINTR */
};

static void on_usr1(int s) { (void)s; }

static void *sleeper_thread(void *p) {
    struct sleeper *s = p;
    s->tid = syscall(SYS_gettid);
    int seen = 0;
    for (;;) {
        int g = __atomic_load_n(&s->go, __ATOMIC_SEQ_CST);
        if (g == seen) { msleep(1); continue; }
        seen = g;
        int k = s->kind;
        if (k < 0) {
            /* poll(NULL, 0, -1): only a signal ends it. */
            int r = poll(NULL, 0, -1);
            s->intr = (r < 0 && errno == EINTR) ? 1 : -1;
            __atomic_store_n(&s->done, g, __ATOMIC_SEQ_CST);
            if (k == -2) return NULL;
            continue;
        }
        long long t0 = now_ms();
        int r;
        if (k == SLEEP_POLL_NULL) {
            r = poll(NULL, 0, 100);
        } else if (k == SLEEP_POLL_NEG) {
            struct pollfd pf[2] = { { -1, POLLIN, 0 }, { -5, POLLIN | POLLOUT, 0 } };
            r = poll(pf, 2, 100);
        } else {
            struct timeval tv = { 0, 100000 };
            r = select(0, NULL, NULL, NULL, &tv);
        }
        if (r != 0) s->bad_rc = r < 0 ? -errno : r;
        s->ms[k] = now_ms() - t0;
        __atomic_store_n(&s->done, g, __ATOMIC_SEQ_CST);
    }
}

static void empty_poll_sleep(void) {
    const char *n = "empty_poll_sleep";
    if (skipped(n)) return;
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, NULL);
    struct sleeper s;
    memset(&s, 0, sizeof s);
    pthread_t th;
    if (pthread_create(&th, NULL, sleeper_thread, &s) != 0) { fail(n, "pthread_create"); return; }
    while (!s.tid) msleep(1);
    int bad = 0, g = 0;
    for (int k = 0; k < SLEEP_KINDS && !bad; k++) {
        s.kind = k;
        struct sysw w0 = sysstat_w(s.tid);
        __atomic_store_n(&s.go, ++g, __ATOMIC_SEQ_CST);
        /* Sample the park mid-sleep, then keep traffic going until it ends. */
        msleep(30);
        struct sysw mid = sysstat_w(s.tid);
        long trips = 0;
        while (s.done != g) { long t = unrelated_traffic(20); if (t < 0) { trips = -1; break; } trips += t; }
        struct sysw w1 = sysstat_w(s.tid);
        unsigned long long dw = w1.pwakes - w0.pwakes;
        printf("%s: %s slept %lld ms, mid-sleep state %c mask %#llx, %ld round trips, %llu wakes\n",
               n, sleep_names[k], s.ms[k], mid.state ? mid.state : '?', mid.mask, trips, dw);
        if (trips < 0) { fail(n, "traffic io failed"); bad = 1; }
        else if (s.bad_rc) { fail(n, "%s returned %d", sleep_names[k], s.bad_rc); bad = 1; }
        else if (s.ms[k] < 95 || s.ms[k] > 400) { fail(n, "%s slept %lld ms, not ~100", sleep_names[k], s.ms[k]); bad = 1; }
        else if (mid.found && mid.state == 'S' && mid.mask != 0) {
            fail(n, "%s parked with mask %#llx, not deadline-only (0)", sleep_names[k], mid.mask); bad = 1;
        } else if (dw > 0) { fail(n, "%s took %llu poll wakes", sleep_names[k], dw); bad = 1; }
    }
    /* A signal still ends a deadline-only park: poll(NULL, 0, -1) → EINTR. */
    s.kind = -2;
    __atomic_store_n(&s.go, ++g, __ATOMIC_SEQ_CST);
    msleep(100);
    long long t0 = now_ms();
    pthread_kill(th, SIGUSR1);
    for (int i = 0; i < 2000 && s.done != g; i++) msleep(1);
    if (s.done != g) {
        if (!bad) fail(n, "SIGUSR1 did not end poll(NULL, 0, -1) within 2 s");
        bad = 1;
        /* The thread is stuck; leave it (the process exits at the end). */
        pthread_detach(th);
    } else {
        printf("%s: poll(NULL,0,-1) ended %lld ms after SIGUSR1 (EINTR %d)\n", n, now_ms() - t0, s.intr);
        if (s.intr != 1 && !bad) { fail(n, "poll(NULL, 0, -1) did not fail with EINTR"); bad = 1; }
        pthread_join(th, NULL);
    }
    if (!bad) pass(n);
}

/* ── VT_WAITACTIVE ─────────────────────────────────────────────────────── */

#define VT_GETSTATE   0x5603
#define VT_ACTIVATE   0x5606
#define VT_WAITACTIVE 0x5607
struct vt_stat { unsigned short v_active, v_signal, v_state; };

struct vtwait { int fd, vt; volatile long tid; volatile int rc; volatile long long ms; volatile int done; };

static void *vtwait_thread(void *p) {
    struct vtwait *v = p;
    v->tid = syscall(SYS_gettid);
    int r;
    do { r = ioctl(v->fd, VT_WAITACTIVE, v->vt); } while (r < 0 && errno == EINTR);
    v->rc = r < 0 ? -errno : r;
    v->ms = now_ms();
    __atomic_store_n(&v->done, 1, __ATOMIC_SEQ_CST);
    return NULL;
}

/* VT_ACTIVATE(vt) from a child process. Returns the child's clock right after
 * its ioctl returned (the switch is complete by then), or -1. */
static long long activate_from_child(int vt) {
    int pp[2];
    if (pipe(pp) < 0) return -1;
    pid_t c = fork();
    if (c == 0) {
        close(pp[0]);
        int fd = open("/dev/tty0", O_RDWR);
        long long t = (fd >= 0 && ioctl(fd, VT_ACTIVATE, vt) == 0) ? now_ms() : -1;
        _exit(write(pp[1], &t, sizeof t) == sizeof t ? 0 : 1);
    }
    close(pp[1]);
    long long t = -1;
    if (c < 0 || read(pp[0], &t, sizeof t) != sizeof t) t = -1;
    close(pp[0]);
    int st = 0;
    if (c > 0) waitpid(c, &st, 0);
    return t;
}

static int vt_wait_round(const char *n, int fd, int to, int quiet) {
    struct vtwait v = { .fd = fd, .vt = to };
    pthread_t th;
    if (pthread_create(&th, NULL, vtwait_thread, &v) != 0) { fail(n, "pthread_create"); return 1; }
    int bad = 0;
    struct sysw p = wait_parked(&v.tid);
    if (v.done) { fail(n, "VT_WAITACTIVE(%d) returned %d before the switch", to, v.rc); bad = 1; }
    else if (!p.found || p.state != 'S' || !p.mask) {
        fail(n, "waiter never parked (found %d state %c mask %llx)", p.found, p.state ? p.state : '?', p.mask); bad = 1;
    } else if (p.mask == ~0ULL) { fail(n, "VT_WAITACTIVE parked with the broadcast mask"); bad = 1; }
    if (!bad && quiet) {
        long trips = unrelated_traffic(1000);
        struct sysw q = sysstat_w(v.tid);
        unsigned long long dw = q.pwakes - p.pwakes;
        printf("%s: mask %#llx, %ld round trips, %llu wakes\n", n, p.mask, trips, dw);
        if (trips < 0) { fail(n, "traffic io failed"); bad = 1; }
        else if (dw > 10) { fail(n, "unrelated traffic woke VT_WAITACTIVE %llu times", dw); bad = 1; }
    }
    long long t0 = activate_from_child(to);
    if (t0 < 0) { fail(n, "VT_ACTIVATE(%d) from a child failed", to); bad = 1; t0 = now_ms(); }
    for (int i = 0; i < 3000 && !v.done; i++) msleep(1);
    if (!v.done) {
        fail(n, "VT_WAITACTIVE(%d) did not return within 3 s of the switch", to);
        /* Unblock it so the thread can be joined. */
        pthread_detach(th);
        return 1;
    }
    pthread_join(th, NULL);
    long long dt = v.ms - t0;
    printf("%s: VT_WAITACTIVE(%d) returned %d, %lld ms after the child's VT_ACTIVATE returned\n", n, to, v.rc, dt);
    if (!bad && v.rc != 0) { fail(n, "VT_WAITACTIVE(%d) returned %d", to, v.rc); bad = 1; }
    /* The switch wakes it; the 100 ms deadline backstop must not be what
     * returns it. Negative = it returned before the child's ioctl did. */
    if (!bad && dt > 30) { fail(n, "VT_WAITACTIVE(%d) took %lld ms after the switch", to, dt); bad = 1; }
    return bad;
}

static void vt_waitactive(void) {
    const char *n = "vt_waitactive";
    if (skipped(n)) return;
    int fd = open("/dev/tty0", O_RDWR);
    if (fd < 0) { skip(n, "(no /dev/tty0)"); return; }
    struct vt_stat st = { 0, 0, 0 };
    if (ioctl(fd, VT_GETSTATE, &st) != 0 || st.v_active < 1) { fail(n, "VT_GETSTATE: errno %d", errno); close(fd); return; }
    int orig = st.v_active;
    int other = orig == 2 ? 3 : 2;
    int bad = vt_wait_round(n, fd, other, 1);
    if (!bad) bad = vt_wait_round(n, fd, orig, 0);
    if (ioctl(fd, VT_GETSTATE, &st) == 0 && st.v_active != orig) {
        ioctl(fd, VT_ACTIVATE, orig);
        ioctl(fd, VT_WAITACTIVE, orig);
    }
    close(fd);
    if (!bad) pass(n);
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
        skip("console_poll_narrow", "(no /proc/sysstat)");
        skip("empty_poll_sleep", "(no /proc/sysstat)");
        skip("vt_waitactive", "(no /proc/sysstat)");
    } else {
        close(sfd);
        console_cases(with_input);
        empty_poll_sleep();
        vt_waitactive();
    }
    printf("kparks: %d passed, %d failed, %d skipped\n", n_pass, n_fail, n_skip);
    return n_fail > 125 ? 125 : n_fail;
}
