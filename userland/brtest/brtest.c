// brtest: veth pairs, bridges and forwarding in LeandrOS (design-podnet3).
// Lane veth: veth_pair, veth_dellink, veth_nsgone, veth_naming.
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <linux/if_link.h>
#include <net/if.h>
#include <netinet/in.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef IFLA_NET_NS_FD
#define IFLA_NET_NS_FD 28
#endif
#ifndef VETH_INFO_PEER
#define VETH_INFO_PEER 1
#endif

static int npass, nfail;
#define CHECK(name, cond) do { if (cond) { npass++; printf("PASS %s\n", name); } \
    else { nfail++; printf("FAIL %s (errno %d)\n", name, errno); } fflush(stdout); } while (0)

struct nlb { char b[2048]; int len; };
static void nl_begin(struct nlb *m, int type, int flags, const void *body, int blen) {
    memset(m, 0, sizeof *m);
    struct nlmsghdr *h = (struct nlmsghdr *)m->b;
    h->nlmsg_type = type; h->nlmsg_flags = flags | NLM_F_REQUEST; h->nlmsg_seq = 91;
    memcpy(m->b + NLMSG_HDRLEN, body, blen);
    m->len = NLMSG_HDRLEN + NLMSG_ALIGN(blen);
}
static int nl_attr_at(struct nlb *m, int type, const void *d, int dl) {
    int off = m->len;
    struct rtattr *a = (struct rtattr *)(m->b + m->len);
    a->rta_type = type; a->rta_len = RTA_LENGTH(dl);
    if (dl) memcpy(RTA_DATA(a), d, dl);
    m->len += RTA_ALIGN(a->rta_len);
    return off;
}
static void nl_attr(struct nlb *m, int type, const void *d, int dl) { nl_attr_at(m, type, d, dl); }
static int nl_nest(struct nlb *m, int type) { return nl_attr_at(m, type | NLA_F_NESTED, NULL, 0); }
static void nl_end(struct nlb *m, int off) { ((struct rtattr *)(m->b + off))->rta_len = m->len - off; }
static int nl_do(int s, struct nlb *m) {
    ((struct nlmsghdr *)m->b)->nlmsg_len = m->len;
    ((struct nlmsghdr *)m->b)->nlmsg_flags |= NLM_F_ACK;
    if (send(s, m->b, m->len, 0) != m->len) return -1;
    char rb[2048];
    ssize_t n = recv(s, rb, sizeof rb, 0);
    struct nlmsghdr *h = (struct nlmsghdr *)rb;
    if (n < (ssize_t)NLMSG_HDRLEN || h->nlmsg_type != NLMSG_ERROR) return -1;
    return -((struct nlmsgerr *)NLMSG_DATA(h))->error;
}
static int nl_sock(void) { return socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE); }

