/*
 * nettag2 — blocking socket I/O and polls of sockets with no smoltcp object
 * yet park on a narrow tag, not the broadcast mask, and still wake at once on
 * the real event (lane nettag2).
 *
 * The net server's `fd_wake_tag` gave no tag to any inet socket, so a
 * blocking recv/send/connect parked with POLL_TAG_ALL and a 10 ms re-probe;
 * an ICMP socket's poll answer, an unbound UDP/ICMP socket's and every socket
 * not bound yet carried none either. See
 * artifacts/notes/broadcast-audit-2026-10-19.md, lane 3.
 *
 *   nettag2 [-x NAME]...
 *
 * Every case checks the same three things for one waiter thread: it parks
 * with a mask that is not all ones, 1 s of unrelated AF_UNIX/pipe traffic
 * wakes it 0 times (best of up to 3 windows, so a stray net broadcast cannot
 * fail it), and the real event returns it within 20 ms. The traffic includes
 * round trips on a pipe whose key shares a bloom bit with the waiter's, so a
 * park that keeps only bits (not keys) fails on every arch, not by hash luck. Just before the event
 * a primer datagram on another loopback socket takes the net daemon's 100 ms
 * broadcast window, so the waiter is woken by its own key alone.
 *
 *   tcp_recv        blocking recv() on an idle loopback TCP connection; the
 *                   event is the peer's send, then (second wait) its FIN
 *   udp_recv        blocking recvfrom() on a UDP socket bound to 127.0.0.1
 *   udpany_recv     the same on a wildcard (INADDR_ANY) UDP server
 *   unbound_recv    blocking recvfrom() on an unbound UDP socket; another
 *                   thread binds it, then sends to it
 *   unbound_epollet an EPOLLIN|EPOLLET waiter on an unbound UDP socket;
 *                   another thread binds it and a datagram arrives
 *   icmp_recv       blocking recv() on a ping socket; the event is an echo
 *                   reply from 127.0.0.1
 *   icmp_unbound    poll(POLLIN) on a ping socket that never sent; the first
 *                   sendto() of an echo request gives it a socket and the
 *                   reply wakes the poller
 *   connect_pending a blocking connect() whose SYN cannot leave (a neighbour
 *                   on the NIC's subnet that never answers ARP); the event is
 *                   shutdown() from another thread. SKIP without a NIC route
 *   connect_lo      20 blocking connects to a loopback listener and 20 to a
 *                   closed loopback port, each answered within 20 ms
 *
 * Everything is skipped without /proc/sysstat (a host kernel). Output:
 * "<name>: PASS|FAIL ...|SKIP ..." per case and
 * "nettag2: N passed, M failed, K skipped". Exit status = failures.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/ip_icmp.h>
#include <poll.h>
#include <pthread.h>
#include "../vcpucanary.h"
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/resource.h>
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

#define LIMIT_US 20000

/* ── /proc/sysstat `w` rows ───────────────────────────────────────────── */

struct sysw { int found; char state; long tgid; unsigned long long mask, pwakes; };

static char sbuf[4 << 20]; /* /proc/sysstat passes 256 KiB on a long-lived system */

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

/* ~`ms` of poll traffic that has nothing to do with any inet socket: AF_UNIX
 * round trips (keyed data wakes) and pipe writes/reads. No inet traffic: the
 * net daemon's 100 ms broadcast safety net would wake every poller. */
static long unrelated_traffic(int ms, const int al[2]) {
    int us[2], pp[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, us) < 0 || pipe(pp) < 0) return -1;
    long n = 0;
    long long end = now_ms() + ms;
    char c = 'x';
    while (now_ms() < end) {
        for (int i = 0; i < 50; i++, n++) {
            if (write(us[0], &c, 1) != 1 || read(us[1], &c, 1) != 1) { n = -1; goto out; }
            if (write(pp[1], &c, 1) != 1 || read(pp[0], &c, 1) != 1) { n = -1; goto out; }
            /* The alias pipe: a keyed wake on the waiter's own bit. */
            if (write(al[1], &c, 1) != 1 || read(al[0], &c, 1) != 1) { n = -1; goto out; }
        }
    }
