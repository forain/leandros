/*
 * nettest — IPv4 networking basics through the net server, with the results
 * Linux gives. Like privtest, the same file builds for the host
 * (cc nettest.c) and for LeandrOS (zig cc -target <arch>-linux-musl -static).
 *
 *   nettest [-g GATEWAY] [-d DNS_SERVER] [-n NAME] [-t IP:PORT]
 *
 *   -g  ICMP echo target (default: the default route's gateway from
 *       /proc/net/route, the way `route -n` finds it — 10.0.2.2 on QEMU
 *       user-net, 192.168.105.1 on a Mac running socket_vmnet; 10.0.2.2 if
 *       the file has no default route)
 *   -d  DNS server for the raw UDP and TCP queries (default: the first
 *       nameserver in /etc/resolv.conf)
 *   -n  name to resolve (default example.com)
 *   -t  optional HTTP server for a TCP GET (e.g. a host `python3 -m
 *       http.server`); the case is SKIPped without it
 *
 * Cases:
 *   icmp_raw / icmp_dgram   4 echo requests to the gateway over SOCK_RAW (root
 *                           only; EPERM for others) and over the unprivileged
 *                           SOCK_DGRAM ping socket, each reply awaited with
 *                           poll(); 4/4 replies required
 *   icmp_nonblock           MSG_DONTWAIT recvfrom on an idle ICMP socket is
 *                           EAGAIN at once
 *   icmp_poll_timeout       poll() for a reply that never comes (TEST-NET-1)
 *                           returns 0 after its timeout, not early, not never
 *   icmp_recv_eintr         a blocking recvfrom with nothing to read is ended
 *                           by SIGALRM with EINTR (handler without SA_RESTART)
 *   udp_dns_raw             hand-built DNS A query over UDP, answer matched by
 *                           id with QR set and RCODE 0
 *   udp_getaddrinfo         libc resolver (musl: UDP via /etc/resolv.conf)
 *   tcp_dns                 the same query over TCP port 53 (2-byte length)
 *   tcp_http                GET / from -t, expects an "HTTP/1." status line
 *   proc_net_route          /proc/net/route has an UP|GATEWAY default route
 *   proc_net_dev            the default route's interface is in /proc/net/dev
 *                           and its tx_packets grew across the ICMP cases
 *   proc_net_tcp            a 127.0.0.1 listener (st 0A) and both ends of a
 *                           connection to it (st 01) appear with our euid
 *   proc_net_udp            a bound UDP socket appears (st 07)
 *   proc_net_unix           a listening AF_UNIX path appears with __SO_ACCEPTCON
 *
 * Output: "<name>: PASS|FAIL ...|SKIP ..." per case and a summary
 * "nettest: N passed, M failed, K skipped". Exit status = failures (capped).
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static int npass, nfail, nskip;

static void pass(const char *n) { printf("%s: PASS\n", n); npass++; fflush(stdout); }
static void skip(const char *n, const char *why) { printf("%s: SKIP %s\n", n, why); nskip++; fflush(stdout); }
static void fail(const char *n, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
#include <stdarg.h>
static void fail(const char *n, const char *fmt, ...) {
    va_list ap;
    printf("%s: FAIL ", n);
    va_start(ap, fmt); vprintf(fmt, ap); va_end(ap);
    printf("\n"); nfail++; fflush(stdout);
}

static long long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static uint16_t csum(const uint8_t *d, size_t n) {
    uint32_t s = 0;
    for (size_t i = 0; i + 1 < n; i += 2) s += (uint32_t)(d[i] << 8 | d[i + 1]);
    if (n & 1) s += (uint32_t)d[n - 1] << 8;
    while (s >> 16) s = (s & 0xffff) + (s >> 16);
    return (uint16_t)~s;
}

static struct sockaddr_in sin4(const char *ip, uint16_t port) {
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    inet_pton(AF_INET, ip, &a.sin_addr);
    return a;
}

/* Echo request with ident/seq; returns bytes sent or -1. */
static ssize_t send_echo(int fd, const struct sockaddr_in *to, uint16_t ident, uint16_t seq) {
    uint8_t p[40];
    memset(p, 0xa5, sizeof p);
    p[0] = 8; p[1] = 0; p[2] = p[3] = 0;
    p[4] = ident >> 8; p[5] = ident; p[6] = seq >> 8; p[7] = seq;
    uint16_t c = csum(p, sizeof p);
    p[2] = c >> 8; p[3] = c;
    return sendto(fd, p, sizeof p, 0, (const struct sockaddr *)to, sizeof *to);
}

