// conmontest: syscall sequences conmon (container monitor) performs at start.
// Prints "<step>: PASS" / "<step>: FAIL (errno N)" and "conmontest: ALL PASS".
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

static int fails;
static void ok(const char *s, int cond)
{
    if (cond) printf("%s: PASS\n", s);
    else { printf("%s: FAIL (errno %d)\n", s, errno); fails++; }
}

// Linux: chmod/stat/bind through /proc/self/fd/<O_PATH dir fd> act on the directory.
static void dirfd_proc_path(const char *tag, int oflags)
{
    char d[64], p[64], q[96], s[96];
    snprintf(d, sizeof d, "/tmp/conmontest.%d", getpid());
    rmdir(d);
    mkdir(d, 0755);
    int fd = open(d, oflags);
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    snprintf(s, sizeof s, "%s: open dir", tag); ok(s, fd >= 0);
    struct stat st;
    snprintf(s, sizeof s, "%s: stat /proc/self/fd/N", tag); ok(s, stat(p, &st) == 0 && S_ISDIR(st.st_mode));
    snprintf(s, sizeof s, "%s: chmod /proc/self/fd/N", tag); ok(s, chmod(p, 0700) == 0);
    snprintf(s, sizeof s, "%s: chmod took effect", tag);
    ok(s, stat(d, &st) == 0 && (st.st_mode & 0777) == 0700);
    snprintf(q, sizeof q, "%s/attach", p);
    int so = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    struct sockaddr_un a = { .sun_family = AF_UNIX };
    strncpy(a.sun_path, q, sizeof a.sun_path - 1);
    snprintf(s, sizeof s, "%s: bind /proc/self/fd/N/attach", tag);
    ok(s, bind(so, (struct sockaddr *)&a, sizeof a) == 0);
    char r[96]; snprintf(r, sizeof r, "%s/attach", d);
    snprintf(s, sizeof s, "%s: socket visible in dir", tag); ok(s, stat(r, &st) == 0 && S_ISSOCK(st.st_mode));
    close(so); unlink(r); close(fd); rmdir(d);
}

// conmon: bind a unix socket, then chmod("/proc/self/fd/<socket fd>", 0700).
// On Linux the magic link resolves to the socket's anonymous inode: success.
static void chmod_socket_fd(void)
{
    char d[64], r[96], p[64];
    snprintf(d, sizeof d, "/tmp/conmontest.s%d", getpid());
    rmdir(d); mkdir(d, 0755);
    snprintf(r, sizeof r, "%s/console.sock", d);
    int so = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    struct sockaddr_un a = { .sun_family = AF_UNIX };
    strncpy(a.sun_path, r, sizeof a.sun_path - 1);
    ok("sock: bind", bind(so, (struct sockaddr *)&a, sizeof a) == 0);
    snprintf(p, sizeof p, "/proc/self/fd/%d", so);
    struct stat st;
    ok("sock: stat /proc/self/fd/N is a socket", stat(p, &st) == 0 && S_ISSOCK(st.st_mode));
    ok("sock: chmod /proc/self/fd/N", chmod(p, 0700) == 0);
    close(so); unlink(r); rmdir(d);
}

int main(void)
{
    dirfd_proc_path("o_path", O_PATH | O_DIRECTORY | O_CLOEXEC);
    dirfd_proc_path("o_rdonly", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    chmod_socket_fd();
    printf(fails ? "conmontest: %d FAILED\n" : "conmontest: ALL PASS\n", fails);
    return fails != 0;
}
