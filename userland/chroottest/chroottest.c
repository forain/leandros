/*
 * chroottest — what a toolchain running in a chroot needs from the kernel,
 * with the results Linux gives. Like syscalltest, the same file builds for
 * the host (cc chroottest.c) and for LeandrOS (zig cc -target
 * <arch>-linux-musl -static). Run as root (chroot/mount need it); as another
 * user the root-only cases SKIP.
 *
 *   chroottest [-d DIR]     work directory (default /var/tmp, a disk
 *                           filesystem; it must not be tmpfs: the long-path
 *                           cases need a filesystem with PATH_MAX paths)
 *
 * openat2:
 *   openat2_how          open_how size versioning (EINVAL / E2BIG), flags,
 *                        mode and resolve validation, and that the flags in
 *                        the struct are the ones honoured (O_CREAT|O_EXCL)
 *   openat2_beneath      RESOLVE_BENEATH: "..", absolute paths and symlinks
 *                        leaving the directory are EXDEV, inside ones work
 *   openat2_in_root      RESOLVE_IN_ROOT: "/", ".." and absolute symlinks
 *                        stay in the directory
 *   openat2_no_symlinks  RESOLVE_NO_SYMLINKS: ELOOP; O_PATH|O_NOFOLLOW opens
 *                        a final link itself
 *   openat2_no_magic     RESOLVE_NO_MAGICLINKS: /proc/self/exe is ELOOP
 * vfork / clone:
 *   vfork_shared         vfork(): the parent is suspended until the child
 *                        exits, and sees the child's stores (CLONE_VM)
 *   vfork_exec           vfork() + execve, exit status through wait
 *   clone_vfork_novm     clone(CLONE_VFORK|SIGCHLD) without CLONE_VM: the
 *                        parent still waits for the child's exit
 *   clone3_errors        clone3 size versioning and argument validation
 *   clone3_fork          clone3 as fork: exit status, CLONE_PARENT_SETTID
 *                        and CLONE_PIDFD through their separate pointers
 *   clone3_vfork         clone3(CLONE_VM|CLONE_VFORK) shares memory
 *   clone_pidfd_emfile   clone(CLONE_PIDFD) with a full fd table: EMFILE and
 *                        no child left behind
 * paths:
 *   long_path            a path > 255 (and ~1000) bytes: mkdir, open, stat,
 *                        chdir, getcwd, /proc/self/fd, openat; PATH_MAX and
 *                        NAME_MAX limits are ENAMETOOLONG
 *   long_path_chroot     the same > 255 bytes below a chroot whose root is
 *                        itself > 255 bytes deep
 * chroot:
 *   chroot_proc          mount -t proc onto <root>/proc from outside;
 *                        inside: /proc/self/{exe,cwd,root,fd/N} read
 *                        chroot-relative, /proc/self/status opens
 *   chroot_dev_inside    mount -t devtmpfs /dev from inside the chroot;
 *                        /dev/null works; /proc/mounts lists both
 *   chroot_bind          mount --bind of a host directory into the chroot
 *   chroot_umount        umount of the chroot's /proc: /proc/self is gone
 *   chroot_unix_socket   AF_UNIX bind in the chroot (absolute, and relative
 *                        to the cwd) lands in the jail, under its bind-mounted
 *                        /tmp; the host connects to it there
 *
 * Output: "<name>: PASS|FAIL ...|SKIP ..." per case and a summary
 * "chroottest: N passed, M failed, K skipped". Exit status = failures.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_openat2
#define SYS_openat2 437
#endif
#ifndef SYS_clone3
#define SYS_clone3 435
#endif
#ifndef CLONE_PIDFD
#define CLONE_PIDFD 0x1000
#endif
#ifndef RESOLVE_NO_XDEV
#define RESOLVE_NO_XDEV       0x01
#define RESOLVE_NO_MAGICLINKS 0x02
#define RESOLVE_NO_SYMLINKS   0x04
#define RESOLVE_BENEATH       0x08
#define RESOLVE_IN_ROOT       0x10
#endif

struct how { uint64_t flags, mode, resolve; };
struct cargs {
    uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls,
             set_tid, set_tid_size, cgroup;
};

static int npass, nfail, nskip;
static void pass(const char *n) { printf("%s: PASS\n", n); npass++; fflush(stdout); }
static void skip(const char *n, const char *why) { printf("%s: SKIP %s\n", n, why); nskip++; fflush(stdout); }
static void fail(const char *n, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
static void fail(const char *n, const char *fmt, ...) {
    va_list ap;
    printf("%s: FAIL ", n);
    va_start(ap, fmt); vprintf(fmt, ap); va_end(ap);
    printf("\n"); nfail++; fflush(stdout);
}

static long ct_openat2(int dfd, const char *p, struct how *h, size_t sz) {
    return syscall(SYS_openat2, dfd, p, h, sz);
}
static long clone3(struct cargs *a, size_t sz) { return syscall(SYS_clone3, a, sz); }

/* Expect `r` to be -1 with errno `e`; record a failure message otherwise. */
#define EXPECT_ERR(name, r, e, what) do { long _r = (r); int _e = errno; \
    if (_r != -1 || _e != (e)) { fail(name, "%s: got %ld errno %d, want errno %d", what, _r, _r == -1 ? _e : 0, e); return; } \
    } while (0)
#define EXPECT_OK(name, r, what) do { long _r = (r); \
    if (_r < 0) { fail(name, "%s: errno %d", what, errno); return; } } while (0)

static char work[512];           /* per-run work directory */
static const char *self_path;    /* our own binary */

static void rm_rf(const char *p) {
    char cmd[8192];
    struct stat st;
    if (lstat(p, &st) < 0) return;
    if (S_ISDIR(st.st_mode)) {
        int fd = open(p, O_RDONLY | O_DIRECTORY);
        if (fd >= 0) {
            char buf[4096];
            for (;;) {
                long n = syscall(SYS_getdents64, fd, buf, sizeof buf);
                if (n <= 0) break;
                for (long off = 0; off < n;) {
                    unsigned short reclen = *(unsigned short *)(buf + off + 16);
                    const char *nm = buf + off + 19;
                    if (strcmp(nm, ".") && strcmp(nm, "..")) {
                        snprintf(cmd, sizeof cmd, "%s/%s", p, nm);
                        rm_rf(cmd);
                    }
                    off += reclen;
                }
            }
            close(fd);
        }
        rmdir(p);
    } else {
        unlink(p);
    }
}

