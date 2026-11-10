/*
 * cgtest: cgroup v2 core, cgroup namespace and cgroupfs (design-cgroup lanes
 * CG0/CG1/CG2).
 *
 * ns_*: the cgroup namespace (/proc/<pid>/ns/cgroup, unshare, clone,
 * inheritance across fork, threads and exec) and task membership accounting
 * under fork/exit/thread churn. cg_*: the cgroup2 filesystem at
 * /sys/fs/cgroup (statfs magic, mkdir/rmdir, no-internal-process rule,
 * migration and /proc/<pid>/cgroup, ns-relative paths incl. "/..", delegation
 * with a chowned directory, pids.current balance, /proc/filesystems,
 * /proc/cgroups and mountinfo).
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
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


/* ---- CG2: cgroupfs ------------------------------------------------------ */
#define CGR "/sys/fs/cgroup"
#ifndef CGROUP2_SUPER_MAGIC
#define CGROUP2_SUPER_MAGIC 0x63677270
#endif

static int mnt(void)
{
    if (mount("none", CGR, "cgroup2", 0, NULL) != 0 && errno != EBUSY) return -1;
    return 0;
}

static int rd(const char *path, char *buf, size_t n)
{
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    ssize_t r = read(fd, buf, n - 1);
    close(fd);
    if (r < 0) return -1;
    buf[r] = 0;
    return (int)r;
}

/* write a string; 0 or -errno (from open or write) */
static int wr(const char *path, const char *s)
{
    int fd = open(path, O_WRONLY);
    if (fd < 0) return -errno;
    ssize_t r = write(fd, s, strlen(s));
    int e = r < 0 ? -errno : 0;
    close(fd);
    return e;
}

static int wrpid(const char *path, pid_t p)
{
    char b[32]; snprintf(b, sizeof b, "%d", (int)p);
    return wr(path, b);
}

static int cg_of(pid_t p, char *out, size_t n)
{
    char path[64]; snprintf(path, sizeof path, "/proc/%d/cgroup", (int)p);
    return rd(path, out, n);
}

static int rmdir_retry(const char *p)
{
    for (int i = 0; i < 100; i++) {
        if (rmdir(p) == 0) return 0;
        if (errno != EBUSY) return -1;
        usleep(20000);
    }
    return -1;
}

/* Booted by init: /sys/fs/cgroup is cgroup2 before anything mounts it. */
static int t_cg_boot_mounted(void)
{
    struct statfs sf;
    CHECK(statfs(CGR, &sf) == 0);
    CHECK((unsigned long)sf.f_type == CGROUP2_SUPER_MAGIC);
    return 0;
}

static int t_cg_magic(void)
{
    CHECK(mnt() == 0);
    struct statfs sf;
    CHECK(statfs(CGR, &sf) == 0);
    CHECK((unsigned long)sf.f_type == CGROUP2_SUPER_MAGIC);
    int fd = open(CGR, O_RDONLY | O_DIRECTORY);
    CHECK(fd >= 0);
    CHECK(fstatfs(fd, &sf) == 0 && (unsigned long)sf.f_type == CGROUP2_SUPER_MAGIC);
    close(fd);
    fd = open(CGR "/cgroup.procs", O_RDONLY);
    CHECK(fd >= 0);
    CHECK(fstatfs(fd, &sf) == 0 && (unsigned long)sf.f_type == CGROUP2_SUPER_MAGIC);
    close(fd);
    /* the rest of /sys is not ours */
    CHECK(statfs("/sys", &sf) == 0 && (unsigned long)sf.f_type != CGROUP2_SUPER_MAGIC);
    struct stat st;
    CHECK(stat(CGR, &st) == 0 && S_ISDIR(st.st_mode));
    CHECK(stat("/sys/class", &st) == 0 && S_ISDIR(st.st_mode));
    return 0;
}