/* Wait (poll) up to ms for an echo reply with this seq. Linux's SOCK_RAW
 * hands back the IP header too; skip it when present (version nibble 4). */
static int wait_reply(int fd, uint16_t seq, int ms) {
    long long end = now_ms() + ms;
    for (;;) {
        long long left = end - now_ms();
        if (left <= 0) return 0;
        struct pollfd pfd = { fd, POLLIN, 0 };
        if (poll(&pfd, 1, (int)left) <= 0) continue;
        uint8_t b[256];
        ssize_t n = recvfrom(fd, b, sizeof b, MSG_DONTWAIT, NULL, NULL);
        if (n < 8) continue;
        uint8_t *ic = b;
        if ((b[0] >> 4) == 4 && n >= 28) { ic = b + (b[0] & 15) * 4; n -= (b[0] & 15) * 4; }
        if (ic[0] == 0 && (uint16_t)(ic[6] << 8 | ic[7]) == seq) return 1;
    }
}

static void icmp_echo_case(const char *name, int type, const char *gw) {
    int fd = socket(AF_INET, type, IPPROTO_ICMP);
    if (fd < 0) {
        if (type == SOCK_RAW && errno == EPERM && geteuid() != 0) { skip(name, "(not root: EPERM as on Linux)"); return; }
        fail(name, "socket: %s", strerror(errno)); return;
    }
    struct sockaddr_in to = sin4(gw, 0);
    uint16_t ident = (uint16_t)(getpid() ^ (type << 8));
    int got = 0;
    long long worst = 0;
    for (uint16_t seq = 0; seq < 4; seq++) {
        long long t0 = now_ms();
        if (send_echo(fd, &to, ident, seq) < 0) { fail(name, "sendto: %s", strerror(errno)); close(fd); return; }
        if (wait_reply(fd, seq, 2000)) { got++; if (now_ms() - t0 > worst) worst = now_ms() - t0; }
    }
    close(fd);
    if (got == 4) { printf("%s: 4/4 replies from %s, worst rtt %lld ms\n", name, gw, worst); pass(name); }
    else fail(name, "%d/4 replies from %s", got, gw);
}

static void on_alarm(int s) { (void)s; }

static void icmp_timeout_cases(void) {
    int fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    if (fd < 0) { fail("icmp_nonblock", "socket: %s", strerror(errno)); return; }
    /* TEST-NET-1 (RFC 5737): routed via the gateway, never answered. */
    struct sockaddr_in to = sin4("192.0.2.1", 0);
    if (send_echo(fd, &to, (uint16_t)getpid(), 7) < 0) { fail("icmp_nonblock", "sendto: %s", strerror(errno)); close(fd); return; }

    uint8_t b[128];
    long long t0 = now_ms();
    ssize_t n = recvfrom(fd, b, sizeof b, MSG_DONTWAIT, NULL, NULL);
    long long dt = now_ms() - t0;
    if (n < 0 && errno == EAGAIN && dt < 200) pass("icmp_nonblock");
    else fail("icmp_nonblock", "n=%zd errno=%s after %lld ms (want EAGAIN at once)", n, strerror(errno), dt);

    struct pollfd pfd = { fd, POLLIN, 0 };
    t0 = now_ms();
    int r = poll(&pfd, 1, 700);
    dt = now_ms() - t0;
    if (r == 0 && dt >= 650 && dt < 2000) pass("icmp_poll_timeout");
    else fail("icmp_poll_timeout", "poll=%d revents=%#x after %lld ms (want 0 after ~700)", r, pfd.revents, dt);

    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm; /* no SA_RESTART: the blocked call must fail EINTR */
    sigaction(SIGALRM, &sa, NULL);
    struct itimerval it = { {0, 0}, {0, 500000} };
    setitimer(ITIMER_REAL, &it, NULL);
    t0 = now_ms();
    n = recvfrom(fd, b, sizeof b, 0, NULL, NULL);
    int e = errno;
    dt = now_ms() - t0;
    if (n < 0 && e == EINTR && dt >= 400 && dt < 3000) pass("icmp_recv_eintr");
    else fail("icmp_recv_eintr", "n=%zd errno=%s after %lld ms (want EINTR after ~500)", n, strerror(e), dt);
    signal(SIGALRM, SIG_DFL);
    close(fd);
}