/* RTM_NEWLINK kind=veth: host end `a` (NULL = veth%d), peer `b` in the netns of `nsfd` (-1 = same). */
static int veth_new(int s, const char *a, const char *b, int nsfd) {
    struct ifinfomsg i; memset(&i, 0, sizeof i);
    struct nlb m; nl_begin(&m, RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, &i, sizeof i);
    if (a) nl_attr(&m, IFLA_IFNAME, a, strlen(a) + 1);
    int li = nl_nest(&m, IFLA_LINKINFO);
    nl_attr(&m, IFLA_INFO_KIND, "veth", 5);
    int di = nl_nest(&m, IFLA_INFO_DATA);
    int pi = nl_nest(&m, VETH_INFO_PEER);
    m.len += sizeof i;                      /* the peer's ifinfomsg (zeroed) */
    if (b) nl_attr(&m, IFLA_IFNAME, b, strlen(b) + 1);
    if (nsfd >= 0) nl_attr(&m, IFLA_NET_NS_FD, &nsfd, 4);
    nl_end(&m, pi); nl_end(&m, di); nl_end(&m, li);
    return nl_do(s, &m);
}
static int link_set_up(int s, int idx) {
    struct ifinfomsg i; memset(&i, 0, sizeof i);
    i.ifi_index = idx; i.ifi_flags = IFF_UP; i.ifi_change = IFF_UP;
    struct nlb m; nl_begin(&m, RTM_NEWLINK, 0, &i, sizeof i);
    return nl_do(s, &m);
}
static int link_del(int s, int idx) {
    struct ifinfomsg i; memset(&i, 0, sizeof i); i.ifi_index = idx;
    struct nlb m; nl_begin(&m, RTM_DELLINK, 0, &i, sizeof i);
    return nl_do(s, &m);
}
static int addr_add(int s, int idx, const char *ip, int plen) {
    struct ifaddrmsg a; memset(&a, 0, sizeof a);
    a.ifa_family = AF_INET; a.ifa_prefixlen = plen; a.ifa_index = idx;
    struct in_addr in; inet_pton(AF_INET, ip, &in);
    struct nlb m; nl_begin(&m, RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, &a, sizeof a);
    nl_attr(&m, IFA_LOCAL, &in, 4); nl_attr(&m, IFA_ADDRESS, &in, 4);
    return nl_do(s, &m);
}

/* ifindex of link `name` via a RTM_GETLINK by name; 0 if absent. *peer = IFLA_LINK, *kindveth = kind "veth". */
static int link_get(int s, const char *name, int *peer, int *kindveth) {
    struct ifinfomsg i; memset(&i, 0, sizeof i);
    struct nlb m; nl_begin(&m, RTM_GETLINK, 0, &i, sizeof i);
    nl_attr(&m, IFLA_IFNAME, name, strlen(name) + 1);
    ((struct nlmsghdr *)m.b)->nlmsg_len = m.len;
    if (send(s, m.b, m.len, 0) != m.len) return 0;
    char rb[4096];
    ssize_t n = recv(s, rb, sizeof rb, 0);
    struct nlmsghdr *h = (struct nlmsghdr *)rb;
    if (n < (ssize_t)NLMSG_HDRLEN || h->nlmsg_type != RTM_NEWLINK) return 0;
    struct ifinfomsg *ifi = NLMSG_DATA(h);
    int len = IFLA_PAYLOAD(h);
    if (peer) *peer = 0;
    if (kindveth) *kindveth = 0;
    for (struct rtattr *a = IFLA_RTA(ifi); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
        if (a->rta_type == IFLA_LINK && peer) memcpy(peer, RTA_DATA(a), 4);
        if ((a->rta_type & 0x3fff) == IFLA_LINKINFO && kindveth) {
            int l2 = RTA_PAYLOAD(a);
            for (struct rtattr *b = RTA_DATA(a); RTA_OK(b, l2); b = RTA_NEXT(b, l2))
                if ((b->rta_type & 0x3fff) == IFLA_INFO_KIND && !strcmp(RTA_DATA(b), "veth")) *kindveth = 1;
        }
    }
    return ifi->ifi_index;
}
/* Poll (up to 1 s) until `name` is gone: namespace teardown is lazy. */
static int wait_gone(int s, const char *name) {
    for (int w = 0; w < 1000; w++) { if (!link_get(s, name, NULL, NULL)) { printf("gone after %d ms\n", w * 20); return 1; } usleep(20000); }
    return 0;
}
/* Count links named veth* in a full dump. */
static int count_veth(int s) {
    struct { struct nlmsghdr h; struct rtgenmsg g; } r;
    memset(&r, 0, sizeof r);
    r.h.nlmsg_len = sizeof r; r.h.nlmsg_type = RTM_GETLINK; r.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP; r.h.nlmsg_seq = 5;
    if (send(s, &r, sizeof r, 0) != sizeof r) return -1;
    int cnt = 0;
    for (;;) {
        char buf[8192];
        ssize_t n = recv(s, buf, sizeof buf, 0);
        if (n <= 0) return -1;
        int done = 0;
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_type == NLMSG_DONE) { done = 1; break; }
            if (h->nlmsg_type != RTM_NEWLINK) continue;
            struct ifinfomsg *ifi = NLMSG_DATA(h);
            int len = IFLA_PAYLOAD(h);
            for (struct rtattr *a = IFLA_RTA(ifi); RTA_OK(a, len); a = RTA_NEXT(a, len))
                if (a->rta_type == IFLA_IFNAME && !strncmp(RTA_DATA(a), "veth", 4)) cnt++;
        }
        if (done) return cnt;
    }
}

