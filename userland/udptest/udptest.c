/*
 * udptest — UDP (and the ICMP / TCP-listener edges next to it) through the
 * net server, with the results Linux gives. Like nettest, the same file
 * builds for the host (cc udptest.c -lpthread, checked on a Linux kernel)
 * and for LeandrOS (zig cc -target <arch>-linux-musl -static).
 *
 *   udptest [-x NAME]...      skip case NAME
 *
 * Everything runs over 127.0.0.1 except udp_getsockname's NIC half, which
 * needs a default route (/proc/net/route) and is skipped without one.
 *
 * Cases:
 *   udp_et_edges        a bound socket registered EPOLLIN|EPOLLOUT|EPOLLET
 *                       reports EPOLLOUT once and then stays quiet; each
 *                       datagram is an EPOLLIN edge, also a second one that
 *                       arrives while the first is unread; quiet again after
 *   icmp_et_edges       the same for a ping socket (echo to 127.0.0.1)
 *   listen_et_edges     a TCP listener registered EPOLLIN|EPOLLET: nothing
 *                       while idle, one edge per connection (also a second
 *                       one while the first is not accepted), quiet between
 *   udp_connect_filter  a connected socket only receives from its peer (a
 *                       datagram from a third socket is dropped, never makes
 *                       it readable); getpeername is the peer
 *   udp_disconnect      connect(AF_UNSPEC) dissolves the association:
 *                       ENOTCONN from getpeername, EDESTADDRREQ from send,
 *                       datagrams from anyone again
 *   udp_sendto_connected sendto() with an address on a connected socket goes
 *                       to that address
 *   udp_send_unconnected send() without a destination is EDESTADDRREQ
 *   udp_peek_trunc      MSG_PEEK leaves the datagram; recv(MSG_TRUNC) answers
 *                       the real length; a short buffer drops the rest;
 *                       recvmsg sets MSG_TRUNC in msg_flags
 *   udp_zero_len        a zero-length datagram is delivered (POLLIN,
 *                       FIONREAD 0, recv 0) and consumed
 *   udp_fionread        FIONREAD is the size of the next datagram only
 *   udp_nonblock        MSG_DONTWAIT and O_NONBLOCK answer EAGAIN at once
 *   udp_getsockname     unbound 0.0.0.0:0; after sendto 0.0.0.0:port; after
 *                       connect the source address the route picks
 *                       (127.0.0.1, and the NIC address for the gateway)
 *   udp_bind_conflict   EADDRINUSE for a second bind of a UDP port (also
 *                       wildcard over specific), allowed with SO_REUSEADDR or
 *                       SO_REUSEPORT on both, free again after close; a
 *                       second bind() of one socket is EINVAL
 *   tcp_bind_conflict   EADDRINUSE for a TCP port held by a listener even
 *                       with SO_REUSEADDR, two bound (not listening) sockets
 *                       with SO_REUSEADDR are fine, without it they are not
 *   udp_shutdown        ENOTCONN when unconnected; SHUT_RD makes recv return 0
 *                       (and poll POLLIN) at once; SHUT_WR makes send EPIPE;
 *                       shutdown wakes a thread blocked in recvfrom
 *   udp_sockopts        SO_RCVBUF/SO_SNDBUF/SO_BROADCAST/SO_REUSEPORT read
 *                       back; IP_ADD_MEMBERSHIP, IP_MULTICAST_LOOP and
 *                       IP_PKTINFO are accepted
 *   udp_burst           3000 nonblocking 1200-byte sends to an unread socket
 *                       answer the length or EAGAIN, nothing else; 500
 *                       blocking sends all succeed
 *   udp_pmtud_notconn   IP_MTU on an unconnected socket is ENOTCONN
 *   udp_pmtud_netns     a path MTU learned in one namespace is invisible in another
 *   tcp_pmtud_clamp     frag-needed shrinks TCP's segment size (TCP_INFO snd_mss/pmtu)
 *   udp6_pmtud          IPV6_MTU_DISCOVER / IPV6_MTU, ICMPv6 packet-too-big, EMSGSIZE
 *   udp_pmtud           IP_MTU_DISCOVER / IP_MTU, frag-needed shrinks the path, DO sends past it are EMSGSIZE
 *   udp_recverr_*       IP_RECVERR: MSG_ERRQUEUE sock_extended_err cmsg (ICMP port unreachable, frag-needed MTU, local EMSGSIZE), POLLERR
 *   udp_huge            4000-, 60000- and 65507-byte datagrams round-trip over loopback; 65508 is EMSGSIZE
 *   udp_large           an 8000-byte datagram over loopback arrives whole
 *   udp_gso             UDP_SEGMENT (cmsg and socket option) splits one send
 *                       into datagrams of the segment size
 *   udp_mmsg            sendmmsg sends 4 datagrams, recvmmsg(MSG_WAITFORONE)
 *                       receives them with their lengths
 *   udp_dup             a dup()'d UDP socket sends and receives
 *   udp_any_poll_tag    a poll on a 0.0.0.0:0 socket + a pipe parks keyed:
 *                       no all-ones mask, AF_UNIX traffic does not wake it,
 *                       a datagram does (anytag.c)
 *   udp_wildcard_connect a socket bound to 0.0.0.0:PORT and then connected
 *                       keeps working (sends, receives, polls)
 *   so_*, tcp_two_syn   SO_RCVTIMEO/SO_SNDTIMEO, blocking accept, two SYNs
 *                       for one listener in one poll (sotimeo.c)
 *
 * Output: "<name>: PASS|FAIL ...|SKIP ..." per case and a summary
 * "udptest: N passed, M failed, K skipped". Exit status = failures (capped).
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/ip.h>
#include <netinet/udp.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

#ifndef UDP_SEGMENT
#define UDP_SEGMENT 103
#endif
#ifndef SOL_UDP
#define SOL_UDP 17
#endif
#ifndef SO_REUSEPORT
#define SO_REUSEPORT 15
#endif

static int npass, nfail, nskip;
static const char *skips[32];
static int nskips;
/* -o NAME: run only the cases whose name starts with NAME (silently). */
static const char *only;

static void pass(const char *n) { printf("%s: PASS\n", n); npass++; fflush(stdout); }
static void skip(const char *n, const char *why) { printf("%s: SKIP %s\n", n, why); nskip++; fflush(stdout); }
static void fail(const char *n, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
static void fail(const char *n, const char *fmt, ...) {
    va_list ap;
    printf("%s: FAIL ", n);
    va_start(ap, fmt); vprintf(fmt, ap); va_end(ap);
    printf("\n"); nfail++; fflush(stdout);
}
static int skipped(const char *n) {
    if (only && strncmp(n, only, strlen(only))) return 1;
    for (int i = 0; i < nskips; i++) if (!strcmp(skips[i], n)) { skip(n, "(-x)"); return 1; }
    return 0;
}

static long long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static void msleep(int ms) { struct timespec t = { ms / 1000, (ms % 1000) * 1000000L }; nanosleep(&t, NULL); }

static struct sockaddr_in sin4(const char *ip, uint16_t port) {
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    inet_pton(AF_INET, ip, &a.sin_addr);
    return a;
}

/* A UDP socket bound to ip:port (port 0 = any); its address in *out. -1 on error. */
static int udp_bound(const char *ip, uint16_t port, struct sockaddr_in *out) {
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) return -1;
    struct sockaddr_in a = sin4(ip, port);
    if (bind(fd, (struct sockaddr *)&a, sizeof a) < 0) { close(fd); return -1; }
    socklen_t l = sizeof a;
    getsockname(fd, (struct sockaddr *)&a, &l);
    if (out) *out = a;
    return fd;
}

static ssize_t send_to(int fd, const void *b, size_t n, const struct sockaddr_in *to) {
    return sendto(fd, b, n, 0, (const struct sockaddr *)to, sizeof *to);
}

