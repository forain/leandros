/*
 * syscalltest — splice(2)/tee(2)/vmsplice(2), rseq(2) and the pidfd family,
 * with the results Linux gives. Like nettest, the same file builds for the
 * host (cc syscalltest.c -lpthread) and for LeandrOS (scripts/toolchain/<arch>-linux-musl-cc -static).
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
 *   splice_socket_race   socket -> pipe while another thread fills the same
 *                        pipe: no byte read from the socket is lost
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
 *   rseq_cs_flags        flags in rseq_cs and in the area are ignored: the
 *                        deprecated RSEQ_CS_FLAG_NO_RESTART_* bits and
 *                        unknown bits alike, the section still aborts
 *                        (Linux 7.2; 6.x made unknown bits SIGSEGV)
 *   rseq_mm_cid          mm_cid is 0 for a single thread wherever it runs,
 *                        and below the thread count for concurrent threads
 *
 * pidfd cases:
 *   pidfd_open_errors    EINVAL (flags, pid 0, a non-leader thread), ESRCH
 *   pidfd_poll_exit      POLLIN exactly when the child exits, then
 *                        waitpid(WNOHANG) reaps it (tokio's reaping path)
 *   pidfd_wake_conmon    a pidfd is never readable while the process (or its
 *                        waitpid(-1) children) lives, under SIGURG/fork/socket churn
 *   pidfd_zombie_contend pidfd_zombie_stat with exit-path lock contention
 *   pidfd_zombie_stat    once a pidfd reads ready, /proc/<pid>/stat says Z
 *                        and kill(pid, 0) succeeds until the wait (2000x)
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
 * bad user pointers:
 *   uaccess_signal/fs/pty  NULL, unmapped, straddling and kernel pointers are
 *                        EFAULT with the caller alive
 *   uaccess_survivor     another process still uses the pty, pipes and dirs
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
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <sys/un.h>
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

static const char *volatile last_case = "(none)";
static pid_t main_pid; /* forked children fault on purpose (uaccess cases): stay quiet there */
static void pass(const char *n) { printf("%s: PASS\n", n); npass++; last_case = n; fflush(stdout); }
/* A fatal signal used to show only as RC=139: name the case it hit in. */
static void on_fatal(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    if (getpid() != main_pid) { signal(sig, SIG_DFL); raise(sig); return; }
    char b[200];
    int l = snprintf(b, sizeof b, "syscalltest: FATAL signal %d code %d addr %p pid %d tid %ld after case %s\n",
                     sig, si->si_code, si->si_addr, (int)getpid(), (long)syscall(SYS_gettid), last_case);
    if (l > 0) write(1, b, (size_t)l);
    signal(sig, SIG_DFL);
    raise(sig);
}
static void skip(const char *n, const char *why) { printf("%s: SKIP %s\n", n, why); nskip++; fflush(stdout); }
static void fail(const char *n, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
static void fail(const char *n, const char *fmt, ...) {
    va_list ap;
    printf("%s: FAIL ", n);
    va_start(ap, fmt); vprintf(fmt, ap); va_end(ap);
    printf("\n"); nfail++; last_case = n; fflush(stdout);
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
    ".globl rseq_spin_cs\n"
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
    ".globl rseq_spin_cs\n"
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

/* struct rseq_cs of rseq_spin: { u32 version; u32 flags; ... }. */
extern uint32_t rseq_spin_cs[];

static int rseq_flags_child(void *arg) {
    uintptr_t mode = (uintptr_t)arg; /* 0: cs deprecated, 1: cs unknown, 2: area unknown */
    if (rseq_call(&area_main, sizeof area_main, 0, RSEQ_SIG) != 0) return 10;
    if (mode == 0) rseq_spin_cs[1] = 1 | 2 | 4;   /* NO_RESTART_ON_PREEMPT|SIGNAL|MIGRATE */
    if (mode == 1) rseq_spin_cs[1] = 1u << 3;
    if (mode == 2) area_main.flags = 1u << 4;
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alrm;
    sigaction(SIGALRM, &sa, NULL);
    struct itimerval it = { { 0, 0 }, { 0, 100 * 1000 } };
    setitimer(ITIMER_REAL, &it, NULL);
    int r = rseq_spin(&area_main, 1ul << 62);
    return r == 1 ? 0 : 11;
}

static void rseq_cs_flags(void) {
    const char *n = "rseq_cs_flags";
    static const char *what[] = { "deprecated rseq_cs flags", "unknown rseq_cs flag", "unknown rseq flag" };
    for (uintptr_t mode = 0; mode < 3; mode++) {
        int st = in_child(rseq_flags_child, (void *)mode, 20000);
        if (!(st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0)) {
            fail(n, "%s: child status %#x (want the section aborted, exit 0)", what[mode], st);
            return;
        }
    }
    pass(n);
}

#define CID_THREADS 3
static struct rseq_area cid_area[CID_THREADS];
static volatile int cid_go, cid_bad, cid_ready;

static void *cid_thread(void *arg) {
    struct rseq_area *a = arg;
    if (rseq_call(a, sizeof *a, 0, RSEQ_SIG) != 0) { cid_bad = 1000; return NULL; }
    __sync_fetch_and_add(&cid_ready, 1);
    while (!cid_go) {}
    long long end = now_ms() + 300;
    while (now_ms() < end) {
        uint32_t c = a->mm_cid;
        if (c > CID_THREADS) cid_bad = (int)c + 1;
        sched_yield();
    }
    rseq_call(a, sizeof *a, 1, RSEQ_SIG);
    return NULL;
}

static int rseq_mm_cid_child(void *arg) {
    (void)arg;
    if (rseq_call(&area_main, sizeof area_main, 0, RSEQ_SIG) != 0) return 10;
    for (int i = 0; i < 200; i++) {
        sched_yield();
        if (i % 20 == 0) usleep(1000);
        if (area_main.mm_cid != 0) return 11;
    }
    rseq_call(&area_main, sizeof area_main, 1, RSEQ_SIG);
    pthread_t t[CID_THREADS];
    for (int i = 0; i < CID_THREADS; i++) pthread_create(&t[i], NULL, cid_thread, &cid_area[i]);
    while (cid_ready < CID_THREADS && !cid_bad) usleep(1000);
    cid_go = 1;
    for (int i = 0; i < CID_THREADS; i++) pthread_join(t[i], NULL);
    return cid_bad ? 12 : 0;
}

static void rseq_mm_cid(void) {
    int st = in_child(rseq_mm_cid_child, NULL, 20000);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass("rseq_mm_cid");
    else fail("rseq_mm_cid", "child status %#x (11: single thread's mm_cid not 0, 12: a cid above %d)", st, CID_THREADS);
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

/* pidfd_wake_conmon: podman's WaitForExit shape (c127). A "conmon" child
 * lives 300 ms, forks a grandchild that lives 200 ms and waitpid(-1)s until
 * ECHILD. Watchers ppoll(pidfd, 250 ms) in a loop under Go-style SIGURG
 * preemption plus fork/exit and socket churn. A pidfd may become readable
 * only once the process has exited: never while /proc/<pid> shows it alive,
 * never while waitid(P_PIDFD, WNOHANG|WNOWAIT) says running, and not before
 * the grandchild is gone; revents must be exactly POLLIN. */
#define PW_WATCHERS 4
static volatile int pw_stop;
static pthread_t pw_tid[PW_WATCHERS];
static volatile int pw_tid_ok[PW_WATCHERS];
static volatile pid_t pw_ktid[PW_WATCHERS];
static volatile int pw_bad;
static char pw_msg[PW_WATCHERS][960];
/* Per-watcher timeline written by the conmon child and grandchild into a
 * MAP_SHARED page, so a failure shows who was late: the sleeps, the
 * grandchild fork, or the parent getting back from fork(). */
struct pw_tl {
    long long c_start, c_slept, g_forked, g_exit, c_exit;
    int ns1, ns1_errno, gfork, gfork_errno, ns2, ns2_errno;
};
static struct pw_tl *pw_tl;
static volatile int pw_late_fork;
static int pw_iters = 10;

static void pw_urg(int s) { (void)s; }

static void *pw_signaller(void *a) {
    (void)a;
    while (!pw_stop) {
        /* tgkill by kernel tid, not pthread_kill: a watcher may exit (and be
         * joined, its pthread freed) between the check and the kill, and
         * pthread_kill on a freed pthread_t is a use-after-free. A stale tid
         * is just ESRCH. */
        for (int i = 0; i < PW_WATCHERS; i++) { pid_t t = pw_ktid[i]; if (pw_tid_ok[i] && t > 0) syscall(SYS_tgkill, getpid(), t, SIGURG); }
        struct timespec ts = { 0, 5000000 };
        nanosleep(&ts, NULL);
    }
    return NULL;
}

static void *pw_forkchurn(void *a) {
    (void)a;
    while (!pw_stop) {
        pid_t p = fork();
        if (p == 0) _exit(0);
        if (p > 0) waitpid(p, NULL, 0);
        struct timespec ts = { 0, 2000000 };
        nanosleep(&ts, NULL);
    }
    return NULL;
}

static void *pw_sockchurn(void *a) {
    (void)a;
    while (!pw_stop) {
        int sv[2];
        if (socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv) == 0) { close(sv[0]); close(sv[1]); }
        int s = socket(AF_INET, SOCK_STREAM, 0);
        if (s >= 0) close(s);
        struct timespec ts = { 0, 1000000 };
        nanosleep(&ts, NULL);
    }
    return NULL;
}

static void pw_conmon_child(struct pw_tl *tl) {
    tl->c_start = now_ms();
    struct timespec ts = { 0, 300000000 };
    tl->ns1 = nanosleep(&ts, NULL); tl->ns1_errno = tl->ns1 ? errno : 0;
    tl->c_slept = now_ms();
    pid_t g = fork();
    if (g == 0) {
        struct timespec t2 = { 0, 200000000 };
        tl->ns2 = nanosleep(&t2, NULL); tl->ns2_errno = tl->ns2 ? errno : 0;
        tl->g_exit = now_ms();
        _exit(0);
    }
    tl->gfork = (int)g; tl->gfork_errno = g < 0 ? errno : 0;
    tl->g_forked = now_ms();
    for (;;) {
        pid_t w = waitpid(-1, NULL, 0);
        if (w < 0 && errno == EINTR) continue;
        if (w < 0) break;
    }
    tl->c_exit = now_ms();
    _exit(0);
}

static void *pw_watcher(void *arg) {
    int idx = (int)(intptr_t)arg;
    pw_tid[idx] = pthread_self();
    pw_ktid[idx] = (pid_t)syscall(SYS_gettid);
    pw_tid_ok[idx] = 1;
    for (int it = 0; it < pw_iters && !pw_bad; it++) {
        struct pw_tl *tl = &pw_tl[idx];
        memset(tl, 0, sizeof *tl);
        long long tpre = now_ms();
        /* EAGAIN here is setup back-pressure, not a pidfd verdict: right after
         * procmax (it runs just before this suite in regress) exiting tasks can
         * still hold process-table slots for a moment. Retry for up to 2 s and
         * label a persistent failure as setup, never as an early wake. */
        pid_t c = -1;
        for (int t = 0; t < 200; t++) {
            c = fork();
            if (c >= 0 || errno != EAGAIN) break;
            struct timespec ts = { 0, 10 * 1000 * 1000 };
            nanosleep(&ts, NULL);
            tpre = now_ms();
        }
        if (c == 0) pw_conmon_child(tl);
        if (c < 0) { snprintf(pw_msg[idx], sizeof pw_msg[idx], "setup, not a wake verdict: fork: %s", strerror(errno)); pw_bad = 1; break; }
        long long t0 = now_ms();
        int fd = pidfd_open(c, 0);
        if (fd < 0) { snprintf(pw_msg[idx], sizeof pw_msg[idx], "pidfd_open: %s", strerror(errno)); pw_bad = 1; waitpid(c, NULL, 0); break; }
        for (;;) {
            struct pollfd pf = { fd, POLLIN, 0 };
            struct timespec to = { 0, 250000000 };
            int n = ppoll(&pf, 1, &to, NULL);
            if (n < 0 && errno == EINTR) continue;
            if (n == 0) continue;
            long long tready = now_ms();
            long long dt = tready - tpre;
            char pp[48]; snprintf(pp, sizeof pp, "/proc/%d/stat", (int)c);
            int alive = 0; char st = '?';
            FILE *f = fopen(pp, "r");
            if (f) { char buf[256]; if (fgets(buf, sizeof buf, f)) { char *p = strrchr(buf, ')'); if (p && p[1] == ' ') st = p[2]; } fclose(f); alive = st != 'Z' && st != 'X'; }
            siginfo_t si; memset(&si, 0, sizeof si);
            int wr = (int)syscall(SYS_waitid, P_PIDFD, fd, &si, WEXITED | WNOHANG | WNOWAIT, NULL);
            int running = wr == 0 && si.si_pid == 0;
            /* The child cannot exit before tpre + 500 ms (it sleeps 300 ms,
             * then waits for a 200 ms grandchild), and readiness must not
             * precede the exits the child and grandchild recorded. dt used to
             * be taken after fork() returned in the parent, so a parent
             * resumed late (child-first scheduling under load) looked like an
             * early wake. Keep a note when that old measure would have
             * fired, with the timeline, without failing. */
            int early = dt < 495 || (tl->c_exit && tready < tl->c_exit) || (tl->g_exit && tready < tl->g_exit);
            if (!early && tready - t0 < 480 && !pw_late_fork) {
                pw_late_fork = 1;
                printf("pidfd_wake_conmon: note: parent resumed %lld ms after fork (after-fork dt=%lld, from pre-fork %lld); child start +%lld slept +%lld gforked +%lld gexit +%lld cexit +%lld\n",
                       t0 - tpre, tready - t0, dt, tl->c_start - tpre, tl->c_slept - tpre, tl->g_forked - tpre, tl->g_exit - tpre, tl->c_exit - tpre);
            }
            if (n < 0 || pf.revents != POLLIN || alive || running || early) {
                snprintf(pw_msg[idx], sizeof pw_msg[idx], "iter %d pid=%d n=%d errno=%d revents=%#x dt=%lld (fork returned +%lld) proc_state=%c waitid=%d si_pid=%d | child start +%lld slept +%lld ns1=%d/%d gfork=%d/%d at +%lld gexit +%lld ns2=%d/%d cexit +%lld",
                         it, (int)c, n, errno, pf.revents, dt, t0 - tpre, st, wr, (int)si.si_pid,
                         tl->c_start - tpre, tl->c_slept - tpre, tl->ns1, tl->ns1_errno, tl->gfork, tl->gfork_errno, tl->g_forked - tpre, tl->g_exit - tpre, tl->ns2, tl->ns2_errno, tl->c_exit - tpre);
                /* Failure-only DETAIL: is the early readiness transient? */
                struct pollfd p2 = { fd, POLLIN, 0 }; struct timespec z = { 0, 0 };
                int n2 = ppoll(&p2, 1, &z, NULL);
                siginfo_t s2; memset(&s2, 0, sizeof s2);
                int w2 = (int)syscall(SYS_waitid, P_PIDFD, fd, &s2, WEXITED | WNOHANG | WNOWAIT, NULL);
                size_t l = strlen(pw_msg[idx]);
                snprintf(pw_msg[idx] + l, sizeof pw_msg[idx] - l, " | recheck n=%d revents=%#x waitid=%d si_pid=%d", n2, p2.revents, w2, (int)s2.si_pid);
                /* Which clause fired, and whether the clocks agree: an early
                 * wake with a sane timeline points at the kernel's clock or
                 * pidfd state, a bad timeline at nanosleep/scheduling. */
                l = strlen(pw_msg[idx]);
                struct timespec cm, cb, cr;
                clock_gettime(CLOCK_MONOTONIC, &cm); clock_gettime(CLOCK_BOOTTIME, &cb); clock_gettime(CLOCK_REALTIME, &cr);
                snprintf(pw_msg[idx] + l, sizeof pw_msg[idx] - l, " | why: n<0=%d revents!=IN=%d alive=%d running=%d early=%d(dt<495=%d c_exit>tready=%d g_exit>tready=%d) tready=%lld tpre=%lld mono=%lld boot=%lld real=%lld ms",
                         n < 0, pf.revents != POLLIN, alive, running, early, dt < 495, tl->c_exit && tready < tl->c_exit, tl->g_exit && tready < tl->g_exit,
                         tready, tpre, (long long)cm.tv_sec * 1000 + cm.tv_nsec / 1000000, (long long)cb.tv_sec * 1000 + cb.tv_nsec / 1000000, (long long)cr.tv_sec * 1000 + cr.tv_nsec / 1000000);
                pw_bad = 1;
            }
            break;
        }
        close(fd);
        waitpid(c, NULL, 0);
    }
    pw_tid_ok[idx] = 0;
    return NULL;
}

/* pidfd_zombie_stat (lane exitorder): an exited, unwaited child is a zombie
 * to every observer as soon as its pidfd reads ready. The kernel recycles a
 * zombie's run-queue slot eagerly and keeps it only as an exit record; /proc
 * used to answer "R (running)" for such a pid and kill(pid, 0) ESRCH. */
static volatile int zc_stop;
static void *zc_churn(void *a) {
    (void)a;
    while (!zc_stop) {
        int s = socket(AF_UNIX, SOCK_STREAM, 0); if (s >= 0) close(s);
        int p[2]; if (pipe(p) == 0) { close(p[0]); close(p[1]); }
        int o = open("/dev/null", O_RDONLY); if (o >= 0) close(o);
    }
    return NULL;
}

static void zombie_loop(const char *n, int contend);
static void pidfd_zombie_stat(void) { zombie_loop("pidfd_zombie_stat", 0); }
/* pidfd_zombie_contend (lane zombblock): the same check while exiting
 * children close fds/sockets against churning threads, so exit()'s tail meets
 * contended sleeping locks after the task is already a Zombie. */
static void pidfd_zombie_contend(void) { zombie_loop("pidfd_zombie_contend", 1); }

static void zombie_loop(const char *n, int contend) {
    const char *e = getenv("PIDFDZOMBIE_ITERS");
    int iters = e && atoi(e) > 0 ? atoi(e) : 2000;
    int bad_state = 0, bad_kill = 0; char first[160] = "";
    pthread_t ch[4]; int nch = 0;
    if (contend) { zc_stop = 0; for (; nch < 4; nch++) pthread_create(&ch[nch], NULL, zc_churn, NULL); }
    for (int it = 0; it < iters; it++) {
        pid_t c = fork();
        if (c < 0) { if (errno == EAGAIN) { usleep(10000); it--; continue; } fail(n, "fork: %s", strerror(errno)); return; }
        if (c == 0) {
            if (contend) for (int k = 0; k < 6; k++) { int p[2]; if (pipe(p) < 0) break; socket(AF_UNIX, SOCK_STREAM, 0); }
            _exit(0);
        }
        int fd = pidfd_open(c, 0);
        if (fd < 0) { fail(n, "pidfd_open: %s", strerror(errno)); waitpid(c, NULL, 0); goto out; }
        struct pollfd pf = { fd, POLLIN, 0 };
        int r;
        do r = poll(&pf, 1, 5000); while (r < 0 && errno == EINTR);
        if (r != 1) { fail(n, "iter %d: poll=%d revents=%#x", it, r, pf.revents); close(fd); waitpid(c, NULL, 0); goto out; }
        char pp[48], buf[256], st = '?'; snprintf(pp, sizeof pp, "/proc/%d/stat", (int)c);
        FILE *f = fopen(pp, "r");
        if (f) { if (fgets(buf, sizeof buf, f)) { char *p = strrchr(buf, ')'); if (p && p[1] == ' ') st = p[2]; } fclose(f); }
        int kr = kill(c, 0), ke = kr ? errno : 0;
        if (st != 'Z' && !bad_state++ && !first[0])
            snprintf(first, sizeof first, "iter %d pid %d: stat state '%c' (%s)", it, (int)c, st, f ? "read" : strerror(errno));
        if (kr != 0 && !bad_kill++ && !first[0])
            snprintf(first, sizeof first, "iter %d pid %d: kill(pid,0)=%s", it, (int)c, strerror(ke));
        close(fd);
        if (waitpid(c, NULL, 0) != c) { fail(n, "iter %d: waitpid: %s", it, strerror(errno)); goto out; }
    }
    if (bad_state || bad_kill) fail(n, "%d/%d not Z in /proc, %d kill ESRCH; first: %s", bad_state, iters, bad_kill, first);
    else pass(n);
out:
    if (contend) { zc_stop = 1; for (int i = 0; i < nch; i++) pthread_join(ch[i], NULL); }
}

static void pidfd_wake_conmon(void) {
    const char *n = "pidfd_wake_conmon";
    const char *e = getenv("PIDFDWAKE_ITERS");
    if (e && atoi(e) > 0) pw_iters = atoi(e);
    struct sigaction sa; memset(&sa, 0, sizeof sa);
    sa.sa_handler = pw_urg; sa.sa_flags = SA_RESTART;
    sigaction(SIGURG, &sa, NULL);
    pw_stop = 0; pw_bad = 0; pw_late_fork = 0;
    if (!pw_tl) {
        void *m = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
        if (m == MAP_FAILED) { fail(n, "mmap shared timeline: %s", strerror(errno)); return; }
        pw_tl = m;
    }
    pthread_t sig, fc, sc, w[PW_WATCHERS];
    pthread_create(&fc, NULL, pw_forkchurn, NULL);
    pthread_create(&sc, NULL, pw_sockchurn, NULL);
    for (int i = 0; i < PW_WATCHERS; i++) { pw_msg[i][0] = 0; pthread_create(&w[i], NULL, pw_watcher, (void *)(intptr_t)i); }
    pthread_create(&sig, NULL, pw_signaller, NULL);
    for (int i = 0; i < PW_WATCHERS; i++) pthread_join(w[i], NULL);
    pw_stop = 1;
    pthread_join(sig, NULL); pthread_join(fc, NULL); pthread_join(sc, NULL);
    if (!pw_bad) { printf("%s: %d x %d iterations clean\n", n, PW_WATCHERS, pw_iters); pass(n); return; }
    for (int i = 0; i < PW_WATCHERS; i++) if (pw_msg[i][0]) { fail(n, "early pidfd wake: %s", pw_msg[i]); return; }
    fail(n, "unknown");
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

/* ── bad iov_base must give EFAULT, never SIGSEGV in the caller ─────────── */

/* mode: 0 sendmsg, 1 writev, 2 readv, 3 recvmsg, 4 pwritev, 5 preadv */
static int iov_fault_child(void *arg) {
    long mode = (long)arg / 2, which = (long)arg % 2;
    void *bad = which ? (void *)0xffff800000001000UL : (void *)1;
    int sv[2], pp[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0 || pipe(pp) < 0) return 90;
    char ok[8] = "x";
    write(pp[1], ok, 1);
    char tmpl[] = "/tmp/iovfaultXXXXXX";
    int ff = mkstemp(tmpl);
    if (ff < 0) return 92;
    unlink(tmpl);
    write(ff, "0123456789abcdef0", 17);
    struct iovec iov[2] = { { ok, 1 }, { bad, 16 } };
    struct msghdr m = { .msg_iov = iov, .msg_iovlen = 2 };
    errno = 0;
    ssize_t r;
    switch (mode) {
    case 0: r = sendmsg(sv[0], &m, 0); break;
    case 1: r = writev(sv[0], iov, 2); break;
    case 2: write(sv[1], "0123456789abcdef0", 17); r = readv(sv[0], iov, 2); break;
    case 3: write(sv[1], "0123456789abcdef0", 17); r = recvmsg(sv[0], &m, 0); break;
    case 4: r = pwritev(ff, iov, 2, 0); break;
    default: r = preadv(ff, iov, 2, 0); break;
    }
    /* Linux: the first iovec may be consumed (short count) before the fault;
     * either -1/EFAULT or a short count is acceptable, a signal is not. */
    if (r == -1 && errno == EFAULT) return 0;
    if (r >= 0 && r < 17) return 0;
    return 91;
}

static void iov_fault(void) {
    static const char *names[] = { "sendmsg", "writev", "readv", "recvmsg", "pwritev", "preadv" };
    for (long mode = 0; mode < 6; mode++)
        for (long which = 0; which < 2; which++) {
            char n[48];
            snprintf(n, sizeof n, "iov_efault_%s_%s", names[mode], which ? "kernel" : "one");
            int st = in_child(iov_fault_child, (void *)(mode * 2 + which), 10000);
            if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass(n);
            else fail(n, "status=%d%s", st, (st >= 0 && WIFSIGNALED(st)) ? " (killed by signal)" : "");
        }
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

/* splice_socket_race: a socket -> pipe splice blocks in the socket read
 * while another thread nearly fills the same pipe; then 4096 bytes arrive.
 * Every one of them must end up in the pipe or still be in the socket: the
 * splice used to size its read from the space it saw before blocking, find
 * 100 bytes left after the read, and drop the rest (a socket cannot be
 * rewound). */
struct ssr { int in, out; size_t len; ssize_t r; volatile int done; };
static void *ssr_splice(void *p) {
    struct ssr *s = p;
    s->r = sys_splice(s->in, NULL, s->out, NULL, s->len, 0);
    s->done = 1;
    return NULL;
}
static void *ssr_write(void *p) {
    struct ssr *s = p;
    char *b = malloc(s->len);
    memset(b, 'B', s->len);
    size_t off = 0;
    while (off < s->len) {
        ssize_t w = write(s->out, b + off, s->len - off);
        if (w <= 0) break;
        off += (size_t)w;
    }
    s->r = (ssize_t)off;
    free(b);
    s->done = 1;
    return NULL;
}
static void splice_socket_race(void) {
    const char *n = "splice_socket_race";
    int sv[2], pp[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0 || pipe(pp) < 0) { fail(n, "setup: %s", strerror(errno)); return; }
    long cap = 16384;
#ifdef F_GETPIPE_SZ
    long c = fcntl(pp[1], F_GETPIPE_SZ);
    if (c > 0) cap = c;
#endif
    struct ssr sp = { .in = sv[0], .out = pp[1], .len = 16384 };
    struct ssr wr = { .in = -1, .out = pp[1], .len = (size_t)cap - 100 };
    pthread_t ta, tb;
    alarm(30);
    pthread_create(&ta, NULL, ssr_splice, &sp);
    usleep(200 * 1000);                    /* the splice is parked in the socket read */
    pthread_create(&tb, NULL, ssr_write, &wr);
    usleep(200 * 1000);                    /* the writer took what the pipe let it */
    static char sbuf[4096];
    memset(sbuf, 'S', sizeof sbuf);
    ssize_t sw = write(sv[1], sbuf, sizeof sbuf);
    /* Let the splice finish before draining anything: a drain would free
     * the room the racing writer took and hide the loss. With its space
     * reserved the splice needs no reader to complete. */
    for (int i = 0; i < 100 && !sp.done; i++) usleep(20 * 1000);
    fcntl(pp[0], F_SETFL, fcntl(pp[0], F_GETFL) | O_NONBLOCK);
    size_t got_s = 0, got_b = 0, other = 0;
    char rb[4096];
    for (;;) {
        ssize_t r = read(pp[0], rb, sizeof rb);
        if (r > 0) {
            for (ssize_t i = 0; i < r; i++) {
                if (rb[i] == 'S') got_s++; else if (rb[i] == 'B') got_b++; else other++;
            }
            continue;
        }
        if (sp.done && wr.done) break;
        struct pollfd pf = { .fd = pp[0], .events = POLLIN };
        poll(&pf, 1, 50);
    }
    pthread_join(ta, NULL);
    pthread_join(tb, NULL);
    /* Whatever the splice left in the socket was not lost. */
    fcntl(sv[0], F_SETFL, fcntl(sv[0], F_GETFL) | O_NONBLOCK);
    size_t left = 0;
    for (;;) { ssize_t r = read(sv[0], rb, sizeof rb); if (r <= 0) break; left += (size_t)r; }
    alarm(0);
    if (sw == 4096 && got_s + left == 4096 && sp.r == (ssize_t)got_s && got_b == (size_t)wr.r
        && wr.r == (ssize_t)wr.len && other == 0) pass(n);
    else fail(n, "sent %zd: splice=%zd, in pipe S=%zu B=%zu (wrote %zd) other=%zu, left in socket %zu",
              sw, sp.r, got_s, got_b, wr.r, other, left);
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
/* musl 1.2.6 pwrite() tries pwritev2(.., RWF_NOAPPEND) first. */
#ifndef RWF_APPEND
#define RWF_APPEND 0x10
#endif
#ifndef RWF_NOAPPEND
#define RWF_NOAPPEND 0x20
#endif
static void pwritev2_append_cases(const char *dir) {
    const char *name = "pwritev2_append";
    char path[256], buf[16] = {0};
    int fd = make_file(dir, "noapp", 0, path, sizeof path);
    if (fd < 0) { skip(name, "no file"); return; }
    close(fd);
    struct iovec w[1] = { { "AB", 2 } };
    int p[2] = { -1, -1 };
    const char *why = NULL;
    long n = 0;
    fd = open(path, O_RDWR | O_APPEND);
    if (fd < 0) { fail(name, "open O_APPEND: %s", strerror(errno)); unlink(path); return; }
    if (write(fd, "0123456789", 10) != 10) why = "seed write";
    else if ((n = rwv2(SYS_pwritev2, fd, w, 1, 0, RWF_NOAPPEND)) != 2) why = "NOAPPEND on O_APPEND fd did not write";
    else if (pread(fd, buf, 4, 0) != 4 || memcmp(buf, "AB23", 4)) why = "NOAPPEND did not write at offset 0";
    else if (lseek(fd, 0, SEEK_END) != 10) why = "NOAPPEND changed the file size";
    else if ((n = rwv2(SYS_pwritev2, fd, w, 1, 0, 0)) != 2) why = "plain pwritev2 on O_APPEND fd";
    else if (lseek(fd, 0, SEEK_END) != 12) why = "plain pwritev2 on O_APPEND fd did not append";
    else if (rwv2(SYS_pwritev2, fd, w, 1, 0, 0x40000000) != -1 || errno != EOPNOTSUPP)
        why = "unknown flag is not EOPNOTSUPP";
    else if (rwv2(SYS_pwritev2, 9999, w, 1, 0, RWF_NOAPPEND) != -1 || errno != EBADF)
        why = "NOAPPEND on a bad fd is not EBADF";
    else if (pipe(p)) why = "pipe";
    else if (rwv2(SYS_pwritev2, p[1], w, 1, 0, RWF_NOAPPEND) != -1 || errno != ESPIPE)
        why = "NOAPPEND with an offset on a pipe is not ESPIPE";
    if (!why) {
        close(fd);
        fd = open(path, O_RDWR);
        if (fd < 0) why = "reopen";
        else if ((n = rwv2(SYS_pwritev2, fd, w, 1, 0, RWF_APPEND)) != 2) why = "RWF_APPEND did not write";
        else if (lseek(fd, 0, SEEK_END) != 14) why = "RWF_APPEND on a non-append fd did not append";
        else if (pread(fd, buf, 2, 12) != 2 || memcmp(buf, "AB", 2)) why = "RWF_APPEND data not at EOF";
    }
    if (why) fail(name, "%s (n=%ld, %s)", why, n, strerror(errno));
    else pass(name);
    if (p[0] >= 0) { close(p[0]); close(p[1]); }
    if (fd >= 0) close(fd);
    unlink(path);
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

/* ── bad user pointers ────────────────────────────────────────────────────
 * Every syscall below used to dereference its user pointer directly — some
 * with a server lock held (the fd table, the pty pool, the console termios,
 * a mount) — so a bad pointer killed the caller instead of returning EFAULT,
 * and a killed caller holding a lock wedged everyone else. Each case hands
 * the syscall four bad pointers in a forked child with a watchdog: NULL, an
 * unmapped page, a buffer straddling the end of a mapping (its first bytes
 * mapped, the rest not), and a kernel address. Every one must be EFAULT with
 * the child alive; `uaccess_survivor` then checks that another process can
 * still use the pty, pipes, directories and signals. */

#include <sys/ioctl.h>
#include <sys/mman.h>
#include <termios.h>

struct badp { const char *kind; uintptr_t p; };
enum { NBAD = 4 };

/* The four bad pointers. `valid` is how many bytes of the straddling buffer
 * are mapped (filled with 'a', no NUL), the rest of it lies in an unmapped
 * page. */
static int make_bad(size_t valid, struct badp b[NBAD]) {
    long pg = sysconf(_SC_PAGESIZE);
    char *m = mmap(NULL, 2 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *u = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED || u == MAP_FAILED) return -1;
    memset(m, 'a', pg);
    munmap(m + pg, pg);
    munmap(u, pg);
    /* Unmapping the neighbour must not touch the mapped page. */
    for (long i = 0; i < pg; i++)
        if (m[i] != 'a') {
            printf("  make_bad: byte %ld of the mapped page is 0x%02x after munmap of its neighbour\n",
                   i, (unsigned char)m[i]);
            fflush(stdout);
            return -1;
        }
    /* ...and leave no translation behind: a read of either unmapped page
     * from user mode must SIGSEGV. */
    const volatile char *probe[2] = { m + pg, u };
    for (int k = 0; k < 2; k++) {
        int fd[2];
        if (pipe(fd)) return -1;
        pid_t c = fork();
        if (c == 0) { char v = probe[k][0]; write(fd[1], &v, 1); _exit(0); }
        close(fd[1]);
        char v = 0; ssize_t got = read(fd[0], &v, 1); close(fd[0]);
        int st = 0; waitpid(c, &st, 0);
        if (got == 1 || !WIFSIGNALED(st) || WTERMSIG(st) != SIGSEGV) {
            printf("  make_bad: %s page %p still readable after munmap (byte 0x%02x, status 0x%x)\n",
                   k ? "unmapped" : "straddle tail", (void *)probe[k], (unsigned char)v, st);
            fflush(stdout);
            return -1;
        }
    }
    b[0] = (struct badp){ "null", 0 };
    b[1] = (struct badp){ "unmapped", (uintptr_t)u };
    b[2] = (struct badp){ "straddle", (uintptr_t)(m + pg - valid) };
    b[3] = (struct badp){ "kernel", (uintptr_t)0xffff800000001000ULL };
    return 0;
}

static int ua_bad;
#define WANT_EFAULT(what, kind, call) do { \
        errno = 0; long r_ = (long)(call); \
        if (!(r_ == -1 && errno == EFAULT)) { \
            printf("  %s/%s: r=%ld errno=%d (want EFAULT)\n", what, kind, r_, errno); \
            fflush(stdout); ua_bad++; \
        } \
    } while (0)

static int ua_sigaction(void *a) {
    (void)a; struct badp b[NBAD];
    if (make_bad(4, b)) return 99; /* less than a sigset, sigaction, stack_t */
    for (int i = 0; i < NBAD; i++) {
        if (!b[i].p) continue; /* NULL act/oldact is legal */
        WANT_EFAULT("rt_sigaction act", b[i].kind, syscall(SYS_rt_sigaction, SIGUSR1, b[i].p, 0, 8));
        WANT_EFAULT("rt_sigaction oldact", b[i].kind, syscall(SYS_rt_sigaction, SIGUSR1, 0, b[i].p, 8));
        WANT_EFAULT("rt_sigprocmask set", b[i].kind, syscall(SYS_rt_sigprocmask, SIG_BLOCK, b[i].p, 0, 8));
        WANT_EFAULT("rt_sigprocmask oldset", b[i].kind, syscall(SYS_rt_sigprocmask, SIG_BLOCK, 0, b[i].p, 8));
        WANT_EFAULT("sigaltstack ss", b[i].kind, syscall(SYS_sigaltstack, b[i].p, 0));
        WANT_EFAULT("sigaltstack oss", b[i].kind, syscall(SYS_sigaltstack, 0, b[i].p));
    }
    return ua_bad;
}

static int ua_fs(void *a) {
    (void)a; struct badp b[NBAD];
    /* 2 mapped bytes: less than the "/tmp" cwd, less than one dirent, less
     * than the two ints of pipe2 and the int of FIONREAD. */
    if (make_bad(2, b)) return 99;
    if (chdir("/tmp")) return 98;
    for (int i = 0; i < NBAD; i++) {
        int d = open("/", O_RDONLY | O_DIRECTORY);
        WANT_EFAULT("getdents64", b[i].kind, syscall(SYS_getdents64, d, b[i].p, 4096));
        close(d);
        WANT_EFAULT("getcwd", b[i].kind, syscall(SYS_getcwd, b[i].p, 4096));
        if (i == 2) memset((void *)b[i].p, 'a', 2);
        WANT_EFAULT("pipe2", b[i].kind, syscall(SYS_pipe2, b[i].p, 0));
        /* pipe2 is all or nothing (put_user): no fd bytes in the head. */
        if (i == 2 && memcmp((void *)b[i].p, "aa", 2)) {
            printf("  pipe2/straddle: wrote 0x%02x 0x%02x into the mapped head\n",
                   ((unsigned char *)b[i].p)[0], ((unsigned char *)b[i].p)[1]);
            fflush(stdout); ua_bad++;
        }
        /* The output cases above may legally store their first bytes into
         * the mapped head of the straddle before faulting (Linux's
         * copy_to_user does too: getcwd leaves "/t"). The string cases below
         * need it NUL-free again, or they read a valid 1-byte name. */
#define UA_REFILL() do { if (i == 2) memset((void *)b[i].p, 'a', 2); } while (0)
        UA_REFILL();
        WANT_EFAULT("memfd_create", b[i].kind, syscall(SYS_memfd_create, b[i].p, 0));
        char v[64];
        UA_REFILL();
        WANT_EFAULT("getxattr name", b[i].kind, syscall(SYS_getxattr, "/tmp", b[i].p, v, sizeof v));
        UA_REFILL();
        if (geteuid() == 0)
            WANT_EFAULT("mount target", b[i].kind, syscall(SYS_mount, "/dev/none", b[i].p, "f2fs", 0, 0));
#undef UA_REFILL
        int pp[2];
        if (pipe(pp) == 0) {
            WANT_EFAULT("FIONREAD pipe", b[i].kind, ioctl(pp[0], FIONREAD, b[i].p));
            WANT_EFAULT("FIONBIO pipe", b[i].kind, ioctl(pp[0], FIONBIO, b[i].p));
            close(pp[0]); close(pp[1]);
        }
    }
    return ua_bad;
}

static int ua_pty(void *a) {
    int m = *(int *)a; struct badp b[NBAD];
    if (make_bad(2, b)) return 99;
    for (int i = 0; i < NBAD; i++) {
        WANT_EFAULT("TCGETS", b[i].kind, ioctl(m, TCGETS, b[i].p));
        WANT_EFAULT("TCSETS", b[i].kind, ioctl(m, TCSETS, b[i].p));
        WANT_EFAULT("TIOCGWINSZ", b[i].kind, ioctl(m, TIOCGWINSZ, b[i].p));
        WANT_EFAULT("TIOCSWINSZ", b[i].kind, ioctl(m, TIOCSWINSZ, b[i].p));
        WANT_EFAULT("TIOCGPTN", b[i].kind, ioctl(m, TIOCGPTN, b[i].p));
        WANT_EFAULT("TIOCSPTLCK", b[i].kind, ioctl(m, TIOCSPTLCK, b[i].p));
        WANT_EFAULT("FIONREAD pty", b[i].kind, ioctl(m, FIONREAD, b[i].p));
    }
    return ua_bad;
}

/* After the bad pointers: another process still gets the pty pool, the fd
 * table, directories and its signal state — a lock leaked by a killed caller
 * would hang this child, and the watchdog would report it. */
static int ua_survivor(void *a) {
    int m = *(int *)a;
    int s = open(ptsname(m), O_RDWR | O_NOCTTY);
    if (s < 0) return 1;
    struct termios t;
    if (tcgetattr(s, &t)) return 2;
    cfmakeraw(&t);
    if (tcsetattr(s, TCSANOW, &t)) return 3;
    struct winsize ws = { 30, 100, 0, 0 }, ws2;
    if (ioctl(m, TIOCSWINSZ, &ws) || ioctl(s, TIOCGWINSZ, &ws2) || ws2.ws_col != 100) return 4;
    if (write(m, "hi", 2) != 2) return 5;
    char c[2];
    if (read(s, c, 2) != 2 || c[0] != 'h') return 6;
    int pp[2];
    if (pipe(pp) || write(pp[1], "x", 1) != 1) return 7;
    int n = -1;
    if (ioctl(pp[0], FIONREAD, &n) || n != 1) return 8;
    char buf[4096];
    int d = open("/tmp", O_RDONLY | O_DIRECTORY);
    if (d < 0 || syscall(SYS_getdents64, d, buf, sizeof buf) <= 0) return 9;
    if (!getcwd(buf, sizeof buf)) return 10;
    struct sigaction sa = { 0 }, old;
    sa.sa_handler = SIG_IGN;
    if (sigaction(SIGUSR2, &sa, &old)) return 11;
    int mf = memfd_create("survivor", 0);
    if (mf < 0) return 12;
    close(mf); close(d); close(pp[0]); close(pp[1]); close(s);
    return 0;
}

static void uaccess_cases(void) {
    struct { const char *name; int (*fn)(void *); } cs[] = {
        { "uaccess_signal", ua_sigaction },
        { "uaccess_fs", ua_fs },
        { "uaccess_pty", ua_pty },
    };
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0 || grantpt(m) || unlockpt(m)) { if (m >= 0) close(m); m = -1; }
    for (unsigned i = 0; i < sizeof cs / sizeof cs[0]; i++) {
        if (cs[i].fn == ua_pty && m < 0) { skip(cs[i].name, "no pty"); continue; }
        int st = in_child(cs[i].fn, &m, 10000);
        if (st == -1) fail(cs[i].name, "timed out (a syscall hung)");
        else if (WIFSIGNALED(st)) fail(cs[i].name, "killed by signal %d", WTERMSIG(st));
        else if (WEXITSTATUS(st)) fail(cs[i].name, "%d case(s) not EFAULT", WEXITSTATUS(st));
        else pass(cs[i].name);
    }
    if (m < 0) { skip("uaccess_survivor", "no pty"); return; }
    unlockpt(m); /* a TIOCSPTLCK case may have locked it */
    int st = in_child(ua_survivor, &m, 10000);
    if (st == -1) fail("uaccess_survivor", "timed out (a lock was left held)");
    else if (!WIFEXITED(st) || WEXITSTATUS(st)) fail("uaccess_survivor", "status %#x", st);
    else pass("uaccess_survivor");
    close(m);
}

/* ── VFS ABI (lane vfsabi) ───────────────────────────────────────────────── */

#include <dirent.h>
#include <sys/statfs.h>
#include <sys/mount.h>
#include <limits.h>

/* close(1) on a terminal stdout: from then on fd 1 is closed — write, fstat
 * and a second close are EBADF — and the next open() takes the number. The
 * child reports through a pipe opened before the close. */
static int stdio_close_child(void *arg) {
    int rep = *(int *)arg;
    int code = 0;
    if (close(1) != 0) code |= 1;
    errno = 0;
    if (!(write(1, "x", 1) < 0 && errno == EBADF)) code |= 2;
    struct stat st;
    errno = 0;
    if (!(fstat(1, &st) < 0 && errno == EBADF)) code |= 4;
    errno = 0;
    if (!(close(1) < 0 && errno == EBADF)) code |= 8;
    errno = 0;
    if (!(dup(1) < 0 && errno == EBADF)) code |= 16;
    errno = 0;
    if (!(fcntl(1, F_GETFL) < 0 && errno == EBADF)) code |= 32;
    int fd = open("/dev/null", O_WRONLY);
    if (fd != 1) code |= 64;            /* lowest free number */
    if (write(1, "x", 1) != 1) code |= 128; /* now /dev/null */
    /* A forked child inherits the closed state. */
    close(1);
    pid_t p = fork();
    if (p == 0) {
        errno = 0;
        _exit(write(1, "x", 1) < 0 && errno == EBADF ? 0 : 1);
    }
    int st2 = 0;
    waitpid(p, &st2, 0);
    if (!WIFEXITED(st2) || WEXITSTATUS(st2) != 0) code |= 256;
    (void)!write(rep, &code, sizeof code);
    return 0;
}

static void stdio_close_ebadf(void) {
    const char *n = "stdio_close_ebadf";
    if (!isatty(1)) { skip(n, "stdout is not a terminal"); return; }
    int pp[2];
    if (pipe(pp) != 0) { fail(n, "pipe: %s", strerror(errno)); return; }
    int st = in_child(stdio_close_child, &pp[1], 5000);
    close(pp[1]);
    int code = -1;
    if (read(pp[0], &code, sizeof code) != (ssize_t)sizeof code) code = -1;
    close(pp[0]);
    if (st == -1) fail(n, "child hung");
    else if (code != 0) fail(n, "code %#x (1 close 2 write 4 fstat 8 reclose 16 dup 32 fcntl 64 reuse 128 null 256 fork)", code);
    else pass(n);
}

/* Names returned by one getdents64 call, joined with '/'. */
static long dents_names(int fd, char *out, size_t outlen, size_t bufsz) {
    char buf[4096];
    if (bufsz > sizeof buf) bufsz = sizeof buf;
    long n = syscall(SYS_getdents64, fd, buf, bufsz);
    if (n <= 0) return n;
    for (long off = 0; off < n;) {
        unsigned short reclen = *(unsigned short *)(buf + off + 16);
        size_t l = strlen(out);
        snprintf(out + l, outlen - l, "%s/", buf + off + 19);
        off += reclen;
    }
    return n;
}

static int count_names(const char *s) { int c = 0; for (; *s; s++) if (*s == '/') c++; return c; }

/* The getdents cursor belongs to the open file description: a dup continues
 * where the original stopped, a fork's child moves the parent's, and a
 * rewind through one rewinds both. */
static void getdents_shared(const char *dir) {
    char name[96], d[256], p[512];
    snprintf(name, sizeof name, "getdents_shared(%s)", dir);
    int made = 0;
    if (strcmp(dir, "/dev")) {
        snprintf(d, sizeof d, "%s/.syscalltest-gds-%d", dir, (int)getpid());
        if (mkdir(d, 0700) != 0) { fail(name, "mkdir: %s", strerror(errno)); return; }
        for (int i = 0; i < 12; i++) {
            snprintf(p, sizeof p, "%s/e%02d", d, i);
            int fd = open(p, O_CREAT | O_WRONLY, 0600);
            if (fd >= 0) close(fd);
        }
        made = 1;
    } else snprintf(d, sizeof d, "%s", dir);
    int fd = open(d, O_RDONLY | O_DIRECTORY);
    static char all[16384], a[16384], b[16384];
    all[0] = a[0] = b[0] = 0;
    if (fd < 0) { fail(name, "open: %s", strerror(errno)); goto out; }
    while (dents_names(fd, all, sizeof all, 4096) > 0) {}
    int total = count_names(all);
    lseek(fd, 0, SEEK_SET);
    if (dents_names(fd, a, sizeof a, 72) <= 0) { fail(name, "first getdents empty"); close(fd); goto out; }
    int fd2 = dup(fd);
    while (dents_names(fd2, b, sizeof b, 4096) > 0) {}
    int na = count_names(a), nb = count_names(b);
    if (na + nb != total) {
        fail(name, "dup restarted the listing: %d + %d entries of %d", na, nb, total);
        close(fd); close(fd2); goto out;
    }
    /* Rewind through the dup, read through the original. */
    lseek(fd2, 0, SEEK_SET);
    char c[16384]; c[0] = 0;
    if (dents_names(fd, c, sizeof c, 4096) <= 0 || strncmp(c, all, strlen(c) < 3 ? strlen(c) : 3) != 0) {
        fail(name, "rewind through the dup did not rewind the original");
        close(fd); close(fd2); goto out;
    }
    /* Fork: the child drains the directory; the parent then reads nothing. */
    lseek(fd, 0, SEEK_SET);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        char x[16384]; x[0] = 0;
        while (dents_names(fd, x, sizeof x, 4096) > 0) {}
        _exit(0);
    }
    int st;
    waitpid(pid, &st, 0);
    char rest[16384]; rest[0] = 0;
    long r = dents_names(fd, rest, sizeof rest, 4096);
    close(fd); close(fd2);
    if (r != 0) fail(name, "after the child drained it the parent read %ld bytes (%s)", r, rest);
    else pass(name);
out:
    if (made) {
        for (int i = 0; i < 12; i++) { snprintf(p, sizeof p, "%s/e%02d", d, i); unlink(p); }
        rmdir(d);
    }
}

/* O_NONBLOCK is a status flag of the open file description; F_SETFL changes
 * only the status flags, never the access mode. */
static void nonblock_shared(const char *dir) {
    const char *n = "nonblock_shared";
    int pp[2];
    if (pipe(pp) != 0) { fail(n, "pipe: %s", strerror(errno)); return; }
    int r2 = dup(pp[0]);
    if (fcntl(r2, F_SETFL, fcntl(r2, F_GETFL) | O_NONBLOCK) != 0) { fail(n, "F_SETFL: %s", strerror(errno)); goto out; }
    if (!(fcntl(pp[0], F_GETFL) & O_NONBLOCK)) { fail(n, "dup's O_NONBLOCK not seen on the original"); goto out; }
    char c;
    alarm(5);
    errno = 0;
    ssize_t r = read(pp[0], &c, 1);
    alarm(0);
    if (!(r < 0 && errno == EAGAIN)) { fail(n, "read on the original r=%zd errno=%s (want EAGAIN)", r, strerror(errno)); goto out; }
    /* A forked child clears it; the parent sees that. */
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) _exit(fcntl(pp[0], F_SETFL, 0) == 0 ? 0 : 1);
    int st;
    waitpid(pid, &st, 0);
    if (fcntl(r2, F_GETFL) & O_NONBLOCK) { fail(n, "child's F_SETFL not seen by the parent"); goto out; }
    /* F_SETFL keeps the access mode. */
    char path[256];
    int fd = make_file(dir, "setfl", 0, path, sizeof path);
    if (fd < 0) { fail(n, "file: %s", strerror(errno)); goto out; }
    close(fd);
    fd = open(path, O_WRONLY);
    fcntl(fd, F_SETFL, O_NONBLOCK);
    int fl = fcntl(fd, F_GETFL);
    ssize_t w = write(fd, "abc", 3);
    close(fd); unlink(path);
    if ((fl & O_ACCMODE) != O_WRONLY || w != 3) { fail(n, "F_SETFL(O_NONBLOCK) on O_WRONLY: flags %#x write %zd", fl, w); goto out; }
    pass(n);
out:
    close(pp[0]); close(pp[1]); close(r2);
}

/* /dev/input is a directory listing the event nodes. */
static void dev_input_dir(void) {
    const char *n = "dev_input_dir";
    struct stat st;
    if (stat("/dev/input/event0", &st) != 0) { skip(n, "no /dev/input/event0"); return; }
    DIR *d = opendir("/dev/input");
    if (!d) { fail(n, "opendir: %s", strerror(errno)); return; }
    int seen = 0, chr = 1;
    struct dirent *e;
    while ((e = readdir(d))) {
        if (!strcmp(e->d_name, "event0")) { seen = 1; if (e->d_type != DT_CHR && e->d_type != DT_UNKNOWN) chr = 0; }
    }
    closedir(d);
    if (stat("/dev/input", &st) != 0 || !S_ISDIR(st.st_mode)) fail(n, "stat: not a directory");
    else if (!seen) fail(n, "event0 not listed");
    else if (!chr) fail(n, "event0 d_type is not DT_CHR");
    else pass(n);
}

/* /etc is the root filesystem's: a file the static built-ins also have lives
 * on the same device as /etc/passwd, and the listing shows the disk's files. */
static void etc_on_root_fs(void) {
    const char *n = "etc_on_root_fs";
    struct stat a, b, c;
    if (stat("/etc/passwd", &a) != 0) { skip(n, "no /etc/passwd"); return; }
    if (stat("/etc/hostname", &b) != 0) { skip(n, "no /etc/hostname"); return; }
    if (stat("/etc", &c) != 0) { fail(n, "stat /etc: %s", strerror(errno)); return; }
    if (a.st_dev != b.st_dev || a.st_dev != c.st_dev) {
        fail(n, "st_dev passwd %#lx hostname %#lx /etc %#lx", (long)a.st_dev, (long)b.st_dev, (long)c.st_dev);
        return;
    }
    DIR *d = opendir("/etc");
    if (!d) { fail(n, "opendir: %s", strerror(errno)); return; }
    int pw = 0, hn = 0;
    struct dirent *e;
    while ((e = readdir(d))) {
        if (!strcmp(e->d_name, "passwd")) pw = 1;
        if (!strcmp(e->d_name, "hostname")) hn = 1;
    }
    closedir(d);
    if (!pw || !hn) { fail(n, "listing: passwd %d hostname %d", pw, hn); return; }
    pass(n);
}

/* Long paths and symlink bodies on tmpfs, as Linux allows them. */
static void tmpfs_long_paths(void) {
    const char *n = "tmpfs_long_paths";
    char base[64], p[PATH_MAX], comp[256];
    snprintf(base, sizeof base, "/tmp/.syscalltest-lp-%d", (int)getpid());
    memset(comp, 'c', 200); comp[200] = 0;
    if (mkdir(base, 0700) != 0) { fail(n, "mkdir: %s", strerror(errno)); return; }
    snprintf(p, sizeof p, "%s", base);
    int depth = 0, ok = 1;
    for (; depth < 8; depth++) {   /* > 1600 bytes */
        size_t l = strlen(p);
        snprintf(p + l, sizeof p - l, "/%s%d", comp, depth);
        if (mkdir(p, 0700) != 0) { fail(n, "mkdir depth %d (len %zu): %s", depth, strlen(p), strerror(errno)); ok = 0; break; }
    }
    char f[PATH_MAX];
    if (ok) {
        snprintf(f, sizeof f, "%s/file", p);
        int fd = open(f, O_CREAT | O_RDWR, 0600);
        if (fd < 0 || write(fd, "hi", 2) != 2) { fail(n, "create %zu-byte path: %s", strlen(f), strerror(errno)); ok = 0; }
        if (fd >= 0) close(fd);
    }
    /* A 255-byte component works, 256 is ENAMETOOLONG. */
    if (ok) {
        char nm[300];
        memset(nm, 'n', 255); nm[255] = 0;
        snprintf(f, sizeof f, "%s/%s", base, nm);
        int fd = open(f, O_CREAT | O_RDWR, 0600);
        if (fd < 0) { fail(n, "255-byte name: %s", strerror(errno)); ok = 0; } else { close(fd); unlink(f); }
        memset(nm, 'n', 256); nm[256] = 0;
        snprintf(f, sizeof f, "%s/%s", base, nm);
        errno = 0;
        fd = open(f, O_CREAT | O_RDWR, 0600);
        if (ok && !(fd < 0 && errno == ENAMETOOLONG)) { fail(n, "256-byte name: fd %d errno %s", fd, strerror(errno)); ok = 0; }
        if (fd >= 0) { close(fd); unlink(f); }
    }
    /* A 3000-byte symlink body round-trips. */
    static char target[3001], got[4096];
    if (ok) {
        for (int i = 0; i < 3000; i++) target[i] = (i % 50 == 49) ? '/' : 'a' + i % 26;
        target[3000] = 0;
        snprintf(f, sizeof f, "%s/longlink", base);
        if (symlink(target, f) != 0) { fail(n, "symlink 3000: %s", strerror(errno)); ok = 0; }
        else {
            ssize_t r = readlink(f, got, sizeof got);
            if (r != 3000 || memcmp(got, target, 3000)) { fail(n, "readlink: %zd", r); ok = 0; }
            unlink(f);
        }
    }
    struct statfs sf;
    if (ok && (statfs("/tmp", &sf) != 0 || sf.f_namelen != 255)) { fail(n, "statfs /tmp f_namelen %ld", (long)sf.f_namelen); ok = 0; }
    /* Rename the whole deep tree and find the file under the new name. */
    char nb[64];
    snprintf(nb, sizeof nb, "%s-mv", base);
    if (ok && rename(base, nb) != 0) { fail(n, "rename tree: %s", strerror(errno)); ok = 0; }
    if (ok) {
        char q[PATH_MAX];
        snprintf(q, sizeof q, "%s%s/file", nb, p + strlen(base));
        struct stat st;
        if (stat(q, &st) != 0 || st.st_size != 2) { fail(n, "file after tree rename: %s", strerror(errno)); ok = 0; }
        snprintf(base, sizeof base, "%s", nb);
        snprintf(f, sizeof f, "%s", q);
        memmove(p, q, strlen(q) - 5); p[strlen(q) - 5] = 0;
    }
    /* Clean up from the bottom (a non-empty rmdir must fail). */
    errno = 0;
    if (ok && !(rmdir(base) < 0 && errno == ENOTEMPTY)) { fail(n, "rmdir of a non-empty dir: %s", strerror(errno)); ok = 0; }
    snprintf(f, sizeof f, "%s/file", p);
    unlink(f);
    while (strlen(p) > strlen(base)) {
        if (rmdir(p) != 0 && ok) { fail(n, "rmdir %zu: %s", strlen(p), strerror(errno)); ok = 0; }
        *strrchr(p, '/') = 0;
    }
    if (rmdir(base) != 0 && ok) { fail(n, "rmdir base: %s", strerror(errno)); ok = 0; }
    if (ok) pass(n);
}

/* The thread name of a program exec'd from a path longer than 256 bytes is
 * its basename. */
static int comm_check(void) {
    char c[64] = {0};
    int fd = open("/proc/self/comm", O_RDONLY);
    if (fd < 0) return 2;
    ssize_t r = read(fd, c, sizeof c - 1);
    close(fd);
    if (r <= 0) return 3;
    return strcmp(c, "deep-comm\n") ? 4 : 0;
}

static void comm_deep_path(const char *self) {
    const char *n = "comm_deep_path";
    char d[PATH_MAX], comp[160];
    memset(comp, 'd', 150); comp[150] = 0;
    snprintf(d, sizeof d, "/tmp/.syscalltest-cm-%d", (int)getpid());
    mkdir(d, 0700);
    size_t l = strlen(d);
    snprintf(d + l, sizeof d - l, "/%s", comp); mkdir(d, 0700);
    l = strlen(d);
    snprintf(d + l, sizeof d - l, "/%s", comp); mkdir(d, 0700);
    char exe[PATH_MAX];
    snprintf(exe, sizeof exe, "%s/deep-comm", d);
    int in = open(self, O_RDONLY), out = open(exe, O_CREAT | O_WRONLY | O_TRUNC, 0700);
    int ok = in >= 0 && out >= 0;
    char buf[65536];
    ssize_t r;
    while (ok && (r = read(in, buf, sizeof buf)) > 0) if (write(out, buf, (size_t)r) != r) ok = 0;
    if (in >= 0) close(in);
    if (out >= 0) close(out);
    int st = -1;
    if (ok) {
        fflush(stdout);
        pid_t p = fork();
        if (p == 0) { execl(exe, "deep-comm", "--comm-check", (char *)NULL); _exit(9); }
        waitpid(p, &st, 0);
    }
    unlink(exe);
    *strrchr(d, '/') = 0; *strrchr(d, '/') = 0;
    char q[PATH_MAX];
    snprintf(q, sizeof q, "%s/%s/%s", d, comp, comp); rmdir(q);
    snprintf(q, sizeof q, "%s/%s", d, comp); rmdir(q);
    rmdir(d);
    if (!ok) fail(n, "copy of %s failed", self);
    else if (!WIFEXITED(st) || WEXITSTATUS(st)) fail(n, "comm check status %#x (4: wrong name)", st);
    else pass(n);
}

/* A pty slave belongs to whoever opened /dev/ptmx (group tty, mode 0620);
 * chown/chmod on it stick, and opening it by path obeys them. */
static void pts_owner(void) {
    const char *n = "pts_owner";
    if (geteuid() != 0) { skip(n, "needs root"); return; }
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0) { skip(n, "no ptmx"); return; }
    grantpt(m); unlockpt(m);
    char *sn = ptsname(m);
    struct stat st;
    if (!sn || stat(sn, &st) != 0) { fail(n, "stat slave: %s", strerror(errno)); close(m); return; }
    /* devpts' mode= mount option: 620 on most distributions, 600 on Arch. */
    if (st.st_uid != 0 || st.st_gid != 5 || ((st.st_mode & 07777) != 0620 && (st.st_mode & 07777) != 0600)) {
        fail(n, "new slave uid %d gid %d mode %o (want 0, 5, 0620|0600)", (int)st.st_uid, (int)st.st_gid,
             st.st_mode & 07777); close(m); return;
    }
    gid_t ttygid = st.st_gid;
    if (chown(sn, 1000, ttygid) != 0 || chmod(sn, 0600) != 0) { fail(n, "chown/chmod: %s", strerror(errno)); close(m); return; }
    int s = open(sn, O_RDWR | O_NOCTTY);
    struct stat fs;
    if (s < 0 || fstat(s, &fs) != 0 || stat(sn, &st) != 0) { fail(n, "open/stat: %s", strerror(errno)); close(m); return; }
    if (st.st_uid != 1000 || (st.st_mode & 07777) != 0600 || fs.st_uid != 1000 || (fs.st_mode & 07777) != 0600) {
        fail(n, "after chown: stat uid %d mode %o, fstat uid %d mode %o",
             (int)st.st_uid, st.st_mode & 07777, (int)fs.st_uid, fs.st_mode & 07777);
        close(s); close(m); return;
    }
    /* uid 1000 may open it now; uid 1001 may not. */
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        if (setgid(1000) || setuid(1000)) _exit(5);
        int f = open(sn, O_RDWR | O_NOCTTY);
        _exit(f >= 0 ? 0 : 1);
    }
    int s1; waitpid(p, &s1, 0);
    p = fork();
    if (p == 0) {
        if (setgid(1001) || setuid(1001)) _exit(5);
        errno = 0;
        int f = open(sn, O_RDWR | O_NOCTTY);
        _exit(f < 0 && errno == EACCES ? 0 : 1);
    }
    int s2; waitpid(p, &s2, 0);
    /* fchown through the slave fd. */
    int fr = fchown(s, 0, ttygid);
    stat(sn, &st);
    close(s); close(m);
    if (!WIFEXITED(s1) || WEXITSTATUS(s1)) fail(n, "owner (uid 1000) could not open its pty: %#x", s1);
    else if (!WIFEXITED(s2) || WEXITSTATUS(s2)) fail(n, "uid 1001 opened a 0600 pty of uid 1000: %#x", s2);
    else if (fr != 0 || st.st_uid != 0) fail(n, "fchown on the slave fd: %d uid %d", fr, (int)st.st_uid);
    else pass(n);
}

