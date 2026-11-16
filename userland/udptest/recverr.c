/* Lane udpgaps: IP_RECVERR -- the socket error queue (MSG_ERRQUEUE,
 * linux/errqueue.h). Included from udptest.c after pmtud2.c (uses
 * pmtu_csum, udp_bound, wait_ev). */
#include <linux/errqueue.h>

#ifndef MSG_ERRQUEUE
#define MSG_ERRQUEUE 0x2000
#endif

struct errq_info {
    int got, have_cmsg, level, type;
    struct sock_extended_err ee;
    struct sockaddr_in off, name;
    int flags;
};

static ssize_t errq_read(int fd, struct errq_info *q, int flags) {
    char data[64], cbuf[128];
    struct iovec iov = { data, sizeof data };
    struct msghdr m = { .msg_name = &q->name, .msg_namelen = sizeof q->name,
                        .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf };
    memset(q, 0, sizeof *q);
    ssize_t n = recvmsg(fd, &m, flags | MSG_ERRQUEUE);
    if (n < 0) return n;
    q->got = 1; q->flags = m.msg_flags;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c)) {
        if (c->cmsg_level == IPPROTO_IP && c->cmsg_type == IP_RECVERR) {
            q->have_cmsg = 1;
            memcpy(&q->ee, CMSG_DATA(c), sizeof q->ee);
            memcpy(&q->off, CMSG_DATA(c) + sizeof q->ee, sizeof q->off);
        }
    }
    return n;
}

/* An unconnected socket with IP_RECVERR sends to a closed port: the ICMP
 * port unreachable lands on the error queue (and only there). */
static void udp_recverr_refused(void) {
    const char *N = "udp_recverr_refused";
    if (skipped(N)) return;
    int u = udp_bound("127.0.0.1", 0, NULL);
    uint16_t cp = closed_port();
    int on = 1;
    if (u < 0 || setsockopt(u, IPPROTO_IP, IP_RECVERR, &on, sizeof on)) { fail(N, "setup: %s", strerror(errno)); return; }
    struct sockaddr_in to = sin4("127.0.0.1", cp);
    sendto(u, "hi", 2, 0, (struct sockaddr *)&to, sizeof to);
    int ev = wait_ev(u, POLLIN, 2000);
    char b[16];
    ssize_t r0 = recv(u, b, sizeof b, MSG_DONTWAIT); int e0 = errno;
    struct errq_info q;
    ssize_t n = errq_read(u, &q, MSG_PEEK);
    int ev1 = wait_ev(u, 0, 200);
    struct errq_info q2;
    ssize_t n2 = errq_read(u, &q2, MSG_DONTWAIT);
    int ev2 = wait_ev(u, 0, 100);
    ssize_t n3 = errq_read(u, &q, MSG_DONTWAIT); int e3 = errno;
    int se = so_error(u);
    close(u);
    if (!(ev & POLLERR)) fail(N, "poll %#x, want POLLERR", ev);
    else if (n < 0 || !q2.got || n2 < 0) fail(N, "MSG_ERRQUEUE: %zd/%zd %s", n, n2, strerror(errno));
    else if (!q2.have_cmsg) fail(N, "no IP_RECVERR cmsg");
    else if (q2.ee.ee_errno != ECONNREFUSED || q2.ee.ee_origin != SO_EE_ORIGIN_ICMP || q2.ee.ee_type != 3 || q2.ee.ee_code != 3)
        fail(N, "ee errno %u origin %u type %u code %u, want %d/2/3/3", q2.ee.ee_errno, q2.ee.ee_origin, q2.ee.ee_type, q2.ee.ee_code, ECONNREFUSED);
    else if (q2.off.sin_family != AF_INET || q2.off.sin_addr.s_addr != htonl(INADDR_LOOPBACK)) fail(N, "offender %08x fam %d", ntohl(q2.off.sin_addr.s_addr), q2.off.sin_family);
    else if (q2.name.sin_port != htons(cp)) fail(N, "msg_name port %u, want %u", ntohs(q2.name.sin_port), cp);
    else if (!(q2.flags & MSG_ERRQUEUE)) fail(N, "msg_flags %#x lacks MSG_ERRQUEUE", q2.flags);
    else if (r0 >= 0 || e0 != ECONNREFUSED) fail(N, "plain recv: %zd %s, want ECONNREFUSED (sk_err armed)", r0, strerror(e0));
    else if (!(ev1 & POLLERR)) fail(N, "peek consumed the entry (poll %#x)", ev1);
    else if (ev2 & POLLERR) fail(N, "POLLERR after the queue drained: %#x", ev2);
    else if (n3 >= 0 || e3 != EAGAIN) fail(N, "empty queue: %zd %s, want EAGAIN", n3, strerror(e3));
    else if (se != 0) fail(N, "SO_ERROR %d after dequeue, want 0", se);
    else pass(N);
}

