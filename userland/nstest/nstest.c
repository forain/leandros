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
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <linux/sched.h>
#include <fcntl.h>
#include <pthread.h>
#include <sys/mount.h>
#include <sys/stat.h>

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
    {"NEWPID", CLONE_NEWPID},
    {"NEWNET", CLONE_NEWNET},   {"NEWIPC", CLONE_NEWIPC},
    {"NEWUTS", CLONE_NEWUTS},   {"NEWNS", CLONE_NEWNS},
    {"NEWCGROUP", CLONE_NEWCGROUP},
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
    CHECK(geteuid() == 1000);
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
    int fl[] = { 0x00020000, 0x02000000, 0x04000000, 0x08000000, 0x20000000, 0x40000000 };
    CHECK(drop_to(1000) == 0);
    for (unsigned i = 0; i < 6; i++) {
        CHECK(unshare(fl[i]) == -1 && errno == EINVAL);
        long r = syscall(SYS_clone, SIGCHLD | fl[i], 0, 0, 0, 0);
        CHECK(r == -1 && errno == EINVAL);
        r = syscall(SYS_clone, SIGCHLD | CLONE_NEWUSER | fl[i], 0, 0, 0, 0);
        CHECK(r == -1 && errno == EINVAL);
    }
    /* NEWUSER excludes CLONE_FS */
    long r = syscall(SYS_clone, SIGCHLD | CLONE_NEWUSER | CLONE_FS, 0, 0, 0, 0);
    CHECK(r == -1 && errno == EINVAL);
    return 0;
}
static int t_depth_limit(void)
{
    CHECK(drop_to(1000) == 0);
    for (int i = 0; i < 32; i++) CHECK(unshare(CLONE_NEWUSER) == 0);
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
        if (c == 0) { for (int j = 0; j < 8; j++) if (unshare(CLONE_NEWUSER)) _exit(1); _exit(0); }
        int st = 0; waitpid(c, &st, 0);
        CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    }
    CHECK(newuser_child(caps_ok) == 0);
    return 0;
}

static int print_euid(void) { printf("  euid=%d\n", geteuid()); return geteuid() == 1000 ? 0 : 7; }
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


static void reap(long r)
{
    if (r == 0) _exit(0);
    if (r > 0) waitpid((pid_t)r, NULL, 0);
}

static int run_userns_cases(void)
{
    struct { const char *n; int (*f)(void); } cs[] = {
        { "unpriv_clone_full_caps", t_unpriv_clone }, { "ns_chroot", t_ns_chroot },
        { "host_root_unaffected", t_host_unaffected }, { "unshare_basic", t_unshare_basic },
        { "unshare_threaded_einval", t_unshare_threaded }, { "other_new_einval", t_other_new_einval },
        { "depth_limit_32", t_depth_limit }, { "ns_leak", t_leak }, { "suid_ignored_in_userns", t_suid_ignored },
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

        errno = 0; r = do_clone(flags[i].f);
        int e = errno; reap(r);
        snprintf(nm, sizeof nm, "clone_%s_einval", flags[i].n);
        check(nm, r == -1 && e == EINVAL);

        errno = 0; r = do_clone3(flags[i].f);
        e = errno; reap(r);
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