/* An unprivileged opener owns the pty it opens: devpts gives a new slave the
 * opener's euid, group tty (5), mode 0620 (lane runtmpfs). */
static void pts_owner_user(void) {
    const char *n = "pts_owner_user";
    if (geteuid() != 0) { skip(n, "needs root"); return; }
    int pf[2];
    if (pipe(pf) != 0) { fail(n, "pipe"); return; }
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        close(pf[0]);
        if (setgid(1000) || setuid(1000)) _exit(5);
        int m = posix_openpt(O_RDWR | O_NOCTTY);
        if (m < 0) _exit(6);
        grantpt(m); unlockpt(m);
        char *sn = ptsname(m);
        struct stat st;
        if (!sn || stat(sn, &st) != 0) _exit(7);
        int bad = st.st_uid != 1000 || st.st_gid != 5 ||
                  ((st.st_mode & 07777) != 0620 && (st.st_mode & 07777) != 0600);
        /* The owner may chmod its own pty; chown to another uid is refused. */
        if (chmod(sn, 0600) != 0) bad |= 2;
        if (chown(sn, 0, (gid_t)-1) == 0) bad |= 4;
        char b = (char)bad;
        if (write(pf[1], &b, 1) != 1) _exit(8);
        _exit(0);
    }
    close(pf[1]);
    char b = 99;
    ssize_t r = read(pf[0], &b, 1);
    close(pf[0]);
    int s; waitpid(p, &s, 0);
    if (r != 1 || !WIFEXITED(s) || WEXITSTATUS(s)) fail(n, "child failed: read %zd status %#x", r, s);
    else if (b) fail(n, "unprivileged opener's pty wrong: flags %d", (int)b);
    else pass(n);
}

