/*
 * listentag — a poller of a TCP listener parks on the listener's own poll
 * key, not the broadcast mask, and still sees every connection at once
 * (lane listentag).
 *
 * A listener's poll answer used to carry no tag, so every poll/epoll set
 * holding one (sshd's tokio reactor) parked with POLL_TAG_ALL and was woken
 * by every poll wake in the system: ~2,800 wakes/s on the laptop. See
 * artifacts/notes/broadcast-audit-2026-10-19.md, lane 1.
 *
 *   listentag [-x NAME]...
 *
 * Cases (all on a 127.0.0.1 listener):
 *   epollet_quiet    an EPOLLET acceptor parked in epoll_wait on the listener
 *                    alone parks with a mask that is not all ones, and 1 s of
 *                    unrelated AF_UNIX/pipe traffic wakes it 0 times (its
 *                    `pwakes` column in /proc/sysstat does not move; up to 3
 *                    windows, so a stray net broadcast cannot fail it)
 *   epollet_accept   then 50 connects with idle gaps of 25-150 ms between
 *                    them: each one is accepted within 20 ms of connect()
 *                    returning. The gaps are shorter and longer than the net
 *                    daemon's 100 ms broadcast window, so a missed key wake
 *                    is not hidden by a broadcast
 *   poll_listener    the same park/quiet check for a thread in poll() on the
 *                    listener, then a connect returns it within 20 ms (a
 *                    primer connect to another listener just before takes
 *                    the daemon's broadcast window)
 *   select_listener  the same for select()
 *   sshd_rows        (informational) sshd's parked threads and their masks
 *
 * Everything is skipped without /proc/sysstat (a host kernel). Output:
 * "<name>: PASS|FAIL ...|SKIP ..." per case and
 * "listentag: N passed, M failed, K skipped". Exit status = failures.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/syscall.h>
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
static long long now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}
static long long now_ms(void) { return now_us() / 1000; }
static void msleep(int ms) { struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L }; nanosleep(&ts, NULL); }

struct sysw { int found; char state; long tgid; unsigned long long mask, pwakes; };

static char sbuf[256 * 1024];

static size_t read_sysstat(void) {
    int fd = open("/proc/sysstat", O_RDONLY);
    if (fd < 0) return 0;
    size_t len = 0;
    for (;;) {
        ssize_t n = read(fd, sbuf + len, sizeof sbuf - 1 - len);
        if (n <= 0) break;
        len += (size_t)n;
        if (len >= sizeof sbuf - 1) break;
    }
    close(fd);
    sbuf[len] = 0;
    return len;
}

/* Parse one `w tid tgid state sc port listen poll_mask in_sc_ns pwakes` line. */
static int parse_w(const char *line, long *tid, struct sysw *r) {
    long t = 0, tg = 0, sc = 0, lis = 0; unsigned long port = 0;
    char st = 0;
    unsigned long long mask = 0, insc = 0, pw = 0;
    if (line[0] != 'w' || line[1] != ' ') return 0;
    if (sscanf(line, "w %ld %ld %c %ld %lx %ld %llx %llu %llu",
               &t, &tg, &st, &sc, &port, &lis, &mask, &insc, &pw) != 9) return 0;
    *tid = t;
    r->found = 1; r->state = st; r->tgid = tg; r->mask = mask; r->pwakes = pw;
    return 1;
}

static struct sysw sysstat_w(long tid) {
    struct sysw r = { 0 };
    read_sysstat();
    for (char *line = sbuf; line && *line; ) {
        char *nl = strchr(line, '\n');
        if (nl) *nl = 0;
        long t; struct sysw w = { 0 };
        if (parse_w(line, &t, &w) && t == tid) return w;
        line = nl ? nl + 1 : NULL;
    }
    return r;
}

/* Wait (up to 3 s) until thread `*tid` sleeps with a non-zero mask. */
static struct sysw wait_parked(volatile long *tid) {
    struct sysw w = { 0 };
    for (int i = 0; i < 300; i++) {
        if (*tid) { w = sysstat_w(*tid); if (w.found && w.state == 'S' && w.mask) return w; }
        msleep(10);
    }
    return w;
}

