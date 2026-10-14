/*
 * syscalltest — splice(2)/tee(2)/vmsplice(2), rseq(2) and the pidfd family,
 * with the results Linux gives. Like nettest, the same file builds for the
 * host (cc syscalltest.c -lpthread) and for LeandrOS (zig cc -target
 * <arch>-linux-musl -static).
 *
 *   syscalltest [-d DIR]...   extra directories for the file-backed splice
 *                             cases (default: /tmp, plus /root when root)
 *
 * splice cases:
 *   splice_einval        neither end a pipe -> EINVAL; wrong pipe end -> EBADF
 *   splice_espipe        an offset on the pipe end -> ESPIPE
 *   splice_file_pipe     file -> pipe with len far above the pipe capacity
 *                        returns a short count (no hang) and the right bytes
 *   splice_pipe_file     pipe -> file moves what is queued, at the file position
 *   splice_offsets       off_in/off_out: pread/pwrite semantics, *off advanced,
 *                        the descriptor position untouched
 *   splice_nonblock      SPLICE_F_NONBLOCK on an empty source / full
 *                        destination pipe -> EAGAIN; 0 at end of input
 *   splice_block_wake    a splice from an empty pipe blocks until a writer
 *                        appears, then returns its bytes
 *   splice_cat           the uutils cat shape: file -> broker pipe -> output
 *                        pipe drained by another process, 300 KiB, checksummed
 *   splice_pipe_pipe     pipe -> pipe, and tee(2) leaving the source intact
 *   vmsplice             user memory into a pipe and back out
 *
 * rseq cases:
 *   rseq_register        register/EBUSY/EINVAL/EPERM/unregister rules; cpu_id
 *                        is published at registration and reset by unregister
 *   rseq_cpu_id          cpu_id stays a valid CPU number across yields
 *   rseq_abort_signal    a signal taken inside the critical section resumes
 *                        at abort_ip
 *   rseq_abort_preempt   preemption inside the critical section aborts it
 *   rseq_bad_sig         a descriptor whose abort signature differs from the
 *                        registered one is SIGSEGV
 *   rseq_inherit         fork inherits the registration (EBUSY in the child),
 *                        a new thread does not, execve clears it
 *
 * pidfd cases:
 *   pidfd_open_errors    EINVAL (flags, pid 0, a non-leader thread), ESRCH
 *   pidfd_poll_exit      POLLIN exactly when the child exits, then
 *                        waitpid(WNOHANG) reaps it (tokio's reaping path)
 *   pidfd_epoll_et       EPOLLIN|EPOLLET fires once for the exit
 *   pidfd_waitid         waitid(P_PIDFD): status, WNOHANG, EAGAIN on an
 *                        O_NONBLOCK pidfd
 *   pidfd_send_signal    SIGTERM through the pidfd kills the child; ESRCH once
 *                        exited; EINVAL/EBADF on a non-pidfd/closed fd
 *   pidfd_clone          clone(CLONE_PIDFD) returns a working pidfd; EINVAL
 *                        with CLONE_PARENT_SETTID
 *   pidfd_getfd          a duplicate of one of our own fds, close-on-exec
 *   pidfd_proc_link      /proc/self/fd/N reads anon_inode:[pidfd]
 *
 * vectored I/O:
 *   preadv2              preadv/pwritev at an offset (position untouched),
 *                        preadv2 at -1 (position used and advanced), unknown
 *                        RWF flag EOPNOTSUPP, RWF_NOWAIT on an empty pipe
 *                        EAGAIN, RWF_DSYNC, ESPIPE on a pipe
 *   preadv2_tty          RWF_NOWAIT on a pty never blocks (EOPNOTSUPP/EAGAIN)
 *   statx_matches_stat   statx reports the uid/gid, times, dev, ino, mode and
 *                        size stat does
 *
 * Output: "<name>: PASS|FAIL ...|SKIP ..." per case and a summary
 * "syscalltest: N passed, M failed, K skipped". Exit status = failures.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif
#ifndef SYS_pidfd_send_signal
#define SYS_pidfd_send_signal 424
#endif
#ifndef SYS_pidfd_getfd
#define SYS_pidfd_getfd 438
#endif
#ifndef SYS_close_range
#define SYS_close_range 436
#endif
#ifndef SYS_rseq
#if defined(__x86_64__)
#define SYS_rseq 334
#else
#define SYS_rseq 293
#endif
#endif
#ifndef P_PIDFD
#define P_PIDFD 3
#endif
#ifndef CLONE_PIDFD
#define CLONE_PIDFD 0x1000
#endif
#ifndef SPLICE_F_NONBLOCK
#define SPLICE_F_NONBLOCK 2
#endif

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

static long long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static ssize_t sys_splice(int in, loff_t *oin, int out, loff_t *oout, size_t len, unsigned f) {
    return syscall(SYS_splice, in, oin, out, oout, len, f);
}
static int pidfd_open(pid_t pid, unsigned f) { return (int)syscall(SYS_pidfd_open, pid, f); }
static int pidfd_send_signal(int fd, int sig, siginfo_t *info, unsigned f) {
    return (int)syscall(SYS_pidfd_send_signal, fd, sig, info, f);
}

/* Fill buf with a position-dependent pattern so misordered bytes show. */
static void pattern(unsigned char *b, size_t n, size_t base) {
    for (size_t i = 0; i < n; i++) b[i] = (unsigned char)((base + i) * 131 + ((base + i) >> 8));
}

static int make_file(const char *dir, const char *tag, size_t n, char *path, size_t pathlen) {
    snprintf(path, pathlen, "%s/.syscalltest-%s-%d", dir, tag, (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) return -1;
    unsigned char *b = malloc(n ? n : 1);
    pattern(b, n, 0);
    size_t done = 0;
    while (done < n) {
        ssize_t w = write(fd, b + done, n - done);
        if (w <= 0) { free(b); close(fd); unlink(path); return -1; }
        done += (size_t)w;
    }
    free(b);
    lseek(fd, 0, SEEK_SET);
    return fd;
}

/* Run fn in a child with a timeout; returns its wait status, or -1 on timeout
 * (the child is then killed). Keeps a hang in one case from eating the run. */
static int in_child(int (*fn)(void *), void *arg, int timeout_ms) {
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) _exit(fn(arg));
    if (pid < 0) return -2;
    long long end = now_ms() + timeout_ms;
    int st;
    for (;;) {
        pid_t r = waitpid(pid, &st, WNOHANG);
        if (r == pid) return st;
        if (now_ms() > end) { kill(pid, SIGKILL); waitpid(pid, &st, 0); return -1; }
        usleep(5000);
    }
}

/* ── splice ──────────────────────────────────────────────────────────────── */

static void splice_einval(const char *dir) {
    const char *n = "splice_einval";
    char p1[256], p2[256];
    int a = make_file(dir, "ea", 10, p1, sizeof p1), b = make_file(dir, "eb", 0, p2, sizeof p2);
    int pp[2];
    if (a < 0 || b < 0 || pipe(pp) < 0) { fail(n, "setup: %s", strerror(errno)); goto out; }
    errno = 0;
    ssize_t r = sys_splice(a, NULL, b, NULL, 10, 0);
    if (!(r < 0 && errno == EINVAL)) { fail(n, "file->file r=%zd errno=%s (want EINVAL)", r, strerror(errno)); goto outp; }
    r = sys_splice(a, NULL, pp[0], NULL, 10, 0);   /* read end used as output */
    if (!(r < 0 && errno == EBADF)) { fail(n, "file->pipe read end r=%zd errno=%s (want EBADF)", r, strerror(errno)); goto outp; }
    r = sys_splice(pp[1], NULL, b, NULL, 10, 0);   /* write end used as input */
    if (!(r < 0 && errno == EBADF)) { fail(n, "pipe write end->file r=%zd errno=%s (want EBADF)", r, strerror(errno)); goto outp; }
    r = sys_splice(a, NULL, pp[1], NULL, 0, 0);
    if (r != 0) { fail(n, "len 0 r=%zd errno=%s (want 0)", r, strerror(errno)); goto outp; }
    pass(n);
outp:
    close(pp[0]); close(pp[1]);
out:
    if (a >= 0) { close(a); unlink(p1); }
    if (b >= 0) { close(b); unlink(p2); }
}

static void splice_espipe(const char *dir) {
    const char *n = "splice_espipe";
    char p1[256];
    int a = make_file(dir, "sp", 100, p1, sizeof p1);
    int pp[2];
    if (a < 0 || pipe(pp) < 0) { fail(n, "setup: %s", strerror(errno)); if (a >= 0) { close(a); unlink(p1); } return; }
    loff_t off = 0;
    errno = 0;
    ssize_t r = sys_splice(a, NULL, pp[1], &off, 10, 0);
    int e1 = errno;
    if (write(pp[1], "x", 1) != 1) e1 = -1;
    errno = 0;
    ssize_t r2 = sys_splice(pp[0], &off, a, NULL, 1, 0);
    if (r < 0 && e1 == ESPIPE && r2 < 0 && errno == ESPIPE) pass(n);
    else fail(n, "r=%zd/%s r2=%zd/%s (want ESPIPE both)", r, strerror(e1), r2, strerror(errno));
    close(pp[0]); close(pp[1]); close(a); unlink(p1);
}

