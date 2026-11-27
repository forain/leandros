// ip6test: AF_INET6 on the loopback (lane ip6lo, L1 of design-ipv6).
// Builds for the host too (cc ip6test.c) to confirm the expectations on Linux.
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <netdb.h>
#include <time.h>
#include <netinet/in.h>
#include <poll.h>
#include <net/if.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <netinet/icmp6.h>
#include <sys/wait.h>
#include <stdint.h>
#include <stdio.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

static int npass, nfail;
#define CHECK(name, cond) do { if (cond) { npass++; printf("PASS %s\n", name); } \
    else { nfail++; printf("FAIL %s (errno %d)\n", name, errno); } } while (0)

static void tmo(int s, int sec) {
    struct timeval tv = { .tv_sec = sec };
    setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    setsockopt(s, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
}

static struct sockaddr_in6 lo6(int port) {
    struct sockaddr_in6 a;
    memset(&a, 0, sizeof a);
    a.sin6_family = AF_INET6;
    a.sin6_port = htons(port);
    a.sin6_addr = in6addr_loopback;
    return a;
}

static int writeall(int s, const char *b, size_t n) {
    while (n) { ssize_t w = write(s, b, n); if (w <= 0) return -1; b += w; n -= w; }
    return 0;
}
static int readall(int s, char *b, size_t n) {
    while (n) { ssize_t r = read(s, b, n); if (r <= 0) return -1; b += r; n -= r; }
    return 0;
}

static void test_socket6(void) {
    int s = socket(AF_INET6, SOCK_STREAM, 0);
    CHECK("socket6", s >= 0);
    if (s >= 0) close(s);
}

static void test_tcp6_lo_echo(void) {
    enum { N = 65536 };
    int l = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 a = lo6(0), got;
    socklen_t gl = sizeof got;
    int ok = l >= 0 && bind(l, (struct sockaddr *)&a, sizeof a) == 0 && listen(l, 4) == 0 &&
             getsockname(l, (struct sockaddr *)&got, &gl) == 0;
    CHECK("tcp6_listen_getsockname", ok && gl == sizeof got && got.sin6_family == AF_INET6 &&
          got.sin6_port != 0 && !memcmp(&got.sin6_addr, &in6addr_loopback, 16));
    if (!ok) { CHECK("tcp6_lo_echo", 0); return; }
    int c = socket(AF_INET6, SOCK_STREAM, 0);
    tmo(c, 10);
    int r = connect(c, (struct sockaddr *)&got, sizeof got);
    CHECK("tcp6_connect", r == 0);
    if (r) { CHECK("tcp6_lo_echo", 0); return; }
    int srv = accept(l, NULL, NULL);
    tmo(srv, 10);
    char *out = malloc(N), *in = malloc(N), *back = malloc(N);
    for (int i = 0; i < N; i++) out[i] = (char)(i * 7 + 3);
    struct sockaddr_in6 peer; socklen_t pl = sizeof peer;
    int pn = srv >= 0 && getpeername(srv, (struct sockaddr *)&peer, &pl) == 0 && pl == sizeof peer &&
             peer.sin6_family == AF_INET6 && !memcmp(&peer.sin6_addr, &in6addr_loopback, 16);
    CHECK("tcp6_accept_peer", pn);
    // client -> server, then server echoes back
    int good = srv >= 0 && writeall(c, out, N) == 0 && readall(srv, in, N) == 0 &&
               !memcmp(in, out, N) && writeall(srv, in, N) == 0 && readall(c, back, N) == 0 &&
               !memcmp(back, out, N);
    CHECK("tcp6_lo_echo", good);
    close(c); if (srv >= 0) close(srv); close(l);
    free(out); free(in); free(back);
}

static void test_udp6_lo(void) {
    int a = socket(AF_INET6, SOCK_DGRAM, 0), b = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 la = lo6(0), got; socklen_t gl = sizeof got;
    int ok = a >= 0 && b >= 0 && bind(a, (struct sockaddr *)&la, sizeof la) == 0 &&
             getsockname(a, (struct sockaddr *)&got, &gl) == 0;
    if (!ok) { CHECK("udp6_lo", 0); return; }
    tmo(a, 5);
    ssize_t w = sendto(b, "hello6", 6, 0, (struct sockaddr *)&got, sizeof got);
    char buf[32]; struct sockaddr_in6 from; socklen_t fl = sizeof from;
    ssize_t r = recvfrom(a, buf, sizeof buf, 0, (struct sockaddr *)&from, &fl);
    CHECK("udp6_lo", w == 6 && r == 6 && !memcmp(buf, "hello6", 6) && fl == sizeof from &&
          from.sin6_family == AF_INET6 && !memcmp(&from.sin6_addr, &in6addr_loopback, 16));
    close(a); close(b);
}

static void test_v4_listener_no_v6(void) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_ANY) }, got;
    socklen_t gl = sizeof got;
    int ok = l >= 0 && bind(l, (struct sockaddr *)&a, sizeof a) == 0 && listen(l, 4) == 0 &&
             getsockname(l, (struct sockaddr *)&got, &gl) == 0;
    if (!ok) { CHECK("v4_listener_no_v6", 0); return; }
    int c = socket(AF_INET6, SOCK_STREAM, 0);
    tmo(c, 5);
    struct sockaddr_in6 t = lo6(ntohs(got.sin_port));
    int r = connect(c, (struct sockaddr *)&t, sizeof t);
    int e = errno;
    // nothing must be queued in the AF_INET listener
    struct pollfd p = { .fd = l, .events = POLLIN };
    int queued = poll(&p, 1, 200);
    CHECK("v4_listener_no_v6", r < 0 && e == ECONNREFUSED && queued == 0);
    close(c); close(l);
}

static void test_bind_family_aware(void) {
    int a = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in v4 = { .sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_ANY) }, got;
    socklen_t gl = sizeof got;
    int ok = a >= 0 && bind(a, (struct sockaddr *)&v4, sizeof v4) == 0 &&
             getsockname(a, (struct sockaddr *)&got, &gl) == 0;
    int b = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 t = lo6(ntohs(got.sin_port));
    int r = ok && b >= 0 ? bind(b, (struct sockaddr *)&t, sizeof t) : -1;
    CHECK("bind_family_aware", ok && r == 0);
    if (a >= 0) close(a);
    if (b >= 0) close(b);
}

static void test_unreach6(void) {
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 a; memset(&a, 0, sizeof a);
    a.sin6_family = AF_INET6; a.sin6_port = htons(9);
    inet_pton(AF_INET6, "2001:db8::1", &a.sin6_addr);
    int r = connect(s, (struct sockaddr *)&a, sizeof a);
    int e = errno;
    close(s);
    int t = socket(AF_INET6, SOCK_STREAM, 0); tmo(t, 1);
    int r2 = connect(t, (struct sockaddr *)&a, sizeof a);
    int e2 = errno;
    close(t);
    printf("  unreach6: udp %d/%d tcp %d/%d\n", r, e, r2, e2);
    /* With a SLAAC router the route exists (Linux: connect succeeds); without
     * one it is ENETUNREACH. Anything else is a bug. */
    CHECK("unreach6", (r == 0 || e == ENETUNREACH || e == ENETDOWN) &&
                      (r2 == 0 || e2 == ENETUNREACH || e2 == ENETDOWN || e2 == EINPROGRESS || e2 == ETIMEDOUT || e2 == EAGAIN));
}

static void test_bad_len6(void) {
    int s = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 a = lo6(0);
    int r = bind(s, (struct sockaddr *)&a, 16);
    CHECK("bad_len6", r < 0 && errno == EINVAL);
    close(s);
}

// ---- L2 ip6dual: dual-stack, V6ONLY, v4-mapped (design-ipv6 L2) ----
static int sock6(int type) { int s = socket(AF_INET6, type, 0); if (s >= 0) tmo(s, 3); return s; }
static struct sockaddr_in any6(int port) {
    struct sockaddr_in a; memset(&a, 0, sizeof a);
    a.sin_family = AF_INET; a.sin_port = htons(port); a.sin_addr.s_addr = htonl(INADDR_ANY);
    return a;
}
static int bind_any6(int s, int port) {
    struct sockaddr_in6 a; memset(&a, 0, sizeof a);
    a.sin6_family = AF_INET6; a.sin6_port = htons(port); a.sin6_addr = in6addr_any;
    return bind(s, (struct sockaddr *)&a, sizeof a);
}
static int bind_any4(int s, int port) {
    struct sockaddr_in a = any6(port);
    return bind(s, (struct sockaddr *)&a, sizeof a);
}
static int v6only_of(int s) { int v = -1; socklen_t l = sizeof v; if (getsockopt(s, IPPROTO_IPV6, IPV6_V6ONLY, &v, &l)) return -1; return v; }

static void test_v6only_default(void) {
    int s = sock6(SOCK_STREAM);
    CHECK("v6only_default", s >= 0 && v6only_of(s) == 0);
    int one = 1;
    CHECK("v6only_set_get", setsockopt(s, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one) == 0 && v6only_of(s) == 1);
    close(s);
}

static void test_dual_accept_v4(void) {
    int l = sock6(SOCK_STREAM), ok = 0;
    int one = 1; setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    if (l >= 0 && bind_any6(l, 27301) == 0 && listen(l, 4) == 0) {
        int c = socket(AF_INET, SOCK_STREAM, 0); tmo(c, 3);
        struct sockaddr_in d; memset(&d, 0, sizeof d);
        d.sin_family = AF_INET; d.sin_port = htons(27301); d.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        if (connect(c, (struct sockaddr *)&d, sizeof d) == 0) {
            struct sockaddr_in6 p; socklen_t pl = sizeof p; memset(&p, 0, sizeof p);
            int a = accept(l, (struct sockaddr *)&p, &pl);
            struct in6_addr want; inet_pton(AF_INET6, "::ffff:127.0.0.1", &want);
            ok = a >= 0 && p.sin6_family == AF_INET6 && memcmp(&p.sin6_addr, &want, 16) == 0;
            if (a >= 0) close(a);
        }
        close(c);
    }
    CHECK("dual_accept_v4", ok);
    if (l >= 0) close(l);
}

