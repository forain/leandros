/* Multicast filters beyond the four ip_mreq_source options: IP_MSFILTER,
 * IP_MULTICAST_ALL=0, MCAST_* group_req at IPPROTO_IP, and the IPv6 group
 * options. Receivers share a port on the NIC; the sender is this host. */
#include <net/if.h>

#ifndef IP_MSFILTER
#define IP_MSFILTER 41
#endif
#ifndef IP_MULTICAST_ALL
#define IP_MULTICAST_ALL 49
#endif
#define T_MCAST_JOIN_GROUP 42
#define T_MCAST_UNBLOCK_SOURCE 44
#define T_MCAST_LEAVE_GROUP 45

static int ms_set(int fd, const char *group, unsigned fmode, int n, const char **srcs) {
    unsigned char b[16 + 4 * 16]; memset(b, 0, sizeof b);
    inet_pton(AF_INET, group, b);
    memcpy(b + 8, &fmode, 4); memcpy(b + 12, &n, 4);
    for (int i = 0; i < n; i++) inet_pton(AF_INET, srcs[i], b + 16 + 4 * i);
    return setsockopt(fd, IPPROTO_IP, IP_MSFILTER, b, 16 + 4 * n);
}

/* Returns the numsrc the kernel reports (-1 on error); *fmode and the first source. */
static int ms_get(int fd, const char *group, int room, unsigned *fmode, unsigned *first) {
    unsigned char b[16 + 4 * 16]; memset(b, 0, sizeof b);
    inet_pton(AF_INET, group, b);
    memcpy(b + 12, &room, 4);
    socklen_t l = 16 + 4 * room;
    if (getsockopt(fd, IPPROTO_IP, IP_MSFILTER, b, &l) < 0) return -1;
    int n; memcpy(&n, b + 12, 4); memcpy(fmode, b + 8, 4); memcpy(first, b + 16, 4);
    return n;
}

