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

/* New mount API (lane newmntapi, M8): fsopen and friends return ENOSYS
 * (open_tree/move_mount/mount_setattr are real since lane opentree). */
static void newapi_enosys(void) {
    static const struct { const char *n; long nr; } c[] = {
        {"fsopen", 430}, {"fsconfig", 431}, {"fsmount", 432}, {"fspick", 433} };
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

/* ---- lane mntfdpath: mount(2) on /proc/self/fd/N, crun's make-private walk,
 * nsfs binds inside a non-initial mount namespace, binds copied by NEWNS ---- */
static int mount_fd_private(int fd) {
    char p[40]; snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    return mount(NULL, p, NULL, MS_PRIVATE, NULL);
}
/* crun make_parent_mount_private: open the rootfs O_PATH and mount MS_PRIVATE on
 * its /proc/self/fd link; on EINVAL walk to ".." until a mount point accepts.
 * Returns the number of steps taken, -1 if the walk never succeeds. */
static int crun_make_private_walk(const char *rootfs) {
    int fd = open(rootfs, O_PATH | O_DIRECTORY | O_CLOEXEC);
    if (fd < 0) return -2;
    for (int step = 0; step < 32; step++) {
        errno = 0;
        if (mount_fd_private(fd) == 0) { close(fd); return step; }
        if (errno != EINVAL) { printf("  walk step %d errno=%d\n", step, errno); close(fd); return -1; }
        int up = openat(fd, "..", O_PATH | O_DIRECTORY | O_CLOEXEC);
        close(fd);
        if (up < 0) return -1;
        fd = up;
    }
    close(fd); return -1;
}
static int fdp_direct(int m) {
    (void)m;
    mkdir("/tmp/fdp.m", 0755);
    if (mount("/tmp/fdp.m", "/tmp/fdp.m", "none", MS_BIND, NULL) < 0) return 1;
    int fd = open("/tmp/fdp.m", O_PATH | O_DIRECTORY);
    if (fd < 0) return 2;
    int r = mount_fd_private(fd);
    close(fd);
    if (r < 0) return 3;
    mkdir("/tmp/fdp.m/a", 0755);
    fd = open("/tmp/fdp.m/a", O_PATH | O_DIRECTORY);
    if (fd < 0) return 4;
    errno = 0; r = mount_fd_private(fd);
    int e = errno; close(fd);
    if (!(r < 0 && e == EINVAL)) return 5;
    if (mount(NULL, "/proc/self/fd/999", NULL, MS_PRIVATE, NULL) == 0) return 6;
    return 0;
}
static int fdp_walk(int m) {
    (void)m;
    mkdir("/tmp/fdp.m", 0755); mkdir("/tmp/fdp.m/a", 0755); mkdir("/tmp/fdp.m/a/b", 0755);
    return crun_make_private_walk("/tmp/fdp.m/a/b") == 2 ? 0 : 1;
}
static int fdp_userns_mntns(int m) {
    (void)m;
    if (setgid(1000) < 0 || setuid(1000) < 0) return 90;
    /* made before the unshare: without idmap=test the new userns has no uid map */
    mkdir("/tmp/fdpu", 0755); mkdir("/tmp/fdpu/a", 0755); mkdir("/tmp/fdpu/a/b", 0755);
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS) < 0) return 91;
    wr("/proc/self/setgroups", "deny"); wr("/proc/self/uid_map", "0 1000 1"); wr("/proc/self/gid_map", "0 1000 1");
    if (mount("/tmp/fdpu", "/tmp/fdpu", "none", MS_BIND, NULL) < 0) { printf("  userns bind errno=%d\n", errno); return 1; }
    int w = crun_make_private_walk("/tmp/fdpu/a/b");
    if (w != 2) { printf("  userns walk=%d\n", w); return 2; }
    int fd = open("/", O_PATH | O_DIRECTORY);
    if (fd < 0 || mount_fd_private(fd) < 0) return 3;
    close(fd);
    return 0;
}
static int fdp_root_mntns(int m) {
    (void)m;
    if (unshare(CLONE_NEWNS) < 0) return 91;
    mkdir("/tmp/fdpr", 0755); mkdir("/tmp/fdpr/a", 0755);
    if (mount("/tmp/fdpr", "/tmp/fdpr", "none", MS_BIND, NULL) < 0) return 1;
    int w = crun_make_private_walk("/tmp/fdpr/a");
    if (w != 1) { printf("  root mntns walk=%d\n", w); return 2; }
    return 0;
}
/* nsbindtask: per-thread namespace links (Go/netavark LockOSThread + bind of
 * /proc/self/task/<tid>/ns/net onto an O_EXCL mode-0 file). */
static volatile int tb_tid, tb_state;           /* state: 0 start, 1 ready, 2 quit */
static volatile ino_t tb_ino;
static const char *tb_self_bind;                /* thread binds thread-self itself */
static volatile int tb_bind_rc;
static void *tb_thread(void *a) {
    (void)a;
    struct stat st;
    if (unshare(CLONE_NEWNET) < 0 || stat("/proc/thread-self/ns/net", &st) < 0) { tb_state = -1; return NULL; }
    tb_ino = st.st_ino; tb_tid = (int)syscall(SYS_gettid);
    if (tb_self_bind) {
        tb_bind_rc = mount("/proc/thread-self/ns/net", tb_self_bind, "none", MS_BIND, "") < 0 ? errno : 0;
    }
    tb_state = 1;
    while (tb_state == 1) usleep(1000);
    return NULL;
}
/* Go's containers/common netns bind passes MS_BIND|MS_REC|MS_SHARED together:
 * bind wins, the propagation bits are ignored (Linux do_mount order). */
