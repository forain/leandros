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
#include <sys/wait.h>
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

// AF_UNIX pathname sockets must work on any filesystem (f2fs root, tmpfs):
// bind creates an S_IFSOCK node, connect reaches the listener, bind on an
// existing path is EADDRINUSE, unlink makes the listener unreachable by
// path, and a node with no listener (what a reboot leaves) is ECONNREFUSED.
static int usock(const char *path, int *err, int do_bind, int do_listen)
{
    int so = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a = { .sun_family = AF_UNIX };
    strncpy(a.sun_path, path, sizeof a.sun_path - 1);
    errno = 0;
    int r = do_bind ? bind(so, (struct sockaddr *)&a, sizeof a) : connect(so, (struct sockaddr *)&a, sizeof a);
    *err = errno;
    if (r != 0) { close(so); return -1; }
    if (do_listen) listen(so, 4);
    return so;
}

static void unix_on_fs(const char *tag, const char *base)
{
    char d[96], p[128], s[160];
    int e;
    snprintf(d, sizeof d, "%s/usock.%d", base, getpid());
    rmdir(d);
    mkdir(d, 0755);
    snprintf(p, sizeof p, "%s/s", d);
    int ls = usock(p, &e, 1, 1);
    snprintf(s, sizeof s, "%s: bind", tag); errno = e; ok(s, ls >= 0);
    struct stat st;
    snprintf(s, sizeof s, "%s: node is S_IFSOCK", tag); ok(s, stat(p, &st) == 0 && S_ISSOCK(st.st_mode));
    int c = usock(p, &e, 0, 0);
    snprintf(s, sizeof s, "%s: connect", tag); errno = e; ok(s, c >= 0);
    int acc = ls >= 0 ? accept(ls, NULL, NULL) : -1;
    char b[8] = {0};
    snprintf(s, sizeof s, "%s: send/recv", tag);
    ok(s, c >= 0 && acc >= 0 && write(c, "hi", 2) == 2 && read(acc, b, 8) == 2 && !memcmp(b, "hi", 2));
    int dup = usock(p, &e, 1, 0);
    snprintf(s, sizeof s, "%s: bind existing -> EADDRINUSE", tag); errno = e; ok(s, dup < 0 && e == EADDRINUSE);
    // non-root caller may not create a node in a root-owned 0755 dir
    pid_t k = fork();
    if (k == 0) {
        char q[128]; int e2;
        snprintf(q, sizeof q, "%s/denied", d);
        if (setuid(1000) != 0) _exit(2);
        usock(q, &e2, 1, 0);
        _exit(e2 == EACCES ? 0 : 1);
    }
    int stt = 0; waitpid(k, &stt, 0);
    snprintf(s, sizeof s, "%s: bind without parent W+X -> EACCES", tag); ok(s, WIFEXITED(stt) && WEXITSTATUS(stt) == 0);
    close(c); if (acc >= 0) close(acc);
    unlink(p);
    c = usock(p, &e, 0, 0);
    snprintf(s, sizeof s, "%s: connect after unlink -> ENOENT", tag); errno = e; ok(s, c < 0 && e == ENOENT);
    if (ls >= 0) close(ls);
    // stale node: S_IFSOCK inode with no listener
    int mk = mknod(p, S_IFSOCK | 0777, 0);
    c = usock(p, &e, 0, 0);
    snprintf(s, sizeof s, "%s: stale node -> ECONNREFUSED", tag); errno = e; ok(s, mk == 0 && c < 0 && e == ECONNREFUSED);
    // rebind over a stale node name is EADDRINUSE until it is unlinked
    int rb = usock(p, &e, 1, 0);
    snprintf(s, sizeof s, "%s: bind over stale node -> EADDRINUSE", tag); errno = e; ok(s, rb < 0 && e == EADDRINUSE);
    unlink(p);
    // a fresh bind after unlink reaches only the new listener
    int n1 = usock(p, &e, 1, 1);
    unlink(p);
    int n2 = usock(p, &e, 1, 1);
    c = usock(p, &e, 0, 0);
    snprintf(s, sizeof s, "%s: rebind after unlink", tag); ok(s, n1 >= 0 && n2 >= 0 && c >= 0);
    char x = 'z'; int a2 = n2 >= 0 ? accept(n2, NULL, NULL) : -1;
    snprintf(s, sizeof s, "%s: connect reaches newest listener", tag);
    ok(s, c >= 0 && a2 >= 0 && write(c, &x, 1) == 1);
    unlink(p); rmdir(d);
}

int main(void)
{
    dirfd_proc_path("o_path", O_PATH | O_DIRECTORY | O_CLOEXEC);
    dirfd_proc_path("o_rdonly", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    chmod_socket_fd();
    unix_on_fs("unix tmpfs", "/tmp");
    unix_on_fs("unix f2fs", "/root");
    printf(fails ? "conmontest: %d FAILED\n" : "conmontest: ALL PASS\n", fails);
    return fails != 0;
}