/* /run is a tmpfs like Linux/Alpine: it holds unix sockets, is mode 0755, and
 * /var/run is a symlink to it (lane runtmpfs). Emptiness at boot follows from
 * tmpfs; the mount type is what a persistent f2fs /run would fail. */
static void run_tmpfs(void) {
    const char *n = "run_tmpfs";
    FILE *f = fopen("/proc/mounts", "r");
    char dev[256], dir[512], type[64], o[256];
    int found = 0;
    if (f) {
        while (fscanf(f, "%255s %511s %63s %255s %*d %*d", dev, dir, type, o) == 4)
            if (!strcmp(dir, "/run") && !strcmp(type, "tmpfs")) found = 1;
        fclose(f);
    }
    if (!found) { fail(n, "/run is not a tmpfs mount in /proc/mounts"); return; }
    struct stat st;
    if (stat("/run", &st) != 0 || !S_ISDIR(st.st_mode) || (st.st_mode & 07777) != 0755 || st.st_uid != 0) {
        fail(n, "/run mode %o uid %d (want dir 0755 root)", st.st_mode & 07777, (int)st.st_uid); return;
    }
    char lk[64];
    ssize_t l = readlink("/var/run", lk, sizeof lk - 1);
    if (l < 0) { fail(n, "/var/run is not a symlink: %s", strerror(errno)); return; }
    lk[l] = 0;
    if (strcmp(lk, "/run") && strcmp(lk, "../run")) { fail(n, "/var/run -> %s", lk); return; }
    struct sockaddr_un a = { .sun_family = AF_UNIX };
    snprintf(a.sun_path, sizeof a.sun_path, "/run/runtmpfs-%d.sock", (int)getpid());
    unlink(a.sun_path);
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (s < 0 || bind(s, (struct sockaddr *)&a, sizeof a) != 0 || listen(s, 1) != 0) {
        fail(n, "bind/listen in /run: %s", strerror(errno)); if (s >= 0) close(s); unlink(a.sun_path); return;
    }
    /* Reachable through /var/run too, and it is a socket there. */
    char via[128];
    snprintf(via, sizeof via, "/var/run/runtmpfs-%d.sock", (int)getpid());
    int ok = stat(via, &st) == 0 && S_ISSOCK(st.st_mode);
    close(s); unlink(a.sun_path);
    if (!ok) { fail(n, "socket not visible/typed through /var/run: %s", strerror(errno)); return; }
    /* Created through /var/run, it must land in the tmpfs /run (not the
     * persistent on-disk /run the tmpfs shadows). */
    char dn[96], dv[96];
    snprintf(dn, sizeof dn, "/run/runtmpfs-d%d", (int)getpid());
    snprintf(dv, sizeof dv, "/var/run/runtmpfs-d%d", (int)getpid());
    if (mkdir(dv, 0700) != 0) { fail(n, "mkdir via /var/run: %s", strerror(errno)); return; }
    ok = stat(dn, &st) == 0 && S_ISDIR(st.st_mode);
    rmdir(dn);
    if (!ok) fail(n, "mkdir /var/run/x did not appear in /run");
    else pass(n);
}

