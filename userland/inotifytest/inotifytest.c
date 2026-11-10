/*
 * inotifytest -- inotify semantics (design-inotify.md, lane L1: tmpfs dirent
 * events). Static zig-cc build, runs as root; one "case: PASS|FAIL|SKIP" line
 * per case. The real cases need the kernel booted with vfs.inotify=1 and SKIP
 * without it; `inotifytest force` runs them regardless (fail-first evidence
 * against the silent stub). The kill-switch guard runs only with the knob off.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sched.h>
#include <sys/inotify.h>
#include <sys/mount.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int npass, nfail;
static char base[64];

static void report(const char *name, int ok)
{
    printf("%s: %s\n", name, ok ? "PASS" : "FAIL");
    if (ok) npass++; else nfail++;
}

static long now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static void path(char *out, const char *rel) { snprintf(out, 128, "%s/%s", base, rel); }

static void touch(const char *rel)
{
    char p[128]; path(p, rel);
    int fd = open(p, O_CREAT | O_WRONLY, 0644);
    if (fd >= 0) close(fd);
}
static void mkd(const char *rel) { char p[128]; path(p, rel); mkdir(p, 0755); }
static void ren(const char *a, const char *b)
{
    char pa[128], pb[128]; path(pa, a); path(pb, b); rename(pa, pb);
}
static void rm(const char *rel) { char p[128]; path(p, rel); unlink(p); }

static int watch(int ifd, const char *rel, uint32_t mask)
{
    char p[128]; path(p, rel);
    return inotify_add_watch(ifd, p, mask);
}

/* Parse the next event out of buf at *off; returns pointer or NULL. */
static const struct inotify_event *next_ev(const char *buf, ssize_t n, ssize_t *off)
{
    if (*off + (ssize_t)sizeof(struct inotify_event) > n) return NULL;
    const struct inotify_event *e = (const struct inotify_event *)(buf + *off);
    *off += sizeof(*e) + e->len;
    return e;
}

/* Fork a child that sleeps ms then runs fn(). */
static pid_t later(int ms, void (*fn)(void))
{
    pid_t p = fork();
    if (p == 0) { usleep(ms * 1000); fn(); _exit(0); }
    return p;
}

static void c_rename_in(void) { ren("y/f", "x/f"); }
static void c_create_f(void) { touch("x/f"); }

static void setup(void)
{
    snprintf(base, sizeof base, "/run/inot-%d", getpid());
    mkdir(base, 0755);
    mkd("x"); mkd("y");
}

static int epoll_et(int ifd)
{
    int ep = epoll_create1(0);
    struct epoll_event ev = { .events = EPOLLIN | EPOLLET, .data.fd = ifd };
    epoll_ctl(ep, EPOLL_CTL_ADD, ifd, &ev);
    return ep;
}

static void t_create(void)
{
    int ifd = inotify_init1(IN_NONBLOCK);
    int wd = watch(ifd, "x", IN_CREATE | IN_DELETE);
    touch("x/a");
    char buf[512]; ssize_t n = read(ifd, buf, sizeof buf), off = 0;
    const struct inotify_event *e = n > 0 ? next_ev(buf, n, &off) : NULL;
    report("t_create", wd > 0 && e && e->wd == wd && e->mask == IN_CREATE && !strcmp(e->name, "a"));
    close(ifd);
}

static void t_delete(void)
{
    touch("x/b");
    int ifd = inotify_init1(IN_NONBLOCK);
    int wd = watch(ifd, "x", IN_CREATE | IN_DELETE);
    rm("x/b");
    char buf[512]; ssize_t n = read(ifd, buf, sizeof buf), off = 0;
    const struct inotify_event *e = n > 0 ? next_ev(buf, n, &off) : NULL;
    report("t_delete", wd > 0 && e && e->mask == IN_DELETE && !strcmp(e->name, "b"));
    close(ifd);
}

