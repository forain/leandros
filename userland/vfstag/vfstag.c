/*
 * vfstag — blocked VFS waiters park on a narrow poll tag, not the broadcast
 * mask, and still wake on their real event (lane vfstag; audit
 * artifacts/notes/broadcast-audit-2026-10-19.md, lane 2).
 *
 *   vfstag [--input] [-x NAME]...
 *
 * Each case parks a waiter, then checks from outside it, through its `w`
 * line in /proc/sysstat, that
 *   (1) its poll mask is not all ones,
 *   (2) 1 s of unrelated AF_UNIX + pipe traffic wakes it (almost) never,
 *   (3) its real event wakes it promptly.
 *
 * Cases:
 *   flock_wait      flock(LOCK_EX) blocked on another process's lock, released
 *                   by LOCK_UN, by close and by the holder's exit
 *   setlkw_wait     fcntl(F_SETLKW) the same, released by F_UNLCK and by close
 *   signalfd_read   a blocking signalfd read, woken by kill(SIGUSR1) from
 *                   another process
 *   inotify_read    a blocking inotify read (nothing queues inotify events
 *                   here), ended by a signal (EINTR)
 *   static_poll     poll() on /dev/null for POLLPRI (never reported) plus a
 *                   pipe; and an EPOLLET /dev/null interest after its edge:
 *                   the waiter wakes only for the pipe write
 *   pipewire_poll   /dev/pipewire: POLLOUT always; POLLIN + a read for the
 *                   capture holder when a capture device exists, never POLLIN
 *                   for a second open (narrow static tag)
 *   drm_read        a blocking read of /dev/dri/card0, woken by the
 *                   completion event of a PAGE_FLIP issued from another
 *                   thread
 *   evdev_read      (--input, driven by host:vfstagtest.py) a blocking read
 *                   of /dev/input/event1 (the tablet); after "VT_WANT_INPUT"
 *                   the host moves the tablet through QMP
 *
 * Output: "<name>: PASS|FAIL ...|SKIP ..." per case and
 * "vfstag: N passed, M failed, K skipped". Exit status = failures.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/file.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

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

/* A woken waiter must be back well inside this: the lock waits keep a 1 s
 * backstop deadline, so anything near it means the release did not wake. */
#define WAKE_MS 250

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

/* Wait (up to 3 s) until `*tid` is parked: sleeping with a non-zero mask. */
static struct sysw wait_parked(volatile long *tid) {
    struct sysw w = { 0, 0, 0, 0 };
    for (int i = 0; i < 300; i++) {
        if (*tid) { w = sysstat_w(*tid); if (w.found && w.state == 'S' && w.mask) return w; }
        msleep(10);
    }
    return w;
}

/* ~1 s of poll traffic that has nothing to do with the waiter: AF_UNIX round
 * trips (keyed data wakes) and pipe writes/reads. Returns the round trips. */
static long unrelated_traffic(int ms) {
    int us[2], pp[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, us) < 0) return -1;
    if (pipe(pp) < 0) { close(us[0]); close(us[1]); return -1; }
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

static int traffic_ms = 1000;

/* (1) and (2) for the waiter `*tid`. `traffic` = 0 checks the mask only.
 * `*done` (may be NULL) going non-zero meanwhile means the waiter got its
 * event early, so the wake count is not this check's to judge. Returns 1 on
 * failure (already reported). */
static int check_narrow(const char *n, volatile long *tid, volatile int *done, int traffic) {
    struct sysw w = wait_parked(tid);
    if (!w.found || w.state != 'S' || !w.mask) {
        fail(n, "waiter never parked (found %d state %c mask %llx)", w.found, w.state ? w.state : '?', w.mask);
        return 1;
    }
    if (w.mask == ~0ULL) { fail(n, "parked with the broadcast mask (all ones)"); return 1; }
    if (!traffic) { printf("%s: mask %#llx\n", n, w.mask); return 0; }
    long trips = unrelated_traffic(traffic_ms);
    struct sysw w1 = sysstat_w(*tid);
    unsigned long long dw = w1.pwakes - w.pwakes;
    printf("%s: mask %#llx, %ld round trips, %llu wakes\n", n, w.mask, trips, dw);
    if (trips < 0) { fail(n, "traffic io failed"); return 1; }
    if (done && *done) { printf("%s: note: the waiter woke during the window\n", n); return 0; }
    /* A bloom-bit collision with the traffic's own keys can cost a few. */
    if (!w1.found || dw > 10) {
        fail(n, "unrelated traffic woke the waiter %llu times (%ld round trips)", dw, trips);
        return 1;
    }
    return 0;
}

