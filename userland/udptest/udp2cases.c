/* ── lane udp2: ICMP errors, multicast, broadcast, cmsgs, disconnect ───── */

#ifndef UDP_GRO
#define UDP_GRO 104
#endif
#ifndef IP_RECVTOS
#define IP_RECVTOS 13
#endif
#ifndef IP_MULTICAST_ALL
#define IP_MULTICAST_ALL 49
#endif

/* This host's address on the default route's interface ("" without one):
 * the source a UDP socket connected to the gateway gets. */
static int nic_addr(char *ip, size_t n) {
    char gw[64];
    if (!default_gw(gw, sizeof gw)) return 0;
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in g = sin4(gw, 53), me;
    socklen_t l = sizeof me;
    int ok = s >= 0 && connect(s, (struct sockaddr *)&g, sizeof g) == 0
             && getsockname(s, (struct sockaddr *)&me, &l) == 0 && me.sin_addr.s_addr != 0;
    if (ok) snprintf(ip, n, "%s", inet_ntoa(me.sin_addr));
    if (s >= 0) close(s);
    return ok;
}

/* recvmsg with a control buffer; the datagram length, -1 on error. Fills
 * pktinfo / tos / ttl / gro (-1 when that cmsg is absent). */
struct rxinfo { int have_pi; struct in_pktinfo pi; int tos, ttl, gro; int flags; };
static ssize_t recv_info(int fd, void *buf, size_t cap, struct rxinfo *ri, int flags) {
    char ctl[256];
    struct iovec iov = { buf, cap };
    struct sockaddr_in from;
    struct msghdr m;
    memset(&m, 0, sizeof m);
    m.msg_name = &from; m.msg_namelen = sizeof from;
    m.msg_iov = &iov; m.msg_iovlen = 1;
    m.msg_control = ctl; m.msg_controllen = sizeof ctl;
    memset(ri, 0, sizeof *ri);
    ri->tos = ri->ttl = ri->gro = -1;
    ssize_t n = recvmsg(fd, &m, flags);
    if (n < 0) return n;
    ri->flags = m.msg_flags;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c)) {
        if (c->cmsg_level == IPPROTO_IP && c->cmsg_type == IP_PKTINFO) { ri->have_pi = 1; memcpy(&ri->pi, CMSG_DATA(c), sizeof ri->pi); }
        else if (c->cmsg_level == IPPROTO_IP && c->cmsg_type == IP_TOS) ri->tos = *(unsigned char *)CMSG_DATA(c);
        else if (c->cmsg_level == IPPROTO_IP && c->cmsg_type == IP_TTL) memcpy(&ri->ttl, CMSG_DATA(c), sizeof(int));
        else if (c->cmsg_level == SOL_UDP && c->cmsg_type == UDP_GRO) memcpy(&ri->gro, CMSG_DATA(c), sizeof(int));
    }
    return n;
}

static int so_error(int fd) {
    int e = -1; socklen_t l = sizeof e;
    if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &e, &l) < 0) return -1;
    return e;
}

/* A UDP port on 127.0.0.1 that nothing listens on. */
static uint16_t closed_port(void) {
    struct sockaddr_in a;
    int t = udp_bound("127.0.0.1", 0, &a);
    close(t);
    return ntohs(a.sin_port);
}

