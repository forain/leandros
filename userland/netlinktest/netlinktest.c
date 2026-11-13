// netlinktest: NETLINK_ROUTE (RTM_GETLINK/RTM_GETADDR dumps), getifaddrs,
// if_nameindex and the SIOCGIF* ioctls. nICEr (Firefox WebRTC) gathers its
// local ICE candidates through exactly these.
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <ifaddrs.h>
#include <linux/if_tun.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <net/if_arp.h>
#include <netinet/in.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

static int npass, nfail;
#define CHECK(name, cond) do { if (cond) { npass++; printf("PASS %s\n", name); } \
    else { nfail++; printf("FAIL %s (errno %d)\n", name, errno); } } while (0)

struct req { struct nlmsghdr h; struct rtgenmsg g; };

static int dump_fam(int s, int type, unsigned seq, int fam, char *buf, int cap) {
    struct req r;
    memset(&r, 0, sizeof r);
    r.h.nlmsg_len = sizeof r;
    r.h.nlmsg_type = type;
    r.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    r.h.nlmsg_seq = seq;
    r.g.rtgen_family = fam;
    struct sockaddr_nl to = { .nl_family = AF_NETLINK };
    if (sendto(s, &r, sizeof r, 0, (struct sockaddr *)&to, sizeof to) != (ssize_t)sizeof r) return -1;
    int total = 0;
    for (;;) {
        ssize_t n = recv(s, buf + total, cap - total, 0);
        if (n <= 0) return -1;
        struct nlmsghdr *h = (struct nlmsghdr *)(buf + total);
        int off = 0, done = 0;
        while (NLMSG_OK(h, n - off)) {
            if (h->nlmsg_type == NLMSG_DONE) done = 1;
            if (h->nlmsg_type == NLMSG_ERROR) return -2;
            off += NLMSG_ALIGN(h->nlmsg_len);
            h = (struct nlmsghdr *)((char *)h + NLMSG_ALIGN(h->nlmsg_len));
        }
        total += n;
        if (done) return total;
    }
}


/* ---- rtnetlink writes (lane rtnlw) ---------------------------------------- */
#ifndef TUNSETIFF
#define TUNSETIFF 0x400454ca
#endif
#ifndef RTM_SETLINK
#define RTM_SETLINK 19
#endif

static int dump(int s, int type, unsigned seq, char *buf, int cap) { return dump_fam(s, type, seq, AF_UNSPEC, buf, cap); }

