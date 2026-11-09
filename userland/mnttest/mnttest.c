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
#include <stdlib.h>
#include <sys/mount.h>
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


/* ---- M3 mntinfo ---- */
#ifndef MS_PRIVATE
#define MS_PRIVATE (1 << 18)
#endif
#ifndef MS_SHARED
#define MS_SHARED (1 << 20)
#endif
struct mi { int id, parent, maj, min; char root[256], mp[256], opts[128], fs[64], src[256], sopts[128], opt[8][32]; int nopt; };
/* libmount-style parse: 6 fixed fields, optional fields up to a lone "-", then fstype source superopts. */
static int parse_mi(char *line, struct mi *m) {
    char *f[24]; int n = 0;
    for (char *t = strtok(line, " "); t && n < 24; t = strtok(NULL, " ")) f[n++] = t;
    if (n < 10) return -1;
    if (sscanf(f[2], "%d:%d", &m->maj, &m->min) != 2) return -1;
    m->id = atoi(f[0]); m->parent = atoi(f[1]);
    snprintf(m->root, sizeof m->root, "%s", f[3]); snprintf(m->mp, sizeof m->mp, "%s", f[4]); snprintf(m->opts, sizeof m->opts, "%s", f[5]);
    int i = 6; m->nopt = 0;
    while (i < n && strcmp(f[i], "-")) { if (m->nopt < 8) snprintf(m->opt[m->nopt++], 32, "%s", f[i]); i++; }
    if (i + 3 >= n + 0 && i + 3 > n) return -1;
    if (i + 2 >= n) return -1;
    snprintf(m->fs, sizeof m->fs, "%s", f[i + 1]); snprintf(m->src, sizeof m->src, "%s", f[i + 2]);
    snprintf(m->sopts, sizeof m->sopts, "%s", i + 3 < n ? f[i + 3] : "");
    return 0;
}
static struct mi MI[64];
static int read_mi(const char *path) {
    static char buf[16384];
    if (slurp(path, buf, sizeof buf) <= 0) return -1;
    int n = 0;
    for (char *l = strtok(buf, "\n"); l && n < 64; l = strtok(NULL, "\n")) {
        char tmp[1024]; snprintf(tmp, sizeof tmp, "%s", l);
        if (parse_mi(tmp, &MI[n]) != 0) return -1;
        n++;
    }
    return n;
}
static const char *opt_of(struct mi *m, const char *pfx) {
    for (int i = 0; i < m->nopt; i++) if (!strncmp(m->opt[i], pfx, strlen(pfx))) return m->opt[i];
    return NULL;
}
static struct mi *find_mp(int n, const char *mp) { for (int i = 0; i < n; i++) if (!strcmp(MI[i].mp, mp)) return &MI[i]; return NULL; }

static void mountinfo_tests(void) {
    int n = read_mi("/proc/self/mountinfo");
    report("mountinfo_layout", n > 0);
    if (n <= 0) return;
    struct mi *r = find_mp(n, "/");
    report("mountinfo_root_present", r != NULL && r->fs[0] != 0 && r->src[0] != 0);
    const char *o = r ? opt_of(r, "shared:") : NULL;
    report("mountinfo_root_shared1", o && !strcmp(o, "shared:1"));
    report("mountinfo_pid_matches_self", ({ char p[64]; snprintf(p, sizeof p, "/proc/%d/mountinfo", getpid());
        struct mi save = MI[0]; int n2 = read_mi(p); int ok = n2 == n && MI[0].id == save.id; ok; }));
    /* ids vs /proc/self/mounts */
    n = read_mi("/proc/self/mountinfo");
    int uniq = 1, parents = 1;
    for (int i = 0; i < n; i++) {
        int pf = 0;
        for (int j = 0; j < n; j++) { if (j != i && MI[j].id == MI[i].id) uniq = 0; if (MI[j].id == MI[i].parent) pf = 1; }
        if (!pf) parents = 0;
    }
    report("mountinfo_ids_unique", uniq);
    report("mountinfo_parents_exist", parents);
    static char mb[16384]; int mn = slurp("/proc/self/mounts", mb, sizeof mb);
    int ok = mn > 0;
    for (int i = 0; ok && i < n; i++) {
        char key[600]; snprintf(key, sizeof key, " %s %s ", MI[i].mp, MI[i].fs);
        if (!strstr(mb, key)) ok = 0;
    }
    report("mountinfo_consistent_with_mounts", ok);
    int lines = 0; for (char *c = mb; *c; c++) if (*c == '\n') lines++;
    report("mounts_covers_mountinfo", lines >= n);
    /* propagation */
    errno = 0; mkdir("/tmp/mi_nonmp", 0755);
    int rc = mount("", "/tmp/mi_nonmp", NULL, MS_PRIVATE, NULL);
    report("private_nonmountpoint_einval", rc < 0 && errno == EINVAL);
    rmdir("/tmp/mi_nonmp");
    rc = mount("", "/", NULL, MS_PRIVATE, NULL);
    n = read_mi("/proc/self/mountinfo"); r = find_mp(n, "/");
    report("private_root_clears_shared", rc == 0 && r && !opt_of(r, "shared:"));
    rc = mount("", "/", NULL, MS_SHARED, NULL);
    n = read_mi("/proc/self/mountinfo"); r = find_mp(n, "/");
    o = r ? opt_of(r, "shared:") : NULL;
    report("shared_root_restored", rc == 0 && o && !strcmp(o, "shared:1"));
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
    mountinfo_tests();
    printf("%s\n", fails ? "mnttest: FAILED" : "mnttest: ALL PASSED");
    return fails ? 1 : 0;
}
