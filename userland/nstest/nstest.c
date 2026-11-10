/*
 * nstest — namespace syscalls. Builds for the host (cc nstest.c) and for
 * LeandrOS (zig cc -target <arch>-linux-musl -static).
 *
 * L0 (nsreject): clone/clone3/unshare with a CLONE_NEW* flag the kernel does
 * not implement must fail with EINVAL instead of silently creating nothing.
 * These cases are LeandrOS-interim: stock Linux would create the namespace
 * for root, so set NSTEST_HOST=1 to skip them on a host. Later lanes turn
 * individual flags on and move their cases to real behaviour checks.
 */
#define _GNU_SOURCE
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <sys/utsname.h>
#include <linux/sched.h>
#include <fcntl.h>
#include <dirent.h>
#include <stddef.h>
#include <pthread.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/uio.h>
#include <netinet/in.h>
#include <netinet/in.h>
#include <arpa/inet.h>

#ifndef CLONE_NEWCGROUP
#define CLONE_NEWCGROUP 0x02000000
#endif

static int npass, nfail;

static void check(const char *name, int ok)
{
    printf("%s %s\n", ok ? "PASS" : "FAIL", name);
    if (ok) npass++; else nfail++;
}

static const struct { const char *n; unsigned long f; } flags[] = {
    {"NEWTIME", 0x80},
};

static long do_clone(unsigned long fl)
{
    return syscall(SYS_clone, fl | SIGCHLD, 0, 0, 0, 0);
}

static long do_clone3(unsigned long fl)
{
    struct clone_args a;
    memset(&a, 0, sizeof a);
    a.flags = fl;
    a.exit_signal = SIGCHLD;
    return syscall(SYS_clone3, &a, sizeof a);
}

#define V3 0x20080522
#define CAP_SYS_CHROOT 18
struct hdr { unsigned v; int pid; };
struct dat { unsigned eff, prm, inh; };

#define CHECK(c) do { if (!(c)) { printf("  FAIL line %d: %s (errno %d)\n", __LINE__, #c, errno); return 1; } } while (0)

static int cget(struct dat d[2]) { struct hdr h = { V3, 0 }; return syscall(SYS_capget, &h, d); }
static int drop_to(int uid) { return setgid(uid) || setuid(uid); }
static int newuser_child(int (*fn)(void))
{
    pid_t c = syscall(SYS_clone, SIGCHLD | CLONE_NEWUSER, 0, 0, 0, 0);
    if (c == 0) _exit(fn());
    if (c < 0) return -1;
    int st = 0; waitpid(c, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 100;
}
static int full_caps(void)
{
    struct dat d[2];
    return cget(d) == 0 && d[0].eff == 0xFFFFFFFFu && (d[1].eff & 0x1FF) == 0x1FF && d[0].prm == d[0].eff;
}

static int caps_ok(void) { return full_caps() ? 0 : 1; }

static int t_unpriv_clone(void)
{
    struct dat d[2];
    CHECK(drop_to(1000) == 0);
    CHECK(cget(d) == 0 && d[0].eff == 0);
    int r = newuser_child(caps_ok);
    CHECK(r == 0);
    CHECK(cget(d) == 0 && d[0].eff == 0 && d[0].prm == 0);  /* parent unchanged */
    return 0;
}
static int chroot_in_ns(void) { return chroot("/") == 0 ? 0 : 1; }
static int t_ns_chroot(void)
{
    CHECK(drop_to(1000) == 0);
    CHECK(chroot("/") == -1 && errno == EPERM);
    CHECK(newuser_child(chroot_in_ns) == 0);
    return 0;
}

static pid_t root_sleeper;
static int host_guards(void)
{
    CHECK(full_caps());
    CHECK(geteuid() == 65534);                                /* unmapped: overflow id */
    CHECK(setuid(0) == -1);                                   /* credentials are host ids */
    CHECK(sethostname("nstest", 6) == -1);
    CHECK(mount("none", "/tmp", "tmpfs", 0, "") == -1);
    CHECK(chmod("/etc/passwd", 0666) == -1);                  /* not the owner */
    CHECK(kill(root_sleeper, SIGKILL) == -1 && errno == EPERM);
    return 0;
}
static int t_host_unaffected(void)
{
    root_sleeper = fork();
    if (root_sleeper == 0) { pause(); _exit(0); }
    pid_t c = fork();
    if (c == 0) { if (drop_to(1000)) _exit(2); _exit(newuser_child(host_guards)); }
    int st = 0; waitpid(c, &st, 0);
    kill(root_sleeper, SIGKILL); waitpid(root_sleeper, 0, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}


/* H1: chdir/open/stat through another user's /proc/<pid>/{cwd,root,ns} are EACCES. */
static int magic_unpriv(void)
{
    char p[96], buf[8]; struct stat sb;
    if (drop_to(1000)) return 2;
    snprintf(p, sizeof p, "/proc/%d/cwd", (int)root_sleeper);
    CHECK(chdir(p) == -1 && errno == EACCES);
    CHECK(stat(p, &sb) == -1 && errno == EACCES);
    CHECK(lstat(p, &sb) == -1 && errno == EACCES);
    snprintf(p, sizeof p, "/proc/%d/root/etc/hostname", (int)root_sleeper);
    CHECK(open(p, O_RDONLY) == -1 && errno == EACCES);
    snprintf(p, sizeof p, "/proc/%d/ns/net", (int)root_sleeper);
    CHECK(readlink(p, buf, sizeof buf) == -1 && errno == EACCES);
    CHECK(chdir("/proc/self/cwd") == 0);
    int fd = open("/proc/self/root/etc/hostname", O_RDONLY);
    CHECK(fd >= 0); if (fd >= 0) close(fd);
    return 0;
}
static int t_magic_link_access(void)
{
    root_sleeper = fork();
    if (root_sleeper == 0) { pause(); _exit(0); }
    pid_t c = fork();
    if (c == 0) _exit(magic_unpriv());
    int st = 0; waitpid(c, &st, 0);
    kill(root_sleeper, SIGKILL); waitpid(root_sleeper, 0, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}

static int t_unshare_basic(void)
{
    CHECK(drop_to(1000) == 0);
    CHECK(unshare(CLONE_NEWUSER) == 0);
    CHECK(full_caps());
    CHECK(chroot("/") == 0);
    return 0;
}
static void *idle_thread(void *a) { (void)a; pause(); return 0; }
static int t_unshare_threaded(void)
{
    pthread_t t;
    CHECK(drop_to(1000) == 0);
    CHECK(pthread_create(&t, 0, idle_thread, 0) == 0);
    CHECK(unshare(CLONE_NEWUSER) == -1 && errno == EINVAL);
    return 0;
}
static int t_other_new_einval(void)
{
    int fl[] = { 0x80 }; /* CLONE_NEWTIME: still unimplemented */
    CHECK(drop_to(1000) == 0);
    for (unsigned i = 0; i < 1; i++) {
        CHECK(unshare(fl[i]) == -1 && errno == EINVAL);
        long r = do_clone3(fl[i]);
        CHECK(r == -1 && errno == EINVAL);
        r = do_clone3(CLONE_NEWUSER | fl[i]);
        CHECK(r == -1 && errno == EINVAL);
    }
    /* NEWUSER excludes CLONE_FS */
    long r = syscall(SYS_clone, SIGCHLD | CLONE_NEWUSER | CLONE_FS, 0, 0, 0, 0);
    CHECK(r == -1 && errno == EINVAL);
    return 0;
}
/* Map uid/gid 1000 onto itself in the caller's fresh namespace: a nested
 * unshare needs the creator's ids mapped (design-idmap rule 8). */
static int map_self_1000(void)
{
    int fd;
    if ((fd = open("/proc/self/uid_map", O_WRONLY)) < 0) return 1;
    if (write(fd, "1000 1000 1\n", 12) != 12) { close(fd); return 1; }
    close(fd);
    if ((fd = open("/proc/self/setgroups", O_WRONLY)) < 0) return 1;
    if (write(fd, "deny", 4) != 4) { close(fd); return 1; }
    close(fd);
    if ((fd = open("/proc/self/gid_map", O_WRONLY)) < 0) return 1;
    if (write(fd, "1000 1000 1\n", 12) != 12) { close(fd); return 1; }
    close(fd);
    return 0;
}
static int t_depth_limit(void)
{
    CHECK(drop_to(1000) == 0);
    for (int i = 0; i < 32; i++) { CHECK(unshare(CLONE_NEWUSER) == 0); CHECK(map_self_1000() == 0); }
    CHECK(unshare(CLONE_NEWUSER) == -1 && errno == EUSERS);
    return 0;
}
static int t_leak(void)
{
    /* the table holds 255 namespaces: 1200 sequential ones only fit if they are released */
    CHECK(drop_to(1000) == 0);
    for (int i = 0; i < 1200; i++) {
        pid_t c = syscall(SYS_clone, SIGCHLD | CLONE_NEWUSER, 0, 0, 0, 0);
        if (c == 0) _exit(0);
        CHECK(c > 0);
        waitpid(c, 0, 0);
    }
    for (int i = 0; i < 40; i++) {  /* grand-children share, nested chains release together */
        pid_t c = fork();
        if (c == 0) { for (int j = 0; j < 8; j++) if (unshare(CLONE_NEWUSER) || map_self_1000()) _exit(1); _exit(0); }
        int st = 0; waitpid(c, &st, 0);
        CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    }
    CHECK(newuser_child(caps_ok) == 0);
    return 0;
}

static int print_euid(void) { printf("  euid=%d\n", geteuid()); return (geteuid() == 1000 || geteuid() == 65534) ? 0 : 7; }
static int exec_suid(void)
{
    execl("/tmp/nstest-suid", "nstest-suid", "--euid", (char *)0);
    return 9;
}
static int t_suid_ignored(void)
{
    int in = open("/proc/self/exe", O_RDONLY), out = open("/tmp/nstest-suid", O_WRONLY | O_CREAT | O_TRUNC, 0755);
    char b[4096]; int n;
    CHECK(in >= 0 && out >= 0);
    while ((n = read(in, b, sizeof b)) > 0) CHECK(write(out, b, n) == n);
    close(out); close(in);
    CHECK(chmod("/tmp/nstest-suid", 04755) == 0);
    CHECK(drop_to(1000) == 0);
    /* control: outside a userns the set-id bit is honoured (euid 0) */
    pid_t c = fork();
    if (c == 0) { execl("/tmp/nstest-suid", "nstest-suid", "--euid", (char *)0); _exit(9); }
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 7);
    /* inside: ignored, euid stays 1000 */
    CHECK(newuser_child(exec_suid) == 0);
    return 0;
}


/* ---- pid namespaces (lane nspid) ---------------------------------------- */
/* An "init" is the first child forked after unshare(CLONE_NEWPID) inside a
 * user namespace. Every case is a user-namespace child body (newuser_child)
 * that arms alarm(10) itself. */
#define PCASE(name, ub) static int t_##name(void) { CHECK(drop_to(1000) == 0); return newuser_child(ub); }
static int child_ok(pid_t c)
{
    int st = 0;
    if (waitpid(c, &st, 0) != c) return 1;
    return WIFEXITED(st) && WEXITSTATUS(st) == 0 ? 0 : 1;
}
/* In a userns child: unshare(NEWPID), fork the init running body; 0 = it returned 0. */
static int in_init(int (*body)(void))
{
    alarm(10);
    CHECK(unshare(CLONE_NEWPID) == 0);
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) _exit(body());
    return child_ok(c);
}

static int ub_ff(void) { return unshare(CLONE_NEWPID) == 0 ? 0 : 1; }
PCASE(ff_probe, ub_ff)

static int t_unshare_pid_eperm(void)
{
    CHECK(drop_to(1000) == 0);
    CHECK(unshare(CLONE_NEWPID) == -1 && errno == EPERM);
    return 0;
}

static int ub_twice(void)
{
    CHECK(unshare(CLONE_NEWPID) == 0);
    CHECK(unshare(CLONE_NEWPID) == -1 && errno == EINVAL);
    return 0;
}
PCASE(unshare_pid_twice, ub_twice)

static int ub_thread_after(void)
{
    CHECK(unshare(CLONE_NEWPID) == 0);
    pthread_t t;
    CHECK(pthread_create(&t, 0, idle_thread, 0) != 0);
    return 0;
}
PCASE(thread_after_unshare, ub_thread_after)

static int tidword;
static int ib_pid1(void)
{
    CHECK(getpid() == 1);
    CHECK(syscall(SYS_gettid) == 1);
    CHECK(getppid() == 0);
    CHECK(syscall(SYS_set_tid_address, &tidword) == 1);
    return 0;
}
static int ub_pid1(void) { return in_init(ib_pid1); }
PCASE(pid1_self, ub_pid1)

static int ub_parent_view(void)
{
    alarm(10);
    CHECK(unshare(CLONE_NEWPID) == 0);
    int p[2]; CHECK(pipe(p) == 0);
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) { char b; read(p[0], &b, 1); _exit(0); }
    CHECK(c > 1);
    CHECK(kill(c, 0) == 0);
    CHECK(write(p[1], "x", 1) == 1);
    int st = 0;
    CHECK(waitpid(-1, &st, 0) == c);
    return 0;
}
PCASE(parent_view, ub_parent_view)

static int ib_child(void)
{
    pid_t c = fork();
    if (c == 0) _exit(getppid() == 1 && kill(1, 0) == 0 && getpid() != 1 ? 0 : 1);
    return child_ok(c);
}
static int ub_child_of_init(void) { return in_init(ib_child); }
PCASE(child_of_init, ub_child_of_init)

static int ub_visibility(void)
{
    alarm(10);
    pid_t outside = getpid();
    CHECK(unshare(CLONE_NEWPID) == 0);
    int p[2]; CHECK(pipe(p) == 0);
    pid_t i = fork();
    CHECK(i >= 0);
    if (i == 0) {
        pid_t c = fork();
        if (c == 0) {
            pid_t kinit = 0;
            if (read(p[0], &kinit, sizeof kinit) != sizeof kinit) _exit(2);
            if (kill(outside, 0) != -1 || errno != ESRCH) _exit(3);
            if (kill(kinit, 0) != -1 || errno != ESRCH) _exit(4);
            _exit(0);
        }
        _exit(child_ok(c));
    }
    CHECK(write(p[1], &i, sizeof i) == sizeof i);
    return child_ok(i);
}
PCASE(visibility, ub_visibility)

static int ub_member(void)
{
    alarm(10);
    CHECK(unshare(CLONE_NEWPID) == 0);
    int p[2]; CHECK(pipe(p) == 0);
    pid_t a = fork();
    CHECK(a >= 0);
    if (a == 0) { char b; read(p[0], &b, 1); _exit(0); }
    pid_t b = fork();
    CHECK(b >= 0);
    if (b == 0) _exit(getpid() != 1 && getppid() == 0 && kill(1, 0) == 0 ? 0 : 1);
    CHECK(child_ok(b) == 0);
    CHECK(write(p[1], "x", 1) == 1);
    return child_ok(a);
}
PCASE(member_after_unshare, ub_member)

static int ib_wait(void)
{
    pid_t c[3];
    for (int i = 0; i < 3; i++) {
        c[i] = fork();
        if (c[i] == 0) _exit(10 + i);
        CHECK(c[i] > 1);
    }
    for (int i = 2; i >= 0; i--) {
        int st = 0;
        CHECK(waitpid(c[i], &st, 0) == c[i]);
        CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 10 + i);
    }
    return 0;
}
static int ub_wait_inside(void) { return in_init(ib_wait); }
PCASE(wait_inside, ub_wait_inside)

static volatile pid_t got_pid = -1;
static volatile int got;
static void sig_h(int s, siginfo_t *si, void *u) { (void)s; (void)u; got_pid = si->si_pid; got = 1; }
static void arm_h(void)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = sig_h; sa.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR1, &sa, 0);
}
static int wait_got(void) { for (int n = 0; !got && n < 8000; n++) usleep(1000); return got; }
static int ib_siginfo(void)
{
    int a[2]; if (pipe(a)) return 1;
    arm_h();
    pid_t c = fork();
    if (c == 0) {
        got = 0; got_pid = -1;
        if (write(a[1], "x", 1) != 1) _exit(2);
        if (!wait_got() || got_pid != 1) _exit(3);          /* init -> C: si_pid == 1 */
        if (kill(1, SIGUSR1) != 0) _exit(4);
        _exit(0);
    }
    char b; if (read(a[0], &b, 1) != 1) return 2;
    if (kill(c, SIGUSR1) != 0) return 3;
    if (!wait_got() || got_pid != c) return 4;               /* C -> init: si_pid == C */
    return child_ok(c);
}
static int ub_siginfo(void) { return in_init(ib_siginfo); }
PCASE(siginfo_pid, ub_siginfo)

