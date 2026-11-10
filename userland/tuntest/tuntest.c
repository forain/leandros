/*
 * tuntest - /dev/net/tun in a new user+net namespace.
 *   tap_udp      create tap0 (TUNSETIFF "tap%d"), address + IFF_UP through
 *                ioctls on an AF_INET socket, UDP to a peer on the subnet:
 *                the tap fd yields an ARP request, then (after an ARP reply is
 *                written) the UDP frame
 *   tap_rx       a crafted UDP reply frame written to the fd arrives on the UDP socket
 *   tap_poll     poll() readiness and O_NONBLOCK EAGAIN
 *   tun_l3       IFF_TUN: raw IP packets, ARP answered by the kernel
 *   tun_getiff   TUNGETIFF / TUNSETIFF errors (VNET_HDR, MULTI_QUEUE: EINVAL)
 *   tun_eperm    an unprivileged task in the initial namespaces: EPERM
 *   tcp_out / tcp_in  TCP in a netns over a tap: connect (SYN frame, injected
 *                SYN-ACK, data, FIN) and listen/accept (injected SYN)
 * Run as root.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <netinet/in.h>
#include <poll.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>
#include <stdint.h>

#ifndef TUNSETIFF
#define TUNSETIFF 0x400454ca
#define TUNGETIFF 0x800454d2
#define TUNSETPERSIST 0x400454cb
#endif
#define F_TUN 1
#define F_TAP 2
#define F_NO_PI 0x1000
#define F_VNET 0x4000
#define F_MQ 0x100

static int npass, nfail;
#define CHECK(name, cond) do { if (cond) { npass++; printf("%s: PASS\n", name); } \
    else { nfail++; printf("%s: FAIL (errno %d, line %d)\n", name, errno, __LINE__); } } while (0)

static int enter_ns(void)
{
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return -1;
    return 0;
}

static int tun_open(const char *name, int flags, char *out)
{
    int fd = open("/dev/net/tun", O_RDWR);
    if (fd < 0) return -1;
    struct ifreq ifr;
    memset(&ifr, 0, sizeof ifr);
    strncpy(ifr.ifr_name, name, IFNAMSIZ - 1);
    ifr.ifr_flags = flags;
    if (ioctl(fd, TUNSETIFF, &ifr) < 0) { int e = errno; close(fd); errno = e; return -1; }
    if (out) strcpy(out, ifr.ifr_name);
    return fd;
}

static int set_addr(const char *ifn, const char *addr, const char *mask)
{
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct ifreq ifr;
    struct sockaddr_in *sin = (struct sockaddr_in *)&ifr.ifr_addr;
    int rc = 0;
    memset(&ifr, 0, sizeof ifr);
    strcpy(ifr.ifr_name, ifn);
    sin->sin_family = AF_INET;
    inet_pton(AF_INET, addr, &sin->sin_addr);
    rc |= ioctl(s, SIOCSIFADDR, &ifr);
    inet_pton(AF_INET, mask, &sin->sin_addr);
    rc |= ioctl(s, SIOCSIFNETMASK, &ifr);
    rc |= ioctl(s, SIOCGIFFLAGS, &ifr);
    ifr.ifr_flags |= IFF_UP;
    rc |= ioctl(s, SIOCSIFFLAGS, &ifr);
    close(s);
    return rc;
}

static int rd_timeout(int fd, unsigned char *b, int n, int ms)
{
    struct pollfd p = { fd, POLLIN, 0 };
    if (poll(&p, 1, ms) <= 0) return -1;
    return read(fd, b, n);
}

static uint16_t csum16(const unsigned char *d, int n)
{
    uint32_t s = 0;
    for (int i = 0; i + 1 < n; i += 2) s += (d[i] << 8) | d[i + 1];
    if (n & 1) s += d[n - 1] << 8;
    while (s >> 16) s = (s & 0xffff) + (s >> 16);
    return ~s;
}

/* The ns child: runs all in-namespace cases. */
static int ns_cases(void)
{
    char name[IFNAMSIZ] = "";
    unsigned char buf[2048];
    int fd, n, r = 0;
    if (enter_ns() != 0) { printf("tap_udp: FAIL (unshare errno %d)\n", errno); return 1; }
    fd = tun_open("tap%d", F_TAP | F_NO_PI, name);
    CHECK("tap_create", fd >= 0 && strcmp(name, "tap0") == 0);
    if (fd < 0) return 1;
    CHECK("tap_addr_up", set_addr("tap0", "10.9.0.1", "255.255.255.0") == 0);
    {
        int s = socket(AF_INET, SOCK_DGRAM, 0);
        struct ifreq ifr; memset(&ifr, 0, sizeof ifr); strcpy(ifr.ifr_name, "tap0");
        CHECK("sioc_ifindex", ioctl(s, SIOCGIFINDEX, &ifr) == 0 && ifr.ifr_ifindex > 0);
        memset(&ifr, 0, sizeof ifr); strcpy(ifr.ifr_name, "tap0");
        CHECK("sioc_flags", ioctl(s, SIOCGIFFLAGS, &ifr) == 0 && (ifr.ifr_flags & IFF_UP));
        close(s);
    }
    /* O_NONBLOCK read on an idle tap */
    fcntl(fd, F_SETFL, O_NONBLOCK);
    CHECK("tap_nonblock_eagain", read(fd, buf, sizeof buf) < 0 && errno == EAGAIN);

    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in me = { .sin_family = AF_INET, .sin_port = htons(4242) };
    struct sockaddr_in peer = { .sin_family = AF_INET, .sin_port = htons(9999) };
    inet_pton(AF_INET, "10.9.0.2", &peer.sin_addr);
    CHECK("udp_bind", bind(u, (struct sockaddr *)&me, sizeof me) == 0);
    CHECK("udp_sendto", sendto(u, "hello", 5, 0, (struct sockaddr *)&peer, sizeof peer) == 5);

    /* ARP request first */
    n = rd_timeout(fd, buf, sizeof buf, 3000);
    int arp = n >= 42 && buf[12] == 0x08 && buf[13] == 0x06 && buf[21] == 1 &&
              memcmp(buf + 38, "\x0a\x09\x00\x02", 4) == 0;
    CHECK("tap_arp_request", arp);
    if (!arp) return 1;
    unsigned char tapmac[6]; memcpy(tapmac, buf + 22, 6);
    unsigned char pmac[6] = { 0x02, 0x11, 0x22, 0x33, 0x44, 0x55 };
    {   /* ARP reply from the peer */
        unsigned char a[42];
        memcpy(a, tapmac, 6); memcpy(a + 6, pmac, 6);
        a[12] = 0x08; a[13] = 0x06;
        memcpy(a + 14, "\x00\x01\x08\x00\x06\x04\x00\x02", 8);
        memcpy(a + 22, pmac, 6); memcpy(a + 28, "\x0a\x09\x00\x02", 4);
        memcpy(a + 32, tapmac, 6); memcpy(a + 38, "\x0a\x09\x00\x01", 4);
        CHECK("tap_write_arp", write(fd, a, 42) == 42);
    }
    n = rd_timeout(fd, buf, sizeof buf, 3000);
    int udp = n >= 14 + 20 + 8 + 5 && buf[12] == 0x08 && buf[13] == 0x00 && buf[14 + 9] == 17 &&
              memcmp(buf, pmac, 6) == 0 && memcmp(buf + 30, "\x0a\x09\x00\x02", 4) == 0 &&
              ntohs(*(uint16_t *)(buf + 34 + 2)) == 9999 && memcmp(buf + 42, "hello", 5) == 0;
    CHECK("tap_udp_frame", udp);
    if (!udp) return 1;
    int sport = ntohs(*(uint16_t *)(buf + 34));
    CHECK("udp_source_port", sport == 4242);

    /* Craft a reply: peer 10.9.0.2:9999 -> 10.9.0.1:4242 "pong" */
    {
        unsigned char f[14 + 20 + 8 + 4];
        memset(f, 0, sizeof f);
        memcpy(f, tapmac, 6); memcpy(f + 6, pmac, 6); f[12] = 8; f[13] = 0;
        unsigned char *ip = f + 14;
        ip[0] = 0x45; ip[2] = 0; ip[3] = 20 + 8 + 4; ip[8] = 64; ip[9] = 17;
        memcpy(ip + 12, "\x0a\x09\x00\x02", 4); memcpy(ip + 16, "\x0a\x09\x00\x01", 4);
        uint16_t c = csum16(ip, 20); ip[10] = c >> 8; ip[11] = c & 0xff;
        unsigned char *ud = ip + 20;
        ud[0] = 9999 >> 8; ud[1] = 9999 & 0xff; ud[2] = 4242 >> 8; ud[3] = 4242 & 0xff;
        ud[5] = 8 + 4;
        memcpy(ud + 8, "pong", 4);   /* UDP checksum 0 = none (IPv4) */
        CHECK("tap_write_udp", write(fd, f, sizeof f) == (ssize_t)sizeof f);
    }
    {
        struct pollfd p = { u, POLLIN, 0 };
        char rb[64];
        struct sockaddr_in from; socklen_t fl = sizeof from;
        int pr = poll(&p, 1, 3000);
        n = pr > 0 ? recvfrom(u, rb, sizeof rb, 0, (struct sockaddr *)&from, &fl) : -1;
        CHECK("udp_recv_reply", n == 4 && memcmp(rb, "pong", 4) == 0 && ntohs(from.sin_port) == 9999);
    }
    /* poll: nothing pending -> no POLLIN; a send -> POLLIN */
    {
        struct pollfd p = { fd, POLLIN, 0 };
        while (read(fd, buf, sizeof buf) > 0) ;
        CHECK("tap_poll_idle", poll(&p, 1, 50) == 0);
        sendto(u, "x", 1, 0, (struct sockaddr *)&peer, sizeof peer);
        CHECK("tap_poll_ready", poll(&p, 1, 3000) == 1 && (p.revents & POLLIN));
    }
    {   /* TUNGETIFF and errors */
        struct ifreq ifr; memset(&ifr, 0, sizeof ifr);
        CHECK("tungetiff", ioctl(fd, TUNGETIFF, &ifr) == 0 && strcmp(ifr.ifr_name, "tap0") == 0 &&
                           (ifr.ifr_flags & F_TAP));
        int e1 = tun_open("tap%d", F_TAP | F_VNET, NULL) < 0 && errno == EINVAL;
        int e2 = tun_open("tap%d", F_TAP | F_MQ, NULL) < 0 && errno == EINVAL;
        CHECK("tun_einval_flags", e1 && e2);
        char n2[IFNAMSIZ];
        int fd2 = tun_open("tap%d", F_TAP | F_NO_PI, n2);
        CHECK("tap_second_name", fd2 >= 0 && strcmp(n2, "tap1") == 0);
        if (fd2 >= 0) close(fd2);
    }
    /* L3 tun: raw IP, kernel answers ARP */
    {
        char tn[IFNAMSIZ];
        int tf = tun_open("tun%d", F_TUN | F_NO_PI, tn);
        CHECK("tun_create", tf >= 0 && strcmp(tn, "tun0") == 0);
        if (tf >= 0) {
            CHECK("tun_addr_up", set_addr("tun0", "10.8.0.1", "255.255.255.0") == 0);
            int u2 = socket(AF_INET, SOCK_DGRAM, 0);
            struct sockaddr_in p2 = { .sin_family = AF_INET, .sin_port = htons(7777) };
            inet_pton(AF_INET, "10.8.0.2", &p2.sin_addr);
            sendto(u2, "l3", 2, 0, (struct sockaddr *)&p2, sizeof p2);
            n = rd_timeout(tf, buf, sizeof buf, 3000);
            CHECK("tun_ip_packet", n == 20 + 8 + 2 && buf[0] == 0x45 && buf[9] == 17 &&
                                   memcmp(buf + 16, "\x0a\x08\x00\x02", 4) == 0 && memcmp(buf + 28, "l3", 2) == 0);
            close(u2); close(tf);
        }
    }
    close(u);
    close(fd);
    printf("ns_cases: %d passed, %d failed\n", npass, nfail);
    return r | (nfail != 0);
}