static void splice_file_pipe(const char *dir, const char *tag) {
    char n[64];
    snprintf(n, sizeof n, "splice_file_pipe[%s]", tag);
    char p1[256];
    const size_t sz = 200 * 1024;
    int a = make_file(dir, "fp", sz, p1, sizeof p1);
    int pp[2];
    if (a < 0 || pipe(pp) < 0) { fail(n, "setup: %s", strerror(errno)); if (a >= 0) { close(a); unlink(p1); } return; }
    alarm(20);
    ssize_t r = sys_splice(a, NULL, pp[1], NULL, 1 << 20, 0);
    alarm(0);
    if (r <= 0 || (size_t)r >= sz) { fail(n, "r=%zd errno=%s (want a short count)", r, strerror(errno)); goto out; }
    off_t pos = lseek(a, 0, SEEK_CUR);
    if (pos != r) { fail(n, "file position %lld after moving %zd", (long long)pos, r); goto out; }
    unsigned char *got = malloc((size_t)r), *want = malloc((size_t)r);
    size_t have = 0;
    while (have < (size_t)r) {
        ssize_t k = read(pp[0], got + have, (size_t)r - have);
        if (k <= 0) break;
        have += (size_t)k;
    }
    pattern(want, (size_t)r, 0);
    if (have == (size_t)r && !memcmp(got, want, (size_t)r)) { printf("%s: moved %zd of %zu\n", n, r, sz); pass(n); }
    else fail(n, "pipe held %zu bytes, content %s", have, memcmp(got, want, have) ? "differs" : "matches");
    free(got); free(want);
out:
    close(pp[0]); close(pp[1]); close(a); unlink(p1);
}

static void splice_pipe_file(const char *dir, const char *tag) {
    char n[64];
    snprintf(n, sizeof n, "splice_pipe_file[%s]", tag);
    char p1[256];
    int a = make_file(dir, "pf", 0, p1, sizeof p1);
    int pp[2];
    if (a < 0 || pipe(pp) < 0) { fail(n, "setup: %s", strerror(errno)); if (a >= 0) { close(a); unlink(p1); } return; }
    unsigned char b[5000], c[5000];
    pattern(b, sizeof b, 7);
    if (write(pp[1], b, sizeof b) != (ssize_t)sizeof b) { fail(n, "write: %s", strerror(errno)); goto out; }
    ssize_t r = sys_splice(pp[0], NULL, a, NULL, 1 << 20, 0);
    if (r != (ssize_t)sizeof b) { fail(n, "r=%zd errno=%s (want %zu)", r, strerror(errno), sizeof b); goto out; }
    if (lseek(a, 0, SEEK_CUR) != r) { fail(n, "file position not advanced"); goto out; }
    if (pread(a, c, sizeof c, 0) != (ssize_t)sizeof c || memcmp(b, c, sizeof b)) { fail(n, "file content differs"); goto out; }
    struct pollfd pf = { pp[0], POLLIN, 0 };
    if (poll(&pf, 1, 0) != 0) { fail(n, "pipe still readable after the splice"); goto out; }
    pass(n);
out:
    close(pp[0]); close(pp[1]); close(a); unlink(p1);
}

static void splice_offsets(const char *dir, const char *tag) {
    char n[64];
    snprintf(n, sizeof n, "splice_offsets[%s]", tag);
    char p1[256], p2[256];
    int a = make_file(dir, "oi", 8000, p1, sizeof p1), b = make_file(dir, "oo", 0, p2, sizeof p2);
    int pp[2];
    if (a < 0 || b < 0 || pipe(pp) < 0) { fail(n, "setup: %s", strerror(errno)); goto out; }
    loff_t oin = 1000;
    ssize_t r = sys_splice(a, &oin, pp[1], NULL, 3000, 0);
    if (r != 3000 || oin != 4000 || lseek(a, 0, SEEK_CUR) != 0) {
        fail(n, "in: r=%zd off=%lld pos=%lld (want 3000, 4000, 0)", r, (long long)oin, (long long)lseek(a, 0, SEEK_CUR));
        goto outp;
    }
    loff_t oout = 500;
    r = sys_splice(pp[0], NULL, b, &oout, 3000, 0);
    if (r != 3000 || oout != 3500 || lseek(b, 0, SEEK_CUR) != 0) {
        fail(n, "out: r=%zd off=%lld pos=%lld (want 3000, 3500, 0)", r, (long long)oout, (long long)lseek(b, 0, SEEK_CUR));
        goto outp;
    }
    unsigned char got[3000], want[3000];
    pattern(want, sizeof want, 1000);
    if (pread(b, got, sizeof got, 500) != 3000 || memcmp(got, want, sizeof got)) { fail(n, "content at offset 500 differs"); goto outp; }
    pass(n);
outp:
    close(pp[0]); close(pp[1]);
out:
    if (a >= 0) { close(a); unlink(p1); }
    if (b >= 0) { close(b); unlink(p2); }
}

static void splice_nonblock(const char *dir) {
    const char *n = "splice_nonblock";
    char p1[256];
    int a = make_file(dir, "nb", 300000, p1, sizeof p1);
    int pp[2], qq[2];
    if (a < 0 || pipe(pp) < 0 || pipe(qq) < 0) { fail(n, "setup: %s", strerror(errno)); if (a >= 0) { close(a); unlink(p1); } return; }
    errno = 0;
    ssize_t r = sys_splice(pp[0], NULL, a, NULL, 100, SPLICE_F_NONBLOCK);
    if (!(r < 0 && errno == EAGAIN)) { fail(n, "empty pipe r=%zd errno=%s (want EAGAIN)", r, strerror(errno)); goto out; }
    /* Fill qq completely, then a nonblocking splice into it must not block. */
    long long t0 = now_ms();
    ssize_t total = 0;
    for (int i = 0; i < 64; i++) {
        r = sys_splice(a, NULL, qq[1], NULL, 1 << 20, SPLICE_F_NONBLOCK);
        if (r < 0) break;
        total += r;
    }
    if (!(r < 0 && errno == EAGAIN) || total <= 0 || now_ms() - t0 > 5000) {
        fail(n, "full pipe r=%zd errno=%s after %zd bytes (want EAGAIN)", r, strerror(errno), total);
        goto out;
    }
    close(pp[1]); pp[1] = -1;
    r = sys_splice(pp[0], NULL, a, NULL, 100, 0);
    if (r != 0) { fail(n, "closed empty pipe r=%zd errno=%s (want 0)", r, strerror(errno)); goto out; }
    printf("%s: pipe filled with %zd bytes before EAGAIN\n", n, total);
    pass(n);
out:
    close(pp[0]); if (pp[1] >= 0) close(pp[1]); close(qq[0]); close(qq[1]); close(a); unlink(p1);
}

static void splice_block_wake(const char *dir) {
    const char *n = "splice_block_wake";
    char p1[256];
    int a = make_file(dir, "bw", 0, p1, sizeof p1);
    int pp[2];
    if (a < 0 || pipe(pp) < 0) { fail(n, "setup: %s", strerror(errno)); if (a >= 0) { close(a); unlink(p1); } return; }
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) { usleep(300 * 1000); if (write(pp[1], "hello", 5) != 5) _exit(1); _exit(0); }
    long long t0 = now_ms();
    alarm(20);
    ssize_t r = sys_splice(pp[0], NULL, a, NULL, 4096, 0);
    alarm(0);
    long long dt = now_ms() - t0;
    waitpid(c, NULL, 0);
    char b[8] = {0};
    if (r == 5 && dt >= 200 && pread(a, b, 5, 0) == 5 && !memcmp(b, "hello", 5)) { printf("%s: woke after %lld ms\n", n, dt); pass(n); }
    else fail(n, "r=%zd errno=%s after %lld ms", r, strerror(errno), dt);
    close(pp[0]); close(pp[1]); close(a); unlink(p1);
}

/* uutils cat: splice(file -> broker, 128 KiB) then drain the broker into the
 * output with splice until the count is moved; the output is a pipe drained
 * by another process. */
static void splice_cat(const char *dir, const char *tag) {
    char n[64];
    snprintf(n, sizeof n, "splice_cat[%s]", tag);
    char p1[256];
    const size_t sz = 300 * 1024;
    int a = make_file(dir, "cat", sz, p1, sizeof p1);
    int broker[2], out[2], res[2];
    if (a < 0 || pipe(broker) < 0 || pipe(out) < 0 || pipe(res) < 0) { fail(n, "setup: %s", strerror(errno)); if (a >= 0) { close(a); unlink(p1); } return; }
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        close(out[1]); close(broker[0]); close(broker[1]);
        unsigned char b[4096];
        size_t pos = 0, bad = 0;
        ssize_t k;
        while ((k = read(out[0], b, sizeof b)) > 0) {
            unsigned char w[4096];
            pattern(w, (size_t)k, pos);
            if (memcmp(w, b, (size_t)k)) bad++;
            pos += (size_t)k;
        }
        unsigned long long msg[2] = { pos, bad };
        if (write(res[1], msg, sizeof msg) != sizeof msg) _exit(1);
        _exit(0);
    }
    close(out[0]); close(res[1]);
    alarm(30);
    ssize_t total = 0, r;
    int ok = 1;
    while ((r = sys_splice(a, NULL, broker[1], NULL, 128 * 1024, 0)) > 0) {
        ssize_t left = r;
        while (left > 0) {
            ssize_t s = sys_splice(broker[0], NULL, out[1], NULL, (size_t)left, 0);
            if (s <= 0) { ok = 0; break; }
            left -= s;
        }
        if (!ok) break;
        total += r;
    }
    alarm(0);
    close(out[1]);
    unsigned long long msg[2] = { 0, 1 };
    if (read(res[0], msg, sizeof msg) != sizeof msg) msg[1] = 99;
    waitpid(c, NULL, 0);
    if (ok && r == 0 && (size_t)total == sz && msg[0] == sz && msg[1] == 0) pass(n);
    else fail(n, "moved %zd (last r=%zd errno=%s), reader got %llu bytes, %llu bad chunks", total, r, strerror(errno), msg[0], msg[1]);
    close(broker[0]); close(broker[1]); close(res[0]); close(a); unlink(p1);
}