/* Read one byte from `fd` within `ms`. 1 = got it. */
static int wait_byte(int fd, int ms) {
    struct pollfd pf = { fd, POLLIN, 0 };
    char c;
    return poll(&pf, 1, ms) == 1 && read(fd, &c, 1) == 1;
}

/* ── flock / F_SETLKW ─────────────────────────────────────────────────── */

static char lockfile[64] = "/tmp/vfstag.lock";
#define LOCKFILE lockfile

enum lock_api { API_FLOCK, API_SETLKW };
enum release { REL_UNLOCK, REL_CLOSE, REL_EXIT };

static int take_lock(int fd, enum lock_api api, int wait) {
    if (api == API_FLOCK) return flock(fd, LOCK_EX | (wait ? 0 : LOCK_NB));
    struct flock fl = { .l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0 };
    return fcntl(fd, wait ? F_SETLKW : F_SETLK, &fl);
}

static int drop_lock(int fd, enum lock_api api) {
    if (api == API_FLOCK) return flock(fd, LOCK_UN);
    struct flock fl = { .l_type = F_UNLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0 };
    return fcntl(fd, F_SETLK, &fl);
}

/* One round: a holder process takes the lock, a waiter process blocks on it,
 * the holder releases it the `rel` way. 0 = ok. */
static int lock_round(const char *n, enum lock_api api, enum release rel, int traffic) {
    static const char *rels[] = { "unlock", "close", "exit" };
    int hcmd[2], hready[2], wready[2], wdone[2];
    if (pipe(hcmd) || pipe(hready) || pipe(wready) || pipe(wdone)) { fail(n, "pipe"); return 1; }
    pid_t h = fork();
    if (h == 0) {
        close(hcmd[1]);
        int fd = open(LOCKFILE, O_RDWR | O_CREAT, 0644);
        if (fd < 0 || take_lock(fd, api, 0) != 0) _exit(2);
        (void)!write(hready[1], "h", 1);
        char c;
        if (read(hcmd[0], &c, 1) != 1) _exit(3);
        if (rel == REL_EXIT) _exit(0);
        if (rel == REL_UNLOCK && drop_lock(fd, api) != 0) _exit(4);
        if (rel == REL_CLOSE) close(fd);
        /* Stay alive, so only the unlock / close can have released it. */
        (void)!read(hcmd[0], &c, 1);
        _exit(0);
    }
    if (!wait_byte(hready[0], 3000)) { fail(n, "%s: the holder did not take the lock", rels[rel]); kill(h, SIGKILL); waitpid(h, NULL, 0); return 1; }
    pid_t w = fork();
    if (w == 0) {
        close(hcmd[0]); close(hcmd[1]);
        int fd = open(LOCKFILE, O_RDWR);
        if (fd < 0) _exit(2);
        if (take_lock(fd, api, 0) == 0) _exit(5); /* must conflict */
        (void)!write(wready[1], "w", 1);
        if (take_lock(fd, api, 1) != 0) _exit(3);
        (void)!write(wdone[1], "d", 1);
        _exit(0);
    }
    int bad = 0;
    volatile long tid = 0;
    if (!wait_byte(wready[0], 3000)) { fail(n, "%s: the waiter did not start", rels[rel]); bad = 1; }
    else {
        tid = w;
        bad = check_narrow(n, &tid, NULL, traffic);
    }
    long long t0 = now_ms();
    (void)!write(hcmd[1], "r", 1);
    if (!bad) {
        if (!wait_byte(wdone[0], 3000)) {
            fail(n, "%s: the waiter did not get the lock within 3 s of the release", rels[rel]); bad = 1;
        } else {
            long long dt = now_ms() - t0;
            printf("%s: %s: waiter acquired %lld ms after the release\n", n, rels[rel], dt);
            if (dt > WAKE_MS) { fail(n, "%s: %lld ms: the release did not wake the waiter (backstop)", rels[rel], dt); bad = 1; }
        }
    }
    close(hcmd[1]);
    kill(w, SIGKILL);
    kill(h, SIGKILL);
    waitpid(w, NULL, 0);
    waitpid(h, NULL, 0);
    close(hcmd[0]); close(hready[0]); close(hready[1]); close(wready[0]); close(wready[1]);
    close(wdone[0]); close(wdone[1]);
    return bad;
}