static void test_v6only_coexist(void) {
    int a = sock6(SOCK_STREAM), b = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(a, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
    int r1 = bind_any6(a, 27302), r2 = bind_any4(b, 27302);
    CHECK("v6only_coexist", r1 == 0 && r2 == 0);
    close(a); close(b);
}

static void test_dual_conflict(void) {
    int a = sock6(SOCK_STREAM), b = socket(AF_INET, SOCK_STREAM, 0);
    int r1 = bind_any6(a, 27303), r2 = bind_any4(b, 27303), e2 = errno;
    CHECK("dual_conflict_v4_second", r1 == 0 && r2 < 0 && e2 == EADDRINUSE);
    close(a); close(b);
    a = socket(AF_INET, SOCK_STREAM, 0); b = sock6(SOCK_STREAM);
    r1 = bind_any4(a, 27304); r2 = bind_any6(b, 27304); e2 = errno;
    CHECK("dual_conflict_v6_second", r1 == 0 && r2 < 0 && e2 == EADDRINUSE);
    close(a); close(b);
}

static void test_v6only_after_bind(void) {
    int s = sock6(SOCK_STREAM), one = 1;
    int r = bind_any6(s, 27305);
    int r2 = setsockopt(s, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
    CHECK("v6only_after_bind", r == 0 && r2 < 0 && errno == EINVAL);
    close(s);
}

static void test_mapped_connect_v6only(void) {
    int s = sock6(SOCK_STREAM), one = 1;
    setsockopt(s, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
    struct sockaddr_in6 d; memset(&d, 0, sizeof d);
    d.sin6_family = AF_INET6; d.sin6_port = htons(27306);
    inet_pton(AF_INET6, "::ffff:127.0.0.1", &d.sin6_addr);
    int r = connect(s, (struct sockaddr *)&d, sizeof d);
    CHECK("mapped_connect_v6only", r < 0 && errno == ENETUNREACH);
    close(s);
}

static void test_udp_dual(void) {
    int r = sock6(SOCK_DGRAM), ok = 0;
    if (r >= 0 && bind_any6(r, 27307) == 0) {
        int c = socket(AF_INET, SOCK_DGRAM, 0);
        struct sockaddr_in d; memset(&d, 0, sizeof d);
        d.sin_family = AF_INET; d.sin_port = htons(27307); d.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        sendto(c, "hi", 2, 0, (struct sockaddr *)&d, sizeof d);
        char buf[8]; struct sockaddr_in6 p; socklen_t pl = sizeof p; memset(&p, 0, sizeof p);
        ssize_t n = recvfrom(r, buf, sizeof buf, 0, (struct sockaddr *)&p, &pl);
        struct in6_addr want; inet_pton(AF_INET6, "::ffff:127.0.0.1", &want);
        ok = n == 2 && p.sin6_family == AF_INET6 && memcmp(&p.sin6_addr, &want, 16) == 0;
        close(c);
    }
    CHECK("udp_dual", ok);
    if (r >= 0) close(r);
}


// ---- L3 ip6icmp: ICMPv6 echo, ICMP6_FILTER, IPv6 options and cmsgs ----
static uint16_t csum6(const struct in6_addr *src, const struct in6_addr *dst, const uint8_t *m, size_t n) {
    uint32_t sum = 0; size_t i;
    for (i = 0; i < 16; i += 2) sum += (src->s6_addr[i] << 8) | src->s6_addr[i + 1];
    for (i = 0; i < 16; i += 2) sum += (dst->s6_addr[i] << 8) | dst->s6_addr[i + 1];
    sum += n; sum += 58;
    for (i = 0; i + 1 < n; i += 2) sum += (m[i] << 8) | m[i + 1];
    if (n & 1) sum += m[n - 1] << 8;
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return (uint16_t)~sum;
}

static size_t echo_req(uint8_t *b, int id, int seq) {
    memset(b, 0, 64);
    b[0] = ICMP6_ECHO_REQUEST; b[4] = id >> 8; b[5] = id; b[6] = seq >> 8; b[7] = seq;
    memcpy(b + 8, "ip6icmp-payload!", 16);
    return 24;
}

// Read until an echo reply (type 129) with `seq` arrives; returns its length or -1.
static int await_reply(int s, int seq, uint8_t *r, size_t cap) {
    for (int i = 0; i < 8; i++) {
        struct sockaddr_in6 from; socklen_t fl = sizeof from;
        ssize_t n = recvfrom(s, r, cap, 0, (struct sockaddr *)&from, &fl);
        if (n < 8) return -1;
        if (r[0] == ICMP6_ECHO_REPLY && ((r[6] << 8) | r[7]) == seq) return (int)n;
    }
    return -1;
}

static int ping6_run(int type, int raw) {
    int s = socket(AF_INET6, type, IPPROTO_ICMPV6);
    if (s < 0) return -1;
    tmo(s, 3);
    struct sockaddr_in6 a = lo6(0);
    int good = 0;
    for (int seq = 1; seq <= 4; seq++) {
        uint8_t q[64], r[256];
        size_t n = echo_req(q, 0x1234, seq);
        if (sendto(s, q, n, 0, (struct sockaddr *)&a, sizeof a) != (ssize_t)n) continue;
        int got = await_reply(s, seq, r, sizeof r);
        if (got != 24 || memcmp(r + 8, q + 8, 16)) continue;
        if (raw && csum6(&in6addr_loopback, &in6addr_loopback, r, got) != 0) continue;
        good++;
    }
    close(s);
    return good;
}

// Hop limit 0 stays 0 (Linux), -1 restores the default (lane v6gaps).
static void test_hops0(void) {
    int s = sock6(SOCK_DGRAM), v = -1, w = 0;
    socklen_t l = sizeof v;
    int ok = s >= 0 && setsockopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &w, sizeof w) == 0
          && getsockopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &v, &l) == 0 && v == 0;
    w = -1; l = sizeof v; v = 7;
    ok = ok && setsockopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &w, sizeof w) == 0
          && getsockopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &v, &l) == 0 && v == 64;
    CHECK("hops0", ok);
    if (s >= 0) close(s);
}

// A v4-mapped destination on a v6 ping socket is EINVAL (Linux ping_v6_sendmsg).
static void test_ping6_mapped_einval(void) {
    int s = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
    if (s < 0) { CHECK("ping6_mapped_einval", 0); return; }
    struct sockaddr_in6 a; memset(&a, 0, sizeof a);
    a.sin6_family = AF_INET6;
    inet_pton(AF_INET6, "::ffff:127.0.0.1", &a.sin6_addr);
    uint8_t q[64]; size_t n = echo_req(q, 0x1234, 1);
    CHECK("ping6_mapped_einval", sendto(s, q, n, 0, (struct sockaddr *)&a, sizeof a) < 0 && errno == EINVAL);
    close(s);
}

// ICMPv6 sockets carry IPV6_HOPLIMIT / IPV6_TCLASS / IPV6_PKTINFO cmsgs (lane v6gaps).
static void test_icmp6_cmsgs(void) {
    int s = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
    if (s < 0) { CHECK("icmp6_cmsgs", 0); return; }
    tmo(s, 3);
    int on = 1;
    setsockopt(s, IPPROTO_IPV6, IPV6_RECVHOPLIMIT, &on, sizeof on);
    setsockopt(s, IPPROTO_IPV6, IPV6_RECVTCLASS, &on, sizeof on);
    setsockopt(s, IPPROTO_IPV6, IPV6_RECVPKTINFO, &on, sizeof on);
    struct sockaddr_in6 a = lo6(0);
    uint8_t q[64], r[256]; size_t n = echo_req(q, 0x1234, 9);
    int hop = -1, tcl = -1, pi = 0;
    if (sendto(s, q, n, 0, (struct sockaddr *)&a, sizeof a) == (ssize_t)n) {
        for (int i = 0; i < 4; i++) {
            char cb[256];
            struct iovec iv = { r, sizeof r };
            struct msghdr m; memset(&m, 0, sizeof m);
            m.msg_iov = &iv; m.msg_iovlen = 1; m.msg_control = cb; m.msg_controllen = sizeof cb;
            ssize_t g = recvmsg(s, &m, 0);
            if (g < 8) break;
            if (r[0] != 129) continue;
            for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c)) {
                if (c->cmsg_level != IPPROTO_IPV6) continue;
                if (c->cmsg_type == IPV6_HOPLIMIT) memcpy(&hop, CMSG_DATA(c), sizeof hop);
                else if (c->cmsg_type == IPV6_TCLASS) memcpy(&tcl, CMSG_DATA(c), sizeof tcl);
                else if (c->cmsg_type == IPV6_PKTINFO) {
                    struct in6_pktinfo p; memcpy(&p, CMSG_DATA(c), sizeof p);
                    pi = !memcmp(&p.ipi6_addr, &in6addr_loopback, 16);
                }
            }
            break;
        }
    }
    CHECK("icmp6_cmsgs", hop > 0 && hop <= 255 && tcl == 0 && pi);
    close(s);
}

static void test_ping6_dgram_lo(void) {
    // As an ordinary user: SOCK_DGRAM/IPPROTO_ICMPV6 is the unprivileged ping socket.
    pid_t p = fork();
    if (p == 0) {
        if (setgid(1000) || setuid(1000)) _exit(2);
        _exit(ping6_run(SOCK_DGRAM, 0) == 4 ? 0 : 1);
    }
    int st = 0; waitpid(p, &st, 0);
    CHECK("ping6_dgram_lo", WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

static void test_ping6_raw_lo(void) {
    CHECK("ping6_raw_lo", ping6_run(SOCK_RAW, 1) == 4);
}

static void test_raw6_eperm(void) {
    pid_t p = fork();
    if (p == 0) {
        if (setgid(1000) || setuid(1000)) _exit(2);
        int s = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
        _exit(s < 0 && errno == EPERM ? 0 : 1);
    }
    int st = 0; waitpid(p, &st, 0);
    CHECK("raw6_eperm", WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

static void test_icmp6_filter(void) {
    int s = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
    if (s < 0) { CHECK("icmp6_filter", 0); return; }
    tmo(s, 1);
    struct icmp6_filter f, g; socklen_t gl = sizeof g;
    ICMP6_FILTER_SETBLOCKALL(&f);
    int ok = setsockopt(s, IPPROTO_ICMPV6, ICMP6_FILTER, &f, sizeof f) == 0 &&
             getsockopt(s, IPPROTO_ICMPV6, ICMP6_FILTER, &g, &gl) == 0 && gl == sizeof g && !memcmp(&f, &g, sizeof f);
    struct sockaddr_in6 a = lo6(0);
    uint8_t q[64]; size_t n = echo_req(q, 0x4321, 9);
    ok = ok && sendto(s, q, n, 0, (struct sockaddr *)&a, sizeof a) == (ssize_t)n;
    struct pollfd pf = { .fd = s, .events = POLLIN };
    CHECK("icmp6_filter_blocks", ok && poll(&pf, 1, 600) == 0);
    // Let echo replies through again and the next one is delivered.
    ICMP6_FILTER_SETPASSALL(&f);
    ok = setsockopt(s, IPPROTO_ICMPV6, ICMP6_FILTER, &f, sizeof f) == 0;
    n = echo_req(q, 0x4321, 10);
    uint8_t r[128];
    ok = ok && sendto(s, q, n, 0, (struct sockaddr *)&a, sizeof a) == (ssize_t)n && await_reply(s, 10, r, sizeof r) == 24;
    CHECK("icmp6_filter_pass", ok);
    close(s);
}

static void test_pktinfo6(void) {
    int r = sock6(SOCK_DGRAM), t = sock6(SOCK_DGRAM);
    struct sockaddr_in6 a = lo6(0), got; socklen_t gl = sizeof got;
    int one = 1, hl = 33, tc = 0x28;
    int ok = r >= 0 && t >= 0 && bind(r, (struct sockaddr *)&a, sizeof a) == 0 &&
             getsockname(r, (struct sockaddr *)&got, &gl) == 0;
    ok = ok && setsockopt(r, IPPROTO_IPV6, IPV6_RECVPKTINFO, &one, sizeof one) == 0 &&
         setsockopt(r, IPPROTO_IPV6, IPV6_RECVHOPLIMIT, &one, sizeof one) == 0 &&
         setsockopt(r, IPPROTO_IPV6, IPV6_RECVTCLASS, &one, sizeof one) == 0 &&
         setsockopt(t, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &hl, sizeof hl) == 0 &&
         setsockopt(t, IPPROTO_IPV6, IPV6_TCLASS, &tc, sizeof tc) == 0;
    CHECK("pktinfo6_setup", ok);
    if (!ok) return;
    ok = sendto(t, "hi", 2, 0, (struct sockaddr *)&got, gl) == 2;
    char buf[16], cb[128];
    struct iovec iov = { buf, sizeof buf };
    struct msghdr m; memset(&m, 0, sizeof m);
    m.msg_iov = &iov; m.msg_iovlen = 1; m.msg_control = cb; m.msg_controllen = sizeof cb;
    ok = ok && recvmsg(r, &m, 0) == 2;
    int pi = 0, hop = -1, tcl = -1;
    for (struct cmsghdr *c = ok ? CMSG_FIRSTHDR(&m) : NULL; c; c = CMSG_NXTHDR(&m, c)) {
        if (c->cmsg_level != IPPROTO_IPV6) continue;
        if (c->cmsg_type == IPV6_PKTINFO) {
            struct in6_pktinfo *p = (struct in6_pktinfo *)CMSG_DATA(c);
            pi = !memcmp(&p->ipi6_addr, &in6addr_loopback, 16) && p->ipi6_ifindex == 1;
        } else if (c->cmsg_type == IPV6_HOPLIMIT) memcpy(&hop, CMSG_DATA(c), sizeof hop);
        else if (c->cmsg_type == IPV6_TCLASS) memcpy(&tcl, CMSG_DATA(c), sizeof tcl);
    }
    CHECK("pktinfo6", pi);
    CHECK("hoplimit6", hop == 33);
    CHECK("tclass6", tcl == 0x28);
    close(r); close(t);
}

static void test_opts6(void) {
    int s = sock6(SOCK_DGRAM), v = -1, w;
    socklen_t l = sizeof v;
    int ok = s >= 0 && getsockopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &v, &l) == 0 && v == 64;
    l = sizeof v; v = -1;
    ok = ok && getsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_HOPS, &v, &l) == 0 && v == 1;
    w = 17; ok = ok && setsockopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &w, sizeof w) == 0;
    l = sizeof v; v = -1;
    ok = ok && getsockopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &v, &l) == 0 && v == 17;
    w = 256; int bad = setsockopt(s, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &w, sizeof w) == -1 && errno == EINVAL;
    w = 1; int rp = setsockopt(s, IPPROTO_IPV6, IPV6_RECVPKTINFO, &w, sizeof w) == 0;
    l = sizeof v; v = -1;
    rp = rp && getsockopt(s, IPPROTO_IPV6, IPV6_RECVPKTINFO, &v, &l) == 0 && v == 1;
    CHECK("opts6", ok && bad && rp);
    if (s >= 0) close(s);
}