static void splice_pipe_pipe(void) {
    const char *n = "splice_pipe_pipe";
    int pp[2], qq[2], tt[2];
    if (pipe(pp) < 0 || pipe(qq) < 0 || pipe(tt) < 0) { fail(n, "pipe: %s", strerror(errno)); return; }
    if (write(pp[1], "abcdefgh", 8) != 8) { fail(n, "write"); return; }
    ssize_t t = syscall(SYS_tee, pp[0], tt[1], 100, 0);
    ssize_t r = sys_splice(pp[0], NULL, qq[1], NULL, 5, 0);
    char b[16] = {0}, c[16] = {0}, d[16] = {0};
    ssize_t k1 = read(qq[0], b, sizeof b);
    ssize_t k2 = read(pp[0], c, sizeof c);
    ssize_t k3 = read(tt[0], d, sizeof d);
    errno = 0;
    ssize_t self = sys_splice(pp[0], NULL, pp[1], NULL, 5, 0);
    int eself = errno;
    if (t == 8 && r == 5 && k1 == 5 && !memcmp(b, "abcde", 5) && k2 == 3 && !memcmp(c, "fgh", 3)
        && k3 == 8 && !memcmp(d, "abcdefgh", 8) && self < 0 && eself == EINVAL) pass(n);
    else fail(n, "tee=%zd splice=%zd reads %zd/%zd/%zd self=%zd(%s)", t, r, k1, k2, k3, self, strerror(eself));
    close(pp[0]); close(pp[1]); close(qq[0]); close(qq[1]); close(tt[0]); close(tt[1]);
}

static void vmsplice_case(void) {
    const char *n = "vmsplice";
    int pp[2];
    if (pipe(pp) < 0) { fail(n, "pipe"); return; }
    char a[] = "vm", b[] = "splice";
    struct iovec iv[2] = { { a, 2 }, { b, 6 } };
    ssize_t w = syscall(SYS_vmsplice, pp[1], iv, 2, 0);
    char o1[4] = {0}, o2[8] = {0};
    struct iovec ov[2] = { { o1, 3 }, { o2, 5 } };
    ssize_t r = syscall(SYS_vmsplice, pp[0], ov, 2, 0);
    errno = 0;
    ssize_t e = syscall(SYS_vmsplice, pp[0], ov, 2, SPLICE_F_NONBLOCK);
    if (w == 8 && r == 8 && !memcmp(o1, "vms", 3) && !memcmp(o2, "plice", 5) && e < 0 && errno == EAGAIN) pass(n);
    else fail(n, "w=%zd r=%zd o=%.3s%.5s e=%zd(%s)", w, r, o1, o2, e, strerror(errno));
    close(pp[0]); close(pp[1]);
}

/* ── rseq ────────────────────────────────────────────────────────────────── */

struct rseq_area {
    uint32_t cpu_id_start;
    uint32_t cpu_id;
    uint64_t rseq_cs;
    uint32_t flags;
    uint32_t node_id;
    uint32_t mm_cid;
    uint32_t pad;
} __attribute__((aligned(32)));

#if defined(__x86_64__)
#define RSEQ_SIG 0x53053053u
#else
#define RSEQ_SIG 0xd428bc00u
#endif

/* rseq_spin(area, count): store the critical-section descriptor in
 * area->rseq_cs, then count `count` down to 0 inside the section. Returns 0
 * if the loop finished, 1 if the kernel resumed it at abort_ip. The abort
 * handler is preceded by RSEQ_SIG, as the ABI requires. */
extern int rseq_spin(struct rseq_area *a, unsigned long count);
#if defined(__x86_64__)
__asm__(
    ".text\n"
    ".globl rseq_spin\n"
    ".type rseq_spin, @function\n"
    "rseq_spin:\n"
    "    leaq rseq_spin_cs(%rip), %rax\n"
    "    movq %rax, 8(%rdi)\n"
    "rseq_spin_start:\n"
    "    subq $1, %rsi\n"
    "    jnz rseq_spin_start\n"
    "rseq_spin_end:\n"
    "    movq $0, 8(%rdi)\n"
    "    xorl %eax, %eax\n"
    "    ret\n"
    "    .byte 0x0f, 0xb9, 0x3d\n"
    "    .long 0x53053053\n"
    "rseq_spin_abort:\n"
    "    movl $1, %eax\n"
    "    ret\n"
    ".size rseq_spin, .-rseq_spin\n"
    ".data\n"
    ".balign 32\n"
    "rseq_spin_cs:\n"
    "    .long 0, 0\n"
    "    .quad rseq_spin_start, rseq_spin_end - rseq_spin_start, rseq_spin_abort\n"
    ".text\n");
#else
__asm__(
    ".text\n"
    ".globl rseq_spin\n"
    ".type rseq_spin, %function\n"
    "rseq_spin:\n"
    "    adrp x2, rseq_spin_cs\n"
    "    add x2, x2, :lo12:rseq_spin_cs\n"
    "    str x2, [x0, #8]\n"
    "rseq_spin_start:\n"
    "    subs x1, x1, #1\n"
    "    b.ne rseq_spin_start\n"
    "rseq_spin_end:\n"
    "    str xzr, [x0, #8]\n"
    "    mov w0, #0\n"
    "    ret\n"
    "    .inst 0xd428bc00\n"
    "rseq_spin_abort:\n"
    "    mov w0, #1\n"
    "    ret\n"
    ".size rseq_spin, .-rseq_spin\n"
    ".data\n"
    ".balign 32\n"
    "rseq_spin_cs:\n"
    "    .long 0, 0\n"
    "    .quad rseq_spin_start, rseq_spin_end - rseq_spin_start, rseq_spin_abort\n"
    ".text\n");
#endif

static long rseq_call(struct rseq_area *a, uint32_t len, int flags, uint32_t sig) {
    return syscall(SYS_rseq, a, len, flags, sig);
}

static struct rseq_area area_main, area_alt, area_thread;
static char misaligned_buf[96] __attribute__((aligned(32)));

static void rseq_register(void) {
    const char *n = "rseq_register";
    struct rseq_area *a = &area_main;
    memset(a, 0, sizeof *a);
    a->cpu_id = 0xfffffffe;
    long r = rseq_call(a, sizeof *a, 0, RSEQ_SIG);
    if (r != 0) {
        if (errno == EBUSY) { skip(n, "(libc already registered an rseq area)"); return; }
        fail(n, "register: %s", strerror(errno)); return;
    }
    if (a->cpu_id == 0xfffffffe || a->cpu_id != a->cpu_id_start) { fail(n, "cpu_id %u / start %u not published", a->cpu_id, a->cpu_id_start); return; }
    errno = 0; r = rseq_call(a, sizeof *a, 0, RSEQ_SIG);
    if (!(r < 0 && errno == EBUSY)) { fail(n, "second register r=%ld errno=%s (want EBUSY)", r, strerror(errno)); return; }
    errno = 0; r = rseq_call(&area_alt, sizeof area_alt, 0, RSEQ_SIG);
    if (!(r < 0 && errno == EINVAL)) { fail(n, "other area r=%ld errno=%s (want EINVAL)", r, strerror(errno)); return; }
    errno = 0; r = rseq_call(a, sizeof *a, 0, RSEQ_SIG + 1);
    if (!(r < 0 && errno == EPERM)) { fail(n, "other sig r=%ld errno=%s (want EPERM)", r, strerror(errno)); return; }
    errno = 0; r = rseq_call(a, sizeof *a, 1, RSEQ_SIG + 1);
    if (!(r < 0 && errno == EPERM)) { fail(n, "unregister bad sig r=%ld errno=%s (want EPERM)", r, strerror(errno)); return; }
    errno = 0; r = rseq_call(a, 16, 1, RSEQ_SIG);
    if (!(r < 0 && errno == EINVAL)) { fail(n, "unregister bad len r=%ld errno=%s (want EINVAL)", r, strerror(errno)); return; }
    r = rseq_call(a, sizeof *a, 1, RSEQ_SIG);
    if (r != 0) { fail(n, "unregister: %s", strerror(errno)); return; }
    if (a->cpu_id != 0xffffffffu) { fail(n, "cpu_id %u after unregister (want -1)", a->cpu_id); return; }
    errno = 0; r = rseq_call(a, sizeof *a, 1, RSEQ_SIG);
    if (!(r < 0 && errno == EINVAL)) { fail(n, "unregister twice r=%ld errno=%s (want EINVAL)", r, strerror(errno)); return; }
    errno = 0; r = rseq_call((struct rseq_area *)(misaligned_buf + 16), 32, 0, RSEQ_SIG);
    if (!(r < 0 && errno == EINVAL)) { fail(n, "misaligned r=%ld errno=%s (want EINVAL)", r, strerror(errno)); return; }
    errno = 0; r = rseq_call(a, 16, 0, RSEQ_SIG);
    if (!(r < 0 && errno == EINVAL)) { fail(n, "len 16 r=%ld errno=%s (want EINVAL)", r, strerror(errno)); return; }
    errno = 0; r = rseq_call(a, sizeof *a, 2, RSEQ_SIG);
    if (!(r < 0 && errno == EINVAL)) { fail(n, "bad flags r=%ld errno=%s (want EINVAL)", r, strerror(errno)); return; }
    pass(n);
}

static int rseq_cpu_id_child(void *arg) {
    (void)arg;
    if (rseq_call(&area_main, sizeof area_main, 0, RSEQ_SIG) != 0) return 10;
    long ncpu = sysconf(_SC_NPROCESSORS_ONLN);
    if (ncpu < 1) ncpu = 1;
    for (int i = 0; i < 200; i++) {
        sched_yield();
        uint32_t c = area_main.cpu_id;
        if (c != area_main.cpu_id_start || (long)c >= ncpu) return 11;
        if (i % 20 == 0) usleep(1000);
    }
    return 0;
}

static void rseq_cpu_id(void) {
    int st = in_child(rseq_cpu_id_child, NULL, 20000);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass("rseq_cpu_id");
    else fail("rseq_cpu_id", "child status %#x", st);
}

static void on_alrm(int s) { (void)s; }

static int rseq_abort_signal_child(void *arg) {
    (void)arg;
    if (rseq_call(&area_main, sizeof area_main, 0, RSEQ_SIG) != 0) return 10;
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alrm;
    sigaction(SIGALRM, &sa, NULL);
    struct itimerval it = { { 0, 0 }, { 0, 100 * 1000 } };
    setitimer(ITIMER_REAL, &it, NULL);
    /* Never finishes on its own (2^62 iterations): only the abort ends it. */
    int r = rseq_spin(&area_main, 1ul << 62);
    if (r != 1) return 11;
    if (area_main.rseq_cs != 0) return 12; /* the kernel clears rseq_cs on abort */
    return 0;
}