struct nlb { char b[1024]; int len; };
static void nl_begin(struct nlb *m, int type, int flags, const void *body, int blen) {
    memset(m, 0, sizeof *m);
    struct nlmsghdr *h = (struct nlmsghdr *)m->b;
    h->nlmsg_type = type; h->nlmsg_flags = flags | NLM_F_REQUEST; h->nlmsg_seq = 77;
    memcpy(m->b + NLMSG_HDRLEN, body, blen);
    m->len = NLMSG_HDRLEN + NLMSG_ALIGN(blen);
}
static void nl_attr(struct nlb *m, int type, const void *d, int dl) {
    struct rtattr *a = (struct rtattr *)(m->b + m->len);
    a->rta_type = type; a->rta_len = RTA_LENGTH(dl);
    memcpy(RTA_DATA(a), d, dl);
    m->len += RTA_ALIGN(a->rta_len);
}
/* Send; return 0 on ack, else the positive errno (or -1 on transport failure). */
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
static int nl_link(int s, int type, int flags, int idx, unsigned ifl, unsigned chg,
                   int mtu, const unsigned char *mac, const char *name) {
    struct ifinfomsg i; memset(&i, 0, sizeof i);
    i.ifi_family = AF_UNSPEC; i.ifi_index = idx; i.ifi_flags = ifl; i.ifi_change = chg;
    struct nlb m; nl_begin(&m, type, flags, &i, sizeof i);
    if (mtu) nl_attr(&m, IFLA_MTU, &mtu, 4);
    if (mac) nl_attr(&m, IFLA_ADDRESS, mac, 6);
    if (name) nl_attr(&m, IFLA_IFNAME, name, strlen(name) + 1);
    return nl_do(s, &m);
}
static int nl_addr(int s, int type, int flags, int idx, const char *ip, int plen) {
    struct ifaddrmsg a; memset(&a, 0, sizeof a);
    a.ifa_family = AF_INET; a.ifa_prefixlen = plen; a.ifa_index = idx;
    struct in_addr in; inet_pton(AF_INET, ip, &in);
    struct nlb m; nl_begin(&m, type, flags, &a, sizeof a);
    nl_attr(&m, IFA_LOCAL, &in, 4);
    nl_attr(&m, IFA_ADDRESS, &in, 4);
    return nl_do(s, &m);
}
static int nl_route(int s, int type, int flags, const char *dst, int plen, const char *gw, int oif) {
    struct rtmsg r; memset(&r, 0, sizeof r);
    r.rtm_family = AF_INET; r.rtm_dst_len = plen; r.rtm_table = RT_TABLE_MAIN;
    r.rtm_protocol = RTPROT_BOOT; r.rtm_scope = gw ? RT_SCOPE_UNIVERSE : RT_SCOPE_LINK; r.rtm_type = RTN_UNICAST;
    struct nlb m; nl_begin(&m, type, flags, &r, sizeof r);
    struct in_addr in;
    if (plen) { inet_pton(AF_INET, dst, &in); nl_attr(&m, RTA_DST, &in, 4); }
    if (gw) { inet_pton(AF_INET, gw, &in); nl_attr(&m, RTA_GATEWAY, &in, 4); }
    if (oif) nl_attr(&m, RTA_OIF, &oif, 4);
    return nl_do(s, &m);
}
/* Dump routes: does one with dst/plen (and gw when non-NULL) exist? */
static int has_route(int s, const char *dst, int plen, const char *gw) {
    static char b[16384];
    int n = dump(s, RTM_GETROUTE, 5, b, sizeof b), found = 0;
    struct in_addr d, g; inet_pton(AF_INET, dst, &d); if (gw) inet_pton(AF_INET, gw, &g);
    for (struct nlmsghdr *h = (struct nlmsghdr *)b; n > 0 && NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
        if (h->nlmsg_type != RTM_NEWROUTE) continue;
        struct rtmsg *r = NLMSG_DATA(h);
        if (r->rtm_dst_len != plen) continue;
        int al = RTM_PAYLOAD(h), dok = (plen == 0), gok = (gw == NULL);
        for (struct rtattr *a = RTM_RTA(r); RTA_OK(a, al); a = RTA_NEXT(a, al)) {
            if (a->rta_type == RTA_DST && !memcmp(RTA_DATA(a), &d, 4)) dok = 1;
            if (gw && a->rta_type == RTA_GATEWAY && !memcmp(RTA_DATA(a), &g, 4)) gok = 1;
        }
        if (dok && gok) found = 1;
    }
    return found;
}
/* Find link idx in a dump: flags/mtu/mac/name/txqlen/linkinfo-kind. */
struct linfo { int found; unsigned flags; unsigned mtu; unsigned char mac[6]; char name[32]; int txq; char kind[16]; int has_oper; };
static void get_link(int s, int idx, struct linfo *o) {
    static char b[16384];
    memset(o, 0, sizeof *o);
    int n = dump(s, RTM_GETLINK, 6, b, sizeof b);
    for (struct nlmsghdr *h = (struct nlmsghdr *)b; n > 0 && NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
        if (h->nlmsg_type != RTM_NEWLINK) continue;
        struct ifinfomsg *i = NLMSG_DATA(h);
        if (i->ifi_index != idx) continue;
        o->found = 1; o->flags = i->ifi_flags;
        int al = IFLA_PAYLOAD(h);
        for (struct rtattr *a = IFLA_RTA(i); RTA_OK(a, al); a = RTA_NEXT(a, al)) {
            if (a->rta_type == IFLA_MTU) memcpy(&o->mtu, RTA_DATA(a), 4);
            if (a->rta_type == IFLA_ADDRESS) memcpy(o->mac, RTA_DATA(a), 6);
            if (a->rta_type == IFLA_IFNAME) strncpy(o->name, RTA_DATA(a), 31);
            if (a->rta_type == IFLA_TXQLEN) o->txq = *(int *)RTA_DATA(a);
            if (a->rta_type == IFLA_OPERSTATE) o->has_oper = 1;
            if (a->rta_type == IFLA_LINKINFO) {
                int bl = RTA_PAYLOAD(a);
                for (struct rtattr *c = RTA_DATA(a); RTA_OK(c, bl); c = RTA_NEXT(c, bl))
                    if (c->rta_type == IFLA_INFO_KIND) strncpy(o->kind, RTA_DATA(c), 15);
            }
        }
    }
}
static int has_addr(int s, int idx, const char *ip) {
    static char b[16384];
    int n = dump(s, RTM_GETADDR, 8, b, sizeof b), found = 0;
    struct in_addr want; inet_pton(AF_INET, ip, &want);
    for (struct nlmsghdr *h = (struct nlmsghdr *)b; n > 0 && NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
        if (h->nlmsg_type != RTM_NEWADDR) continue;
        struct ifaddrmsg *a = NLMSG_DATA(h);
        if ((int)a->ifa_index != idx) continue;
        int al = IFA_PAYLOAD(h);
        for (struct rtattr *r = IFA_RTA(a); RTA_OK(r, al); r = RTA_NEXT(r, al))
            if (r->rta_type == IFA_LOCAL && !memcmp(RTA_DATA(r), &want, 4)) found = 1;
    }
    return found;
}