static void t_move_pair(void)
{
    touch("x/m1");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "x", IN_MOVED_FROM | IN_MOVED_TO);
    ren("x/m1", "x/m2");
    char buf[512]; ssize_t n = read(ifd, buf, sizeof buf), off = 0;
    const struct inotify_event *a = n > 0 ? next_ev(buf, n, &off) : NULL;
    const struct inotify_event *b = a ? next_ev(buf, n, &off) : NULL;
    report("t_move_pair", a && b && a->mask == IN_MOVED_FROM && b->mask == IN_MOVED_TO
           && a->cookie != 0 && a->cookie == b->cookie
           && !strcmp(a->name, "m1") && !strcmp(b->name, "m2"));
    close(ifd);
}

static void t_moved_to(void)
{
    touch("y/f");
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    int ep = epoll_et(ifd);
    watch(ifd, "x", IN_MOVED_TO | IN_CREATE);
    pid_t c = later(100, c_rename_in);
    long t0 = now_ms();
    struct epoll_event out;
    int r = epoll_wait(ep, &out, 1, 3000);
    long dt = now_ms() - t0;
    char buf[512]; ssize_t n = read(ifd, buf, sizeof buf), off = 0;
    const struct inotify_event *e = n > 0 ? next_ev(buf, n, &off) : NULL;
    waitpid(c, NULL, 0);
    /* The child renames at ~100 ms; the wake must follow within 100 ms. */
    report("t_moved_to", r == 1 && e && e->mask == IN_MOVED_TO && !strcmp(e->name, "f") && dt < 200);
    close(ep); close(ifd);
    rm("x/f");
}

static void t_create_waitfile(void)
{
    /* fsnotify WaitForFile shape: nonblocking fd, epoll ET, watch, stat, then
     * another process creates the file. */
    char p[128]; path(p, "x/f");
    unlink(p);
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    int ep = epoll_et(ifd);
    int wd = watch(ifd, "x", IN_CREATE | IN_MOVED_TO);
    struct stat st;
    int s = stat(p, &st);
    pid_t c = later(50, c_create_f);
    struct epoll_event out;
    int r = epoll_wait(ep, &out, 1, 3000);
    char buf[512]; ssize_t n = read(ifd, buf, sizeof buf), off = 0;
    const struct inotify_event *e = n > 0 ? next_ev(buf, n, &off) : NULL;
    waitpid(c, NULL, 0);
    report("t_create_waitfile", wd > 0 && s < 0 && r == 1 && e && e->mask == IN_CREATE && !strcmp(e->name, "f"));
    close(ep); close(ifd);
    rm("x/f");
}

static void t_layout(void)
{
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "x", IN_CREATE);
    touch("x/f");
    char buf[512]; memset(buf, 0xAA, sizeof buf);
    ssize_t n = read(ifd, buf, sizeof buf);
    const struct inotify_event *e = (const struct inotify_event *)buf;
    report("t_layout", n == 16 + 16 && e->len == 16 && e->name[0] == 'f'
           && e->name[1] == 0 && e->name[15] == 0);
    close(ifd);
    rm("x/f");
}

static void t_einval_small(void)
{
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "x", IN_CREATE);
    touch("x/f");
    char buf[512];
    ssize_t n1 = read(ifd, buf, 16); int e1 = errno;
    ssize_t n2 = read(ifd, buf, sizeof buf);   /* event still queued */
    report("t_einval_small", n1 < 0 && e1 == EINVAL && n2 == 32);
    close(ifd);
    rm("x/f");
}

static void t_fionread(void)
{
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "x", IN_CREATE | IN_DELETE);
    int z = -1; ioctl(ifd, FIONREAD, &z);
    touch("x/f");
    rm("x/f");
    int n = -1; ioctl(ifd, FIONREAD, &n);
    report("t_fionread", z == 0 && n == 64);
    close(ifd);
}