out:
    close(us[0]); close(us[1]); close(pp[0]); close(pp[1]);
    return n;
}

/* ── an alias pipe: unrelated traffic on the waiter's own bloom bit ───────
 *
 * Which bit a socket's key and a pipe's key land on is hash luck that differs
 * per arch (one stack's handles, another's ring numbers), so plain traffic
 * may never share the waiter's bit, and a bits-only park would then pass
 * unnoticed (lane netrecvkey: x86_64 failed, aarch64 passed the same bug).
 * Find a pipe whose blocked reader parks on a bit of the waiter's mask, by
 * parking a reader on candidate pipes and reading its mask from sysstat, and
 * add round trips on it to the unrelated traffic: a park refined by key
 * ignores them, a bits-only park is woken by each one, on every arch. */

/* Pipe rings are numbered from a pool whose position depends on every suite
 * that ran earlier in the boot, so a window of N consecutive ring numbers can
 * miss a given bit for good (full regress: 320 rings covered 63 of 64 bits,
 * never the waiter's bit 13, on both arches). The window just keeps sliding
 * while candidates stay open, so allow many more, raising the soft fd limit
 * to the hard one first. */
#define ALIAS_MAX 1500
static int alias_fd[ALIAS_MAX][2];
static unsigned long long alias_bit[ALIAS_MAX];
static int n_alias;

struct prober { int fd; volatile long tid; };
static void *prober_thread(void *p) {
    struct prober *pr = p;
    pr->tid = syscall(SYS_gettid);
    char c;
    while (read(pr->fd, &c, 1) < 0 && errno == EINTR) {}
    return NULL;
}

/* The bit a blocked read() on pipe `pp` parks on, 0 if it did not park. */
static unsigned long long probe_pipe(int pp[2]) {
    struct prober pr = { .fd = pp[0] };
    pthread_t th;
    if (pthread_create(&th, NULL, prober_thread, &pr)) return 0;
    unsigned long long m = 0;
    for (int i = 0; i < 2000 && !m; i++) {
        if (pr.tid) { struct sysw w = sysstat_w(pr.tid); if (w.found && w.state == 'S') m = w.mask; }
        if (!m) { struct timespec ts = { 0, 1000000 }; nanosleep(&ts, NULL); }
    }
    char c = 'a';
    if (write(pp[1], &c, 1) != 1) {}
    pthread_join(th, NULL);
    return m == ~0ULL ? 0 : m;
}

/* A pipe (its fds in `out`) whose reader parks on a bit of `mask`; 0 if one
 * was found. Candidates stay open, so ring numbers are not reused. */
static int alias_pipe(unsigned long long mask, int out[2]) {
    static int raised;
    if (!raised) {
        struct rlimit rl;
        raised = 1;
        if (getrlimit(RLIMIT_NOFILE, &rl) == 0 && rl.rlim_cur < rl.rlim_max) { rl.rlim_cur = rl.rlim_max; setrlimit(RLIMIT_NOFILE, &rl); }
    }
    for (int i = 0; ; i++) {
        if (i == n_alias) {
            if (n_alias == ALIAS_MAX) return -1;
            /* A probe that never parked (a loaded machine) says nothing about the
             * pipe: drop it and try another instead of spending a slot. */
            unsigned long long b = 0;
            for (int tries = 0; tries < 8 && !b; tries++) {
                if (pipe(alias_fd[n_alias]) < 0) return -1;
                b = probe_pipe(alias_fd[n_alias]);
                if (!b) { close(alias_fd[n_alias][0]); close(alias_fd[n_alias][1]); }
            }
            if (!b) return -1;
            alias_bit[n_alias] = b;
            n_alias++;
        }
        if (alias_bit[i] & mask) { out[0] = alias_fd[i][0]; out[1] = alias_fd[i][1]; return 0; }
    }
}

/* Parked narrow (mask not all ones) and woken 0 times by unrelated traffic in
 * at least one of 3 windows of 1 s. 0 when fine, 1 after a FAIL. */