static int write_file(const char *p, const char *s) {
    int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return -1;
    ssize_t w = write(fd, s, strlen(s));
    close(fd);
    return w == (ssize_t)strlen(s) ? 0 : -1;
}

/* ── openat2 ──────────────────────────────────────────────────────────── */

static void openat2_how(void) {
    const char *N = "openat2_how";
    char p[600];
    snprintf(p, sizeof p, "%s/how", work);
    unlink(p);
    struct { struct how h; uint64_t ext[2]; } big;
    memset(&big, 0, sizeof big);
    big.h.flags = O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC;
    big.h.mode = 0600;
    EXPECT_ERR(N, ct_openat2(AT_FDCWD, p, &big.h, 16), EINVAL, "size 16");
    EXPECT_ERR(N, ct_openat2(AT_FDCWD, p, &big.h, 8192), E2BIG, "size 8192");
    big.ext[1] = 1;
    EXPECT_ERR(N, ct_openat2(AT_FDCWD, p, &big.h, sizeof big), E2BIG, "nonzero tail");
    big.ext[1] = 0;
    /* The struct's flags are honoured: O_CREAT|O_EXCL creates, then EEXIST. */
    long fd = ct_openat2(AT_FDCWD, p, &big.h, sizeof big);
    EXPECT_OK(N, fd, "create (zero tail, size 40)");
    struct stat st;
    /* umask may only clear bits of the requested 0600 */
    if (fstat((int)fd, &st) < 0 || (st.st_mode & 07777 & ~0600) != 0) {
        fail(N, "mode %o", st.st_mode & 07777); close((int)fd); return;
    }
    if (!(fcntl((int)fd, F_GETFD) & FD_CLOEXEC)) { fail(N, "O_CLOEXEC not set"); close((int)fd); return; }
    close((int)fd);
    EXPECT_ERR(N, ct_openat2(AT_FDCWD, p, &big.h, sizeof(struct how)), EEXIST, "O_EXCL again");
    struct how h = { O_RDONLY, 0644, 0 };
    EXPECT_ERR(N, ct_openat2(AT_FDCWD, p, &h, sizeof h), EINVAL, "mode without O_CREAT");
    h = (struct how){ O_RDONLY, 0, 0x40 };
    EXPECT_ERR(N, ct_openat2(AT_FDCWD, p, &h, sizeof h), EINVAL, "unknown resolve bit");
    h = (struct how){ O_RDONLY, 0, RESOLVE_BENEATH | RESOLVE_IN_ROOT };
    EXPECT_ERR(N, ct_openat2(AT_FDCWD, p, &h, sizeof h), EINVAL, "BENEATH|IN_ROOT");
    h = (struct how){ (uint64_t)1 << 40, 0, 0 };
    EXPECT_ERR(N, ct_openat2(AT_FDCWD, p, &h, sizeof h), EINVAL, "flags above 32 bits");
    h = (struct how){ O_PATH | O_RDWR, 0, 0 };
    EXPECT_ERR(N, ct_openat2(AT_FDCWD, p, &h, sizeof h), EINVAL, "O_PATH|O_RDWR");
    h = (struct how){ O_RDONLY, 0, 0 };
    fd = ct_openat2(AT_FDCWD, p, &h, sizeof h);
    EXPECT_OK(N, fd, "plain O_RDONLY");
    if (write((int)fd, "x", 1) != -1) { fail(N, "O_RDONLY fd writable"); close((int)fd); return; }
    close((int)fd);
    unlink(p);
    pass(N);
}

/* d/ { f, sub/g, up -> ../outside, abs -> /etc, inl -> sub/g, absin -> /f } */
static int make_tree(char *d, size_t dl) {
    snprintf(d, dl, "%s/tree", work);
    rm_rf(d);
    char p[700];
    if (mkdir(d, 0755) < 0) return -1;
    snprintf(p, sizeof p, "%s/f", d);       if (write_file(p, "f") < 0) return -1;
    snprintf(p, sizeof p, "%s/sub", d);     if (mkdir(p, 0755) < 0) return -1;
    snprintf(p, sizeof p, "%s/sub/g", d);   if (write_file(p, "g") < 0) return -1;
    snprintf(p, sizeof p, "%s/outside", work); if (write_file(p, "o") < 0) return -1;
    snprintf(p, sizeof p, "%s/up", d);      if (symlink("../outside", p) < 0) return -1;
    snprintf(p, sizeof p, "%s/abs", d);     if (symlink("/etc", p) < 0) return -1;
    snprintf(p, sizeof p, "%s/inl", d);     if (symlink("sub/g", p) < 0) return -1;
    snprintf(p, sizeof p, "%s/absin", d);   if (symlink("/f", p) < 0) return -1;
    return 0;
}

static int read1(int fd) { char c = 0; return read(fd, &c, 1) == 1 ? c : -1; }

static void openat2_beneath(void) {
    const char *N = "openat2_beneath";
    char d[600];
    if (make_tree(d, sizeof d) < 0) { fail(N, "setup: errno %d", errno); return; }
    int dfd = open(d, O_RDONLY | O_DIRECTORY);
    EXPECT_OK(N, dfd, "open tree");
    struct how h = { O_RDONLY, 0, RESOLVE_BENEATH };
    long fd = ct_openat2(dfd, "f", &h, sizeof h);
    EXPECT_OK(N, fd, "f");
    if (read1((int)fd) != 'f') { fail(N, "f content"); return; }
    close((int)fd);
    fd = ct_openat2(dfd, "sub/../inl", &h, sizeof h);
    EXPECT_OK(N, fd, "sub/../inl");
    if (read1((int)fd) != 'g') { fail(N, "inl content"); return; }
    close((int)fd);
    EXPECT_ERR(N, ct_openat2(dfd, "../outside", &h, sizeof h), EXDEV, "../outside");
    EXPECT_ERR(N, ct_openat2(dfd, "sub/../../outside", &h, sizeof h), EXDEV, "sub/../../outside");
    EXPECT_ERR(N, ct_openat2(dfd, "/etc/passwd", &h, sizeof h), EXDEV, "absolute");
    EXPECT_ERR(N, ct_openat2(dfd, "up", &h, sizeof h), EXDEV, "symlink up");
    EXPECT_ERR(N, ct_openat2(dfd, "abs/passwd", &h, sizeof h), EXDEV, "absolute symlink");
    close(dfd);
    pass(N);
}

