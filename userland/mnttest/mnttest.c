/* mnttest: mount-namespace tests. M0 (mntcore) cases are unit-level checks
 * reachable without CLONE_NEWNS: the in-kernel mount-tree selftest
 * (/proc/mntselftest) and ns-0 invariants. Output: "case: PASS|FAIL|SKIP". */
#define _GNU_SOURCE
#include <dirent.h>
#include <sys/socket.h>
#include <sys/un.h>
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
#include <sys/sysmacros.h>
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

/* Minor of the "major:minor" mountinfo field of the mount at `mp` (0 = not found). */
static unsigned long mount_dev_minor(const char *mp) {
    static char buf[65536];
    if (slurp("/proc/self/mountinfo", buf, sizeof buf) <= 0) return 0;
    char pat[160];
    snprintf(pat, sizeof pat, " %s ", mp);
    for (char *ln = buf; ln && *ln; ln = strchr(ln, '\n') ? strchr(ln, '\n') + 1 : NULL) {
        char *eol = strchr(ln, '\n');
        char *h = strstr(ln, pat);
        if (!h || (eol && h > eol)) continue;
        unsigned id, parent, maj, mnr;
        if (sscanf(ln, "%u %u %u:%u", &id, &parent, &maj, &mnr) == 4) return ((unsigned long)maj << 8) | mnr;
    }
    return 0;
}

/* Is the superblock with this dev minor live? Identity, not a global count:
 * other tasks mount and unmount concurrently, so sb_count deltas flake. */