static void rseq_abort_signal(void) {
    int st = in_child(rseq_abort_signal_child, NULL, 20000);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass("rseq_abort_signal");
    else fail("rseq_abort_signal", "child status %#x%s", st, st == -1 ? " (timeout: no abort)" : "");
}

static int rseq_abort_preempt_child(void *arg) {
    (void)arg;
    if (rseq_call(&area_main, sizeof area_main, 0, RSEQ_SIG) != 0) return 10;
    /* Short sections, many tries: with every CPU oversubscribed by the
     * hogs the parent started, some section is preempted. */
    for (int i = 0; i < 4000; i++) {
        if (rseq_spin(&area_main, 1ul << 20) == 1) return 0;
    }
    return 11;
}

static void rseq_abort_preempt(void) {
    long ncpu = sysconf(_SC_NPROCESSORS_ONLN);
    if (ncpu < 1) ncpu = 1;
    pid_t hogs[64];
    int nh = (int)(ncpu * 2 > 64 ? 64 : ncpu * 2);
    fflush(stdout);
    for (int i = 0; i < nh; i++) {
        hogs[i] = fork();
        if (hogs[i] == 0) { for (volatile unsigned long x = 0;; x++) {} }
    }
    int st = in_child(rseq_abort_preempt_child, NULL, 60000);
    for (int i = 0; i < nh; i++) { if (hogs[i] > 0) { kill(hogs[i], SIGKILL); waitpid(hogs[i], NULL, 0); } }
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass("rseq_abort_preempt");
    else fail("rseq_abort_preempt", "child status %#x", st);
}

static int rseq_bad_sig_child(void *arg) {
    (void)arg;
    if (rseq_call(&area_main, sizeof area_main, 0, 0x12345678) != 0) return 10;
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alrm;
    sigaction(SIGALRM, &sa, NULL);
    struct itimerval it = { { 0, 0 }, { 0, 100 * 1000 } };
    setitimer(ITIMER_REAL, &it, NULL);
    rseq_spin(&area_main, 1ul << 62);
    return 11;
}

static void rseq_bad_sig(void) {
    int st = in_child(rseq_bad_sig_child, NULL, 20000);
    if (st >= 0 && WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV) pass("rseq_bad_sig");
    else fail("rseq_bad_sig", "child status %#x (want SIGSEGV)", st);
}

static void *rseq_thread_fn(void *arg) {
    (void)arg;
    long r = rseq_call(&area_thread, sizeof area_thread, 0, RSEQ_SIG);
    if (r == 0) rseq_call(&area_thread, sizeof area_thread, 1, RSEQ_SIG);
    return (void *)(intptr_t)(r == 0 ? 0 : errno);
}

static int rseq_inherit_child(void *arg) {
    const char *self = arg;
    if (rseq_call(&area_main, sizeof area_main, 0, RSEQ_SIG) != 0) return 10;
    pthread_t th;
    void *res;
    if (pthread_create(&th, NULL, rseq_thread_fn, NULL) != 0) return 11;
    pthread_join(th, &res);
    if (res != NULL) return 12;            /* a thread starts unregistered */
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        errno = 0;
        long r = rseq_call(&area_main, sizeof area_main, 0, RSEQ_SIG);
        _exit(r < 0 && errno == EBUSY ? 0 : 1);  /* fork inherits */
    }
    int st;
    waitpid(c, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) return 13;
    char *argv[] = { (char *)self, "--rseq-exec-check", NULL };
    execv(self, argv);
    return 14;
}

static void rseq_inherit(const char *self) {
    int st = in_child(rseq_inherit_child, (void *)self, 20000);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass("rseq_inherit");
    else fail("rseq_inherit", "child status %#x", st);
}

/* After execve: the registration is gone, so registering succeeds. */
static int rseq_exec_check(void) {
    long r = rseq_call(&area_main, sizeof area_main, 0, RSEQ_SIG);
    return r == 0 ? 0 : 20;
}

/* ── pidfd ───────────────────────────────────────────────────────────────── */

static void *idle_thread(void *arg) {
    *(volatile pid_t *)arg = (pid_t)syscall(SYS_gettid);
    for (;;) pause();
    return NULL;
}

static void pidfd_open_errors(void) {
    const char *n = "pidfd_open_errors";
    errno = 0;
    int r1 = pidfd_open(getpid(), 1);
    int e1 = errno;
    errno = 0;
    int r2 = pidfd_open(0, 0);
    int e2 = errno;
    errno = 0;
    int r3 = pidfd_open(0x3ffffff0, 0);
    int e3 = errno;
    volatile pid_t tid = 0;
    pthread_t th;
    pthread_create(&th, NULL, idle_thread, (void *)&tid);
    while (!tid) usleep(1000);
    errno = 0;
    int r4 = pidfd_open(tid, 0);
    int e4 = errno;
    int self = pidfd_open(getpid(), 0);
    int fdfl = self >= 0 ? fcntl(self, F_GETFD) : -1;
    if (r1 < 0 && e1 == EINVAL && r2 < 0 && e2 == EINVAL && r3 < 0 && e3 == ESRCH && r4 < 0 && e4 == EINVAL
        && self >= 0 && (fdfl & FD_CLOEXEC)) pass(n);
    else fail(n, "flags %d/%s pid0 %d/%s nopid %d/%s thread %d/%s self %d cloexec %d",
              r1, strerror(e1), r2, strerror(e2), r3, strerror(e3), r4, strerror(e4), self, fdfl);
    if (self >= 0) close(self);
}

static pid_t spawn_sleeper(int ms, int code) {
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) { usleep((useconds_t)ms * 1000); _exit(code); }
    return c;
}

static void pidfd_poll_exit(void) {
    const char *n = "pidfd_poll_exit";
    pid_t c = spawn_sleeper(300, 5);
    int fd = pidfd_open(c, 0);
    if (fd < 0) { fail(n, "pidfd_open: %s", strerror(errno)); waitpid(c, NULL, 0); return; }
    struct pollfd pf = { fd, POLLIN, 0 };
    int early = poll(&pf, 1, 0);
    pid_t w0 = waitpid(c, NULL, WNOHANG);
    long long t0 = now_ms();
    int r = poll(&pf, 1, 10000);
    long long dt = now_ms() - t0;
    int st = 0;
    pid_t w = waitpid(c, &st, WNOHANG);   /* what tokio does on POLLIN */
    struct pollfd pf2 = { fd, POLLIN, 0 };
    int after = poll(&pf2, 1, 0);
    if (early == 0 && w0 == 0 && r == 1 && (pf.revents & POLLIN) && dt >= 150 && w == c
        && WIFEXITED(st) && WEXITSTATUS(st) == 5 && after == 1 && (pf2.revents & POLLIN)) {
        printf("%s: readable after %lld ms\n", n, dt); pass(n);
    } else {
        fail(n, "early=%d w0=%d poll=%d rev=%#x dt=%lld waitpid=%d st=%#x after=%d", early, (int)w0, r, pf.revents, dt, (int)w, st, after);
        if (w != c) waitpid(c, NULL, 0);
    }
    close(fd);
}

static void pidfd_epoll_et(void) {
    const char *n = "pidfd_epoll_et";
    pid_t c = spawn_sleeper(200, 0);
    int fd = pidfd_open(c, 0);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = { .events = EPOLLIN | EPOLLET | EPOLLRDHUP, .data.u64 = 42 };
    if (fd < 0 || ep < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, fd, &ev) < 0) { fail(n, "setup: %s", strerror(errno)); waitpid(c, NULL, 0); return; }
    struct epoll_event out[2];
    int r0 = epoll_wait(ep, out, 2, 0);
    int r1 = epoll_wait(ep, out, 2, 10000);
    uint64_t data = r1 == 1 ? out[0].data.u64 : 0;
    uint32_t evs = r1 == 1 ? out[0].events : 0;
    int r2 = epoll_wait(ep, out, 2, 100);
    pid_t w = waitpid(c, NULL, 0);
    if (r0 == 0 && r1 == 1 && data == 42 && (evs & EPOLLIN) && r2 == 0 && w == c) pass(n);
    else fail(n, "before=%d exit=%d data=%llu ev=%#x again=%d", r0, r1, (unsigned long long)data, evs, r2);
    close(ep); close(fd);
}

static void pidfd_waitid(void) {
    const char *n = "pidfd_waitid";
    pid_t c = spawn_sleeper(200, 7);
    int fd = pidfd_open(c, 0);
    int nbfd = pidfd_open(c, O_NONBLOCK);
    siginfo_t si;
    memset(&si, 0xff, sizeof si);
    int rn = waitid(P_PIDFD, (id_t)fd, &si, WEXITED | WNOHANG);
    int pid_nohang = si.si_pid;
    errno = 0;
    int ra = waitid(P_PIDFD, (id_t)nbfd, &si, WEXITED);
    int ea = errno;
    memset(&si, 0, sizeof si);
    int r = waitid(P_PIDFD, (id_t)fd, &si, WEXITED);
    int ok = rn == 0 && pid_nohang == 0 && ra < 0 && ea == EAGAIN && r == 0 && si.si_pid == c
          && si.si_code == CLD_EXITED && si.si_status == 7;
    errno = 0;
    int rr = waitid(P_PIDFD, (id_t)fd, &si, WEXITED | WNOHANG);
    int er = errno;
    errno = 0;
    int rb = waitid(P_PIDFD, 0x1ff, &si, WEXITED);
    int eb = errno;
    if (ok && rr < 0 && er == ECHILD && rb < 0 && eb == EBADF) pass(n);
    else fail(n, "nohang=%d/%d nonblock=%d/%s wait=%d pid=%d code=%d status=%d again=%d/%s badfd=%d/%s",
              rn, pid_nohang, ra, strerror(ea), r, si.si_pid, si.si_code, si.si_status, rr, strerror(er), rb, strerror(eb));
    close(fd); close(nbfd);
}

