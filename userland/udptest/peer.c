/*
 * peer.c — udptest's cases against the frame-level peer of
 * .claude/skills/run-leandros/udppeertest.py (the guest's second NIC, eth1):
 *
 *   udptest peer <peer_ip> <local_ip> [-x NAME]...
 *
 * #include'd by udptest.c right before main, so it shares that file's helpers.
 * The peer (10.9.0.1) echoes on UDP 7, answers UDP 9 and TCP SYNs to 81/82/83
 * with ICMP errors, tracks IGMP, and takes ASCII commands on UDP 7000
 * (MCAST / IGMP / FRAGS / TOS / STATS; see udppeertest.py).
 *
 * Cases:
 *   peer_frag_echo   4000-, 20000- and 65507-byte datagrams echo back whole
 *                    (IP fragmentation out, reassembly in); the peer saw >= 3
 *                    fragmented datagrams, frames <= 1514, fragment payloads a
 *                    multiple of 8, DF clear, no overlaps
 *   peer_frag_rx     3000-byte datagram sent as shuffled fragments (one twice)
 *                    arrives once and whole; an incomplete set and a ping of
 *                    death deliver nothing
 *   peer_mdns        two SO_REUSEADDR sockets on 0.0.0.0:5353 join 224.0.0.251
 *                    (ip_mreq / ip_mreqn): IGMP report seen, both receive the
 *                    peer's multicast, IP_PKTINFO is right, leaving works
 *   peer_refused     ICMP port unreachable -> ECONNREFUSED on a connected UDP
 *                    socket (recv, POLLERR, SO_ERROR), ignored when unconnected
 *   peer_tcp_icmp    connect() fails EHOSTUNREACH / ECONNREFUSED / (non-blocking)
 *                    EHOSTUNREACH on ICMP host / port / admin-prohibited
 *   peer_bcast       EACCES for broadcast sendto without SO_BROADCAST; with it
 *                    the broadcast echo reaches two SO_REUSEADDR sockets
 *   peer_tos         IP_RECVTOS / IP_RECVTTL cmsgs carry the packet's TOS/TTL
 *   peer_weakhost    a datagram for the OTHER interface's address (eth0's,
 *                    found with a connected UDP socket's getsockname) that
 *                    arrives on this link reaches a 0.0.0.0 socket, with
 *                    IP_PKTINFO naming that address (Linux weak host)
 *
 * Output as udptest: "<name>: PASS|FAIL|SKIP ..." and a summary line
 * "udptest peer: N passed, M failed, K skipped"; exit status = failures.
 */

#ifndef IP_RECVTOS
#define IP_RECVTOS 13
#endif
#ifndef IP_RECVTTL
#define IP_RECVTTL 12
#endif

static const char *pc_peer, *pc_local;

/* Watchdog: a case that blocks for 40 s is interrupted (EINTR) by SIGALRM; if
 * it is still stuck 10 s later the process exits, so a kernel bug in one case
 * cannot hang the whole run. */
static volatile sig_atomic_t pc_alarms;
static const char *volatile pc_case = "";
static void peer_alarm(int sig) {
    (void)sig;
    char m[128];
    int n = snprintf(m, sizeof m, "%s: watchdog %s\n", (const char *)pc_case, pc_alarms ? "exit: still blocked after EINTR" : "interrupted a blocked call");
    if (write(1, m, (size_t)n) < 0) {}
    if (pc_alarms++) _exit(99);
    alarm(10);
}
static void peer_begin(const char *name) {
    pc_case = name;
    pc_alarms = 0;
    alarm(40);
}

/* One command to the peer's control port (UDP 7000) from a fresh socket, up to
 * 3 times if no reply comes; the reply in r (NUL-terminated), its length, or -1. */
static int peer_ctl(const char *cmd, char *r, size_t n) {
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) return -1;
    struct sockaddr_in to = sin4(pc_peer, 7000);
    int k = -1;
    for (int tries = 0; tries < 3 && k < 0; tries++) {
        if (send_to(fd, cmd, strlen(cmd), &to) < 0) { msleep(300); continue; }
        if (wait_ev(fd, POLLIN, 1000) & POLLIN) {
            ssize_t x = recv(fd, r, n - 1, MSG_DONTWAIT);
            if (x >= 0) { r[x] = 0; k = (int)x; }
        }
    }
    close(fd);
    return k;
}