// --- resolver (L7): all guest userland is musl (relibc retired 2026-11-02), so these exercise musl's
// inet_pton/ntop, getaddrinfo (/etc/hosts, AI_ADDRCONFIG, scope ids) and getnameinfo on our kernel.
static void test_pton_ntop6(void) {
    struct in6_addr a, b; char buf[INET6_ADDRSTRLEN];
    int r = inet_pton(AF_INET6, "2001:db8::1", &a);
    unsigned char want[16] = {0x20,1,0x0d,0xb8,0,0,0,0,0,0,0,0,0,0,0,1};
    CHECK("pton6", r == 1 && !memcmp(&a, want, 16));
    CHECK("ntop6", inet_ntop(AF_INET6, &a, buf, sizeof buf) && !strcmp(buf, "2001:db8::1"));
    r = inet_pton(AF_INET6, "::ffff:1.2.3.4", &b);
    CHECK("pton6_mapped", r == 1 && inet_ntop(AF_INET6, &b, buf, sizeof buf) && !strcmp(buf, "::ffff:1.2.3.4"));
    CHECK("pton6_bad", inet_pton(AF_INET6, "2001:db8:::1", &a) == 0 && inet_pton(AF_INET6, "1:2:3:4:5:6:7:8:9", &a) == 0);
    CHECK("ntop6_loopback", inet_ntop(AF_INET6, &in6addr_loopback, buf, sizeof buf) && !strcmp(buf, "::1"));
    CHECK("ntop6_enospc", !inet_ntop(AF_INET6, &a, buf, 4) && errno == ENOSPC);
}

static int gai_count(const char *host, int fam, int flags, int *n4, int *n6, int *first) {
    struct addrinfo h, *res = 0, *p; memset(&h, 0, sizeof h);
    h.ai_family = fam; h.ai_flags = flags; h.ai_socktype = SOCK_STREAM;
    int r = getaddrinfo(host, "80", &h, &res);
    *n4 = *n6 = 0; *first = 0;
    if (r) return r;
    for (p = res; p; p = p->ai_next) { if (p == res) *first = p->ai_family; if (p->ai_family == AF_INET) (*n4)++; else if (p->ai_family == AF_INET6) (*n6)++; }
    freeaddrinfo(res);
    return 0;
}

static void test_gai(void) {
    int n4, n6, f, r;
    r = gai_count("localhost", AF_INET6, 0, &n4, &n6, &f);
    CHECK("gai_localhost6", r == 0 && n6 >= 1 && n4 == 0);
    r = gai_count("localhost", AF_INET, 0, &n4, &n6, &f);
    CHECK("gai_localhost4", r == 0 && n4 >= 1 && n6 == 0);
    r = gai_count("localhost", AF_UNSPEC, 0, &n4, &n6, &f);
    CHECK("gai_localhost_unspec", r == 0 && n4 >= 1 && n6 >= 1);
    r = gai_count("localhost", AF_UNSPEC, AI_ADDRCONFIG, &n4, &n6, &f);
    CHECK("gai_localhost_addrconfig", r == 0 && n4 >= 1 && n6 >= 1);
    r = gai_count("::1", AF_UNSPEC, AI_NUMERICHOST, &n4, &n6, &f);
    CHECK("gai_numeric_loopback6", r == 0 && n6 == 1 && n4 == 0);
    r = gai_count("::1", AF_INET, AI_NUMERICHOST, &n4, &n6, &f);
    CHECK("gai_numeric6_vs_inet", r != 0); /* musl: EAI_NODATA/NONAME, glibc: EAI_ADDRFAMILY */
    r = gai_count("127.0.0.1", AF_INET6, AI_V4MAPPED, &n4, &n6, &f);
    CHECK("gai_v4mapped", r == 0 && n6 == 1);
    struct addrinfo h, *res = 0; memset(&h, 0, sizeof h);
    h.ai_family = AF_INET6; h.ai_flags = AI_NUMERICHOST; h.ai_socktype = SOCK_STREAM;
    r = getaddrinfo("fe80::1%lo", "80", &h, &res);
    unsigned want = if_nametoindex("lo");
    CHECK("gai_numeric6_scope", r == 0 && res && want &&
          ((struct sockaddr_in6 *)res->ai_addr)->sin6_scope_id == want &&
          ntohs(((struct sockaddr_in6 *)res->ai_addr)->sin6_port) == 80);
    if (res) freeaddrinfo(res);
}

static void test_gni6(void) {
    struct sockaddr_in6 a = lo6(8080); char h[64], sv[16];
    int r = getnameinfo((struct sockaddr *)&a, sizeof a, h, sizeof h, sv, sizeof sv, NI_NUMERICHOST | NI_NUMERICSERV);
    CHECK("gni6_numeric", r == 0 && !strcmp(h, "::1") && !strcmp(sv, "8080"));
    r = getnameinfo((struct sockaddr *)&a, sizeof a, h, sizeof h, 0, 0, 0);
    CHECK("gni6_name_localhost", r == 0 && !strcmp(h, "localhost"));
}

// R1: a client that resolves "localhost" (::1 first) must fall through to a 127.0.0.1-only
// listener quickly, never hang on the refused ::1 attempt.
static void test_localhost_fallback(void) {
    int l = socket(AF_INET, SOCK_STREAM, 0), ok = 0;
    struct sockaddr_in a; memset(&a, 0, sizeof a);
    a.sin_family = AF_INET; a.sin_port = htons(27320); a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(l, (struct sockaddr *)&a, sizeof a) == 0 && listen(l, 4) == 0) {
        struct addrinfo h, *res = 0, *p; memset(&h, 0, sizeof h);
        h.ai_family = AF_UNSPEC; h.ai_socktype = SOCK_STREAM;
        struct timespec t0, t1; clock_gettime(CLOCK_MONOTONIC, &t0);
        if (getaddrinfo("localhost", "27320", &h, &res) == 0) {
            for (p = res; p && !ok; p = p->ai_next) {
                int c = socket(p->ai_family, SOCK_STREAM, 0); tmo(c, 3);
                if (connect(c, p->ai_addr, p->ai_addrlen) == 0) ok = 1;
                close(c);
            }
            freeaddrinfo(res);
        }
        clock_gettime(CLOCK_MONOTONIC, &t1);
        if (t1.tv_sec - t0.tv_sec > 2) ok = 0;
    }
    CHECK("localhost_fallback_v4only", ok);
    close(l);
}

// `ip6test banner ADDR PORT`: connect (v4 or v6 literal), print the first bytes the server sends.
static int banner(const char *addr, int port) {
    struct sockaddr_in6 a6; struct sockaddr_in a4; int s, r;
    memset(&a6, 0, sizeof a6); memset(&a4, 0, sizeof a4);
    if (inet_pton(AF_INET6, addr, &a6.sin6_addr) == 1) {
        a6.sin6_family = AF_INET6; a6.sin6_port = htons(port);
        s = socket(AF_INET6, SOCK_STREAM, 0); tmo(s, 5);
        r = connect(s, (struct sockaddr *)&a6, sizeof a6);
    } else if (inet_pton(AF_INET, addr, &a4.sin_addr) == 1) {
        a4.sin_family = AF_INET; a4.sin_port = htons(port);
        s = socket(AF_INET, SOCK_STREAM, 0); tmo(s, 5);
        r = connect(s, (struct sockaddr *)&a4, sizeof a4);
    } else return 2;
    if (r) { printf("connect failed: %d\n", errno); return 1; }
    char b[16]; ssize_t n = read(s, b, 4);
    if (n < 4) { printf("read failed: %zd\n", n); return 1; }
    b[4] = 0; printf("%s\n", b);
    return 0;
}