static void lock_cases(void) {
    if (!skipped("flock_wait")) {
        int bad = lock_round("flock_wait", API_FLOCK, REL_UNLOCK, 1);
        bad |= lock_round("flock_wait", API_FLOCK, REL_CLOSE, 0);
        bad |= lock_round("flock_wait", API_FLOCK, REL_EXIT, 0);
        if (!bad) pass("flock_wait");
    }
    if (!skipped("setlkw_wait")) {
        int bad = lock_round("setlkw_wait", API_SETLKW, REL_UNLOCK, 1);
        bad |= lock_round("setlkw_wait", API_SETLKW, REL_CLOSE, 0);
        bad |= lock_round("setlkw_wait", API_SETLKW, REL_EXIT, 0);
        if (!bad) pass("setlkw_wait");
    }
    /* The traffic's poll keys are nearly the same every round (small slot
     * numbers), so whether one shares a bloom bit with the lock waiter's tag
     * depends on the lock file. 64 files cover the 64 bits with high odds. A
     * waiter parked on the bit alone is woken once per trip then; one parked on
     * its exact key is not. */
    if (!skipped("flock_collide")) {
        int bad = 0;
        traffic_ms = 100;
        for (int i = 0; i < 64 && !bad; i++) {
            /* The tag is a hash of the file's tmpfs slot: a new file per round
             * moves it, and the earlier files stay put so no slot is reused. */
            snprintf(lockfile, sizeof lockfile, "/tmp/vfstag.lock.%d", i);
            bad = lock_round("flock_collide", (i & 1) ? API_SETLKW : API_FLOCK, REL_UNLOCK, 1);
        }
        for (int i = 0; i < 64; i++) {
            snprintf(lockfile, sizeof lockfile, "/tmp/vfstag.lock.%d", i);
            unlink(lockfile);
        }
        strcpy(lockfile, "/tmp/vfstag.lock");
        traffic_ms = 1000;
        if (!bad) pass("flock_collide");
    }
    unlink(LOCKFILE);
}

/* ── signalfd / inotify: a child blocks in read(), the parent watches ──── */

static void on_usr1(int s) { (void)s; }

static void child_read_case(const char *n, int inotify) {
    if (skipped(n)) return;
    int ready[2], done[2];
    if (pipe(ready) || pipe(done)) { fail(n, "pipe"); return; }
    if (inotify) mkdir("/tmp/vfstag.dir", 0755);
    pid_t c = fork();
    if (c == 0) {
        int fd;
        if (inotify) {
            struct sigaction sa;
            memset(&sa, 0, sizeof sa);
            sa.sa_handler = on_usr1; /* no SA_RESTART: the read ends EINTR */
            sigaction(SIGUSR1, &sa, NULL);
            fd = inotify_init1(0);
            if (fd < 0 || inotify_add_watch(fd, "/tmp/vfstag.dir", IN_CREATE) < 0) _exit(2);
        } else {
            sigset_t m;
            sigemptyset(&m);
            sigaddset(&m, SIGUSR1);
            sigprocmask(SIG_BLOCK, &m, NULL);
            fd = signalfd(-1, &m, 0);
            if (fd < 0) _exit(2);
        }
        (void)!write(ready[1], "r", 1);
        char b[sizeof(struct signalfd_siginfo)];
        ssize_t r = read(fd, b, sizeof b);
        int ok = inotify ? (r < 0 && errno == EINTR)
                         : (r == (ssize_t)sizeof b && ((struct signalfd_siginfo *)b)->ssi_signo == SIGUSR1);
        if (ok) (void)!write(done[1], "d", 1);
        _exit(ok ? 0 : 3);
    }
    int bad = 0;
    volatile long tid = 0;
    if (!wait_byte(ready[0], 3000)) { fail(n, "the child did not start"); bad = 1; }
    else { tid = c; bad = check_narrow(n, &tid, NULL, 1); }
    if (!bad) {
        long long t0 = now_ms();
        kill(c, SIGUSR1);
        if (!wait_byte(done[0], 3000)) { fail(n, "SIGUSR1 did not end the blocked read within 3 s"); bad = 1; }
        else {
            long long dt = now_ms() - t0;
            printf("%s: the read returned %lld ms after kill(SIGUSR1)\n", n, dt);
            if (dt > WAKE_MS) { fail(n, "%lld ms after the signal", dt); bad = 1; }
        }
    }
    kill(c, SIGKILL);
    waitpid(c, NULL, 0);
    close(ready[0]); close(ready[1]); close(done[0]); close(done[1]);
    if (inotify) rmdir("/tmp/vfstag.dir");
    if (!bad) pass(n);
}

