/* nettool: resolve / plain-HTTP GET / AF_INET6 probe, for LeandrOS network debugging.
 * Build (static musl): zig cc -target <arch>-linux-musl -O2 -static nettool.c -o nettool-<arch>
 * Stage: put nettool-aarch64 / nettool-x86_64 in a dir and build with LEANDROS_EXTRA_BIN=<dir>.
 * Usage: nettool resolve NAME | nettool get HOST PORT PATH | nettool v6 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <poll.h>
#include <netdb.h>
#include <time.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>

static long ms(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1000+t.tv_nsec/1000000;}

static int resolve(const char *name, int fam) {
    struct addrinfo h = {0}, *r, *p;
    h.ai_family = fam; h.ai_socktype = SOCK_STREAM; h.ai_flags = AI_ADDRCONFIG;
    long t0 = ms();
    int e = getaddrinfo(name, "80", &h, &r);
    printf("getaddrinfo(%s, fam=%d) = %d (%s) in %ld ms\n", name, fam, e, e ? gai_strerror(e) : "ok", ms()-t0);
    if (e) return 1;
    for (p = r; p; p = p->ai_next) {
        char buf[64];
        void *a = p->ai_family == AF_INET ? (void*)&((struct sockaddr_in*)p->ai_addr)->sin_addr
                                          : (void*)&((struct sockaddr_in6*)p->ai_addr)->sin6_addr;
        inet_ntop(p->ai_family, a, buf, sizeof buf);
        printf("  fam=%d %s\n", p->ai_family, buf);
    }
    freeaddrinfo(r);
    return 0;
}

static int get(const char *host, const char *port, const char *path) {
    struct addrinfo h = {0}, *r;
    h.ai_family = AF_INET; h.ai_socktype = SOCK_STREAM;
    int e = getaddrinfo(host, port, &h, &r);
    if (e) { printf("gai: %s\n", gai_strerror(e)); return 1; }
    int fd = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    printf("socket = %d errno=%d\n", fd, errno);
    int one = 1;
    printf("setsockopt NODELAY = %d errno=%d\n", setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one), errno);
    printf("setsockopt KEEPALIVE = %d errno=%d\n", setsockopt(fd, SOL_SOCKET, SO_KEEPALIVE, &one, sizeof one), errno);
    long t0 = ms();
    errno = 0;
    int c = connect(fd, r->ai_addr, r->ai_addrlen);
    printf("connect = %d errno=%d (%s)\n", c, errno, strerror(errno));
    struct pollfd pf = { fd, POLLOUT, 0 };
    int pr = poll(&pf, 1, 10000);
    printf("poll(OUT) = %d revents=0x%x after %ld ms\n", pr, pf.revents, ms()-t0);
    int soerr = -1; socklen_t sl = sizeof soerr;
    printf("getsockopt SO_ERROR = %d soerr=%d\n", getsockopt(fd, SOL_SOCKET, SO_ERROR, &soerr, &sl), soerr);
    struct sockaddr_in sa; socklen_t al = sizeof sa;
    if (getsockname(fd, (void*)&sa, &al) == 0) printf("local %s:%d\n", inet_ntoa(sa.sin_addr), ntohs(sa.sin_port));
    else printf("getsockname errno=%d\n", errno);
    al = sizeof sa;
    if (getpeername(fd, (void*)&sa, &al) == 0) printf("peer %s:%d\n", inet_ntoa(sa.sin_addr), ntohs(sa.sin_port));
    else printf("getpeername errno=%d\n", errno);
    char req[512];
    int n = snprintf(req, sizeof req, "GET %s HTTP/1.0\r\nHost: %s\r\nUser-Agent: nettool\r\n\r\n", path, host);
    ssize_t w = send(fd, req, n, MSG_NOSIGNAL);
    printf("send = %zd errno=%d\n", w, errno);
    long total = 0; char buf[4096]; int first = 1;
    for (;;) {
        pf.events = POLLIN; pf.revents = 0;
        pr = poll(&pf, 1, 10000);
        if (pr <= 0) { printf("poll(IN) = %d (timeout?)\n", pr); break; }
        ssize_t k = recv(fd, buf, sizeof buf, 0);
        if (k < 0) { if (errno == EAGAIN) continue; printf("recv err %d revents=0x%x\n", errno, pf.revents); break; }
        if (k == 0) { printf("EOF revents=0x%x\n", pf.revents); break; }
        if (first) { char *nl = memchr(buf, '\n', k); printf("first line: %.*s\n", nl ? (int)(nl-buf) : (int)k, buf); first = 0; }
        total += k;
    }
    printf("total %ld bytes in %ld ms\n", total, ms()-t0);
    close(fd);
    return 0;
}

int main(int argc, char **argv) {
    if (argc >= 3 && !strcmp(argv[1], "resolve")) {
        resolve(argv[2], AF_UNSPEC); resolve(argv[2], AF_INET); return 0;
    }
    if (argc >= 5 && !strcmp(argv[1], "get")) return get(argv[2], argv[3], argv[4]);
    if (argc >= 2 && !strcmp(argv[1], "v6")) {
        int fd = socket(AF_INET6, SOCK_STREAM, 0);
        printf("socket(AF_INET6,STREAM) = %d errno=%d\n", fd, errno);
        fd = socket(AF_INET6, SOCK_DGRAM, 0);
        printf("socket(AF_INET6,DGRAM) = %d errno=%d\n", fd, errno);
        return 0;
    }
    fprintf(stderr, "usage: nettool resolve NAME | get HOST PORT PATH | v6\n");
    return 2;
}