static void openat2_in_root(void) {
    const char *N = "openat2_in_root";
    char d[600];
    if (make_tree(d, sizeof d) < 0) { fail(N, "setup: errno %d", errno); return; }
    int dfd = open(d, O_RDONLY | O_DIRECTORY);
    EXPECT_OK(N, dfd, "open tree");
    struct how h = { O_RDONLY, 0, RESOLVE_IN_ROOT };
    const char *names[] = { "/f", "../../f", "absin", "sub/../../../f", "/sub/g" };
    const char want[] = { 'f', 'f', 'f', 'f', 'g' };
    for (int i = 0; i < 5; i++) {
        long fd = ct_openat2(dfd, names[i], &h, sizeof h);
        if (fd < 0) { fail(N, "%s: errno %d", names[i], errno); close(dfd); return; }
        int c = read1((int)fd);
        close((int)fd);
        if (c != want[i]) { fail(N, "%s read %c", names[i], c); close(dfd); return; }
    }
    /* "up" -> ../outside, clamped at the root: tree/outside does not exist. */
    EXPECT_ERR(N, ct_openat2(dfd, "up", &h, sizeof h), ENOENT, "up (clamped)");
    close(dfd);
    pass(N);
}

static void openat2_no_symlinks(void) {
    const char *N = "openat2_no_symlinks";
    char d[600];
    if (make_tree(d, sizeof d) < 0) { fail(N, "setup: errno %d", errno); return; }
    int dfd = open(d, O_RDONLY | O_DIRECTORY);
    EXPECT_OK(N, dfd, "open tree");
    struct how h = { O_RDONLY, 0, RESOLVE_NO_SYMLINKS };
    long fd = ct_openat2(dfd, "sub/g", &h, sizeof h);
    EXPECT_OK(N, fd, "sub/g");
    close((int)fd);
    EXPECT_ERR(N, ct_openat2(dfd, "inl", &h, sizeof h), ELOOP, "final symlink");
    EXPECT_ERR(N, ct_openat2(dfd, "abs/passwd", &h, sizeof h), ELOOP, "intermediate symlink");
    h.flags = O_PATH | O_NOFOLLOW;
    fd = ct_openat2(dfd, "inl", &h, sizeof h);
    EXPECT_OK(N, fd, "O_PATH|O_NOFOLLOW on the link");
    close((int)fd);
    close(dfd);
    pass(N);
}

static void openat2_no_magic(void) {
    const char *N = "openat2_no_magic";
    struct how h = { O_RDONLY, 0, RESOLVE_NO_MAGICLINKS };
    EXPECT_ERR(N, ct_openat2(AT_FDCWD, "/proc/self/exe", &h, sizeof h), ELOOP, "/proc/self/exe");
    h.resolve = 0;
    long fd = ct_openat2(AT_FDCWD, "/proc/self/exe", &h, sizeof h);
    EXPECT_OK(N, fd, "/proc/self/exe without the flag");
    close((int)fd);
    pass(N);
}

/* ── vfork / clone ────────────────────────────────────────────────────── */

static volatile int shared_word;

static void vfork_shared(void) {
    const char *N = "vfork_shared";
    shared_word = 0;
    pid_t pid = vfork();
    if (pid == 0) {
        /* Parent must still be suspended: it would otherwise run ahead. */
        struct timespec ts = { 0, 100 * 1000 * 1000 };
        syscall(SYS_nanosleep, &ts, NULL);
        shared_word = 42;
        _exit(3);
    }
    if (pid < 0) { fail(N, "vfork: errno %d", errno); return; }
    int seen = shared_word; /* first thing after resuming */
    int st;
    if (waitpid(pid, &st, 0) != pid) { fail(N, "waitpid errno %d", errno); return; }
    if (seen != 42) { fail(N, "parent saw %d before the child's exit (not suspended / not shared)", seen); return; }
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 3) { fail(N, "status %#x", st); return; }
    pass(N);
}

static void vfork_exec(void) {
    const char *N = "vfork_exec";
    pid_t pid = vfork();
    if (pid == 0) {
        char *av[] = { (char *)self_path, "--exit", "9", NULL };
        execv(self_path, av);
        _exit(127);
    }
    if (pid < 0) { fail(N, "vfork: errno %d", errno); return; }
    int st;
    if (waitpid(pid, &st, 0) != pid) { fail(N, "waitpid errno %d", errno); return; }
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 9) { fail(N, "status %#x", st); return; }
    pass(N);
}

static void clone_vfork_novm(void) {
    const char *N = "clone_vfork_novm";
    int p[2];
    if (pipe2(p, O_NONBLOCK) < 0) { fail(N, "pipe"); return; }
#if defined(__x86_64__)
    long pid = syscall(SYS_clone, CLONE_VFORK | SIGCHLD, 0, NULL, NULL, 0);
#else
    long pid = syscall(SYS_clone, CLONE_VFORK | SIGCHLD, 0, NULL, 0, NULL);
#endif
    if (pid == 0) {
        struct timespec ts = { 0, 100 * 1000 * 1000 };
        syscall(SYS_nanosleep, &ts, NULL);
        (void)!write(p[1], "x", 1);
        syscall(SYS_exit_group, 5);
    }
    if (pid < 0) { fail(N, "clone: errno %d", errno); return; }
    char c;
    ssize_t r = read(p[0], &c, 1); /* nonblocking: data only if we waited */
    int st;
    waitpid((pid_t)pid, &st, 0);
    close(p[0]); close(p[1]);
    if (r != 1) { fail(N, "parent resumed before the child exited"); return; }
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 5) { fail(N, "status %#x", st); return; }
    pass(N);
}

