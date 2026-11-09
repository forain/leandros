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
#include <sys/wait.h>
#include <stdlib.h>
#include <signal.h>
#include <stdint.h>
#include <linux/sched.h>

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


/* ── Mount namespaces (lane M1, mntunshare) ─────────────────────────────── */
#ifndef CLONE_NEWNS
#define CLONE_NEWNS 0x00020000
#endif
#ifndef CLONE_NEWUSER
#define CLONE_NEWUSER 0x10000000
#endif
#ifndef CLONE_FS
#define CLONE_FS 0x00000200
#endif
#define M_BIND 4096UL
#define M_REC 0x4000UL
#define M_DETACH 2

static int mount_(const char *s, const char *t, const char *f, unsigned long fl) {
    return (int)syscall(SYS_mount, s, t, f, fl, NULL);
}
static int umount_(const char *t, int fl) { return (int)syscall(SYS_umount2, t, fl); }
static int exists(const char *p) { struct stat st; return stat(p, &st) == 0; }
static void put(const char *p, const char *c, int mode) {
    int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, mode);
    if (fd >= 0) { if (c) write(fd, c, strlen(c)); close(fd); }
    chmod(p, mode);
}
static void dir(const char *p) { mkdir(p, 0777); chmod(p, 0777); }
static void path(char *o, size_t n, const char *b, const char *r) { snprintf(o, n, "%s/%s", b, r); }

/* Run `file` with one argument; true when it exits 42 (see main). */
static int exec42(const char *file, const char *arg) {
    pid_t p = fork();
    if (p == 0) { execl(file, file, arg, (char *)NULL); _exit(97); }
    int st = 0;
    if (p < 0 || waitpid(p, &st, 0) < 0) return 0;
    return WIFEXITED(st) && WEXITSTATUS(st) == 42;
}

struct cl3 { uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls; };
static long clone3_(uint64_t flags) {
    struct cl3 a; memset(&a, 0, sizeof a);
    a.flags = flags; a.exit_signal = SIGCHLD;
    return syscall(SYS_clone3, &a, sizeof a);
}

/* ns_live / sb_count from the kernel selftest snapshot. */
static int ns_counts(int *live, int *sbs) {
    static char buf[8192];
    if (slurp("/proc/mntselftest", buf, sizeof buf) <= 0) return -1;
    char *q = strstr(buf, "ns_live=");
    if (!q) return -1;
    int dying = 0;
    return sscanf(q, "ns_live=%d ns_dying=%d sb_count=%d", live, &dying, sbs) == 3 ? 0 : -1;
}

static void sync_parent(int wr, int rd) { char c = 'x'; write(wr, &c, 1); read(rd, &c, 1); }

