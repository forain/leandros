/* ── lane sotimeo: SO_RCVTIMEO / SO_SNDTIMEO, blocking accept, two SYNs ──
 *
 * Every case is checked against a Linux host kernel.
 *
 *   so_timeo_sockopt   the timeval reads back on UDP, TCP and AF_UNIX
 *                      sockets; {0,0} reads back as none; tv_usec 1000000 is
 *                      EDOM and a short optlen EINVAL; the two are separate
 *   so_rcvtimeo_udp    recv/read on an empty UDP socket answer EAGAIN after
 *                      the timeout (they used to wait forever)
 *   so_rcvtimeo_tcp    the same on a connected TCP socket
 *   so_rcvtimeo_unix   the same on AF_UNIX stream and datagram socketpairs
 *   so_sndtimeo_unix   send into a full AF_UNIX socket answers EAGAIN after
 *                      the timeout
 *   so_rcvtimeo_accept accept on a blocking TCP / AF_UNIX listener with a
 *                      timeout answers EAGAIN after it; without one it
 *                      waits for the connection (it used to answer EAGAIN
 *                      at once); an accepted TCP socket inherits the
 *                      listener's timeouts, as Linux clones them
 *   so_timeo_zero      setting {0,0} again removes the timeout
 *   so_timeo_signal    with SA_RESTART, a recv with a timeout is EINTR on a
 *                      signal and one without is restarted (Linux's
 *                      sock_intr_errno)
 *   so_sndtimeo_connect a TCP connect to an address that never answers ends
 *                      with EINPROGRESS at SO_SNDTIMEO (skipped without a
 *                      default route or when the network refuses at once)
 *   tcp_two_syn        four non-blocking connects issued back to back (their
 *                      SYNs reach the listener in one daemon poll) all
 *                      connect and are all accepted; the second used to get
 *                      an RST
 */

#include <sys/un.h>

static int set_tv(int fd, int opt, long s, long us) {
    struct timeval tv = { s, us };
    return setsockopt(fd, SOL_SOCKET, opt, &tv, sizeof tv);
}

static int get_tv(int fd, int opt, struct timeval *tv) {
    socklen_t l = sizeof *tv;
    memset(tv, 0xff, sizeof *tv);
    int r = getsockopt(fd, SOL_SOCKET, opt, tv, &l);
    if (r == 0 && l != sizeof *tv) return -2;
    return r;
}

/* A connected TCP pair over loopback: *c the client, *s the accepted end. */
static int tcp_pair(int *c, int *s) {
    *c = *s = -1;
    int l = socket(AF_INET, SOCK_STREAM, 0);
    if (l < 0) return -1;
    struct sockaddr_in a = sin4("127.0.0.1", 0);
    socklen_t al = sizeof a;
    if (bind(l, (struct sockaddr *)&a, sizeof a) < 0 || listen(l, 4) < 0
        || getsockname(l, (struct sockaddr *)&a, &al) < 0) { close(l); return -1; }
    *c = socket(AF_INET, SOCK_STREAM, 0);
    if (*c < 0 || connect(*c, (struct sockaddr *)&a, sizeof a) < 0) { close(l); return -1; }
    if (!(wait_ev(l, POLLIN, 3000) & POLLIN)) { close(l); return -1; }
    *s = accept(l, NULL, NULL);
    close(l);
    return *s < 0 ? -1 : 0;
}

/* Run `op` on `fd` after setting a 200 ms timeout; check it fails with
 * EAGAIN after about that long. 0 = good; else a reason in `why`. */
enum { OP_RECV, OP_READ, OP_ACCEPT };
static int times_out(int fd, int op, char *why, size_t n) {
    char b[64];
    if (set_tv(fd, SO_RCVTIMEO, 0, 200000) < 0) { snprintf(why, n, "setsockopt: %s", strerror(errno)); return 1; }
    long long t0 = now_ms();
    errno = 0;
    ssize_t r = op == OP_RECV ? recv(fd, b, sizeof b, 0)
              : op == OP_READ ? read(fd, b, sizeof b)
              : accept(fd, NULL, NULL);
    int e = errno;
    long long dt = now_ms() - t0;
    if (r != -1 || (e != EAGAIN && e != EWOULDBLOCK)) {
        snprintf(why, n, "op %d: %zd (%s), want EAGAIN", op, r, strerror(e));
        return 1;
    }
    if (dt < 150 || dt > 2000) { snprintf(why, n, "op %d: EAGAIN after %lld ms, want ~200", op, dt); return 1; }
    return 0;
}

