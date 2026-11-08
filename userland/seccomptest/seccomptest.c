/*
 * seccomptest — seccomp(2) / prctl(PR_*_SECCOMP) behaviour, with the results
 * Linux gives. Builds for the host (cc) and LeandrOS (zig cc -target
 * <arch>-linux-musl -static).
 *
 * Group S1 (historic; the kernel now installs filters, see the S3 cases below):
 *   get_seccomp_disabled   PR_GET_SECCOMP == 0
 *   set_seccomp_einval     PR_SET_SECCOMP(SECCOMP_MODE_FILTER, prog) == EINVAL
 *   seccomp_set_filter     seccomp(SET_MODE_FILTER, 0, prog) == EINVAL
 *   seccomp_bad_op         seccomp(unknown op) == EINVAL
 *
 * Groups A-L (install, actions, SIGSYS, TSYNC, /proc) are added by the hook lane.
 * Output: "<name>: PASS|FAIL ...|SKIP ..." per case and a summary line.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <pthread.h>
#include <signal.h>
#include <string.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/wait.h>

#ifndef SYS_seccomp
# if defined(__aarch64__)
#  define SYS_seccomp 277
# else
#  define SYS_seccomp 317
# endif
#endif
#ifndef PR_GET_SECCOMP
# define PR_GET_SECCOMP 21
# define PR_SET_SECCOMP 22
#endif
#define SECCOMP_MODE_FILTER 2
#define SECCOMP_SET_MODE_FILTER 1

struct sf { uint16_t code; uint8_t jt, jf; uint32_t k; };
struct sfprog { uint16_t len; struct sf *filter; };

#define SECCOMP_SET_MODE_STRICT 0
#ifndef EOPNOTSUPP
# define EOPNOTSUPP 95
#endif
#define SECCOMP_GET_ACTION_AVAIL 2
#define RET_ALLOW 0x7fff0000u
#define RET_KILL_PROCESS 0x80000000u
#define RET_KILL_THREAD 0u
#define RET_ERRNO(e) (0x00050000u | (e))
#define RET_TRACE 0x7ff00000u
#define RET_LOG 0x7ffc0000u
#define LD_NR   { 0x20, 0, 0, 0 }
#define JEQ(k)  { 0x15, 0, 1, (k) }  /* if nr==k fall to next insn, else skip it */
#define RET(v)  { 0x06, 0, 0, (v) }

static int npass, nfail, nskip;
static void pass(const char *n) { printf("%s: PASS\n", n); npass++; fflush(stdout); }
static void fail(const char *n, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
static void fail(const char *n, const char *fmt, ...) {
    va_list ap;
    printf("%s: FAIL ", n);
    va_start(ap, fmt); vprintf(fmt, ap); va_end(ap);
    printf("\n"); nfail++; fflush(stdout);
}
static void expect_errno(const char *n, long r, int want) {
    if (r == -1 && errno == want) pass(n);
    else fail(n, "ret %ld errno %d, want -1/%d", r, errno, want);
}

static struct sf allow_all[1] = { RET(RET_ALLOW) };
static struct sfprog prog = { 1, allow_all };

/* Install "nr == SYS_getppid -> act, else ALLOW". */
static long install_getppid(unsigned act, unsigned long flags) {
    struct sf f[4] = { LD_NR, JEQ(SYS_getppid), RET(act), RET(RET_ALLOW) };
    struct sfprog p = { 4, f };
    return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, flags, &p);
}
typedef int (*childfn)(void);
/* Run fn in a forked child; return its wait status. */
static int run(childfn fn) {
    int st = 0;
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) _exit(fn());
    if (p < 0 || waitpid(p, &st, 0) != p) return -1;
    return st;
}
static void exit0(const char *n, childfn fn) {
    int st = run(fn);
    if (WIFEXITED(st) && WEXITSTATUS(st) == 0) pass(n);
    else fail(n, "status 0x%x (exit %d)", st, WIFEXITED(st) ? WEXITSTATUS(st) : -1);
}
static void killed(const char *n, childfn fn, int sig) {
    int st = run(fn);
    if (WIFSIGNALED(st) && WTERMSIG(st) == sig) pass(n);
    else fail(n, "status 0x%x, want signal %d", st, sig);
}
static int nnp(void) { return prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0); }
#define PPID_IS(e) (syscall(SYS_getppid) == -1 && errno == (e))

