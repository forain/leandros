/* ── lane udpanytag: a wildcard UDP server parks with a keyed poll mask ──
 *
 * Firefox's QUIC socket is bound to 0.0.0.0:0, which the net server turns
 * into a wildcard server with a socket on every stack. Its poll answer used
 * to carry no tag, so the kernel parked the whole pollfd set with a
 * broadcast mask (all ones) and every AF_UNIX message in the system woke it.
 *
 * udp_any_poll_tag: a thread polls {0.0.0.0:0 UDP socket, a pipe} for
 * POLLIN with a long timeout. While it is parked:
 *   - its /proc/sysstat `w` line poll mask is not all ones;
 *   - AF_UNIX traffic between two other sockets does not wake it (its
 *     `pwakes` column, the last one, barely moves);
 *   - a datagram to 127.0.0.1:<port> (and, with a default route, to the NIC
 *     address: another stack's mirror, checked only when such a self-send
 *     is delivered at all) wakes it promptly with POLLIN.
 * Skipped without /proc/sysstat (a host kernel). */

#include <sys/syscall.h>

struct sysw { int found; char state; unsigned long long mask, pwakes; };

/* The `w` line of task `tid`:
 * `w tid tgid state sc port listen poll_mask in_sc_ns pwakes`. */
static struct sysw sysstat_w(long tid) {
    static char buf[256 * 1024];
    struct sysw r = { 0, 0, 0, 0 };
    int fd = open("/proc/sysstat", O_RDONLY);
    if (fd < 0) return r;
    size_t len = 0;
    for (;;) {
        ssize_t n = read(fd, buf + len, sizeof buf - 1 - len);
        if (n <= 0) break;
        len += (size_t)n;
        if (len >= sizeof buf - 1) break;
    }
    close(fd);
    buf[len] = 0;
    for (char *line = buf; line && *line; ) {
        char *nl = strchr(line, '\n');
        if (nl) *nl = 0;
        long t = 0, tg = 0, sc = 0, lis = 0; unsigned long port = 0;
        char st = 0;
        unsigned long long mask = 0, insc = 0, pw = 0;
        if (line[0] == 'w' && line[1] == ' '
            && sscanf(line, "w %ld %ld %c %ld %lx %ld %llx %llu %llu",
                      &t, &tg, &st, &sc, &port, &lis, &mask, &insc, &pw) == 9
            && t == tid) {
            r.found = 1; r.state = st; r.mask = mask; r.pwakes = pw;
            return r;
        }
        line = nl ? nl + 1 : NULL;
    }
    return r;
}

struct anypoll {
    int udp, prd;
    volatile long tid;
    volatile int done;
    int rc;
    short udp_rev;
    long long woke_ms;
};

static void *anypoll_thread(void *p) {
    struct anypoll *a = p;
    a->tid = syscall(SYS_gettid);
    struct pollfd f[2] = { { a->udp, POLLIN, 0 }, { a->prd, POLLIN, 0 } };
    a->rc = poll(f, 2, 8000);
    a->woke_ms = now_ms();
    a->udp_rev = f[0].revents;
    __atomic_store_n(&a->done, 1, __ATOMIC_SEQ_CST);
    return NULL;
}

/* One round: park the poller, check mask + AF_UNIX quiet, then wake it with
 * a datagram to `to_ip`. Returns 0 on success, else reports and returns 1. */