static int nsfs_bind_with_shared(int mode) {
    if (mode == 1 && unshare(CLONE_NEWNS) < 0) return 89;
    mkdir("/run", 0755); mkdir("/run/netns", 0755);
    static char file[] = "/run/netns/netns-bs";
    int cf = open(file, O_RDONLY | O_CREAT | O_EXCL, 0); if (cf < 0) return 92; close(cf);
    int rc = 0;
    if (mount("/proc/self/ns/net", file, "none", MS_BIND | MS_REC | MS_SHARED, NULL) < 0) {
        printf("  bind|rec|shared errno=%d\n", errno); rc = 1;
    } else {
        struct stat a, b;
        if (stat(file, &a) < 0 || stat("/proc/self/ns/net", &b) < 0 || a.st_ino != b.st_ino) rc = 4;
        struct statfs sf;
        if (!rc && (statfs(file, &sf) < 0 || (unsigned long)sf.f_type != 0x6e736673UL)) rc = 5;
        int ffd = open(file, O_RDONLY);
        if (!rc && (ffd < 0 || fstatfs(ffd, &sf) < 0 || (unsigned long)sf.f_type != 0x6e736673UL)) rc = 6;
        if (ffd >= 0) close(ffd);
        if (umount(file) < 0 && !rc) rc = 7;
    }
    unlink(file);
    return rc;
}
static int nsfs_thread_selfbind(int mode) {
    if (mode == 1 && unshare(CLONE_NEWNS) < 0) return 89;
    mkdir("/run", 0755); mkdir("/run/netns", 0755);
    if (mount("/run/netns", "/run/netns", "none", MS_BIND | MS_REC, NULL) < 0) return 90;
    if (mount("", "/run/netns", NULL, MS_SHARED | MS_REC, NULL) < 0) return 91;
    static char file[] = "/run/netns/netns-tb";
    int cf = open(file, O_RDONLY | O_CREAT | O_EXCL, 0); if (cf < 0) return 92; close(cf);
    tb_self_bind = file; tb_state = 0;
    pthread_t th;
    if (pthread_create(&th, NULL, tb_thread, NULL)) return 93;
    for (int i = 0; i < 2000 && tb_state == 0; i++) usleep(1000);
    int rc = tb_state != 1 ? 94 : 0;
    if (!rc && tb_bind_rc) { printf("  thread-self bind errno=%d\n", tb_bind_rc); rc = 1; }
    if (!rc) {
        int fd = open(file, O_RDONLY); struct stat a;
        if (fd < 0 || fstat(fd, &a) < 0 || a.st_ino != tb_ino) rc = 4;
        if (fd >= 0) close(fd);
        if (umount(file) < 0 && !rc) rc = 7;
        if (unlink(file) < 0 && !rc) rc = 8;
    }
    tb_state = 2; pthread_join(th, NULL);
    umount2("/run/netns", MNT_DETACH);
    return rc;
}
static int nsfs_thread_bind(int mode) {
    char file[64], src[96];
    snprintf(file, sizeof file, "/tmp/nsfst.%d", (int)getpid());
    if (mode == 1) {
        if (setgid(1000) < 0 || setuid(1000) < 0) return 90;
        int pf = open(file, O_RDONLY | O_CREAT | O_EXCL, 0); if (pf < 0) return 89; close(pf);
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS) < 0) return 91;
        wr("/proc/self/setgroups", "deny"); wr("/proc/self/uid_map", "0 1000 1"); wr("/proc/self/gid_map", "0 1000 1");
    } else if (mode == 2 && unshare(CLONE_NEWNS) < 0) return 91;
    struct stat lead;
    if (stat("/proc/self/ns/net", &lead) < 0) return 92;
    pthread_t th;
    if (pthread_create(&th, NULL, tb_thread, NULL)) return 93;
    for (int i = 0; i < 2000 && tb_state == 0; i++) usleep(1000);
    if (tb_state != 1) return 94;
    int rc = 0;
    snprintf(src, sizeof src, "/proc/self/task/%d/ns/net", tb_tid);
    struct stat ts, ls;
    if (stat(src, &ts) < 0) { printf("  stat task ns errno=%d\n", errno); rc = 95; goto out; }
    if (ts.st_ino != tb_ino || ts.st_ino == lead.st_ino) { rc = 96; goto out; }
    snprintf(src, sizeof src, "/proc/%d/task/%d/ns/net", (int)getpid(), tb_tid);
    if (stat(src, &ls) < 0 || ls.st_ino != tb_ino) { rc = 97; goto out; }
    snprintf(src, sizeof src, "/proc/self/task/%d/ns/net", tb_tid);
    /* podman: O_CREAT|O_EXCL, mode 0 */
    if (mode != 1) { int cf = open(file, O_RDONLY | O_CREAT | O_EXCL, 0); if (cf < 0) { rc = 98; goto out; } close(cf); }
    if (mount(src, file, "none", MS_BIND, NULL) < 0) { printf("  thread nsfs bind errno=%d\n", errno); rc = 1; goto out; }
    int fd = open(file, O_RDONLY);
    struct stat a;
    if (fd < 0) { rc = 3; goto out_um; }
    if (fstat(fd, &a) < 0 || a.st_ino != tb_ino) { rc = 4; close(fd); goto out_um; }
    if (setns_(fd, CLONE_NEWNET) < 0) { rc = 5; close(fd); goto out_um; }
    close(fd);
    struct stat now;
    if (stat("/proc/thread-self/ns/net", &now) < 0 || now.st_ino != tb_ino) rc = 6;
out_um:
    if (umount(file) < 0 && rc == 0) rc = 7;
    if (unlink(file) < 0 && rc == 0) rc = 8;
out:
    tb_state = 2; pthread_join(th, NULL);
    return rc;
}
/* nsfs bind in a mount namespace of its own; mode 0 root, 1 rootless
 * (userns+mntns), 2 root + a further unshare(NEWNS) after the bind */