/* `mount -o remount,ro` shows as `ro` in /proc/mounts and ST_RDONLY in statfs. */
static int mounts_flag(const char *mp, char *opts, size_t n) {
    FILE *f = fopen("/proc/mounts", "r");
    if (!f) return -1;
    char dev[256], dir[512], type[64], o[256];
    int found = -1;
    while (fscanf(f, "%255s %511s %63s %255s %*d %*d", dev, dir, type, o) == 4) {
        if (!strcmp(dir, mp)) { snprintf(opts, n, "%s", o); found = !strcmp(type, "f2fs"); }
    }
    fclose(f);
    return found;
}

static void mount_ro_flag(void) {
    const char *n = "mount_ro_flag";
    char o[256];
    if (geteuid() != 0) { skip(n, "needs root"); return; }
    if (mounts_flag("/home", o, sizeof o) != 1) { skip(n, "/home is not an f2fs mount"); return; }
    if (strncmp(o, "rw", 2)) { fail(n, "/home starts as %s", o); return; }
    if (mount(NULL, "/home", NULL, MS_REMOUNT | MS_RDONLY, NULL) != 0) { fail(n, "remount ro: %s", strerror(errno)); return; }
    struct statfs sf;
    int sr = statfs("/home", &sf);
    mounts_flag("/home", o, sizeof o);
    int ro_listed = !strncmp(o, "ro", 2);
    char ob[256]; snprintf(ob, sizeof ob, "%s", o);
    errno = 0;
    int fd = open("/home/.syscalltest-ro", O_CREAT | O_WRONLY, 0600);
    int erofs = fd < 0 && errno == EROFS;
    if (fd >= 0) { close(fd); unlink("/home/.syscalltest-ro"); }
    if (mount(NULL, "/home", NULL, MS_REMOUNT, NULL) != 0) { fail(n, "remount rw: %s", strerror(errno)); return; }
    mounts_flag("/home", o, sizeof o);
    if (!ro_listed) fail(n, "/proc/mounts shows %s after remount,ro", ob);
    else if (sr != 0 || !(sf.f_flags & ST_RDONLY)) fail(n, "statfs f_flags %#lx", (long)sf.f_flags);
    else if (!erofs) fail(n, "create on a read-only mount was not EROFS");
    else if (strncmp(o, "rw", 2)) fail(n, "after remount,rw: %s", o);
    else pass(n);
}

