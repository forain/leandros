/*
 * netcheck — end-to-end "can this machine reach the Internet" check, for the
 * boot-time network self-test (net.test=1 -> init -> /bin/net-autotest) and
 * for running by hand. Plain C, static musl (zig cc), POSIX sockets only.
 *
 *   netcheck [--link-wait S] [--dhcp-wait S] [--no-download] [--quick]
 *   netcheck --nic        just print /proc/net/nic
 *   netcheck --get http://host[:port]/path [--par N] [--secs S] [--mb M]
 *                         throughput of N parallel GETs, per-second rate
 *
 * Steps, each ending in one "NETCHECK <step>: PASS|FAIL|SKIP <detail>" line:
 *   nic       /proc/net/nic: which driver (r8169 / virtio-net), chip, XID, MAC;
 *             also says whether a Realtek RTL8168 exists at all
 *   link      wait up to --link-wait (10 s) for "link: up"; speed/duplex
 *   dhcp      wait up to --dhcp-wait (30 s) for a lease; address/gw/DNS
 *   dns       getaddrinfo www.youtube.com (and two more names), with timing
 *   ping      4 ICMP echoes to the gateway (unprivileged ping socket)
 *   tcp443    TCP connect to www.youtube.com:443 (what Firefox does first)
 *   http      GET http://detectportal.firefox.com/success.txt -> "success"
 *             and GET http://example.com/ -> HTTP 200
 *   download  single-stream HTTP throughput from the first source that
 *             answers (CDN first), per-second rate; then a far server
 *   download4 four parallel streams from that source (both skipped by
 *             --no-download / --quick; the whole download part is < 60 s)
 * then the interface counters, and
 *   NETCHECK summary: <pass> pass, <fail> fail, <skip> skip
 * Exit status: 0 if nothing failed.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/ip_icmp.h>
#include <poll.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static int n_pass, n_fail, n_skip;

static double now_s(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void result(const char *step, const char *verdict, const char *fmt, ...)
{
    char buf[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    if (!strcmp(verdict, "PASS")) n_pass++;
    else if (!strcmp(verdict, "FAIL")) n_fail++;
    else n_skip++;
    printf("NETCHECK %s: %s %s\n", step, verdict, buf);
    fflush(stdout);
}

/* ---- /proc/net/nic ---- */

static char nic[8192];

static int read_file(const char *path, char *buf, size_t cap)
{
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    size_t n = 0;
    for (;;) {
        ssize_t r = read(fd, buf + n, cap - 1 - n);
        if (r <= 0) break;
        n += (size_t)r;
        if (n >= cap - 1) break;
    }
    close(fd);
    buf[n] = 0;
    return (int)n;
}

static void nic_refresh(void)
{
    if (read_file("/proc/net/nic", nic, sizeof nic) < 0) nic[0] = 0;
}

/* Value of "key: value" in the last /proc/net/nic snapshot ("" if absent). */
static const char *nic_get(const char *key, char *out, size_t cap)
{
    size_t kl = strlen(key);
    const char *p = nic;
    out[0] = 0;
    while (*p) {
        const char *e = strchr(p, '\n');
        size_t ll = e ? (size_t)(e - p) : strlen(p);
        if (ll > kl + 1 && !strncmp(p, key, kl) && p[kl] == ':') {
            const char *v = p + kl + 1;
            while (*v == ' ') v++;
            size_t vl = ll - (size_t)(v - p);
            if (vl >= cap) vl = cap - 1;
            memcpy(out, v, vl);
            out[vl] = 0;
            return out;
        }
        if (!e) break;
        p = e + 1;
    }
    return out;
}

static void dump_nic(const char *prefix)
{
    const char *p = nic;
    while (*p) {
        const char *e = strchr(p, '\n');
        int ll = e ? (int)(e - p) : (int)strlen(p);
        printf("%s%.*s\n", prefix, ll, p);
        if (!e) break;
        p = e + 1;
    }
    fflush(stdout);
}

/* ---- sockets ---- */