static int check_quiet(const char *n, volatile long *tid, volatile int *returned) {
    struct sysw w = wait_parked(tid);
    if (*returned) { fail(n, "the waiter returned before any event"); return 1; }
    if (!w.found || w.state != 'S' || !w.mask) {
        fail(n, "waiter never parked (found %d state %c mask %llx)", w.found, w.state ? w.state : '?', w.mask);
        return 1;
    }
    if (w.mask == ~0ULL) { fail(n, "parked with the broadcast mask (all ones)"); return 1; }
    int al[2];
    if (alias_pipe(w.mask, al) < 0) {
        /* (63/64)^320 < 1 in 150 000: only a probe failing does this. */
        { unsigned long long u = 0; int z = 0, all = 0;
          for (int i = 0; i < n_alias; i++) { u |= alias_bit[i]; if (!alias_bit[i]) z++; }
          (void)all; printf("alias: union %#llx zero %d first %#llx last %#llx\n", u, z, alias_bit[0], alias_bit[n_alias-1]); }
        fail(n, "no pipe parks on a bit of mask %#llx (%d probed)", w.mask, n_alias);
        return 1;
    }
    unsigned long long best = ~0ULL;
    for (int k = 0; k < 3 && best != 0; k++) {
        struct sysw a = sysstat_w(*tid);
        long trips = unrelated_traffic(1000, al);
        struct sysw b = sysstat_w(*tid);
        if (trips < 0) { fail(n, "traffic io failed"); return 1; }
        if (*returned) { fail(n, "the waiter returned with no event"); return 1; }
        unsigned long long dw = b.pwakes - a.pwakes;
        printf("%s: mask %#llx, %ld round trips, %llu wakes\n", n, w.mask, trips, dw);
        if (dw < best) best = dw;
    }
    if (best != 0) { fail(n, "unrelated traffic woke the waiter (best window: %llu wakes)", best); return 1; }
    return 0;
}

/* A loopback datagram to a socket nobody waits on: its daemon pass takes the
 * 100 ms broadcast window, so the event that follows is woken by key alone. */
static void primer(void) {
    static int sink = -1, src = -1;
    static struct sockaddr_in sa;
    if (sink < 0) {
        sink = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0);
        src = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0);
        sa.sin_family = AF_INET; sa.sin_port = 0; sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        socklen_t l = sizeof sa;
        if (bind(sink, (struct sockaddr *)&sa, sizeof sa) < 0
            || getsockname(sink, (struct sockaddr *)&sa, &l) < 0) return;
    }
    char b[64];
    while (recv(sink, b, sizeof b, 0) > 0) {}
    sendto(src, "p", 1, 0, (struct sockaddr *)&sa, sizeof sa);
    msleep(5);
}

/* ── the waiter thread ─────────────────────────────────────────────────── */

enum op { OP_RECV, OP_EPOLL, OP_POLL, OP_CONNECT };

struct waiter {
    enum op op;
    int fd, ep;
    struct sockaddr_in to;      /* OP_CONNECT */
    volatile long tid;
    volatile int returned;
    volatile long long at;
    volatile long rv;
    volatile int err;
    volatile unsigned rev;      /* OP_EPOLL / OP_POLL: events returned */
};

static void *waiter_thread(void *p) {
    struct waiter *w = p;
    w->tid = syscall(SYS_gettid);
    long r;
    char buf[256];
    switch (w->op) {
    case OP_RECV:
        do r = recv(w->fd, buf, sizeof buf, 0); while (r < 0 && errno == EINTR);
        break;
    case OP_EPOLL: {
        struct epoll_event ev;
        do r = epoll_wait(w->ep, &ev, 1, -1); while (r < 0 && errno == EINTR);
        if (r == 1) w->rev = ev.events;
        break;
    }
    case OP_POLL: {
        struct pollfd pf = { .fd = w->fd, .events = POLLIN };
        do r = poll(&pf, 1, -1); while (r < 0 && errno == EINTR);
        if (r == 1) w->rev = pf.revents;
        break;
    }
    case OP_CONNECT:
        do r = connect(w->fd, (struct sockaddr *)&w->to, sizeof w->to); while (r < 0 && errno == EINTR);
        break;
    }
    w->err = r < 0 ? errno : 0;
    w->at = now_us();
    w->rv = r;
    __atomic_store_n(&w->returned, 1, __ATOMIC_SEQ_CST);
    return NULL;
}