static int netns_fd_of(pid_t p) {
    char path[64]; snprintf(path, sizeof path, "/proc/%d/ns/net", p);
    return open(path, O_RDONLY | O_CLOEXEC);
}

/* A child in a new netns that waits for a byte on `gofd`, runs `body(child_sock)` and exits with its result. */
static int ctl[2], done_[2];
/* read one byte, giving up after 15 s (a wedged child must not hang the suite) */
static int rd_to(int fd, char *ch) {
    struct pollfd p = { fd, POLLIN, 0 };
    if (poll(&p, 1, 15000) <= 0) return -1;
    return read(fd, ch, 1);
}
static pid_t spawn_ns(int (*body)(void)) {
    if (pipe(ctl) || pipe(done_)) return -1;
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        close(ctl[1]); close(done_[0]);
        if (unshare(CLONE_NEWNET) != 0) _exit(90);
        char ch = 'r'; write(done_[1], &ch, 1);      /* netns exists */
        if (read(ctl[0], &ch, 1) != 1) _exit(91);   /* veth arrived */
        _exit(body());
    }
    close(ctl[0]); close(done_[1]);
    char ch;
    if (rd_to(done_[0], &ch) != 1) return -1;
    return c;
}
static int wait_child(pid_t c) { int st = 0; waitpid(c, &st, 0); return WIFEXITED(st) ? WEXITSTATUS(st) : 100 + WTERMSIG(st); }

/* ---- child bodies --------------------------------------------------------- */

static int echo_server_body(void) {
    int s = nl_sock();
    int idx = link_get(s, "eth0", NULL, NULL);
    if (!idx) { printf("brtest child fail %d errno %d\n", 1, errno); fflush(stdout); return 1; }
    if (addr_add(s, idx, "10.99.0.2", 24)) { printf("brtest child fail %d errno %d\n", 2, errno); fflush(stdout); return 2; }
    if (link_set_up(s, idx)) { printf("brtest child fail %d errno %d\n", 3, errno); fflush(stdout); return 3; }
    int lo = link_get(s, "lo", NULL, NULL);
    link_set_up(s, lo);
    int u = socket(AF_INET, SOCK_DGRAM, 0), t = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(7777) };
    if (bind(u, (struct sockaddr *)&a, sizeof a)) { printf("brtest child fail %d errno %d\n", 4, errno); fflush(stdout); return 4; }
    if (bind(t, (struct sockaddr *)&a, sizeof a) || listen(t, 4)) { printf("brtest child fail %d errno %d\n", 5, errno); fflush(stdout); return 5; }
    char ch = 'r'; write(done_[1], &ch, 1);          /* listening */
    int served = 0;
    while (served < 2) {
        struct pollfd p[2] = { { u, POLLIN, 0 }, { t, POLLIN, 0 } };
        if (poll(p, 2, 15000) <= 0) { printf("brtest child fail %d errno %d\n", 6, errno); fflush(stdout); return 6; }
        if (p[0].revents & POLLIN) {
            char b[256]; struct sockaddr_in f; socklen_t fl = sizeof f;
            ssize_t n = recvfrom(u, b, sizeof b, 0, (struct sockaddr *)&f, &fl);
            if (n > 0) sendto(u, b, n, 0, (struct sockaddr *)&f, fl);
            served++;
        }
        if (p[1].revents & POLLIN) {
            int c = accept(t, NULL, NULL);
            char b[256]; ssize_t n = read(c, b, sizeof b);
            if (n > 0) write(c, b, n);
            close(c);
            served++;
        }
    }
    { printf("brtest child fail %d errno %d\n", 0, errno); fflush(stdout); return 0; }
}

