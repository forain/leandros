/* Lane pmtud2: path-MTU discovery gaps -- ENOTCONN, per-namespace cache,
 * fragmenting at the learned path, TCP segment clamp, IPv6. Included from
 * udptest.c after udp_pmtud (uses its pmtu_csum). */
#define _GNU_SOURCE
#include <sched.h>
#include <sys/wait.h>
#include <netinet/ip6.h>
#include <netinet/icmp6.h>
#include <net/if.h>
#include <sys/ioctl.h>

#ifndef IPV6_MTU_DISCOVER
#define IPV6_MTU_DISCOVER 23
#endif
#ifndef IPV6_MTU
#define IPV6_MTU 24
#endif
#ifndef CLONE_NEWNET
#define CLONE_NEWNET 0x40000000
#endif

/* Send an ICMP frag-needed (mtu) about a UDP/TCP datagram from loopback to
 * `dst`, as the stack sees it on 127.0.0.1. proto 17 or 6. */
static const char *pmtu_qsrc = "127.0.0.1";
static int pmtu_inject4(const struct in_addr *dst, uint8_t proto, uint16_t mtu) {
    int raw = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    if (raw < 0) return -1;
    uint8_t m[8 + 20 + 8];
    memset(m, 0, sizeof m);
    m[0] = 3; m[1] = 4; m[6] = mtu >> 8; m[7] = mtu & 255;
    uint8_t *ip = m + 8;
    ip[0] = 0x45; ip[3] = 28 + 1500 - 28; ip[2] = (1500) >> 8; ip[6] = 0x40; ip[8] = 64; ip[9] = proto;
    uint32_t src; inet_pton(AF_INET, pmtu_qsrc, &src);
    memcpy(ip + 12, &src, 4); memcpy(ip + 16, dst, 4);
    uint16_t c = pmtu_csum(ip, 20); ip[10] = c >> 8; ip[11] = c & 255;
    ip[20] = 0x30; ip[21] = 0x39; ip[22] = 0x30; ip[23] = 0x3a; /* ports, unmatched */
    c = pmtu_csum(m, sizeof m); m[2] = c >> 8; m[3] = c & 255;
    struct sockaddr_in to = { .sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    int r = sendto(raw, m, sizeof m, 0, (struct sockaddr *)&to, sizeof to) == (ssize_t)sizeof m ? 0 : -1;
    close(raw);
    msleep(200);
    return r;
}

static void udp_pmtud_notconn(void) {
    const char *N = "udp_pmtud_notconn";
    if (skipped(N)) return;
    int s = socket(AF_INET, SOCK_DGRAM, 0), v = 0; socklen_t vl = sizeof v;
    errno = 0;
    int r = getsockopt(s, IPPROTO_IP, IP_MTU, &v, &vl), e = errno;
    close(s);
    if (r == 0 || e != ENOTCONN) fail(N, "IP_MTU on an unconnected socket: r=%d errno=%s, want ENOTCONN", r, strerror(e));
    else pass(N);
}

/* What one namespace learns, another does not see. */
static void udp_pmtud_netns(void) {
    const char *N = "udp_pmtud_netns";
    if (skipped(N)) return;
    struct in_addr dst; inet_pton(AF_INET, "127.0.0.1", &dst);
    int up[2], dn[2]; if (pipe(up) || pipe(dn)) { fail(N, "pipe"); return; }
    pid_t c = fork();
    if (c == 0) {
        int out = -1; char go = 0;
        if (unshare(CLONE_NEWNET) == 0) {
            /* a new namespace's lo starts down, as on Linux */
            int u = socket(AF_INET, SOCK_DGRAM, 0); struct ifreq ifr; memset(&ifr, 0, sizeof ifr);
            strcpy(ifr.ifr_name, "lo"); ifr.ifr_flags = IFF_UP | IFF_RUNNING;
            ioctl(u, SIOCSIFFLAGS, &ifr); close(u);
            write(up[1], &go, 1);          /* namespace exists */
            read(dn[0], &go, 1);           /* parent has learned its path */
            int s = socket(AF_INET, SOCK_DGRAM, 0), v = 0; socklen_t vl = sizeof v;
            struct sockaddr_in a = sin4("127.0.0.1", 9);
            if (connect(s, (struct sockaddr *)&a, sizeof a) == 0 && getsockopt(s, IPPROTO_IP, IP_MTU, &v, &vl) == 0) out = v;
            else out = -errno;
        } else { char g = 0; write(up[1], &g, 1); out = -1000 - errno; }
        write(up[1], &out, sizeof out);
        _exit(0);
    }
    char g; read(up[0], &g, 1);
    /* the parent namespace learns 1000 towards 127.0.0.1 while the child's
     * namespace exists (a global cache would hand it to the child) */
    pmtu_inject4(&dst, 17, 1000);
    int s = socket(AF_INET, SOCK_DGRAM, 0), v = 0; socklen_t vl = sizeof v;
    struct sockaddr_in a = sin4("127.0.0.1", 9);
    connect(s, (struct sockaddr *)&a, sizeof a);
    getsockopt(s, IPPROTO_IP, IP_MTU, &v, &vl);
    close(s);
    write(dn[1], &g, 1);
    int child = 0; read(up[0], &child, sizeof child); waitpid(c, NULL, 0);
    if (v != 1000) fail(N, "parent namespace IP_MTU %d, want 1000 (child %d)", v, child);
    else if (child != 65536) fail(N, "child namespace IP_MTU %d, want 65536 (cache leaked across namespaces)", child);
    else pass(N);
}

/* TCP segments shrink to the learned path: TCP_INFO reports it and a bulk
 * transfer still completes. */
static void tcp_pmtud_clamp(void) {
    const char *N = "tcp_pmtud_clamp";
    if (skipped(N)) return;
    char why[200] = "";
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in la = sin4("127.0.0.1", 0);
    bind(l, (struct sockaddr *)&la, sizeof la); listen(l, 1);
    socklen_t ll = sizeof la; getsockname(l, (struct sockaddr *)&la, &ll);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    if (connect(c, (struct sockaddr *)&la, sizeof la)) snprintf(why, sizeof why, "connect: %s", strerror(errno));
    int a = why[0] ? -1 : accept(l, NULL, NULL);
    uint8_t info[104]; socklen_t il = sizeof info;
    uint32_t mss0 = 0, mss1 = 0, pm1 = 0;
    if (!why[0]) {
        memset(info, 0, sizeof info);
        getsockopt(c, IPPROTO_TCP, 11, info, &il); memcpy(&mss0, info + 16, 4);
        pmtu_inject4(&la.sin_addr, 6, 900);
        il = sizeof info; getsockopt(c, IPPROTO_TCP, 11, info, &il);
        memcpy(&mss1, info + 16, 4); memcpy(&pm1, info + 60, 4);
        if (pm1 != 900) snprintf(why, sizeof why, "TCP_INFO pmtu %u, want 900", pm1);
        else if (mss1 > 848 || mss1 >= mss0) snprintf(why, sizeof why, "snd_mss %u (was %u), want <= 848", mss1, mss0);
    }
    if (!why[0]) {
        static char d[30000], g[30000];
        memset(d, 5, sizeof d);
        size_t sent = 0, got = 0;
        while (got < sizeof d) {
            if (sent < sizeof d) { ssize_t n = send(c, d + sent, sizeof d - sent, MSG_DONTWAIT); if (n > 0) sent += n; }
            ssize_t n = recv(a, g + got, sizeof g - got, MSG_DONTWAIT);
            if (n > 0) got += n; else msleep(5);
            static int spins; if (++spins > 4000) { snprintf(why, sizeof why, "transfer stalled at %zu", got); break; }
        }
    }
    if (a >= 0) close(a);
    close(c); close(l);
    if (why[0]) fail(N, "%s", why); else pass(N);
}

static uint16_t csum6(const struct in6_addr *s, const struct in6_addr *d, const uint8_t *p, size_t n) {
    uint32_t sum = 0;
    for (int i = 0; i < 16; i += 2) sum += (s->s6_addr[i] << 8) | s->s6_addr[i + 1];
    for (int i = 0; i < 16; i += 2) sum += (d->s6_addr[i] << 8) | d->s6_addr[i + 1];
    sum += n; sum += 58;
    for (size_t i = 0; i + 1 < n; i += 2) sum += (p[i] << 8) | p[i + 1];
    if (n & 1) sum += p[n - 1] << 8;
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return (uint16_t)~sum;
}

static void udp6_pmtud(void) {
    const char *N = "udp6_pmtud";
    if (skipped(N)) return;
    char why[200] = "";
    static char d[4000];
    memset(d, 3, sizeof d);
    int r = socket(AF_INET6, SOCK_DGRAM, 0), s = socket(AF_INET6, SOCK_DGRAM, 0);
    if (r < 0 || s < 0) { fail(N, "AF_INET6 socket: %s", strerror(errno)); return; }
    struct sockaddr_in6 ra = { .sin6_family = AF_INET6, .sin6_addr = IN6ADDR_LOOPBACK_INIT };
    if (bind(r, (struct sockaddr *)&ra, sizeof ra)) { fail(N, "bind ::1: %s", strerror(errno)); return; }
    socklen_t rl = sizeof ra; getsockname(r, (struct sockaddr *)&ra, &rl);
    int v = -1; socklen_t vl = sizeof v;
    errno = 0;
    if (getsockopt(s, IPPROTO_IPV6, IPV6_MTU, &v, &vl) == 0 || errno != ENOTCONN)
        snprintf(why, sizeof why, "IPV6_MTU unconnected: errno %s, want ENOTCONN", strerror(errno));
    if (!why[0] && (getsockopt(s, IPPROTO_IPV6, IPV6_MTU_DISCOVER, &v, &vl) || v != 1))
        snprintf(why, sizeof why, "default IPV6_MTU_DISCOVER %d (%s), want 1", v, strerror(errno));
    int on = 9; errno = 0;
    if (!why[0] && (setsockopt(s, IPPROTO_IPV6, IPV6_MTU_DISCOVER, &on, sizeof on) == 0 || errno != EINVAL))
        snprintf(why, sizeof why, "IPV6_MTU_DISCOVER=9 not EINVAL");
    on = 2;
    if (!why[0] && setsockopt(s, IPPROTO_IPV6, IPV6_MTU_DISCOVER, &on, sizeof on))
        snprintf(why, sizeof why, "set IPV6_MTU_DISCOVER=DO: %s", strerror(errno));
    if (!why[0] && connect(s, (struct sockaddr *)&ra, sizeof ra)) snprintf(why, sizeof why, "connect: %s", strerror(errno));
    v = 0;
    if (!why[0] && (getsockopt(s, IPPROTO_IPV6, IPV6_MTU, &v, &vl) || v != 65536))
        snprintf(why, sizeof why, "IPV6_MTU before PTB: %d (%s), want 65536", v, strerror(errno));
    if (!why[0] && send(s, d, 3000, 0) != 3000) snprintf(why, sizeof why, "3000-byte DO send: %s", strerror(errno));
    struct sockaddr_in6 la; socklen_t ll = sizeof la; getsockname(s, (struct sockaddr *)&la, &ll);
    if (!why[0]) {
        int raw = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
        if (raw < 0) snprintf(why, sizeof why, "raw icmpv6 socket: %s", strerror(errno));
        else {
            uint8_t m[8 + 40 + 8];
            memset(m, 0, sizeof m);
            m[0] = 2; /* packet too big */
            uint32_t mtu = htonl(1300); memcpy(m + 4, &mtu, 4);
            uint8_t *ip = m + 8;
            ip[0] = 0x60; ip[4] = (1300 >> 8); ip[5] = 1300 & 255; ip[6] = 17; ip[7] = 64;
            memcpy(ip + 8, &in6addr_loopback, 16); memcpy(ip + 24, &ra.sin6_addr, 16);
            memcpy(ip + 40, &la.sin6_port, 2); memcpy(ip + 42, &ra.sin6_port, 2);
            uint16_t c = csum6(&in6addr_loopback, &in6addr_loopback, m, sizeof m); m[2] = c >> 8; m[3] = c & 255;
            struct sockaddr_in6 to = { .sin6_family = AF_INET6, .sin6_addr = IN6ADDR_LOOPBACK_INIT };
            int on2 = 2; setsockopt(raw, IPPROTO_IPV6, IPV6_CHECKSUM, &on2, sizeof on2);
            if (sendto(raw, m, sizeof m, 0, (struct sockaddr *)&to, sizeof to) != (ssize_t)sizeof m)
                snprintf(why, sizeof why, "inject packet-too-big: %s", strerror(errno));
            close(raw);
            msleep(200);
        }
    }
    if (!why[0]) {
        errno = 0; ssize_t big = send(s, d, 1400, 0); int be = errno;
        ssize_t small = send(s, d, 1000, 0);
        v = 0;
        if (big != -1 || be != EMSGSIZE) snprintf(why, sizeof why, "1400-byte send over a 1300 path: %zd %s, want EMSGSIZE", big, strerror(be));
        else if (small != 1000) snprintf(why, sizeof why, "1000-byte send: %zd %s", small, strerror(errno));
        else if (getsockopt(s, IPPROTO_IPV6, IPV6_MTU, &v, &vl) || v != 1300) snprintf(why, sizeof why, "IPV6_MTU after PTB %d, want 1300", v);
    }
    close(r); close(s);
    if (why[0]) fail(N, "%s", why); else pass(N);
}
