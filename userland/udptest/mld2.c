/* Lane mldv2: MLDv2 listener reports (RFC 3810), the IPv6 twin of igmp3.c,
 * read off an IFF_TUN wire in a fresh netns (i3_tun): ICMPv6 type 143 to
 * ff02::16, hop limit 1, Router Alert hop-by-hop, one record per (group,
 * interface) from the union of the per-socket filters; MODE_IS_* answers to
 * general / group / group-and-source queries; MLDv1 compatibility after a v1
 * query; the solicited-node group in /proc/net/igmp6. And the local copy of
 * IPv6 multicast on lo. Included from udptest.c after igmp3.c (uses its tun
 * helpers, i3_csum, g6req, mf6, v6rx and got). */

struct m6rec { int ty; uint8_t group[16]; int n; uint8_t src[4][16]; };
struct m6v1 { int ty; uint8_t group[16]; };
struct m6seen { int nrep, nrec, nv1; struct m6rec r[32]; struct m6v1 v1[16]; int v2_bad; char bad[140]; };

static const uint8_t M6_ALLR[16] = { 0xff, 2, [15] = 2 };

/* Read the wire for `ms`, parsing every MLD packet into `o`. */
static void m6_collect(int tun, struct m6seen *o, int ms) {
    static uint8_t pk[2048];
    memset(o, 0, sizeof *o);
    for (int t = 0; t < ms; t += 20) {
        ssize_t n;
        while ((n = read(tun, pk, sizeof pk)) > 0) {
            if ((pk[0] >> 4) != 6 || n < 48 || pk[6] != 0 || pk[40] != 58) continue; /* hop-by-hop -> ICMPv6 */
            uint8_t *m = pk + 48; int ilen = n - 48;
            if (m[0] != 143 && m[0] != 131 && m[0] != 132) continue;
            uint8_t ps[2048]; memcpy(ps, pk + 8, 32); ps[32] = ps[33] = 0; ps[34] = ilen >> 8; ps[35] = ilen & 255; ps[36] = ps[37] = ps[38] = 0; ps[39] = 58;
            memcpy(ps + 40, m, ilen);
            int ra = !memcmp(pk + 40, "\x3a\x00\x05\x02\x00\x00\x01\x00", 8);
            if (pk[7] != 1 || !ra || i3_csum(ps, 40 + ilen)) {
                snprintf(o->bad, sizeof o->bad, "malformed MLD type %d: hop %d router-alert %d csum %04x", m[0], pk[7], ra, i3_csum(ps, 40 + ilen));
                continue;
            }
            if (m[0] != 143) {
                /* v1: Report goes to the group, Done to ff02::2 */
                int done = m[0] == 132;
                if (memcmp(pk + 24, done ? M6_ALLR : m + 8, 16)) { snprintf(o->bad, sizeof o->bad, "MLDv1 type %d to the wrong address", m[0]); continue; }
                if (o->nv1 < 16) { o->v1[o->nv1].ty = m[0]; memcpy(o->v1[o->nv1].group, m + 8, 16); o->nv1++; }
                continue;
            }
            if (memcmp(pk + 24, "\xff\x02\0\0\0\0\0\0\0\0\0\0\0\0\0\x16", 16)) {
                snprintf(o->bad, sizeof o->bad, "MLDv2 report not to ff02::16");
                continue;
            }
            int nr = (m[6] << 8) | m[7], off = 8;
            o->nrep++;
            for (int i = 0; i < nr && off + 20 <= ilen && o->nrec < 32; i++) {
                struct m6rec *r = &o->r[o->nrec++];
                r->ty = m[off]; r->n = (m[off + 2] << 8) | m[off + 3]; memcpy(r->group, m + off + 4, 16);
                for (int k = 0; k < r->n && k < 4; k++) memcpy(r->src[k], m + off + 20 + 16 * k, 16);
                off += 20 + 16 * r->n;
            }
        }
        msleep(20);
    }
}

static int m6_count(const struct m6seen *o, int ty, const char *group, int nsrc, const char *src0) {
    uint8_t g[16], s[16]; inet_pton(AF_INET6, group, g); if (src0) inet_pton(AF_INET6, src0, s);
    int c = 0;
    for (int i = 0; i < o->nrec; i++) {
        const struct m6rec *r = &o->r[i];
        if (r->ty != ty || memcmp(r->group, g, 16) || r->n != nsrc) continue;
        if (src0 && memcmp(r->src[0], s, 16)) continue;
        c++;
    }
    return c;
}