static int t_cg_mkdir_rmdir(void)
{
    CHECK(mnt() == 0);
    struct stat st;
    CHECK(mkdir(CGR "/cgt_a", 0755) == 0);
    CHECK(mkdir(CGR "/cgt_a", 0755) == -1 && errno == EEXIST);
    CHECK(stat(CGR "/cgt_a", &st) == 0 && S_ISDIR(st.st_mode));
    CHECK(stat(CGR "/cgt_a/cgroup.procs", &st) == 0 && S_ISREG(st.st_mode));
    char b[256];
    CHECK(rd(CGR "/cgt_a/cgroup.events", b, sizeof b) > 0 && strstr(b, "populated 0"));
    CHECK(rd(CGR "/cgt_a/cgroup.type", b, sizeof b) > 0 && !strncmp(b, "domain", 6));
    /* readdir lists the control files and the child */
    CHECK(mkdir(CGR "/cgt_a/sub", 0755) == 0);
    DIR *d = opendir(CGR "/cgt_a");
    CHECK(d != NULL);
    int procs = 0, sub = 0;
    struct dirent *e;
    while ((e = readdir(d))) {
        if (!strcmp(e->d_name, "cgroup.procs")) procs = 1;
        if (!strcmp(e->d_name, "sub") && e->d_type == DT_DIR) sub = 1;
    }
    closedir(d);
    CHECK(procs && sub);
    CHECK(rmdir(CGR "/cgt_a") == -1 && errno == EBUSY);     /* has a child */
    CHECK(rmdir(CGR "/cgt_a/sub") == 0);
    CHECK(rmdir(CGR "/cgt_a") == 0);
    CHECK(stat(CGR "/cgt_a", &st) == -1 && errno == ENOENT);
    CHECK(unlink(CGR "/cgroup.procs") == -1);
    CHECK(rmdir(CGR) == -1);
    return 0;
}

static int t_cg_files(void)
{
    CHECK(mnt() == 0);
    char b[256];
    CHECK(rd(CGR "/cgroup.controllers", b, sizeof b) > 0 && strstr(b, "pids"));
    CHECK(wr(CGR "/cgroup.subtree_control", "+bogus") == -2);   /* ENOENT */
    CHECK(wr(CGR "/cgroup.subtree_control", "pids") == -22);    /* EINVAL */
    CHECK(wr(CGR "/cgroup.controllers", "x") == -13);           /* read-only */
    CHECK(rd(CGR "/cgroup.stat", b, sizeof b) > 0 && strstr(b, "nr_descendants"));
    CHECK(rd(CGR "/cgroup.max.depth", b, sizeof b) > 0 && !strncmp(b, "max", 3));
    return 0;
}

/* a process that parks in `cg` (a path below CGR) until told to exit */
static pid_t spawn_in(const char *procs_file, int *kill_fd)
{
    int p[2];
    if (pipe(p) != 0) return -1;
    pid_t c = fork();
    if (c == 0) {
        close(p[1]);
        char x;
        (void)read(p[0], &x, 1);
        _exit(0);
    }
    close(p[0]);
    *kill_fd = p[1];
    if (wrpid(procs_file, c) != 0) { close(p[1]); return -1; }
    return c;
}

static int t_cg_busy_noproc(void)
{
    CHECK(mnt() == 0);
    CHECK(mkdir(CGR "/cgt_b", 0755) == 0);
    int k;
    pid_t c = spawn_in(CGR "/cgt_b/cgroup.procs", &k);
    CHECK(c > 0);
    char b[256];
    CHECK(rd(CGR "/cgt_b/cgroup.procs", b, sizeof b) > 0 && atoi(b) == c);
    CHECK(rd(CGR "/cgt_b/cgroup.events", b, sizeof b) > 0 && strstr(b, "populated 1"));
    CHECK(rmdir(CGR "/cgt_b") == -1 && errno == EBUSY);      /* has a task */
    /* no-internal-process: a cgroup holding a task cannot enable controllers */
    CHECK(wr(CGR "/cgroup.subtree_control", "+pids") == 0);
    CHECK(wr(CGR "/cgt_b/cgroup.subtree_control", "+pids") == -EBUSY);
    /* and a cgroup with subtree_control set cannot receive tasks */
    CHECK(mkdir(CGR "/cgt_c", 0755) == 0);
    CHECK(wr(CGR "/cgt_c/cgroup.subtree_control", "+pids") == 0);
    CHECK(wrpid(CGR "/cgt_c/cgroup.procs", c) == -EBUSY);
    CHECK(wr(CGR "/cgt_c/cgroup.subtree_control", "-pids") == 0);
    CHECK(rmdir(CGR "/cgt_c") == 0);
    /* back to the root, then the directory can go */
    CHECK(wrpid(CGR "/cgroup.procs", c) == 0);
    CHECK(rmdir(CGR "/cgt_b") == 0);
    close(k);
    CHECK(waitpid(c, NULL, 0) == c);
    /* init keeps the controllers enabled on the root; nothing to undo */
    return 0;
}