/* Runs inside unshare -Urn: returns the number of failures. */
static int tap_cfg_body(void) {
    int bad = 0;
#define C(name, cond) do { if (cond) printf("PASS %s\n", name); else { bad++; printf("FAIL %s (errno %d)\n", name, errno); } } while (0)
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) { printf("FAIL unshare\n"); return 1; }
    int tfd = open("/dev/net/tun", O_RDWR);
    struct ifreq ifr; memset(&ifr, 0, sizeof ifr);
    strcpy(ifr.ifr_name, "tap0"); ifr.ifr_flags = IFF_TAP | IFF_NO_PI;
    if (tfd < 0 || ioctl(tfd, TUNSETIFF, &ifr) < 0) { printf("FAIL TUNSETIFF\n"); return 1; }
    int idx = if_nametoindex("tap0");
    int s = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    unsigned char mac[6] = { 0x02, 0x11, 0x22, 0x33, 0x44, 0x55 };
    C("setlink_type19_mtu_mac", nl_link(s, RTM_SETLINK, 0, idx, 0, 0, 65520, mac, NULL) == 0);
    struct linfo li; get_link(s, idx, &li);
    C("link_mtu_mac_visible", li.found && li.mtu == 65520 && !memcmp(li.mac, mac, 6));
    C("link_has_txqlen_oper", li.txq == 1000 && li.has_oper);
    C("link_kind_tun", !strcmp(li.kind, "tun"));
    C("newaddr", nl_addr(s, RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, idx, "10.0.2.15", 24) == 0);
    C("newaddr_excl_eexist", nl_addr(s, RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, idx, "10.0.2.15", 24) == EEXIST);
    C("setlink_up", nl_link(s, RTM_SETLINK, 0, idx, IFF_UP, IFF_UP, 0, NULL, NULL) == 0);
    get_link(s, idx, &li);
    C("link_up_visible", li.found && (li.flags & IFF_UP));
    C("newroute_default", nl_route(s, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, "0.0.0.0", 0, "10.0.2.2", idx) == 0);
    C("newroute_excl_eexist", nl_route(s, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, "0.0.0.0", 0, "10.0.2.2", idx) == EEXIST);
    C("route_dump_default", has_route(s, "0.0.0.0", 0, "10.0.2.2"));
    C("newroute_static", nl_route(s, RTM_NEWROUTE, NLM_F_CREATE, "192.168.7.0", 24, "10.0.2.2", idx) == 0);
    C("route_dump_static", has_route(s, "192.168.7.0", 24, "10.0.2.2"));
    C("addr_dump_has", has_addr(s, idx, "10.0.2.15"));
    C("delroute_static", nl_route(s, RTM_DELROUTE, 0, "192.168.7.0", 24, "10.0.2.2", idx) == 0);
    C("delroute_missing_esrch", nl_route(s, RTM_DELROUTE, 0, "192.168.7.0", 24, "10.0.2.2", idx) == ESRCH);
    C("delroute_default", nl_route(s, RTM_DELROUTE, 0, "0.0.0.0", 0, "10.0.2.2", idx) == 0);
    C("route_gone", !has_route(s, "0.0.0.0", 0, "10.0.2.2") && !has_route(s, "192.168.7.0", 24, NULL));
    C("setlink_down", nl_link(s, RTM_SETLINK, 0, idx, 0, IFF_UP, 0, NULL, NULL) == 0);
    C("setlink_rename", nl_link(s, RTM_SETLINK, 0, idx, 0, 0, 0, NULL, "tapx") == 0);
    get_link(s, idx, &li);
    C("rename_visible", li.found && !strcmp(li.name, "tapx"));
    C("deladdr", nl_addr(s, RTM_DELADDR, 0, idx, "10.0.2.15", 24) == 0);
    C("addr_gone", !has_addr(s, idx, "10.0.2.15"));
    C("deladdr_missing_eaddrnotavail", nl_addr(s, RTM_DELADDR, 0, idx, "10.0.2.15", 24) == EADDRNOTAVAIL);
    /* lo inside the new netns: up through SETLINK(19), then down */
    C("setlink_lo_up", nl_link(s, RTM_SETLINK, 0, 1, IFF_UP, IFF_UP, 0, NULL, NULL) == 0);
    C("setlink_lo_down", nl_link(s, RTM_SETLINK, 0, 1, 0, IFF_UP, 0, NULL, NULL) == 0);
    C("setlink_enodev", nl_link(s, RTM_SETLINK, 0, 9999, IFF_UP, IFF_UP, 0, NULL, NULL) == ENODEV);
    close(s); close(tfd);
    return bad;
}

static void nl_connect_test(void) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    struct sockaddr_nl k = { .nl_family = AF_NETLINK };
    CHECK("nl_connect_kernel", connect(s, (struct sockaddr *)&k, sizeof k) == 0);
    struct sockaddr_nl pn; socklen_t pl = sizeof pn; memset(&pn, 0xff, sizeof pn);
    CHECK("nl_getpeername", getpeername(s, (struct sockaddr *)&pn, &pl) == 0 && pn.nl_family == AF_NETLINK && pn.nl_pid == 0);
    struct nlb m; struct rtgenmsg g = { AF_UNSPEC };
    nl_begin(&m, RTM_GETLINK, NLM_F_DUMP, &g, sizeof g);
    ((struct nlmsghdr *)m.b)->nlmsg_len = m.len;
    CHECK("nl_send_after_connect", send(s, m.b, m.len, 0) == m.len);
    close(s);
}

static void rtnl_write_tests(void) {
    nl_connect_test();
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) { int bad = tap_cfg_body(); fflush(stdout); _exit(bad ? 1 : 0); }
    int st = 0; waitpid(c, &st, 0);
    CHECK("tap_cfg_netlink", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    /* non-root in the init namespace: writes need CAP_NET_ADMIN */
    fflush(stdout);
    c = fork();
    if (c == 0) {
        int s = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
        if (setgid(1000) || setuid(1000)) _exit(2);
        int e = nl_route(s, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, "192.168.99.0", 24, "10.0.2.2", 2);
        _exit(e == EPERM ? 0 : 1);
    }
    waitpid(c, &st, 0);
    CHECK("newroute_needs_cap", WIFEXITED(st) && WEXITSTATUS(st) == 0);
}