static int m6_any(const struct m6seen *o, const char *group) {
    uint8_t g[16]; inet_pton(AF_INET6, group, g);
    int c = 0;
    for (int i = 0; i < o->nrec; i++) if (!memcmp(o->r[i].group, g, 16)) c++;
    for (int i = 0; i < o->nv1; i++) if (!memcmp(o->v1[i].group, g, 16)) c++;
    return c;
}

static int m6_v1count(const struct m6seen *o, int ty, const char *group) {
    uint8_t g[16]; inet_pton(AF_INET6, group, g);
    int c = 0;
    for (int i = 0; i < o->nv1; i++) if (o->v1[i].ty == ty && !memcmp(o->v1[i].group, g, 16)) c++;
    return c;
}

/* A listener query into the tun: v2 (nsrc >= 0) or the 24-byte v1 (nsrc < 0),
 * to ff02::1 (general, group NULL) or to the group. Source fe80::beef. */
static void m6_query(int tun, const char *group, int maxresp_ms, int nsrc, const char **srcs) {
    uint8_t p[512]; memset(p, 0, sizeof p);
    int len = nsrc < 0 ? 24 : 28 + 16 * nsrc, tot = 40 + 8 + len;
    p[0] = 0x60; p[4] = (8 + len) >> 8; p[5] = (8 + len) & 255; p[6] = 0; p[7] = 1;
    inet_pton(AF_INET6, "fe80::beef", p + 8);
    inet_pton(AF_INET6, group ? group : "ff02::1", p + 24);
    memcpy(p + 40, "\x3a\x00\x05\x02\x00\x00\x01\x00", 8);
    uint8_t *m = p + 48;
    m[0] = 130; m[4] = maxresp_ms >> 8; m[5] = maxresp_ms & 255;
    if (group) inet_pton(AF_INET6, group, m + 8);
    if (nsrc >= 0) {
        m[24] = 2; m[25] = 125; m[26] = nsrc >> 8; m[27] = nsrc & 255;
        for (int i = 0; i < nsrc; i++) inet_pton(AF_INET6, srcs[i], m + 28 + 16 * i);
    }
    uint8_t ps[640]; memcpy(ps, p + 8, 32); ps[32] = ps[33] = 0; ps[34] = len >> 8; ps[35] = len & 255; ps[36] = ps[37] = ps[38] = 0; ps[39] = 58;
    memcpy(ps + 40, m, len);
    uint16_t c = i3_csum(ps, 40 + len); m[2] = c >> 8; m[3] = c & 255;
    if (write(tun, p, tot) != tot) printf("# mld2: tun write failed: %s\n", strerror(errno));
}

static int m6_join(int ifx, const char *g) {
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    struct ipv6_mreq m; memset(&m, 0, sizeof m); inet_pton(AF_INET6, g, &m.ipv6mr_multiaddr); m.ipv6mr_interface = ifx;
    if (s < 0 || setsockopt(s, IPPROTO_IPV6, IPV6_JOIN_GROUP, &m, sizeof m)) { if (s >= 0) close(s); return -1; }
    return s;
}

#define M6_WANT(cond, ...) do { if (!(cond)) { snprintf(why, sizeof why, __VA_ARGS__); return; } } while (0)
#define M6_DRAIN 1300