static void clone3_errors(void) {
    const char *N = "clone3_errors";
    struct { struct cargs a; uint64_t ext[4]; } big;
    memset(&big, 0, sizeof big);
    big.a.exit_signal = SIGCHLD;
    EXPECT_ERR(N, clone3(&big.a, 56), EINVAL, "size 56");
    EXPECT_ERR(N, clone3(&big.a, 8192), E2BIG, "size 8192");
    big.ext[3] = 7;
    EXPECT_ERR(N, clone3(&big.a, sizeof big), E2BIG, "nonzero tail");
    big.ext[3] = 0;
    big.a.flags = SIGCHLD; /* CSIGNAL bits belong in exit_signal */
    EXPECT_ERR(N, clone3(&big.a, sizeof big.a), EINVAL, "CSIGNAL in flags");
    big.a.flags = 0;
    big.a.stack = 0x1000; big.a.stack_size = 0;
    EXPECT_ERR(N, clone3(&big.a, sizeof big.a), EINVAL, "stack without size");
    big.a.stack = 0; big.a.stack_size = 4096;
    EXPECT_ERR(N, clone3(&big.a, sizeof big.a), EINVAL, "size without stack");
    big.a.stack_size = 0;
    big.a.flags = CLONE_THREAD | CLONE_SIGHAND | CLONE_VM; /* + exit_signal */
    EXPECT_ERR(N, clone3(&big.a, sizeof big.a), EINVAL, "CLONE_THREAD with exit_signal");
    big.a.flags = 0;
    big.a.exit_signal = 65;
    EXPECT_ERR(N, clone3(&big.a, sizeof big.a), EINVAL, "exit_signal 65");
    big.a.exit_signal = SIGCHLD;
    pid_t tid = 0;
    big.a.set_tid = (uintptr_t)&tid; big.a.set_tid_size = 1;
    tid = 12345;
    long r = clone3(&big.a, sizeof big.a);
    if (r == 0) syscall(SYS_exit_group, 0);
    if (r > 0) waitpid((pid_t)r, NULL, 0);
    /* Linux needs CAP_SYS_ADMIN in the pid namespace and a free pid; any
     * error is acceptable, a child is not unless it got that pid. */
    if (r > 0 && r != 12345) { fail(N, "set_tid ignored"); return; }
    if (r < 0 && errno != EINVAL && errno != EPERM && errno != EEXIST) {
        fail(N, "set_tid: errno %d", errno); return;
    }
    pass(N);
}

static void clone3_fork(void) {
    const char *N = "clone3_fork";
    struct cargs a;
    memset(&a, 0, sizeof a);
    int pidfd = -1;
    pid_t ptid = 0;
    a.flags = CLONE_PIDFD | CLONE_PARENT_SETTID;
    a.pidfd = (uintptr_t)&pidfd;
    a.parent_tid = (uintptr_t)&ptid;
    a.exit_signal = SIGCHLD;
    long pid = clone3(&a, 64); /* VER0 size */
    if (pid == 0) {
        /* The child must not hold the pidfd. */
        int has = fcntl(pidfd, F_GETFD) != -1;
        syscall(SYS_exit_group, has ? 99 : 7);
    }
    if (pid < 0) { fail(N, "clone3: errno %d", errno); return; }
    if (ptid != pid) { fail(N, "parent_tid %d != %ld", ptid, pid); return; }
    if (pidfd < 0) { fail(N, "no pidfd"); return; }
    struct pollfd pf = { pidfd, POLLIN, 0 };
    int pr = poll(&pf, 1, 5000);
    int st;
    if (waitpid((pid_t)pid, &st, 0) != pid) { fail(N, "waitpid errno %d", errno); return; }
    close(pidfd);
    if (pr != 1) { fail(N, "pidfd not readable on exit"); return; }
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 7) { fail(N, "status %#x (99 = child held the pidfd)", st); return; }
    pass(N);
}

/* clone3 whose child touches no stack at all: in raw assembly it stores 77
 * to *word and its stack pointer to *sp, then exit_group(0)s. A CLONE_VM
 * child must not return through C frames the suspended parent still owns. */
static long clone3_store_exit(struct cargs *a, size_t sz, volatile int *word, volatile uintptr_t *sp) {
#if defined(__x86_64__)
    long ret;
    register volatile int *r12 __asm__("r12") = word;
    register volatile uintptr_t *r13 __asm__("r13") = sp;
    __asm__ volatile(
        "mov $435, %%eax\n\t"
        "syscall\n\t"
        "test %%rax, %%rax\n\t"
        "jnz 1f\n\t"
        "movl $77, (%%r12)\n\t"
        "mov %%rsp, (%%r13)\n\t"
        "mov $231, %%eax\n\t"
        "xor %%edi, %%edi\n\t"
        "syscall\n\t"
        "1:\n\t"
        : "=a"(ret) : "D"(a), "S"(sz), "r"(r12), "r"(r13) : "rcx", "r11", "memory");
    return ret < 0 && ret > -4096 ? (errno = (int)-ret, -1) : ret;
#else
    register long x0 __asm__("x0") = (long)a;
    register long x1 __asm__("x1") = (long)sz;
    register volatile int *x9 __asm__("x9") = word;
    register volatile uintptr_t *x11 __asm__("x11") = sp;
    __asm__ volatile(
        "mov x8, #435\n\t"
        "svc #0\n\t"
        "cbnz x0, 1f\n\t"
        "mov w10, #77\n\t"
        "str w10, [x9]\n\t"
        "mov x10, sp\n\t"
        "str x10, [x11]\n\t"
        "mov x8, #94\n\t"
        "mov x0, #0\n\t"
        "svc #0\n\t"
        "1:\n\t"
        : "+r"(x0) : "r"(x1), "r"(x9), "r"(x11) : "x8", "x10", "memory");
    return x0 < 0 && x0 > -4096 ? (errno = (int)-x0, -1) : x0;
#endif
}