/* recv/recvfrom/recvmmsg with MSG_ERRQUEUE dequeue the entry too: the
 * quoted payload, the failed destination as source. */
#include <sys/syscall.h>
#include <unistd.h>
static void udp_recverr_recvvariants(void) {
    const char *N = "udp_recverr_recvvariants";
    if (skipped(N)) return;
    int u = udp_bound("127.0.0.1", 0, NULL);
    uint16_t cp = closed_port();
    int on = 1;
    if (u < 0 || setsockopt(u, IPPROTO_IP, IP_RECVERR, &on, sizeof on)) { fail(N, "setup: %s", strerror(errno)); return; }
    struct sockaddr_in to = sin4("127.0.0.1", cp);
    char b[16], why[96] = "";
    for (int k = 0; k < 4 && !why[0]; k++) {
        sendto(u, "hi", 2, 0, (struct sockaddr *)&to, sizeof to);
        wait_ev(u, POLLERR, 2000);
        ssize_t n = -1;
        struct sockaddr_in a; socklen_t al = sizeof a;
        memset(&a, 0, sizeof a);
        if (k == 0) {
            n = recv(u, b, sizeof b, MSG_ERRQUEUE | MSG_DONTWAIT);
            if (n != 2 || memcmp(b, "hi", 2)) snprintf(why, sizeof why, "recv: %zd %s", n, strerror(errno));
        } else if (k == 1) {
            n = recvfrom(u, b, sizeof b, MSG_ERRQUEUE | MSG_DONTWAIT, (struct sockaddr *)&a, &al);
            if (n != 2 || memcmp(b, "hi", 2)) snprintf(why, sizeof why, "recvfrom: %zd %s", n, strerror(errno));
            else if (a.sin_port != htons(cp)) snprintf(why, sizeof why, "recvfrom name port %u", ntohs(a.sin_port));
        } else if (k == 3) {
            /* the raw recvfrom syscall, as a static binary without relibc's recvmsg conversion issues it */
            socklen_t al2 = sizeof a;
            n = syscall(SYS_recvfrom, u, b, sizeof b, MSG_ERRQUEUE | MSG_DONTWAIT, &a, &al2);
            if (n != 2 || memcmp(b, "hi", 2)) snprintf(why, sizeof why, "raw recvfrom: %zd %s", n, strerror(errno));
            else if (a.sin_port != htons(cp)) snprintf(why, sizeof why, "raw recvfrom name port %u", ntohs(a.sin_port));
        } else {
            char cb[128];
            struct iovec iov = { b, sizeof b };
            struct mmsghdr mm; memset(&mm, 0, sizeof mm);
            mm.msg_hdr.msg_iov = &iov; mm.msg_hdr.msg_iovlen = 1;
            mm.msg_hdr.msg_control = cb; mm.msg_hdr.msg_controllen = sizeof cb;
            n = recvmmsg(u, &mm, 1, MSG_ERRQUEUE | MSG_DONTWAIT, NULL);
            if (n != 1 || mm.msg_len != 2 || memcmp(b, "hi", 2)) snprintf(why, sizeof why, "recvmmsg: %zd len %u %s", n, mm.msg_len, strerror(errno));
            else if (mm.msg_hdr.msg_controllen == 0) snprintf(why, sizeof why, "recvmmsg: no cmsg");
        }
    }
    close(u);
    if (why[0]) fail(N, "%s", why); else pass(N);
}

/* Frag-needed on a connected IP_RECVERR socket: EMSGSIZE, ee_info = MTU;
 * and a DF send past the learned path fails locally with an
 * SO_EE_ORIGIN_LOCAL entry carrying the MTU. */
