/* mnttest: mount-namespace tests. M0 (mntcore) cases are unit-level checks
 * reachable without CLONE_NEWNS: the in-kernel mount-tree selftest
 * (/proc/mntselftest) and ns-0 invariants. Output: "case: PASS|FAIL|SKIP". */
#define _GNU_SOURCE
#include <dirent.h>
#include <sys/ioctl.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sched.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <time.h>
#include <unistd.h>
#include <stdlib.h>
#include <sys/mount.h>
#include <sys/wait.h>
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
 *  */
static void ns0_invariants(void) {
    static char buf[1024];
    int ok = slurp("/proc/mntselftest", buf, sizeof buf) > 0 && strstr(buf, "ns_dying=0") != NULL;
    report("no_dying_ns", ok);
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
    fails = 0; /* inherited from the parent */
    if (setgid(65534) || setuid(65534)) _exit(99);
    if (chdir(a)) _exit(98);
    int r = unshare(CLONE_NEWUSER | CLONE_NEWNS);
    report("newns_unpriv_unshare", r == 0);
    if (r) { fflush(stdout); _exit(1); }
    /* Map our own ids (identity): creating files with unmapped ids is EOVERFLOW. */
    { static const char *mp[][2] = { {"/proc/self/setgroups", "deny"}, {"/proc/self/uid_map", "65534 65534 1"}, {"/proc/self/gid_map", "65534 65534 1"} };
      for (int mi = 0; mi < 3; mi++) { int mfd = open(mp[mi][0], O_WRONLY); if (mfd >= 0) { if (write(mfd, mp[mi][1], strlen(mp[mi][1])) < 0) {} close(mfd); } } }
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

/* ── Review fixes (mntunshare): pool squatting, pivot, private, proc ────── */
#ifndef MS_PRIVATE_
#define MS_PRIVATE_ (1UL << 18)
#define MS_SLAVE_ (1UL << 19)
#define MS_SHARED_ (1UL << 20)
#endif

static int in_ns_dying0(void) {
    static char buf[1024];
    return slurp("/proc/mntselftest", buf, sizeof buf) > 0 && strstr(buf, "ns_dying=0") != NULL;
}

static int vm_child(void *arg) {
    (void)arg;
    execl("/bin/mnttest", "/bin/mnttest", "child-ok", (char *)NULL);
    _exit(97);
}

static void hardening_suite(void) {
    char sq[64];
    int st = 0;
    pid_t pid;
    int k;

    /* pool_squat: an unprivileged user pre-creates the old /tmp pool and a
     * reachable directory for every likely instance number; a later tmpfs
     * mount (root, in its own mount namespace) must land elsewhere and the
     * squatter's directories stay untouched and are not the backing store. */
    pid = fork();
    if (pid == 0) {
        if (setgid(65534) || setuid(65534)) _exit(99);
        mkdir("/tmp/.leandros-sb", 0777);
        mkdir("/tmp/.leandros-sb/tmpfs", 0777);
        for (k = 1; k <= 24; k++) {
            snprintf(sq, sizeof sq, "/tmp/.leandros-sb/tmpfs/%d", k);
            mkdir(sq, 0777);
            strcat(sq, "/sentinel");
            put(sq, "S", 0666);
        }
        /* and the real hidden space is not creatable by name */
        errno = 0;
        int r1 = mkdir("/.leandros-sb/tmpfs/9999", 0777);
        _exit(r1 == -1 && errno == ENOENT ? 0 : 1);
    }
    waitpid(pid, &st, 0);
    report("pool_squat_hidden_mkdir_enoent", WIFEXITED(st) && WEXITSTATUS(st) == 0);

    char base[64], tm[96], mk[128];
    snprintf(base, sizeof base, "/tmp/mh.%d", (int)getpid());
    dir(base);
    path(tm, sizeof tm, base, "t");
    dir(tm);
    path(mk, sizeof mk, tm, "mark");
    pid = fork();
    if (pid == 0) {
        int ok = unshare(CLONE_NEWNS) == 0 && mount_("tmpfs", tm, "tmpfs", 0) == 0;
        if (ok) put(mk, "M", 0644);
        ok = ok && exists(mk);
        _exit(ok ? 0 : 1);
    }
    waitpid(pid, &st, 0);
    report("pool_squat_tmpfs_mounts", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    int leaked = 0, sent_ok = 1, reach = 0;
    for (k = 1; k <= 64; k++) {
        snprintf(sq, sizeof sq, "/tmp/.leandros-sb/tmpfs/%d/mark", k);
        if (exists(sq)) leaked = 1;
        if (k <= 24) {
            snprintf(sq, sizeof sq, "/tmp/.leandros-sb/tmpfs/%d/sentinel", k);
            if (!exists(sq)) sent_ok = 0;
        }
        snprintf(sq, sizeof sq, "/.leandros-sb/tmpfs/%d", k);
        if (exists(sq)) reach = 1;
    }
    report("pool_squat_not_adopted", !leaked && sent_ok);
    report("pool_hidden_from_ns0", !reach);

    /* pivot_root inside a mount namespace never touches ns 0 */
    pid = fork();
    if (pid == 0) {
        if (unshare(CLONE_NEWNS) != 0) _exit(2);
        errno = 0;
        long r = syscall(SYS_pivot_root, "/tmp", "/tmp");
        _exit(r == -1 && errno == EINVAL ? 0 : 1);
    }
    waitpid(pid, &st, 0);
    report("pivot_in_ns_einval", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    report("pivot_ns0_untouched", exists("/bin/mnttest") && exists("/dev/shm"));

    /* MS_PRIVATE / MS_SLAVE are accepted no-ops; MS_SHARED is not */
    pid = fork();
    if (pid == 0) {
        if (setgid(65534) || setuid(65534)) _exit(99);
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS) != 0) _exit(2);
        int ok = mount_("none", "/", NULL, M_REC | MS_PRIVATE_) == 0
              && mount_("none", "/", NULL, M_REC | MS_SLAVE_) == 0
              && mount_("none", "/", NULL, MS_PRIVATE_) == 0;
        _exit(ok ? 0 : 1);
    }
    waitpid(pid, &st, 0);
    report("ns_private_slave_noop", WIFEXITED(st) && WEXITSTATUS(st) == 0);

    /* clone(NEWNS|VM|VFORK) as Linux allows */
    static char vmstack[16384] __attribute__((aligned(16)));
    pid = (pid_t)clone(vm_child, vmstack + sizeof vmstack, CLONE_NEWNS | CLONE_VM | CLONE_VFORK | SIGCHLD, NULL);
    int okvm = 0;
    if (pid > 0 && waitpid(pid, &st, 0) > 0) okvm = WIFEXITED(st) && WEXITSTATUS(st) == 42;
    report("clone_newns_vm", okvm);

    /* /proc/<pid>/{root,cwd} of a task in another mount namespace is refused */
    int c2p[2], p2c[2];
    if (pipe(c2p) || pipe(p2c)) { report("proc_root_pipe", 0); return; }
    pid = fork();
    if (pid == 0) {
        char c = 'x';
        if (unshare(CLONE_NEWNS) != 0) _exit(2);
        write(c2p[1], &c, 1);
        read(p2c[0], &c, 1);
        _exit(0);
    }
    char c;
    int up = read(c2p[0], &c, 1) == 1;
    char pp[64];
    int e1 = 0, e2 = 0, e3 = 0, f1;
    snprintf(pp, sizeof pp, "/proc/%d/root/etc", (int)pid);
    errno = 0; f1 = open(pp, O_RDONLY); e1 = errno;
    int bad1 = f1 >= 0; if (f1 >= 0) close(f1);
    snprintf(pp, sizeof pp, "/proc/%d/cwd", (int)pid);
    errno = 0; int bad2 = chdir(pp) == 0; e2 = errno;
    if (bad2) chdir("/");
    snprintf(pp, sizeof pp, "/proc/%d/root", (int)pid);
    errno = 0; f1 = open(pp, O_RDONLY); e3 = errno;
    int bad3 = f1 >= 0; if (f1 >= 0) close(f1);
    report("proc_root_cross_ns_refused", up && !bad1 && !bad2 && !bad3
           && (e1 == EACCES || e1 == ENOENT) && (e2 == EACCES || e2 == ENOENT) && (e3 == EACCES || e3 == ENOENT));
    report("proc_root_same_ns_ok", exists("/proc/self/root/etc"));
    write(p2c[1], &c, 1);
    waitpid(pid, &st, 0);
    close(c2p[0]); close(c2p[1]); close(p2c[0]); close(p2c[1]);

    /* ns 0 cost: a dead namespace is freed by the wait that reaps its task */
    pid = fork();
    if (pid == 0) _exit(unshare(CLONE_NEWNS) == 0 ? 0 : 1);
    waitpid(pid, &st, 0);
    /* The task's reference can drop a moment after wait returns on SMP; each
     * further wait reaps. ns 0 pays nothing meanwhile (MNT_LIVE is already 0). */
    int reaped = in_ns_dying0();
    for (k = 0; !reaped && k < 100; k++) {
        usleep(20000);
        pid = fork();
        if (pid == 0) _exit(0);
        waitpid(pid, &st, 0);
        reaped = in_ns_dying0();
    }
    report("dead_ns_reaped_on_wait", reaped);
}

/* ---- Pool reachability via /proc magic links, fd-relative paths, symlinks ---- */
#ifndef O_PATH
#define O_PATH 010000000
#endif

/* A live pool instance holding "marker" must exist while the checks run: a
 * sibling task keeps a tmpfs mounted in its own namespace. A check passes when
 * no instance number 1..79 is reachable through the given route. */
static int pool_hit(const char *fmt_prefix) {
    char p[160];
    for (int m = 1; m < 80; m++) {
        snprintf(p, sizeof p, "%s.leandros-sb/tmpfs/%d/marker", fmt_prefix, m);
        int fd = open(p, O_RDONLY);
        if (fd >= 0) { close(fd); return 1; }
    }
    return 0;
}

static int pool_proc_ok(void) {
    return !pool_hit("/proc/self/root/") && !pool_hit("/") ;
}
static int pool_cwd_ok(void) {
    if (chdir("/") != 0) return 0;
    return !pool_hit("/proc/self/cwd/");
}
static int pool_fd_ok(void) {
    int dfd = open("/", O_RDONLY | O_DIRECTORY);
    if (dfd < 0) return 0;
    int ok = 1;
    char p[96];
    for (int m = 1; m < 80 && ok; m++) {
        snprintf(p, sizeof p, ".leandros-sb/tmpfs/%d/marker", m);
        int f = openat(dfd, p, O_RDONLY); if (f >= 0) { close(f); ok = 0; }
    }
    snprintf(p, sizeof p, "/proc/self/fd/%d/", dfd);
    if (pool_hit(p)) ok = 0;
    if (fchdir(dfd) != 0) ok = 0;
    else if (pool_hit("")) ok = 0;
    close(dfd);
    chdir("/");
    return ok;
}

/* Each check runs in ns 0 (mode 0) or in a fresh mount namespace with a tmpfs (mode 1). */
static int pool_in_child(int mode, int (*fn)(void)) {
    pid_t pid = fork();
    if (pid == 0) {
        if (mode) {
            if (unshare(CLONE_NEWNS) != 0) _exit(2);
            mkdir("/mnt", 0755);
            if (mount("t", "/mnt", "tmpfs", 0, NULL) != 0) _exit(3);
            { int mf = open("/mnt/marker", O_CREAT | O_WRONLY, 0666); if (mf >= 0) close(mf); }
        }
        _exit(fn() ? 0 : 1);
    }
    int st = 0;
    if (pid < 0 || waitpid(pid, &st, 0) < 0) return 0;
    return WIFEXITED(st) && WEXITSTATUS(st) == 0;
}

static int cross_symlink_ok(void) {
    mkdir("/mnt/ia", 0755); mkdir("/mnt/ib", 0755);
    if (mount("a", "/mnt/ia", "tmpfs", 0, NULL) || mount("b", "/mnt/ib", "tmpfs", 0, NULL)) return 0;
    int f = open("/mnt/ib/secret", O_CREAT | O_WRONLY, 0666);
    if (f < 0) return 0;
    close(f);
    /* The secret exists in ib only; no link inside ia may ever reach it. */
    for (int m = 1; m < 80; m++) {
        char body[96];
        for (int abs = 0; abs < 2; abs++) {
            if (abs) snprintf(body, sizeof body, "/.leandros-sb/tmpfs/%d/secret", m);
            else snprintf(body, sizeof body, "../%d/secret", m);
            unlink("/mnt/ia/l");
            if (symlink(body, "/mnt/ia/l") != 0) return 0;
            int g = open("/mnt/ia/l", O_RDONLY);
            if (g >= 0) { close(g); return 0; }
        }
    }
    return 1;
}

/* lane conmondbg: stat/chmod of /proc/self/fd/N for a tmpfs-instance directory
 * fd act on that directory only; no spelling reaches a sibling instance or the
 * raw pool path, and the link text never names the pool. */
static int procfd_tmpfs_dir_ok(void) {
    int dfd = open("/mnt", O_RDONLY | O_DIRECTORY);
    if (dfd < 0) return 0;
    char p[96], q[160], lnk[256];
    struct stat a, b;
    snprintf(p, sizeof p, "/proc/self/fd/%d", dfd);
    if (stat(p, &a) != 0 || !S_ISDIR(a.st_mode)) return 0;
    if (fstat(dfd, &b) != 0 || a.st_ino != b.st_ino) return 0;
    if (chmod(p, 0711) != 0) return 0;
    if (stat("/mnt", &b) != 0 || (b.st_mode & 0777) != 0711) return 0;
    ssize_t n = readlink(p, lnk, sizeof lnk - 1);
    if (n > 0) { lnk[n] = 0; if (strstr(lnk, ".leandros-sb")) return 0; }
    for (int m = 1; m < 80; m++) {
        /* "..": lexically folded, so /proc/self/fd/<m> for m == dfd is this very dir */
        snprintf(q, sizeof q, "%s/../%d/marker", p, m);
        if (m != dfd && stat(q, &b) == 0) return 0;
        snprintf(q, sizeof q, "%s/.leandros-sb/tmpfs/%d/marker", p, m);
        if (stat(q, &b) == 0) return 0;
        snprintf(q, sizeof q, "%s/../.leandros-sb/tmpfs/%d/marker", p, m);
        if (stat(q, &b) == 0) return 0;
        snprintf(q, sizeof q, "/.leandros-sb/tmpfs/%d", m);
        if (chmod(q, 0777) == 0) return 0;
    }
    close(dfd);
    return 1;
}

static int mountinfo_nopool_ok(void) {
    static char b[16384];
    for (int k = 0; k < 2; k++) {
        if (slurp(k ? "/proc/self/mounts" : "/proc/self/mountinfo", b, sizeof b) <= 0) return 0;
        if (strstr(b, ".leandros-sb")) return 0;
    }
    return 1;
}

static void pool_tests(void) {
    int hp[2], rp[2];
    pid_t holder = -1;
    if (pipe(hp) == 0 && pipe(rp) == 0 && (holder = fork()) == 0) {
        char c = 'x';
        if (unshare(CLONE_NEWNS) != 0) _exit(2);
        mkdir("/mnt", 0755);
        if (mount("h", "/mnt", "tmpfs", 0, NULL) != 0) _exit(3);
        int f = open("/mnt/marker", O_CREAT | O_WRONLY, 0666);
        if (f < 0) _exit(4);
        close(f);
        write(hp[1], &c, 1);
        read(rp[0], &c, 1);
        _exit(0);
    }
    char hc;
    int up = holder > 0 && read(hp[0], &hc, 1) == 1;
    report("pool_holder_up", up);
    report("pool_via_proc_root_ns0", pool_in_child(0, pool_proc_ok));
    report("pool_via_proc_root_mntns", pool_in_child(1, pool_proc_ok));
    report("pool_via_proc_cwd_ns0", pool_in_child(0, pool_cwd_ok));
    report("pool_via_proc_cwd_mntns", pool_in_child(1, pool_cwd_ok));
    report("pool_via_fd_relative_ns0", pool_in_child(0, pool_fd_ok));
    report("pool_via_fd_relative_mntns", pool_in_child(1, pool_fd_ok));
    report("pool_cross_instance_symlink", pool_in_child(1, cross_symlink_ok));
    report("pool_procfd_tmpfs_dir", pool_in_child(1, procfd_tmpfs_dir_ok));
    report("mountinfo_no_pool_path", pool_in_child(1, mountinfo_nopool_ok));
    if (holder > 0) { write(rp[1], &hc, 1); waitpid(holder, NULL, 0); }
}

/* lane mountperm: CAP_SYS_ADMIN in a child userns must NOT allow mounting into
 * the initial mount namespace (owned by the initial userns). */
static void wr(const char *p, const char *v) { int fd = open(p, O_WRONLY); if (fd >= 0) { if (write(fd, v, strlen(v)) < 0) {} close(fd); } }

static int userns_child(void) {
    static char before[16384], after[16384];
    if (setgid(1000) < 0 || setuid(1000) < 0) return 90;
    if (unshare(CLONE_NEWUSER) < 0) return 91;
    wr("/proc/self/setgroups", "deny");
    wr("/proc/self/uid_map", "0 1000 1");
    wr("/proc/self/gid_map", "0 1000 1");
    if (slurp("/proc/self/mountinfo", before, sizeof before) <= 0) return 92;
    const char *d = "/tmp/mountperm.d";
    static const char *fs[] = { "devpts", "mqueue", "tmpfs", "cgroup2" };
    for (int i = 0; i < 4; i++) {
        errno = 0;
        int r = mount("none", d, fs[i], 0, NULL);
        if (!(r < 0 && errno == EPERM)) { if (r == 0) umount(d); return 10 + i; }
    }
    errno = 0;
    int r = mount("/etc", d, NULL, MS_BIND, NULL);
    if (!(r < 0 && errno == EPERM)) { if (r == 0) umount(d); return 20; }
    if (slurp("/proc/self/mountinfo", after, sizeof after) <= 0) return 93;
    return strcmp(before, after) ? 94 : 0;
}

static void mount_userns_perm(void) {
    mkdir("/tmp/mountperm.d", 0777);
    chmod("/tmp/mountperm.d", 0777);
    pid_t c = fork();
    if (c == 0) _exit(userns_child());
    int st = 0; waitpid(c, &st, 0);
    if (!(WIFEXITED(st) && WEXITSTATUS(st) == 0)) printf("userns_child status=%d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    report("userns_root_mount_eperm", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    int r = mount("none", "/tmp/mountperm.d", "devpts", 0, NULL);
    report("host_root_devpts_ok", r == 0);
    if (r == 0) umount("/tmp/mountperm.d");
    rmdir("/tmp/mountperm.d");
}


/* setns(CLONE_NEWNS) (lane mntsetns, M7). A helper child unshares into its own
 * mount namespace and mounts a tmpfs; joiners are forked from the parent so the
 * parent's own namespace never changes. */
#ifndef SYS_setns
#define SYS_setns 268
#endif
#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif
static int setns_(int fd, int t) { return (int)syscall(SYS_setns, fd, t); }

static int same_link(const char *a, const char *b) {
    char x[64] = {0}, y[64] = {0};
    ssize_t n = readlink(a, x, sizeof x - 1), m = readlink(b, y, sizeof y - 1);
    return n > 0 && m > 0 && n == m && !memcmp(x, y, (size_t)n);
}

/* mode: 0 join via fd, 1 root/cwd reset, 2 unprivileged, 3 shared fs, 4 pidfd */
static int joiner(pid_t target, const char *d, int mode) {
    char np[64], tx[160];
    snprintf(np, sizeof np, "/proc/%d/ns/mnt", (int)target);
    snprintf(tx, sizeof tx, "%s/x", d);
    char pid0[64] = {0}, usr0[64] = {0}, pidn[64] = {0}, usrn[64] = {0};
    ssize_t n;
    n = readlink("/proc/self/ns/pid", pid0, sizeof pid0 - 1); if (n > 0) pid0[n] = 0;
    n = readlink("/proc/self/ns/user", usr0, sizeof usr0 - 1); if (n > 0) usr0[n] = 0;
    if (exists(tx)) return 10; /* must not be visible before the join */
    int fd, pfd = -1;
    if (mode == 4) {
        pfd = (int)syscall(SYS_pidfd_open, (long)target, 0L);
        if (pfd < 0) return 13;
    } else {
        fd = open(np, O_RDONLY);
        if (fd < 0) return 14;
        pfd = fd;
    }
    if (mode == 1) {
        if (chdir(d) || chroot(d)) return 11;   /* root and cwd inside the old tree */
        if (exists("/bin/mnttest")) return 12;
    }
    if (mode == 2) {
        if (setgid(65534) || setuid(65534)) return 15;
        errno = 0;
        return setns_(pfd, CLONE_NEWNS) == -1 && errno == EPERM ? 0 : 16;
    }
    int cpipe[2] = {-1, -1};
    pid_t kid = 0;
    if (mode == 3) {
        if (pipe(cpipe)) return 17;
        kid = (pid_t)syscall(SYS_clone, (long)(CLONE_FS | SIGCHLD), 0L, 0L, 0L, 0L);
        if (kid == 0) { char c; close(cpipe[1]); (void)!read(cpipe[0], &c, 1); _exit(0); }
        if (kid < 0) return 18;
        errno = 0;
        int r = setns_(pfd, CLONE_NEWNS);
        int e = errno;
        close(cpipe[1]); waitpid(kid, NULL, 0);
        return r == -1 && e == EINVAL && !exists(tx) ? 0 : 19;
    }
    if (setns_(pfd, CLONE_NEWNS) != 0) return 20;
    if (!exists(tx)) return 21;                                  /* sees the child's tmpfs */
    if (!same_link("/proc/self/ns/mnt", np)) return 22;
    n = readlink("/proc/self/ns/pid", pidn, sizeof pidn - 1); if (n > 0) pidn[n] = 0;
    n = readlink("/proc/self/ns/user", usrn, sizeof usrn - 1); if (n > 0) usrn[n] = 0;
    if (strcmp(pid0, pidn) || strcmp(usr0, usrn)) return 23;    /* pid/user ns untouched */
    if (mode == 1) {
        char cw[64] = {0};
        if (!getcwd(cw, sizeof cw) || strcmp(cw, "/")) return 24;
        if (!exists("/bin/mnttest")) return 25;                   /* chroot is gone */
    }
    return 0;
}

static int run_joiner(pid_t target, const char *d, int mode) {
    pid_t j = fork();
    if (j == 0) _exit(joiner(target, d, mode));
    int st = 0; waitpid(j, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 200;
}

static void setns_mnt_suite(void) {
    char base[64], d[128], q[128];
    snprintf(base, sizeof base, "/tmp/ms.%d", (int)getpid());
    dir(base); path(d, sizeof d, base, "d"); dir(d);
    path(q, sizeof q, base, "c"); dir(q);
    int c2p[2], p2c[2];
    if (pipe(c2p) || pipe(p2c)) { report("setns_mnt_pipe", 0); return; }
    pid_t t = fork();
    if (t == 0) {
        close(c2p[0]); close(p2c[1]);
        char ch = 1, x[160];
        if (unshare(CLONE_NEWNS)) _exit(90);
        if (mount_("none", d, "tmpfs", 0)) _exit(91);
        snprintf(x, sizeof x, "%s/x", d); put(x, "T", 0666);
        (void)!write(c2p[1], &ch, 1);
        (void)!read(p2c[0], &ch, 1);
        _exit(0);
    }
    close(c2p[1]); close(p2c[0]);
    char ch;
    if (read(c2p[0], &ch, 1) != 1) { report("setns_mnt_child_ready", 0); return; }
    int r;
    r = run_joiner(t, d, 0); if (r) printf("join=%d\n", r);
    report("setns_mnt_join", r == 0);
    r = run_joiner(t, d, 1); if (r) printf("root_cwd=%d\n", r);
    report("setns_mnt_root_cwd_reset", r == 0);
    r = run_joiner(t, d, 2); if (r) printf("eperm=%d\n", r);
    report("setns_mnt_eperm_unpriv", r == 0);
    r = run_joiner(t, d, 3); if (r) printf("clonefs=%d\n", r);
    report("setns_mnt_clone_fs_einval", r == 0);
    r = run_joiner(t, d, 4); if (r) printf("pidfd=%d\n", r);
    report("setns_mnt_pidfd", r == 0);
    /* the fd keeps the namespace alive after its creator exits */
    {
        char np[64]; snprintf(np, sizeof np, "/proc/%d/ns/mnt", (int)t);
        int fd = open(np, O_RDONLY);
        (void)!write(p2c[1], &ch, 1);
        int st = 0; waitpid(t, &st, 0);
        int ok = fd >= 0;
        if (ok) {
            pid_t j = fork();
            if (j == 0) {
                if (setns_(fd, CLONE_NEWNS)) _exit(1);
                char x[160]; snprintf(x, sizeof x, "%s/x", d);
                _exit(exists(x) ? 0 : 2);
            }
            waitpid(j, &st, 0);
            ok = WIFEXITED(st) && WEXITSTATUS(st) == 0;
        }
        report("setns_mnt_fd_keeps_alive", ok);
        if (fd >= 0) close(fd);
    }
    close(c2p[0]); close(p2c[1]);
}

/* lane podsys: tmpfs mountable on any directory in the initial mount
 * namespace (podman's shm, crun's /dev), options honoured, umount frees it. */
static int tfs_unpriv_child(void) {
    if (setgid(1000) < 0 || setuid(1000) < 0) return 90;
    if (unshare(CLONE_NEWUSER) < 0) return 91;
    wr("/proc/self/setgroups", "deny");
    wr("/proc/self/uid_map", "0 1000 1");
    wr("/proc/self/gid_map", "0 1000 1");
    errno = 0;
    int r = mount("shm", "/root/mt-tfs.d", "tmpfs", 0, "mode=1777");
    if (r == 0) { umount("/root/mt-tfs.d"); return 10; }
    return errno == EPERM ? 0 : 11;
}

/* devpts instance: pty index of a fresh <mnt>/ptmx (-1 on failure); *mfd gets the master. */
static int dpts_open(const char *d, int *mfd) {
    char f[128]; unsigned n = 0;
    snprintf(f, sizeof f, "%s/ptmx", d);
    int fd = open(f, O_RDWR | O_NOCTTY);
    if (fd < 0) return -1;
    if (ioctl(fd, 0x80045430 /* TIOCGPTN */, &n) != 0) { close(fd); return -1; }
    { int z = 0; ioctl(fd, 0x40045431 /* TIOCSPTLCK */, &z); }
    *mfd = fd;
    return (int)n;
}

static void devpts_copies(void) {
    const char *d = "/root/mt-dpts.d";
    int up[2], dn[2], m0 = -1;
    mkdir(d, 0755);
    if (mount("none", d, "devpts", 0, "ptmxmode=0666") != 0 || pipe(up) != 0 || pipe(dn) != 0) {
    report("devpts_copy_keeps_instance", 0);
        report("devpts_no_reuse_after_umount", 0);
        return;
    }
    int n0 = dpts_open(d, &m0);
    pid_t cp = fork();
    if (cp == 0) {
        char c = 0, f[128];
        int res = 0, m1 = -1;
        if (unshare(CLONE_NEWNS) != 0) _exit(128);
        write(up[1], "r", 1);
        read(dn[0], &c, 1);
        int n1 = dpts_open(d, &m1);
        /* the old pair is still live in the old instance: next index is 1 */
        if (n1 == 1) res |= 1;
        snprintf(f, sizeof f, "%s/%d", d, n0);
        int sfd = open(f, O_RDWR | O_NOCTTY);
        if (sfd >= 0) res |= 4;
        _exit(res);
    }
    char c;
    read(up[0], &c, 1);
    int ur = umount(d);
    int m2 = -1, mr = mount("none", d, "devpts", 0, "ptmxmode=0666");
    int n2 = (mr == 0) ? dpts_open(d, &m2) : -1;
    write(dn[1], "g", 1);
    int stt = 0;
    waitpid(cp, &stt, 0);
    int code = WIFEXITED(stt) ? WEXITSTATUS(stt) : 255;
    report("devpts_copy_keeps_instance", n0 == 0 && ur == 0 && (code & 1) && (code & 4));
    /* the new instance has a fresh index space (0), not the old one's (1) */
    report("devpts_no_reuse_after_umount", mr == 0 && n2 == 0);
    if (m2 >= 0) close(m2);
    if (m0 >= 0) close(m0);
    umount(d);
    close(up[0]); close(up[1]); close(dn[0]); close(dn[1]);
    rmdir(d);
}

static void tmpfs_anywhere(void) {
    const char *d = "/root/mt-tfs.d";
    int live0 = 0, sb0 = 0, live1 = 0, sb1 = 0;
    char buf[8192];
    struct stat st0, st1;
    int n;
    /* /proc/sys/kernel entries podman/crun read */
    n = slurp("/proc/sys/kernel/cap_last_cap", buf, sizeof buf);
    report("proc_cap_last_cap", n > 0 && atoi(buf) == 40);
    n = slurp("/proc/sys/kernel/ngroups_max", buf, sizeof buf);
    report("proc_ngroups_max", n > 0 && atoi(buf) > 0);
    mkdir(d, 0755);
    ns_counts(&live0, &sb0);
    stat(d, &st0);
    int r = mount("shm", d, "tmpfs", MS_NOSUID | MS_NODEV, "mode=1777,size=65536k");
    report("tfs_mount_arbitrary_dir", r == 0);
    if (r != 0) { rmdir(d); return; }
    char f[128];
    snprintf(f, sizeof f, "%s/a", d);
    put(f, "hello", 0644);
    n = slurp(f, buf, sizeof buf);
    report("tfs_content_independent", n == 5 && !strcmp(buf, "hello") && stat(d, &st1) == 0 && st1.st_dev != st0.st_dev);
    report("tfs_mode_default_opts", stat(d, &st1) == 0 && (st1.st_mode & 07777) == 01777);
    struct statfs sf;
    report("tfs_nosuid_flag", statfs(d, &sf) == 0 && (sf.f_flags & 2));
    ns_counts(&live1, &sb1);
    report("tfs_sb_registered", sb1 == sb0 + 1);
    r = umount(d);
    report("tfs_umount_ok", r == 0);
    report("tfs_umount_frees", stat(f, &st1) != 0 && ns_counts(&live1, &sb1) == 0 && sb1 == sb0);
    /* a second umount is not a mountpoint */
    errno = 0;
    report("tfs_umount_twice_einval", umount(d) < 0 && errno == EINVAL);

    /* a namespace copy keeps the instance alive and is isolated from reuse */
    {
        int up[2], dn[2];
        char fa[128], fb[128];
        snprintf(fa, sizeof fa, "%s/a", d);
        snprintf(fb, sizeof fb, "%s/b", d);
        if (mount("shm", d, "tmpfs", 0, "") == 0 && pipe(up) == 0 && pipe(dn) == 0) {
            put(fa, "old", 0644);
            pid_t cp = fork();
            if (cp == 0) {
                char c = 0, b2[64];
                int res = 0;
                if (unshare(CLONE_NEWNS) != 0) _exit(128);
                write(up[1], "r", 1);
                read(dn[0], &c, 1);
                int m = slurp(fa, b2, sizeof b2);
                if (m == 3 && !strcmp(b2, "old")) res |= 1;
                if (access(fb, F_OK) != 0) res |= 2;
                _exit(res);
            }
            char c;
            read(up[0], &c, 1);
            int ur = umount(d);
            int mr = mount("shm", d, "tmpfs", 0, "");
            if (mr == 0) put(fb, "new", 0644);
            write(dn[1], "g", 1);
            int stt = 0;
            waitpid(cp, &stt, 0);
            int code = WIFEXITED(stt) ? WEXITSTATUS(stt) : 255;
            report("tfs_copy_keeps_instance", ur == 0 && (code & 1));
            report("tfs_no_reuse_after_umount", mr == 0 && (code & 2));
            umount(d);
            close(up[0]); close(up[1]); close(dn[0]); close(dn[1]);
        } else {
            report("tfs_copy_keeps_instance", 0);
            report("tfs_no_reuse_after_umount", 0);
        }
    }

    /* options: mode/uid/gid/size */
    r = mount("shm", d, "tmpfs", 0, "mode=0750,uid=1234,gid=5678,size=64k");
    report("tfs_opts_mount", r == 0);
    if (r == 0) {
        report("tfs_opts_mode_uid_gid", stat(d, &st1) == 0 && (st1.st_mode & 07777) == 0750 && st1.st_uid == 1234 && st1.st_gid == 5678);
        snprintf(f, sizeof f, "%s/big", d);
        int fd = open(f, O_WRONLY | O_CREAT, 0644);
        static char blk[4096];
        long total = 0; int err = 0;
        for (int i = 0; i < 40 && fd >= 0; i++) {
            ssize_t w = write(fd, blk, sizeof blk);
            if (w < 0) { err = errno; break; }
            total += w;
        }
        if (fd >= 0) close(fd);
        report("tfs_size_enforced", total == 65536 && err == ENOSPC);
        umount(d);
    }

    /* noexec: a copy of this binary will not run from it */
    r = mount("shm", d, "tmpfs", MS_NOEXEC, "mode=755");
    report("tfs_noexec_mount", r == 0);
    if (r == 0) {
        snprintf(f, sizeof f, "%s/x", d);
        int in = open("/bin/mnttest", O_RDONLY), out = open(f, O_WRONLY | O_CREAT, 0755);
        static char cb[65536]; ssize_t k;
        while (in >= 0 && out >= 0 && (k = read(in, cb, sizeof cb)) > 0) if (write(out, cb, (size_t)k) != k) break;
        if (in >= 0) close(in);
        if (out >= 0) close(out);
        pid_t c = fork();
        if (c == 0) { execl(f, f, "child-ok", (char *)NULL); _exit(errno == EACCES ? 0 : 1); }
        int st = 0; waitpid(c, &st, 0);
        report("tfs_noexec_eacces", WIFEXITED(st) && WEXITSTATUS(st) == 0);
        umount(d);
    }

    /* an unprivileged user namespace cannot mount it in ns 0 */
    pid_t c = fork();
    if (c == 0) _exit(tfs_unpriv_child());
    int st = 0; waitpid(c, &st, 0);
    if (!(WIFEXITED(st) && WEXITSTATUS(st) == 0)) printf("tfs_unpriv status=%d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    report("tfs_userns_eperm_ns0", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    rmdir(d);

}


/* netns lane (netnsfix): unshare(CLONE_NEWNET) alone / threaded / unprivileged,
 * self-bind + make-rshared, nsfs bind mounts keeping a netns alive. */
#include <pthread.h>
#ifndef CLONE_NEWNET
#define CLONE_NEWNET 0x40000000
#endif
static void *nn_thread(void *a) { *(int *)a = unshare(CLONE_NEWNET) == 0 ? 0 : errno; return NULL; }
static int nn_threaded(void) {
    int res = -1; pthread_t t;
    if (pthread_create(&t, NULL, nn_thread, &res)) return 99;
    pthread_join(t, NULL);
    return res;
}
static int nn_unpriv(int mode) {
    if (setgid(1000) < 0 || setuid(1000) < 0) return 90;
    errno = 0;
    switch (mode) {
    case 0: return unshare(CLONE_NEWNET) < 0 && errno == EPERM ? 0 : 10;
    case 1: return unshare(CLONE_NEWUSER | CLONE_NEWNET) == 0 ? 0 : 11;
    case 2: return unshare(CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWNET) == 0 ? 0 : 12;
    case 3:
        if (unshare(CLONE_NEWUSER) < 0) return 91;
        wr("/proc/self/setgroups", "deny"); wr("/proc/self/uid_map", "0 1000 1"); wr("/proc/self/gid_map", "0 1000 1");
        return unshare(CLONE_NEWNET) == 0 ? 0 : 13;
    case 4:
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS) < 0) return 91;
        wr("/proc/self/setgroups", "deny"); wr("/proc/self/uid_map", "0 1000 1"); wr("/proc/self/gid_map", "0 1000 1");
        return unshare(CLONE_NEWNET | CLONE_NEWUTS | CLONE_NEWIPC) == 0 ? 0 : 14;
    }
    return 98;
}
static int nn_in_child(int (*fn)(int), int arg) {
    pid_t c = fork();
    if (c == 0) _exit(fn(arg));
    int st = 0; waitpid(c, &st, 0);
    if (!(WIFEXITED(st) && WEXITSTATUS(st) == 0)) printf("  child status=%d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    return WIFEXITED(st) && WEXITSTATUS(st) == 0;
}
static int nn_root(int m) {
    if (m == 0) return unshare(CLONE_NEWNET) == 0 ? 0 : 1;
    if (m == 1) return unshare(CLONE_NEWNET | CLONE_NEWNS | CLONE_NEWUTS | CLONE_NEWIPC) == 0 ? 0 : 2;
    return nn_threaded();
}
static int nn_join_check(const char *file) {
    int fd = open(file, O_RDONLY);
    if (fd < 0) return 1;
    struct stat a, b, before; char me[] = "/proc/self/ns/net";
    if (fstat(fd, &a) < 0) return 2;
    stat(me, &before);
    if (setns_(fd, CLONE_NEWNET) < 0) return 3;
    if (stat(me, &b) < 0) return 4;
    if (a.st_ino != b.st_ino || before.st_ino == b.st_ino) return 5;
    return 0;
}
static void netns_suite(void) {
    report("unshare_newnet_root", nn_in_child(nn_root, 0));
    report("unshare_newnet_all_flat_newns", nn_in_child(nn_root, 1));
    report("unshare_newnet_threaded", nn_in_child(nn_root, 2));
    report("unshare_newnet_unpriv_eperm", nn_in_child(nn_unpriv, 0));
    report("unshare_newuser_newnet_unpriv", nn_in_child(nn_unpriv, 1));
    report("unshare_newuser_newns_newnet_unpriv", nn_in_child(nn_unpriv, 2));
    report("unshare_newnet_in_userns", nn_in_child(nn_unpriv, 3));
    report("unshare_newnet_uts_ipc_in_userns_mntns", nn_in_child(nn_unpriv, 4));

    /* /run/netns: self-bind then make-rshared, as netavark and ip-netns do */
    mkdir("/run", 0755); mkdir("/run/netns", 0755);
    int okb = mount("/run/netns", "/run/netns", "none", MS_BIND | MS_REC, NULL) == 0;
    int oks = mount("", "/run/netns", NULL, MS_SHARED | MS_REC, NULL) == 0;
    static char mi[16384]; int shared = 0;
    if (slurp("/proc/self/mountinfo", mi, sizeof mi) > 0) {
        char *l = strstr(mi, " /run/netns ");
        if (l) { char *e = strchr(l, '\n'); char *sh = strstr(l, "shared:"); shared = sh && (!e || sh < e); }
    }
    report("netns_self_bind_ok", okb);
    report("netns_make_rshared_ok", oks);
    report("netns_make_rshared_visible", shared);

    /* nsfs bind mount keeps a netns alive after its creator exits */
    int p[2], q[2];
    if (pipe(p) || pipe(q)) { report("netns_nsfs_bind", 0); return; }
    pid_t c = fork();
    if (c == 0) {
        char ch;
        if (unshare(CLONE_NEWNET) < 0) _exit(1);
        if (write(p[1], "x", 1) != 1) _exit(2);
        if (read(q[0], &ch, 1) < 0) _exit(3);
        _exit(0);
    }
    char ch; int ok = read(p[0], &ch, 1) == 1;
    char src[64], file[] = "/run/netns/nsfstest";
    snprintf(src, sizeof src, "/proc/%d/ns/net", (int)c);
    int fd = open(file, O_RDWR | O_CREAT, 0444); if (fd >= 0) close(fd);
    int br = mount(src, file, "none", MS_BIND, NULL);
    if (br < 0) printf("  nsfs bind errno=%d\n", errno);
    report("netns_nsfs_bind_mount", ok && br == 0);
    if (write(q[1], "x", 1) != 1) {}
    int st = 0; waitpid(c, &st, 0);
    pid_t j = fork();
    if (j == 0) _exit(br == 0 ? nn_join_check(file) : 77);
    int js = 0; waitpid(j, &js, 0);
    if (!(WIFEXITED(js) && WEXITSTATUS(js) == 0)) printf("  join status=%d\n", WIFEXITED(js) ? WEXITSTATUS(js) : -1);
    report("netns_nsfs_bound_survives_and_setns", WIFEXITED(js) && WEXITSTATUS(js) == 0);
    umount2(file, MNT_DETACH); unlink(file);
    umount2("/run/netns", MNT_DETACH); rmdir("/run/netns");
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "bench")) { bench(); return 0; }
    if (argc > 1 && !strcmp(argv[1], "child-ok")) return 42;
    newapi_enosys();
    kernel_selftest();
    sb_hidden_ns0();
    ns0_invariants();
    newns_suite();
    hardening_suite();
    mountinfo_tests();
    pool_tests();
    mount_userns_perm();
    setns_mnt_suite();
    tmpfs_anywhere();
    netns_suite();
    devpts_copies();
    printf("%s\n", fails ? "mnttest: FAILED" : "mnttest: ALL PASSED");
    return fails ? 1 : 0;
}