/* TCP-only child: configure eth0 (10.99.N.2), listen on 7777, serve `tcp_accepts` connections, then
 * exit (0) or, with tcp_accepts == 0, signal and pause with the listener left open. */
static int tcp_accepts;
static int tcp_net;
static int tcp_body(void) {
    char ip[32]; snprintf(ip, sizeof ip, "10.99.%d.2", tcp_net);
    int s = nl_sock();
    int idx = link_get(s, "eth0", NULL, NULL);
    if (!idx || addr_add(s, idx, ip, 24) || link_set_up(s, idx)) return 1;
    int t = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(7777) };
    if (bind(t, (struct sockaddr *)&a, sizeof a) || listen(t, 4)) return 2;
    char ch = 'r'; write(done_[1], &ch, 1);
    for (int i = 0; i < tcp_accepts; i++) {
        struct pollfd p = { t, POLLIN, 0 };
        if (poll(&p, 1, 15000) <= 0) return 3;
        int c = accept(t, NULL, NULL);
        char b[64]; ssize_t n = read(c, b, sizeof b);
        if (n > 0) write(c, b, n);
        close(c);
    }
    if (!tcp_accepts) pause();
    return 0;
}

static int noop_body(void) { return 0; }
static int idle_body(void) { pause(); return 0; }

static int dellink_body(void) {
    /* wait for the parent's go, then delete our end */
    int s = nl_sock();
    int idx = link_get(s, "eth0", NULL, NULL);
    if (!idx) return 1;
    int e = link_del(s, idx);
    if (e) return 2;
    if (link_get(s, "eth0", NULL, NULL)) return 3;
    return 0;
}

/* ---- cases ----------------------------------------------------------------- */

/* True when link `name` disappears from the host within `ms` (each request also drives the reaper). */
static int gone_within(int s, const char *name, int ms) {
    for (int w = 0; w <= ms / 20; w++) {
        if (!link_get(s, name, NULL, NULL)) return 1;
        usleep(20000);
    }
    return 0;
}

/* Child netns with a pair; returns the child pid (or -1) after the host end is configured. */
static pid_t tcp_ns_start(int s, const char *host_if, int net, int accepts) {
    tcp_net = net; tcp_accepts = accepts;
    pid_t c = spawn_ns(tcp_body);
    if (c <= 0) return -1;
    int nsfd = netns_fd_of(c);
    int e = nsfd < 0 ? -1 : veth_new(s, host_if, "eth0", nsfd);
    if (nsfd >= 0) close(nsfd);
    char ip[32]; snprintf(ip, sizeof ip, "10.99.%d.1", net);
    int ia = e ? 0 : link_get(s, host_if, NULL, NULL);
    if (!ia || addr_add(s, ia, ip, 24) || link_set_up(s, ia)) { kill(c, SIGKILL); wait_child(c); return -1; }
    char ch = 'g'; write(ctl[1], &ch, 1);
    if (rd_to(done_[0], &ch) != 1) { kill(c, SIGKILL); wait_child(c); return -1; }
    return c;
}

