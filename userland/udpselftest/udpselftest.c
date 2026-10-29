// udpselftest: a UDP datagram to one of our own interface addresses is
// delivered locally, and a datagram whose next hop never answers ARP does not
// block the socket's later sends (lane udpself).
#include <arpa/inet.h>
#include <errno.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int npass, nfail;
#define CHECK(name, cond) do { if (cond) { npass++; printf("PASS %s\n", name); } \
    else { nfail++; printf("FAIL %s (errno %d)\n", name, errno); } } while (0)

static struct in_addr own, mask;

static int udp_bound(struct in_addr ip, int port, struct sockaddr_in *out) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port), .sin_addr = ip };
    if (s < 0 || bind(s, (struct sockaddr *)&a, sizeof a) < 0) { if (s >= 0) close(s); return -1; }
    socklen_t l = sizeof a;
    getsockname(s, (struct sockaddr *)&a, &l);
    if (out) *out = a;
    return s;
}

static int recv_ms(int s, char *buf, int cap, int ms) {
    struct pollfd p = { .fd = s, .events = POLLIN };
    if (poll(&p, 1, ms) <= 0) return -1;
    return (int)recv(s, buf, cap, 0);
}

static struct in_addr nameserver(void) {
    struct in_addr a;
    FILE *f = fopen("/etc/resolv.conf", "r");
    char line[128], ip[64];
    if (f) {
        while (fgets(line, sizeof line, f))
            if (sscanf(line, "nameserver %63s", ip) == 1 && inet_pton(AF_INET, ip, &a) == 1) { fclose(f); return a; }
        fclose(f);
    }
    inet_pton(AF_INET, "10.0.2.3", &a);
    return a;
}

int main(void) {
    struct ifaddrs *ifa, *i;
    int have = 0;
    if (getifaddrs(&ifa) == 0) {
        for (i = ifa; i; i = i->ifa_next) {
            if (!i->ifa_addr || i->ifa_addr->sa_family != AF_INET || (i->ifa_flags & IFF_LOOPBACK)) continue;
            own = ((struct sockaddr_in *)i->ifa_addr)->sin_addr;
            mask = ((struct sockaddr_in *)i->ifa_netmask)->sin_addr;
            have = 1;
            break;
        }
    }
    if (!have) { printf("udpselftest: no non-loopback IPv4 address, skipped\n"); return 0; }
    char b[64];
    printf("own %s\n", inet_ntop(AF_INET, &own, b, sizeof b));
    char buf[512];

    // 1. unconnected send to our own interface address, and the reply.
    struct sockaddr_in ba, aa;
    int B = udp_bound(own, 0, &ba), A = udp_bound(own, 0, &aa);
    CHECK("bind_own", A >= 0 && B >= 0);
    CHECK("sendto_own", sendto(A, "ping", 4, 0, (struct sockaddr *)&ba, sizeof ba) == 4);
    int n = recv_ms(B, buf, sizeof buf, 2000);
    CHECK("own_delivered", n == 4 && memcmp(buf, "ping", 4) == 0);
    CHECK("own_reply", sendto(B, "pong", 4, 0, (struct sockaddr *)&aa, sizeof aa) == 4
          && recv_ms(A, buf, sizeof buf, 2000) == 4);
    // Wildcard-bound sender to our own address.
    struct in_addr any = { 0 };
    int W = udp_bound(any, 0, NULL);
    CHECK("wildcard_to_own", sendto(W, "wild", 4, 0, (struct sockaddr *)&ba, sizeof ba) == 4
          && recv_ms(B, buf, sizeof buf, 2000) == 4);

    // 2. connected UDP socket to our own address.
    struct sockaddr_in da;
    int D = udp_bound(own, 0, &da), C = udp_bound(own, 0, NULL);
    CHECK("connect_own", connect(C, (struct sockaddr *)&da, sizeof da) == 0);
    CHECK("send_connected_own", send(C, "conn", 4, 0) == 4 && recv_ms(D, buf, sizeof buf, 2000) == 4);

    // 3. an on-link address nobody answers ARP for, then a live peer.
    struct in_addr dead;
    dead.s_addr = (own.s_addr & mask.s_addr) | htonl(254);
    if (dead.s_addr == own.s_addr) dead.s_addr = (own.s_addr & mask.s_addr) | htonl(253);
    struct sockaddr_in dd = { .sin_family = AF_INET, .sin_port = htons(9999), .sin_addr = dead };
    struct sockaddr_in ns = { .sin_family = AF_INET, .sin_port = htons(53), .sin_addr = nameserver() };
    int E = udp_bound(any, 0, NULL);
    CHECK("sendto_dead", sendto(E, "x", 1, 0, (struct sockaddr *)&dd, sizeof dd) == 1);
    static const unsigned char q[] = { 0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0,
        7, 'e', 'x', 'a', 'm', 'p', 'l', 'e', 3, 'c', 'o', 'm', 0, 0, 1, 0, 1 };
    CHECK("sendto_after_dead", sendto(E, q, sizeof q, 0, (struct sockaddr *)&ns, sizeof ns) == (ssize_t)sizeof q);
    n = recv_ms(E, buf, sizeof buf, 10000);
    CHECK("reply_after_dead", n >= 12 && (unsigned char)buf[0] == 0x12 && (unsigned char)buf[1] == 0x34);

    printf("udpselftest: %d passed, %d failed\n", npass, nfail);
    return nfail != 0;
}