static int t_cg_migrate(void)
{
    CHECK(mnt() == 0);
    CHECK(mkdir(CGR "/cgt_m", 0755) == 0);
    char b[256], want[64];
    pid_t me = getpid();
    CHECK(cg_of(me, b, sizeof b) > 0 && !strcmp(b, "0::/\n"));
    CHECK(wrpid(CGR "/cgt_m/cgroup.procs", me) == 0);
    CHECK(rd("/proc/self/cgroup", b, sizeof b) > 0 && !strcmp(b, "0::/cgt_m\n"));
    CHECK(rd(CGR "/cgt_m/cgroup.procs", b, sizeof b) > 0);
    snprintf(want, sizeof want, "%d\n", (int)me);
    CHECK(strstr(b, want) != NULL);
    /* a fork inherits the cgroup */
    pid_t c = fork();
    if (c == 0) {
        char x[64];
        _exit(rd("/proc/self/cgroup", x, sizeof x) > 0 && !strcmp(x, "0::/cgt_m\n") ? 0 : 1);
    }
    CHECK(wait_ok(c));
    CHECK(wr(CGR "/cgt_m/cgroup.procs", "999999") == -3);     /* ESRCH */
    CHECK(wrpid(CGR "/cgroup.procs", me) == 0);
    CHECK(rd("/proc/self/cgroup", b, sizeof b) > 0 && !strcmp(b, "0::/\n"));
    CHECK(rmdir(CGR "/cgt_m") == 0);
    return 0;
}

/* ns-relative paths, including "/.." for a task outside the namespace root */
static int t_cg_cgns_rel(void)
{
    CHECK(mnt() == 0);
    CHECK(mkdir(CGR "/cgt_n", 0755) == 0);
    CHECK(mkdir(CGR "/cgt_n/b", 0755) == 0);
    int qp[2], go[2], qgo[2];
    CHECK(pipe(qp) == 0 && pipe(go) == 0 && pipe(qgo) == 0);
    pid_t p = fork();
    if (p == 0) {
        char x[128];
        if (wrpid(CGR "/cgt_n/b/cgroup.procs", getpid()) != 0) _exit(1);
        if (unshare(CLONE_NEWCGROUP) != 0) _exit(2);
        if (rd("/proc/self/cgroup", x, sizeof x) <= 0 || strcmp(x, "0::/\n")) _exit(3);
        pid_t q = fork();
        if (q == 0) { char z; (void)read(qgo[0], &z, 1); _exit(0); }
        (void)write(qp[1], &q, sizeof q);
        char z; (void)read(go[0], &z, 1);   /* parent moved q to cgt_n */
        char path[64]; snprintf(path, sizeof path, "/proc/%d/cgroup", (int)q);
        if (rd(path, x, sizeof x) <= 0 || strcmp(x, "0::/..\n")) _exit(4);
        /* q is outside this namespace: the move is ENOENT here */
        if (wrpid(CGR "/cgroup.procs", q) != -2) _exit(5);
        /* the filesystem root is the namespace root */
        struct stat st;
        if (stat(CGR "/cgt_n", &st) == 0) _exit(6);
        (void)write(qgo[1], "x", 1);
        waitpid(q, NULL, 0);
        _exit(0);
    }
    pid_t q;
    CHECK(read(qp[0], &q, sizeof q) == sizeof q);
    char b[128];
    CHECK(cg_of(q, b, sizeof b) > 0 && !strcmp(b, "0::/cgt_n/b\n"));   /* initial ns view */
    CHECK(wrpid(CGR "/cgt_n/cgroup.procs", q) == 0);
    CHECK(cg_of(q, b, sizeof b) > 0 && !strcmp(b, "0::/cgt_n\n"));
    CHECK(write(go[1], "x", 1) == 1);
    CHECK(wait_ok(p));
    CHECK(wrpid(CGR "/cgroup.procs", getpid()) == 0);
    CHECK(rmdir_retry(CGR "/cgt_n/b") == 0);
    CHECK(rmdir_retry(CGR "/cgt_n") == 0);
    return 0;
}

