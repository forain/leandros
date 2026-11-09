/*
 * idmaptest -- user-namespace uid/gid map behaviour. Static zig-cc build,
 * runs as root; each case prints "case: PASS|FAIL|SKIP". Non-identity map
 * cases SKIP when the map write returns EPERM (kernel booted without
 * idmap=test) and must PASS with it. Lanes append cases as separate functions.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int npass, nfail;

static void report(const char *name, int ok)
{
    printf("%s: %s\n", name, ok ? "PASS" : "FAIL");
    if (ok) npass++; else nfail++;
}
static void skip(const char *name, const char *why) { printf("%s: SKIP (%s)\n", name, why); }

static int write_file(const char *path, const char *s)
{
    int fd = open(path, O_WRONLY);
    if (fd < 0) return -1;
    ssize_t n = write(fd, s, strlen(s));
    int e = errno;
    close(fd);
    errno = e;
    return n == (ssize_t)strlen(s) ? 0 : -1;
}

/* Parent (ns 0, root) writes "0 <lower> 1" to uid_map and gid_map of pid. */
static int write_maps(pid_t pid, unsigned lower)
{
    char p[64], m[64];
    snprintf(m, sizeof m, "0 %u 1", lower);
    snprintf(p, sizeof p, "/proc/%d/uid_map", pid);
    if (write_file(p, m)) return -1;
    snprintf(p, sizeof p, "/proc/%d/gid_map", pid);
    if (write_file(p, m)) return -1;
    return 0;
}

/* ---- statx_matches_stat ------------------------------------------------- */
static void statx_matches_stat(void)
{
    const char *tmp = "/tmp/idmaptest-statx";
    int fd = open(tmp, O_CREAT | O_WRONLY, 0644);
    int ok = fd >= 0;
    if (ok) { close(fd); ok = chown(tmp, 1234, 4321) == 0; }
    const char *paths[] = { tmp, "/", "/etc/passwd", "/proc/self" };
    for (unsigned i = 0; ok && i < sizeof paths / sizeof *paths; i++) {
        struct stat st; struct statx sx;
        if (stat(paths[i], &st) || syscall(SYS_statx, AT_FDCWD, paths[i], 0, STATX_BASIC_STATS, &sx)) {
            ok = 0; break;
        }
        if (st.st_uid != sx.stx_uid || st.st_gid != sx.stx_gid) ok = 0;
    }
    unlink(tmp);
    report("statx_matches_stat", ok);
}

/* ---- proc_pid_owner ----------------------------------------------------- */
static void proc_pid_owner(void)
{
    int sync[2];
    pipe(sync);
    pid_t c = fork();
    if (c == 0) {
        close(sync[1]);
        if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)) _exit(1);
        char b; read(sync[0], &b, 1);
        _exit(0);
    }
    close(sync[0]);
    usleep(200000);
    char p[32]; struct stat st;
    snprintf(p, sizeof p, "/proc/%d", c);
    int ok = stat(p, &st) == 0 && st.st_uid == 1000 && st.st_gid == 1000;
    close(sync[1]);
    waitpid(c, NULL, 0);
    report("proc_pid_owner", ok);
}

/* ---- status_groups_view ------------------------------------------------- */
static void status_groups_view(void)
{
    pid_t c = fork();
    if (c == 0) {
        gid_t g[2] = { 1000, 20 };
        if (setgroups(2, g)) _exit(2);
        FILE *f = fopen("/proc/self/status", "r");
        char l[256];
        while (f && fgets(l, sizeof l, f))
            if (!strncmp(l, "Groups:", 7)) _exit(strstr(l, "1000") && strstr(l, "20") ? 0 : 3);
        _exit(4);
    }
    int st; waitpid(c, &st, 0);
    report("status_groups_view", WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

/* ---- knob cases: child unshares a userns, parent writes "0 100000 1" ----- */
static pid_t spawn_in_ns(int *go, int *res, int (*body)(void))
{
    int a[2], b[2];
    pipe(a); pipe(b);
    pid_t c = fork();
    if (c == 0) {
        close(a[1]); close(b[0]);
        char x = 0;
        if (unshare(CLONE_NEWUSER)) { x = 'u'; write(b[1], &x, 1); _exit(1); }
        write(b[1], &x, 1);            /* unshared */
        read(a[0], &x, 1);             /* maps written ('m') or refused ('n') */
        if (x != 'm') _exit(77);
        _exit(body());
    }
    close(a[0]); close(b[1]);
    *go = a[1]; *res = b[0];
    return c;
}

static int run_ns_case(const char *name, int (*body)(void))
{
    int go, res; char x;
    pid_t c = spawn_in_ns(&go, &res, body);
    if (read(res, &x, 1) != 1 || x != 0) { write(go, "n", 1); waitpid(c, NULL, 0); skip(name, "unshare failed"); return 0; }
    char m = write_maps(c, 100000) ? 'n' : 'm';
    write(go, &m, 1);
    int st; waitpid(c, &st, 0);
    if (m == 'n') { skip(name, "non-identity map refused (no idmap=test)"); return 0; }
    report(name, WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 1;
}

static int body_stat_unmapped(void)
{
    struct stat st;
    /* /etc/passwd is kuid 0 = unmapped here (ns 0 -> 100000 only). */
    if (stat("/etc/passwd", &st)) return 2;
    return st.st_uid == 65534 && st.st_gid == 65534 ? 0 : 3;
}
static void stat_unmapped_65534(void) { run_ns_case("stat_unmapped_65534", body_stat_unmapped); }

static volatile sig_atomic_t chld_uid = -2;
static void on_chld(int s, siginfo_t *si, void *u) { (void)s; (void)u; chld_uid = (sig_atomic_t)si->si_uid; }

static int body_sigchld(void)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_chld;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGCHLD, &sa, NULL);
    pid_t k = fork();
    if (k == 0) _exit(0);
    for (int i = 0; i < 200 && chld_uid == -2; i++) usleep(10000);
    waitpid(k, NULL, 0);
    /* We are ns-root (kuid 100000): the handler must see 0, not 100000. */
    return chld_uid == 0 ? 0 : 3;
}
static void sigchld_si_uid_view(void) { run_ns_case("sigchld_si_uid_view", body_sigchld); }

int main(void)
{
    statx_matches_stat();
    proc_pid_owner();
    status_groups_view();
    stat_unmapped_65534();
    sigchld_si_uid_view();
    printf("idmaptest: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