static void pidfd_send_signal_case(void) {
    const char *n = "pidfd_send_signal";
    pid_t c = spawn_sleeper(20000, 0);
    int fd = pidfd_open(c, 0);
    if (fd < 0) { fail(n, "pidfd_open: %s", strerror(errno)); kill(c, SIGKILL); waitpid(c, NULL, 0); return; }
    int r0 = pidfd_send_signal(fd, 0, NULL, 0);
    int r1 = pidfd_send_signal(fd, SIGTERM, NULL, 0);
    struct pollfd pf = { fd, POLLIN, 0 };
    int pr = poll(&pf, 1, 10000);
    /* A zombie still takes (and drops) a signal; a reaped process is ESRCH. */
    int rz = pidfd_send_signal(fd, SIGTERM, NULL, 0);
    int st = 0;
    waitpid(c, &st, 0);
    errno = 0;
    int r2 = pidfd_send_signal(fd, SIGTERM, NULL, 0);
    int e2 = errno;
    int pp[2];
    if (pipe(pp) < 0) pp[0] = pp[1] = -1;
    errno = 0;
    int r3 = pidfd_send_signal(pp[0], SIGTERM, NULL, 0);
    int e3 = errno;
    errno = 0;
    int r4 = pidfd_send_signal(0x1fe, SIGTERM, NULL, 0);
    int e4 = errno;
    errno = 0;
    int r5 = pidfd_send_signal(fd, SIGTERM, NULL, 1);
    int e5 = errno;
    if (r0 == 0 && r1 == 0 && pr == 1 && rz == 0 && r2 < 0 && e2 == ESRCH && WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM
        && r3 < 0 && e3 == EINVAL && r4 < 0 && e4 == EBADF && r5 < 0 && e5 == EINVAL) pass(n);
    else fail(n, "sig0=%d term=%d poll=%d zombie=%d reaped=%d/%s status=%#x pipe=%d/%s badfd=%d/%s flags=%d/%s",
              r0, r1, pr, rz, r2, strerror(e2), st, r3, strerror(e3), r4, strerror(e4), r5, strerror(e5));
    close(fd); close(pp[0]); close(pp[1]);
}

static void pidfd_clone(void) {
    const char *n = "pidfd_clone";
    int pidfd = -1;
    fflush(stdout);
    long c = syscall(SYS_clone, CLONE_PIDFD | SIGCHLD, 0, &pidfd, 0, 0);
    if (c == 0) syscall(SYS_exit_group, 3);
    if (c < 0) { fail(n, "clone: %s", strerror(errno)); return; }
    struct pollfd pf = { pidfd, POLLIN, 0 };
    int pr = pidfd >= 0 ? poll(&pf, 1, 10000) : -1;
    siginfo_t si;
    memset(&si, 0, sizeof si);
    int wr = pidfd >= 0 ? waitid(P_PIDFD, (id_t)pidfd, &si, WEXITED) : -1;
    if (wr < 0) waitpid((pid_t)c, NULL, 0);
    int dummy = -1;
    errno = 0;
    long bad = syscall(SYS_clone, CLONE_PIDFD | CLONE_PARENT_SETTID | SIGCHLD, 0, &dummy, 0, 0);
    int eb = errno;
    if (bad == 0) syscall(SYS_exit_group, 0);
    if (bad > 0) waitpid((pid_t)bad, NULL, 0);
    if (pidfd >= 0 && pr == 1 && wr == 0 && si.si_pid == c && si.si_status == 3 && bad < 0 && eb == EINVAL) pass(n);
    else fail(n, "pidfd=%d poll=%d waitid=%d pid=%d/%ld status=%d settid=%ld/%s", pidfd, pr, wr, si.si_pid, c, si.si_status, bad, strerror(eb));
    if (pidfd >= 0) close(pidfd);
}

static void pidfd_getfd(void) {
    const char *n = "pidfd_getfd";
    int pp[2];
    if (pipe(pp) < 0) { fail(n, "pipe"); return; }
    int self = pidfd_open(getpid(), 0);
    int dup = (int)syscall(SYS_pidfd_getfd, self, pp[1], 0);
    int fl = dup >= 0 ? fcntl(dup, F_GETFD) : -1;
    ssize_t w = dup >= 0 ? write(dup, "z", 1) : -1;
    char b = 0;
    ssize_t r = read(pp[0], &b, 1);
    errno = 0;
    int bad = (int)syscall(SYS_pidfd_getfd, self, 0x1fd, 0);
    int eb = errno;
    if (dup >= 0 && dup != pp[1] && (fl & FD_CLOEXEC) && w == 1 && r == 1 && b == 'z' && bad < 0 && eb == EBADF) pass(n);
    else fail(n, "dup=%d cloexec=%d w=%zd r=%zd bad=%d/%s", dup, fl, w, r, bad, strerror(eb));
    if (dup >= 0) close(dup);
    close(self); close(pp[0]); close(pp[1]);
}

static void pidfd_proc_link(void) {
    const char *n = "pidfd_proc_link";
    int fd = pidfd_open(getpid(), 0);
    char path[64], buf[64] = {0};
    snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
    ssize_t k = readlink(path, buf, sizeof buf - 1);
    if (k > 0 && !strcmp(buf, "anon_inode:[pidfd]")) pass(n);
    else fail(n, "readlink %s -> %zd '%s' (%s)", path, k, buf, strerror(errno));
    close(fd);
}

/* ── one descriptor namespace ────────────────────────────────────────────── */

static int send_fd(int sock, int fd) {
    char c = 'F';
    struct iovec iov = { &c, 1 };
    union { struct cmsghdr h; char b[CMSG_SPACE(sizeof(int))]; } u;
    memset(&u, 0, sizeof u);
    struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.b, .msg_controllen = sizeof u.b };
    struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
    cm->cmsg_level = SOL_SOCKET;
    cm->cmsg_type = SCM_RIGHTS;
    cm->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cm), &fd, sizeof fd);
    return sendmsg(sock, &m, 0) == 1 ? 0 : -1;
}

static int recv_fd(int sock) {
    char c;
    struct iovec iov = { &c, 1 };
    union { struct cmsghdr h; char b[CMSG_SPACE(sizeof(int))]; } u;
    memset(&u, 0, sizeof u);
    struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.b, .msg_controllen = sizeof u.b };
    if (recvmsg(sock, &m, 0) != 1) return -1;
    struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
    if (!cm || cm->cmsg_type != SCM_RIGHTS) return -1;
    int fd;
    memcpy(&fd, CMSG_DATA(cm), sizeof fd);
    return fd;
}

/* A pidfd is an ordinary descriptor next to sockets and epoll fds: it takes
 * the lowest free number, dup2 / close_range / SCM_RIGHTS treat it like any
 * fd, an epoll fd watches it, and pidfd_getfd fetches a socket and an epoll
 * fd out of a process's table. */
static void pidfd_namespace(void) {
    const char *n = "pidfd_namespace";
    int sv[2];
    int ep = epoll_create1(EPOLL_CLOEXEC);
    if (ep < 0 || socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0) { fail(n, "setup: %s", strerror(errno)); return; }
    /* Free the number under one socket end; dup() reports the lowest free. */
    int hole = sv[0];
    close(sv[0]);
    int expect = dup(sv[1]);
    close(expect);
    pid_t c = spawn_sleeper(300, 4);
    int pfd = pidfd_open(c, 0);
    int lowest = pfd >= 0 && pfd == expect && expect <= hole;
    /* The next socket is numbered in the same space, past the pidfd. */
    int s2 = socket(AF_UNIX, SOCK_DGRAM, 0);
    int shared = s2 >= 0 && s2 != pfd && s2 < 0x10000;
    /* dup2 to a high number names the same process; close_range drops it. */
    int hi = dup2(pfd, 200);
    int hi_ok = hi == 200 && pidfd_send_signal(200, 0, NULL, 0) == 0;
    long cr = syscall(SYS_close_range, 200, 200, 0);
    errno = 0;
    int gone = pidfd_send_signal(200, 0, NULL, 0) < 0 && errno == EBADF;
    /* SCM_RIGHTS: the received descriptor is a pidfd for the same process. */
    int tx[2] = { -1, -1 }, rxfd = -1;
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, tx) == 0 && send_fd(tx[0], pfd) == 0) rxfd = recv_fd(tx[1]);
    int scm = rxfd >= 0 && rxfd != pfd && pidfd_send_signal(rxfd, 0, NULL, 0) == 0;
    /* epoll on the pidfd, waited on through a pidfd_getfd copy of the epoll fd. */
    struct epoll_event ev = { .events = EPOLLIN, .data.u64 = 77 };
    int add = epoll_ctl(ep, EPOLL_CTL_ADD, pfd, &ev);
    int self = pidfd_open(getpid(), 0);
    int ep2 = (int)syscall(SYS_pidfd_getfd, self, ep, 0);
    int ep2fl = ep2 >= 0 ? fcntl(ep2, F_GETFD) : -1;
    struct epoll_event out[2];
    memset(out, 0, sizeof out);
    int w0 = ep2 >= 0 ? epoll_wait(ep2, out, 2, 0) : -1;
    int w1 = ep2 >= 0 ? epoll_wait(ep2, out, 2, 10000) : -1;
    int epok = add == 0 && ep2 >= 0 && ep2 != ep && (ep2fl & FD_CLOEXEC) && w0 == 0 && w1 == 1
               && out[0].data.u64 == 77 && (out[0].events & EPOLLIN);
    int st = 0;
    pid_t w = waitpid(c, &st, 0);
    /* pidfd_getfd of a socket: the copy talks to the same peer. */
    int pr[2] = { -1, -1 }, sd = -1;
    char b = 0;
    int sockok = 0;
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pr) == 0) {
        sd = (int)syscall(SYS_pidfd_getfd, self, pr[0], 0);
        sockok = sd >= 0 && sd != pr[0] && send(sd, "k", 1, 0) == 1 && recv(pr[1], &b, 1, 0) == 1 && b == 'k';
    }
    if (lowest && shared && hi_ok && cr == 0 && gone && scm && epok && w == c && WEXITSTATUS(st) == 4 && sockok) pass(n);
    else fail(n, "pidfd=%d expect=%d hole=%d sock=%d dup2=%d close_range=%ld gone=%d scm=%d "
              "add=%d ep2=%d cloexec=%d wait=%d/%d getfd(sock)=%d b=%c",
              pfd, expect, hole, s2, hi, cr, gone, scm, add, ep2, ep2fl, w0, w1, sd, b ? b : '-');
    if (w != c) waitpid(c, NULL, 0);
    close(sd); close(pr[0]); close(pr[1]);
    close(self); close(ep2); close(rxfd); close(tx[0]); close(tx[1]);
    close(s2); close(pfd); close(sv[1]); close(ep);
}

