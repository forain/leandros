#ifndef _SYS_SOCKET_H
#define _SYS_SOCKET_H

#include <stdint.h>
#include <stddef.h>

#define AF_UNIX 1
#define SOCK_STREAM 1

struct sockaddr {
    uint16_t sa_family;
    char sa_data[14];
};

struct sockaddr_un {
    uint16_t sun_family;
    char sun_path[108];
};

int socket(int domain, int type, int protocol);
int connect(int sockfd, const struct sockaddr *addr, uint32_t addrlen);

#endif