static int nsfs_in_mntns(int mode) {
    char file[64]; snprintf(file, sizeof file, "/tmp/nsfsm.%d", (int)getpid());
    if (mode == 1) {
        if (setgid(1000) < 0 || setuid(1000) < 0) return 90;
        int cf = open(file, O_RDWR | O_CREAT, 0444); if (cf < 0) return 97; close(cf);
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS) < 0) return 91;
        wr("/proc/self/setgroups", "deny"); wr("/proc/self/uid_map", "0 1000 1"); wr("/proc/self/gid_map", "0 1000 1");
    } else if (unshare(CLONE_NEWNS) < 0) return 91;
    if (unshare(CLONE_NEWNET) < 0) return 92;
    struct stat orig, a, b;
    if (stat("/proc/self/ns/net", &orig) < 0) return 93;
    int fd;
    if (mode != 1) { fd = open(file, O_RDWR | O_CREAT, 0444); if (fd < 0) return 94; close(fd); }
    if (mount("/proc/self/ns/net", file, "none", MS_BIND, NULL) < 0) { printf("  nsfs bind errno=%d\n", errno); return 1; }
    if (mode == 2 && unshare(CLONE_NEWNS) < 0) return 95;
    static char mi[16384];
    if (slurp("/proc/self/mountinfo", mi, sizeof mi) <= 0 || !strstr(mi, file)) return 2;
    if (unshare(CLONE_NEWNET) < 0) return 96;       /* the bind now holds the old netns alone */
    fd = open(file, O_RDONLY);
    if (fd < 0) return 3;
    if (fstat(fd, &a) < 0 || a.st_ino != orig.st_ino) return 4;
    if (setns_(fd, CLONE_NEWNET) < 0) return 5;
    if (stat("/proc/self/ns/net", &b) < 0 || b.st_ino != orig.st_ino) return 6;
    close(fd);
    if (umount(file) < 0) return 7;
    return 0;
}
/* Closing an nsfs fd must drop the namespace reference its open took. */
static int nsfd_close_child(int m) {
    (void)m;
    if (unshare(CLONE_NEWNET) < 0) return 92;
    for (int i = 0; i < 4; i++) {
        int fd = open("/proc/self/ns/net", O_RDONLY);
        if (fd < 0) return 93;
        int d = dup(fd);
        close(fd); close(d);
    }
    return 0;
}
static void nsfd_close_leak(void) {
    int l0, s0, l1, s1;
    ns_counts(&l0, &s0);
    int ok = 1;
    for (int i = 0; i < 100; i++) if (nn_in_child(nsfd_close_child, 0) != 1) ok = 0;
    ns_counts(&l1, &s1);
    if (l1 != l0) printf("  nsfd leak: live %d -> %d\n", l0, l1);
    report("nsfd_close_releases_namespace_refs", ok && l1 == l0);
}
static int selfbind_copied(int m) {
    (void)m;
    if (unshare(CLONE_NEWNS) < 0) return 91;
    static char mi[16384];
    if (slurp("/proc/self/mountinfo", mi, sizeof mi) <= 0) return 92;
    if (!strstr(mi, " /tmp/fdp.m ")) return 1;
    int fd = open("/tmp/fdp.m", O_PATH | O_DIRECTORY);
    if (fd < 0 || mount_fd_private(fd) < 0) return 2;
    if (mount("", "/tmp/fdp.m", NULL, MS_SHARED | MS_REC, NULL) < 0) return 3;
    return 0;
}
static int nsfs_ns0_copied(int m) {
    (void)m;
    const char *file = "/tmp/nsfs0.file";
    if (unshare(CLONE_NEWNS) < 0) return 91;
    static char mi[16384];
    if (slurp("/proc/self/mountinfo", mi, sizeof mi) <= 0 || !strstr(mi, file)) return 1;
    if (unshare(CLONE_NEWNS) < 0) return 92;          /* and once more */
    int r = nn_join_check(file);
    return r ? 10 + r : 0;
}
static void fdpath_suite(void) {
    nsfd_close_leak();
    report("mount_private_via_fd_link_root", nn_in_child(fdp_direct, 0));
    report("crun_make_private_walk_root", nn_in_child(fdp_walk, 0));
    report("crun_make_private_walk_mntns", nn_in_child(fdp_root_mntns, 0));
    report("crun_make_private_walk_userns_mntns", nn_in_child(fdp_userns_mntns, 0));
    report("selfbind_visible_after_unshare_newns", nn_in_child(selfbind_copied, 0));
    umount2("/tmp/fdp.m", MNT_DETACH);
    report("nsfs_bind_in_mntns_root", nn_in_child(nsfs_in_mntns, 0));
    report("nsfs_bind_in_userns_mntns", nn_in_child(nsfs_in_mntns, 1));
    report("nsfs_bind_in_mntns_then_unshare_newns", nn_in_child(nsfs_in_mntns, 2));
    report("nsfs_bind_with_shared_flag_root", nn_in_child(nsfs_bind_with_shared, 0));
    report("nsfs_bind_with_shared_flag_mntns", nn_in_child(nsfs_bind_with_shared, 1));
    report("nsfs_thread_self_bind_run_netns", nn_in_child(nsfs_thread_selfbind, 0));
    report("nsfs_thread_self_bind_run_netns_mntns", nn_in_child(nsfs_thread_selfbind, 1));
    report("nsfs_thread_task_bind_root", nn_in_child(nsfs_thread_bind, 0));
    report("nsfs_thread_task_bind_mntns", nn_in_child(nsfs_thread_bind, 2));
    report("nsfs_thread_task_bind_userns_mntns", nn_in_child(nsfs_thread_bind, 1));
    /* an ns-0 nsfs bind is copied into new mount namespaces */
    int p[2], q[2];
    if (pipe(p) || pipe(q)) { report("nsfs_ns0_bind_copied_by_unshare", 0); return; }
    pid_t c = fork();
    if (c == 0) {
        char ch;
        if (unshare(CLONE_NEWNET) < 0) _exit(1);
        if (write(p[1], "x", 1) != 1) _exit(2);
        if (read(q[0], &ch, 1) < 0) _exit(3);
        _exit(0);
    }
    char ch; int ok = read(p[0], &ch, 1) == 1;
    char src[64]; const char *file = "/tmp/nsfs0.file";
    snprintf(src, sizeof src, "/proc/%d/ns/net", (int)c);
    int fd = open(file, O_RDWR | O_CREAT, 0444); if (fd >= 0) close(fd);
    int br = mount(src, file, "none", MS_BIND, NULL);
    if (write(q[1], "x", 1) != 1) {}
    int st = 0; waitpid(c, &st, 0);
    report("nsfs_ns0_bind_copied_by_unshare", ok && br == 0 && nn_in_child(nsfs_ns0_copied, 0));
    umount2(file, MNT_DETACH); unlink(file);
}

