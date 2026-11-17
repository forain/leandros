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
#include <poll.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>

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
    msleep(200); /* the stack sends on its next poll; closing first drops the queued packet */
    close(raw);
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
            msleep(200); /* send before close: a closed socket drops its queued packet */
            close(raw);
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

/* Lane leftovers: the cached v6 path MTU expires after net.ipv6.route.mtu_expires
 * (default 600 s, so the test shortens it to 2 s and restores it). */
static int sysctl_write(const char *path, const char *val) {
    int f = open(path, O_WRONLY); if (f < 0) return -1;
    int r = write(f, val, strlen(val)) == (ssize_t)strlen(val) ? 0 : -1; close(f); return r;
}
static void udp6_pmtud_expiry(void) {
    const char *N = "udp6_pmtud_expiry";
    if (skipped(N)) return;
    char why[200] = "", cur[32] = "";
    static char d[2000];
    memset(d, 3, sizeof d);
    int f = open("/proc/sys/net/ipv6/route/mtu_expires", O_RDONLY);
    ssize_t n = f >= 0 ? read(f, cur, sizeof cur - 1) : -1; if (f >= 0) close(f);
    if (n <= 0 || strncmp(cur, "600\n", 4)) { fail(N, "mtu_expires default %.*s, want 600", (int)(n > 0 ? n : 0), cur); return; }
    if (sysctl_write("/proc/sys/net/ipv6/route/mtu_expires", "2") || sysctl_write("/proc/sys/net/ipv4/route/mtu_expires", "2")) {
        fail(N, "cannot shorten mtu_expires: %s", strerror(errno)); return;
    }
    int r = socket(AF_INET6, SOCK_DGRAM, 0), s = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 ra = { .sin6_family = AF_INET6, .sin6_addr = IN6ADDR_LOOPBACK_INIT };
    if (r < 0 || s < 0 || bind(r, (struct sockaddr *)&ra, sizeof ra)) { fail(N, "socket/bind: %s", strerror(errno)); goto restore; }
    socklen_t rl = sizeof ra; getsockname(r, (struct sockaddr *)&ra, &rl);
    int v = 0; socklen_t vl = sizeof v;
    if (connect(s, (struct sockaddr *)&ra, sizeof ra)) { snprintf(why, sizeof why, "connect: %s", strerror(errno)); goto out; }
    struct sockaddr_in6 la; socklen_t ll = sizeof la; getsockname(s, (struct sockaddr *)&la, &ll);
    int raw = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
    if (raw < 0) { snprintf(why, sizeof why, "raw icmpv6 socket: %s", strerror(errno)); goto out; }
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
    if (sendto(raw, m, sizeof m, 0, (struct sockaddr *)&to, sizeof to) != (ssize_t)sizeof m) snprintf(why, sizeof why, "inject packet-too-big: %s", strerror(errno));
    msleep(200);
    close(raw);
    if (!why[0] && (getsockopt(s, IPPROTO_IPV6, IPV6_MTU, &v, &vl) || v != 1300)) snprintf(why, sizeof why, "IPV6_MTU after PTB %d, want 1300", v);
    if (!why[0]) {
        msleep(3000); /* past the shortened 2 s lifetime */
        v = 0;
        if (getsockopt(s, IPPROTO_IPV6, IPV6_MTU, &v, &vl) || v != 65536) snprintf(why, sizeof why, "IPV6_MTU after expiry %d, want 65536", v);
        else if (send(s, d, 1400, 0) != 1400) snprintf(why, sizeof why, "1400-byte send after expiry: %s", strerror(errno));
    }
out:
    close(r); close(s);
    if (why[0]) fail(N, "%s", why); else pass(N);
restore:
    sysctl_write("/proc/sys/net/ipv6/route/mtu_expires", "600");
    sysctl_write("/proc/sys/net/ipv4/route/mtu_expires", "600");
}

/* Lane udppmtu: a WANT-mode (default) datagram larger than the cached path
 * MTU leaves the host as IP fragments cut to that MTU (DF clear, MF on all
 * but the last, one ident, offsets contiguous), read off an IFF_TUN wire in
 * a private namespace; DO refuses it with EMSGSIZE and puts nothing on the
 * wire; IP_MTU reports the cached value. */