/* ---- TCP in a netns ---------------------------------------------------- */
static unsigned char t_mac[6], p_mac[6] = { 0x02, 0x11, 0x22, 0x33, 0x44, 0x55 };
static const unsigned char T_IP[4] = { 10, 9, 0, 1 }, P_IP[4] = { 10, 9, 0, 2 };

static uint16_t tcp_csum(const unsigned char *src, const unsigned char *dst, const unsigned char *tcp, int len)
{
    uint32_t s = 0;
    for (int i = 0; i < 4; i += 2) { s += (src[i] << 8) | src[i + 1]; s += (dst[i] << 8) | dst[i + 1]; }
    s += 6; s += len;
    for (int i = 0; i + 1 < len; i += 2) s += (tcp[i] << 8) | tcp[i + 1];
    if (len & 1) s += tcp[len - 1] << 8;
    while (s >> 16) s = (s & 0xffff) + (s >> 16);
    return ~s;
}

/* Build an Ethernet+IPv4+TCP frame from the peer to the ns; returns length. */
static int tcp_frame(unsigned char *f, int sport, int dport, uint32_t seq, uint32_t ack,
                     int flags, const void *data, int dlen)
{
    memset(f, 0, 14 + 20 + 20 + dlen);
    memcpy(f, t_mac, 6); memcpy(f + 6, p_mac, 6); f[12] = 8;
    unsigned char *ip = f + 14, *t = ip + 20;
    int tl = 20 + dlen;
    ip[0] = 0x45; ip[2] = (20 + tl) >> 8; ip[3] = (20 + tl) & 0xff; ip[8] = 64; ip[9] = 6;
    memcpy(ip + 12, P_IP, 4); memcpy(ip + 16, T_IP, 4);
    uint16_t c = csum16(ip, 20); ip[10] = c >> 8; ip[11] = c & 0xff;
    t[0] = sport >> 8; t[1] = sport & 0xff; t[2] = dport >> 8; t[3] = dport & 0xff;
    t[4] = seq >> 24; t[5] = seq >> 16; t[6] = seq >> 8; t[7] = seq;
    t[8] = ack >> 24; t[9] = ack >> 16; t[10] = ack >> 8; t[11] = ack;
    t[12] = 5 << 4; t[13] = flags; t[14] = 0xff; t[15] = 0xff;
    if (dlen) memcpy(t + 20, data, dlen);
    c = tcp_csum(P_IP, T_IP, t, tl); t[16] = c >> 8; t[17] = c & 0xff;
    return 14 + 20 + tl;
}