static void clone3_vfork(void) {
    const char *N = "clone3_vfork";
    struct cargs a;
    static volatile uintptr_t child_sp;
    static char stk[16384] __attribute__((aligned(16)));
    for (int with_stack = 0; with_stack < 2; with_stack++) {
        memset(&a, 0, sizeof a);
        a.flags = CLONE_VM | CLONE_VFORK;
        a.exit_signal = SIGCHLD;
        if (with_stack) { a.stack = (uintptr_t)stk; a.stack_size = sizeof stk; }
        shared_word = 0;
        child_sp = 0;
        long pid = clone3_store_exit(&a, sizeof a, &shared_word, &child_sp);
        if (pid < 0) { fail(N, "clone3 (stack %d): errno %d", with_stack, errno); return; }
        int seen = shared_word; /* the child has exited: we were suspended */
        int st;
        if (waitpid((pid_t)pid, &st, 0) != pid) { fail(N, "waitpid errno %d", errno); return; }
        if (seen != 77) { fail(N, "stack %d: parent saw %d", with_stack, seen); return; }
        if (with_stack && (child_sp <= (uintptr_t)stk || child_sp > (uintptr_t)stk + sizeof stk)) {
            fail(N, "child sp %#lx outside the given stack", (unsigned long)child_sp); return;
        }
    }
    pass(N);
}

static void clone_pidfd_emfile(void) {
    const char *N = "clone_pidfd_emfile";
    while (waitpid(-1, NULL, WNOHANG) > 0) {} /* reap anything earlier left */
    struct rlimit old, lim;
    getrlimit(RLIMIT_NOFILE, &old);
    lim = old;
    lim.rlim_cur = 64;
    if (setrlimit(RLIMIT_NOFILE, &lim) < 0) { skip(N, "setrlimit"); return; }
    int fds[128], n = 0;
    while (n < 128) {
        int fd = dup(0);
        if (fd < 0) break;
        fds[n++] = fd;
    }
    int full = errno == EMFILE;
    int pidfd = -1;
#if defined(__x86_64__)
    long pid = syscall(SYS_clone, CLONE_PIDFD | SIGCHLD, 0, &pidfd, NULL, 0);
#else
    long pid = syscall(SYS_clone, CLONE_PIDFD | SIGCHLD, 0, &pidfd, 0, NULL);
#endif
    int e = errno;
    if (pid == 0) syscall(SYS_exit_group, 0);
    for (int i = 0; i < n; i++) close(fds[i]);
    setrlimit(RLIMIT_NOFILE, &old);
    if (!full) { skip(N, "could not fill the fd table"); if (pid > 0) waitpid((pid_t)pid, NULL, 0); return; }
    if (pid > 0) { waitpid((pid_t)pid, NULL, 0); fail(N, "clone succeeded with a full table (pidfd %d)", pidfd); return; }
    if (e != EMFILE) { fail(N, "errno %d, want EMFILE", e); return; }
    /* No child may have been created. */
    pid_t w = waitpid(-1, NULL, WNOHANG);
    if (w != -1 || errno != ECHILD) { fail(N, "a child was left behind (waitpid %d errno %d)", (int)w, errno); return; }
    pass(N);
}

/* ── long paths ───────────────────────────────────────────────────────── */

/* Build `base` + n components of 50 bytes; returns the path's length. */
static size_t deep_path(char *out, size_t cap, const char *base, int n, int do_mkdir) {
    size_t l = (size_t)snprintf(out, cap, "%s", base);
    for (int i = 0; i < n; i++) {
        l += (size_t)snprintf(out + l, cap - l, "/d%02d_%046d", i, 0);
        if (do_mkdir && mkdir(out, 0755) < 0 && errno != EEXIST) return 0;
    }
    return l;
}

