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
static int tcp_bulk;   /* serve a BULK_BYTES stream instead of an echo */
#define BULK_BYTES (192 * 1024)
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
        if (tcp_bulk) {
            static char blk[4096];
            memset(blk, 'z', sizeof blk);
            char go; if (read(c, &go, 1) != 1) return 4;
            for (int sent = 0; sent < BULK_BYTES; ) {
                ssize_t w = write(c, blk, sizeof blk);
                if (w <= 0) return 5;
                sent += (int)w;
            }
            close(c);
            continue;
        }
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

/* ---- veth_setlink_move: RTM_SETLINK + IFLA_NET_NS_FD moves an existing link --------------- */
static int setlink_move_body(void) {
    int s = nl_sock();
    return link_get(s, "vsmB", NULL, NULL) ? 0 : 1;   /* arrived in our netns */
}
static void veth_setlink_move(void) {
    int s = nl_sock();
    CHECK("veth_setlink_move: veth pair created in the host netns", veth_new(s, "vsmA", "vsmB", -1) == 0);
    pid_t c = spawn_ns(setlink_move_body);
    CHECK("veth_setlink_move: netns spawned", c > 0);
    if (c <= 0) { link_del(s, link_get(s, "vsmA", NULL, NULL)); close(s); return; }
    int fc = netns_fd_of(c);
    int idx = link_get(s, "vsmB", NULL, NULL);
    struct ifinfomsg i; memset(&i, 0, sizeof i); i.ifi_index = idx;
    struct nlb m; nl_begin(&m, RTM_SETLINK, 0, &i, sizeof i);
    nl_attr(&m, IFLA_NET_NS_FD, &fc, 4);
    CHECK("veth_setlink_move: SETLINK with IFLA_NET_NS_FD accepted", nl_do(s, &m) == 0);
    close(fc);
    CHECK("veth_setlink_move: link left the host netns", link_get(s, "vsmB", NULL, NULL) == 0);
    char ch = 'g'; write(ctl[1], &ch, 1);
    CHECK("veth_setlink_move: link present in the target netns", wait_child(c) == 0);
    int a = link_get(s, "vsmA", NULL, NULL);
    if (a) link_del(s, a);
    close(s);
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

/* The netns child writes BULK_BYTES, closes and exits at once; the unsent/unread tail must survive the
 * reaping of its netns (the dead stacks are kept until their TCP data drains). Repeated: loss was racy. */
static void veth_tcp_bulk_exit(void) {
    int s = nl_sock();
    int bad = 0, rounds = 6;
    for (int r = 0; r < rounds; r++) {
        tcp_bulk = 1;
        pid_t c = tcp_ns_start(s, "vethK", 97, 1);
        tcp_bulk = 0;
        if (c <= 0) { bad++; break; }
        int t = socket(AF_INET, SOCK_STREAM, 0);
        struct timeval tv = { 5, 0 };
        setsockopt(t, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
        struct sockaddr_in to = { .sin_family = AF_INET, .sin_port = htons(7777) };
        inet_pton(AF_INET, "10.99.97.2", &to.sin_addr);
        long got = 0;
        if (connect(t, (struct sockaddr *)&to, sizeof to) == 0 && write(t, "x", 1) == 1) {
            static char rb[8192];
            ssize_t n;
            while ((n = read(t, rb, sizeof rb)) > 0) got += n;
        }
        close(t);
        if (got != BULK_BYTES) { printf("  round %d: got %ld of %d\n", r, got, BULK_BYTES); bad++; }
        wait_child(c);
        if (!gone_within(s, "vethK", 5000)) bad++;
    }
    CHECK("veth_tcp_bulk_exit: every round delivered all bytes after the writer exited", bad == 0);
    close(s);
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


static int addr6_add(int s, int idx, const char *ip, int plen) {
    struct ifaddrmsg a; memset(&a, 0, sizeof a);
    a.ifa_family = AF_INET6; a.ifa_prefixlen = plen; a.ifa_index = idx;
    struct in6_addr in; inet_pton(AF_INET6, ip, &in);
    struct nlb m; nl_begin(&m, RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, &a, sizeof a);
    nl_attr(&m, IFA_LOCAL, &in, 16); nl_attr(&m, IFA_ADDRESS, &in, 16);
    unsigned nodad = 0x02; /* IFA_F_NODAD: these cases test forwarding, not RFC 4862 (tuntest dad_*) */
    nl_attr(&m, 8 /* IFA_FLAGS */, &nodad, 4);
    return nl_do(s, &m);
}
/* ICMPv6 echo through an unprivileged ping socket; 0 on a reply. */
static int ping6_once(const char *dst, int ifidx) {
    int ic = socket(AF_INET6, SOCK_DGRAM, IPPROTO_ICMPV6);
    if (ic < 0) return 1;
    struct timeval tv = { 5, 0 };
    setsockopt(ic, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    struct sockaddr_in6 to; memset(&to, 0, sizeof to); to.sin6_family = AF_INET6;
    inet_pton(AF_INET6, dst, &to.sin6_addr); to.sin6_scope_id = ifidx;
    unsigned char pk[8] = { 128, 0, 0, 0, 0x12, 0x34, 0, 1 };
    if (sendto(ic, pk, 8, 0, (struct sockaddr *)&to, sizeof to) != 8) { close(ic); return 2; }
    unsigned char rb[64];
    ssize_t n = recv(ic, rb, sizeof rb, 0);
    close(ic);
    return n >= 8 && rb[0] == 129 ? 0 : 3;
}
static int tcp6_echo_srv(const char *bindip, int port, int notify) {
    int l = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 a; memset(&a, 0, sizeof a); a.sin6_family = AF_INET6; a.sin6_port = htons(port);
    inet_pton(AF_INET6, bindip, &a.sin6_addr);
    struct timeval tv = { 8, 0 };
    setsockopt(l, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    if (bind(l, (struct sockaddr *)&a, sizeof a) || listen(l, 2)) return 1;
    if (notify >= 0) { char ch = 'x'; write(notify, &ch, 1); }
    int c = accept(l, NULL, NULL);
    if (c < 0) return 2;
    char b[8]; ssize_t n = read(c, b, 8);
    if (n != 4) return 3;
    return write(c, b, 4) == 4 ? 0 : 4;
}
static int v6_child_body(void) {
    int cs = nl_sock();
    int idx = link_get(cs, "eth0", NULL, NULL);
    if (!idx || addr6_add(cs, idx, "fd00:6::2", 64) || link_set_up(cs, idx)) return 2;
    link_set_up(cs, link_get(cs, "lo", NULL, NULL));
    int r = tcp6_echo_srv("fd00:6::2", 47661, done_[1]);
    if (r) return 10 + r;
    return ping6_once("fd00:6::1", 0) ? 20 : 0;
}

static void veth6(void) {
    int s = nl_sock();
    pid_t c = spawn_ns(v6_child_body);
    int nsfd = netns_fd_of(c);
    int e = veth_new(s, "veth6A", "eth0", nsfd);
    close(nsfd);
    CHECK("veth6: pair created", e == 0);
    int ia = link_get(s, "veth6A", NULL, NULL);
    int ar = ia > 0 ? addr6_add(s, ia, "fd00:6::1", 64) : -1;
    errno = ar;
    CHECK("veth6: host end gets fd00:6::1/64", ar == 0);
    link_set_up(s, ia);
    write(ctl[1], "g", 1);
    char ch;
    CHECK("veth6: child configured", rd_to(done_[0], &ch) == 1);
    CHECK("veth6: host pings child fd00:6::2", ping6_once("fd00:6::2", 0) == 0);
    int t = socket(AF_INET6, SOCK_STREAM, 0);
    struct timeval tv = { 5, 0 };
    setsockopt(t, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    struct sockaddr_in6 d; memset(&d, 0, sizeof d); d.sin6_family = AF_INET6; d.sin6_port = htons(47661);
    inet_pton(AF_INET6, "fd00:6::2", &d.sin6_addr);
    int ce = connect(t, (struct sockaddr *)&d, sizeof d);
    CHECK("veth6: TCP connect to the child over v6", ce == 0);
    char b[8] = "ping";
    int ok = ce == 0 && write(t, b, 4) == 4 && read(t, b, 4) == 4 && !memcmp(b, "ping", 4);
    CHECK("veth6: TCP echo over v6", ok);
    close(t);
    CHECK("veth6: child served and pinged the host back", wait_child(c) == 0);
    close(s);
}

/* ---- bridge6 / veth6gw: IPv6 over a bridge (33:33 multicast flooding) and netns gateway routes ---- */
static const char *g_ip6;
static int v6_conf_eth0(void) {
    int cs = nl_sock();
    int idx = link_get(cs, "eth0", NULL, NULL);
    if (!idx || addr6_add(cs, idx, g_ip6, 64) || link_set_up(cs, idx)) return 2;
    link_set_up(cs, link_get(cs, "lo", NULL, NULL));
    close(cs);
    return 0;
}
static int br6_server_body(void) {
    int e = v6_conf_eth0(); if (e) return e;
    int r = tcp6_echo_srv(g_ip6, 47662, done_[1]);
    if (r) return 10 + r;
    return ping6_once("fd00:66::1", 0) ? 20 : 0;
}
static int br6_client_body(void) {
    int e = v6_conf_eth0(); if (e) return e;
    char ch = 'r'; write(done_[1], &ch, 1);
    if (read(ctl[0], &ch, 1) != 1) return 3;
    if (ping6_once("fd00:66::2", 0)) return 4;
    if (ping6_once("fd00:66::1", 0)) return 5;
    int t = socket(AF_INET6, SOCK_STREAM, 0);
    struct timeval tv = { 5, 0 };
    setsockopt(t, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    struct sockaddr_in6 d; memset(&d, 0, sizeof d); d.sin6_family = AF_INET6; d.sin6_port = htons(47662);
    inet_pton(AF_INET6, "fd00:66::2", &d.sin6_addr);
    if (connect(t, (struct sockaddr *)&d, sizeof d)) return 6;
    char b[8] = "ping";
    return write(t, b, 4) == 4 && read(t, b, 4) == 4 && !memcmp(b, "ping", 4) ? 0 : 7;
}
static void bridge6(void) {
    int s = nl_sock();
    unsigned char brmac[6] = { 0x02, 0x00, 0x5e, 0x98, 0x06, 0x01 };
    CHECK("bridge6: br6 created", br_new(s, "br6", brmac) == 0);
    int bidx = link_get(s, "br6", NULL, NULL);
    CHECK("bridge6: br6 fd00:66::1/64 + up", bidx > 0 && addr6_add(s, bidx, "fd00:66::1", 64) == 0 && link_set_up(s, bidx) == 0);
    g_ip6 = "fd00:66::2";
    pid_t ca = spawn_ns(br6_server_body);
    int cA[2] = { ctl[0], ctl[1] }, dA[2] = { done_[0], done_[1] };
    g_ip6 = "fd00:66::3";
    pid_t cb = spawn_ns(br6_client_body);
    int cB[2] = { ctl[0], ctl[1] }, dB[2] = { done_[0], done_[1] };
    CHECK("bridge6: netns A and B spawned", ca > 0 && cb > 0);
    if (ca <= 0 || cb <= 0) return;
    int fa = netns_fd_of(ca), fb = netns_fd_of(cb);
    CHECK("bridge6: veth ports created", veth_new_m(s, "v6brA", "eth0", fa, bidx) == 0 && veth_new_m(s, "v6brB", "eth0", fb, bidx) == 0);
    close(fa); close(fb);
    link_set_up(s, link_get(s, "v6brA", NULL, NULL)); link_set_up(s, link_get(s, "v6brB", NULL, NULL));
    char ch = 'g';
    write(cA[1], &ch, 1); write(cB[1], &ch, 1);
    CHECK("bridge6: A listening", rd_to(dA[0], &ch) == 1);
    CHECK("bridge6: B configured", rd_to(dB[0], &ch) == 1);
    CHECK("bridge6: host pings A over the bridge", ping6_once("fd00:66::2", 0) == 0);
    write(cB[1], &ch, 1);
    int rb = wait_child(cb);
    printf("B exit %d\n", rb);
    CHECK("bridge6: B: ping A, ping bridge addr, TCP echo with A", rb == 0);
    CHECK("bridge6: A served B and pinged the host back", wait_child(ca) == 0);
    int ia = link_get(s, "v6brA", NULL, NULL), ib = link_get(s, "v6brB", NULL, NULL);
    if (ia) link_del(s, ia);
    if (ib) link_del(s, ib);
    if (bidx) link_del(s, bidx);
    close(s);
}

static int route6_default(int s, int oif, const char *gw) {
    struct rtmsg r; memset(&r, 0, sizeof r);
    r.rtm_family = AF_INET6; r.rtm_table = RT_TABLE_MAIN; r.rtm_protocol = RTPROT_BOOT;
    r.rtm_scope = RT_SCOPE_UNIVERSE; r.rtm_type = RTN_UNICAST;
    struct in6_addr g; inet_pton(AF_INET6, gw, &g);
    struct nlb m; nl_begin(&m, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, &r, sizeof r);
    nl_attr(&m, RTA_GATEWAY, &g, 16); nl_attr(&m, RTA_OIF, &oif, 4);
    return nl_do(s, &m);
}
static int gw6_child_body(void) {
    g_ip6 = "fd00:7::2";
    int e = v6_conf_eth0(); if (e) return e;
    int cs = nl_sock();
    if (ping6_once("fd00:9::1", 0) == 0) return 3;            /* no route yet: must fail */
    int idx = link_get(cs, "eth0", NULL, NULL);
    errno = route6_default(cs, idx, "fd00:7::1");
    if (errno) return 4;
    if (ping6_once("fd00:9::1", 0)) return 5;                 /* off-link host address through the gateway */
    close(cs);
    return 0;
}
static void veth6gw(void) {
    int s = nl_sock();
    pid_t c = spawn_ns(gw6_child_body);
    int nsfd = netns_fd_of(c);
    CHECK("veth6gw: pair created", veth_new(s, "v6gwA", "eth0", nsfd) == 0);
    close(nsfd);
    int ia = link_get(s, "v6gwA", NULL, NULL);
    CHECK("veth6gw: host end fd00:7::1/64 + fd00:9::1/128", ia > 0 && addr6_add(s, ia, "fd00:7::1", 64) == 0 && addr6_add(s, ia, "fd00:9::1", 128) == 0);
    link_set_up(s, ia);
    write(ctl[1], "g", 1);
    CHECK("veth6gw: child: no route fails, default via gw works", wait_child(c) == 0);
    if (ia) link_del(s, ia);
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
    /* UDP to a server bound to the bridge address itself (aardvark-dns on 10.89.0.1:53) */
    int hu = socket(AF_INET, SOCK_DGRAM, 0);
    hh.sin_port = htons(7779);
    setsockopt(hu, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    if (sendto(hu, "q-udp", 5, 0, (struct sockaddr *)&hh, sizeof hh) != 5) return 19;
    memset(b, 0, sizeof b);
    if (recv(hu, b, sizeof b, 0) != 5 || memcmp(b, "a-udp", 5)) return 20;
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
    { int one = 1; setsockopt(hl, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one); }
    struct sockaddr_in ha = { .sin_family = AF_INET, .sin_port = htons(7778) };
    CHECK("bridge_l2: host wildcard listener", !bind(hl, (struct sockaddr *)&ha, sizeof ha) && !listen(hl, 4));
    /* host UDP server bound to the bridge address (not the wildcard) */
    int hu = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in hua = { .sin_family = AF_INET, .sin_port = htons(7779) };
    inet_pton(AF_INET, "10.98.0.1", &hua.sin_addr);
    CHECK("bridge_l2: host UDP bound to the bridge address", !bind(hu, (struct sockaddr *)&hua, sizeof hua));
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
    {
        struct pollfd pu = { hu, POLLIN, 0 };
        char ub[32] = {0};
        struct sockaddr_in from; socklen_t fl = sizeof from;
        int ug = 0;
        if (poll(&pu, 1, 10000) > 0 && recvfrom(hu, ub, sizeof ub, 0, (struct sockaddr *)&from, &fl) == 5
            && !memcmp(ub, "q-udp", 5)) { sendto(hu, "a-udp", 5, 0, (struct sockaddr *)&from, fl); ug = 1; }
        CHECK("bridge_l2: B's UDP reached the server bound to 10.98.0.1", ug);
    }
    close(hu);
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
static const unsigned char VICTIM_MAC[6] = { 0x02, 0x00, 0x5e, 0x97, 0x00, 0xaa };
static int g_fwd_mode; /* 0 = UDP DNS query, 1 = ping the QEMU gateway; expect success */
static int g_fwd_expect = 1;
static const char *g_fwd_nft;   /* nft ruleset loaded before the container starts (nftnat_*) */
static int g_fwd_expect_ovr = -1;
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
static long mono_ms(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1000L + t.tv_nsec / 1000000L; }
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
    long t0 = mono_ms();
    for (int tries = 0; tries < 3 && !ok; tries++) {
        long ts = mono_ms();
        if (sendto(u, q, sizeof q, 0, (struct sockaddr *)&to, sizeof to) != (ssize_t)sizeof q) { printf("DETAIL dns try %d sendto errno %d\n", tries, errno); break; }
        ssize_t n = recv(u, rb, sizeof rb, 0);
        int e = errno;
        ok = n >= 12 && rb[0] == 0x4c && rb[1] == 0x45 && (rb[2] & 0x80);
        if (!ok) printf("DETAIL dns %s try %d: recv=%zd errno=%d after %ld ms (rcode/flags %02x%02x)\n", srv, tries, n, n < 0 ? e : 0, mono_ms() - ts, n >= 4 ? rb[2] : 0, n >= 4 ? rb[3] : 0);
    }
    if (ok) printf("DETAIL dns %s answered after %ld ms\n", srv, mono_ms() - t0);
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
static int tcp_dns_fd(int fd) {
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
    return ok;
}
static int tcp_dns_c(const char *srv) {
    int fd = tcp_conn(srv, 53);
    if (fd < 0) return 0;
    int ok = tcp_dns_fd(fd);
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
/* First live TCP NAT port the kernel reports (/proc/net/nic "fwd_nat_tcp:"); 0 if none.
 * Asked of the kernel, not probed with bind(): TIME_WAIT ports from earlier cases or
 * suites are not NAT-held and must not count. */
static int nat_scan(void) {
    FILE *f = fopen("/proc/net/nic", "r");
    if (!f) return 0;
    char line[1024]; int port = 0;
    while (fgets(line, sizeof line, f)) {
        if (strncmp(line, "fwd_nat_tcp:", 12)) continue;
        port = atoi(line + 12);
        break;
    }
    fclose(f);
    return port;
}
/* Failure diagnostic: print the kernel's live TCP NAT entries (fwd_nat_ent lines of /proc/net/nic). */
static void nat_dump(void) {
    FILE *f = fopen("/proc/net/nic", "r");
    if (!f) return;
    char line[1024];
    while (fgets(line, sizeof line, f))
        if (!strncmp(line, "fwd_nat_ent:", 12) || !strncmp(line, "fwd:", 4) || !strncmp(line, "fwd_stats:", 10)
            || !strncmp(line, "fwd_arp:", 8) || !strncmp(line, "fwd_pend:", 9) || !strncmp(line, "fwd_ct:", 7)) printf("DIAG %s", line);
    fclose(f);
}
/* The same lines as nat_dump(), collected into buf (only the fwd_stats line when it is long). */
static void nat_snap(char *buf, size_t n) {
    FILE *f = fopen("/proc/net/nic", "r");
    size_t o = 0;
    if (!f) return;
    char line[1024];
    while (fgets(line, sizeof line, f))
        if (!strncmp(line, "fwd_stats:", 10) || !strncmp(line, "fwd:", 4)) {
            size_t l = strlen(line);
            if (o + l + 1 < n) { memcpy(buf + o, line, l + 1); o += l; }
        }
    fclose(f);
}
/* A host bind() to `p` must fail with EADDRINUSE. */
static int nat_inuse(int p) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(p) };
    int r = bind(fd, (struct sockaddr *)&a, sizeof a), e = errno;
    close(fd);
    return r && e == EADDRINUSE;
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
    if (g_fwd_mode >= 8) link_set_mac(s, idx, VICTIM_MAC);
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
    else if (g_fwd_mode >= 8) { /* hold a flow; after the attacker acted, the same connection must still work */
        char srv[64]; fwd_resolver(srv);
        int fd = tcp_conn(srv, 53);
        if (fd < 0) return 9;
        write(done_[1], &ch, 1);
        rd_to(ctl[0], &ch);
        ok = tcp_dns_fd(fd);
        close(fd);
    }
    else if (g_fwd_mode == 3) { /* UDP to a closed port beyond the NAT: the ICMP port-unreachable must come back */
        char gw[32]; fwd_gateway(gw);
        int u = socket(AF_INET, SOCK_DGRAM, 0);
        struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(9) };
        inet_pton(AF_INET, gw, &a.sin_addr);
        struct timeval tv = { 3, 0 };
        setsockopt(u, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
        ok = 0;
        if (u >= 0 && !connect(u, (struct sockaddr *)&a, sizeof a)) {
            char b[8];
            for (int k = 0; k < 3 && !ok; k++) {
                long ts = mono_ms();
                ssize_t sn = send(u, "x", 1, 0), se = errno;
                ssize_t rn = recv(u, b, sizeof b, 0);
                int e = errno;
                if (rn < 0 && e == ECONNREFUSED) ok = 1;
                else printf("DETAIL icmperr %s:9 try %d: send=%zd(errno %zd) recv=%zd errno=%d after %ld ms\n", gw, k, sn, sn < 0 ? se : 0, rn, rn < 0 ? e : 0, mono_ms() - ts);
            }
        }
        if (u >= 0) close(u);
    }
    else if (g_fwd_mode == 6) { /* TCP to the bridge's own address: a host 0.0.0.0 listener, with forwarding on */
        int fd = tcp_conn("10.97.0.1", 7779);
        ok = fd >= 0;
        if (fd >= 0) close(fd);
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
    else { char gw[32]; fwd_gateway(gw); ok = icmp_ping(gw); }
    return ok == g_fwd_expect ? 0 : 4 + ok;
}
/* Attacker netns: takes the victim's MAC and the victim-network address, then pings the bridge to get learned. */
static int spoof_body(void) {
    int s = nl_sock();
    int idx = link_get(s, "eth0", NULL, NULL);
    if (!idx || link_set_mac(s, idx, VICTIM_MAC) || addr_add(s, idx, "10.97.0.3", 24) || link_set_up(s, idx)) return 1;
    int ok = icmp_ping("10.97.0.1");
    char ch = 'r'; write(done_[1], &ch, 1);
    return ok ? 0 : 2;
}
static int ip_forward_set(int on) {
    int fd = open("/proc/sys/net/ipv4/ip_forward", O_WRONLY);
    if (fd < 0) return -1;
    int r = write(fd, on ? "1\n" : "0\n", 2);
    close(fd);
    return r == 2 ? 0 : -1;
}
static const char *g_fwd_msg; /* overrides the per-mode check text */
static int g_early_listen; /* mode 6: bind the host listener before br0 exists (httpd before podman0) */
static void fwd_case(const char *name, int mode, int ipfwd) {
    char msg[96];
    int hl = -1;
    if (mode == 6 && g_early_listen) {
        hl = socket(AF_INET, SOCK_STREAM, 0);
    { int one = 1; setsockopt(hl, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one); }
        struct sockaddr_in ha = { .sin_family = AF_INET, .sin_port = htons(7779) };
        snprintf(msg, sizeof msg, "%s: host wildcard listener on 7779 (before br0)", name);
        CHECK(msg, !bind(hl, (struct sockaddr *)&ha, sizeof ha) && !listen(hl, 4));
    }
    int s = nl_sock();
    unsigned char brmac[6] = { 0x02, 0x00, 0x5e, 0x97, 0x00, 0x01 };
    int ok = br_new(s, "br0", brmac) == 0;
    int bidx = link_get(s, "br0", NULL, NULL);
    ok = ok && bidx > 0 && addr_add(s, bidx, "10.97.0.1", 24) == 0 && link_set_up(s, bidx) == 0;
    snprintf(msg, sizeof msg, "%s: br0 10.97.0.1/24 up", name); CHECK(msg, ok);
    snprintf(msg, sizeof msg, "%s: ip_forward=%d", name, ipfwd); CHECK(msg, ip_forward_set(ipfwd) == 0);
    g_fwd_mode = mode; g_fwd_expect = g_fwd_expect_ovr >= 0 ? g_fwd_expect_ovr : ipfwd;
    if (g_fwd_nft) {
        FILE *nf = fopen("/tmp/fn.nft", "w"); if (nf) { fputs(g_fwd_nft, nf); fclose(nf); }
        snprintf(msg, sizeof msg, "%s: nft -f loads the ruleset", name);
        CHECK(msg, system("nft -f /tmp/fn.nft >/tmp/fn.err 2>&1") == 0);
    }
    if (mode == 6 && !g_early_listen) {
        hl = socket(AF_INET, SOCK_STREAM, 0);
    { int one = 1; setsockopt(hl, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one); }
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
    /* The precondition must be sampled BEFORE "go": once the child is released its own resolver flow
     * can be established before the parent looks (seen on slow TCG: the only entry was the case's own
     * 10.97.0.2 -> resolver flow, age 0), which is not a leak. */
    if (mode >= 4 && mode != 6) {
        snprintf(msg, sizeof msg, "%s: precondition: no NAT port is held yet", name);
        { int held = nat_scan(); if (held) nat_dump(); CHECK(msg, held == 0); }
    }
    char pre[1536]; pre[0] = 0;
    if (mode < 4) nat_snap(pre, sizeof pre); /* counters before the flow, printed with the failure dump to diff against */
    if (rd == 1) write(cc[1], &ch, 1); /* go */
    int natp = 0;
    if (mode >= 4 && mode != 6) {
        rd = rd_to(dd[0], &ch);
        snprintf(msg, sizeof msg, "%s: TCP flow to the resolver established through NAT", name); CHECK(msg, rd == 1);
        natp = nat_scan();
        if (mode == 4) {
            snprintf(msg, sizeof msg, "%s: host bind() to the in-use NAT port gets EADDRINUSE (port %d)", name, natp);
            CHECK(msg, natp > 0 && nat_inuse(natp));
            write(cc[1], &ch, 1);
        } else if (mode == 10) {
            /* a filter rule added while the flow is established must bite its next packets */
            FILE *nf = fopen("/tmp/fn.nft", "w");
            if (nf) { fputs("table inet midflow {\n chain FORWARD { type filter hook forward priority filter; policy accept;\n  ct state established drop\n }\n}\n", nf); fclose(nf); }
            snprintf(msg, sizeof msg, "%s: nft -f adds `ct state established drop` mid-flow", name);
            CHECK(msg, system("nft -f /tmp/fn.nft >/tmp/fn.err 2>&1") == 0);
            usleep(200000);
            write(cc[1], &ch, 1);
        } else if (mode >= 8) {
            /* attacker B: own netns, veth with the victim's MAC, then act (6: delete it, 7: join br0 and get learned) */
            int bc[2] = { ctl[0], ctl[1] };
            (void)bc;
            pid_t b = spawn_ns(mode == 9 ? spoof_body : idle_body);
            int bcc[2] = { ctl[0], ctl[1] }, bdd[2] = { done_[0], done_[1] };
            snprintf(msg, sizeof msg, "%s: attacker netns spawned", name); CHECK(msg, b > 0);
            int fb = b > 0 ? netns_fd_of(b) : -1;
            int mst = mode == 9 ? bidx : 0;
            int vr = veth_new_m(s, "vethB", "eth0", fb, mst);
            if (fb >= 0) close(fb);
            int ib = link_get(s, "vethB", NULL, NULL);
            snprintf(msg, sizeof msg, "%s: attacker veth created", name); CHECK(msg, vr == 0 && ib > 0);
            if (mode == 8) {
                link_set_mac(s, ib, VICTIM_MAC);
                link_del(s, ib);
                snprintf(msg, sizeof msg, "%s: attacker veth deleted", name); CHECK(msg, gone_within(s, "vethB", 3000));
                usleep(300000);
            } else {
                link_set_up(s, ib);
                write(bcc[1], &ch, 1);
                snprintf(msg, sizeof msg, "%s: attacker (victim MAC) pinged br0", name); CHECK(msg, rd_to(bdd[0], &ch) == 1);
                usleep(200000);
            }
            write(cc[1], &ch, 1);   /* victim: query on its held flow */
            int brc = 0;
            if (b > 0) { kill(b, SIGKILL); brc = wait_child(b); }
            (void)brc;
        } else kill(c, SIGKILL);
    }
    int rc = wait_child(c);
    if (mode == 5) {
        /* netns teardown (veth/vif forget) is asynchronous; on a loaded TCG box it takes longer than 0.5 s */
        for (int i = 0; i < 100 && !(natp > 0 && nat_bindable(natp) && nat_scan() == 0); i++) usleep(100000);
        snprintf(msg, sizeof msg, "%s: after the netns died its NAT port %d is released", name, natp);
        { int rel = natp > 0 && nat_bindable(natp); if (!rel) nat_dump(); CHECK(msg, rel); }
        snprintf(msg, sizeof msg, "%s: no NAT port stays reserved", name); CHECK(msg, nat_scan() == 0);
        rc = 0;
    }
    printf("%s child exit %d\n", name, rc);
    if (g_fwd_msg) snprintf(msg, sizeof msg, "%s: %s", name, g_fwd_msg);
    else if (g_fwd_nft) snprintf(msg, sizeof msg, "%s: %s", name, g_fwd_expect ? "UDP DNS query is answered: the subnet has a masquerade rule" : "UDP DNS query gets no reply: no masquerade rule for the subnet (internal)");
    else if (mode == 6) snprintf(msg, sizeof msg, "%s: connect to the bridge address reaches the host 0.0.0.0 listener", name);
    else if (mode == 10) snprintf(msg, sizeof msg, "%s: established TCP flow is dropped by a rule added mid-flow (FORWARD sees established packets)", name);
    else if (mode == 8) snprintf(msg, sizeof msg, "%s: victim flow survives another netns deleting a link with its MAC", name);
    else if (mode == 9) snprintf(msg, sizeof msg, "%s: spoofed MAC does not steal the victim's return traffic", name);
    else if (mode == 2) snprintf(msg, sizeof msg, "%s: TCP DNS query to the resolver%s is answered through NAT", name, g_http ? " and an HTTP GET" : "");
    else if (mode >= 4) snprintf(msg, sizeof msg, "%s: flow holder exited cleanly", name);
    else if (mode == 0 && ipfwd) snprintf(msg, sizeof msg, "%s: UDP DNS query to the resolver is answered through NAT", name);
    else if (mode == 3 && ipfwd) snprintf(msg, sizeof msg, "%s: ICMP port-unreachable for a NATed UDP flow reaches the container (ECONNREFUSED)", name);
    else if (mode == 1 && ipfwd) snprintf(msg, sizeof msg, "%s: ICMP echo to the default gateway is answered through NAT", name);
    else snprintf(msg, sizeof msg, "%s: ip_forward=0 means no reply (nothing forwarded)", name);
    if (rc != 0 && mode < 4) {
        printf("DIAG %s failed (child rc %d); counters BEFORE the flow:\n%s", name, rc, pre[0] ? pre : "(none)\n");
        printf("DIAG counters AFTER (conntrack/ARP/pending live now):\n");
        nat_dump();
    }
    CHECK(msg, rc == 0);
    ip_forward_set(0);
    if (g_fwd_nft) system("nft delete table inet netavark");
    if (mode == 10) system("nft delete table inet midflow");
    if (hl >= 0) close(hl);
    if (iv > 0) link_del(s, iv);
    link_del(s, bidx);
    close(s);
}


/* ---- routed forwarding without NAT: bridge A -> host (ip_forward) -> bridge B -------------------------------- */
static int g_rt_role; /* 0 client in 10.97.0.0/24, 1 UDP server in 10.98.0.0/24 */
static int g_rt_expect; /* client: 1 = reply with the untranslated source expected */
static int rt_body(void) {
    const char *ip = g_rt_role ? "10.98.0.2" : "10.97.0.2", *gw = g_rt_role ? "10.98.0.1" : "10.97.0.1";
    int s = nl_sock();
    int idx = link_get(s, "eth0", NULL, NULL);
    if (!idx || addr_add(s, idx, ip, 24) || link_set_up(s, idx)) return 1;
    int lo = link_get(s, "lo", NULL, NULL); link_set_up(s, lo);
    if (route_default(s, idx, gw)) return 2;
    close(s);
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in me = { .sin_family = AF_INET, .sin_port = htons(7000) };
    if (g_rt_role) { inet_pton(AF_INET, ip, &me.sin_addr); if (bind(u, (struct sockaddr *)&me, sizeof me)) return 6; }
    char ch = 'r'; write(done_[1], &ch, 1);
    if (g_rt_role) {
        for (int i = 0; i < 40; i++) {
            struct pollfd p = { u, POLLIN, 0 };
            if (poll(&p, 1, 1000) <= 0) continue;
            char b[64]; struct sockaddr_in from; socklen_t fl = sizeof from;
            if (recvfrom(u, b, sizeof b, 0, (struct sockaddr *)&from, &fl) <= 0) continue;
            char r[64]; char fa[32]; inet_ntop(AF_INET, &from.sin_addr, fa, sizeof fa);
            int n = snprintf(r, sizeof r, "from=%s", fa);
            sendto(u, r, n, 0, (struct sockaddr *)&from, fl);
        }
        return 0;
    }
    if (rd_to(ctl[0], &ch) != 1) return 3;
    struct timeval tv = { 2, 0 };
    setsockopt(u, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    struct sockaddr_in to = { .sin_family = AF_INET, .sin_port = htons(7000) };
    inet_pton(AF_INET, "10.98.0.2", &to.sin_addr);
    int got = 0;
    for (int k = 0; k < 6 && !got; k++) {
        char b[64];
        sendto(u, "hi", 2, 0, (struct sockaddr *)&to, sizeof to);
        ssize_t n = recv(u, b, sizeof b - 1, 0);
        if (n > 0) { b[n] = 0; got = 1; printf("DETAIL routed reply '%s' try %d\n", b, k); if (strcmp(b, "from=10.97.0.2")) return 5; }
    }
    return got == g_rt_expect ? 0 : 4 + got;
}
static void routed_case(const char *name, int ipfwd) {
    char msg[96];
    int s = nl_sock();
    unsigned char m0[6] = { 0x02, 0x00, 0x5e, 0x97, 0x00, 0x01 }, m1[6] = { 0x02, 0x00, 0x5e, 0x98, 0x00, 0x01 };
    int ok = br_new(s, "br0", m0) == 0 && br_new(s, "br1", m1) == 0;
    int b0 = link_get(s, "br0", NULL, NULL), b1 = link_get(s, "br1", NULL, NULL);
    ok = ok && b0 > 0 && b1 > 0 && addr_add(s, b0, "10.97.0.1", 24) == 0 && link_set_up(s, b0) == 0
        && addr_add(s, b1, "10.98.0.1", 24) == 0 && link_set_up(s, b1) == 0;
    snprintf(msg, sizeof msg, "%s: br0 10.97.0.1/24 and br1 10.98.0.1/24 up", name); CHECK(msg, ok);
    snprintf(msg, sizeof msg, "%s: ip_forward=%d", name, ipfwd); CHECK(msg, ip_forward_set(ipfwd) == 0);
    g_rt_expect = ipfwd;
    char ch = 'g';
    g_rt_role = 1;
    pid_t sv = spawn_ns(rt_body);
    int scc[2] = { ctl[0], ctl[1] }, sdd[2] = { done_[0], done_[1] };
    int fc = netns_fd_of(sv);
    int vs = veth_new_m(s, "vethS", "eth0", fc, b1) == 0 && link_set_up(s, link_get(s, "vethS", NULL, NULL)) == 0;
    close(fc);
    write(scc[1], &ch, 1);
    snprintf(msg, sizeof msg, "%s: server container on br1 serves UDP 7000", name); CHECK(msg, sv > 0 && vs && rd_to(sdd[0], &ch) == 1);
    g_rt_role = 0;
    pid_t cl = spawn_ns(rt_body);
    int cc[2] = { ctl[0], ctl[1] }, dd[2] = { done_[0], done_[1] };
    fc = netns_fd_of(cl);
    int vc = veth_new_m(s, "vethC", "eth0", fc, b0) == 0 && link_set_up(s, link_get(s, "vethC", NULL, NULL)) == 0;
    close(fc);
    write(cc[1], &ch, 1);
    snprintf(msg, sizeof msg, "%s: client container on br0 configured", name); CHECK(msg, cl > 0 && vc && rd_to(dd[0], &ch) == 1);
    write(cc[1], &ch, 1);
    int rc = wait_child(cl);
    kill(sv, SIGKILL); wait_child(sv);
    if (ipfwd) snprintf(msg, sizeof msg, "%s: UDP crosses br0 -> host -> br1 routed, source address kept (no NAT)", name);
    else snprintf(msg, sizeof msg, "%s: ip_forward=0 means nothing is routed between the bridges", name);
    CHECK(msg, rc == 0);
    ip_forward_set(0);
    link_del(s, link_get(s, "vethS", NULL, NULL)); link_del(s, link_get(s, "vethC", NULL, NULL));
    link_del(s, b0); link_del(s, b1);
    close(s);
}
static void routed_on(void) { routed_case("routed_l3", 1); }
static void routed_off(void) { routed_case("routed_l3_off", 0); }

/* ---- nftnat: rulesets from netavark's nftables driver drive DNAT (podnat L2) ------------------------- */
static int g_nn_mode;      /* 0 host->127.0.0.1:H, 1 container -> bridge:H (self hairpin), 2 second container -> bridge:H */
static int g_nn_ms;        /* last nn_connect latency */
static int g_nn_role;      /* 0 server container, 1 client container */
static const char NN_RULES[] =
 "table inet netavark {\n"
 " chain FORWARD { type filter hook forward priority filter; policy accept;\n"
 "  ct state invalid drop\n"
 "  ip daddr 10.97.0.0/24 ct state established,related accept\n"
 "  ip saddr 10.97.0.0/24 accept\n }\n"
 " chain POSTROUTING { type nat hook postrouting priority srcnat; policy accept;\n"
 "  meta mark & 0x00002000 == 0x00002000 masquerade\n }\n"
 " chain PREROUTING { type nat hook prerouting priority dstnat; policy accept;\n"
 "  fib daddr type local jump NETAVARK-HOSTPORT-DNAT\n }\n"
 " chain OUTPUT { type nat hook output priority dstnat; policy accept;\n"
 "  fib daddr type local jump NETAVARK-HOSTPORT-DNAT\n }\n"
 " chain NETAVARK-HOSTPORT-DNAT { tcp dport 8080 jump nv_x_dnat\n }\n"
 " chain NETAVARK-HOSTPORT-SETMARK { meta mark set meta mark | 0x00002000\n }\n"
 " chain nv_x_dnat {\n"
 "  ip saddr 10.97.0.0/24 tcp dport 8080 jump NETAVARK-HOSTPORT-SETMARK\n"
 "  ip saddr 127.0.0.1 tcp dport 8080 jump NETAVARK-HOSTPORT-SETMARK\n"
 "  tcp dport 8080 dnat ip to 10.97.0.2:80\n }\n"
 "}\n";
static const char NN_RULES_UDP[] =
 "table inet netavark {\n"
 " chain FORWARD { type filter hook forward priority filter; policy accept;\n  ct state invalid drop\n  ip saddr 10.97.0.0/24 accept\n }\n"
 " chain POSTROUTING { type nat hook postrouting priority srcnat; policy accept;\n  meta mark & 0x00002000 == 0x00002000 masquerade\n }\n"
 " chain PREROUTING { type nat hook prerouting priority dstnat; policy accept;\n  fib daddr type local jump NETAVARK-HOSTPORT-DNAT\n }\n"
 " chain OUTPUT { type nat hook output priority dstnat; policy accept;\n  fib daddr type local jump NETAVARK-HOSTPORT-DNAT\n }\n"
 " chain NETAVARK-HOSTPORT-DNAT { ip daddr 127.0.0.1 udp dport 9090 jump nv_x_dnat\n }\n"
 " chain NETAVARK-HOSTPORT-SETMARK { meta mark set meta mark | 0x00002000\n }\n"
 " chain nv_x_dnat {\n"
 "  ip saddr 10.97.0.0/24 ip daddr 127.0.0.1 udp dport 9090 jump NETAVARK-HOSTPORT-SETMARK\n"
 "  ip saddr 127.0.0.1 ip daddr 127.0.0.1 udp dport 9090 jump NETAVARK-HOSTPORT-SETMARK\n"
 "  ip daddr 127.0.0.1 udp dport 9090 dnat ip to 10.97.0.2:90\n }\n"
 "}\n";
static const char NN_RULES_RANGE[] =
 "table inet netavark {\n"
 " chain FORWARD { type filter hook forward priority filter; policy accept;\n  ct state invalid drop\n  ip saddr 10.97.0.0/24 accept\n }\n"
 " chain POSTROUTING { type nat hook postrouting priority srcnat; policy accept;\n  meta mark & 0x00002000 == 0x00002000 masquerade\n }\n"
 " chain PREROUTING { type nat hook prerouting priority dstnat; policy accept;\n  fib daddr type local jump NETAVARK-HOSTPORT-DNAT\n }\n"
 " chain OUTPUT { type nat hook output priority dstnat; policy accept;\n  fib daddr type local jump NETAVARK-HOSTPORT-DNAT\n }\n"
 " chain NETAVARK-HOSTPORT-DNAT { tcp dport 8000-8002 jump nv_x_dnat\n }\n"
 " chain NETAVARK-HOSTPORT-SETMARK { meta mark set meta mark | 0x00002000\n }\n"
 " chain nv_x_dnat {\n"
 "  ip saddr 127.0.0.1 tcp dport 8000-8002 jump NETAVARK-HOSTPORT-SETMARK\n"
 "  tcp dport 8000 dnat ip to 10.97.0.2:8000\n  tcp dport 8001 dnat ip to 10.97.0.2:8001\n  tcp dport 8002 dnat ip to 10.97.0.2:8002\n }\n"
 "}\n";
static int nn_poll(int fd, short ev, int ms) { struct pollfd p = { fd, ev, 0 }; return poll(&p, 1, ms) > 0 && (p.revents & ev); }
static int nn_connect(const char *ip, int port) {
    int c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port) };
    inet_pton(AF_INET, ip, &a.sin_addr);
    struct timespec t0, t1; clock_gettime(CLOCK_MONOTONIC, &t0);
    connect(c, (struct sockaddr *)&a, sizeof a);
    /* the first SYN may be dropped while the bridge learns a MAC: allow 12 s (SYN retransmit at 1 s) */
    int err = -1; socklen_t el = sizeof err;
    if (nn_poll(c, POLLOUT, 12000)) getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &el);
    clock_gettime(CLOCK_MONOTONIC, &t1);
    g_nn_ms = (t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_nsec - t0.tv_nsec) / 1000000;
    if (err != 0) { close(c); return -1; }
    return c;
}
static int nn_udp_serve(void) {
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(90) };
    if (bind(u, (struct sockaddr *)&a, sizeof a)) return 10;
    char ch = 'r'; write(done_[1], &ch, 1);
    for (int i = 0; i < 2; i++) {            /* connected-socket query, then the unconnected one */
        char b[16]; struct sockaddr_in p; socklen_t pl = sizeof p;
        if (!nn_poll(u, POLLIN, 15000)) return 11;
        ssize_t n = recvfrom(u, b, sizeof b, 0, (struct sockaddr *)&p, &pl);
        if (n < 0) return 12;
        if (sendto(u, "hi", 2, 0, (struct sockaddr *)&p, pl) != 2) return 13;
    }
    return 0;
}
static int nn_serve(const char *want_peer) {
    if (g_nn_mode == 3) return nn_udp_serve();
    int l = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1; setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(g_nn_mode == 4 ? 8001 : 80) };
    if (bind(l, (struct sockaddr *)&a, sizeof a) || listen(l, 4)) return 10;
    char ch = 'r'; write(done_[1], &ch, 1);     /* listening */
    int selfc = -1;
    if (g_nn_mode == 1 || g_nn_mode == 5) selfc = nn_connect("10.97.0.1", 8080);
    if (!nn_poll(l, POLLIN, 15000)) return 11;
    struct sockaddr_in p; socklen_t pl = sizeof p;
    int c = accept(l, (struct sockaddr *)&p, &pl);
    if (c < 0) return 12;
    char got[32]; inet_ntop(AF_INET, &p.sin_addr, got, sizeof got);
    if (g_nn_mode == 5) {
        /* established hairpin flow: the parent loads `ct state established drop`; our reply must not arrive */
        char e = 'e'; write(done_[1], &e, 1);
        if (rd_to(ctl[0], &e) != 1) return 14;
        write(c, "hi", 2);
        int got_data = nn_poll(selfc, POLLIN, 2500);
        close(c); if (selfc >= 0) close(selfc);
        return got_data ? 15 : 0;
    }
    write(c, "hi", 2);
    usleep(200000);
    if (selfc >= 0) close(selfc);
    return want_peer && strcmp(got, want_peer) ? 13 : 0;
}
static int nn_body(void) {
    g_ip = g_nn_role ? "10.97.0.3" : "10.97.0.2";
    int s = nl_sock();
    int idx = link_get(s, "eth0", NULL, NULL);
    if (!idx || addr_add(s, idx, g_ip, 24) || link_set_up(s, idx)) return 1;
    int lo = link_get(s, "lo", NULL, NULL); link_set_up(s, lo);
    if (route_default(s, idx, "10.97.0.1")) return 2;
    close(s);
    char ch = 'c'; write(done_[1], &ch, 1);
    if (rd_to(ctl[0], &ch) != 1) return 3;
    if (!g_nn_role) return nn_serve(g_nn_mode == 0 ? NULL : "10.97.0.1");
    int c = nn_connect("10.97.0.1", 8080);
    if (c < 0) return 20;
    /* The parked SYN is replayed on the ARP reply; the ARP request must be kicked out at once (was a ~450 ms tick wait). */
    { char m2[96]; snprintf(m2, sizeof m2, "first connect through DNAT is fast (%d ms < 100)", g_nn_ms); CHECK(m2, g_nn_ms < 100); }
    char b[8] = { 0 };
    if (!nn_poll(c, POLLIN, 8000) || read(c, b, 2) != 2) return 21;
    return !strcmp(b, "hi") ? 0 : 22;
}
static void nn_case(const char *name, int mode) {
    char msg[120];
    int s = nl_sock();
    unsigned char brmac[6] = { 0x02, 0x00, 0x5e, 0x97, 0x00, 0x09 };
    int ok = br_new(s, "br0", brmac) == 0;
    int bidx = link_get(s, "br0", NULL, NULL);
    ok = ok && bidx > 0 && addr_add(s, bidx, "10.97.0.1", 24) == 0 && link_set_up(s, bidx) == 0 && ip_forward_set(1) == 0;
    snprintf(msg, sizeof msg, "%s: br0 10.97.0.1/24 up, ip_forward=1", name); CHECK(msg, ok);
    FILE *fp = fopen("/tmp/nn.nft", "w");
    if (fp) { fputs(mode == 3 ? NN_RULES_UDP : mode == 4 ? NN_RULES_RANGE : NN_RULES, fp); fclose(fp); }
    snprintf(msg, sizeof msg, "%s: nft -f loads the netavark-shaped ruleset", name);
    int nr = system("nft -f /tmp/nn.nft >/tmp/nn.err 2>&1");
    if (nr) { char eb[200] = ""; FILE *ef = fopen("/tmp/nn.err", "r"); if (ef) { size_t n = fread(eb, 1, 190, ef); eb[n] = 0; fclose(ef); }
        for (char *q = eb; *q; q++) if (*q == '\n') *q = ' ';
        printf("# nft rc=%d: %s\n", nr, eb); }
    CHECK(msg, nr == 0);
    g_nn_mode = mode; g_nn_role = 0;
    pid_t c = spawn_ns(nn_body);
    int cc[2] = { ctl[0], ctl[1] }, dd[2] = { done_[0], done_[1] };
    pid_t c2 = -1; int cc2[2] = { -1, -1 }, dd2[2] = { -1, -1 };
    char ch = 'g';
    int fc = c > 0 ? netns_fd_of(c) : -1;
    ok = c > 0 && veth_new_m(s, "vethN", "eth0", fc, bidx) == 0;
    if (fc >= 0) close(fc);
    int iv = link_get(s, "vethN", NULL, NULL);
    ok = ok && iv > 0 && link_set_up(s, iv) == 0;
    snprintf(msg, sizeof msg, "%s: server netns + veth", name); CHECK(msg, ok);
    if (ok) {
        write(cc[1], &ch, 1);
        ok = rd_to(dd[0], &ch) == 1;
        snprintf(msg, sizeof msg, "%s: server eth0 configured", name); CHECK(msg, ok);
        if (ok) write(cc[1], &ch, 1);
        ok = ok && rd_to(dd[0], &ch) == 1; /* listening */
    }
    if (ok && mode == 2) {
        g_nn_role = 1;
        c2 = spawn_ns(nn_body);
        cc2[0] = ctl[0]; cc2[1] = ctl[1]; dd2[0] = done_[0]; dd2[1] = done_[1];
        int f2 = c2 > 0 ? netns_fd_of(c2) : -1;
        int ok2 = c2 > 0 && veth_new_m(s, "vethM", "eth0", f2, bidx) == 0;
        if (f2 >= 0) close(f2);
        int i2 = link_get(s, "vethM", NULL, NULL);
        ok2 = ok2 && i2 > 0 && link_set_up(s, i2) == 0;
        if (ok2) { write(cc2[1], &ch, 1); ok2 = rd_to(dd2[0], &ch) == 1; }
        if (ok2) write(cc2[1], &ch, 1);
        snprintf(msg, sizeof msg, "%s: client netns up", name); CHECK(msg, ok2);
    }
    if (ok && mode == 0) {
        int h = nn_connect("127.0.0.1", 8080);
        char b[8] = { 0 };
        int got = h >= 0 && nn_poll(h, POLLIN, 8000) && read(h, b, 2) == 2 && !strcmp(b, "hi");
        snprintf(msg, sizeof msg, "%s: host connect 127.0.0.1:8080 reaches the container listener", name); CHECK(msg, got);
        struct sockaddr_in pa; socklen_t pl = sizeof pa; char ip[32] = "";
        if (h >= 0 && !getpeername(h, (struct sockaddr *)&pa, &pl)) inet_ntop(AF_INET, &pa.sin_addr, ip, sizeof ip);
        snprintf(msg, sizeof msg, "%s: getpeername keeps 127.0.0.1:8080 (got %s:%d)", name, ip, ntohs(pa.sin_port));
        CHECK(msg, !strcmp(ip, "127.0.0.1") && ntohs(pa.sin_port) == 8080);
        if (h >= 0) close(h);
    }
    if (ok && mode == 4) {
        int h = nn_connect("127.0.0.1", 8001);
        char b[8] = { 0 };
        int got = h >= 0 && nn_poll(h, POLLIN, 8000) && read(h, b, 2) == 2 && !strcmp(b, "hi");
        snprintf(msg, sizeof msg, "%s: host connect 127.0.0.1:8001 (inside 8000-8002) reaches the container listener", name); CHECK(msg, got);
        if (h >= 0) close(h);
    }
    if (ok && mode == 3) {
        char b[8] = { 0 };
        int u = socket(AF_INET, SOCK_DGRAM, 0);
        struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(9090) };
        inet_pton(AF_INET, "127.0.0.1", &a.sin_addr);
        int got = connect(u, (struct sockaddr *)&a, sizeof a) == 0 && send(u, "q1", 2, 0) == 2 && nn_poll(u, POLLIN, 8000)
            && recv(u, b, 2, 0) == 2 && !strcmp(b, "hi");
        snprintf(msg, sizeof msg, "%s: connected UDP 127.0.0.1:9090 is answered by the container", name); CHECK(msg, got);
        close(u);
        u = socket(AF_INET, SOCK_DGRAM, 0);
        struct sockaddr_in from; socklen_t fl = sizeof from; memset(b, 0, sizeof b);
        got = sendto(u, "q2", 2, 0, (struct sockaddr *)&a, sizeof a) == 2 && nn_poll(u, POLLIN, 8000)
            && recvfrom(u, b, 2, 0, (struct sockaddr *)&from, &fl) == 2 && !strcmp(b, "hi");
        snprintf(msg, sizeof msg, "%s: unconnected sendto 127.0.0.1:9090 is answered", name); CHECK(msg, got);
        char ip[32] = ""; inet_ntop(AF_INET, &from.sin_addr, ip, sizeof ip);
        snprintf(msg, sizeof msg, "%s: recvfrom reports 127.0.0.1:9090 (got %s:%d)", name, ip, ntohs(from.sin_port));
        CHECK(msg, !strcmp(ip, "127.0.0.1") && ntohs(from.sin_port) == 9090);
        close(u);
    }
    if (ok && mode == 5) {
        ok = rd_to(dd[0], &ch) == 1;
        snprintf(msg, sizeof msg, "%s: hairpin flow established through DNAT", name); CHECK(msg, ok);
        FILE *nf = fopen("/tmp/fn.nft", "w");
        if (nf) { fputs("table inet midflow {\n chain FORWARD { type filter hook forward priority filter; policy accept;\n  ct state established drop\n }\n}\n", nf); fclose(nf); }
        snprintf(msg, sizeof msg, "%s: nft -f adds `ct state established drop` mid-flow", name);
        CHECK(msg, system("nft -f /tmp/fn.nft >/tmp/fn.err 2>&1") == 0);
        usleep(200000);
        write(cc[1], &ch, 1);
    }
    if (c > 0) {
        int r = wait_child(c);
        if (mode == 5) { snprintf(msg, sizeof msg, "%s: established hairpin flow is dropped by a rule added mid-flow (exit %d)", name, r); CHECK(msg, r == 0); }
        else {
        snprintf(msg, sizeof msg, "%s: server saw the connection%s (exit %d)", name, mode ? " from 10.97.0.1" : "", r); CHECK(msg, r == 0); }
    }
    if (c2 > 0) {
        int r = wait_child(c2);
        snprintf(msg, sizeof msg, "%s: client got the reply through the published port (exit %d)", name, r); CHECK(msg, r == 0);
    }
    system("nft delete table inet netavark");
    if (mode == 5) system("nft delete table inet midflow");
    link_del(s, bidx); close(s);
}
static void nftnat_hairpin_midflow(void) { nn_case("nftnat_hairpin_midflow", 5); }
static void nftnat_local(void) { nn_case("nftnat_local_redirect", 0); }
static void nftnat_hairpin(void) { nn_case("nftnat_hairpin", 1); }
static void nftnat_cross(void) { nn_case("nftnat_cross_netns", 2); }
static void nftnat_udp(void) { nn_case("nftnat_udp_local", 3); }
static void nftnat_range(void) { nn_case("nftnat_port_range", 4); }

static const char FN_RULES_FMT_A[] =
 "table inet netavark {\n chain FORWARD { type filter hook forward priority filter; policy accept;\n  ct state invalid drop\n  oifname \"br0\" drop\n }\n"
 " chain POSTROUTING { type nat hook postrouting priority srcnat; policy accept;\n  ip saddr 10.98.0.0/24 masquerade\n }\n}\n";
static const char FN_RULES_FMT_B[] =
 "table inet netavark {\n chain FORWARD { type filter hook forward priority filter; policy accept;\n  ct state invalid drop\n  ip saddr 10.97.0.0/24 accept\n }\n"
 " chain POSTROUTING { type nat hook postrouting priority srcnat; policy accept;\n  ip saddr 10.97.0.0/24 masquerade\n }\n}\n";
/* --internal: ip_forward=1 (a later network clobbered forwarding=0) but no masquerade intent for the subnet */
static void nftnat_internal(void) { g_fwd_nft = FN_RULES_FMT_A; g_fwd_expect_ovr = 0; fwd_case("nftnat_internal", 0, 1); g_fwd_nft = NULL; g_fwd_expect_ovr = -1; }
static void nftnat_masq_ok(void) { g_fwd_nft = FN_RULES_FMT_B; fwd_case("nftnat_masq_ok", 0, 1); g_fwd_nft = NULL; }
static void fwd_udp(void) { fwd_case("fwd_udp", 0, 1); }
static void fwd_ping(void) { fwd_case("fwd_ping", 1, 1); }
static void fwd_off(void) { fwd_case("fwd_off", 0, 0); }
static void fwd_icmperr(void) { fwd_case("fwd_icmperr", 3, 1); }
static void fwd_tcp(void) { fwd_case("fwd_tcp", 2, 1); }
static void fwd_port_reserved(void) { fwd_case("fwd_port_reserved", 4, 1); }
static void fwd_mac_forge(void) { fwd_case("fwd_mac_forge", 8, 1); }
static void fwd_mac_steal(void) { fwd_case("fwd_mac_steal", 9, 1); }
static void fwd_hostlisten(void) { fwd_case("fwd_hostlisten", 6, 1); }
static void fwd_hostlisten_early(void) { g_early_listen = 1; fwd_case("fwd_hostlisten_early", 6, 1); g_early_listen = 0; }
static void nftfwd_midflow(void) { g_fwd_expect_ovr = 0; fwd_case("nftfwd_midflow_established", 10, 1); g_fwd_expect_ovr = -1; }
static void fwd_reap(void) { fwd_case("fwd_reap", 5, 1); }
/* ICMP errors of a tracked flow are `ct state related`: with a default-drop FORWARD policy they pass only when a rule accepts them. */
static const char FR_RULES_OFF[] =
 "table inet netavark {\n chain FORWARD { type filter hook forward priority filter; policy drop;\n  ct state new accept\n  ct state established accept\n }\n}\n";
static const char FR_RULES_ON[] =
 "table inet netavark {\n chain FORWARD { type filter hook forward priority filter; policy drop;\n  ct state new accept\n  ct state established,related accept\n }\n}\n";
static void nftfwd_related_off(void) {
    g_fwd_nft = FR_RULES_OFF; g_fwd_expect_ovr = 0;
    g_fwd_msg = "ICMP port-unreachable (ct state related) is dropped by the default-drop policy";
    fwd_case("nftfwd_related_drop", 3, 1);
    g_fwd_nft = NULL; g_fwd_expect_ovr = -1; g_fwd_msg = NULL;
}
static void nftfwd_related_on(void) {
    g_fwd_nft = FR_RULES_ON;
    g_fwd_msg = "ICMP port-unreachable is delivered by `ct state related accept` under a default-drop policy (ECONNREFUSED)";
    fwd_case("nftfwd_related_accept", 3, 1);
    g_fwd_nft = NULL; g_fwd_msg = NULL;
}

/* ---- IPv6 established flows traverse FORWARD (NAT66 through the NIC) -------------------------------------- */
static char g_gw6[64];
static int gw6_find(void) {
    /* QEMU slirp: the router is <SLAAC prefix>::2 (the default route's next hop is a link-local address) */
    FILE *f = fopen("/proc/net/if_inet6", "r");
    if (!f) return -1;
    char l[256]; int r = -1;
    while (fgets(l, sizeof l, f)) {
        char a[40], ifn[24]; unsigned ix, pl, sc, fl;
        if (sscanf(l, "%32s %x %x %x %x %23s", a, &ix, &pl, &sc, &fl, ifn) != 6) continue;
        if (sc != 0 || strncmp(ifn, "eth", 3)) continue;
        unsigned g[4];
        for (int i = 0; i < 4; i++) { char q[5]; memcpy(q, a + i * 4, 4); q[4] = 0; sscanf(q, "%x", &g[i]); }
        snprintf(g_gw6, sizeof g_gw6, "%x:%x:%x:%x::2", g[0], g[1], g[2], g[3]);
        r = 0; break;
    }
    fclose(f);
    return r;
}
static int ping6_sock(int ic, const char *dst, unsigned short seq) {
    struct sockaddr_in6 to; memset(&to, 0, sizeof to); to.sin6_family = AF_INET6;
    inet_pton(AF_INET6, dst, &to.sin6_addr);
    unsigned char pk[8] = { 128, 0, 0, 0, 0x12, 0x34, (unsigned char)(seq >> 8), (unsigned char)seq };
    if (sendto(ic, pk, 8, 0, (struct sockaddr *)&to, sizeof to) != 8) return 2;
    unsigned char rb[64];
    ssize_t n = recv(ic, rb, sizeof rb, 0);
    return n >= 8 && rb[0] == 129 ? 0 : 3;
}
static int fwd6_body(void) {
    g_ip6 = "fd00:97::2";
    int e = v6_conf_eth0(); if (e) return e;
    int cs = nl_sock();
    int idx = link_get(cs, "eth0", NULL, NULL);
    if (route6_default(cs, idx, "fd00:97::1")) return 3;
    close(cs);
    sleep(2);                                        /* 1 s IPv6 DAD: a tentative source address sends nothing */
    char ch = 'r'; write(done_[1], &ch, 1);
    if (rd_to(ctl[0], &ch) != 1) return 4;
    int ic = socket(AF_INET6, SOCK_DGRAM, IPPROTO_ICMPV6);
    if (ic < 0) return 5;
    struct timeval tv = { 4, 0 };
    setsockopt(ic, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    int pr = 3;
    printf("DIAG child ping bridge fd00:97::1 -> %d\n", ping6_sock(ic, "fd00:97::1", 9)); fflush(stdout);
    for (int k = 0; k < 3 && pr; k++) pr = ping6_sock(ic, g_gw6, 1 + k); /* NEW then reply (the first may be lost to ND): the flow is now established */
    if (pr) {
        printf("DIAG v6 ping to %s: rc %d errno %d\n", g_gw6, pr, errno); fflush(stdout);
        system("cat /proc/net/if_inet6 /proc/net/ipv6_route 2>&1 | head -12; cat /proc/net/nic | head -30");
        return 6;
    }
    write(done_[1], &ch, 1);
    if (rd_to(ctl[0], &ch) != 1) return 7;
    return ping6_sock(ic, g_gw6, 2) ? 0 : 8;         /* must NOT be answered once `ct state established drop` is loaded */
}
static int ip6_forward_set(int on) {
    int fd = open("/proc/sys/net/ipv6/conf/all/forwarding", O_WRONLY);
    if (fd < 0) return -1;
    int r = write(fd, on ? "1\n" : "0\n", 2);
    close(fd);
    return r == 2 ? 0 : -1;
}
static void nftfwd6_midflow(void) {
    const char *name = "nftfwd6_midflow_established";
    char msg[140];
    snprintf(msg, sizeof msg, "%s: the NIC has a global v6 address (slirp router ::2)", name);
    int have = gw6_find() == 0;
    CHECK(msg, have);
    if (!have) return;
    { int hp = ping6_once(g_gw6, 0); printf("DIAG host ping %s -> %d\n", g_gw6, hp); fflush(stdout); }
    int s = nl_sock();
    unsigned char brmac[6] = { 0x02, 0x00, 0x5e, 0x97, 0x00, 0x06 };
    int ok = br_new(s, "br0", brmac) == 0;
    int bidx = link_get(s, "br0", NULL, NULL);
    ok = ok && bidx > 0 && addr6_add(s, bidx, "fd00:97::1", 64) == 0 && link_set_up(s, bidx) == 0 && ip_forward_set(1) == 0 && ip6_forward_set(1) == 0;
    snprintf(msg, sizeof msg, "%s: br0 fd00:97::1/64 up, ipv6 forwarding=1", name); CHECK(msg, ok);
    pid_t c = spawn_ns(fwd6_body);
    int cc[2] = { ctl[0], ctl[1] }, dd[2] = { done_[0], done_[1] };
    int fc = c > 0 ? netns_fd_of(c) : -1;
    ok = c > 0 && veth_new_m(s, "vethS", "eth0", fc, bidx) == 0;
    if (fc >= 0) close(fc);
    int iv = link_get(s, "vethS", NULL, NULL);
    ok = ok && iv > 0 && link_set_up(s, iv) == 0;
    snprintf(msg, sizeof msg, "%s: netns + veth", name); CHECK(msg, ok);
    char ch = 'g';
    if (ok) {
        write(cc[1], &ch, 1);
        ok = rd_to(dd[0], &ch) == 1;
        snprintf(msg, sizeof msg, "%s: netns configured", name); CHECK(msg, ok);
        if (ok) write(cc[1], &ch, 1);
        ok = ok && rd_to(dd[0], &ch) == 1;
        snprintf(msg, sizeof msg, "%s: ICMPv6 echo to %s answered through NAT66 (flow established)", name, g_gw6); CHECK(msg, ok);
        if (ok) {
            FILE *nf = fopen("/tmp/fn.nft", "w");
            if (nf) { fputs("table inet midflow {\n chain FORWARD { type filter hook forward priority filter; policy accept;\n  ct state established drop\n }\n}\n", nf); fclose(nf); }
            snprintf(msg, sizeof msg, "%s: nft -f adds `ct state established drop` mid-flow", name);
            CHECK(msg, system("nft -f /tmp/fn.nft >/tmp/fn.err 2>&1") == 0);
            usleep(200000);
            write(cc[1], &ch, 1);
        }
    }
    int rc = c > 0 ? wait_child(c) : -1;
    snprintf(msg, sizeof msg, "%s: established v6 flow is dropped by a rule added mid-flow (exit %d)", name, rc); CHECK(msg, rc == 0);
    system("nft delete table inet midflow");
    ip6_forward_set(0); ip_forward_set(0);
    if (iv > 0) link_del(s, iv);
    link_del(s, bidx);
    close(s);
}

/* A host wildcard listener must not leak into a container netns: a netns
 * stack that connects to its OWN address on the listener's port gets
 * ECONNREFUSED, and the host listener never sees a connection. */
static void hostlisten_not_in_ns(void) {
    int s = nl_sock();
    int hl = socket(AF_INET, SOCK_STREAM, 0);
    { int one = 1; setsockopt(hl, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one); }
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
        if (connect(t, (struct sockaddr *)&to, sizeof to) == 0) _exit(200);
        _exit(errno == ECONNREFUSED ? 0 : errno);
    }
    int rc = wait_child(h);
    printf("hostlisten_not_in_ns child exit %d (200 = connected, else errno)\n", rc);
    CHECK("hostlisten_not_in_ns: connect to the netns's own address is refused", rc == 0);
    int fl = fcntl(hl, F_GETFL); fcntl(hl, F_SETFL, fl | O_NONBLOCK);
    int a = accept(hl, NULL, NULL);
    CHECK("hostlisten_not_in_ns: the host listener saw no connection", a < 0 && (errno == EAGAIN || errno == EWOULDBLOCK));
    if (a >= 0) close(a);
    close(ns); close(hl);
    kill(c, SIGKILL); wait_child(c);
    close(s);
}

/* A socket bound to a stack that was freed (DELLINK) must fail like a socket on a
 * dead link; the net server used to hit SocketSet::get on the stale handle and panic. */
static void stale_stack(void) {
    int s = nl_sock();
    pid_t c = spawn_ns(idle_body);
    int nsfd = netns_fd_of(c);
    int e = veth_new(s, "vethZ", "eth0", nsfd);
    close(nsfd);
    int iz = link_get(s, "vethZ", NULL, NULL);
    CHECK("stale_stack: pair created", e == 0 && iz > 0);
    addr_add(s, iz, "10.99.9.1", 24); link_set_up(s, iz);
    int t = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    struct sockaddr_in to = { .sin_family = AF_INET, .sin_port = htons(7777) };
    inet_pton(AF_INET, "10.99.9.2", &to.sin_addr);
    int r = connect(t, (struct sockaddr *)&to, sizeof to);
    CHECK("stale_stack: connect in flight on the veth stack", r < 0 && errno == EINPROGRESS);
    CHECK("stale_stack: DELLINK of the stack's link", link_del(s, iz) == 0);
    /* a new pair reuses the freed stack slot */
    int e2 = veth_new(s, "vethSa", "vethSb", -1);
    (void)e2;
    struct pollfd p = { t, POLLIN | POLLOUT, 0 };
    int n = poll(&p, 1, 0);
    CHECK("stale_stack: poll answers (no panic)", n >= 0);
    char b[8];
    ssize_t w = send(t, "x", 1, MSG_NOSIGNAL);
    CHECK("stale_stack: send fails, not hangs", w < 0);
    ssize_t rd = recv(t, b, sizeof b, 0);
    CHECK("stale_stack: recv fails", rd < 0);
    int soerr = 0; socklen_t sl = sizeof soerr;
    CHECK("stale_stack: getsockopt answers", getsockopt(t, SOL_SOCKET, SO_ERROR, &soerr, &sl) == 0);
    CHECK("stale_stack: close", close(t) == 0);
    CHECK("stale_stack: net server alive", link_get(s, "lo", NULL, NULL) > 0);
    { int ia = link_get(s, "vethSa", NULL, NULL); if (ia > 0) link_del(s, ia); } /* leave no link behind */
    kill(c, SIGKILL); wait_child(c);
    close(s);
}

/* Race: a child keeps driving TCP/UDP/ICMP sockets that sit on a veth stack while the
 * parent deletes and recreates the pair (stack freed, slot reused) many times. The
 * per-op heal_stale check is racy against the reap; the net server must never panic
 * ("handle does not refer to a valid socket"), only answer errors. */
static void stale_race(void) {
    int s = nl_sock();
    int ok_rounds = 0;
    for (int round = 0; round < 20; round++) {
        pid_t c = spawn_ns(idle_body);
        int nsfd = netns_fd_of(c);
        int e = veth_new(s, "vethR", "eth0", nsfd);
        close(nsfd);
        int iz = link_get(s, "vethR", NULL, NULL);
        if (e != 0 || iz <= 0) { kill(c, SIGKILL); wait_child(c); continue; }
        addr_add(s, iz, "10.99.8.1", 24); link_set_up(s, iz);
        pid_t hs[3];
        for (int d = 0; d < 2; d++) {
            hs[d] = fork();
            if (hs[d] != 0) continue;
            alarm(10);
            struct sockaddr_in to = { .sin_family = AF_INET, .sin_port = htons(7777) };
            inet_pton(AF_INET, "10.99.8.2", &to.sin_addr);
            char b[64];
            for (int k = 0; k < 100; k++) {
                int t = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
                int u = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0);
                int i = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, IPPROTO_ICMP);
                connect(t, (struct sockaddr *)&to, sizeof to);
                connect(u, (struct sockaddr *)&to, sizeof to);
                connect(i, (struct sockaddr *)&to, sizeof to);
                for (int j = 0; j < 8; j++) {
                    struct pollfd p[3] = { { t, POLLIN | POLLOUT, 0 }, { u, POLLIN | POLLOUT, 0 }, { i, POLLIN | POLLOUT, 0 } };
                    poll(p, 3, 0);
                    send(t, "x", 1, MSG_NOSIGNAL); send(u, "x", 1, MSG_NOSIGNAL); send(i, b, 8, MSG_NOSIGNAL);
                    recv(t, b, sizeof b, 0); recv(u, b, sizeof b, 0); recv(i, b, sizeof b, 0);
                    int se; socklen_t sl = sizeof se;
                    getsockopt(t, SOL_SOCKET, SO_ERROR, &se, &sl);
                    struct sockaddr_in a; socklen_t al = sizeof a;
                    getsockname(t, (struct sockaddr *)&a, &al); getpeername(u, (struct sockaddr *)&a, &al);
                }
                close(t); close(u); close(i);
            }
            _exit(0);
        }
        usleep(1000 + (round * 937) % 15000);
        link_del(s, iz);
        veth_new(s, "vethRa", "vethRb", -1);
        for (int d = 0; d < 2; d++) { if (hs[d] > 0) wait_child(hs[d]); }
        { int ia = link_get(s, "vethRa", NULL, NULL); if (ia > 0) link_del(s, ia); }
        kill(c, SIGKILL); wait_child(c);
        ok_rounds++;
    }
    CHECK("stale_race: 20 reap rounds, drivers finished", ok_rounds >= 15);
    CHECK("stale_race: net server alive", link_get(s, "lo", NULL, NULL) > 0);
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
    if (!only || !strcmp(only, "veth_tcp_bulk_exit")) veth_tcp_bulk_exit();
    if (!only || !strcmp(only, "veth_listen_reap")) veth_listen_reap();
    if (!only || !strcmp(only, "veth_ping")) veth_ping();
    if (!only || !strcmp(only, "veth6")) veth6();
    if (!only || !strcmp(only, "bridge6")) bridge6();
    if (!only || !strcmp(only, "veth6gw")) veth6gw();
    if (!only || !strcmp(only, "veth_dellink")) veth_dellink();
    if (!only || !strcmp(only, "veth_naming")) veth_naming();
    if (!only || !strcmp(only, "veth_setlink_move")) veth_setlink_move();
    if (!only || !strcmp(only, "veth_nsgone")) veth_nsgone();
    if (!only || !strcmp(only, "bridge_l2")) bridge_l2();
    if (!only || !strcmp(only, "nftnat_local_redirect")) nftnat_local();
    if (!only || !strcmp(only, "nftnat_hairpin")) nftnat_hairpin();
    if (!only || !strcmp(only, "nftnat_cross_netns")) nftnat_cross();
    if (!only || !strcmp(only, "nftnat_udp_local")) nftnat_udp();
    if (!only || !strcmp(only, "nftnat_port_range")) nftnat_range();
    if (!only || !strcmp(only, "nftnat_internal")) nftnat_internal();
    if (!only || !strcmp(only, "nftnat_masq_ok")) nftnat_masq_ok();
    if (!only || !strcmp(only, "fwd_udp")) fwd_udp();
    if (!only || !strcmp(only, "nftfwd_midflow_established")) nftfwd_midflow();
    if (!only || !strcmp(only, "nftfwd6_midflow_established")) nftfwd6_midflow();
    if (!only || !strcmp(only, "nftfwd_related_drop")) nftfwd_related_off();
    if (!only || !strcmp(only, "nftfwd_related_accept")) nftfwd_related_on();
    if (!only || !strcmp(only, "nftnat_hairpin_midflow")) nftnat_hairpin_midflow();
    if (!only || !strcmp(only, "routed_l3")) routed_on();
    if (!only || !strcmp(only, "routed_l3_off")) routed_off();
    if (!only || !strcmp(only, "fwd_ping")) fwd_ping();
    if (!only || !strcmp(only, "fwd_off")) fwd_off();
    if (!only || !strcmp(only, "fwd_icmperr")) fwd_icmperr();
    if (!only || !strcmp(only, "fwd_tcp")) fwd_tcp();
    if (!only || !strcmp(only, "fwd_port_reserved")) fwd_port_reserved();
    if (!only || !strcmp(only, "fwd_reap")) fwd_reap();
    if (!only || !strcmp(only, "fwd_mac_forge")) fwd_mac_forge();
    if (!only || !strcmp(only, "fwd_mac_steal")) fwd_mac_steal();
    if (!only || !strcmp(only, "fwd_hostlisten")) fwd_hostlisten();
    if (!only || !strcmp(only, "fwd_hostlisten_early")) fwd_hostlisten_early();
    if (!only || !strcmp(only, "hostlisten_not_in_ns")) hostlisten_not_in_ns();
    if (!only || !strcmp(only, "stale_stack")) stale_stack();
    if (!only || !strcmp(only, "stale_race")) stale_race();
    printf("brtest: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