static int start(struct waiter *w, pthread_t *th) {
    w->tid = 0; w->returned = 0; w->rv = 0; w->err = 0; w->rev = 0;
    return pthread_create(th, NULL, waiter_thread, w);
}

/* Wait for the waiter to return after an event at `t1`; 0 = in time. */
static int check_prompt(const char *n, const char *what, struct waiter *w, long long t1) {
    while (!w->returned && now_us() - t1 < 2000000) msleep(1);
    if (!w->returned) { fail(n, "%s did not return the waiter within 2 s", what); return 1; }
    long long d = w->at - t1;
    if (d < 0) d = 0;
    printf("%s: %s returned it after %lld us (rv %ld errno %d events %#x)\n", n, what, d, w->rv, w->err, w->rev);
    if (d > LIMIT_US) { char cb[160]; fail(n, "%s returned it %lld us late (limit 20 ms) %s", what, d, canary_report(t1, w->at, cb, sizeof cb)); return 1; }
    return 0;
}

/* ── helpers ───────────────────────────────────────────────────────────── */

static int udp_bound(in_addr_t addr, struct sockaddr_in *out) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    if (s < 0) return -1;
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = 0 };
    a.sin_addr.s_addr = htonl(addr);
    socklen_t l = sizeof a;
    if (bind(s, (struct sockaddr *)&a, sizeof a) < 0 || getsockname(s, (struct sockaddr *)&a, &l) < 0) {
        close(s); return -1;
    }
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    *out = a;
    return s;
}

static int lo_listener(int *port) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
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

static struct sockaddr_in lo_addr(int port) {
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port) };
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return a;
}

/* ── cases ─────────────────────────────────────────────────────────────── */

static void tcp_recv_case(void) {
    const char *n = "tcp_recv";
    if (skipped(n)) return;
    int port, l = lo_listener(&port);
    if (l < 0) { fail(n, "listener: errno %d", errno); return; }
    int c = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = lo_addr(port);
    if (c < 0 || connect(c, (struct sockaddr *)&a, sizeof a) < 0) { fail(n, "connect: errno %d", errno); close(l); return; }
    int s = accept(l, NULL, NULL);
    if (s < 0) { fail(n, "accept: errno %d", errno); close(c); close(l); return; }
    struct waiter w = { .op = OP_RECV, .fd = s };
    pthread_t th;
    int bad = 0;
    if (start(&w, &th)) { fail(n, "pthread_create"); goto out; }
    bad = check_quiet(n, &w.tid, &w.returned);
    primer();
    long long t1 = now_us();
    if (send(c, "hello", 5, 0) != 5) { fail(n, "send: errno %d", errno); bad = 1; }
    if (!bad) bad = check_prompt(n, "the peer's send", &w, t1);
    if (!bad && w.rv != 5) { fail(n, "recv returned %ld (errno %d), wanted 5", w.rv, w.err); bad = 1; }
    if (!w.returned) send(c, "x", 1, 0);
    pthread_join(th, NULL);
    if (!bad) {
        /* Second wait: the peer's FIN must return recv() with 0. */
        if (start(&w, &th)) { fail(n, "pthread_create"); goto out; }
        struct sysw p = wait_parked(&w.tid);
        if (p.found && p.mask == ~0ULL) { fail(n, "parked with the broadcast mask before the FIN"); bad = 1; }
        primer();
        t1 = now_us();
        shutdown(c, SHUT_WR);
        if (!bad) bad = check_prompt(n, "the peer's FIN", &w, t1);
        if (!bad && w.rv != 0) { fail(n, "recv after the FIN returned %ld (errno %d)", w.rv, w.err); bad = 1; }
        if (!w.returned) shutdown(s, SHUT_RDWR);
        pthread_join(th, NULL);
    }
    if (!bad) pass(n);
out:
    close(s); close(c); close(l);
}