/* The unprivileged child: user + mount namespace, then the whole tour. */
static void ns_child(const char *base, int wr, int rd) {
    char a[96], b[96], c[96], t[96], f[96], d[96], x[128], bsub[128], bmark[128], cwd[256];
    path(a, sizeof a, base, "a"); path(b, sizeof b, base, "b"); path(c, sizeof c, base, "c");
    path(t, sizeof t, base, "t"); path(f, sizeof f, base, "f"); path(d, sizeof d, base, "d");
    if (setgid(65534) || setuid(65534)) _exit(99);
    if (chdir(a)) _exit(98);
    int r = unshare(CLONE_NEWUSER | CLONE_NEWNS);
    report("newns_unpriv_unshare", r == 0);
    if (r) { fflush(stdout); _exit(1); }
    /* cwd survives the unshare: a relative open still finds the file. */
    report("newns_cwd_kept", access("marker", F_OK) == 0 && getcwd(cwd, sizeof cwd) && !strcmp(cwd, a));
    /* a bind: visible here, not outside */
    report("newns_bind_mount", mount_(a, b, NULL, M_BIND) == 0);
    path(bmark, sizeof bmark, b, "marker");
    report("newns_bind_visible_in", exists(bmark));
    sync_parent(wr, rd);
    report("newns_rel_bind", chdir(b) == 0 && access("marker", F_OK) == 0 && getcwd(cwd, sizeof cwd) && !strcmp(cwd, b));
    report("newns_rel_dotdot", chdir("..") == 0 && chdir("a") == 0 && access("marker", F_OK) == 0);
    /* a tmpfs mount of our own */
    report("newns_tmpfs_mount", mount_("tmpfs", t, "tmpfs", 0) == 0);
    path(x, sizeof x, t, "x");
    int ok = 0;
    int fd = open(x, O_RDWR | O_CREAT, 0644);
    if (fd >= 0) {
        char rb[8] = {0};
        ok = write(fd, "hi", 2) == 2 && lseek(fd, 0, SEEK_SET) == 0 && read(fd, rb, 7) == 2 && !memcmp(rb, "hi", 2);
        close(fd);
    }
    report("newns_tmpfs_rw", ok);
    sync_parent(wr, rd);
    report("newns_umount", umount_(t, 0) == 0);
    report("newns_umount_gone", !exists(x));
    /* exec through a file bind and a #! script through a dir bind */
    report("newns_bind_file", mount_("/bin/mnttest", f, NULL, M_BIND) == 0);
    report("newns_exec_bind", exec42(f, "child-ok"));
    char s[128]; path(s, sizeof s, b, "script");
    report("newns_exec_script_bind", exec42(s, "unused"));
    /* locks inherited across the userns boundary */
    errno = 0; r = umount_("/tmp", 0);
    report("umount_locked_einval", r == -1 && errno == EINVAL);
    errno = 0; r = mount_("/", d, NULL, M_BIND);
    report("bind_locked_children_einval", r == -1 && errno == EINVAL);
    report("rbind_ok", mount_("/", d, NULL, M_BIND | M_REC) == 0);
    path(x, sizeof x, d, "tmp");
    errno = 0; r = umount_(x, 0);
    report("rbind_child_locked", r == -1 && errno == EINVAL);
    report("rbind_detach", umount_(d, M_DETACH) == 0);
    errno = 0; r = umount_(d, 0);
    report("umount_not_mountpoint", r == -1 && errno == EINVAL);
    /* busy and detach: a submount keeps its parent busy */
    path(bsub, sizeof bsub, b, "sub");
    report("detach_setup", mount_(c, bsub, NULL, M_BIND) == 0);
    errno = 0; r = umount_(b, 0);
    report("umount_busy", r == -1 && errno == EBUSY);
    report("detach_subtree", umount_(b, M_DETACH) == 0 && !exists(bmark));
    sync_parent(wr, rd);
    fflush(stdout);
    _exit(fails ? 1 : 0);
}