static void t_rmwatch_ignored(void)
{
    int ifd = inotify_init1(IN_NONBLOCK);
    int wd = watch(ifd, "x", IN_CREATE);
    int r = inotify_rm_watch(ifd, wd);
    char buf[512]; ssize_t n = read(ifd, buf, sizeof buf);
    const struct inotify_event *e = (const struct inotify_event *)buf;
    int r2 = inotify_rm_watch(ifd, wd); int e2 = errno;
    touch("x/f"); rm("x/f");
    ssize_t n3 = read(ifd, buf, sizeof buf);
    report("t_rmwatch_ignored", r == 0 && n == 16 && e->wd == wd && e->mask == IN_IGNORED
           && r2 < 0 && e2 == EINVAL && n3 < 0);
    close(ifd);
}

static void t_block_read(void)
{
    int ifd = inotify_init1(0);
    watch(ifd, "x", IN_CREATE);
    char p[128]; path(p, "x/f"); unlink(p);
    pid_t c = later(100, c_create_f);
    long t0 = now_ms();
    char buf[512]; ssize_t n = read(ifd, buf, sizeof buf);
    long dt = now_ms() - t0;
    const struct inotify_event *e = (const struct inotify_event *)buf;
    waitpid(c, NULL, 0);
    report("t_block_read", n > 0 && e->mask == IN_CREATE && dt < 1000);
    close(ifd);
    rm("x/f");
}

static void t_silent_unrelated(void)
{
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "x", IN_CREATE | IN_DELETE | IN_MOVED_TO | IN_MOVED_FROM);
    touch("y/zz"); rm("y/zz");
    char buf[512]; ssize_t n = read(ifd, buf, sizeof buf);
    report("t_silent_unrelated", n < 0 && errno == EAGAIN);
    close(ifd);
}

static void t_dir_events(void)
{
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "x", IN_CREATE | IN_DELETE);
    mkd("x/d");
    char p[128]; path(p, "x/d"); rmdir(p);
    char buf[512]; ssize_t n = read(ifd, buf, sizeof buf), off = 0;
    const struct inotify_event *a = n > 0 ? next_ev(buf, n, &off) : NULL;
    const struct inotify_event *b = a ? next_ev(buf, n, &off) : NULL;
    report("t_dir_events", a && b && a->mask == (IN_CREATE | IN_ISDIR) && b->mask == (IN_DELETE | IN_ISDIR));
    close(ifd);
}

static void t_dir_follow(void)
{
    mkd("fa");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "fa", IN_CREATE);
    ren("fa", "fb");
    touch("fb/n");
    char buf[512]; ssize_t n = read(ifd, buf, sizeof buf);
    const struct inotify_event *e = (const struct inotify_event *)buf;
    report("t_dir_follow", n > 0 && e->mask == IN_CREATE && !strcmp(e->name, "n"));
    close(ifd);
}

static void t_eacces(void)
{
    char p[128]; path(p, "secret");
    int fd = open(p, O_CREAT | O_WRONLY, 0600);
    if (fd >= 0) close(fd);
    chmod(base, 0755);
    pid_t c = fork();
    if (c == 0) {
        if (setgid(65534) || setuid(65534)) _exit(2);
        int ifd = inotify_init1(IN_NONBLOCK);
        int w = inotify_add_watch(ifd, p, IN_MODIFY);
        _exit(w < 0 && errno == EACCES ? 0 : 1);
    }
    int st = 0; waitpid(c, &st, 0);
    report("t_eacces", WIFEXITED(st) && WEXITSTATUS(st) == 0);
}


/* ---- L4: flags, overflow, limits ---------------------------------------- */
#define LIM_Q "/proc/sys/fs/inotify/max_queued_events"
#define LIM_I "/proc/sys/fs/inotify/max_user_instances"
#define LIM_W "/proc/sys/fs/inotify/max_user_watches"