/* Next TCP frame the ns sends (ARP requests for the peer are answered on
 * the way). Returns the frame length or -1 on timeout. */
static int next_tcp(int fd, unsigned char *b, int ms)
{
    for (;;) {
        int n = rd_timeout(fd, b, 2048, ms);
        if (n < 0) return -1;
        if (n >= 42 && b[12] == 0x08 && b[13] == 0x06 && b[21] == 1) {
            memcpy(t_mac, b + 22, 6);
            unsigned char a[42];
            memcpy(a, t_mac, 6); memcpy(a + 6, p_mac, 6);
            a[12] = 0x08; a[13] = 0x06;
            memcpy(a + 14, "\x00\x01\x08\x00\x06\x04\x00\x02", 8);
            memcpy(a + 22, p_mac, 6); memcpy(a + 28, P_IP, 4);
            memcpy(a + 32, t_mac, 6); memcpy(a + 38, T_IP, 4);
            write(fd, a, 42);
            continue;
        }
        if (n >= 54 && b[12] == 8 && b[13] == 0 && b[14 + 9] == 6) return n;
    }
}
#define TSEQ(b) (((uint32_t)(b)[38] << 24) | ((b)[39] << 16) | ((b)[40] << 8) | (b)[41])
#define TACK(b) (((uint32_t)(b)[42] << 24) | ((b)[43] << 16) | ((b)[44] << 8) | (b)[45])
#define TFLAGS(b) ((b)[47])
#define TSPORT(b) (((b)[34] << 8) | (b)[35])