/* Files staged in the image carry real timestamps, not the epoch. */
/* access(2) and execve(2) follow a final /proc/self/exe magic link, as open
 * and stat do (both answered ENOENT, so `env /proc/self/exe` failed). */
static void proc_self_exe_access(void) {
    const char *n = "proc_self_exe_access";
    if (access("/proc/self/exe", X_OK) != 0) { fail(n, "access X_OK: %s", strerror(errno)); return; }
    pid_t p = fork();
    if (p == 0) {
        char *argv[] = { "syscalltest", "--comm-check", NULL };
        execv("/proc/self/exe", argv);
        _exit(100 + errno);
    }
    int st = 0;
    waitpid(p, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) >= 100) fail(n, "execv status %#x", st);
    else pass(n);
}

static void image_timestamps(void) {
    const char *n = "image_timestamps";
    const char *paths[] = { "/bin/sh", "/etc/passwd", "/bin", "/etc", NULL };
    for (int i = 0; paths[i]; i++) {
        struct stat st;
        if (stat(paths[i], &st) != 0) continue;
        if (st.st_mtime < 1577836800 /* 2020-01-01 */) {
            fail(n, "%s mtime %ld", paths[i], (long)st.st_mtime); return;
        }
    }
    pass(n);
}

#ifndef SYS_faccessat2
#define SYS_faccessat2 439
#endif
/* Legacy faccessat has no flags argument: garbage in a3 is ignored.
 * faccessat2 rejects unknown flags with EINVAL. */
