// ip6test: AF_INET6 on the loopback (lane ip6lo, L1 of design-ipv6).
// Builds for the host too (cc ip6test.c) to confirm the expectations on Linux.
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
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
    CHECK("unreach6", r < 0 && (errno == ENETUNREACH || errno == ENETDOWN));
    close(s);
}

static void test_bad_len6(void) {
    int s = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 a = lo6(0);
    int r = bind(s, (struct sockaddr *)&a, 16);
    CHECK("bad_len6", r < 0 && errno == EINVAL);
    close(s);
}

int main(void) {
    test_socket6();
    test_bad_len6();
    test_unreach6();
    test_tcp6_lo_echo();
    test_udp6_lo();
    test_v4_listener_no_v6();
    test_bind_family_aware();
    printf("ip6test: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