/* ── in-process waiter threads: poll() and blocking reads ─────────────── */

struct waiter {
    volatile long tid;
    volatile int done;       /* events seen */
    volatile int stop;
    int kind;                /* 0 poll set, 1 epoll, 2 blocking read */
    int fd, fd2, ev;         /* poll: fd with `ev` + fd2 (pipe, POLLIN); read: fd */
    int ep;
    ssize_t last;
};

static void *waiter_thread(void *p) {
    struct waiter *w = p;
    w->tid = syscall(SYS_gettid);
    while (!__atomic_load_n(&w->stop, __ATOMIC_SEQ_CST)) {
        if (w->kind == 0) {
            struct pollfd pf[2] = { { w->fd, (short)w->ev, 0 }, { w->fd2, POLLIN, 0 } };
            int r = poll(pf, 2, -1);
            if (r < 0 && errno == EINTR) continue;
            if (r < 0) break;
            if (pf[0].revents) { w->last = pf[0].revents; break; } /* must never happen */
            if (pf[1].revents & POLLIN) { char c; (void)!read(w->fd2, &c, 1); __atomic_add_fetch(&w->done, 1, __ATOMIC_SEQ_CST); }
        } else if (w->kind == 1) {
            struct epoll_event ev[4];
            int r = epoll_wait(w->ep, ev, 4, -1);
            if (r < 0 && errno == EINTR) continue;
            if (r < 0) break;
            for (int i = 0; i < r; i++) {
                if (ev[i].data.fd == w->fd2) { char c; (void)!read(w->fd2, &c, 1); __atomic_add_fetch(&w->done, 1, __ATOMIC_SEQ_CST); }
                else w->last++; /* the static fd fired again: a second edge */
            }
        } else {
            char b[512];
            ssize_t r = read(w->fd, b, sizeof b);
            if (r < 0 && errno == EINTR) continue;
            if (r <= 0) { w->last = r; break; }
            w->last = r;
            __atomic_add_fetch(&w->done, 1, __ATOMIC_SEQ_CST);
        }
    }
    return NULL;
}

static int wait_done(struct waiter *w, int above, int ms) {
    for (int i = 0; i < ms / 5; i++) { if (w->done > above) return 1; msleep(5); }
    return w->done > above;
}