static void newns_suite(void) {
    char base[64], p[128], q[128];
    snprintf(base, sizeof base, "/tmp/mt.%d", (int)getpid());
    dir(base);
    path(p, sizeof p, base, "a"); dir(p);
    path(q, sizeof q, p, "marker"); put(q, "A", 0666);
    path(q, sizeof q, p, "sub"); dir(q);
    path(q, sizeof q, p, "script");
    { char body[160]; snprintf(body, sizeof body, "#!%s/f child-ok\n", base); put(q, body, 0755); }
    path(p, sizeof p, base, "b"); dir(p);
    path(p, sizeof p, base, "c"); dir(p);
    path(q, sizeof q, p, "cmark"); put(q, "C", 0666);
    path(p, sizeof p, base, "t"); dir(p);
    path(p, sizeof p, base, "d"); dir(p);
    path(p, sizeof p, base, "f"); put(p, "", 0755);

    /* ns inode differs inside */
    char mine[64] = {0}, theirs[64] = {0};
    ssize_t n = readlink("/proc/self/ns/mnt", mine, sizeof mine - 1); if (n > 0) mine[n] = 0;
    int live0 = -1, sb0 = -1;
    ns_counts(&live0, &sb0);

    int c2p[2], p2c[2];
    if (pipe(c2p) || pipe(p2c)) { report("newns_pipe", 0); return; }
    pid_t pid = fork();
    if (pid == 0) { close(c2p[0]); close(p2c[1]); ns_child(base, c2p[1], p2c[0]); _exit(98); }
    close(c2p[1]); close(p2c[0]);
    char ch;
    char bm[128], tx[128];
    path(bm, sizeof bm, base, "b/marker"); path(tx, sizeof tx, base, "t/x");
    int got = read(c2p[0], &ch, 1) == 1;
    report("newns_bind_invisible_out", got && !exists(bm));
    write(p2c[1], &ch, 1);
    got = read(c2p[0], &ch, 1) == 1;
    report("newns_tmpfs_invisible_out", got && !exists(tx));
    write(p2c[1], &ch, 1);
    got = read(c2p[0], &ch, 1) == 1;
    write(p2c[1], &ch, 1);
    int st = 0;
    waitpid(pid, &st, 0);
    report("newns_child_clean", got && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    close(c2p[0]); close(p2c[1]);

    /* NEWNS|CLONE_FS is EINVAL; unprivileged NEWNS without NEWUSER is EPERM */
    errno = 0; long r = clone3_(CLONE_NEWNS | CLONE_FS);
    report("newns_fs_einval", r == -1 && errno == EINVAL);
    pid = fork();
    if (pid == 0) {
        if (setgid(65534) || setuid(65534)) _exit(99);
        errno = 0;
        _exit(unshare(CLONE_NEWNS) == -1 && errno == EPERM ? 0 : 1);
    }
    waitpid(pid, &st, 0);
    report("newns_perm", WIFEXITED(st) && WEXITSTATUS(st) == 0);

    /* clone3(NEWNS|NEWUSER): the child owns a private mount tree */
    r = clone3_(CLONE_NEWNS | CLONE_NEWUSER);
    if (r == 0) {
        char a2[96], b2[96], m2[128];
        path(a2, sizeof a2, base, "a"); path(b2, sizeof b2, base, "b"); path(m2, sizeof m2, b2, "marker");
        int ok = mount_(a2, b2, NULL, M_BIND) == 0 && exists(m2) && umount_(b2, 0) == 0 && !exists(m2);
        _exit(ok ? 0 : 1);
    }
    if (r > 0) { waitpid((pid_t)r, &st, 0); }
    report("clone3_newns", r > 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0 && !exists(bm));

    /* root, NEWNS only: nothing locked, the namespace is its own */
    pid = fork();
    if (pid == 0) {
        char in[64] = {0};
        int ok = unshare(CLONE_NEWNS) == 0;
        ssize_t k = readlink("/proc/self/ns/mnt", in, sizeof in - 1); if (k > 0) in[k] = 0;
        ok = ok && strcmp(in, mine) != 0;
        ok = ok && umount_("/dev/shm", 0) == 0;
        errno = 0;
        ok = ok && umount_("/dev/shm", 0) == -1 && errno == EINVAL;
        _exit(ok ? 0 : 1);
    }
    waitpid(pid, &st, 0);
    report("newns_root_umount_ns_ino", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    (void)theirs;
    report("newns_ns0_still_mounted", exists("/dev/shm"));

    /* leak: a further namespace creation reaps the dead ones */
    pid = fork();
    if (pid == 0) _exit(unshare(CLONE_NEWNS) == 0 ? 0 : 1);
    waitpid(pid, &st, 0);
    int live1 = -2, sb1 = -2;
    ns_counts(&live1, &sb1);
    report("newns_leak", live0 >= 0 && live1 == live0 && sb1 == sb0);
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "bench")) { bench(); return 0; }
    if (argc > 1 && !strcmp(argv[1], "child-ok")) return 42;
    newapi_enosys();
    kernel_selftest();
    sb_hidden_ns0();
    ns0_invariants();
    newns_suite();
    printf("%s\n", fails ? "mnttest: FAILED" : "mnttest: ALL PASSED");
    return fails ? 1 : 0;
}