/* Build a DNS A query for name; returns its length. */
static int dns_query(uint8_t *q, uint16_t id, const char *name) {
    int n = 0;
    q[n++] = id >> 8; q[n++] = id; q[n++] = 0x01; q[n++] = 0x00; /* RD */
    q[n++] = 0; q[n++] = 1; memset(q + n, 0, 6); n += 6;           /* QD=1 */
    const char *p = name;
    while (*p) {
        const char *dot = strchr(p, '.');
        int l = dot ? (int)(dot - p) : (int)strlen(p);
        q[n++] = (uint8_t)l; memcpy(q + n, p, l); n += l;
        p += l; if (*p == '.') p++;
    }
    q[n++] = 0; q[n++] = 0; q[n++] = 1; q[n++] = 0; q[n++] = 1;    /* A, IN */
    return n;
}

static int dns_ok(const uint8_t *r, ssize_t n, uint16_t id, int *ancount) {
    if (n < 12 || (uint16_t)(r[0] << 8 | r[1]) != id || !(r[2] & 0x80)) return 0;
    *ancount = r[6] << 8 | r[7];
    return (r[3] & 0x0f) == 0;
}

static void udp_dns_raw(const char *dns, const char *name) {
    const char *N = "udp_dns_raw";
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) { fail(N, "socket: %s", strerror(errno)); return; }
    struct sockaddr_in to = sin4(dns, 53);
    uint8_t q[300], r[1500];
    uint16_t id = (uint16_t)(getpid() * 31 + 7);
    int ql = dns_query(q, id, name);
    for (int attempt = 0; attempt < 3; attempt++) {
        if (sendto(fd, q, ql, 0, (struct sockaddr *)&to, sizeof to) != ql) { fail(N, "sendto: %s", strerror(errno)); close(fd); return; }
        long long end = now_ms() + 2000;
        while (now_ms() < end) {
            struct pollfd pfd = { fd, POLLIN, 0 };
            if (poll(&pfd, 1, (int)(end - now_ms())) <= 0) continue;
            struct sockaddr_in from; socklen_t fl = sizeof from;
            ssize_t n = recvfrom(fd, r, sizeof r, MSG_DONTWAIT, (struct sockaddr *)&from, &fl);
            int an = 0;
            if (dns_ok(r, n, id, &an)) {
                printf("%s: %s via %s: %d answers, %zd bytes\n", N, name, dns, an, n);
                if (an > 0) pass(N); else fail(N, "no answers");
                close(fd); return;
            }
        }
    }
    fail(N, "no reply from %s:53 in 3 x 2 s", dns);
    close(fd);
}

static void udp_getaddrinfo(const char *name) {
    const char *N = "udp_getaddrinfo";
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    long long t0 = now_ms();
    int r = getaddrinfo(name, "80", &hints, &res);
    if (r != 0) { fail(N, "getaddrinfo(%s): %s", name, gai_strerror(r)); return; }
    char ip[INET_ADDRSTRLEN] = "?";
    inet_ntop(AF_INET, &((struct sockaddr_in *)res->ai_addr)->sin_addr, ip, sizeof ip);
    printf("%s: %s -> %s in %lld ms\n", N, name, ip, now_ms() - t0);
    freeaddrinfo(res);
    pass(N);
}

/* connect with a deadline: nonblocking connect + poll(POLLOUT) + SO_ERROR. */
static int tcp_connect(const char *ip, uint16_t port, int ms, char *err, size_t errn) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) { snprintf(err, errn, "socket: %s", strerror(errno)); return -1; }
    fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK);
    struct sockaddr_in to = sin4(ip, port);
    if (connect(fd, (struct sockaddr *)&to, sizeof to) < 0 && errno != EINPROGRESS) {
        snprintf(err, errn, "connect: %s", strerror(errno)); close(fd); return -1;
    }
    struct pollfd pfd = { fd, POLLOUT, 0 };
    if (poll(&pfd, 1, ms) <= 0) { snprintf(err, errn, "connect timed out after %d ms", ms); close(fd); return -1; }
    int so = 0; socklen_t sl = sizeof so;
    getsockopt(fd, SOL_SOCKET, SO_ERROR, &so, &sl);
    if (so) { snprintf(err, errn, "connect: %s", strerror(so)); close(fd); return -1; }
    return fd;
}

/* Read until `want` bytes or EOF or deadline. */
static ssize_t read_some(int fd, uint8_t *b, size_t want, int ms) {
    size_t got = 0;
    long long end = now_ms() + ms;
    while (got < want && now_ms() < end) {
        struct pollfd pfd = { fd, POLLIN, 0 };
        if (poll(&pfd, 1, (int)(end - now_ms())) <= 0) continue;
        ssize_t n = recv(fd, b + got, want - got, MSG_DONTWAIT);
        if (n == 0) break;
        if (n < 0) { if (errno == EAGAIN || errno == EINTR) continue; return got ? (ssize_t)got : -1; }
        got += n;
    }
    return got;
}