/* Number following "key": in a flat JSON object; true/false are 1/0. */
static long peer_json(const char *j, const char *key, long dflt) {
    char pat[64];
    snprintf(pat, sizeof pat, "\"%s\":", key);
    const char *p = strstr(j, pat);
    if (!p) return dflt;
    p += strlen(pat);
    while (*p == ' ') p++;
    if (*p == 't') return 1;
    if (*p == 'f') return 0;
    return strtol(p, NULL, 10);
}

static void peer_fill(uint8_t *b, size_t n) {
    for (size_t i = 0; i < n; i++) b[i] = (uint8_t)(i % 251);
}

/* recvfrom on a nonblocking-after-poll socket until a datagram from `from_ip`
 * (network order; 0 = anyone) arrives, up to ms. Length or -1. */
static ssize_t peer_recv_from(int fd, void *b, size_t n, in_addr_t from_ip, int ms, struct sockaddr_in *src) {
    long long end = now_ms() + ms;
    for (;;) {
        long long left = end - now_ms();
        if (left <= 0) return -1;
        if (!(wait_ev(fd, POLLIN, (int)left) & POLLIN)) return -1;
        struct sockaddr_in a; socklen_t l = sizeof a;
        ssize_t k = recvfrom(fd, b, n, MSG_DONTWAIT, (struct sockaddr *)&a, &l);
        if (k < 0) continue;
        if (from_ip && a.sin_addr.s_addr != from_ip) continue;
        if (src) *src = a;
        return k;
    }
}

/* recv() with a timeout: poll, then a non-blocking recv (EAGAIN on
 * timeout). Written before LeandrOS implemented SO_RCVTIMEO. */
static ssize_t peer_recv_ms(int fd, void *b, size_t n, int ms) {
    wait_ev(fd, POLLIN, ms);
    return recv(fd, b, n, MSG_DONTWAIT);
}

/* ── fragmentation out and reassembly in ────────────────────────────────── */

static void peer_frag_echo(void) {
    const char *N = "peer_frag_echo";
    if (skipped(N)) return;
    peer_begin(N);
    static uint8_t d[65507], g[70000];
    peer_fill(d, sizeof d);
    static const size_t sizes[3] = { 4000, 20000, 65507 };
    struct sockaddr_in echo = sin4(pc_peer, 7);
    int s = udp_bound("0.0.0.0", 0, NULL);
    if (s < 0) { fail(N, "socket: %s", strerror(errno)); return; }
    int bad = -1, why = 0;      /* why: 1 send error, 2 no echo, 3 wrong echo */
    ssize_t last = 0;
    int serr = 0;
    for (int i = 0; i < 3 && bad < 0; i++) {
        int got = 0;
        for (int attempt = 0; attempt < 3 && !got; attempt++) {
            drain(s);
            if (send_to(s, d, sizes[i], &echo) != (ssize_t)sizes[i]) { serr = errno; why = 1; continue; }
            ssize_t k = peer_recv_from(s, g, sizeof g, 0, 3000, NULL);
            last = k;
            if (k < 0) { why = 2; continue; }
            if ((size_t)k != sizes[i] || memcmp(g, d, sizes[i])) { why = 3; continue; }
            got = 1;
        }
        if (!got) bad = (int)sizes[i];
    }
    close(s);
    if (bad >= 0) {
        if (why == 1) fail(N, "send of %d bytes: %s", bad, strerror(serr));
        else if (why == 2) fail(N, "no echo of the %d-byte datagram in 3 tries", bad);
        else fail(N, "echo of %d bytes came back as %zd bytes or corrupt", bad, last);
        return;
    }
    char r[2048];
    if (peer_ctl("STATS", r, sizeof r) < 0) { fail(N, "no STATS reply from the peer"); return; }
    long fd_n = peer_json(r, "frag_datagrams", -1), mf = peer_json(r, "max_frame", -1);
    long al = peer_json(r, "frag_aligned", 0), dfc = peer_json(r, "df_clear", 0), ov = peer_json(r, "overlap", 1);
    printf("peer_frag_echo: peer saw %ld fragmented datagrams, max frame %ld\n", fd_n, mf);
    if (fd_n < 3) fail(N, "peer reassembled %ld fragmented datagrams, want >= 3", fd_n);
    else if (mf > 1514) fail(N, "a frame of %ld bytes left the NIC (max 1514)", mf);
    else if (!al) fail(N, "a non-last fragment's payload was not a multiple of 8");
    else if (!dfc) fail(N, "a fragment had DF set");
    else if (ov) fail(N, "overlapping fragments");
    else pass(N);
}