// ── v6 / v4-mapped TCP inside a non-initial netns (lane ip6nswake) ───────────
// Peer writes then closes; the client must see POLLIN, the data, EOF
// (peer shutdown(SHUT_WR) -> POLLIN|POLLRDHUP, both shut -> POLLHUP).
// mode: 0 v6 client -> dual [::] server (::ffff:127.0.0.1), 1 v4 client -> dual
// server, 4 v4/v4 baseline, 2 v6 client -> v4 server (mapped), 3 v6 client -> [::1] server.
static int ns6_body(int mode) {
    if (unshare(CLONE_NEWNET) != 0) return 2;
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct ifreq q; memset(&q, 0, sizeof q); strcpy(q.ifr_name, "lo"); q.ifr_flags = IFF_UP | IFF_RUNNING;
    if (u < 0 || ioctl(u, SIOCSIFFLAGS, &q) != 0) return 3;
    close(u);
    int port = 27400 + mode;
    int v4s = mode == 2 || mode == 4;
    int l = socket(v4s ? AF_INET : AF_INET6, SOCK_STREAM, 0);
    if (l < 0) return 4;
    tmo(l, 3);
    if (v4s) {
        struct sockaddr_in a; memset(&a, 0, sizeof a); a.sin_family = AF_INET; a.sin_port = htons(port);
        a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        if (bind(l, (struct sockaddr *)&a, sizeof a)) return 5;
    } else {
        struct sockaddr_in6 a; memset(&a, 0, sizeof a); a.sin6_family = AF_INET6; a.sin6_port = htons(port);
        if (mode == 3) a.sin6_addr = in6addr_loopback;
        if (bind(l, (struct sockaddr *)&a, sizeof a)) return 5;
    }
    if (listen(l, 4)) return 6;
    int c = socket(mode == 1 || mode == 4 ? AF_INET : AF_INET6, SOCK_STREAM, 0);
    if (c < 0) return 7;
    tmo(c, 3);
    int rc;
    if (mode == 1 || mode == 4) {
        struct sockaddr_in d; memset(&d, 0, sizeof d); d.sin_family = AF_INET; d.sin_port = htons(port);
        d.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        rc = connect(c, (struct sockaddr *)&d, sizeof d);
    } else {
        struct sockaddr_in6 d; memset(&d, 0, sizeof d); d.sin6_family = AF_INET6; d.sin6_port = htons(port);
        if (mode == 3) d.sin6_addr = in6addr_loopback; else inet_pton(AF_INET6, "::ffff:127.0.0.1", &d.sin6_addr);
        rc = connect(c, (struct sockaddr *)&d, sizeof d);
    }
    if (rc) return 8;
    int a = accept(l, NULL, NULL);
    if (a < 0) return 9;
    if (write(a, "hi", 2) != 2) return 10;
    // Peer FIN without closing: Linux reports POLLIN|POLLRDHUP, not POLLHUP.
    if (shutdown(a, SHUT_WR) != 0) return 15;
    struct pollfd p = { .fd = c, .events = POLLIN | POLLRDHUP };
    int pr = poll(&p, 1, 3000);
    if (pr != 1) return 11;
    if (!(p.revents & POLLIN)) return 14;
    // The data may be readable before the FIN has been processed.
    for (int i = 0; i < 150 && !(p.revents & POLLRDHUP); i++) {
        usleep(20000);
        p.revents = 0;
        poll(&p, 1, 0);
    }
    if (!(p.revents & POLLRDHUP)) return 16;
    if (p.revents & POLLHUP) return 17;
    char b[8];
    if (read(c, b, sizeof b) != 2) return 12;
    if (read(c, b, sizeof b) != 0) return 13;
    // Both directions shut: POLLHUP.
    if (shutdown(c, SHUT_WR) != 0) return 18;
    p.revents = 0;
    for (int i = 0; i < 30 && !(p.revents & POLLHUP); i++) {
        p.revents = 0;
        poll(&p, 1, 100);
    }
    if (!(p.revents & POLLHUP)) return 19;
    close(a);
    return 0;
}

static void test_netns_v6_tcp(void) {
    static const char *nm[] = { "ns_v6_to_dual", "ns_v4_to_dual", "ns_v6_to_v4", "ns_v6_loopback", "ns_v4_baseline" };
    for (int m = 0; m < 5; m++) {
        pid_t k = fork();
        if (k == 0) _exit(ns6_body(m));
        int st = 0; waitpid(k, &st, 0);
        errno = WIFEXITED(st) ? WEXITSTATUS(st) : -1;
        CHECK(nm[m], WIFEXITED(st) && WEXITSTATUS(st) == 0);
    }
}