static int sb_dev_live(unsigned long dev) {
    static char buf[8192];
    if (slurp("/proc/mntselftest", buf, sizeof buf) <= 0) return -1;
    char *q = strstr(buf, "sb_devs=");
    if (!q) return -1;
    q += 8;
    while (*q && *q != '\n') {
        char *e;
        unsigned long v = strtoul(q, &e, 10);
        if (e == q) break;
        if (v == dev) return 1;
        q = (*e == ',') ? e + 1 : e;
    }
    return 0;
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
    /* Concurrent mount churn elsewhere (any other task): the superblock
     * registry is system-wide, so a count delta flakes; identity does not. */
    const char *cd = "/root/mt-tfs-churn.d";
    mkdir(cd, 0755);
    pid_t churn = fork();
    if (churn == 0) {
        for (;;) {
            if (mount("shm", cd, "tmpfs", 0, "size=64k") == 0) umount(cd);
        }
    }
    usleep(20000);
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
    unsigned long tdev = mount_dev_minor(d);
    printf("# tfs count delta (old verdict) %d, churn running; mount dev minor %lu\n", sb1 - sb0, tdev);
    report("tfs_sb_registered", sb_dev_live(tdev) == 1);
    r = umount(d);
    report("tfs_umount_ok", r == 0);
    report("tfs_umount_frees", stat(f, &st1) != 0 && sb_dev_live(tdev) == 0);
    if (churn > 0) { kill(churn, SIGKILL); waitpid(churn, NULL, 0); umount(cd); rmdir(cd); }
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
    /* made before the unshare: with idmap=off the new userns has no uid map */
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
static int ot_mi_ro(const char *mp) { /* 1 ro, 0 rw, -1 absent */
    static char mi[16384];
    char key[96]; snprintf(key, sizeof key, " %s ", mp);
    if (slurp("/proc/self/mountinfo", mi, sizeof mi) <= 0) return -1;
    char *l = strstr(mi, key); if (!l) return -1;
    char *e = strchr(l, '\n'); if (e) *e = 0;
    char *o = l + strlen(key);
    return (o[0] == 'r' && o[1] == 'o' && (o[2] == ',' || o[2] == ' ')) ? 1 : 0;
}
static int ot_setattr(int m) {
    (void)m;
    mkdir("/tmp/ots", 0755); mkdir("/tmp/otk", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    if (mount("tmpfs", "/tmp/ots", "tmpfs", 0, NULL) < 0) return 92;
    if (mount("tmpfs", "/tmp/otk", "tmpfs", MS_RDONLY, NULL) < 0) return 93;
    if (osetattr(AT_FDCWD, "/tmp/ots", 0, 1, 0) < 0) { printf("  setattr ro errno=%d\n", errno); return 10; }
    if (ot_mi_ro("/tmp/ots") != 1) return 11;
    if (osetattr(AT_FDCWD, "/tmp/ots", 0, 0, 1) < 0) return 12;
    if (ot_mi_ro("/tmp/ots") != 0) return 13;
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

/* lane fdbindprop: crun's etc/hosts sequence - bind a source onto a target in a
 * rootfs, open the target O_PATH, mount_setattr(fd, "", EMPTY_PATH|RECURSIVE),
 * then mount(NULL, "/proc/self/fd/N", MS_REC|MS_PRIVATE). */
static int fb_seq(int mode) {
    int isdir = mode & 1, disk = mode & 2, pre = mode & 4, pv = mode & 8;
    const char *B = disk ? "/root/fb" : "/tmp/fb";
    char R[64], E[64], S[64], src[64], dst[64];
    mkdir("/root", 0700); mkdir(B, 0755);
    snprintf(R, sizeof R, "%s/r", B); snprintf(E, sizeof E, "%s/r/etc", B); snprintf(S, sizeof S, "%s/s", B);
    mkdir(R, 0755); mkdir(E, 0755); mkdir(S, 0755);
    snprintf(src, sizeof src, "%s/%s", S, isdir ? "d" : "hosts");
    snprintf(dst, sizeof dst, "%s/%s", E, isdir ? "d" : "hosts");
    if (isdir) { mkdir(src, 0755); mkdir(dst, 0755); }
    else { int f = open(src, O_CREAT | O_WRONLY, 0644); if (f < 0) return 90; close(f);
           f = open(dst, O_CREAT | O_WRONLY, 0644); if (f < 0) return 90; close(f); }
    if (ot_userns() < 0) return 91;
    if (mount(R, R, NULL, MS_BIND | MS_REC, NULL) < 0) { printf("  rootfs bind errno=%d\n", errno); return 10; }
    if (pv) { if (chdir(R) < 0) return 16; }
    int fd = -1;
    if (pre) { fd = open(pv ? (isdir ? "etc/d" : "etc/hosts") : dst, O_PATH | O_CLOEXEC | (isdir ? O_DIRECTORY : 0));
               if (fd < 0) { printf("  preopen errno=%d\n", errno); return 12; } }
    if (pre) { char q[48]; snprintf(q, sizeof q, "/proc/self/fd/%d", fd);
               if (mount(src, q, NULL, MS_BIND, NULL) < 0) { printf("  bind(fd) errno=%d\n", errno); return 11; } }
    else if (mount(src, dst, NULL, MS_BIND, NULL) < 0) { printf("  bind errno=%d\n", errno); return 11; }
    if (!pre) { fd = open(pv ? (isdir ? "etc/d" : "etc/hosts") : dst, O_PATH | O_CLOEXEC | (isdir ? O_DIRECTORY : 0));
                if (fd < 0) { printf("  open errno=%d\n", errno); return 12; } }
    if (osetattr(fd, "", 0x1000 | OT_REC, 0, 0) < 0) { printf("  mount_setattr errno=%d\n", errno); return 13; }
    char p[48]; snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    if (mount(NULL, p, NULL, MS_REC | MS_PRIVATE, NULL) < 0) { printf("  propagation errno=%d\n", errno); return 14; }
    close(fd);
    return 0;
}
/* crun's new-API variant: open_tree(CLONE) the source, move_mount it onto the
 * target, then setattr / make-private through the SAME descriptor. */
static int fb_mv(int isdir) {
    mkdir("/tmp/fm", 0755); mkdir("/tmp/fm/r", 0755); mkdir("/tmp/fm/r/etc", 0755); mkdir("/tmp/fm/s", 0755);
    const char *src = isdir ? "/tmp/fm/s/d" : "/tmp/fm/s/hosts";
    const char *dst = isdir ? "/tmp/fm/r/etc/d" : "/tmp/fm/r/etc/hosts";
    if (isdir) { mkdir(src, 0755); mkdir(dst, 0755); }
    else { int f = open(src, O_CREAT | O_WRONLY, 0644); if (f < 0) return 90; close(f);
           f = open(dst, O_CREAT | O_WRONLY, 0644); if (f < 0) return 90; close(f); }
    if (ot_userns() < 0) return 91;
    if (mount("/tmp/fm/r", "/tmp/fm/r", NULL, MS_BIND | MS_REC, NULL) < 0) return 10;
    int fd = otree(AT_FDCWD, src, OT_CLONE | OT_CLOEXEC);
    if (fd < 0) { printf("  open_tree errno=%d\n", errno); return 11; }
    if (omove(fd, "", AT_FDCWD, dst, 4) < 0) { printf("  move_mount errno=%d\n", errno); return 12; }
    if (osetattr(fd, "", 0x1000 | OT_REC, 0, 0) < 0) { printf("  mount_setattr errno=%d\n", errno); return 13; }
    char p[48]; snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    if (mount(NULL, p, NULL, MS_REC | MS_PRIVATE, NULL) < 0) { printf("  propagation errno=%d\n", errno); return 14; }
    static char mi[16384];
    if (slurp("/proc/self/mountinfo", mi, sizeof mi) <= 0 || !strstr(mi, isdir ? "/tmp/fm/r/etc/d " : "/tmp/fm/r/etc/hosts ")) return 15;
    close(fd);
    return 0;
}
static int fb_mvf(int m) { (void)m; return fb_mv(0); }
static int fb_mvd(int m) { (void)m; return fb_mv(1); }
static int fb_m(int m) { return fb_seq(m); }
static void fdbindprop_suite(void) {
    static const char *nm[] = { "file_tmp", "dir_tmp", "file_disk", "dir_disk", "file_tmp_pre", "dir_tmp_pre", "file_disk_pre", "dir_disk_pre", "file_tmp_rel", "dir_tmp_rel", "file_disk_rel", "dir_disk_rel", "file_tmp_pre_rel", "dir_tmp_pre_rel", "file_disk_pre_rel", "dir_disk_pre_rel" };
    report("fdbindprop_open_tree_move_file_then_setattr_private", nn_in_child(fb_mvf, 0));
    report("fdbindprop_open_tree_move_dir_then_setattr_private", nn_in_child(fb_mvd, 0));
    for (int m = 0; m < 16; m++) {
        char n[64]; snprintf(n, sizeof n, "fdbindprop_%s", nm[m]);
        report(n, nn_in_child(fb_m, m));
    }
}

/* ── lane mntro: per-mount ro / noexec / nodev enforcement ─────────────── */
static int mro_setup(void) {
    mkdir("/tmp/mro", 0755); mkdir("/tmp/mrb", 0755); mkdir("/tmp/mrw", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    mount("", "/", NULL, MS_REC | MS_PRIVATE, NULL);
    if (mount("tmpfs", "/tmp/mro", "tmpfs", 0, NULL) < 0) return 92;
    if (mount("tmpfs", "/tmp/mrw", "tmpfs", 0, NULL) < 0) return 93;
    put("/tmp/mro/f", "x", 0644); mkdir("/tmp/mro/d", 0755); put("/tmp/mro/d/g", "y", 0644);
    if (mount("/tmp/mro", "/tmp/mrb", NULL, MS_BIND, NULL) < 0) return 94;
    if (mount(NULL, "/tmp/mrb", NULL, MS_BIND | MS_REMOUNT | MS_RDONLY, NULL) < 0) return 95;
    return 0;
}
static int mro_erofs(int m) {
    (void)m;
    int r = mro_setup(); if (r) return r;
    errno = 0; if (open("/tmp/mrb/f", O_WRONLY) >= 0 || errno != EROFS) { printf("  open wr errno=%d\n", errno); return 10; }
    errno = 0; if (open("/tmp/mrb/f", O_RDWR) >= 0 || errno != EROFS) return 11;
    errno = 0; if (open("/tmp/mrb/f", O_RDONLY | O_TRUNC) >= 0 || errno != EROFS) return 12;
    errno = 0; if (open("/tmp/mrb/new", O_WRONLY | O_CREAT, 0644) >= 0 || errno != EROFS) return 13;
    int fd = open("/tmp/mrb/f", O_RDONLY | O_CREAT, 0644); if (fd < 0) return 14; /* existing: ok */
    close(fd);
    errno = 0; if (mkdir("/tmp/mrb/nd", 0755) == 0 || errno != EROFS) { printf("  mkdir errno=%d\n", errno); return 15; }
    errno = 0; if (unlink("/tmp/mrb/f") == 0 || errno != EROFS) return 16;
    errno = 0; if (rmdir("/tmp/mrb/d") == 0 || errno != EROFS) return 17;
    errno = 0; if (rename("/tmp/mrw/none", "/tmp/mrb/x") == 0 || errno == 0) {}
    put("/tmp/mrw/s", "z", 0644);
    errno = 0; if (rename("/tmp/mrw/s", "/tmp/mrb/s") == 0 || (errno != EROFS && errno != EXDEV)) { printf("  rename onto errno=%d\n", errno); return 18; }
    errno = 0; if (rename("/tmp/mrb/f", "/tmp/mrb/f2") == 0 || errno != EROFS) return 19;
    errno = 0; if (symlink("a", "/tmp/mrb/sl") == 0 || errno != EROFS) return 20;
    errno = 0; if (link("/tmp/mrb/f", "/tmp/mrb/hl") == 0 || errno != EROFS) return 21;
    errno = 0; if (chmod("/tmp/mrb/f", 0600) == 0 || errno != EROFS) return 22;
    errno = 0; if (truncate("/tmp/mrb/f", 0) == 0 || errno != EROFS) return 23;
    /* the same files through the rw original and an rw sibling stay writable */
    fd = open("/tmp/mro/f", O_WRONLY); if (fd < 0) { printf("  rw orig errno=%d\n", errno); return 30; }
    close(fd);
    if (mkdir("/tmp/mrw/nd", 0755) < 0) return 31;
    put("/tmp/mrw/ok", "k", 0644); if (!exists("/tmp/mrw/ok")) return 32;
    return 0;
}
static int mro_openfd_keeps_writing(int m) {
    (void)m;
    int r = mro_setup(); if (r) return r;
    int fd = open("/tmp/mro/f", O_RDWR); if (fd < 0) return 10;
    mkdir("/tmp/mrc", 0755);
    if (mount("/tmp/mro", "/tmp/mrc", NULL, MS_BIND, NULL) < 0) return 11;
    int fd2 = open("/tmp/mrc/f", O_RDWR); if (fd2 < 0) return 12;
    if (mount(NULL, "/tmp/mrc", NULL, MS_BIND | MS_REMOUNT | MS_RDONLY, NULL) < 0) return 13;
    if (write(fd2, "q", 1) != 1) { printf("  write after remount-ro errno=%d\n", errno); return 14; }
    close(fd); close(fd2);
    errno = 0; if (open("/tmp/mrc/f", O_RDWR) >= 0 || errno != EROFS) return 15;
    return 0;
}
/* fchmod/fchown/futimens on an fd opened through a read-only mount: EROFS
 * (also when the mount turns read-only after the open, as Linux mnt_want_write). */
static int mro_fd_attr(int m) {
    (void)m;
    int r = mro_setup(); if (r) return r;
    int fd = open("/tmp/mrb/f", O_RDONLY); if (fd < 0) return 10;
    errno = 0; if (fchmod(fd, 0600) == 0 || errno != EROFS) { printf("  fchmod errno=%d\n", errno); return 11; }
    errno = 0; if (fchown(fd, 5, 5) == 0 || errno != EROFS) { printf("  fchown errno=%d\n", errno); return 12; }
    struct timespec ts[2] = {{1, 0}, {1, 0}};
    errno = 0; if (futimens(fd, ts) == 0 || errno != EROFS) { printf("  futimens errno=%d\n", errno); return 13; }
    close(fd);
    /* the rw original stays changeable */
    fd = open("/tmp/mro/f", O_RDONLY); if (fd < 0) return 20;
    if (fchmod(fd, 0600) < 0) { printf("  rw fchmod errno=%d\n", errno); return 21; }
    if (futimens(fd, ts) < 0) return 22;
    close(fd);
    return 0;
}
static int mro_noexec(int m) {
    (void)m;
    mkdir("/tmp/mrx", 0755); mkdir("/tmp/mry", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    if (mount("tmpfs", "/tmp/mrx", "tmpfs", MS_NOEXEC, NULL) < 0) return 92;
    if (mount("tmpfs", "/tmp/mry", "tmpfs", 0, NULL) < 0) return 93;
    int in = open("/bin/mnttest", O_RDONLY); if (in < 0) return 94;
    int a = open("/tmp/mrx/m", O_WRONLY | O_CREAT, 0755), b = open("/tmp/mry/m", O_WRONLY | O_CREAT, 0755);
    if (a < 0 || b < 0) return 95;
    char buf[4096]; ssize_t n;
    while ((n = read(in, buf, sizeof buf)) > 0) { if (write(a, buf, n) != n || write(b, buf, n) != n) return 96; }
    close(a); close(b); close(in);
    pid_t p = fork();
    if (p == 0) { execl("/tmp/mry/m", "m", "child-ok", (char *)0); _exit(99); }
    int st = 0; waitpid(p, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 42) { printf("  control exec st=%d\n", st); return 10; }
    p = fork();
    if (p == 0) { execl("/tmp/mrx/m", "m", "child-ok", (char *)0); _exit(errno == EACCES ? 77 : 78); }
    waitpid(p, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 77) { printf("  noexec exec st=%d\n", st); return 11; }
    return 0;
}
static int mro_nodev(int m) {
    (void)m;
    mkdir("/tmp/mrn", 0755); mkdir("/tmp/mrv", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    if (mount("tmpfs", "/tmp/mrn", "tmpfs", MS_NODEV, NULL) < 0) return 92;
    if (mount("tmpfs", "/tmp/mrv", "tmpfs", 0, NULL) < 0) return 93;
    if (mknod("/tmp/mrn/null", S_IFCHR | 0666, makedev(1, 3)) < 0) { printf("  mknod nodev errno=%d\n", errno); return 94; }
    if (mknod("/tmp/mrv/null", S_IFCHR | 0666, makedev(1, 3)) < 0) return 95;
    int fd = open("/tmp/mrv/null", O_RDONLY); if (fd < 0) { printf("  control open errno=%d\n", errno); return 10; }
    close(fd);
    errno = 0; fd = open("/tmp/mrn/null", O_RDONLY);
    if (fd >= 0 || errno != EACCES) { printf("  nodev open fd=%d errno=%d\n", fd, errno); return 11; }
    return 0;
}
static int mns_exec_euid(const char *f) {
    pid_t p = fork();
    if (p == 0) { if (setgid(1000) < 0 || setuid(1000) < 0) _exit(90); execl(f, f, "euid-chk", (char *)0); _exit(99); }
    int st = 0; waitpid(p, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : -1;
}
static int mns_nosuid_bind(int m) {
    (void)m;
    mkdir("/tmp/mns", 0755); mkdir("/tmp/mnb", 0755); mkdir("/tmp/mnc", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    mount("", "/", NULL, MS_REC | MS_PRIVATE, NULL);
    if (mount("tmpfs", "/tmp/mns", "tmpfs", 0, NULL) < 0) return 92;
    chmod("/tmp/mns", 0755);
    int in = open("/bin/mnttest", O_RDONLY); if (in < 0) return 94;
    int a = open("/tmp/mns/m", O_WRONLY | O_CREAT, 0755); if (a < 0) return 95;
    char buf[4096]; ssize_t n;
    while ((n = read(in, buf, sizeof buf)) > 0) { if (write(a, buf, n) != n) return 96; }
    close(a); close(in);
    if (chown("/tmp/mns/m", 0, 0) < 0 || chmod("/tmp/mns/m", 04755) < 0) return 97;
    if (mount("/tmp/mns", "/tmp/mnb", NULL, MS_BIND, NULL) < 0) return 98;
    if (mount("/tmp/mns", "/tmp/mnc", NULL, MS_BIND, NULL) < 0) return 99;
    int r = mns_exec_euid("/tmp/mnb/m");
    if (r != 44) { printf("  control (suid bind) r=%d\n", r); return 10; }
    if (mount(NULL, "/tmp/mnc", NULL, MS_BIND | MS_REMOUNT | MS_NOSUID, NULL) < 0) { printf("  remount errno=%d\n", errno); return 11; }
    r = mns_exec_euid("/tmp/mnc/m");
    if (r != 42) { printf("  nosuid bind exec r=%d\n", r); return 12; }
    r = mns_exec_euid("/tmp/mnb/m");
    if (r != 44) { printf("  sibling lost suid r=%d\n", r); return 13; }
    return 0;
}
static void mro_suite(void) {
    report("mntnosuid_bind_ignores_setuid", nn_in_child(mns_nosuid_bind, 0));
    report("mntro_ro_bind_erofs", nn_in_child(mro_erofs, 0));
    report("mntro_rw_fd_survives_remount_ro", nn_in_child(mro_openfd_keeps_writing, 0));
    report("mntro_fd_attr_erofs", nn_in_child(mro_fd_attr, 0));
    report("mntro_noexec_eacces", nn_in_child(mro_noexec, 0));
    report("mntro_nodev_eacces", nn_in_child(mro_nodev, 0));
}

/* ── lane shmbind: crun's dev/shm bind and bind-remount flags ─────────── */
/* mode bit0: source on disk (/root), bit1: relative open after chdir(rootfs),
 * bit2: mount the tmpfs through an O_PATH fd of the rootfs dev dir */
static int sb_shm(int mode) {
    static const char DEEP[] = "/home/leandro/.local/share/containers/storage/vfs/dir/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const char *B = (mode & 16) ? DEEP : (mode & 1) ? "/root/sbm" : "/tmp/sbm";
    char R[256], D[256], SH[256], S[256];
    mkdir("/root", 0700);
    if (mode & 16) { char t[256]; size_t i; for (i = 1; B[i]; i++) if (B[i] == '/') { memcpy(t, B, i); t[i] = 0; mkdir(t, 0755); chown(t, 1000, 1000); } }
    mkdir(B, 0755);
    snprintf(R, sizeof R, "%s/r", B); snprintf(D, sizeof D, "%s/r/dev", B);
    snprintf(SH, sizeof SH, "%s/r/dev/shm", B); snprintf(S, sizeof S, "%s/userdata", B);
    mkdir(R, 0755); mkdir(D, 0755); mkdir(S, 0755);
    char src[300]; snprintf(src, sizeof src, "%s/shm", S); mkdir(src, 0755);
    /* uid 1000 in a fresh userns + mntns */
    chmod("/root", 0711);
    chown(B, 1000, 1000); chown(R, 1000, 1000); chown(D, 1000, 1000); chown(S, 1000, 1000); chown(src, 1000, 1000);
    if (setgid(1000) < 0 || setuid(1000) < 0) return 90;
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS) < 0) return 91;
    wr("/proc/self/setgroups", "deny"); wr("/proc/self/uid_map", "1000 1000 1"); wr("/proc/self/gid_map", "1000 1000 1");
    if (mount(R, R, NULL, MS_BIND | MS_REC, NULL) < 0) { printf("  rootfs bind errno=%d\n", errno); return 10; }
    /* podman's userdata/shm is a tmpfs mounted in its own mntns */
    if (mode & 8) { if (mount("tmpfs", src, "tmpfs", 0, "mode=1777,size=65536k") < 0) { printf("  src tmpfs errno=%d\n", errno); return 18; } }
    if (mode & 8) {
        /* crun decides file vs directory target from stat of the source */
        struct stat a; struct statx sx;
        if (stat(src, &a) < 0 || !S_ISDIR(a.st_mode)) { printf("  stat src mode=%o\n", a.st_mode); return 20; }
        if (lstat(src, &a) < 0 || !S_ISDIR(a.st_mode)) { printf("  lstat src mode=%o\n", a.st_mode); return 21; }
        if (statx(AT_FDCWD, src, AT_SYMLINK_NOFOLLOW, STATX_TYPE, &sx) < 0 || !S_ISDIR(sx.stx_mode)) { printf("  statx src mode=%o\n", sx.stx_mode); return 22; }
        int sfd = open(src, O_PATH | O_CLOEXEC); if (sfd < 0) return 23;
        if (fstat(sfd, &a) < 0 || !S_ISDIR(a.st_mode)) { printf("  fstat(O_PATH) mode=%o\n", a.st_mode); return 24; }
        if (statx(sfd, "", AT_EMPTY_PATH, STATX_TYPE, &sx) < 0 || !S_ISDIR(sx.stx_mode)) { printf("  statx(EMPTY) mode=%o\n", sx.stx_mode); return 25; }
        if (fstatat(sfd, "", &a, AT_EMPTY_PATH) < 0 || !S_ISDIR(a.st_mode)) { printf("  fstatat(EMPTY) mode=%o\n", a.st_mode); return 26; }
        close(sfd);
    }
    if (mode & 32) {
        /* crun clones the source with open_tree and decides file vs directory
         * from fstat of that detached-mount descriptor */
        struct stat a; struct statx sx;
        int ofd = (int)syscall(428, AT_FDCWD, src, 0x1 | 0x80000 | 0x8000 /* CLONE|CLOEXEC|AT_RECURSIVE */);
        if (ofd < 0) { printf("  open_tree errno=%d\n", errno); return 30; }
        if (fstat(ofd, &a) < 0 || !S_ISDIR(a.st_mode)) { printf("  fstat(open_tree) mode=%o\n", a.st_mode); return 31; }
        if (statx(ofd, "", AT_EMPTY_PATH, STATX_TYPE, &sx) < 0 || !S_ISDIR(sx.stx_mode)) { printf("  statx(open_tree) mode=%o\n", sx.stx_mode); return 32; }
        close(ofd);
    }
    if (mode & 2) { if (chdir(R) < 0) return 16; }
    if (mode & 4) {
        int dfd = open(mode & 2 ? "dev" : D, O_PATH | O_CLOEXEC | O_DIRECTORY); if (dfd < 0) return 17;
        char q[48]; snprintf(q, sizeof q, "/proc/self/fd/%d", dfd);
        if (mount("tmpfs", q, "tmpfs", 0, "mode=755") < 0) { printf("  tmpfs(fd) errno=%d\n", errno); return 11; }
        close(dfd);
    } else if (mount("tmpfs", D, "tmpfs", 0, "mode=755") < 0) { printf("  tmpfs errno=%d\n", errno); return 11; }
    if (mkdir(mode & 2 ? "dev/shm" : SH, 0755) < 0) { { static char um[256]; slurp("/proc/self/uid_map", um, sizeof um); printf("  mkdir shm errno=%d uid=%d map=[%s]\n", errno, (int)getuid(), um); } return 12; }
    int fd = open(mode & 2 ? "dev/shm" : SH, O_PATH | O_CLOEXEC | O_DIRECTORY);
    if (fd < 0) { printf("  open errno=%d\n", errno); return 13; }
    char p[48]; snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    if (mount(src, p, NULL, MS_BIND, NULL) < 0) { printf("  shm bind errno=%d\n", errno); return 14; }
    close(fd);
    return 0;
}
static int sb_shm_m(int m) { return sb_shm(m); }
/* MS_BIND|MS_REMOUNT inside a userns+mntns: set/clear ro, locked flags, EROFS */
static int sb_remount(int m) {
    (void)m;
    mkdir("/tmp/sbr", 0755); mkdir("/tmp/sbr/a", 0755); mkdir("/tmp/sbr/b", 0755);
    put("/tmp/sbr/a/f", "x", 0644);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    mount("", "/", NULL, MS_REC | MS_PRIVATE, NULL);
    if (mount("/tmp/sbr/a", "/tmp/sbr/b", NULL, MS_BIND, NULL) < 0) return 10;
    if (mount(NULL, "/tmp/sbr/b", NULL, MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL) < 0) { printf("  remount ro errno=%d\n", errno); return 11; }
    if (ot_userns() < 0) return 91; /* the copy locks the inherited mounts' flags */
    errno = 0; if (open("/tmp/sbr/b/f", O_WRONLY) >= 0 || errno != EROFS) { printf("  wr errno=%d\n", errno); return 12; }
    int fd = open("/tmp/sbr/a/f", O_WRONLY); if (fd < 0) return 13; close(fd); /* original stays rw */
    /* flags locked by the userns transition cannot be cleared */
    errno = 0;
    if (mount(NULL, "/tmp/sbr/b", NULL, MS_BIND | MS_REMOUNT, NULL) == 0 || errno != EPERM) { printf("  clear locked errno=%d\n", errno); return 14; }
    /* keeping them set is fine */
    if (mount(NULL, "/tmp/sbr/b", NULL, MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL) < 0) return 15;
    return 0;
}
static int sb_remount_clear(int m) {
    (void)m;
    mkdir("/tmp/sbc", 0755); mkdir("/tmp/sbc/a", 0755); mkdir("/tmp/sbc/b", 0755);
    put("/tmp/sbc/a/f", "x", 0644);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    mount("", "/", NULL, MS_REC | MS_PRIVATE, NULL);
    if (mount("/tmp/sbc/a", "/tmp/sbc/b", NULL, MS_BIND, NULL) < 0) return 10;
    if (mount(NULL, "/tmp/sbc/b", NULL, MS_BIND | MS_REMOUNT | MS_RDONLY, NULL) < 0) return 11;
    errno = 0; if (open("/tmp/sbc/b/f", O_WRONLY) >= 0 || errno != EROFS) return 12;
    if (mount(NULL, "/tmp/sbc/b", NULL, MS_BIND | MS_REMOUNT, NULL) < 0) { printf("  clear ro errno=%d\n", errno); return 13; }
    int fd = open("/tmp/sbc/b/f", O_WRONLY); if (fd < 0) { printf("  wr after clear errno=%d\n", errno); return 14; } close(fd);
    return 0;
}
static void shmbind_suite(void) {
    static const char *nm[] = {"tmp", "disk", "tmp_rel", "disk_rel", "tmp_fdtmpfs", "disk_fdtmpfs", "tmp_rel_fdtmpfs", "disk_rel_fdtmpfs"};
    for (int m = 0; m < 8; m++) { char n[64]; snprintf(n, sizeof n, "shmbind_dev_shm_%s", nm[m]); report(n, nn_in_child(sb_shm_m, m)); }
    report("shmbind_dev_shm_src_tmpfs", nn_in_child(sb_shm_m, 8));
    report("shmbind_open_tree_fstat_dir", nn_in_child(sb_shm_m, 32));
    report("shmbind_open_tree_fstat_dir_tmpfs", nn_in_child(sb_shm_m, 32 | 8));
    report("shmbind_dev_shm_home_deep", nn_in_child(sb_shm_m, 16));
    report("shmbind_dev_shm_home_deep_fdtmpfs", nn_in_child(sb_shm_m, 16 | 4));
    report("shmbind_dev_shm_home_deep_rel_fdtmpfs_srctmpfs", nn_in_child(sb_shm_m, 16 | 2 | 4 | 8));
    report("shmbind_dev_shm_src_tmpfs_disk_rel_fdtmpfs", nn_in_child(sb_shm_m, 8 | 1 | 2 | 4));
    report("shmbind_bind_remount_userns_locked_flags", nn_in_child(sb_remount, 0));
    report("shmbind_bind_remount_clear_ro", nn_in_child(sb_remount_clear, 0));
}

/* ── lane devbind: crun's rootless default-device binds ─────────────────
 * A rootless container cannot mknod, so crun creates an empty file in the
 * container's /dev tmpfs and bind-mounts the host node onto it (by path, or
 * through /proc/self/fd/N of an O_PATH fd of the target). mode bit0: target
 * through an fd. */
static const char *const DB_DEV[] = {"null", "zero", "full", "tty", "random", "urandom"};
static int db_dev(int m) {
    int idx = m >> 4, via_fd = m & 1;
    const char *B = "/tmp/dbd", *name = DB_DEV[idx];
    char D[64], T[96], src[32];
    mkdir(B, 0755); snprintf(D, sizeof D, "%s/dev", B); mkdir(D, 0755);
    chown(B, 1000, 1000); chown(D, 1000, 1000);
    snprintf(src, sizeof src, "/dev/%s", name);
    if (setgid(1000) < 0 || setuid(1000) < 0) return 90;
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS) < 0) return 91;
    wr("/proc/self/setgroups", "deny"); wr("/proc/self/uid_map", "1000 1000 1"); wr("/proc/self/gid_map", "1000 1000 1");
    if (mount("tmpfs", D, "tmpfs", MS_NOSUID, "mode=755") < 0) { printf("  tmpfs errno=%d\n", errno); return 11; }
    snprintf(T, sizeof T, "%s/%s", D, name);
    int cfd = open(T, O_CREAT | O_WRONLY | O_EXCL, 0644); if (cfd < 0) { printf("  create errno=%d\n", errno); return 12; } close(cfd);
    struct stat st;
    if (stat(src, &st) < 0) { printf("  host %s errno=%d\n", src, errno); return 13; }
    if (!S_ISCHR(st.st_mode)) { printf("  host %s not chr mode=%o\n", src, st.st_mode); return 14; }
    if (via_fd) {
        int fd = open(T, O_PATH | O_CLOEXEC); if (fd < 0) return 15;
        char q[48]; snprintf(q, sizeof q, "/proc/self/fd/%d", fd);
        if (mount(src, q, NULL, MS_BIND, NULL) < 0) { printf("  bind(fd) %s errno=%d\n", src, errno); return 16; }
        close(fd);
    } else if (mount(src, T, NULL, MS_BIND, NULL) < 0) { printf("  bind %s errno=%d\n", src, errno); return 16; }
    /* crun then remounts it nosuid|noexec (never nodev for devices) */
    if (mount(NULL, T, NULL, MS_BIND | MS_REMOUNT | MS_NOSUID | MS_NOEXEC, NULL) < 0) { printf("  remount errno=%d\n", errno); return 17; }
    struct stat bt;
    if (stat(T, &bt) < 0 || !S_ISCHR(bt.st_mode) || bt.st_rdev != st.st_rdev) { printf("  bound stat mode=%o rdev=%lx want %lx\n", bt.st_mode, (unsigned long)bt.st_rdev, (unsigned long)st.st_rdev); return 18; }
    if (!strcmp(name, "tty")) return 0; /* no controlling terminal to exercise */
    int fd = open(T, O_RDWR); if (fd < 0) { printf("  open bound errno=%d\n", errno); return 19; }
    unsigned char b[32]; ssize_t n;
    if (!strcmp(name, "null")) {
        if (write(fd, "abc", 3) != 3) return 20;
        if (read(fd, b, sizeof b) != 0) return 21;
    } else if (!strcmp(name, "zero") || !strcmp(name, "full")) {
        memset(b, 0xff, sizeof b);
        if (read(fd, b, sizeof b) != (ssize_t)sizeof b) return 22;
        for (size_t i = 0; i < sizeof b; i++) if (b[i]) return 23;
        if (!strcmp(name, "full")) {
            errno = 0; n = write(fd, "abc", 3);
            if (n != -1 || errno != ENOSPC) { printf("  full write n=%zd errno=%d\n", n, errno); return 24; }
        } else if (write(fd, "abc", 3) != 3) return 25;
    } else {
        memset(b, 0, sizeof b);
        if (read(fd, b, sizeof b) != (ssize_t)sizeof b) return 26;
        int nz = 0; for (size_t i = 0; i < sizeof b; i++) nz |= b[i];
        if (!nz) return 27;
    }
    close(fd);
    return 0;
}
/* crun masks a directory (/sys/firmware) by moving a clone of a shared empty
 * dir onto it, then re-opens the target with open_tree and requires the same
 * st_dev/st_ino as the clone it moved ("race condition detected remounting
 * masked path" otherwise). */
static int db_maskdir(int m) {
    (void)m;
    const char *B = "/tmp/dbm";
    char E[64], T[64];
    mkdir(B, 0755); snprintf(E, sizeof E, "%s/empty", B); snprintf(T, sizeof T, "%s/target", B);
    mkdir(E, 0755); mkdir(T, 0755);
    chown(B, 1000, 1000); chown(E, 1000, 1000); chown(T, 1000, 1000);
    if (setgid(1000) < 0 || setuid(1000) < 0) return 90;
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS) < 0) return 91;
    wr("/proc/self/setgroups", "deny"); wr("/proc/self/uid_map", "1000 1000 1"); wr("/proc/self/gid_map", "1000 1000 1");
    if (mount("tmpfs", E, "tmpfs", 0, "mode=755") < 0) { printf("  tmpfs errno=%d\n", errno); return 11; }
    int a = (int)syscall(428, AT_FDCWD, E, 0x1 | 0x80000 | 0x8000);
    if (a < 0) { printf("  open_tree empty errno=%d\n", errno); return 12; }
    struct stat before, after;
    if (fstat(a, &before) < 0) return 13;
    int tfd = open(T, O_PATH | O_DIRECTORY | O_CLOEXEC); if (tfd < 0) return 14;
    if (syscall(429, a, "", tfd, "", 0x4 | 0x40) < 0) { printf("  move_mount errno=%d\n", errno); return 15; }
    int b = (int)syscall(428, AT_FDCWD, T, 0x1 | 0x80000 | 0x8000);
    if (b < 0) { printf("  open_tree target errno=%d\n", errno); return 16; }
    if (fstat(b, &after) < 0) return 17;
    if (before.st_dev != after.st_dev || before.st_ino != after.st_ino) {
        printf("  dev/ino differ: %lx/%lx vs %lx/%lx\n", (unsigned long)before.st_dev, (unsigned long)before.st_ino, (unsigned long)after.st_dev, (unsigned long)after.st_ino);
        return 18;
    }
    return 0;
}
static void devbind_suite(void) {
    report("devbind_maskdir_same_ident", nn_in_child(db_maskdir, 0));
    for (int i = 0; i < 6; i++) for (int f = 0; f < 2; f++) {
        char n[64]; snprintf(n, sizeof n, "devbind_%s_%s", DB_DEV[i], f ? "fd" : "path");
        report(n, nn_in_child(db_dev, (i << 4) | f));
    }
}

/* ── lane mntiso-procfd: /proc/self/fd/N reopen and /proc/self/fd/N/<below>
 * honour the fd's mount (ro/nodev/mask) and never follow links in host space.
 * Host sentinel /root/mntiso-secret ("HOST") lives on the root volume in ns 0;
 * the pivoted root holds its own /root/mntiso-secret ("INNER"). ──────────── */
static int mi_enter(const char *tag, int ro, int nodev) {
    char r[96], p[160];
    mkdir("/root/mipv", 0755);
    snprintf(r, sizeof r, "/root/mipv/%s", tag); mkdir(r, 0755);
    snprintf(p, sizeof p, "%s/dd", r); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/mi", r); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/proc", r); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/root", r); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/root/mntiso-secret", r); put(p, "INNER", 0644);
    snprintf(p, sizeof p, "%s/dd/f", r); put(p, "data", 0644);
    snprintf(p, sizeof p, "%s/dd/l", r); unlink(p); if (symlink("/root/mntiso-secret", p) < 0) return 90;
    snprintf(p, sizeof p, "%s/dd/null", r); unlink(p);
    if (mknod(p, S_IFCHR | 0666, makedev(1, 3)) < 0) return 89;
    snprintf(p, sizeof p, "%s/dd/x", r); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/dd/x/f", r); put(p, "BACK", 0644);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    if (mount("", "/", NULL, MS_REC | MS_SLAVE, NULL) < 0) return 93;
    if (mount(r, r, NULL, MS_BIND | MS_REC, NULL) < 0) return 94;
    char src[128], dst[128];
    snprintf(src, sizeof src, "%s/dd", r); snprintf(dst, sizeof dst, "%s/mi", r);
    if (mount(src, dst, NULL, MS_BIND, NULL) < 0) return 95;
    unsigned long fl = MS_BIND | MS_REMOUNT | (ro ? MS_RDONLY : 0) | (nodev ? MS_NODEV : 0);
    if ((ro || nodev) && mount(NULL, dst, NULL, fl, NULL) < 0) return 96;
    if (chdir(r) < 0) return 97;
    int oldroot = open("/", O_DIRECTORY | O_RDONLY);
    if (oldroot < 0) return 98;
    if (syscall(SYS_pivot_root, ".", ".") < 0) return 99;
    if (fchdir(oldroot) < 0) return 100;
    close(oldroot);
    if (mount("", ".", NULL, MS_SLAVE | MS_REC, NULL) < 0) return 101;
    if (umount2(".", MNT_DETACH) < 0) return 102;
    if (chdir("/") < 0) return 103;
    if (mount("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL) < 0) return 104;
    return 0;
}
static int mi_host_intact(void) { char b[16] = {0}; return slurp("/root/mntiso-secret", b, sizeof b) > 0 && !strncmp(b, "HOST", 4); }
static int mi_procfd_reopen_ro(int m) {
    (void)m; int r = mi_enter("a", 1, 0); if (r) return r;
    int fd = open("/mi/f", O_PATH); if (fd < 0) return 10;
    char p[48]; snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    int c = open(p, O_RDONLY); if (c < 0) { printf("  ro reopen errno=%d\n", errno); return 11; }
    close(c);
    errno = 0; c = open(p, O_RDWR);
    if (c >= 0 || errno != EROFS) { printf("  rw reopen fd=%d errno=%d\n", c, errno); return 12; }
    return 0;
}
static int mi_procfd_below_ro(int m) {
    (void)m; int r = mi_enter("b", 1, 0); if (r) return r;
    int fd = open("/mi", O_PATH | O_DIRECTORY); if (fd < 0) return 10;
    char p[64]; snprintf(p, sizeof p, "/proc/self/fd/%d/f", fd);
    errno = 0; int c = open(p, O_RDWR);
    if (c >= 0 || errno != EROFS) { printf("  below rw fd=%d errno=%d\n", c, errno); return 11; }
    return 0;
}
static int mi_procfd_below_nodev(int m) {
    (void)m; int r = mi_enter("c", 0, 1); if (r) return r;
    int fd = open("/mi", O_PATH | O_DIRECTORY); if (fd < 0) return 10;
    char p[64]; snprintf(p, sizeof p, "/proc/self/fd/%d/null", fd);
    errno = 0; int c = open(p, O_RDONLY);
    if (c >= 0 || errno != EACCES) { printf("  below nodev fd=%d errno=%d\n", c, errno); return 11; }
    return 0;
}
static int mi_procfd_below_link(int m) {
    (void)m; int r = mi_enter("d", 0, 0); if (r) return r;
    int fd = open("/mi", O_PATH | O_DIRECTORY); if (fd < 0) return 10;
    char p[64], b[16] = {0}; snprintf(p, sizeof p, "/proc/self/fd/%d/l", fd);
    int c = open(p, O_RDONLY);
    if (c >= 0) { ssize_t n = read(c, b, 15); (void)n; }
    if (!strncmp(b, "HOST", 4)) { printf("  link followed into the host\n"); return 11; }
    if (c < 0 || strncmp(b, "INNER", 5)) { printf("  link: fd=%d errno=%d body=%s\n", c, errno, b); return 12; }
    return 0;
}
static int mi_procfd_below_mask(int m) {
    (void)m; int r = mi_enter("e", 0, 0); if (r) return r;
    int fd = open("/mi", O_PATH | O_DIRECTORY); if (fd < 0) return 10;
    if (mount("tmpfs", "/mi/x", "tmpfs", 0, NULL) < 0) return 11;
    put("/mi/x/f", "TMPFS", 0644);
    char p[64], b[16] = {0}; snprintf(p, sizeof p, "/proc/self/fd/%d/x/f", fd);
    if (slurp(p, b, sizeof b) <= 0 || strncmp(b, "TMPFS", 5)) { printf("  mask: body=%s\n", b); return 12; }
    return 0;
}
static void mntiso_procfd_suite(void) {
    put("/root/mntiso-secret", "HOST", 0644);
    report("procfd_reopen_ro_erofs", nn_in_child(mi_procfd_reopen_ro, 0));
    report("procfd_below_ro_erofs", nn_in_child(mi_procfd_below_ro, 0));
    report("procfd_below_nodev_eacces", nn_in_child(mi_procfd_below_nodev, 0));
    report("procfd_below_link_in_ns", nn_in_child(mi_procfd_below_link, 0));
    report("procfd_below_mask", nn_in_child(mi_procfd_below_mask, 0));
    report("procfd_host_sentinel_intact", mi_host_intact());
}

/* ── lane mntiso-e7: a dirfd keeps its object after the tree changes ──── */
#define E7H "/tmp/e7/host/d"
#define E7R "/tmp/e7/root"
static void e7_mk(void) {
    mkdir("/tmp/e7", 0755); mkdir("/tmp/e7/host", 0755); mkdir(E7H, 0755);
    mkdir(E7R, 0755); mkdir(E7R "/tmp", 0755); mkdir(E7R "/tmp/e7", 0755);
    mkdir(E7R "/tmp/e7/host", 0755); mkdir(E7R "/tmp/e7/host/d", 0755);
    mkdir(E7R "/old", 0755); mkdir(E7R "/proc", 0755);
    put(E7H "/f", "HOST", 0644); put(E7H "/u", "HOST", 0644);
    put(E7R E7H "/f", "VIEW", 0644); put(E7R E7H "/u", "VIEW", 0644);
}
static int e7_child(int m) {
    int dfd = open(E7H, O_DIRECTORY | O_RDONLY); if (dfd < 0) return 10;
    if (unshare(CLONE_NEWNS) < 0) return 11;
    if (mount("", "/", NULL, MS_REC | MS_PRIVATE, NULL) < 0) return 12;
    if (m == 0) {
        if (mount(E7R, E7R, NULL, MS_BIND | MS_REC, NULL) < 0) return 13;
        if (chdir(E7R) < 0) return 14;
        if (syscall(SYS_pivot_root, ".", "old") < 0) return 15;
        if (chdir("/") < 0) return 16;
        if (umount2("/old", MNT_DETACH) < 0) return 18;
        if (exists("/tmp/e7/host/d/m")) return 17;
    } else {
        if (chroot(E7R) < 0) return 15;
        if (chdir("/") < 0) return 16;
    }
    char b[16] = {0}; int fd = openat(dfd, "f", O_RDONLY);
    if (fd < 0) { printf("  openat errno=%d\n", errno); return 20; }
    if (read(fd, b, 15) < 0) {} close(fd);
    if (strncmp(b, "HOST", 4)) { printf("  openat read '%s'\n", b); return 21; }
    struct stat st; if (fstatat(dfd, "f", &st, 0) < 0 || st.st_size != 4) { printf("  fstatat errno=%d\n", errno); return 22; }
    if (mkdirat(dfd, "m", 0755) < 0) { printf("  mkdirat errno=%d\n", errno); return 23; }
    if (unlinkat(dfd, "u", 0) < 0) { printf("  unlinkat errno=%d\n", errno); return 24; }
    if (m == 1) return 0; /* the chroot has no procfs, as on Linux */
    char p[64]; snprintf(p, sizeof p, "/proc/self/fd/%d/f", dfd);
    memset(b, 0, sizeof b);
    if (slurp(p, b, sizeof b) < 0 || strncmp(b, "HOST", 4)) { printf("  procfd read '%s'\n", b); return 25; }
    return 0;
}
static void mntiso_e7_suite(void) {
    for (int m = 0; m < 2; m++) {
        e7_mk();
        int ok = nn_in_child(e7_child, m);
        char n[40]; snprintf(n, sizeof n, "e7_dirfd_keeps_object_%s", m ? "chroot" : "pivot");
        int host = exists(E7H "/m") && !exists(E7H "/u") && exists(E7R E7H "/u") && !exists(E7R E7H "/m");
        report(n, ok && host);
        system("rm -rf /tmp/e7");
    }
}

/* ── lane e7flags: a hidden dirfd keeps its mount's flags ── */
static int e7f_child(int m) {
    mkdir("/tmp/e7f/ro", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 11;
    if (mount("", "/", NULL, MS_REC | MS_PRIVATE, NULL) < 0) return 12;
    if (mount("/tmp/e7f/src", "/tmp/e7f/ro", NULL, MS_BIND, NULL) < 0) return 13;
    if (mount(NULL, "/tmp/e7f/ro", NULL, MS_REMOUNT | MS_BIND | MS_RDONLY, NULL) < 0) return 14;
    int dfd = open("/tmp/e7f/ro", O_DIRECTORY | O_RDONLY); if (dfd < 0) return 15;
    if (mount(E7R, E7R, NULL, MS_BIND | MS_REC, NULL) < 0) return 16;
    if (chdir(E7R) < 0) return 17;
    if (syscall(SYS_pivot_root, ".", "old") < 0) return 18;
    if (chdir("/") < 0) return 19;
    if (umount2("/old", MNT_DETACH) < 0) return 20;
    if (exists("/tmp/e7f/ro")) return 21;
    char b[16] = {0}; int fd = openat(dfd, "f", O_RDONLY);
    if (fd < 0) { printf("  ro read errno=%d\n", errno); return 22; }
    if (read(fd, b, 15) < 0) {} close(fd);
    if (strncmp(b, "HOST", 4)) return 23;
    errno = 0; if (mkdirat(dfd, "m", 0755) == 0 || errno != EROFS) { printf("  mkdirat errno=%d\n", errno); return 24; }
    errno = 0; fd = openat(dfd, "n", O_WRONLY | O_CREAT, 0644);
    if (fd >= 0 || errno != EROFS) { printf("  creat fd=%d errno=%d\n", fd, errno); return 25; }
    errno = 0; fd = openat(dfd, "f", O_WRONLY);
    if (fd >= 0 || errno != EROFS) { printf("  open wr fd=%d errno=%d\n", fd, errno); return 26; }
    errno = 0; if (unlinkat(dfd, "u", 0) == 0 || errno != EROFS) { printf("  unlinkat errno=%d\n", errno); return 27; }
    errno = 0; if (renameat(dfd, "u", dfd, "u2") == 0 || errno != EROFS) { printf("  renameat errno=%d\n", errno); return 28; }
    errno = 0; if (symlinkat("x", dfd, "sl") == 0 || errno != EROFS) { printf("  symlinkat errno=%d\n", errno); return 29; }
    (void)m; return 0;
}
static void e7flags_suite(void) {
    e7_mk();
    mkdir("/tmp/e7f", 0755); mkdir("/tmp/e7f/src", 0755);
    put("/tmp/e7f/src/f", "HOST", 0644); put("/tmp/e7f/src/u", "HOST", 0644);
    int ok = nn_in_child(e7f_child, 0);
    int host = !exists("/tmp/e7f/src/m") && !exists("/tmp/e7f/src/n") && exists("/tmp/e7f/src/u")
            && !exists("/tmp/e7f/src/u2") && !exists("/tmp/e7f/src/sl");
    report("e7_hidden_fd_keeps_mount_flags", ok && host);
    system("rm -rf /tmp/e7 /tmp/e7f");
}

/* ── lane e7flags2: ns-0 per-mount flags; hidden dirfd noexec / nodev ── */
static int e7x_child(int m) {
    mkdir("/tmp/e7x/mp", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 11;
    if (mount("", "/", NULL, MS_REC | MS_PRIVATE, NULL) < 0) return 12;
    if (mount("/tmp/e7x/src", "/tmp/e7x/mp", NULL, MS_BIND, NULL) < 0) return 13;
    if (mount(NULL, "/tmp/e7x/mp", NULL, MS_REMOUNT | MS_BIND | MS_NOEXEC | MS_NODEV, NULL) < 0) return 14;
    int dfd = open("/tmp/e7x/mp", O_DIRECTORY | O_RDONLY); if (dfd < 0) return 15;
    if (mount(E7R, E7R, NULL, MS_BIND | MS_REC, NULL) < 0) return 16;
    if (chdir(E7R) < 0) return 17;
    if (syscall(SYS_pivot_root, ".", "old") < 0) return 18;
    if (chdir("/") < 0) return 19;
    if (umount2("/old", MNT_DETACH) < 0) return 20;
    if (exists("/tmp/e7x/mp")) return 21;
    char *av[] = { "bin", NULL }, *ev[] = { NULL };
    errno = 0; syscall(SYS_execveat, dfd, "bin", av, ev, 0);
    if (errno != EACCES) { printf("  execveat errno=%d\n", errno); return 22; }
    int fd = openat(dfd, "bin", O_RDONLY); if (fd < 0) return 23;
    errno = 0; syscall(SYS_execveat, fd, "", av, ev, 0x1000);
    if (errno != EACCES) { printf("  fexecve errno=%d\n", errno); return 24; }
    errno = 0; int nd = openat(dfd, "nul", O_RDONLY);
    if (nd >= 0 || errno != EACCES) { printf("  nodev open fd=%d errno=%d\n", nd, errno); return 25; }
    (void)m; return 0;
}
static int e7x_ns0(int m) {
    (void)m;
    int rc = 0;
    mkdir("/tmp/e7x/mp", 0755);
    if (mount("/tmp/e7x/src", "/tmp/e7x/mp", NULL, MS_BIND | MS_RDONLY, NULL) < 0) return 13;
    /* MS_BIND|MS_RDONLY in one call: ro is ignored (Linux), writable. */
    errno = 0; if (mkdir("/tmp/e7x/mp/m0", 0755) != 0) rc = 20;
    rmdir("/tmp/e7x/src/m0");
    if (mount(NULL, "/tmp/e7x/mp", NULL, MS_REMOUNT | MS_BIND | MS_RDONLY, NULL) < 0) { printf("  ns0 remount errno=%d\n", errno); umount("/tmp/e7x/mp"); return 14; }
    errno = 0; if (!rc && (mkdir("/tmp/e7x/mp/m", 0755) == 0 || errno != EROFS)) { printf("  ns0 ro mkdir errno=%d\n", errno); rc = 21; }
    errno = 0; if (!rc && (open("/tmp/e7x/mp/c", O_WRONLY | O_CREAT, 0644) >= 0 || errno != EROFS)) rc = 22;
    errno = 0; if (!rc && (open("/tmp/e7x/mp/bin", O_WRONLY) >= 0 || errno != EROFS)) rc = 23;
    if (!rc) { int fd = open("/tmp/e7x/mp/bin", O_RDONLY); if (fd < 0) rc = 24; else close(fd); }
    if (!rc) { /* a dirfd keeps the mount's flags */
        int d = open("/tmp/e7x/mp", O_RDONLY | O_DIRECTORY);
        if (d < 0) rc = 25;
        else {
            errno = 0; if (mkdirat(d, "m2", 0755) == 0 || errno != EROFS) { printf("  ns0 dirfd mkdirat errno=%d\n", errno); rc = 26; }
            errno = 0; if (!rc && (openat(d, "c2", O_WRONLY | O_CREAT, 0644) >= 0 || errno != EROFS)) rc = 27;
            errno = 0; if (!rc && (unlinkat(d, "bin", 0) == 0 || errno != EROFS)) rc = 28;
            close(d);
        }
    }
    /* the source stays writable */
    if (!rc && mkdir("/tmp/e7x/src/ok", 0755) != 0) rc = 29;
    rmdir("/tmp/e7x/src/ok");
    /* noexec + nodev */
    if (!rc && mount(NULL, "/tmp/e7x/mp", NULL, MS_REMOUNT | MS_BIND | MS_NOEXEC | MS_NODEV, NULL) < 0) rc = 30;
    if (!rc) { /* rw again after the remount replaced the flags */
        if (mkdir("/tmp/e7x/mp/m3", 0755) != 0) { printf("  ns0 rw again errno=%d\n", errno); rc = 31; }
        rmdir("/tmp/e7x/src/m3");
    }
    if (!rc) { pid_t p = fork(); if (p == 0) { execl("/tmp/e7x/mp/bin", "bin", (char *)0); _exit(errno == EACCES ? 0 : 1); }
        int st = 0; waitpid(p, &st, 0); if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) rc = 32; }
    errno = 0; if (!rc && (open("/tmp/e7x/mp/nul", O_RDONLY) >= 0 || errno != EACCES)) rc = 33;
    umount("/tmp/e7x/mp");
    /* self-bind hardening pattern */
    if (!rc && mount("/tmp/e7x/src", "/tmp/e7x/src", NULL, MS_BIND, NULL) < 0) rc = 40;
    if (!rc && mount(NULL, "/tmp/e7x/src", NULL, MS_REMOUNT | MS_BIND | MS_RDONLY, NULL) < 0) rc = 41;
    errno = 0; if (!rc && (mkdir("/tmp/e7x/src/m4", 0755) == 0 || errno != EROFS)) rc = 42;
    umount("/tmp/e7x/src");
    if (!rc && mkdir("/tmp/e7x/src/m5", 0755) != 0) rc = 43;
    rmdir("/tmp/e7x/src/m5");
    return rc;
}
static void e7x_suite(void) {
    e7_mk();
    mkdir("/tmp/e7x", 0755); mkdir("/tmp/e7x/src", 0755);
    system("cp /bin/true /tmp/e7x/src/bin; chmod 755 /tmp/e7x/src/bin");
    mknod("/tmp/e7x/src/nul", S_IFCHR | 0666, makedev(1, 3));
    report("e7_hidden_fd_noexec_nodev", nn_in_child(e7x_child, 0));
    report("e7_ns0_permount_flags_enforced", nn_in_child(e7x_ns0, 0));
    system("rm -rf /tmp/e7 /tmp/e7x");
}

/* ── lane mntiso-busy: mount roots are EBUSY, cross-mount ops EXDEV ── */
static int mb_prep(void) {
    if (unshare(CLONE_NEWNS) < 0) return 91;
    mkdir("/root", 0700); mkdir("/root/mb", 0755);
    mkdir("/root/mb/src1", 0755); mkdir("/root/mb/src2", 0755);
    mkdir("/root/mb/t1", 0755); mkdir("/root/mb/t2", 0755);
    int fd = open("/root/mb/hostfile", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return 92; if (write(fd, "HOST", 4) < 0) {} close(fd);
    fd = open("/root/mb/src1/f", O_WRONLY | O_CREAT | O_TRUNC, 0644); if (fd >= 0) close(fd);
    fd = open("/root/mb/src2/g", O_WRONLY | O_CREAT | O_TRUNC, 0644); if (fd >= 0) close(fd);
    fd = open("/root/mb/fbind", O_WRONLY | O_CREAT | O_TRUNC, 0644); if (fd >= 0) close(fd);
    if (mount("", "/", NULL, MS_REC | MS_SLAVE, NULL) < 0) return 93;
    if (mount("/root/mb/src1", "/root/mb/t1", NULL, MS_BIND, NULL) < 0) return 94;
    if (mount("/root/mb/src2", "/root/mb/t2", NULL, MS_BIND, NULL) < 0) return 95;
    fd = open("/root/mb/fmp", O_WRONLY | O_CREAT | O_TRUNC, 0644); if (fd >= 0) close(fd);
    if (mount("/root/mb/hostfile", "/root/mb/fmp", NULL, MS_BIND, NULL) < 0) return 96;
    return 0;
}
static int mb_case(int m) {
    int r = mb_prep(); if (r) return r;
    switch (m) {
    case 0: if (rename("/root/mb/t1", "/root/mb/t1x") == 0 || errno != EBUSY) return 1;
            return exists("/root/mb/src1/f") && exists("/root/mb/t1/f") ? 0 : 2;
    case 1: if (unlink("/root/mb/fmp") == 0 || errno != EBUSY) return 1;
            return exists("/root/mb/hostfile") ? 0 : 2;
    case 2: if (rmdir("/root/mb/t1") == 0 || errno != EBUSY) return 1;
            return exists("/root/mb/src1") ? 0 : 2;
    case 3: if (rename("/root/mb/t1/f", "/root/mb/t2/f2") == 0 || errno != EXDEV) return 1;
            return exists("/root/mb/src1/f") ? 0 : 2;
    case 4: if (link("/root/mb/t1/f", "/root/mb/t2/f3") == 0 || errno != EXDEV) return 1;
            return !exists("/root/mb/src2/f3") ? 0 : 2;
    case 5: /* same mount still works */
            if (rename("/root/mb/t1/f", "/root/mb/t1/f4") < 0) return 1;
            return exists("/root/mb/src1/f4") ? 0 : 2;
    case 6: { char b[8] = {0}; int fd = open("/root/mb/hostfile", O_RDONLY);
            if (fd < 0) return 1; if (read(fd, b, 4) < 0) {} close(fd);
            return !strcmp(b, "HOST") ? 0 : 2; }
    }
    return 99;
}
static void mntiso_busy_suite(void) {
    report("busy_rename_bind_root", nn_in_child(mb_case, 0));
    report("busy_unlink_file_bind", nn_in_child(mb_case, 1));
    report("busy_rmdir_mountpoint", nn_in_child(mb_case, 2));
    report("xdev_rename_across_binds", nn_in_child(mb_case, 3));
    report("xdev_link_across_binds", nn_in_child(mb_case, 4));
    report("busy_same_mount_rename_ok", nn_in_child(mb_case, 5));
    report("busy_host_sentinel_intact", nn_in_child(mb_case, 6));
}
/* ── lane mntiso-chroot: chroot inside a mntns (E4) ───────────────────── */
/* mode 0: abs link to / , 1: ".." link, 2: exec through an in-jail abs link */
static int cj_case(int m) {
    mkdir("/tmp/cjm", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    mount("", "/", NULL, MS_REC | MS_PRIVATE, NULL);
    if (mount("tmpfs", "/tmp/cjm", "tmpfs", 0, NULL) < 0) return 92;
    chmod("/tmp/cjm", 0755);
    mkdir("/tmp/cjm/sub", 0755); chmod("/tmp/cjm/sub", 0755);
    if (symlink("/", "/tmp/cjm/sub/esc") < 0) return 93;
    if (symlink("../../../..", "/tmp/cjm/sub/up") < 0) return 94;
    if (m == 2) {
        int in = open("/bin/mnttest", O_RDONLY); if (in < 0) return 95;
        int a = open("/tmp/cjm/sub/m", O_WRONLY | O_CREAT, 0755); if (a < 0) return 96;
        char buf[4096]; ssize_t n;
        while ((n = read(in, buf, sizeof buf)) > 0) { if (write(a, buf, n) != n) return 97; }
        close(a); close(in);
        if (symlink("/m", "/tmp/cjm/sub/l") < 0) return 98;
    }
    if (chroot("/tmp/cjm/sub") < 0 || chdir("/") < 0) return 99;
    if (setgid(1000) < 0 || setuid(1000) < 0) return 90;
    if (m == 2) {
        pid_t p = fork();
        if (p == 0) { execl("/l", "/l", "child-ok", (char *)0); _exit(99); }
        int st = 0; waitpid(p, &st, 0);
        return WIFEXITED(st) && WEXITSTATUS(st) == 42 ? 0 : 10;
    }
    int fd = open(m == 0 ? "/esc/root/mntiso-secret" : "/up/root/mntiso-secret", O_RDONLY);
    if (fd >= 0) { close(fd); return 11; }
    return errno == ENOENT ? 0 : 12;
}
static void chroot_ns_suite(void) {
    mkdir("/root", 0755);
    int f = open("/root/mntiso-secret", O_WRONLY | O_CREAT, 0644);
    if (f >= 0) { if (write(f, "HOST", 4) != 4) {} close(f); }
    report("chroot_ns_abs_link", nn_in_child(cj_case, 0));
    report("chroot_ns_dotdot_link", nn_in_child(cj_case, 1));
    report("chroot_ns_exec_through_link", nn_in_child(cj_case, 2));
    unlink("/root/mntiso-secret");
}

/* ── lane mntiso-entry: exec PT_INTERP, AF_UNIX and mount bind source follow ns links (E2) ── */
/* A minimal ELF that exits with `code`; `interp` (or NULL) becomes PT_INTERP. */
static int ce_write_elf(const char *path, int code, const char *interp) {
    unsigned char b[512]; memset(b, 0, sizeof b);
    int dyn = interp == NULL;                  /* the loader is ET_DYN, the program ET_EXEC */
    int nph = interp ? 2 : 1, phoff = 64, ioff = 64 + 56 * nph;
    int cstart = interp ? ioff + (int)strlen(interp) + 1 : ioff;
    cstart = (cstart + 7) & ~7;
    unsigned char code_b[16]; int clen;
#if defined(__x86_64__)
    code_b[0] = 0xBF; code_b[1] = code; code_b[2] = code_b[3] = code_b[4] = 0;
    code_b[5] = 0xB8; code_b[6] = 60; code_b[7] = code_b[8] = code_b[9] = 0; code_b[10] = 0x0F; code_b[11] = 0x05; clen = 12;
#else
    unsigned w[3] = { 0xD2800000u | ((unsigned)code << 5), 0xD2800BA8u, 0xD4000001u };
    memcpy(code_b, w, 12); clen = 12;
#endif
    long base = dyn ? 0 : 0x400000;
    memcpy(b, "\177ELF", 4); b[4] = 2; b[5] = 1; b[6] = 1;
    *(uint16_t *)(b + 16) = dyn ? 3 : 2;
#if defined(__x86_64__)
    *(uint16_t *)(b + 18) = 62;
#else
    *(uint16_t *)(b + 18) = 183;
#endif
    *(uint32_t *)(b + 20) = 1;
    *(uint64_t *)(b + 24) = base + cstart;     /* e_entry */
    *(uint64_t *)(b + 32) = phoff;
    *(uint16_t *)(b + 52) = 64; *(uint16_t *)(b + 54) = 56; *(uint16_t *)(b + 56) = nph;
    int total = cstart + clen;
    unsigned char *ph = b + phoff;
    if (interp) {
        *(uint32_t *)(ph) = 3; *(uint32_t *)(ph + 4) = 4;       /* PT_INTERP, R */
        *(uint64_t *)(ph + 8) = ioff; *(uint64_t *)(ph + 16) = base + ioff; *(uint64_t *)(ph + 24) = base + ioff;
        *(uint64_t *)(ph + 32) = strlen(interp) + 1; *(uint64_t *)(ph + 40) = strlen(interp) + 1; *(uint64_t *)(ph + 48) = 1;
        ph += 56;
        memcpy(b + ioff, interp, strlen(interp) + 1);
    }
    *(uint32_t *)(ph) = 1; *(uint32_t *)(ph + 4) = 5;            /* PT_LOAD, R+X */
    *(uint64_t *)(ph + 8) = 0; *(uint64_t *)(ph + 16) = base; *(uint64_t *)(ph + 24) = base;
    *(uint64_t *)(ph + 32) = total; *(uint64_t *)(ph + 40) = total; *(uint64_t *)(ph + 48) = 0x1000;
    memcpy(b + cstart, code_b, clen);
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    if (fd < 0) return -1;
    int ok = write(fd, b, total) == total;
    close(fd); chmod(path, 0755);
    return ok ? 0 : -1;
}
/* mode 0: PT_INTERP abs link, 1: AF_UNIX connect abs link, 2: mount --bind source abs link,
   3: AF_UNIX bind through an abs-link directory */
static int ce_case(int m) {
    mkdir("/tmp/cem", 0755);
    if (unshare(CLONE_NEWNS) < 0) return 91;
    mount("", "/", NULL, MS_REC | MS_PRIVATE, NULL);
    if (mount("tmpfs", "/tmp/cem", "tmpfs", 0, NULL) < 0) return 92;
    chmod("/tmp/cem", 0755);
    mkdir("/tmp/cem/sub", 0755); chmod("/tmp/cem/sub", 0755);
    if (m == 0) {
        if (ce_write_elf("/tmp/cem/sub/ldx", 43, NULL) < 0) return 93;      /* the in-root loader */
        if (ce_write_elf("/tmp/cem/sub/prog", 41, "/ldl") < 0) return 94;
        if (symlink("/ldx", "/tmp/cem/sub/ldl") < 0) return 95;
    } else if (m == 1) {
        if (symlink("/root/mntiso.sock", "/tmp/cem/sub/l") < 0) return 93;
    } else if (m == 2) {
        mkdir("/tmp/cem/sub/root", 0755); mkdir("/tmp/cem/sub/mnt", 0755);
        int f = open("/tmp/cem/sub/root/inroot", O_WRONLY | O_CREAT, 0644); if (f >= 0) close(f);
        if (symlink("/root", "/tmp/cem/sub/l") < 0) return 93;
    } else {
        if (symlink("/root", "/tmp/cem/sub/d") < 0) return 93;
        mkdir("/tmp/cem/sub/root", 0755);
    }
    if (chroot("/tmp/cem/sub") < 0 || chdir("/") < 0) return 99;
    if (m == 0) {
        pid_t p = fork();
        if (p == 0) { execl("/prog", "/prog", (char *)0); _exit(99); }
        int st = 0; waitpid(p, &st, 0);
        return WIFEXITED(st) && WEXITSTATUS(st) == 43 ? 0 : 10 + (WIFEXITED(st) ? WEXITSTATUS(st) : 0);
    }
    if (m == 1 || m == 3) {
        struct sockaddr_un a; memset(&a, 0, sizeof a); a.sun_family = AF_UNIX;
        strcpy(a.sun_path, m == 1 ? "/l" : "/d/mntiso-bound.sock");
        int s = socket(AF_UNIX, SOCK_STREAM, 0); if (s < 0) return 96;
        int r = m == 1 ? connect(s, (struct sockaddr *)&a, sizeof a) : bind(s, (struct sockaddr *)&a, sizeof a);
        close(s);
        if (m == 1) return r < 0 ? 0 : 11;
        /* bind must have created the node inside the jail's /root, not the host's */
        return r == 0 && access("/root/mntiso-bound.sock", F_OK) == 0 ? 0 : 12;
    }
    if (mount("/l", "/mnt", NULL, MS_BIND, NULL) < 0) return 97;
    if (access("/mnt/mntiso-secret", F_OK) == 0) return 11;
    return access("/mnt/inroot", F_OK) == 0 ? 0 : 12;
}
static void entry_ns_suite(void) {
    mkdir("/root", 0755);
    int f = open("/root/mntiso-secret", O_WRONLY | O_CREAT, 0644);
    if (f >= 0) { if (write(f, "HOST", 4) != 4) {} close(f); }
    ce_write_elf("/ldx", 44, NULL);                                          /* the host's loader */
    int ls = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a; memset(&a, 0, sizeof a); a.sun_family = AF_UNIX;
    strcpy(a.sun_path, "/root/mntiso.sock");
    unlink(a.sun_path);
    if (ls >= 0 && (bind(ls, (struct sockaddr *)&a, sizeof a) < 0 || listen(ls, 4) < 0)) { close(ls); ls = -1; }
    report("interp_abs_link_in_root", nn_in_child(ce_case, 0));
    report("unix_connect_abs_link", ls >= 0 && nn_in_child(ce_case, 1));
    report("bind_src_abs_link", nn_in_child(ce_case, 2));
    report("unix_bind_abs_link_dir", nn_in_child(ce_case, 3));
    report("unix_bind_host_node_absent", access("/root/mntiso-bound.sock", F_OK) != 0);
    if (ls >= 0) close(ls);
    unlink("/root/mntiso.sock"); unlink("/root/mntiso-bound.sock"); unlink("/root/mntiso-secret"); unlink("/ldx");
}

/* mntiso-strict (D1): in a mount namespace the tmpfs walk must refuse a link on its own
 * (/proc/mntselftest row strict_tmp_link_refused); unlinking the fixture removes the row. */
static int strict_case(int m) {
    (void)m;
    if (unshare(CLONE_NEWNS) < 0) return 91;
    static char buf[8192];
    if (slurp("/proc/mntselftest", buf, sizeof buf) <= 0) return 92;
    if (strstr(buf, "strict_tmp_link_refused: PASS")) return 0;
    return strstr(buf, "strict_tmp_link_refused: FAIL") ? 11 : 93;
}
static void strict_ns_suite(void) {
    unlink("/tmp/mntiso-strict-l");
    if (symlink("/tmp", "/tmp/mntiso-strict-l") < 0) { report("strict_tmp_link_refused", 0); return; }
    report("strict_tmp_link_refused", nn_in_child(strict_case, 0));
    unlink("/tmp/mntiso-strict-l");
}

/* mntiso-bindpin (D5): a bind root is pinned by (dev, ino). The source is swapped on the host side
 * (ns 0) after the bind; access through the bind must be ESTALE (or ELOOP for a link), never the
 * replacement. m: 0 dir swap (NEW), 1 link swap (host sentinel), 2 unrelated renames keep working. */
static int bp_child(int m, int rd, int wr) {
    if (unshare(CLONE_NEWNS) < 0) return 91;
    if (mount("/tmp/bp/vol", "/tmp/bp/mp", NULL, MS_BIND, NULL) < 0) return 92;
    char b[16] = {0}, c = 'x';
    if (slurp("/tmp/bp/mp/f", b, sizeof b) <= 0 || strncmp(b, "OLD", 3)) return 93;
    if (write(wr, &c, 1) != 1) return 94;
    if (read(rd, &c, 1) != 1) return 95;
    memset(b, 0, sizeof b); errno = 0;
    int n = slurp(m == 1 ? "/tmp/bp/mp/mntiso-secret" : "/tmp/bp/mp/f", b, sizeof b), e = errno;
    if (n > 0 && (!strncmp(b, "NEW", 3) || !strncmp(b, "HOST", 4))) { printf("  reached replacement: %s\n", b); return 11; }
    if (m == 2) return n > 0 && !strncmp(b, "OLD", 3) ? 0 : 12;
    if (n > 0 || (e != ESTALE && !(m == 1 && e == ELOOP))) { printf("  n=%d errno=%d\n", n, e); return 12; }
    return 0;
}
static int bp_case(int m) {
    system("rm -rf /tmp/bp"); mkdir("/tmp/bp", 0755); mkdir("/tmp/bp/vol", 0755); mkdir("/tmp/bp/mp", 0755);
    put("/tmp/bp/vol/f", "OLD", 0644);
    mkdir("/root", 0755); put("/root/mntiso-secret", "HOST", 0644);
    int up[2], dn[2];
    if (pipe(up) || pipe(dn)) return 90;
    pid_t c = fork();
    if (c == 0) _exit(bp_child(m, dn[0], up[1]));
    char x;
    if (read(up[0], &x, 1) != 1) { int st; waitpid(c, &st, 0); printf("  child setup failed %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1); return 1; }
    if (m == 2) { mkdir("/tmp/bp/other", 0755); rename("/tmp/bp/other", "/tmp/bp/other2"); rmdir("/tmp/bp/other2"); }
    else {
        if (m == 3) { unlink("/tmp/bp/vol/f"); if (rmdir("/tmp/bp/vol") < 0) return 5; }
        else rename("/tmp/bp/vol", "/tmp/bp/x");
        if (m == 0 || m == 3) { mkdir("/tmp/bp/vol", 0755); put("/tmp/bp/vol/f", "NEW", 0644); }
        else if (symlink("/root", "/tmp/bp/vol") < 0) return 2;
    }
    if (write(dn[1], &x, 1) != 1) return 3;
    int st = 0; waitpid(c, &st, 0);
    system("rm -rf /tmp/bp"); unlink("/root/mntiso-secret");
    if (!(WIFEXITED(st) && WEXITSTATUS(st) == 0)) printf("  child status=%d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    return WIFEXITED(st) && WEXITSTATUS(st) == 0 ? 0 : 4;
}
static void bindpin_suite(void) {
    report("bindpin_dir_swap", bp_case(0) == 0);
    report("bindpin_link_swap", bp_case(1) == 0);
    report("bindpin_unrelated_ok", bp_case(2) == 0);
    report("bindpin_dir_replaced_same_path", bp_case(3) == 0);
}

/* tmpfsino: tmpfs st_ino is a per-inode number, as on Linux. */
static int ino_of(const char *p) { struct stat st; return stat(p, &st) < 0 ? -1 : (int)(st.st_ino & 0x7fffffff); }
static void tmpfs_ino_suite(void) {
    system("rm -rf /tmp/ino"); mkdir("/tmp/ino", 0755);
    put("/tmp/ino/a", "x", 0644);
    int a = ino_of("/tmp/ino/a");
    rename("/tmp/ino/a", "/tmp/ino/b");
    report("tmpfs_ino_rename_stable", a > 0 && ino_of("/tmp/ino/b") == a);
    unlink("/tmp/ino/b"); put("/tmp/ino/b", "y", 0644);
    report("tmpfs_ino_new_file_fresh", ino_of("/tmp/ino/b") != a);
    int d = ino_of("/tmp/ino"); mkdir("/tmp/ino/d1", 0755); int d1 = ino_of("/tmp/ino/d1");
    rmdir("/tmp/ino/d1"); mkdir("/tmp/ino/d1", 0755);
    report("tmpfs_ino_new_dir_fresh", d1 > 0 && ino_of("/tmp/ino/d1") != d1 && d1 != d);
    link("/tmp/ino/b", "/tmp/ino/c");
    struct stat sb, sc; stat("/tmp/ino/b", &sb); stat("/tmp/ino/c", &sc);
    int fd = open("/tmp/ino/c", O_RDONLY); struct stat sf; int fr = fstat(fd, &sf); close(fd);
    report("tmpfs_ino_hardlink_shared", sb.st_ino == sc.st_ino && sb.st_nlink == 2 && fr == 0 && sf.st_ino == sb.st_ino);
    system("rm -rf /tmp/ino");
}

/* mntiso-bindpin cost: stat through a bind in a mntns, (a) quiet: no name-changing op between
 * stats (pin verified once), (b) churn: a rename in /tmp between every stat (one pin stat per op). */
static void bindbench(void) {
    system("rm -rf /tmp/bb"); mkdir("/tmp/bb", 0755); mkdir("/tmp/bb/vol", 0755); mkdir("/tmp/bb/mp", 0755);
    put("/tmp/bb/vol/f", "x", 0644); put("/tmp/bb/a", "x", 0644);
    if (unshare(CLONE_NEWNS) < 0) { printf("bindbench: unshare failed\n"); return; }
    if (mount("/tmp/bb/vol", "/tmp/bb/mp", NULL, MS_BIND, NULL) < 0) { printf("bindbench: bind failed\n"); return; }
    struct stat st; const int N = 20000;
    for (int round = 0; round < 3; round++) {
        long long t0 = now_ns();
        for (int i = 0; i < N; i++) stat("/tmp/bb/mp/f", &st);
        long long t1 = now_ns();
        for (int i = 0; i < N; i++) { rename(i & 1 ? "/tmp/bb/b" : "/tmp/bb/a", i & 1 ? "/tmp/bb/a" : "/tmp/bb/b"); stat("/tmp/bb/mp/f", &st); }
        long long t2 = now_ns();
        for (int i = 0; i < N; i++) { rename(i & 1 ? "/tmp/bb/b" : "/tmp/bb/a", i & 1 ? "/tmp/bb/a" : "/tmp/bb/b"); stat("/tmp/bb/vol/f", &st); }
        long long t3 = now_ns();
        printf("bindbench %d: quiet %lld ns/stat; churn via-bind %lld ns/(rename+stat); churn no-bind %lld ns/(rename+stat)\n",
               round, (t1 - t0) / N, (t2 - t1) / N, (t3 - t2) / N);
    }
    static char buf[8192];
    if (slurp("/proc/mntselftest", buf, sizeof buf) > 0) { char *q = strstr(buf, "bindpin_checks"); if (q) { char *e = strchr(q, '\n'); if (e) *e = 0; printf("%s\n", q); } }
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "bench")) { bench(); return 0; }
    if (argc > 1 && !strcmp(argv[1], "bindbench")) { bindbench(); return 0; }
    if (argc > 1 && !strcmp(argv[1], "child-ok")) return 42;
    if (argc > 1 && !strcmp(argv[1], "euid-chk")) return geteuid() == 1000 ? 42 : geteuid() == 0 ? 44 : 45;
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
    fdbindprop_suite();
    mro_suite();
    mntiso_procfd_suite();
    mntiso_e7_suite();
    e7flags_suite();
    e7x_suite();
    shmbind_suite();
    devbind_suite();
    mntiso_busy_suite();
    chroot_ns_suite();
    entry_ns_suite();
    strict_ns_suite();
    bindpin_suite();
    tmpfs_ino_suite();
    printf("%s\n", fails ? "mnttest: FAILED" : "mnttest: ALL PASSED");
    return fails ? 1 : 0;
}
