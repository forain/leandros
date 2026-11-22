/* Lane igmpv3: IGMPv3 membership reports (RFC 3376), read off an IFF_TUN wire
 * in a fresh netns. One group record per (group, interface) from the union of
 * the per-socket source filters: CHANGE_TO_EXCLUDE/INCLUDE, ALLOW_NEW,
 * BLOCK_OLD for join / leave / filter changes (sent twice: robustness 2),
 * MODE_IS_* for general, group and group-and-source queries, and no v2 report
 * from smoltcp while the interface speaks v3. Included from udptest.c after
 * pmtud2.c (uses its includes and CLONE_NEWNET). */

struct i3rec { int ty; uint8_t group[4]; int n; uint8_t src[4][4]; };
struct i3seen { int nrep, nrec, v2; struct i3rec r[24]; char bad[120]; };

static uint16_t i3_csum(const uint8_t *b, int len) {
    uint32_t s = 0; int i;
    for (i = 0; i + 1 < len; i += 2) s += (b[i] << 8) | b[i + 1];
    if (i < len) s += b[i] << 8;
    while (s >> 16) s = (s & 0xffff) + (s >> 16);
    return (uint16_t)~s;
}

/* Read the wire for `ms`, parsing every IGMP packet into `o`. */
static void i3_collect(int tun, struct i3seen *o, int ms) {
    static uint8_t pk[2048];
    memset(o, 0, sizeof *o);
    for (int t = 0; t < ms; t += 20) {
        ssize_t n;
        while ((n = read(tun, pk, sizeof pk)) > 0) {
            if ((pk[0] >> 4) != 4 || pk[9] != 2) continue;
            int ihl = (pk[0] & 15) * 4;
            if (n < ihl + 8) continue;
            uint8_t *m = pk + ihl; int ilen = n - ihl;
            if (m[0] == 0x16) { o->v2++; continue; }
            if (m[0] != 0x22) continue;
            if (ihl != 24 || pk[20] != 0x94 || pk[21] != 4 || pk[8] != 1 || pk[1] != 0xc0 ||
                memcmp(pk + 16, "\xe0\x00\x00\x16", 4) || i3_csum(pk, ihl) || i3_csum(m, ilen)) {
                snprintf(o->bad, sizeof o->bad, "malformed v3 report: ihl %d ttl %d tos %02x dst %d.%d.%d.%d", ihl, pk[8], pk[1], pk[16], pk[17], pk[18], pk[19]);
                continue;
            }
            int nr = (m[6] << 8) | m[7], off = 8;
            o->nrep++;
            for (int i = 0; i < nr && off + 8 <= ilen && o->nrec < 24; i++) {
                struct i3rec *r = &o->r[o->nrec++];
                r->ty = m[off]; r->n = (m[off + 2] << 8) | m[off + 3]; memcpy(r->group, m + off + 4, 4);
                for (int k = 0; k < r->n && k < 4; k++) memcpy(r->src[k], m + off + 8 + 4 * k, 4);
                off += 8 + 4 * r->n;
            }
        }
        msleep(20);
    }
}

static int i3_count(const struct i3seen *o, int ty, const char *group, int nsrc, const char *src0) {
    uint8_t g[4], s[4]; inet_pton(AF_INET, group, g); if (src0) inet_pton(AF_INET, src0, s);
    int c = 0;
    for (int i = 0; i < o->nrec; i++) {
        const struct i3rec *r = &o->r[i];
        if (r->ty != ty || memcmp(r->group, g, 4) || r->n != nsrc) continue;
        if (src0 && memcmp(r->src[0], s, 4)) continue;
        c++;
    }
    return c;
}

/* Any record of `group` at all (to prove one is absent). */
static int i3_any(const struct i3seen *o, const char *group) {
    uint8_t g[4]; inet_pton(AF_INET, group, g);
    int c = 0;
    for (int i = 0; i < o->nrec; i++) if (!memcmp(o->r[i].group, g, 4)) c++;
    return c;
}

/* A membership query into the tun: version 3 when nsrc >= 0 (or 8-byte v2
 * when nsrc < 0), to 224.0.0.1 (general) or to the group. */