static void udp_recv_case(const char *n, in_addr_t bind_addr) {
    if (skipped(n)) return;
    struct sockaddr_in to;
    int s = udp_bound(bind_addr, &to);
    int src = socket(AF_INET, SOCK_DGRAM, 0);
    if (s < 0 || src < 0) { fail(n, "socket/bind: errno %d", errno); if (s >= 0) close(s); if (src >= 0) close(src); return; }
    struct waiter w = { .op = OP_RECV, .fd = s };
    pthread_t th;
    if (start(&w, &th)) { fail(n, "pthread_create"); close(s); close(src); return; }
    int bad = check_quiet(n, &w.tid, &w.returned);
    primer();
    long long t1 = now_us();
    sendto(src, "dgram", 5, 0, (struct sockaddr *)&to, sizeof to);
    if (!bad) bad = check_prompt(n, "a datagram", &w, t1);
    if (!bad && w.rv != 5) { fail(n, "recv returned %ld (errno %d)", w.rv, w.err); bad = 1; }
    if (!w.returned) sendto(src, "x", 1, 0, (struct sockaddr *)&to, sizeof to);
    pthread_join(th, NULL);
    close(s); close(src);
    if (!bad) pass(n);
}

/* Bind `s` to an ephemeral 127.0.0.1 port; its address, port 0 on failure. */
static struct sockaddr_in bind_lo(int s) {
    struct sockaddr_in a = lo_addr(0);
    socklen_t l = sizeof a;
    if (bind(s, (struct sockaddr *)&a, sizeof a) < 0 || getsockname(s, (struct sockaddr *)&a, &l) < 0)
        a.sin_port = 0;
    return a;
}

static void unbound_recv_case(void) {
    const char *n = "unbound_recv";
    if (skipped(n)) return;
    int s = socket(AF_INET, SOCK_DGRAM, 0), src = socket(AF_INET, SOCK_DGRAM, 0);
    if (s < 0 || src < 0) { fail(n, "socket: errno %d", errno); return; }
    struct waiter w = { .op = OP_RECV, .fd = s };
    pthread_t th;
    if (start(&w, &th)) { fail(n, "pthread_create"); close(s); close(src); return; }
    int bad = check_quiet(n, &w.tid, &w.returned);
    struct sockaddr_in to = bind_lo(s);
    if (!to.sin_port) { fail(n, "bind: errno %d", errno); bad = 1; }
    /* The waiter re-probes under the bound socket's key; it must park again
     * narrow before the datagram. */
    msleep(20);
    if (!bad && w.returned) { fail(n, "recv returned %ld (errno %d) on bind alone", w.rv, w.err); bad = 1; }
    struct sysw p = sysstat_w(w.tid);
    if (!bad && p.found && p.mask == ~0ULL) { fail(n, "re-parked with the broadcast mask after bind"); bad = 1; }
    primer();
    long long t1 = now_us();
    sendto(src, "dgram", 5, 0, (struct sockaddr *)&to, sizeof to);
    if (!bad) bad = check_prompt(n, "a datagram after bind", &w, t1);
    if (!bad && w.rv != 5) { fail(n, "recv returned %ld (errno %d)", w.rv, w.err); bad = 1; }
    if (!w.returned) { shutdown(s, SHUT_RDWR); sendto(src, "x", 1, 0, (struct sockaddr *)&to, sizeof to); }
    pthread_join(th, NULL);
    close(s); close(src);
    if (!bad) pass(n);
}

static void unbound_epollet_case(void) {
    const char *n = "unbound_epollet";
    if (skipped(n)) return;
    int s = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0), src = socket(AF_INET, SOCK_DGRAM, 0);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = { .events = EPOLLIN | EPOLLET, .data.fd = s };
    if (s < 0 || src < 0 || ep < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, s, &ev) < 0) {
        fail(n, "socket/epoll: errno %d", errno); return;
    }
    struct waiter w = { .op = OP_EPOLL, .fd = s, .ep = ep };
    pthread_t th;
    if (start(&w, &th)) { fail(n, "pthread_create"); return; }
    int bad = check_quiet(n, &w.tid, &w.returned);
    struct sockaddr_in to = bind_lo(s);
    if (!to.sin_port) { fail(n, "bind: errno %d", errno); bad = 1; }
    msleep(20);
    if (!bad && w.returned) { fail(n, "epoll_wait returned %#x on bind alone", w.rev); bad = 1; }
    primer();
    long long t1 = now_us();
    sendto(src, "dgram", 5, 0, (struct sockaddr *)&to, sizeof to);
    if (!bad) bad = check_prompt(n, "a datagram after bind", &w, t1);
    if (!bad && !(w.rev & EPOLLIN)) { fail(n, "epoll_wait returned %ld, events %#x", w.rv, w.rev); bad = 1; }
    if (!w.returned) { sendto(src, "x", 1, 0, (struct sockaddr *)&to, sizeof to); }
    pthread_join(th, NULL);
    close(ep); close(s); close(src);
    if (!bad) pass(n);
}