static void faccessat_legacy_a3(void) {
    const char *n = "faccessat_legacy_a3";
    /* 0x1 is not a valid AT_* flag for faccessat2. 0x100 would be NOFOLLOW. */
    long r = syscall(SYS_faccessat, AT_FDCWD, "/proc/self/exe", X_OK, 0x1UL | 0x100UL);
    if (r != 0) { fail(n, "faccessat(/proc/self/exe, X_OK, a3=0x101) = %ld errno %s (a3 must be ignored)", r, strerror(errno)); return; }
    r = syscall(SYS_faccessat2, AT_FDCWD, "/", F_OK, 0x1UL);
    if (r != -1 || errno != EINVAL) { fail(n, "faccessat2(flags=0x1) = %ld errno %s (want EINVAL)", r, strerror(errno)); return; }
    r = syscall(SYS_faccessat2, AT_FDCWD, "/", F_OK, 0x200UL);   /* AT_EACCESS ok */
    if (r != 0) { fail(n, "faccessat2(AT_EACCESS) = %ld errno %s", r, strerror(errno)); return; }
    pass(n);
}

/* L10.8: Linux-visible process facts: AT_HWCAP, uname release, the stored
 * per-thread affinity mask (round trip + fork inheritance), getsid on a
 * zombie. Raw syscalls for affinity: relibc has no wrappers. */