/* An outside parent signalling the init: si_pid is 0. */
static int ub_siginfo_out(void)
{
    alarm(10);
    CHECK(unshare(CLONE_NEWPID) == 0);
    int a[2]; CHECK(pipe(a) == 0);
    pid_t i = fork();
    CHECK(i >= 0);
    if (i == 0) {
        arm_h();
        if (write(a[1], "x", 1) != 1) _exit(2);
        _exit(wait_got() && got_pid == 0 ? 0 : 3);
    }
    char b; CHECK(read(a[0], &b, 1) == 1);
    CHECK(kill(i, SIGUSR1) == 0);
    return child_ok(i);
}
PCASE(siginfo_outside, ub_siginfo_out)

static volatile int thr_tid, thr_flag, thr_pi_ok, thr_busy;
static pthread_mutex_t pimx, rmx;
static void *thr_fn(void *a)
{
    (void)a;
    thr_tid = (int)syscall(SYS_gettid);
    thr_flag = 1;
    return 0;
}
static void *thr_pi(void *a)
{
    (void)a;
    thr_busy = pthread_mutex_trylock(&pimx);
    thr_flag = 1;
    thr_pi_ok = pthread_mutex_lock(&pimx) == 0;
    if (thr_pi_ok) pthread_mutex_unlock(&pimx);
    return 0;
}
static void *thr_robust(void *a) { (void)a; pthread_mutex_lock(&rmx); return 0; }
static void usr2(int s) { (void)s; thr_flag = 2; }
static void *thr_wait_sig(void *a)
{
    (void)a;
    thr_flag = 1;
    for (int n = 0; thr_flag != 2 && n < 8000; n++) usleep(1000);
    return 0;
}
static int ib_threads(void)
{
    pthread_t t;
    CHECK(getpid() == 1 && syscall(SYS_gettid) == 1);
    CHECK(pthread_create(&t, 0, thr_fn, 0) == 0);
    pthread_join(t, 0);
    CHECK(thr_tid != 1 && thr_tid > 0);
    /* pthread_kill uses the tid musl cached from CLONE_PARENT_SETTID. */
    signal(SIGUSR2, usr2);
    thr_flag = 0;
    CHECK(pthread_create(&t, 0, thr_wait_sig, 0) == 0);
    while (!thr_flag) usleep(1000);
    CHECK(pthread_kill(t, SIGUSR2) == 0);
    pthread_join(t, 0);
    CHECK(thr_flag == 2);
    /* Priority-inheritance mutex owned by the main thread (owner word == 1). */
    pthread_mutexattr_t at; pthread_mutexattr_init(&at);
    pthread_mutexattr_setprotocol(&at, PTHREAD_PRIO_INHERIT);
    pthread_mutex_init(&pimx, &at);
    CHECK(pthread_mutex_lock(&pimx) == 0);
    thr_flag = 0;
    CHECK(pthread_create(&t, 0, thr_pi, 0) == 0);
    while (!thr_flag) usleep(1000);
    usleep(20000);
    CHECK(thr_busy == EBUSY);
    CHECK(pthread_mutex_unlock(&pimx) == 0);
    pthread_join(t, 0);
    CHECK(thr_pi_ok);
    /* Robust mutex whose owner thread exits. */
    pthread_mutexattr_t ra; pthread_mutexattr_init(&ra);
    pthread_mutexattr_setrobust(&ra, PTHREAD_MUTEX_ROBUST);
    pthread_mutex_init(&rmx, &ra);
    CHECK(pthread_create(&t, 0, thr_robust, 0) == 0);
    pthread_join(t, 0);
    CHECK(pthread_mutex_lock(&rmx) == EOWNERDEAD);
    return 0;
}
static int ub_threads(void) { return in_init(ib_threads); }
PCASE(pthread_in_pidns, ub_threads)

static int ib_nested(void)
{
    if (unshare(CLONE_NEWPID) != 0) return 1;
    pid_t g = fork();
    if (g == 0) _exit(getpid() == 1 && getppid() == 0 ? 0 : 1);
    if (g <= 1) return 2;
    int st = 0;
    if (waitpid(g, &st, 0) != g) return 3;
    return WIFEXITED(st) && WEXITSTATUS(st) == 0 ? 0 : 4;
}
static int ub_nested(void) { return in_init(ib_nested); }
PCASE(nested, ub_nested)

static int ub_leak1(void)
{
    if (unshare(CLONE_NEWPID) != 0) return 1;
    pid_t c = fork();
    if (c == 0) _exit(0);
    return c > 0 ? child_ok(c) : 2;
}
static int t_pidns_leak(void)
{
    for (int i = 0; i < 600; i++) CHECK(newuser_child(ub_leak1) == 0);
    CHECK(drop_to(1000) == 0);
    CHECK(newuser_child(ub_ff) == 0);
    return 0;
}

static int ub_fork_enomem(void)
{
    alarm(10);
    CHECK(unshare(CLONE_NEWPID) == 0);
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) _exit(0);
    CHECK(child_ok(c) == 0);
    errno = 0;
    CHECK(fork() == -1 && errno == ENOMEM);
    return 0;
}
PCASE(fork_enomem, ub_fork_enomem)


/* ---- /proc in a pid namespace (lane nspidproc) --------------------------- */
static int slurp(const char *p, char *b, int n)
{
    int fd = open(p, O_RDONLY);
    if (fd < 0) return -1;
    int r = read(fd, b, n - 1);
    close(fd);
    if (r >= 0) b[r] = 0;
    return r;
}
/* Tab-separated values of the "<key>:" line of a status buffer into out (as text). */
static int status_line(const char *st, const char *key, char *out, int n)
{
    char k[32]; snprintf(k, sizeof k, "\n%s:\t", key);
    const char *a = strstr(st, k);
    if (!a) return -1;
    a += strlen(k);
    int i = 0;
    while (*a && *a != '\n' && i < n - 1) out[i++] = *a++;
    out[i] = 0;
    return 0;
}
static int ntabs(const char *s) { int n = 1; for (; *s; s++) if (*s == '\t') n++; return n; }
static pid_t g_outer;

static int ib_proc_self(void)
{
    char b[64] = {0};
    ssize_t r = readlink("/proc/self", b, sizeof b - 1);
    if (r != 1 || b[0] != '1') return 1;
    char st[4096], v[64];
    if (slurp("/proc/self/status", st, sizeof st) < 0) return 2;
    if (status_line(st, "Pid", v, sizeof v) || strcmp(v, "1")) return 3;
    if (status_line(st, "PPid", v, sizeof v) || strcmp(v, "0")) return 4;
    if (status_line(st, "NSpid", v, sizeof v) || strcmp(v, "1")) return 5;
    return 0;
}
static int ub_proc_self(void) { return in_init(ib_proc_self); }
PCASE(proc_self_init, ub_proc_self)

/* init forks a child that blocks; init lists /proc. */
static int ib_listing(void)
{
    int p[2];
    if (pipe(p)) return 1;
    pid_t c = fork();
    if (c == 0) { char x; close(p[1]); read(p[0], &x, 1); _exit(0); }
    if (c <= 1) return 2;
    int saw1 = 0, sawc = 0, bad = 0;
    DIR *d = opendir("/proc");
    if (!d) return 3;
    struct dirent *e;
    while ((e = readdir(d))) {
        if (e->d_name[0] < '0' || e->d_name[0] > '9') continue;
        long v = atol(e->d_name);
        if (v == 1) saw1 = 1; else if (v == c) sawc = 1; else bad++;
    }
    closedir(d);
    /* hidden: the outside pid and the host-view number of init */
    char path[64]; struct stat sb;
    snprintf(path, sizeof path, "/proc/%d", (int)g_outer);
    int hid_outer = stat(path, &sb) == -1 && errno == ENOENT;
    int ok_c;
    snprintf(path, sizeof path, "/proc/%d", (int)c);
    ok_c = stat(path, &sb) == 0;
    write(p[1], "x", 1); close(p[1]);
    if (child_ok(c)) return 4;
    if (!saw1 || !sawc || bad) return 5;
    if (!hid_outer) return 6;
    if (!ok_c) return 7;
    return 0;
}
static int ub_listing(void) { g_outer = getpid(); return in_init(ib_listing); }
PCASE(proc_listing, ub_listing)

/* Outside reader sees host pid + 1, and three levels when nested. */
static int ub_status_views(void)
{
    alarm(10);
    CHECK(unshare(CLONE_NEWPID) == 0);
    int up[2], dn[2], gp[2];
    CHECK(pipe(up) == 0 && pipe(dn) == 0 && pipe(gp) == 0);
    pid_t i = fork();
    CHECK(i > 0 || i == 0);
    if (i == 0) {
        if (unshare(CLONE_NEWPID)) _exit(1);
        pid_t g = fork();
        if (g == 0) { char x; read(gp[0], &x, 1); _exit(0); }
        if (g <= 0) _exit(2);
        write(up[1], &g, sizeof g);
        char x; read(dn[0], &x, 1);
        write(gp[1], "x", 1);
        int st; waitpid(g, &st, 0);
        _exit(0);
    }
    pid_t g = 0;
    CHECK(read(up[0], &g, sizeof g) == sizeof g);
    char st[4096], v[64], path[64];
    snprintf(path, sizeof path, "/proc/%d/status", (int)i);
    CHECK(slurp(path, st, sizeof st) > 0);
    CHECK(status_line(st, "NSpid", v, sizeof v) == 0 && ntabs(v) == 2);
    char want[64]; snprintf(want, sizeof want, "%d\t1", (int)i);
    CHECK(strcmp(v, want) == 0);
    CHECK(status_line(st, "Pid", v, sizeof v) == 0 && atoi(v) == (int)i);
    CHECK(status_line(st, "NStgid", v, sizeof v) == 0 && ntabs(v) == 2);
    snprintf(path, sizeof path, "/proc/%d/status", (int)g);
    CHECK(slurp(path, st, sizeof st) > 0);
    CHECK(status_line(st, "NSpid", v, sizeof v) == 0 && ntabs(v) == 3);
    write(dn[1], "x", 1);
    int s2; waitpid(i, &s2, 0);
    CHECK(WIFEXITED(s2) && WEXITSTATUS(s2) == 0);
    return 0;
}
PCASE(status_views, ub_status_views)

static int t_proc_self_tgid(void)
{
    char b[32] = {0};
    ssize_t r = readlink("/proc/self", b, sizeof b - 1);
    CHECK(r > 0 && atoi(b) == (int)getpid());
    return 0;
}

static void reap(long r)
{
    if (r == 0) _exit(0);
    if (r > 0) waitpid((pid_t)r, NULL, 0);
}


/* ---- L2: /proc/<pid>/{uid_map,gid_map,setgroups}, /proc/<pid>/ns/* ---- */
static int wr(const char *path, const char *txt)
{
    int fd = open(path, O_WRONLY);
    if (fd < 0) return -errno;
    ssize_t n = write(fd, txt, strlen(txt));
    int e = errno;
    close(fd);
    return n == (ssize_t)strlen(txt) ? 0 : -e;
}
static int rd(const char *path, char *buf, size_t sz)
{
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    ssize_t n = read(fd, buf, sz - 1);
    close(fd);
    if (n < 0) return -1;
    buf[n] = 0;
    return (int)n;
}
static int nslink(const char *path, char *buf, size_t sz)
{
    ssize_t n = readlink(path, buf, sz - 1);
    if (n < 0) return -1;
    buf[n] = 0;
    return (int)n;
}

/* Sync helper: child tells the parent "ready" and waits for "go". */
static int p2c[2], c2p[2];
static void child_ready_wait(void) { char c = 'r'; (void)!write(c2p[1], &c, 1); (void)!read(p2c[0], &c, 1); }

static int ff_seq_child(void)
{
    char m[64]; char num[64];
    int uid = 1000, gid = 1000;
    CHECK(getuid() == 65534 && getgid() == 65534);        /* unmapped: overflow ids */
    CHECK(wr("/proc/self/gid_map", "1000 1000 1\n") == -EPERM); /* gid_map before deny */
    /* Non-identity maps are refused unless the kernel runs with idmap=test
     * (then "0 1000 1" is accepted and consumes the write-once uid_map). */
    int r = wr("/proc/self/uid_map", "0 1000 1\n");
    int knob = (r == 0);
    CHECK(knob || r == -EPERM);                                 /* non-identity */
    if (!knob) {
        CHECK(wr("/proc/self/uid_map", "0 0 1\n") == -EPERM);   /* foreign uid */
        CHECK(wr("/proc/self/setgroups", "deny") == 0);
        snprintf(num, sizeof num, "%d %d 1", uid, uid);
        CHECK(wr("/proc/self/uid_map", num) == 0);
        snprintf(num, sizeof num, "%d %d 1", gid, gid);
        CHECK(wr("/proc/self/gid_map", num) == 0);
    } else {
        CHECK(wr("/proc/self/setgroups", "deny") == 0);
        snprintf(num, sizeof num, "0 %d 1\n", gid);
        CHECK(wr("/proc/self/gid_map", num) == 0);
        uid = 0; gid = 0;
        snprintf(num, sizeof num, "0 1000 1");
    }
    CHECK(wr("/proc/self/uid_map", num) == -EPERM);             /* write once */
    CHECK(wr("/proc/self/gid_map", num) == -EPERM);
    CHECK(wr("/proc/self/setgroups", "deny") == -EPERM);        /* after gid_map */
    CHECK(getuid() == uid && geteuid() == uid && getgid() == gid && getegid() == gid);
    CHECK(setgroups(0, NULL) == -1 && errno == EPERM);
    CHECK(rd("/proc/self/setgroups", m, sizeof m) > 0 && !strncmp(m, "deny", 4));
    CHECK(rd("/proc/self/uid_map", m, sizeof m) > 0 && strstr(m, "1000") && strstr(m, " 1\n"));
    child_ready_wait();
    return 0;
}

static int t_ff_seq(void)
{
    CHECK(drop_to(1000) == 0);
    CHECK(pipe(p2c) == 0 && pipe(c2p) == 0);
    pid_t c = syscall(SYS_clone, SIGCHLD | CLONE_NEWUSER, 0, 0, 0, 0);
    if (c == 0) _exit(ff_seq_child());
    CHECK(c > 0);
    char b; (void)!read(c2p[0], &b, 1);
    char path[64], m[128];
    snprintf(path, sizeof path, "/proc/%d/uid_map", (int)c);
    CHECK(rd(path, m, sizeof m) > 0 && strstr(m, "1000          1\n"));
    snprintf(path, sizeof path, "/proc/%d/gid_map", (int)c);
    CHECK(rd(path, m, sizeof m) > 0 && strstr(m, "1000          1\n"));
    CHECK(rd("/proc/self/uid_map", m, sizeof m) > 0 && strstr(m, "4294967295"));
    CHECK(getuid() == 1000);
    (void)!write(p2c[1], "g", 1);
    int st = 0; waitpid(c, &st, 0);
    return WIFEXITED(st) && WEXITSTATUS(st) == 0 ? 0 : 1;
}
static int links_child(void)
{
    char a[64], b[64];
    CHECK(nslink("/proc/self/ns/user", a, sizeof a) > 0);
    CHECK(strncmp(a, "user:[", 6) == 0 && strcmp(a, "user:[4026531837]") != 0);
    (void)b;
    return 0;
}
static int t_ns_links(void)
{
    const char *n[] = { "user", "pid", "net", "ipc", "mnt", "uts", "cgroup" };
    char p[64], a[64], b[64]; struct stat sb;
    for (int i = 0; i < 7; i++) {
        snprintf(p, sizeof p, "/proc/self/ns/%s", n[i]);
        CHECK(access(p, F_OK) == 0);
        CHECK(nslink(p, a, sizeof a) > 0 && !strncmp(a, n[i], strlen(n[i])) && a[strlen(n[i])] == ':');
        CHECK(lstat(p, &sb) == 0 && S_ISLNK(sb.st_mode));
        CHECK(stat(p, &sb) == 0 && S_ISREG(sb.st_mode));
    }
    CHECK(nslink("/proc/self/ns/user", a, sizeof a) > 0 && !strcmp(a, "user:[4026531837]"));
    CHECK(stat("/proc/self/ns/user", &sb) == 0 && sb.st_ino == 4026531837UL);
    CHECK(nslink("/proc/self/ns/net", a, sizeof a) > 0 && !strcmp(a, "net:[4026531840]"));
    /* same ns => same text; sibling in same ns equal, child userns differs */
    pid_t c = fork();
    if (c == 0) { int r = nslink("/proc/self/ns/user", b, sizeof b) > 0 && !strcmp(b, "user:[4026531837]"); _exit(r ? 0 : 1); }
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    CHECK(newuser_child(links_child) == 0);
    c = fork();
    if (c == 0) {
        char x[64], y[64];
        if (unshare(CLONE_NEWNET | CLONE_NEWIPC) != 0) _exit(2);
        int r = nslink("/proc/self/ns/net", x, sizeof x) > 0 && strcmp(x, "net:[4026531840]") != 0
             && nslink("/proc/self/ns/ipc", y, sizeof y) > 0 && strcmp(y, "ipc:[4026531839]") != 0
             && nslink("/proc/self/ns/user", y, sizeof y) > 0 && !strcmp(y, "user:[4026531837]");
        _exit(r ? 0 : 1);
    }
    waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}
