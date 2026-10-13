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
 * Output: "<name>: PASS|FAIL ...|SKIP ..." per case and a summary
 * "syscalltest: N passed, M failed, K skipped". Exit status = failures.
 */
#define _GNU_SOURCE
#include <errno.h>
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
#include <sys/stat.h>
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

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--rseq-exec-check")) return rseq_exec_check();
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

    printf("syscalltest: %d passed, %d failed, %d skipped\n", npass, nfail, nskip);
    return nfail > 100 ? 100 : nfail;
}