// ---- L8 ip6ns: UDP / unconnected v6 in a non-initial netns ----
static int nsudp6_body(void) {
    if (unshare(CLONE_NEWNET) != 0) return 2;
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct ifreq q; memset(&q, 0, sizeof q); strcpy(q.ifr_name, "lo"); q.ifr_flags = IFF_UP | IFF_RUNNING;
    if (u < 0 || ioctl(u, SIOCSIFFLAGS, &q) != 0) return 3;
    close(u);
    int a = socket(AF_INET6, SOCK_DGRAM, 0), b = socket(AF_INET6, SOCK_DGRAM, 0);
    if (a < 0 || b < 0) return 4;
    tmo(a, 3); tmo(b, 3);
    struct sockaddr_in6 la; memset(&la, 0, sizeof la); la.sin6_family = AF_INET6; la.sin6_addr = in6addr_loopback; la.sin6_port = htons(27520);
    if (bind(a, (struct sockaddr *)&la, sizeof la)) return 5;
    if (sendto(b, "udp6", 4, 0, (struct sockaddr *)&la, sizeof la) != 4) return 6;
    char r[8]; struct sockaddr_in6 from; socklen_t fl = sizeof from;
    if (recvfrom(a, r, sizeof r, 0, (struct sockaddr *)&from, &fl) != 4 || memcmp(r, "udp6", 4)) return 7;
    if (!IN6_IS_ADDR_LOOPBACK(&from.sin6_addr)) return 8;
    // an unreachable global destination is a routing error, not a blanket refusal
    struct sockaddr_in6 far; memset(&far, 0, sizeof far); far.sin6_family = AF_INET6; far.sin6_port = htons(9);
    inet_pton(AF_INET6, "2001:db8::1", &far.sin6_addr);
    { int fs = socket(AF_INET6, SOCK_DGRAM, 0); ssize_t fr = sendto(fs, "x", 1, 0, (struct sockaddr *)&far, sizeof far); if (fr >= 0) return 9; if (errno != ENETUNREACH) return 100 + errno; }
    return 0;
}
static void test_netns_udp6(void) {
    pid_t k = fork();
    if (k == 0) _exit(nsudp6_body());
    int st = 0; waitpid(k, &st, 0);
    errno = WIFEXITED(st) ? WEXITSTATUS(st) : -1;
    CHECK("ns_udp6_lo", WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

// ---- L4 ip6link: link-local addresses, scope ids, NDP on the real NIC ----
#include <sys/ioctl.h>

static int if_inet6_find(const char *ifn, const char *prefix, char *line, size_t ll) {
    FILE *f = fopen("/proc/net/if_inet6", "r");
    if (!f) return 0;
    char b[256]; int found = 0;
    while (fgets(b, sizeof b, f)) {
        size_t n = strlen(b);
        while (n && (b[n-1] == '\n' || b[n-1] == ' ')) b[--n] = 0;
        size_t k = strlen(ifn);
        if (n >= k && !strcmp(b + n - k, ifn) && !strncmp(b, prefix, strlen(prefix))) {
            if (line) snprintf(line, ll, "%s", b);
            found = 1; break;
        }
    }
    fclose(f);
    return found;
}

static int eth0_mac(unsigned char *m) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct ifreq r; memset(&r, 0, sizeof r); strcpy(r.ifr_name, "eth0");
    int ok = s >= 0 && ioctl(s, 0x8927 /* SIOCGIFHWADDR */, &r) == 0;
    if (ok) memcpy(m, r.ifr_hwaddr.sa_data, 6);
    if (s >= 0) close(s);
    return ok;
}

static void test_ll_present(void) {
    unsigned char m[6]; char want[40], line[256] = "";
    int ok = eth0_mac(m);
    if (ok) {
        snprintf(want, sizeof want, "fe80000000000000%02x%02x%02xfffe%02x%02x%02x",
                 m[0] ^ 2, m[1], m[2], m[3], m[4], m[5]);
        ok = if_inet6_find("eth0", "fe80", line, sizeof line) && !strncmp(line, want, 32);
        // addr ifindex prefix(0x40) scope(0x20)
        if (ok) { unsigned ix, pl, sc; ok = sscanf(line + 33, "%x %x %x", &ix, &pl, &sc) == 3 && pl == 0x40 && sc == 0x20 && ix == if_nametoindex("eth0"); }
    }
    if (!ok) printf("  want fe80::..%s got [%s]\n", ok ? "" : "", line);
    CHECK("ll_present", ok);
}

/* The default router learned from the RA (nexthop of the ::/0 line of
 * /proc/net/ipv6_route); slirp's is fe80::2, a vmnet host's is not. */
static int find_router(unsigned char *out16) {
    FILE *f = fopen("/proc/net/ipv6_route", "r");
    if (!f) return 0;
    char b[256], d[40], sr[40], nh[40]; unsigned pl, sp; int found = 0;
    while (!found && fgets(b, sizeof b, f)) {
        if (sscanf(b, "%32s %x %32s %x %32s", d, &pl, sr, &sp, nh) == 5 && pl == 0 &&
            strspn(d, "0") == 32) {
            for (int i = 0; i < 16; i++) sscanf(nh + 2 * i, "%2hhx", &out16[i]);
            found = 1;
        }
    }
    fclose(f);
    return found;
}

static struct sockaddr_in6 router_ll(void) {
    struct sockaddr_in6 a; memset(&a, 0, sizeof a);
    a.sin6_family = AF_INET6;
    if (!find_router(a.sin6_addr.s6_addr)) inet_pton(AF_INET6, "fe80::2", &a.sin6_addr);
    a.sin6_scope_id = if_nametoindex("eth0");
    return a;
}

static void test_ping6_ll_router(void) {
    int s = socket(AF_INET6, SOCK_DGRAM, IPPROTO_ICMPV6);
    if (s < 0) { CHECK("ping6_ll_router", 0); return; }
    tmo(s, 3);
    struct sockaddr_in6 a = router_ll();
    int good = 0;
    for (int seq = 1; seq <= 4; seq++) {
        uint8_t q[64], r[256];
        size_t n = echo_req(q, 0x4321, seq);
        if (sendto(s, q, n, 0, (struct sockaddr *)&a, sizeof a) != (ssize_t)n) continue;
        int got = await_reply(s, seq, r, sizeof r);
        if (got == 24 && !memcmp(r + 8, q + 8, 16)) good++;
    }
    close(s);
    if (good != 4) printf("  ping router%%eth0: %d/4\n", good);
    CHECK("ping6_ll_router", good == 4);
}

/* The SLAAC address of eth0 (any non-fe80 line), as 32 hex digits. */
static int find_global(char *hex) {
    FILE *f = fopen("/proc/net/if_inet6", "r");
    if (!f) return 0;
    char b[256]; int found = 0;
    while (fgets(b, sizeof b, f)) {
        if (strncmp(b, "fe80", 4) && strstr(b, "eth0") && strncmp(b, "0000000000000000", 16)) {
            memcpy(hex, b, 32); hex[32] = 0; found = 1; break;
        }
    }
    fclose(f);
    return found;
}

static void test_slaac_addr(void) {
    unsigned char m[6];
    char line[256], want[40];
    int ok = eth0_mac(m), found = 0;
    for (int i = 0; ok && i < 100 && !found; i++) {
        found = find_global(line);
        if (!found) usleep(100000);
    }
    if (ok) {
        snprintf(want, sizeof want, "%02x%02x%02xfffe%02x%02x%02x",
                 m[0] ^ 2, m[1], m[2], m[3], m[4], m[5]);
        ok = found && !strncmp(line + 16, want, 16);
    }
    CHECK("slaac_addr", ok);
}

/* RDNSS (RFC 8106): slirp's RA carries the v6 resolver; the net server stands in for the
 * connection manager and lists it in /etc/resolv.conf (after any DHCP v4 servers), exactly the
 * view NetworkManager/systemd-networkd give a Linux userland. */
static int resolv_v6(char *out, size_t n) {
    FILE *f = fopen("/etc/resolv.conf", "r");
    if (!f) return 0;
    char b[256]; int found = 0;
    while (!found && fgets(b, sizeof b, f)) {
        char a[128];
        if (sscanf(b, "nameserver %127s", a) == 1 && strchr(a, ':')) { snprintf(out, n, "%s", a); found = 1; }
    }
    fclose(f);
    return found;
}

static void test_ping6_global(void) {
    int s = socket(AF_INET6, SOCK_DGRAM, IPPROTO_ICMPV6);
    if (s < 0) { CHECK("ping6_global", 0); return; }
    tmo(s, 3);
    struct sockaddr_in6 a; memset(&a, 0, sizeof a);
    a.sin6_family = AF_INET6;
    char g[40] = "";
    if (!find_global(g)) { CHECK("ping6_global", 0); close(s); return; }
    for (int i = 0; i < 8; i++) sscanf(g + 2 * i, "%2hhx", &a.sin6_addr.s6_addr[i]);
    unsigned char rt[16] = {0};
    if (!find_router(rt)) rt[15] = 2;
    /* The router's interface id on our prefix: slirp's host is prefix::2
     * (= fe80::2); on a vmnet host the global address is a privacy
     * address that cannot be derived, so there is nothing to ping. */
    static const unsigned char two[16] = {[15] = 2};
    if (memcmp(rt + 8, two + 8, 8)) {
        printf("  skip: router is not slirp's (iid != ::2), cannot derive its global address\n");
        CHECK("ping6_global", 1);
        close(s);
        return;
    }
    memcpy(a.sin6_addr.s6_addr + 8, rt + 8, 8);
    int good = 0;
    for (int seq = 1; seq <= 4; seq++) {
        uint8_t q[64], r[256];
        size_t n = echo_req(q, 0x4322, seq);
        if (sendto(s, q, n, 0, (struct sockaddr *)&a, sizeof a) != (ssize_t)n) continue;
        int got = await_reply(s, seq, r, sizeof r);
        if (got == 24 && !memcmp(r + 8, q + 8, 16)) good++;
    }
    close(s);
    if (good != 4) printf("  ping <prefix>::2: %d/4\n", good);
    CHECK("ping6_global", good == 4);
}

/* ICMPv6 echo to this host's OWN global address is delivered locally (it
 * used to go out the NIC and never come back); the v4 twin pings eth0's
 * own IPv4 address. */
static void test_ping6_own_global(void) {
    char g[40] = "";
    if (!find_global(g)) { CHECK("ping6_own_global", 0); return; }
    int s = socket(AF_INET6, SOCK_DGRAM, IPPROTO_ICMPV6);
    if (s < 0) { CHECK("ping6_own_global", 0); return; }
    tmo(s, 3);
    struct sockaddr_in6 a; memset(&a, 0, sizeof a);
    a.sin6_family = AF_INET6;
    for (int i = 0; i < 16; i++) sscanf(g + 2 * i, "%2hhx", &a.sin6_addr.s6_addr[i]);
    int good = 0;
    for (int seq = 1; seq <= 4; seq++) {
        uint8_t q[64], r[256];
        size_t n = echo_req(q, 0x4323, seq);
        if (sendto(s, q, n, 0, (struct sockaddr *)&a, sizeof a) != (ssize_t)n) continue;
        int got = await_reply(s, seq, r, sizeof r);
        if (got == 24 && !memcmp(r + 8, q + 8, 16)) good++;
        else printf("  own global seq %d: got=%d errno=%d\n", seq, got, errno);
    }
    close(s);
    if (good != 4) {
        printf("  ping own global: %d/4 errno=%d target=%s\n", good, errno, g);
        FILE *f = fopen("/proc/net/if_inet6", "r");
        char b[256];
        while (f && fgets(b, sizeof b, f)) printf("    %s", b);
        if (f) fclose(f);
    }
    CHECK("ping6_own_global", good == 4);
}

/* The router answers a ping to its link-local address: the recvfrom() source of the reply is
 * fe80::2%eth0, so sin6_scope_id must be eth0's index (Linux fills it for link-local peers). */
static void test_ll_recv_scope(void) {
    int s = socket(AF_INET6, SOCK_DGRAM, IPPROTO_ICMPV6);
    if (s < 0) { CHECK("ll_recv_scope", 0); return; }
    tmo(s, 3);
    struct sockaddr_in6 a = router_ll(), from; socklen_t fl = sizeof from;
    unsigned want = if_nametoindex("eth0");
    int ok = 0;
    for (int seq = 1; seq <= 4 && !ok; seq++) {
        uint8_t q[64], r[256];
        size_t n = echo_req(q, 0x4324, seq);
        if (sendto(s, q, n, 0, (struct sockaddr *)&a, sizeof a) != (ssize_t)n) continue;
        for (int i = 0; i < 8; i++) {
            fl = sizeof from; memset(&from, 0, sizeof from);
            ssize_t m = recvfrom(s, r, sizeof r, 0, (struct sockaddr *)&from, &fl);
            if (m < 8) break;
            if (r[0] == ICMP6_ECHO_REPLY && ((r[6] << 8) | r[7]) == seq) { ok = 1; break; }
        }
    }
    close(s);
    if (ok) printf("  recvfrom scope_id %u want %u\n", from.sin6_scope_id, want);
    CHECK("ll_recv_scope", ok && from.sin6_scope_id == want);
}

/* Ping our own link-local address through eth0: delivered locally, no wire. */
static void test_ping6_own_ll(void) {
    char line[128];
    if (!if_inet6_find("eth0", "fe80", line, sizeof line)) { CHECK("ping6_own_ll", 0); return; }
    int s = socket(AF_INET6, SOCK_DGRAM, IPPROTO_ICMPV6);
    if (s < 0) { CHECK("ping6_own_ll", 0); return; }
    tmo(s, 3);
    struct sockaddr_in6 a; memset(&a, 0, sizeof a);
    a.sin6_family = AF_INET6;
    for (int i = 0; i < 16; i++) sscanf(line + 2 * i, "%2hhx", &a.sin6_addr.s6_addr[i]);
    a.sin6_scope_id = if_nametoindex("eth0");
    int good = 0;
    for (int seq = 1; seq <= 4; seq++) {
        uint8_t q[64], r[256];
        size_t n = echo_req(q, 0x4325, seq);
        if (sendto(s, q, n, 0, (struct sockaddr *)&a, sizeof a) != (ssize_t)n) continue;
        int got = await_reply(s, seq, r, sizeof r);
        if (got == 24 && !memcmp(r + 8, q + 8, 16)) good++;
    }
    close(s);
    if (good != 4) printf("  ping own fe80: %d/4\n", good);
    CHECK("ping6_own_ll", good == 4);
}

static void test_ll_noscope(void) {
    struct sockaddr_in6 a = router_ll();
    a.sin6_port = htons(53);
    a.sin6_scope_id = 0;
    int s = socket(AF_INET6, SOCK_STREAM, 0); tmo(s, 2);
    int r = connect(s, (struct sockaddr *)&a, sizeof a);
    int e1 = errno; close(s);
    int u = socket(AF_INET6, SOCK_DGRAM, 0); tmo(u, 2);
    ssize_t w = sendto(u, "x", 1, 0, (struct sockaddr *)&a, sizeof a);
    int e2 = errno; close(u);
    // A scope id naming no interface is ENODEV on Linux.
    a.sin6_scope_id = 9999;
    int s2 = socket(AF_INET6, SOCK_STREAM, 0); tmo(s2, 2);
    int r2 = connect(s2, (struct sockaddr *)&a, sizeof a);
    int e3 = errno; close(s2);
    CHECK("ll_noscope", r == -1 && e1 == EINVAL && w == -1 && e2 == EINVAL && r2 == -1 && e3 == ENODEV);
}

static void test_ll_bind_scope(void) {
    char line[256];
    struct sockaddr_in6 a; memset(&a, 0, sizeof a);
    a.sin6_family = AF_INET6;
    int ok = if_inet6_find("eth0", "fe80", line, sizeof line);
    if (ok) {
        for (int i = 0; i < 16; i++) { unsigned v; sscanf(line + 2 * i, "%2x", &v); a.sin6_addr.s6_addr[i] = v; }
        int s = socket(AF_INET6, SOCK_DGRAM, 0);
        int r0 = bind(s, (struct sockaddr *)&a, sizeof a); int e0 = errno;   // no scope: EINVAL
        a.sin6_scope_id = if_nametoindex("eth0");
        int r1 = bind(s, (struct sockaddr *)&a, sizeof a);
        ok = r0 == -1 && e0 == EINVAL && r1 == 0;
        close(s);
    }
    CHECK("ll_bind_scope", ok);
}

static int set_disable(int v) {
    int fd = open("/proc/sys/net/ipv6/conf/eth0/disable_ipv6", O_WRONLY);
    if (fd < 0) return -1;
    char c[2] = { (char)('0' + v), '\n' };
    int r = write(fd, c, 2) == 2 ? 0 : -1;
    close(fd);
    return r;
}

static void test_disable_ipv6(void) {
    int a = set_disable(1) == 0;
    int gone = 0;
    for (int i = 0; a && i < 50 && !gone; i++) { gone = !if_inet6_find("eth0", "fe80", NULL, 0); if (!gone) usleep(100000); }
    int b = set_disable(0) == 0;
    int back = 0;
    for (int i = 0; b && i < 50 && !back; i++) { back = if_inet6_find("eth0", "fe80", NULL, 0); if (!back) usleep(100000); }
    CHECK("disable_ipv6", gone && back);
}

// ---- DAD of the NIC addresses (lane ip6dad): tentative -> permanent, accept_dad,
// dad_transmits, a duplicate -> DADFAILED, flags in /proc/net/if_inet6 ----
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <linux/if_addr.h>

static int conf_rw(const char *field, int set, int v) {
    char p[96]; snprintf(p, sizeof p, "/proc/sys/net/ipv6/conf/eth0/%s", field);
    int fd = open(p, set ? O_WRONLY : O_RDONLY);
    if (fd < 0) return -1;
    char b[16] = ""; int r;
    if (set) { int n = snprintf(b, sizeof b, "%d\n", v); r = write(fd, b, n) == n ? 0 : -1; }
    else { r = read(fd, b, sizeof b - 1) > 0 ? atoi(b) : -1; }
    close(fd);
    return r;
}

/* Flags (hex column 5) of the if_inet6 line of eth0 starting with `prefix`; -1 when absent. */
static int inet6_flags(const char *prefix) {
    char l[256];
    if (!if_inet6_find("eth0", prefix, l, sizeof l)) return -1;
    unsigned ix, pl, sc, fl;
    if (sscanf(l + 33, "%x %x %x %x", &ix, &pl, &sc, &fl) != 4) return -1;
    return (int)fl;
}

static void test_dad_sysctl(void) {
    int a = conf_rw("accept_dad", 0, 0), t = conf_rw("dad_transmits", 0, 0);
    int w = conf_rw("dad_transmits", 1, 3) == 0 && conf_rw("dad_transmits", 0, 0) == 3;
    conf_rw("dad_transmits", 1, 1);
    CHECK("dad_sysctl", a == 1 && t == 1 && w && conf_rw("dad_transmits", 0, 0) == 1);
}

static void test_dad_tentative_then_permanent(void) {
    conf_rw("dad_transmits", 1, 2);                  /* a 2 s probing window */
    int off = set_disable(1) == 0;
    for (int i = 0; off && i < 50 && inet6_flags("fe80") >= 0; i++) usleep(100000);
    int on = set_disable(0) == 0, first = -1;
    for (int i = 0; on && i < 400 && first < 0; i++) { first = inet6_flags("fe80"); if (first < 0) usleep(5000); }
    int last = first;
    for (int i = 0; first >= 0 && i < 100 && (last & 0x40); i++) { usleep(100000); last = inet6_flags("fe80"); }
    conf_rw("dad_transmits", 1, 1);
    printf("  first flags %x then %x\n", first, last);
    CHECK("dad_tentative_flag", first >= 0 && (first & 0x40) && !(first & 0x800));
    CHECK("dad_becomes_permanent", last == 0x80);
}

static void test_dad_off(void) {
    conf_rw("accept_dad", 1, 0);
    int off = set_disable(1) == 0;
    for (int i = 0; off && i < 50 && inet6_flags("fe80") >= 0; i++) usleep(100000);
    int on = set_disable(0) == 0, first = -1;
    for (int i = 0; on && i < 400 && first < 0; i++) { first = inet6_flags("fe80"); if (first < 0) usleep(5000); }
    conf_rw("accept_dad", 1, 1);
    CHECK("dad_accept_dad_0", first == 0x80);
}

/* RTM_NEWADDR / RTM_DELADDR of a v6 address on eth0. */
static int nl_addr6_if(const char *ifn, int add, const unsigned char *a, int plen, unsigned flags) {
    int s = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (s < 0) return -1;
    tmo(s, 3);
    struct { struct nlmsghdr h; struct ifaddrmsg m; struct rtattr la; unsigned char a[16]; struct rtattr fa; unsigned f; } q;
    memset(&q, 0, sizeof q);
    q.h.nlmsg_len = sizeof q; q.h.nlmsg_type = add ? RTM_NEWADDR : RTM_DELADDR;
    q.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | (add ? NLM_F_CREATE | NLM_F_EXCL : 0);
    q.m.ifa_family = AF_INET6; q.m.ifa_prefixlen = plen; q.m.ifa_index = if_nametoindex(ifn);
    q.la.rta_len = RTA_LENGTH(16); q.la.rta_type = IFA_LOCAL; memcpy(q.a, a, 16);
    q.fa.rta_len = RTA_LENGTH(4); q.fa.rta_type = IFA_FLAGS; q.f = flags;
    int r = -1;
    if (send(s, &q, sizeof q, 0) == (ssize_t)sizeof q) {
        char b[256];
        ssize_t n = recv(s, b, sizeof b, 0);
        if (n >= (ssize_t)sizeof(struct nlmsghdr) + 4) { struct nlmsgerr *e = NLMSG_DATA((struct nlmsghdr *)b); r = e->error; }
    }
    close(s);
    return r;
}
static int nl_addr6(int add, const unsigned char *a, int plen, unsigned flags) { return nl_addr6_if("eth0", add, a, plen, flags); }

/* The flags of eth0's 2001:db8:dad::<n> style line: `hex` is the 32 hex digits. */
static void test_dad_static_and_dup(void) {
    unsigned char fresh[16] = { 0x20, 0x01, 0x0d, 0xb8, 0x0d, 0xad, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x05 };
    const char *fh = "20010db80dad00000000000000000005";
    /* fresh address: tentative, then permanent */
    conf_rw("dad_transmits", 1, 2);
    int r = nl_addr6(1, fresh, 64, 0);
    int first = inet6_flags(fh), last = first;
    for (int i = 0; first >= 0 && i < 100 && (last & 0x40); i++) { usleep(100000); last = inet6_flags(fh); }
    CHECK("dad_static_tentative", r == 0 && first >= 0 && (first & 0x40));
    CHECK("dad_static_permanent", last == 0x80);
    nl_addr6(0, fresh, 64, 0);
    /* duplicate -> DADFAILED: slirp does not answer an NS for fec0::2, so it is not testable here (TODO.md) */
    /* IFA_F_NODAD skips it */
    r = nl_addr6(1, fresh, 64, 0x02);
    int fl = inet6_flags(fh);
    CHECK("dad_nodad", r == 0 && fl == 0x80);
    nl_addr6(0, fresh, 64, 0);
    conf_rw("dad_transmits", 1, 1);
}


// ---- Peers on the NIC's link (lane dhcpv6): the SIOCDEVPRIVATE test knob hands a frame to eth0's
// receive side, so the test plays the router, the DHCPv6 server and the node that owns a
// duplicate address. ----
static void put16(unsigned char *p, unsigned v) { p[0] = v >> 8; p[1] = v; }
static unsigned csum_v6(const unsigned char *src, const unsigned char *dst, int proto, const unsigned char *p, int n) {
    unsigned long sum = 0;
    for (int i = 0; i < 16; i += 2) sum += (src[i] << 8) | src[i + 1];
    for (int i = 0; i < 16; i += 2) sum += (dst[i] << 8) | dst[i + 1];
    sum += n + proto;
    for (int i = 0; i < n; i += 2) sum += (p[i] << 8) | (i + 1 < n ? p[i + 1] : 0);
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return ~sum & 0xffff;
}
/* One Ethernet+IPv6 frame (`proto` 58 = ICMPv6 with its checksum filled, 17 = UDP likewise) to eth0. */
static int nic_send6(const unsigned char *src, const unsigned char *dst, int proto, int hl, unsigned char *l4, int n, int mcast) {
    unsigned char mac[6], f[1600];
    if (!eth0_mac(mac)) return -1;
    memset(f, 0, 14);
    if (mcast) { f[0] = f[1] = 0x33; memcpy(f + 2, dst + 12, 4); } else memcpy(f, mac, 6);
    f[6] = 0x02; f[7] = 0xaa; f[8] = f[9] = f[10] = 0; f[11] = 0x77;
    f[12] = 0x86; f[13] = 0xdd;
    f[14] = 0x60; put16(f + 18, n); f[20] = proto; f[21] = hl;
    memcpy(f + 22, src, 16); memcpy(f + 38, dst, 16);
    int ck = proto == 58 ? 2 : 6;
    l4[ck] = l4[ck + 1] = 0;
    unsigned c = csum_v6(src, dst, proto, l4, n);
    if (proto == 17 && c == 0) c = 0xffff;
    put16(l4 + ck, c);
    memcpy(f + 54, l4, n);
    struct { unsigned idx, len; unsigned char f[1600]; } b;
    b.idx = if_nametoindex("eth0"); b.len = 54 + n; memcpy(b.f, f, 54 + n);
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    int r = s < 0 ? -1 : ioctl(s, 0x89f0, &b);
    if (s >= 0) close(s);
    return r;
}
static const unsigned char ALLNODES[16] = { 0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1 };
static const unsigned char FAKE_MAC[6] = { 0x02, 0xaa, 0, 0, 0, 0x77 };

/* An RA from the real router's link-local address (so its default route stays) with `flags`
 * (M 0x80, O 0x40) and, when `dns` is set, an RDNSS option of lifetime `life`. */
static int inject_ra(unsigned char flags, const unsigned char *dns, unsigned life) {
    struct sockaddr_in6 r = router_ll();
    unsigned char ra[64] = { 134, 0, 0, 0, 64, flags, 0x07, 0x08, 0, 0, 0, 0, 0, 0, 0, 0 };
    int n = 16;
    if (dns) {
        ra[n] = 25; ra[n + 1] = 3; ra[n + 2] = ra[n + 3] = 0;
        ra[n + 4] = life >> 24; ra[n + 5] = life >> 16; ra[n + 6] = life >> 8; ra[n + 7] = life;
        memcpy(ra + n + 8, dns, 16);
        n += 24;
    }
    return nic_send6(r.sin6_addr.s6_addr, ALLNODES, 58, 255, ra, n, 1);
}

static int resolv_has(const char *needle) {
    FILE *f = fopen("/etc/resolv.conf", "r");
    if (!f) return 0;
    char b[256]; int found = 0;
    while (!found && fgets(b, sizeof b, f)) found = strstr(b, needle) != NULL;
    fclose(f);
    return found;
}
static int resolv_wait(const char *needle, int want) {
    for (int i = 0; i < 50; i++) { if (resolv_has(needle) == want) return 1; usleep(100000); }
    return 0;
}


/* RDNSS (RFC 8106), self-contained: slirp only advertises a v6 resolver when the HOST has one in its
 * resolv.conf (libslirp copies it), so the test supplies its own: an RA with an RDNSS of ::1 (a
 * lifetime of 600 s), and a forked UDP responder on [::1]:53 standing in for the DNS server. */
static const unsigned char LOOP6[16] = { [15] = 1 };
static int resolv_lo_wait(int want) { return resolv_wait("nameserver ::1", want); }

static pid_t dns_responder(void) {
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 a; memset(&a, 0, sizeof a);
    a.sin6_family = AF_INET6; a.sin6_addr.s6_addr[15] = 1; a.sin6_port = htons(53);
    if (s < 0 || bind(s, (struct sockaddr *)&a, sizeof a) < 0) { if (s >= 0) close(s); return -1; }
    pid_t p = fork();
    if (p != 0) { close(s); return p; }
    alarm(30);
    for (;;) {
        unsigned char q[512]; struct sockaddr_in6 f; socklen_t fl = sizeof f;
        ssize_t n = recvfrom(s, q, sizeof q, 0, (struct sockaddr *)&f, &fl);
        if (n < 12) continue;
        q[2] |= 0x80;
        sendto(s, q, n, 0, (struct sockaddr *)&f, fl);
    }
}

static void test_rdnss_resolv(void) {
    int r = inject_ra(0, LOOP6, 600);
    int ok = r == 0 && resolv_lo_wait(1);
    if (!ok) printf("  inject_ra=%d, no 'nameserver ::1' in resolv.conf\n", r);
    CHECK("rdnss_resolv", ok);
}

/* The listed RDNSS server answers: a DNS query over v6 UDP gets a reply with our id. */
static void test_rdnss_query(void) {
    char a[128] = "";
    struct sockaddr_in6 d; memset(&d, 0, sizeof d);
    int ok = resolv_lo_wait(1) && resolv_v6(a, sizeof a) && inet_pton(AF_INET6, "::1", &d.sin6_addr) == 1;
    pid_t rp = ok ? dns_responder() : -1;
    int s = rp > 0 ? socket(AF_INET6, SOCK_DGRAM, 0) : -1;
    if (s < 0) { if (rp > 0) kill(rp, SIGKILL); CHECK("rdnss_query", 0); return; }
    d.sin6_family = AF_INET6; d.sin6_port = htons(53);
    tmo(s, 3);
    unsigned char q[] = {0x4b, 0x1d, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0,
                         7, 'e', 'x', 'a', 'm', 'p', 'l', 'e', 3, 'c', 'o', 'm', 0, 0, 1, 0, 1};
    unsigned char r[512];
    ok = 0;
    for (int i = 0; i < 3 && !ok; i++) {
        if (sendto(s, q, sizeof q, 0, (struct sockaddr *)&d, sizeof d) < 0) break;
        ssize_t n = recv(s, r, sizeof r, 0);
        ok = n >= 12 && r[0] == 0x4b && r[1] == 0x1d && (r[2] & 0x80);
    }
    close(s);
    kill(rp, SIGKILL); waitpid(rp, NULL, 0);
    inject_ra(0, LOOP6, 0);
    resolv_lo_wait(0);
    CHECK("rdnss_query", ok);
}

static void test_rdnss_linklocal_scope(void) {
    unsigned char d[16] = { 0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x53 };
    int r = inject_ra(0, d, 600);
    CHECK("rdnss_ll_scoped", r == 0 && resolv_wait("nameserver fe80::53%eth0", 1));
    r = inject_ra(0, d, 0);
    CHECK("rdnss_ll_withdrawn", r == 0 && resolv_wait("fe80::53", 0));
}

/* The state line of eth0 in /proc/net/dhcp6: mode, state, xid. */
static int d6_state(char *mode, char *st, char *xid, int *replies) {
    FILE *f = fopen("/proc/net/dhcp6", "r");
    if (!f) return 0;
    char b[512], ifn[32]; int tries, ok = 0;
    while (fgets(b, sizeof b, f)) {
        if (sscanf(b, "%31s %15s %15s %6s %d %d", ifn, mode, st, xid, &tries, replies) == 6 && !strcmp(ifn, "eth0")) { ok = 1; break; }
    }
    fclose(f);
    return ok;
}
static int d6_wait(const char *want_st) {
    char m[16], s[16], x[8]; int rep;
    for (int i = 0; i < 100; i++) {
        if (d6_state(m, s, x, &rep) && !strcmp(s, want_st)) return 1;
        usleep(100000);
    }
    return 0;
}
static int eth0_ll(unsigned char *a) {
    char l[256];
    if (!if_inet6_find("eth0", "fe80", l, sizeof l)) return 0;
    for (int i = 0; i < 16; i++) { unsigned v; sscanf(l + 2 * i, "%2x", &v); a[i] = v; }
    return 1;
}
/* A DHCPv6 message from the router to eth0: header, ClientID, ServerID, `extra` options. */
static int d6_send(int type, const char *xid, const unsigned char *extra, int en) {
    unsigned char mac[6], ll[16], msg[600], p[700];
    if (!eth0_mac(mac) || !eth0_ll(ll)) return -1;
    struct sockaddr_in6 r = router_ll();
    int n = 4;
    msg[0] = type;
    for (int i = 0; i < 3; i++) { unsigned v; sscanf(xid + 2 * i, "%2x", &v); msg[1 + i] = v; }
    unsigned char cid[14] = { 0, 1, 0, 10, 0, 3, 0, 1 }; memcpy(cid + 8, mac, 6);
    unsigned char sid[14] = { 0, 2, 0, 10, 0, 3, 0, 1 }; memcpy(sid + 8, FAKE_MAC, 6);
    memcpy(msg + n, cid, 14); n += 14;
    memcpy(msg + n, sid, 14); n += 14;
    memcpy(msg + n, extra, en); n += en;
    put16(p, 547); put16(p + 2, 546); put16(p + 4, 8 + n); p[6] = p[7] = 0;
    memcpy(p + 8, msg, n);
    return nic_send6(r.sin6_addr.s6_addr, ll, 17, 255, p, 8 + n, 0);
}
static const unsigned char DNS_OPTS[] = {
    0, 23, 0, 16, 0x20, 0x01, 0x0d, 0xb8, 0, 0xd6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x53,
    0, 24, 0, 10, 3, 'd', 'h', '6', 4, 't', 'e', 's', 't', 0 };

/* Back to a clean slate: accept_ra off drops the manager's state (DHCPv6, RDNSS, SLAAC); on
 * again re-solicits, and the slirp router's SLAAC address comes back. */
static void d6_reset(void) {
    char m[16], st[16], x[8]; int rep;
    conf_rw("accept_ra", 1, 0);
    for (int i = 0; i < 100; i++) { if (d6_state(m, st, x, &rep) && !strcmp(m, "none")) break; usleep(100000); }
    conf_rw("accept_ra", 1, 2);
    for (int i = 0; i < 200; i++) { if (inet6_flags("fec0") >= 0) break; usleep(100000); }
}

static void test_dhcp6_info(void) {
    char m[16] = "", s[16] = "", x[8] = ""; int rep = 0;
    int r = inject_ra(0x40, NULL, 0);
    int got = d6_wait("info-sent") || d6_wait("done");
    d6_state(m, s, x, &rep);
    if (!strcmp(s, "done")) printf("  note: a server on the wire (slirp) already answered the Information-Request\n");
    CHECK("dhcp6_info_request_sent", r == 0 && got && !strcmp(m, "info"));
    if (strcmp(s, "done")) {
        int rep0 = rep;
        r = d6_send(7, x, DNS_OPTS, sizeof DNS_OPTS);
        int dns = r == 0 && resolv_wait("nameserver 2001:db8:d6::53", 1) && resolv_has("dh6.test");
        d6_state(m, s, x, &rep);
        if (!dns && r == 0 && !strcmp(s, "done") && rep == rep0 + 1 && !resolv_has("nameserver 2001:db8:d6::53")) {
            /* The wire server (slirp) answered between our state read and our reply; the client keeps
             * only the first Reply of a transaction, so ours was (correctly) ignored. */
            printf("  note: a server on the wire answered the Information-Request just before our reply\n");
        } else {
            CHECK("dhcp6_info_reply_dns", dns);
            CHECK("dhcp6_info_done", !strcmp(s, "done"));
        }
    }
    d6_reset();
    CHECK("dhcp6_info_cleared", resolv_wait("2001:db8:d6::53", 0));
}

/* A field of the "# eth0 ..." detail line of /proc/net/dhcp6 (t1, t2, refresh, solicits, renews, rebinds,
 * inforeqs); LONG_MIN when absent. */
static long d6_field(const char *key) {
    FILE *f = fopen("/proc/net/dhcp6", "r");
    if (!f) return LONG_MIN;
    char b[512], pat[32]; long v = LONG_MIN;
    snprintf(pat, sizeof pat, " %s=", key);
    while (fgets(b, sizeof b, f)) {
        if (strncmp(b, "# eth0 ", 7)) continue;
        char *p = strstr(b, pat);
        if (p) v = strtol(p + strlen(pat), NULL, 10);
        break;
    }
    fclose(f);
    return v;
}
static void test_dhcp6_refresh(void) {
    char m[16] = "", s[16] = "", x[8] = ""; int rep = 0;
    unsigned char o[sizeof DNS_OPTS + 8];
    memcpy(o, DNS_OPTS, sizeof DNS_OPTS);
    unsigned char rt[8] = { 0, 32, 0, 4, 0, 0, 0, 30 };    /* INFORMATION_REFRESH_TIME 30 s: floored to 600 */
    memcpy(o + sizeof DNS_OPTS, rt, 8);
    int r = inject_ra(0x40, NULL, 0);
    int got = d6_wait("info-sent") || d6_wait("done");
    d6_state(m, s, x, &rep);
    if (r == 0 && got && !strcmp(s, "done")) {
        /* The wire server (slirp) answered and sends no refresh option: the default applies. */
        long t = d6_field("refresh");
        CHECK("dhcp6_refresh_default_86400", t >= 86300 && t <= 86400);
        d6_reset();
        return;
    }
    if (r != 0 || !got) { CHECK("dhcp6_refresh_min_600", 0); d6_reset(); return; }
    r = d6_send(7, x, o, sizeof o);
    int ok = r == 0 && d6_wait("done");
    long t = d6_field("refresh");
    CHECK("dhcp6_refresh_min_600", ok && t >= 590 && t <= 600);
    d6_reset();
    r = inject_ra(0x40, NULL, 0);
    got = d6_wait("info-sent") || d6_wait("done");
    d6_state(m, s, x, &rep);
    if (r != 0 || !got || !strcmp(s, "done")) { d6_reset(); return; }
    r = d6_send(7, x, DNS_OPTS, sizeof DNS_OPTS);
    ok = r == 0 && d6_wait("done");
    t = d6_field("refresh");
    CHECK("dhcp6_refresh_default_86400", ok && t >= 86300 && t <= 86400);
    d6_reset();
}

/* T1/T2 with short timers: Renew goes out at T1 (same address, no Solicit), Rebind at T2 when the
 * Renew stays unanswered, the address is kept while valid, a Reply to the Renew closes it. */
static void test_dhcp6_renew(void) {
    char m[16] = "", s[16] = "", x[8] = "", x2[8] = "", x3[8] = ""; int rep = 0;
    unsigned char ia[] = { 0, 3, 0, 40, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 5,   /* iaid, T1 = 2 s, T2 = 5 s */
        0, 5, 0, 24, 0x20, 0x01, 0x0d, 0xb8, 0, 0xd6, 0, 0, 0, 0, 0, 0, 0, 0, 0x12, 0x35,
        0, 0, 0x0e, 0x10, 0, 0, 0x1c, 0x20 };
    const char *addr = "20010db800d600000000000000001235";
    int r = inject_ra(0x80, NULL, 0);
    int ok = r == 0 && d6_wait("solicit") && d6_state(m, s, x, &rep);
    if (ok) { r = d6_send(2, x, ia, sizeof ia); ok = r == 0 && d6_wait("request"); }
    d6_state(m, s, x2, &rep);
    unsigned char rp[sizeof ia + sizeof DNS_OPTS]; memcpy(rp, ia, sizeof ia); memcpy(rp + sizeof ia, DNS_OPTS, sizeof DNS_OPTS);
    if (ok) r = d6_send(7, x2, rp, sizeof rp);
    for (int i = 0; i < 50 && ok && inet6_flags(addr) != 0x80; i++) usleep(100000);
    CHECK("dhcp6_renew_bound", ok && r == 0 && inet6_flags(addr) == 0x80);
    long t1 = d6_field("t1"), t2 = d6_field("t2");
    CHECK("dhcp6_renew_timers", t1 >= 0 && t1 <= 2 && t2 >= 3 && t2 <= 5);
    int renew = d6_wait("renew");
    d6_state(m, s, x3, &rep);
    CHECK("dhcp6_renew_sent", renew && d6_field("renews") >= 1 && d6_field("solicits") == 1 && strcmp(x2, x3) != 0 && inet6_flags(addr) == 0x80);
    int reb = d6_wait("rebind");                       /* the Renew went unanswered: T2 */
    CHECK("dhcp6_rebind_sent", reb && d6_field("rebinds") >= 1 && d6_field("solicits") == 1 && inet6_flags(addr) == 0x80);
    d6_state(m, s, x3, &rep);
    r = d6_send(7, x3, rp, sizeof rp);                 /* a server answers the Rebind */
    int done = r == 0 && d6_wait("done");
    CHECK("dhcp6_renew_done_same_addr", done && inet6_flags(addr) == 0x80 && d6_field("solicits") == 1 && d6_field("t1") >= 0);
    d6_reset();
    CHECK("dhcp6_renew_released", inet6_flags(addr) < 0);
}

static void test_dhcp6_stateful(void) {
    char m[16] = "", s[16] = "", x[8] = ""; int rep = 0;
    unsigned char ia[] = { 0, 3, 0, 40, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 5, 0, 24, 0x20, 0x01, 0x0d, 0xb8, 0, 0xd6, 0, 0, 0, 0, 0, 0, 0, 0, 0x12, 0x34,
        0, 0, 0x0e, 0x10, 0, 0, 0x1c, 0x20 };
    int r = inject_ra(0x80, NULL, 0);
    CHECK("dhcp6_solicit_sent", r == 0 && d6_wait("solicit") && d6_state(m, s, x, &rep) && !strcmp(m, "stateful"));
    r = d6_send(2, x, ia, sizeof ia);                 /* Advertise */
    CHECK("dhcp6_request_sent", r == 0 && d6_wait("request"));
    char x2[8] = "";
    d6_state(m, s, x2, &rep);
    CHECK("dhcp6_request_new_xid", strcmp(x, x2) != 0);
    unsigned char rp[sizeof ia + sizeof DNS_OPTS]; memcpy(rp, ia, sizeof ia); memcpy(rp + sizeof ia, DNS_OPTS, sizeof DNS_OPTS);
    r = d6_send(7, x2, rp, sizeof rp);                /* Reply */
    int ok = 0;
    for (int i = 0; i < 50 && !ok; i++) { ok = inet6_flags("20010db800d600000000000000001234") == 0x80; if (!ok) usleep(100000); }
    CHECK("dhcp6_address_bound", r == 0 && ok);
    char l[256] = "";
    if_inet6_find("eth0", "20010db800d600000000000000001234", l, sizeof l);
    unsigned ix = 0, pl = 0; sscanf(l + 33, "%x %x", &ix, &pl);
    CHECK("dhcp6_address_prefix128", pl == 0x80);
    CHECK("dhcp6_stateful_dns", resolv_wait("nameserver 2001:db8:d6::53", 1));
    d6_reset();
    CHECK("dhcp6_address_released", inet6_flags("20010db800d600000000000000001234") < 0 && resolv_wait("2001:db8:d6::53", 0));
}

/* Addresses of eth0 through RTM_GETADDR: the flags (IFA_FLAGS) of `a`, -1 when absent. */
static long nl_get_flags(const unsigned char *a) {
    int s = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (s < 0) return -1;
    tmo(s, 3);
    struct { struct nlmsghdr h; struct ifaddrmsg m; } q;
    memset(&q, 0, sizeof q);
    q.h.nlmsg_len = sizeof q; q.h.nlmsg_type = RTM_GETADDR; q.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    q.m.ifa_family = AF_INET6;
    long res = -1;
    if (send(s, &q, sizeof q, 0) == (ssize_t)sizeof q) {
        char b[8192];
        for (int done = 0; !done;) {
            ssize_t n = recv(s, b, sizeof b, 0);
            if (n <= 0) break;
            for (struct nlmsghdr *h = (struct nlmsghdr *)b; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
                if (h->nlmsg_type == NLMSG_DONE) { done = 1; break; }
                if (h->nlmsg_type != RTM_NEWADDR) continue;
                struct ifaddrmsg *m = NLMSG_DATA(h);
                int len = IFA_PAYLOAD(h); unsigned fl = m->ifa_flags; int match = 0;
                for (struct rtattr *ra = IFA_RTA(m); RTA_OK(ra, len); ra = RTA_NEXT(ra, len)) {
                    if (ra->rta_type == IFA_ADDRESS && !memcmp(RTA_DATA(ra), a, 16)) match = 1;
                    if (ra->rta_type == IFA_FLAGS) fl = *(unsigned *)RTA_DATA(ra);
                }
                if (match) res = fl;
            }
        }
    }
    close(s);
    return res;
}

/* A peer claims a tentative NIC address: a NA for it, or a NS from `::` probing for it too,
 * makes it DADFAILED (flags 0x40|0x800), in /proc/net/if_inet6 and in rtnetlink. */
static void test_dad_failed_peer(void) {
    unsigned char a1[16] = { 0x20, 0x01, 0x0d, 0xb8, 0x0d, 0xad, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x11 };
    unsigned char a2[16] = { 0x20, 0x01, 0x0d, 0xb8, 0x0d, 0xad, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x22 };
    const char *h1 = "20010db80dad00000000000000000011", *h2 = "20010db80dad00000000000000000022";
    conf_rw("dad_transmits", 1, 3);
    int r1 = nl_addr6(1, a1, 64, 0), r2 = nl_addr6(1, a2, 64, 0);
    CHECK("dad_peer_tentative", r1 == 0 && r2 == 0 && (inet6_flags(h1) & 0x40) && !(inet6_flags(h1) & 0x800));
    unsigned char na[32] = { 136, 0, 0, 0, 0x20, 0, 0, 0 };           /* NA: override */
    memcpy(na + 8, a1, 16); na[24] = 2; na[25] = 1; memcpy(na + 26, FAKE_MAC, 6);
    unsigned char sn1[16] = { 0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x77 };
    int r = nic_send6(sn1, ALLNODES, 58, 255, na, 32, 1);
    unsigned char ns[24] = { 135, 0, 0, 0, 0, 0, 0, 0 };              /* NS from :: (another node's DAD probe) */
    memcpy(ns + 8, a2, 16);
    unsigned char unspec[16] = { 0 }, snm[16] = { 0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0xff, 0, 0, 0x22 };
    r |= nic_send6(unspec, snm, 58, 255, ns, 24, 1);
    int ok1 = 0, ok2 = 0;
    for (int i = 0; i < 30 && !(ok1 && ok2); i++) {
        ok1 = (inet6_flags(h1) & 0x840) == 0x840; ok2 = (inet6_flags(h2) & 0x840) == 0x840;
        if (!(ok1 && ok2)) usleep(100000);
    }
    CHECK("dad_peer_na_failed", r == 0 && ok1);
    CHECK("dad_peer_ns_failed", ok2);
    long n1 = nl_get_flags(a1), n2 = nl_get_flags(a2);
    CHECK("dad_peer_rtnetlink", n1 >= 0 && (n1 & 0x800) && n2 >= 0 && (n2 & 0x800));
    sleep(4);                                          /* a failed address stays failed, never permanent */
    CHECK("dad_peer_stays_failed", (inet6_flags(h1) & 0x800) && (inet6_flags(h2) & 0x800));
    nl_addr6(0, a1, 64, 0); nl_addr6(0, a2, 64, 0);
    CHECK("dad_peer_removed", inet6_flags(h1) < 0 && inet6_flags(h2) < 0);
    conf_rw("dad_transmits", 1, 1);
}


/* A vif of a non-initial netns: its address is listed in THAT netns's /proc/net/if_inet6 (not the
 * NICs of the initial one), and not in the initial netns's. */
#include <linux/if_tun.h>
static int vif_ns_body(void) {
    unsigned char a[16] = { 0x20, 0x01, 0x0d, 0xb8, 0x0d, 0x66, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01 };
    if (unshare(CLONE_NEWNET) != 0) return 2;
    int t = open("/dev/net/tun", O_RDWR);
    if (t < 0) return 3;
    struct ifreq q; memset(&q, 0, sizeof q); strcpy(q.ifr_name, "dh6t0"); q.ifr_flags = IFF_TAP | IFF_NO_PI;
    if (ioctl(t, TUNSETIFF, &q) != 0) return 4;
    if (nl_addr6_if("dh6t0", 1, a, 64, 0x02) != 0) return 5;
    char l[256];
    int found = 0;
    for (int i = 0; i < 30 && !found; i++) { found = if_inet6_find("dh6t0", "20010db80d660000", l, sizeof l); if (!found) usleep(100000); }
    if (!found) return 6;
    FILE *f = fopen("/proc/net/if_inet6", "r");
    char b[256]; int nic = 0;
    while (f && fgets(b, sizeof b, f)) if (strstr(b, " eth0")) nic = 1;
    if (f) fclose(f);
    return nic ? 7 : 0;
}
static void test_vif_ns_if_inet6(void) {
    pid_t k = fork();
    if (k == 0) _exit(vif_ns_body());
    int st = 0; waitpid(k, &st, 0);
    errno = WIFEXITED(st) ? WEXITSTATUS(st) : -1;
    CHECK("vif_ns_if_inet6", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    CHECK("vif_ns_not_in_initial", !if_inet6_find("dh6t0", "2001", NULL, 0));
}

/* Host-wire helpers (v6injecttest.py): `mtu <addr>` prints the IPV6_MTU of a connected UDP socket
 * to <addr>; `bindchk <addr>` prints whether bind() to the local address <addr> succeeds. */
static int wire_mtu(const char *a) {
    struct sockaddr_in6 d; memset(&d, 0, sizeof d); d.sin6_family = AF_INET6; d.sin6_port = htons(9);
    if (inet_pton(AF_INET6, a, &d.sin6_addr) != 1) return 2;
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    if (s < 0 || connect(s, (struct sockaddr *)&d, sizeof d) != 0) { printf("mtu connect errno %d\n", errno); return 1; }
    int m = 0; socklen_t l = sizeof m;
    if (getsockopt(s, IPPROTO_IPV6, IPV6_MTU, &m, &l) != 0) { printf("mtu getsockopt errno %d\n", errno); return 1; }
    printf("ip6mtu %d\n", m);
    return 0;
}
static int wire_bindchk(const char *a) {
    struct sockaddr_in6 d; memset(&d, 0, sizeof d); d.sin6_family = AF_INET6;
    if (inet_pton(AF_INET6, a, &d.sin6_addr) != 1) return 2;
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    int r = bind(s, (struct sockaddr *)&d, sizeof d);
    printf("ip6bind %s\n", r == 0 ? "ok" : (errno == EADDRNOTAVAIL ? "EADDRNOTAVAIL" : "other"));
    return 0;
}

int main(int argc, char **argv) {
    if (argc >= 3 && !strcmp(argv[1], "mtu")) return wire_mtu(argv[2]);
    if (argc >= 3 && !strcmp(argv[1], "bindchk")) return wire_bindchk(argv[2]);
    if (argc >= 4 && !strcmp(argv[1], "banner")) return banner(argv[2], atoi(argv[3]));
    test_socket6();
    test_bad_len6();
    test_unreach6();
    test_tcp6_lo_echo();
    test_udp6_lo();
    test_v4_listener_no_v6();
    test_bind_family_aware();
    test_v6only_default();
    test_dual_accept_v4();
    test_v6only_coexist();
    test_dual_conflict();
    test_v6only_after_bind();
    test_mapped_connect_v6only();
    test_udp_dual();
    test_netns_v6_tcp();
    test_netns_udp6();
    test_ping6_dgram_lo();
    test_ping6_raw_lo();
    test_hops0();
    test_ping6_mapped_einval();
    test_icmp6_cmsgs();
    test_raw6_eperm();
    test_icmp6_filter();
    test_pktinfo6();
    test_opts6();
    test_pton_ntop6();
    test_gai();
    test_gni6();
    test_localhost_fallback();
    test_ll_present();
    test_ping6_ll_router();
    test_slaac_addr();
    test_ping6_global();
    test_ping6_own_global();
    test_ll_noscope();
    test_ping6_own_ll();
    test_ll_recv_scope();
    test_ll_bind_scope();
    test_disable_ipv6();
    test_dad_sysctl();
    test_dad_tentative_then_permanent();
    test_dad_off();
    test_dad_static_and_dup();
    test_dad_failed_peer();
    test_rdnss_resolv();
    test_rdnss_query();
    test_rdnss_linklocal_scope();
    test_dhcp6_info();
    test_dhcp6_stateful();
    test_dhcp6_refresh();
    test_dhcp6_renew();
    test_vif_ns_if_inet6();
    printf("ip6test: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