static int t_void_root_link(void)
{
    char buf[128], path[64];
    CHECK(pipe(p2c) == 0 && pipe(c2p) == 0);
    pid_t c = fork();
    if (c == 0) {
        if (chroot("/proc") != 0) _exit(2);
        char r[64]; ssize_t n = readlink("/proc/self/root", r, sizeof r);
        /* inside the void, our own root reads as "/" */
        child_ready_wait();
        _exit(n < 0 ? 3 : 0);
    }
    char x; (void)!read(c2p[0], &x, 1);
    snprintf(path, sizeof path, "/proc/%d/root", (int)c);
    ssize_t n = readlink(path, buf, sizeof buf - 1);
    int ok = n > 0;
    if (ok) { buf[n] = 0; ok = strcmp(buf, "/") != 0; }
    (void)!write(p2c[1], "g", 1);
    int st = 0; waitpid(c, &st, 0);
    CHECK(ok);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) != 2);
    return 0;
}

/* ---- net / ipc namespaces (lanes nsnet, nsipc) ---- */
#define NSALL (CLONE_NEWUSER | CLONE_NEWNET | CLONE_NEWIPC)
static int abs_addr(struct sockaddr_un *a, const char *n)
{
    memset(a, 0, sizeof *a); a->sun_family = AF_UNIX;
    memcpy(a->sun_path + 1, n, strlen(n));
    return offsetof(struct sockaddr_un, sun_path) + 1 + strlen(n);
}
static int abs_bind(const char *n)
{
    struct sockaddr_un a; int len = abs_addr(&a, n);
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (s < 0 || bind(s, (void *)&a, len) || listen(s, 4)) return -1;
    return s;
}
static int abs_connect(const char *n)
{
    struct sockaddr_un a; int len = abs_addr(&a, n);
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    return (s >= 0 && connect(s, (void *)&a, len) == 0) ? 0 : -errno;
}
static int ns_child_netns(int (*fn)(void))
{
    pid_t c = syscall(SYS_clone, SIGCHLD | NSALL, 0, 0, 0, 0);
    if (c == 0) _exit(fn());
    if (c < 0) return -1;
    int st = 0; waitpid(c, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 100;
}
static int netns_child_body(void)
{
    /* parent's name is invisible; same name binds fine here */
    CHECK(abs_connect("nsnet-par") == -ECONNREFUSED);
    int s = abs_bind("nsnet-par");
    CHECK(s >= 0);
    /* path sockets are NOT netns-scoped */
    struct sockaddr_un p; memset(&p, 0, sizeof p); p.sun_family = AF_UNIX;
    strcpy(p.sun_path, "/tmp/nsnet-path.sock");
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(connect(c, (void *)&p, sizeof p) == 0);
    /* INET: socket works, nothing routes, lo is down */
    int t = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(t >= 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(80) };
    a.sin_addr.s_addr = htonl(0x0a000001);
    CHECK(connect(t, (void *)&a, sizeof a) == -1 && errno == ENETUNREACH);
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    CHECK(connect(t, (void *)&a, sizeof a) == -1 && errno == ENETUNREACH);
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    CHECK(sendto(u, "x", 1, 0, (void *)&a, sizeof a) == -1 && errno == ENETUNREACH);
    struct sockaddr_in b = { .sin_family = AF_INET };
    b.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    CHECK(bind(t, (void *)&b, sizeof b) == -1 && errno == EADDRNOTAVAIL);
    b.sin_addr.s_addr = htonl(INADDR_ANY);
    CHECK(bind(t, (void *)&b, sizeof b) == 0);
    return 0;
}
/* netsec: a send in a child netns must be refused by the SOCKET's domain,
 * whatever sa_family user memory claims (it used to gate on sa_family). */
static int sendfam_child(void)
{
    int bad = 0;
    unsigned char pkt[8] = { 8, 0, 0, 0, 0x12, 0x34, 0, 1 };
    struct sockaddr_in a = { .sin_family = AF_UNIX, .sin_port = htons(41047) };
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    unsigned short fams[3] = { AF_UNIX, AF_INET6, 0 };
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    if (u < 0) return 1;
    for (int i = 0; i < 3; i++) {
        a.sin_family = fams[i];
        errno = 0;
        if (sendto(u, "evil", 4, 0, (void *)&a, sizeof a) != -1) bad |= 2 << i;
        else if (errno != ENETUNREACH && errno != EAFNOSUPPORT && errno != EINVAL) bad |= 2 << i;
        struct iovec iv = { "evil", 4 };
        struct msghdr m = { .msg_name = &a, .msg_namelen = sizeof a, .msg_iov = &iv, .msg_iovlen = 1 };
        if (sendmsg(u, &m, 0) != -1) bad |= 16 << i;
    }
    int ic = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    if (ic >= 0) {
        a.sin_family = AF_UNIX;
        if (sendto(ic, pkt, sizeof pkt, 0, (void *)&a, sizeof a) != -1) bad |= 128;
    }
    return bad;
}
/* ---- netlink / ifreq view inside a new netns (lane nlns) ---- */
static int nl_rt(int s, int type, int flags, unsigned idx, unsigned ifflags, unsigned change,
                 char *out, int cap)
{
    struct { struct nlmsghdr h; struct ifinfomsg i; } r;
    memset(&r, 0, sizeof r);
    r.h.nlmsg_len = sizeof r; r.h.nlmsg_type = type; r.h.nlmsg_flags = NLM_F_REQUEST | flags; r.h.nlmsg_seq = 5;
    r.i.ifi_index = idx; r.i.ifi_flags = ifflags; r.i.ifi_change = change;
    if (send(s, &r, sizeof r, 0) != (ssize_t)sizeof r) return -1;
    int tot = 0;
    for (;;) {
        ssize_t n = recv(s, out + tot, cap - tot, 0);
        if (n <= 0) return -1;
        struct nlmsghdr *h = (struct nlmsghdr *)(out + tot);
        int k = (int)n, done = 0;
        for (; NLMSG_OK(h, k); h = NLMSG_NEXT(h, k))
            if (h->nlmsg_type == NLMSG_DONE || h->nlmsg_type == NLMSG_ERROR || !(type == RTM_GETLINK || type == RTM_GETADDR) || !(h->nlmsg_flags & NLM_F_MULTI)) done = 1;
        tot += n;
        if (done) return tot;
        if (!(flags & NLM_F_DUMP)) return tot;
    }
}
static int nl_count(char *b, int n, int type, int *first_index)
{
    int c = 0;
    for (struct nlmsghdr *h = (struct nlmsghdr *)b; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n))
        if (h->nlmsg_type == type) { if (!c && first_index) *first_index = ((struct ifinfomsg *)NLMSG_DATA(h))->ifi_index; c++; }
    return c;
}
static int netns_only_lo_body(void)
{
    static char b[8192];
    int s = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (s < 0) return 2;
    int n = nl_rt(s, RTM_GETLINK, NLM_F_DUMP, 0, 0, 0, b, sizeof b), idx = 0;
    if (n <= 0 || nl_count(b, n, RTM_NEWLINK, &idx) != 1 || idx != 1) return 3;
    n = nl_rt(s, RTM_GETADDR, NLM_F_DUMP, 0, 0, 0, b, sizeof b);
    if (n <= 0 || nl_count(b, n, RTM_NEWADDR, NULL) != 0) return 4;
    if (if_nametoindex("eth0") != 0 || if_nametoindex("lo") != 1) return 5;
    struct ifreq q; memset(&q, 0, sizeof q); strcpy(q.ifr_name, "eth0");
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    if (ioctl(u, SIOCGIFFLAGS, &q) == 0 || errno != ENODEV) return 6;
    struct ifconf ic; char cb[512]; ic.ifc_len = sizeof cb; ic.ifc_buf = cb;
    if (ioctl(u, SIOCGIFCONF, &ic) != 0 || ic.ifc_len != 0) return 7;
    n = nl_rt(s, RTM_GETROUTE, NLM_F_DUMP, 0, 0, 0, b, sizeof b);
    if (n <= 0 || nl_count(b, n, RTM_NEWROUTE, NULL) != 0) return 8;
    return 0;
}
static int t_netns_sees_only_lo(void)
{
    CHECK(ns_child_netns(netns_only_lo_body) == 0);
    return 0;
}
static int netns_lo_up_body(void)
{
    static char b[8192];
    int s = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (s < 0) return 2;
    int n = nl_rt(s, RTM_NEWLINK, NLM_F_ACK, 1, IFF_UP, IFF_UP, b, sizeof b);
    struct nlmsghdr *h = (struct nlmsghdr *)b;
    if (n <= 0 || h->nlmsg_type != NLMSG_ERROR || ((struct nlmsgerr *)NLMSG_DATA(h))->error != 0) return 3;
    struct ifreq q; memset(&q, 0, sizeof q); strcpy(q.ifr_name, "lo");
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    if (ioctl(u, SIOCGIFFLAGS, &q) != 0 || !(q.ifr_flags & IFF_UP)) return 4;
    n = nl_rt(s, RTM_NEWLINK, NLM_F_ACK, 99, IFF_UP, IFF_UP, b, sizeof b);
    if (n <= 0 || ((struct nlmsgerr *)NLMSG_DATA((struct nlmsghdr *)b))->error != -ENODEV) return 5;
    return 0;
}
static int t_netns_lo_up_ack(void)
{
    CHECK(ns_child_netns(netns_lo_up_body) == 0);
    return 0;
}

/* SIOCSIFFLAGS on a netns's lo (crun brings loopback up this way) agrees with netlink. */
static int sioc_lo_body(int userns)
{
    static char b[8192];
    if (unshare((userns ? CLONE_NEWUSER : 0) | CLONE_NEWNET) != 0) return 2;
    int u = socket(AF_INET, SOCK_DGRAM, 0), s = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (u < 0 || s < 0) return 3;
    struct ifreq q; memset(&q, 0, sizeof q); strcpy(q.ifr_name, "lo");
    if (ioctl(u, SIOCGIFFLAGS, &q) != 0 || (q.ifr_flags & IFF_UP)) return 4;
    if (ioctl(u, SIOCGIFINDEX, &q) != 0 || q.ifr_ifindex != 1) return 5;
    memset(&q, 0, sizeof q); strcpy(q.ifr_name, "lo"); q.ifr_flags = IFF_UP | IFF_RUNNING;
    if (ioctl(u, SIOCSIFFLAGS, &q) != 0) return 6;
    memset(&q, 0, sizeof q); strcpy(q.ifr_name, "lo");
    if (ioctl(u, SIOCGIFFLAGS, &q) != 0 || (q.ifr_flags & (IFF_UP | IFF_RUNNING)) != (IFF_UP | IFF_RUNNING)) return 7;
    int n = nl_rt(s, RTM_GETLINK, NLM_F_DUMP, 0, 0, 0, b, sizeof b);
    int ok = 0;
    for (struct nlmsghdr *h = (struct nlmsghdr *)b; n > 0 && NLMSG_OK(h, n); h = NLMSG_NEXT(h, n))
        if (h->nlmsg_type == RTM_NEWLINK && (((struct ifinfomsg *)NLMSG_DATA(h))->ifi_flags & IFF_UP)) ok = 1;
    if (!ok) return 8;
    memset(&q, 0, sizeof q); strcpy(q.ifr_name, "lo"); q.ifr_flags = 0;
    if (ioctl(u, SIOCSIFFLAGS, &q) != 0) return 9;
    memset(&q, 0, sizeof q); strcpy(q.ifr_name, "lo");
    if (ioctl(u, SIOCGIFFLAGS, &q) != 0 || (q.ifr_flags & IFF_UP)) return 10;
    if (!userns) { /* unprivileged user: EPERM */
        if (setgid(1000) < 0 || setuid(1000) < 0) return 11;
        memset(&q, 0, sizeof q); strcpy(q.ifr_name, "lo"); q.ifr_flags = IFF_UP;
        errno = 0;
        if (ioctl(u, SIOCSIFFLAGS, &q) == 0 || errno != EPERM) return 12;
    }
    return 0;
}
static int sioc_lo_run(int userns)
{
    pid_t c = fork();
    if (c == 0) _exit(sioc_lo_body(userns));
    int st = 0; waitpid(c, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 100;
}
static int t_netns_siocsifflags_lo(void)
{
    CHECK(sioc_lo_run(0) == 0);
    return 0;
}
static int t_netns_siocsifflags_lo_userns(void)
{
    CHECK(sioc_lo_run(1) == 0);
    return 0;
}
/* ---- /proc/sys/net is per network namespace ---- */
#define PSN "/proc/sys/net/"
static int psn_rd(const char *f, char *b, int n) { char p[128]; snprintf(p, sizeof p, PSN "%s", f); return slurp(p, b, n); }
static int psn_wr(const char *f, const char *v)
{
    char p[128]; snprintf(p, sizeof p, PSN "%s", f);
    int fd = open(p, O_WRONLY | O_TRUNC);
    if (fd < 0) return -errno;
    int r = write(fd, v, strlen(v)); int e = errno;
    close(fd);
    return r < 0 ? -e : 0;
}
static int psn_is(const char *f, const char *want)
{
    char b[64]; return psn_rd(f, b, sizeof b) > 0 && strcmp(b, want) == 0;
}
static int t_procsysnet_defaults(void)
{
    CHECK(psn_is("ipv4/ip_unprivileged_port_start", "1024\n"));
    CHECK(psn_is("ipv4/ip_forward", "0\n"));
    CHECK(psn_is("ipv4/conf/all/forwarding", "0\n"));
    CHECK(psn_is("ipv4/conf/default/forwarding", "0\n"));
    CHECK(psn_is("ipv4/conf/lo/forwarding", "0\n"));
    CHECK(psn_is("ipv4/ip_local_port_range", "32768\t60999\n"));
    CHECK(psn_is("ipv6/conf/all/disable_ipv6", "0\n"));
    CHECK(psn_is("ipv6/conf/default/disable_ipv6", "0\n"));
    CHECK(psn_is("core/somaxconn", "4096\n"));
    char b[64]; CHECK(psn_rd("ipv4/ping_group_range", b, sizeof b) > 0);
    return 0;
}
/* Fresh netns of a new userns: Linux defaults, writable by its root, host untouched. */
static int psn_child_userns(void)
{
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return 2;
    if (!psn_is("ipv4/ping_group_range", "1\t0\n")) return 3;
    if (!psn_is("ipv4/ip_unprivileged_port_start", "1024\n")) return 4;
    if (psn_wr("ipv4/ping_group_range", "0 0") != 0) return 5;
    if (!psn_is("ipv4/ping_group_range", "0\t0\n")) return 6;
    if (psn_wr("ipv4/ip_unprivileged_port_start", "80") != 0) return 7;
    if (!psn_is("ipv4/ip_unprivileged_port_start", "80\n")) return 8;
    if (psn_wr("ipv4/ip_forward", "1") != 0 || !psn_is("ipv4/ip_forward", "1\n")) return 9;
    if (psn_wr("ipv6/conf/all/disable_ipv6", "1") != 0 || !psn_is("ipv6/conf/all/disable_ipv6", "1\n")) return 10;
    if (psn_wr("ipv4/ip_forward", "7") != -EINVAL) return 11;
    /* port 81 is now unprivileged for an unprivileged-uid thread of this netns */
    return 0;
}
static int t_procsysnet_netns_isolated(void)
{
    char before[64], after[64];
    CHECK(psn_rd("ipv4/ip_unprivileged_port_start", before, sizeof before) > 0);
    char pb[64]; CHECK(psn_rd("ipv4/ping_group_range", pb, sizeof pb) > 0);
    pid_t c = fork();
    if (c == 0) _exit(psn_child_userns());
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    CHECK(psn_rd("ipv4/ip_unprivileged_port_start", after, sizeof after) > 0 && strcmp(before, after) == 0);
    CHECK(psn_is("ipv4/ip_forward", "0\n") && psn_is("ipv6/conf/all/disable_ipv6", "0\n"));
    CHECK(psn_rd("ipv4/ping_group_range", after, sizeof after) > 0 && strcmp(pb, after) == 0);
    /* a second new netns starts from defaults again */
    c = fork();
    if (c == 0) { if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) _exit(2); _exit(psn_is("ipv4/ip_forward", "0\n") && psn_is("ipv4/ping_group_range", "1\t0\n") ? 0 : 3); }
    waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}
