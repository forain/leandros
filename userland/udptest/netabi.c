/* ── lane netabi: net stack ABI gaps (bug list L9.3/4/6/9, L2.2) ──────────
 *
 * Every case is checked against a Linux host kernel.
 *
 *   eph_port_spread     20 loopback TCP connects whose server side closes
 *                       first (so no TIME_WAIT reservation hides it) and 20
 *                       UDP connects each get a port in 32768-60999 and no
 *                       port twice in a row (allocation was lowest-free from
 *                       `ticks % range`, so a port came straight back)
 *   unix_accept_closed  an AF_UNIX connector that wrote and closed before the
 *                       accept is still accepted: the data reads back, then
 *                       EOF; the backlog is FIFO (it used to be dropped)
 *   unix_listener_reset closing an AF_UNIX listener with a connector nobody
 *                       accepted: the connector polls POLLIN|POLLHUP|POLLERR,
 *                       SO_ERROR is ECONNRESET once, then reads see EOF
 *   tcp_nodelay_push    TCP_NODELAY set with a Nagle-held byte pending pushes
 *                       it; the keepalive options read back on a live
 *                       connection and probes (KEEPIDLE 1 KEEPINTVL 1
 *                       KEEPCNT 2) keep an idle loopback connection alive
 *   unix_dgram_rights   SCM_RIGHTS on an AF_UNIX datagram socketpair: the fd
 *                       arrives with its own record, not with the next one
 *                       (they were dropped without a word)
 *   unix_dgram_trunc    a datagram longer than the buffer: MSG_TRUNC in
 *                       msg_flags, and recv/recvmsg(MSG_TRUNC) answer the
 *                       full length; a zero-length datagram reads as 0
 *   unix_dgram_sndbuf   SO_SNDBUF reads back doubled (minimum 4608), a
 *                       datagram over sndbuf-32 is EMSGSIZE and a small
 *                       sndbuf fills after a few datagrams (it was ignored)
 */

#include <netinet/tcp.h>
#include <sys/un.h>

/* ── L9.3 ephemeral ports ────────────────────────────────────────────── */

static int local_port(int fd) {
    struct sockaddr_in a; socklen_t l = sizeof a;
    if (getsockname(fd, (struct sockaddr *)&a, &l) < 0) return -1;
    return ntohs(a.sin_port);
}

static void eph_port_spread(void) {
    const char *N = "eph_port_spread";
    if (skipped(N)) return;
    char why[200] = "";
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = sin4("127.0.0.1", 0);
    socklen_t al = sizeof a;
    if (l < 0 || bind(l, (struct sockaddr *)&a, sizeof a) < 0 || listen(l, 8) < 0
        || getsockname(l, (struct sockaddr *)&a, &al) < 0) { fail(N, "listener: %s", strerror(errno)); return; }
    int prev = -1;
    for (int i = 0; i < 20 && !why[0]; i++) {
        int c = socket(AF_INET, SOCK_STREAM, 0);
        if (c < 0 || connect(c, (struct sockaddr *)&a, sizeof a) < 0) {
            snprintf(why, sizeof why, "tcp connect %d: %s", i, strerror(errno)); if (c >= 0) close(c); break;
        }
        int p = local_port(c);
        int s = accept(l, NULL, NULL);
        if (s >= 0) close(s);   /* the server closes first: no TIME_WAIT for p */
        close(c);
        if (p < 32768 || p > 60999) snprintf(why, sizeof why, "tcp port %d outside 32768-60999", p);
        else if (p == prev) snprintf(why, sizeof why, "tcp connect %d reused port %d at once", i, p);
        prev = p;
    }
    close(l);
    struct sockaddr_in d = sin4("127.0.0.1", 9);
    prev = -1;
    for (int i = 0; i < 20 && !why[0]; i++) {
        int u = socket(AF_INET, SOCK_DGRAM, 0);
        if (u < 0 || connect(u, (struct sockaddr *)&d, sizeof d) < 0) {
            snprintf(why, sizeof why, "udp connect %d: %s", i, strerror(errno)); if (u >= 0) close(u); break;
        }
        int p = local_port(u);
        close(u);
        if (p < 32768 || p > 60999) snprintf(why, sizeof why, "udp port %d outside 32768-60999", p);
        else if (p == prev) snprintf(why, sizeof why, "udp connect %d reused port %d at once", i, p);
        prev = p;
    }
    if (why[0]) fail(N, "%s", why); else pass(N);
}