/* ---- IPv6 (design-ipv6 L6) ---- */
static int nl_addr6(int s, int type, int flags, int idx, const char *ip, int plen) {
    struct ifaddrmsg a; memset(&a, 0, sizeof a);
    a.ifa_family = AF_INET6; a.ifa_prefixlen = plen; a.ifa_index = idx; a.ifa_flags = IFA_F_NODAD;
    struct in6_addr in; inet_pton(AF_INET6, ip, &in);
    struct nlb m; nl_begin(&m, type, flags, &a, sizeof a);
    nl_attr(&m, IFA_LOCAL, &in, 16);
    nl_attr(&m, IFA_ADDRESS, &in, 16);
    return nl_do(s, &m);
}
/* Find an address in an AF_INET6 dump (idx 0 = any): returns scope+1, or 0. */
static int find_addr6(int s, int idx, const char *ip, int plen, int fam) {
    static char b[16384];
    int n = dump_fam(s, RTM_GETADDR, 20, fam, b, sizeof b), found = 0;
    struct in6_addr want; inet_pton(AF_INET6, ip, &want);
    for (struct nlmsghdr *h = (struct nlmsghdr *)b; n > 0 && NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
        if (h->nlmsg_type != RTM_NEWADDR) continue;
        struct ifaddrmsg *a = NLMSG_DATA(h);
        if (a->ifa_family != AF_INET6 || (idx && (int)a->ifa_index != idx) || a->ifa_prefixlen != plen) continue;
        int al = IFA_PAYLOAD(h);
        for (struct rtattr *r = IFA_RTA(a); RTA_OK(r, al); r = RTA_NEXT(r, al))
            if (r->rta_type == IFA_ADDRESS && !memcmp(RTA_DATA(r), &want, 16)) found = a->ifa_scope + 1;
    }
    return found;
}
static int nl_route6(int s, int type, int flags, const char *dst, int plen, const char *gw, int oif) {
    struct rtmsg r; memset(&r, 0, sizeof r);
    r.rtm_family = AF_INET6; r.rtm_dst_len = plen; r.rtm_table = RT_TABLE_MAIN;
    r.rtm_protocol = RTPROT_BOOT; r.rtm_scope = RT_SCOPE_UNIVERSE; r.rtm_type = RTN_UNICAST;
    struct nlb m; nl_begin(&m, type, flags, &r, sizeof r);
    struct in6_addr in;
    inet_pton(AF_INET6, dst, &in); nl_attr(&m, RTA_DST, &in, 16);
    if (gw) { inet_pton(AF_INET6, gw, &in); nl_attr(&m, RTA_GATEWAY, &in, 16); }
    if (oif) nl_attr(&m, RTA_OIF, &oif, 4);
    return nl_do(s, &m);
}
/* RTM_GETROUTE for dst: 0 if answered (oif/gw filled), else the positive errno. */
static int get_route6(int s, const char *dst, int *oif, struct in6_addr *gw, int *has_gw) {
    struct rtmsg r; memset(&r, 0, sizeof r);
    r.rtm_family = AF_INET6; r.rtm_dst_len = 128;
    struct nlb m; nl_begin(&m, RTM_GETROUTE, 0, &r, sizeof r);
    struct in6_addr in; inet_pton(AF_INET6, dst, &in); nl_attr(&m, RTA_DST, &in, 16);
    ((struct nlmsghdr *)m.b)->nlmsg_len = m.len;
    if (send(s, m.b, m.len, 0) != m.len) return -1;
    char rb[2048];
    ssize_t n = recv(s, rb, sizeof rb, 0);
    struct nlmsghdr *h = (struct nlmsghdr *)rb;
    if (n < (ssize_t)NLMSG_HDRLEN) return -1;
    if (h->nlmsg_type == NLMSG_ERROR) return -((struct nlmsgerr *)NLMSG_DATA(h))->error;
    if (h->nlmsg_type != RTM_NEWROUTE) return -1;
    struct rtmsg *rt = NLMSG_DATA(h);
    int al = RTM_PAYLOAD(h); *oif = 0; *has_gw = 0;
    for (struct rtattr *a = RTM_RTA(rt); RTA_OK(a, al); a = RTA_NEXT(a, al)) {
        if (a->rta_type == RTA_OIF) memcpy(oif, RTA_DATA(a), 4);
        if (a->rta_type == RTA_GATEWAY) { memcpy(gw, RTA_DATA(a), 16); *has_gw = 1; }
    }
    return 0;
}
static int in_dump6(int s, const char *dst, int plen) {
    static char b[16384];
    int n = dump_fam(s, RTM_GETROUTE, 21, AF_INET6, b, sizeof b), found = 0;
    struct in6_addr d; inet_pton(AF_INET6, dst, &d);
    for (struct nlmsghdr *h = (struct nlmsghdr *)b; n > 0 && NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
        if (h->nlmsg_type != RTM_NEWROUTE) continue;
        struct rtmsg *r = NLMSG_DATA(h);
        if (r->rtm_family != AF_INET6 || r->rtm_dst_len != plen) continue;
        int al = RTM_PAYLOAD(h);
        for (struct rtattr *a = RTM_RTA(r); RTA_OK(a, al); a = RTA_NEXT(a, al))
            if (a->rta_type == RTA_DST && !memcmp(RTA_DATA(a), &d, 16)) found = 1;
    }
    return found;
}
/* The default router in /proc/net/ipv6_route as text; 0 when none. */
static int proc_router6(char *out, int cap) {
    FILE *f = fopen("/proc/net/ipv6_route", "r");
    if (!f) return 0;
    char l[256]; int ok = 0;
    while (fgets(l, sizeof l, f)) {
        char dst[40], src[40], nh[40]; unsigned dl, sl;
        if (sscanf(l, "%32s %x %32s %x %32s", dst, &dl, src, &sl, nh) == 5 && dl == 0
            && strcmp(dst, "00000000000000000000000000000000") == 0 && strcmp(nh, "00000000000000000000000000000000") != 0) {
            snprintf(out, cap, "%s", nh); ok = 1; break;
        }
    }
    fclose(f);
    return ok;
}
static int proc_has(const char *file, const char *needle) {
    FILE *f = fopen(file, "r");
    if (!f) return 0;
    char l[512]; int ok = 0;
    while (fgets(l, sizeof l, f)) if (strstr(l, needle)) ok = 1;
    fclose(f);
    return ok;
}