/* Root in the init ns writes and restores; init-ns writes are visible to later reads. */
static int t_procsysnet_root_write(void)
{
    char old[64]; CHECK(psn_rd("core/somaxconn", old, sizeof old) > 0);
    CHECK(psn_wr("core/somaxconn", "1234") == 0);
    CHECK(psn_is("core/somaxconn", "1234\n"));
    CHECK(psn_wr("core/somaxconn", "abc") == -EINVAL);
    CHECK(psn_wr("core/somaxconn", old) == 0 && psn_is("core/somaxconn", old));
    return 0;
}
static int t_procsysnet_unpriv_denied(void)
{
    pid_t c = fork();
    if (c == 0) {
        if (drop_to(1000) != 0) _exit(2);
        int r = psn_wr("ipv4/ip_forward", "1");
        if (r != -EACCES && r != -EPERM) _exit(3);
        /* from an unprivileged user namespace over the HOST netns: still denied */
        if (unshare(CLONE_NEWUSER) != 0) _exit(4);
        r = psn_wr("ipv4/ip_forward", "1");
        if (r != -EACCES && r != -EPERM) _exit(5);
        _exit(psn_is("ipv4/ip_forward", "0\n") ? 0 : 6);
    }
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    CHECK(psn_is("ipv4/ip_forward", "0\n"));
    return 0;
}
static int t_procsysnet_port_start(void)
{
    CHECK(psn_wr("ipv4/ip_unprivileged_port_start", "1024") == 0);
    pid_t c = fork();
    if (c == 0) {
        if (drop_to(1000) != 0) _exit(2);
        int s = socket(AF_INET, SOCK_DGRAM, 0);
        struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(81) };
        if (bind(s, (void *)&a, sizeof a) == 0 || errno != EACCES) _exit(3);
        _exit(0);
    }
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    /* lower the host threshold: unprivileged bind of 81 now works */
    CHECK(psn_wr("ipv4/ip_unprivileged_port_start", "80") == 0);
    c = fork();
    if (c == 0) {
        if (drop_to(1000) != 0) _exit(2);
        int s = socket(AF_INET, SOCK_DGRAM, 0);
        struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(81) };
        if (bind(s, (void *)&a, sizeof a) != 0) _exit(3);
        a.sin_port = htons(79); int s2 = socket(AF_INET, SOCK_DGRAM, 0);
        if (bind(s2, (void *)&a, sizeof a) == 0 || errno != EACCES) _exit(4);
        _exit(0);
    }
    waitpid(c, &st, 0);
    int ok = WIFEXITED(st) && WEXITSTATUS(st) == 0;
    CHECK(psn_wr("ipv4/ip_unprivileged_port_start", "1024") == 0);
    CHECK(ok);
    return 0;
}
static int t_procsysnet_ping_range(void)
{
    /* a new netns defaults to "1 0": an unprivileged ping socket is refused; widening it allows it */
    pid_t c = fork();
    if (c == 0) {
        if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) _exit(2);
        /* give up the namespace root's CAP_NET_RAW bypass: be a non-zero gid with no caps */
        if (psn_wr("ipv4/ping_group_range", "1 0") != 0) _exit(3);
        if (setgid(1000) < 0 || setuid(1000) < 0) {
            /* unmapped ids in a bare userns: fall back to checking the stored range only */
            _exit(psn_is("ipv4/ping_group_range", "1\t0\n") ? 0 : 5);
        }
        int s = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
        if (s >= 0 || errno != EACCES) _exit(6);
        _exit(0);
    }
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    /* init ns: unprivileged gid 1000 within the range may ping; out of range may not (init-ns range narrowed, restored) */
    char old[64]; CHECK(psn_rd("ipv4/ping_group_range", old, sizeof old) > 0);
    CHECK(psn_wr("ipv4/ping_group_range", "2000 3000") == 0);
    c = fork();
    if (c == 0) {
        if (drop_to(1000) != 0) _exit(2);
        int s = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
        _exit(s < 0 && errno == EACCES ? 0 : 3);
    }
    waitpid(c, &st, 0);
    int ok1 = WIFEXITED(st) && WEXITSTATUS(st) == 0;
    CHECK(psn_wr("ipv4/ping_group_range", "900 1100") == 0);
    c = fork();
    if (c == 0) {
        if (drop_to(1000) != 0) _exit(2);
        int s = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
        _exit(s >= 0 ? 0 : 3);
    }
    waitpid(c, &st, 0);
    int ok2 = WIFEXITED(st) && WEXITSTATUS(st) == 0;
    for (char *q = old; *q; q++) if (*q == '\t') *q = ' ';
    CHECK(psn_wr("ipv4/ping_group_range", old) == 0);
    CHECK(ok1 && ok2);
    return 0;
}
static int t_netns_sendfam(void)
{
    int h = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(41047) };
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    CHECK(h >= 0 && bind(h, (void *)&a, sizeof a) == 0);
    pid_t c = fork();
    if (c == 0) { if (unshare(CLONE_NEWNET) != 0) _exit(255); _exit(sendfam_child()); }
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    usleep(200000);
    char buf[16];
    CHECK(recv(h, buf, sizeof buf, MSG_DONTWAIT) == -1 && errno == EAGAIN);  /* host listener got nothing */
    close(h);
    return 0;
}
static int t_netns_ipcns(void)
{
    CHECK(drop_to(1000) == 0);
    int ls = abs_bind("nsnet-par");
    CHECK(ls >= 0);
    int ps = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un p; memset(&p, 0, sizeof p); p.sun_family = AF_UNIX;
    strcpy(p.sun_path, "/tmp/nsnet-path.sock"); unlink(p.sun_path);
    CHECK(bind(ps, (void *)&p, sizeof p) == 0 && listen(ps, 4) == 0);
    /* parent networking unaffected: loopback TCP listener + connect */
    int tl = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET };
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    CHECK(bind(tl, (void *)&a, sizeof a) == 0 && listen(tl, 4) == 0);
    socklen_t al = sizeof a; getsockname(tl, (void *)&a, &al);
    int tc = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(connect(tc, (void *)&a, sizeof a) == 0);
    CHECK(ns_child_netns(netns_child_body) == 0);
    /* reverse: name bound in the child is invisible to the parent */
    CHECK(abs_connect("nsnet-par") == 0);              /* still the parent's own listener */
    CHECK(abs_connect("nsnet-child-only") == -ECONNREFUSED);
    /* unshare in a plain process needs CAP_SYS_ADMIN */
    CHECK(unshare(CLONE_NEWNET) == -1 && errno == EPERM);
    CHECK(unshare(CLONE_NEWIPC) == -1 && errno == EPERM);
    unlink(p.sun_path);
    return 0;
}
static int t_netns_isolate_reverse(void)
{
    CHECK(drop_to(1000) == 0);
    int pfd[2], qfd[2]; CHECK(pipe(pfd) == 0 && pipe(qfd) == 0);
    pid_t c = syscall(SYS_clone, SIGCHLD | NSALL, 0, 0, 0, 0);
    if (c == 0) {
        int s = abs_bind("nsnet-child-only");
        char ok = s >= 0; write(pfd[1], &ok, 1);
        char b; read(qfd[0], &b, 1);
        _exit(0);
    }
    CHECK(c > 0);
    char ok = 0; CHECK(read(pfd[0], &ok, 1) == 1 && ok);
    CHECK(abs_connect("nsnet-child-only") == -ECONNREFUSED);
    write(qfd[1], "x", 1);
    int st; waitpid(c, &st, 0);
    return 0;
}

/* ---- pid namespace init semantics (lane nspidinit) ---------------------- */
#include <sys/resource.h>
#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif
#ifndef SYS_pidfd_send_signal
#define SYS_pidfd_send_signal 424
#endif
static pid_t outer_pid;
static volatile int usr1_cnt;
static void usr1_h(int s) { (void)s; usr1_cnt++; }

/* init is signalled from inside: SIGTERM/SIGKILL dropped (return 0), init lives. */
static int ib_ign(void)
{
    int p[2]; CHECK(pipe(p) == 0);
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) {
        CHECK(kill(1, SIGTERM) == 0);
        CHECK(kill(1, SIGKILL) == 0);
        char b = 'x'; (void)!write(p[1], &b, 1);
        _exit(0);
    }
    char b = 0;
    CHECK(read(p[0], &b, 1) == 1 && b == 'x');   /* we survived both signals */
    return child_ok(c);
}
static int ub_init_ignores(void) { return in_init(ib_ign); }
PCASE(init_ignores_inside, ub_init_ignores)

static int ub_init_kill_outside(void)
{
    alarm(10);
    int a[2], b[2];
    CHECK(pipe(a) == 0 && pipe(b) == 0);
    CHECK(unshare(CLONE_NEWPID) == 0);
    pid_t init = fork();
    CHECK(init >= 0);
    if (init == 0) {
        char ch;
        while (read(a[0], &ch, 1) == 1) (void)!write(b[1], "q", 1);
        _exit(0);
    }
    char ch = 'p';
    CHECK(kill(init, SIGTERM) == 0);
    CHECK(write(a[1], &ch, 1) == 1 && read(b[0], &ch, 1) == 1);  /* still alive */
    CHECK(kill(init, SIGKILL) == 0);
    int st = 0;
    CHECK(waitpid(init, &st, 0) == init && WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL);
    return 0;
}
PCASE(init_kill_outside, ub_init_kill_outside)

static int ib_handler(void)
{
    struct sigaction sa; memset(&sa, 0, sizeof sa);
    sa.sa_handler = usr1_h;
    CHECK(sigaction(SIGUSR1, &sa, 0) == 0);
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) _exit(kill(1, SIGUSR1) == 0 ? 0 : 1);
    for (int n = 0; !usr1_cnt && n < 5000; n++) usleep(1000);
    CHECK(usr1_cnt == 1);
    return child_ok(c);
}
static int ub_init_handler(void) { return in_init(ib_handler); }
PCASE(init_handler, ub_init_handler)

static int ib_orphan(void)
{
    pid_t a = fork();
    CHECK(a >= 0);
    if (a == 0) {
        pid_t b = fork();
        if (b == 0) {
            for (int n = 0; getppid() != 1 && n < 5000; n++) usleep(1000);
            _exit(getppid() == 1 ? 0 : 1);
        }
        _exit(b > 0 ? 0 : 2);
    }
    int st, got = 0, bad = 0;
    for (int i = 0; i < 2; i++) {
        pid_t r = waitpid(-1, &st, 0);
        if (r > 0) { got++; if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) bad++; }
    }
    CHECK(got == 2 && bad == 0);
    return 0;
}
static int ub_orphan(void) { return in_init(ib_orphan); }
PCASE(orphan_to_init, ub_orphan)

static int ub_init_death(void)
{
    alarm(10);
    int p[2]; CHECK(pipe(p) == 0);
    CHECK(unshare(CLONE_NEWPID) == 0);
    pid_t init = fork();
    CHECK(init >= 0);
    if (init == 0) {
        pid_t m = fork();
        if (m == 0) { for (;;) pause(); }
        usleep(100000);
        _exit(m > 0 ? 0 : 1);
    }
    close(p[1]);
    int st = 0;
    CHECK(waitpid(init, &st, 0) == init);
    char ch;
    CHECK(read(p[0], &ch, 1) == 0);   /* every member died: EOF */
    return 0;
}
PCASE(init_death_kills, ub_init_death)

/* pidsec G2: /proc/<outside pid>/{exe,cwd} are invisible inside a pid ns; /proc/1/exe is the ns init's. */
static int ub_proc_magic_ns(void)
{
    alarm(10);
    pid_t outside = getpid();
    CHECK(unshare(CLONE_NEWPID) == 0);
    pid_t init = fork();
    CHECK(init >= 0);
    if (init == 0) {
        char b[256], mine[256], path[64];
        int bad = 0;
        snprintf(path, sizeof path, "/proc/%d/exe", (int)outside);
        if (readlink(path, b, sizeof b) >= 0 || errno != ENOENT) bad |= 1;
        snprintf(path, sizeof path, "/proc/%d/cwd", (int)outside);
        if (readlink(path, b, sizeof b) >= 0 || errno != ENOENT) bad |= 2;
        ssize_t a = readlink("/proc/1/exe", b, sizeof b - 1);
        ssize_t m = readlink("/proc/self/exe", mine, sizeof mine - 1);
        if (a <= 0 || a != m || memcmp(b, mine, (size_t)a)) bad |= 4;
        _exit(bad);
    }
    CHECK(child_ok(init) == 0);
    return 0;
}
PCASE(proc_magic_pidns, ub_proc_magic_ns)

static void *leaderless_thr(void *a) { (void)a; for (;;) pause(); return 0; }
/* pidsec G3: a member whose main thread exited (thread still runs) dies with the ns init. */
static int ub_zap_leaderless(void)
{
    alarm(10);
    int p[2]; CHECK(pipe(p) == 0);
    CHECK(unshare(CLONE_NEWPID) == 0);
    pid_t init = fork();
    CHECK(init >= 0);
    if (init == 0) {
        pid_t m = fork();
        if (m == 0) {
            pthread_t t;
            if (pthread_create(&t, 0, leaderless_thr, 0)) _exit(1);
            pthread_exit(0);
        }
        usleep(300000);
        _exit(m > 0 ? 0 : 1);
    }
    close(p[1]);
    int st = 0;
    CHECK(waitpid(init, &st, 0) == init);
    char ch;
    CHECK(read(p[0], &ch, 1) == 0);
    return 0;
}
PCASE(zap_leaderless, ub_zap_leaderless)

/* pidsec G4: init exits while a member forks in a loop; no task survives the zap. */
static int ub_zap_forkrace(void)
{
    alarm(20);
    int p[2]; CHECK(pipe(p) == 0);
    CHECK(unshare(CLONE_NEWPID) == 0);
    pid_t init = fork();
    CHECK(init >= 0);
    if (init == 0) {
        for (int w = 0; w < 3; w++) {
            if (fork() == 0) {
                for (int i = 0; i < 3000; i++) {
                    pid_t c = fork();
                    if (c == 0) { for (;;) pause(); }
                    if (c < 0) break;
                }
                for (;;) pause();
            }
        }
        usleep(20000);
        _exit(0);
    }
    close(p[1]);
    int st = 0;
    CHECK(waitpid(init, &st, 0) == init);
    char ch;
    CHECK(read(p[0], &ch, 1) == 0);
    return 0;
}
PCASE(zap_forkrace, ub_zap_forkrace)

static int ib_minus1(void)
{
    struct sigaction sa; memset(&sa, 0, sizeof sa);
    sa.sa_handler = usr1_h;
    CHECK(sigaction(SIGUSR1, &sa, 0) == 0);
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) { (void)kill(-1, SIGUSR1); _exit(0); }
    CHECK(child_ok(c) == 0);
    usleep(100000);
    CHECK(usr1_cnt == 0);
    return 0;
}
static int ub_minus1(void)
{
    alarm(10);
    struct sigaction sa; memset(&sa, 0, sizeof sa);
    sa.sa_handler = usr1_h;
    CHECK(sigaction(SIGUSR1, &sa, 0) == 0);
    CHECK(unshare(CLONE_NEWPID) == 0);
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) {
        sa.sa_handler = SIG_DFL; sigaction(SIGUSR1, &sa, 0);
        _exit(ib_minus1());
    }
    CHECK(child_ok(c) == 0);
    usleep(100000);
    CHECK(usr1_cnt == 0);
    return 0;
}
PCASE(kill_minus1_scoped, ub_minus1)

static int ib_sidpgid(void)
{
    CHECK(setsid() == 1);
    CHECK(getsid(0) == 1 && getpgid(0) == 1 && getpgrp() == 1);
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) {
        if (getpgid(1) != 1 || getsid(1) != 1) _exit(1);
        if (setpgid(0, 0) != 0 || getpgrp() != getpid()) _exit(2);
        _exit(0);
    }
    return child_ok(c);
}
static int ub_sidpgid(void) { return in_init(ib_sidpgid); }
PCASE(sid_pgid, ub_sidpgid)

static int ib_byid(void)
{
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) {
        struct rlimit r; unsigned long m[16];
        if (prlimit(1, RLIMIT_NOFILE, 0, &r) != 0) { printf("  byid step %d errno %d\n", 1, errno); _exit(1); }
        errno = 0; if (getpriority(PRIO_PROCESS, 1) == -1 && errno) { printf("  byid step %d errno %d\n", 2, errno); _exit(2); }
        if (sched_getaffinity(1, sizeof m, (cpu_set_t *)m) != 0) { printf("  byid step %d errno %d\n", 3, errno); _exit(3); }
        int fd = syscall(SYS_pidfd_open, 1, 0);
        if (fd < 0) { printf("  byid step %d errno %d\n", 4, errno); _exit(4); }
        if (syscall(SYS_pidfd_send_signal, fd, 0, 0, 0) != 0) { printf("  byid step %d errno %d\n", 5, errno); _exit(5); }
        errno = 0; if (prlimit(outer_pid, RLIMIT_NOFILE, 0, &r) != -1 || errno != ESRCH) { printf("  byid step %d errno %d\n", 6, errno); _exit(6); }
        errno = 0; getpriority(PRIO_PROCESS, outer_pid); if (errno != ESRCH) { printf("  byid step %d errno %d\n", 7, errno); _exit(7); }
        errno = 0; if (sched_getaffinity(outer_pid, sizeof m, (cpu_set_t *)m) != -1 || errno != ESRCH) { printf("  byid step %d errno %d\n", 8, errno); _exit(8); }
        errno = 0; if (syscall(SYS_pidfd_open, outer_pid, 0) != -1 || errno != ESRCH) { printf("  byid step %d errno %d\n", 9, errno); _exit(9); }
        _exit(0);
    }
    return child_ok(c);
}
static int ub_byid(void) { outer_pid = getpid(); return in_init(ib_byid); }
PCASE(byid_calls, ub_byid)