#ifndef IFF_TUN
#define IFF_TUN 1
#endif
#ifndef IFF_NO_PI
#define IFF_NO_PI 0x1000
#endif
static void pmtud_want_child(int wfd, int tun) {
    char why[200] = "";
    uint16_t pmtu = 1000;
    struct in_addr me, peer;
    inet_pton(AF_INET, "10.77.0.1", &me); inet_pton(AF_INET, "10.77.0.2", &peer);
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in pa = sin4("10.77.0.2", 9000);
    static char d[4000]; static uint8_t pk[2048];
    for (int i = 0; i < 4000; i++) d[i] = (char)i;
    if (connect(s, (struct sockaddr *)&pa, sizeof pa)) { snprintf(why, sizeof why, "connect: %s", strerror(errno)); goto out; }
    struct sockaddr_in la; socklen_t ll = sizeof la; getsockname(s, (struct sockaddr *)&la, &ll);
    /* ICMP frag-needed from the peer, quoting our UDP datagram, written onto the wire */
    uint8_t m[20 + 8 + 20 + 8]; memset(m, 0, sizeof m);
    m[0] = 0x45; m[2] = 0; m[3] = sizeof m; m[8] = 64; m[9] = 1;
    memcpy(m + 12, &peer, 4); memcpy(m + 16, &me, 4);
    uint16_t c = pmtu_csum(m, 20); m[10] = c >> 8; m[11] = c & 255;
    uint8_t *ic = m + 20; ic[0] = 3; ic[1] = 4; ic[6] = pmtu >> 8; ic[7] = pmtu & 255;
    uint8_t *q = ic + 8; q[0] = 0x45; q[3] = 28 + 3000 > 65535 ? 0 : 20 + 8 + 3000 - 0; q[2] = (20 + 8 + 3000) >> 8; q[3] = (20 + 8 + 3000) & 255;
    q[6] = 0x40; q[8] = 63; q[9] = 17; memcpy(q + 12, &me, 4); memcpy(q + 16, &peer, 4);
    c = pmtu_csum(q, 20); q[10] = c >> 8; q[11] = c & 255;
    memcpy(q + 20, &la.sin_port, 2); memcpy(q + 22, &pa.sin_port, 2);
    c = pmtu_csum(ic, 8 + 28); ic[2] = c >> 8; ic[3] = c & 255;
    if (write(tun, m, sizeof m) != (ssize_t)sizeof m) { snprintf(why, sizeof why, "inject: %s", strerror(errno)); goto out; }
    msleep(200);
    int v = 0; socklen_t vl = sizeof v;
    if (getsockopt(s, IPPROTO_IP, IP_MTU, &v, &vl) || v != pmtu) { snprintf(why, sizeof why, "IP_MTU %d (%s), want %d", v, strerror(errno), pmtu); goto out; }
    while (read(tun, pk, sizeof pk) > 0) {}   /* drop anything queued so far */
    if (send(s, d, 3000, 0) != 3000) { snprintf(why, sizeof why, "WANT send of 3000: %s", strerror(errno)); goto out; }
    msleep(200);
    int nfr = 0, total = 0, next = 0, ident = -1;
    for (;;) {
        ssize_t n = read(tun, pk, sizeof pk);
        if (n <= 0) break;
        if ((pk[0] >> 4) != 4 || pk[9] != 17) continue;
        int len = (pk[2] << 8) | pk[3], fo = (((pk[6] & 0x1f) << 8) | pk[7]) * 8, mf = !!(pk[6] & 0x20), df = !!(pk[6] & 0x40);
        int id = (pk[4] << 8) | pk[5], pay = len - 20;
        if (len > pmtu) { snprintf(why, sizeof why, "fragment %d is %d bytes, over the %d path", nfr, len, pmtu); goto out; }
        if (df) { snprintf(why, sizeof why, "DF set on a WANT fragment"); goto out; }
        if (ident >= 0 && id != ident) { snprintf(why, sizeof why, "ident changed %d -> %d", ident, id); goto out; }
        ident = id;
        if (fo != next) { snprintf(why, sizeof why, "fragment %d offset %d, want %d", nfr, fo, next); goto out; }
        if (mf && (pay & 7)) { snprintf(why, sizeof why, "non-last fragment payload %d not a multiple of 8", pay); goto out; }
        next += pay; total += pay; nfr++;
        if (!mf) break;
    }
    if (nfr < 4 || total != 3008) { snprintf(why, sizeof why, "%d fragments carrying %d bytes, want >=4 carrying 3008", nfr, total); goto out; }
    int pd = 2;
    setsockopt(s, IPPROTO_IP, IP_MTU_DISCOVER, &pd, sizeof pd);
    errno = 0;
    if (send(s, d, 3000, 0) != -1 || errno != EMSGSIZE) { snprintf(why, sizeof why, "DO send of 3000: %s, want EMSGSIZE", strerror(errno)); goto out; }
    msleep(100);
    if (read(tun, pk, sizeof pk) > 0) snprintf(why, sizeof why, "DO refused send still put a packet on the wire");
out:
    close(s);
    write(wfd, why, strlen(why) + 1);
}