static int anytag_round(const char *n, int udp, uint16_t port, const char *to_ip, int may_drop) {
    int pp[2], us[2];
    if (pipe(pp) < 0 || socketpair(AF_UNIX, SOCK_STREAM, 0, us) < 0) { fail(n, "pipe/socketpair"); return 1; }
    struct anypoll a = { udp, pp[0], 0, 0, 0, 0, 0 };
    pthread_t th;
    if (pthread_create(&th, NULL, anypoll_thread, &a) != 0) { fail(n, "pthread_create"); return 1; }
    int bad = 0, dropped = 0;
    struct sysw w = { 0, 0, 0, 0 };
    /* Parked: sleeping with a non-zero mask (up to 2 s). */
    for (int i = 0; i < 200; i++) {
        if (a.tid) { w = sysstat_w(a.tid); if (w.found && w.state == 'S' && w.mask) break; }
        msleep(10);
    }
    if (!w.found || w.state != 'S' || !w.mask) {
        fail(n, "poller never parked (found %d state %c mask %llx)", w.found, w.state ? w.state : '?', w.mask);
        bad = 1;
    } else if (w.mask == ~0ULL) {
        fail(n, "parked with the broadcast mask (all ones)");
        bad = 1;
    }
    /* Unrelated AF_UNIX traffic: 300 round trips, each a keyed data wake. */
    unsigned long long p0 = w.pwakes;
    char c = 'x';
    for (int i = 0; i < 300 && !bad; i++) {
        if (write(us[0], &c, 1) != 1 || read(us[1], &c, 1) != 1) { fail(n, "AF_UNIX io"); bad = 1; }
    }
    if (!bad) {
        struct sysw w1 = sysstat_w(a.tid);
        unsigned long long dw = w1.pwakes - p0;
        if (__atomic_load_n(&a.done, __ATOMIC_SEQ_CST)) {
            fail(n, "poll returned on AF_UNIX traffic (rc %d)", a.rc); bad = 1;
        } else if (!w1.found || dw > 10) {
            fail(n, "AF_UNIX traffic woke the poller %llu times", dw); bad = 1;
        }
    }
    /* A datagram wakes it. */
    struct sockaddr_in to = sin4(to_ip, port);
    int tx = socket(AF_INET, SOCK_DGRAM, 0);
    long long t0 = now_ms();
    if (tx < 0 || send_to(tx, "q", 1, &to) != 1) { if (!bad) fail(n, "sendto %s: errno %d", to_ip, errno); bad = 1; }
    for (int i = 0; i < 300 && !__atomic_load_n(&a.done, __ATOMIC_SEQ_CST); i++) msleep(10);
    if (!__atomic_load_n(&a.done, __ATOMIC_SEQ_CST)) {
        int rd = wait_ev(udp, POLLIN, 0);
        if (!bad && may_drop && !rd) {
            /* Never delivered (a send to this host's own NIC address leaves
             * through that stack and is not looped back): no wake to test. */
            printf("%s: note: datagram to %s not delivered, NIC half not checked\n", n, to_ip);
            dropped = 1;
        } else {
            if (!bad) fail(n, "datagram to %s did not wake the poll within 3 s (readable now: %#x)", to_ip, rd);
            bad = 1;
        }
        (void)!write(pp[1], "z", 1); /* release the poller */
    }
    pthread_join(th, NULL);
    if (!bad && !dropped && (a.rc != 1 || !(a.udp_rev & POLLIN))) {
        fail(n, "poll rc %d udp revents %#x", a.rc, a.udp_rev); bad = 1;
    } else if (!bad && !dropped && a.woke_ms - t0 > 1000) {
        fail(n, "woke %lld ms after the datagram", a.woke_ms - t0); bad = 1;
    }
    char b[8];
    while (recv(udp, b, sizeof b, MSG_DONTWAIT) > 0) {}
    if (tx >= 0) close(tx);
    close(pp[0]); close(pp[1]); close(us[0]); close(us[1]);
    return bad;
}

static void udp_any_poll_tag(void) {
    const char *n = "udp_any_poll_tag";
    if (skipped(n)) return;
    /* open, not access(): /proc/sysstat exists only as an open() snapshot. */
    int sfd = open("/proc/sysstat", O_RDONLY);
    if (sfd < 0) { skip(n, "(no /proc/sysstat)"); return; }
    close(sfd);
    struct sockaddr_in me;
    int udp = udp_bound("0.0.0.0", 0, &me);
    if (udp < 0) { fail(n, "bind 0.0.0.0:0"); return; }
    uint16_t port = ntohs(me.sin_port);
    int bad = anytag_round(n, udp, port, "127.0.0.1", 0);
    char ip[64];
    if (!bad && nic_addr(ip, sizeof ip)) bad = anytag_round(n, udp, port, ip, 1);
    close(udp);
    if (!bad) pass(n);
}