static int ucred_pid(int fd)
{
    struct ucred u; socklen_t l = sizeof u;
    if (getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &u, &l) != 0) return -1;
    return u.pid;
}
static int sp_out[2];
static int ib_peercred(void)
{
    int sp[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sp) == 0);
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) _exit(ucred_pid(sp[1]) == 1 && ucred_pid(sp_out[1]) == 0 ? 0 : 1);
    return child_ok(c);
}
static int ub_peercred(void)
{
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sp_out) == 0);
    return in_init(ib_peercred);
}
PCASE(peercred, ub_peercred)


/* ---- nsids: id-changing syscalls and id translation inside a userns ---- */
#include <sys/stat.h>
#include <sys/socket.h>
#include <signal.h>
#define NSF "/tmp/nstest-ids-file"

static int setid_attempts(int uid_before)
{
    gid_t gl[1] = { 0 };
    CHECK(setuid(0) == -1 && setuid(1001) == -1);
    CHECK(setreuid(1001, 1001) == -1 && setreuid(0, 0) == -1);
    CHECK(setresuid(1001, 1001, 1001) == -1 && setresuid(0, 0, 0) == -1);
    syscall(SYS_setfsuid, 1001); syscall(SYS_setfsgid, 1001);
    CHECK(setgid(0) == -1 && setgid(1001) == -1);
    CHECK(setregid(0, 0) == -1 && setresgid(0, 0, 0) == -1);
    CHECK(setgroups(1, gl) == -1);
    CHECK((int)getuid() == uid_before && (int)geteuid() == uid_before);
    return 0;
}
static int setid_nomap_child(void) { return setid_attempts(65534); }
static int setid_map_child(void)
{
    CHECK(wr("/proc/self/setgroups", "deny") == 0);
    CHECK(wr("/proc/self/uid_map", "1000 1000 1") == 0);
    CHECK(wr("/proc/self/gid_map", "1000 1000 1") == 0);
    return setid_attempts(1000);
}
static int t_setid_guard(void)
{
    CHECK(drop_to(1000) == 0);
    CHECK(newuser_child(setid_nomap_child) == 0);
    CHECK(newuser_child(setid_map_child) == 0);
    CHECK((int)getuid() == 1000);
    return 0;
}
/* host root in a userns whose only mapped id is 0: other ids are EINVAL */
static int root_einval_child(void)
{
    CHECK(wr("/proc/self/setgroups", "deny") == 0);
    /* a map covering kuid 0 is refused until lane idroot0 (design-idmap rule 6) */
    int r0 = wr("/proc/self/uid_map", "0 0 1");
    if (r0 == -EPERM) { printf("  SKIP root_einval: kuid-0 maps refused\n"); return 0; }
    CHECK(r0 == 0);
    CHECK(wr("/proc/self/gid_map", "0 0 1") == 0);
    errno = 0; CHECK(setuid(1000) == -1 && errno == EINVAL);
    errno = 0; CHECK(setresuid(1000, -1, -1) == -1 && errno == EINVAL);
    errno = 0; CHECK(setreuid(-1, 1000) == -1 && errno == EINVAL);
    errno = 0; CHECK(setgid(1000) == -1 && errno == EINVAL);
    errno = 0; CHECK(setresgid(-1, 1000, -1) == -1 && errno == EINVAL);
    errno = 0; CHECK(setregid(1000, -1) == -1 && errno == EINVAL);
    CHECK(setuid(0) == 0 && setgid(0) == 0);       /* mapped id still fine */
    CHECK(getuid() == 0 && getgid() == 0);
    return 0;
}
static int t_root_einval(void) { return newuser_child(root_einval_child); }

static volatile int got_siuid = -1;
static void sigh(int s, siginfo_t *si, void *u) { (void)s; (void)u; got_siuid = (int)si->si_uid; }
static int tr_probe(int want_u, int want_g)
{
    struct stat st; struct ucred uc; socklen_t l = sizeof uc; int sv[2];
    char b[1024];
    CHECK(stat(NSF, &st) == 0 && (int)st.st_uid == want_u && (int)st.st_gid == want_g);
    CHECK(lstat(NSF, &st) == 0 && (int)st.st_uid == want_u);
    int fd = open(NSF, O_RDONLY); CHECK(fd >= 0);
    CHECK(fstat(fd, &st) == 0 && (int)st.st_uid == want_u && (int)st.st_gid == want_g);
    close(fd);
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0);
    CHECK(getsockopt(sv[0], SOL_SOCKET, SO_PEERCRED, &uc, &l) == 0);
    CHECK((int)uc.uid == want_u && (int)uc.gid == want_g);
    int n = rd("/proc/self/status", b, sizeof b);
    char *u = n > 0 ? strstr(b, "Uid:") : 0;
    CHECK(u != 0);
    int a, bb, c, d; CHECK(sscanf(u, "Uid:\t%d\t%d\t%d\t%d", &a, &bb, &c, &d) == 4 && a == want_u && bb == want_u);
    char *g = strstr(b, "\nGid:"); CHECK(g != 0); g++;
    CHECK(sscanf(g, "Gid:\t%d\t%d\t%d\t%d", &a, &bb, &c, &d) == 4 && a == want_g);
    struct sigaction sa; memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = sigh; sa.sa_flags = SA_SIGINFO; sigaction(SIGUSR1, &sa, 0);
    got_siuid = -1; kill(getpid(), SIGUSR1);
    CHECK(got_siuid == want_u);
    return 0;
}
static int tr_child(void)
{
    CHECK(tr_probe(65534, 65534) == 0);          /* unmapped: overflow ids */
    CHECK(wr("/proc/self/setgroups", "deny") == 0);
    CHECK(wr("/proc/self/uid_map", "1000 1000 1") == 0);
    CHECK(wr("/proc/self/gid_map", "1000 1000 1") == 0);
    CHECK(tr_probe(1000, 1000) == 0);
    CHECK(chown(NSF, 1000, 1000) == 0 && chown(NSF, -1, -1) == 0);
    errno = 0; CHECK(chown(NSF, 1001, -1) == -1 && errno == EINVAL);
    errno = 0; CHECK(chown(NSF, -1, 1001) == -1 && errno == EINVAL);
    int fd = open(NSF, O_RDONLY);
    errno = 0; CHECK(fchown(fd, 0, -1) == -1 && errno == EINVAL);
    errno = 0; CHECK(fchownat(AT_FDCWD, NSF, 1001, -1, 0) == -1 && errno == EINVAL);
    close(fd);
    return 0;
}
static int t_id_translate(void)
{
    int fd = open(NSF, O_WRONLY | O_CREAT | O_TRUNC, 0666); CHECK(fd >= 0); close(fd);
    CHECK(chown(NSF, 1000, 1000) == 0 && chmod(NSF, 0666) == 0);
    CHECK(drop_to(1000) == 0);
    int r = newuser_child(tr_child);
    return r;
}

/* ---- nsnet2: netns captured at socket() ---- */
static int inet_listener(unsigned port, int *fdout)
{
    int one = 1, l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port) };
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    if (bind(l, (void *)&a, sizeof a) || listen(l, 4)) return -errno;
    *fdout = l; return 0;
}
static int inet_connect_fd(int s, unsigned port)
{
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port) };
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return connect(s, (void *)&a, sizeof a) == 0 ? 0 : -errno;
}
static int sockcap_child_inherited(void)
{
    /* the fd was created by the parent: parent netns, may reach the host listener */
    extern int g_cap_sock;
    CHECK(inet_connect_fd(g_cap_sock, 41011) == 0);
    int fresh = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(inet_connect_fd(fresh, 41011) == -ENETUNREACH);
    return 0;
}
int g_cap_sock;
static int t_sock_netns_inherited(void)
{
    CHECK(drop_to(1000) == 0);
    int l; CHECK(inet_listener(41011, &l) == 0);
    g_cap_sock = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(ns_child_netns(sockcap_child_inherited) == 0);
    return 0;
}
static int t_sock_netns_scm(void)
{
    CHECK(drop_to(1000) == 0);
    int l; CHECK(inet_listener(41012, &l) == 0);
    int sv[2]; CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0);
    pid_t c = syscall(SYS_clone, SIGCHLD | NSALL, 0, 0, 0, 0);
    if (c == 0) {
        char b; struct iovec iv = { &b, 1 };
        char cb[CMSG_SPACE(sizeof(int))];
        struct msghdr m = { .msg_iov = &iv, .msg_iovlen = 1, .msg_control = cb, .msg_controllen = sizeof cb };
        if (recvmsg(sv[1], &m, 0) != 1) _exit(1);
        struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
        if (!cm || cm->cmsg_type != SCM_RIGHTS) _exit(2);
        int fd; memcpy(&fd, CMSG_DATA(cm), sizeof fd);
        if (inet_connect_fd(fd, 41012) != 0) _exit(3);
        int fresh = socket(AF_INET, SOCK_STREAM, 0);
        _exit(inet_connect_fd(fresh, 41012) == -ENETUNREACH ? 0 : 4);
    }
    CHECK(c > 0);
    int s = socket(AF_INET, SOCK_STREAM, 0);
    char b = 'x'; struct iovec iv = { &b, 1 };
    char cb[CMSG_SPACE(sizeof(int))]; memset(cb, 0, sizeof cb);
    struct msghdr m = { .msg_iov = &iv, .msg_iovlen = 1, .msg_control = cb, .msg_controllen = sizeof cb };
    struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
    cm->cmsg_level = SOL_SOCKET; cm->cmsg_type = SCM_RIGHTS; cm->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cm), &s, sizeof s);
    CHECK(sendmsg(sv[0], &m, 0) == 1);
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}

/* A socket keeps its network namespace alive (counted ref): once the creating
 * tasks are gone the id must not be reused by a new namespace that would then
 * inherit the old abstract listeners. */
static int nsr_send_fd(int sv, int fd)
{
    char b = 'x'; struct iovec iv = { &b, 1 };
    char cb[CMSG_SPACE(sizeof(int))]; memset(cb, 0, sizeof cb);
    struct msghdr m = { .msg_iov = &iv, .msg_iovlen = 1, .msg_control = cb, .msg_controllen = sizeof cb };
    struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
    cm->cmsg_level = SOL_SOCKET; cm->cmsg_type = SCM_RIGHTS; cm->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cm), &fd, sizeof fd);
    return sendmsg(sv, &m, 0) == 1 ? 0 : -1;
}
static int nsr_recv_fd(int sv)
{
    char b; struct iovec iv = { &b, 1 };
    char cb[CMSG_SPACE(sizeof(int))];
    struct msghdr m = { .msg_iov = &iv, .msg_iovlen = 1, .msg_control = cb, .msg_controllen = sizeof cb };
    if (recvmsg(sv, &m, 0) != 1) return -1;
    struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
    if (!cm || cm->cmsg_type != SCM_RIGHTS) return -1;
    int fd; memcpy(&fd, CMSG_DATA(cm), sizeof fd);
    return fd;
}
static int probe_child(void) { return abs_connect("nssockref-x") == -ECONNREFUSED ? 0 : 1; }
static int t_sock_ns_reuse(void)
{
    CHECK(drop_to(1000) == 0);
    int sv[2]; CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0);
    pid_t c = syscall(SYS_clone, SIGCHLD | NSALL, 0, 0, 0, 0);
    if (c == 0) {
        int s = abs_bind("nssockref-x");
        if (s < 0 || nsr_send_fd(sv[0], s)) _exit(1);
        _exit(0);
    }
    CHECK(c > 0);
    int fd = nsr_recv_fd(sv[1]);
    CHECK(fd >= 0);
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    /* the creator is gone; only the passed fd keeps its netns alive */
    for (int i = 0; i < 40; i++) {
        pid_t p = syscall(SYS_clone, SIGCHLD | ((i & 1) ? NSALL : (CLONE_NEWUSER | CLONE_NEWNET)), 0, 0, 0, 0);
        if (p == 0) _exit(probe_child());
        CHECK(p > 0);
        st = 0; waitpid(p, &st, 0);
        if (!(WIFEXITED(st) && WEXITSTATUS(st) == 0)) { printf("  stale abstract name seen by new ns #%d\n", i); return 1; }
        p = syscall(SYS_clone, SIGCHLD | CLONE_NEWUSER, 0, 0, 0, 0);
        if (p == 0) _exit(probe_child());
        CHECK(p > 0);
        st = 0; waitpid(p, &st, 0);
        CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    }
    /* the original socket is still a working listener */
    int fl = fcntl(fd, F_GETFL); fcntl(fd, F_SETFL, fl | O_NONBLOCK);
    errno = 0; CHECK(accept(fd, 0, 0) == -1 && errno == EAGAIN);
    struct sockaddr_un a; socklen_t al = sizeof a;
    CHECK(getsockname(fd, (void *)&a, &al) == 0 && al > 2 && a.sun_path[0] == 0);
    return 0;
}
static int leak_child(void)
{
    for (int i = 0; i < 4; i++) {
        int s = socket(AF_UNIX, SOCK_STREAM, 0), u = socket(AF_INET, SOCK_DGRAM, 0);
        if (s < 0 || u < 0) return 1;
        close(s); close(u);
    }
    int sv[2]; if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) return 2;
    int l = abs_bind("nssockref-leak"); if (l < 0) return 3;
    return 0; /* l, sv left open at exit */
}
static int t_sock_ns_leak(void)
{
    CHECK(drop_to(1000) == 0);
    for (int i = 0; i < 200; i++) {   /* 200 x 11 sockets > 2000, ns table is 256 */
        pid_t p = syscall(SYS_clone, SIGCHLD | NSALL, 0, 0, 0, 0);
        if (p == 0) _exit(leak_child());
        if (p < 0) { printf("  clone failed at %d (errno %d): ns table exhausted\n", i, errno); return 1; }
        int st = 0; waitpid(p, &st, 0);
        if (!(WIFEXITED(st) && WEXITSTATUS(st) == 0)) { printf("  child %d status %d\n", i, st); return 1; }
    }
    return 0;
}
static int t_sock_before_unshare(void)
{
    CHECK(drop_to(1000) == 0);
    int l; CHECK(inet_listener(41013, &l) == 0);
    pid_t c = fork();
    if (c == 0) {
        int old = socket(AF_INET, SOCK_STREAM, 0);
        if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) _exit(1);
        if (inet_connect_fd(old, 41013) != 0) _exit(2);
        int fresh = socket(AF_INET, SOCK_STREAM, 0);
        _exit(inet_connect_fd(fresh, 41013) == -ENETUNREACH ? 0 : 3);
    }
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}

/* ---- nsnet2: per-netns port space ---- */
static int wild_listener(unsigned port, int *fdout)
{
    int one = 1, l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port) };
    a.sin_addr.s_addr = htonl(INADDR_ANY);
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    if (bind(l, (void *)&a, sizeof a)) return -errno;
    if (listen(l, 4)) return -errno;
    *fdout = l; return 0;
}
static int t_netns_portspace(void)
{
    CHECK(drop_to(1000) == 0);
    int hp; CHECK(wild_listener(41021, &hp) == 0);       /* host holds P */
    int pfd[2], qfd[2]; CHECK(pipe(pfd) == 0 && pipe(qfd) == 0);
    pid_t c = syscall(SYS_clone, SIGCHLD | NSALL, 0, 0, 0, 0);
    if (c == 0) {
        int a, b;
        char ok = wild_listener(41021, &a) == 0 && wild_listener(41022, &b) == 0;
        write(pfd[1], &ok, 1);
        char x; read(qfd[0], &x, 1);
        _exit(0);
    }
    CHECK(c > 0);
    char ok = 0; CHECK(read(pfd[0], &ok, 1) == 1 && ok);  /* child bound P too, and Q */
    int s = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(inet_connect_fd(s, 41022) == -ECONNREFUSED);    /* child's listener unreachable */
    int h2; CHECK(wild_listener(41022, &h2) == 0);        /* and Q is free on the host */
    write(qfd[1], "x", 1);
    int st; waitpid(c, &st, 0);
    return 0;
}