static int connect_timeout(const struct sockaddr *sa, socklen_t sl, int timeout_ms, int *err)
{
    int s = socket(sa->sa_family, SOCK_STREAM, 0);
    if (s < 0) { *err = errno; return -1; }
    int fl = fcntl(s, F_GETFL, 0);
    fcntl(s, F_SETFL, fl | O_NONBLOCK);
    int r = connect(s, sa, sl);
    if (r < 0 && errno != EINPROGRESS) { *err = errno; close(s); return -1; }
    if (r < 0) {
        struct pollfd pfd = { .fd = s, .events = POLLOUT };
        r = poll(&pfd, 1, timeout_ms);
        if (r <= 0) { *err = r == 0 ? ETIMEDOUT : errno; close(s); return -1; }
        int so = 0;
        socklen_t ol = sizeof so;
        getsockopt(s, SOL_SOCKET, SO_ERROR, &so, &ol);
        if (so) { *err = so; close(s); return -1; }
    }
    fcntl(s, F_SETFL, fl);
    return s;
}

static int resolve4(const char *host, struct sockaddr_in *out, char *txt, size_t cap, double *secs)
{
    struct addrinfo hints = { .ai_family = AF_INET, .ai_socktype = SOCK_STREAM }, *res = 0;
    double t0 = now_s();
    int r = getaddrinfo(host, NULL, &hints, &res);
    if (secs) *secs = now_s() - t0;
    if (r != 0 || !res) {
        if (txt) snprintf(txt, cap, "%s", gai_strerror(r));
        return -1;
    }
    *out = *(struct sockaddr_in *)res->ai_addr;
    if (txt) {
        size_t n = 0;
        txt[0] = 0;
        for (struct addrinfo *a = res; a && n + 20 < cap; a = a->ai_next) {
            char ip[INET_ADDRSTRLEN];
            inet_ntop(AF_INET, &((struct sockaddr_in *)a->ai_addr)->sin_addr, ip, sizeof ip);
            if (strstr(txt, ip)) continue;
            n += (size_t)snprintf(txt + n, cap - n, "%s%s", n ? " " : "", ip);
        }
    }
    freeaddrinfo(res);
    return 0;
}

/* HTTP/1.0-style GET (Connection: close). Fills status, body bytes, the
 * first bytes of the body, and the transfer time of the body. Stops after
 * max_body bytes or max_secs seconds. Returns 0 if a status line came back. */
struct http_res {
    int status;
    long long body;
    double secs;
    double connect_secs;
    char head[96];
    char err[128];
};

static int http_get(const char *host, const char *path, long long max_body, double max_secs, struct http_res *hr)
{
    memset(hr, 0, sizeof *hr);
    struct sockaddr_in sa;
    char txt[128];
    if (resolve4(host, &sa, txt, sizeof txt, NULL) < 0) {
        snprintf(hr->err, sizeof hr->err, "resolve %s: %s", host, txt);
        return -1;
    }
    sa.sin_port = htons(80);
    int err = 0;
    double t0 = now_s();
    int s = connect_timeout((struct sockaddr *)&sa, sizeof sa, 10000, &err);
    hr->connect_secs = now_s() - t0;
    if (s < 0) {
        snprintf(hr->err, sizeof hr->err, "connect %s:80: %s", host, strerror(err));
        return -1;
    }
    char req[512];
    int rl = snprintf(req, sizeof req,
                      "GET %s HTTP/1.1\r\nHost: %s\r\nUser-Agent: LeandrOS-netcheck/1\r\n"
                      "Accept: */*\r\nConnection: close\r\n\r\n", path, host);
    if (write(s, req, (size_t)rl) != rl) {
        snprintf(hr->err, sizeof hr->err, "send: %s", strerror(errno));
        close(s);
        return -1;
    }
    static char buf[65536];
    char hdr[4096];
    size_t hl = 0;
    int in_body = 0;
    double tb = 0, tstart = now_s();
    for (;;) {
        struct pollfd pfd = { .fd = s, .events = POLLIN };
        int pr = poll(&pfd, 1, 10000);
        if (pr <= 0) { snprintf(hr->err, sizeof hr->err, pr == 0 ? "read timeout (10 s idle)" : "poll: %s", strerror(errno)); break; }
        ssize_t n = read(s, buf, sizeof buf);
        if (n <= 0) { if (n < 0) snprintf(hr->err, sizeof hr->err, "read: %s", strerror(errno)); break; }
        size_t off = 0;
        if (!in_body) {
            size_t take = (size_t)n;
            if (take > sizeof hdr - 1 - hl) take = sizeof hdr - 1 - hl;
            memcpy(hdr + hl, buf, take);
            hl += take;
            hdr[hl] = 0;
            char *eoh = strstr(hdr, "\r\n\r\n");
            if (!eoh) { if (hl >= sizeof hdr - 1) { snprintf(hr->err, sizeof hr->err, "header too long"); break; } continue; }
            size_t hdr_len = (size_t)(eoh + 4 - hdr);
            sscanf(hdr, "HTTP/%*s %d", &hr->status);
            /* bytes of this read that are body */
            off = (size_t)n - (hl - hdr_len);
            in_body = 1;
            tb = now_s();
        }
        size_t bl = (size_t)n - off;
        if (bl && hr->body < (long long)sizeof hr->head - 1) {
            size_t k = sizeof hr->head - 1 - (size_t)hr->body;
            if (k > bl) k = bl;
            memcpy(hr->head + hr->body, buf + off, k);
        }
        hr->body += (long long)bl;
        if (hr->body >= max_body || now_s() - tstart > max_secs) break;
    }
    for (size_t i = 0; i < sizeof hr->head && hr->head[i]; i++)
        if (hr->head[i] < 32 || hr->head[i] > 126) hr->head[i] = '.';
    hr->secs = in_body ? now_s() - tb : 0;
    close(s);
    return hr->status ? 0 : -1;
}