static void udp_recverr_emsgsize(void) {
    const char *N = "udp_recverr_emsgsize";
    if (skipped(N)) return;
    struct sockaddr_in ra, la; socklen_t ll = sizeof la;
    int r = udp_bound("127.0.0.5", 0, &ra), s = socket(AF_INET, SOCK_DGRAM, 0);
    int on = 1, pd = 2; static char d[3000];
    int raw = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    if (r < 0 || s < 0 || raw < 0 || setsockopt(s, IPPROTO_IP, IP_RECVERR, &on, sizeof on)
        || setsockopt(s, IPPROTO_IP, IP_MTU_DISCOVER, &pd, sizeof pd)
        || connect(s, (struct sockaddr *)&ra, sizeof ra) || getsockname(s, (struct sockaddr *)&la, &ll)) {
        fail(N, "setup: %s", strerror(errno)); return;
    }
    uint8_t m[8 + 20 + 8];
    memset(m, 0, sizeof m);
    m[0] = 3; m[1] = 4; m[6] = 1000 >> 8; m[7] = 1000 & 255;
    uint8_t *ip = m + 8;
    ip[0] = 0x45; ip[3] = 28 + 100; ip[6] = 0x40; ip[8] = 64; ip[9] = 17;
    uint32_t src = htonl(INADDR_LOOPBACK);
    memcpy(ip + 12, &src, 4); memcpy(ip + 16, &ra.sin_addr, 4);
    uint16_t c = pmtu_csum(ip, 20); ip[10] = c >> 8; ip[11] = c & 255;
    memcpy(ip + 20, &la.sin_port, 2); memcpy(ip + 22, &ra.sin_port, 2);
    c = pmtu_csum(m, sizeof m); m[2] = c >> 8; m[3] = c & 255;
    struct sockaddr_in to = { .sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    sendto(raw, m, sizeof m, 0, (struct sockaddr *)&to, sizeof to);
    msleep(200);
    int ev = wait_ev(s, 0, 1500);
    struct errq_info q;
    ssize_t n = errq_read(s, &q, MSG_DONTWAIT);
    ssize_t s1 = send(s, d, 3000, 0); int e1 = errno;
    struct errq_info q2;
    ssize_t n2 = errq_read(s, &q2, MSG_DONTWAIT);
    close(s); close(r); close(raw);
    if (!(ev & POLLERR)) fail(N, "poll %#x after frag-needed, want POLLERR", ev);
    else if (n < 0 || !q.have_cmsg) fail(N, "errqueue read: %zd %s", n, strerror(errno));
    else if (q.ee.ee_errno != EMSGSIZE || q.ee.ee_origin != SO_EE_ORIGIN_ICMP || q.ee.ee_type != 3 || q.ee.ee_code != 4 || q.ee.ee_info != 1000)
        fail(N, "ee errno %u origin %u type %u code %u info %u, want 90/2/3/4/1000", q.ee.ee_errno, q.ee.ee_origin, q.ee.ee_type, q.ee.ee_code, q.ee.ee_info);
    else if (s1 >= 0 || e1 != EMSGSIZE) fail(N, "DF send past the path: %zd %s, want EMSGSIZE", s1, strerror(e1));
    else if (n2 < 0 || !q2.have_cmsg) fail(N, "no local EMSGSIZE entry: %zd %s", n2, strerror(errno));
    else if (q2.ee.ee_errno != EMSGSIZE || q2.ee.ee_origin != SO_EE_ORIGIN_LOCAL || q2.ee.ee_info != 1000)
        fail(N, "local ee errno %u origin %u info %u, want 90/1/1000", q2.ee.ee_errno, q2.ee.ee_origin, q2.ee.ee_info);
    else pass(N);
}

/* Lane udp6err: ICMPv6 destination-unreachable / time-exceeded reach a
 * connected UDP6 socket: IPV6_RECVERR queue entry (origin ICMP6, ee_type/
 * ee_code = the ICMPv6 type/code, Linux icmpv6_err_convert errno) and, for
 * fatal codes without RECVERR, the pending socket error. */
static int icmp6_inject(int s, struct sockaddr_in6 *ra, int ty, int code) {
    struct sockaddr_in6 la; socklen_t ll = sizeof la;
    int raw = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
    if (raw < 0 || getsockname(s, (struct sockaddr *)&la, &ll)) return -1;
    uint8_t m[8 + 40 + 8];
    memset(m, 0, sizeof m);
    m[0] = ty; m[1] = code;
    uint8_t *ip = m + 8;
    ip[0] = 0x60; ip[4] = 0; ip[5] = 8; ip[6] = 17; ip[7] = 64;
    memcpy(ip + 8, &in6addr_loopback, 16); memcpy(ip + 24, &ra->sin6_addr, 16);
    memcpy(ip + 40, &la.sin6_port, 2); memcpy(ip + 42, &ra->sin6_port, 2);
    uint16_t c = csum6(&in6addr_loopback, &in6addr_loopback, m, sizeof m); m[2] = c >> 8; m[3] = c & 255;
    struct sockaddr_in6 to = { .sin6_family = AF_INET6, .sin6_addr = IN6ADDR_LOOPBACK_INIT };
    int on2 = 2; setsockopt(raw, IPPROTO_IPV6, IPV6_CHECKSUM, &on2, sizeof on2);
    ssize_t r = sendto(raw, m, sizeof m, 0, (struct sockaddr *)&to, sizeof to);
    msleep(200);
    close(raw);
    return r == (ssize_t)sizeof m ? 0 : -1;
}

static void udp6_recverr_one(const char *N, int ty, int code, int want_errno, int fatal) {
    if (skipped(N)) return;
    int r = socket(AF_INET6, SOCK_DGRAM, 0), s = socket(AF_INET6, SOCK_DGRAM, 0), on = 1;
    struct sockaddr_in6 ra = { .sin6_family = AF_INET6, .sin6_addr = IN6ADDR_LOOPBACK_INIT };
    socklen_t rl = sizeof ra;
    if (r < 0 || s < 0 || bind(r, (struct sockaddr *)&ra, sizeof ra) || getsockname(r, (struct sockaddr *)&ra, &rl)
        || setsockopt(s, IPPROTO_IPV6, IPV6_RECVERR, &on, sizeof on)
        || connect(s, (struct sockaddr *)&ra, sizeof ra)) { fail(N, "setup: %s", strerror(errno)); return; }
    if (icmp6_inject(s, &ra, ty, code)) { fail(N, "inject: %s", strerror(errno)); return; }
    int ev = wait_ev(s, 0, 1500);
    char data[64], cbuf[128], b[8];
    struct sockaddr_in6 nm;
    struct iovec iov = { data, sizeof data };
    struct msghdr m = { .msg_name = &nm, .msg_namelen = sizeof nm, .msg_iov = &iov, .msg_iovlen = 1,
                        .msg_control = cbuf, .msg_controllen = sizeof cbuf };
    /* plain recv first: reading the queue clears the pending error */
    ssize_t r0 = recv(s, b, sizeof b, MSG_DONTWAIT); int e0 = errno;
    ssize_t n = recvmsg(s, &m, MSG_ERRQUEUE | MSG_DONTWAIT);
    int have = 0; struct sock_extended_err ee; memset(&ee, 0, sizeof ee);
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); n >= 0 && c; c = CMSG_NXTHDR(&m, c))
        if (c->cmsg_level == IPPROTO_IPV6 && c->cmsg_type == IPV6_RECVERR) { have = 1; memcpy(&ee, CMSG_DATA(c), sizeof ee); }
    close(s); close(r);
    if (!(ev & POLLERR)) fail(N, "poll %#x, want POLLERR", ev);
    else if (n < 0 || !have) fail(N, "no IPV6_RECVERR entry: %zd %s", n, strerror(errno));
    else if (ee.ee_errno != want_errno || ee.ee_origin != SO_EE_ORIGIN_ICMP6 || ee.ee_type != ty || ee.ee_code != code)
        fail(N, "ee errno %u origin %u type %u code %u, want %d/3/%d/%d", ee.ee_errno, ee.ee_origin, ee.ee_type, ee.ee_code, want_errno, ty, code);
    else if (fatal && (r0 >= 0 || e0 != want_errno)) fail(N, "plain recv: %zd %s, want %d", r0, strerror(e0), want_errno);
    else pass(N);
}

