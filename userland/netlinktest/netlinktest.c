// netlinktest: NETLINK_ROUTE (RTM_GETLINK/RTM_GETADDR dumps), getifaddrs,
// if_nameindex and the SIOCGIF* ioctls. nICEr (Firefox WebRTC) gathers its
// local ICE candidates through exactly these.
#include <arpa/inet.h>
#include <errno.h>
#include <ifaddrs.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <net/if_arp.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

static int npass, nfail;
#define CHECK(name, cond) do { if (cond) { npass++; printf("PASS %s\n", name); } \
    else { nfail++; printf("FAIL %s (errno %d)\n", name, errno); } } while (0)

struct req { struct nlmsghdr h; struct rtgenmsg g; };

static int dump(int s, int type, unsigned seq, char *buf, int cap) {
    struct req r;
    memset(&r, 0, sizeof r);
    r.h.nlmsg_len = sizeof r;
    r.h.nlmsg_type = type;
    r.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    r.h.nlmsg_seq = seq;
    r.g.rtgen_family = AF_UNSPEC;
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

int main(void) {
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

    // Non-blocking socket with nothing queued.
    int nb = socket(AF_NETLINK, SOCK_DGRAM | SOCK_NONBLOCK, NETLINK_ROUTE);
    CHECK("SOCK_DGRAM|SOCK_NONBLOCK netlink socket", nb >= 0);
    CHECK("empty recv -> EAGAIN", recv(nb, buf, sizeof buf, 0) < 0 && errno == EAGAIN);
    close(nb);
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