/* Poll-set case: `fd` (asked for `ev`, which it never reports) + a pipe. */
static int poll_set_case(const char *n, int fd, int ev, int epoll_et) {
    int pp[2];
    if (pipe(pp)) { fail(n, "pipe"); return 1; }
    struct waiter w = { .kind = epoll_et ? 1 : 0, .fd = fd, .fd2 = pp[0], .ev = ev };
    if (epoll_et) {
        w.ep = epoll_create1(0);
        struct epoll_event e1 = { .events = EPOLLIN | EPOLLOUT | EPOLLET, .data.fd = fd };
        struct epoll_event e2 = { .events = EPOLLIN, .data.fd = pp[0] };
        if (epoll_ctl(w.ep, EPOLL_CTL_ADD, fd, &e1) != 0) {
            int e = errno;
            close(w.ep); close(pp[0]); close(pp[1]);
            return e == EPERM ? -1 : (fail(n, "epoll_ctl ADD: errno %d", e), 1);
        }
        epoll_ctl(w.ep, EPOLL_CTL_ADD, pp[0], &e2);
        /* Consume the one edge of the always-ready fd. */
        struct epoll_event got[4];
        if (epoll_wait(w.ep, got, 4, 1000) != 1) { fail(n, "the EPOLLET edge did not fire once"); close(w.ep); close(pp[0]); close(pp[1]); return 1; }
    }
    pthread_t th;
    if (pthread_create(&th, NULL, waiter_thread, &w) != 0) { fail(n, "pthread_create"); return 1; }
    int bad = check_narrow(n, &w.tid, &w.done, 1);
    long long t0 = now_ms();
    (void)!write(pp[1], "x", 1);
    if (!wait_done(&w, 0, 2000)) { if (!bad) fail(n, "the pipe write did not wake the waiter within 2 s"); bad = 1; }
    else printf("%s: woke %lld ms after the pipe write\n", n, now_ms() - t0);
    if (w.last) { if (!bad) fail(n, "the static fd reported readiness it never has (%zd)", w.last); bad = 1; }
    __atomic_store_n(&w.stop, 1, __ATOMIC_SEQ_CST);
    (void)!write(pp[1], "x", 1);
    pthread_join(th, NULL);
    if (epoll_et) close(w.ep);
    close(pp[0]); close(pp[1]);
    return bad;
}

static void static_case(void) {
    const char *n = "static_poll";
    if (skipped(n)) return;
    int fd = open("/dev/null", O_RDWR);
    if (fd < 0) { fail(n, "open /dev/null"); return; }
    int bad = poll_set_case(n, fd, POLLPRI, 0);
    int r = poll_set_case(n, fd, 0, 1);
    if (r < 0) printf("%s: note: epoll refuses /dev/null (EPERM), EPOLLET part not run\n", n);
    else bad |= r;
    /* A regular file too (poll only: epoll refuses regular files). */
    int f = open("/tmp/vfstag.static", O_RDWR | O_CREAT, 0644);
    if (f >= 0) { bad |= poll_set_case(n, f, POLLPRI, 0); close(f); unlink("/tmp/vfstag.static"); }
    close(fd);
    if (!bad) pass(n);
}

static void pipewire_case(void) {
    const char *n = "pipewire_poll";
    if (skipped(n)) return;
    int fd = open("/dev/pipewire", O_RDONLY | O_NONBLOCK);
    if (fd < 0) { skip(n, "(no /dev/pipewire)"); return; }
    /* The first poller claims the capture side; always writable. */
    struct pollfd pf = { fd, POLLIN | POLLOUT, 0 };
    int pr = poll(&pf, 1, 0);
    if (pr < 1 || !(pf.revents & POLLOUT) || (pf.revents & (POLLERR | POLLHUP))) {
        fail(n, "poll answered %d revents %#x, want POLLOUT", pr, pf.revents);
        close(fd); return;
    }
    /* With a capture device (QEMU hda-micro, the laptop) POLLIN arrives
     * within a period or two and a read then yields data and never 0;
     * without one (virtio-sound) POLLIN must never fire. */
    pf.events = POLLIN; pf.revents = 0;
    pr = poll(&pf, 1, 400);
    int capture = pr == 1 && (pf.revents & POLLIN);
    if (capture) {
        char buf[1920];
        ssize_t r = read(fd, buf, sizeof buf);
        if (r <= 0) { fail(n, "POLLIN but read returned %zd errno %d", r, errno); close(fd); return; }
        /* Drained: not readable again right away. */
        r = read(fd, buf, sizeof buf);
        for (int i = 0; i < 100 && r > 0; i++) r = read(fd, buf, sizeof buf);
        if (r != -1 || errno != EAGAIN) { fail(n, "empty O_NONBLOCK read returned %zd errno %d, want EAGAIN", r, errno); close(fd); return; }
    } else if (pr != 0) {
        fail(n, "poll(POLLIN) answered %d revents %#x without a capture device", pr, pf.revents);
        close(fd); return;
    }
    /* A second open is not the capture holder: never POLLIN, still POLLOUT. */
    int fd2 = open("/dev/pipewire", O_RDONLY | O_NONBLOCK);
    if (fd2 >= 0) {
        struct pollfd p2 = { fd2, POLLIN | POLLOUT, 0 };
        pr = poll(&p2, 1, capture ? 100 : 0);
        if (pr != 1 || p2.revents != POLLOUT) {
            fail(n, "second open: poll answered %d revents %#x, want POLLOUT only", pr, p2.revents);
            close(fd2); close(fd); return;
        }
        /* Its wake tag is the narrow static one: the existing narrow-wake case. */
        int bad = poll_set_case(n, fd2, POLLIN, 0);
        close(fd2); close(fd);
        if (!bad) pass(n);
        return;
    }
    close(fd);
    fail(n, "second open: errno %d", errno);
}