static int lim_set(const char *f, long v)
{
    char b[24]; int n = snprintf(b, sizeof b, "%ld\n", v);
    int fd = open(f, O_WRONLY);
    if (fd < 0) return -errno;
    ssize_t r = write(fd, b, n); int e = errno; close(fd);
    return r == n ? 0 : -e;
}
static long lim_get(const char *f)
{
    char b[32]; int fd = open(f, O_RDONLY);
    if (fd < 0) return -1;
    ssize_t n = read(fd, b, sizeof b - 1); close(fd);
    if (n <= 0) return -1;
    b[n] = 0; return atol(b);
}

static int count_events(int ifd, int *overflow, int *ign)
{
    char buf[4096]; int n = 0;
    *overflow = 0; *ign = 0;
    for (;;) {
        ssize_t r = read(ifd, buf, sizeof buf), off = 0;
        if (r <= 0) break;
        const struct inotify_event *e;
        while ((e = next_ev(buf, r, &off))) {
            n++;
            if (e->mask & IN_Q_OVERFLOW) (*overflow)++;
            if (e->mask & IN_IGNORED) (*ign)++;
        }
    }
    return n;
}

static void t_oneshot(void)
{
    int ifd = inotify_init1(IN_NONBLOCK);
    int wd = watch(ifd, "x", IN_CREATE | IN_ONESHOT);
    touch("x/o1"); touch("x/o2");
    int ov, ig, n = count_events(ifd, &ov, &ig);
    report("t_oneshot", wd > 0 && n == 2 && ig == 1);
    close(ifd); rm("x/o1"); rm("x/o2");
}

static void t_mask_add(void)
{
    int ifd = inotify_init1(IN_NONBLOCK);
    int w1 = watch(ifd, "x", IN_CREATE);
    int w2 = watch(ifd, "x", IN_DELETE | IN_MASK_ADD);
    touch("x/ma"); rm("x/ma");
    int ov, ig, n = count_events(ifd, &ov, &ig);
    report("t_mask_add", w1 > 0 && w1 == w2 && n == 2);
    close(ifd);
}

static void t_mask_create_eexist(void)
{
    int ifd = inotify_init1(IN_NONBLOCK);
    int w1 = watch(ifd, "x", IN_CREATE);
    int w2 = watch(ifd, "x", IN_CREATE | IN_MASK_CREATE); int e2 = errno;
    int w3 = watch(ifd, "y", IN_CREATE | IN_MASK_CREATE);
    int w4 = watch(ifd, "y", IN_CREATE | IN_MASK_CREATE | IN_MASK_ADD); int e4 = errno;
    report("t_mask_create_eexist", w1 > 0 && w2 < 0 && e2 == EEXIST && w3 > 0 && w4 < 0 && e4 == EINVAL);
    close(ifd);
}

static void t_onlydir(void)
{
    touch("x/file");
    int ifd = inotify_init1(IN_NONBLOCK);
    int w1 = watch(ifd, "x/file", IN_MODIFY | IN_ONLYDIR); int e1 = errno;
    int w2 = watch(ifd, "x", IN_CREATE | IN_ONLYDIR);
    report("t_onlydir", w1 < 0 && e1 == ENOTDIR && w2 > 0);
    close(ifd); rm("x/file");
}

static void t_excl_unlink(void)
{
    int ifd = inotify_init1(IN_NONBLOCK);
    int wd = watch(ifd, "x", IN_CREATE | IN_EXCL_UNLINK);
    touch("x/eu");
    int ov, ig, n = count_events(ifd, &ov, &ig);
    report("t_excl_unlink", wd > 0 && n == 1);
    close(ifd); rm("x/eu");
}