static void peer_frag_rx(void) {
    const char *N = "peer_frag_rx";
    if (skipped(N)) return;
    peer_begin(N);
    struct sockaddr_in a;
    int s = udp_bound("0.0.0.0", 0, &a);
    if (s < 0) { fail(N, "socket: %s", strerror(errno)); return; }
    char cmd[64], r[256];
    snprintf(cmd, sizeof cmd, "FRAGS %u", ntohs(a.sin_port));
    if (peer_ctl(cmd, r, sizeof r) < 0) { close(s); fail(N, "no reply to FRAGS"); return; }
    static uint8_t g[8192];
    struct sockaddr_in from;
    ssize_t k = peer_recv_from(s, g, sizeof g, 0, 4000, &from);
    int extra = 0;
    ssize_t k2 = -1;
    if (k >= 0) {
        long long end = now_ms() + 2000;
        while (now_ms() < end) {
            uint8_t x[8192];
            ssize_t y = peer_recv_from(s, x, sizeof x, 0, (int)(end - now_ms()), NULL);
            if (y >= 0) { extra++; k2 = y; }
        }
    }
    close(s);
    int content = k == 3000 && !memcmp(g, "complete", 8);
    for (int i = 8; content && i < 3000; i++) if (g[i] != (uint8_t)(i % 251)) content = 0;
    if (k < 0) fail(N, "the complete fragmented datagram never arrived (reply '%s')", r);
    else if (k != 3000) fail(N, "got %zd bytes starting '%.10s', want 3000 starting 'complete'", k, (const char *)g);
    else if (memcmp(g, "complete", 8)) fail(N, "got the wrong datagram: starts '%.10s'", (const char *)g);
    else if (!content) fail(N, "3000-byte datagram reassembled with wrong content");
    else if (from.sin_addr.s_addr != inet_addr(pc_peer) || ntohs(from.sin_port) != 7001) fail(N, "datagram from %s:%u, want %s:7001", inet_ntoa(from.sin_addr), ntohs(from.sin_port), pc_peer);
    else if (extra) fail(N, "%d more datagram(s) arrived (last %zd bytes): duplicate, incomplete set or ping of death delivered", extra, k2);
    else pass(N);
}

/* ── multicast ──────────────────────────────────────────────────────────── */

static ssize_t peer_recv_pktinfo(int fd, char *b, size_t n, struct in_pktinfo *pi, int *have, int ms, struct sockaddr_in *src) {
    *have = 0;
    if (!(wait_ev(fd, POLLIN, ms) & POLLIN)) return -1;
    char cb[256];
    struct iovec iov = { b, n };
    struct msghdr m; memset(&m, 0, sizeof m);
    m.msg_name = src; m.msg_namelen = sizeof *src;
    m.msg_iov = &iov; m.msg_iovlen = 1;
    m.msg_control = cb; m.msg_controllen = sizeof cb;
    ssize_t k = recvmsg(fd, &m, MSG_DONTWAIT);
    if (k < 0) return -1;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c))
        if (c->cmsg_level == IPPROTO_IP && c->cmsg_type == IP_PKTINFO && c->cmsg_len >= CMSG_LEN(sizeof *pi)) {
            memcpy(pi, CMSG_DATA(c), sizeof *pi);
            *have = 1;
        }
    return k;
}

static int peer_igmp_state(const char *grp, const char *want, int ms) {
    char cmd[64], r[64];
    snprintf(cmd, sizeof cmd, "IGMP %s", grp);
    long long end = now_ms() + ms;
    for (;;) {
        if (peer_ctl(cmd, r, sizeof r) >= 0 && !strcmp(r, want)) return 1;
        if (now_ms() >= end) return 0;
        msleep(300);
    }
}