/* ── DRM: a blocking card read woken by a page-flip event ─────────────── */

static int drm_setup(int fd, uint32_t *crtc_out, uint32_t *fb_out, char *why, size_t whylen) {
    uint32_t crtcs[8], conns[8], encs[8], fbs[8];
    struct drm_mode_card_res res;
    memset(&res, 0, sizeof res);
    if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) != 0) { snprintf(why, whylen, "GETRESOURCES errno %d", errno); return -1; }
    if (!res.count_crtcs || !res.count_connectors) { snprintf(why, whylen, "no crtc/connector"); return -1; }
    if (res.count_crtcs > 8) res.count_crtcs = 8;
    if (res.count_connectors > 8) res.count_connectors = 8;
    if (res.count_encoders > 8) res.count_encoders = 8;
    if (res.count_fbs > 8) res.count_fbs = 8;
    res.crtc_id_ptr = (uintptr_t)crtcs; res.connector_id_ptr = (uintptr_t)conns;
    res.encoder_id_ptr = (uintptr_t)encs; res.fb_id_ptr = (uintptr_t)fbs;
    if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) != 0) { snprintf(why, whylen, "GETRESOURCES(2) errno %d", errno); return -1; }
    struct drm_mode_modeinfo modes[16];
    struct drm_mode_get_connector c;
    memset(&c, 0, sizeof c);
    c.connector_id = conns[0];
    if (ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &c) != 0 || !c.count_modes) { snprintf(why, whylen, "GETCONNECTOR errno %d modes %u", errno, c.count_modes); return -1; }
    if (c.count_modes > 16) c.count_modes = 16;
    c.modes_ptr = (uintptr_t)modes;
    c.count_props = 0; c.count_encoders = 0;
    if (ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &c) != 0) { snprintf(why, whylen, "GETCONNECTOR(2) errno %d", errno); return -1; }
    struct drm_mode_create_dumb cd = { .width = modes[0].hdisplay, .height = modes[0].vdisplay, .bpp = 32 };
    if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &cd) != 0) { snprintf(why, whylen, "CREATE_DUMB errno %d", errno); return -1; }
    struct drm_mode_fb_cmd fb = { .width = cd.width, .height = cd.height, .pitch = cd.pitch,
                                  .bpp = 32, .depth = 24, .handle = cd.handle };
    if (ioctl(fd, DRM_IOCTL_MODE_ADDFB, &fb) != 0) { snprintf(why, whylen, "ADDFB errno %d", errno); return -1; }
    struct drm_mode_crtc set;
    memset(&set, 0, sizeof set);
    set.crtc_id = crtcs[0];
    set.fb_id = fb.fb_id;
    set.set_connectors_ptr = (uintptr_t)&conns[0];
    set.count_connectors = 1;
    set.mode = modes[0];
    set.mode_valid = 1;
    if (ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &set) != 0) { snprintf(why, whylen, "SETCRTC errno %d", errno); return -1; }
    *crtc_out = crtcs[0];
    *fb_out = fb.fb_id;
    return 0;
}

static int drm_flip(int fd, uint32_t crtc, uint32_t fb) {
    struct drm_mode_crtc_page_flip f = { .crtc_id = crtc, .fb_id = fb,
                                         .flags = DRM_MODE_PAGE_FLIP_EVENT, .user_data = 0x7a6 };
    return ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &f);
}