static int t_cg_delegate(void)
{
    CHECK(mnt() == 0);
    CHECK(mkdir(CGR "/cgt_d", 0755) == 0);
    int up[2], dn[2];
    CHECK(pipe(up) == 0 && pipe(dn) == 0);
    pid_t c = fork();
    if (c == 0) {
        char z;
        if (setgid(1000) != 0 || setuid(1000) != 0) _exit(1);
        (void)write(up[1], "r", 1);          /* ready */
        (void)read(dn[0], &z, 1);            /* moved into cgt_d by root */
        if (mkdir(CGR "/cgt_d/sub", 0755) == 0 || errno != EACCES) _exit(2);   /* not delegated yet */
        (void)write(up[1], "d", 1);
        (void)read(dn[0], &z, 1);            /* now chowned */
        if (mkdir(CGR "/cgt_d/sub", 0755) != 0) _exit(3);
        if (wrpid(CGR "/cgt_d/sub/cgroup.procs", getpid()) != 0) _exit(4);
        /* leaving the delegated subtree needs write on the common ancestor (root) */
        if (wrpid(CGR "/cgroup.procs", getpid()) != -EACCES) _exit(5);
        /* siblings outside are root's */
        if (mkdir(CGR "/cgt_d_other", 0755) == 0 || errno != EACCES) _exit(6);
        if (wr(CGR "/cgroup.subtree_control", "+pids") != -EACCES) _exit(8);
        /* back inside, remove the child we made */
        if (wrpid(CGR "/cgt_d/cgroup.procs", getpid()) != 0) _exit(9);
        if (rmdir(CGR "/cgt_d/sub") != 0) _exit(10);
        (void)write(up[1], "k", 1);
        (void)read(dn[0], &z, 1);
        _exit(0);
    }
    char z;
    CHECK(read(up[0], &z, 1) == 1);
    CHECK(wrpid(CGR "/cgt_d/cgroup.procs", c) == 0);
    CHECK(write(dn[1], "x", 1) == 1);
    CHECK(read(up[0], &z, 1) == 1 && z == 'd');
    CHECK(chown(CGR "/cgt_d", 1000, 1000) == 0);
    struct stat st;
    CHECK(stat(CGR "/cgt_d", &st) == 0 && st.st_uid == 1000 && st.st_gid == 1000);
    CHECK(write(dn[1], "x", 1) == 1);
    CHECK(read(up[0], &z, 1) == 1 && z == 'k');
    CHECK(write(dn[1], "x", 1) == 1);
    CHECK(wait_ok(c));
    CHECK(rmdir_retry(CGR "/cgt_d") == 0);
    return 0;
}

/* pids.current must return to its baseline: CG1's charge/uncharge balance */
static int cur(const char *path)
{
    char b[64];
    if (rd(path, b, sizeof b) <= 0) return -1;
    return atoi(b);
}

static void *idle_thread(void *a) { (void)a; usleep(1000); return NULL; }