/* The long-path checks below `base` (the caller's view). */
static int long_path_checks(const char *N, const char *base, const char *expect_prefix) {
    static char p[4200], q[4400], buf[4400];
    size_t l = deep_path(p, sizeof p, base, 20, 1); /* ~1020+ bytes */
    if (l == 0) { fail(N, "mkdir chain: errno %d", errno); return -1; }
    if (l <= 255) { fail(N, "path too short"); return -1; }
    snprintf(q, sizeof q, "%s/file", p);
    int fd = open(q, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { fail(N, "create %zu-byte path: errno %d", strlen(q), errno); return -1; }
    if (write(fd, "L", 1) != 1) { fail(N, "write"); close(fd); return -1; }
    struct stat st;
    if (stat(q, &st) < 0 || st.st_size != 1) { fail(N, "stat: errno %d", errno); close(fd); return -1; }
    /* /proc/self/fd/N names the file by its full path. */
    char link[64];
    snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
    ssize_t r = readlink(link, buf, sizeof buf - 1);
    if (r < 0) { fail(N, "readlink %s: errno %d", link, errno); close(fd); return -1; }
    buf[r] = 0;
    snprintf(p + l, sizeof p - l, "%s", ""); /* p unchanged */
    char want[4400];
    snprintf(want, sizeof want, "%s%s", expect_prefix, q + strlen(base));
    if (strcmp(buf, want)) { fail(N, "fd link %.60s... (%zd bytes) != %zu-byte want", buf, r, strlen(want)); close(fd); return -1; }
    close(fd);
    /* chdir + getcwd + relative open. */
    if (chdir(p) < 0) { fail(N, "chdir: errno %d", errno); return -1; }
    if (!getcwd(buf, sizeof buf)) { fail(N, "getcwd: errno %d", errno); return -1; }
    snprintf(want, sizeof want, "%s%s", expect_prefix, p + strlen(base));
    if (strcmp(buf, want)) { fail(N, "getcwd %zu bytes != %zu-byte want", strlen(buf), strlen(want)); return -1; }
    if (getcwd(buf, 100) != NULL || errno != ERANGE) { fail(N, "small getcwd: errno %d", errno); return -1; }
    fd = open("file", O_RDONLY);
    if (fd < 0) { fail(N, "relative open: errno %d", errno); return -1; }
    close(fd);
    /* openat relative to a deep directory fd. */
    int dfd = open(".", O_RDONLY | O_DIRECTORY);
    if (dfd < 0) { fail(N, "open .: errno %d", errno); return -1; }
    if (chdir("/") < 0) { fail(N, "chdir /"); return -1; }
    fd = openat(dfd, "file", O_RDONLY);
    if (fd < 0) { fail(N, "openat(deep dirfd): errno %d", errno); close(dfd); return -1; }
    close(fd);
    if (fchdir(dfd) < 0 || !getcwd(buf, sizeof buf) || strcmp(buf, want)) {
        fail(N, "fchdir/getcwd: errno %d", errno); close(dfd); return -1;
    }
    close(dfd);
    if (chdir("/") < 0) return -1;
    /* Limits: a component of 256 bytes, and a path of PATH_MAX bytes. */
    char name[300];
    memset(name, 'n', 256); name[256] = 0;
    snprintf(q, sizeof q, "%s/%s", base, name);
    if (open(q, O_RDONLY) != -1 || errno != ENAMETOOLONG) { fail(N, "256-byte component: errno %d", errno); return -1; }
    char *huge = malloc(PATH_MAX + 16);
    memset(huge, 'a', PATH_MAX + 15);
    huge[0] = '/';
    for (int i = 100; i < PATH_MAX + 15; i += 100) huge[i] = '/';
    huge[PATH_MAX] = 0;          /* strlen == PATH_MAX: too long */
    if (open(huge, O_RDONLY) != -1 || errno != ENAMETOOLONG) { fail(N, "PATH_MAX path: errno %d", errno); free(huge); return -1; }
    free(huge);
    return 0;
}

static void long_path(void) {
    const char *N = "long_path";
    char base[700];
    snprintf(base, sizeof base, "%s/long", work);
    rm_rf(base);
    if (mkdir(base, 0755) < 0) { fail(N, "mkdir base: errno %d", errno); return; }
    if (long_path_checks(N, base, base) == 0) pass(N);
}

static void long_path_chroot(void) {
    const char *N = "long_path_chroot";
    if (geteuid() != 0) { skip(N, "not root"); return; }
    static char jail[4200];
    char base[700];
    snprintf(base, sizeof base, "%s/ljail", work);
    rm_rf(base);
    if (mkdir(base, 0755) < 0) { fail(N, "mkdir: errno %d", errno); return; }
    size_t l = deep_path(jail, sizeof jail, base, 6, 1); /* root itself > 255 bytes */
    if (l <= 255) { fail(N, "jail path %zu bytes", l); return; }
    static char sub[4300];
    snprintf(sub, sizeof sub, "%s/in", jail);
    mkdir(sub, 0755);
    /* /proc/self/fd inside the jail needs a /proc there, as on Linux. */
    static char jproc[4300];
    snprintf(jproc, sizeof jproc, "%s/proc", jail);
    mkdir(jproc, 0755);
    if (mount("proc", jproc, "proc", 0, NULL) < 0) { fail(N, "mount proc: errno %d", errno); return; }
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        if (chroot(jail) < 0 || chdir("/") < 0) { printf("%s: FAIL chroot errno %d\n", N, errno); _exit(1); }
        int f = nfail;
        if (long_path_checks(N, "/in", "/in") < 0 || nfail != f) _exit(1);
        _exit(0);
    }
    int st;
    waitpid(pid, &st, 0);
    umount(jproc);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) { nfail++; printf("%s: FAIL (child)\n", N); return; }
    pass(N);
}

/* ── chroot + /proc, /dev, bind ───────────────────────────────────────── */