/* ---- throughput: N concurrent HTTP GETs ----
 *
 * One process, one poll() loop over `n` connections (each its own GET, so
 * n = 1 is the single-stream number and n = 4 shows whether one connection's
 * window, not the link, is the limit). Bounded by max_secs of transfer and
 * max_body bytes per connection. With `tag` set, prints the aggregate rate
 * once a second ("<tag>| 3 s: 12.3 Mbit/s ..."). */
struct dl_target { char host[128]; char path[256]; int port; };

static int parse_url(const char *url, struct dl_target *t)
{
    memset(t, 0, sizeof *t);
    t->port = 80;
    if (!strncmp(url, "http://", 7)) url += 7;
    else if (strstr(url, "://")) return -1;
    const char *slash = strchr(url, '/');
    size_t hl = slash ? (size_t)(slash - url) : strlen(url);
    if (hl == 0 || hl >= sizeof t->host) return -1;
    memcpy(t->host, url, hl);
    char *colon = strchr(t->host, ':');
    if (colon) { *colon = 0; t->port = atoi(colon + 1); if (t->port <= 0 || t->port > 65535) return -1; }
    snprintf(t->path, sizeof t->path, "%s", slash ? slash : "/");
    return 0;
}

struct dl_res {
    long long bytes;      /* body bytes, all connections */
    double secs;          /* first body byte .. end */
    double connect_ms;    /* mean TCP connect time (~ one RTT) */
    int conns_ok;         /* connections that answered 200 */
    int status;           /* first connection's status */
    double peak_mbps;     /* best one-second interval */
    char err[160];
};