static void m6_reports_child(int tun, char *why_out) {
    char why[200] = ""; struct m6seen o;
    const char *G1 = "ff15::78:1", *G2 = "ff15::78:2", *G3 = "ff15::78:3", *G4 = "ff15::78:4";
    const char *GN = "ff01::78:99", *GA = "ff02::1";
    const char *S7 = "2001:db8::7", *S8 = "2001:db8::8", *S9 = "2001:db8::9";
    int ifx = if_nametoindex("igm0");
    do {
        /* the two groups MLD never reports, joined first: no record for them */
        int sn = m6_join(ifx, GN), sa = m6_join(ifx, GA);
        I3_WANT(sn >= 0 && sa >= 0, "join of an unreported group: %s", strerror(errno));
        /* any-source join: CHANGE_TO_EXCLUDE({}) sent twice */
        int s1 = m6_join(ifx, G1);
        M6_WANT(s1 >= 0, "join G1: %s", strerror(errno));
        m6_collect(tun, &o, M6_DRAIN + 500);
        M6_WANT(!o.bad[0], "%s", o.bad);
        M6_WANT(m6_count(&o, 4, G1, 0, NULL) == 2, "join: TO_EX({}) x%d (want 2), reports %d", m6_count(&o, 4, G1, 0, NULL), o.nrep);
        M6_WANT(m6_any(&o, GN) == 0 && m6_any(&o, GA) == 0, "a node-local group or ff02::1 was reported");
        M6_WANT(o.nv1 == 0, "%d MLDv1 messages on a v2 interface", o.nv1);
        /* source join on a fresh group: ALLOW_NEW(S9) */
        int s2 = socket(AF_INET6, SOCK_DGRAM, 0);
        M6_WANT(g6req(s2, T_MCAST_JOIN_SOURCE_GROUP, G2, S9, ifx) == 0, "JOIN_SOURCE_GROUP: %s", strerror(errno));
        m6_collect(tun, &o, M6_DRAIN);
        M6_WANT(m6_count(&o, 5, G2, 1, S9) >= 1, "JOIN_SOURCE_GROUP: no ALLOW_NEW(G2,{S9}) (records %d)", o.nrec);
        /* block a source on the any-source group: BLOCK_OLD(S8) */
        M6_WANT(g6req(s1, T_MCAST_BLOCK_SOURCE, G1, S8, ifx) == 0, "BLOCK_SOURCE: %s", strerror(errno));
        m6_collect(tun, &o, M6_DRAIN);
        M6_WANT(m6_count(&o, 6, G1, 1, S8) >= 1, "BLOCK_SOURCE: no BLOCK_OLD(G1,{S8}) (records %d)", o.nrec);
        /* unblock: ALLOW_NEW(S8) */
        M6_WANT(g6req(s1, T_MCAST_UNBLOCK_SOURCE, G1, S8, ifx) == 0, "UNBLOCK_SOURCE: %s", strerror(errno));
        m6_collect(tun, &o, M6_DRAIN);
        M6_WANT(m6_count(&o, 5, G1, 1, S8) >= 1, "UNBLOCK_SOURCE: no ALLOW_NEW(G1,{S8}) (records %d)", o.nrec);
        /* MCAST_MSFILTER to INCLUDE({S7}): exclude -> include = CHANGE_TO_INCLUDE({S7}) */
        const char *only7[] = { S7 };
        M6_WANT(mf6(s1, 1, G1, ifx, 1, 1, only7, NULL) == 0, "MCAST_MSFILTER: %s", strerror(errno));
        m6_collect(tun, &o, M6_DRAIN);
        M6_WANT(m6_count(&o, 3, G1, 1, S7) >= 1, "MSFILTER include: no TO_IN(G1,{S7}) (records %d)", o.nrec);
        /* leave: BLOCK_OLD of the sources it still had */
        close(s1);
        m6_collect(tun, &o, M6_DRAIN);
        M6_WANT(m6_count(&o, 6, G1, 1, S7) >= 1, "leave include group: no BLOCK_OLD(G1,{S7}) (records %d)", o.nrec);
        close(s2);
        m6_collect(tun, &o, M6_DRAIN);
        M6_WANT(m6_count(&o, 6, G2, 1, S9) >= 1, "leave: no BLOCK_OLD(G2,{S9}) (records %d)", o.nrec);
        /* any-source leave: CHANGE_TO_INCLUDE({}) */
        int s3 = m6_join(ifx, G3);
        M6_WANT(s3 >= 0, "join G3");
        m6_collect(tun, &o, M6_DRAIN + 400);
        close(s3);
        m6_collect(tun, &o, M6_DRAIN);
        M6_WANT(m6_count(&o, 3, G3, 0, NULL) >= 1, "any-source leave: no TO_IN({}) (records %d)", o.nrec);
        /* union of two sockets: include {S7} then include {S8} reports only the new source,
         * an any-source member then turns the group to EXCLUDE({}) */
        int a = socket(AF_INET6, SOCK_DGRAM, 0), b = socket(AF_INET6, SOCK_DGRAM, 0);
        M6_WANT(g6req(a, T_MCAST_JOIN_SOURCE_GROUP, G4, S7, ifx) == 0, "G4 add S7");
        m6_collect(tun, &o, M6_DRAIN + 400);
        M6_WANT(g6req(b, T_MCAST_JOIN_SOURCE_GROUP, G4, S8, ifx) == 0, "G4 add S8");
        m6_collect(tun, &o, M6_DRAIN);
        M6_WANT(m6_count(&o, 5, G4, 1, S8) >= 1 && m6_count(&o, 5, G4, 1, S7) == 0, "union: want ALLOW_NEW({S8}) only (S8 %d, S7 %d)", m6_count(&o, 5, G4, 1, S8), m6_count(&o, 5, G4, 1, S7));
        int c = m6_join(ifx, G4);
        M6_WANT(c >= 0, "G4 any-source join");
        m6_collect(tun, &o, M6_DRAIN);
        M6_WANT(m6_count(&o, 4, G4, 0, NULL) >= 1, "union: any-source member did not make TO_EX({}) (records %d)", o.nrec);
        close(a); close(b); close(c); close(sn); close(sa);
    } while (0);
    snprintf(why_out, 200, "%s", why);
}