/* ── L9.4 / L2.2 AF_UNIX backlog ─────────────────────────────────────── */

/* An abstract-namespace AF_UNIX stream listener; its address in *sa. */
static int unix_listener(const char *tag, struct sockaddr_un *sa, socklen_t *sl) {
    memset(sa, 0, sizeof *sa);
    sa->sun_family = AF_UNIX;
    int n = snprintf(sa->sun_path + 1, sizeof sa->sun_path - 1, "netabi-%s-%d", tag, (int)getpid());
    *sl = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + n);
    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    if (l < 0 || bind(l, (struct sockaddr *)sa, *sl) < 0 || listen(l, 8) < 0) { if (l >= 0) close(l); return -1; }
    return l;
}

static int unix_connect(const struct sockaddr_un *sa, socklen_t sl) {
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    if (c < 0) return -1;
    if (connect(c, (const struct sockaddr *)sa, sl) < 0) { close(c); return -1; }
    return c;
}

static void unix_accept_closed(void) {
    const char *N = "unix_accept_closed";
    if (skipped(N)) return;
    struct sockaddr_un sa; socklen_t sl;
    int l = unix_listener("acc", &sa, &sl);
    if (l < 0) { fail(N, "listener: %s", strerror(errno)); return; }
    char why[200] = "";
    int c1 = unix_connect(&sa, sl), c2 = unix_connect(&sa, sl);
    if (c1 < 0 || c2 < 0) { fail(N, "connect: %s", strerror(errno)); close(l); return; }
    if (write(c1, "first", 5) != 5 || write(c2, "second", 6) != 6) snprintf(why, sizeof why, "write: %s", strerror(errno));
    close(c1);
    close(c2);
    if (!why[0] && !(wait_ev(l, POLLIN, 1000) & POLLIN)) snprintf(why, sizeof why, "listener not readable after the connectors closed");
    const char *want[2] = { "first", "second" };
    for (int k = 0; k < 2 && !why[0]; k++) {
        int s = accept4(l, NULL, NULL, SOCK_NONBLOCK);
        if (s < 0) { snprintf(why, sizeof why, "accept %d: %s (closed connector dropped)", k, strerror(errno)); break; }
        char b[16] = { 0 };
        ssize_t n = read(s, b, sizeof b - 1);
        ssize_t e = read(s, b + 8, 4);
        if (n != (ssize_t)strlen(want[k]) || memcmp(b, want[k], n))
            snprintf(why, sizeof why, "accept %d read %zd \"%s\", want \"%s\"", k, n, n > 0 ? b : "", want[k]);
        else if (e != 0) snprintf(why, sizeof why, "accept %d: second read %zd (%s), want EOF", k, e, e < 0 ? strerror(errno) : "");
        close(s);
    }
    if (!why[0]) {
        fcntl(l, F_SETFL, O_NONBLOCK);
        int s = accept4(l, NULL, NULL, SOCK_NONBLOCK);
        if (s >= 0 || errno != EAGAIN) { snprintf(why, sizeof why, "third accept: %d (%s), want EAGAIN", s, strerror(errno)); if (s >= 0) close(s); }
    }
    close(l);
    if (why[0]) fail(N, "%s", why); else pass(N);
}

static void unix_listener_reset(void) {
    const char *N = "unix_listener_reset";
    if (skipped(N)) return;
    struct sockaddr_un sa; socklen_t sl;
    int l = unix_listener("rst", &sa, &sl);
    if (l < 0) { fail(N, "listener: %s", strerror(errno)); return; }
    char why[200] = "";
    int c = unix_connect(&sa, sl);
    if (c < 0) { fail(N, "connect: %s", strerror(errno)); close(l); return; }
    int c2 = unix_connect(&sa, sl);
    close(l);
    int ev = wait_ev(c, POLLIN, 1000);
    if ((ev & (POLLIN | POLLHUP | POLLERR)) != (POLLIN | POLLHUP | POLLERR))
        snprintf(why, sizeof why, "connector revents %#x, want POLLIN|POLLHUP|POLLERR", ev);
    int err = -1; socklen_t el = sizeof err;
    if (!why[0] && (getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &el) < 0 || err != ECONNRESET))
        snprintf(why, sizeof why, "SO_ERROR %d (%s), want ECONNRESET", err, strerror(err));
    el = sizeof err;
    if (!why[0] && (getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &el) < 0 || err != 0))
        snprintf(why, sizeof why, "second SO_ERROR %d, want 0", err);
    char b[4];
    ssize_t n = why[0] ? 0 : read(c, b, sizeof b);
    if (!why[0] && n != 0) snprintf(why, sizeof why, "read after SO_ERROR: %zd (%s), want EOF", n, n < 0 ? strerror(errno) : "");
    /* Without SO_ERROR first, the first read reports the reset. */
    if (!why[0] && c2 >= 0) {
        n = read(c2, b, sizeof b);
        if (n != -1 || errno != ECONNRESET) snprintf(why, sizeof why, "first read %zd (%s), want ECONNRESET", n, n < 0 ? strerror(errno) : "");
        else if ((n = read(c2, b, sizeof b)) != 0) snprintf(why, sizeof why, "second read %zd, want EOF", n);
    }
    close(c);
    if (c2 >= 0) close(c2);
    if (why[0]) fail(N, "%s", why); else pass(N);
}