/* ~`ms` of poll traffic that has nothing to do with any listener: AF_UNIX
 * round trips (keyed data wakes) and pipe writes/reads. No inet traffic: the
 * net daemon's 100 ms broadcast safety net would wake every poller. */
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

/* Parked narrow (mask not all ones) and woken 0 times by unrelated traffic in
 * at least one of 3 windows of 1 s. Returns 0 when fine (nothing printed as
 * FAIL), 1 after a FAIL. `*returned` is the waiter's has-returned flag. */
static int check_quiet(const char *n, volatile long *tid, volatile int *returned) {
    struct sysw w = wait_parked(tid);
    if (!w.found || w.state != 'S' || !w.mask) {
        fail(n, "waiter never parked (found %d state %c mask %llx)", w.found, w.state ? w.state : '?', w.mask);
        return 1;
    }
    if (w.mask == ~0ULL) { fail(n, "parked with the broadcast mask (all ones)"); return 1; }
    unsigned long long best = ~0ULL;
    for (int k = 0; k < 3 && best != 0; k++) {
        struct sysw a = sysstat_w(*tid);
        long trips = unrelated_traffic(1000);
        struct sysw b = sysstat_w(*tid);
        if (trips < 0) { fail(n, "traffic io failed"); return 1; }
        if (*returned) { fail(n, "the waiter returned with no connection"); return 1; }
        unsigned long long dw = b.pwakes - a.pwakes;
        printf("%s: mask %#llx, %ld round trips, %llu wakes\n", n, w.mask, trips, dw);
        if (dw < best) best = dw;
    }
    if (best != 0) { fail(n, "unrelated traffic woke the waiter (best window: %llu wakes)", best); return 1; }
    return 0;
}

static int listener(int *port) {
    int l = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    if (l < 0) return -1;
    int one = 1;
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = 0 };
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t al = sizeof a;
    if (bind(l, (struct sockaddr *)&a, sizeof a) < 0 || listen(l, 64) < 0
        || getsockname(l, (struct sockaddr *)&a, &al) < 0) { close(l); return -1; }
    *port = ntohs(a.sin_port);
    return l;
}

/* A blocking connect to 127.0.0.1:port; the fd, or -1. */
static int connect_lo(int port) {
    int c = socket(AF_INET, SOCK_STREAM, 0);
    if (c < 0) return -1;
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port) };
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (connect(c, (struct sockaddr *)&a, sizeof a) < 0) { close(c); return -1; }
    return c;
}

/* ── EPOLLET acceptor ──────────────────────────────────────────────────── */

#define NCONN 50
#define LIMIT_US 20000

struct acceptor {
    int lfd, ep;
    volatile long tid;
    volatile int accepted;
    volatile int returned;   /* epoll_wait returns so far */
    volatile int stop;
    long long at[NCONN + 8];
};

static void *acceptor_thread(void *p) {
    struct acceptor *a = p;
    a->tid = syscall(SYS_gettid);
    while (!__atomic_load_n(&a->stop, __ATOMIC_SEQ_CST)) {
        struct epoll_event ev[4];
        int n = epoll_wait(a->ep, ev, 4, -1);
        if (n < 0 && errno == EINTR) continue;
        if (n < 0) break;
        __atomic_add_fetch(&a->returned, 1, __ATOMIC_SEQ_CST);
        /* Edge-triggered: drain until EAGAIN (what tokio does). */
        for (;;) {
            int c = accept4(a->lfd, NULL, NULL, SOCK_NONBLOCK | SOCK_CLOEXEC);
            if (c < 0) break;
            long long t = now_us();
            int i = a->accepted;
            if (i < NCONN + 8) a->at[i] = t;
            close(c);
            __atomic_add_fetch(&a->accepted, 1, __ATOMIC_SEQ_CST);
        }
    }
    return NULL;
}