static void veth_tcp_reap(void) {
    int s = nl_sock();
    pid_t c = tcp_ns_start(s, "vethT", 98, 1);
    CHECK("veth_tcp_reap: child netns listens", c > 0);
    if (c <= 0) { close(s); return; }
    int t = socket(AF_INET, SOCK_STREAM, 0);
    struct timeval tv = { 5, 0 };
    setsockopt(t, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    struct sockaddr_in to = { .sin_family = AF_INET, .sin_port = htons(7777) };
    inet_pton(AF_INET, "10.99.98.2", &to.sin_addr);
    char b[16] = {0};
    int ok = connect(t, (struct sockaddr *)&to, sizeof to) == 0 && write(t, "x", 1) == 1 && read(t, b, sizeof b) == 1;
    CHECK("veth_tcp_reap: TCP connect + echo + close", ok);
    close(t);
    CHECK("veth_tcp_reap: child exited 0", wait_child(c) == 0);
    CHECK("veth_tcp_reap: veth end gone within 5 s of the child exiting", gone_within(s, "vethT", 5000));
    close(s);
}

static void veth_listen_reap(void) {
    int s = nl_sock();
    pid_t c = tcp_ns_start(s, "vethL", 97, 0);
    CHECK("veth_listen_reap: child netns listens", c > 0);
    if (c <= 0) { close(s); return; }
    kill(c, SIGKILL);
    wait_child(c);
    CHECK("veth_listen_reap: veth end gone within 5 s with a never-closed listener", gone_within(s, "vethL", 5000));
    close(s);
}

static void veth_pair(void) {
    int s = nl_sock();
    pid_t c = spawn_ns(echo_server_body);
    CHECK("veth_pair: child netns spawned", c > 0);
    if (c <= 0) return;
    int nsfd = netns_fd_of(c);
    CHECK("veth_pair: open child netns fd", nsfd >= 0);
    int e = veth_new(s, "vethA", "eth0", nsfd);
    CHECK("veth_pair: RTM_NEWLINK kind=veth with VETH_INFO_PEER + NET_NS_FD", e == 0);
    close(nsfd);
    int peer = 0, isv = 0;
    int ia = link_get(s, "vethA", &peer, &isv);
    CHECK("veth_pair: vethA exists, kind veth, IFLA_LINK set", ia > 0 && isv && peer > 0);
    CHECK("veth_pair: addr 10.99.0.1/24 on vethA", ia > 0 && addr_add(s, ia, "10.99.0.1", 24) == 0);
    CHECK("veth_pair: vethA up", ia > 0 && link_set_up(s, ia) == 0);
    char ch = 'g'; write(ctl[1], &ch, 1);
    /* child configures eth0 and starts listening */
    int rd = rd_to(done_[0], &ch);
    CHECK("veth_pair: child configured eth0 and listens", rd == 1);
    if (rd != 1) { kill(c, SIGKILL); wait_child(c); return; }
    /* UDP echo (host -> child, via ARP over the pair) */
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct timeval tv = { 5, 0 };
    setsockopt(u, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    struct sockaddr_in to = { .sin_family = AF_INET, .sin_port = htons(7777) };
    inet_pton(AF_INET, "10.99.0.2", &to.sin_addr);
    ssize_t n = sendto(u, "udp-ping", 8, 0, (struct sockaddr *)&to, sizeof to);
    char b[64] = {0};
    ssize_t r = n == 8 ? recv(u, b, sizeof b, 0) : -1;
    CHECK("veth_pair: UDP echo over the pair", r == 8 && !memcmp(b, "udp-ping", 8));
    close(u);
    /* TCP echo */
    int t = socket(AF_INET, SOCK_STREAM, 0);
    setsockopt(t, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    int ce = connect(t, (struct sockaddr *)&to, sizeof to);
    CHECK("veth_pair: TCP connect over the pair", ce == 0);
    memset(b, 0, sizeof b);
    r = -1;
    if (ce == 0 && write(t, "tcp-ping", 8) == 8) r = read(t, b, sizeof b);
    CHECK("veth_pair: TCP echo over the pair", r == 8 && !memcmp(b, "tcp-ping", 8));
    close(t);
    CHECK("veth_pair: child served both and exited 0", wait_child(c) == 0);
    CHECK("veth_pair: pair reaped after the TCP netns died", gone_within(s, "vethA", 5000));
    close(s);
}

static void veth_ping(void) {
    /* ICMP echo both ways: child pings the host end, host pings the child. */
    int s = nl_sock();
    pid_t c = spawn_ns(idle_body);
    int nsfd = netns_fd_of(c);
    int e = veth_new(s, "vethP", "eth0", nsfd);
    close(nsfd);
    CHECK("veth_ping: pair created", e == 0);
    int ia = link_get(s, "vethP", NULL, NULL);
    addr_add(s, ia, "10.99.1.1", 24); link_set_up(s, ia);
    char cmd[256];
    /* the child's side is configured through a helper child that joins its netns */
    int ns = netns_fd_of(c);
    pid_t h = fork();
    if (h == 0) {
        if (setns(ns, CLONE_NEWNET)) _exit(1);
        int cs = nl_sock();
        int idx = link_get(cs, "eth0", NULL, NULL);
        if (!idx || addr_add(cs, idx, "10.99.1.2", 24) || link_set_up(cs, idx)) _exit(2);
        int lo = link_get(cs, "lo", NULL, NULL); link_set_up(cs, lo);
        int ic = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
        if (ic < 0) _exit(3);
        struct timeval tv = { 5, 0 };
        setsockopt(ic, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
        struct sockaddr_in to = { .sin_family = AF_INET };
        inet_pton(AF_INET, "10.99.1.1", &to.sin_addr);
        unsigned char pk[8] = { 8, 0, 0, 0, 0x12, 0x34, 0, 1 };
        if (sendto(ic, pk, 8, 0, (struct sockaddr *)&to, sizeof to) != 8) _exit(4);
        unsigned char rb[64];
        ssize_t n = recv(ic, rb, sizeof rb, 0);
        _exit(n >= 8 && rb[0] == 0 ? 0 : 5);
    }
    CHECK("veth_ping: child pings the host end", wait_child(h) == 0);
    /* host -> child */
    int ic = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    struct timeval tv = { 5, 0 };
    setsockopt(ic, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    struct sockaddr_in to = { .sin_family = AF_INET };
    inet_pton(AF_INET, "10.99.1.2", &to.sin_addr);
    unsigned char pk[8] = { 8, 0, 0, 0, 0x43, 0x21, 0, 1 };
    unsigned char rb[64];
    ssize_t n = ic >= 0 && sendto(ic, pk, 8, 0, (struct sockaddr *)&to, sizeof to) == 8 ? recv(ic, rb, sizeof rb, 0) : -1;
    CHECK("veth_ping: host pings the child end", n >= 8 && rb[0] == 0);
    (void)cmd;
    close(ns); close(ic);
    kill(c, SIGKILL); wait_child(c);
    close(s);
}

static void veth_dellink(void) {
    int s = nl_sock();
    pid_t c = spawn_ns(dellink_body);
    int nsfd = netns_fd_of(c);
    int e = veth_new(s, "vethD", "eth0", nsfd);
    close(nsfd);
    CHECK("veth_dellink: pair created", e == 0);
    CHECK("veth_dellink: vethD exists", link_get(s, "vethD", NULL, NULL) > 0);
    char ch = 'g'; write(ctl[1], &ch, 1);
    CHECK("veth_dellink: DELLINK eth0 in the child succeeded", wait_child(c) == 0);
    CHECK("veth_dellink: vethD gone from the host", link_get(s, "vethD", NULL, NULL) == 0);
    /* and from the host side, with the peer in a live netns */
    c = spawn_ns(idle_body);
    nsfd = netns_fd_of(c);
    e = veth_new(s, "vethE", "eth0", nsfd);
    close(nsfd);
    int ie = link_get(s, "vethE", NULL, NULL);
    CHECK("veth_dellink: host-side DELLINK", e == 0 && ie > 0 && link_del(s, ie) == 0 && link_get(s, "vethE", NULL, NULL) == 0);
    kill(c, SIGKILL); wait_child(c);
    CHECK("veth_dellink: DELLINK of a missing index is ENODEV", link_del(s, 9999) == ENODEV);
    close(s);
}

static void veth_naming(void) {
    int s = nl_sock();
    int e1 = veth_new(s, NULL, NULL, -1);
    int e2 = veth_new(s, NULL, NULL, -1);
    CHECK("veth_naming: unnamed pairs created", e1 == 0 && e2 == 0);
    int i0 = link_get(s, "veth0", NULL, NULL), i1 = link_get(s, "veth1", NULL, NULL);
    int i2 = link_get(s, "veth2", NULL, NULL), i3 = link_get(s, "veth3", NULL, NULL);
    CHECK("veth_naming: veth%d expands to veth0..veth3 (both ends, same netns)", i0 && i1 && i2 && i3);
    CHECK("veth_naming: EEXIST on a repeated name", veth_new(s, "vethN", "vethM", -1) == 0 && veth_new(s, "vethN", "vethX", -1) == EEXIST);
    if (i0) link_del(s, i0);
    if (i2) link_del(s, i2);
    int vn = link_get(s, "vethN", NULL, NULL);
    if (vn) link_del(s, vn);
    CHECK("veth_naming: DELLINK removes both ends", link_get(s, "veth1", NULL, NULL) == 0 && link_get(s, "veth3", NULL, NULL) == 0
          && link_get(s, "vethM", NULL, NULL) == 0 && count_veth(s) == 0);
    close(s);
}

static void veth_nsgone(void) {
    int s = nl_sock();
    int base = count_veth(s);
    int bad = 0;
    for (int i = 0; i < 200; i++) {
        pid_t c = spawn_ns(noop_body);
        if (c <= 0) { bad = 1; break; }
        int nsfd = netns_fd_of(c);
        int e = veth_new(s, "vethZ", "eth0", nsfd);
        close(nsfd);
        if (e) { printf("iteration %d: NEWLINK errno %d\n", i, e); bad = 1; kill(c, SIGKILL); wait_child(c); break; }
        char ch = 'g'; write(ctl[1], &ch, 1);
        wait_child(c);
        close(ctl[0]); close(ctl[1]); close(done_[0]); close(done_[1]);
        /* the netns died with the child: the pair must be reaped by the next request */
        int w = 0;
        while (link_get(s, "vethZ", NULL, NULL) && w++ < 50) usleep(20000);
        if (w >= 50) { printf("iteration %d: vethZ still present\n", i); bad = 1; break; }
    }
    CHECK("veth_nsgone: 200 pairs created and reaped when the netns died", !bad);
    CHECK("veth_nsgone: link count back to baseline", count_veth(s) == base);
    close(s);
}

/* Accepts one connection, hands the accepted fd to the parent over `fdpass` (SCM_RIGHTS) and exits,
 * leaving the accepted socket as the only thing that can still reference this netns. */
static int fdpass[2];
static int acceptpass_body(void) {
    char ip[32] = "10.99.96.2";
    int s = nl_sock();
    int idx = link_get(s, "eth0", NULL, NULL);
    if (!idx || addr_add(s, idx, ip, 24) || link_set_up(s, idx)) return 1;
    int t = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(7777) };
    if (bind(t, (struct sockaddr *)&a, sizeof a) || listen(t, 4)) return 2;
    char ch = 'r'; write(done_[1], &ch, 1);
    struct pollfd p = { t, POLLIN, 0 };
    if (poll(&p, 1, 15000) <= 0) return 3;
    int c = accept(t, NULL, NULL);
    if (c < 0) return 4;
    char cb[CMSG_SPACE(sizeof(int))]; memset(cb, 0, sizeof cb);
    char d = 'a'; struct iovec io = { &d, 1 };
    struct msghdr m = { .msg_iov = &io, .msg_iovlen = 1, .msg_control = cb, .msg_controllen = sizeof cb };
    struct cmsghdr *h = CMSG_FIRSTHDR(&m);
    h->cmsg_level = SOL_SOCKET; h->cmsg_type = SCM_RIGHTS; h->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(h), &c, sizeof c);
    if (sendmsg(fdpass[1], &m, 0) != 1) return 5;
    return 0;
}

/* An accepted socket belongs to the listener's netns (Linux sk_clone): while the parent holds it the netns
 * and its veth stay alive after every process in the netns exited, and are reaped once it is closed. */
static void accept_ns(void) {
    int s = nl_sock();
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, fdpass)) { CHECK("accept_ns: socketpair", 0); return; }
    pid_t c = spawn_ns(acceptpass_body);
    int nsfd = c > 0 ? netns_fd_of(c) : -1;
    int e = nsfd < 0 ? -1 : veth_new(s, "vethC", "eth0", nsfd);
    if (nsfd >= 0) close(nsfd);
    int ia = e ? 0 : link_get(s, "vethC", NULL, NULL);
    CHECK("accept_ns: pair created", ia > 0 && addr_add(s, ia, "10.99.96.1", 24) == 0 && link_set_up(s, ia) == 0);
    if (ia <= 0) { if (c > 0) { kill(c, SIGKILL); wait_child(c); } close(s); return; }
    char ch = 'g'; write(ctl[1], &ch, 1);
    CHECK("accept_ns: child netns listens", rd_to(done_[0], &ch) == 1);
    int t = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in to = { .sin_family = AF_INET, .sin_port = htons(7777) };
    inet_pton(AF_INET, "10.99.96.2", &to.sin_addr);
    CHECK("accept_ns: connect", connect(t, (struct sockaddr *)&to, sizeof to) == 0);
    char cb[CMSG_SPACE(sizeof(int))]; memset(cb, 0, sizeof cb);
    char d; struct iovec io = { &d, 1 };
    struct msghdr m = { .msg_iov = &io, .msg_iovlen = 1, .msg_control = cb, .msg_controllen = sizeof cb };
    int got = -1;
    struct pollfd p = { fdpass[0], POLLIN, 0 };
    if (poll(&p, 1, 15000) > 0 && recvmsg(fdpass[0], &m, 0) == 1) {
        struct cmsghdr *h = CMSG_FIRSTHDR(&m);
        if (h && h->cmsg_type == SCM_RIGHTS) memcpy(&got, CMSG_DATA(h), sizeof got);
    }
    CHECK("accept_ns: accepted fd received", got >= 0);
    CHECK("accept_ns: child exited 0", wait_child(c) == 0);
    /* the reaper runs with every request; give it ample time to (wrongly) reap */
    int alive = 1;
    for (int w = 0; w < 100; w++) { if (!link_get(s, "vethC", NULL, NULL)) { alive = 0; break; } usleep(20000); }
    CHECK("accept_ns: netns pinned by the accepted socket (veth survives 2 s)", alive);
    if (got >= 0) {
        struct timeval tv = { 5, 0 };
        setsockopt(t, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
        char b[4] = {0};
        int ok = write(t, "x", 1) == 1 && read(got, b, 1) == 1 && b[0] == 'x';
        CHECK("accept_ns: accepted socket still carries data", ok);
        close(got);
    }
    close(t);
    CHECK("accept_ns: veth reaped within 5 s of closing the accepted socket", gone_within(s, "vethC", 5000));
    close(fdpass[0]); close(fdpass[1]);
    close(s);
}

int main(int argc, char **argv) {
    const char *only = argc > 1 ? argv[1] : NULL;
    if (!only || !strcmp(only, "veth_pair")) veth_pair();
    if (!only || !strcmp(only, "accept_ns")) accept_ns();
    if (!only || !strcmp(only, "veth_tcp_reap")) veth_tcp_reap();
    if (!only || !strcmp(only, "veth_listen_reap")) veth_listen_reap();
    if (!only || !strcmp(only, "veth_ping")) veth_ping();
    if (!only || !strcmp(only, "veth_dellink")) veth_dellink();
    if (!only || !strcmp(only, "veth_naming")) veth_naming();
    if (!only || !strcmp(only, "veth_nsgone")) veth_nsgone();
    printf("brtest: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