static int t_netns_udp_bind(void)
{
    CHECK(drop_to(1000) == 0);
    int pfd[2], qfd[2]; CHECK(pipe(pfd) == 0 && pipe(qfd) == 0);
    pid_t c = syscall(SYS_clone, SIGCHLD | NSALL, 0, 0, 0, 0);
    if (c == 0) {
        struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(41031) };
        int u = socket(AF_INET, SOCK_DGRAM, 0);
        struct timeval tv = { 0, 300000 };
        setsockopt(u, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
        char ok = bind(u, (void *)&a, sizeof a) == 0;
        write(pfd[1], &ok, 1);
        char x; read(qfd[0], &x, 1);               /* host has sent */
        char buf[8];
        ssize_t r = recv(u, buf, sizeof buf, 0);
        char res = (r < 0 && (errno == EAGAIN || errno == EWOULDBLOCK));
        write(pfd[1], &res, 1);
        _exit(0);
    }
    CHECK(c > 0);
    char ok = 0; CHECK(read(pfd[0], &ok, 1) == 1 && ok);
    int h = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in d = { .sin_family = AF_INET, .sin_port = htons(41031) };
    d.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    sendto(h, "hi", 2, 0, (void *)&d, sizeof d);
    write(qfd[1], "x", 1);
    char res = 0; CHECK(read(pfd[0], &res, 1) == 1 && res);   /* child got nothing */
    int hb = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(41031) };
    CHECK(bind(hb, (void *)&a, sizeof a) == 0);               /* P free on the host */
    int st; waitpid(c, &st, 0);
    return 0;
}

/* ---- nsmapsec: map-file confused deputy, write race, chrooted NEWUSER, fdinfo ---- */
static pid_t spawn_userns(int *go_w, int (*after)(void))
{
    int go[2];
    if (pipe(go)) return -1;
    pid_t c = syscall(SYS_clone, SIGCHLD | CLONE_NEWUSER, 0, 0, 0, 0);
    if (c == 0) { char b; close(go[1]); (void)!read(go[0], &b, 1); _exit(after ? after() : 0); }
    close(go[0]);
    *go_w = go[1];
    return c;
}
static int deputy_child_check(void)
{
    CHECK(getuid() == 65534);
    CHECK(setuid(0) != 0);
    CHECK(geteuid() != 0);
    return 0;
}
static void send_fd(int sk, int fd)
{
    char b = 'x'; struct iovec iov = { &b, 1 };
    union { struct cmsghdr h; char buf[CMSG_SPACE(sizeof(int))]; } u;
    struct msghdr m; memset(&m, 0, sizeof m); memset(&u, 0, sizeof u);
    m.msg_iov = &iov; m.msg_iovlen = 1; m.msg_control = u.buf; m.msg_controllen = sizeof u.buf;
    struct cmsghdr *h = CMSG_FIRSTHDR(&m);
    h->cmsg_level = SOL_SOCKET; h->cmsg_type = SCM_RIGHTS; h->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(h), &fd, sizeof fd);
    (void)!sendmsg(sk, &m, 0);
}
static int recv_fd(int sk)
{
    char b; struct iovec iov = { &b, 1 };
    union { struct cmsghdr h; char buf[CMSG_SPACE(sizeof(int))]; } u;
    struct msghdr m; memset(&m, 0, sizeof m);
    m.msg_iov = &iov; m.msg_iovlen = 1; m.msg_control = u.buf; m.msg_controllen = sizeof u.buf;
    if (recvmsg(sk, &m, 0) <= 0) return -1;
    struct cmsghdr *h = CMSG_FIRSTHDR(&m);
    if (!h || h->cmsg_type != SCM_RIGHTS) return -1;
    int fd; memcpy(&fd, CMSG_DATA(h), sizeof fd);
    return fd;
}
/* Attacker (uid 1000) opens the child's uid_map and passes the fd to a root
 * helper that writes "0 0 1". Must not map host root. */
static int t_map_deputy(void)
{
    int sp[2], res[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sp) == 0 && pipe(res) == 0);
    pid_t h = fork();
    if (h == 0) {   /* root helper */
        close(sp[0]); close(res[0]);
        int fd = recv_fd(sp[1]);
        int e = 0;
        if (fd >= 0) { ssize_t n = write(fd, "0 0 1", 5); e = n == 5 ? 0 : errno; }
        else e = 255;
        (void)!write(res[1], &e, sizeof e);
        _exit(0);
    }
    pid_t a = fork();
    if (a == 0) {   /* attacker */
        close(sp[1]); close(res[1]);
        if (drop_to(1000)) _exit(2);
        int go;
        pid_t c = spawn_userns(&go, deputy_child_check);
        if (c < 0) _exit(3);
        char p[64]; snprintf(p, sizeof p, "/proc/%d/uid_map", (int)c);
        int fd = open(p, O_WRONLY);
        int opened = fd >= 0;
        if (opened) send_fd(sp[0], fd); else close(sp[0]);
        int e = 0;
        if (read(res[0], &e, sizeof e) != (ssize_t)sizeof e) e = 254;
        int blocked = !opened || (e != 0);
        (void)!write(go, "g", 1);
        int st = 0; waitpid(c, &st, 0);
        printf("  deputy: opened=%d helper_errno=%d child=%d\n", opened, e, WIFEXITED(st) ? WEXITSTATUS(st) : 100);
        _exit(blocked && WIFEXITED(st) && WEXITSTATUS(st) == 0 ? 0 : 1);
    }
    close(sp[0]); close(sp[1]);
    int st = 0; waitpid(a, &st, 0); waitpid(h, NULL, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 100;
}

struct wrarg { const char *p, *txt; int r; pthread_barrier_t *b; };
static void *wr_thr(void *v)
{
    struct wrarg *w = v;
    int fd = open(w->p, O_WRONLY);
    pthread_barrier_wait(w->b);
    if (fd < 0) { w->r = -1; return NULL; }
    w->r = write(fd, w->txt, strlen(w->txt)) == (ssize_t)strlen(w->txt) ? 0 : -1;
    close(fd);
    return NULL;
}
static int t_map_race(void)
{
    for (int it = 0; it < 20; it++) {
        int go; pid_t c = spawn_userns(&go, NULL);
        CHECK(c > 0);
        char p[64]; snprintf(p, sizeof p, "/proc/%d/uid_map", (int)c);
        pthread_barrier_t b; pthread_barrier_init(&b, NULL, 2);
        struct wrarg a = { p, "1000 1000 1\n", 99, &b }, d = { p, "2000 2000 5\n", 99, &b };
        pthread_t t1, t2;
        pthread_create(&t1, NULL, wr_thr, &a); pthread_create(&t2, NULL, wr_thr, &d);
        pthread_join(t1, NULL); pthread_join(t2, NULL);
        char buf[256]; int n = rd(p, buf, sizeof buf);
        int ok = (a.r == 0) != (d.r == 0) && n > 0;
        if (ok) {
            const char *want = a.r == 0 ? "1000 1000 1" : "2000 2000 5";
            unsigned x, y, z; ok = sscanf(buf, "%u %u %u", &x, &y, &z) == 3 && strchr(buf, '\n') == buf + strlen(buf) - 1;
            char exp[64]; if (ok) { snprintf(exp, sizeof exp, "%u %u %u", x, y, z); ok = !strcmp(exp, want); }
        }
        (void)!write(go, "g", 1); waitpid(c, NULL, 0);
        if (!ok) { printf("  race iter %d: a=%d d=%d map=[%s]\n", it, a.r, d.r, buf); return 1; }
    }
    return 0;
}

static int t_chroot_newuser(void)
{
    CHECK(chroot("/tmp") == 0);
    CHECK(chdir("/") == 0);
    errno = 0; CHECK(unshare(CLONE_NEWUSER) == -1 && errno == EPERM);
    errno = 0; long r = syscall(SYS_clone, SIGCHLD | CLONE_NEWUSER, 0, 0, 0, 0);
    if (r == 0) _exit(0);
    CHECK(r == -1 && errno == EPERM);
    return 0;
}

static int t_fdinfo_other_uid(void)
{
    int fd = open("/dev/null", O_RDONLY);
    CHECK(fd >= 0);
    char p[64]; snprintf(p, sizeof p, "/proc/%d/fdinfo/%d", (int)getpid(), fd);
    int own = open(p, O_RDONLY);
    CHECK(own >= 0);
    pid_t c = fork();
    if (c == 0) {
        if (drop_to(1000)) _exit(2);
        errno = 0; int f = open(p, O_RDONLY);
        _exit(f < 0 && (errno == EACCES || errno == EPERM) ? 0 : 1);
    }
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}

/* ---- lane pidmisc: nice / SIGEV_THREAD_ID / tty pgrp translation -------- */
#include <termios.h>
#include <sys/ioctl.h>
static int ib_prio(void)
{
    CHECK(setpriority(PRIO_USER, 0, 19) == 0);
    errno = 0;
    CHECK(getpriority(PRIO_USER, 0) == 19 || errno == 0);
    return 0;
}
static int ub_prio(void)
{
    CHECK(setpriority(PRIO_PROCESS, 0, 5) == 0);
    int r = in_init(ib_prio);
    CHECK(r == 0);
    errno = 0;
    int n = getpriority(PRIO_PROCESS, 0);
    CHECK(errno == 0 && n == 5);          /* outside process untouched */
    return 0;
}
PCASE(prio_user_scoped, ub_prio)

struct pm_sigev { long val; int signo; int notify; int tid; char pad[44]; };
static int ib_sigev_tid(void)
{
    struct pm_sigev sev; memset(&sev, 0, sizeof sev);
    sev.signo = SIGUSR1; sev.notify = 4 /* SIGEV_THREAD_ID */; sev.tid = (int)syscall(SYS_gettid);
    CHECK(sev.tid == 1);
    int id = -1;
    CHECK(syscall(SYS_timer_create, CLOCK_MONOTONIC, &sev, &id) == 0);
    syscall(SYS_timer_delete, id);
    sev.tid = 99999;
    errno = 0;
    CHECK(syscall(SYS_timer_create, CLOCK_MONOTONIC, &sev, &id) == -1 && errno == EINVAL);
    return 0;
}
static int ub_sigev_tid(void) { return in_init(ib_sigev_tid); }
PCASE(timer_thread_id, ub_sigev_tid)

static int ib_ttypg(void)
{
    CHECK(setsid() == 1);
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    CHECK(m >= 0 && grantpt(m) == 0 && unlockpt(m) == 0);
    int s = open(ptsname(m), O_RDWR | O_NOCTTY);
    CHECK(s >= 0);
    CHECK(ioctl(s, TIOCSCTTY, 0) == 0);
    int sid = 0;
    CHECK(ioctl(s, TIOCGSID, &sid) == 0 && sid == 1);
    CHECK(tcsetpgrp(s, getpgrp()) == 0);
    CHECK(tcgetpgrp(s) == 1);
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) { setpgid(0, 0); pause(); _exit(0); }
    CHECK(setpgid(c, c) == 0 || errno == EACCES);
    CHECK(tcsetpgrp(s, c) == 0);
    CHECK(tcgetpgrp(s) == c);
    CHECK(tcsetpgrp(s, 1) == 0);
    kill(c, SIGKILL); waitpid(c, NULL, 0);
    return 0;
}
static int ub_ttypg(void) { return in_init(ib_ttypg); }
PCASE(tty_pgrp_view, ub_ttypg)

/* ---- SCM_CREDENTIALS / SO_PASSCRED translation (lane credxlate) ---- */
#include <sys/prctl.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <time.h>

static int cred_send(int s, const struct ucred *uc)
{
    char d = 'x', cb[CMSG_SPACE(sizeof(struct ucred))];
    struct iovec iov = { &d, 1 };
    struct msghdr m; memset(&m, 0, sizeof m);
    m.msg_iov = &iov; m.msg_iovlen = 1;
    if (uc) {
        memset(cb, 0, sizeof cb);
        m.msg_control = cb; m.msg_controllen = sizeof cb;
        struct cmsghdr *c = CMSG_FIRSTHDR(&m);
        c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_CREDENTIALS;
        c->cmsg_len = CMSG_LEN(sizeof *uc);
        memcpy(CMSG_DATA(c), uc, sizeof *uc);
    }
    return sendmsg(s, &m, 0) == 1 ? 0 : -1;
}
/* 0 = got a byte and an SCM_CREDENTIALS cmsg (stored in *out). */
static int cred_recv(int s, struct ucred *out)
{
    char d, cb[64];
    struct iovec iov = { &d, 1 };
    struct msghdr m; memset(&m, 0, sizeof m);
    m.msg_iov = &iov; m.msg_iovlen = 1; m.msg_control = cb; m.msg_controllen = sizeof cb;
    if (recvmsg(s, &m, 0) != 1) return -1;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c))
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_CREDENTIALS) {
            memcpy(out, CMSG_DATA(c), sizeof *out);
            return 0;
        }
    return -2;
}
static int set_passcred(int s) { int on = 1; return setsockopt(s, SOL_SOCKET, SO_PASSCRED, &on, sizeof on); }

static int map_wr(const char *f, const char *t)
{
    int fd = open(f, O_WRONLY);
    if (fd < 0) return -1;
    int r = write(fd, t, strlen(t)) == (ssize_t)strlen(t) ? 0 : -1;
    close(fd);
    return r;
}
/* Map the caller's own uid/gid (1000) identity in its fresh userns. */
static int map_self(void)
{
    CHECK(map_wr("/proc/self/setgroups", "deny") == 0);
    CHECK(map_wr("/proc/self/uid_map", "1000 1000 1\n") == 0);
    CHECK(map_wr("/proc/self/gid_map", "1000 1000 1\n") == 0);
    return 0;
}

static int sc_types[2] = { SOCK_DGRAM, SOCK_STREAM };

/* Parent outside the pidns sends to a child inside: pid 0, uid 1000. */
static int sc_in_one(int ty)
{
    {
        int t = ty;
        CHECK(unshare(CLONE_NEWPID) == 0);
        int sp[2], rd[2];
        CHECK(socketpair(AF_UNIX, sc_types[t], 0, sp) == 0 && pipe(rd) == 0);
        pid_t c = fork();
        CHECK(c >= 0);
        if (c == 0) {
            if (set_passcred(sp[1])) _exit(2);
            if (write(rd[1], "x", 1) != 1) _exit(3);
            struct ucred u;
            if (cred_recv(sp[1], &u)) _exit(4);
            _exit(u.pid == 0 && u.uid == 1000 && u.gid == 1000 ? 0 : 5);
        }
        char b; CHECK(read(rd[0], &b, 1) == 1);
        CHECK(cred_send(sp[0], 0) == 0);
        CHECK(child_ok(c) == 0);
    }
    return 0;
}
static int ub_scm_in(void)
{
    alarm(20);
    CHECK(map_self() == 0);
    for (int t = 0; t < 2; t++) {
        pid_t w = fork();
        CHECK(w >= 0);
        if (w == 0) _exit(sc_in_one(t));
        CHECK(child_ok(w) == 0);
    }
    return 0;
}
PCASE(scm_cred_into_pidns, ub_scm_in)

/* Child inside the pidns sends to the outside parent: the real pid. */
static int sc_out_one(int ty)
{
    {
        int t = ty;
        CHECK(unshare(CLONE_NEWPID) == 0);
        int sp[2];
        CHECK(socketpair(AF_UNIX, sc_types[t], 0, sp) == 0);
        CHECK(set_passcred(sp[0]) == 0);
        pid_t c = fork();
        CHECK(c >= 0);
        if (c == 0) _exit(cred_send(sp[1], 0) == 0 ? 0 : 2);
        struct ucred u;
        CHECK(cred_recv(sp[0], &u) == 0);
        CHECK(u.pid == c && u.uid == 1000 && u.gid == 1000);
        CHECK(child_ok(c) == 0);
    }
    return 0;
}
static int ub_scm_out(void)
{
    alarm(20);
    CHECK(map_self() == 0);
    for (int t = 0; t < 2; t++) {
        pid_t w = fork();
        CHECK(w >= 0);
        if (w == 0) _exit(sc_out_one(t));
        CHECK(child_ok(w) == 0);
    }
    return 0;
}
PCASE(scm_cred_from_pidns, ub_scm_out)

/* Inside the pidns, an explicit claim of its own pid 1 arrives as 1. */
static int ib_scm_self(void)
{
    int sp[2];
    CHECK(socketpair(AF_UNIX, SOCK_DGRAM, 0, sp) == 0 && set_passcred(sp[0]) == 0);
    struct ucred me = { getpid(), getuid(), getgid() }, u;
    CHECK(me.pid == 1);
    CHECK(cred_send(sp[1], &me) == 0 && cred_recv(sp[0], &u) == 0);
    CHECK(u.pid == 1 && u.uid == 1000);
    return 0;
}
static int ub_scm_self(void) { CHECK(map_self() == 0); return in_init(ib_scm_self); }
PCASE(scm_cred_self_in_pidns, ub_scm_self)

/* An unmapped sender uid reads as 65534 in the receiver's userns. */
static int unm_sp[2], unm_rd[2];
static int unm_inner(void)
{
    alarm(20);
    CHECK(map_self() == 0);
    CHECK(set_passcred(unm_sp[1]) == 0);
    CHECK(write(unm_rd[1], "x", 1) == 1);
    struct ucred u;
    CHECK(cred_recv(unm_sp[1], &u) == 0);
    CHECK(u.uid == 65534 && u.gid == 65534);
    return 0;
}
static int unm_mid(void) { CHECK(drop_to(1000) == 0); return newuser_child(unm_inner); }
static int t_scm_cred_unmapped(void)
{
    CHECK(socketpair(AF_UNIX, SOCK_DGRAM, 0, unm_sp) == 0 && pipe(unm_rd) == 0);
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) _exit(unm_mid());
    char b; CHECK(read(unm_rd[0], &b, 1) == 1);
    CHECK(cred_send(unm_sp[0], 0) == 0);           /* sent as root (uid 0) */
    return child_ok(c);
}