static void v6_tests(void) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    int eth = if_nametoindex("eth0");
    CHECK("getaddr6_dump lo ::1/128 host", find_addr6(s, 1, "::1", 128, AF_INET6) == RT_SCOPE_HOST + 1);
    if (eth) {
        struct in6_addr ll; memset(&ll, 0, 16);
        CHECK("getaddr6_dump fe80 on eth0 scope link", ({ static char b[16384]; int n = dump_fam(s, RTM_GETADDR, 22, AF_INET6, b, sizeof b), f = 0;
            for (struct nlmsghdr *h = (struct nlmsghdr *)b; n > 0 && NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
                if (h->nlmsg_type != RTM_NEWADDR) continue;
                struct ifaddrmsg *a = NLMSG_DATA(h);
                int al = IFA_PAYLOAD(h);
                for (struct rtattr *r = IFA_RTA(a); RTA_OK(r, al); r = RTA_NEXT(r, al))
                    if (r->rta_type == IFA_ADDRESS && a->ifa_index == (unsigned)eth && a->ifa_prefixlen == 64
                        && a->ifa_scope == RT_SCOPE_LINK && ((unsigned char *)RTA_DATA(r))[0] == 0xfe) f = 1;
            } f; }));
        CHECK("getaddr6 AF_UNSPEC dump has both families", find_addr6(s, 1, "::1", 128, AF_UNSPEC) != 0);
    }
    struct ifaddrs *ifa, *p; int saw6 = 0;
    if (getifaddrs(&ifa) == 0) {
        for (p = ifa; p; p = p->ifa_next) if (p->ifa_addr && p->ifa_addr->sa_family == AF_INET6) saw6 = 1;
        freeifaddrs(ifa);
    }
    CHECK("getifaddrs6 returns AF_INET6 entries", saw6);

    /* newaddr6: add, bind succeeds, dump shows; delete, bind EADDRNOTAVAIL */
    struct sockaddr_in6 sa; memset(&sa, 0, sizeof sa);
    sa.sin6_family = AF_INET6; inet_pton(AF_INET6, "2001:db8:1::5", &sa.sin6_addr);
    int b0 = socket(AF_INET6, SOCK_DGRAM, 0);
    CHECK("newaddr6 bind before: EADDRNOTAVAIL", bind(b0, (struct sockaddr *)&sa, sizeof sa) < 0 && errno == EADDRNOTAVAIL);
    close(b0);
    CHECK("newaddr6 add", nl_addr6(s, RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, 1, "2001:db8:1::5", 64) == 0);
    int b1 = socket(AF_INET6, SOCK_DGRAM, 0);
    CHECK("newaddr6 bind succeeds", bind(b1, (struct sockaddr *)&sa, sizeof sa) == 0);
    close(b1);
    CHECK("newaddr6 dump shows it", find_addr6(s, 1, "2001:db8:1::5", 64, AF_INET6) != 0);
    CHECK("newaddr6 EEXIST", nl_addr6(s, RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, 1, "2001:db8:1::5", 64) == EEXIST);
    CHECK("newaddr6 ENODEV", nl_addr6(s, RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, 999, "2001:db8:1::6", 64) == ENODEV);
    CHECK("deladdr6", nl_addr6(s, RTM_DELADDR, 0, 1, "2001:db8:1::5", 64) == 0);
    CHECK("deladdr6 again EADDRNOTAVAIL", nl_addr6(s, RTM_DELADDR, 0, 1, "2001:db8:1::5", 64) == EADDRNOTAVAIL);
    int b2 = socket(AF_INET6, SOCK_DGRAM, 0);
    CHECK("deladdr6 bind EADDRNOTAVAIL", bind(b2, (struct sockaddr *)&sa, sizeof sa) < 0 && errno == EADDRNOTAVAIL);
    close(b2);

    /* routes: discover the router, never hardcode it */
    char nh[40]; int have = eth && proc_router6(nh, sizeof nh);
    struct in6_addr gw, want; int oif, hg;
    if (have) {
        for (int i = 0; i < 16; i++) { unsigned v; sscanf(nh + 2 * i, "%2x", &v); ((unsigned char *)&want)[i] = v; }
        CHECK("route6_get global via router on eth0", get_route6(s, "2001:db8:ffff::3", &oif, &gw, &hg) == 0
              && oif == eth && hg && !memcmp(&gw, &want, 16));
        char gs[64]; inet_ntop(AF_INET6, &want, gs, sizeof gs);
        CHECK("newroute6 add", nl_route6(s, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, "2001:db8:2::", 64, gs, eth) == 0);
        CHECK("newroute6 in dump", in_dump6(s, "2001:db8:2::", 64));
        CHECK("newroute6 GETROUTE picks it", get_route6(s, "2001:db8:2::9", &oif, &gw, &hg) == 0 && oif == eth && hg);
        CHECK("newroute6 EEXIST", nl_route6(s, RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, "2001:db8:2::", 64, gs, eth) == EEXIST);
        CHECK("delroute6", nl_route6(s, RTM_DELROUTE, 0, "2001:db8:2::", 64, gs, eth) == 0);
        CHECK("delroute6 gone", !in_dump6(s, "2001:db8:2::", 64));
    } else {
        CHECK("route6_get no router: ENETUNREACH", get_route6(s, "2001:db8:ffff::3", &oif, &gw, &hg) == ENETUNREACH);
    }
    CHECK("route6_get ::1 via lo", get_route6(s, "::1", &oif, &gw, &hg) == 0 && oif == 1);
    close(s);

    /* proc_tcp6: a [::1] listener is in tcp6, not tcp */
    int l = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 lo6; memset(&lo6, 0, sizeof lo6);
    lo6.sin6_family = AF_INET6; lo6.sin6_addr = in6addr_loopback; lo6.sin6_port = htons(40611);
    CHECK("proc_tcp6 listen", bind(l, (struct sockaddr *)&lo6, sizeof lo6) == 0 && listen(l, 1) == 0);
    CHECK("proc_tcp6 listed in tcp6", proc_has("/proc/net/tcp6", "00000000000000000000000001000000:9EA3 00000000000000000000000000000000:0000 0A"));
    CHECK("proc_tcp6 not in tcp", !proc_has("/proc/net/tcp", ":9EA3 "));
    close(l);
    int u = socket(AF_INET6, SOCK_DGRAM, 0);
    lo6.sin6_port = htons(40612);
    CHECK("proc_udp6 bind", bind(u, (struct sockaddr *)&lo6, sizeof lo6) == 0);
    CHECK("proc_udp6 listed in udp6", proc_has("/proc/net/udp6", "00000000000000000000000001000000:9EA4 "));
    CHECK("proc_udp6 not in udp", !proc_has("/proc/net/udp", ":9EA4 "));
    close(u);
}