/* ── L9.6 TCP_NODELAY / keepalive ────────────────────────────────────── */

static void tcp_nodelay_push(void) {
    const char *N = "tcp_nodelay_push";
    if (skipped(N)) return;
    int c, s;
    if (tcp_pair(&c, &s) < 0) { fail(N, "tcp pair: %s", strerror(errno)); return; }
    char why[200] = "";
    /* One byte goes out, the second waits for its ACK (Nagle); setting
     * TCP_NODELAY must push it. */
    if (write(c, "a", 1) != 1 || write(c, "b", 1) != 1) snprintf(why, sizeof why, "write: %s", strerror(errno));
    int on = 1;
    if (!why[0] && setsockopt(c, IPPROTO_TCP, TCP_NODELAY, &on, sizeof on) < 0)
        snprintf(why, sizeof why, "TCP_NODELAY: %s", strerror(errno));
    char b[4]; int got = 0;
    long long end = now_ms() + 500;
    while (!why[0] && got < 2 && now_ms() < end) {
        if (!(wait_ev(s, POLLIN, 50) & POLLIN)) continue;
        ssize_t n = recv(s, b + got, sizeof b - got, MSG_DONTWAIT);
        if (n > 0) got += (int)n;
    }
    if (!why[0] && (got != 2 || memcmp(b, "ab", 2))) snprintf(why, sizeof why, "server got %d bytes within 500 ms", got);
    /* Keepalive options read back as set, and probes every second keep an
     * idle connection alive past KEEPIDLE + KEEPINTVL * KEEPCNT (the peer
     * answers them). */
    int v[4] = { 1, 1, 1, 2 }, opt[4] = { SO_KEEPALIVE, TCP_KEEPIDLE, TCP_KEEPINTVL, TCP_KEEPCNT };
    for (int i = 0; i < 4 && !why[0]; i++) {
        int lvl = i ? IPPROTO_TCP : SOL_SOCKET, r = -1; socklen_t rl = sizeof r;
        if (setsockopt(c, lvl, opt[i], &v[i], sizeof v[i]) < 0
            || getsockopt(c, lvl, opt[i], &r, &rl) < 0 || r != v[i])
            snprintf(why, sizeof why, "keepalive option %d: set %d read %d (%s)", opt[i], v[i], r, strerror(errno));
    }
    if (!why[0]) {
        msleep(4000);
        int err = -1; socklen_t el = sizeof err;
        getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &el);
        if (err != 0) snprintf(why, sizeof why, "idle keepalive connection failed: %s", strerror(err));
        else if (write(c, "z", 1) != 1) snprintf(why, sizeof why, "write after idle: %s", strerror(errno));
        else if (!(wait_ev(s, POLLIN, 1000) & POLLIN) || recv(s, b, 1, 0) != 1 || b[0] != 'z')
            snprintf(why, sizeof why, "byte after idle not received");
    }
    close(c); close(s);
    if (why[0]) fail(N, "%s", why); else pass(N);
}

/* ── L9.9 AF_UNIX datagrams ──────────────────────────────────────────── */

static ssize_t send_fd(int sock, const char *data, size_t len, int fd) {
    struct iovec iov = { (void *)data, len };
    union { struct cmsghdr h; char b[CMSG_SPACE(sizeof(int))]; } u;
    struct msghdr m = { 0 };
    m.msg_iov = &iov; m.msg_iovlen = 1;
    if (fd >= 0) {
        memset(&u, 0, sizeof u);
        m.msg_control = u.b; m.msg_controllen = sizeof u.b;
        struct cmsghdr *c = CMSG_FIRSTHDR(&m);
        c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(c), &fd, sizeof fd);
    }
    return sendmsg(sock, &m, 0);
}