static void drm_case(void) {
    const char *n = "drm_read";
    if (skipped(n)) return;
    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) { skip(n, "(no /dev/dri/card0)"); return; }
    uint32_t crtc = 0, fb = 0;
    char why[96];
    if (drm_setup(fd, &crtc, &fb, why, sizeof why) != 0) { fail(n, "modeset: %s", why); close(fd); return; }
    /* No event may be queued yet: SETCRTC sends none. */
    struct waiter w = { .kind = 2, .fd = fd };
    pthread_t th;
    if (pthread_create(&th, NULL, waiter_thread, &w) != 0) { fail(n, "pthread_create"); close(fd); return; }
    int bad = check_narrow(n, &w.tid, &w.done, 1);
    long long t0 = now_ms();
    if (drm_flip(fd, crtc, fb) != 0) { if (!bad) fail(n, "PAGE_FLIP errno %d", errno); bad = 1; }
    else if (!wait_done(&w, 0, 2000)) { if (!bad) fail(n, "the flip event did not wake the blocked read within 2 s"); bad = 1; }
    else {
        printf("%s: read returned %zd bytes %lld ms after PAGE_FLIP\n", n, w.last, now_ms() - t0);
        if (w.last != (ssize_t)sizeof(struct drm_event_vblank)) { if (!bad) fail(n, "read %zd bytes", w.last); bad = 1; }
    }
    /* Release the reader: one more flip event. */
    __atomic_store_n(&w.stop, 1, __ATOMIC_SEQ_CST);
    for (int i = 0; i < 20 && !__atomic_load_n(&w.done, __ATOMIC_SEQ_CST); i++) msleep(10);
    int d = w.done;
    for (int i = 0; i < 5; i++) {
        msleep(50);
        if (drm_flip(fd, crtc, fb) == 0 && wait_done(&w, d, 1000)) break;
    }
    pthread_join(th, NULL);
    close(fd);
    if (!bad) pass(n);
}

/* ── evdev: a blocking tablet read woken by real input (host QMP) ─────── */

static void evdev_case(int with_input) {
    const char *n = "evdev_read";
    if (skipped(n)) return;
    int fd = open("/dev/input/event1", O_RDONLY | O_NONBLOCK);
    if (fd < 0) { skip(n, "(no /dev/input/event1)"); return; }
    char b[4096];
    while (read(fd, b, sizeof b) > 0) {}
    fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) & ~O_NONBLOCK);
    struct waiter w = { .kind = 2, .fd = fd };
    pthread_t th;
    if (pthread_create(&th, NULL, waiter_thread, &w) != 0) { fail(n, "pthread_create"); close(fd); return; }
    int bad = check_narrow(n, &w.tid, &w.done, 1);
    if (with_input) {
        int d = w.done;
        long long t0 = now_ms();
        printf("VT_WANT_INPUT\n"); fflush(stdout);
        if (!wait_done(&w, d, 15000)) { if (!bad) fail(n, "tablet input did not wake the blocked read within 15 s"); bad = 1; }
        else printf("%s: the read returned %lld ms after the cue\n", n, now_ms() - t0);
        printf("VT_INPUT_DONE\n"); fflush(stdout);
    }
    /* The reader may sit in read() again: leave it; main() ends the group
     * with _exit. */
    __atomic_store_n(&w.stop, 1, __ATOMIC_SEQ_CST);
    pthread_detach(th);
    if (!bad) pass(n);
}

int main(int argc, char **argv) {
    int with_input = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--input")) with_input = 1;
        else if (!strcmp(argv[i], "-x") && i + 1 < argc && n_skips < 16) skips[n_skips++] = argv[++i];
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    signal(SIGPIPE, SIG_IGN);
    int sfd = open("/proc/sysstat", O_RDONLY);
    if (sfd < 0) {
        skip("vfstag", "(no /proc/sysstat)");
    } else {
        close(sfd);
        lock_cases();
        child_read_case("signalfd_read", 0);
        child_read_case("inotify_read", 1);
        static_case();
        pipewire_case();
        drm_case();
        evdev_case(with_input);
    }
    printf("vfstag: %d passed, %d failed, %d skipped\n", n_pass, n_fail, n_skip);
    /* A detached evdev reader may still sit in read(): exit the group. */
    fflush(stdout);
    _exit(n_fail > 125 ? 125 : n_fail);
}