int main(void) {
    rtnl_write_tests();
    v6_tests();
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    CHECK("socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE)", s >= 0);
    CHECK("other netlink protocol refused", socket(AF_NETLINK, SOCK_RAW, 99) < 0 && errno == EPROTONOSUPPORT);

    struct sockaddr_nl me = { .nl_family = AF_NETLINK };
    CHECK("bind", bind(s, (struct sockaddr *)&me, sizeof me) == 0);
    struct sockaddr_nl got;
    socklen_t gl = sizeof got;
    CHECK("getsockname", getsockname(s, (struct sockaddr *)&got, &gl) == 0 && gl == sizeof got
          && got.nl_family == AF_NETLINK && got.nl_pid != 0);

    struct pollfd p = { .fd = s, .events = POLLIN };
    CHECK("idle socket not readable", poll(&p, 1, 0) == 0);

    static char buf[16384];
    int n = dump(s, RTM_GETLINK, 11, buf, sizeof buf);
    CHECK("RTM_GETLINK dump", n > 0);
    int nlinks = 0, saw_lo = 0, saw_eth = 0, seq_ok = 1;
    for (struct nlmsghdr *h = (struct nlmsghdr *)buf; n > 0 && NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
        if (h->nlmsg_seq != 11) seq_ok = 0;
        if (h->nlmsg_type != RTM_NEWLINK) continue;
        struct ifinfomsg *ifi = NLMSG_DATA(h);
        int len = IFLA_PAYLOAD(h);
        for (struct rtattr *a = IFLA_RTA(ifi); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
            if (a->rta_type != IFLA_IFNAME) continue;
            nlinks++;
            if (!strcmp(RTA_DATA(a), "lo")) saw_lo = (ifi->ifi_flags & IFF_LOOPBACK) && (ifi->ifi_flags & IFF_UP);
            if (!strcmp(RTA_DATA(a), "eth0")) saw_eth = (ifi->ifi_flags & IFF_UP) != 0;
        }
    }
    CHECK("links: lo (loopback, up)", saw_lo);
    CHECK("links: eth0 (up)", saw_eth);
    CHECK("links: nlmsg_seq echoed", seq_ok && nlinks >= 2);

    n = dump(s, RTM_GETADDR, 12, buf, sizeof buf);
    CHECK("RTM_GETADDR dump", n > 0);
    int saw_lo4 = 0, eth_addr = 0;
    for (struct nlmsghdr *h = (struct nlmsghdr *)buf; n > 0 && NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
        if (h->nlmsg_type != RTM_NEWADDR) continue;
        struct ifaddrmsg *ifa = NLMSG_DATA(h);
        int len = IFA_PAYLOAD(h);
        char label[IFNAMSIZ] = "";
        unsigned addr = 0;
        for (struct rtattr *a = IFA_RTA(ifa); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
            if (a->rta_type == IFA_LABEL) strncpy(label, RTA_DATA(a), IFNAMSIZ - 1);
            if (a->rta_type == IFA_LOCAL) memcpy(&addr, RTA_DATA(a), 4);
        }
        if (ifa->ifa_family != AF_INET) continue;
        if (!strcmp(label, "lo") && addr == htonl(INADDR_LOOPBACK) && ifa->ifa_prefixlen == 8) saw_lo4 = 1;
        if (!strcmp(label, "eth0") && addr != 0 && ifa->ifa_prefixlen > 0) eth_addr = 1;
    }
    CHECK("addrs: lo 127.0.0.1/8", saw_lo4);
    CHECK("addrs: eth0 has an IPv4 address", eth_addr);

    // MSG_PEEK|MSG_TRUNC with a tiny buffer reports the real length (glibc).
    struct req r = { .h = { .nlmsg_len = sizeof r, .nlmsg_type = RTM_GETADDR,
                            .nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP, .nlmsg_seq = 13 } };
    send(s, &r, sizeof r, 0);
    char tiny[4];
    ssize_t full = recv(s, tiny, sizeof tiny, MSG_PEEK | MSG_TRUNC);
    CHECK("recv(MSG_PEEK|MSG_TRUNC) = real length", full > (ssize_t)sizeof tiny);
    CHECK("poll sees the reply", poll(&p, 1, 0) == 1 && (p.revents & POLLIN));
    struct iovec iov = { buf, sizeof buf };
    struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1 };
    struct sockaddr_nl from;
    m.msg_name = &from; m.msg_namelen = sizeof from;
    ssize_t got1 = recvmsg(s, &m, 0);
    CHECK("recvmsg after peek returns the same datagram", got1 == full && from.nl_family == AF_NETLINK);
    recv(s, buf, sizeof buf, 0); // NLMSG_DONE datagram

    // Unsupported request: NLMSG_ERROR.
    struct req bad = { .h = { .nlmsg_len = sizeof bad, .nlmsg_type = RTM_GETRULE, .nlmsg_flags = NLM_F_REQUEST, .nlmsg_seq = 14 } };
    send(s, &bad, sizeof bad, 0);
    ssize_t en = recv(s, buf, sizeof buf, 0);
    struct nlmsghdr *eh = (struct nlmsghdr *)buf;
    CHECK("unsupported request -> NLMSG_ERROR", en > 0 && eh->nlmsg_type == NLMSG_ERROR && eh->nlmsg_seq == 14);

    // nICEr (Firefox): RTM_GETADDR with NLM_F_ROOT|NLM_F_REQUEST (0x101) is a dump.
    struct req ra = { .h = { .nlmsg_len = sizeof ra, .nlmsg_type = RTM_GETADDR,
                             .nlmsg_flags = NLM_F_ROOT | NLM_F_REQUEST, .nlmsg_seq = 20 } };
    send(s, &ra, sizeof ra, 0);
    ssize_t na = recv(s, buf, sizeof buf, 0);
    CHECK("GETADDR ROOT|REQUEST (0x101) returns addresses",
          na > 0 && ((struct nlmsghdr *)buf)->nlmsg_type == RTM_NEWADDR);
    recv(s, buf, sizeof buf, 0); // DONE

    // Firefox NetlinkService: non-dump RTM_GETROUTE to 8.8.8.8 -> one RTM_NEWROUTE.
    struct { struct nlmsghdr h; struct rtmsg m; struct rtattr a; unsigned char d[4]; } rr;
    memset(&rr, 0, sizeof rr);
    rr.h.nlmsg_len = sizeof rr; rr.h.nlmsg_type = RTM_GETROUTE;
    rr.h.nlmsg_flags = NLM_F_REQUEST; rr.h.nlmsg_seq = 21;
    rr.m.rtm_family = AF_INET; rr.m.rtm_dst_len = 32;
    rr.a.rta_len = RTA_LENGTH(4); rr.a.rta_type = RTA_DST;
    rr.d[0] = 8; rr.d[1] = 8; rr.d[2] = 8; rr.d[3] = 8;
    send(s, &rr, sizeof rr, 0);
    ssize_t nr = recv(s, buf, sizeof buf, 0);
    struct nlmsghdr *rh = (struct nlmsghdr *)buf;
    int oif = 0, pref = 0, gw = 0;
    if (nr > 0 && rh->nlmsg_type == RTM_NEWROUTE) {
        struct rtmsg *rm = NLMSG_DATA(rh);
        int rl = RTM_PAYLOAD(rh);
        for (struct rtattr *ra2 = RTM_RTA(rm); RTA_OK(ra2, rl); ra2 = RTA_NEXT(ra2, rl)) {
            if (ra2->rta_type == RTA_OIF) oif = 1;
            if (ra2->rta_type == RTA_PREFSRC) pref = 1;
            if (ra2->rta_type == RTA_GATEWAY) gw = 1;
        }
    }
    CHECK("GETROUTE 8.8.8.8 -> RTM_NEWROUTE seq echoed",
          nr > 0 && rh->nlmsg_type == RTM_NEWROUTE && rh->nlmsg_seq == 21 && !(rh->nlmsg_flags & NLM_F_MULTI));
    CHECK("GETROUTE reply has OIF, PREFSRC and GATEWAY", oif && pref && gw);

    // RTM_GETNEIGH dump -> empty DONE.
    struct req rn = { .h = { .nlmsg_len = sizeof rn, .nlmsg_type = RTM_GETNEIGH,
                             .nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP, .nlmsg_seq = 22 } };
    send(s, &rn, sizeof rn, 0);
    ssize_t nn = recv(s, buf, sizeof buf, 0);
    CHECK("GETNEIGH dump -> NLMSG_DONE", nn > 0 && ((struct nlmsghdr *)buf)->nlmsg_type == NLMSG_DONE);

    // Non-blocking socket with nothing queued.
    int nb = socket(AF_NETLINK, SOCK_DGRAM | SOCK_NONBLOCK, NETLINK_ROUTE);
    CHECK("SOCK_DGRAM|SOCK_NONBLOCK netlink socket", nb >= 0);
    CHECK("empty recv -> EAGAIN", recv(nb, buf, sizeof buf, 0) < 0 && errno == EAGAIN);
    close(nb);
    /* RTM_GETROUTE dump: the default route and a connected route, via eth0 */
    {
        n = dump(s, RTM_GETROUTE, 21, buf, sizeof buf);
        int got_route = n > 0;
        int ndef = 0, nconn = 0, oif_ok = 1;
        for (struct nlmsghdr *h = (struct nlmsghdr *)buf; n > 0 && NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_type != RTM_NEWROUTE) continue;
            struct rtmsg *rm = NLMSG_DATA(h);
            int gw = 0, oif = 0, len = RTM_PAYLOAD(h);
            for (struct rtattr *a = RTM_RTA(rm); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
                if (a->rta_type == RTA_GATEWAY) gw = 1;
                if (a->rta_type == RTA_OIF) oif = *(int *)RTA_DATA(a);
            }
            if (rm->rtm_dst_len == 0 && gw) ndef++;
            if (rm->rtm_dst_len > 0 && !gw) nconn++;
            if (oif < 2) oif_ok = 0;
        }
        CHECK("route_dump_has_default", got_route && ndef == 1 && nconn >= 1 && oif_ok);
    }

    /* RTM_GETLINK by ifindex, with NLM_F_ACK: the link, then an NLMSG_ERROR{0} */
    {
        struct { struct nlmsghdr h; struct ifinfomsg i; } r;
        memset(&r, 0, sizeof r);
        r.h.nlmsg_len = sizeof r;
        r.h.nlmsg_type = RTM_GETLINK;
        r.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
        r.h.nlmsg_seq = 31;
        r.i.ifi_index = 1;
        int sent = send(s, &r, sizeof r, 0) == (ssize_t)sizeof r;
        char rb[2048];
        ssize_t m = 0;
        int saw_lo1 = 0, ack = 0;
        for (int tries = 0; sent && tries < 4 && !ack; tries++) {
            m = recv(s, rb, sizeof rb, 0);
            int k = (int)m;
            for (struct nlmsghdr *h = (struct nlmsghdr *)rb; m > 0 && NLMSG_OK(h, k); h = NLMSG_NEXT(h, k)) {
                if (h->nlmsg_type == RTM_NEWLINK && ((struct ifinfomsg *)NLMSG_DATA(h))->ifi_index == 1) saw_lo1 = 1;
                if (h->nlmsg_type == NLMSG_ERROR && ((struct nlmsgerr *)NLMSG_DATA(h))->error == 0) ack = 1;
            }
        }
        CHECK("getlink_by_index", sent && saw_lo1);
        CHECK("ack_flag", ack);
        r.i.ifi_index = 999;
        r.h.nlmsg_flags = NLM_F_REQUEST;
        sent = send(s, &r, sizeof r, 0) == (ssize_t)sizeof r;
        m = sent ? recv(s, rb, sizeof rb, 0) : -1;
        struct nlmsghdr *h = (struct nlmsghdr *)rb;
        CHECK("getlink_unknown_index_enodev", m > 0 && h->nlmsg_type == NLMSG_ERROR
              && ((struct nlmsgerr *)NLMSG_DATA(h))->error == -ENODEV);
    }
    close(s);

    // libc consumers.
    struct ifaddrs *ifa0 = NULL;
    CHECK("getifaddrs", getifaddrs(&ifa0) == 0);
    int g_lo = 0, g_eth = 0;
    char ethip[32] = "";
    for (struct ifaddrs *i = ifa0; i; i = i->ifa_next) {
        if (!i->ifa_addr || i->ifa_addr->sa_family != AF_INET) continue;
        struct in_addr a = ((struct sockaddr_in *)i->ifa_addr)->sin_addr;
        if (!strcmp(i->ifa_name, "lo") && a.s_addr == htonl(INADDR_LOOPBACK)) g_lo = 1;
        if (!strcmp(i->ifa_name, "eth0") && a.s_addr) { g_eth = 1; inet_ntop(AF_INET, &a, ethip, sizeof ethip); }
    }
    CHECK("getifaddrs lists lo 127.0.0.1", g_lo);
    CHECK("getifaddrs lists eth0 IPv4", g_eth);
    if (ifa0) freeifaddrs(ifa0);

    struct if_nameindex *ni = if_nameindex();
    int i_lo = 0, i_eth = 0;
    for (struct if_nameindex *i = ni; i && i->if_index; i++) {
        if (!strcmp(i->if_name, "lo") && i->if_index == 1) i_lo = 1;
        if (!strcmp(i->if_name, "eth0") && i->if_index >= 2) i_eth = 1;
    }
    CHECK("if_nameindex lists lo and eth0", i_lo && i_eth);
    if (ni) if_freenameindex(ni);
    CHECK("if_nametoindex(lo) == 1", if_nametoindex("lo") == 1);

    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct ifconf ic;
    struct ifreq ifr[16];
    memset(&ic, 0, sizeof ic);
    ic.ifc_len = sizeof ifr;
    ic.ifc_req = ifr;
    CHECK("SIOCGIFCONF", ioctl(u, SIOCGIFCONF, &ic) == 0 && ic.ifc_len >= (int)sizeof(struct ifreq));
    int c_lo = 0, c_eth = 0;
    for (int k = 0; k < ic.ifc_len / (int)sizeof(struct ifreq); k++) {
        if (!strcmp(ifr[k].ifr_name, "lo")) c_lo = 1;
        if (!strcmp(ifr[k].ifr_name, "eth0")) c_eth = 1;
    }
    CHECK("SIOCGIFCONF lists lo and eth0", c_lo && c_eth);

    struct ifreq q;
    memset(&q, 0, sizeof q);
    strcpy(q.ifr_name, "eth0");
    CHECK("SIOCGIFFLAGS eth0 UP", ioctl(u, SIOCGIFFLAGS, &q) == 0 && (q.ifr_flags & IFF_UP) && !(q.ifr_flags & IFF_LOOPBACK));
    strcpy(q.ifr_name, "lo");
    CHECK("SIOCGIFFLAGS lo LOOPBACK", ioctl(u, SIOCGIFFLAGS, &q) == 0 && (q.ifr_flags & IFF_LOOPBACK));
    strcpy(q.ifr_name, "eth0");
    char got_ip[32] = "";
    CHECK("SIOCGIFADDR eth0", ioctl(u, SIOCGIFADDR, &q) == 0);
    inet_ntop(AF_INET, &((struct sockaddr_in *)&q.ifr_addr)->sin_addr, got_ip, sizeof got_ip);
    CHECK("SIOCGIFADDR matches getifaddrs", !strcmp(got_ip, ethip));
    CHECK("SIOCGIFNETMASK eth0", ioctl(u, SIOCGIFNETMASK, &q) == 0 && ((struct sockaddr_in *)&q.ifr_netmask)->sin_addr.s_addr != 0);
    CHECK("SIOCGIFMTU eth0", ioctl(u, SIOCGIFMTU, &q) == 0 && q.ifr_mtu >= 576);
    CHECK("SIOCGIFHWADDR eth0", ioctl(u, SIOCGIFHWADDR, &q) == 0 && q.ifr_hwaddr.sa_family == ARPHRD_ETHER);
    CHECK("SIOCGIFINDEX eth0", ioctl(u, SIOCGIFINDEX, &q) == 0 && q.ifr_ifindex >= 2);
    int idx = q.ifr_ifindex;
    memset(&q, 0, sizeof q);
    q.ifr_ifindex = idx;
    CHECK("SIOCGIFNAME", ioctl(u, SIOCGIFNAME, &q) == 0 && !strcmp(q.ifr_name, "eth0"));
    strcpy(q.ifr_name, "nope0");
    CHECK("unknown interface -> ENODEV", ioctl(u, SIOCGIFFLAGS, &q) < 0 && errno == ENODEV);
    strcpy(q.ifr_name, "eth0");
    CHECK("SIOCETHTOOL -> EOPNOTSUPP", ioctl(u, 0x8946, &q) < 0 && errno == EOPNOTSUPP);
    close(u);


    printf("netlinktest: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