static void t_dont_follow(void)
{
    char lp[128], tp[128]; path(lp, "ln"); path(tp, "x");
    unlink(lp); symlink(tp, lp);
    int ifd = inotify_init1(IN_NONBLOCK);
    int w1 = watch(ifd, "ln", IN_CREATE | IN_DONT_FOLLOW);
    touch("x/df1");
    int ov, ig, n1 = count_events(ifd, &ov, &ig);
    int ifd2 = inotify_init1(IN_NONBLOCK);
    int w2 = watch(ifd2, "ln", IN_CREATE);
    touch("x/df2");
    int n2 = count_events(ifd2, &ov, &ig);
    report("t_dont_follow", w1 > 0 && n1 == 0 && w2 > 0 && n2 == 1);
    close(ifd); close(ifd2); unlink(lp); rm("x/df1"); rm("x/df2");
}

static void t_overflow(void)
{
    int rc = lim_set(LIM_Q, 16);
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "x", IN_CREATE);
    char nm[32];
    for (int i = 0; i < 20; i++) { snprintf(nm, sizeof nm, "x/ov%d", i); touch(nm); }
    int ov, ig, n = count_events(ifd, &ov, &ig);
    report("t_overflow", rc == 0 && n == 17 && ov == 1);
    close(ifd);
    for (int i = 0; i < 20; i++) { snprintf(nm, sizeof nm, "x/ov%d", i); rm(nm); }
    lim_set(LIM_Q, 16384);
}