static int dl_run(const struct dl_target *t, int n, long long max_body, double max_secs, const char *tag, struct dl_res *res)
{
    enum { MAXC = 16 };
    struct { int s, in_body, status, done; size_t hl; long long body; char hdr[2048]; } c[MAXC];
    static char buf[65536];
    if (n > MAXC) n = MAXC;
    memset(res, 0, sizeof *res);
    memset(c, 0, sizeof c);
    struct sockaddr_in sa;
    char txt[128];
    if (resolve4(t->host, &sa, txt, sizeof txt, NULL) < 0) {
        snprintf(res->err, sizeof res->err, "resolve %s: %s", t->host, txt);
        return -1;
    }
    sa.sin_port = htons((unsigned short)t->port);
    int live = 0;
    double csum = 0;
    for (int i = 0; i < n; i++) {
        int err = 0;
        double t0 = now_s();
        c[i].s = connect_timeout((struct sockaddr *)&sa, sizeof sa, 10000, &err);
        if (c[i].s < 0) {
            c[i].done = 1;
            snprintf(res->err, sizeof res->err, "connect %s:%d: %s", t->host, t->port, strerror(err));
            continue;
        }
        csum += (now_s() - t0) * 1000;
        char req[512];
        int rl = snprintf(req, sizeof req,
                          "GET %s HTTP/1.1\r\nHost: %s\r\nUser-Agent: LeandrOS-netcheck/1\r\n"
                          "Accept: */*\r\nConnection: close\r\n\r\n", t->path, t->host);
        if (write(c[i].s, req, (size_t)rl) != rl) {
            snprintf(res->err, sizeof res->err, "send: %s", strerror(errno));
            close(c[i].s);
            c[i].s = -1;
            c[i].done = 1;
            continue;
        }
        fcntl(c[i].s, F_SETFL, fcntl(c[i].s, F_GETFL, 0) | O_NONBLOCK);
        live++;
    }
    if (!live) return -1;
    res->connect_ms = csum / live;
    double tstart = now_s(), tb = 0, tick = 0;
    long long tick_bytes = 0;
    int sec = 0;
    while (live > 0) {
        struct pollfd pfd[MAXC];
        int map[MAXC], np = 0;
        for (int i = 0; i < n; i++)
            if (!c[i].done) { pfd[np].fd = c[i].s; pfd[np].events = POLLIN; pfd[np].revents = 0; map[np++] = i; }
        int pr = poll(pfd, (nfds_t)np, 1000);
        double now = now_s();
        if (pr < 0) { snprintf(res->err, sizeof res->err, "poll: %s", strerror(errno)); break; }
        if (pr == 0 && now - tstart > 10 && !tb) { snprintf(res->err, sizeof res->err, "no data for 10 s"); break; }
        for (int k = 0; k < np && pr > 0; k++) {
            if (!pfd[k].revents) continue;
            int i = map[k];
            for (int rounds = 0; rounds < 8; rounds++) {
                ssize_t r = read(c[i].s, buf, sizeof buf);
                if (r < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) break;
                if (r <= 0) {
                    if (r < 0) snprintf(res->err, sizeof res->err, "read: %s", strerror(errno));
                    c[i].done = 1;
                    live--;
                    break;
                }
                size_t off = 0;
                if (!c[i].in_body) {
                    size_t take = (size_t)r;
                    if (take > sizeof c[i].hdr - 1 - c[i].hl) take = sizeof c[i].hdr - 1 - c[i].hl;
                    memcpy(c[i].hdr + c[i].hl, buf, take);
                    c[i].hl += take;
                    c[i].hdr[c[i].hl] = 0;
                    char *eoh = strstr(c[i].hdr, "\r\n\r\n");
                    if (!eoh) {
                        if (c[i].hl >= sizeof c[i].hdr - 1) { snprintf(res->err, sizeof res->err, "header too long"); c[i].done = 1; live--; break; }
                        continue;
                    }
                    size_t hdr_len = (size_t)(eoh + 4 - c[i].hdr);
                    sscanf(c[i].hdr, "HTTP/%*s %d", &c[i].status);
                    if (c[i].status == 200) res->conns_ok++;
                    if (!res->status) res->status = c[i].status;
                    off = (size_t)r - (c[i].hl - hdr_len);
                    c[i].in_body = 1;
                    if (!tb) { tb = now; tick = now; }
                }
                long long bl = (long long)((size_t)r - off);
                c[i].body += bl;
                res->bytes += bl;
                tick_bytes += bl;
                if (c[i].body >= max_body) { c[i].done = 1; live--; break; }
            }
        }
        if (tb && now - tick >= 1.0) {
            double mbps = tick_bytes * 8 / (now - tick) / 1e6;
            if (mbps > res->peak_mbps) res->peak_mbps = mbps;
            sec++;
            if (tag) printf("  %s| %2d s: %6.1f Mbit/s (%.1f MB so far)\n", tag, sec, mbps, res->bytes / 1048576.0);
            tick = now;
            tick_bytes = 0;
        }
        if (tb && now - tb > max_secs) break;
    }
    res->secs = tb ? now_s() - tb : 0;
    for (int i = 0; i < n; i++) if (c[i].s >= 0) close(c[i].s);
    return res->status ? 0 : -1;
}

static double mbit(const struct dl_res *r) { return r->secs > 0 ? r->bytes * 8 / r->secs / 1e6 : 0; }

/* ---- ICMP ---- */

static unsigned short csum(const void *p, int len)
{
    const unsigned short *w = p;
    unsigned long sum = 0;
    while (len > 1) { sum += *w++; len -= 2; }
    if (len) sum += *(const unsigned char *)w;
    sum = (sum >> 16) + (sum & 0xffff);
    sum += sum >> 16;
    return (unsigned short)~sum;
}