static void udp_pmtud_want_frag(void) {
    const char *N = "udp_pmtud_want_frag";
    if (skipped(N)) return;
    int p[2]; if (pipe(p)) { fail(N, "pipe"); return; }
    pid_t c = fork();
    if (c == 0) {
        char why[220] = "";
        if (unshare(CLONE_NEWNET)) { snprintf(why, sizeof why, "SKIP unshare: %s", strerror(errno)); write(p[1], why, strlen(why) + 1); _exit(0); }
        int tun = open("/dev/net/tun", O_RDWR | O_NONBLOCK);
        struct ifreq ifr; memset(&ifr, 0, sizeof ifr);
        strcpy(ifr.ifr_name, "pmt%d"); ifr.ifr_flags = IFF_TUN | IFF_NO_PI;
        if (tun < 0 || ioctl(tun, 0x400454ca, &ifr) < 0) { snprintf(why, sizeof why, "SKIP tun: %s", strerror(errno)); write(p[1], why, strlen(why) + 1); _exit(0); }
        int u = socket(AF_INET, SOCK_DGRAM, 0); struct sockaddr_in *sa = (struct sockaddr_in *)&ifr.ifr_addr;
        sa->sin_family = AF_INET; inet_pton(AF_INET, "10.77.0.1", &sa->sin_addr); ioctl(u, SIOCSIFADDR, &ifr);
        inet_pton(AF_INET, "255.255.255.0", &sa->sin_addr); ioctl(u, SIOCSIFNETMASK, &ifr);
        ioctl(u, SIOCGIFFLAGS, &ifr); ifr.ifr_flags |= IFF_UP; ioctl(u, SIOCSIFFLAGS, &ifr); close(u);
        pmtud_want_child(p[1], tun);
        _exit(0);
    }
    char why[220] = ""; ssize_t n = read(p[0], why, sizeof why - 1); waitpid(c, NULL, 0);
    if (n <= 0) fail(N, "child died");
    else if (!strncmp(why, "SKIP", 4)) fail(N, "%s", why);
    else if (why[0]) fail(N, "%s", why);
    else pass(N);
}

/* Lane udp6frag: IPv6 source fragmentation. A datagram over the cached path
 * MTU (ICMPv6 packet-too-big, 1280) leaves as Fragment-header packets cut to
 * it (one ident, offsets contiguous, M on all but the last, each <= 1280);
 * looped back through the wire they reassemble (in order and reversed);
 * IPV6_DONTFRAG makes the same send EMSGSIZE with nothing on the wire. */