static void i3_query(int tun, const char *group, int maxresp, int nsrc, const char **srcs) {
    uint8_t p[96]; memset(p, 0, sizeof p);
    int len = nsrc < 0 ? 8 : 12 + 4 * nsrc;
    int g0 = group == NULL;
    p[0] = 0x46; p[1] = 0xc0; p[8] = 1; p[9] = 2;
    inet_pton(AF_INET, "10.78.0.2", p + 12);
    if (g0) { p[16] = 224; p[19] = 1; } else inet_pton(AF_INET, group, p + 16);
    p[20] = 0x94; p[21] = 4;
    int tot = 24 + len; p[2] = tot >> 8; p[3] = tot & 255; p[6] = 0x40;
    uint16_t c = i3_csum(p, 24); p[10] = c >> 8; p[11] = c & 255;
    uint8_t *m = p + 24;
    m[0] = 0x11; m[1] = maxresp;
    if (!g0) inet_pton(AF_INET, group, m + 4);
    if (nsrc >= 0) {
        m[8] = 2; m[9] = 125; m[10] = nsrc >> 8; m[11] = nsrc & 255;
        for (int i = 0; i < nsrc; i++) inet_pton(AF_INET, srcs[i], m + 12 + 4 * i);
    }
    c = i3_csum(m, len); m[2] = c >> 8; m[3] = c & 255;
    if (write(tun, p, tot) != tot) printf("# igmp3: tun write failed: %s\n", strerror(errno));
}

static int i3_tun(char *why, size_t wl) {
    if (unshare(CLONE_NEWNET)) { snprintf(why, wl, "SKIP unshare: %s", strerror(errno)); return -1; }
    int tun = open("/dev/net/tun", O_RDWR | O_NONBLOCK);
    struct ifreq ifr; memset(&ifr, 0, sizeof ifr);
    strcpy(ifr.ifr_name, "igm%d"); ifr.ifr_flags = IFF_TUN | IFF_NO_PI;
    if (tun < 0 || ioctl(tun, 0x400454ca, &ifr) < 0) { snprintf(why, wl, "SKIP tun: %s", strerror(errno)); return -1; }
    int u = socket(AF_INET, SOCK_DGRAM, 0); struct sockaddr_in *sa = (struct sockaddr_in *)&ifr.ifr_addr;
    sa->sin_family = AF_INET; inet_pton(AF_INET, "10.78.0.1", &sa->sin_addr); ioctl(u, SIOCSIFADDR, &ifr);
    inet_pton(AF_INET, "255.255.255.0", &sa->sin_addr); ioctl(u, SIOCSIFNETMASK, &ifr);
    ioctl(u, SIOCGIFFLAGS, &ifr); ifr.ifr_flags |= IFF_UP; ioctl(u, SIOCSIFFLAGS, &ifr); close(u);
    return tun;
}

static int i3_join(int ifx, const char *g) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct ip_mreqn m; memset(&m, 0, sizeof m); inet_pton(AF_INET, g, &m.imr_multiaddr); m.imr_ifindex = ifx;
    if (s < 0 || setsockopt(s, IPPROTO_IP, IP_ADD_MEMBERSHIP, &m, sizeof m)) { if (s >= 0) close(s); return -1; }
    return s;
}

static int i3_src(int s, int opt, int ifx, const char *g, const char *src) {
    struct ip_mreq_source m; memset(&m, 0, sizeof m);
    inet_pton(AF_INET, g, &m.imr_multiaddr); inet_pton(AF_INET, src, &m.imr_sourceaddr);
    (void)ifx;
    /* the interface comes from the address of the tun */
    inet_pton(AF_INET, "10.78.0.1", &m.imr_interface);
    return setsockopt(s, IPPROTO_IP, opt, &m, sizeof m);
}

#define I3_WANT(cond, ...) do { if (!(cond)) { snprintf(why, sizeof why, __VA_ARGS__); return; } } while (0)
#define I3_DRAIN 1300

