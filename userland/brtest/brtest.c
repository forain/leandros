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


/* ---- bridge helpers (lane bridge) ----------------------------------------- */
#ifndef IFLA_BR_FORWARD_DELAY
#define IFLA_BR_FORWARD_DELAY 1
#define IFLA_BR_AGEING_TIME 4
#define IFLA_BR_STP_STATE 5
#define IFLA_BR_VLAN_FILTERING 7
#endif
static int br_new(int s, const char *name, const unsigned char *mac) {
    struct ifinfomsg i; memset(&i, 0, sizeof i);
    struct nlb m; nl_begin(&m, RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, &i, sizeof i);
    nl_attr(&m, IFLA_IFNAME, name, strlen(name) + 1);
    if (mac) nl_attr(&m, IFLA_ADDRESS, mac, 6);
    int li = nl_nest(&m, IFLA_LINKINFO);
    nl_attr(&m, IFLA_INFO_KIND, "bridge", 7);
    nl_end(&m, li);
    return nl_do(s, &m);
}
/* veth with the host end `a` enslaved to `master` at create time (IFLA_MASTER on the main message). */
static int veth_new_m(int s, const char *a, const char *b, int nsfd, int master) {
    struct ifinfomsg i; memset(&i, 0, sizeof i);
    struct nlb m; nl_begin(&m, RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, &i, sizeof i);
    nl_attr(&m, IFLA_IFNAME, a, strlen(a) + 1);
    if (master) nl_attr(&m, IFLA_MASTER, &master, 4);
    int li = nl_nest(&m, IFLA_LINKINFO);
    nl_attr(&m, IFLA_INFO_KIND, "veth", 5);
    int di = nl_nest(&m, IFLA_INFO_DATA);
    int pi = nl_nest(&m, VETH_INFO_PEER);
    m.len += sizeof i;
    nl_attr(&m, IFLA_IFNAME, b, strlen(b) + 1);
    if (nsfd >= 0) nl_attr(&m, IFLA_NET_NS_FD, &nsfd, 4);
    nl_end(&m, pi); nl_end(&m, di); nl_end(&m, li);
    return nl_do(s, &m);
}
static int link_set_master(int s, int idx, int master) {
    struct ifinfomsg i; memset(&i, 0, sizeof i); i.ifi_index = idx;
    struct nlb m; nl_begin(&m, RTM_SETLINK, 0, &i, sizeof i);
    nl_attr(&m, IFLA_MASTER, &master, 4);
    return nl_do(s, &m);
}
static int link_set_mac(int s, int idx, const unsigned char *mac) {
    struct ifinfomsg i; memset(&i, 0, sizeof i); i.ifi_index = idx;
    struct nlb m; nl_begin(&m, RTM_NEWLINK, 0, &i, sizeof i);
    nl_attr(&m, IFLA_ADDRESS, mac, 6);
    return nl_do(s, &m);
}
struct brinfo { int idx, master, isbr; unsigned char mac[6]; int have_mac;
                int ageing_sz, stp_sz, vlan_sz, fwd_sz; unsigned ageing, fwd, stp; unsigned char vlan; };