static void so_timeo_sockopt(void) {
    const char *name = "so_timeo_sockopt";
    if (skipped(name)) return;
    int fds[4];
    int sp[2] = { -1, -1 };
    fds[0] = socket(AF_INET, SOCK_DGRAM, 0);
    fds[1] = socket(AF_INET, SOCK_STREAM, 0);
    socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
    fds[2] = sp[0];
    fds[3] = socket(AF_UNIX, SOCK_DGRAM, 0);
    char why[160] = "";
    for (int i = 0; i < 4 && !why[0]; i++) {
        int fd = fds[i];
        struct timeval tv;
        if (fd < 0) { snprintf(why, sizeof why, "socket %d: %s", i, strerror(errno)); break; }
        if (get_tv(fd, SO_RCVTIMEO, &tv) != 0 || tv.tv_sec || tv.tv_usec) {
            snprintf(why, sizeof why, "sock %d: default rcvtimeo %ld.%06ld", i, (long)tv.tv_sec, (long)tv.tv_usec); break;
        }
        if (set_tv(fd, SO_RCVTIMEO, 1, 500000) < 0 || set_tv(fd, SO_SNDTIMEO, 3, 250000) < 0) {
            snprintf(why, sizeof why, "sock %d: set: %s", i, strerror(errno)); break;
        }
        if (get_tv(fd, SO_RCVTIMEO, &tv) != 0 || tv.tv_sec != 1 || tv.tv_usec != 500000) {
            snprintf(why, sizeof why, "sock %d: rcvtimeo reads %ld.%06ld", i, (long)tv.tv_sec, (long)tv.tv_usec); break;
        }
        if (get_tv(fd, SO_SNDTIMEO, &tv) != 0 || tv.tv_sec != 3 || tv.tv_usec != 250000) {
            snprintf(why, sizeof why, "sock %d: sndtimeo reads %ld.%06ld", i, (long)tv.tv_sec, (long)tv.tv_usec); break;
        }
        errno = 0;
        if (set_tv(fd, SO_RCVTIMEO, 1, 1000000) != -1 || errno != EDOM) {
            snprintf(why, sizeof why, "sock %d: usec 1e6: %s, want EDOM", i, strerror(errno)); break;
        }
        struct timeval s = { 1, 0 };
        errno = 0;
        if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &s, 8) != -1 || errno != EINVAL) {
            snprintf(why, sizeof why, "sock %d: optlen 8: %s, want EINVAL", i, strerror(errno)); break;
        }
        if (set_tv(fd, SO_RCVTIMEO, 0, 0) < 0 || get_tv(fd, SO_RCVTIMEO, &tv) != 0 || tv.tv_sec || tv.tv_usec) {
            snprintf(why, sizeof why, "sock %d: {0,0} reads %ld.%06ld", i, (long)tv.tv_sec, (long)tv.tv_usec); break;
        }
    }
    for (int i = 0; i < 4; i++) if (fds[i] >= 0) close(fds[i]);
    if (sp[1] >= 0) close(sp[1]);
    if (why[0]) fail(name, "%s", why); else pass(name);
}

static void so_rcvtimeo_udp(void) {
    const char *name = "so_rcvtimeo_udp";
    if (skipped(name)) return;
    int u = udp_bound("127.0.0.1", 0, NULL);
    if (u < 0) { fail(name, "socket: %s", strerror(errno)); return; }
    char why[160] = "";
    if (!times_out(u, OP_RECV, why, sizeof why)) times_out(u, OP_READ, why, sizeof why);
    close(u);
    if (why[0]) fail(name, "%s", why); else pass(name);
}

static void so_rcvtimeo_tcp(void) {
    const char *name = "so_rcvtimeo_tcp";
    if (skipped(name)) return;
    int c, s;
    if (tcp_pair(&c, &s) < 0) { fail(name, "tcp pair: %s", strerror(errno)); if (c >= 0) close(c); return; }
    char why[160] = "";
    if (!times_out(s, OP_RECV, why, sizeof why) && !times_out(c, OP_READ, why, sizeof why)) {
        /* Data that arrives in time is still delivered. */
        set_tv(s, SO_RCVTIMEO, 2, 0);
        char b[8];
        if (write(c, "hi", 2) != 2 || recv(s, b, sizeof b, 0) != 2)
            snprintf(why, sizeof why, "data with a timeout set: %s", strerror(errno));
    }
    close(c); close(s);
    if (why[0]) fail(name, "%s", why); else pass(name);
}