/* poll one fd for `ev` up to ms; revents or 0. */
static int wait_ev(int fd, short ev, int ms) {
    struct pollfd p = { fd, ev, 0 };
    return poll(&p, 1, ms) > 0 ? p.revents : 0;
}

/* epoll_wait up to ms; the event mask of the first event, 0 if none. */
static unsigned ep_wait(int ep, int ms) {
    struct epoll_event e[4];
    int n = epoll_wait(ep, e, 4, ms);
    return n > 0 ? e[0].events : 0;
}

/* How many of `rounds` epoll_wait(ep, 10 ms) calls returned an event. */
static int ep_refires(int ep, int rounds) {
    int k = 0;
    for (int i = 0; i < rounds; i++) if (ep_wait(ep, 10)) k++;
    return k;
}

/* Wait up to ms for an event that has `bit` set. */
static int ep_until(int ep, unsigned bit, int ms) {
    long long end = now_ms() + ms;
    while (now_ms() < end) {
        unsigned e = ep_wait(ep, 50);
        if (e & bit) return 1;
    }
    return 0;
}

static int drain(int fd) {
    char b[65536]; int k = 0;
    while (recv(fd, b, sizeof b, MSG_DONTWAIT) >= 0) k++;
    return k;
}

/* ── EPOLLET ────────────────────────────────────────────────────────────── */