static void peer_mdns(void) {
    const char *N = "peer_mdns";
    if (skipped(N)) return;
    peer_begin(N);
    const char *grp = "224.0.0.251";
    int one = 1;
    int a = socket(AF_INET, SOCK_DGRAM, 0), b = socket(AF_INET, SOCK_DGRAM, 0);
    if (a < 0 || b < 0) { fail(N, "socket: %s", strerror(errno)); return; }
    setsockopt(a, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    setsockopt(b, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in any = sin4("0.0.0.0", 5353);
    int ba = bind(a, (struct sockaddr *)&any, sizeof any), ea = errno;
    int bb = bind(b, (struct sockaddr *)&any, sizeof any), eb = errno;
    struct ip_mreq ma;
    memset(&ma, 0, sizeof ma);
    inet_pton(AF_INET, grp, &ma.imr_multiaddr);
    inet_pton(AF_INET, pc_local, &ma.imr_interface);
    struct ip_mreqn mb;
    memset(&mb, 0, sizeof mb);
    inet_pton(AF_INET, grp, &mb.imr_multiaddr);
    inet_pton(AF_INET, pc_local, &mb.imr_address);
    int ja = setsockopt(a, IPPROTO_IP, IP_ADD_MEMBERSHIP, &ma, sizeof ma), eja = errno;
    int jb = setsockopt(b, IPPROTO_IP, IP_ADD_MEMBERSHIP, &mb, sizeof mb), ejb = errno;
    int pk = setsockopt(a, IPPROTO_IP, IP_PKTINFO, &one, sizeof one), epk = errno;
    const char *err = NULL;
    char msg[160] = "";
    if (ba || bb) { snprintf(msg, sizeof msg, "bind 0.0.0.0:5353 twice with SO_REUSEADDR: %s / %s", ba ? strerror(ea) : "ok", bb ? strerror(eb) : "ok"); err = msg; }
    else if (ja) { snprintf(msg, sizeof msg, "IP_ADD_MEMBERSHIP (ip_mreq): %s", strerror(eja)); err = msg; }
    else if (jb) { snprintf(msg, sizeof msg, "IP_ADD_MEMBERSHIP (ip_mreqn): %s", strerror(ejb)); err = msg; }
    else if (pk) { snprintf(msg, sizeof msg, "IP_PKTINFO: %s", strerror(epk)); err = msg; }
    if (err) goto out;

    char cmd[96], r[128];
    if (!peer_igmp_state(grp, "JOINED", 4000)) { err = "the peer saw no IGMP membership report for 224.0.0.251"; goto out; }
    snprintf(cmd, sizeof cmd, "MCAST %s 5353 3", grp);
    int k = peer_ctl(cmd, r, sizeof r);
    if (k < 0) { err = "no reply to MCAST"; goto out; }
    if (strncmp(r, "OK", 2)) { snprintf(msg, sizeof msg, "MCAST refused: '%s'", r); err = msg; goto out; }
    struct in_addr want_grp, want_local;
    inet_pton(AF_INET, grp, &want_grp);
    inet_pton(AF_INET, pc_local, &want_local);
    for (int i = 0; i < 3 && !err; i++) {
        char exp[16], g[64];
        snprintf(exp, sizeof exp, "mdns-%d", i);
        struct sockaddr_in src;
        struct in_pktinfo pi;
        int have;
        memset(&pi, 0, sizeof pi);
        ssize_t x = peer_recv_pktinfo(a, g, sizeof g - 1, &pi, &have, 3000, &src);
        if (x < 0) { snprintf(msg, sizeof msg, "socket A (ip_mreq) never got '%s'", exp); err = msg; break; }
        g[x] = 0;
        if (strcmp(g, exp) || src.sin_addr.s_addr != inet_addr(pc_peer) || ntohs(src.sin_port) != 5353) {
            snprintf(msg, sizeof msg, "socket A got '%s' from %s:%u, want '%s' from %s:5353", g, inet_ntoa(src.sin_addr), ntohs(src.sin_port), exp, pc_peer);
            err = msg; break;
        }
        if (!have) { err = "socket A: no IP_PKTINFO cmsg"; break; }
        if (pi.ipi_addr.s_addr != want_grp.s_addr || pi.ipi_spec_dst.s_addr != want_local.s_addr || pi.ipi_ifindex <= 1) {
            char t1[20], t2[20];
            snprintf(t1, sizeof t1, "%s", inet_ntoa(pi.ipi_addr));
            snprintf(t2, sizeof t2, "%s", inet_ntoa(pi.ipi_spec_dst));
            snprintf(msg, sizeof msg, "IP_PKTINFO addr %s spec_dst %s ifindex %d (want %s, %s, > 1)", t1, t2, pi.ipi_ifindex, grp, pc_local);
            err = msg; break;
        }
        x = peer_recv_from(b, g, sizeof g - 1, 0, 3000, &src);
        if (x < 0) { snprintf(msg, sizeof msg, "socket B (ip_mreqn) never got '%s'", exp); err = msg; break; }
        g[x] = 0;
        if (strcmp(g, exp)) { snprintf(msg, sizeof msg, "socket B got '%s', want '%s'", g, exp); err = msg; }
    }
    if (err) goto out;

    /* Leave on both: the peer sees the leave; further multicast is not sent or not received. */
    if (setsockopt(a, IPPROTO_IP, IP_DROP_MEMBERSHIP, &ma, sizeof ma)) { snprintf(msg, sizeof msg, "IP_DROP_MEMBERSHIP (ip_mreq): %s", strerror(errno)); err = msg; goto out; }
    if (setsockopt(b, IPPROTO_IP, IP_DROP_MEMBERSHIP, &mb, sizeof mb)) { snprintf(msg, sizeof msg, "IP_DROP_MEMBERSHIP (ip_mreqn): %s", strerror(errno)); err = msg; goto out; }
    if (!peer_igmp_state(grp, "LEFT", 2000)) { err = "the peer saw no IGMP leave within 2 s"; goto out; }
    drain(a); drain(b);
    k = peer_ctl(cmd, r, sizeof r);
    if (k < 0) { err = "no reply to the second MCAST"; goto out; }
    if (strncmp(r, "NOJOIN", 6)) {
        char g[64];
        if (peer_recv_from(a, g, sizeof g, 0, 1500, NULL) >= 0 || peer_recv_from(b, g, sizeof g, 0, 1500, NULL) >= 0) {
            err = "multicast still delivered after IP_DROP_MEMBERSHIP";
            goto out;
        }
    }
out:
    close(a); close(b);
    if (err) fail(N, "%s", err);
    else pass(N);
}

/* ── ICMP errors ────────────────────────────────────────────────────────── */

static void peer_refused(void) {
    const char *N = "peer_refused";
    if (skipped(N)) return;
    peer_begin(N);
    struct sockaddr_in to = sin4(pc_peer, 9);
    int c = socket(AF_INET, SOCK_DGRAM, 0);
    if (c < 0) { fail(N, "socket: %s", strerror(errno)); return; }
    char b[16];
    const char *err = NULL;
    char msg[160] = "";
    if (connect(c, (struct sockaddr *)&to, sizeof to) < 0) { snprintf(msg, sizeof msg, "connect: %s", strerror(errno)); err = msg; goto out; }
    if (send(c, "x", 1, 0) != 1) { snprintf(msg, sizeof msg, "send: %s", strerror(errno)); err = msg; goto out; }
    long long t0 = now_ms();
    ssize_t k = peer_recv_ms(c, b, sizeof b, 2000);
    int e = errno;
    if (k >= 0 || e != ECONNREFUSED) {
        snprintf(msg, sizeof msg, "blocking recv after ICMP port unreachable: %zd %s after %lld ms (want ECONNREFUSED)", k, k < 0 ? strerror(e) : "", now_ms() - t0);
        err = msg; goto out;
    }
    k = peer_recv_ms(c, b, sizeof b, 600);
    e = errno;
    if (k >= 0 || (e != EAGAIN && e != EWOULDBLOCK)) {
        snprintf(msg, sizeof msg, "second recv: %zd %s (want EAGAIN: the error is consumed)", k, k < 0 ? strerror(e) : "");
        err = msg; goto out;
    }
    if (send(c, "x", 1, 0) != 1) { snprintf(msg, sizeof msg, "second send: %s", strerror(errno)); err = msg; goto out; }
    msleep(300);
    int ev = wait_ev(c, POLLIN, 1000);
    int so = -1; socklen_t sl = sizeof so;
    getsockopt(c, SOL_SOCKET, SO_ERROR, &so, &sl);
    int so2 = -1; sl = sizeof so2;
    getsockopt(c, SOL_SOCKET, SO_ERROR, &so2, &sl);
    if (!(ev & POLLERR)) { snprintf(msg, sizeof msg, "poll revents %#x after the ICMP error, want POLLERR", ev); err = msg; goto out; }
    if (so != ECONNREFUSED) { snprintf(msg, sizeof msg, "SO_ERROR %d (%s), want ECONNREFUSED", so, strerror(so)); err = msg; goto out; }
    if (so2 != 0) { snprintf(msg, sizeof msg, "second SO_ERROR %d (%s), want 0", so2, strerror(so2)); err = msg; goto out; }

    /* Unconnected: Linux ignores ICMP errors without IP_RECVERR. */
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    ssize_t s = send_to(u, "y", 1, &to);
    int se = errno;
    k = peer_recv_ms(u, b, sizeof b, 1000);
    e = errno;
    int uev = wait_ev(u, POLLIN, 0);
    int uso = -1; sl = sizeof uso;
    getsockopt(u, SOL_SOCKET, SO_ERROR, &uso, &sl);
    close(u);
    if (s != 1) { snprintf(msg, sizeof msg, "unconnected sendto: %zd %s", s, strerror(se)); err = msg; }
    else if (k >= 0 || (e != EAGAIN && e != EWOULDBLOCK)) { snprintf(msg, sizeof msg, "unconnected recv: %zd %s (want a timeout, no error)", k, k < 0 ? strerror(e) : ""); err = msg; }
    else if (uev & POLLERR || uso != 0) { snprintf(msg, sizeof msg, "unconnected socket saw the ICMP error (revents %#x SO_ERROR %d)", uev, uso); err = msg; }
out:
    close(c);
    if (err) fail(N, "%s", err);
    else pass(N);
}

static void peer_tcp_icmp(void) {
    const char *N = "peer_tcp_icmp";
    if (skipped(N)) return;
    peer_begin(N);
    static const struct { int port; int err; const char *what; } t[2] = {
        { 81, EHOSTUNREACH, "EHOSTUNREACH" }, { 82, ECONNREFUSED, "ECONNREFUSED" },
    };
    char msg[160] = "";
    const char *err = NULL;
    for (int i = 0; i < 2 && !err; i++) {
        struct sockaddr_in to = sin4(pc_peer, (uint16_t)t[i].port);
        int c = socket(AF_INET, SOCK_STREAM, 0);
        struct timeval tv = { 3, 0 };
        setsockopt(c, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
        long long t0 = now_ms();
        int rc = connect(c, (struct sockaddr *)&to, sizeof to);
        int e = errno;
        long long dt = now_ms() - t0;
        close(c);
        if (rc == 0) snprintf(msg, sizeof msg, "connect to %s:%d succeeded", pc_peer, t[i].port);
        else if (e != t[i].err) snprintf(msg, sizeof msg, "connect to :%d: %s after %lld ms, want %s", t[i].port, strerror(e), dt, t[i].what);
        else if (dt > 3000) snprintf(msg, sizeof msg, "connect to :%d took %lld ms (> 3 s)", t[i].port, dt);
        else continue;
        err = msg;
    }
    if (!err) {
        struct sockaddr_in to = sin4(pc_peer, 83);
        int c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
        int rc = connect(c, (struct sockaddr *)&to, sizeof to);
        int e = errno;
        if (rc == 0 || e != EINPROGRESS) { snprintf(msg, sizeof msg, "non-blocking connect to :83: %d %s (want EINPROGRESS)", rc, strerror(e)); err = msg; }
        else {
            int ev = wait_ev(c, POLLOUT, 3000);
            int so = -1; socklen_t sl = sizeof so;
            getsockopt(c, SOL_SOCKET, SO_ERROR, &so, &sl);
            if (!(ev & (POLLOUT | POLLERR))) { snprintf(msg, sizeof msg, "non-blocking connect to :83: no POLLOUT/POLLERR in 3 s"); err = msg; }
            else if (so != EHOSTUNREACH) { snprintf(msg, sizeof msg, "non-blocking connect to :83: SO_ERROR %d (%s), want EHOSTUNREACH (revents %#x)", so, strerror(so), ev); err = msg; }
        }
        close(c);
    }
    if (err) fail(N, "%s", err);
    else pass(N);
}

/* ── broadcast ──────────────────────────────────────────────────────────── */

static void peer_bcast(void) {
    const char *N = "peer_bcast";
    if (skipped(N)) return;
    peer_begin(N);
    const uint16_t Q = 40123;
    int one = 1;
    int s = socket(AF_INET, SOCK_DGRAM, 0), r = socket(AF_INET, SOCK_DGRAM, 0);
    if (s < 0 || r < 0) { fail(N, "socket: %s", strerror(errno)); return; }
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    setsockopt(r, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in any = sin4("0.0.0.0", Q);
    int bs = bind(s, (struct sockaddr *)&any, sizeof any), es = errno;
    int br = bind(r, (struct sockaddr *)&any, sizeof any), er = errno;
    char msg[160] = "";
    const char *err = NULL;
    char bufs[64], bufr[64];
    struct sockaddr_in b1 = sin4("10.9.0.255", 7), b2 = sin4("255.255.255.255", 7);
    if (bs || br) { snprintf(msg, sizeof msg, "bind 0.0.0.0:%u twice with SO_REUSEADDR: %s / %s", Q, bs ? strerror(es) : "ok", br ? strerror(er) : "ok"); err = msg; goto out; }
    ssize_t x1 = send_to(s, "nobcast", 7, &b1);
    int e1 = errno;
    ssize_t x2 = send_to(s, "nobcast", 7, &b2);
    int e2 = errno;
    if (x1 >= 0 || e1 != EACCES) { snprintf(msg, sizeof msg, "sendto 10.9.0.255 without SO_BROADCAST: %zd %s (want EACCES)", x1, x1 < 0 ? strerror(e1) : "ok"); err = msg; goto out; }
    if (x2 >= 0 || e2 != EACCES) { snprintf(msg, sizeof msg, "sendto 255.255.255.255 without SO_BROADCAST: %zd %s (want EACCES)", x2, x2 < 0 ? strerror(e2) : "ok"); err = msg; goto out; }
    if (setsockopt(s, SOL_SOCKET, SO_BROADCAST, &one, sizeof one)) { snprintf(msg, sizeof msg, "SO_BROADCAST: %s", strerror(errno)); err = msg; goto out; }
    drain(s); drain(r);
    x1 = send_to(s, "bcast-echo", 10, &b1);
    if (x1 != 10) { snprintf(msg, sizeof msg, "sendto 10.9.0.255 with SO_BROADCAST: %zd %s", x1, strerror(errno)); err = msg; goto out; }
    /* Linux also loops our own broadcast back to local sockets: only the peer's echo counts. */
    in_addr_t pip = inet_addr(pc_peer);
    struct sockaddr_in fs, fr;
    ssize_t ks = peer_recv_from(s, bufs, sizeof bufs, pip, 3000, &fs);
    ssize_t kr = peer_recv_from(r, bufr, sizeof bufr, pip, 3000, &fr);
    if (ks < 0) err = "socket S (sender) never got the peer's broadcast echo";
    else if (kr < 0) err = "socket R (second SO_REUSEADDR socket) never got the peer's broadcast echo";
    else if (ks != 10 || kr != 10 || memcmp(bufs, "bcast-echo", 10) || memcmp(bufr, "bcast-echo", 10)) err = "the broadcast echo payload differs";
    else if (ntohs(fs.sin_port) != 7) err = "the echo did not come from port 7";
out:
    close(s); close(r);
    if (err) fail(N, "%s", err);
    else pass(N);
}

/* ── TOS / TTL ──────────────────────────────────────────────────────────── */

static void peer_tos(void) {
    const char *N = "peer_tos";
    if (skipped(N)) return;
    peer_begin(N);
    struct sockaddr_in a;
    int s = udp_bound("0.0.0.0", 0, &a);
    if (s < 0) { fail(N, "socket: %s", strerror(errno)); return; }
    int one = 1;
    int r1 = setsockopt(s, IPPROTO_IP, IP_RECVTOS, &one, sizeof one), e1 = errno;
    int r2 = setsockopt(s, IPPROTO_IP, IP_RECVTTL, &one, sizeof one), e2 = errno;
    char msg[160] = "";
    const char *err = NULL;
    char cmd[64], rep[64], g[16], cb[128];
    if (r1 || r2) { snprintf(msg, sizeof msg, "IP_RECVTOS %s IP_RECVTTL %s", r1 ? strerror(e1) : "ok", r2 ? strerror(e2) : "ok"); err = msg; goto out; }
    snprintf(cmd, sizeof cmd, "TOS %u 184 77", ntohs(a.sin_port));
    if (peer_ctl(cmd, rep, sizeof rep) < 0) { err = "no reply to TOS"; goto out; }
    if (!(wait_ev(s, POLLIN, 3000) & POLLIN)) { err = "the TOS datagram never arrived"; goto out; }
    struct iovec iov = { g, sizeof g };
    struct msghdr m; memset(&m, 0, sizeof m);
    m.msg_iov = &iov; m.msg_iovlen = 1;
    m.msg_control = cb; m.msg_controllen = sizeof cb;
    ssize_t k = recvmsg(s, &m, MSG_DONTWAIT);
    if (k != 3 || memcmp(g, "tos", 3)) { snprintf(msg, sizeof msg, "recvmsg: %zd bytes, want 'tos'", k); err = msg; goto out; }
    int tos = -1, ttl = -1;
    size_t tos_len = 0;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c)) {
        if (c->cmsg_level != IPPROTO_IP) continue;
        if (c->cmsg_type == IP_TOS) { tos_len = c->cmsg_len - CMSG_LEN(0); tos = CMSG_DATA(c)[0]; }
        else if (c->cmsg_type == IP_TTL && c->cmsg_len >= CMSG_LEN(sizeof(int))) memcpy(&ttl, CMSG_DATA(c), sizeof ttl);
    }
    if (tos != 184) { snprintf(msg, sizeof msg, "IP_TOS cmsg %d (data %zu bytes), want 184 in 1 byte", tos, tos_len); err = msg; }
    else if (tos_len != 1) { snprintf(msg, sizeof msg, "IP_TOS cmsg has %zu data bytes, want 1", tos_len); err = msg; }
    else if (ttl != 77) { snprintf(msg, sizeof msg, "IP_TTL cmsg %d, want 77", ttl); err = msg; }
out:
    close(s);
    if (err) fail(N, "%s", err);
    else pass(N);
}

/* ── weak host ──────────────────────────────────────────────────────────── */

static void peer_weakhost(void) {
    const char *N = "peer_weakhost";
    if (skipped(N)) return;
    peer_begin(N);
    // The other interface's address: what a flow to the default route uses.
    int c = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in far = sin4("192.0.2.1", 9), me; socklen_t ml = sizeof me;
    int cr = connect(c, (struct sockaddr *)&far, sizeof far);
    int gr = cr ? -1 : getsockname(c, (struct sockaddr *)&me, &ml);
    close(c);
    if (cr || gr || me.sin_addr.s_addr == 0 || me.sin_addr.s_addr == inet_addr(pc_local)) {
        skip(N, "no second interface address");
        return;
    }
    char other[16];
    snprintf(other, sizeof other, "%s", inet_ntoa(me.sin_addr));
    struct sockaddr_in a;
    int s = udp_bound("0.0.0.0", 0, &a);
    if (s < 0) { fail(N, "socket: %s", strerror(errno)); return; }
    int one = 1;
    setsockopt(s, IPPROTO_IP, IP_PKTINFO, &one, sizeof one);
    char cmd[64], rep[64], b[16], msg[160] = "";
    const char *err = NULL;
    snprintf(cmd, sizeof cmd, "WEAK %s %u", other, ntohs(a.sin_port));
    if (peer_ctl(cmd, rep, sizeof rep) < 0 || strncmp(rep, "OK", 2)) { err = "no OK to WEAK (old peer?)"; goto out; }
    struct in_pktinfo pi; int have = 0; struct sockaddr_in src;
    ssize_t k = peer_recv_pktinfo(s, b, sizeof b, &pi, &have, 3000, &src);
    if (k < 0) { snprintf(msg, sizeof msg, "the datagram to %s (sent on the peer link) never arrived", other); err = msg; }
    else if (k != 4 || memcmp(b, "weak", 4)) { snprintf(msg, sizeof msg, "got %zd bytes, want 'weak'", k); err = msg; }
    else if (src.sin_addr.s_addr != inet_addr(pc_peer)) { snprintf(msg, sizeof msg, "from %s, want %s", inet_ntoa(src.sin_addr), pc_peer); err = msg; }
    else if (!have || pi.ipi_addr.s_addr != me.sin_addr.s_addr) {
        snprintf(msg, sizeof msg, "IP_PKTINFO %s, want destination %s", have ? inet_ntoa(pi.ipi_addr) : "missing", other); err = msg;
    }
out:
    close(s);
    if (err) fail(N, "%s", err);
    else pass(N);
}

static int peer_main(int argc, char **argv) {
    if (argc < 3) { fprintf(stderr, "usage: udptest peer <peer_ip> <local_ip> [-x case]...\n"); return 2; }
    pc_peer = argv[1];
    pc_local = argv[2];
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = peer_alarm;
    sigaction(SIGALRM, &sa, NULL);
    for (int i = 3; i + 1 < argc; i++)
        if (!strcmp(argv[i], "-x") && nskips < 32) skips[nskips++] = argv[++i];
    printf("udptest peer: peer %s local %s uid %d\n", pc_peer, pc_local, (int)geteuid());
    char r[2048];
    if (peer_ctl("STATS", r, sizeof r) < 0) {
        fail("peer_ctl", "no reply from the peer's control port %s:7000", pc_peer);
        printf("udptest peer: %d passed, %d failed, %d skipped\n", npass, nfail, nskip);
        return 1;
    }
    peer_frag_echo();
    peer_frag_rx();
    peer_tos();
    peer_refused();
    peer_tcp_icmp();
    peer_bcast();
    peer_mdns();
    peer_weakhost();
    printf("udptest peer: %d passed, %d failed, %d skipped\n", npass, nfail, nskip);
    return nfail > 100 ? 100 : nfail;
}