static void i3_reports_child(int tun, char *why_out) {
    char why[200] = ""; struct i3seen o;
    const char *G1 = "239.78.1.1", *G2 = "239.78.1.2", *G3 = "239.78.1.3", *G4 = "239.78.1.4";
    const char *S7 = "10.78.0.7", *S8 = "10.78.0.8", *S9 = "10.78.0.9";
    int ifx = if_nametoindex("igm0");
    if (!ifx) ifx = 1 + 0; /* tun names count up per netns: igm0 */
    do {
        /* any-source join: CHANGE_TO_EXCLUDE({}) sent twice, no v2 report */
        int s1 = i3_join(ifx, G1);
        I3_WANT(s1 >= 0, "join G1: %s", strerror(errno));
        i3_collect(tun, &o, I3_DRAIN + 500);
        I3_WANT(!o.bad[0], "%s", o.bad);
        I3_WANT(i3_count(&o, 4, G1, 0, NULL) == 2, "join: TO_EX({}) x%d (want 2), reports %d", i3_count(&o, 4, G1, 0, NULL), o.nrep);
        I3_WANT(o.v2 == 0, "join: %d IGMPv2 reports on a v3 interface", o.v2);
        /* include join: ALLOW_NEW(S9) */
        int s2 = i3_join(ifx, G2); /* any-source first, then narrow to include */
        I3_WANT(s2 >= 0, "join G2");
        i3_collect(tun, &o, I3_DRAIN + 400);
        close(s2);
        i3_collect(tun, &o, I3_DRAIN + 400);
        s2 = socket(AF_INET, SOCK_DGRAM, 0);
        I3_WANT(i3_src(s2, IP_ADD_SOURCE_MEMBERSHIP, ifx, G2, S9) == 0, "ADD_SOURCE: %s", strerror(errno));
        i3_collect(tun, &o, I3_DRAIN);
        I3_WANT(i3_count(&o, 5, G2, 1, S9) >= 1, "ADD_SOURCE: no ALLOW_NEW(G2,{S9}) (records %d)", o.nrec);
        /* block a source on the any-source group: BLOCK_OLD(S8) */
        i3_collect(tun, &o, 0);
        I3_WANT(i3_src(s1, IP_BLOCK_SOURCE, ifx, G1, S8) == 0, "BLOCK_SOURCE: %s", strerror(errno));
        i3_collect(tun, &o, I3_DRAIN);
        I3_WANT(i3_count(&o, 6, G1, 1, S8) >= 1, "BLOCK_SOURCE: no BLOCK_OLD(G1,{S8}) (records %d)", o.nrec);
        /* unblock: ALLOW_NEW(S8) */
        I3_WANT(i3_src(s1, IP_UNBLOCK_SOURCE, ifx, G1, S8) == 0, "UNBLOCK_SOURCE: %s", strerror(errno));
        i3_collect(tun, &o, I3_DRAIN);
        I3_WANT(i3_count(&o, 5, G1, 1, S8) >= 1, "UNBLOCK_SOURCE: no ALLOW_NEW(G1,{S8}) (records %d)", o.nrec);
        /* IP_MSFILTER to INCLUDE({S7}): exclude -> include = CHANGE_TO_INCLUDE({S7}) */
        {
            unsigned char b[32]; memset(b, 0, sizeof b);
            inet_pton(AF_INET, G1, b); inet_pton(AF_INET, "10.78.0.1", b + 4);
            unsigned fmode = 1; int n = 1; memcpy(b + 8, &fmode, 4); memcpy(b + 12, &n, 4); inet_pton(AF_INET, S7, b + 16);
            I3_WANT(setsockopt(s1, IPPROTO_IP, 41, b, 20) == 0, "IP_MSFILTER: %s", strerror(errno));
        }
        i3_collect(tun, &o, I3_DRAIN);
        I3_WANT(i3_count(&o, 3, G1, 1, S7) >= 1, "MSFILTER include: no TO_IN(G1,{S7}) (records %d)", o.nrec);
        /* leave: BLOCK_OLD of the sources it still had */
        close(s1);
        i3_collect(tun, &o, I3_DRAIN);
        I3_WANT(i3_count(&o, 6, G1, 1, S7) >= 1, "leave include group: no BLOCK_OLD(G1,{S7}) (records %d)", o.nrec);
        close(s2);
        i3_collect(tun, &o, I3_DRAIN);
        I3_WANT(i3_count(&o, 6, G2, 1, S9) >= 1, "leave: no BLOCK_OLD(G2,{S9}) (records %d)", o.nrec);
        /* any-source leave: CHANGE_TO_INCLUDE({}) */
        int s3 = i3_join(ifx, G3);
        I3_WANT(s3 >= 0, "join G3");
        i3_collect(tun, &o, I3_DRAIN + 400);
        close(s3);
        i3_collect(tun, &o, I3_DRAIN);
        I3_WANT(i3_count(&o, 3, G3, 0, NULL) >= 1, "any-source leave: no TO_IN({}) (records %d)", o.nrec);
        /* union of two sockets: include {S7} then include {S8} reports only the new source,
         * an any-source member then turns the group to EXCLUDE({}) */
        int a = socket(AF_INET, SOCK_DGRAM, 0), b = socket(AF_INET, SOCK_DGRAM, 0);
        I3_WANT(i3_src(a, IP_ADD_SOURCE_MEMBERSHIP, ifx, G4, S7) == 0, "G4 add S7");
        i3_collect(tun, &o, I3_DRAIN + 400);
        I3_WANT(i3_src(b, IP_ADD_SOURCE_MEMBERSHIP, ifx, G4, S8) == 0, "G4 add S8");
        i3_collect(tun, &o, I3_DRAIN);
        I3_WANT(i3_count(&o, 5, G4, 1, S8) >= 1 && i3_count(&o, 5, G4, 1, S7) == 0, "union: want ALLOW_NEW({S8}) only (S8 %d, S7 %d)", i3_count(&o, 5, G4, 1, S8), i3_count(&o, 5, G4, 1, S7));
        int c = i3_join(ifx, G4);
        I3_WANT(c >= 0, "G4 any-source join");
        i3_collect(tun, &o, I3_DRAIN);
        I3_WANT(i3_count(&o, 4, G4, 0, NULL) >= 1, "union: any-source member did not make TO_EX({}) (records %d)", o.nrec);
        close(a); close(b); close(c);
    } while (0);
    snprintf(why_out, 200, "%s", why);
}