static void udp6_recverr_dstunreach(void) {
    udp6_recverr_one("udp6_recverr_port_unreach", 1, 4, ECONNREFUSED, 1);
    udp6_recverr_one("udp6_recverr_no_route", 1, 0, ENETUNREACH, 0);
    udp6_recverr_one("udp6_recverr_adm_prohibited", 1, 1, EACCES, 1);
    udp6_recverr_one("udp6_recverr_time_exceeded", 3, 0, EHOSTUNREACH, 0);
}

/* Without IPV6_RECVERR a fatal code still fails the connected socket. */
static void udp6_icmp6_refused_plain(void) {
    const char *N = "udp6_icmp6_refused_plain";
    if (skipped(N)) return;
    int r = socket(AF_INET6, SOCK_DGRAM, 0), s = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 ra = { .sin6_family = AF_INET6, .sin6_addr = IN6ADDR_LOOPBACK_INIT };
    socklen_t rl = sizeof ra;
    if (r < 0 || s < 0 || bind(r, (struct sockaddr *)&ra, sizeof ra) || getsockname(r, (struct sockaddr *)&ra, &rl)
        || connect(s, (struct sockaddr *)&ra, sizeof ra)) { fail(N, "setup: %s", strerror(errno)); return; }
    if (icmp6_inject(s, &ra, 1, 4)) { fail(N, "inject: %s", strerror(errno)); return; }
    char b[8];
    ssize_t r0 = recv(s, b, sizeof b, MSG_DONTWAIT); int e0 = errno;
    close(s); close(r);
    if (r0 >= 0 || e0 != ECONNREFUSED) fail(N, "recv %zd %s, want ECONNREFUSED", r0, strerror(e0));
    else pass(N);
}