static void udp_mcast_msfilter(void) {
    const char *N = "udp_mcast_msfilter";
    if (skipped(N)) return;
    char me[64];
    if (!nic_addr(me, sizeof me)) { skip(N, "no default route"); return; }
    const char *G = "239.255.42.97", *OTHER = "192.0.2.77";
    uint16_t port = 27031;
    int a = bound_reuse(port, SO_REUSEADDR), s = socket(AF_INET, SOCK_DGRAM, 0);
    if (a < 0 || s < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    struct sockaddr_in to = sin4(G, port);
    unsigned fm = 9, first = 0;
    int nomem = ms_set(a, G, 0, 0, NULL), enomem = errno;   /* not a member yet */
    join(a, G, "0.0.0.0", 0);
    int n0 = ms_get(a, G, 4, &fm, &first); unsigned fm0 = fm;
    const char *only_other[] = { OTHER }, *only_me[] = { me };
    int s1 = ms_set(a, G, 1, 1, only_other);               /* INCLUDE {OTHER} */
    send_to(s, "f1", 2, &to);
    int r1 = got(a, "f1", 400);
    int n1 = ms_get(a, G, 4, &fm, &first); unsigned fm1 = fm;
    int s2 = ms_set(a, G, 1, 1, only_me);                  /* INCLUDE {me} */
    send_to(s, "f2", 2, &to);
    int r2 = got(a, "f2", 600);
    int s3 = ms_set(a, G, 0, 1, only_me);                  /* EXCLUDE {me} */
    send_to(s, "f3", 2, &to);
    int r3 = got(a, "f3", 400);
    int s4 = ms_set(a, G, 1, 0, NULL);                     /* INCLUDE {}: nothing */
    send_to(s, "f4", 2, &to);
    int r4 = got(a, "f4", 400);
    int s5 = ms_set(a, G, 0, 0, NULL);                     /* EXCLUDE {}: all */
    send_to(s, "f5", 2, &to);
    int r5 = got(a, "f5", 600);
    const char *many[11]; char names[11][16];
    for (int i = 0; i < 11; i++) { snprintf(names[i], sizeof names[i], "192.0.2.%d", i + 1); many[i] = names[i]; }
    unsigned char big[16 + 4 * 11]; memset(big, 0, sizeof big);
    inet_pton(AF_INET, G, big); unsigned one = 1, eleven = 11; memcpy(big + 8, &one, 4); memcpy(big + 12, &eleven, 4);
    for (int i = 0; i < 11; i++) inet_pton(AF_INET, many[i], big + 16 + 4 * i);
    int s6 = setsockopt(a, IPPROTO_IP, IP_MSFILTER, big, sizeof big), e6 = errno;
    /* Two sources, room for one: numsrc counts both, one is copied. */
    const char *two[] = { OTHER, me };
    ms_set(a, G, 1, 2, two);
    int n2 = ms_get(a, G, 1, &fm, &first);
    close(a); close(s);
    if (nomem == 0 || enomem != EADDRNOTAVAIL) fail(N, "set without membership: %d %s, want EADDRNOTAVAIL", nomem, strerror(enomem));
    else if (n0 != 0 || fm0 != 0) fail(N, "default filter: numsrc %d fmode %u, want 0 / EXCLUDE (0)", n0, fm0);
    else if (s1 || s2 || s3 || s4 || s5) fail(N, "IP_MSFILTER set: %d %d %d %d %d (%s)", s1, s2, s3, s4, s5, strerror(errno));
    else if (r1 != 0) fail(N, "INCLUDE {other} still heard us");
    else if (n1 != 1 || fm1 != 1) fail(N, "get after INCLUDE {other}: numsrc %d fmode %u, want 1 / 1", n1, fm1);
    else if (r2 != 1) fail(N, "INCLUDE {me}: heard %d, want 1", r2);
    else if (r3 != 0) fail(N, "EXCLUDE {me} still heard us");
    else if (r4 != 0) fail(N, "INCLUDE {} still heard us");
    else if (r5 != 1) fail(N, "EXCLUDE {}: heard %d, want 1", r5);
    else if (s6 == 0 || e6 != ENOBUFS) fail(N, "11 sources: %d %s, want ENOBUFS", s6, strerror(e6));
    else if (n2 != 2) fail(N, "numsrc with a short buffer: %d, want 2", n2);
    else pass(N);
}

/* IP_MULTICAST_ALL=0: a socket hears only the groups it joined itself. */
static void udp_mcast_all(void) {
    const char *N = "udp_mcast_all";
    if (skipped(N)) return;
    char me[64];
    if (!nic_addr(me, sizeof me)) { skip(N, "no default route"); return; }
    const char *G = "239.255.42.96";
    uint16_t port = 27032;
    int a = bound_reuse(port, SO_REUSEADDR), b = bound_reuse(port, SO_REUSEADDR);
    int s = socket(AF_INET, SOCK_DGRAM, 0), zero = 0;
    if (a < 0 || b < 0 || s < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    struct sockaddr_in to = sin4(G, port);
    join(b, G, "0.0.0.0", 0);                       /* the interface is a member via B */
    int sa = setsockopt(a, IPPROTO_IP, IP_MULTICAST_ALL, &zero, sizeof zero);
    int v = -1; socklen_t l = sizeof v;
    getsockopt(a, IPPROTO_IP, IP_MULTICAST_ALL, &v, &l);
    send_to(s, "a1", 2, &to);
    int ra = got(a, "a1", 400), rb = got(b, "a1", 600);
    int ja = join(a, G, "0.0.0.0", 0);              /* A joins: it hears it */
    send_to(s, "a2", 2, &to);
    int ra2 = got(a, "a2", 600);
    int la = join(a, G, "0.0.0.0", 1);              /* and not after leaving */
    send_to(s, "a3", 2, &to);
    int ra3 = got(a, "a3", 400), rb3 = got(b, "a3", 600);
    int one = 1;
    setsockopt(a, IPPROTO_IP, IP_MULTICAST_ALL, &one, sizeof one);   /* default again */
    send_to(s, "a4", 2, &to);
    int ra4 = got(a, "a4", 600);
    close(a); close(b); close(s);
    if (sa || v != 0) fail(N, "IP_MULTICAST_ALL=0: set %d, reads %d", sa, v);
    else if (ra != 0 || rb != 1) fail(N, "nonmember with ALL=0 heard %d (member %d), want 0 1", ra, rb);
    else if (ja || ra2 != 1) fail(N, "after own join: join %d heard %d, want 0 1", ja, ra2);
    else if (la || ra3 != 0 || rb3 != 1) fail(N, "after own leave: leave %d heard %d (B %d), want 0 0 1", la, ra3, rb3);
    else if (ra4 != 1) fail(N, "ALL=1 again: heard %d, want 1", ra4);
    else pass(N);
}

/* MCAST_JOIN_GROUP / LEAVE_GROUP / BLOCK_SOURCE / UNBLOCK_SOURCE (group_req). */
static void udp_mcast_group_req(void) {
    const char *N = "udp_mcast_group_req";
    if (skipped(N)) return;
    char me[64];
    if (!nic_addr(me, sizeof me)) { skip(N, "no default route"); return; }
    const char *G = "239.255.42.95";
    uint16_t port = 27033;
    int c = bound_reuse(port, SO_REUSEADDR), s = socket(AF_INET, SOCK_DGRAM, 0);
    if (c < 0 || s < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    struct sockaddr_in to = sin4(G, port);
    int z = 0; setsockopt(c, IPPROTO_IP, IP_MULTICAST_ALL, &z, sizeof z);
    int jc = gsropt(c, T_MCAST_JOIN_GROUP, G, me);
    int jdup = gsropt(c, T_MCAST_JOIN_GROUP, G, me), edup = errno;
    send_to(s, "g1", 2, &to);
    int r1 = got(c, "g1", 600);
    int bc = gsropt(c, T_MCAST_BLOCK_SOURCE, G, me);
    send_to(s, "g2", 2, &to);
    int r2 = got(c, "g2", 400);
    int uc = gsropt(c, T_MCAST_UNBLOCK_SOURCE, G, me);
    send_to(s, "g3", 2, &to);
    int r3 = got(c, "g3", 600);
    int lc = gsropt(c, T_MCAST_LEAVE_GROUP, G, me), lc2 = gsropt(c, T_MCAST_LEAVE_GROUP, G, me), elc2 = errno;
    send_to(s, "g4", 2, &to);
    int r4 = got(c, "g4", 400);
    close(c); close(s);
    if (jc || bc || uc || lc) fail(N, "options: join %d block %d unblock %d leave %d (%s)", jc, bc, uc, lc, strerror(errno));
    else if (jdup == 0 || edup != EADDRINUSE) fail(N, "second MCAST_JOIN_GROUP: %d %s, want EADDRINUSE", jdup, strerror(edup));
    else if (r1 != 1 || r2 != 0 || r3 != 1) fail(N, "rx join/block/unblock: %d %d %d, want 1 0 1", r1, r2, r3);
    else if (lc2 == 0 || elc2 != EADDRNOTAVAIL) fail(N, "second leave: %d %s, want EADDRNOTAVAIL", lc2, strerror(elc2));
    else if (r4 != 0) fail(N, "heard the group after MCAST_LEAVE_GROUP (ALL=0)");
    else pass(N);
}

/* IPv6 group options: state and errors (delivery needs the NIC's v6 loop). */
static int g6req(int fd, int opt, const char *group, const char *src, int ifx) {
    unsigned char r[264]; memset(r, 0, sizeof r);
    struct sockaddr_in6 g = {0}, sr = {0};
    g.sin6_family = AF_INET6; inet_pton(AF_INET6, group, &g.sin6_addr);
    if (src) { sr.sin6_family = AF_INET6; inet_pton(AF_INET6, src, &sr.sin6_addr); }
    memcpy(r, &ifx, 4); memcpy(r + 8, &g, sizeof g); memcpy(r + 136, &sr, sizeof sr);
    return setsockopt(fd, IPPROTO_IPV6, opt, r, src ? 264 : 136);
}

static void udp6_mcast_req(void) {
    const char *N = "udp6_mcast_req";
    if (skipped(N)) return;
    int a = socket(AF_INET6, SOCK_DGRAM, 0);
    if (a < 0) { skip(N, "no AF_INET6"); return; }
    int lo = if_nametoindex("lo");
    const char *G = "ff02::4242", *S = "fe80::1", *G2 = "ff02::4243";
    int j = g6req(a, T_MCAST_JOIN_GROUP, G, NULL, lo);
    int jdup = g6req(a, T_MCAST_JOIN_GROUP, G, NULL, lo), edup = errno;
    int bl = g6req(a, T_MCAST_BLOCK_SOURCE, G, S, lo);
    int bl2 = g6req(a, T_MCAST_BLOCK_SOURCE, G, S, lo), ebl2 = errno;
    int ub = g6req(a, T_MCAST_UNBLOCK_SOURCE, G, S, lo);
    int ub2 = g6req(a, T_MCAST_UNBLOCK_SOURCE, G, S, lo), eub2 = errno;
    int lv = g6req(a, T_MCAST_LEAVE_GROUP, G, NULL, lo);
    int lv2 = g6req(a, T_MCAST_LEAVE_GROUP, G, NULL, lo), elv2 = errno;
    int sj = g6req(a, T_MCAST_JOIN_SOURCE_GROUP, G2, S, lo);
    int sj2 = g6req(a, T_MCAST_JOIN_SOURCE_GROUP, G2, S, lo), esj2 = errno;
    int sl = g6req(a, T_MCAST_LEAVE_SOURCE_GROUP, G2, S, lo);
    int uni = g6req(a, T_MCAST_JOIN_GROUP, "fe80::2", NULL, lo), euni = errno;
    struct ipv6_mreq m; memset(&m, 0, sizeof m);
    inet_pton(AF_INET6, G, &m.ipv6mr_multiaddr); m.ipv6mr_interface = lo;
    int mj = setsockopt(a, IPPROTO_IPV6, IPV6_JOIN_GROUP, &m, sizeof m);
    int mj2 = setsockopt(a, IPPROTO_IPV6, IPV6_JOIN_GROUP, &m, sizeof m), emj2 = errno;
    int ml = setsockopt(a, IPPROTO_IPV6, IPV6_LEAVE_GROUP, &m, sizeof m);
    int all = -1, z = 0; socklen_t l = sizeof all;
    getsockopt(a, IPPROTO_IPV6, 29, &all, &l);
    int sa = setsockopt(a, IPPROTO_IPV6, 29, &z, sizeof z);
    int all0 = -1; l = sizeof all0;
    getsockopt(a, IPPROTO_IPV6, 29, &all0, &l);
    close(a);
    if (j || bl || ub || lv || sj || sl) fail(N, "options: join %d block %d unblock %d leave %d srcjoin %d srcleave %d (%s)", j, bl, ub, lv, sj, sl, strerror(errno));
    else if (jdup == 0 || edup != EADDRINUSE) fail(N, "second join: %d %s, want EADDRINUSE", jdup, strerror(edup));
    else if (bl2 == 0 || ebl2 != EADDRINUSE) fail(N, "second block: %d %s, want EADDRINUSE", bl2, strerror(ebl2));
    else if (ub2 == 0 || eub2 != EADDRNOTAVAIL) fail(N, "second unblock: %d %s, want EADDRNOTAVAIL", ub2, strerror(eub2));
    else if (lv2 == 0 || elv2 != EADDRNOTAVAIL) fail(N, "second leave: %d %s, want EADDRNOTAVAIL", lv2, strerror(elv2));
    else if (sj2 == 0 || esj2 != EADDRINUSE) fail(N, "second source join: %d %s, want EADDRINUSE", sj2, strerror(esj2));
    else if (uni == 0 || euni != EINVAL) fail(N, "join of a unicast v6 address: %d %s, want EINVAL", uni, strerror(euni));
    else if (mj || mj2 == 0 || emj2 != EADDRINUSE || ml) fail(N, "IPV6_JOIN_GROUP: %d, again %d %s, leave %d", mj, mj2, strerror(emj2), ml);
    else if (all != 1 || sa || all0 != 0) fail(N, "IPV6_MULTICAST_ALL: default %d set %d now %d, want 1 0 0", all, sa, all0);
    else pass(N);
}

/* The default NIC's link-local address and index, from /proc/net/if_inet6. */
static int nic6(char *ip, size_t n, int *ifx) {
    FILE *f = fopen("/proc/net/if_inet6", "r");
    if (!f) return 0;
    char h[40], dev[32]; unsigned idx, plen, scope, fl; int ok = 0;
    while (fscanf(f, "%32s %x %x %x %x %31s", h, &idx, &plen, &scope, &fl, dev) == 6) {
        if (scope != 0x20 || !strcmp(dev, "lo")) continue;
        struct in6_addr a; for (int i = 0; i < 16; i++) { unsigned b; sscanf(h + 2 * i, "%2x", &b); a.s6_addr[i] = b; }
        inet_ntop(AF_INET6, &a, ip, n); *ifx = idx; ok = 1; break;
    }
    fclose(f);
    return ok;
}

static int v6rx(const char *group, uint16_t port, int ifx, int mode) {
    int fd = socket(AF_INET6, SOCK_DGRAM, 0), one = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in6 a = {0}; a.sin6_family = AF_INET6; a.sin6_port = htons(port);
    if (bind(fd, (struct sockaddr *)&a, sizeof a) < 0) { close(fd); return -1; }
    if (mode == 0 && g6req(fd, T_MCAST_JOIN_GROUP, group, NULL, ifx)) { close(fd); return -1; }
    return fd;
}

static int mf6(int fd, int set, const char *group, int ifx, unsigned fmode, int n, const char **srcs, unsigned *fm_out) {
    unsigned char b[144 + 128 * 4]; memset(b, 0, sizeof b);
    struct sockaddr_in6 g = {0}; g.sin6_family = AF_INET6; inet_pton(AF_INET6, group, &g.sin6_addr);
    memcpy(b, &ifx, 4); memcpy(b + 8, &g, sizeof g);
    unsigned numsrc = set ? n : 4;
    memcpy(b + 136, &fmode, 4); memcpy(b + 140, &numsrc, 4);
    for (int i = 0; i < n && set; i++) {
        struct sockaddr_in6 s = {0}; s.sin6_family = AF_INET6; inet_pton(AF_INET6, srcs[i], &s.sin6_addr);
        memcpy(b + 144 + 128 * i, &s, sizeof s);
    }
    if (set) return setsockopt(fd, IPPROTO_IPV6, 48, b, 144 + 128 * n);
    socklen_t l = sizeof b;
    if (getsockopt(fd, IPPROTO_IPV6, 48, b, &l) < 0) return -1;
    memcpy(&numsrc, b + 140, 4); memcpy(fm_out, b + 136, 4);
    return (int)numsrc;
}

/* IPv6 multicast through the NIC: delivery to a joined socket, the source
 * filters, IPV6_MULTICAST_LOOP / _IF / _HOPS and MCAST_MSFILTER. */
static void udp6_mcast_deliver(void) {
    const char *N = "udp6_mcast_deliver";
    if (skipped(N)) return;
    char me[64]; int ifx = 0;
    if (!nic6(me, sizeof me, &ifx)) { skip(N, "no NIC link-local address"); return; }
    const char *G = "ff15::4a01", *OTHER = "fe80::99";
    uint16_t port = 27041;
    int a = v6rx(G, port, ifx, 0), c = v6rx(G, port, ifx, 1);   /* c never joined */
    int s = socket(AF_INET6, SOCK_DGRAM, 0), noall = 0;
    if (a < 0 || c < 0 || s < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    setsockopt(c, IPPROTO_IPV6, 29, &noall, sizeof noall);   /* IPV6_MULTICAST_ALL=0 */
    { struct sockaddr_in6 sb = {0}; sb.sin6_family = AF_INET6; inet_pton(AF_INET6, me, &sb.sin6_addr); sb.sin6_scope_id = ifx;
      if (bind(s, (struct sockaddr *)&sb, sizeof sb) < 0) { fail(N, "bind sender to %s: %s", me, strerror(errno)); return; } }
    int sif = setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_IF, &ifx, sizeof ifx);
    int hops = 4, sh = setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_HOPS, &hops, sizeof hops);
    int rif = -1; socklen_t l = sizeof rif;
    getsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_IF, &rif, &l);
    int lp = -1; l = sizeof lp;
    getsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_LOOP, &lp, &l);
    struct sockaddr_in6 to = {0}; to.sin6_family = AF_INET6; to.sin6_port = htons(port);
    inet_pton(AF_INET6, G, &to.sin6_addr); to.sin6_scope_id = ifx;
    #define S6(m) sendto(s, m, 2, 0, (struct sockaddr *)&to, sizeof to)
    long sd = S6("d1");
    int r1 = got(a, "d1", 800), rc = got(c, "d1", 200);
    const char *only_other[] = { OTHER }, *only_me[] = { me };
    unsigned fm = 9;
    int m1 = mf6(a, 1, G, ifx, 1, 1, only_other, NULL);          /* INCLUDE {other} */
    S6("d2"); int r2 = got(a, "d2", 400);
    int n1 = mf6(a, 0, G, ifx, 0, 0, NULL, &fm); unsigned fm1 = fm;
    int m2 = mf6(a, 1, G, ifx, 1, 1, only_me, NULL);             /* INCLUDE {me} */
    S6("d3"); int r3 = got(a, "d3", 800);
    int m3 = mf6(a, 1, G, ifx, 0, 1, only_me, NULL);             /* EXCLUDE {me} */
    S6("d4"); int r4 = got(a, "d4", 400);
    int m4 = mf6(a, 1, G, ifx, 0, 0, NULL, NULL);                /* EXCLUDE {} */
    int zero = 0, sl = setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_LOOP, &zero, sizeof zero);
    S6("d5"); int r5 = got(a, "d5", 400);                        /* loop off: nothing */
    int one = 1; setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_LOOP, &one, sizeof one);
    S6("d6"); int r6 = got(a, "d6", 800);
    int bad = -1, e7 = 0;
    { int nope = 99999; bad = setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_IF, &nope, sizeof nope); e7 = errno; }
    int nomem = mf6(c, 1, G, ifx, 0, 0, NULL, NULL), enomem = errno;  /* c is no member */
    char igmp6[4096] = ""; int hasg = 0;
    FILE *f = fopen("/proc/net/igmp6", "r");
    if (f) { size_t k = fread(igmp6, 1, sizeof igmp6 - 1, f); igmp6[k] = 0; fclose(f);
             hasg = strstr(igmp6, "ff150000000000000000000000004a01") != NULL; }
    int alln = strstr(igmp6, "ff020000000000000000000000000001") != NULL;
    close(a); close(c); close(s);
    if (sif || sh || rif != ifx || lp != 1) fail(N, "options: IF %d (reads %d, want %d) HOPS %d LOOP default %d (%s)", sif, rif, ifx, sh, lp, strerror(errno));
    else if (sd != 2) fail(N, "sendto the group: %ld %s", sd, strerror(errno));
    else if (r1 != 1) fail(N, "member did not hear the v6 group: %d", r1);
    else if (rc != 0) fail(N, "a non-member heard the group");
    else if (m1 || m2 || m3 || m4 || sl) fail(N, "MCAST_MSFILTER set: %d %d %d %d loop %d (%s)", m1, m2, m3, m4, sl, strerror(errno));
    else if (r2 != 0) fail(N, "INCLUDE {other} still heard us");
    else if (n1 != 1 || fm1 != 1) fail(N, "MCAST_MSFILTER get: numsrc %d fmode %u, want 1 / 1", n1, fm1);
    else if (r3 != 1) fail(N, "INCLUDE {me}: heard %d, want 1", r3);
    else if (r4 != 0) fail(N, "EXCLUDE {me} still heard us");
    else if (r5 != 0) fail(N, "IPV6_MULTICAST_LOOP=0 still delivered");
    else if (r6 != 1) fail(N, "after EXCLUDE {} and loop on: heard %d, want 1", r6);
    else if (bad == 0 || e7 != ENODEV) fail(N, "IPV6_MULTICAST_IF bad index: %d %s, want ENODEV", bad, strerror(e7));
    else if (nomem == 0 || enomem != EADDRNOTAVAIL) fail(N, "MCAST_MSFILTER without membership: %d %s", nomem, strerror(enomem));
    else if (!hasg || !alln) fail(N, "/proc/net/igmp6 lacks the joined group (%d) or ff02::1 (%d)", hasg, alln);
    else pass(N);
}