/* Unprivileged: only its own credentials may be claimed. */
static int t_scm_cred_forge(void)
{
    int sp[2];
    CHECK(drop_to(1000) == 0);
    CHECK(socketpair(AF_UNIX, SOCK_DGRAM, 0, sp) == 0 && set_passcred(sp[0]) == 0);
    struct ucred bad_uid = { getpid(), 0, 1000 }, bad_pid = { 1, 1000, 1000 }, bad_gid = { getpid(), 1000, 0 };
    struct ucred ok = { getpid(), 1000, 1000 }, u;
    errno = 0; CHECK(cred_send(sp[1], &bad_uid) == -1 && errno == EPERM);
    errno = 0; CHECK(cred_send(sp[1], &bad_pid) == -1 && errno == EPERM);
    errno = 0; CHECK(cred_send(sp[1], &bad_gid) == -1 && errno == EPERM);
    CHECK(cred_send(sp[1], &ok) == 0 && cred_recv(sp[0], &u) == 0);
    CHECK(u.pid == getpid() && u.uid == 1000 && u.gid == 1000);
    return 0;
}

/* Seccomp SIGSYS and timer signals inside a pidns keep their fields. */
static volatile int sg_got, sg_code, sg_errno, sg_tid, sg_ov, sg_sysnr, sg_arch, sg_val;
static void sg_h(int s, siginfo_t *si, void *u)
{
    (void)u;
    unsigned char *b = (unsigned char *)si;
    sg_code = si->si_code; sg_errno = si->si_errno;
    if (s == SIGSYS) {
        memcpy((void *)&sg_sysnr, b + 24, 4); memcpy((void *)&sg_arch, b + 28, 4);
    } else {
        sg_tid = si->si_timerid; sg_ov = si->si_overrun; sg_val = si->si_value.sival_int;
    }
    sg_got = 1;
}
static int ib_sigsys(void)
{
    alarm(20);
    struct sigaction sa; memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = sg_h; sa.sa_flags = SA_SIGINFO;
    CHECK(sigaction(SIGSYS, &sa, 0) == 0);
    struct sock_filter f[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_TRAP | 0x42),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog pr = { sizeof f / sizeof f[0], f };
    CHECK(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0);
    CHECK(prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &pr) == 0);
    syscall(SYS_getppid);
    CHECK(sg_got && sg_code == 1 /* SYS_SECCOMP */ && sg_errno == 0x42);
    CHECK(sg_sysnr == SYS_getppid && sg_arch != 0);
    return 0;
}
static int ub_sigsys(void) { return in_init(ib_sigsys); }
PCASE(sigsys_in_pidns, ub_sigsys)

static int ib_timer_sig(void)
{
    alarm(20);
    struct sigaction sa; memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = sg_h; sa.sa_flags = SA_SIGINFO;
    CHECK(sigaction(SIGUSR2, &sa, 0) == 0);
    struct sigevent sev; memset(&sev, 0, sizeof sev);
    sev.sigev_notify = SIGEV_SIGNAL; sev.sigev_signo = SIGUSR2; sev.sigev_value.sival_int = 0x1234;
    timer_t tm;
    CHECK(timer_create(CLOCK_MONOTONIC, &sev, &tm) == 0);
    struct itimerspec its = { {0, 0}, {0, 10000000} };
    CHECK(timer_settime(tm, 0, &its, 0) == 0);
    for (int n = 0; !sg_got && n < 3000; n++) usleep(1000);
    CHECK(sg_got && sg_code == -2 /* SI_TIMER */ && sg_val == 0x1234 && sg_ov == 0);
    return 0;
}
static int ub_timer_sig(void) { return in_init(ib_timer_sig); }
PCASE(timer_sig_in_pidns, ub_timer_sig)

/* ---- setns(2) and openable ns files ---- */
#ifndef NS_GET_USERNS
#define NS_GET_USERNS 0xb701
#define NS_GET_PARENT 0xb702
#define NS_GET_NSTYPE 0xb703
#define NS_GET_OWNER_UID 0xb704
#endif
#ifdef __aarch64__
#define NR_SETNS 268
#else
#define NR_SETNS 308
#endif
#define NR_PIDFD_OPEN 434
static long sys_setns(int fd, int t) { return syscall(NR_SETNS, fd, t); }

/* Fork a child that unshares `fl` (as `uid` when non-zero), listens on
 * 127.0.0.1:41500 when it has a new netns, then parks until *hold_w closes. */
static pid_t ns_spawn(unsigned long fl, int uid, int *hold_w)
{
    int rdy[2], hold[2];
    if (pipe(rdy) || pipe(hold)) return -1;
    pid_t c = fork();
    if (c == 0) {
        close(rdy[0]); close(hold[1]);
        if (uid && drop_to(uid)) _exit(1);
        if (unshare(fl)) _exit(2);
        if (fl & CLONE_NEWNET) {
            int s = socket(AF_INET, SOCK_STREAM, 0);
            struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(41500) };
            if (s < 0 || bind(s, (void *)&a, sizeof a) || listen(s, 4)) _exit(3);
        }
        char b = 'r'; (void)!write(rdy[1], &b, 1);
        (void)!read(hold[0], &b, 1);
        _exit(0);
    }
    close(rdy[1]); close(hold[0]);
    char b;
    if (c < 0 || read(rdy[0], &b, 1) != 1) { close(rdy[0]); close(hold[1]); return -1; }
    close(rdy[0]);
    *hold_w = hold[1];
    return c;
}
static void ns_release(pid_t c, int hold_w) { close(hold_w); int st; waitpid(c, &st, 0); }
static int ns_open(pid_t pid, const char *nm)
{
    char p[64]; snprintf(p, sizeof p, "/proc/%d/ns/%s", pid, nm);
    return open(p, O_RDONLY);
}
static unsigned long link_ino(const char *path)
{
    char b[96]; ssize_t n = readlink(path, b, sizeof b - 1);
    if (n <= 0) return 0;
    b[n] = 0;
    char *q = strchr(b, '[');
    return q ? strtoul(q + 1, 0, 10) : 0;
}
static unsigned long ns_link_ino(pid_t pid, const char *nm)
{
    char p[64]; snprintf(p, sizeof p, "/proc/%d/ns/%s", pid, nm);
    return link_ino(p);
}
static unsigned long fd_ino(int fd) { struct stat st; return fstat(fd, &st) ? 0 : st.st_ino; }
static struct sockaddr_in ns_lo(void)
{
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(41500) };
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return a;
}

static int t_setns_net(void)
{
    int hw; pid_t c = ns_spawn(CLONE_NEWNET, 0, &hw);
    CHECK(c > 0);
    int fd = ns_open(c, "net");
    CHECK(fd >= 0);
    struct sockaddr_in a = ns_lo(), w = { .sin_family = AF_INET, .sin_port = htons(41500) };
    int s = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(bind(s, (void *)&w, sizeof w) == 0);   /* host namespace: port is free */
    close(s);
    CHECK(sys_setns(fd, CLONE_NEWNET) == 0);
    s = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(bind(s, (void *)&w, sizeof w) == -1 && errno == EADDRINUSE);   /* the child's listener */
    close(s);
    (void)a;   /* a non-initial netns has a down loopback: no connect to check */
    close(fd);
    ns_release(c, hw);
    return 0;
}

static int t_setns_stat(void)
{
    int hw; pid_t c = ns_spawn(CLONE_NEWNET | CLONE_NEWIPC, 0, &hw);
    CHECK(c > 0);
    int nf = ns_open(c, "net"), inf = ns_open(c, "ipc"), sf = open("/proc/self/ns/net", O_RDONLY);
    CHECK(nf >= 0 && inf >= 0 && sf >= 0);
    struct stat a, b, s;
    CHECK(fstat(nf, &a) == 0 && fstat(inf, &b) == 0 && fstat(sf, &s) == 0);
    CHECK(a.st_ino == ns_link_ino(c, "net") && a.st_ino != 0);
    CHECK(b.st_ino == ns_link_ino(c, "ipc"));
    CHECK(a.st_ino != s.st_ino && a.st_ino != b.st_ino);
    CHECK(a.st_dev == b.st_dev && a.st_dev == s.st_dev);
    ns_release(c, hw);
    return 0;
}

static int t_setns_eperm(void)
{
    int hw, hw2;
    pid_t c = ns_spawn(CLONE_NEWUSER | CLONE_NEWNET, 0, &hw);
    pid_t c2 = ns_spawn(CLONE_NEWNET, 0, &hw2);
    CHECK(c > 0 && c2 > 0);
    int nf = ns_open(c, "net"), uf = ns_open(c, "user"), n2 = ns_open(c2, "net");
    CHECK(nf >= 0 && uf >= 0 && n2 >= 0);
    CHECK(drop_to(1000) == 0);
    errno = 0; CHECK(sys_setns(nf, 0) == -1 && errno == EPERM);
    errno = 0; CHECK(sys_setns(uf, 0) == -1 && errno == EPERM);
    errno = 0; CHECK(sys_setns(n2, CLONE_NEWNET) == -1 && errno == EPERM);
    ns_release(c2, hw2); ns_release(c, hw);   /* c2 inherited hw: release it first */
    return 0;
}

static int t_setns_userns_owner(void)
{
    int hw; pid_t c = ns_spawn(CLONE_NEWUSER, 1000, &hw);
    CHECK(c > 0);
    int uf = ns_open(c, "user");
    CHECK(uf >= 0);
    CHECK(drop_to(1000) == 0);
    struct dat d[2];
    CHECK(cget(d) == 0 && !(d[0].eff & (1u << 21)));
    CHECK(sys_setns(uf, CLONE_NEWUSER) == 0);   /* the creator may rejoin */
    CHECK(cget(d) == 0 && (d[0].eff & (1u << 21)));
    CHECK(link_ino("/proc/self/ns/user") == ns_link_ino(c, "user"));
    errno = 0; CHECK(sys_setns(uf, CLONE_NEWUSER) == -1 && errno == EINVAL);   /* already there */
    ns_release(c, hw);
    return 0;
}

static int t_setns_userns_root(void)
{
    int hw; pid_t c = ns_spawn(CLONE_NEWUSER, 0, &hw);
    CHECK(c > 0);
    int uf = ns_open(c, "user");
    CHECK(sys_setns(uf, 0) == 0);
    CHECK(link_ino("/proc/self/ns/user") == ns_link_ino(c, "user"));
    ns_release(c, hw);
    return 0;
}

static int t_setns_userns_threaded(void)
{
    int hw; pid_t c = ns_spawn(CLONE_NEWUSER, 0, &hw);
    CHECK(c > 0);
    int uf = ns_open(c, "user");
    pthread_t th;
    CHECK(pthread_create(&th, 0, idle_thread, 0) == 0);
    errno = 0; CHECK(sys_setns(uf, CLONE_NEWUSER) == -1 && errno == EINVAL);
    ns_release(c, hw);
    return 0;
}

static int t_setns_userns_chroot(void)
{
    int hw; pid_t c = ns_spawn(CLONE_NEWUSER, 0, &hw);
    CHECK(c > 0);
    int uf = ns_open(c, "user");
    CHECK(chroot("/tmp") == 0);
    errno = 0; CHECK(sys_setns(uf, CLONE_NEWUSER) == -1 && errno == EPERM);
    ns_release(c, hw);
    return 0;
}

static int t_setns_misc(void)
{
    errno = 0; CHECK(sys_setns(-1, 0) == -1 && errno == EBADF);
    int z = open("/dev/null", O_RDONLY);
    errno = 0; CHECK(sys_setns(z, 0) == -1 && errno == EINVAL);
    int hw; pid_t c = ns_spawn(CLONE_NEWNET, 0, &hw);
    int nf = ns_open(c, "net");
    errno = 0; CHECK(sys_setns(nf, CLONE_NEWIPC) == -1 && errno == EINVAL);   /* wrong nstype */
    int m = open("/proc/self/ns/mnt", O_RDONLY), g = open("/proc/self/ns/cgroup", O_RDONLY);
    CHECK(m >= 0 && g >= 0);
    errno = 0; CHECK(sys_setns(m, CLONE_NEWNET) == -1 && errno == EINVAL);
    CHECK(sys_setns(g, CLONE_NEWCGROUP) == 0);   /* rejoining one's own cgroup namespace */
    int self = open("/proc/self/ns/net", O_RDONLY);
    CHECK(sys_setns(self, CLONE_NEWNET) == 0);
    ns_release(c, hw);
    return 0;
}