/* splice between a pipe and a socket end, both directions. */
static void splice_socket(void) {
    const char *n = "splice_socket";
    int sv[2], pp[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0 || pipe(pp) < 0) { fail(n, "setup: %s", strerror(errno)); return; }
    char buf[16] = {0};
    ssize_t w = write(sv[1], "hello", 5);
    ssize_t a = sys_splice(sv[0], NULL, pp[1], NULL, 5, 0);   /* socket -> pipe */
    ssize_t b = sys_splice(pp[0], NULL, sv[0], NULL, 5, 0);   /* pipe -> socket */
    ssize_t r = read(sv[1], buf, sizeof buf);
    if (w == 5 && a == 5 && b == 5 && r == 5 && !memcmp(buf, "hello", 5)) pass(n);
    else fail(n, "write=%zd sock->pipe=%zd pipe->sock=%zd read=%zd (%s)", w, a, b, r, strerror(errno));
    close(sv[0]); close(sv[1]); close(pp[0]); close(pp[1]);
}

/* ── inet sockets shared across holders (lane fdgaps) ────────────────────── */

/* A TCP listener on 127.0.0.1 with a kernel-chosen port; *port gets it. */
static int tcp_listener(unsigned short *port) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    if (l < 0) return -1;
    int one = 1;
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = 0, .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    socklen_t al = sizeof a;
    if (bind(l, (struct sockaddr *)&a, sizeof a) < 0 || listen(l, 8) < 0
        || getsockname(l, (struct sockaddr *)&a, &al) < 0) { close(l); return -1; }
    *port = ntohs(a.sin_port);
    return l;
}

static int tcp_connect(unsigned short port) {
    int c = socket(AF_INET, SOCK_STREAM, 0);
    if (c < 0) return -1;
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port), .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    if (connect(c, (struct sockaddr *)&a, sizeof a) < 0) { close(c); return -1; }
    return c;
}

/* recv with a deadline, so a lost byte fails the case instead of hanging. */
static ssize_t recv_wait(int fd, char *b, size_t n, int ms) {
    struct pollfd p = { .fd = fd, .events = POLLIN };
    if (poll(&p, 1, ms) != 1) return -2;
    return recv(fd, b, n, 0);
}

static int accept_wait(int l, int ms) {
    struct pollfd p = { .fd = l, .events = POLLIN };
    if (poll(&p, 1, ms) != 1) return -2;
    return accept(l, NULL, NULL);
}

/* pidfd_getfd on an inet socket duplicates it into the caller (it was
 * EBADF): the copy shares the connection, the original's close leaves the
 * copy working, and the last close is what the peer sees as EOF. A listener
 * fetched the same way accepts. dup() of an inet socket works too (EINVAL). */
static void pidfd_getfd_inet(void) {
    const char *n = "pidfd_getfd_inet";
    unsigned short port = 0;
    int l = tcp_listener(&port);
    int c = l >= 0 ? tcp_connect(port) : -1;
    int s = c >= 0 ? accept_wait(l, 5000) : -1;
    if (s < 0) { fail(n, "setup l=%d c=%d s=%d: %s", l, c, s, strerror(errno)); close(l); close(c); return; }
    int self = pidfd_open(getpid(), 0);
    int cd = (int)syscall(SYS_pidfd_getfd, self, c, 0);
    int cl = cd >= 0 ? fcntl(cd, F_GETFD) : -1;
    char b = 0, e = 0;
    ssize_t s1 = cd >= 0 ? send(cd, "g", 1, 0) : -1;
    ssize_t r1 = recv_wait(s, &b, 1, 5000);
    close(c);                                   /* the copy keeps the socket */
    ssize_t s2 = cd >= 0 ? send(cd, "h", 1, 0) : -1;
    ssize_t r2 = recv_wait(s, &e, 1, 5000);
    int d = cd >= 0 ? dup(cd) : -1;             /* dup of inet: no longer EINVAL */
    ssize_t s3 = d >= 0 ? send(d, "i", 1, 0) : -1;
    char f = 0;
    ssize_t r3 = recv_wait(s, &f, 1, 5000);
    close(cd); close(d);                        /* last holder: the peer sees EOF */
    char z;
    ssize_t eof = recv_wait(s, &z, 1, 5000);
    /* The listener, fetched by pidfd_getfd, accepts. */
    int ld = (int)syscall(SYS_pidfd_getfd, self, l, 0);
    int c2 = tcp_connect(port);
    int s4 = ld >= 0 && c2 >= 0 ? accept_wait(ld, 5000) : -1;
    if (cd >= 0 && (cl & FD_CLOEXEC) && s1 == 1 && r1 == 1 && b == 'g' && s2 == 1 && r2 == 1 && e == 'h'
        && d >= 0 && s3 == 1 && r3 == 1 && f == 'i' && eof == 0 && ld >= 0 && s4 >= 0) pass(n);
    else fail(n, "getfd=%d cloexec=%d send/recv %zd/%zd %zd/%zd dup=%d %zd/%zd eof=%zd listener=%d accept=%d (%s)",
              cd, cl, s1, r1, s2, r2, d, s3, r3, eof, ld, s4, strerror(errno));
    close(s4); close(c2); close(ld); close(s); close(l); close(self);
}

/* fork shares inet sockets with the child, refcounted: the child talks on the
 * inherited connection and accepts on the inherited listener; its exit closes
 * nothing the parent still holds; the parent's listener still accepts after
 * the child re-armed it; and a bound UDP socket is shared too. */