#include <sys/auxv.h>
#include <sys/utsname.h>
#ifndef AT_HWCAP
#define AT_HWCAP 16
#endif
static void linux_compat_facts(void) {
    unsigned long hw = getauxval(AT_HWCAP);
    if (hw == 0) fail("auxv_hwcap", "getauxval(AT_HWCAP)=0 errno=%d\n", errno);
    else { printf("  AT_HWCAP=%#lx\n", hw); pass("auxv_hwcap"); }

    struct utsname u;
    if (uname(&u) != 0) fail("uname_release", "uname errno=%d\n", errno);
    else if (strcmp(u.release, "6.0.0-leandros") != 0) fail("uname_release", "release=%s\n", u.release);
    else pass("uname_release");

    uint64_t all = 0, one = 1, got = 0;
    long r = syscall(SYS_sched_getaffinity, 0, sizeof all, &all);
    if (r <= 0 || all == 0) { fail("affinity_roundtrip", "getaffinity r=%ld errno=%d\n", r, errno); return; }
    if (syscall(SYS_sched_setaffinity, 0, sizeof one, &one) != 0) {
        fail("affinity_roundtrip", "setaffinity errno=%d\n", errno);
    } else if (syscall(SYS_sched_getaffinity, 0, sizeof got, &got) <= 0 || got != 1) {
        fail("affinity_roundtrip", "got=%#llx want 1 (all=%#llx)\n", (unsigned long long)got, (unsigned long long)all);
    } else {
        pass("affinity_roundtrip");
        pid_t c = fork();
        if (c == 0) {
            uint64_t m = 0;
            _exit(syscall(SYS_sched_getaffinity, 0, sizeof m, &m) > 0 && m == 1 ? 0 : 1);
        }
        int st = 0;
        if (c < 0 || waitpid(c, &st, 0) != c || !WIFEXITED(st) || WEXITSTATUS(st) != 0)
            fail("affinity_inherit", "child status=%#x\n", st);
        else pass("affinity_inherit");
        uint64_t zero = 0;
        errno = 0;
        if (syscall(SYS_sched_setaffinity, 0, sizeof zero, &zero) != -1 || errno != EINVAL)
            fail("affinity_empty_einval", "errno=%d\n", errno);
        else pass("affinity_empty_einval");
    }
    syscall(SYS_sched_setaffinity, 0, sizeof all, &all);

    pid_t z = fork();
    if (z == 0) _exit(0);
    if (z < 0) { fail("getsid_zombie", "fork errno=%d\n", errno); return; }
    siginfo_t si;
    memset(&si, 0, sizeof si);
    /* Raw waitid: relibc's wrapper does not honor WNOWAIT. */
    long wr = syscall(SYS_waitid, P_PID, z, &si, WEXITED | WNOWAIT, NULL);
    if (wr != 0) { printf("  waitid(WNOWAIT) r=%ld errno=%d\n", wr, errno); usleep(300000); }
    pid_t zs = getsid(z), me = getsid(0);
    if (zs != me) fail("getsid_zombie", "getsid(zombie)=%d errno=%d want %d\n", zs, errno, me);
    else pass("getsid_zombie");
    waitpid(z, NULL, 0);
}


/* L10.8 follow-up: placement honours sched_setaffinity. The scheduler must
 * run a pinned task only on its CPUs: sample getcpu() while busy. */
static int sample_cpus(int want, int n, int *bad_cpu) {
    int bad = 0;
    for (int i = 0; i < n; i++) {
        unsigned cpu = 99, node = 99;
        for (volatile int k = 0; k < 2000; k++) {}
        if (syscall(SYS_getcpu, &cpu, &node, NULL) != 0) return -1;
        if ((int)cpu != want || node != 0) { bad++; *bad_cpu = (int)cpu; }
    }
    return bad;
}

static int pin_self(int cpu) {
    uint64_t m = 1ULL << cpu;
    return (int)syscall(SYS_sched_setaffinity, 0, sizeof m, &m);
}