static int t_setns_pid_children(void)
{
    int hw; pid_t c = ns_spawn(CLONE_NEWPID, 0, &hw);
    CHECK(c > 0);
    int pf = ns_open(c, "pid_for_children"), p0 = open("/proc/self/ns/pid", O_RDONLY);
    CHECK(pf >= 0 && p0 >= 0);
    CHECK(sys_setns(pf, CLONE_NEWPID) == 0);
    pid_t g = fork();
    if (g == 0) {
        if (getpid() != 1) _exit(4);
        errno = 0;
        _exit(sys_setns(p0, CLONE_NEWPID) == -1 && errno == EINVAL ? 0 : 5);   /* not a descendant */
    }
    int st = 0; waitpid(g, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    CHECK(getpid() != 1);
    ns_release(c, hw);
    return 0;
}

static int t_setns_ioctl(void)
{
    int hw; pid_t c = ns_spawn(CLONE_NEWUSER | CLONE_NEWNET, 1000, &hw);
    CHECK(c > 0);
    int nf = ns_open(c, "net"), uf = ns_open(c, "user");
    CHECK(nf >= 0 && uf >= 0);
    CHECK(ioctl(nf, NS_GET_NSTYPE) == CLONE_NEWNET);
    CHECK(ioctl(uf, NS_GET_NSTYPE) == CLONE_NEWUSER);
    int o = ioctl(nf, NS_GET_USERNS);
    CHECK(o >= 0 && fd_ino(o) == ns_link_ino(c, "user"));
    int p = ioctl(uf, NS_GET_PARENT);
    CHECK(p >= 0 && fd_ino(p) == link_ino("/proc/self/ns/user"));
    errno = 0; CHECK(ioctl(nf, NS_GET_PARENT) == -1 && errno == EINVAL);
    unsigned uid = 0;
    CHECK(ioctl(uf, NS_GET_OWNER_UID, &uid) == 0 && uid == 1000);
    errno = 0; CHECK(ioctl(nf, NS_GET_OWNER_UID, &uid) == -1 && errno == EINVAL);
    int u0 = open("/proc/self/ns/user", O_RDONLY);
    errno = 0; CHECK(ioctl(u0, NS_GET_PARENT) == -1 && errno == EPERM);
    errno = 0; CHECK(ioctl(u0, NS_GET_USERNS) == -1 && errno == EPERM);
    ns_release(c, hw);
    return 0;
}

static int t_setns_pidfd(void)
{
    int hw; pid_t c = ns_spawn(CLONE_NEWNET | CLONE_NEWIPC, 0, &hw);
    CHECK(c > 0);
    int pf = syscall(NR_PIDFD_OPEN, c, 0);
    CHECK(pf >= 0);
    errno = 0; CHECK(sys_setns(pf, 0) == -1 && errno == EINVAL);
    errno = 0; CHECK(sys_setns(pf, 0x80 /* CLONE_NEWTIME: unsupported */) == -1 && errno == EINVAL);
    CHECK(link_ino("/proc/self/ns/net") != ns_link_ino(c, "net"));
    CHECK(sys_setns(pf, CLONE_NEWNET | CLONE_NEWIPC) == 0);
    CHECK(link_ino("/proc/self/ns/net") == ns_link_ino(c, "net"));
    CHECK(link_ino("/proc/self/ns/ipc") == ns_link_ino(c, "ipc"));
    ns_release(c, hw);
    return 0;
}

static int t_setns_uts(void)
{
    int rdy[2], hold[2];
    CHECK(pipe(rdy) == 0 && pipe(hold) == 0);
    pid_t c = fork();
    if (c == 0) {
        close(rdy[0]); close(hold[1]);
        if (unshare(CLONE_NEWUTS)) _exit(1);
        if (sethostname("nsjoin", 6)) _exit(2);
        char b = 'r'; (void)!write(rdy[1], &b, 1); (void)!read(hold[0], &b, 1); _exit(0);
    }
    close(rdy[1]);
    char b; CHECK(read(rdy[0], &b, 1) == 1);
    int uf = ns_open(c, "uts");
    CHECK(uf >= 0);
    char h[64];
    CHECK(gethostname(h, sizeof h) == 0 && strcmp(h, "nsjoin") != 0);
    pid_t g = fork();
    if (g == 0) { if (drop_to(1000)) _exit(1); errno = 0; _exit(sys_setns(uf, 0) == -1 && errno == EPERM ? 0 : 2); }
    int st = 0; waitpid(g, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    CHECK(sys_setns(uf, CLONE_NEWUTS) == 0);
    CHECK(gethostname(h, sizeof h) == 0 && strcmp(h, "nsjoin") == 0);
    close(hold[1]); waitpid(c, &st, 0);
    return 0;
}

static int t_setns_fd_holds(void)
{
    int hw; pid_t c = ns_spawn(CLONE_NEWNET, 0, &hw);
    CHECK(c > 0);
    int nf = ns_open(c, "net");
    unsigned long ino = fd_ino(nf);
    CHECK(ino != 0);
    ns_release(c, hw);   /* the last task is gone; only the fd keeps the namespace */
    int hw2; pid_t c2 = ns_spawn(CLONE_NEWNET, 0, &hw2);
    CHECK(c2 > 0 && ns_link_ino(c2, "net") != ino);   /* the held slot was not reused */
    CHECK(sys_setns(nf, CLONE_NEWNET) == 0);
    CHECK(link_ino("/proc/self/ns/net") == ino);
    ns_release(c2, hw2);
    return 0;
}

/* ── UTS namespaces ─────────────────────────────────────────────────────── */
static int hn_is(const char *want)
{
    char b[80] = {0};
    return gethostname(b, sizeof b) == 0 && !strcmp(b, want);
}
static int proc_hn_is(const char *path, const char *want)
{
    char b[80] = {0};
    int fd = open(path, O_RDONLY);
    if (fd < 0) return 0;
    int n = read(fd, b, sizeof b - 1); close(fd);
    if (n > 0 && b[n - 1] == '\n') b[n - 1] = 0;
    return !strcmp(b, want);
}
static int rd_link(const char *p, char *out, size_t n)
{
    ssize_t r = readlink(p, out, n - 1);
    if (r < 0) return -1;
    out[r] = 0; return 0;
}
static int t_uts_unpriv(void)
{
    char before[80], after[80], l1[80], l2[80];
    CHECK(gethostname(before, sizeof before) == 0);
    CHECK(drop_to(1000) == 0);
    CHECK(sethostname("nope", 4) == -1 && errno == EPERM);
    CHECK(unshare(CLONE_NEWUTS) == -1 && errno == EPERM);
    CHECK(rd_link("/proc/self/ns/uts", l1, sizeof l1) == 0);
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) {
        if (unshare(CLONE_NEWUSER | CLONE_NEWUTS) != 0) _exit(1);
        if (rd_link("/proc/self/ns/uts", l2, sizeof l2) != 0 || !strcmp(l1, l2)) _exit(2);
        if (sethostname("inner", 5) != 0) _exit(3);
        if (setdomainname("innerdom", 8) != 0) _exit(4);
        if (!hn_is("inner")) _exit(5);
        if (!proc_hn_is("/proc/sys/kernel/hostname", "inner")) _exit(6);
        if (!proc_hn_is("/proc/sys/kernel/domainname", "innerdom")) _exit(7);
        struct utsname u;
        if (uname(&u) != 0 || strcmp(u.nodename, "inner") || strcmp(u.domainname, "innerdom")) _exit(8);
        pid_t g = fork();
        if (g == 0) _exit(hn_is("inner") ? 0 : 1);   /* inherited on fork */
        int gs = 0; waitpid(g, &gs, 0);
        if (!WIFEXITED(gs) || WEXITSTATUS(gs) != 0) _exit(9);
        _exit(0);
    }
    int st = 0; waitpid(c, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) { printf("  child exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1); return 1; }
    CHECK(gethostname(after, sizeof after) == 0 && !strcmp(before, after));
    CHECK(rd_link("/proc/self/ns/uts", l2, sizeof l2) == 0 && !strcmp(l1, l2));
    return 0;
}
static int t_uts_copy_inherit(void)
{
    CHECK(sethostname("parenthost", 10) == 0);
    pid_t c = syscall(SYS_clone, SIGCHLD | CLONE_NEWUTS, 0, 0, 0, 0);
    CHECK(c >= 0);
    if (c == 0) {
        if (!hn_is("parenthost")) _exit(1);          /* copied from the parent */
        if (sethostname("childhost", 9) != 0) _exit(2);
        _exit(hn_is("childhost") ? 0 : 3);
    }
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    CHECK(hn_is("parenthost"));
    pid_t f = fork();
    if (f == 0) _exit(hn_is("parenthost") ? 0 : 1);  /* plain fork shares the name */
    waitpid(f, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    CHECK(sethostname("leandros", 8) == 0);
    return 0;
}
static int t_uts_in_host_userns(void)
{
    /* a child userns holds no power over the host's UTS namespace */
    CHECK(unshare(CLONE_NEWUSER) == 0);
    CHECK(sethostname("evil", 4) == -1 && errno == EPERM);
    return 0;
}

/* ---- cgroup namespaces (design-cgroup CG0) ---- */
static int cgns_link(char *buf, size_t n)
{
    ssize_t r = readlink("/proc/self/ns/cgroup", buf, n - 1);
    if (r < 0) return -1;
    buf[r] = 0;
    return 0;
}
static int t_cgns_unshare(void)
{
    char a[64], b[64], c[64];
    CHECK(cgns_link(a, sizeof a) == 0);
    CHECK(unshare(CLONE_NEWCGROUP) == 0);
    CHECK(cgns_link(b, sizeof b) == 0);
    CHECK(strcmp(a, b) != 0);
    CHECK(strncmp(b, "cgroup:[", 8) == 0);
    /* a fork child inherits the namespace */
    pid_t p = fork();
    if (p == 0) { char x[64]; _exit(cgns_link(x, sizeof x) == 0 && !strcmp(x, b) ? 0 : 1); }
    int st = 0; CHECK(p > 0 && waitpid(p, &st, 0) == p && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    /* a second unshare gives yet another one */
    CHECK(unshare(CLONE_NEWCGROUP) == 0);
    CHECK(cgns_link(c, sizeof c) == 0 && strcmp(b, c) != 0 && strcmp(a, c) != 0);
    return 0;
}
static int t_cgns_clone(void)
{
    char a[64], b[64];
    CHECK(cgns_link(a, sizeof a) == 0);
    int pfd[2]; CHECK(pipe(pfd) == 0);
    long r = syscall(SYS_clone, SIGCHLD | CLONE_NEWCGROUP, 0, 0, 0, 0);
    CHECK(r >= 0);
    if (r == 0) { char x[64]; if (cgns_link(x, sizeof x)) _exit(2); write(pfd[1], x, strlen(x) + 1); _exit(0); }
    int st = 0; CHECK(waitpid(r, &st, 0) == r && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    CHECK(read(pfd[0], b, sizeof b) > 0);
    CHECK(strcmp(a, b) != 0);
    char a2[64]; CHECK(cgns_link(a2, sizeof a2) == 0 && !strcmp(a, a2)); /* the parent is untouched */
    /* clone3 as well */
    struct clone_args ca; memset(&ca, 0, sizeof ca); ca.flags = CLONE_NEWCGROUP; ca.exit_signal = SIGCHLD;
    r = syscall(SYS_clone3, &ca, sizeof ca);
    CHECK(r >= 0);
    if (r == 0) { char x[64]; _exit(cgns_link(x, sizeof x) == 0 && strcmp(x, a) != 0 ? 0 : 1); }
    CHECK(waitpid(r, &st, 0) == r && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    /* threads cannot be given their own */
    r = syscall(SYS_clone, CLONE_NEWCGROUP | CLONE_THREAD | CLONE_SIGHAND | CLONE_VM, 0, 0, 0, 0);
    CHECK(r == -1 && errno == EINVAL);
    return 0;
}
static int t_cgns_eperm(void)
{
    CHECK(drop_to(1000) == 0);
    CHECK(unshare(CLONE_NEWCGROUP) == -1 && errno == EPERM);
    long r = syscall(SYS_clone, SIGCHLD | CLONE_NEWCGROUP, 0, 0, 0, 0);
    CHECK(r == -1 && errno == EPERM);
    return 0;
}
static int t_cgns_in_userns(void)
{
    /* an unprivileged user may unshare(NEWUSER|NEWCGROUP) and, once in the
     * userns, unshare(NEWCGROUP) again */
    CHECK(drop_to(1000) == 0);
    char a[64], b[64], c[64];
    CHECK(cgns_link(a, sizeof a) == 0);
    CHECK(unshare(CLONE_NEWUSER | CLONE_NEWCGROUP) == 0);
    CHECK(cgns_link(b, sizeof b) == 0 && strcmp(a, b) != 0);
    CHECK(unshare(CLONE_NEWCGROUP) == 0);
    CHECK(cgns_link(c, sizeof c) == 0 && strcmp(b, c) != 0);
    /* clone(NEWUSER|NEWCGROUP) too; a nested userns needs the creator's ids
     * mapped (Linux create_user_ns: EPERM when !kuid_has_mapping) */
    CHECK(map_self_1000() == 0);
    long r = syscall(SYS_clone, SIGCHLD | CLONE_NEWUSER | CLONE_NEWCGROUP, 0, 0, 0, 0);
    CHECK(r >= 0);
    if (r == 0) { char x[64]; _exit(cgns_link(x, sizeof x) == 0 && strcmp(x, c) != 0 ? 0 : 1); }
    int st = 0; CHECK(waitpid(r, &st, 0) == r && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}
static int t_cgns_leak(void)
{
    /* many create/destroy cycles must not exhaust the namespace table (256) */
    for (int i = 0; i < 600; i++) {
        pid_t p = fork();
        if (p == 0) _exit(unshare(CLONE_NEWCGROUP) == 0 ? 0 : 1);
        int st = 0; CHECK(p > 0 && waitpid(p, &st, 0) == p && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    }
    return 0;
}

static int run_userns_cases(void)
{
    struct { const char *n; int (*f)(void); } cs[] = {
        { "unpriv_clone_full_caps", t_unpriv_clone }, { "ns_chroot", t_ns_chroot },
        { "host_root_unaffected", t_host_unaffected }, { "magic_link_access", t_magic_link_access }, { "unshare_basic", t_unshare_basic },
        { "unshare_threaded_einval", t_unshare_threaded }, { "other_new_einval", t_other_new_einval },
        { "depth_limit_32", t_depth_limit }, { "ns_leak", t_leak }, { "suid_ignored_in_userns", t_suid_ignored },
        { "ff_probe", t_ff_probe }, { "unshare_pid_eperm", t_unshare_pid_eperm },
        { "unshare_pid_twice", t_unshare_pid_twice }, { "thread_after_unshare", t_thread_after_unshare },
        { "pid1_self", t_pid1_self }, { "parent_view", t_parent_view }, { "child_of_init", t_child_of_init },
        { "visibility", t_visibility }, { "member_after_unshare", t_member_after_unshare },
        { "wait_inside", t_wait_inside }, { "siginfo_pid", t_siginfo_pid }, { "siginfo_outside", t_siginfo_outside },
        { "pthread_in_pidns", t_pthread_in_pidns }, { "nested", t_nested },
        { "pidns_leak", t_pidns_leak }, { "fork_enomem", t_fork_enomem },
        { "init_ignores_inside", t_init_ignores_inside }, { "init_kill_outside", t_init_kill_outside },
        { "init_handler", t_init_handler }, { "orphan_to_init", t_orphan_to_init },
        { "init_death_kills", t_init_death_kills }, { "proc_magic_pidns", t_proc_magic_pidns },
        { "zap_leaderless", t_zap_leaderless }, { "zap_forkrace", t_zap_forkrace }, { "kill_minus1_scoped", t_kill_minus1_scoped },
        { "sid_pgid", t_sid_pgid }, { "byid_calls", t_byid_calls }, { "peercred", t_peercred },
        { "proc_self_tgid", t_proc_self_tgid }, { "proc_self_init", t_proc_self_init },
        { "proc_listing", t_proc_listing }, { "status_views", t_status_views },
        { "map_firefox_seq", t_ff_seq }, { "ns_links", t_ns_links }, { "void_root_link", t_void_root_link },
        { "setid_guard", t_setid_guard }, { "root_einval", t_root_einval }, { "id_translate", t_id_translate },
        { "procsysnet_defaults", t_procsysnet_defaults }, { "procsysnet_netns_isolated", t_procsysnet_netns_isolated }, { "procsysnet_root_write", t_procsysnet_root_write }, { "procsysnet_unpriv_denied", t_procsysnet_unpriv_denied }, { "procsysnet_port_start", t_procsysnet_port_start }, { "procsysnet_ping_range", t_procsysnet_ping_range }, { "netns_sendfam", t_netns_sendfam }, { "netns_sees_only_lo", t_netns_sees_only_lo }, { "netns_lo_up_ack", t_netns_lo_up_ack }, { "netns_siocsifflags_lo", t_netns_siocsifflags_lo }, { "netns_siocsifflags_lo_userns", t_netns_siocsifflags_lo_userns }, { "netns_ipcns", t_netns_ipcns }, { "netns_isolate_reverse", t_netns_isolate_reverse },
        { "sock_netns_inherited", t_sock_netns_inherited }, { "sock_netns_scm", t_sock_netns_scm },
        { "sock_before_unshare", t_sock_before_unshare }, { "netns_portspace", t_netns_portspace }, { "netns_udp_bind", t_netns_udp_bind },
        { "sock_ns_reuse", t_sock_ns_reuse }, { "sock_ns_leak", t_sock_ns_leak },
        { "map_deputy", t_map_deputy }, { "map_race", t_map_race }, { "chroot_newuser_eperm", t_chroot_newuser },
        { "fdinfo_other_uid", t_fdinfo_other_uid },
        { "setns_net", t_setns_net }, { "setns_stat", t_setns_stat }, { "setns_eperm", t_setns_eperm },
        { "setns_userns_owner", t_setns_userns_owner }, { "setns_userns_root", t_setns_userns_root },
        { "setns_userns_threaded", t_setns_userns_threaded }, { "setns_userns_chroot", t_setns_userns_chroot },
        { "setns_misc", t_setns_misc }, { "setns_pid_children", t_setns_pid_children },
        { "setns_ioctl", t_setns_ioctl }, { "setns_pidfd", t_setns_pidfd }, { "setns_fd_holds", t_setns_fd_holds }, { "setns_uts", t_setns_uts },
        { "scm_cred_into_pidns", t_scm_cred_into_pidns }, { "scm_cred_from_pidns", t_scm_cred_from_pidns },
        { "scm_cred_self_in_pidns", t_scm_cred_self_in_pidns }, { "scm_cred_unmapped", t_scm_cred_unmapped },
        { "scm_cred_forge", t_scm_cred_forge }, { "sigsys_in_pidns", t_sigsys_in_pidns }, { "timer_sig_in_pidns", t_timer_sig_in_pidns },
        { "uts_unpriv", t_uts_unpriv }, { "uts_copy_inherit", t_uts_copy_inherit }, { "uts_host_userns_eperm", t_uts_in_host_userns },
        { "prio_user_scoped", t_prio_user_scoped }, { "timer_thread_id", t_timer_thread_id }, { "tty_pgrp_view", t_tty_pgrp_view },
        { "cgns_unshare", t_cgns_unshare }, { "cgns_clone", t_cgns_clone }, { "cgns_eperm", t_cgns_eperm },
        { "cgns_in_userns", t_cgns_in_userns }, { "cgns_leak", t_cgns_leak },
    };
    for (unsigned i = 0; i < sizeof cs / sizeof *cs; i++) {
        pid_t c = fork();
        if (c == 0) _exit(cs[i].f());
        int st = 0; waitpid(c, &st, 0);
        check(cs[i].n, WIFEXITED(st) && WEXITSTATUS(st) == 0);
    }
    return 0;
}

int main(int argc, char **argv)
{
    if (argc > 1 && !strcmp(argv[1], "--euid")) return print_euid();
    if (getenv("NSTEST_HOST")) { printf("nstest: SKIP (host)\n"); return 0; }
    for (unsigned i = 0; i < sizeof flags / sizeof flags[0]; i++) {
        char nm[64];
        long r;

        errno = 0; r = do_clone3(flags[i].f);
        int e = errno; reap(r);
        snprintf(nm, sizeof nm, "clone3_%s_einval", flags[i].n);
        check(nm, r == -1 && e == EINVAL);

        errno = 0; r = unshare((int)flags[i].f);
        e = errno;
        snprintf(nm, sizeof nm, "unshare_%s_einval", flags[i].n);
        check(nm, r == -1 && e == EINVAL);
    }
    if (geteuid() == 0) run_userns_cases(); else printf("SKIP userns cases: not root\n");
    /* A plain fork-style clone still works. */
    long r = do_clone(0);
    int ok = r >= 0;
    reap(r);
    check("clone_plain_ok", ok);

    printf("nstest: %d passed, %d failed\n", npass, nfail);
    return nfail != 0;
}