static int t_cg_pids_balance(void)
{
    CHECK(mnt() == 0);
    CHECK(wr(CGR "/cgroup.subtree_control", "+pids") == 0);
    CHECK(mkdir(CGR "/cgt_p", 0755) == 0);
    CHECK(cur(CGR "/cgt_p/pids.current") == 0);
    int up[2], dn[2];
    CHECK(pipe(up) == 0 && pipe(dn) == 0);
    pid_t c = fork();
    if (c == 0) {
        char z;
        (void)read(dn[0], &z, 1);                       /* parent moved us in */
        for (int i = 0; i < 40; i++) {
            pid_t k = fork();
            if (k == 0) _exit(0);
            if (k < 0 || waitpid(k, NULL, 0) != k) _exit(1);
        }
        for (int r = 0; r < 4; r++) {
            pthread_t t[8];
            for (int i = 0; i < 8; i++) if (pthread_create(&t[i], NULL, idle_thread, NULL)) _exit(2);
            for (int i = 0; i < 8; i++) pthread_join(t[i], NULL);
        }
        (void)write(up[1], "d", 1);
        (void)read(dn[0], &z, 1);
        _exit(0);
    }
    CHECK(wrpid(CGR "/cgt_p/cgroup.procs", c) == 0);
    CHECK(cur(CGR "/cgt_p/pids.current") == 1);
    char z;
    CHECK(write(dn[1], "x", 1) == 1);
    CHECK(read(up[0], &z, 1) == 1);                    /* churn done, c still alive */
    int v = -1;
    for (int i = 0; i < 100 && (v = cur(CGR "/cgt_p/pids.current")) != 1; i++) usleep(20000);
    CHECK(v == 1);
    CHECK(write(dn[1], "x", 1) == 1);
    CHECK(wait_ok(c));
    for (int i = 0; i < 100 && (v = cur(CGR "/cgt_p/pids.current")) != 0; i++) usleep(20000);
    CHECK(v == 0);
    char b[64];
    CHECK(rd(CGR "/cgt_p/cgroup.procs", b, sizeof b) == 0);
    CHECK(rmdir(CGR "/cgt_p") == 0);
    /* init keeps the controllers enabled on the root; nothing to undo */
    return 0;
}

static int t_cg_pids_max(void)
{
    CHECK(mnt() == 0);
    CHECK(wr(CGR "/cgroup.subtree_control", "+pids") == 0);
    CHECK(mkdir(CGR "/cgt_q", 0755) == 0);
    CHECK(wr(CGR "/cgt_q/pids.max", "2") == 0);
    int up[2], dn[2];
    CHECK(pipe(up) == 0 && pipe(dn) == 0);
    pid_t c = fork();
    if (c == 0) {
        char z;
        (void)read(dn[0], &z, 1);
        pid_t k1 = fork();
        if (k1 == 0) { (void)read(dn[0], &z, 1); _exit(0); }
        if (k1 < 0) _exit(1);
        pid_t k2 = fork();
        if (k2 >= 0 || errno != EAGAIN) _exit(2);
        (void)write(up[1], "d", 1);
        (void)write(dn[1], "x", 1);                    /* release k1 (we hold both ends) */
        waitpid(k1, NULL, 0);
        _exit(0);
    }
    CHECK(wrpid(CGR "/cgt_q/cgroup.procs", c) == 0);
    CHECK(write(dn[1], "x", 1) == 1);
    char z;
    CHECK(read(up[0], &z, 1) == 1);
    CHECK(wait_ok(c));
    char b[64];
    CHECK(rd(CGR "/cgt_q/pids.events", b, sizeof b) > 0 && !strncmp(b, "max 1", 5));
    CHECK(rmdir_retry(CGR "/cgt_q") == 0);
    /* init keeps the controllers enabled on the root; nothing to undo */
    return 0;
}

static int has_word(const char *buf, const char *w)
{
    size_t n = strlen(w);
    for (const char *p = buf; (p = strstr(p, w)); p++)
        if ((p == buf || p[-1] == ' ') && (p[n] == ' ' || p[n] == '\n' || p[n] == 0)) return 1;
    return 0;
}