static int ping(const char *gw, int count, int *replies, double *avg_ms)
{
    *replies = 0;
    *avg_ms = 0;
    int s = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    int raw = 0;
    if (s < 0) { s = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP); raw = 1; }
    if (s < 0) return -errno;
    struct sockaddr_in to = { .sin_family = AF_INET };
    inet_pton(AF_INET, gw, &to.sin_addr);
    double total = 0;
    for (int i = 0; i < count; i++) {
        unsigned char pkt[64];
        memset(pkt, 0, sizeof pkt);
        struct icmphdr *h = (struct icmphdr *)pkt;
        h->type = ICMP_ECHO;
        h->un.echo.id = htons((unsigned short)getpid());
        h->un.echo.sequence = htons((unsigned short)(i + 1));
        for (int k = 8; k < 64; k++) pkt[k] = (unsigned char)k;
        h->checksum = csum(pkt, sizeof pkt);
        double t0 = now_s();
        if (sendto(s, pkt, sizeof pkt, 0, (struct sockaddr *)&to, sizeof to) < 0) continue;
        for (;;) {
            int left = 1000 - (int)((now_s() - t0) * 1000);
            if (left <= 0) break;
            struct pollfd pfd = { .fd = s, .events = POLLIN };
            if (poll(&pfd, 1, left) <= 0) break;
            unsigned char in[256];
            ssize_t n = recv(s, in, sizeof in, 0);
            if (n <= 0) break;
            unsigned char *ic = in;
            if (raw) { int ihl = (in[0] & 0xf) * 4; if (n < ihl + 8) continue; ic = in + ihl; }
            struct icmphdr *rh = (struct icmphdr *)ic;
            if (rh->type == ICMP_ECHOREPLY && ntohs(rh->un.echo.sequence) == i + 1) {
                (*replies)++;
                total += (now_s() - t0) * 1000;
                break;
            }
        }
        usleep(200000);
    }
    close(s);
    if (*replies) *avg_ms = total / *replies;
    return 0;
}

/* ---- main ---- */