static void so_rcvtimeo_unix(void) {
    const char *name = "so_rcvtimeo_unix";
    if (skipped(name)) return;
    char why[160] = "";
    int types[2] = { SOCK_STREAM, SOCK_DGRAM };
    for (int i = 0; i < 2 && !why[0]; i++) {
        int sp[2];
        if (socketpair(AF_UNIX, types[i], 0, sp) < 0) { snprintf(why, sizeof why, "socketpair: %s", strerror(errno)); break; }
        if (!times_out(sp[0], OP_RECV, why, sizeof why)) times_out(sp[1], OP_READ, why, sizeof why);
        close(sp[0]); close(sp[1]);
    }
    if (why[0]) fail(name, "%s", why); else pass(name);
}

static void so_sndtimeo_unix(void) {
    const char *name = "so_sndtimeo_unix";
    if (skipped(name)) return;
    int sp[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sp) < 0) { fail(name, "socketpair: %s", strerror(errno)); return; }
    set_tv(sp[0], SO_SNDTIMEO, 0, 200000);
    static char big[65536];
    long long t0 = now_ms(), tfull = -1;
    ssize_t r = 0;
    int e = 0;
    /* Fill the ring; the send that finds it full waits 200 ms, then EAGAIN. */
    while (now_ms() - t0 < 10000) {
        long long t1 = now_ms();
        errno = 0;
        r = send(sp[0], big, sizeof big, MSG_NOSIGNAL);
        e = errno;
        if (r < 0) { tfull = now_ms() - t1; break; }
    }
    close(sp[0]); close(sp[1]);
    if (r != -1 || (e != EAGAIN && e != EWOULDBLOCK)) fail(name, "send: %zd (%s), want EAGAIN", r, strerror(e));
    else if (tfull < 150 || tfull > 2000) fail(name, "EAGAIN after %lld ms, want ~200", tfull);
    else pass(name);
}

struct delayed { int ms; int fd; struct sockaddr_storage to; socklen_t tolen; const char *msg; int sig; pthread_t target; };

/* After `ms`: connect a new socket of to's family to `to`, or send `msg` on
 * `fd`, or signal `target`. */
static void *delayed_act(void *p) {
    struct delayed *d = p;
    msleep(d->ms);
    if (d->sig) { pthread_kill(d->target, d->sig); return NULL; }
    if (d->msg) { send(d->fd, d->msg, strlen(d->msg), MSG_NOSIGNAL); return NULL; }
    int c = socket(d->to.ss_family, SOCK_STREAM, 0);
    if (c >= 0 && connect(c, (struct sockaddr *)&d->to, d->tolen) == 0) d->fd = c;
    else if (c >= 0) close(c);
    return NULL;
}

