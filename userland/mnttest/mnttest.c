/* mnttest: mount-namespace tests. M0 (mntcore) cases are unit-level checks
 * reachable without CLONE_NEWNS: the in-kernel mount-tree selftest
 * (/proc/mntselftest) and ns-0 invariants. Output: "case: PASS|FAIL|SKIP". */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sched.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#include <sys/syscall.h>

static int fails;
static void report(const char *name, int ok) { printf("%s: %s\n", name, ok ? "PASS" : "FAIL"); if (!ok) fails++; }

static int slurp(const char *path, char *buf, size_t n) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    size_t t = 0; ssize_t r;
    while (t + 1 < n && (r = read(fd, buf + t, n - 1 - t)) > 0) t += (size_t)r;
    close(fd); buf[t] = 0; return (int)t;
}

static void kernel_selftest(void) {
    static char buf[8192];
    if (slurp("/proc/mntselftest", buf, sizeof buf) <= 0) { report("selftest_available", 0); return; }
    int rows = 0, bad = 0;
    for (char *l = strtok(buf, "\n"); l; l = strtok(NULL, "\n")) {
        char *c = strstr(l, ": ");
        if (!c) continue;
        if (!strncmp(c, ": PASS", 6)) { *c = 0; report(l, 1); rows++; }
        else if (!strncmp(c, ": FAIL", 6)) { *c = 0; report(l, 0); rows++; bad++; }
    }
    report("selftest_rows", rows >= 15 && bad == 0);
}

/* New mount API (lane newmntapi, M8): every call returns ENOSYS. */
static void newapi_enosys(void) {
    static const struct { const char *n; long nr; } c[] = {
        {"open_tree", 428}, {"move_mount", 429}, {"fsopen", 430}, {"fsconfig", 431},
        {"fsmount", 432}, {"fspick", 433}, {"mount_setattr", 442} };
    int ok = 1;
    for (unsigned i = 0; i < sizeof c / sizeof c[0]; i++) {
        errno = 0;
        long r = syscall(c[i].nr, 0L, 0L, 0L, 0L, 0L, 0L);
        if (!(r == -1 && errno == ENOSYS)) ok = 0;
    }
    report("newapi_enosys", ok);
}

/* The hidden superblock prefix must not be reachable or listed. */
static void sb_hidden_ns0(void) {
    struct stat st;
    int r = stat("/.leandros-sb", &st);
    report("sb_prefix_enoent", r < 0 && errno == ENOENT);
    r = open("/.leandros-sb/tmpfs/1", O_RDONLY);
    report("sb_prefix_open_enoent", r < 0 && errno == ENOENT);
    int seen = 0;
    DIR *d = opendir("/");
    if (d) { struct dirent *e; while ((e = readdir(d))) if (!strcmp(e->d_name, ".leandros-sb")) seen = 1; closedir(d); }
    report("sb_prefix_not_listed", !seen);
}

/* Counters: no mount namespace may exist at baseline, and CLONE_NEWNS is
 * still rejected until M1. */
static void ns0_invariants(void) {
    static char buf[1024];
    int ok = slurp("/proc/mntselftest", buf, sizeof buf) > 0 && strstr(buf, "ns_dying=0") != NULL;
    report("no_dying_ns", ok);
    errno = 0;
    int r = unshare(CLONE_NEWNS);
    report("newns_still_rejected", r < 0 && (errno == EINVAL || errno == EPERM));
    char cwd[256];
    report("getcwd_ns0", getcwd(cwd, sizeof cwd) != NULL);
    struct stat st;
    report("proc_mounts_readable", stat("/proc/self/mountinfo", &st) == 0);
}

/* "mnttest bench": path-lookup cost (ns per call), for A/B against a base kernel. */
static long long now_ns(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1000000000LL + t.tv_nsec; }
static void bench(void) {
    static const char *paths[] = { "/bin/mnttest", "/etc/../bin/./mnttest", "/proc/self/status", "/tmp", "/dev/null" };
    mkdir("/tmp/mb", 0755); mkdir("/tmp/mb/d", 0755);
    if (chdir("/tmp/mb") != 0) return;
    struct stat st;
    for (int round = 0; round < 3; round++) {
        long long t0 = now_ns(); int n = 0;
        for (int i = 0; i < 20000; i++) for (int k = 0; k < 5; k++) { stat(paths[k], &st); n++; }
        long long t1 = now_ns();
        for (int i = 0; i < 20000; i++) { stat("d/../d", &st); stat("d", &st); n += 2; }
        long long t2 = now_ns();
        printf("bench round %d: abs %lld ns/op, rel %lld ns/op\n", round, (t1 - t0) / 100000, (t2 - t1) / 40000);
    }
    chdir("/");
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "bench")) { bench(); return 0; }
    newapi_enosys();
    kernel_selftest();
    sb_hidden_ns0();
    ns0_invariants();
    printf("%s\n", fails ? "mnttest: FAILED" : "mnttest: ALL PASSED");
    return fails ? 1 : 0;
}