int main(int argc, char **argv)
{
    int link_wait = 10, dhcp_wait = 30, download = 1;
    if (argc == 2 && !strcmp(argv[1], "--nic")) {
        nic_refresh();
        dump_nic("  nic| ");
        return nic[0] ? 0 : 1;
    }
    if (argc >= 3 && !strcmp(argv[1], "--get")) {
        /* netcheck --get http://host[:port]/path [--par N] [--secs S] [--mb M] */
        struct dl_target t;
        int par = 1;
        double secs = 15;
        long long mb = 1024;
        if (parse_url(argv[2], &t) < 0) { fprintf(stderr, "netcheck: bad URL %s (http:// only)\n", argv[2]); return 2; }
        for (int i = 3; i + 1 < argc; i += 2) {
            if (!strcmp(argv[i], "--par")) par = atoi(argv[i + 1]);
            else if (!strcmp(argv[i], "--secs")) secs = atof(argv[i + 1]);
            else if (!strcmp(argv[i], "--mb")) mb = atoll(argv[i + 1]);
        }
        if (par < 1) par = 1;
        setvbuf(stdout, NULL, _IOLBF, 0);
        struct dl_res dr;
        int r = dl_run(&t, par, mb << 20, secs, "get", &dr);
        printf("NETCHECK get: %s %s:%d%s x%d: status %d (%d ok), %.1f MB in %.2f s = %.1f Mbit/s (peak %.1f), connect %.0f ms%s%s\n",
               r == 0 && dr.conns_ok ? "PASS" : "FAIL", t.host, t.port, t.path, par, dr.status, dr.conns_ok,
               dr.bytes / 1048576.0, dr.secs, mbit(&dr), dr.peak_mbps, dr.connect_ms, dr.err[0] ? ", " : "", dr.err);
        return r == 0 && dr.conns_ok ? 0 : 1;
    }
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--link-wait") && i + 1 < argc) link_wait = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--dhcp-wait") && i + 1 < argc) dhcp_wait = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--no-download") || !strcmp(argv[i], "--quick")) download = 0;
        else { fprintf(stderr, "usage: netcheck [--link-wait S] [--dhcp-wait S] [--no-download]\n"); return 2; }
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    char v[256], v2[256], v3[256], v4[256];
    double t_start = now_s();

    /* nic */
    nic_refresh();
    if (!nic[0]) {
        result("nic", "FAIL", "/proc/net/nic missing (kernel without the r8169 lane?)");
        printf("NETCHECK summary: %d pass, %d fail, %d skip\n", n_pass, n_fail, n_skip);
        return 1;
    }
    dump_nic("  nic| ");
    nic_get("driver", v, sizeof v);
    nic_get("realtek", v4, sizeof v4);
    if (!strcmp(v, "r8169")) {
        result("nic", "PASS", "r8169: %s XID %s (MAC version %s), MAC %s, irq %s, firmware %s",
               nic_get("chip", v2, sizeof v2), nic_get("xid", v3, sizeof v3), nic_get("mac_version", v4, sizeof v4),
               nic_get("mac", (char[32]){0}, 32), nic_get("irq", (char[96]){0}, 96), nic_get("firmware", (char[160]){0}, 160));
    } else if (!strcmp(v, "virtio-net")) {
        result("nic", "PASS", "virtio-net (QEMU), MAC %s; no RTL8168 in this machine%s%s",
               nic_get("mac", v2, sizeof v2), v4[0] && strcmp(v4, "none") ? " (but found: " : "",
               v4[0] && strcmp(v4, "none") ? v4 : "");
    } else {
        result("nic", "FAIL", "no network device: no RTL8168 (10ec:8168) and no virtio-net");
        printf("NETCHECK summary: %d pass, %d fail, %d skip\n", n_pass, n_fail, n_skip);
        return 1;
    }

    /* link */
    double t0 = now_s();
    for (;;) {
        nic_refresh();
        if (!strcmp(nic_get("link", v, sizeof v), "up")) break;
        if (now_s() - t0 > link_wait) break;
        usleep(250000);
    }
    if (!strcmp(nic_get("link", v, sizeof v), "up")) {
        nic_get("speed", v2, sizeof v2);
        nic_get("duplex", v3, sizeof v3);
        if (!strcmp(nic_get("driver", v4, sizeof v4), "virtio-net"))
            result("link", "PASS", "up (virtio-net has no link state)");
        else
            result("link", "PASS", "up %s Mbps %s duplex, flow control %s (waited %.1f s, %.1f s since netcheck start)",
                   v2, v3, nic_get("flow_control", (char[64]){0}, 64), now_s() - t0, now_s() - t_start);
    } else {
        result("link", "FAIL", "no link after %d s (PHYstatus %s, link_changes %s): cable, switch port, or PHY init",
               link_wait, nic_get("phystatus", v2, sizeof v2), nic_get("link_changes", v3, sizeof v3));
    }

    /* dhcp */
    t0 = now_s();
    for (;;) {
        nic_refresh();
        if (!strcmp(nic_get("dhcp", v, sizeof v), "bound")) break;
        if (now_s() - t0 > dhcp_wait) break;
        usleep(250000);
    }
    char gw[64] = "";
    if (!strcmp(nic_get("dhcp", v, sizeof v), "bound")) {
        nic_get("gateway", gw, sizeof gw);
        result("dhcp", "PASS", "lease %s gateway %s dns %s (waited %.1f s)",
               nic_get("address", v2, sizeof v2), gw, nic_get("dns", v3, sizeof v3), now_s() - t0);
    } else {
        result("dhcp", "FAIL", "no lease after %d s (dhcp: %s; %s; %s)", dhcp_wait, v,
               nic_get("stack_rx", v2, sizeof v2), nic_get("stack_tx", v3, sizeof v3));
    }
    char resolv[1024];
    if (read_file("/etc/resolv.conf", resolv, sizeof resolv) > 0) {
        for (char *l = strtok(resolv, "\n"); l; l = strtok(NULL, "\n"))
            if (!strncmp(l, "nameserver", 10)) printf("  resolv.conf| %s\n", l);
    }

    /* dns */
    struct sockaddr_in sa;
    double secs;
    const char *names[] = { "www.youtube.com", "example.com", "detectportal.firefox.com" };
    int dns_ok = 0;
    for (int i = 0; i < 3; i++) {
        if (resolve4(names[i], &sa, v, sizeof v, &secs) == 0) {
            printf("  dns| %s -> %s (%.0f ms)\n", names[i], v, secs * 1000);
            if (i == 0) dns_ok = 1;
        } else {
            printf("  dns| %s: %s (%.0f ms)\n", names[i], v, secs * 1000);
        }
    }
    if (dns_ok) {
        resolve4("www.youtube.com", &sa, v, sizeof v, &secs);
        result("dns", "PASS", "www.youtube.com -> %s", v);
    } else {
        result("dns", "FAIL", "www.youtube.com did not resolve (see dns| lines; resolv.conf above)");
    }

    /* ping */
    if (gw[0] && strcmp(gw, "-")) {
        int rep;
        double avg;
        int r = ping(gw, 4, &rep, &avg);
        if (r < 0) result("ping", "SKIP", "no ICMP socket (%s)", strerror(-r));
        else if (rep > 0) result("ping", "PASS", "gateway %s: %d/4 replies, avg %.1f ms", gw, rep, avg);
        else result("ping", "FAIL", "gateway %s: 0/4 replies (some routers drop ICMP; see tcp/http)", gw);
    } else {
        result("ping", "SKIP", "no gateway");
    }

    /* tcp 443: up to 3 of the addresses, 6 s each (one unreachable front
     * end must not fail the step) */
    if (dns_ok) {
        struct addrinfo hints = { .ai_family = AF_INET, .ai_socktype = SOCK_STREAM }, *res = 0;
        int ok = 0, tries = 0, err = 0;
        char tried[256] = "";
        if (getaddrinfo("www.youtube.com", "443", &hints, &res) == 0) {
            for (struct addrinfo *a = res; a && tries < 3 && !ok; a = a->ai_next, tries++) {
                t0 = now_s();
                int s = connect_timeout(a->ai_addr, a->ai_addrlen, 6000, &err);
                inet_ntop(AF_INET, &((struct sockaddr_in *)a->ai_addr)->sin_addr, v, sizeof v);
                if (s >= 0) {
                    result("tcp443", "PASS", "connected to www.youtube.com:443 (%s) in %.0f ms%s%s",
                           v, (now_s() - t0) * 1000, tried[0] ? " after failing " : "", tried);
                    close(s);
                    ok = 1;
                } else {
                    size_t tl = strlen(tried);
                    snprintf(tried + tl, sizeof tried - tl, "%s%s (%s)", tl ? ", " : "", v, strerror(err));
                }
            }
            freeaddrinfo(res);
        }
        if (!ok) result("tcp443", "FAIL", "connect www.youtube.com:443 failed: %s", tried[0] ? tried : "no address");
    } else {
        result("tcp443", "SKIP", "no DNS");
    }

    /* http */
    struct http_res hr;
    int r1 = http_get("detectportal.firefox.com", "/success.txt", 4096, 15, &hr);
    int ok1 = r1 == 0 && hr.status == 200 && !strncmp(hr.head, "success", 7);
    printf("  http| detectportal.firefox.com/success.txt: status %d, %lld bytes, body '%s'%s%s\n",
           hr.status, hr.body, hr.head, hr.err[0] ? ", " : "", hr.err);
    int r2 = http_get("example.com", "/", 1 << 20, 15, &hr);
    int ok2 = r2 == 0 && hr.status == 200;
    printf("  http| example.com/: status %d, %lld bytes (connect %.0f ms)%s%s\n",
           hr.status, hr.body, hr.connect_secs * 1000, hr.err[0] ? ", " : "", hr.err);
    if (ok1 || ok2)
        result("http", "PASS", "%s%s%s", ok1 ? "detectportal 'success'" : "", ok1 && ok2 ? ", " : "", ok2 ? "example.com 200" : "");
    else
        result("http", "FAIL", "no HTTP answer (see http| lines)");

    /* download: single stream from the first source that answers (a CDN
     * first: cachefly is anycast, so it is near wherever the PC is), then a
     * far server (one window per round trip shows up there), then four
     * parallel streams. Each part is bounded; the whole step stays < 60 s. */
    if (!download) {
        result("download", "SKIP", "--no-download");
    } else {
        static const char *mirrors[][2] = {
            { "cachefly.cachefly.net", "/100mb.test" },
            { "speedtest.tele2.net", "/100MB.zip" },
            { "proof.ovh.net", "/files/100Mb.dat" },
            { "ipv4.download.thinkbroadband.com", "/50MB.zip" },
        };
        int done = -1;
        struct dl_res dr;
        struct dl_target t;
        double t_dl = now_s();
        for (unsigned i = 0; i < sizeof mirrors / sizeof mirrors[0] && done < 0; i++) {
            if (now_s() - t_dl > 30) break;
            snprintf(t.host, sizeof t.host, "%s", mirrors[i][0]);
            snprintf(t.path, sizeof t.path, "%s", mirrors[i][1]);
            t.port = 80;
            printf("  download| single stream http://%s%s (12 s, <= 50 MB)\n", t.host, t.path);
            int r = dl_run(&t, 1, 50LL << 20, 12, "download", &dr);
            printf("  download| %s: status %d, %.1f MB in %.2f s = %.1f Mbit/s (peak %.1f), connect %.0f ms%s%s\n",
                   t.host, dr.status, dr.bytes / 1048576.0, dr.secs, mbit(&dr), dr.peak_mbps, dr.connect_ms,
                   dr.err[0] ? ", " : "", dr.err);
            if (r == 0 && dr.status == 200 && dr.bytes >= (1 << 20)) {
                result("download", "PASS", "%.1f MB from %s in %.2f s = %.1f Mbit/s (%.2f MB/s, peak %.1f Mbit/s, rtt ~%.0f ms)%s",
                       dr.bytes / 1048576.0, t.host, dr.secs, mbit(&dr), mbit(&dr) / 8, dr.peak_mbps, dr.connect_ms,
                       mbit(&dr) < 10 ? " [below 10 Mbit/s]" : "");
                done = (int)i;
            }
        }
        if (done < 0) {
            result("download", "FAIL", "no mirror delivered >= 1 MB (see download| lines)");
        } else {
            /* far server, single stream (skipped if it was the one above) */
            if (strcmp(mirrors[done][0], "speedtest.tele2.net")) {
                snprintf(t.host, sizeof t.host, "speedtest.tele2.net");
                snprintf(t.path, sizeof t.path, "/100MB.zip");
                t.port = 80;
                int r = dl_run(&t, 1, 50LL << 20, 8, "download-far", &dr);
                printf("  download| far %s: status %d, %.1f MB in %.2f s = %.1f Mbit/s (peak %.1f), connect %.0f ms%s%s\n",
                       t.host, dr.status, dr.bytes / 1048576.0, dr.secs, r == 0 ? mbit(&dr) : 0.0, dr.peak_mbps,
                       dr.connect_ms, dr.err[0] ? ", " : "", dr.err);
            }
            /* four parallel streams from the source that worked */
            snprintf(t.host, sizeof t.host, "%s", mirrors[done][0]);
            snprintf(t.path, sizeof t.path, "%s", mirrors[done][1]);
            t.port = 80;
            int r = dl_run(&t, 4, 50LL << 20, 10, "download4", &dr);
            if (r == 0 && dr.conns_ok > 0 && dr.bytes >= (1 << 20))
                result("download4", "PASS", "4 streams (%d answered) from %s: %.1f MB in %.2f s = %.1f Mbit/s (peak %.1f)%s",
                       dr.conns_ok, t.host, dr.bytes / 1048576.0, dr.secs, mbit(&dr), dr.peak_mbps,
                       mbit(&dr) < 10 ? " [below 10 Mbit/s]" : "");
            else
                result("download4", "FAIL", "4 streams from %s: status %d, %.1f MB%s%s", t.host, dr.status,
                       dr.bytes / 1048576.0, dr.err[0] ? ", " : "", dr.err);
        }
    }

    /* counters */
    nic_refresh();
    printf("  counters| rx_packets %s rx_bytes %s rx_errors %s | tx_packets %s tx_bytes %s tx_ring_full %s\n",
           nic_get("rx_packets", v, sizeof v), nic_get("rx_bytes", v2, sizeof v2), nic_get("rx_errors", v3, sizeof v3),
           nic_get("tx_packets", v4, sizeof v4), nic_get("tx_bytes", (char[32]){0}, 32), nic_get("tx_ring_full", (char[32]){0}, 32));
    printf("  counters| irq_count %s\n", nic_get("irq_count", v, sizeof v));
    printf("  counters| %s\n", nic_get("hw_tally", v, sizeof v));
    printf("  counters| stack_rx %s | stack_tx %s\n", nic_get("stack_rx", v2, sizeof v2), nic_get("stack_tx", v3, sizeof v3));
    char dev[2048];
    if (read_file("/proc/net/dev", dev, sizeof dev) > 0) {
        for (char *l = strtok(dev, "\n"); l; l = strtok(NULL, "\n"))
            if (strstr(l, "eth0:")) printf("  proc_net_dev| %s\n", l);
    }

    printf("NETCHECK summary: %d pass, %d fail, %d skip (%.1f s)\n", n_pass, n_fail, n_skip, now_s() - t_start);
    return n_fail ? 1 : 0;
}