static void i3_query_child(int tun, char *why_out) {
    char why[200] = ""; struct i3seen o;
    const char *G1 = "239.78.2.1", *G2 = "239.78.2.2", *S1 = "10.78.0.5", *SX = "10.78.0.6";
    int ifx = if_nametoindex("igm0");
    do {
        int s1 = i3_join(ifx, G1);
        I3_WANT(s1 >= 0, "join G1: %s", strerror(errno));
        int s2 = socket(AF_INET, SOCK_DGRAM, 0);
        I3_WANT(i3_src(s2, IP_ADD_SOURCE_MEMBERSHIP, ifx, G2, S1) == 0, "ADD_SOURCE: %s", strerror(errno));
        i3_collect(tun, &o, 2200);
        I3_WANT(o.v2 == 0, "%d IGMPv2 reports", o.v2);
        /* general query, max resp 1 s */
        i3_query(tun, NULL, 10, 0, NULL);
        i3_collect(tun, &o, 1500);
        I3_WANT(!o.bad[0], "%s", o.bad);
        I3_WANT(i3_count(&o, 2, G1, 0, NULL) == 1 && i3_count(&o, 1, G2, 1, S1) == 1,
                "general query: want IS_EX(G1,{}) and IS_IN(G2,{S1}), got %d records over %d reports", o.nrec, o.nrep);
        I3_WANT(o.v2 == 0, "general v3 query was answered with %d v2 reports", o.v2);
        /* group-specific */
        i3_query(tun, G2, 10, 0, NULL);
        i3_collect(tun, &o, 1500);
        I3_WANT(i3_count(&o, 1, G2, 1, S1) == 1 && i3_any(&o, G1) == 0, "group query: want IS_IN(G2,{S1}) only, got %d records (G1 %d)", o.nrec, i3_any(&o, G1));
        /* group-and-source-specific: only the sources asked about that are forwarded */
        const char *q[2] = { S1, SX };
        i3_query(tun, G2, 10, 2, q);
        i3_collect(tun, &o, 1500);
        I3_WANT(i3_count(&o, 1, G2, 1, S1) == 1, "group+source query: want IS_IN(G2,{S1}), got %d records", o.nrec);
        const char *q2[1] = { SX };
        i3_query(tun, G2, 10, 1, q2);
        i3_collect(tun, &o, 1500);
        I3_WANT(o.nrec == 0, "group+source query for an unforwarded source answered with %d records", o.nrec);
        /* a query for a group we are not in: silence */
        i3_query(tun, "239.78.2.9", 10, 0, NULL);
        i3_collect(tun, &o, 1200);
        I3_WANT(o.nrec == 0 && o.v2 == 0, "query for a foreign group answered");
        /* an IGMPv2 query puts the interface in v2 compatibility: v2 reports, no v3 ones */
        i3_query(tun, NULL, 10, -1, NULL);
        i3_collect(tun, &o, 2500);
        I3_WANT(o.v2 >= 1, "v2 query: no IGMPv2 report");
        I3_WANT(o.nrec == 0, "v2 query: %d v3 records in v2 mode", o.nrec);
        close(s1); close(s2);
    } while (0);
    snprintf(why_out, 200, "%s", why);
}

static void igmp3_run(const char *N, void (*child_body)(int, char *)) {
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
    else if (!strncmp(why, "SKIP", 4)) fail(N, "%s", why);
    else if (why[0]) fail(N, "%s", why);
    else pass(N);
}

static void igmp3_reports(void) { igmp3_run("igmp3_reports", i3_reports_child); }
static void igmp3_query(void) { igmp3_run("igmp3_query", i3_query_child); }

/* /proc/net/igmp shows the version in force: V3 on every interface, lo
 * included (Linux shows lo as V3 although it runs no IGMP there). */
static void igmp3_proc(void) {
    const char *N = "igmp3_proc";
    if (skipped(N)) return;
    char b[2048]; int f = open("/proc/net/igmp", O_RDONLY);
    ssize_t n = f >= 0 ? read(f, b, sizeof b - 1) : -1; if (f >= 0) close(f);
    if (n <= 0) { fail(N, "no /proc/net/igmp"); return; }
    b[n] = 0;
    if (!strstr(b, "V3")) { fail(N, "no interface reports V3: %.80s", b + 60); return; }
    char *lo = strstr(b, "\tlo ");
    if (!lo) { fail(N, "no lo line: %.80s", b); return; }
    char *eol = strchr(lo, '\n'); if (eol) *eol = 0;
    if (!strstr(lo, "V3")) fail(N, "lo is not V3: %s", lo);
    else pass(N);
}

static void igmp3_cases(void) {
    igmp3_proc();
    igmp3_reports();
    igmp3_query();
}
