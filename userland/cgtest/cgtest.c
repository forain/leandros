/*
 * cgtest: cgroup v2 core and cgroup namespace (design-cgroup lanes CG0/CG1).
 *
 * CG0/CG1 have no cgroupfs yet (CG2), so this exercises what is visible from
 * userland: the cgroup namespace (/proc/<pid>/ns/cgroup, unshare, clone,
 * inheritance across fork, threads and exec) and that task membership
 * accounting survives heavy fork/exit/thread churn. CG2 extends it with the
 * cgroupfs cases.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef CLONE_NEWCGROUP
#define CLONE_NEWCGROUP 0x02000000
#endif
#ifndef CLONE_NEWUSER
#define CLONE_NEWUSER 0x10000000
#endif

static int npass, nfail;
#define CHECK(c) do { if (!(c)) { printf("  FAIL line %d: %s (errno %d)\n", __LINE__, #c, errno); return 1; } } while (0)

static int link_of(const char *path, char *buf, size_t n)
{
    ssize_t r = readlink(path, buf, n - 1);
    if (r < 0) return -1;
    buf[r] = 0;
    return 0;
}
static int self_link(char *buf, size_t n) { return link_of("/proc/self/ns/cgroup", buf, n); }

static int wait_ok(pid_t p)
{
    int st = 0;
    return p > 0 && waitpid(p, &st, 0) == p && WIFEXITED(st) && WEXITSTATUS(st) == 0;
}

static int t_initial(void)
{
    char a[64];
    CHECK(self_link(a, sizeof a) == 0);
    CHECK(strncmp(a, "cgroup:[", 8) == 0);
    struct stat st;
    CHECK(stat("/proc/self/ns/cgroup", &st) == 0);
    char want[64]; snprintf(want, sizeof want, "cgroup:[%lu]", (unsigned long)st.st_ino);
    CHECK(strcmp(a, want) == 0);
    return 0;
}

static int t_unshare_changes(void)
{
    char a[64], b[64];
    CHECK(self_link(a, sizeof a) == 0);
    CHECK(unshare(CLONE_NEWCGROUP) == 0);
    CHECK(self_link(b, sizeof b) == 0);
    CHECK(strcmp(a, b) != 0);
    struct stat st;
    CHECK(stat("/proc/self/ns/cgroup", &st) == 0);
    char want[64]; snprintf(want, sizeof want, "cgroup:[%lu]", (unsigned long)st.st_ino);
    CHECK(strcmp(b, want) == 0);
    char pidpath[64]; snprintf(pidpath, sizeof pidpath, "/proc/%d/ns/cgroup", getpid());
    char c[64]; CHECK(link_of(pidpath, c, sizeof c) == 0 && !strcmp(b, c));
    return 0;
}

static int t_fork_inherits(void)
{
    char a[64], b[64];
    CHECK(unshare(CLONE_NEWCGROUP) == 0);
    CHECK(self_link(a, sizeof a) == 0);
    pid_t p = fork();
    if (p == 0) _exit(self_link(b, sizeof b) == 0 && !strcmp(a, b) ? 0 : 1);
    CHECK(wait_ok(p));
    return 0;
}

static char thr_link[64];
static void *thr_fn(void *a) { (void)a; self_link(thr_link, sizeof thr_link); return 0; }
static int t_thread_shares(void)
{
    char a[64];
    CHECK(unshare(CLONE_NEWCGROUP) == 0);
    CHECK(self_link(a, sizeof a) == 0);
    pthread_t t;
    CHECK(pthread_create(&t, 0, thr_fn, 0) == 0);
    pthread_join(t, 0);
    CHECK(strcmp(a, thr_link) == 0);
    return 0;
}

static int t_exec_keeps(void)
{
    char a[64];
    CHECK(unshare(CLONE_NEWCGROUP) == 0);
    CHECK(self_link(a, sizeof a) == 0);
    int pfd[2]; CHECK(pipe(pfd) == 0);
    pid_t p = fork();
    if (p == 0) {
        dup2(pfd[1], 1);
        execl("/bin/cgtest", "cgtest", "--print", (char *)0);
        _exit(127);
    }
    close(pfd[1]);
    char b[64] = {0};
    ssize_t n = read(pfd[0], b, sizeof b - 1);
    CHECK(n > 0);
    while (n > 0 && (b[n - 1] == '\n')) b[--n] = 0;
    CHECK(wait_ok(p));
    CHECK(strcmp(a, b) == 0);
    return 0;
}

static int t_clone_new(void)
{
    char a[64], b[64];
    CHECK(self_link(a, sizeof a) == 0);
    int pfd[2]; CHECK(pipe(pfd) == 0);
    long r = syscall(SYS_clone, SIGCHLD | CLONE_NEWCGROUP, 0, 0, 0, 0);
    CHECK(r >= 0);
    if (r == 0) { char x[64]; if (self_link(x, sizeof x)) _exit(2); (void)!write(pfd[1], x, strlen(x) + 1); _exit(0); }
    CHECK(wait_ok(r));
    CHECK(read(pfd[0], b, sizeof b) > 0 && strcmp(a, b) != 0);
    char a2[64]; CHECK(self_link(a2, sizeof a2) == 0 && !strcmp(a, a2));
    return 0;
}

static int t_eperm_nonroot(void)
{
    CHECK(setgid(1000) == 0 && setuid(1000) == 0);
    CHECK(unshare(CLONE_NEWCGROUP) == -1 && errno == EPERM);
    return 0;
}

static int t_userns_unshare(void)
{
    CHECK(setgid(1000) == 0 && setuid(1000) == 0);
    char a[64], b[64];
    CHECK(self_link(a, sizeof a) == 0);
    CHECK(unshare(CLONE_NEWUSER | CLONE_NEWCGROUP) == 0);
    CHECK(self_link(b, sizeof b) == 0 && strcmp(a, b) != 0);
    CHECK(unshare(CLONE_NEWCGROUP) == 0);
    return 0;
}

static void *noop_thr(void *a) { (void)a; return 0; }
static int t_churn(void)
{
    /* membership accounting: heavy fork/exit/thread churn must neither fail
     * nor wedge (a charge/uncharge imbalance would break rmdir later) */
    for (int i = 0; i < 300; i++) {
        pid_t p = fork();
        if (p == 0) {
            pthread_t t;
            if (pthread_create(&t, 0, noop_thr, 0) == 0) pthread_join(t, 0);
            _exit(0);
        }
        CHECK(wait_ok(p));
    }
    return 0;
}