static int link_info(int s, const char *name, struct brinfo *o) {
    struct ifinfomsg i; memset(&i, 0, sizeof i);
    struct nlb m; nl_begin(&m, RTM_GETLINK, 0, &i, sizeof i);
    nl_attr(&m, IFLA_IFNAME, name, strlen(name) + 1);
    ((struct nlmsghdr *)m.b)->nlmsg_len = m.len;
    memset(o, 0, sizeof *o);
    if (send(s, m.b, m.len, 0) != m.len) return 0;
    char rb[4096];
    ssize_t n = recv(s, rb, sizeof rb, 0);
    struct nlmsghdr *h = (struct nlmsghdr *)rb;
    if (n < (ssize_t)NLMSG_HDRLEN || h->nlmsg_type != RTM_NEWLINK) return 0;
    struct ifinfomsg *ifi = NLMSG_DATA(h);
    o->idx = ifi->ifi_index;
    int len = IFLA_PAYLOAD(h);
    for (struct rtattr *a = IFLA_RTA(ifi); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
        int t = a->rta_type & 0x3fff;
        if (t == IFLA_MASTER && RTA_PAYLOAD(a) == 4) memcpy(&o->master, RTA_DATA(a), 4);
        if (t == IFLA_ADDRESS && RTA_PAYLOAD(a) == 6) { memcpy(o->mac, RTA_DATA(a), 6); o->have_mac = 1; }
        if (t == IFLA_LINKINFO) {
            int l2 = RTA_PAYLOAD(a);
            for (struct rtattr *b = RTA_DATA(a); RTA_OK(b, l2); b = RTA_NEXT(b, l2)) {
                int bt = b->rta_type & 0x3fff;
                if (bt == IFLA_INFO_KIND && !strcmp(RTA_DATA(b), "bridge")) o->isbr = 1;
                if (bt == IFLA_INFO_DATA) {
                    int l3 = RTA_PAYLOAD(b);
                    for (struct rtattr *c = RTA_DATA(b); RTA_OK(c, l3); c = RTA_NEXT(c, l3)) {
                        int ct = c->rta_type & 0x3fff, pl = RTA_PAYLOAD(c);
                        if (ct == IFLA_BR_AGEING_TIME) { o->ageing_sz = pl; if (pl == 4) memcpy(&o->ageing, RTA_DATA(c), 4); }
                        if (ct == IFLA_BR_FORWARD_DELAY) { o->fwd_sz = pl; if (pl == 4) memcpy(&o->fwd, RTA_DATA(c), 4); }
                        if (ct == IFLA_BR_STP_STATE) { o->stp_sz = pl; if (pl == 4) memcpy(&o->stp, RTA_DATA(c), 4); }
                        if (ct == IFLA_BR_VLAN_FILTERING) { o->vlan_sz = pl; if (pl == 1) o->vlan = *(unsigned char *)RTA_DATA(c); }
                    }
                }
            }
        }
    }
    return o->idx;
}
/* Count links in a dump whose IFLA_MASTER == master. */
static int count_master(int s, int master) {
    struct { struct nlmsghdr h; struct ifinfomsg g; char pad[16]; } r;
    memset(&r, 0, sizeof r);
    r.h.nlmsg_type = RTM_GETLINK; r.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP; r.h.nlmsg_seq = 6;
    struct rtattr *a = (struct rtattr *)r.pad;
    a->rta_type = IFLA_MASTER; a->rta_len = RTA_LENGTH(4); memcpy(RTA_DATA(a), &master, 4);
    r.h.nlmsg_len = NLMSG_HDRLEN + sizeof r.g + RTA_ALIGN(a->rta_len);
    if (send(s, &r, r.h.nlmsg_len, 0) != (ssize_t)r.h.nlmsg_len) return -1;
    int cnt = 0;
    for (;;) {
        char buf[8192];
        ssize_t n = recv(s, buf, sizeof buf, 0);
        if (n <= 0) return -1;
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_type == NLMSG_DONE) return cnt;
            if (h->nlmsg_type == RTM_NEWLINK) cnt++;
        }
    }
}

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