static void m6_query_child(int tun, char *why_out) {
    char why[200] = ""; struct m6seen o;
    const char *G1 = "ff15::78:11", *G2 = "ff15::78:12", *S1 = "2001:db8::5", *SX = "2001:db8::6";
    int ifx = if_nametoindex("igm0");
    do {
        int s1 = m6_join(ifx, G1);
        M6_WANT(s1 >= 0, "join G1: %s", strerror(errno));
        int s2 = socket(AF_INET6, SOCK_DGRAM, 0);
        M6_WANT(g6req(s2, T_MCAST_JOIN_SOURCE_GROUP, G2, S1, ifx) == 0, "JOIN_SOURCE_GROUP: %s", strerror(errno));
        m6_collect(tun, &o, 2200);
        M6_WANT(o.nv1 == 0, "%d MLDv1 messages", o.nv1);
        /* general query, max response 1 s */
        m6_query(tun, NULL, 1000, 0, NULL);
        m6_collect(tun, &o, 1500);
        M6_WANT(!o.bad[0], "%s", o.bad);
        M6_WANT(m6_count(&o, 2, G1, 0, NULL) == 1 && m6_count(&o, 1, G2, 1, S1) == 1,
                "general query: want IS_EX(G1,{}) and IS_IN(G2,{S1}), got %d records over %d reports", o.nrec, o.nrep);
        /* group-specific */
        m6_query(tun, G2, 1000, 0, NULL);
        m6_collect(tun, &o, 1500);
        M6_WANT(m6_count(&o, 1, G2, 1, S1) == 1 && m6_any(&o, G1) == 0, "group query: want IS_IN(G2,{S1}) only, got %d records (G1 %d)", o.nrec, m6_any(&o, G1));
        /* group-and-source-specific: only the sources asked about that are forwarded */
        const char *q[2] = { S1, SX };
        m6_query(tun, G2, 1000, 2, q);
        m6_collect(tun, &o, 1500);
        M6_WANT(m6_count(&o, 1, G2, 1, S1) == 1, "group+source query: want IS_IN(G2,{S1}), got %d records", o.nrec);
        const char *q2[1] = { SX };
        m6_query(tun, G2, 1000, 1, q2);
        m6_collect(tun, &o, 1500);
        M6_WANT(o.nrec == 0, "group+source query for an unforwarded source answered with %d records", o.nrec);
        /* a query for a group we are not in: silence */
        m6_query(tun, "ff15::78:99", 1000, 0, NULL);
        m6_collect(tun, &o, 1200);
        M6_WANT(o.nrec == 0 && o.nv1 == 0, "query for a foreign group answered");
        /* an MLDv1 query puts the interface in v1 compatibility: v1 reports to the group, no v2 */
        m6_query(tun, NULL, 1000, -1, NULL);
        m6_collect(tun, &o, 1800);
        M6_WANT(!o.bad[0], "%s", o.bad);
        M6_WANT(m6_v1count(&o, 131, G1) == 1 && m6_v1count(&o, 131, G2) == 1, "v1 query: want one MLDv1 report for G1 and G2, got %d messages (G1 %d, G2 %d)", o.nv1, m6_v1count(&o, 131, G1), m6_v1count(&o, 131, G2));
        M6_WANT(o.nrec == 0, "v1 query: %d v2 records in v1 mode", o.nrec);
        /* in v1 mode a leave is an MLDv1 Done to ff02::2 */
        close(s1);
        m6_collect(tun, &o, M6_DRAIN + 400);
        M6_WANT(!o.bad[0], "%s", o.bad);
        M6_WANT(m6_v1count(&o, 132, G1) >= 1 && o.nrec == 0, "v1 mode leave: want MLDv1 Done(G1), got %d v1 messages, %d v2 records", o.nv1, o.nrec);
        close(s2);
    } while (0);
    snprintf(why_out, 200, "%s", why);
}

static void mld2_run(const char *N, void (*child_body)(int, char *)) {
    if (skipped(N)) return;
    int p[2]; if (pipe(p)) { fail(N, "pipe"); return; }
    pid_t c = fork();
    if (c == 0) {
        char why[220] = "";
        int tun = i3_tun(why, sizeof why);
        if (tun < 0) { write(p[1], why, strlen(why) + 1); _exit(0); }
        child_body(tun, why);
        write(p[1], why, strlen(why) + 1);
        _exit(0);
    }
    char why[220] = ""; ssize_t n = read(p[0], why, sizeof why - 1); waitpid(c, NULL, 0);
    if (n <= 0) fail(N, "child died");
    else if (why[0]) fail(N, "%s", why);
    else pass(N);
}