static int c_errno(void) {
    nnp();
    if (install_getppid(RET_ERRNO(EPERM), 0) != 0) return 1;
    if (!PPID_IS(EPERM)) return 2;
    if (syscall(SYS_getpid) <= 0) return 3;           /* others still run */
    return prctl(PR_GET_SECCOMP, 0, 0, 0, 0) == 2 ? 0 : 4;
}
static int c_errno_cap(void) {
    nnp();
    if (install_getppid(RET_ERRNO(0xffff), 0) != 0) return 1;
    return PPID_IS(4095) ? 0 : 2;
}
static int c_errno_zero(void) {
    nnp();
    if (install_getppid(RET_ERRNO(0), 0) != 0) return 1;
    return syscall(SYS_getppid) == 0 ? 0 : 2;
}
static int c_kill_thread(void) { nnp(); install_getppid(RET_KILL_THREAD, 0); syscall(SYS_getppid); return 0; }
static int c_kill_process(void) { nnp(); install_getppid(RET_KILL_PROCESS, 0); syscall(SYS_getppid); return 0; }
static int c_log(void) {
    nnp();
    if (install_getppid(RET_LOG, 2 /*FILTER_FLAG_LOG*/) != 0) return 1;
    return syscall(SYS_getppid) > 0 ? 0 : 2;
}
static int c_trace(void) { nnp(); install_getppid(RET_TRACE, 0); return PPID_IS(ENOSYS) ? 0 : 1; }
static int c_stack_errno_then_allow(void) {
    nnp();
    if (install_getppid(RET_ERRNO(EPERM), 0) != 0) return 1;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog) != 0) return 2;
    return PPID_IS(EPERM) ? 0 : 3;                    /* older ERRNO still wins */
}
static int c_stack_errno_then_kill(void) {
    nnp();
    install_getppid(RET_ERRNO(EPERM), 0);
    install_getppid(RET_KILL_PROCESS, 0);
    syscall(SYS_getppid);                              /* KILL beats ERRNO */
    return 0;
}
static int c_inherit_fork(void) {
    nnp();
    if (install_getppid(RET_ERRNO(EACCES), 0) != 0) return 1;
    pid_t p = fork();
    if (p == 0) _exit(PPID_IS(EACCES) ? 0 : 1);
    int st = 0;
    if (waitpid(p, &st, 0) != p) return 2;
    return (WIFEXITED(st) && WEXITSTATUS(st) == 0) ? 0 : 3;
}
static char *self;
static int c_exec(void) {
    nnp();
    if (install_getppid(RET_ERRNO(EACCES), 0) != 0) return 1;
    execl(self, self, "exec-child", (char *)0);
    return 2;
}
static void *exec_thread(void *a) {
    (void)a;
    nnp();
    if (install_getppid(RET_ERRNO(EACCES), 0) != 0) _exit(1);
    execl(self, self, "exec-child", (char *)0);
    _exit(2);
    return 0;
}
static int c_exec_from_thread(void) {
    pthread_t t;
    if (pthread_create(&t, 0, exec_thread, 0) != 0) return 1;
    pthread_join(t, 0);                                /* exec replaces the process */
    return 3;
}
static int c_nnp_eacces(void) {
    if (setgid(1000) != 0 || setuid(1000) != 0) return 77;   /* cannot drop root */
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog) != -1 || errno != EACCES) return 1;
    errno = 0;
    if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog, 0, 0) != -1 || errno != EACCES) return 2;
    nnp();
    return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog) == 0 ? 0 : 3;
}
static int c_verifier(void) {
    nnp();
    struct sf bad[1] = { { 0xffff, 0, 0, 0 } };
    struct sfprog pb = { 1, bad };
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &pb) != -1 || errno != EINVAL) return 1;
    struct sf noret[1] = { LD_NR };
    struct sfprog pn = { 1, noret };
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &pn) != -1 || errno != EINVAL) return 2;
    struct sfprog pz = { 0, allow_all };
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &pz) != -1 || errno != EINVAL) return 3;
    struct sfprog pf = { 1, (struct sf *)8 };
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &pf) != -1 || errno != EFAULT) return 4;
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 8 /*unknown flag*/, &prog) != -1 || errno != EINVAL) return 5;
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 1 /*TSYNC*/, &prog) != -1 || errno != EINVAL) return 6;
    return prctl(PR_GET_SECCOMP, 0, 0, 0, 0) == 0 ? 0 : 7;   /* nothing got installed */
}
static int c_strict_ok(void) {
    if (prctl(PR_SET_SECCOMP, 1, 0, 0, 0) != 0) return 1;
    if (write(1, "", 0) != 0) return 2;
    syscall(SYS_exit_group, 0);
    return 3;
}
static int c_strict_kill(void) {
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_STRICT, 0, 0) != 0) return 1;
    syscall(SYS_getppid);
    return 0;
}
static int c_action_avail(void) {
    unsigned a = RET_KILL_PROCESS;
    if (syscall(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0, &a) != 0) return 1;
    a = 0x12340000u;
    errno = 0;
    return (syscall(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0, &a) == -1 && errno == EOPNOTSUPP) ? 0 : 2;
}