static void epollet_cases(void) {
    const char *nq = "epollet_quiet", *na = "epollet_accept";
    int port;
    int l = listener(&port);
    if (l < 0) { fail(nq, "listener: errno %d", errno); fail(na, "no listener"); return; }
    struct acceptor a = { .lfd = l };
    a.ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = { .events = EPOLLIN | EPOLLET, .data.fd = l };
    if (a.ep < 0 || epoll_ctl(a.ep, EPOLL_CTL_ADD, l, &ev) < 0) {
        fail(nq, "epoll: errno %d", errno); fail(na, "no epoll"); close(l); return;
    }
    pthread_t th;
    if (pthread_create(&th, NULL, acceptor_thread, &a) != 0) { fail(nq, "pthread_create"); close(l); return; }

    int bad = 0;
    if (!skipped(nq)) {
        /* Count the acceptor's returns, not just its accepts: a wake with no
         * connection (an ALL park woken by the traffic) returns nothing from
         * epoll_wait, but the pwakes column still moves. */
        volatile int none = 0;
        bad = check_quiet(nq, &a.tid, &none);
        if (!bad && a.returned) { fail(nq, "epoll_wait returned %d times with no connection", a.returned); bad = 1; }
        if (!bad) pass(nq);
    }

    if (!skipped(na)) {
        static const int gaps[] = { 25, 40, 60, 150, 30, 110 };
        long long worst = 0;
        int late = 0, lost = 0;
        for (int i = 0; i < NCONN; i++) {
            msleep(gaps[i % 6]);
            int base = a.accepted;
            int c = connect_lo(port);
            long long t1 = now_us();
            if (c < 0) { fail(na, "connect %d: errno %d", i, errno); bad = 1; break; }
            /* Wait up to 1 s; past 20 ms it is late but still counted. */
            while (a.accepted == base && now_us() - t1 < 1000000) msleep(1);
            if (a.accepted == base) { lost++; close(c); continue; }
            long long d = a.at[base] - t1;
            if (d < 0) d = 0;
            if (d > worst) worst = d;
            if (d > LIMIT_US) {
                late++;
                printf("%s: connect %d (gap %d ms) accepted after %lld us\n", na, i, gaps[i % 6], d);
            }
            close(c);
        }
        struct sysw w = wait_parked(&a.tid);
        printf("%s: %d connects, worst %lld us, %d late, %d lost, %d epoll returns, mask %#llx\n",
               na, NCONN, worst, late, lost, a.returned, w.mask);
        if (late || lost) { fail(na, "%d accepts later than 20 ms, %d never (worst %lld us)", late, lost, worst); }
        else if (w.found && w.mask == ~0ULL) { fail(na, "acceptor parked with the broadcast mask"); }
        else pass(na);
    }

    __atomic_store_n(&a.stop, 1, __ATOMIC_SEQ_CST);
    int c = connect_lo(port);
    pthread_join(th, NULL);
    if (c >= 0) close(c);
    close(a.ep);
    close(l);
}

/* ── poll() / select() on a listener ───────────────────────────────────── */

struct waiter {
    int lfd, use_select;
    volatile long tid;
    volatile int returned;
    volatile long long at;
    volatile int rv;
};

static void *waiter_thread(void *p) {
    struct waiter *w = p;
    w->tid = syscall(SYS_gettid);
    int r;
    if (w->use_select) {
        fd_set rs;
        do { FD_ZERO(&rs); FD_SET(w->lfd, &rs); r = select(w->lfd + 1, &rs, NULL, NULL, NULL); }
        while (r < 0 && errno == EINTR);
        if (r == 1 && !FD_ISSET(w->lfd, &rs)) r = -100;
    } else {
        struct pollfd pf;
        do { pf.fd = w->lfd; pf.events = POLLIN; pf.revents = 0; r = poll(&pf, 1, -1); }
        while (r < 0 && errno == EINTR);
        if (r == 1 && !(pf.revents & POLLIN)) r = -100;
    }
    w->at = now_us();
    w->rv = r;
    __atomic_store_n(&w->returned, 1, __ATOMIC_SEQ_CST);
    return NULL;
}