/* init enables the controllers at boot, and the user slice too. */
static int t_cg_boot_controllers(void)
{
    char b[128];
    CHECK(rd(CGR "/cgroup.subtree_control", b, sizeof b) > 0);
    CHECK(has_word(b, "cpu") && has_word(b, "io") && has_word(b, "memory") && has_word(b, "pids"));
    struct stat st;
    CHECK(stat(CGR "/user.slice", &st) == 0 && S_ISDIR(st.st_mode));
    CHECK(rd(CGR "/user.slice/cgroup.subtree_control", b, sizeof b) > 0 && has_word(b, "pids"));
    return 0;
}

/* A fresh child exposes every enabled controller's files; knobs round-trip. */
static int t_cg_child_ctrl_files(void)
{
    char b[256];
    CHECK(mnt() == 0);
    CHECK(mkdir(CGR "/cgt_cf", 0755) == 0);
    CHECK(rd(CGR "/cgt_cf/cgroup.controllers", b, sizeof b) > 0);
    CHECK(has_word(b, "cpu") && has_word(b, "io") && has_word(b, "memory") && has_word(b, "pids"));
    static const char *const f[] = { "pids.max", "pids.current", "memory.max", "memory.current",
        "memory.high", "cpu.max", "cpu.weight", "cpu.stat", "io.max", "io.weight" };
    for (unsigned i = 0; i < sizeof f / sizeof *f; i++) {
        char p[96]; snprintf(p, sizeof p, CGR "/cgt_cf/%s", f[i]);
        struct stat st;
        if (stat(p, &st) != 0) { printf("  missing %s\n", f[i]); return 1; }
    }
    CHECK(rd(CGR "/cgt_cf/pids.max", b, sizeof b) > 0 && !strncmp(b, "max", 3));
    CHECK(wr(CGR "/cgt_cf/pids.max", "7") == 0);
    CHECK(rd(CGR "/cgt_cf/pids.max", b, sizeof b) > 0 && !strncmp(b, "7\n", 2));
    CHECK(wr(CGR "/cgt_cf/memory.max", "67108864") == 0);
    CHECK(rd(CGR "/cgt_cf/memory.max", b, sizeof b) > 0 && !strncmp(b, "67108864\n", 9));
    CHECK(wr(CGR "/cgt_cf/cpu.weight", "200") == 0);
    CHECK(rd(CGR "/cgt_cf/cpu.weight", b, sizeof b) > 0 && !strncmp(b, "200\n", 4));
    CHECK(wr(CGR "/cgt_cf/cpu.weight", "0") == -EINVAL);
    CHECK(wr(CGR "/cgt_cf/cpu.max", "50000 100000") == 0);
    CHECK(rd(CGR "/cgt_cf/cpu.max", b, sizeof b) > 0 && !strncmp(b, "50000 100000", 12));
    CHECK(wr(CGR "/cgt_cf/memory.current", "1") == -EACCES);
    /* a grandchild gets nothing: the child enabled no controller for it */
    CHECK(mkdir(CGR "/cgt_cf/g", 0755) == 0);
    CHECK(rd(CGR "/cgt_cf/g/cgroup.controllers", b, sizeof b) >= 0 && !has_word(b, "pids"));
    struct stat st;
    CHECK(stat(CGR "/cgt_cf/g/pids.max", &st) == -1 && errno == ENOENT);
    CHECK(rmdir(CGR "/cgt_cf/g") == 0);
    /* a recreated cgroup starts from defaults */
    CHECK(rmdir(CGR "/cgt_cf") == 0);
    CHECK(mkdir(CGR "/cgt_cf", 0755) == 0);
    CHECK(rd(CGR "/cgt_cf/pids.max", b, sizeof b) > 0 && !strncmp(b, "max", 3));
    CHECK(rd(CGR "/cgt_cf/cpu.weight", b, sizeof b) > 0 && !strncmp(b, "100\n", 4));
    CHECK(rmdir(CGR "/cgt_cf") == 0);
    return 0;
}