static void inet_fork_shared(void) {
    const char *n = "inet_fork_shared";
    unsigned short port = 0;
    int l = tcp_listener(&port);
    int c = l >= 0 ? tcp_connect(port) : -1;
    int s = c >= 0 ? accept_wait(l, 5000) : -1;
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in ua = { .sin_family = AF_INET, .sin_port = 0, .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    socklen_t ual = sizeof ua;
    if (s < 0 || u < 0 || bind(u, (struct sockaddr *)&ua, sizeof ua) < 0
        || getsockname(u, (struct sockaddr *)&ua, &ual) < 0) {
        fail(n, "setup l=%d c=%d s=%d u=%d: %s", l, c, s, u, strerror(errno));
        close(l); close(c); close(s); close(u); return;
    }
    /* A second client the child will accept on the inherited listener. */
    int c2 = tcp_connect(port);
    pid_t pid = fork();
    if (pid == 0) {
        int code = 0;
        if (send(c, "c", 1, 0) != 1) code |= 1;
        int a = accept_wait(l, 5000);
        if (a < 0 || send(a, "a", 1, 0) != 1) code |= 2;
        if (sendto(u, "u", 1, 0, (struct sockaddr *)&ua, sizeof ua) != 1) code |= 4;
        _exit(code);
    }
    int st = -1;
    waitpid(pid, &st, 0);
    char b1 = 0, b2 = 0, b3 = 0, b4 = 0;
    ssize_t r1 = recv_wait(s, &b1, 1, 5000);    /* the child's byte, on the shared conn */
    ssize_t r2 = c2 >= 0 ? recv_wait(c2, &b2, 1, 5000) : -1; /* from the child's accept */
    ssize_t r3 = recv_wait(u, &b3, 1, 5000);    /* the child's datagram to the shared UDP socket */
    /* The child is gone; the parent's copies still work. */
    ssize_t w4 = send(c, "p", 1, 0);
    ssize_t r4 = recv_wait(s, &b4, 1, 5000);
    /* The listener the child re-armed still accepts in the parent. */
    int c3 = tcp_connect(port);
    int s3 = c3 >= 0 ? accept_wait(l, 5000) : -1;
    close(c);
    char z;
    ssize_t eof = recv_wait(s, &z, 1, 5000);
    if (WIFEXITED(st) && WEXITSTATUS(st) == 0 && r1 == 1 && b1 == 'c' && r2 == 1 && b2 == 'a'
        && r3 == 1 && b3 == 'u' && w4 == 1 && r4 == 1 && b4 == 'p' && s3 >= 0 && eof == 0) pass(n);
    else fail(n, "child=%#x conn=%zd/%c accept=%zd/%c udp=%zd/%c after=%zd/%zd/%c relisten=%d eof=%zd",
              st, r1, b1 ? b1 : '-', r2, b2 ? b2 : '-', r3, b3 ? b3 : '-', w4, r4, b4 ? b4 : '-', s3, eof);
    close(s3); close(c3); close(c2); close(s); close(l); close(u);
}

/* fstat on sockets and epoll fds (it was EBADF), and /proc/self/fd names:
 * a socket is S_IFSOCK with the inode readlink shows as socket:[ino], shared
 * by a dup; an epoll fd is an anonymous inode, mode 0600 with no type bits. */
static void fstat_sock_epoll(void) {
    const char *n = "fstat_sock_epoll";
    int t = socket(AF_INET, SOCK_STREAM, 0);
    int sv[2] = { -1, -1 };
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    int ep = epoll_create1(0);
    struct stat a, b, c, d;
    memset(&a, 0, sizeof a); memset(&b, 0, sizeof b); memset(&c, 0, sizeof c); memset(&d, 0, sizeof d);
    int fa = fstat(t, &a), fb = fstat(sv[0], &b), fc = fstat(ep, &c);
    int td = dup(t);
    int fd = fstat(td, &d);
    char path[64], la[64] = {0}, le[64] = {0}, want[64];
    snprintf(path, sizeof path, "/proc/self/fd/%d", t);
    ssize_t ka = readlink(path, la, sizeof la - 1);
    snprintf(path, sizeof path, "/proc/self/fd/%d", ep);
    ssize_t ke = readlink(path, le, sizeof le - 1);
    snprintf(want, sizeof want, "socket:[%lu]", (unsigned long)a.st_ino);
    if (fa == 0 && S_ISSOCK(a.st_mode) && fb == 0 && S_ISSOCK(b.st_mode) && a.st_ino != b.st_ino
        && fc == 0 && (c.st_mode & S_IFMT) == 0 && (c.st_mode & 0777) == 0600
        && fd == 0 && d.st_ino == a.st_ino && ka > 0 && !strcmp(la, want)
        && ke > 0 && !strcmp(le, "anon_inode:[eventpoll]")) pass(n);
    else fail(n, "inet=%d mode=%o ino=%lu unix=%d mode=%o ino=%lu epoll=%d mode=%o dup=%d ino=%lu link='%s' epoll link='%s'",
              fa, a.st_mode, (unsigned long)a.st_ino, fb, b.st_mode, (unsigned long)b.st_ino,
              fc, c.st_mode, fd, (unsigned long)d.st_ino, la, le);
    close(td); close(t); close(sv[0]); close(sv[1]); close(ep);
}

/* ── found by the in-guest kernel build (lane selfkernel) ─────────────────── */

/* rm -rf's loop: read a small batch of entries, unlink them, read on. A
 * getdents position that counts live entries skips as many survivors as were
 * just removed, and the rmdir then fails with ENOTEMPTY. */
static void readdir_unlink(const char *dir) {
    char name[64], d[256], p[512];
    snprintf(name, sizeof name, "readdir_unlink(%s)", dir);
    snprintf(d, sizeof d, "%s/.syscalltest-rdu-%d", dir, (int)getpid());
    if (mkdir(d, 0700) != 0) { fail(name, "mkdir: %s", strerror(errno)); return; }
    enum { N = 300 };
    for (int i = 0; i < N; i++) {
        snprintf(p, sizeof p, "%s/entry-with-a-longish-name-%03d", d, i);
        int fd = open(p, O_CREAT | O_WRONLY, 0600);
        if (fd < 0) { fail(name, "create %d: %s", i, strerror(errno)); return; }
        close(fd);
    }
    int dfd = open(d, O_RDONLY | O_DIRECTORY);
    if (dfd < 0) { fail(name, "open: %s", strerror(errno)); return; }
    int removed = 0;
    char buf[512]; /* a handful of entries per call */
    for (;;) {
        long n = syscall(SYS_getdents64, dfd, buf, sizeof buf);
        if (n < 0) { fail(name, "getdents64: %s", strerror(errno)); close(dfd); return; }
        if (n == 0) break;
        for (long off = 0; off < n;) {
            unsigned short reclen = *(unsigned short *)(buf + off + 16);
            const char *nm = buf + off + 19;
            if (strcmp(nm, ".") && strcmp(nm, "..")) {
                snprintf(p, sizeof p, "%s/%s", d, nm);
                if (unlink(p) == 0) removed++;
            }
            off += reclen;
        }
    }
    close(dfd);
    if (removed != N) { fail(name, "one pass removed %d of %d", removed, N); }
    else if (rmdir(d) != 0) { fail(name, "rmdir: %s", strerror(errno)); }
    else { pass(name); return; }
    /* clean up whatever the failing pass left behind */
    for (int i = 0; i < N; i++) { snprintf(p, sizeof p, "%s/entry-with-a-longish-name-%03d", d, i); unlink(p); }
    rmdir(d);
}

/* A regular file on fd 0 is always readable. busybox `read` polls fd 0
 * first; polling the console instead hung `while read; done < file`. */
static int poll_stdin_child(void *arg) {
    int fd = open((const char *)arg, O_RDONLY);
    if (fd < 0 || dup2(fd, 0) != 0) return 2;
    struct pollfd p = { .fd = 0, .events = POLLIN };
    int r = poll(&p, 1, 1000);
    if (r != 1 || !(p.revents & POLLIN)) return 3;
    char c;
    return read(0, &c, 1) == 1 ? 0 : 4;
}

static void poll_redirected_stdin(const char *dir) {
    const char *name = "poll_redirected_stdin";
    char path[256];
    int fd = make_file(dir, "stdin", 16, path, sizeof path);
    if (fd < 0) { skip(name, "no file"); return; }
    close(fd);
    int st = in_child(poll_stdin_child, path, 5000);
    unlink(path);
    if (st == -1) fail(name, "poll on a file stdin hung");
    else if (!WIFEXITED(st) || WEXITSTATUS(st)) fail(name, "child status %#x", st);
    else pass(name);
}

#ifndef SYS_fchmodat2
#define SYS_fchmodat2 452
#endif
/* fchmodat has no flags argument (junk in the 4th register is ignored);
 * fchmodat2 has one, and musl uses it for AT_SYMLINK_NOFOLLOW. */
static void fchmodat_flags(const char *dir) {
    const char *name = "fchmodat_flags";
    char path[256];
    int fd = make_file(dir, "chmod", 1, path, sizeof path);
    if (fd < 0) { skip(name, "no file"); return; }
    close(fd);
    struct stat st;
    long r = syscall(SYS_fchmodat, AT_FDCWD, path, 0640, AT_SYMLINK_NOFOLLOW);
    if (r != 0 || stat(path, &st) || (st.st_mode & 07777) != 0640) {
        fail(name, "fchmodat with a junk 4th argument: r=%ld mode=%o", r, st.st_mode & 07777);
    } else if (syscall(SYS_fchmodat2, AT_FDCWD, path, 0604, AT_SYMLINK_NOFOLLOW) != 0
               || stat(path, &st) || (st.st_mode & 07777) != 0604) {
        fail(name, "fchmodat2 NOFOLLOW: %s mode=%o", strerror(errno), st.st_mode & 07777);
    } else if (syscall(SYS_fchmodat2, AT_FDCWD, path, 0600, 0x4) != -1 || errno != EINVAL) {
        fail(name, "fchmodat2 with an unknown flag did not fail EINVAL");
    } else if (fchmodat(AT_FDCWD, path, 0600, AT_SYMLINK_NOFOLLOW) != 0
               || stat(path, &st) || (st.st_mode & 07777) != 0600) {
        fail(name, "libc fchmodat NOFOLLOW: %s", strerror(errno));
    } else {
        pass(name);
    }
    unlink(path);
}

/* preadv/pwritev/preadv2/pwritev2 were ENOSYS. Bun (the Claude Code CLI) reads
 * a non-blocking stdin with preadv2(0, iov, 1, -1, RWF_NOWAIT) and only falls
 * back to poll + read on EOPNOTSUPP; ENOSYS left its TUI deaf to keys. */
#ifndef RWF_NOWAIT
#define RWF_DSYNC 0x2
#define RWF_NOWAIT 0x8
#endif
static long rwv2(long nr, int fd, struct iovec *iov, int cnt, long long off, int flags) {
    return syscall(nr, fd, iov, cnt, (long)off, 0L, flags);
}
static void preadv2_cases(const char *dir) {
    const char *name = "preadv2";
    char path[256];
    int fd = make_file(dir, "preadv", 0, path, sizeof path);
    if (fd < 0) { skip(name, "no file"); return; }
    char a[4] = {0}, b[5] = {0}, c[3] = {0};
    struct iovec w[2] = { { "0123", 4 }, { "456789", 6 } };
    struct iovec r[2] = { { a, 3 }, { b, 4 } };
    struct iovec r1[1] = { { c, 2 } };
    int p[2] = { -1, -1 };
    const char *why = NULL;
    long n = 0;
    if ((n = rwv2(SYS_pwritev, fd, w, 2, 0, 0)) != 10) why = "pwritev at 0 did not write 10";
    else if (lseek(fd, 0, SEEK_CUR) != 0) why = "pwritev moved the file position";
    else if ((n = rwv2(SYS_preadv, fd, r, 2, 2, 0)) != 7 || memcmp(a, "234", 3) || memcmp(b, "5678", 4))
        why = "preadv at 2 did not read 234|5678";
    else if (lseek(fd, 0, SEEK_CUR) != 0) why = "preadv moved the file position";
    else if (rwv2(SYS_preadv, fd, r, 2, -1, 0) != -1 || errno != EINVAL) why = "preadv at -1 is not EINVAL";
    else if (rwv2(SYS_preadv2, fd, r1, 1, -1, 0) != 2 || memcmp(c, "01", 2) || lseek(fd, 0, SEEK_CUR) != 2)
        why = "preadv2 at -1 did not read at and advance the position";
    /* RWF_NOWAIT on a file: a read, or EOPNOTSUPP where the filesystem lacks
     * FMODE_NOWAIT (Linux tmpfs). */
    else if ((n = rwv2(SYS_preadv2, fd, r1, 1, 8, RWF_NOWAIT)) == -1 ? errno != EOPNOTSUPP
             : (n != 2 || memcmp(c, "89", 2)))
        why = "preadv2 RWF_NOWAIT on a file neither read nor said EOPNOTSUPP";
    else if (rwv2(SYS_preadv2, fd, r1, 1, 0, 0x40000000) != -1 || errno != EOPNOTSUPP)
        why = "preadv2 with an unknown flag is not EOPNOTSUPP";
    else if (rwv2(SYS_pwritev2, fd, w, 1, 10, RWF_DSYNC) != 4) why = "pwritev2 RWF_DSYNC did not write";
    else if (pipe(p)) why = "pipe";
    else if (rwv2(SYS_preadv2, p[0], r1, 1, -1, RWF_NOWAIT) != -1 || errno != EAGAIN)
        why = "preadv2 RWF_NOWAIT on an empty blocking pipe is not EAGAIN";
    else if (write(p[1], "xy", 2) != 2 || rwv2(SYS_preadv2, p[0], r1, 1, -1, RWF_NOWAIT) != 2
             || memcmp(c, "xy", 2))
        why = "preadv2 RWF_NOWAIT on a filled pipe did not read";
    else if (rwv2(SYS_preadv, p[0], r1, 1, 0, 0) != -1 || errno != ESPIPE)
        why = "preadv with an offset on a pipe is not ESPIPE";
    if (why) fail(name, "%s (n=%ld, %s)", why, n, strerror(errno));
    else pass(name);
    if (p[0] >= 0) { close(p[0]); close(p[1]); }
    close(fd); unlink(path);

    /* A terminal: Linux ttys lack FMODE_NOWAIT, so RWF_NOWAIT is EOPNOTSUPP
     * (an EAGAIN would also be a non-blocking answer). Never a block, never ENOSYS. */
    const char *tname = "preadv2_tty";
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0 || grantpt(m) || unlockpt(m)) { skip(tname, "no pty"); if (m >= 0) close(m); return; }
    int s = open(ptsname(m), O_RDWR | O_NOCTTY);
    if (s < 0) { skip(tname, "no pty slave"); close(m); return; }
    alarm(5);
    long t = rwv2(SYS_preadv2, s, r1, 1, -1, RWF_NOWAIT);
    int e = errno;
    alarm(0);
    if (t == -1 && (e == EOPNOTSUPP || e == EAGAIN)) pass(tname);
    else fail(tname, "preadv2 RWF_NOWAIT on an idle pty slave: %ld (%s)", t, strerror(e));
    close(s); close(m);
}