static int t_ns_leak(void)
{
    /* the namespace table holds 256 ids: 600 create/exit cycles must not run it dry */
    for (int i = 0; i < 600; i++) {
        pid_t p = fork();
        if (p == 0) _exit(unshare(CLONE_NEWCGROUP) == 0 ? 0 : 1);
        CHECK(wait_ok(p));
    }
    char a[64]; CHECK(unshare(CLONE_NEWCGROUP) == 0 && self_link(a, sizeof a) == 0);
    return 0;
}

int main(int argc, char **argv)
{
    if (argc > 1 && !strcmp(argv[1], "--print")) {
        char a[64];
        if (self_link(a, sizeof a)) return 1;
        printf("%s\n", a);
        return 0;
    }
    if (getuid() != 0) { printf("cgtest: needs root\n"); return 1; }
    static const struct { const char *n; int (*f)(void); } cs[] = {
        { "ns_initial", t_initial }, { "ns_unshare_changes", t_unshare_changes },
        { "ns_fork_inherits", t_fork_inherits }, { "ns_thread_shares", t_thread_shares },
        { "ns_exec_keeps", t_exec_keeps }, { "ns_clone_new", t_clone_new },
        { "ns_eperm_nonroot", t_eperm_nonroot }, { "ns_userns_unshare", t_userns_unshare },
        { "membership_churn", t_churn }, { "ns_leak", t_ns_leak },
    };
    for (unsigned i = 0; i < sizeof cs / sizeof *cs; i++) {
        pid_t c = fork();
        if (c == 0) _exit(cs[i].f());
        int ok = wait_ok(c);
        printf("%s %s\n", ok ? "PASS" : "FAIL", cs[i].n);
        if (ok) npass++; else nfail++;
    }
    printf("cgtest: %d passed, %d failed\n", npass, nfail);
    return nfail != 0;
}