/* MCAST_MSFILTER at IPPROTO_IP (group_filter with AF_INET sockaddrs). */
static void udp_mcast_msfilter4(void) {
    const char *N = "udp_mcast_msfilter4";
    if (skipped(N)) return;
    char me[64];
    if (!nic_addr(me, sizeof me)) { skip(N, "no default route"); return; }
    int a = bound_reuse(27042, SO_REUSEADDR);
    if (a < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    join(a, "239.255.42.98", "0.0.0.0", 0);
    unsigned char b[144 + 128 * 2]; memset(b, 0, sizeof b);
    struct sockaddr_in g = sin4("239.255.42.98", 0), s1 = sin4("192.0.2.5", 0), s2 = sin4("192.0.2.6", 0);
    unsigned fmode = 1, num = 2;
    memcpy(b + 8, &g, sizeof g); memcpy(b + 136, &fmode, 4); memcpy(b + 140, &num, 4);
    memcpy(b + 144, &s1, sizeof s1); memcpy(b + 144 + 128, &s2, sizeof s2);
    int st = setsockopt(a, IPPROTO_IP, 48, b, sizeof b), est = errno;
    unsigned char r[sizeof b]; memset(r, 0, sizeof r);
    memcpy(r + 8, &g, sizeof g); num = 2; memcpy(r + 140, &num, 4);
    socklen_t l = sizeof r;
    int gt = getsockopt(a, IPPROTO_IP, 48, r, &l), egt = errno;
    unsigned rf, rn; memcpy(&rf, r + 136, 4); memcpy(&rn, r + 140, 4);
    struct sockaddr_in f1; memcpy(&f1, r + 144, sizeof f1);
    close(a);
    if (st) fail(N, "MCAST_MSFILTER set: %s", strerror(est));
    else if (gt) fail(N, "MCAST_MSFILTER get: %s", strerror(egt));
    else if (rf != 1 || rn != 2 || f1.sin_addr.s_addr != s1.sin_addr.s_addr) fail(N, "get: fmode %u numsrc %u first %s, want 1 2 192.0.2.5", rf, rn, inet_ntoa(f1.sin_addr));
    else pass(N);
}