static void tcp_dns(const char *dns, const char *name) {
    const char *N = "tcp_dns";
    char err[128];
    int fd = tcp_connect(dns, 53, 5000, err, sizeof err);
    if (fd < 0) { fail(N, "%s:53 %s", dns, err); return; }
    uint8_t q[302], r[2048];
    uint16_t id = (uint16_t)(getpid() * 17 + 3);
    int ql = dns_query(q + 2, id, name);
    q[0] = ql >> 8; q[1] = ql;
    if (send(fd, q, ql + 2, 0) != ql + 2) { fail(N, "send: %s", strerror(errno)); close(fd); return; }
    ssize_t n = read_some(fd, r, 2, 5000);
    int an = 0;
    if (n == 2) {
        int len = r[0] << 8 | r[1];
        if (len > (int)sizeof r) len = sizeof r;
        n = read_some(fd, r, len, 5000);
        if (n == len && dns_ok(r, n, id, &an) && an > 0) {
            printf("%s: %s via %s:53/tcp: %d answers\n", N, name, dns, an);
            pass(N); close(fd); return;
        }
    }
    fail(N, "bad/no answer (n=%zd an=%d)", n, an);
    close(fd);
}

static void tcp_http(const char *hostport) {
    const char *N = "tcp_http";
    if (!hostport) { skip(N, "(no -t IP:PORT)"); return; }
    char ip[64]; int port = 80;
    snprintf(ip, sizeof ip, "%s", hostport);
    char *c = strchr(ip, ':');
    if (c) { *c = 0; port = atoi(c + 1); }
    char err[128];
    int fd = tcp_connect(ip, (uint16_t)port, 5000, err, sizeof err);
    if (fd < 0) { fail(N, "%s:%d %s", ip, port, err); return; }
    char req[160];
    int rl = snprintf(req, sizeof req, "GET / HTTP/1.0\r\nHost: %s\r\nConnection: close\r\n\r\n", ip);
    if (send(fd, req, rl, 0) != rl) { fail(N, "send: %s", strerror(errno)); close(fd); return; }
    /* Drain to EOF: proves FIN delivery as well as data. */
    static uint8_t body[1 << 20];
    long long t0 = now_ms();
    ssize_t total = read_some(fd, body, sizeof body, 10000);
    close(fd);
    if (total >= 12 && memcmp(body, "HTTP/1.", 7) == 0) {
        char line[64]; int i = 0;
        while (i < total && i < 63 && body[i] != '\r' && body[i] != '\n') { line[i] = body[i]; i++; }
        line[i] = 0;
        printf("%s: %s:%d \"%s\", %zd bytes in %lld ms\n", N, ip, port, line, total, now_ms() - t0);
        pass(N);
    } else fail(N, "no HTTP status line (%zd bytes)", total);
}


/* ---- /proc/net ---------------------------------------------------------- */

/* The default route as `route -n` reads it: Destination 0, flags UP|GATEWAY. */
static int default_route(char *gw, size_t gwn, char *ifname, size_t ifn) {
    FILE *f = fopen("/proc/net/route", "r");
    if (!f) return 0;
    char line[256];
    int ok = 0;
    if (!fgets(line, sizeof line, f)) { fclose(f); return 0; } /* header */
    while (fgets(line, sizeof line, f)) {
        char ifc[64];
        unsigned long dest, gate;
        unsigned flags;
        if (sscanf(line, "%63s %lx %lx %x", ifc, &dest, &gate, &flags) != 4) continue;
        if (dest != 0 || (flags & 3) != 3) continue;
        struct in_addr a;
        a.s_addr = (in_addr_t)gate; /* the file holds the network-order word */
        inet_ntop(AF_INET, &a, gw, gwn);
        snprintf(ifname, ifn, "%s", ifc);
        ok = 1;
        break;
    }
    fclose(f);
    return ok;
}