#ifndef IPV6_DONTFRAG
#define IPV6_DONTFRAG 62
#endif
static int tun6_addr(const char *ifn, const char *ip, int plen) {
    struct { struct nlmsghdr h; struct ifaddrmsg a; struct rtattr r1; struct in6_addr l; struct rtattr r2; struct in6_addr x; } m;
    memset(&m, 0, sizeof m);
    m.h.nlmsg_len = sizeof m; m.h.nlmsg_type = RTM_NEWADDR; m.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_CREATE | NLM_F_EXCL | NLM_F_ACK;
    m.a.ifa_family = AF_INET6; m.a.ifa_prefixlen = plen; m.a.ifa_index = if_nametoindex(ifn);
    m.r1.rta_len = RTA_LENGTH(16); m.r1.rta_type = IFA_LOCAL; inet_pton(AF_INET6, ip, &m.l);
    m.r2.rta_len = RTA_LENGTH(16); m.r2.rta_type = IFA_ADDRESS; m.x = m.l;
    int s = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (s < 0) return -1;
    char rb[512]; int r = -1;
    if (send(s, &m, sizeof m, 0) == (ssize_t)sizeof m) {
        ssize_t n = recv(s, rb, sizeof rb, 0);
        struct nlmsghdr *h = (struct nlmsghdr *)rb;
        if (n >= (ssize_t)NLMSG_HDRLEN && h->nlmsg_type == NLMSG_ERROR) r = -((struct nlmsgerr *)NLMSG_DATA(h))->error;
    }
    close(s);
    return r;
}

/* Read the fragments of one datagram off the wire into fr[] (raw packets). */
static int pmtud6_collect(int tun, uint8_t fr[][1600], int *frlen, int max, char *why, size_t wl, int pmtu, int paylen) {
    int nfr = 0, total = 0, next = 0; uint32_t ident = 0; static uint8_t pk[2048];
    for (int tries = 0; tries < 40; tries++) {
        ssize_t n = read(tun, pk, sizeof pk);
        if (n <= 0) { msleep(50); continue; }
        if ((pk[0] >> 4) != 6 || pk[6] != 44) continue;       /* only Fragment-header packets */
        int plen = (pk[4] << 8) | pk[5], len = 40 + plen;
        uint8_t *h = pk + 40;
        int fo = (((h[2] << 8) | h[3]) & 0xfff8), mf = h[3] & 1, pay = plen - 8;
        uint32_t id = (h[4] << 24) | (h[5] << 16) | (h[6] << 8) | h[7];
        if (len > pmtu) { snprintf(why, wl, "fragment %d is %d bytes, over the %d path", nfr, len, pmtu); return -1; }
        if (h[0] != 17) { snprintf(why, wl, "fragment header next header %d, want 17", h[0]); return -1; }
        if (nfr && id != ident) { snprintf(why, wl, "ident changed %u -> %u", ident, id); return -1; }
        ident = id;
        if (fo != next) { snprintf(why, wl, "fragment %d offset %d, want %d", nfr, fo, next); return -1; }
        if (mf && (pay & 7)) { snprintf(why, wl, "non-last fragment payload %d not a multiple of 8", pay); return -1; }
        if (nfr >= max) { snprintf(why, wl, "too many fragments"); return -1; }
        memcpy(fr[nfr], pk, len); frlen[nfr] = len;
        next += pay; total += pay; nfr++;
        if (!mf) break;
    }
    if (nfr < 3 || total != paylen + 8) { snprintf(why, wl, "%d fragments carrying %d bytes, want >=3 carrying %d", nfr, total, paylen + 8); return -1; }
    return nfr;
}