static void level_case(const char *n, int use_select) {
    if (skipped(n)) return;
    int port;
    int l = listener(&port);
    if (l < 0) { fail(n, "listener: errno %d", errno); return; }
    if (use_select && l >= FD_SETSIZE) { skip(n, "(listener fd beyond FD_SETSIZE)"); close(l); return; }
    struct waiter w = { .lfd = l, .use_select = use_select };
    pthread_t th;
    if (pthread_create(&th, NULL, waiter_thread, &w) != 0) { fail(n, "pthread_create"); close(l); return; }
    int bad = check_quiet(n, &w.tid, &w.returned);
    /* A primer connect to another listener first: after the idle quiet
     * windows its pass takes the net daemon's 100 ms broadcast, so the one
     * the waiter is owed is woken by the listener's key alone. */
    int port2, c0 = -1;
    int l2 = listener(&port2);
    if (l2 >= 0) { c0 = connect_lo(port2); msleep(5); }
    int c = connect_lo(port);
    long long t1 = now_us();
    if (c < 0) { if (!bad) fail(n, "connect: errno %d", errno); bad = 1; }
    while (!w.returned && now_us() - t1 < 2000000) msleep(1);
    if (!bad) {
        long long d = w.at - t1;
        if (d < 0) d = 0;
        if (!w.returned) { fail(n, "a connect did not return the waiter within 2 s"); bad = 1; }
        else if (w.rv != 1) { fail(n, "returned %d", w.rv); bad = 1; }
        else if (d > LIMIT_US) { fail(n, "returned %lld us after the connect (limit 20 ms)", d); bad = 1; }
        else printf("%s: returned %lld us after the connect\n", n, d);
    }
    int acc = accept4(l, NULL, NULL, 0);
    if (acc >= 0) close(acc);
    if (!w.returned) {
        /* Still parked: release it with another connection. */
        int c2 = connect_lo(port);
        pthread_join(th, NULL);
        if (c2 >= 0) close(c2);
    } else {
        pthread_join(th, NULL);
    }
    if (c >= 0) close(c);
    if (c0 >= 0) close(c0);
    if (l2 >= 0) close(l2);
    close(l);
    if (!bad) pass(n);
}

/* ── sshd (informational) ──────────────────────────────────────────────── */

static void sshd_rows(void) {
    const char *n = "sshd_rows";
    if (skipped(n)) return;
    long tg[16]; int ntg = 0;
    DIR *d = opendir("/proc");
    if (!d) { skip(n, "(no /proc)"); return; }
    struct dirent *e;
    while ((e = readdir(d)) && ntg < 16) {
        char path[64], comm[32] = { 0 };
        long pid = atol(e->d_name);
        if (pid <= 0) continue;
        snprintf(path, sizeof path, "/proc/%ld/comm", pid);
        int fd = open(path, O_RDONLY);
        if (fd < 0) continue;
        ssize_t k = read(fd, comm, sizeof comm - 1);
        close(fd);
        if (k > 0 && !strncmp(comm, "sshd", 4)) tg[ntg++] = pid;
    }
    closedir(d);
    if (!ntg) { skip(n, "(no sshd running)"); return; }
    read_sysstat();
    int all = 0, rows = 0;
    for (char *line = sbuf; line && *line; ) {
        char *nl = strchr(line, '\n');
        if (nl) *nl = 0;
        long t; struct sysw w = { 0 };
        if (parse_w(line, &t, &w))
            for (int i = 0; i < ntg; i++) if (w.tgid == tg[i] && w.state == 'S') {
                rows++;
                if (w.mask == ~0ULL) all++;
                printf("%s: tid %ld mask %#llx pwakes %llu\n", n, t, w.mask, w.pwakes);
            }
        line = nl ? nl + 1 : NULL;
    }
    printf("%s: %d sleeping sshd threads, %d with the broadcast mask (informational)\n", n, rows, all);
}

int main(int argc, char **argv) {
    for (int i = 1; i < argc; i++)
        if (!strcmp(argv[i], "-x") && i + 1 < argc && n_skips < 16) skips[n_skips++] = argv[++i];
    setvbuf(stdout, NULL, _IOLBF, 0);
    int sfd = open("/proc/sysstat", O_RDONLY);
    if (sfd < 0) {
        skip("epollet_quiet", "(no /proc/sysstat)");
        skip("epollet_accept", "(no /proc/sysstat)");
        skip("poll_listener", "(no /proc/sysstat)");
        skip("select_listener", "(no /proc/sysstat)");
    } else {
        close(sfd);
        epollet_cases();
        level_case("poll_listener", 0);
        level_case("select_listener", 1);
        sshd_rows();
    }
    printf("listentag: %d passed, %d failed, %d skipped\n", n_pass, n_fail, n_skip);
    return n_fail > 125 ? 125 : n_fail;
}