/* ---- lane fdmounts: new-filesystem mounts onto /proc/self/fd/N (crun) ---- */
static int fdm_mount(const char *dir, const char *src, const char *fs, unsigned long fl, const char *data) {
    char t[64];
    int fd = open(dir, O_PATH | O_DIRECTORY | O_CLOEXEC);
    if (fd < 0) { printf("  open %s errno=%d\n", dir, errno); return -1; }
    snprintf(t, sizeof t, "/proc/self/fd/%d", fd);
    errno = 0;
    int r = mount(src, t, fs, fl, data);
    int e = errno;
    close(fd);
    if (r < 0) printf("  mount %s -> fd link errno=%d\n", fs, e);
    errno = e;
    return r;
}
static int fdm_has(const char *dir, const char *fs) {
    static char mi[32768]; char pat[64];
    if (slurp("/proc/self/mountinfo", mi, sizeof mi) <= 0) return 0;
    snprintf(pat, sizeof pat, " %s ", dir);
    for (char *l = strtok(mi, "\n"); l; l = strtok(NULL, "\n")) {
        char *d = strstr(l, pat), *h = strstr(l, " - ");
        if (d && h && !strncmp(h + 3, fs, strlen(fs))) return 1;
    }
    return 0;
}
static int fdm_all(void) {
    static const char *dirs[] = {"proc", "tmp", "sys", "pts", "mq", "cg"};
    char d[64];
    mkdir("/tmp/fdm", 0755);
    for (int i = 0; i < 6; i++) { snprintf(d, sizeof d, "/tmp/fdm/%s", dirs[i]); mkdir(d, 0755); }
    if (fdm_mount("/tmp/fdm/proc", "proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL) < 0) return 1;
    if (fdm_mount("/tmp/fdm/tmp", "tmpfs", "tmpfs", MS_NOSUID | MS_STRICTATIME, "mode=755,size=65536k") < 0) return 2;
    if (fdm_mount("/tmp/fdm/sys", "sysfs", "sysfs", MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL) < 0) return 3;
    if (fdm_mount("/tmp/fdm/pts", "devpts", "devpts", MS_NOSUID | MS_NOEXEC, "newinstance,ptmxmode=0666,mode=0620") < 0) return 4;
    if (fdm_mount("/tmp/fdm/mq", "mqueue", "mqueue", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL) < 0) return 5;
    if (fdm_mount("/tmp/fdm/cg", "cgroup2", "cgroup2", MS_NOSUID | MS_NODEV | MS_NOEXEC | MS_RELATIME, "nsdelegate") < 0) return 6;
    return 0;
}
static int fdm_check(void) {
    struct stat st;
    if (!fdm_has("/tmp/fdm/proc", "proc") || !fdm_has("/tmp/fdm/tmp", "tmpfs") || !fdm_has("/tmp/fdm/sys", "sysfs")
        || !fdm_has("/tmp/fdm/pts", "devpts") || !fdm_has("/tmp/fdm/mq", "mqueue") || !fdm_has("/tmp/fdm/cg", "cgroup2")) return 10;
    if (stat("/tmp/fdm/proc/self", &st) < 0) return 11;
    if (stat("/tmp/fdm/sys/kernel", &st) < 0 && stat("/tmp/fdm/sys/class", &st) < 0) return 12;
    if (stat("/tmp/fdm/pts/ptmx", &st) < 0) return 13;
    if (open("/tmp/fdm/tmp/x", O_CREAT | O_WRONLY, 0600) < 0) return 14;
    if (open("/tmp/fdm/sys/kernel/zz", O_CREAT | O_WRONLY, 0600) >= 0) return 15;   /* ro */
    return 0;
}
static int fdm_root_ns0(int m) { (void)m; int r = fdm_all(); return r ? r : fdm_check(); }
static int fdm_root_mntns(int m) { (void)m; if (unshare(CLONE_NEWNS) < 0) return 91; int r = fdm_all(); return r ? r : fdm_check(); }
/* userns+mntns+pidns after fork; the grandchild is pid 1 of the new pidns */
static int fdm_userns_pid(int m) {
    (void)m;
    if (setgid(1000) < 0 || setuid(1000) < 0) return 90;
    mkdir("/tmp/fdmu", 0755);
    { static const char *pd[] = {"proc", "tmp", "pts", "mq", "cg"}; char q[64];
      for (int i = 0; i < 5; i++) { snprintf(q, sizeof q, "/tmp/fdmu/%s", pd[i]); mkdir(q, 0755); } }
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWPID) < 0) return 91;
    wr("/proc/self/setgroups", "deny"); wr("/proc/self/uid_map", "0 1000 1"); wr("/proc/self/gid_map", "0 1000 1");
    pid_t c = fork();
    if (c < 0) return 92;
    if (c == 0) {
        if (getpid() != 1) _exit(93);
        if (fdm_mount("/tmp/fdmu/proc", "proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL) < 0) _exit(errno == EPERM ? 96 : 1);
        if (fdm_mount("/tmp/fdmu/tmp", "tmpfs", "tmpfs", MS_NOSUID, "mode=755") < 0) _exit(2);
        if (fdm_mount("/tmp/fdmu/pts", "devpts", "devpts", MS_NOSUID | MS_NOEXEC, "newinstance,ptmxmode=0666,mode=0620") < 0) _exit(4);
        if (fdm_mount("/tmp/fdmu/mq", "mqueue", "mqueue", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL) < 0) _exit(5);
        if (fdm_mount("/tmp/fdmu/cg", "cgroup2", "cgroup2", MS_NOSUID | MS_NODEV | MS_NOEXEC, "nsdelegate") < 0) _exit(6);
        /* the new proc shows THIS pidns: pid 1 is us, and no host pids */
        DIR *dp = opendir("/tmp/fdmu/proc");
        if (!dp) _exit(10);
        int saw1 = 0, n = 0; struct dirent *e;
        while ((e = readdir(dp))) if (e->d_name[0] >= '1' && e->d_name[0] <= '9') { n++; if (!strcmp(e->d_name, "1")) saw1 = 1; }
        closedir(dp);
        char lnk[32]; ssize_t l = readlink("/tmp/fdmu/proc/self", lnk, sizeof lnk - 1);
        if (l <= 0) _exit(11);
        lnk[l] = 0;
        if (!saw1 || n > 3 || strcmp(lnk, "1")) { printf("  proc pidns saw1=%d n=%d self=%s\n", saw1, n, lnk); _exit(12); }
        _exit(0);
    }
    int st = 0; waitpid(c, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 99;
}
/* proc is mounted by a userns task only when an unobstructed proc is visible */
static int fdm_visible(int obstruct) {
    mkdir("/tmp/fdv", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    if (obstruct && mount("tmpfs", "/proc/sys", "tmpfs", 0, NULL) < 0) { printf("  obstruct errno=%d\n", errno); return 92; }
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS) < 0) return 93;
    wr("/proc/self/setgroups", "deny"); wr("/proc/self/uid_map", "0 0 1"); wr("/proc/self/gid_map", "0 0 1");
    errno = 0;
    int r = mount("proc", "/tmp/fdv", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
    if (obstruct) return (r < 0 && errno == EPERM) ? 0 : (printf("  obstructed proc r=%d errno=%d\n", r, errno), 1);
    return r == 0 ? 0 : (printf("  visible proc errno=%d\n", errno), 2);
}
static int fdm_visible_ok(int m) { (void)m; return fdm_visible(0); }
static int fdm_visible_eperm(int m) { (void)m; return fdm_visible(1); }
static void fdmounts_suite(void) {
    report("fdmounts_all_fstypes_root_ns0", nn_in_child(fdm_root_ns0, 0));
    umount2("/tmp/fdm/proc", MNT_DETACH); umount2("/tmp/fdm/tmp", MNT_DETACH); umount2("/tmp/fdm/sys", MNT_DETACH);
    umount2("/tmp/fdm/pts", MNT_DETACH); umount2("/tmp/fdm/mq", MNT_DETACH); umount2("/tmp/fdm/cg", MNT_DETACH);
    report("fdmounts_all_fstypes_root_mntns", nn_in_child(fdm_root_mntns, 0));
    report("fdmounts_userns_mntns_pidns_proc_shows_pidns", nn_in_child(fdm_userns_pid, 0));
    report("fdmounts_proc_fully_visible_allowed", nn_in_child(fdm_visible_ok, 0));
    report("fdmounts_proc_not_fully_visible_eperm", nn_in_child(fdm_visible_eperm, 0));
}

/* ── lane mntpivot (M4): pivot_root inside a mount namespace ─────────────── */
static int pv_prepare(const char *root) {
    mkdir("/tmp/pv", 0755); mkdir(root, 0755);
    char p[128];
    snprintf(p, sizeof p, "%s/hello", root);
    int fd = open(p, O_CREAT | O_WRONLY, 0644);
    if (fd < 0) return -1;
    if (write(fd, "hi", 2) < 0) {}
    close(fd);
    snprintf(p, sizeof p, "%s/old", root);
    mkdir(p, 0755);
    return 0;
}
/* crun's sequence: chdir(root); pivot_root(".", "."); fchdir(oldroot);
 * rslave; umount2(".", MNT_DETACH); chdir("/") */
static int pv_crun_seq(const char *root) {
    if (mount("", "/", NULL, MS_REC | MS_SLAVE, NULL) < 0) { printf("  rslave errno=%d\n", errno); return 10; }
    if (mount(root, root, NULL, MS_BIND | MS_REC, NULL) < 0) { printf("  bind errno=%d\n", errno); return 11; }
    if (chdir(root) < 0) return 12;
    int oldroot = open("/", O_DIRECTORY | O_RDONLY);
    if (oldroot < 0) return 13;
    if (syscall(SYS_pivot_root, ".", ".") < 0) { printf("  pivot errno=%d\n", errno); return 14; }
    if (fchdir(oldroot) < 0) { printf("  fchdir errno=%d\n", errno); return 15; }
    close(oldroot);
    if (mount("", ".", NULL, MS_SLAVE | MS_REC, NULL) < 0) { printf("  rslave2 errno=%d\n", errno); return 16; }
    if (umount2(".", MNT_DETACH) < 0) { printf("  detach errno=%d\n", errno); return 17; }
    if (chdir("/") < 0) return 18;
    char cwd[64];
    if (!getcwd(cwd, sizeof cwd) || strcmp(cwd, "/")) { printf("  cwd=%s\n", cwd); return 19; }
    if (!exists("/hello")) return 20;
    if (exists("/bin/mnttest") || exists("/proc/self")) { printf("  old root still visible\n"); return 21; }
    return 0;
}
static int pv_dotdot(int m) {
    (void)m;
    if (unshare(CLONE_NEWNS) < 0) return 91;
    if (pv_prepare("/tmp/pv/r1") < 0) return 92;
    return pv_crun_seq("/tmp/pv/r1");
}
static int pv_errs(int m) {
    (void)m;
    if (unshare(CLONE_NEWNS) < 0) return 91;
    if (pv_prepare("/tmp/pv/r2") < 0) return 92;
    if (mount("", "/", NULL, MS_REC | MS_PRIVATE, NULL) < 0) return 93;
    errno = 0;
    if (!(syscall(SYS_pivot_root, "/tmp/pv/r2", "/tmp/pv/r2") == -1 && errno == EINVAL)) { printf("  nonmp errno=%d\n", errno); return 1; }
    if (mount("/tmp/pv/r2", "/tmp/pv/r2", NULL, MS_BIND, NULL) < 0) return 94;
    errno = 0;
    if (!(syscall(SYS_pivot_root, "/tmp/pv/r2", "/tmp") == -1 && errno == EINVAL)) { printf("  put_old outside errno=%d\n", errno); return 2; }
    errno = 0;
    if (!(syscall(SYS_pivot_root, "/", "/") == -1 && errno == EBUSY)) { printf("  root errno=%d\n", errno); return 3; }
    if (mount("", "/tmp/pv/r2", NULL, MS_SHARED, NULL) < 0) return 95;
    errno = 0;
    if (!(syscall(SYS_pivot_root, "/tmp/pv/r2", "/tmp/pv/r2/old") == -1 && errno == EINVAL)) { printf("  shared errno=%d\n", errno); return 4; }
    if (mount("", "/tmp/pv/r2", NULL, MS_PRIVATE, NULL) < 0) return 96;
    /* with a subdirectory as put_old the old root stays reachable there */
    if (chdir("/tmp/pv/r2") < 0) return 97;
    if (syscall(SYS_pivot_root, "/tmp/pv/r2", "/tmp/pv/r2/old") < 0) { printf("  ok pivot errno=%d\n", errno); return 5; }
    if (chdir("/") < 0) return 98;
    if (!exists("/hello") || !exists("/old/bin/mnttest")) { printf("  layout wrong\n"); return 6; }
    if (umount2("/old", MNT_DETACH) < 0) { printf("  umount old errno=%d\n", errno); return 7; }
    if (exists("/old/bin/mnttest")) return 8;
    return 0;
}
static int pv_locked_userns(int m) {
    (void)m;
    if (setgid(1000) < 0 || setuid(1000) < 0) return 90;
    mkdir("/tmp/pv", 0755);
    if (pv_prepare("/tmp/pvu3") < 0) return 92;
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS) < 0) return 91;
    wr("/proc/self/setgroups", "deny"); wr("/proc/self/uid_map", "0 1000 1"); wr("/proc/self/gid_map", "0 1000 1");
    return pv_crun_seq("/tmp/pvu3");
}
static void pivot_suite(void) {
    report("pivot_dotdot", nn_in_child(pv_dotdot, 0));
    report("pivot_errs", nn_in_child(pv_errs, 0));
    report("pivot_locked_userns", nn_in_child(pv_locked_userns, 0));
    report("pivot_ns0_intact", exists("/bin/mnttest") && exists("/tmp/pv/r1/hello"));
}

/* ── lane podexec (M6): symlinks resolve against the pivoted root ────────── */
static int pl_copy(const char *src, const char *dst) {
    int a = open(src, O_RDONLY), b = open(dst, O_CREAT | O_WRONLY | O_TRUNC, 0755);
    if (a < 0 || b < 0) return -1;
    char buf[8192]; ssize_t n;
    while ((n = read(a, buf, sizeof buf)) > 0) if (write(b, buf, n) != n) return -1;
    close(a); close(b);
    return 0;
}
/* root/bin/real (an executable), root/bin/abs -> /bin/real (absolute),
 * root/usr/lib/rel -> ../../bin/real, root/usr/lib/up -> ../../../../bin/real */
static int pl_prepare(const char *root) {
    char p[160];
    if (pv_prepare(root) < 0) return -1;
    snprintf(p, sizeof p, "%s/bin", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/proc", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/usr", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/usr/lib", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/bin/real", root);
    if (pl_copy("/bin/mnttest", p) < 0) return -1;
    snprintf(p, sizeof p, "%s/bin/abs", root);
    if (symlink("/bin/real", p) < 0) return -1;
    snprintf(p, sizeof p, "%s/usr/lib/rel", root);
    if (symlink("../../bin/real", p) < 0) return -1;
    snprintf(p, sizeof p, "%s/usr/lib/up", root);
    if (symlink("../../../../bin/real", p) < 0) return -1;
    return 0;
}
static int pl_exec(const char *path) {
    pid_t c = fork();
    if (c < 0) return -1;
    if (c == 0) { char *av[] = {(char *)path, (char *)"child-ok", NULL}; execv(path, av); _exit(127); }
    int st = 0; waitpid(c, &st, 0);
    return WIFEXITED(st) && WEXITSTATUS(st) == 42 ? 0 : -1;
}
static int pl_check(const char *root) {
    int r = pv_crun_seq(root);
    if (r) return r;
    struct stat st;
    if (stat("/bin/abs", &st) < 0) { printf("  stat /bin/abs errno=%d\n", errno); return 30; }
    if (access("/bin/abs", X_OK) < 0) { printf("  access /bin/abs errno=%d\n", errno); return 31; }
    if (pl_exec("/bin/abs") < 0) { printf("  exec /bin/abs failed\n"); return 32; }
    return 0;
}
static int pl_rel(const char *root) {
    int r = pv_crun_seq(root);
    if (r) return r;
    struct stat st;
    if (stat("/usr/lib/rel", &st) < 0) { printf("  stat rel errno=%d\n", errno); return 30; }
    if (pl_exec("/usr/lib/rel") < 0) { printf("  exec rel failed\n"); return 32; }
    /* ".." at the pivoted root stays at the root */
    if (stat("/usr/lib/up", &st) < 0) { printf("  stat up errno=%d\n", errno); return 33; }
    if (pl_exec("/usr/lib/up") < 0) { printf("  exec up failed\n"); return 34; }
    return 0;
}
static int pl_abs_tmp(int m) { (void)m; if (unshare(CLONE_NEWNS) < 0) return 91; if (pl_prepare("/tmp/pv/l1") < 0) return 92; return pl_check("/tmp/pv/l1"); }
static int pl_abs_disk(int m) { (void)m; if (unshare(CLONE_NEWNS) < 0) return 91; mkdir("/root", 0700); mkdir("/root/pvl", 0755);
    if (pl_prepare("/root/pvl/r") < 0) return 92; return pl_check("/root/pvl/r"); }
static int pl_rel_tmp(int m) { (void)m; if (unshare(CLONE_NEWNS) < 0) return 91; if (pl_prepare("/tmp/pv/l2") < 0) return 92; return pl_rel("/tmp/pv/l2"); }
static int pl_rel_disk(int m) { (void)m; if (unshare(CLONE_NEWNS) < 0) return 91; mkdir("/root/pvl", 0755);
    if (pl_prepare("/root/pvl/r2") < 0) return 92; return pl_rel("/root/pvl/r2"); }
static int pl_fdpath(int m) {
    (void)m;
    if (unshare(CLONE_NEWNS) < 0) return 91;
    if (pl_prepare("/tmp/pv/l3") < 0) return 92;
    int r = pv_crun_seq("/tmp/pv/l3");
    if (r) return r;
    if (mount("proc", "/proc", "proc", 0, NULL) < 0) { printf("  proc mount errno=%d\n", errno); return 45; }
    int fd = open("/bin", O_DIRECTORY | O_RDONLY);
    if (fd < 0) return 40;
    char p[64], t[128]; snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(p, t, sizeof t - 1);
    if (n < 0) return 41;
    t[n] = 0;
    if (strcmp(t, "/bin")) { printf("  fd path=%s\n", t); return 42; }
    char q[96]; snprintf(q, sizeof q, "%s/real", p);
    struct stat st;
    if (stat(q, &st) < 0) { printf("  stat %s errno=%d\n", q, errno); return 43; }
    if (!exists("/proc/self/fd")) return 44;
    return 0;
}
static void podexec_suite(void) {
    report("abs_link_in_root", nn_in_child(pl_abs_tmp, 0));
    report("abs_link_in_root_disk", nn_in_child(pl_abs_disk, 0));
    report("rel_dotdot_link", nn_in_child(pl_rel_tmp, 0));
    report("rel_dotdot_link_disk", nn_in_child(pl_rel_disk, 0));
    report("fd_path_after_pivot", nn_in_child(pl_fdpath, 0));
}

/* ── lane opentree: open_tree / move_mount / mount_setattr ──────────────── */
struct ot_attr { uint64_t set, clr, prop, userns; };
static int otree(int dfd, const char *p, unsigned fl) { return (int)syscall(428, dfd, p, fl); }
static int omove(int fd, const char *fp, int tfd, const char *tp, unsigned fl) { return (int)syscall(429, fd, fp, tfd, tp, fl); }
static int osetattr(int dfd, const char *p, unsigned fl, uint64_t set, uint64_t clr) {
    struct ot_attr a = { set, clr, 0, 0 };
    return (int)syscall(442, dfd, p, fl, &a, sizeof a);
}
#define OT_CLONE 1U
#define OT_CLOEXEC 02000000U
#define OT_REC 0x8000U
static int ot_userns(void) {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS) < 0) return -1;
    wr("/proc/self/setgroups", "deny"); wr("/proc/self/uid_map", "0 0 1"); wr("/proc/self/gid_map", "0 0 1");
    return 0;
}
static int ot_noclone(int m) {
    (void)m;
    int fd = otree(AT_FDCWD, "/tmp", OT_CLOEXEC);
    if (fd < 0) { printf("  open_tree errno=%d\n", errno); return 1; }
    close(fd);
    return 0;
}
/* crun's fallback when mount("sysfs") is EPERM: clone /sys, move it into the rootfs */
static int ot_crun_sys(int m) {
    (void)m;
    mkdir("/tmp/otr", 0755); mkdir("/tmp/otr/sys", 0755);
    if (ot_userns() < 0) return 91;
    int fd = otree(AT_FDCWD, "/sys", OT_CLONE | OT_REC | OT_CLOEXEC);
    if (fd < 0) { printf("  open_tree errno=%d\n", errno); return 10; }
    if (omove(fd, "", AT_FDCWD, "/tmp/otr/sys", 4) < 0) { printf("  move_mount errno=%d\n", errno); return 11; }
    if (!exists("/tmp/otr/sys/kernel")) return 12;
    static char mi[16384];
    if (slurp("/proc/self/mountinfo", mi, sizeof mi) <= 0 || !strstr(mi, " /tmp/otr/sys ")) return 13;
    errno = 0;
    if (omove(fd, "", AT_FDCWD, "/tmp/otr/sys", 4) == 0 || errno != EINVAL) return 14; /* consumed */
    close(fd);
    errno = 0;
    if (otree(AT_FDCWD, "/.leandros-sb", OT_CLONE | OT_REC) >= 0) return 15;
    return 0;
}
static int ot_locked(int m) {
    (void)m;
    mkdir("/tmp/otl", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    if (mount("tmpfs", "/tmp/otl", "tmpfs", 0, NULL) < 0) return 92;
    mkdir("/tmp/otl/c", 0755);
    if (mount("tmpfs", "/tmp/otl/c", "tmpfs", 0, NULL) < 0) return 93;
    if (ot_userns() < 0) return 94;
    errno = 0;
    int fd = otree(AT_FDCWD, "/tmp/otl", OT_CLONE);
    if (fd >= 0 || errno != EINVAL) { printf("  nonrec clone fd=%d errno=%d\n", fd, errno); return 10; }
    fd = otree(AT_FDCWD, "/tmp/otl", OT_CLONE | OT_REC);
    if (fd < 0) { printf("  rec clone errno=%d\n", errno); return 11; }
    return 0;
}
static int ot_eperm(int m) {
    (void)m;
    mkdir("/tmp/ote", 0755); mkdir("/tmp/ote/a", 0755); mkdir("/tmp/ote/b", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    if (setgid(1000) < 0 || setuid(1000) < 0) return 92;
    errno = 0;
    if (otree(AT_FDCWD, "/tmp", OT_CLONE | OT_REC) >= 0 || errno != EPERM) return 10;
    int fd = otree(AT_FDCWD, "/tmp", 0);
    if (fd < 0) return 11; /* a plain handle needs no privilege */
    close(fd);
    errno = 0;
    if (omove(AT_FDCWD, "/tmp/ote/a", AT_FDCWD, "/tmp/ote/b", 0) == 0 || errno != EPERM) return 12;
    errno = 0;
    if (osetattr(AT_FDCWD, "/tmp/ote/a", 0, 1, 0) == 0 || errno != EPERM) return 13;
    return 0;
}
static int ot_release(int m) {
    (void)m;
    int l0 = 0, l1 = 0, l2 = 0, s = 0;
    if (unshare(CLONE_NEWNS) < 0) return 91;
    mount("", "/", NULL, MS_REC | MS_PRIVATE, NULL);
    if (ns_counts(&l0, &s) < 0) return 92;
    int fd = otree(AT_FDCWD, "/tmp", OT_CLONE | OT_REC);
    if (fd < 0) { printf("  open_tree errno=%d\n", errno); return 10; }
    if (ns_counts(&l1, &s) < 0 || l1 != l0 + 1) { printf("  live %d -> %d\n", l0, l1); return 11; }
    close(fd);
    mount("", "/", NULL, MS_REC | MS_PRIVATE, NULL); /* reaps */
    if (ns_counts(&l2, &s) < 0 || l2 != l0) { printf("  after close live %d (want %d)\n", l2, l0); return 12; }
    return 0;
}
static int ot_move(int m) {
    (void)m;
    mkdir("/tmp/otm", 0755); mkdir("/tmp/otm/a", 0755); mkdir("/tmp/otm/b", 0755); mkdir("/tmp/otm/c", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    if (mount("tmpfs", "/tmp/otm/a", "tmpfs", 0, NULL) < 0) return 92;
    put("/tmp/otm/a/f", "x", 0644);
    if (omove(AT_FDCWD, "/tmp/otm/a", AT_FDCWD, "/tmp/otm/b", 0) < 0) { printf("  move errno=%d\n", errno); return 10; }
    if (!exists("/tmp/otm/b/f") || exists("/tmp/otm/a/f")) return 11;
    if (ot_userns() < 0) return 93;
    errno = 0;
    if (omove(AT_FDCWD, "/tmp/otm/b", AT_FDCWD, "/tmp/otm/c", 0) == 0 || errno != EINVAL) return 12; /* locked */
    return 0;
}
static int ot_setattr(int m) {
    (void)m;
    mkdir("/tmp/ots", 0755); mkdir("/tmp/otk", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    if (mount("tmpfs", "/tmp/ots", "tmpfs", 0, NULL) < 0) return 92;
    if (mount("tmpfs", "/tmp/otk", "tmpfs", MS_RDONLY, NULL) < 0) return 93;
    if (osetattr(AT_FDCWD, "/tmp/ots", 0, 1, 0) < 0) { printf("  setattr ro errno=%d\n", errno); return 10; }
    errno = 0;
    if (open("/tmp/ots/f", O_CREAT | O_WRONLY, 0644) >= 0 || errno != EROFS) return 11;
    if (osetattr(AT_FDCWD, "/tmp/ots", 0, 0, 1) < 0) return 12;
    if (open("/tmp/ots/f", O_CREAT | O_WRONLY, 0644) < 0) return 13;
    if (ot_userns() < 0) return 94;
    errno = 0;
    if (osetattr(AT_FDCWD, "/tmp/otk", 0, 0, 1) == 0 || errno != EPERM) return 14; /* locked ro */
    return 0;
}
static void opentree_suite(void) {
    report("opentree_noclone_handle", nn_in_child(ot_noclone, 0));
    report("opentree_crun_sys_clone_move", nn_in_child(ot_crun_sys, 0));
    report("opentree_locked_children_einval", nn_in_child(ot_locked, 0));
    report("opentree_eperm_without_cap", nn_in_child(ot_eperm, 0));
    report("opentree_detached_release", nn_in_child(ot_release, 0));
    report("opentree_move_existing", nn_in_child(ot_move, 0));
    report("opentree_setattr_ro_and_locked", nn_in_child(ot_setattr, 0));
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
    fdpath_suite();
    fdmounts_suite();
    pivot_suite();
    podexec_suite();
    devpts_copies();
    opentree_suite();
    printf("%s\n", fails ? "mnttest: FAILED" : "mnttest: ALL PASSED");
    return fails ? 1 : 0;
}