static void udp_et_edges(void) {
    const char *N = "udp_et_edges";
    if (skipped(N)) return;
    struct sockaddr_in ra, sa;
    int r = udp_bound("127.0.0.1", 0, &ra), s = udp_bound("127.0.0.1", 0, &sa);
    int ep = epoll_create1(0);
    if (r < 0 || s < 0 || ep < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    struct epoll_event ev = { EPOLLIN | EPOLLOUT | EPOLLET, { .fd = r } };
    epoll_ctl(ep, EPOLL_CTL_ADD, r, &ev);
    unsigned first = ep_wait(ep, 200);
    int quiet0 = ep_refires(ep, 20);
    send_to(s, "a", 1, &ra);
    int in1 = ep_until(ep, EPOLLIN, 1500);
    send_to(s, "b", 1, &ra);
    int in2 = ep_until(ep, EPOLLIN, 1500);
    int quiet1 = ep_refires(ep, 20);
    int got = drain(r);
    int quiet2 = ep_refires(ep, 10);
    send_to(s, "c", 1, &ra);
    int in3 = ep_until(ep, EPOLLIN, 1500);
    drain(r);
    close(ep); close(r); close(s);
    if (!(first & EPOLLOUT)) fail(N, "no initial EPOLLOUT (got %#x)", first);
    else if (quiet0 > 1) fail(N, "idle socket re-fired %d/20 times", quiet0);
    else if (!in1) fail(N, "no EPOLLIN edge for the first datagram");
    else if (!in2) fail(N, "no EPOLLIN edge for a second datagram while the first was unread");
    else if (quiet1 > 1) fail(N, "readable socket re-fired %d/20 times without new data", quiet1);
    else if (got != 2) fail(N, "drained %d datagrams, want 2", got);
    else if (quiet2 > 1) fail(N, "drained socket re-fired %d/10 times", quiet2);
    else if (!in3) fail(N, "no EPOLLIN edge after draining");
    else pass(N);
}

static uint16_t csum(const uint8_t *d, size_t n) {
    uint32_t s = 0;
    for (size_t i = 0; i + 1 < n; i += 2) s += (uint32_t)(d[i] << 8 | d[i + 1]);
    if (n & 1) s += (uint32_t)d[n - 1] << 8;
    while (s >> 16) s = (s & 0xffff) + (s >> 16);
    return (uint16_t)~s;
}

static void icmp_et_edges(void) {
    const char *N = "icmp_et_edges";
    if (skipped(N)) return;
    int fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    if (fd < 0) { skip(N, "(no ping socket: net.ipv4.ping_group_range)"); return; }
    int ep = epoll_create1(0);
    struct epoll_event ev = { EPOLLIN | EPOLLOUT | EPOLLET, { .fd = fd } };
    struct sockaddr_in to = sin4("127.0.0.1", 0);
    uint8_t p[16];
    memset(p, 0x5a, sizeof p);
    p[0] = 8; p[1] = 0; p[2] = p[3] = 0; p[4] = 0x12; p[5] = 0x34; p[6] = 0; p[7] = 1;
    uint16_t c = csum(p, sizeof p); p[2] = c >> 8; p[3] = c;
    /* Linux binds a ping socket at its first send; ours too. */
    if (sendto(fd, p, sizeof p, 0, (struct sockaddr *)&to, sizeof to) < 0) { fail(N, "sendto: %s", strerror(errno)); close(fd); close(ep); return; }
    msleep(200);
    drain(fd);
    epoll_ctl(ep, EPOLL_CTL_ADD, fd, &ev);
    unsigned first = ep_wait(ep, 200);
    int quiet0 = ep_refires(ep, 20);
    p[7] = 2; p[2] = p[3] = 0; c = csum(p, sizeof p); p[2] = c >> 8; p[3] = c;
    sendto(fd, p, sizeof p, 0, (struct sockaddr *)&to, sizeof to);
    int in1 = ep_until(ep, EPOLLIN, 1500);
    msleep(100);
    int quiet1 = ep_refires(ep, 20);
    drain(fd);
    int quiet2 = ep_refires(ep, 10);
    close(ep); close(fd);
    if (!(first & EPOLLOUT)) fail(N, "no initial EPOLLOUT (got %#x)", first);
    else if (quiet0 > 1) fail(N, "idle ping socket re-fired %d/20 times", quiet0);
    else if (!in1) fail(N, "no EPOLLIN edge for the echo reply");
    else if (quiet1 > 1) fail(N, "readable ping socket re-fired %d/20 times", quiet1);
    else if (quiet2 > 1) fail(N, "drained ping socket re-fired %d/10 times", quiet2);
    else pass(N);
}

static int tcp_connect_nb(const struct sockaddr_in *to) {
    int c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    if (c < 0) return -1;
    connect(c, (const struct sockaddr *)to, sizeof *to);
    return c;
}

static void listen_et_edges(void) {
    const char *N = "listen_et_edges";
    if (skipped(N)) return;
    int l = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    struct sockaddr_in a = sin4("127.0.0.1", 0);
    socklen_t al = sizeof a;
    if (l < 0 || bind(l, (struct sockaddr *)&a, sizeof a) < 0 || listen(l, 16) < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    getsockname(l, (struct sockaddr *)&a, &al);
    int ep = epoll_create1(0);
    struct epoll_event ev = { EPOLLIN | EPOLLET, { .fd = l } };
    epoll_ctl(ep, EPOLL_CTL_ADD, l, &ev);
    int idle = ep_refires(ep, 5);
    int c1 = tcp_connect_nb(&a);
    int in1 = ep_until(ep, EPOLLIN, 2000);
    msleep(50);
    int quiet1 = ep_refires(ep, 20);
    int c2 = tcp_connect_nb(&a);
    int in2 = ep_until(ep, EPOLLIN, 2000);
    msleep(50);
    int quiet2 = ep_refires(ep, 10);
    int acc = 0, x;
    while ((x = accept(l, NULL, NULL)) >= 0) { acc++; close(x); }
    int quiet3 = ep_refires(ep, 10);
    int c3 = tcp_connect_nb(&a);
    int in3 = ep_until(ep, EPOLLIN, 2000);
    while ((x = accept(l, NULL, NULL)) >= 0) close(x);
    close(c1); close(c2); close(c3); close(ep); close(l);
    if (idle) fail(N, "idle listener fired %d/5 times", idle);
    else if (!in1) fail(N, "no EPOLLIN edge for the first connection");
    else if (quiet1 > 1) fail(N, "listener with a pending connection re-fired %d/20 times", quiet1);
    else if (!in2) fail(N, "no EPOLLIN edge for a second connection while the first was not accepted");
    else if (quiet2 > 1) fail(N, "re-fired %d/10 times after the second edge", quiet2);
    else if (acc != 2) fail(N, "accepted %d connections, want 2", acc);
    else if (quiet3 > 1) fail(N, "drained listener re-fired %d/10 times", quiet3);
    else if (!in3) fail(N, "no EPOLLIN edge for a connection after draining");
    else pass(N);
}

static uint16_t free_port(int type);

/* ── connect / addressing ───────────────────────────────────────────────── */

static int recv_str(int fd, char *b, size_t n, int ms) {
    if (!(wait_ev(fd, POLLIN, ms) & POLLIN)) return -1;
    ssize_t k = recv(fd, b, n - 1, MSG_DONTWAIT);
    if (k < 0) return -1;
    b[k] = 0;
    return (int)k;
}

static void udp_connect_filter(void) {
    const char *N = "udp_connect_filter", *D = "udp_disconnect", *S = "udp_sendto_connected";
    struct sockaddr_in aa, ba, ca;
    /* An explicit port: Linux releases a port bind(0) picked when the
     * socket is disconnected (no SOCK_BINDPORT_LOCK), and udp_disconnect
     * below sends to it again. */
    int a = udp_bound("127.0.0.1", free_port(SOCK_DGRAM), &aa);
    int b = udp_bound("127.0.0.1", 0, &ba), c = udp_bound("127.0.0.1", 0, &ca);
    if (a < 0 || b < 0 || c < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    char buf[64];
    if (connect(a, (struct sockaddr *)&ba, sizeof ba) < 0) { fail(N, "connect: %s", strerror(errno)); goto out; }
    struct sockaddr_in pa; socklen_t pl = sizeof pa;
    int gp = getpeername(a, (struct sockaddr *)&pa, &pl);
    send_to(c, "from-c", 6, &aa);
    msleep(150);
    int early = wait_ev(a, POLLIN, 0) & POLLIN;
    send_to(b, "from-b", 6, &aa);
    int k = recv_str(a, buf, sizeof buf, 1500);
    int more = recv(a, buf + 32, 16, MSG_DONTWAIT);
    if (skipped(N)) {}
    else if (gp < 0 || pa.sin_port != ba.sin_port) fail(N, "getpeername: %d port %u want %u", gp, ntohs(pa.sin_port), ntohs(ba.sin_port));
    else if (early) fail(N, "a datagram from a third socket made the connected socket readable");
    else if (k != 6 || strcmp(buf, "from-b")) fail(N, "first datagram '%s' (%d), want 'from-b'", k > 0 ? buf : "", k);
    else if (more >= 0) fail(N, "a second datagram (%d bytes) was queued", more);
    else pass(N);

    /* sendto with an address on a connected socket: goes there. */
    if (!skipped(S)) {
        ssize_t r = send_to(a, "to-c", 4, &ca);
        int kc = recv_str(c, buf, sizeof buf, 1500);
        int kb = recv(b, buf + 32, 16, MSG_DONTWAIT);
        if (r != 4) fail(S, "sendto: %zd %s", r, strerror(errno));
        else if (kc != 4 || strcmp(buf, "to-c")) fail(S, "c got %d '%s'", kc, kc > 0 ? buf : "");
        else if (kb >= 0) fail(S, "the peer got it instead");
        else pass(S);
    }

    /* connect(AF_UNSPEC): back to unconnected. */
    if (!skipped(D)) {
        struct sockaddr un; memset(&un, 0, sizeof un); un.sa_family = AF_UNSPEC;
        int rc = connect(a, &un, sizeof un);
        pl = sizeof pa;
        int gp2 = getpeername(a, (struct sockaddr *)&pa, &pl);
        int gpe = errno;
        ssize_t sr = send(a, "x", 1, MSG_DONTWAIT);
        int se = errno;
        send_to(c, "again-c", 7, &aa);
        int kc = recv_str(a, buf, sizeof buf, 1500);
        if (rc < 0) fail(D, "connect(AF_UNSPEC): %s", strerror(errno));
        else if (gp2 == 0 || gpe != ENOTCONN) fail(D, "getpeername after disconnect: %d %s", gp2, strerror(gpe));
        else if (sr >= 0 || se != EDESTADDRREQ) fail(D, "send after disconnect: %zd %s (want EDESTADDRREQ)", sr, strerror(se));
        else if (kc != 7 || strcmp(buf, "again-c")) fail(D, "datagram from c after disconnect: %d '%s'", kc, kc > 0 ? buf : "");
        else pass(D);
    }
out:
    close(a); close(b); close(c);
}

static void udp_send_unconnected(void) {
    const char *N = "udp_send_unconnected";
    if (skipped(N)) return;
    int a = udp_bound("127.0.0.1", 0, NULL);
    ssize_t r = send(a, "x", 1, MSG_DONTWAIT);
    int e = errno;
    close(a);
    if (r < 0 && e == EDESTADDRREQ) pass(N);
    else fail(N, "send: %zd %s (want EDESTADDRREQ)", r, strerror(e));
}

static void udp_peek_trunc(void) {
    const char *N = "udp_peek_trunc";
    if (skipped(N)) return;
    struct sockaddr_in aa;
    int a = udp_bound("127.0.0.1", 0, &aa), b = udp_bound("127.0.0.1", 0, NULL);
    char d[100], r[128];
    for (int i = 0; i < 100; i++) d[i] = (char)i;
    send_to(b, d, 100, &aa);
    wait_ev(a, POLLIN, 1500);
    ssize_t p1 = recv(a, r, sizeof r, MSG_PEEK | MSG_DONTWAIT);
    ssize_t p2 = recv(a, r, 10, MSG_PEEK | MSG_DONTWAIT);
    ssize_t t = recv(a, r, 10, MSG_TRUNC | MSG_DONTWAIT);
    ssize_t e1 = recv(a, r, sizeof r, MSG_DONTWAIT);
    send_to(b, d, 100, &aa);
    wait_ev(a, POLLIN, 1500);
    ssize_t s1 = recv(a, r, 10, MSG_DONTWAIT);
    ssize_t e2 = recv(a, r, sizeof r, MSG_DONTWAIT);
    /* recvmsg: MSG_TRUNC in msg_flags, and with MSG_TRUNC the real length */
    send_to(b, d, 100, &aa);
    send_to(b, d, 100, &aa);
    wait_ev(a, POLLIN, 1500);
    struct iovec iov = { r, 10 };
    struct sockaddr_in from;
    struct msghdr m; memset(&m, 0, sizeof m);
    m.msg_iov = &iov; m.msg_iovlen = 1; m.msg_name = &from; m.msg_namelen = sizeof from;
    ssize_t m1 = recvmsg(a, &m, MSG_DONTWAIT);
    int f1 = m.msg_flags;
    msleep(50);
    m.msg_flags = 0; m.msg_namelen = sizeof from;
    ssize_t m2 = recvmsg(a, &m, MSG_DONTWAIT | MSG_TRUNC);
    int f2 = m.msg_flags;
    close(a); close(b);
    if (p1 != 100 || p2 != 10) fail(N, "peek: %zd, %zd (want 100, 10)", p1, p2);
    else if (t != 100) fail(N, "recv(10, MSG_TRUNC) = %zd, want 100 (the real length)", t);
    else if (e1 >= 0) fail(N, "datagram still queued after a truncating read (%zd)", e1);
    else if (s1 != 10 || e2 >= 0) fail(N, "short read %zd then %zd (want 10, then EAGAIN)", s1, e2);
    else if (m1 != 10 || !(f1 & MSG_TRUNC)) fail(N, "recvmsg %zd flags %#x (want 10 with MSG_TRUNC)", m1, f1);
    else if (m2 != 100 || !(f2 & MSG_TRUNC)) fail(N, "recvmsg(MSG_TRUNC) %zd flags %#x (want 100 with MSG_TRUNC)", m2, f2);
    else pass(N);
}

static void udp_zero_len(void) {
    const char *N = "udp_zero_len";
    if (skipped(N)) return;
    struct sockaddr_in aa;
    int a = udp_bound("127.0.0.1", 0, &aa), b = udp_bound("127.0.0.1", 0, NULL);
    ssize_t s = send_to(b, "", 0, &aa);
    int ev = wait_ev(a, POLLIN, 1500);
    int q = -1;
    ioctl(a, FIONREAD, &q);
    char r[16];
    ssize_t n = recv(a, r, sizeof r, MSG_DONTWAIT);
    ssize_t n2 = recv(a, r, sizeof r, MSG_DONTWAIT);
    int e2 = errno;
    close(a); close(b);
    if (s != 0) fail(N, "send of 0 bytes: %zd %s", s, strerror(errno));
    else if (!(ev & POLLIN)) fail(N, "not readable after a 0-byte datagram");
    else if (q != 0) fail(N, "FIONREAD %d, want 0", q);
    else if (n != 0) fail(N, "recv %zd, want 0", n);
    else if (n2 >= 0 || e2 != EAGAIN) fail(N, "second recv %zd %s, want EAGAIN", n2, strerror(e2));
    else pass(N);
}

static void udp_fionread(void) {
    const char *N = "udp_fionread";
    if (skipped(N)) return;
    struct sockaddr_in aa;
    int a = udp_bound("127.0.0.1", 0, &aa), b = udp_bound("127.0.0.1", 0, NULL);
    char d[64] = {0};
    send_to(b, d, 10, &aa); send_to(b, d, 20, &aa); send_to(b, d, 30, &aa);
    msleep(100);
    wait_ev(a, POLLIN, 1500);
    int q1 = -1, q2 = -1, q3 = -1;
    ioctl(a, FIONREAD, &q1);
    recv(a, d, sizeof d, MSG_DONTWAIT);
    ioctl(a, FIONREAD, &q2);
    drain(a);
    ioctl(a, FIONREAD, &q3);
    close(a); close(b);
    if (q1 == 10 && q2 == 20 && q3 == 0) pass(N);
    else fail(N, "FIONREAD %d, %d, %d (want 10, 20, 0)", q1, q2, q3);
}

static void udp_nonblock(void) {
    const char *N = "udp_nonblock";
    if (skipped(N)) return;
    int a = udp_bound("127.0.0.1", 0, NULL);
    char r[8];
    long long t0 = now_ms();
    ssize_t n1 = recv(a, r, sizeof r, MSG_DONTWAIT);
    int e1 = errno;
    fcntl(a, F_SETFL, fcntl(a, F_GETFL) | O_NONBLOCK);
    ssize_t n2 = recvfrom(a, r, sizeof r, 0, NULL, NULL);
    int e2 = errno;
    long long dt = now_ms() - t0;
    close(a);
    if (n1 < 0 && e1 == EAGAIN && n2 < 0 && e2 == EAGAIN && dt < 200) pass(N);
    else fail(N, "%zd %s / %zd %s after %lld ms", n1, strerror(e1), n2, strerror(e2), dt);
}

/* Default route's gateway from /proc/net/route (0 if none). */
static int default_gw(char *gw, size_t n) {
    FILE *f = fopen("/proc/net/route", "r");
    if (!f) return 0;
    char line[256], ifn[32];
    unsigned dst, g, flags;
    int ok = 0;
    while (fgets(line, sizeof line, f)) {
        if (sscanf(line, "%31s %x %x %x", ifn, &dst, &g, &flags) == 4 && dst == 0 && (flags & 2)) {
            struct in_addr ia = { g };
            snprintf(gw, n, "%s", inet_ntoa(ia));
            ok = 1; break;
        }
    }
    fclose(f);
    return ok;
}

static void udp_getsockname(void) {
    const char *N = "udp_getsockname";
    if (skipped(N)) return;
    struct sockaddr_in x, peer;
    socklen_t l;
    int b = udp_bound("127.0.0.1", 0, &peer);
    int a = socket(AF_INET, SOCK_DGRAM, 0);
    l = sizeof x; memset(&x, 0xff, sizeof x);
    getsockname(a, (struct sockaddr *)&x, &l);
    int unb_ok = x.sin_port == 0 && x.sin_addr.s_addr == 0;
    send_to(a, "x", 1, &peer);
    l = sizeof x;
    getsockname(a, (struct sockaddr *)&x, &l);
    int after_send_ok = x.sin_port != 0 && x.sin_addr.s_addr == 0;
    unsigned sp = ntohs(x.sin_port);
    close(a);
    a = socket(AF_INET, SOCK_DGRAM, 0);
    connect(a, (struct sockaddr *)&peer, sizeof peer);
    l = sizeof x;
    getsockname(a, (struct sockaddr *)&x, &l);
    char lo[32]; snprintf(lo, sizeof lo, "%s", inet_ntoa(x.sin_addr));
    int conn_ok = x.sin_port != 0 && !strcmp(lo, "127.0.0.1");
    close(a); close(b);
    /* The "which address would I use" trick (WebRTC, musl's address sort). */
    char gw[64], nic[32] = "";
    int nic_ok = 1, have = default_gw(gw, sizeof gw);
    if (have) {
        a = socket(AF_INET, SOCK_DGRAM, 0);
        struct sockaddr_in g = sin4(gw, 53);
        connect(a, (struct sockaddr *)&g, sizeof g);
        l = sizeof x;
        getsockname(a, (struct sockaddr *)&x, &l);
        snprintf(nic, sizeof nic, "%s", inet_ntoa(x.sin_addr));
        nic_ok = x.sin_addr.s_addr != 0 && (ntohl(x.sin_addr.s_addr) >> 24) != 127 && x.sin_port != 0;
        close(a);
    }
    if (!unb_ok) fail(N, "unbound socket: %s:%u, want 0.0.0.0:0", inet_ntoa(x.sin_addr), ntohs(x.sin_port));
    else if (!after_send_ok) fail(N, "after sendto: port %u (want a port, address 0.0.0.0)", sp);
    else if (!conn_ok) fail(N, "after connect to 127.0.0.1: %s, want 127.0.0.1", lo);
    else if (!nic_ok) fail(N, "after connect to the gateway %s: %s, want the interface address", gw, nic);
    else { if (have) printf("udp_getsockname: via gateway %s the source is %s\n", gw, nic); pass(N); }
}

/* ── bind conflicts ─────────────────────────────────────────────────────── */

static int bind_with(int type, const char *ip, uint16_t port, int reuseaddr, int reuseport, int *err) {
    int fd = socket(AF_INET, type, 0);
    int one = 1;
    if (reuseaddr) setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    if (reuseport) setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &one, sizeof one);
    struct sockaddr_in a = sin4(ip, port);
    if (bind(fd, (struct sockaddr *)&a, sizeof a) < 0) { *err = errno; close(fd); return -1; }
    *err = 0;
    return fd;
}

static uint16_t free_port(int type) {
    struct sockaddr_in a;
    int fd = socket(AF_INET, type, 0);
    a = sin4("127.0.0.1", 0);
    bind(fd, (struct sockaddr *)&a, sizeof a);
    socklen_t l = sizeof a;
    getsockname(fd, (struct sockaddr *)&a, &l);
    close(fd);
    return ntohs(a.sin_port);
}

static void udp_bind_conflict(void) {
    const char *N = "udp_bind_conflict";
    if (skipped(N)) return;
    uint16_t p = free_port(SOCK_DGRAM);
    int e1, e2, e3, e4, e5, e6, e7, e8, e9;
    int a = bind_with(SOCK_DGRAM, "127.0.0.1", p, 0, 0, &e1);
    int b = bind_with(SOCK_DGRAM, "127.0.0.1", p, 0, 0, &e2);
    int c = bind_with(SOCK_DGRAM, "0.0.0.0", p, 0, 0, &e3);
    /* SO_REUSEADDR on only the second one is not enough. */
    int d = bind_with(SOCK_DGRAM, "127.0.0.1", p, 1, 0, &e4);
    /* Rebinding a bound socket is EINVAL. */
    struct sockaddr_in o = sin4("127.0.0.1", 0);
    int rb = bind(a, (struct sockaddr *)&o, sizeof o);
    int erb = errno;
    close(a);
    int f = bind_with(SOCK_DGRAM, "127.0.0.1", p, 0, 0, &e5);
    if (f >= 0) close(f);
    uint16_t q = free_port(SOCK_DGRAM);
    int g = bind_with(SOCK_DGRAM, "0.0.0.0", q, 1, 0, &e6);
    int h = bind_with(SOCK_DGRAM, "0.0.0.0", q, 1, 0, &e7);
    uint16_t r = free_port(SOCK_DGRAM);
    int i = bind_with(SOCK_DGRAM, "127.0.0.1", r, 0, 1, &e8);
    int j = bind_with(SOCK_DGRAM, "127.0.0.1", r, 0, 1, &e9);
    if (b >= 0) close(b);
    if (c >= 0) close(c);
    if (d >= 0) close(d);
    if (g >= 0) close(g);
    if (h >= 0) close(h);
    if (i >= 0) close(i);
    if (j >= 0) close(j);
    if (a < 0) fail(N, "first bind of %u: %s", p, strerror(e1));
    else if (b >= 0 || e2 != EADDRINUSE) fail(N, "second bind 127.0.0.1:%u: %s (want EADDRINUSE)", p, b >= 0 ? "ok" : strerror(e2));
    else if (c >= 0 || e3 != EADDRINUSE) fail(N, "bind 0.0.0.0:%u over 127.0.0.1: %s (want EADDRINUSE)", p, c >= 0 ? "ok" : strerror(e3));
    else if (d >= 0 || e4 != EADDRINUSE) fail(N, "SO_REUSEADDR on one side only: %s (want EADDRINUSE)", d >= 0 ? "ok" : strerror(e4));
    else if (rb == 0 || erb != EINVAL) fail(N, "second bind() of a bound socket: %s (want EINVAL)", rb == 0 ? "ok" : strerror(erb));
    else if (f < 0) fail(N, "bind after the holder closed: %s", strerror(e5));
    else if (g < 0 || h < 0) fail(N, "SO_REUSEADDR on both: %s / %s", g < 0 ? strerror(e6) : "ok", h < 0 ? strerror(e7) : "ok");
    else if (i < 0 || j < 0) fail(N, "SO_REUSEPORT on both: %s / %s", i < 0 ? strerror(e8) : "ok", j < 0 ? strerror(e9) : "ok");
    else pass(N);
}

static void tcp_bind_conflict(void) {
    const char *N = "tcp_bind_conflict";
    if (skipped(N)) return;
    uint16_t p = free_port(SOCK_STREAM);
    int e1, e2, e3, e4, e5, e6, e7;
    int l = bind_with(SOCK_STREAM, "127.0.0.1", p, 1, 0, &e1);
    if (l >= 0) listen(l, 4);
    int b = bind_with(SOCK_STREAM, "127.0.0.1", p, 1, 0, &e2);
    int c = bind_with(SOCK_STREAM, "0.0.0.0", p, 1, 0, &e3);
    if (b >= 0) close(b);
    if (c >= 0) close(c);
    if (l >= 0) close(l);
    int d = bind_with(SOCK_STREAM, "127.0.0.1", p, 1, 0, &e4);  /* listener gone, no TIME_WAIT */
    if (d >= 0) close(d);
    uint16_t q = free_port(SOCK_STREAM);
    int x = bind_with(SOCK_STREAM, "127.0.0.1", q, 1, 0, &e5);
    int y = bind_with(SOCK_STREAM, "127.0.0.1", q, 1, 0, &e6);   /* both REUSEADDR, neither listening */
    int z = bind_with(SOCK_STREAM, "127.0.0.1", q, 0, 0, &e7);   /* no REUSEADDR */
    if (x >= 0) close(x);
    if (y >= 0) close(y);
    if (z >= 0) close(z);
    if (l < 0) fail(N, "listener bind: %s", strerror(e1));
    else if (b >= 0 || e2 != EADDRINUSE) fail(N, "bind over a listener (SO_REUSEADDR both): %s (want EADDRINUSE)", b >= 0 ? "ok" : strerror(e2));
    else if (c >= 0 || e3 != EADDRINUSE) fail(N, "bind 0.0.0.0 over a 127.0.0.1 listener: %s (want EADDRINUSE)", c >= 0 ? "ok" : strerror(e3));
    else if (d < 0) fail(N, "bind after the listener closed: %s", strerror(e4));
    else if (x < 0 || y < 0) fail(N, "two bound sockets with SO_REUSEADDR: %s / %s", x < 0 ? strerror(e5) : "ok", y < 0 ? strerror(e6) : "ok");
    else if (z >= 0 || e7 != EADDRINUSE) fail(N, "third bind without SO_REUSEADDR: %s (want EADDRINUSE)", z >= 0 ? "ok" : strerror(e7));
    else pass(N);
}

/* ── shutdown ───────────────────────────────────────────────────────────── */

struct blk { int fd; ssize_t n; int err; long long dt; };
static void *blocked_recv(void *p) {
    struct blk *b = p;
    char r[16];
    long long t0 = now_ms();
    b->n = recv(b->fd, r, sizeof r, 0);
    b->err = errno;
    b->dt = now_ms() - t0;
    return NULL;
}

static void udp_shutdown(void) {
    const char *N = "udp_shutdown";
    if (skipped(N)) return;
    struct sockaddr_in ba;
    int a = udp_bound("127.0.0.1", 0, NULL), b = udp_bound("127.0.0.1", 0, &ba);
    int r0 = shutdown(a, SHUT_RD);
    int e0 = errno;
    connect(a, (struct sockaddr *)&ba, sizeof ba);
    int r1 = shutdown(a, SHUT_RD);
    int ev = wait_ev(a, POLLIN, 500);
    char r[16];
    long long t0 = now_ms();
    ssize_t n = recv(a, r, sizeof r, 0);   /* must not block */
    long long dt = now_ms() - t0;
    int r2 = shutdown(a, SHUT_WR);
    ssize_t s = send(a, "x", 1, MSG_NOSIGNAL);
    int es = errno;
    close(a);
    /* A thread blocked in recv is woken by shutdown. */
    int c = udp_bound("127.0.0.1", 0, NULL);
    connect(c, (struct sockaddr *)&ba, sizeof ba);
    struct blk bk = { c, -2, 0, 0 };
    pthread_t th;
    pthread_create(&th, NULL, blocked_recv, &bk);
    msleep(300);
    shutdown(c, SHUT_RDWR);
    long long end = now_ms() + 3000;
    while (bk.n == -2 && now_ms() < end) msleep(20);
    int woke = bk.n != -2;
    if (!woke) { /* unstick it so the process can exit */ send_to(b, "z", 1, &ba); close(c); msleep(100); }
    else pthread_join(th, NULL);
    if (woke) close(c);
    close(b);
    if (r0 == 0 || e0 != ENOTCONN) fail(N, "shutdown of an unconnected socket: %s (want ENOTCONN)", r0 == 0 ? "ok" : strerror(e0));
    else if (r1 != 0) fail(N, "shutdown(SHUT_RD) of a connected socket: %s", strerror(errno));
    else if (!(ev & POLLIN)) fail(N, "not readable after SHUT_RD (revents %#x)", ev);
    else if (n != 0 || dt > 300) fail(N, "recv after SHUT_RD: %zd after %lld ms (want 0 at once)", n, dt);
    else if (r2 != 0 || s >= 0 || es != EPIPE) fail(N, "send after SHUT_WR: %zd %s (want EPIPE)", s, strerror(es));
    else if (!woke) fail(N, "a thread blocked in recv was not woken by shutdown(SHUT_RDWR)");
    else if (bk.n != 0) fail(N, "the woken recv returned %zd %s (want 0)", bk.n, strerror(bk.err));
    else pass(N);
}

/* ── options ────────────────────────────────────────────────────────────── */

static void udp_sockopts(void) {
    const char *N = "udp_sockopts";
    if (skipped(N)) return;
    int a = socket(AF_INET, SOCK_DGRAM, 0);
    int v = -1, one = 1; socklen_t l = sizeof v;
    int rb = getsockopt(a, SOL_SOCKET, SO_RCVBUF, &v, &l); int rcv = v;
    l = sizeof v; v = -1;
    int sb = getsockopt(a, SOL_SOCKET, SO_SNDBUF, &v, &l); int snd = v;
    int bs = setsockopt(a, SOL_SOCKET, SO_BROADCAST, &one, sizeof one);
    l = sizeof v; v = -1;
    int bg = getsockopt(a, SOL_SOCKET, SO_BROADCAST, &v, &l); int bv = v;
    setsockopt(a, SOL_SOCKET, SO_REUSEPORT, &one, sizeof one);
    l = sizeof v; v = -1;
    int pg = getsockopt(a, SOL_SOCKET, SO_REUSEPORT, &v, &l); int pv = v;
    struct sockaddr_in any = sin4("0.0.0.0", 0);
    bind(a, (struct sockaddr *)&any, sizeof any);
    struct ip_mreq mr;
    inet_pton(AF_INET, "224.0.0.251", &mr.imr_multiaddr);
    mr.imr_interface.s_addr = htonl(INADDR_ANY);
    int mj = setsockopt(a, IPPROTO_IP, IP_ADD_MEMBERSHIP, &mr, sizeof mr);
    int me = errno;
    int ml = setsockopt(a, IPPROTO_IP, IP_MULTICAST_LOOP, &one, sizeof one);
    int pk = setsockopt(a, IPPROTO_IP, IP_PKTINFO, &one, sizeof one);
    close(a);
    if (rb || sb || rcv <= 0 || snd <= 0) fail(N, "SO_RCVBUF %d (%d) SO_SNDBUF %d (%d)", rcv, rb, snd, sb);
    else if (bs || bg || bv != 1) fail(N, "SO_BROADCAST set %d get %d value %d", bs, bg, bv);
    else if (pg || pv != 1) fail(N, "SO_REUSEPORT get %d value %d", pg, pv);
    /* Linux needs a route for the group; ENODEV is what a box without one says. */
    else if (mj && me != ENODEV) fail(N, "IP_ADD_MEMBERSHIP: %s", strerror(me));
    else if (ml || pk) fail(N, "IP_MULTICAST_LOOP %d IP_PKTINFO %d", ml, pk);
    else { printf("udp_sockopts: SO_RCVBUF %d SO_SNDBUF %d\n", rcv, snd); pass(N); }
}

/* ── volume ─────────────────────────────────────────────────────────────── */

static void udp_burst(void) {
    const char *N = "udp_burst";
    if (skipped(N)) return;
    struct sockaddr_in ra;
    int r = udp_bound("127.0.0.1", 0, &ra), s = udp_bound("127.0.0.1", 0, NULL);
    fcntl(s, F_SETFL, fcntl(s, F_GETFL) | O_NONBLOCK);
    static char d[1200];
    int ok = 0, again = 0, other = 0, oe = 0;
    for (int i = 0; i < 3000; i++) {
        ssize_t n = send_to(s, d, sizeof d, &ra);
        if (n == (ssize_t)sizeof d) ok++;
        else if (n < 0 && (errno == EAGAIN || errno == ENOBUFS)) again++;
        else { other++; oe = errno; }
    }
    msleep(200);
    int got = drain(r);
    fcntl(s, F_SETFL, fcntl(s, F_GETFL) & ~O_NONBLOCK);
    int bok = 0, got2 = 0;
    for (int i = 0; i < 500; i++) {
        if (send_to(s, d, sizeof d, &ra) == (ssize_t)sizeof d) bok++;
        if (i % 50 == 49) got2 += drain(r);
    }
    msleep(200);
    got2 += drain(r);
    close(r); close(s);
    printf("udp_burst: nonblocking %d sent, %d EAGAIN, %d received; blocking %d/500 sent, %d received\n", ok, again, got, bok, got2);
    if (other) fail(N, "%d sends failed with %s (want only EAGAIN)", other, strerror(oe));
    else if (got == 0) fail(N, "nothing received");
    else if (bok != 500) fail(N, "%d/500 blocking sends succeeded", bok);
    else pass(N);
}

static void udp_large(void) {
    const char *N = "udp_large";
    if (skipped(N)) return;
    struct sockaddr_in ra;
    int r = udp_bound("127.0.0.1", 0, &ra), s = udp_bound("127.0.0.1", 0, NULL);
    static char d[8000], g[9000];
    for (int i = 0; i < 8000; i++) d[i] = (char)(i * 7);
    ssize_t n = send_to(s, d, sizeof d, &ra);
    wait_ev(r, POLLIN, 1500);
    ssize_t k = recv(r, g, sizeof g, MSG_DONTWAIT);
    close(r); close(s);
    if (n != 8000) fail(N, "send: %zd %s", n, strerror(errno));
    else if (k != 8000 || memcmp(d, g, 8000)) fail(N, "received %zd bytes%s", k, k == 8000 ? " (corrupt)" : "");
    else pass(N);
}

static void udp_huge(void) {
    const char *N = "udp_huge";
    if (skipped(N)) return;
    static const size_t sizes[3] = { 4000, 60000, 65507 };
    static char d[65508], g[70000];
    for (size_t i = 0; i < sizeof d; i++) d[i] = (char)(i * 13 + (i >> 8));
    char why[160] = "";
    struct sockaddr_in ra;
    int r = udp_bound("127.0.0.1", 0, &ra), s = udp_bound("127.0.0.1", 0, NULL);
    int rcv = 1 << 20; setsockopt(r, SOL_SOCKET, SO_RCVBUF, &rcv, sizeof rcv);
    for (int i = 0; i < 3 && !why[0]; i++) {
        ssize_t n = send_to(s, d, sizes[i], &ra);
        if (n != (ssize_t)sizes[i]) { snprintf(why, sizeof why, "send %zu: %zd %s", sizes[i], n, strerror(errno)); break; }
        wait_ev(r, POLLIN, 2000);
        ssize_t k = recv(r, g, sizeof g, MSG_DONTWAIT);
        if (k != (ssize_t)sizes[i] || memcmp(d, g, sizes[i])) snprintf(why, sizeof why, "%zu bytes came back as %zd", sizes[i], k);
    }
    errno = 0;
    ssize_t e = send_to(s, d, 65508, &ra); int ee = errno;
    close(r); close(s);
    if (why[0]) fail(N, "%s", why);
    else if (e != -1 || ee != EMSGSIZE) fail(N, "65508-byte send: %zd %s, want EMSGSIZE", e, strerror(ee));
    else pass(N);
}

/* Path-MTU discovery (IP_MTU_DISCOVER, IP_MTU, ICMP "fragmentation needed").
 * The frag-needed message is injected through a raw ICMP socket aimed at
 * loopback, quoting a datagram this socket "sent" to 127.0.0.2. */
#ifndef IP_MTU_DISCOVER
#define IP_MTU_DISCOVER 10
#endif
#ifndef IP_MTU
#define IP_MTU 14
#endif
static uint16_t pmtu_csum(const void *p, size_t n) {
    const uint8_t *b = p; uint32_t sum = 0;
    for (size_t i = 0; i + 1 < n; i += 2) sum += (b[i] << 8) | b[i + 1];
    if (n & 1) sum += b[n - 1] << 8;
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return (uint16_t)~sum;
}

static void udp_pmtud(void) {
    const char *N = "udp_pmtud";
    if (skipped(N)) return;
    struct sockaddr_in ra;
    int r = udp_bound("127.0.0.2", 0, &ra), s = socket(AF_INET, SOCK_DGRAM, 0);
    static char d[4000];
    memset(d, 7, sizeof d);
    int v = -1; socklen_t vl = sizeof v;
    char why[200] = "";
    if (getsockopt(s, IPPROTO_IP, IP_MTU_DISCOVER, &v, &vl) || v != 1)
        snprintf(why, sizeof why, "default IP_MTU_DISCOVER %d (%s), want 1 (WANT)", v, strerror(errno));
    int on = 2;
    if (!why[0] && setsockopt(s, IPPROTO_IP, IP_MTU_DISCOVER, &on, sizeof on))
        snprintf(why, sizeof why, "set IP_MTU_DISCOVER=DO: %s", strerror(errno));
    on = 9; errno = 0;
    if (!why[0] && (setsockopt(s, IPPROTO_IP, IP_MTU_DISCOVER, &on, sizeof on) == 0 || errno != EINVAL))
        snprintf(why, sizeof why, "IP_MTU_DISCOVER=9 not EINVAL (%s)", strerror(errno));
    v = -1;
    if (!why[0] && (getsockopt(s, IPPROTO_IP, IP_MTU_DISCOVER, &v, &vl) || v != 2))
        snprintf(why, sizeof why, "IP_MTU_DISCOVER reads %d, want 2", v);
    if (!why[0] && connect(s, (struct sockaddr *)&ra, sizeof ra)) snprintf(why, sizeof why, "connect: %s", strerror(errno));
    v = 0;
    if (!why[0] && (getsockopt(s, IPPROTO_IP, IP_MTU, &v, &vl) || v != 65536))
        snprintf(why, sizeof why, "IP_MTU before ICMP: %d (%s), want 65536", v, strerror(errno));
    if (!why[0] && send(s, d, 3000, 0) != 3000) snprintf(why, sizeof why, "3000-byte DO send: %s", strerror(errno));
    struct sockaddr_in la; socklen_t ll = sizeof la;
    getsockname(s, (struct sockaddr *)&la, &ll);
    int raw = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    if (!why[0] && raw < 0) snprintf(why, sizeof why, "raw icmp socket: %s", strerror(errno));
    if (!why[0]) {
        uint8_t m[8 + 20 + 8];
        memset(m, 0, sizeof m);
        m[0] = 3; m[1] = 4; m[6] = 1000 >> 8; m[7] = 1000 & 255;
        uint8_t *ip = m + 8;
        ip[0] = 0x45; ip[3] = 28 + 1500; ip[2] = (28 + 1500) >> 8; ip[6] = 0x40; ip[8] = 64; ip[9] = 17;
        uint32_t src = htonl(INADDR_LOOPBACK);
        memcpy(ip + 12, &src, 4); memcpy(ip + 16, &ra.sin_addr, 4);
        uint16_t c = pmtu_csum(ip, 20); ip[10] = c >> 8; ip[11] = c & 255;
        memcpy(ip + 20, &la.sin_port, 2); memcpy(ip + 22, &ra.sin_port, 2);
        c = pmtu_csum(m, sizeof m); m[2] = c >> 8; m[3] = c & 255;
        struct sockaddr_in to = { .sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
        if (sendto(raw, m, sizeof m, 0, (struct sockaddr *)&to, sizeof to) != (ssize_t)sizeof m)
            snprintf(why, sizeof why, "inject frag-needed: %s", strerror(errno));
        msleep(200);
    }
    if (raw >= 0) close(raw);
    ssize_t big = -2, small = -2; int be = 0;
    if (!why[0]) {
        errno = 0; big = send(s, d, 1200, 0); be = errno;
        small = send(s, d, 900, 0);
        v = 0;
        if (big != -1 || be != EMSGSIZE) snprintf(why, sizeof why, "1200-byte send over a 1000 path: %zd %s, want EMSGSIZE", big, strerror(be));
        else if (small != 900) snprintf(why, sizeof why, "900-byte send over a 1000 path: %zd %s", small, strerror(errno));
        else if (getsockopt(s, IPPROTO_IP, IP_MTU, &v, &vl) || v != 1000) snprintf(why, sizeof why, "IP_MTU after ICMP %d, want 1000", v);
    }
    /* IP_PMTUDISC_DONT ignores the path: the same 1200 bytes go out. */
    if (!why[0]) {
        on = 0; setsockopt(s, IPPROTO_IP, IP_MTU_DISCOVER, &on, sizeof on);
        if (send(s, d, 1200, 0) != 1200) snprintf(why, sizeof why, "DONT send of 1200: %s", strerror(errno));
    }
    close(r); close(s);
    if (why[0]) fail(N, "%s", why); else pass(N);
}

static void udp_gso(void) {
    const char *N = "udp_gso";
    if (skipped(N)) return;
    struct sockaddr_in ra;
    int r = udp_bound("127.0.0.1", 0, &ra), s = udp_bound("127.0.0.1", 0, NULL);
    static char d[1300];
    for (int i = 0; i < 1300; i++) d[i] = (char)i;
    /* cmsg: 1300 bytes in segments of 500 -> 500, 500, 300 */
    struct iovec iov = { d, sizeof d };
    char cbuf[CMSG_SPACE(sizeof(uint16_t))];
    memset(cbuf, 0, sizeof cbuf);
    struct msghdr m; memset(&m, 0, sizeof m);
    m.msg_name = &ra; m.msg_namelen = sizeof ra;
    m.msg_iov = &iov; m.msg_iovlen = 1;
    m.msg_control = cbuf; m.msg_controllen = sizeof cbuf;
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    c->cmsg_level = SOL_UDP; c->cmsg_type = UDP_SEGMENT; c->cmsg_len = CMSG_LEN(sizeof(uint16_t));
    uint16_t seg = 500; memcpy(CMSG_DATA(c), &seg, sizeof seg);
    ssize_t n = sendmsg(s, &m, 0);
    int ne = errno;
    msleep(150);
    int sizes[8], k = 0; char g[2000];
    ssize_t x;
    while (k < 8 && (wait_ev(r, POLLIN, 300) & POLLIN) && (x = recv(r, g, sizeof g, MSG_DONTWAIT)) >= 0) sizes[k++] = (int)x;
    /* socket option: 1000 bytes in segments of 400 -> 400, 400, 200 */
    int opt = 400;
    int so = setsockopt(s, SOL_UDP, UDP_SEGMENT, &opt, sizeof opt);
    ssize_t n2 = send_to(s, d, 1000, &ra);
    msleep(150);
    int sizes2[8], k2 = 0;
    while (k2 < 8 && (wait_ev(r, POLLIN, 300) & POLLIN) && (x = recv(r, g, sizeof g, MSG_DONTWAIT)) >= 0) sizes2[k2++] = (int)x;
    close(r); close(s);
    if (n != 1300) fail(N, "sendmsg with UDP_SEGMENT: %zd %s", n, strerror(ne));
    else if (k != 3 || sizes[0] != 500 || sizes[1] != 500 || sizes[2] != 300) fail(N, "cmsg: %d datagrams (%d %d %d), want 500 500 300", k, k > 0 ? sizes[0] : -1, k > 1 ? sizes[1] : -1, k > 2 ? sizes[2] : -1);
    else if (so) fail(N, "setsockopt(UDP_SEGMENT): %s", strerror(errno));
    else if (n2 != 1000 || k2 != 3 || sizes2[0] != 400 || sizes2[2] != 200) fail(N, "socket option: sent %zd, %d datagrams (%d .. %d), want 400 400 200", n2, k2, k2 > 0 ? sizes2[0] : -1, k2 > 2 ? sizes2[2] : -1);
    else pass(N);
}

static void udp_mmsg(void) {
    const char *N = "udp_mmsg";
    if (skipped(N)) return;
    struct sockaddr_in ra;
    int r = udp_bound("127.0.0.1", 0, &ra), s = udp_bound("127.0.0.1", 0, NULL);
    struct mmsghdr out[4], in[8];
    struct iovec oi[4], ii[8];
    char ob[4][32], ib[8][64];
    memset(out, 0, sizeof out); memset(in, 0, sizeof in);
    for (int i = 0; i < 4; i++) {
        int len = snprintf(ob[i], sizeof ob[i], "dgram-%d%.*s", i, i * 3, "xxxxxxxxxxxx");
        oi[i].iov_base = ob[i]; oi[i].iov_len = len;
        out[i].msg_hdr.msg_iov = &oi[i]; out[i].msg_hdr.msg_iovlen = 1;
        out[i].msg_hdr.msg_name = &ra; out[i].msg_hdr.msg_namelen = sizeof ra;
    }
    int ns = sendmmsg(s, out, 4, 0);
    int se = errno;
    for (int i = 0; i < 8; i++) {
        ii[i].iov_base = ib[i]; ii[i].iov_len = sizeof ib[i];
        in[i].msg_hdr.msg_iov = &ii[i]; in[i].msg_hdr.msg_iovlen = 1;
    }
    msleep(150);
    wait_ev(r, POLLIN, 1000);
    int nr = recvmmsg(r, in, 8, MSG_WAITFORONE, NULL);
    int re = errno;
    int lens_ok = nr == 4;
    for (int i = 0; lens_ok && i < 4; i++)
        lens_ok = in[i].msg_len == oi[i].iov_len && out[i].msg_len == oi[i].iov_len
                  && !memcmp(ib[i], ob[i], oi[i].iov_len);
    close(r); close(s);
    if (ns != 4) fail(N, "sendmmsg: %d %s", ns, strerror(se));
    else if (nr != 4) fail(N, "recvmmsg: %d %s (want 4)", nr, strerror(re));
    else if (!lens_ok) fail(N, "msg_len or payload mismatch");
    else pass(N);
}

static void udp_dup(void) {
    const char *N = "udp_dup";
    if (skipped(N)) return;
    struct sockaddr_in aa, ba;
    int a = udp_bound("127.0.0.1", 0, &aa), b = udp_bound("127.0.0.1", 0, &ba);
    int d = dup(a);
    char buf[16];
    ssize_t s = send_to(d, "via-dup", 7, &ba);
    int k = recv_str(b, buf, sizeof buf, 1500);
    send_to(b, "back", 4, &aa);
    char buf2[16];
    int k2 = recv_str(d, buf2, sizeof buf2, 1500);
    close(a);
    send_to(b, "after", 5, &aa);
    char buf3[16];
    int k3 = recv_str(d, buf3, sizeof buf3, 1500);
    close(d); close(b);
    if (d < 0) fail(N, "dup: %s", strerror(errno));
    else if (s != 7 || k != 7) fail(N, "send via the dup: %zd, received %d", s, k);
    else if (k2 != 4) fail(N, "receive via the dup: %d", k2);
    else if (k3 != 5) fail(N, "the dup stopped working when the original closed: %d", k3);
    else pass(N);
}

static void udp_wildcard_connect(void) {
    const char *N = "udp_wildcard_connect";
    if (skipped(N)) return;
    uint16_t p = free_port(SOCK_DGRAM);
    int e;
    int a = bind_with(SOCK_DGRAM, "0.0.0.0", p, 0, 0, &e);
    struct sockaddr_in ba, aa = sin4("127.0.0.1", p);
    int b = udp_bound("127.0.0.1", 0, &ba);
    if (a < 0 || b < 0) { fail(N, "setup: %s", strerror(e)); return; }
    char buf[16];
    send_to(b, "pre", 3, &aa);
    int k0 = recv_str(a, buf, sizeof buf, 1500);
    int rc = connect(a, (struct sockaddr *)&ba, sizeof ba);
    ssize_t s = send(a, "ping", 4, 0);
    int k1 = recv_str(b, buf, sizeof buf, 1500);
    send_to(b, "pong", 4, &aa);
    int k2 = recv_str(a, buf, sizeof buf, 1500);
    int q = -1; ioctl(a, FIONREAD, &q);
    close(a); close(b);
    if (k0 != 3) fail(N, "wildcard server did not receive over loopback (%d)", k0);
    else if (rc) fail(N, "connect: %s", strerror(errno));
    else if (s != 4 || k1 != 4) fail(N, "send after connect: %zd, peer got %d", s, k1);
    else if (k2 != 4 || strcmp(buf, "pong")) fail(N, "receive after connect: %d", k2);
    else pass(N);
}

/* Lane udp2: ICMP errors, multicast, broadcast, cmsgs, disconnect. */
#include "udp2cases.c"
#include "pmtud2.c"
#include "recverr.c"

/* Cases against the frame-level peer of udppeertest.py (second NIC). */
#include "peer.c"

/* The wildcard server's poll tag (lane udpanytag). */
#include "anytag.c"

/* SO_RCVTIMEO/SO_SNDTIMEO, blocking accept, two SYNs in one poll (lane sotimeo). */
#include "sotimeo.c"

/* Ephemeral ports, AF_UNIX backlog and datagrams, TCP_NODELAY (lane netabi). */
#include "netabi.c"

int main(int argc, char **argv) {
    int o;
    if (argc >= 2 && !strcmp(argv[1], "peer")) {
        setvbuf(stdout, NULL, _IOLBF, 0);
        signal(SIGPIPE, SIG_IGN);
        return peer_main(argc - 1, argv + 1);
    }
    while ((o = getopt(argc, argv, "x:o:")) != -1) {
        if (o == 'x' && nskips < 32) skips[nskips++] = optarg;
        else if (o == 'o') only = optarg;
        else { fprintf(stderr, "usage: udptest [-x case]... [-o prefix]\n"); return 2; }
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    signal(SIGPIPE, SIG_IGN);
    printf("udptest: uid %d\n", (int)geteuid());
    udp_et_edges();
    icmp_et_edges();
    listen_et_edges();
    udp_connect_filter();
    udp_send_unconnected();
    udp_peek_trunc();
    udp_zero_len();
    udp_fionread();
    udp_nonblock();
    udp_getsockname();
    udp_bind_conflict();
    tcp_bind_conflict();
    udp_shutdown();
    udp_sockopts();
    udp_burst();
    udp_large();
    udp_huge();
    udp_pmtud();
    udp_pmtud_notconn();
    udp_pmtud_netns();
    tcp_pmtud_clamp();
    udp6_pmtud();
    udp_recverr_refused();
    udp_recverr_emsgsize();
    udp_gso();
    udp_mmsg();
    udp_dup();
    udp_wildcard_connect();
    udp_icmp_refused();
    udp_mcast_loop();
    udp_broadcast();
    udp_reuse_unicast();
    udp_cmsgs();
    udp_gro();
    udp_disconnect_port();
    tcp_disconnect();
    udp_any_poll_tag();
    so_timeo_cases();
    netabi_cases();
    printf("udptest: %d passed, %d failed, %d skipped\n", npass, nfail, nskip);
    return nfail > 100 ? 100 : nfail;
}