static void pmtud6_child(int wfd, int tun, const char *ifn) {
    char why[200] = "";
    uint16_t pmtu = 1280;
    struct in6_addr me, peer;
    inet_pton(AF_INET6, "fd00:77::1", &me); inet_pton(AF_INET6, "fd00:77::2", &peer);
    static char d[4000], g[4000]; static uint8_t fr[8][1600]; int frlen[8];
    for (int i = 0; i < 4000; i++) d[i] = (char)(i * 7);
    if (tun6_addr(ifn, "fd00:77::1", 64)) { snprintf(why, sizeof why, "add v6 address: %s", strerror(errno)); goto out; }
    int s = socket(AF_INET6, SOCK_DGRAM, 0), r = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 pa = { .sin6_family = AF_INET6, .sin6_port = htons(9000), .sin6_addr = peer };
    struct sockaddr_in6 ra = { .sin6_family = AF_INET6, .sin6_port = htons(9000), .sin6_addr = IN6ADDR_ANY_INIT };
    if (bind(r, (struct sockaddr *)&ra, sizeof ra)) { snprintf(why, sizeof why, "bind r: %s", strerror(errno)); goto out; }
    if (connect(s, (struct sockaddr *)&pa, sizeof pa)) { snprintf(why, sizeof why, "connect: %s", strerror(errno)); goto out; }
    struct sockaddr_in6 la; socklen_t ll = sizeof la; getsockname(s, (struct sockaddr *)&la, &ll);
    /* ICMPv6 packet-too-big from the peer quoting our datagram, onto the wire */
    uint8_t m[40 + 8 + 40 + 8]; memset(m, 0, sizeof m);
    m[0] = 0x60; m[5] = sizeof m - 40; m[6] = 58; m[7] = 64;
    memcpy(m + 8, &peer, 16); memcpy(m + 24, &me, 16);
    uint8_t *ic = m + 40; ic[0] = 2; ic[6] = pmtu >> 8; ic[7] = pmtu & 255;
    uint8_t *q = ic + 8; q[0] = 0x60; q[4] = (8 + 3000) >> 8; q[5] = (8 + 3000) & 255; q[6] = 17; q[7] = 63;
    memcpy(q + 8, &me, 16); memcpy(q + 24, &peer, 16);
    memcpy(q + 40, &la.sin6_port, 2); memcpy(q + 42, &pa.sin6_port, 2);
    uint16_t c = csum6(&peer, &me, ic, 8 + 48); ic[2] = c >> 8; ic[3] = c & 255;
    if (write(tun, m, sizeof m) != (ssize_t)sizeof m) { snprintf(why, sizeof why, "inject: %s", strerror(errno)); goto out; }
    msleep(200);
    int v = 0; socklen_t vl = sizeof v;
    if (getsockopt(s, IPPROTO_IPV6, IPV6_MTU, &v, &vl) || v != pmtu) { snprintf(why, sizeof why, "IPV6_MTU %d (%s), want %d", v, strerror(errno), pmtu); goto out; }
    while (read(tun, fr[0], 1600) > 0) {}
    if (send(s, d, 3000, 0) != 3000) { snprintf(why, sizeof why, "default send of 3000: %s", strerror(errno)); goto out; }
    msleep(200);
    int nfr = pmtud6_collect(tun, fr, frlen, 8, why, sizeof why, pmtu, 3000);
    if (nfr < 0) goto out;
    /* control: one UNfragmented datagram reflected the same way must reach r (separates rx path from reassembly) */
    {
        while (read(tun, fr[7], 1600) > 0) {}
        if (send(s, d, 600, 0) != 600) { snprintf(why, sizeof why, "send of 600: %s", strerror(errno)); goto out; }
        msleep(200);
        ssize_t pn = -1;
        for (int i = 0; i < 20 && pn <= 0; i++) { pn = read(tun, fr[7], 1600); if (pn <= 0) msleep(50); }
        if (pn < 48 || fr[7][6] != 17) { snprintf(why, sizeof why, "plain reflect: no UDP packet on the wire (%zd)", pn); goto out; }
        uint8_t t[16]; memcpy(t, fr[7] + 8, 16); memcpy(fr[7] + 8, fr[7] + 24, 16); memcpy(fr[7] + 24, t, 16);
        if (write(tun, fr[7], pn) != pn) { snprintf(why, sizeof why, "plain reflect write: %s", strerror(errno)); goto out; }
        struct pollfd pf = { r, POLLIN, 0 };
        ssize_t got = poll(&pf, 1, 3000) > 0 ? recv(r, g, sizeof g, 0) : -1;
        if (got != 600) { snprintf(why, sizeof why, "plain (unfragmented) reflect: received %zd bytes: rx path, not reassembly", got); goto out; }
    }
    /* loop the fragments back (addresses swapped: the UDP checksum is unchanged), in order then reversed */
    for (int round = 0; round < 2; round++) {
        if (round) {
            while (read(tun, fr[7], 1600) > 0) {}
            if (send(s, d, 3000, 0) != 3000) { snprintf(why, sizeof why, "second send: %s", strerror(errno)); goto out; }
            msleep(200);
            nfr = pmtud6_collect(tun, fr, frlen, 8, why, sizeof why, pmtu, 3000);
            if (nfr < 0) goto out;
        }
        for (int i = 0; i < nfr; i++) {
            int k = round ? nfr - 1 - i : i;
            uint8_t t[16]; memcpy(t, fr[k] + 8, 16); memcpy(fr[k] + 8, fr[k] + 24, 16); memcpy(fr[k] + 24, t, 16);
            if (write(tun, fr[k], frlen[k]) != frlen[k]) { snprintf(why, sizeof why, "loopback write: %s", strerror(errno)); goto out; }
        }
        struct pollfd pf = { r, POLLIN, 0 };
        ssize_t got = poll(&pf, 1, 3000) > 0 ? recv(r, g, sizeof g, 0) : -1;
        if (got != 3000 || memcmp(g, d, 3000)) { snprintf(why, sizeof why, "reassembly round %d: received %zd bytes (%s), want the 3000 sent", round, got, got < 0 ? "none" : "content or size differs"); goto out; }
    }
    int on = 1;
    if (setsockopt(s, IPPROTO_IPV6, IPV6_DONTFRAG, &on, sizeof on)) { snprintf(why, sizeof why, "set IPV6_DONTFRAG: %s", strerror(errno)); goto out; }
    v = 0; vl = sizeof v;
    if (getsockopt(s, IPPROTO_IPV6, IPV6_DONTFRAG, &v, &vl) || v != 1) { snprintf(why, sizeof why, "IPV6_DONTFRAG reads %d, want 1", v); goto out; }
    while (read(tun, fr[0], 1600) > 0) {}
    errno = 0;
    if (send(s, d, 3000, 0) != -1 || errno != EMSGSIZE) { snprintf(why, sizeof why, "DONTFRAG send of 3000: %s, want EMSGSIZE", strerror(errno)); goto out; }
    msleep(100);
    for (int i = 0; i < 5; i++) {
        ssize_t n = read(tun, fr[0], 1600);
        if (n > 0 && (fr[0][0] >> 4) == 6 && (fr[0][6] == 44 || fr[0][6] == 17)) { snprintf(why, sizeof why, "DONTFRAG refused send still put a packet on the wire"); goto out; }
    }
    if (send(s, d, 1000, 0) != 1000) snprintf(why, sizeof why, "DONTFRAG send of 1000: %s", strerror(errno));
out:
    write(wfd, why, strlen(why) + 1);
}