/* recvmsg one record into b; the received fd (or -1) in *fd, msg_flags in *fl. */
static ssize_t recv_fd(int sock, char *b, size_t cap, int *fd, int *fl, int flags) {
    struct iovec iov = { b, cap };
    union { struct cmsghdr h; char b[CMSG_SPACE(4 * sizeof(int))]; } u;
    struct msghdr m = { 0 };
    m.msg_iov = &iov; m.msg_iovlen = 1;
    m.msg_control = u.b; m.msg_controllen = sizeof u.b;
    *fd = -1;
    ssize_t n = recvmsg(sock, &m, flags);
    if (n < 0) return n;
    *fl = m.msg_flags;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c))
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) memcpy(fd, CMSG_DATA(c), sizeof *fd);
    return n;
}

static void unix_dgram_rights(void) {
    const char *N = "unix_dgram_rights";
    if (skipped(N)) return;
    int sp[2], pp[2];
    if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sp) < 0 || pipe(pp) < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    char why[200] = "";
    if (send_fd(sp[0], "plain", 5, -1) != 5 || send_fd(sp[0], "withfd", 6, pp[1]) != 6
        || send_fd(sp[0], "after", 5, -1) != 5)
        snprintf(why, sizeof why, "sendmsg: %s", strerror(errno));
    close(pp[1]);
    const char *want[3] = { "plain", "withfd", "after" };
    for (int k = 0; k < 3 && !why[0]; k++) {
        char b[16] = { 0 }; int fd = -1, fl = 0;
        ssize_t n = recv_fd(sp[1], b, sizeof b, &fd, &fl, MSG_DONTWAIT);
        if (n != (ssize_t)strlen(want[k]) || memcmp(b, want[k], n)) {
            snprintf(why, sizeof why, "record %d: %zd \"%s\" (%s), want \"%s\"", k, n, n > 0 ? b : "", n < 0 ? strerror(errno) : "", want[k]);
        } else if ((k == 1) != (fd >= 0)) {
            snprintf(why, sizeof why, "record %d (\"%s\") came with fd %d", k, want[k], fd);
        } else if (fd >= 0) {
            char r[4] = { 0 };
            if (write(fd, "ok", 2) != 2 || read(pp[0], r, 2) != 2 || memcmp(r, "ok", 2))
                snprintf(why, sizeof why, "received fd does not reach the pipe");
            close(fd);
        }
    }
    close(sp[0]); close(sp[1]); close(pp[0]);
    if (why[0]) fail(N, "%s", why); else pass(N);
}

static void unix_dgram_trunc(void) {
    const char *N = "unix_dgram_trunc";
    if (skipped(N)) return;
    int sp[2];
    if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sp) < 0) { fail(N, "socketpair: %s", strerror(errno)); return; }
    char why[200] = "", big[100], b[16];
    memset(big, 'q', sizeof big);
    int fd, fl;
    ssize_t n;
    if (send(sp[0], big, 100, 0) != 100) snprintf(why, sizeof why, "send: %s", strerror(errno));
    if (!why[0] && ((n = recv_fd(sp[1], b, 10, &fd, &fl, MSG_DONTWAIT)) != 10 || !(fl & MSG_TRUNC)))
        snprintf(why, sizeof why, "recvmsg: %zd flags %#x, want 10 with MSG_TRUNC", n, n < 0 ? 0 : fl);
    if (!why[0] && send(sp[0], big, 100, 0) != 100) snprintf(why, sizeof why, "send: %s", strerror(errno));
    if (!why[0] && (n = recv(sp[1], b, 10, MSG_TRUNC | MSG_DONTWAIT)) != 100)
        snprintf(why, sizeof why, "recv(MSG_TRUNC) answered %zd, want 100", n);
    if (!why[0] && send(sp[0], big, 100, 0) != 100) snprintf(why, sizeof why, "send: %s", strerror(errno));
    if (!why[0] && ((n = recv_fd(sp[1], b, 10, &fd, &fl, MSG_TRUNC | MSG_DONTWAIT)) != 100 || !(fl & MSG_TRUNC)))
        snprintf(why, sizeof why, "recvmsg(MSG_TRUNC): %zd flags %#x, want 100 with MSG_TRUNC", n, n < 0 ? 0 : fl);
    /* A record that fits: no MSG_TRUNC. */
    if (!why[0] && send(sp[0], big, 8, 0) != 8) snprintf(why, sizeof why, "send: %s", strerror(errno));
    if (!why[0] && ((n = recv_fd(sp[1], b, 10, &fd, &fl, MSG_DONTWAIT)) != 8 || (fl & MSG_TRUNC)))
        snprintf(why, sizeof why, "short record: %zd flags %#x", n, n < 0 ? 0 : fl);
    /* A zero-length datagram is a record: it reads as 0, not EAGAIN. */
    if (!why[0] && send(sp[0], big, 0, 0) != 0) snprintf(why, sizeof why, "send 0: %s", strerror(errno));
    if (!why[0] && (n = recv(sp[1], b, sizeof b, MSG_DONTWAIT)) != 0)
        snprintf(why, sizeof why, "zero-length datagram read %zd (%s)", n, n < 0 ? strerror(errno) : "");
    if (!why[0] && ((n = recv(sp[1], b, sizeof b, MSG_DONTWAIT)) != -1 || errno != EAGAIN))
        snprintf(why, sizeof why, "empty queue read %zd, want EAGAIN", n);
    close(sp[0]); close(sp[1]);
    if (why[0]) fail(N, "%s", why); else pass(N);
}