/* pids.max of a child is enforced without anyone enabling +pids first. */
static int t_cg_child_pids_enforced(void)
{
    CHECK(mnt() == 0);
    CHECK(mkdir(CGR "/cgt_pe", 0755) == 0);
    CHECK(wr(CGR "/cgt_pe/pids.max", "2") == 0);
    int up[2], dn[2];
    CHECK(pipe(up) == 0 && pipe(dn) == 0);
    pid_t c = fork();
    if (c == 0) {
        char z;
        (void)read(dn[0], &z, 1);
        pid_t k1 = fork();
        if (k1 == 0) { (void)read(dn[0], &z, 1); _exit(0); }
        if (k1 < 0) _exit(1);
        pid_t k2 = fork();
        if (k2 >= 0 || errno != EAGAIN) _exit(2);
        (void)write(up[1], "d", 1);
        (void)write(dn[1], "x", 1);
        waitpid(k1, NULL, 0);
        _exit(0);
    }
    CHECK(wrpid(CGR "/cgt_pe/cgroup.procs", c) == 0);
    CHECK(write(dn[1], "x", 1) == 1);
    char z;
    CHECK(read(up[0], &z, 1) == 1);
    CHECK(wait_ok(c));
    CHECK(rmdir_retry(CGR "/cgt_pe") == 0);
    return 0;
}

/* crun opens control files relative to a cgroup directory fd (openat, openat2). */
static int t_cg_dirfd_open(void)
{
    char b[64];
    CHECK(mnt() == 0);
    CHECK(mkdir(CGR "/cgt_df", 0755) == 0);
    int d = open(CGR "/cgt_df", O_RDONLY | O_DIRECTORY);
    CHECK(d >= 0);
    int f = openat(d, "pids.max", O_WRONLY);
    CHECK(f >= 0);
    CHECK(write(f, "9", 1) == 1);
    close(f);
    struct { unsigned long long flags, mode, resolve; } how = { O_RDONLY, 0, 0 };
    long r = syscall(SYS_openat2, d, "pids.max", &how, sizeof how);
    CHECK(r >= 0);
    CHECK(read((int)r, b, sizeof b) > 0 && b[0] == '9');
    close((int)r);
    close(d);
    CHECK(rmdir(CGR "/cgt_df") == 0);
    return 0;
}

static int t_cg_proc_files(void)
{
    CHECK(mnt() == 0);
    char b[4096];
    CHECK(rd("/proc/filesystems", b, sizeof b) > 0 && strstr(b, "cgroup2"));
    CHECK(rd("/proc/cgroups", b, sizeof b) > 0 && !strncmp(b, "#subsys_name", 12) && strstr(b, "pids"));
    CHECK(rd("/proc/self/mountinfo", b, sizeof b) > 0);
    char *l = strstr(b, " - cgroup2 ");
    CHECK(l != NULL);
    CHECK(strstr(b, "/sys/fs/cgroup ") != NULL);
    CHECK(rd("/proc/mounts", b, sizeof b) > 0 && strstr(b, "cgroup2 /sys/fs/cgroup cgroup2"));
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
        { "cg_boot_mounted", t_cg_boot_mounted }, { "cg_magic", t_cg_magic }, { "cg_mkdir_rmdir", t_cg_mkdir_rmdir },
        { "cg_files", t_cg_files }, { "cg_busy_noproc", t_cg_busy_noproc },
        { "cg_migrate", t_cg_migrate }, { "cg_cgns_rel", t_cg_cgns_rel },
        { "cg_delegate", t_cg_delegate }, { "cg_pids_balance", t_cg_pids_balance },
        { "cg_pids_max", t_cg_pids_max }, { "cg_proc_files", t_cg_proc_files },
        { "cg_boot_controllers", t_cg_boot_controllers }, { "cg_child_ctrl_files", t_cg_child_ctrl_files },
        { "cg_child_pids_enforced", t_cg_child_pids_enforced }, { "cg_dirfd_open", t_cg_dirfd_open },
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