/* ---- bridge_l2: two netns behind br0 ------------------------------------- */
static const char *g_ip;
static int g_peer_is_client;
static int br_conf_eth0(void) {
    int s = nl_sock();
    int idx = link_get(s, "eth0", NULL, NULL);
    if (!idx) return 1;
    if (addr_add(s, idx, g_ip, 24)) return 2;
    if (link_set_up(s, idx)) return 3;
    int lo = link_get(s, "lo", NULL, NULL); link_set_up(s, lo);
    close(s);
    return 0;
}
/* netns A: eth0 10.98.0.2, serves one UDP echo and one TCP echo on 7777 */
static int br_server_body(void) {
    int e = br_conf_eth0(); if (e) return e;
    int u = socket(AF_INET, SOCK_DGRAM, 0), t = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(7777) };
    if (bind(u, (struct sockaddr *)&a, sizeof a)) return 4;
    if (bind(t, (struct sockaddr *)&a, sizeof a) || listen(t, 4)) return 5;
    char ch = 'r'; write(done_[1], &ch, 1);
    int served = 0;
    while (served < 2) {
        struct pollfd p[2] = { { u, POLLIN, 0 }, { t, POLLIN, 0 } };
        if (poll(p, 2, 20000) <= 0) return 6;
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
    return 0;
}
static int icmp_ping(const char *ip) {
    int ic = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    if (ic < 0) return 0;
    struct timeval tv = { 5, 0 };
    setsockopt(ic, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    struct sockaddr_in to = { .sin_family = AF_INET };
    inet_pton(AF_INET, ip, &to.sin_addr);
    unsigned char pk[8] = { 8, 0, 0, 0, 0x55, 0x66, 0, 1 }, rb[64];
    ssize_t n = sendto(ic, pk, 8, 0, (struct sockaddr *)&to, sizeof to) == 8 ? recv(ic, rb, sizeof rb, 0) : -1;
    close(ic);
    return n >= 8 && rb[0] == 0;
}
/* netns B: eth0 10.98.0.3; talks to A (UDP+TCP), pings the bridge address, TCP to the host listener at 10.98.0.1:7778 */
static int br_client_body(void) {
    int e = br_conf_eth0(); if (e) return e;
    struct timeval tv = { 5, 0 };
    struct sockaddr_in to = { .sin_family = AF_INET, .sin_port = htons(7777) };
    inet_pton(AF_INET, "10.98.0.2", &to.sin_addr);
    char b[64] = {0};
    if (!icmp_ping("10.98.0.1")) return 14;
    if (!icmp_ping("10.98.0.2")) return 15;
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    setsockopt(u, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    if (sendto(u, "b-udp", 5, 0, (struct sockaddr *)&to, sizeof to) != 5) return 10;
    if (recv(u, b, sizeof b, 0) != 5 || memcmp(b, "b-udp", 5)) return 11;
    int t = socket(AF_INET, SOCK_STREAM, 0);
    setsockopt(t, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    if (connect(t, (struct sockaddr *)&to, sizeof to)) return 12;
    memset(b, 0, sizeof b);
    if (write(t, "b-tcp", 5) != 5 || read(t, b, sizeof b) != 5 || memcmp(b, "b-tcp", 5)) return 13;
    close(t);
    int h = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in hh = { .sin_family = AF_INET, .sin_port = htons(7778) };
    inet_pton(AF_INET, "10.98.0.1", &hh.sin_addr);
    setsockopt(h, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    if (connect(h, (struct sockaddr *)&hh, sizeof hh)) return 16;
    if (write(h, "to-host", 7) != 7) return 17;
    memset(b, 0, sizeof b);
    if (read(h, b, sizeof b) != 2 || memcmp(b, "ok", 2)) return 18;
    return 0;
}

static void bridge_l2(void) {
    int s = nl_sock();
    unsigned char brmac[6] = { 0x02, 0x00, 0x5e, 0x98, 0x00, 0x01 };
    CHECK("bridge_l2: RTM_NEWLINK kind=bridge br0", br_new(s, "br0", brmac) == 0);
    struct brinfo bi;
    int bidx = link_info(s, "br0", &bi);
    CHECK("bridge_l2: br0 exists, kind bridge", bidx > 0 && bi.isbr);
    CHECK("bridge_l2: br0 IFLA_ADDRESS is the requested MAC", bi.have_mac && !memcmp(bi.mac, brmac, 6));
    CHECK("bridge_l2: br0 10.98.0.1/24 + up", bidx > 0 && addr_add(s, bidx, "10.98.0.1", 24) == 0 && link_set_up(s, bidx) == 0);
    CHECK("bridge_l2: duplicate br0 is EEXIST", br_new(s, "br0", NULL) == EEXIST);
    /* host listener on 0.0.0.0:7778, reached through the bridge address */
    int hl = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in ha = { .sin_family = AF_INET, .sin_port = htons(7778) };
    CHECK("bridge_l2: host wildcard listener", !bind(hl, (struct sockaddr *)&ha, sizeof ha) && !listen(hl, 4));
    /* netns A and B, host ends enslaved at create time */
    g_ip = "10.98.0.2";
    pid_t ca = spawn_ns(br_server_body);
    int cA[2] = { ctl[0], ctl[1] }, dA[2] = { done_[0], done_[1] };
    g_ip = "10.98.0.3";
    pid_t cb = spawn_ns(br_client_body);
    int cB[2] = { ctl[0], ctl[1] }, dB[2] = { done_[0], done_[1] };
    CHECK("bridge_l2: netns A and B spawned", ca > 0 && cb > 0);
    if (ca <= 0 || cb <= 0) return;
    int fa = netns_fd_of(ca), fb = netns_fd_of(cb);
    CHECK("bridge_l2: veth A created enslaved to br0 (IFLA_MASTER at create)", veth_new_m(s, "vethA", "eth0", fa, bidx) == 0);
    CHECK("bridge_l2: veth B created enslaved to br0", veth_new_m(s, "vethB", "eth0", fb, bidx) == 0);
    close(fa); close(fb);
    struct brinfo pa, pb;
    int ia = link_info(s, "vethA", &pa), ib = link_info(s, "vethB", &pb);
    CHECK("bridge_l2: ports report IFLA_MASTER = br0", ia > 0 && ib > 0 && pa.master == bidx && pb.master == bidx);
    CHECK("bridge_l2: dump with IFLA_MASTER=br0 lists exactly the two ports", count_master(s, bidx) == 2);
    CHECK("bridge_l2: ports up", link_set_up(s, ia) == 0 && link_set_up(s, ib) == 0);
    char ch = 'g';
    write(cA[1], &ch, 1);
    int rd = rd_to(dA[0], &ch);
    CHECK("bridge_l2: A configured eth0 and listens", rd == 1);
    if (rd != 1) { kill(ca, SIGKILL); kill(cb, SIGKILL); wait_child(ca); wait_child(cb); return; }
    CHECK("bridge_l2: host pings A across the bridge", icmp_ping("10.98.0.2"));
    /* B runs its script; the host accepts B's connection on 7778 */
    write(cB[1], &ch, 1);
    struct pollfd pl = { hl, POLLIN, 0 };
    int got = 0;
    if (poll(&pl, 1, 20000) > 0) {
        int c = accept(hl, NULL, NULL);
        char b[32] = {0};
        struct pollfd pc = { c, POLLIN, 0 };
        if (c >= 0 && poll(&pc, 1, 10000) > 0 && read(c, b, sizeof b) == 7 && !memcmp(b, "to-host", 7)) { write(c, "ok", 2); got = 1; }
        if (c >= 0) close(c);
    }
    CHECK("bridge_l2: B reached the host listener at 10.98.0.1", got);
    int rb = wait_child(cb);
    printf("B exit %d\n", rb);
    CHECK("bridge_l2: B: UDP+TCP to A, ping bridge addr, ping A, TCP to host", rb == 0);
    CHECK("bridge_l2: A served B and exited 0", wait_child(ca) == 0);
    close(hl);
    /* a netns that ran TCP may be reaped late (veth lane note): compare against what is left now */
    int base_m = count_master(s, bidx), base_v = count_veth(s);
    /* bridge_getlink (ports whose peer stays in the host netns, so they outlive the children) */
    CHECK("bridge_getlink: persistent ports vethQ/vethR enslaved at create",
          veth_new_m(s, "vethQ", "vethX", -1, bidx) == 0 && veth_new_m(s, "vethR", "vethY", -1, bidx) == 0);
    int qa = link_get(s, "vethQ", NULL, NULL), qb = link_get(s, "vethR", NULL, NULL);
    CHECK("bridge_getlink: br0 INFO_DATA has exact-size AGEING_TIME/FORWARD_DELAY/STP_STATE/VLAN_FILTERING",
          link_info(s, "br0", &bi) > 0 && bi.ageing_sz == 4 && bi.fwd_sz == 4 && bi.stp_sz == 4 && bi.vlan_sz == 1);
    CHECK("bridge_getlink: defaults ageing 30000, fwd_delay 1500, stp 0, vlan_filtering 0",
          bi.ageing == 30000 && bi.fwd == 1500 && bi.stp == 0 && bi.vlan == 0);
    CHECK("bridge_getlink: a non-bridge has no bridge INFO_DATA", link_info(s, "vethQ", &pa) > 0 && !pa.isbr && pa.master == bidx);
    CHECK("bridge_getlink: dump filter lists the two new ports", count_master(s, bidx) == base_m + 2);
    CHECK("bridge_getlink: SETLINK IFLA_MASTER=0 releases vethQ", link_set_master(s, qa, 0) == 0 && link_info(s, "vethQ", &pa) > 0 && pa.master == 0);
    CHECK("bridge_getlink: dump filter now lists one new port", count_master(s, bidx) == base_m + 1);
    CHECK("bridge_getlink: SETLINK IFLA_MASTER=br0 re-enslaves vethQ", link_set_master(s, qa, bidx) == 0 && link_info(s, "vethQ", &pa) > 0 && pa.master == bidx);
    CHECK("bridge_getlink: IFLA_MASTER to a non-bridge is rejected", link_set_master(s, qa, qb) != 0);
    unsigned char m2[6] = { 0x02, 0x00, 0x5e, 0x98, 0x00, 0x09 };
    CHECK("bridge_getlink: SETLINK changes the bridge MAC", link_set_mac(s, bidx, m2) == 0 && link_info(s, "br0", &bi) > 0 && !memcmp(bi.mac, m2, 6));
    /* bridge_teardown */
    CHECK("bridge_teardown: DELLINK vethQ", link_del(s, qa) == 0 && !link_get(s, "vethQ", NULL, NULL));
    CHECK("bridge_teardown: DELLINK vethR", link_del(s, qb) == 0 && !link_get(s, "vethR", NULL, NULL));
    CHECK("bridge_teardown: DELLINK br0", link_del(s, bidx) == 0 && !link_get(s, "br0", NULL, NULL));
    CHECK("bridge_teardown: no vifs left of ours", count_veth(s) <= base_v && !link_get(s, "vethX", NULL, NULL) && !link_get(s, "vethY", NULL, NULL));
    /* a bridge deleted with a port still attached releases the port */
    base_v = count_veth(s);
    CHECK("bridge_teardown: br1 with a port", br_new(s, "br1", NULL) == 0 && veth_new_m(s, "vethC", "vethD", -1, link_get(s, "br1", NULL, NULL)) == 0);
    int i1 = link_get(s, "br1", NULL, NULL);
    CHECK("bridge_teardown: DELLINK br1 with a live port releases it", link_del(s, i1) == 0 && !link_get(s, "br1", NULL, NULL)
          && link_info(s, "vethC", &pa) > 0 && pa.master == 0);
    int ic = link_get(s, "vethC", NULL, NULL);
    if (ic) link_del(s, ic);
    CHECK("bridge_teardown: nothing left over", count_veth(s) == base_v);
    close(s);
}

/* ---- fwd_udp / fwd_ping / fwd_off: a netns behind br0 reaching the outside via NAT ------------------ */
static int route_default(int s, int oif, const char *gw) {
    struct rtmsg r; memset(&r, 0, sizeof r);
    r.rtm_family = AF_INET; r.rtm_table = RT_TABLE_MAIN; r.rtm_protocol = RTPROT_BOOT;
    r.rtm_scope = RT_SCOPE_UNIVERSE; r.rtm_type = RTN_UNICAST;
    struct in_addr g; inet_pton(AF_INET, gw, &g);
    struct nlb m; nl_begin(&m, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, &r, sizeof r);
    nl_attr(&m, RTA_GATEWAY, &g, 4); nl_attr(&m, RTA_OIF, &oif, 4);
    return nl_do(s, &m);
}
static int g_fwd_mode; /* 0 = UDP DNS query, 1 = ping the QEMU gateway; expect success */
static int g_fwd_expect = 1;
static void fwd_resolver(char *out) {
    strcpy(out, "10.0.2.3");
    FILE *f = fopen("/etc/resolv.conf", "r");
    if (!f) return;
    char l[128];
    while (fgets(l, sizeof l, f)) {
        char ip[64];
        if (sscanf(l, "nameserver %63s", ip) == 1) { strcpy(out, ip); break; }
    }
    fclose(f);
}
/* the host's default gateway, from /proc/net/route (10.0.2.2 on QEMU user-net, other on vmnet) */
static void fwd_gateway(char *out) {
    strcpy(out, "10.0.2.2");
    FILE *f = fopen("/proc/net/route", "r");
    if (!f) return;
    char l[256];
    while (fgets(l, sizeof l, f)) {
        char ifn[32]; unsigned dst, gw, fl;
        if (sscanf(l, "%31s %x %x %x", ifn, &dst, &gw, &fl) == 4 && dst == 0 && (fl & 2)) {
            struct in_addr a = { .s_addr = gw };
            inet_ntop(AF_INET, &a, out, 16);
            break;
        }
    }
    fclose(f);
}
static int dns_query(const char *srv) {
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct timeval tv = { 4, 0 };
    setsockopt(u, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    struct sockaddr_in to = { .sin_family = AF_INET, .sin_port = htons(53) };
    inet_pton(AF_INET, srv, &to.sin_addr);
    unsigned char q[] = { 0x4c, 0x45, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0,
                          7, 'e','x','a','m','p','l','e', 3, 'c','o','m', 0, 0, 1, 0, 1 };
    unsigned char rb[512];
    int ok = 0;
    for (int tries = 0; tries < 3 && !ok; tries++) {
        if (sendto(u, q, sizeof q, 0, (struct sockaddr *)&to, sizeof to) != (ssize_t)sizeof q) break;
        ssize_t n = recv(u, rb, sizeof rb, 0);
        ok = n >= 12 && rb[0] == 0x4c && rb[1] == 0x45 && (rb[2] & 0x80);
    }
    close(u);
    return ok;
}
/* TCP helpers: DNS over TCP/53 (2-byte length prefix) and an HTTP GET (-h IP[:PORT]). */
static const char *g_http; /* host:port for the HTTP GET, NULL = skip */
static int tcp_conn(const char *ip, int port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct timeval tv = { 8, 0 };
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
    struct sockaddr_in to = { .sin_family = AF_INET, .sin_port = htons(port) };
    inet_pton(AF_INET, ip, &to.sin_addr);
    if (connect(fd, (struct sockaddr *)&to, sizeof to)) { close(fd); return -1; }
    return fd;
}
static int tcp_dns_c(const char *srv) {
    int fd = tcp_conn(srv, 53);
    if (fd < 0) return 0;
    unsigned char q[] = { 0, 29, 0x4c, 0x45, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0,
                          7, 'e','x','a','m','p','l','e', 3, 'c','o','m', 0, 0, 1, 0, 1 };
    int ok = 0;
    if (send(fd, q, sizeof q, 0) == (ssize_t)sizeof q) {
        unsigned char rb[600]; int got = 0, want = 2;
        while (got < want) {
            ssize_t n = recv(fd, rb + got, sizeof rb - got, 0);
            if (n <= 0) break;
            got += n;
            if (got >= 2) want = 2 + (rb[0] << 8 | rb[1]);
        }
        ok = got >= 14 && got >= want && rb[2] == 0x4c && rb[3] == 0x45 && (rb[4] & 0x80);
    }
    close(fd);
    return ok;
}
static int http_get(const char *hp) {
    char ip[64]; int port = 80;
    snprintf(ip, sizeof ip, "%s", hp);
    char *c = strchr(ip, ':');
    if (c) { *c = 0; port = atoi(c + 1); }
    int fd = tcp_conn(ip, port);
    if (fd < 0) return 0;
    char req[160];
    int rl = snprintf(req, sizeof req, "GET / HTTP/1.0\r\nHost: %s\r\nConnection: close\r\n\r\n", ip);
    char buf[2048]; int got = 0, ok = 0;
    if (send(fd, req, rl, 0) == rl) {
        ssize_t n;
        while ((n = recv(fd, buf + got, sizeof buf - 1 - got, 0)) > 0 && got < (int)sizeof buf - 1) got += n;
        buf[got] = 0;
        ok = got >= 12 && !strncmp(buf, "HTTP/", 5);
    }
    close(fd);
    return ok;
}
/* First port of the NAT range (61000..64999) a host bind() cannot take; 0 if all bind. */
static int nat_scan(void) {
    for (int p = 61000; p < 65000; p++) {
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(p) };
        int r = bind(fd, (struct sockaddr *)&a, sizeof a), e = errno;
        close(fd);
        if (r && e == EADDRINUSE) return p;
    }
    return 0;
}
static int nat_bindable(int p) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(p) };
    int r = bind(fd, (struct sockaddr *)&a, sizeof a);
    close(fd);
    return r == 0;
}
static int fwd_body(void) {
    g_ip = "10.97.0.2";
    int s = nl_sock();
    int idx = link_get(s, "eth0", NULL, NULL);
    if (!idx || addr_add(s, idx, g_ip, 24) || link_set_up(s, idx)) return 1;
    int lo = link_get(s, "lo", NULL, NULL); link_set_up(s, lo);
    if (route_default(s, idx, "10.97.0.1")) return 2;
    close(s);
    char ch = 'r'; write(done_[1], &ch, 1);
    if (rd_to(ctl[0], &ch) != 1) return 3;
    int ok;
    if (g_fwd_mode == 0) { char srv[64]; fwd_resolver(srv); ok = dns_query(srv); }
    else if (g_fwd_mode == 2) {
        char srv[64]; fwd_resolver(srv);
        ok = tcp_dns_c(srv);
        if (ok && g_http) ok = http_get(g_http);
    }
    else if (g_fwd_mode >= 4) { /* hold an established TCP flow open until released */
        char srv[64]; fwd_resolver(srv);
        int fd = tcp_conn(srv, 53);
        if (fd < 0) return 9;
        write(done_[1], &ch, 1);
        rd_to(ctl[0], &ch);
        close(fd);
        ok = 1;
    }
    else if (g_fwd_mode == 6) { /* TCP to the bridge's own address: a host 0.0.0.0 listener, with forwarding on */
        int fd = tcp_conn("10.97.0.1", 7779);
        ok = fd >= 0;
        if (fd >= 0) close(fd);
    }
    else { char gw[32]; fwd_gateway(gw); ok = icmp_ping(gw); }
    return ok == g_fwd_expect ? 0 : 4 + ok;
}
static int ip_forward_set(int on) {
    int fd = open("/proc/sys/net/ipv4/ip_forward", O_WRONLY);
    if (fd < 0) return -1;
    int r = write(fd, on ? "1\n" : "0\n", 2);
    close(fd);
    return r == 2 ? 0 : -1;
}
static void fwd_case(const char *name, int mode, int ipfwd) {
    char msg[96];
    int s = nl_sock();
    unsigned char brmac[6] = { 0x02, 0x00, 0x5e, 0x97, 0x00, 0x01 };
    int ok = br_new(s, "br0", brmac) == 0;
    int bidx = link_get(s, "br0", NULL, NULL);
    ok = ok && bidx > 0 && addr_add(s, bidx, "10.97.0.1", 24) == 0 && link_set_up(s, bidx) == 0;
    snprintf(msg, sizeof msg, "%s: br0 10.97.0.1/24 up", name); CHECK(msg, ok);
    snprintf(msg, sizeof msg, "%s: ip_forward=%d", name, ipfwd); CHECK(msg, ip_forward_set(ipfwd) == 0);
    g_fwd_mode = mode; g_fwd_expect = ipfwd;
    int hl = -1;
    if (mode == 6) {
        hl = socket(AF_INET, SOCK_STREAM, 0);
        struct sockaddr_in ha = { .sin_family = AF_INET, .sin_port = htons(7779) };
        snprintf(msg, sizeof msg, "%s: host wildcard listener on 7779", name);
        CHECK(msg, !bind(hl, (struct sockaddr *)&ha, sizeof ha) && !listen(hl, 4));
    }
    pid_t c = spawn_ns(fwd_body);
    int cc[2] = { ctl[0], ctl[1] }, dd[2] = { done_[0], done_[1] };
    snprintf(msg, sizeof msg, "%s: netns spawned", name); CHECK(msg, c > 0);
    if (c <= 0) { link_del(s, bidx); close(s); return; }
    int fc = netns_fd_of(c);
    ok = veth_new_m(s, "vethF", "eth0", fc, bidx) == 0;
    close(fc);
    int iv = link_get(s, "vethF", NULL, NULL);
    ok = ok && iv > 0 && link_set_up(s, iv) == 0;
    snprintf(msg, sizeof msg, "%s: veth enslaved + up", name); CHECK(msg, ok);
    char ch = 'g'; write(cc[1], &ch, 1);
    int rd = rd_to(dd[0], &ch);
    snprintf(msg, sizeof msg, "%s: netns configured eth0 + default via 10.97.0.1", name); CHECK(msg, rd == 1);
    if (rd == 1) write(cc[1], &ch, 1); /* go */
    int natp = 0;
    if (mode >= 4) {
        snprintf(msg, sizeof msg, "%s: precondition: no NAT port is held yet", name);
        CHECK(msg, nat_scan() == 0);
        rd = rd_to(dd[0], &ch);
        snprintf(msg, sizeof msg, "%s: TCP flow to the resolver established through NAT", name); CHECK(msg, rd == 1);
        natp = nat_scan();
        if (mode == 4) {
            snprintf(msg, sizeof msg, "%s: host bind() to the in-use NAT port gets EADDRINUSE (port %d)", name, natp);
            CHECK(msg, natp > 0);
            write(cc[1], &ch, 1);
        } else kill(c, SIGKILL);
    }
    int rc = wait_child(c);
    if (mode == 5) {
        usleep(500000);
        snprintf(msg, sizeof msg, "%s: after the netns died its NAT port %d is released", name, natp);
        CHECK(msg, natp > 0 && nat_bindable(natp));
        snprintf(msg, sizeof msg, "%s: no NAT port stays reserved", name); CHECK(msg, nat_scan() == 0);
        rc = 0;
    }
    printf("%s child exit %d\n", name, rc);
    if (mode == 6) snprintf(msg, sizeof msg, "%s: connect to the bridge address reaches the host 0.0.0.0 listener", name);
    else if (mode == 2) snprintf(msg, sizeof msg, "%s: TCP DNS query to the resolver%s is answered through NAT", name, g_http ? " and an HTTP GET" : "");
    else if (mode >= 4) snprintf(msg, sizeof msg, "%s: flow holder exited cleanly", name);
    else if (mode == 0 && ipfwd) snprintf(msg, sizeof msg, "%s: UDP DNS query to the resolver is answered through NAT", name);
    else if (mode == 1 && ipfwd) snprintf(msg, sizeof msg, "%s: ICMP echo to the default gateway is answered through NAT", name);
    else snprintf(msg, sizeof msg, "%s: ip_forward=0 means no reply (nothing forwarded)", name);
    CHECK(msg, rc == 0);
    ip_forward_set(0);
    if (hl >= 0) close(hl);
    if (iv > 0) link_del(s, iv);
    link_del(s, bidx);
    close(s);
}
static void fwd_udp(void) { fwd_case("fwd_udp", 0, 1); }
static void fwd_ping(void) { fwd_case("fwd_ping", 1, 1); }
static void fwd_off(void) { fwd_case("fwd_off", 0, 0); }
static void fwd_tcp(void) { fwd_case("fwd_tcp", 2, 1); }
static void fwd_port_reserved(void) { fwd_case("fwd_port_reserved", 4, 1); }
static void fwd_hostlisten(void) { fwd_case("fwd_hostlisten", 6, 1); }
static void fwd_reap(void) { fwd_case("fwd_reap", 5, 1); }

/* A host wildcard listener must not leak into a container netns: a netns
 * stack that connects to its OWN address on the listener's port gets
 * ECONNREFUSED, and the host listener never sees a connection. */
static void hostlisten_not_in_ns(void) {
    int s = nl_sock();
    int hl = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in ha = { .sin_family = AF_INET, .sin_port = htons(7781) };
    CHECK("hostlisten_not_in_ns: host wildcard listener on 7781",
          !bind(hl, (struct sockaddr *)&ha, sizeof ha) && !listen(hl, 4));
    pid_t c = spawn_ns(idle_body);
    int nsfd = netns_fd_of(c);
    int e = veth_new(s, "vethH", "eth0", nsfd);
    close(nsfd);
    int ia = link_get(s, "vethH", NULL, NULL);
    CHECK("hostlisten_not_in_ns: pair created", e == 0 && ia > 0);
    addr_add(s, ia, "10.99.2.1", 24); link_set_up(s, ia);
    int ns = netns_fd_of(c);
    pid_t h = fork();
    if (h == 0) {
        if (setns(ns, CLONE_NEWNET)) _exit(1);
        int cs = nl_sock();
        int idx = link_get(cs, "eth0", NULL, NULL);
        if (!idx || addr_add(cs, idx, "10.99.2.2", 24) || link_set_up(cs, idx)) _exit(2);
        int lo = link_get(cs, "lo", NULL, NULL); link_set_up(cs, lo);
        int t = socket(AF_INET, SOCK_STREAM, 0);
        struct timeval tv = { 5, 0 };
        setsockopt(t, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
        setsockopt(t, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
        struct sockaddr_in to = { .sin_family = AF_INET, .sin_port = htons(7781) };
        inet_pton(AF_INET, "10.99.2.2", &to.sin_addr);
        if (connect(t, (struct sockaddr *)&to, sizeof to) == 0) _exit(5);
        _exit(errno == ECONNREFUSED ? 0 : 6);
    }
    int rc = wait_child(h);
    CHECK("hostlisten_not_in_ns: connect to the netns's own address is refused", rc == 0);
    int fl = fcntl(hl, F_GETFL); fcntl(hl, F_SETFL, fl | O_NONBLOCK);
    int a = accept(hl, NULL, NULL);
    CHECK("hostlisten_not_in_ns: the host listener saw no connection", a < 0 && (errno == EAGAIN || errno == EWOULDBLOCK));
    if (a >= 0) close(a);
    close(ns); close(hl);
    kill(c, SIGKILL); wait_child(c);
    close(s);
}

int main(int argc, char **argv) {
    const char *only = NULL;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-h") && i + 1 < argc) g_http = argv[++i];
        else only = argv[i];
    }
    if (!only || !strcmp(only, "veth_pair")) veth_pair();
    if (!only || !strcmp(only, "accept_ns")) accept_ns();
    if (!only || !strcmp(only, "veth_tcp_reap")) veth_tcp_reap();
    if (!only || !strcmp(only, "veth_listen_reap")) veth_listen_reap();
    if (!only || !strcmp(only, "veth_ping")) veth_ping();
    if (!only || !strcmp(only, "veth_dellink")) veth_dellink();
    if (!only || !strcmp(only, "veth_naming")) veth_naming();
    if (!only || !strcmp(only, "veth_nsgone")) veth_nsgone();
    if (!only || !strcmp(only, "bridge_l2")) bridge_l2();
    if (!only || !strcmp(only, "fwd_udp")) fwd_udp();
    if (!only || !strcmp(only, "fwd_ping")) fwd_ping();
    if (!only || !strcmp(only, "fwd_off")) fwd_off();
    if (!only || !strcmp(only, "fwd_tcp")) fwd_tcp();
    if (!only || !strcmp(only, "fwd_port_reserved")) fwd_port_reserved();
    if (!only || !strcmp(only, "fwd_reap")) fwd_reap();
    if (!only || !strcmp(only, "fwd_hostlisten")) fwd_hostlisten();
    if (!only || !strcmp(only, "hostlisten_not_in_ns")) hostlisten_not_in_ns();
    printf("brtest: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