static int tcp_cases(void)
{
    unsigned char b[2048], f[2048];
    char name[IFNAMSIZ];
    int fd, n;
    if (enter_ns() != 0) { printf("tcp_out: FAIL (unshare errno %d)\n", errno); return 1; }
    fd = tun_open("tap%d", F_TAP | F_NO_PI, name);
    if (fd < 0 || set_addr(name, "10.9.0.1", "255.255.255.0") != 0) { printf("tcp_out: FAIL (tap setup fd %d name %s errno %d)\n", fd, name, errno); return 1; }

    /* tcp_out: connect over the tap, SYN on the fd, inject SYN-ACK / data / FIN */
    int s = socket(AF_INET, SOCK_STREAM, 0);
    fcntl(s, F_SETFL, O_NONBLOCK);
    struct sockaddr_in peer = { .sin_family = AF_INET, .sin_port = htons(80) };
    memcpy(&peer.sin_addr, P_IP, 4);
    int rc = connect(s, (struct sockaddr *)&peer, sizeof peer);
    CHECK("tcp_connect_inprogress", rc < 0 && errno == EINPROGRESS);
    n = rc < 0 ? next_tcp(fd, b, 3000) : -1;
    int syn = n > 0 && TFLAGS(b) == 0x02 && memcmp(b + 30, P_IP, 4) == 0 && ((b[36] << 8) | b[37]) == 80;
    CHECK("tcp_out_syn_frame", syn);
    if (!syn) return 1;
    int cport = TSPORT(b);
    uint32_t iss = TSEQ(b), pseq = 5000;
    n = tcp_frame(f, 80, cport, pseq, iss + 1, 0x12, NULL, 0);
    write(fd, f, n);
    struct pollfd pw = { s, POLLOUT, 0 };
    int soerr = -1; socklen_t sl = sizeof soerr;
    CHECK("tcp_out_established", poll(&pw, 1, 3000) == 1 && (pw.revents & POLLOUT) &&
          getsockopt(s, SOL_SOCKET, SO_ERROR, &soerr, &sl) == 0 && soerr == 0);
    n = next_tcp(fd, b, 3000);
    CHECK("tcp_out_ack", n > 0 && (TFLAGS(b) & 0x10) && TACK(b) == pseq + 1);
    CHECK("tcp_out_send", write(s, "hello", 5) == 5);
    n = next_tcp(fd, b, 3000);
    CHECK("tcp_out_data_frame", n == 54 + 5 && memcmp(b + 54, "hello", 5) == 0 && TSEQ(b) == iss + 1);
    n = tcp_frame(f, 80, cport, pseq + 1, iss + 6, 0x18, "world", 5);
    write(fd, f, n);
    {
        char rb[16]; struct pollfd pr = { s, POLLIN, 0 };
        int g = poll(&pr, 1, 3000) == 1 ? read(s, rb, sizeof rb) : -1;
        CHECK("tcp_out_recv", g == 5 && memcmp(rb, "world", 5) == 0);
    }
    n = tcp_frame(f, 80, cport, pseq + 6, iss + 6, 0x11, NULL, 0);
    write(fd, f, n);
    {
        char rb[16]; struct pollfd pr = { s, POLLIN, 0 };
        int g = poll(&pr, 1, 3000) == 1 ? read(s, rb, sizeof rb) : -1;
        CHECK("tcp_out_fin_eof", g == 0);
    }
    close(s);

    /* tcp_in: a listener on the ns, injected SYN -> SYN-ACK, data -> recv */
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in me = { .sin_family = AF_INET, .sin_port = htons(8080) };
    CHECK("tcp_listen", bind(l, (struct sockaddr *)&me, sizeof me) == 0 && listen(l, 4) == 0);
    while (rd_timeout(fd, b, sizeof b, 50) > 0) ;
    n = tcp_frame(f, 33333, 8080, 9000, 0, 0x02, NULL, 0);
    write(fd, f, n);
    n = next_tcp(fd, b, 3000);
    int sa = n > 0 && TFLAGS(b) == 0x12 && TACK(b) == 9001 && TSPORT(b) == 8080;
    CHECK("tcp_in_synack", sa);
    if (!sa) return 1;
    uint32_t iss2 = TSEQ(b);
    n = tcp_frame(f, 33333, 8080, 9001, iss2 + 1, 0x10, NULL, 0);
    write(fd, f, n);
    struct pollfd pl = { l, POLLIN, 0 };
    CHECK("tcp_in_accept_ready", poll(&pl, 1, 3000) == 1 && (pl.revents & POLLIN));
    int a = accept(l, NULL, NULL);
    CHECK("tcp_in_accept", a >= 0);
    if (a < 0) return 1;
    n = tcp_frame(f, 33333, 8080, 9001, iss2 + 1, 0x18, "ping", 4);
    write(fd, f, n);
    {
        char rb[16]; struct pollfd pr = { a, POLLIN, 0 };
        int g = poll(&pr, 1, 3000) == 1 ? read(a, rb, sizeof rb) : -1;
        CHECK("tcp_in_recv", g == 4 && memcmp(rb, "ping", 4) == 0);
    }
    close(a); close(l); close(fd);
    printf("tcp_cases: %d passed, %d failed\n", npass, nfail);
    return nfail != 0;
}

int main(void)
{
    pid_t c = fork();
    if (c == 0) _exit(ns_cases());
    int st = 0;
    waitpid(c, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) { nfail++; printf("ns_cases: FAIL (status %d)\n", st); }
    c = fork();
    if (c == 0) _exit(tcp_cases());
    waitpid(c, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) { nfail++; printf("tcp_cases: FAIL (status %d)\n", st); }

    /* unprivileged, initial namespaces */
    c = fork();
    if (c == 0) {
        if (setgid(1000) != 0 || setuid(1000) != 0) _exit(2);
        int fd = tun_open("tap%d", F_TAP | F_NO_PI, NULL);
        _exit(fd < 0 && errno == EPERM ? 0 : 1);
    }
    waitpid(c, &st, 0);
    CHECK("tun_eperm", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    printf("tuntest: %d passed, %d failed, %d skipped\n", npass, nfail, 0);
    return nfail != 0;
}