/* Value of "<key>:\t<n>" in /proc/self/status (or /proc/self/task/<tid>/status), -1 if absent. */
static long pstat_in(const char *path, const char *key) {
    char b[4096]; int fd = open(path, O_RDONLY); if (fd < 0) return -2;
    ssize_t n = read(fd, b, sizeof b - 1); close(fd); if (n <= 0) return -3;
    b[n] = 0;
    size_t kl = strlen(key);
    for (char *l = b; l && *l; l = strchr(l, '\n') ? strchr(l, '\n') + 1 : 0)
        if (!strncmp(l, key, kl) && l[kl] == ':') return strtol(l + kl + 1, 0, 10);
    return -1;
}
static long pstat(const char *key) { return pstat_in("/proc/self/status", key); }
static int c_proc_status(void) {
    char tp[64]; snprintf(tp, sizeof tp, "/proc/self/task/%ld/status", (long)syscall(SYS_gettid));
    if (pstat("Seccomp") != 0 || pstat("Seccomp_filters") != 0 || pstat("NoNewPrivs") != 0) return 1;
    nnp();
    if (pstat("NoNewPrivs") != 1) return 2;
    if (install_getppid(RET_ALLOW, 0) != 0) return 3;
    if (pstat("Seccomp") != 2 || pstat("Seccomp_filters") != 1) return 4;
    if (install_getppid(RET_ALLOW, 0) != 0) return 5;
    if (pstat("Seccomp_filters") != 2) return 6;
    if (pstat_in(tp, "Seccomp") != 2 || pstat_in(tp, "Seccomp_filters") != 2) return 7;
    return 0;
}
/* Strict mode allows only read/write/exit, and /proc content is generated at
 * open(), so a strict child blocks in read() on a pipe while the parent reads
 * the child's /proc/<pid>/status. */
static int c_proc_strict(void) {
    int up[2], dn[2];
    if (pipe(up) || pipe(dn)) return 1;
    pid_t c = fork();
    if (c == 0) {
        char x;
        if (syscall(SYS_seccomp, SECCOMP_SET_MODE_STRICT, 0, 0) != 0) _exit(2);
        if (write(up[1], "r", 1) != 1) _exit(3);
        if (read(dn[0], &x, 1) != 1) _exit(4);
        _exit(0);
    }
    if (c < 0) return 5;
    char x, p[64]; int ok = 0, st = 0;
    if (read(up[0], &x, 1) == 1) {
        snprintf(p, sizeof p, "/proc/%d/status", (int)c);
        ok = pstat_in(p, "Seccomp") == 1 && pstat_in(p, "Seccomp_filters") == 0;
    }
    if (write(dn[1], "g", 1) != 1) return 6;
    if (waitpid(c, &st, 0) != c || !WIFEXITED(st) || WEXITSTATUS(st) != 0) return 7;
    return ok ? 0 : 8;
}
int main(int argc, char **argv) {
    self = argv[0];
    if (argc > 1 && !strcmp(argv[1], "exec-child"))
        return PPID_IS(EACCES) ? 0 : 1;
    long r = prctl(PR_GET_SECCOMP, 0, 0, 0, 0);
    if (r == 0) pass("get_seccomp_disabled"); else fail("get_seccomp_disabled", "ret %ld errno %d", r, errno);
    errno = 0;
    expect_errno("seccomp_bad_op", syscall(SYS_seccomp, 0xdead, 0, 0), EINVAL);

    exit0("errno", c_errno);
    exit0("errno_capped_4095", c_errno_cap);
    exit0("errno_zero_returns_zero", c_errno_zero);
    killed("kill_thread_sigsys", c_kill_thread, SIGSYS);
    killed("kill_process_sigsys", c_kill_process, SIGSYS);
    exit0("log_allows", c_log);
    exit0("trace_without_tracer_enosys", c_trace);
    exit0("stack_errno_then_allow", c_stack_errno_then_allow);
    killed("stack_kill_beats_errno", c_stack_errno_then_kill, SIGSYS);
    exit0("inherit_fork", c_inherit_fork);
    exit0("inherit_execve", c_exec);
    exit0("inherit_exec_from_nonmain_thread", c_exec_from_thread);
    {
        int st = run(c_nnp_eacces);
        if (WIFEXITED(st) && WEXITSTATUS(st) == 77) { printf("no_new_privs_eacces: SKIP (cannot drop root)\n"); nskip++; }
        else if (WIFEXITED(st) && WEXITSTATUS(st) == 0) pass("no_new_privs_eacces");
        else fail("no_new_privs_eacces", "status 0x%x", st);
    }
    exit0("verifier_rejects", c_verifier);
    exit0("strict_allows_rw_exit", c_strict_ok);
    killed("strict_kills_other_syscall", c_strict_kill, SIGKILL);
    exit0("action_avail", c_action_avail);
    exit0("proc_status_seccomp", c_proc_status);
    exit0("proc_status_strict", c_proc_strict);

    printf("seccomptest: %d passed, %d failed, %d skipped\n", npass, nfail, nskip);
    return nfail;
}