/* Connected UDP + ICMP port unreachable: Linux __udp4_lib_err. */
static void udp_icmp_refused(void) {
    const char *N = "udp_icmp_refused";
    if (skipped(N)) return;
    /* Both sockets first: a port just freed may be handed out again. */
    int c = udp_bound("127.0.0.1", 0, NULL);
    int u = udp_bound("127.0.0.1", 0, NULL);
    uint16_t cp = closed_port();
    struct sockaddr_in to = sin4("127.0.0.1", cp);
    if (c < 0 || u < 0 || connect(c, (struct sockaddr *)&to, sizeof to) < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    char b[64];
    send(c, "x", 1, 0);
    long long t0 = now_ms();
    int ev0 = wait_ev(c, POLLIN, 2000);
    ssize_t r1 = recv(c, b, sizeof b, MSG_DONTWAIT); int e1 = errno;
    long long dt = now_ms() - t0;
    ssize_t r2 = recv(c, b, sizeof b, MSG_DONTWAIT); int e2 = errno;
    /* The error again, via poll and SO_ERROR (read once). */
    send(c, "y", 1, 0);
    int ev = wait_ev(c, POLLIN, 1000);
    int se1 = so_error(c), se2 = so_error(c);
    /* And via the next send. */
    send(c, "z", 1, 0);
    msleep(100);
    ssize_t s3 = send(c, "w", 1, 0); int e3 = errno;
    ssize_t s4 = send(c, "v", 1, 0);
    /* An unconnected socket never sees one (no IP_RECVERR). */
    send_to(u, "x", 1, &to);
    msleep(100);
    ssize_t r5 = recv(u, b, sizeof b, MSG_DONTWAIT); int e5 = errno;
    int se5 = so_error(u);
    close(c); close(u);
    if (r1 >= 0 || e1 != ECONNREFUSED) fail(N, "recv after the ICMP: %zd %s (want ECONNREFUSED; poll %#x)", r1, strerror(e1), ev0);
    else if (dt > 1500) fail(N, "ECONNREFUSED after %lld ms", dt);
    else if (r2 >= 0 || e2 != EAGAIN) fail(N, "second recv: %zd %s (want EAGAIN: the error is read once)", r2, strerror(e2));
    else if (!(ev & POLLERR)) fail(N, "poll after the ICMP: %#x, want POLLERR", ev);
    else if (se1 != ECONNREFUSED || se2 != 0) fail(N, "SO_ERROR %d then %d, want %d then 0", se1, se2, ECONNREFUSED);
    else if (s3 >= 0 || e3 != ECONNREFUSED) fail(N, "send after the ICMP: %zd %s (want ECONNREFUSED)", s3, strerror(e3));
    else if (s4 != 1) fail(N, "send after the error was read: %zd", s4);
    else if (r5 >= 0 || e5 != EAGAIN || se5 != 0) fail(N, "unconnected socket: recv %zd %s, SO_ERROR %d", r5, strerror(e5), se5);
    else pass(N);
}

/* /proc/net/igmp lists `group` (as Linux prints it) on some interface. */
static int igmp_listed(const char *group) {
    struct in_addr g; inet_pton(AF_INET, group, &g);
    char want[16]; snprintf(want, sizeof want, "%08X", (unsigned)g.s_addr);
    FILE *f = fopen("/proc/net/igmp", "r");
    if (!f) return -1;
    char line[256]; int found = 0;
    while (fgets(line, sizeof line, f)) if (strstr(line, want)) found = 1;
    fclose(f);
    return found;
}

static int join(int fd, const char *group, const char *ifaddr, int drop) {
    struct ip_mreq mr;
    inet_pton(AF_INET, group, &mr.imr_multiaddr);
    inet_pton(AF_INET, ifaddr, &mr.imr_interface);
    return setsockopt(fd, IPPROTO_IP, drop ? IP_DROP_MEMBERSHIP : IP_ADD_MEMBERSHIP, &mr, sizeof mr);
}

static int bound_reuse(uint16_t port, int opt) {
    int fd = socket(AF_INET, SOCK_DGRAM, 0), one = 1;
    setsockopt(fd, SOL_SOCKET, opt, &one, sizeof one);
    struct sockaddr_in a = sin4("0.0.0.0", port);
    if (bind(fd, (struct sockaddr *)&a, sizeof a) < 0) { close(fd); return -1; }
    return fd;
}

/* Count datagrams with payload `want` arriving on fd within ms. */
static int got(int fd, const char *want, int ms) {
    int k = 0; char b[256];
    long long end = now_ms() + ms;
    /* At least one look: with ms = 1 the clock could pass `end` before the
     * first check, and a datagram already queued counted as missing. */
    do {
        if (!wait_ev(fd, POLLIN, 20)) continue;
        ssize_t n;
        while ((n = recv(fd, b, sizeof b - 1, MSG_DONTWAIT)) >= 0) {
            b[n] = 0;
            if (!strcmp(b, want)) k++;
        }
    } while (now_ms() < end);
    return k;
}

/* Multicast on the NIC: memberships, every socket on the port gets a copy,
 * IP_MULTICAST_LOOP, TTL, IP_PKTINFO, /proc/net/igmp. */
static void udp_mcast_loop(void) {
    const char *N = "udp_mcast_loop";
    if (skipped(N)) return;
    char me[64];
    if (!nic_addr(me, sizeof me)) { skip(N, "no default route"); return; }
    const char *G = "239.255.42.99";
    uint16_t port = 47011;
    int a = bound_reuse(port, SO_REUSEADDR), b = bound_reuse(port, SO_REUSEADDR);
    int s = socket(AF_INET, SOCK_DGRAM, 0), one = 1;
    if (a < 0 || b < 0 || s < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    int ttl = -1, loop = -1, all = -1; socklen_t l = sizeof ttl;
    getsockopt(s, IPPROTO_IP, IP_MULTICAST_TTL, &ttl, &l); l = sizeof loop;
    getsockopt(s, IPPROTO_IP, IP_MULTICAST_LOOP, &loop, &l); l = sizeof all;
    getsockopt(s, IPPROTO_IP, IP_MULTICAST_ALL, &all, &l);
    int listed0 = igmp_listed(G);
    int ja = join(a, G, "0.0.0.0", 0), jb = join(b, G, me, 0);
    int jdup = join(a, G, "0.0.0.0", 0), edup = errno;
    int jbad = join(a, "10.1.2.3", "0.0.0.0", 0), ebad = errno;
    int listed1 = igmp_listed(G);
    setsockopt(a, IPPROTO_IP, IP_PKTINFO, &one, sizeof one);
    setsockopt(a, IPPROTO_IP, IP_RECVTTL, &one, sizeof one);
    struct sockaddr_in to = sin4(G, port);
    send_to(s, "m1", 2, &to);
    char buf[64]; struct rxinfo ri;
    int ea = wait_ev(a, POLLIN, 1000);
    ssize_t na = ea ? recv_info(a, buf, sizeof buf, &ri, MSG_DONTWAIT) : -1;
    int nb = got(b, "m1", 300);
    /* Loop off: nobody here hears it. */
    int zero = 0;
    setsockopt(s, IPPROTO_IP, IP_MULTICAST_LOOP, &zero, sizeof zero);
    send_to(s, "m2", 2, &to);
    int na2 = got(a, "m2", 300), nb2 = got(b, "m2", 1);
    setsockopt(s, IPPROTO_IP, IP_MULTICAST_LOOP, &one, sizeof one);
    /* B leaves; A is still a member, so the interface is, and B, bound to
     * the port, still hears the group (IP_MULTICAST_ALL). */
    int lb = join(b, G, me, 1);
    int lb2 = join(b, G, me, 1), elb2 = errno;
    send_to(s, "m3", 2, &to);
    int na3 = got(a, "m3", 300), nb3 = got(b, "m3", 1);
    int la = join(a, G, "0.0.0.0", 1);
    int listed2 = igmp_listed(G);
    send_to(s, "m4", 2, &to);
    int na4 = got(a, "m4", 300);
    close(a); close(b); close(s);
    struct in_addr me_a; inet_pton(AF_INET, me, &me_a);
    if (ttl != 1 || loop != 1 || all != 1) fail(N, "defaults: MULTICAST_TTL %d LOOP %d ALL %d, want 1 1 1", ttl, loop, all);
    else if (ja || jb) fail(N, "IP_ADD_MEMBERSHIP: %d %d %s", ja, jb, strerror(errno));
    else if (jdup == 0 || edup != EADDRINUSE) fail(N, "second join: %d %s, want EADDRINUSE", jdup, strerror(edup));
    else if (jbad == 0 || ebad != EINVAL) fail(N, "join of a unicast address: %d %s, want EINVAL", jbad, strerror(ebad));
    else if (listed0 == 1 || (listed1 != 1 && listed1 != -1)) fail(N, "/proc/net/igmp: before %d, joined %d", listed0, listed1);
    else if (na != 2 || nb != 1) fail(N, "looped datagram: A %zd B %d, want 2 and 1", na, nb);
    else if (!ri.have_pi || strcmp(inet_ntoa(ri.pi.ipi_addr), G) || ri.pi.ipi_ifindex <= 1
             || ri.pi.ipi_spec_dst.s_addr != me_a.s_addr)
        fail(N, "IP_PKTINFO: have %d addr %s ifindex %d spec_dst %s", ri.have_pi, inet_ntoa(ri.pi.ipi_addr),
             ri.pi.ipi_ifindex, inet_ntoa(ri.pi.ipi_spec_dst));
    else if (ri.ttl != 1) fail(N, "IP_TTL cmsg %d, want 1 (multicast TTL)", ri.ttl);
    else if (na2 || nb2) fail(N, "IP_MULTICAST_LOOP 0: still looped (%d %d)", na2, nb2);
    else if (lb || lb2 == 0 || elb2 != EADDRNOTAVAIL) fail(N, "IP_DROP_MEMBERSHIP: %d, again %d %s", lb, lb2, strerror(elb2));
    else if (na3 != 1 || nb3 != 1) fail(N, "after B left: A %d B %d, want 1 1", na3, nb3);
    else if (la || listed2 == 1) fail(N, "after both left: drop %d, listed %d", la, listed2);
    else if (na4) fail(N, "datagram after the interface left the group");
    else pass(N);
}

#ifndef IP_UNBLOCK_SOURCE
#define IP_UNBLOCK_SOURCE 37
#define IP_BLOCK_SOURCE 38
#define IP_ADD_SOURCE_MEMBERSHIP 39
#define IP_DROP_SOURCE_MEMBERSHIP 40
#endif
#define T_MCAST_JOIN_SOURCE_GROUP 46
#define T_MCAST_LEAVE_SOURCE_GROUP 47
#define T_MCAST_BLOCK_SOURCE 43

/* One of the four ip_mreq_source options. */
static int srcopt(int fd, int opt, const char *group, const char *src) {
    struct { struct in_addr g, i, s; } m;
    inet_pton(AF_INET, group, &m.g); m.i.s_addr = 0; inet_pton(AF_INET, src, &m.s);
    return setsockopt(fd, IPPROTO_IP, opt, &m, sizeof m);
}

/* struct group_source_req by hand (layout: u32 iface, 128-byte group, 128-byte source). */
static int gsropt(int fd, int opt, const char *group, const char *src) {
    unsigned char r[264]; memset(r, 0, sizeof r);
    struct sockaddr_in g = sin4(group, 0), s = sin4(src, 0);
    memcpy(r + 8, &g, sizeof g); memcpy(r + 136, &s, sizeof s);
    return setsockopt(fd, IPPROTO_IP, opt, r, sizeof r);
}

/* Source-specific multicast: the per-socket source filter. */
static void udp_mcast_ssm(void) {
    const char *N = "udp_mcast_ssm";
    if (skipped(N)) return;
    char me[64];
    if (!nic_addr(me, sizeof me)) { skip(N, "no default route"); return; }
    const char *G = "239.255.42.98", *OTHER = "192.0.2.77";
    uint16_t port = 47021;
    int a = bound_reuse(port, SO_REUSEADDR), b = bound_reuse(port, SO_REUSEADDR);
    int c = bound_reuse(port, SO_REUSEADDR), d = bound_reuse(port, SO_REUSEADDR);
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    if (a < 0 || b < 0 || c < 0 || d < 0 || s < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    struct sockaddr_in to = sin4(G, port);
    /* A includes our source, B only someone else, C joins any-source and blocks us, D joins via MCAST_*. */
    int ja = srcopt(a, IP_ADD_SOURCE_MEMBERSHIP, G, me);
    int jb = srcopt(b, IP_ADD_SOURCE_MEMBERSHIP, G, OTHER);
    int jc = join(c, G, "0.0.0.0", 0);
    int bc = srcopt(c, IP_BLOCK_SOURCE, G, me);
    int jd = gsropt(d, T_MCAST_JOIN_SOURCE_GROUP, G, me);
    send_to(s, "s1", 2, &to);
    int na = got(a, "s1", 600), nb = got(b, "s1", 1), nc = got(c, "s1", 1), nd = got(d, "s1", 1);
    /* Unblocking C lets us through; dropping A's only source leaves the group. */
    int uc = srcopt(c, IP_UNBLOCK_SOURCE, G, me);
    int uc2 = srcopt(c, IP_UNBLOCK_SOURCE, G, me), euc2 = errno;
    int da = srcopt(a, IP_DROP_SOURCE_MEMBERSHIP, G, me);
    int da2 = srcopt(a, IP_DROP_SOURCE_MEMBERSHIP, G, me), eda2 = errno;
    send_to(s, "s2", 2, &to);
    int nc2 = got(c, "s2", 600), na2 = got(a, "s2", 1), nd2 = got(d, "s2", 1);
    /* Mixing: include sources on B, then a block is EINVAL; a duplicate add is EADDRINUSE;
     * an ASM join over an SSM one too. */
    int mix = srcopt(b, IP_BLOCK_SOURCE, G, me), emix = errno;
    int dup = srcopt(b, IP_ADD_SOURCE_MEMBERSHIP, G, OTHER), edup = errno;
    int asm_ = join(b, G, "0.0.0.0", 0), easm = errno;
    /* Block-then-add on C (exclude list non-empty) is EINVAL. */
    srcopt(c, IP_BLOCK_SOURCE, G, OTHER);
    int mix2 = srcopt(c, IP_ADD_SOURCE_MEMBERSHIP, G, me), emix2 = errno;
    /* Source ops without a membership: block/drop are EINVAL, MCAST_LEAVE of nothing is an error. */
    int e1 = srcopt(s, IP_BLOCK_SOURCE, G, me), ee1 = errno;
    int e2 = srcopt(s, IP_DROP_SOURCE_MEMBERSHIP, G, me), ee2 = errno;
    int e3 = gsropt(s, T_MCAST_LEAVE_SOURCE_GROUP, G, me), ee3 = errno;
    int e4 = srcopt(s, IP_ADD_SOURCE_MEMBERSHIP, "10.1.2.3", me), ee4 = errno;
    int dd = gsropt(d, T_MCAST_LEAVE_SOURCE_GROUP, G, me);
    close(a); close(b); close(c); close(d); close(s);
    if (ja || jb || jc || bc || jd) fail(N, "setup options: %d %d %d %d %d (%s)", ja, jb, jc, bc, jd, strerror(errno));
    else if (na != 1 || nb != 0 || nc != 0 || nd != 1) fail(N, "filtered rx: A %d B %d C %d D %d, want 1 0 0 1", na, nb, nc, nd);
    else if (uc || uc2 == 0 || euc2 != EADDRNOTAVAIL) fail(N, "unblock: %d, again %d %s, want 0 / EADDRNOTAVAIL", uc, uc2, strerror(euc2));
    else if (da || da2 == 0) fail(N, "drop source: %d, again %d %s", da, da2, strerror(eda2));
    else if (nc2 != 1 || na2 != 0 || nd2 != 1) fail(N, "after unblock/drop: C %d A %d D %d, want 1 0 1", nc2, na2, nd2);
    else if (mix == 0 || emix != EINVAL) fail(N, "block on an include filter: %d %s, want EINVAL", mix, strerror(emix));
    else if (dup == 0 || edup != EADDRINUSE) fail(N, "duplicate source: %d %s, want EADDRINUSE", dup, strerror(edup));
    else if (asm_ == 0 || easm != EADDRINUSE) fail(N, "ASM join over SSM: %d %s, want EADDRINUSE", asm_, strerror(easm));
    else if (mix2 == 0 || emix2 != EINVAL) fail(N, "add on an exclude filter: %d %s, want EINVAL", mix2, strerror(emix2));
    else if (e1 == 0 || ee1 != EINVAL || e2 == 0 || ee2 != EINVAL) fail(N, "no membership: block %d %s, drop %d %s, want EINVAL", e1, strerror(ee1), e2, strerror(ee2));
    else if (e3 == 0 || (ee3 != EINVAL && ee3 != EADDRNOTAVAIL)) fail(N, "MCAST_LEAVE_SOURCE_GROUP without join: %d %s", e3, strerror(ee3));
    else if (e4 == 0 || ee4 != EINVAL) fail(N, "source add on a non-source address: %d %s", e4, strerror(ee4));
    else if (dd) fail(N, "MCAST_LEAVE_SOURCE_GROUP: %d %s", dd, strerror(errno));
    else pass(N);
}

/* SO_BROADCAST is required (EACCES); every socket on the port gets the
 * host's own broadcast (Linux loops broadcast back). */
static void udp_broadcast(void) {
    const char *N = "udp_broadcast";
    if (skipped(N)) return;
    char me[64];
    uint16_t port = 47013;
    int s = socket(AF_INET, SOCK_DGRAM, 0), one = 1;
    struct sockaddr_in all = sin4("255.255.255.255", port), lob = sin4("127.255.255.255", port);
    ssize_t s1 = send_to(s, "b", 1, &all); int e1 = errno;
    ssize_t s2 = send_to(s, "b", 1, &lob); int e2 = errno;
    int c1 = connect(s, (struct sockaddr *)&all, sizeof all); int ec = errno;
    int gs = 0; socklen_t l = sizeof gs;
    getsockopt(s, SOL_SOCKET, SO_BROADCAST, &gs, &l);
    if (s1 >= 0 || e1 != EACCES || s2 >= 0 || e2 != EACCES || c1 == 0 || ec != EACCES || gs != 0) {
        fail(N, "without SO_BROADCAST: send %zd %s, to 127.255.255.255 %zd %s, connect %d %s",
             s1, strerror(e1), s2, strerror(e2), c1, strerror(ec));
        close(s); return;
    }
    if (!nic_addr(me, sizeof me)) { close(s); pass(N); return; }
    int r1 = bound_reuse(port, SO_REUSEADDR), r2 = bound_reuse(port, SO_REUSEADDR);
    setsockopt(s, SOL_SOCKET, SO_BROADCAST, &one, sizeof one);
    ssize_t s3 = send_to(s, "bc", 2, &all); int e3 = errno;
    int n1 = got(r1, "bc", 600), n2 = got(r2, "bc", 1);
    close(s); close(r1); close(r2);
    if (s3 != 2) fail(N, "with SO_BROADCAST: %zd %s", s3, strerror(e3));
    else if (n1 != 1 || n2 != 1) fail(N, "own broadcast: %d and %d copies, want 1 each", n1, n2);
    else pass(N);
}

/* Unicast to sockets sharing a port: SO_REUSEADDR -> the last bound;
 * SO_REUSEPORT -> spread by flow; a connected socket gets its peer's. */
static void udp_reuse_unicast(void) {
    const char *N = "udp_reuse_unicast";
    if (skipped(N)) return;
    uint16_t port = 47015;
    int a = bound_reuse(port, SO_REUSEADDR), b = bound_reuse(port, SO_REUSEADDR);
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in to = sin4("127.0.0.1", port);
    if (a < 0 || b < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    for (int i = 0; i < 4; i++) send_to(s, "u", 1, &to);
    int na = got(a, "u", 300), nb = got(b, "u", 1);
    close(a); close(b); close(s);
    /* SO_REUSEPORT: 24 senders (24 flows). */
    port = 47017;
    int p = bound_reuse(port, SO_REUSEPORT), q = bound_reuse(port, SO_REUSEPORT);
    to = sin4("127.0.0.1", port);
    /* All open at once: 24 different source ports (a port just freed may
     * be handed out again). */
    int ts[24];
    for (int i = 0; i < 24; i++) {
        ts[i] = socket(AF_INET, SOCK_DGRAM, 0);
        send_to(ts[i], "r", 1, &to);
        send_to(ts[i], "r", 1, &to);
    }
    int np = got(p, "r", 300), nq = got(q, "r", 1);
    for (int i = 0; i < 24; i++) close(ts[i]);
    close(p); close(q);
    if (p < 0 || q < 0) fail(N, "SO_REUSEPORT bind: %s", strerror(errno));
    else if (na != 0 || nb != 4) fail(N, "SO_REUSEADDR: first bound got %d, last bound %d (want 0, 4)", na, nb);
    else if (np + nq != 48 || np == 0 || nq == 0 || np % 2 || nq % 2)
        fail(N, "SO_REUSEPORT: %d + %d of 48 (want both > 0, a flow on one socket)", np, nq);
    else pass(N);
}

/* IP_PKTINFO / IP_RECVTOS / IP_RECVTTL on loopback, IP_TOS and IP_TTL on send. */
static void udp_cmsgs(void) {
    const char *N = "udp_cmsgs";
    if (skipped(N)) return;
    struct sockaddr_in ra;
    int r = udp_bound("127.0.0.1", 0, &ra), s = socket(AF_INET, SOCK_DGRAM, 0), one = 1;
    int tos = 0xb8, ttl = 33;
    setsockopt(r, IPPROTO_IP, IP_PKTINFO, &one, sizeof one);
    setsockopt(r, IPPROTO_IP, IP_RECVTOS, &one, sizeof one);
    setsockopt(r, IPPROTO_IP, IP_RECVTTL, &one, sizeof one);
    setsockopt(s, IPPROTO_IP, IP_TOS, &tos, sizeof tos);
    setsockopt(s, IPPROTO_IP, IP_TTL, &ttl, sizeof ttl);
    int gt = 0, gl = 0; socklen_t l = sizeof gt;
    getsockopt(s, IPPROTO_IP, IP_TOS, &gt, &l); l = sizeof gl;
    getsockopt(s, IPPROTO_IP, IP_TTL, &gl, &l);
    send_to(s, "c", 1, &ra);
    char b[16]; struct rxinfo ri;
    wait_ev(r, POLLIN, 1000);
    ssize_t n = recv_info(r, b, sizeof b, &ri, MSG_DONTWAIT);
    /* No cmsg at all once the options are off. */
    int zero = 0;
    setsockopt(r, IPPROTO_IP, IP_PKTINFO, &zero, sizeof zero);
    setsockopt(r, IPPROTO_IP, IP_RECVTOS, &zero, sizeof zero);
    setsockopt(r, IPPROTO_IP, IP_RECVTTL, &zero, sizeof zero);
    send_to(s, "d", 1, &ra);
    wait_ev(r, POLLIN, 1000);
    struct rxinfo r2;
    ssize_t n2 = recv_info(r, b, sizeof b, &r2, MSG_DONTWAIT);
    close(r); close(s);
    if (gt != tos || gl != ttl) fail(N, "getsockopt IP_TOS %#x IP_TTL %d", gt, gl);
    else if (n != 1) fail(N, "recvmsg: %zd %s", n, strerror(errno));
    else if (!ri.have_pi || ri.pi.ipi_ifindex != 1 || strcmp(inet_ntoa(ri.pi.ipi_addr), "127.0.0.1")
             || strcmp(inet_ntoa(ri.pi.ipi_spec_dst), "127.0.0.1"))
        fail(N, "IP_PKTINFO: have %d ifindex %d addr %s", ri.have_pi, ri.pi.ipi_ifindex, inet_ntoa(ri.pi.ipi_addr));
    else if (ri.tos != tos) fail(N, "IP_TOS cmsg %d, want %d", ri.tos, tos);
    else if (ri.ttl != ttl) fail(N, "IP_TTL cmsg %d, want %d", ri.ttl, ttl);
    else if (n2 != 1 || r2.have_pi || r2.tos != -1 || r2.ttl != -1) fail(N, "options off: %zd pi %d tos %d ttl %d", n2, r2.have_pi, r2.tos, r2.ttl);
    else pass(N);
}

/* UDP_GRO: a UDP_SEGMENT send reaches a UDP_GRO socket as one buffer with
 * the segment size in a cmsg (Linux keeps a GSO packet whole for it), and
 * as separate datagrams for a socket without UDP_GRO. */
static void udp_gro(void) {
    const char *N = "udp_gro";
    if (skipped(N)) return;
    struct sockaddr_in ra;
    int r = udp_bound("127.0.0.1", 0, &ra), s = socket(AF_INET, SOCK_DGRAM, 0), one = 1, seg = 1000;
    int sg = setsockopt(r, SOL_UDP, UDP_GRO, &one, sizeof one);
    int v = 0; socklen_t l = sizeof v;
    getsockopt(r, SOL_UDP, UDP_GRO, &v, &l);
    setsockopt(s, SOL_UDP, UDP_SEGMENT, &seg, sizeof seg);
    static char big[5500];
    for (int i = 0; i < 5500; i++) big[i] = (char)(i % 251);
    ssize_t sent = send_to(s, big, sizeof big, &ra);
    msleep(100);
    static char rb[65536]; struct rxinfo ri;
    ssize_t n = recv_info(r, rb, sizeof rb, &ri, MSG_DONTWAIT);
    int same = n == 5500 && !memcmp(rb, big, 5500);
    drain(r);
    close(r); close(s);
    if (sg || v != 1) fail(N, "UDP_GRO setsockopt %d, reads back %d", sg, v);
    else if (sent != 5500) fail(N, "GSO send: %zd %s", sent, strerror(errno));
    else if (!same || ri.gro != 1000) fail(N, "recvmsg %zd bytes (intact %d), UDP_GRO cmsg %d; want 5500 and 1000", n, same, ri.gro);
    else pass(N);
}

/* connect(AF_UNSPEC): a port bind(0)/autobind picked is given up; datagrams
 * queued from the old peer survive a re-connect (Linux). */
static void udp_disconnect_port(void) {
    const char *N = "udp_disconnect_port";
    if (skipped(N)) return;
    struct sockaddr_in pa, pb, x;
    int p1 = udp_bound("127.0.0.1", 0, &pa), p2 = udp_bound("127.0.0.1", 0, &pb);
    int a = socket(AF_INET, SOCK_DGRAM, 0);
    connect(a, (struct sockaddr *)&pa, sizeof pa);
    socklen_t l = sizeof x;
    getsockname(a, (struct sockaddr *)&x, &l);
    uint16_t port1 = ntohs(x.sin_port);
    struct sockaddr un = { .sa_family = AF_UNSPEC };
    int d = connect(a, &un, sizeof un);
    l = sizeof x; getsockname(a, (struct sockaddr *)&x, &l);
    uint16_t port0 = ntohs(x.sin_port);
    /* Sending again binds a new port. */
    send_to(a, "n", 1, &pa);
    l = sizeof x; getsockname(a, (struct sockaddr *)&x, &l);
    uint16_t port2 = ntohs(x.sin_port);
    /* Now connected to p1; p1 sends two, then a re-connect to p2. */
    connect(a, (struct sockaddr *)&pa, sizeof pa);
    l = sizeof x; getsockname(a, (struct sockaddr *)&x, &l);
    struct sockaddr_in me = sin4("127.0.0.1", ntohs(x.sin_port));
    send_to(p1, "q1", 2, &me);
    send_to(p1, "q2", 2, &me);
    msleep(50);
    connect(a, (struct sockaddr *)&pb, sizeof pb);
    send_to(p1, "q3", 2, &me);   /* not the peer any more: dropped */
    send_to(p2, "q4", 2, &me);
    msleep(50);
    char b[8][8]; int k = 0;
    for (; k < 8; k++) {
        ssize_t n = recv(a, b[k], 7, MSG_DONTWAIT);
        if (n < 0) break;
        b[k][n] = 0;
    }
    /* bind() is allowed again after the port went. */
    int a2 = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in any0 = sin4("127.0.0.1", 0);
    connect(a2, (struct sockaddr *)&pa, sizeof pa);
    connect(a2, &un, sizeof un);
    int rb = bind(a2, (struct sockaddr *)&any0, sizeof any0); int erb = errno;
    close(a); close(a2); close(p1); close(p2);
    if (d) fail(N, "connect(AF_UNSPEC): %s", strerror(errno));
    else if (port1 == 0 || port0 != 0 || port2 == 0) fail(N, "ports: connected %u, after AF_UNSPEC %u (want 0), after send %u", port1, port0, port2);
    else if (k != 3 || strcmp(b[0], "q1") || strcmp(b[1], "q2") || strcmp(b[2], "q4"))
        fail(N, "after re-connect read %d: %s %s %s (want q1 q2 q4)", k, k > 0 ? b[0] : "-", k > 1 ? b[1] : "-", k > 2 ? b[2] : "-");
    else if (rb) fail(N, "bind after AF_UNSPEC: %s", strerror(erb));
    else pass(N);
}

/* TCP connect(AF_UNSPEC): aborts the connection (the peer sees a reset or
 * EOF), the socket can connect again (Linux tcp_disconnect). */
static void tcp_disconnect(void) {
    const char *N = "tcp_disconnect";
    if (skipped(N)) return;
    int l = socket(AF_INET, SOCK_STREAM, 0), one = 1;
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in la = sin4("127.0.0.1", 0);
    socklen_t ll = sizeof la;
    if (bind(l, (struct sockaddr *)&la, sizeof la) || listen(l, 4) || getsockname(l, (struct sockaddr *)&la, &ll)) {
        fail(N, "listener: %s", strerror(errno)); return;
    }
    int c = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr un = { .sa_family = AF_UNSPEC };
    int d0 = connect(c, &un, sizeof un);
    int cr = connect(c, (struct sockaddr *)&la, sizeof la);
    int s = accept(l, NULL, NULL);
    int d = connect(c, &un, sizeof un);
    struct sockaddr_in pa; socklen_t pl = sizeof pa;
    int gp = getpeername(c, (struct sockaddr *)&pa, &pl); int egp = errno;
    char b[8];
    wait_ev(s, POLLIN, 1000);
    ssize_t n = recv(s, b, sizeof b, MSG_DONTWAIT); int en = errno;
    int cr2 = connect(c, (struct sockaddr *)&la, sizeof la); int ecr2 = errno;
    int s2 = accept(l, NULL, NULL);
    ssize_t w = send(c, "ok", 2, 0);
    ssize_t r2 = s2 >= 0 && wait_ev(s2, POLLIN, 1000) ? recv(s2, b, sizeof b, 0) : -1;
    close(c); close(s); if (s2 >= 0) close(s2); close(l);
    if (d0) fail(N, "AF_UNSPEC on an unconnected socket: %s", strerror(errno));
    else if (cr || s < 0) fail(N, "first connect %d accept %d", cr, s);
    else if (d) fail(N, "connect(AF_UNSPEC): %s", strerror(errno));
    else if (gp == 0 || egp != ENOTCONN) fail(N, "getpeername after it: %d %s", gp, strerror(egp));
    else if (!(n == 0 || (n < 0 && en == ECONNRESET))) fail(N, "the peer read %zd %s, want a reset (or EOF)", n, strerror(en));
    else if (cr2) fail(N, "connect again: %s", strerror(ecr2));
    else if (s2 < 0 || w != 2 || r2 != 2) fail(N, "second connection: accept %d send %zd recv %zd", s2, w, r2);
    else pass(N);
}