static int copy_file(const char *from, const char *to) {
    int in = open(from, O_RDONLY);
    if (in < 0) return -1;
    int out = open(to, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    if (out < 0) { close(in); return -1; }
    char buf[65536];
    ssize_t n;
    while ((n = read(in, buf, sizeof buf)) > 0) {
        if (write(out, buf, (size_t)n) != n) { n = -1; break; }
    }
    close(in); close(out);
    return n < 0 ? -1 : 0;
}

/* Runs inside the jail (exec'd as /bin/ct --in-jail). */
static int in_jail(void) {
    int bad = 0;
#define CHK(c, ...) do { if (!(c)) { printf("  in-jail: "); printf(__VA_ARGS__); printf("\n"); bad++; } } while (0)
    char buf[4200];
    ssize_t r = readlink("/proc/self/exe", buf, sizeof buf - 1);
    CHK(r > 0 && (buf[r] = 0, !strcmp(buf, "/bin/ct")), "/proc/self/exe = %s (errno %d)", r > 0 ? buf : "-", errno);
    CHK(chdir("/sub") == 0, "chdir /sub errno %d", errno);
    r = readlink("/proc/self/cwd", buf, sizeof buf - 1);
    CHK(r > 0 && (buf[r] = 0, !strcmp(buf, "/sub")), "/proc/self/cwd = %s (errno %d)", r > 0 ? buf : "-", errno);
    r = readlink("/proc/self/root", buf, sizeof buf - 1);
    CHK(r > 0 && (buf[r] = 0, !strcmp(buf, "/")), "/proc/self/root = %s (errno %d)", r > 0 ? buf : "-", errno);
    int fd = open("/sub/file", O_RDONLY);
    CHK(fd >= 0, "open /sub/file errno %d", errno);
    if (fd >= 0) {
        char l[64];
        snprintf(l, sizeof l, "/proc/self/fd/%d", fd);
        r = readlink(l, buf, sizeof buf - 1);
        CHK(r > 0 && (buf[r] = 0, !strcmp(buf, "/sub/file")), "%s = %s (errno %d)", l, r > 0 ? buf : "-", errno);
        /* the magic link opens the same file */
        int fd2 = open(l, O_RDONLY);
        CHK(fd2 >= 0, "open %s errno %d", l, errno);
        if (fd2 >= 0) close(fd2);
        close(fd);
    }
    fd = open("/proc/self/status", O_RDONLY);
    CHK(fd >= 0, "open /proc/self/status errno %d", errno);
    if (fd >= 0) {
        r = read(fd, buf, sizeof buf - 1);
        CHK(r > 0 && (buf[r] = 0, strstr(buf, "Pid:") != NULL), "status content");
        close(fd);
    }
    fd = open("/proc/self/root/sub/file", O_RDONLY);
    CHK(fd >= 0, "open /proc/self/root/sub/file errno %d", errno);
    if (fd >= 0) close(fd);
    /* /dev from inside the jail. */
    CHK(mount("devtmpfs", "/dev", "devtmpfs", 0, NULL) == 0, "mount devtmpfs /dev errno %d", errno);
    fd = open("/dev/null", O_WRONLY);
    CHK(fd >= 0 && write(fd, "x", 1) == 1, "/dev/null errno %d", errno);
    if (fd >= 0) close(fd);
    fd = open("/proc/mounts", O_RDONLY);
    CHK(fd >= 0, "open /proc/mounts errno %d", errno);
    if (fd >= 0) {
        r = read(fd, buf, sizeof buf - 1);
        if (r < 0) r = 0;
        buf[r] = 0;
        CHK(strstr(buf, " /proc proc ") != NULL, "/proc/mounts lacks /proc:\n%s", buf);
        CHK(strstr(buf, " /dev devtmpfs ") != NULL, "/proc/mounts lacks /dev");
        close(fd);
    }
    /* The bind mount set up from outside. */
    fd = open("/mnt/hostfile", O_RDONLY);
    CHK(fd >= 0 && read1(fd) == 'h', "/mnt/hostfile (bind) errno %d", errno);
    if (fd >= 0) close(fd);
    CHK(umount("/dev") == 0, "umount /dev errno %d", errno);
    CHK(open("/dev/null", O_RDONLY) < 0, "/dev/null still there after umount");
    CHK(umount("/proc") == 0, "umount /proc errno %d", errno);
    CHK(readlink("/proc/self/exe", buf, sizeof buf) < 0 && errno == ENOENT, "/proc/self/exe after umount");
    return bad;
}

static void chroot_cases(void) {
    const char *names[] = { "chroot_proc", "chroot_dev_inside", "chroot_bind", "chroot_umount" };
    if (geteuid() != 0) { for (int i = 0; i < 4; i++) skip(names[i], "not root"); return; }
    char j[700], p[800], host[700];
    snprintf(j, sizeof j, "%s/jail", work);
    snprintf(host, sizeof host, "%s/hostdir", work);
    rm_rf(j); rm_rf(host);
    const char *dirs[] = { "", "/proc", "/dev", "/sys", "/bin", "/sub", "/mnt" };
    for (int i = 0; i < 7; i++) {
        snprintf(p, sizeof p, "%s%s", j, dirs[i]);
        if (mkdir(p, 0755) < 0) { fail(names[0], "mkdir %s: errno %d", p, errno); return; }
    }
    snprintf(p, sizeof p, "%s/sub/file", j);
    write_file(p, "s");
    mkdir(host, 0755);
    snprintf(p, sizeof p, "%s/hostfile", host);
    write_file(p, "h");
    snprintf(p, sizeof p, "%s/bin/ct", j);
    if (copy_file("/proc/self/exe", p) < 0) { fail(names[0], "copy binary: errno %d", errno); return; }
    snprintf(p, sizeof p, "%s/proc", j);
    if (mount("proc", p, "proc", 0, NULL) < 0) { fail(names[0], "mount proc on %s: errno %d", p, errno); return; }
    char mnt[800];
    snprintf(mnt, sizeof mnt, "%s/mnt", j);
    if (mount(host, mnt, NULL, MS_BIND, NULL) < 0) { fail(names[2], "bind mount: errno %d", errno); umount(p); return; }
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        if (chroot(j) < 0 || chdir("/") < 0) _exit(100);
        char *av[] = { "/bin/ct", "--in-jail", NULL };
        execv("/bin/ct", av);
        _exit(101);
    }
    int st;
    waitpid(pid, &st, 0);
    /* Whatever happened inside, leave no alias mounts behind. */
    umount(mnt);
    umount(p);
    snprintf(p, sizeof p, "%s/dev", j);
    umount(p);
    if (!WIFEXITED(st) || WEXITSTATUS(st) >= 100) {
        for (int i = 0; i < 4; i++) fail(names[i], "jail child status %#x", st);
        return;
    }
    if (WEXITSTATUS(st) != 0) {
        for (int i = 0; i < 4; i++) fail(names[i], "%d in-jail checks failed (above)", WEXITSTATUS(st));
        return;
    }
    for (int i = 0; i < 4; i++) pass(names[i]);
}

/* ── AF_UNIX pathnames in a chroot ────────────────────────────────────────
 * sun_path used to be taken as a host-absolute path: a chrooted tmux bound
 * /tmp/tmux-0/default in the host's /tmp (ENOENT there), and a relative name
 * never resolved against the cwd. Inside the jail (its /tmp a bind mount of a
 * host directory) bind "/tmp/abs" and, from cwd /tmp, "rel"; outside, both must
 * be sockets in the host directory that a host connect() reaches. */
static int in_jail_unix(int ready_fd) {
    int a = socket(AF_UNIX, SOCK_STREAM, 0), b = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un sa = { .sun_family = AF_UNIX };
    strcpy(sa.sun_path, "/tmp/abs");
    if (a < 0 || b < 0 || bind(a, (struct sockaddr *)&sa, sizeof sa) || listen(a, 4)) return 10;
    if (chdir("/tmp")) return 11;
    strcpy(sa.sun_path, "rel");
    if (bind(b, (struct sockaddr *)&sa, sizeof sa) || listen(b, 4)) return 12;
    /* Inside, both names reach their listeners too. */
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    strcpy(sa.sun_path, "/tmp/rel");
    if (c < 0 || connect(c, (struct sockaddr *)&sa, sizeof sa)) return 13;
    /* c stays open until it is accepted: LeandrOS drops a pending connection
     * whose connector closed before accept (Linux still hands it out). */
    socklen_t sl = sizeof sa;
    memset(&sa, 0, sizeof sa);
    /* getsockname reports the address as bound, not the host path. */
    if (getsockname(a, (struct sockaddr *)&sa, &sl) || strcmp(sa.sun_path, "/tmp/abs")) return 14;
    if (write(ready_fd, "r", 1) != 1) return 15;
    struct pollfd p[2] = { { a, POLLIN, 0 }, { b, POLLIN, 0 } };
    long long end = 10000;
    int got = 0;
    while (got != 3 && end > 0) {
        if (poll(p, 2, 100) < 0 && errno != EINTR) return 16;
        for (int i = 0; i < 2; i++)
            if (p[i].revents & POLLIN) { int x = accept(p[i].fd, NULL, NULL); if (x >= 0) { close(x); got |= 1 << i; } }
        end -= 100;
    }
    close(c);
    /* 20 + the listeners that never accepted (bit 0 abs, bit 1 rel) */
    return got == 3 ? 0 : 20 + (3 & ~got);
}

