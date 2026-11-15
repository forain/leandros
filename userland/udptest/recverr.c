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