static void so_rcvtimeo_accept(void) {
    const char *name = "so_rcvtimeo_accept";
    if (skipped(name)) return;
    char why[200] = "";
    /* TCP */
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = sin4("127.0.0.1", 0);
    socklen_t al = sizeof a;
    if (l < 0 || bind(l, (struct sockaddr *)&a, sizeof a) < 0 || listen(l, 4) < 0
        || getsockname(l, (struct sockaddr *)&a, &al) < 0) {
        fail(name, "listener: %s", strerror(errno)); if (l >= 0) close(l); return;
    }
    if (!times_out(l, OP_ACCEPT, why, sizeof why)) {
        /* An accepted socket inherits the listener's timeout (still 200 ms). */
        struct delayed d = { .ms = 300, .fd = -1, .tolen = sizeof a };
        memcpy(&d.to, &a, sizeof a);
        set_tv(l, SO_RCVTIMEO, 0, 0);
        set_tv(l, SO_SNDTIMEO, 0, 300000);
        pthread_t th;
        pthread_create(&th, NULL, delayed_act, &d);
        long long t0 = now_ms();
        int s = accept(l, NULL, NULL);   /* blocking, no timeout: waits */
        long long dt = now_ms() - t0;
        pthread_join(th, NULL);
        struct timeval tv = { -1, -1 };
        if (s < 0) snprintf(why, sizeof why, "blocking tcp accept: %s", strerror(errno));
        else if (dt < 200) snprintf(why, sizeof why, "blocking tcp accept returned after %lld ms, before the connect", dt);
        else if (get_tv(s, SO_SNDTIMEO, &tv) != 0 || tv.tv_sec != 0 || tv.tv_usec != 300000)
            snprintf(why, sizeof why, "accepted sndtimeo %ld.%06ld, want the listener's 0.300000", (long)tv.tv_sec, (long)tv.tv_usec);
        if (s >= 0) close(s);
        if (d.fd >= 0) close(d.fd);
    }
    close(l);
    /* AF_UNIX (abstract name) */
    if (!why[0]) {
        struct sockaddr_un u;
        memset(&u, 0, sizeof u);
        u.sun_family = AF_UNIX;
        snprintf(u.sun_path + 1, sizeof u.sun_path - 1, "sotimeo-%d", (int)getpid());
        socklen_t ul = offsetof(struct sockaddr_un, sun_path) + 1 + strlen(u.sun_path + 1);
        int ls = socket(AF_UNIX, SOCK_STREAM, 0);
        if (ls < 0 || bind(ls, (struct sockaddr *)&u, ul) < 0 || listen(ls, 4) < 0) {
            snprintf(why, sizeof why, "unix listener: %s", strerror(errno));
        } else if (!times_out(ls, OP_ACCEPT, why, sizeof why)) {
            set_tv(ls, SO_RCVTIMEO, 0, 0);
            struct delayed d = { .ms = 300, .fd = -1, .tolen = ul };
            memcpy(&d.to, &u, sizeof u);
            pthread_t th;
            pthread_create(&th, NULL, delayed_act, &d);
            long long t0 = now_ms();
            int s = accept(ls, NULL, NULL);
            long long dt = now_ms() - t0;
            pthread_join(th, NULL);
            if (s < 0) snprintf(why, sizeof why, "blocking unix accept: %s", strerror(errno));
            else if (dt < 200) snprintf(why, sizeof why, "blocking unix accept returned after %lld ms, before the connect", dt);
            if (s >= 0) close(s);
            if (d.fd >= 0) close(d.fd);
        }
        if (ls >= 0) close(ls);
    }
    if (why[0]) fail(name, "%s", why); else pass(name);
}

static void so_timeo_zero(void) {
    const char *name = "so_timeo_zero";
    if (skipped(name)) return;
    int sp[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sp) < 0) { fail(name, "socketpair: %s", strerror(errno)); return; }
    set_tv(sp[0], SO_RCVTIMEO, 0, 100000);
    set_tv(sp[0], SO_RCVTIMEO, 0, 0);
    struct delayed d = { .ms = 400, .fd = sp[1], .msg = "late" };
    pthread_t th;
    pthread_create(&th, NULL, delayed_act, &d);
    char b[8];
    long long t0 = now_ms();
    ssize_t r = recv(sp[0], b, sizeof b, 0);
    int e = errno;
    long long dt = now_ms() - t0;
    pthread_join(th, NULL);
    close(sp[0]); close(sp[1]);
    if (r != 4) fail(name, "recv: %zd (%s) after %lld ms, want the late 4 bytes", r, strerror(e), dt);
    else pass(name);
}

static void on_usr1(int s) { (void)s; }