static void chroot_unix(void) {
    const char *N = "chroot_unix_socket";
    if (geteuid() != 0) { skip(N, "not root"); return; }
    char j[700], p[800], host[700], hp[800];
    snprintf(j, sizeof j, "%s/ujail", work);
    /* The host side lives on /tmp: LeandrOS binds sockets on tmpfs only. */
    snprintf(host, sizeof host, "/tmp/chroottest-sock.%d", (int)getpid());
    rm_rf(j); rm_rf(host);
    const char *dirs[] = { "", "/tmp", "/bin" };
    for (int i = 0; i < 3; i++) {
        snprintf(p, sizeof p, "%s%s", j, dirs[i]);
        if (mkdir(p, 0755) < 0) { fail(N, "mkdir %s: errno %d", p, errno); return; }
    }
    if (mkdir(host, 0755) < 0) { fail(N, "mkdir %s: errno %d", host, errno); return; }
    snprintf(p, sizeof p, "%s/bin/ct", j);
    if (copy_file("/proc/self/exe", p) < 0) { fail(N, "copy binary: errno %d", errno); return; }
    char jt[800];
    snprintf(jt, sizeof jt, "%s/tmp", j);
    if (mount(host, jt, NULL, MS_BIND, NULL) < 0) { fail(N, "bind mount: errno %d", errno); rm_rf(host); return; }
    int pp[2];
    if (pipe(pp)) { fail(N, "pipe"); umount(jt); return; }
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        close(pp[0]);
        if (chroot(j) < 0 || chdir("/") < 0) _exit(100);
        char fdarg[16];
        snprintf(fdarg, sizeof fdarg, "%d", pp[1]);
        char *av[] = { "/bin/ct", "--in-jail-unix", fdarg, NULL };
        execv("/bin/ct", av);
        _exit(101);
    }
    close(pp[1]);
    const char *why = NULL;
    char c;
    int hs[2] = { -1, -1 };
    struct pollfd rp = { pp[0], POLLIN, 0 };
    if (poll(&rp, 1, 10000) != 1 || read(pp[0], &c, 1) != 1) why = "jail child never became ready";
    for (int i = 0; !why && i < 2; i++) {
        snprintf(hp, sizeof hp, "%s/%s", host, i ? "rel" : "abs");
        struct stat st;
        if (stat(hp, &st) || !S_ISSOCK(st.st_mode)) { why = i ? "no socket at host/rel" : "no socket at host/abs"; break; }
        struct sockaddr_un sa = { .sun_family = AF_UNIX };
        snprintf(sa.sun_path, sizeof sa.sun_path, "%s", hp);
        /* Kept open until the jail child has accepted (see in_jail_unix). */
        hs[i] = socket(AF_UNIX, SOCK_STREAM, 0);
        if (hs[i] < 0 || connect(hs[i], (struct sockaddr *)&sa, sizeof sa)) why = "host connect to the jail's socket failed";
    }
    if (why) kill(pid, SIGKILL);
    int st;
    waitpid(pid, &st, 0);
    for (int i = 0; i < 2; i++) if (hs[i] >= 0) close(hs[i]);
    close(pp[0]);
    umount(jt);
    rm_rf(host);
    if (!why && (!WIFEXITED(st) || WEXITSTATUS(st) != 0)) {
        printf("%s: FAIL jail child status %#x\n", N, st); nfail++; fflush(stdout); return;
    }
    if (why) fail(N, "%s (errno %d, jail child status %#x)", why, errno, st);
    else pass(N);
}

int main(int argc, char **argv) {
    if (argc > 2 && !strcmp(argv[1], "--exit")) return atoi(argv[2]);
    if (argc > 1 && !strcmp(argv[1], "--in-jail")) return in_jail();
    if (argc > 2 && !strcmp(argv[1], "--in-jail-unix")) return in_jail_unix(atoi(argv[2]));
    const char *dir = "/var/tmp";
    int o;
    while ((o = getopt(argc, argv, "d:")) != -1) {
        if (o == 'd') dir = optarg;
        else { fprintf(stderr, "usage: chroottest [-d dir]\n"); return 2; }
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    static char selfbuf[4200];
    ssize_t r = readlink("/proc/self/exe", selfbuf, sizeof selfbuf - 1);
    self_path = r > 0 ? (selfbuf[r] = 0, selfbuf) : argv[0];
    mkdir(dir, 01777);
    snprintf(work, sizeof work, "%s/chroottest.%d", dir, (int)getpid());
    rm_rf(work);
    if (mkdir(work, 0755) < 0) { fprintf(stderr, "chroottest: mkdir %s: %s\n", work, strerror(errno)); return 2; }
    printf("chroottest: uid %d, work %s\n", (int)geteuid(), work);

    openat2_how();
    openat2_beneath();
    openat2_in_root();
    openat2_no_symlinks();
    openat2_no_magic();
    vfork_shared();
    vfork_exec();
    clone_vfork_novm();
    clone3_errors();
    clone3_fork();
    clone3_vfork();
    clone_pidfd_emfile();
    long_path();
    long_path_chroot();
    chroot_cases();
    chroot_unix();

    if (chdir("/") == 0) rm_rf(work);
    printf("chroottest: %d passed, %d failed, %d skipped\n", npass, nfail, nskip);
    return nfail;
}