static void udp6_pmtud_want_frag(void) {
    const char *N = "udp6_pmtud_want_frag";
    if (skipped(N)) return;
    int p[2]; if (pipe(p)) { fail(N, "pipe"); return; }
    pid_t c = fork();
    if (c == 0) {
        char why[220] = "";
        if (unshare(CLONE_NEWNET)) { snprintf(why, sizeof why, "SKIP unshare: %s", strerror(errno)); write(p[1], why, strlen(why) + 1); _exit(0); }
        int tun = open("/dev/net/tun", O_RDWR | O_NONBLOCK);
        struct ifreq ifr; memset(&ifr, 0, sizeof ifr);
        strcpy(ifr.ifr_name, "pm6%d"); ifr.ifr_flags = IFF_TUN | IFF_NO_PI;
        if (tun < 0 || ioctl(tun, 0x400454ca, &ifr) < 0) { snprintf(why, sizeof why, "SKIP tun: %s", strerror(errno)); write(p[1], why, strlen(why) + 1); _exit(0); }
        int u = socket(AF_INET, SOCK_DGRAM, 0); struct sockaddr_in *sa = (struct sockaddr_in *)&ifr.ifr_addr;
        sa->sin_family = AF_INET; inet_pton(AF_INET, "10.78.0.1", &sa->sin_addr); ioctl(u, SIOCSIFADDR, &ifr);
        inet_pton(AF_INET, "255.255.255.0", &sa->sin_addr); ioctl(u, SIOCSIFNETMASK, &ifr);
        ioctl(u, SIOCGIFFLAGS, &ifr); ifr.ifr_flags |= IFF_UP; ioctl(u, SIOCSIFFLAGS, &ifr); close(u);
        pmtud6_child(p[1], tun, ifr.ifr_name);
        _exit(0);
    }
    char why[220] = ""; ssize_t n = read(p[0], why, sizeof why - 1); waitpid(c, NULL, 0);
    if (n <= 0) fail(N, "child died");
    else if (why[0]) fail(N, "%s", why);
    else pass(N);
}