static void mld2_reports(void) { mld2_run("mld2_reports", m6_reports_child); }
static void mld2_query(void) { mld2_run("mld2_query", m6_query_child); }

/* /proc/net/igmp6 lists the solicited-node group of the NIC's link-local
 * address, in Linux's line format (index, device, 32 hex digits, users,
 * flags, timer). */
static void mld2_proc(void) {
    const char *N = "mld2_proc";
    if (skipped(N)) return;
    char me[64]; int ifx = 0;
    if (!nic6(me, sizeof me, &ifx)) { skip(N, "no NIC link-local address"); return; }
    struct in6_addr a; inet_pton(AF_INET6, me, &a);
    char want[40]; snprintf(want, sizeof want, "ff0200000000000000000001ff%02x%02x%02x", a.s6_addr[13], a.s6_addr[14], a.s6_addr[15]);
    char b[8192]; FILE *f = fopen("/proc/net/igmp6", "r");
    if (!f) { fail(N, "no /proc/net/igmp6"); return; }
    int lines = 0, bad = 0, found = 0, idx, users, tm; char dev[32], g[40]; unsigned fl;
    while (fgets(b, sizeof b, f)) {
        lines++;
        if (sscanf(b, "%d %31s %39s %d %x %d", &idx, dev, g, &users, &fl, &tm) != 6 || strlen(g) != 32) { bad++; continue; }
        if (idx == ifx && !strcmp(g, want)) found++;
    }
    fclose(f);
    if (bad) fail(N, "%d of %d /proc/net/igmp6 lines are not in Linux's format", bad, lines);
    else if (found != 1) fail(N, "solicited-node group %s of %s not listed once (%d)", want, me, found);
    else pass(N);
}

/* A socket that joins a group on lo, with IPV6_MULTICAST_LOOP on (the
 * default), hears its own multicast; with the option off it does not. */
static void mcast6_lo_loop(void) {
    const char *N = "mcast6_lo_loop";
    if (skipped(N)) return;
    int lo = if_nametoindex("lo");
    const char *G = "ff12::4a02";
    uint16_t port = 27051;
    int a = v6rx(G, port, lo, 0);
    int s = socket(AF_INET6, SOCK_DGRAM, 0);
    if (a < 0 || s < 0) { fail(N, "setup: %s", strerror(errno)); return; }
    int sif = setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_IF, &lo, sizeof lo);
    struct sockaddr_in6 to = {0}; to.sin6_family = AF_INET6; to.sin6_port = htons(port);
    inet_pton(AF_INET6, G, &to.sin6_addr); to.sin6_scope_id = lo;
    long s1 = sendto(s, "lo1", 3, 0, (struct sockaddr *)&to, sizeof to);
    int r1 = got(a, "lo1", 1000);
    int zero = 0, one = 1;
    setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_LOOP, &zero, sizeof zero);
    sendto(s, "lo2", 3, 0, (struct sockaddr *)&to, sizeof to);
    int r2 = got(a, "lo2", 400);
    setsockopt(s, IPPROTO_IPV6, IPV6_MULTICAST_LOOP, &one, sizeof one);
    sendto(s, "lo3", 3, 0, (struct sockaddr *)&to, sizeof to);
    int r3 = got(a, "lo3", 1000);
    /* a group nobody joined on lo is not delivered */
    inet_pton(AF_INET6, "ff12::4a03", &to.sin6_addr);
    sendto(s, "lo4", 3, 0, (struct sockaddr *)&to, sizeof to);
    int r4 = got(a, "lo4", 300);
    close(a); close(s);
    if (sif) fail(N, "IPV6_MULTICAST_IF lo: %s", strerror(errno));
    else if (s1 != 3) fail(N, "sendto the group on lo: %ld %s", s1, strerror(errno));
    else if (r1 != 1) fail(N, "member did not hear its own multicast on lo: %d", r1);
    else if (r2 != 0) fail(N, "IPV6_MULTICAST_LOOP=0 still delivered on lo");
    else if (r3 != 1) fail(N, "loop back on: heard %d, want 1", r3);
    else if (r4 != 0) fail(N, "a group not joined on lo was delivered");
    else pass(N);
}

static void mld2_cases(void) {
    mld2_proc();
    mld2_reports();
    mld2_query();
    mcast6_lo_loop();
}