/* How many `len`-byte datagrams a non-blocking sp[0] takes before EAGAIN. */
static int dgram_fill(int s, size_t len, int max) {
    static char b[4096];
    int k = 0;
    while (k < max && send(s, b, len, MSG_DONTWAIT) == (ssize_t)len) k++;
    return k;
}

static void unix_dgram_sndbuf(void) {
    const char *N = "unix_dgram_sndbuf";
    if (skipped(N)) return;
    int sp[2], dp[2];
    if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sp) < 0 || socketpair(AF_UNIX, SOCK_DGRAM, 0, dp) < 0) {
        fail(N, "socketpair: %s", strerror(errno)); return;
    }
    char why[200] = "";
    int v = 4096, r = 0; socklen_t rl = sizeof r;
    if (setsockopt(sp[0], SOL_SOCKET, SO_SNDBUF, &v, sizeof v) < 0
        || getsockopt(sp[0], SOL_SOCKET, SO_SNDBUF, &r, &rl) < 0 || r != 8192)
        snprintf(why, sizeof why, "SO_SNDBUF 4096 reads back %d, want 8192", r);
    int one = 1; rl = sizeof r;
    if (!why[0] && (setsockopt(dp[1], SOL_SOCKET, SO_SNDBUF, &one, sizeof one) < 0
        || getsockopt(dp[1], SOL_SOCKET, SO_SNDBUF, &r, &rl) < 0 || r != 4608))
        snprintf(why, sizeof why, "SO_SNDBUF 1 reads back %d, want 4608", r);
    static char big[8192];
    ssize_t n;
    if (!why[0] && ((n = send(sp[0], big, 8192 - 31, MSG_DONTWAIT)) != -1 || errno != EMSGSIZE))
        snprintf(why, sizeof why, "send of sndbuf-31 bytes: %zd (%s), want EMSGSIZE", n, n < 0 ? strerror(errno) : "");
    if (!why[0] && (n = send(sp[0], big, 8192 - 32, MSG_DONTWAIT)) != 8192 - 32)
        snprintf(why, sizeof why, "send of sndbuf-32 bytes: %zd (%s)", n, n < 0 ? strerror(errno) : "");
    drain(sp[1]);
    int small = why[0] ? 0 : dgram_fill(sp[0], 1000, 1000);
    int dflt = why[0] ? 0 : dgram_fill(dp[0], 1000, 1000);
    if (!why[0] && (small < 2 || small > 12))
        snprintf(why, sizeof why, "SO_SNDBUF 4096 took %d datagrams of 1000 bytes, want 2-12", small);
    if (!why[0] && dflt < 50)
        snprintf(why, sizeof why, "default SO_SNDBUF took only %d datagrams of 1000 bytes", dflt);
    if (!why[0]) printf("%s: sndbuf 8192 took %d, default %d datagrams of 1000 bytes\n", N, small, dflt);
    close(sp[0]); close(sp[1]); close(dp[0]); close(dp[1]);
    if (why[0]) fail(N, "%s", why); else pass(N);
}

static void netabi_cases(void) {
    eph_port_spread();
    unix_accept_closed();
    unix_listener_reset();
    tcp_nodelay_push();
    unix_dgram_rights();
    unix_dgram_trunc();
    unix_dgram_sndbuf();
}