/* tx_packets of `ifname` from /proc/net/dev, -1 if absent. */
static long long dev_tx_packets(const char *ifname) {
    FILE *f = fopen("/proc/net/dev", "r");
    if (!f) return -1;
    char line[512];
    long long r = -1;
    while (fgets(line, sizeof line, f)) {
        char *c = strchr(line, ':');
        if (!c) continue;
        *c = 0;
        char *nm = line;
        while (*nm == ' ') nm++;
        if (strcmp(nm, ifname) != 0) continue;
        unsigned long long v[16];
        if (sscanf(c + 1, "%llu %llu %llu %llu %llu %llu %llu %llu %llu %llu",
                   &v[0], &v[1], &v[2], &v[3], &v[4], &v[5], &v[6], &v[7], &v[8], &v[9]) == 10)
            r = (long long)v[9];
        break;
    }
    fclose(f);
    return r;
}

/* Find a /proc/net/{tcp,udp} row: local 127.0.0.1:lport, remote port rport
 * (-1 = any), state st. Returns the row's uid or -1. */
static int find_inet_row(const char *file, unsigned lport, int rport, unsigned st) {
    FILE *f = fopen(file, "r");
    if (!f) return -1;
    char line[512];
    int uid = -1;
    if (fgets(line, sizeof line, f)) {
        while (fgets(line, sizeof line, f)) {
            unsigned sl, la, lp, ra, rp, s, u;
            unsigned long txq, rxq;
            unsigned tr; unsigned long when; unsigned retr;
            if (sscanf(line, " %u: %X:%X %X:%X %X %lX:%lX %X:%lX %X %u",
                       &sl, &la, &lp, &ra, &rp, &s, &txq, &rxq, &tr, &when, &retr, &u) != 12) continue;
            if (la == 0x0100007F && lp == lport && s == st && (rport < 0 || rp == (unsigned)rport)) {
                uid = (int)u;
                break;
            }
        }
    }
    fclose(f);
    return uid;
}

static uint16_t local_port(int fd) {
    struct sockaddr_in a;
    socklen_t l = sizeof a;
    if (getsockname(fd, (struct sockaddr *)&a, &l) < 0) return 0;
    return ntohs(a.sin_port);
}

static void proc_net_route(const char *gw, const char *ifname, int found) {
    const char *N = "proc_net_route";
    if (!found) { fail(N, "no UP|GATEWAY default route in /proc/net/route"); return; }
    printf("%s: default via %s dev %s\n", N, gw, ifname);
    pass(N);
}

static void proc_net_dev(const char *ifname, long long before) {
    const char *N = "proc_net_dev";
    if (!ifname[0]) { skip(N, "(no default route)"); return; }
    long long after = dev_tx_packets(ifname);
    if (before < 0 || after < 0) { fail(N, "%s not in /proc/net/dev", ifname); return; }
    if (after <= before) { fail(N, "%s tx_packets %lld -> %lld across the ICMP cases", ifname, before, after); return; }
    printf("%s: %s tx_packets %lld -> %lld\n", N, ifname, before, after);
    pass(N);
}

static void proc_net_tcp(void) {
    const char *N = "proc_net_tcp";
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = sin4("127.0.0.1", 0);
    if (l < 0 || bind(l, (struct sockaddr *)&a, sizeof a) < 0 || listen(l, 4) < 0) {
        fail(N, "listener: %s", strerror(errno)); if (l >= 0) close(l); return;
    }
    uint16_t port = local_port(l);
    int me = (int)geteuid();
    int u = find_inet_row("/proc/net/tcp", port, 0, 0x0A);
    if (u != me) { fail(N, "listener 127.0.0.1:%u st 0A: uid %d (want %d)", port, u, me); close(l); return; }
    int c = socket(AF_INET, SOCK_STREAM, 0);
    a = sin4("127.0.0.1", port);
    if (c < 0 || connect(c, (struct sockaddr *)&a, sizeof a) < 0) {
        fail(N, "connect: %s", strerror(errno)); if (c >= 0) close(c); close(l); return;
    }
    struct pollfd p = { l, POLLIN, 0 };
    poll(&p, 1, 2000);
    int s = accept(l, NULL, NULL);
    uint16_t cport = local_port(c);
    /* Established rows can lag the handshake by a stack poll: retry briefly. */
    int us = -1, uc = -1;
    for (int i = 0; i < 20 && (us != me || uc != me); i++) {
        us = find_inet_row("/proc/net/tcp", port, cport, 0x01);
        uc = find_inet_row("/proc/net/tcp", cport, port, 0x01);
        if (us != me || uc != me) usleep(50 * 1000);
    }
    if (s < 0) fail(N, "accept: %s", strerror(errno));
    else if (us != me || uc != me)
        fail(N, "established rows %u<->%u: server uid %d, client uid %d (want %d)", port, cport, us, uc, me);
    else { printf("%s: listen 127.0.0.1:%u, established %u<->%u\n", N, port, port, cport); pass(N); }
    if (s >= 0) close(s);
    close(c); close(l);
}