static void t_proc_limits(void)
{
    int ok = lim_get(LIM_Q) == 16384 && lim_get(LIM_I) == 128 && lim_get(LIM_W) == 65536;
    ok = ok && lim_set(LIM_I, 7) == 0 && lim_get(LIM_I) == 7;
    ok = ok && lim_set(LIM_I, 0) == -EINVAL && lim_get(LIM_I) == 7;
    lim_set(LIM_I, 128);
    pid_t c = fork();
    if (c == 0) {
        if (setgid(65534) || setuid(65534)) _exit(2);
        _exit(lim_set(LIM_W, 5) == -EACCES && lim_get(LIM_W) == 65536 ? 0 : 1);
    }
    int st = 0; waitpid(c, &st, 0);
    report("t_proc_limits", ok && WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

static void t_limit_instances(void)
{
    int fds[8], n = 0, ok;
    lim_set(LIM_I, 4);
    for (int i = 0; i < 4; i++) { fds[n] = inotify_init1(IN_NONBLOCK); if (fds[n] >= 0) n++; }
    int extra = inotify_init1(IN_NONBLOCK); int e = errno;
    ok = n == 4 && extra < 0 && e == EMFILE;
    close(fds[0]);
    int again = inotify_init1(IN_NONBLOCK);
    ok = ok && again >= 0;
    if (again >= 0) close(again);
    for (int i = 1; i < n; i++) close(fds[i]);
    lim_set(LIM_I, 128);
    report("t_limit_instances", ok);
}

static void t_limit_watches(void)
{
    lim_set(LIM_W, 3);
    int ifd = inotify_init1(IN_NONBLOCK);
    int a = watch(ifd, "x", IN_CREATE), b = watch(ifd, "y", IN_CREATE), c = watch(ifd, ".", IN_CREATE);
    int d = watch(ifd, "x/..", IN_CREATE) ;
    mkd("z");
    int f = watch(ifd, "z", IN_CREATE); int ef = errno;
    inotify_rm_watch(ifd, a);
    int g = watch(ifd, "z", IN_CREATE);
    (void)d;
    report("t_limit_watches", a > 0 && b > 0 && c > 0 && f < 0 && ef == ENOSPC && g > 0);
    close(ifd);
    lim_set(LIM_W, 65536);
    char p[128]; path(p, "z"); rmdir(p);
}

static void t_limit_perkuid(void)
{
    lim_set(LIM_I, 2);
    int f1 = inotify_init1(IN_NONBLOCK), f2 = inotify_init1(IN_NONBLOCK);
    int f3 = inotify_init1(IN_NONBLOCK); int e3 = errno;
    pid_t c = fork();
    if (c == 0) {
        if (setgid(65534) || setuid(65534)) _exit(2);
        int a = inotify_init1(IN_NONBLOCK), b = inotify_init1(IN_NONBLOCK);
        int x = inotify_init1(IN_NONBLOCK);
        _exit(a >= 0 && b >= 0 && x < 0 && errno == EMFILE ? 0 : 1);
    }
    int st = 0; waitpid(c, &st, 0);
    report("t_limit_perkuid", f1 >= 0 && f2 >= 0 && f3 < 0 && e3 == EMFILE && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    if (f1 >= 0) close(f1);
    if (f2 >= 0) close(f2);
    lim_set(LIM_I, 128);
}

static int wmap(pid_t pid, unsigned lower)
{
    char p[64], m[64];
    snprintf(m, sizeof m, "0 %u 1", lower);
    const char *f[2] = { "uid_map", "gid_map" };
    for (int i = 0; i < 2; i++) {
        snprintf(p, sizeof p, "/proc/%d/%s", pid, f[i]);
        int fd = open(p, O_WRONLY);
        if (fd < 0) return -1;
        ssize_t r = write(fd, m, strlen(m)); close(fd);
        if (r != (ssize_t)strlen(m)) return -1;
    }
    return 0;
}

/* Child unshare(USER|NS); parent maps 0 -> lower; returns the child's pipes. */
static pid_t spawn_userns(int newns, unsigned lower, int *to_child, int *from_child, int (*body)(void))
{
    int a[2], b[2];
    pipe(a); pipe(b);
    pid_t c = fork();
    if (c == 0) {
        char x;
        if (unshare(CLONE_NEWUSER | (newns ? CLONE_NEWNS : 0))) _exit(90);
        write(b[1], "u", 1);
        if (read(a[0], &x, 1) != 1) _exit(91);
        _exit(body());
    }
    char x; read(b[0], &x, 1);
    if (wmap(c, lower)) { kill(c, 9); waitpid(c, NULL, 0); return -1; }
    write(a[1], "m", 1);
    *to_child = a[1]; *from_child = b[0];
    close(a[0]); close(b[1]);
    return c;
}

static int g_pipe_up = -1, g_pipe_dn = -1;
static int limit_userns_body(void)
{
    int a = inotify_init1(IN_NONBLOCK), b = inotify_init1(IN_NONBLOCK);
    int x = inotify_init1(IN_NONBLOCK); int e = errno;
    int ok = a >= 0 && b >= 0 && x < 0 && e == EMFILE;
    return ok ? 0 : 1;
}

static void t_limit_userns(void)
{
    /* Child ns instances are charged to (ns, kuid) AND to the creator (ns 0,
     * kuid 0) up the chain. */
    lim_set(LIM_I, 2);
    int to, from;
    int a[2], b[2];
    pipe(a); pipe(b);
    pid_t c = fork();
    if (c == 0) {
        char x;
        if (unshare(CLONE_NEWUSER)) _exit(90);
        write(b[1], "u", 1);
        if (read(a[0], &x, 1) != 1) _exit(91);
        int i1 = inotify_init1(IN_NONBLOCK), i2 = inotify_init1(IN_NONBLOCK);
        int i3 = inotify_init1(IN_NONBLOCK); int e = errno;
        write(b[1], "d", 1);
        if (read(a[0], &x, 1) != 1) _exit(92);
        _exit(i1 >= 0 && i2 >= 0 && i3 < 0 && e == EMFILE ? 0 : 1);
    }
    char x; read(b[0], &x, 1);
    int ok = wmap(c, 1000) == 0;
    write(a[1], "m", 1);
    read(b[0], &x, 1);
    /* the creator (kuid 0, ns 0) sees both child instances in its own count */
    int mine = inotify_init1(IN_NONBLOCK); int em = errno;
    ok = ok && mine < 0 && em == EMFILE;
    if (mine >= 0) close(mine);
    write(a[1], "g", 1);
    int st = 0; waitpid(c, &st, 0);
    (void)to; (void)from;
    report("t_limit_userns", ok && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    lim_set(LIM_I, 128);
}

static char g_ab[160];
static int userns_mntns_body(void)
{
    char a[128]; snprintf(a, sizeof a, "%s/a", base);
    mkdir("/mnt", 0755); mkdir("/mnt/b", 0755);
    if (mount(a, "/mnt/b", NULL, MS_BIND, NULL) < 0) return 3;
    int ifd = inotify_init1(0);
    int wd = inotify_add_watch(ifd, "/mnt/b", IN_CREATE);
    if (wd < 0) return 4;
    write(g_pipe_up, "w", 1);
    char buf[512]; ssize_t n = read(ifd, buf, sizeof buf), off = 0;
    const struct inotify_event *e = n > 0 ? next_ev(buf, n, &off) : NULL;
    if (!e || e->wd != wd || e->mask != IN_CREATE) return 5;
    if (strcmp(e->name, "f")) return 6;
    if (strchr(e->name, '/') || strstr(e->name, "leandros") || strstr(e->name, "inot-")) return 7;
    return 0;
}

static void t_userns_mntns(void)
{
    mkd("a");
    int a[2], b[2], w[2];
    pipe(a); pipe(b); pipe(w);
    pid_t c = fork();
    if (c == 0) {
        char x;
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS)) _exit(90);
        write(b[1], "u", 1);
        if (read(a[0], &x, 1) != 1) _exit(91);
        g_pipe_up = w[1];
        _exit(userns_mntns_body());
    }
    char x; read(b[0], &x, 1);
    int ok = wmap(c, 0) == 0;
    write(a[1], "m", 1);
    ok = ok && read(w[0], &x, 1) == 1;
    touch("a/f");              /* made from the host, on the same inode */
    int st = 0; waitpid(c, &st, 0);
    if (!(ok && WIFEXITED(st) && WEXITSTATUS(st) == 0)) printf("  userns_mntns child rc=%d ok=%d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1, ok);
    report("t_userns_mntns", ok && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    rm("a/f");
    char p[128]; path(p, "a"); rmdir(p);
}

static void t_killswitch(void)
{
    int ifd = inotify_init1(IN_NONBLOCK);
    int wd = watch(ifd, "x", IN_CREATE);
    touch("x/ks");
    char buf[512]; ssize_t n = read(ifd, buf, sizeof buf);
    int e = errno;
    int r = inotify_rm_watch(ifd, wd);
    report("t_killswitch", ifd >= 0 && wd >= 1 && n < 0 && e == EAGAIN && r == 0);
    close(ifd);
    rm("x/ks");
}

static int knob_on(void)
{
    char b[4096]; int fd = open("/proc/cmdline", O_RDONLY);
    if (fd < 0) return 0;
    ssize_t n = read(fd, b, sizeof b - 1); close(fd);
    if (n < 0) return 0;
    b[n] = 0;
    return strstr(b, "vfs.inotify=1") != NULL;
}

int main(int argc, char **argv)
{
    int force = argc > 1 && !strcmp(argv[1], "force");
    int on = knob_on();
    setup();
    if (on || force) {
        t_create(); t_delete(); t_move_pair(); t_moved_to(); t_create_waitfile();
        t_layout(); t_einval_small(); t_fionread(); t_rmwatch_ignored();
        t_block_read(); t_silent_unrelated(); t_dir_events(); t_dir_follow();
        t_eacces();
        t_oneshot(); t_mask_add(); t_mask_create_eexist(); t_onlydir(); t_excl_unlink();
        t_dont_follow(); t_overflow(); t_proc_limits(); t_limit_instances(); t_limit_watches();
        t_limit_perkuid(); t_limit_userns(); t_userns_mntns();
    } else {
        printf("real cases: SKIP (kernel booted without vfs.inotify=1)\n");
    }
    if (!on) t_killswitch(); else printf("t_killswitch: SKIP (knob on)\n");
    printf("inotifytest: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