/* ── Kernel addresses handed to the kernel as user pointers ─────────────────
 * Every pointer a process passes in — directly, or nested inside an ioctl
 * struct — must be range-checked before the kernel copies through it.
 * `copy_raw` (the fault-tolerant copy) has no range check, so a site that
 * used it on a user-supplied address let a process name kernel memory: on a
 * mapped address the copy simply succeeded (a read or write of kernel
 * memory), on an unmapped one it failed with whatever errno the site chose.
 * The only right answer is EFAULT. These addresses are kernel on every
 * LeandrOS arch (and on Linux), mapped or not. */
static const unsigned long kaddrs[] = {
    0xffff800000000000UL, 0xffff000000001000UL, 0xffffffff80000000UL,
};
#define NKADDR (sizeof kaddrs / sizeof kaddrs[0])

struct ka_syncobj_array { uint64_t handles; uint32_t count_handles, pad; };
#define KA_DRM_IOCTL_SYNCOBJ_RESET   0xC01064C4UL
#define KA_DRM_IOCTL_SYNCOBJ_CREATE  0xC00864BFUL

static void kaddr_drm_nested(void) {
    const char *n = "kaddr_drm_syncobj_handles";
    int fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
    if (fd < 0) { skip(n, "no /dev/dri/card0"); return; }
    struct { uint32_t handle, flags; } c = { 0, 0 };
    if (ioctl(fd, KA_DRM_IOCTL_SYNCOBJ_CREATE, &c) < 0) {
        skip(n, "no syncobj support"); close(fd); return;
    }
    for (size_t i = 0; i < NKADDR; i++) {
        struct ka_syncobj_array a = { kaddrs[i], 1, 0 };
        errno = 0;
        int r = ioctl(fd, KA_DRM_IOCTL_SYNCOBJ_RESET, &a);
        if (!(r < 0 && errno == EFAULT)) {
            fail(n, "handles=%#lx r=%d errno=%s (want EFAULT)", kaddrs[i], r, strerror(errno));
            close(fd); return;
        }
    }
    close(fd);
    pass(n);
}

/* sendmsg/recvmsg: iov_base and msg_control are user pointers like any other.
 * A kernel iov_base on sendmsg used to send kernel memory to the peer; on
 * recvmsg the kernel wrote the payload there. recvmsg is only probed once
 * sendmsg rejects kernel addresses, so an unfixed kernel is never written. */
static void kaddr_sendmsg(void) {
    const char *n = "kaddr_sendmsg_iov";
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0) { fail(n, "socketpair errno=%s", strerror(errno)); return; }
    int ok = 1;
    for (size_t i = 0; i < NKADDR && ok; i++) {
        struct iovec v = { (void *)kaddrs[i], 64 };
        struct msghdr m = { 0 }; m.msg_iov = &v; m.msg_iovlen = 1;
        errno = 0;
        ssize_t r = syscall(SYS_sendmsg, sv[0], &m, MSG_DONTWAIT); /* raw: relibc gathers iovs in user space */
        if (!(r < 0 && errno == EFAULT)) {
            fail(n, "iov_base=%#lx r=%zd errno=%s (want EFAULT)", kaddrs[i], r, strerror(errno)); ok = 0;
        }
    }
    for (size_t i = 0; i < NKADDR && ok; i++) {
        char b = 'x'; struct iovec v = { &b, 1 };
        struct msghdr m = { 0 }; m.msg_iov = &v; m.msg_iovlen = 1;
        m.msg_control = (void *)kaddrs[i]; m.msg_controllen = 64;
        errno = 0;
        ssize_t r = syscall(SYS_sendmsg, sv[0], &m, MSG_DONTWAIT); /* raw: relibc gathers iovs in user space */
        if (!(r < 0 && errno == EFAULT)) {
            fail("kaddr_sendmsg_control", "msg_control=%#lx r=%zd errno=%s (want EFAULT)", kaddrs[i], r, strerror(errno)); ok = 0;
        }
    }
    if (ok) pass(n);
    if (!ok) { close(sv[0]); close(sv[1]); return; }
    const char *rn = "kaddr_recvmsg_iov";
    for (size_t i = 0; i < NKADDR; i++) {
        if (write(sv[0], "k", 1) != 1) { fail(rn, "write errno=%s", strerror(errno)); break; }
        struct iovec v = { (void *)kaddrs[i], 64 };
        struct msghdr m = { 0 }; m.msg_iov = &v; m.msg_iovlen = 1;
        errno = 0;
        ssize_t r = syscall(SYS_recvmsg, sv[1], &m, MSG_DONTWAIT);
        if (!(r < 0 && errno == EFAULT)) {
            fail(rn, "iov_base=%#lx r=%zd errno=%s (want EFAULT)", kaddrs[i], r, strerror(errno)); ok = 0; break;
        }
        char d; (void)read(sv[1], &d, 1); /* drain if the byte is still queued */
    }
    if (ok) pass(rn);
    close(sv[0]); close(sv[1]);
}

/*KADDR_MORE*/
static void kaddr_efault(void) {
    kaddr_drm_nested();
    kaddr_sendmsg();
}

static void affinity_placement(void) {
    uint64_t all = 0;
    if (syscall(SYS_sched_getaffinity, 0, sizeof all, &all) <= 0 || (all & 3) != 3) {
        printf("  affinity_placement: <2 CPUs, skipped\n");
        return;
    }
    int bc = -1, bad;
    if (pin_self(1) != 0) { fail("affinity_pin_cpu1", "setaffinity errno=%d\n", errno); return; }
    bad = sample_cpus(1, 1000, &bc);
    if (bad != 0) fail("affinity_pin_cpu1", "%d/1000 samples off CPU 1 (last %d)\n", bad, bc);
    else pass("affinity_pin_cpu1");
    if (pin_self(0) != 0) { fail("affinity_pin_cpu0", "setaffinity errno=%d\n", errno); return; }
    bad = sample_cpus(0, 1000, &bc);
    if (bad != 0) fail("affinity_pin_cpu0", "%d/1000 samples off CPU 0 (last %d)\n", bad, bc);
    else pass("affinity_pin_cpu0");
    syscall(SYS_sched_setaffinity, 0, sizeof all, &all);

    /* A blocked child pinned by the parent must wake on its CPU. */
    int pfd[2], rfd[2];
    if (pipe(pfd) || pipe(rfd)) { fail("affinity_pin_other", "pipe errno=%d\n", errno); return; }
    pid_t c = fork();
    if (c == 0) {
        char b; int cb = -1;
        close(pfd[1]); close(rfd[0]);
        if (read(pfd[0], &b, 1) != 1) _exit(2);
        _exit(sample_cpus(1, 1000, &cb) == 0 ? 0 : 1);
    }
    close(pfd[0]); close(rfd[1]);
    usleep(50000);
    uint64_t one = 2;
    long sr = syscall(SYS_sched_setaffinity, c, sizeof one, &one);
    write(pfd[1], "x", 1);
    int st = 0;
    if (c < 0 || sr != 0 || waitpid(c, &st, 0) != c || !WIFEXITED(st) || WEXITSTATUS(st) != 0)
        fail("affinity_pin_other", "set=%ld status=%#x\n", sr, st);
    else pass("affinity_pin_other");
    close(pfd[1]); close(rfd[0]);
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--rseq-exec-check")) return rseq_exec_check();
    if (argc > 1 && !strcmp(argv[1], "--exec-count-check")) return exec_count_check(argc, argv);
    if (argc > 1 && !strcmp(argv[1], "--comm-check")) return comm_check();
    if (argc > 1 && !strcmp(argv[1], "--pidfd-zombie-stat")) { pidfd_zombie_stat(); return nfail ? 1 : 0; }
    if (argc > 1 && !strcmp(argv[1], "--pidfd-zombie-contend")) { pidfd_zombie_contend(); return nfail ? 1 : 0; }
    if (argc > 1 && !strcmp(argv[1], "--pidfd-wake-loop")) {
        /* Flake hunter: --pidfd-wake-loop <runs> [cpu-hogs] */
        int runs = argc > 2 ? atoi(argv[2]) : 10, hogs = argc > 3 ? atoi(argv[3]) : 0;
        setvbuf(stdout, NULL, _IOLBF, 0);
        /* Kill only our own hogs: kill(0, ...) would also hit the caller's
         * shell, which shares our process group (sh -c 'loop' exited 143). */
        pid_t hp[64]; if (hogs > 64) hogs = 64;
        for (int i = 0; i < hogs; i++) if ((hp[i] = fork()) == 0) { for (;;) __asm__ volatile(""); }
        for (int i = 0; i < runs; i++) pidfd_wake_conmon();
        for (int i = 0; i < hogs; i++) if (hp[i] > 0) { kill(hp[i], SIGKILL); waitpid(hp[i], NULL, 0); }
        printf("pidfd-wake-loop: %d runs, %d failed\n", runs, nfail);
        return nfail > 100 ? 100 : nfail;
    }
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
    {
        struct sigaction fa; memset(&fa, 0, sizeof fa);
        main_pid = getpid();
        fa.sa_sigaction = on_fatal; fa.sa_flags = SA_SIGINFO | SA_RESETHAND;
        int fsig[] = { SIGSEGV, SIGBUS, SIGILL, SIGFPE };
        for (unsigned i = 0; i < sizeof fsig / sizeof fsig[0]; i++) sigaction(fsig[i], &fa, NULL);
    }
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
    faccessat_legacy_a3();
    splice_socket();
    splice_socket_race();

    rseq_register();
    rseq_cpu_id();
    rseq_abort_signal();
    rseq_abort_preempt();
    rseq_bad_sig();
    rseq_inherit(self);
    rseq_cs_flags();
    rseq_mm_cid();

    pidfd_open_errors();
    pidfd_poll_exit();
    pidfd_wake_conmon();
    pidfd_zombie_stat();
    pidfd_zombie_contend();
    pidfd_epoll_et();
    pidfd_waitid();
    pidfd_send_signal_case();
    pidfd_clone();
    pidfd_getfd();
    pidfd_proc_link();
    iov_fault();
    pidfd_namespace();
    pidfd_getfd_inet();
    inet_fork_shared();
    fstat_sock_epoll();

    for (int i = 0; i < ndirs; i++) readdir_unlink(dirs[i]);
    poll_redirected_stdin(dirs[0]);
    fchmodat_flags(dirs[0]);
    preadv2_cases(dirs[0]);
    pwritev2_append_cases(dirs[0]);
    statx_matches_stat(dirs[0]);
    exec_many_args(self);
    uaccess_cases();

    /* VFS ABI */
    stdio_close_ebadf();
    for (int i = 0; i < ndirs; i++) getdents_shared(dirs[i]);
    getdents_shared("/dev");
    nonblock_shared(dirs[0]);
    dev_input_dir();
    etc_on_root_fs();
    tmpfs_long_paths();
    comm_deep_path(self);
    pts_owner();
    pts_owner_user();
    run_tmpfs();
    mount_ro_flag();
    image_timestamps();
    proc_self_exe_access();
    linux_compat_facts();
    affinity_placement();
    kaddr_efault();

    printf("syscalltest: %d passed, %d failed, %d skipped\n", npass, nfail, nskip);
    return nfail > 100 ? 100 : nfail;
}