static int echo_req(int s, unsigned short seq) {
    struct icmphdr h = { .type = ICMP_ECHO, .code = 0 };
    h.un.echo.sequence = htons(seq);
    struct sockaddr_in to = lo_addr(0);
    return sendto(s, &h, sizeof h, 0, (struct sockaddr *)&to, sizeof to) == (ssize_t)sizeof h ? 0 : -1;
}

static void icmp_recv_case(void) {
    const char *n = "icmp_recv";
    if (skipped(n)) return;
    int s = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    if (s < 0) { skip(n, "(no ping socket)"); return; }
    /* Give it its socket and drain what the echo brought (on loopback the
     * ping socket may see its own request as well as the reply). */
    if (echo_req(s, 1) < 0) { fail(n, "first echo: errno %d", errno); close(s); return; }
    char buf[128];
    struct pollfd pf = { .fd = s, .events = POLLIN };
    if (poll(&pf, 1, 2000) == 1) {
        msleep(50);
        while (recv(s, buf, sizeof buf, MSG_DONTWAIT) > 0) {}
    }
    struct waiter w = { .op = OP_RECV, .fd = s };
    pthread_t th;
    if (start(&w, &th)) { fail(n, "pthread_create"); close(s); return; }
    int bad = check_quiet(n, &w.tid, &w.returned);
    primer();
    long long t1 = now_us();
    if (echo_req(s, 2) < 0) { fail(n, "echo: errno %d", errno); bad = 1; }
    if (!bad) bad = check_prompt(n, "an echo reply", &w, t1);
    if (!bad && w.rv <= 0) { fail(n, "recv returned %ld (errno %d)", w.rv, w.err); bad = 1; }
    if (!w.returned) echo_req(s, 3);
    pthread_join(th, NULL);
    close(s);
    if (!bad) pass(n);
}

static void icmp_unbound_case(void) {
    const char *n = "icmp_unbound";
    if (skipped(n)) return;
    int s = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    if (s < 0) { skip(n, "(no ping socket)"); return; }
    struct waiter w = { .op = OP_POLL, .fd = s };
    pthread_t th;
    if (start(&w, &th)) { fail(n, "pthread_create"); close(s); return; }
    int bad = check_quiet(n, &w.tid, &w.returned);
    primer();
    long long t1 = now_us();
    if (echo_req(s, 1) < 0) { fail(n, "echo: errno %d", errno); bad = 1; }
    if (!bad) bad = check_prompt(n, "the first echo's reply", &w, t1);
    if (!bad && !(w.rev & POLLIN)) { fail(n, "poll returned %ld, revents %#x", w.rv, w.rev); bad = 1; }
    if (!w.returned) echo_req(s, 2);
    pthread_join(th, NULL);
    close(s);
    if (!bad) pass(n);
}

/* An address on the NIC's subnet nobody answers ARP for, or 0. */
static in_addr_t silent_neighbour(void) {
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    if (u < 0) return 0;
    struct sockaddr_in gw = { .sin_family = AF_INET, .sin_port = htons(9) };
    gw.sin_addr.s_addr = inet_addr("10.0.2.2");
    struct sockaddr_in me; socklen_t l = sizeof me;
    in_addr_t r = 0;
    if (connect(u, (struct sockaddr *)&gw, sizeof gw) == 0
        && getsockname(u, (struct sockaddr *)&me, &l) == 0) {
        unsigned a = ntohl(me.sin_addr.s_addr);
        if ((a >> 24) != 127 && a != 0) {
            unsigned t = (a & 0xffffff00u) | 77u;
            if (t == a) t = (a & 0xffffff00u) | 78u;
            r = htonl(t);
        }
    }
    close(u);
    return r;
}