/* statx left stx_uid/stx_gid, the timestamps and the device numbers zero while
 * stx_mask claimed them: every file looked root's to a statx caller (Bun, so
 * the Claude Code CLI refused its own /tmp/claude-<uid> as a regular user). */
static void statx_matches_stat(const char *dir) {
    const char *name = "statx_matches_stat";
    char path[256];
    int fd = make_file(dir, "statx", 10, path, sizeof path);
    if (fd < 0) { skip(name, "no file"); return; }
    close(fd);
    if (geteuid() == 0 && chown(path, 1234, 5678) != 0) { fail(name, "chown: %s", strerror(errno)); unlink(path); return; }
    struct stat st;
    struct statx sx;
    memset(&sx, 0xa5, sizeof sx);
    if (stat(path, &st) != 0 || syscall(SYS_statx, AT_FDCWD, path, 0, STATX_BASIC_STATS, &sx) != 0) {
        fail(name, "stat/statx: %s", strerror(errno));
    } else if (sx.stx_uid != st.st_uid || sx.stx_gid != st.st_gid) {
        fail(name, "uid/gid statx %u/%u stat %u/%u", sx.stx_uid, sx.stx_gid, st.st_uid, st.st_gid);
    } else if (sx.stx_mtime.tv_sec != st.st_mtim.tv_sec || sx.stx_mtime.tv_nsec != (uint32_t)st.st_mtim.tv_nsec
               || sx.stx_ctime.tv_sec != st.st_ctim.tv_sec || sx.stx_atime.tv_sec != st.st_atim.tv_sec) {
        fail(name, "times statx m%lld c%lld a%lld stat m%lld c%lld a%lld",
             (long long)sx.stx_mtime.tv_sec, (long long)sx.stx_ctime.tv_sec, (long long)sx.stx_atime.tv_sec,
             (long long)st.st_mtim.tv_sec, (long long)st.st_ctim.tv_sec, (long long)st.st_atim.tv_sec);
    } else if (makedev(sx.stx_dev_major, sx.stx_dev_minor) != st.st_dev || sx.stx_ino != st.st_ino
               || sx.stx_mode != st.st_mode || sx.stx_size != (uint64_t)st.st_size) {
        fail(name, "dev/ino/mode/size differ");
    } else {
        /* fstat on a descriptor must agree too (tmpfs answered uid 0). */
        struct stat fs;
        int f2 = open(path, O_RDONLY);
        if (f2 < 0 || fstat(f2, &fs) != 0) fail(name, "open/fstat: %s", strerror(errno));
        else if (fs.st_uid != st.st_uid || fs.st_gid != st.st_gid)
            fail(name, "fstat uid/gid %u/%u stat %u/%u", fs.st_uid, fs.st_gid, st.st_uid, st.st_gid);
        else pass(name);
        if (f2 >= 0) close(f2);
    }
    unlink(path);
}

/* execve carried at most 64 argv and 64 envp strings (8 KiB) and dropped the
 * rest without an error. Exec ourselves with many of both, and a huge set
 * that must be refused with E2BIG instead of truncated. */
enum { BIG_ARGC = 3000, BIG_ENVC = 400 };
static int exec_count_check(int argc, char **argv) {
    extern char **environ;
    int envc = 0;
    for (char **e = environ; *e; e++) envc += !strncmp(*e, "STT_", 4);
    if (argc != BIG_ARGC) return 10;
    if (envc != BIG_ENVC) return 11;
    if (strcmp(argv[BIG_ARGC - 1], "arg-2999")) return 12;
    return 0;
}

static int exec_many_child(void *self) {
    char **av = calloc(BIG_ARGC + 1, sizeof *av);
    char **ev = calloc(BIG_ENVC + 1, sizeof *ev);
    av[0] = (char *)self;
    av[1] = "--exec-count-check";
    for (int i = 2; i < BIG_ARGC; i++) { av[i] = malloc(16); snprintf(av[i], 16, "arg-%d", i); }
    for (int i = 0; i < BIG_ENVC; i++) { ev[i] = malloc(32); snprintf(ev[i], 32, "STT_%d=value-%d", i, i); }
    execve(self, av, ev);
    return 20 + errno;
}

static int exec_e2big_child(void *self) {
    /* 4 MiB of arguments: past any ARG_MAX this kernel or Linux allows. */
    size_t n = 64, len = 64 * 1024;
    char **av = calloc(n + 2, sizeof *av);
    av[0] = (char *)self;
    for (size_t i = 1; i <= n; i++) { av[i] = malloc(len); memset(av[i], 'x', len - 1); av[i][len - 1] = 0; }
    execve(self, av, (char *[]){ NULL });
    return errno == E2BIG ? 0 : 1;
}

static void exec_many_args(const char *self) {
    const char *name = "exec_many_args";
    if (self[0] != '/') { skip(name, "no absolute self path"); return; }
    int st = in_child(exec_many_child, (void *)self, 10000);
    if (st == -1) fail(name, "timed out");
    else if (!WIFEXITED(st) || WEXITSTATUS(st)) fail(name, "child status %d (10: argc lost, 11: env lost, >=20: execve errno+20)", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    else pass(name);
    st = in_child(exec_e2big_child, (void *)self, 10000);
    if (st == -1 || !WIFEXITED(st) || WEXITSTATUS(st)) fail("exec_e2big", "status %#x", st);
    else pass("exec_e2big");
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--rseq-exec-check")) return rseq_exec_check();
    if (argc > 1 && !strcmp(argv[1], "--exec-count-check")) return exec_count_check(argc, argv);
    const char *dirs[8];
    int ndirs = 0;
    int o;
    while ((o = getopt(argc, argv, "d:")) != -1) {
        if (o == 'd' && ndirs < 8) dirs[ndirs++] = optarg;
        else { fprintf(stderr, "usage: syscalltest [-d dir]...\n"); return 2; }
    }
    if (ndirs == 0) {
        dirs[ndirs++] = "/tmp";
        if (geteuid() == 0) dirs[ndirs++] = "/root";
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    signal(SIGALRM, on_alrm);
    printf("syscalltest: uid %d, %ld cpus\n", (int)geteuid(), sysconf(_SC_NPROCESSORS_ONLN));
    /* argv[0] for the exec case: the path we were run by, or /proc/self/exe. */
    const char *self = strchr(argv[0], '/') ? argv[0] : "/proc/self/exe";

    splice_einval(dirs[0]);
    splice_espipe(dirs[0]);
    for (int i = 0; i < ndirs; i++) {
        char probe[256];
        int fd = make_file(dirs[i], "probe", 1, probe, sizeof probe);
        if (fd < 0) { skip("splice_dir", dirs[i]); continue; }
        close(fd); unlink(probe);
        splice_file_pipe(dirs[i], dirs[i]);
        splice_pipe_file(dirs[i], dirs[i]);
        splice_offsets(dirs[i], dirs[i]);
        splice_cat(dirs[i], dirs[i]);
    }
    splice_nonblock(dirs[0]);
    splice_block_wake(dirs[0]);
    splice_pipe_pipe();
    vmsplice_case();
    splice_socket();

    rseq_register();
    rseq_cpu_id();
    rseq_abort_signal();
    rseq_abort_preempt();
    rseq_bad_sig();
    rseq_inherit(self);

    pidfd_open_errors();
    pidfd_poll_exit();
    pidfd_epoll_et();
    pidfd_waitid();
    pidfd_send_signal_case();
    pidfd_clone();
    pidfd_getfd();
    pidfd_proc_link();
    pidfd_namespace();
    pidfd_getfd_inet();
    inet_fork_shared();
    fstat_sock_epoll();

    for (int i = 0; i < ndirs; i++) readdir_unlink(dirs[i]);
    poll_redirected_stdin(dirs[0]);
    fchmodat_flags(dirs[0]);
    preadv2_cases(dirs[0]);
    statx_matches_stat(dirs[0]);
    exec_many_args(self);

    printf("syscalltest: %d passed, %d failed, %d skipped\n", npass, nfail, nskip);
    return nfail > 100 ? 100 : nfail;
}