static void so_timeo_signal(void) {
    const char *name = "so_timeo_signal";
    if (skipped(name)) return;
    struct sigaction sa, old;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sa.sa_flags = SA_RESTART;
    sigaction(SIGUSR1, &sa, &old);
    char why[160] = "";
    int sp[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sp) < 0) { fail(name, "socketpair: %s", strerror(errno)); return; }
    char b[8];
    /* With a timeout: EINTR at the signal, not restarted. */
    set_tv(sp[0], SO_RCVTIMEO, 3, 0);
    struct delayed d = { .ms = 150, .sig = SIGUSR1, .target = pthread_self() };
    pthread_t th;
    pthread_create(&th, NULL, delayed_act, &d);
    long long t0 = now_ms();
    errno = 0;
    ssize_t r = recv(sp[0], b, sizeof b, 0);
    int e = errno;
    long long dt = now_ms() - t0;
    pthread_join(th, NULL);
    if (r != -1 || e != EINTR || dt > 2000)
        snprintf(why, sizeof why, "timed recv + SA_RESTART signal: %zd (%s) after %lld ms, want EINTR", r, strerror(e), dt);
    /* Without one: restarted, then the data arrives. */
    if (!why[0]) {
        set_tv(sp[0], SO_RCVTIMEO, 0, 0);
        struct delayed d2 = { .ms = 400, .fd = sp[1], .msg = "data" };
        pthread_t th2;
        pthread_create(&th, NULL, delayed_act, &d);
        pthread_create(&th2, NULL, delayed_act, &d2);
        errno = 0;
        r = recv(sp[0], b, sizeof b, 0);
        e = errno;
        pthread_join(th, NULL);
        pthread_join(th2, NULL);
        if (r != 4) snprintf(why, sizeof why, "untimed recv + SA_RESTART signal: %zd (%s), want restarted and 4 bytes", r, strerror(e));
    }
    close(sp[0]); close(sp[1]);
    sigaction(SIGUSR1, &old, NULL);
    if (why[0]) fail(name, "%s", why); else pass(name);
}

static void so_sndtimeo_connect(void) {
    const char *name = "so_sndtimeo_connect";
    if (skipped(name)) return;
    char gw[64];
    if (!default_gw(gw, sizeof gw)) { skip(name, "no default route"); return; }
    int c = socket(AF_INET, SOCK_STREAM, 0);
    set_tv(c, SO_SNDTIMEO, 0, 300000);
    struct sockaddr_in to = sin4("192.0.2.1", 9);   /* TEST-NET-1: never answers */
    long long t0 = now_ms();
    errno = 0;
    int r = connect(c, (struct sockaddr *)&to, sizeof to);
    int e = errno;
    long long dt = now_ms() - t0;
    close(c);
    if (r == -1 && e == EINPROGRESS && dt >= 250 && dt < 3000) pass(name);
    else if (r == -1 && e != EINPROGRESS && dt < 250) skip(name, "the network answered at once");
    else fail(name, "connect: %d (%s) after %lld ms, want EINPROGRESS at ~300", r, strerror(e), dt);
}

static void tcp_two_syn(void) {
    const char *name = "tcp_two_syn";
    if (skipped(name)) return;
    enum { N = 4 };
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = sin4("127.0.0.1", 0);
    socklen_t al = sizeof a;
    if (l < 0 || bind(l, (struct sockaddr *)&a, sizeof a) < 0 || listen(l, 16) < 0
        || getsockname(l, (struct sockaddr *)&a, &al) < 0) {
        fail(name, "listener: %s", strerror(errno)); if (l >= 0) close(l); return;
    }
    char why[160] = "";
    int c[N];
    for (int i = 0; i < N; i++) c[i] = tcp_connect_nb(&a);
    for (int i = 0; i < N && !why[0]; i++) {
        int ev = wait_ev(c[i], POLLOUT, 3000);
        int err = -1;
        socklen_t el = sizeof err;
        getsockopt(c[i], SOL_SOCKET, SO_ERROR, &err, &el);
        if (!(ev & POLLOUT) || err != 0)
            snprintf(why, sizeof why, "connect %d: revents %#x, SO_ERROR %s", i, ev, strerror(err));
    }
    set_tv(l, SO_RCVTIMEO, 2, 0);
    for (int i = 0; i < N && !why[0]; i++) {
        int s = accept(l, NULL, NULL);
        if (s < 0) snprintf(why, sizeof why, "accept %d: %s", i, strerror(errno));
        else close(s);
    }
    for (int i = 0; i < N; i++) if (c[i] >= 0) close(c[i]);
    close(l);
    if (why[0]) fail(name, "%s", why); else pass(name);
}

static void so_timeo_cases(void) {
    so_timeo_sockopt();
    so_rcvtimeo_udp();
    so_rcvtimeo_tcp();
    so_rcvtimeo_unix();
    so_sndtimeo_unix();
    so_rcvtimeo_accept();
    so_timeo_zero();
    so_timeo_signal();
    so_sndtimeo_connect();
    tcp_two_syn();
}