static void connect_pending_case(void) {
    const char *n = "connect_pending";
    if (skipped(n)) return;
    in_addr_t dst = silent_neighbour();
    if (!dst) { skip(n, "(no NIC route)"); return; }
    int s = socket(AF_INET, SOCK_STREAM, 0);
    if (s < 0) { fail(n, "socket: errno %d", errno); return; }
    struct waiter w = { .op = OP_CONNECT, .fd = s };
    w.to.sin_family = AF_INET; w.to.sin_port = htons(80); w.to.sin_addr.s_addr = dst;
    pthread_t th;
    if (start(&w, &th)) { fail(n, "pthread_create"); close(s); return; }
    int bad = check_quiet(n, &w.tid, &w.returned);
    primer();
    long long t1 = now_us();
    shutdown(s, SHUT_RDWR);
    if (!bad) bad = check_prompt(n, "shutdown()", &w, t1);
    if (!bad && w.rv == 0) { fail(n, "connect succeeded"); bad = 1; }
    pthread_join(th, NULL);   /* worst case: the 75 s connect timeout */
    close(s);
    if (!bad) pass(n);
}

static void connect_lo_case(void) {
    const char *n = "connect_lo";
    if (skipped(n)) return;
    int port, l = lo_listener(&port);
    if (l < 0) { fail(n, "listener: errno %d", errno); return; }
    /* A closed port: a listener's, closed again. */
    int port2, l2 = lo_listener(&port2);
    if (l2 >= 0) close(l2);
    long long worst_ok = 0, worst_ref = 0;
    int bad = 0;
    for (int i = 0; i < 20 && !bad; i++) {
        for (int k = 0; k < 2 && !bad; k++) {
            int s = socket(AF_INET, SOCK_STREAM, 0);
            struct sockaddr_in a = lo_addr(k ? port2 : port);
            primer();
            long long t1 = now_us();
            int r = connect(s, (struct sockaddr *)&a, sizeof a);
            int e = errno;
            long long d = now_us() - t1;
            if (k == 0 && r != 0) { fail(n, "connect %d to the listener: errno %d", i, e); bad = 1; }
            if (k == 1 && (r == 0 || e != ECONNREFUSED)) { fail(n, "connect %d to a closed port: rv %d errno %d", i, r, e); bad = 1; }
            if (k == 0 && d > worst_ok) worst_ok = d;
            if (k == 1 && d > worst_ref) worst_ref = d;
            if (!bad && d > LIMIT_US) { char cb[160]; fail(n, "connect %d (%s) took %lld us %s", i, k ? "refused" : "accepted", d, canary_report(t1, t1 + d, cb, sizeof cb)); bad = 1; }
            close(s);
            if (k == 0) { int c = accept(l, NULL, NULL); if (c >= 0) close(c); }
        }
    }
    printf("%s: worst %lld us accepted, %lld us refused\n", n, worst_ok, worst_ref);
    close(l);
    if (!bad) pass(n);
}

int main(int argc, char **argv) {
    for (int i = 1; i < argc; i++)
        if (!strcmp(argv[i], "-x") && i + 1 < argc && n_skips < 16) skips[n_skips++] = argv[++i];
    setvbuf(stdout, NULL, _IOLBF, 0);
    static const char *all[] = { "tcp_recv", "udp_recv", "udpany_recv", "unbound_recv", "unbound_epollet",
                                 "icmp_recv", "icmp_unbound", "connect_pending", "connect_lo" };
    int sfd = open("/proc/sysstat", O_RDONLY);
    if (sfd < 0) {
        for (unsigned i = 0; i < sizeof all / sizeof *all; i++) skip(all[i], "(no /proc/sysstat)");
    } else {
        close(sfd);
        canary_start();
        tcp_recv_case();
        udp_recv_case("udp_recv", INADDR_LOOPBACK);
        udp_recv_case("udpany_recv", INADDR_ANY);
        unbound_recv_case();
        unbound_epollet_case();
        icmp_recv_case();
        icmp_unbound_case();
        connect_pending_case();
        connect_lo_case();
        canary_stop();
    }
    printf("nettag2: %d passed, %d failed, %d skipped\n", n_pass, n_fail, n_skip);
    return n_fail > 125 ? 125 : n_fail;
}