static void proc_net_udp(void) {
    const char *N = "proc_net_udp";
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = sin4("127.0.0.1", 0);
    if (u < 0 || bind(u, (struct sockaddr *)&a, sizeof a) < 0) {
        fail(N, "bind: %s", strerror(errno)); if (u >= 0) close(u); return;
    }
    uint16_t port = local_port(u);
    int uid = find_inet_row("/proc/net/udp", port, 0, 0x07);
    if (uid != (int)geteuid()) fail(N, "127.0.0.1:%u st 07: uid %d (want %d)", port, uid, (int)geteuid());
    else { printf("%s: 127.0.0.1:%u\n", N, port); pass(N); }
    close(u);
}

static void proc_net_unix(void) {
    const char *N = "proc_net_unix";
    char path[96];
    snprintf(path, sizeof path, "/tmp/nettest-%d.sock", (int)getpid());
    unlink(path);
    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a;
    memset(&a, 0, sizeof a);
    a.sun_family = AF_UNIX;
    snprintf(a.sun_path, sizeof a.sun_path, "%s", path);
    if (l < 0 || bind(l, (struct sockaddr *)&a, sizeof a) < 0 || listen(l, 4) < 0) {
        fail(N, "listener: %s", strerror(errno)); if (l >= 0) close(l); unlink(path); return;
    }
    FILE *f = fopen("/proc/net/unix", "r");
    int found = 0;
    if (f) {
        char line[512];
        while (fgets(line, sizeof line, f)) {
            char num[32], pth[256] = "";
            unsigned ref, proto, flags, type, st;
            unsigned long ino;
            int n = sscanf(line, "%31s %X %X %X %X %X %lu %255s", num, &ref, &proto, &flags, &type, &st, &ino, pth);
            if (n == 8 && strcmp(pth, path) == 0 && (flags & 0x10000) && type == 1) { found = 1; break; }
        }
        fclose(f);
    }
    if (found) { printf("%s: %s listening\n", N, path); pass(N); }
    else fail(N, "%s not listed as a listening stream socket", path);
    close(l);
    unlink(path);
}

static int first_nameserver(char *out, size_t n) {
    FILE *f = fopen("/etc/resolv.conf", "r");
    if (!f) return 0;
    char line[256];
    int ok = 0;
    while (fgets(line, sizeof line, f)) {
        char ip[64];
        if (sscanf(line, " nameserver %63s", ip) == 1) { snprintf(out, n, "%s", ip); ok = 1; break; }
    }
    fclose(f);
    return ok;
}

int main(int argc, char **argv) {
    const char *gw = NULL, *name = "example.com", *http = NULL;
    char dns[64] = "";
    int o;
    while ((o = getopt(argc, argv, "g:d:n:t:")) != -1) {
        switch (o) {
        case 'g': gw = optarg; break;
        case 'd': snprintf(dns, sizeof dns, "%s", optarg); break;
        case 'n': name = optarg; break;
        case 't': http = optarg; break;
        default:
            fprintf(stderr, "usage: nettest [-g gateway] [-d dns] [-n name] [-t ip:port]\n");
            return 2;
        }
    }
    char rgw[64] = "", rif[64] = "";
    int have_route = default_route(rgw, sizeof rgw, rif, sizeof rif);
    if (!gw) gw = have_route ? rgw : "10.0.2.2";
    if (!dns[0] && !first_nameserver(dns, sizeof dns)) snprintf(dns, sizeof dns, "%s", gw);
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("nettest: uid %d, gateway %s, dns %s, name %s\n", (int)geteuid(), gw, dns, name);

    proc_net_route(rgw, rif, have_route);
    long long tx0 = have_route ? dev_tx_packets(rif) : -1;
    icmp_echo_case("icmp_raw", SOCK_RAW, gw);
    icmp_echo_case("icmp_dgram", SOCK_DGRAM, gw);
    icmp_timeout_cases();
    udp_dns_raw(dns, name);
    udp_getaddrinfo(name);
    tcp_dns(dns, name);
    tcp_http(http);
    proc_net_dev(rif, tx0);
    proc_net_tcp();
    proc_net_udp();
    proc_net_unix();

    printf("nettest: %d passed, %d failed, %d skipped\n", npass, nfail, nskip);
    return nfail > 100 ? 100 : nfail;
}
