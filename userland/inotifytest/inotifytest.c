/*
 * inotifytest -- inotify semantics (design-inotify.md, lane L1: tmpfs dirent
 * events). Static clang-musl build, runs as root; one "case: PASS|FAIL|SKIP" line
 * per case. The real cases run by default and SKIP when the kernel is booted
 * with vfs.inotify=0; `inotifytest force` runs them regardless (fail-first evidence
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
#include <sys/fanotify.h>
#include <sys/mount.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef IN_EXCL_UNLINK
#define IN_EXCL_UNLINK 0x04000000
#endif
#ifndef IN_MASK_CREATE
#define IN_MASK_CREATE 0x10000000
#endif
#ifndef IN_MASK_ADD
#define IN_MASK_ADD 0x20000000
#endif
static int npass, nfail;
static char base[64];

static const char *tg = "";   /* suffix naming the filesystem under test */

static void report(const char *name, int ok)
{
    printf("%s%s: %s\n", name, tg, ok ? "PASS" : "FAIL");
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

/* Generated files (/etc/resolv.conf, /proc/interrupts) can be watched:
 * aardvark-dns watches /etc/resolv.conf and aborts if the add fails. */
static void t_generated(void)
{
    const char *gen[] = { "/etc/resolv.conf", "/proc/interrupts" };
    for (int i = 0; i < 2; i++) {
        int ifd = inotify_init1(IN_NONBLOCK);
        int wd = inotify_add_watch(ifd, gen[i], IN_MODIFY | IN_CLOSE_WRITE);
        char name[64];
        snprintf(name, sizeof name, "t_generated_%s", gen[i] + 1);
        report(name, wd >= 0);
        if (wd < 0) printf("  %s: errno=%d\n", gen[i], errno);
        close(ifd);
    }
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

static int wself(const char *f, const char *m)
{
    char p[64]; snprintf(p, sizeof p, "/proc/self/%s", f);
    int fd = open(p, O_WRONLY);
    if (fd < 0) { printf("  wself open %s errno=%d uid=%d\n", p, errno, (int)geteuid()); return -1; }
    ssize_t r = write(fd, m, strlen(m)); int e = errno; close(fd);
    if (r != (ssize_t)strlen(m)) printf("  wself write %s '%s' r=%d errno=%d\n", p, m, (int)r, e);
    return r == (ssize_t)strlen(m) ? 0 : -1;
}
/* Identity self-map, as an unprivileged unshare(CLONE_NEWUSER) caller does. */
static int selfmap(unsigned id)
{
    char m[64]; snprintf(m, sizeof m, "%u %u 1", id, id);
    if (wself("setgroups", "deny")) return -1;
    if (wself("uid_map", m)) return -1;
    return wself("gid_map", m);
}
static int wmap(pid_t pid, unsigned lower) { (void)pid; (void)lower; return 0; }

static int g_pipe_up = -1;
static void t_limit_userns(void)
{
    /* Child ns instances are charged to (ns, kuid) AND to the creator (ns 0,
     * kuid 0) up the chain. */
    lim_set(LIM_I, 2);
    int a[2], b[2];
    pipe(a); pipe(b);
    /* kuid 0 cannot be mapped yet (idroot0), so run the chain as kuid 1000. */
    setresgid(1000, 1000, 0); setresuid(1000, 1000, 0);
    pid_t c = fork();
    if (c == 0) {
        char x;
        { unsigned me = getuid(); if (unshare(CLONE_NEWUSER)) _exit(90); if (selfmap(me)) { write(b[1], "u", 1); _exit(93); } }
        write(b[1], "u", 1);
        if (read(a[0], &x, 1) != 1) _exit(91);
        int i1 = inotify_init1(IN_NONBLOCK), i2 = inotify_init1(IN_NONBLOCK);
        int i3 = inotify_init1(IN_NONBLOCK); int e = errno;
        write(b[1], "d", 1);
        if (read(a[0], &x, 1) != 1) _exit(92);
        _exit(i1 >= 0 && i2 >= 0 && i3 < 0 && e == EMFILE ? 0 : 1);
    }
    close(b[1]);
    char x; read(b[0], &x, 1);
    int ok = wmap(c, 0) == 0;
    write(a[1], "m", 1);
    read(b[0], &x, 1);
    /* the creator (kuid 0, ns 0) sees both child instances in its own count */
    int mine = inotify_init1(IN_NONBLOCK); int em = errno;
    ok = ok && mine < 0 && em == EMFILE;
    if (mine >= 0) close(mine);
    write(a[1], "g", 1);
    int st = 0; waitpid(c, &st, 0);
    setresuid(0, 0, 0); setresgid(0, 0, 0);
    if (!(ok && WIFEXITED(st) && WEXITSTATUS(st) == 0)) printf("  limit_userns ok=%d mine=%d em=%d st=%#x\n", ok, mine, em, st);
    report("t_limit_userns", ok && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    lim_set(LIM_I, 128);
}

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
    mkdir("/mnt", 0755); mkdir("/mnt/b", 0755);
    int a[2], b[2], w[2];
    pipe(a); pipe(b); pipe(w);
    pid_t c = fork();
    if (c == 0) {
        char x;
        { setresgid(1000, 1000, 1000); setresuid(1000, 1000, 1000); unsigned me = getuid(); if (unshare(CLONE_NEWUSER | CLONE_NEWNS)) _exit(90); if (selfmap(me)) { write(b[1], "u", 1); _exit(93); } }
        write(b[1], "u", 1);
        if (read(a[0], &x, 1) != 1) _exit(91);
        g_pipe_up = w[1];
        _exit(userns_mntns_body());
    }
    close(b[1]); close(w[1]);
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

/* ---- L2 (mounted-volume dirent events, self events, UNMOUNT) and L3 (inode
 * events). Everything below runs once on tmpfs (/run) and once on the f2fs
 * root (/root); `base` selects the filesystem. ---- */
#ifndef IN_UNMOUNT
#define IN_UNMOUNT 0x2000
#endif

struct got { uint32_t mask; int wd; char name[32]; };

/* Drain the (non-blocking) inotify fd into g[]; returns the event count. */
static int drain(int ifd, struct got *g, int max)
{
    char buf[4096]; int k = 0;
    ssize_t n = read(ifd, buf, sizeof buf), off = 0;
    const struct inotify_event *e;
    while (n > 0 && k < max && (e = next_ev(buf, n, &off))) {
        g[k].mask = e->mask; g[k].wd = e->wd;
        snprintf(g[k].name, sizeof g[k].name, "%s", e->len ? e->name : "");
        k++;
    }
    return k;
}

static int wr(const char *rel, const char *data)
{
    char p[128]; path(p, rel);
    int fd = open(p, O_CREAT | O_WRONLY | O_APPEND, 0644);
    if (fd < 0) return -1;
    ssize_t r = write(fd, data, strlen(data)); close(fd);
    return (int)r;
}

static void t_rmdir_self(void)
{
    mkd("rs");
    int ifd = inotify_init1(IN_NONBLOCK);
    int wd = watch(ifd, "rs", IN_DELETE_SELF);
    char p[128]; path(p, "rs"); rmdir(p);
    struct got g[4]; int n = drain(ifd, g, 4);
    report("t_rmdir_self", wd > 0 && n == 2 && g[0].mask == (IN_DELETE_SELF | IN_ISDIR) && g[0].wd == wd && g[1].mask == IN_IGNORED);
    close(ifd);
}

static void t_move_self(void)
{
    touch("ms1"); mkd("msd");
    int ifd = inotify_init1(IN_NONBLOCK);
    int w1 = watch(ifd, "ms1", IN_MOVE_SELF), w2 = watch(ifd, "msd", IN_MOVE_SELF);
    ren("ms1", "ms2"); ren("msd", "mse");
    struct got g[4]; int n = drain(ifd, g, 4);
    report("t_move_self", w1 > 0 && w2 > 0 && n == 2 && g[0].mask == IN_MOVE_SELF && g[0].wd == w1
           && g[1].mask == (IN_MOVE_SELF | IN_ISDIR) && g[1].wd == w2);
    close(ifd);
}

static void t_dir_rename_then_create(void)
{
    mkd("drc"); mkd("drc/sub");
    int ifd = inotify_init1(IN_NONBLOCK);
    int wd = watch(ifd, "drc/sub", IN_CREATE);
    ren("drc", "drc2");
    touch("drc2/sub/n");
    struct got g[4]; int n = drain(ifd, g, 4);
    report("t_dir_rename_then_create", wd > 0 && n == 1 && g[0].mask == IN_CREATE && !strcmp(g[0].name, "n"));
    close(ifd);
}

static void t_symlink_link_mknod(void)
{
    touch("x/sl_t");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "x", IN_CREATE | IN_DELETE);
    char a[128], b[128]; path(a, "x/sl_t"); path(b, "x/sl_l");
    symlink(a, b); unlink(b);
    path(b, "x/hl"); link(a, b); unlink(b);
    path(b, "x/ff"); mkfifo(b, 0644); unlink(b);
    struct got g[8]; int n = drain(ifd, g, 8);
    int ok = n == 6;
    const char *nm[] = { "sl_l", "sl_l", "hl", "hl", "ff", "ff" };
    for (int i = 0; ok && i < 6; i++)
        ok = g[i].mask == ((i & 1) ? IN_DELETE : IN_CREATE) && !strcmp(g[i].name, nm[i]);
    report("t_symlink_link_mknod", ok);
    close(ifd);
}

static void t_modify_file(void)
{
    touch("mf");
    int ifd = inotify_init1(IN_NONBLOCK);
    int wd = watch(ifd, "mf", IN_MODIFY);
    wr("mf", "hello");
    struct got g[4]; int n = drain(ifd, g, 4);
    report("t_modify_file", wd > 0 && n == 1 && g[0].mask == IN_MODIFY && g[0].wd == wd && g[0].name[0] == 0);
    close(ifd);
}

static void t_modify_in_dir(void)
{
    touch("x/mid");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "x", IN_MODIFY);
    wr("x/mid", "hello");
    char p[128]; path(p, "x/mid"); truncate(p, 0);
    struct got g[4]; int n = drain(ifd, g, 4);
    report("t_modify_in_dir", n >= 1 && g[0].mask == IN_MODIFY && !strcmp(g[0].name, "mid"));
    close(ifd);
}

static void t_modify_trunc(void)
{
    wr("mt", "data");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "mt", IN_MODIFY);
    char p[128]; path(p, "mt");
    int fd = open(p, O_WRONLY | O_TRUNC); close(fd);
    struct got g[4]; int a = drain(ifd, g, 4);
    fd = open(p, O_WRONLY); int r = ftruncate(fd, 100); close(fd);
    int b = drain(ifd, g, 4);
    report("t_modify_trunc", a >= 1 && r == 0 && b >= 1 && g[0].mask == IN_MODIFY);
    close(ifd);
}

static void t_close_write(void)
{
    touch("cw");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "cw", IN_CLOSE_WRITE | IN_CLOSE_NOWRITE | IN_OPEN);
    char p[128]; path(p, "cw");
    int fd = open(p, O_WRONLY); close(fd);
    struct got g[4]; int n = drain(ifd, g, 4);
    int ok = n == 2 && g[0].mask == IN_OPEN && g[1].mask == IN_CLOSE_WRITE;
    fd = open(p, O_RDONLY); close(fd);
    n = drain(ifd, g, 4);
    report("t_close_write", ok && n == 2 && g[0].mask == IN_OPEN && g[1].mask == IN_CLOSE_NOWRITE);
    close(ifd);
}

static void t_close_dup(void)
{
    touch("cd");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "cd", IN_CLOSE_WRITE);
    char p[128]; path(p, "cd");
    int fd = open(p, O_WRONLY), fd2 = dup(fd);
    pid_t c = fork();
    if (c == 0) { close(fd2); _exit(0); }
    waitpid(c, NULL, 0);
    close(fd);
    struct got g[4]; int n0 = drain(ifd, g, 4);
    close(fd2);
    int n1 = drain(ifd, g, 4);
    report("t_close_dup", n0 == 0 && n1 == 1 && g[0].mask == IN_CLOSE_WRITE);
    close(ifd);
}

static void t_attrib_chmod(void)
{
    touch("at");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "at", IN_ATTRIB);
    watch(ifd, "x", IN_ATTRIB);
    char p[128]; path(p, "at"); chmod(p, 0600);
    struct got g[4]; int n = drain(ifd, g, 4);
    int ok = n == 1 && g[0].mask == IN_ATTRIB && g[0].name[0] == 0;
    path(p, "x/atc"); int fd = open(p, O_CREAT | O_WRONLY, 0644); fchmod(fd, 0600); close(fd);
    n = drain(ifd, g, 4);
    report("t_attrib_chmod", ok && n == 1 && g[0].mask == IN_ATTRIB && !strcmp(g[0].name, "atc"));
    close(ifd);
}

static void t_access(void)
{
    wr("ac", "0123456789");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "ac", IN_ACCESS);
    char p[128], b[16]; path(p, "ac");
    int fd = open(p, O_RDONLY); ssize_t r = read(fd, b, sizeof b); close(fd);
    struct got g[4]; int n = drain(ifd, g, 4);
    report("t_access", r > 0 && n == 1 && g[0].mask == IN_ACCESS);
    close(ifd);
}

static void t_coalesce(void)
{
    touch("co");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "co", IN_MODIFY);
    char p[128]; path(p, "co");
    int fd = open(p, O_WRONLY);
    write(fd, "a", 1); write(fd, "b", 1); write(fd, "c", 1);
    struct got g[8]; int n0 = drain(ifd, g, 8);
    write(fd, "d", 1);
    int n1 = drain(ifd, g, 8);
    write(fd, "e", 1); write(fd, "f", 1);
    int n2 = drain(ifd, g, 8);
    close(fd);
    report("t_coalesce", n0 == 1 && n1 == 1 && n2 == 1);
    close(ifd);
}

static void t_unlink_open(void)
{
    touch("uo");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "uo", IN_DELETE_SELF | IN_ATTRIB);
    char p[128]; path(p, "uo");
    int fd = open(p, O_RDONLY);
    unlink(p);
    struct got g[4]; int n0 = drain(ifd, g, 4);
    int ok0 = n0 == 1 && g[0].mask == IN_ATTRIB;
    close(fd);
    int n1 = drain(ifd, g, 4);
    report("t_unlink_open", ok0 && n1 == 2 && g[0].mask == IN_DELETE_SELF && g[1].mask == IN_IGNORED);
    close(ifd);
}

static void t_unlink_self(void)
{
    touch("us");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "us", IN_DELETE_SELF);
    rm("us");
    struct got g[4]; int n = drain(ifd, g, 4);
    report("t_unlink_self", n == 2 && g[0].mask == IN_DELETE_SELF && g[1].mask == IN_IGNORED);
    close(ifd);
}

static void t_rename_over(void)
{
    touch("ro_a"); touch("ro_b");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "ro_b", IN_DELETE_SELF);
    ren("ro_a", "ro_b");
    struct got g[4]; int n = drain(ifd, g, 4);
    report("t_rename_over", n == 2 && g[0].mask == IN_DELETE_SELF && g[1].mask == IN_IGNORED);
    close(ifd);
}

static void t_umount(void)
{
    mkd("um");
    char p[128]; path(p, "um");
    if (mount("tmpfs", p, "tmpfs", 0, "") != 0) { printf("t_umount%s: SKIP (mount: %d)\n", tg, errno); return; }
    int ifd = inotify_init1(IN_NONBLOCK);
    int wd = inotify_add_watch(ifd, p, IN_CREATE);
    struct got g[4];
    int ok = wd > 0;
    char q[160]; snprintf(q, sizeof q, "%s/f", p);
    close(open(q, O_CREAT | O_WRONLY, 0644));
    int n0 = drain(ifd, g, 4);
    ok = ok && n0 == 1 && g[0].mask == IN_CREATE;
    unlink(q);
    int r = umount(p);
    int n = drain(ifd, g, 4);
    report("t_umount", ok && r == 0 && n == 2 && g[0].mask == IN_UNMOUNT && g[1].mask == IN_IGNORED);
    close(ifd);
}

static void t_cross_bind(void)
{
    mkd("cb_src"); mkd("cb_dst");
    char s[128], d[128]; path(s, "cb_src"); path(d, "cb_dst");
    if (mount(s, d, NULL, MS_BIND, NULL) != 0) { printf("t_cross_bind%s: SKIP (bind: %d)\n", tg, errno); return; }
    int ifd = inotify_init1(IN_NONBLOCK);
    int wd = inotify_add_watch(ifd, d, IN_CREATE);
    touch("cb_src/n");
    struct got g[4]; int n = drain(ifd, g, 4);
    report("t_cross_bind", wd > 0 && n == 1 && g[0].mask == IN_CREATE && !strcmp(g[0].name, "n"));
    close(ifd);
    umount(d);
}

/* ---- (dev, ino) identity: events reach a watch through any hard link ---- */
static void t_hl_self(void)
{
    touch("hl_a");
    char a[128], b[128]; path(a, "hl_a"); path(b, "hl_b"); link(a, b);
    int ifd = inotify_init1(IN_NONBLOCK);
    uint32_t m = IN_ATTRIB | IN_MODIFY | IN_OPEN | IN_CLOSE_WRITE;
    int w1 = watch(ifd, "hl_b", m), w2 = watch(ifd, "hl_a", m);
    chmod(a, 0600);
    wr("hl_a", "x");
    struct got g[8]; int n = drain(ifd, g, 8);
    report("t_hl_self", w1 > 0 && w1 == w2 && n == 4 && g[0].mask == IN_ATTRIB && g[1].mask == IN_OPEN
           && g[2].mask == IN_MODIFY && g[3].mask == IN_CLOSE_WRITE);
    close(ifd);
}

static void t_hl_unlink_one(void)
{
    touch("hu_a");
    char a[128], b[128]; path(a, "hu_a"); path(b, "hu_b"); link(a, b);
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "hu_a", IN_ATTRIB | IN_DELETE_SELF);
    unlink(a);
    struct got g[8]; int n0 = drain(ifd, g, 8);
    int ok0 = n0 == 1 && g[0].mask == IN_ATTRIB;
    chmod(b, 0600);
    int n1 = drain(ifd, g, 8);
    int ok1 = n1 == 1 && g[0].mask == IN_ATTRIB;
    unlink(b);
    int n2 = drain(ifd, g, 8);
    report("t_hl_unlink_one", ok0 && ok1 && n2 == 3 && g[0].mask == IN_ATTRIB && g[1].mask == IN_DELETE_SELF
           && g[2].mask == IN_IGNORED);
    close(ifd);
}

static void t_hl_link_after_watch(void)
{
    touch("hw_a");
    int ifd = inotify_init1(IN_NONBLOCK);
    watch(ifd, "hw_a", IN_ATTRIB | IN_MODIFY);
    char a[128], b[128]; path(a, "hw_a"); path(b, "hw_b"); link(a, b);
    struct got g[8]; int n0 = drain(ifd, g, 8);
    int ok0 = n0 == 1 && g[0].mask == IN_ATTRIB;
    wr("hw_b", "y");
    int n1 = drain(ifd, g, 8);
    report("t_hl_link_after_watch", ok0 && n1 == 1 && g[0].mask == IN_MODIFY);
    close(ifd);
}

/* ---- fanotify ---- */
struct fev { uint64_t mask; int fd; int pid; unsigned char vers; unsigned len; char link[160]; };

static int fan_drain(int ffd, struct fev *g, int max)
{
    char buf[4096]; int k = 0;
    ssize_t n = read(ffd, buf, sizeof buf), off = 0;
    while (n > 0 && off + (ssize_t)sizeof(struct fanotify_event_metadata) <= n && k < max) {
        struct fanotify_event_metadata *m = (void *)(buf + off);
        g[k].mask = m->mask; g[k].fd = m->fd; g[k].pid = m->pid; g[k].vers = m->vers; g[k].len = m->event_len;
        g[k].link[0] = 0;
        if (m->fd >= 0) {
            char l[64]; snprintf(l, sizeof l, "/proc/self/fd/%d", m->fd);
            ssize_t r = readlink(l, g[k].link, sizeof g[k].link - 1);
            g[k].link[r > 0 ? r : 0] = 0;
        }
        off += m->event_len; k++;
    }
    return k;
}
static void fan_closeall(struct fev *g, int n) { for (int i = 0; i < n; i++) if (g[i].fd >= 0) close(g[i].fd); }
static int ends_with(const char *s, const char *suf)
{
    size_t a = strlen(s), b = strlen(suf);
    return a >= b && !strcmp(s + a - b, suf);
}
static int fan_new(void) { return fanotify_init(FAN_CLASS_NOTIF | FAN_NONBLOCK | FAN_CLOEXEC, O_RDONLY); }
#define FANALL (FAN_OPEN | FAN_ACCESS | FAN_MODIFY | FAN_CLOSE_WRITE | FAN_CLOSE_NOWRITE)

static void t_fan_basic(void)
{
    touch("fbasic");
    char p[128]; path(p, "fbasic");
    int ffd = fan_new();
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FANALL, AT_FDCWD, p);
    int fd = open(p, O_RDWR); char c;
    if (fd >= 0) { if (write(fd, "x", 1)) {} lseek(fd, 0, SEEK_SET); if (read(fd, &c, 1)) {} close(fd); }
    struct fev g[8]; int n = ffd < 0 ? 0 : fan_drain(ffd, g, 8);
    int ok = mr == 0 && n == 4 && g[0].mask == FAN_OPEN && g[1].mask == FAN_MODIFY && g[2].mask == FAN_ACCESS
          && g[3].mask == FAN_CLOSE_WRITE;
    for (int i = 0; ok && i < n; i++)
        ok = g[i].pid == getpid() && g[i].fd >= 0 && g[i].vers == FANOTIFY_METADATA_VERSION
          && g[i].len == sizeof(struct fanotify_event_metadata) && ends_with(g[i].link, "/fbasic");
    fan_closeall(g, n);
    int n2 = ffd < 0 ? -1 : fan_drain(ffd, g, 8);   /* closing event fds must not recurse */
    fd = open(p, O_RDONLY); if (fd >= 0) close(fd);
    int n3 = ffd < 0 ? 0 : fan_drain(ffd, g, 8);
    ok = ok && n2 <= 0 && n3 == 2 && g[0].mask == FAN_OPEN && g[1].mask == FAN_CLOSE_NOWRITE;
    fan_closeall(g, n3);
    if (!ok) printf("  fan_basic: mr=%d n=%d n2=%d n3=%d first=%llx,%llx,%llx,%llx pid=%d/%d link=%s\n", mr, n, n2, n3,
                    (unsigned long long)g[0].mask, (unsigned long long)g[1].mask, (unsigned long long)g[2].mask,
                    (unsigned long long)g[3].mask, g[0].pid, getpid(), g[0].link);
    report("t_fan_basic", ok);
    if (ffd >= 0) close(ffd);
}

static void t_fan_mount(void)
{
    mkd("x"); touch("x/mf0");
    char d[128]; path(d, "x");
    int ffd = fan_new();
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD | FAN_MARK_MOUNT, FAN_OPEN | FAN_CLOSE_WRITE, AT_FDCWD, d);
    wr("x/mf0", "z");
    struct fev g[32]; int n = ffd < 0 ? 0 : fan_drain(ffd, g, 32);
    int op = 0, cw = 0;
    for (int i = 0; i < n; i++) {
        if (!ends_with(g[i].link, "/x/mf0")) continue;
        if (g[i].mask & FAN_OPEN) op++;
        if (g[i].mask & FAN_CLOSE_WRITE) cw++;
    }
    fan_closeall(g, n);
    report("t_fan_mount", mr == 0 && op >= 1 && cw >= 1);
    if (ffd >= 0) close(ffd);
}

static void t_fan_child(void)
{
    mkd("fc");
    char d[128]; path(d, "fc");
    int ffd = fan_new();
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FAN_OPEN | FAN_CLOSE_WRITE | FAN_EVENT_ON_CHILD, AT_FDCWD, d);
    touch("fc/cf");
    struct fev g[8]; int n = ffd < 0 ? 0 : fan_drain(ffd, g, 8);
    int ok = mr == 0 && n == 2 && g[0].mask == FAN_OPEN && g[1].mask == FAN_CLOSE_WRITE
          && ends_with(g[0].link, "/fc/cf");
    fan_closeall(g, n);
    report("t_fan_child", ok);
    if (ffd >= 0) close(ffd);
}

static void t_fan_remove_flush(void)
{
    touch("fr");
    char p[128]; path(p, "fr");
    int ffd = fan_new();
    int a = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FAN_OPEN, AT_FDCWD, p);
    int r = fanotify_mark(ffd, FAN_MARK_REMOVE, FAN_OPEN, AT_FDCWD, p);
    int fd = open(p, O_RDONLY); if (fd >= 0) close(fd);
    struct fev g[4]; int n0 = fan_drain(ffd, g, 4);
    int r2 = fanotify_mark(ffd, FAN_MARK_REMOVE, FAN_OPEN, AT_FDCWD, p);
    int e2 = errno;
    int a2 = fanotify_mark(ffd, FAN_MARK_ADD, FAN_OPEN, AT_FDCWD, p);
    int f = fanotify_mark(ffd, FAN_MARK_FLUSH, 0, AT_FDCWD, NULL);
    fd = open(p, O_RDONLY); if (fd >= 0) close(fd);
    int n1 = fan_drain(ffd, g, 4);
    report("t_fan_remove_flush", a == 0 && r == 0 && n0 <= 0 && r2 == -1 && e2 == ENOENT && a2 == 0 && f == 0 && n1 <= 0);
    if (ffd >= 0) close(ffd);
}

static void t_fan_einval(void)
{
    touch("fe");
    char p[128]; path(p, "fe");
    int ffd = fan_new(), ok = ffd >= 0;
    ok = ok && fanotify_mark(ffd, FAN_MARK_ADD, FAN_OPEN_PERM, AT_FDCWD, p) == -1 && errno == EINVAL;
    ok = ok && fanotify_mark(ffd, FAN_MARK_ADD, 0, AT_FDCWD, p) == -1 && errno == EINVAL;
    ok = ok && fanotify_mark(ffd, FAN_MARK_ADD | FAN_MARK_REMOVE, FAN_OPEN, AT_FDCWD, p) == -1 && errno == EINVAL;
    ok = ok && fanotify_mark(ffd, FAN_MARK_ADD, FAN_OPEN, AT_FDCWD, "/nonexistent/fe") == -1 && errno == ENOENT;
    ok = ok && fanotify_mark(9999, FAN_MARK_ADD, FAN_OPEN, AT_FDCWD, p) == -1 && errno == EBADF;
    int ifd = inotify_init1(0);
    ok = ok && fanotify_mark(ifd, FAN_MARK_ADD, FAN_OPEN, AT_FDCWD, p) == -1 && errno == EINVAL;
    ok = ok && fanotify_init(0x40000000, O_RDONLY) == -1 && errno == EINVAL;
    report("t_fan_einval", ok);
    close(ifd); if (ffd >= 0) close(ffd);
}

/* ---- fanotify permission events + ignored mask ---- */
#ifndef FAN_MARK_IGNORED_MASK
#define FAN_MARK_IGNORED_MASK 0x20
#endif
#ifndef FAN_MARK_IGNORED_SURV_MODIFY
#define FAN_MARK_IGNORED_SURV_MODIFY 0x40
#endif

static int fan_perm_new(int cls) { return fanotify_init(cls | FAN_NONBLOCK | FAN_CLOEXEC, O_RDONLY); }

/* Wait (<= 5 s) for one event on a nonblocking group. */
static int fan_wait1(int ffd, struct fev *g)
{
    for (int i = 0; i < 500; i++) {
        int n = fan_drain(ffd, g, 1);
        if (n > 0) return n;
        usleep(10000);
    }
    return 0;
}

static void fan_answer(int ffd, int fd, unsigned resp)
{
    struct fanotify_response r = { .fd = fd, .response = resp };
    (void)!write(ffd, &r, sizeof r);
}

/* The child performs `op` ('o' open, 'r' read) on `p`; the parent answers with `resp`. */
static int fan_perm_run(const char *p, unsigned mask, char op, unsigned resp, int *evmask, int *child_errno)
{
    int ffd = fan_perm_new(FAN_CLASS_CONTENT);
    if (ffd < 0 || fanotify_mark(ffd, FAN_MARK_ADD, mask, AT_FDCWD, p) != 0) return -1;
    pid_t c = fork();
    if (c == 0) {
        int fd = open(p, O_RDONLY);
        if (fd < 0) _exit(100 + (errno & 0x3f));
        if (op == 'r') {
            char b[4];
            if (read(fd, b, sizeof b) < 0) _exit(100 + (errno & 0x3f));
        }
        _exit(0);
    }
    struct fev g[1];
    /* For 'r' the child's open raises no event (mask is ACCESS_PERM only). */
    int n = fan_wait1(ffd, g);
    int ok = n == 1 && g[0].fd >= 0;
    if (n == 1) *evmask = (int)g[0].mask;
    if (ok) { fan_answer(ffd, g[0].fd, resp); close(g[0].fd); }
    int st = 0; waitpid(c, &st, 0);
    *child_errno = WIFEXITED(st) ? WEXITSTATUS(st) : -1;
    close(ffd);
    return ok ? 0 : -2;
}

static void t_fanperm_deny(void)
{
    wr("fp1", "data");
    char p[128]; path(p, "fp1");
    int em = 0, ce = 0;
    int r = fan_perm_run(p, FAN_OPEN_PERM, 'o', FAN_DENY, &em, &ce);
    int ok = r == 0 && (em & FAN_OPEN_PERM) && ce == 100 + EPERM;
    if (!ok) printf("  fanperm_deny: r=%d em=%x ce=%d\n", r, em, ce);
    report("t_fanperm_deny", ok);
}

static void t_fanperm_allow(void)
{
    wr("fp2", "data");
    char p[128]; path(p, "fp2");
    int em = 0, ce = 0;
    int r = fan_perm_run(p, FAN_OPEN_PERM, 'o', FAN_ALLOW, &em, &ce);
    int ok = r == 0 && (em & FAN_OPEN_PERM) && ce == 0;
    if (!ok) printf("  fanperm_allow: r=%d em=%x ce=%d\n", r, em, ce);
    report("t_fanperm_allow", ok);
}

static void t_fanperm_access(void)
{
    wr("fp3", "data");
    char p[128]; path(p, "fp3");
    int em = 0, ce = 0;
    int r = fan_perm_run(p, FAN_ACCESS_PERM, 'r', FAN_DENY, &em, &ce);
    int ok = r == 0 && (em & FAN_ACCESS_PERM) && ce == 100 + EPERM;
    if (!ok) printf("  fanperm_access: r=%d em=%x ce=%d\n", r, em, ce);
    int em2 = 0, ce2 = 0;
    int r2 = fan_perm_run(p, FAN_ACCESS_PERM, 'r', FAN_ALLOW, &em2, &ce2);
    report("t_fanperm_access", ok && r2 == 0 && ce2 == 0);
}

/* Closing the group while the opener is blocked allows it. */
static void t_fanperm_close_allows(void)
{
    wr("fp4", "data");
    char p[128]; path(p, "fp4");
    int ffd = fan_perm_new(FAN_CLASS_CONTENT);
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FAN_OPEN_PERM, AT_FDCWD, p);
    pid_t c = fork();
    if (c == 0) { int fd = open(p, O_RDONLY); _exit(fd >= 0 ? 0 : 1); }
    usleep(300000);
    int st = 0;
    int still_blocked = waitpid(c, &st, WNOHANG) == 0;
    if (ffd >= 0) close(ffd);
    waitpid(c, &st, 0);
    report("t_fanperm_close_allows", mr == 0 && still_blocked && WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

/* The listener's own opens must not wait on its own group. */
static void t_fanperm_self(void)
{
    wr("fp5", "data");
    char p[128]; path(p, "fp5");
    int ffd = fan_perm_new(FAN_CLASS_CONTENT);
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FAN_OPEN_PERM | FAN_ACCESS_PERM, AT_FDCWD, p);
    int fd = open(p, O_RDONLY);
    char b[4]; ssize_t n = fd >= 0 ? read(fd, b, sizeof b) : -1;
    struct fev g[2]; int q = ffd < 0 ? 0 : fan_drain(ffd, g, 2);
    report("t_fanperm_self", mr == 0 && fd >= 0 && n > 0 && q <= 0);
    if (fd >= 0) close(fd);
    if (ffd >= 0) close(ffd);
}

static void t_fanperm_einval(void)
{
    wr("fp6", "x");
    char p[128]; path(p, "fp6");
    int n = fan_new();
    int ok = n >= 0 && fanotify_mark(n, FAN_MARK_ADD, FAN_OPEN_PERM, AT_FDCWD, p) == -1 && errno == EINVAL;
    int f = fan_perm_new(FAN_CLASS_CONTENT);
    struct fanotify_response r = { .fd = 12345, .response = FAN_ALLOW };
    ok = ok && f >= 0 && write(f, &r, sizeof r) == -1 && errno == ENOENT;
    ok = ok && fanotify_init(FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT, O_RDONLY) == -1 && errno == EINVAL;
    report("t_fanperm_einval", ok);
    if (n >= 0) close(n);
    if (f >= 0) close(f);
}

static void t_fan_ignore(void)
{
    mkd("fid");
    touch("fid/a"); touch("fid/b");
    char d[128], a[128], b[128]; path(d, "fid"); path(a, "fid/a"); path(b, "fid/b");
    int ffd = fan_new();
    int m1 = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FAN_OPEN | FAN_EVENT_ON_CHILD, AT_FDCWD, d);
    int m2 = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD | FAN_MARK_IGNORED_MASK | FAN_MARK_IGNORED_SURV_MODIFY, FAN_OPEN, AT_FDCWD, a);
    int fa = open(a, O_RDONLY), fb = open(b, O_RDONLY);
    close(fa); close(fb);
    struct fev g[8]; int n = ffd < 0 ? 0 : fan_drain(ffd, g, 8);
    int nb = 0, na = 0;
    for (int i = 0; i < n; i++) { if (ends_with(g[i].link, "/b")) nb++; if (ends_with(g[i].link, "/a")) na++; }
    fan_closeall(g, n);
    int rm = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_REMOVE | FAN_MARK_IGNORED_MASK, FAN_OPEN, AT_FDCWD, a);
    fa = open(a, O_RDONLY); close(fa);
    n = ffd < 0 ? 0 : fan_drain(ffd, g, 8);
    int na2 = 0;
    for (int i = 0; i < n; i++) if (ends_with(g[i].link, "/a")) na2++;
    fan_closeall(g, n);
    int ok = m1 == 0 && m2 == 0 && nb == 1 && na == 0 && rm == 0 && na2 == 1;
    if (!ok) printf("  fan_ignore: m1=%d m2=%d nb=%d na=%d rm=%d na2=%d\n", m1, m2, nb, na, rm, na2);
    report("t_fan_ignore", ok);
    if (ffd >= 0) close(ffd);
}

static void t_fan_eperm(void)
{
    pid_t c = fork();
    if (c == 0) {
        if (setgid(65534) || setuid(65534)) _exit(2);
        int r = fanotify_init(FAN_CLASS_NOTIF, O_RDONLY);
        _exit(r == -1 && errno == EPERM ? 0 : 1);
    }
    int st = 0; waitpid(c, &st, 0);
    report("t_fan_eperm", WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

static void t_fan_hardlink(void)
{
    touch("fh_a");
    char a[128], b[128]; path(a, "fh_a"); path(b, "fh_b"); link(a, b);
    int ffd = fan_new();
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FAN_OPEN | FAN_MODIFY, AT_FDCWD, b);
    wr("fh_a", "q");
    struct fev g[8]; int n = ffd < 0 ? 0 : fan_drain(ffd, g, 8);
    fan_closeall(g, n);
    report("t_fan_hardlink", mr == 0 && n == 2 && g[0].mask == FAN_OPEN && g[1].mask == FAN_MODIFY);
    if (ffd >= 0) close(ffd);
}


/* ---- fanotify FID reporting, OPEN_EXEC_PERM, DENY_ERRNO ---- */
#ifndef FAN_REPORT_FID
#define FAN_REPORT_FID 0x200
#endif
#ifndef FAN_REPORT_DIR_FID
#define FAN_REPORT_DIR_FID 0x400
#endif
#ifndef FAN_REPORT_NAME
#define FAN_REPORT_NAME 0x800
#endif
#ifndef FAN_REPORT_DFID_NAME
#define FAN_REPORT_DFID_NAME (FAN_REPORT_DIR_FID | FAN_REPORT_NAME)
#endif
#ifndef FAN_CREATE
#define FAN_CREATE 0x100
#define FAN_DELETE 0x200
#define FAN_DELETE_SELF 0x400
#define FAN_MOVE_SELF 0x800
#define FAN_MOVED_FROM 0x40
#define FAN_MOVED_TO 0x80
#endif
#ifndef FAN_ATTRIB
#define FAN_ATTRIB 0x4
#endif
#ifndef FAN_OPEN_EXEC_PERM
#define FAN_OPEN_EXEC_PERM 0x40000
#endif
#ifndef FAN_NOFD
#define FAN_NOFD -1
#endif
#define FANX_ERRNO(e) (FAN_DENY | ((unsigned)(e) << 24))

struct fidrec { int type; unsigned dev_lo, dev_hi, hb; int ht; unsigned long long ino; char name[64]; };
struct fidev { unsigned long long mask; int fd, pid, nr; struct fidrec r[3]; };

/* Parse like fsnotifywait: walk events by event_len, info records by hdr.len. */
static int fid_drain(int ffd, struct fidev *g, int max)
{
    static char buf[8192]; int k = 0;
    ssize_t n = read(ffd, buf, sizeof buf), off = 0;
    while (n > 0 && off + (ssize_t)sizeof(struct fanotify_event_metadata) <= n && k < max) {
        struct fanotify_event_metadata *m = (void *)(buf + off);
        if (m->event_len < m->metadata_len || off + (ssize_t)m->event_len > n) break;
        g[k].mask = m->mask; g[k].fd = m->fd; g[k].pid = m->pid; g[k].nr = 0;
        unsigned o = m->metadata_len;
        while (o + 4 <= m->event_len && g[k].nr < 3) {
            unsigned char *h = (unsigned char *)m + o;
            unsigned len = h[2] | (h[3] << 8);
            if (len < 4 || o + len > m->event_len) break;
            struct fidrec *r = &g[k].r[g[k].nr];
            memset(r, 0, sizeof *r);
            r->type = h[0];
            if (((h[0] >= 1 && h[0] <= 3) || h[0] == 10 || h[0] == 12) && len >= 20) {
                memcpy(&r->dev_lo, h + 4, 4); memcpy(&r->dev_hi, h + 8, 4);
                memcpy(&r->hb, h + 12, 4); memcpy(&r->ht, h + 16, 4);
                if (r->hb >= 8 && 20 + r->hb <= len) memcpy(&r->ino, h + 20, 8);
                if ((h[0] == 2 || h[0] == 10 || h[0] == 12) && 20 + r->hb < len) snprintf(r->name, sizeof r->name, "%s", (char *)h + 20 + r->hb);
                g[k].nr++;
            }
            o += len;
        }
        off += m->event_len; k++;
    }
    return k;
}

static int fid_is(const struct fidrec *r, int type, const struct stat *st, const char *name)
{
    return r->type == type && r->hb >= 8 && r->ino == (unsigned long long)st->st_ino
        && r->dev_lo == (unsigned)st->st_dev && r->dev_hi == (unsigned)((unsigned long long)st->st_dev >> 32)
        && (name == NULL || !strcmp(r->name, name));
}

static void t_fanfid_einval(void)
{
    int ok = 1;
    ok = ok && fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_NAME, O_RDONLY) == -1 && errno == EINVAL;
    ok = ok && fanotify_init(FAN_CLASS_CONTENT | FAN_REPORT_FID, O_RDONLY) == -1 && errno == EINVAL;
    int a = fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_FID | FAN_NONBLOCK, O_RDONLY);
    char d[128]; path(d, "ffinv");
    mkdir(d, 0755);
    ok = ok && a >= 0 && fanotify_mark(a, FAN_MARK_ADD, FAN_CREATE | FAN_ONDIR, AT_FDCWD, d) == -1 && errno == EINVAL;
    ok = ok && fanotify_mark(a, FAN_MARK_ADD | FAN_MARK_MOUNT, FAN_ATTRIB, AT_FDCWD, d) == -1 && errno == EINVAL;
    ok = ok && fanotify_mark(a, FAN_MARK_ADD, FAN_ATTRIB | FAN_DELETE_SELF | FAN_MOVE_SELF, AT_FDCWD, d) == 0;
    int b = fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_DFID_NAME | FAN_NONBLOCK, O_RDONLY);
    ok = ok && b >= 0 && fanotify_mark(b, FAN_MARK_ADD, FAN_CREATE | FAN_DELETE | FAN_ONDIR, AT_FDCWD, d) == 0;
    int pl = fanotify_init(FAN_CLASS_NOTIF | FAN_NONBLOCK, O_RDONLY);
    ok = ok && pl >= 0 && fanotify_mark(pl, FAN_MARK_ADD, FAN_CREATE, AT_FDCWD, d) == -1 && errno == EINVAL;
    report("t_fanfid_einval", ok);
    if (a >= 0) close(a);
    if (b >= 0) close(b);
    if (pl >= 0) close(pl);
    rmdir(d);
}

static void t_fanfid_dfid_name(void)
{
    char d[128], pa[160], pb[160];
    path(d, "ffd"); mkdir(d, 0755);
    snprintf(pa, sizeof pa, "%s/aa", d); snprintf(pb, sizeof pb, "%s/bb", d);
    struct stat dst; int sr = stat(d, &dst);
    int ffd = fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_DFID_NAME | FAN_NONBLOCK | FAN_CLOEXEC, O_RDONLY);
    unsigned long long mk = FAN_CREATE | FAN_DELETE | FAN_MOVED_FROM | FAN_MOVED_TO | FAN_ATTRIB | FAN_EVENT_ON_CHILD | FAN_ONDIR;
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, mk, AT_FDCWD, d);
    int fd = open(pa, O_CREAT | O_WRONLY, 0644); if (fd >= 0) close(fd);
    int c1 = chmod(pa, 0600);
    int r1 = rename(pa, pb);
    int u1 = unlink(pb);
    struct fidev g[8]; int n = ffd < 0 ? 0 : fid_drain(ffd, g, 8);
    /* Like Linux, unlink also raises FAN_ATTRIB (link count change) before FAN_DELETE: drop it. */
    int m = 0;
    for (int i = 0; i < n; i++) {
        if (i >= 4 && (g[i].mask & FAN_ATTRIB) && !(g[i].mask & FAN_DELETE)) continue;
        g[m++] = g[i];
    }
    int n0 = n; n = m;
    int ok = sr == 0 && mr == 0 && c1 == 0 && r1 == 0 && u1 == 0 && n == 5;
    const unsigned long long want[5] = { FAN_CREATE, FAN_ATTRIB, FAN_MOVED_FROM, FAN_MOVED_TO, FAN_DELETE };
    const char *nm[5] = { "aa", "aa", "aa", "bb", "bb" };
    for (int i = 0; ok && i < 5; i++)
        ok = (g[i].mask & ~(unsigned long long)FAN_ONDIR) == want[i] && g[i].fd == FAN_NOFD && g[i].pid == getpid()
          && g[i].nr == 1 && fid_is(&g[i].r[0], 2, &dst, nm[i]);
    if (!ok) printf("  fanfid_dfid_name: n0=%d sr=%d mr=%d n=%d m0=%llx nr0=%d t0=%d name0=%s\n", n0, sr, mr, n, n > 0 ? g[0].mask : 0, n > 0 ? g[0].nr : -1, n > 0 ? g[0].r[0].type : -1, n > 0 ? g[0].r[0].name : "");
    report("t_fanfid_dfid_name", ok);
    if (ffd >= 0) close(ffd);
    rmdir(d);
}

static void t_fanfid_dir_fid(void)
{
    char d[128], pa[160];
    path(d, "ffdd"); mkdir(d, 0755);
    snprintf(pa, sizeof pa, "%s/xx", d);
    struct stat dst; int sr = stat(d, &dst);
    int ffd = fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_DIR_FID | FAN_NONBLOCK | FAN_CLOEXEC, O_RDONLY);
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FAN_CREATE | FAN_ONDIR, AT_FDCWD, d);
    int fd = open(pa, O_CREAT | O_WRONLY, 0644); if (fd >= 0) close(fd);
    struct fidev g[4]; int n = ffd < 0 ? 0 : fid_drain(ffd, g, 4);
    int ok = sr == 0 && mr == 0 && n == 1 && g[0].fd == FAN_NOFD && g[0].nr == 1 && fid_is(&g[0].r[0], 3, &dst, NULL) && g[0].r[0].name[0] == 0;
    report("t_fanfid_dir_fid", ok);
    if (ffd >= 0) close(ffd);
    unlink(pa); rmdir(d);
}

static void t_fanfid_self(void)
{
    wr("ffself", "x");
    char p[128]; path(p, "ffself");
    struct stat st; int sr = stat(p, &st);
    int ffd = fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_FID | FAN_NONBLOCK | FAN_CLOEXEC, O_RDONLY);
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FAN_ATTRIB | FAN_DELETE_SELF, AT_FDCWD, p);
    int c1 = chmod(p, 0600);
    int u1 = unlink(p);
    struct fidev g[4]; int n = ffd < 0 ? 0 : fid_drain(ffd, g, 4);
    int ok = sr == 0 && mr == 0 && c1 == 0 && u1 == 0 && n == 2
          && g[0].mask == FAN_ATTRIB && g[1].mask == FAN_DELETE_SELF
          && g[0].fd == FAN_NOFD && g[1].fd == FAN_NOFD
          && g[0].nr == 1 && g[1].nr == 1 && fid_is(&g[0].r[0], 1, &st, NULL) && fid_is(&g[1].r[0], 1, &st, NULL);
    if (!ok) printf("  fanfid_self: sr=%d mr=%d n=%d m0=%llx m1=%llx\n", sr, mr, n, n > 0 ? g[0].mask : 0, n > 1 ? g[1].mask : 0);
    report("t_fanfid_self", ok);
    if (ffd >= 0) close(ffd);
}

#include <sys/syscall.h>
#if defined(__x86_64__)
#define FH_N2H 303
#define FH_OPEN 304
#else
#define FH_N2H 264
#define FH_OPEN 265
#endif
struct fh20 { unsigned hb; int ht; unsigned long long ino; unsigned gen; };

static void t_fhandle(void)
{
    wr("fhfile", "hello");
    char p[128], d[128]; path(p, "fhfile");
    snprintf(d, sizeof d, "%s", p); *strrchr(d, '/') = 0;
    struct stat st; int sr = stat(p, &st);
    int ffd = fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_FID | FAN_NONBLOCK | FAN_CLOEXEC, O_RDONLY);
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FAN_ATTRIB | FAN_DELETE_SELF, AT_FDCWD, p);
    int c1 = chmod(p, 0600);
    struct fidev g[4]; int n = ffd < 0 ? 0 : fid_drain(ffd, g, 4);
    struct fh20 h; memset(&h, 0xee, sizeof h); h.hb = 20; int mid = 0;
    long nr = syscall(FH_N2H, AT_FDCWD, p, &h, &mid, 0);
    int same = nr == 0 && n >= 1 && g[0].nr == 1 && g[0].r[0].hb == h.hb && g[0].r[0].ht == h.ht
            && g[0].r[0].ino == h.ino && h.hb == 12 && h.ht == 0x4c && h.ino == (unsigned long long)st.st_ino;
    /* too small: EOVERFLOW and the needed size */
    struct fh20 s; memset(&s, 0, sizeof s);
    long ov = syscall(FH_N2H, AT_FDCWD, p, &s, &mid, 0); int ove = errno; (void)ove;
    int ov_ok = ov == -1 && errno == EOVERFLOW && s.hb == 12;
    /* AT_EMPTY_PATH on an fd gives the same handle */
    int pfd = open(p, O_RDONLY); struct fh20 h2; memset(&h2, 0, sizeof h2); h2.hb = 20;
    long er = syscall(FH_N2H, pfd, "", &h2, &mid, 0x1000);
    int empty_ok = er == 0 && h2.ino == h.ino && h2.ht == h.ht;
    /* open_by_handle_at reopens the right file */
    int mfd = open(d, O_RDONLY | O_DIRECTORY);
    int ofd = (int)syscall(FH_OPEN, mfd, &h, O_RDONLY);
    int oe = errno; struct stat sm; fstat(mfd, &sm);
    char b[16] = {0}; ssize_t rn = ofd >= 0 ? read(ofd, b, sizeof b - 1) : -1;
    struct stat so; int open_ok = ofd >= 0 && fstat(ofd, &so) == 0 && so.st_ino == st.st_ino && rn == 5 && !strcmp(b, "hello");
    if (ofd >= 0) close(ofd);
    /* unprivileged: EPERM */
    pid_t c = fork(); int eperm_ok = 0;
    if (c == 0) {
        if (setgid(65534) || setuid(65534)) _exit(2);
        long r = syscall(FH_OPEN, mfd, &h, O_RDONLY);
        _exit(r == -1 && errno == EPERM ? 0 : 3);
    }
    if (c > 0) { int stt = 0; waitpid(c, &stt, 0); eperm_ok = WIFEXITED(stt) && WEXITSTATUS(stt) == 0; }
    /* deleted inode: ESTALE */
    int ul = unlink(p);
    long es = syscall(FH_OPEN, mfd, &h, O_RDONLY);
    int estale_ok = ul == 0 && es == -1 && errno == ESTALE;
    int ok = sr == 0 && mr == 0 && c1 == 0 && same && ov_ok && empty_ok && open_ok && eperm_ok && estale_ok;
    if (!ok) printf("  fhandle: nr=%ld same=%d ov=%d empty=%d open=%d(ofd=%d) eperm=%d estale=%d oe=%d mdev=%llx fdev=%llx\n",
                    nr, same, ov_ok, empty_ok, open_ok, ofd, eperm_ok, estale_ok, oe, (unsigned long long)sm.st_dev, (unsigned long long)st.st_dev);
    report("t_fhandle", ok);
    if (pfd >= 0) close(pfd);
    if (mfd >= 0) close(mfd);
    if (ffd >= 0) close(ffd);
}

/* A listener process denies / allows the execve of /bin/true. */
static int fan_exec_run(unsigned resp, int *evmask, int *child_rc)
{
    int ffd = fan_perm_new(FAN_CLASS_CONTENT);
    if (ffd < 0 || fanotify_mark(ffd, FAN_MARK_ADD, FAN_OPEN_EXEC_PERM, AT_FDCWD, "/bin/true") != 0) return -1;
    pid_t c = fork();
    if (c == 0) {
        char *av[] = { "true", NULL };
        execv("/bin/true", av);
        _exit(100 + (errno & 0x3f));
    }
    struct fev g[1];
    int n = fan_wait1(ffd, g);
    int ok = n == 1 && g[0].fd >= 0;
    if (n == 1) *evmask = (int)g[0].mask;
    if (ok) { fan_answer(ffd, g[0].fd, resp); close(g[0].fd); }
    int st = 0; waitpid(c, &st, 0);
    *child_rc = WIFEXITED(st) ? WEXITSTATUS(st) : -1;
    close(ffd);
    return ok ? 0 : -2;
}

static void t_fanperm_exec(void)
{
    int em = 0, rc = 0, ok;
    int r1 = fan_exec_run(FAN_DENY, &em, &rc);
    ok = r1 == 0 && (em & FAN_OPEN_EXEC_PERM) && rc == 100 + EPERM;
    if (!ok) printf("  fanperm_exec deny: r=%d em=%x rc=%d\n", r1, em, rc);
    int r2 = fan_exec_run(FAN_ALLOW, &em, &rc);
    ok = ok && r2 == 0 && rc == 0;
    int r3 = fan_exec_run(FANX_ERRNO(EACCES), &em, &rc);
    ok = ok && r3 == 0 && rc == 100 + EACCES;
    if (!ok) printf("  fanperm_exec: r2=%d r3=%d rc=%d\n", r2, r3, rc);
    report("t_fanperm_exec", ok);
}

static void t_fanperm_deny_errno(void)
{
    wr("fde", "x");
    char p[128]; path(p, "fde");
    int em = 0, ce = 0;
    int r = fan_perm_run(p, FAN_OPEN_PERM, 'o', FANX_ERRNO(ENOSPC), &em, &ce);
    int ok = r == 0 && ce == 100 + ENOSPC;
    int r2 = fan_perm_run(p, FAN_ACCESS_PERM, 'r', FANX_ERRNO(EACCES), &em, &ce);
    ok = ok && r2 == 0 && ce == 100 + EACCES;
    /* errno bits without FAN_DENY are refused */
    int f = fan_perm_new(FAN_CLASS_CONTENT);
    struct fanotify_response bad = { .fd = 5, .response = FAN_ALLOW | (13u << 24) };
    ok = ok && f >= 0 && write(f, &bad, sizeof bad) == -1 && errno == EINVAL;
    if (f >= 0) close(f);
    if (!ok) printf("  fanperm_deny_errno: r=%d ce=%d r2=%d\n", r, ce, r2);
    report("t_fanperm_deny_errno", ok);
}


/* ---- FAN_RENAME / TARGET_FID, FAN_OPEN_EXEC, OPEN_PERM on create, filesystem-mark FID events ---- */
#ifndef FAN_RENAME
#define FAN_RENAME 0x10000000
#endif
#ifndef FAN_REPORT_TARGET_FID
#define FAN_REPORT_TARGET_FID 0x1000
#endif
#ifndef FAN_OPEN_EXEC
#define FAN_OPEN_EXEC 0x1000
#endif
#ifndef FAN_MARK_FILESYSTEM
#define FAN_MARK_FILESYSTEM 0x100
#endif

static void t_fan_rename(void)
{
    char da[128], db[128], pa[160], pb[160];
    path(da, "frna"); path(db, "frnb"); mkdir(da, 0755); mkdir(db, 0755);
    snprintf(pa, sizeof pa, "%s/xx", da); snprintf(pb, sizeof pb, "%s/yy", db);
    int fd = open(pa, O_CREAT | O_WRONLY, 0644); if (fd >= 0) close(fd);
    struct stat sa, sb, so; int s1 = stat(da, &sa), s2 = stat(db, &sb), s3 = stat(pa, &so);
    int ffd = fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_DFID_NAME | FAN_REPORT_FID | FAN_REPORT_TARGET_FID | FAN_NONBLOCK | FAN_CLOEXEC, O_RDONLY);
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FAN_RENAME | FAN_ONDIR, AT_FDCWD, da);
    int rr = rename(pa, pb);
    struct fidev g[4]; int n = ffd < 0 ? 0 : fid_drain(ffd, g, 4);
    int ok = s1 == 0 && s2 == 0 && s3 == 0 && mr == 0 && rr == 0 && n == 1 && (g[0].mask & ~(unsigned long long)FAN_ONDIR) == FAN_RENAME
          && g[0].fd == FAN_NOFD && g[0].nr == 3
          && g[0].r[0].type == 10 && g[0].r[0].ino == (unsigned long long)sa.st_ino && !strcmp(g[0].r[0].name, "xx")
          && g[0].r[1].type == 12 && g[0].r[1].ino == (unsigned long long)sb.st_ino && !strcmp(g[0].r[1].name, "yy")
          && fid_is(&g[0].r[2], 1, &so, NULL);
    if (!ok) printf("  fan_rename: mr=%d rr=%d n=%d nr=%d t=%d,%d,%d\n", mr, rr, n, n > 0 ? g[0].nr : -1, n > 0 ? g[0].r[0].type : -1, n > 0 ? g[0].r[1].type : -1, n > 0 ? g[0].r[2].type : -1);
    /* EINVAL: TARGET_FID needs DFID_NAME; FAN_RENAME needs a FAN_REPORT_NAME group. */
    int bad = fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_FID | FAN_REPORT_TARGET_FID, O_RDONLY);
    int e1 = bad == -1 && errno == EINVAL;
    int fo = fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_FID | FAN_NONBLOCK, O_RDONLY);
    int e2 = fo >= 0 && fanotify_mark(fo, FAN_MARK_ADD, FAN_RENAME, AT_FDCWD, da) == -1 && errno == EINVAL;
    if (!(e1 && e2)) printf("  fan_rename: e1=%d e2=%d\n", e1, e2);
    report("t_fan_rename", ok && e1 && e2);
    if (ffd >= 0) close(ffd);
    if (fo >= 0) close(fo);
    unlink(pb); rmdir(da); rmdir(db);
}

static void t_fan_open_exec(void)
{
    int ffd = fan_new();
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FAN_OPEN_EXEC, AT_FDCWD, "/bin/true");
    pid_t c = fork();
    if (c == 0) { char *av[] = { "true", NULL }; execv("/bin/true", av); _exit(127); }
    int st = 0; waitpid(c, &st, 0);
    struct fev g[8]; int n = ffd < 0 ? 0 : fan_drain(ffd, g, 8);
    int hit = 0;
    for (int i = 0; i < n; i++) if ((g[i].mask & FAN_OPEN_EXEC) && g[i].pid == c && ends_with(g[i].link, "/true")) hit++;
    fan_closeall(g, n);
    if (!(mr == 0 && hit == 1)) printf("  fan_open_exec: mr=%d n=%d hit=%d\n", mr, n, hit);
    report("t_fan_open_exec", mr == 0 && hit == 1);
    if (ffd >= 0) close(ffd);
}

/* MOUNT = the whole mount the path is on (a sibling dir counts, another tmpfs does not);
 * FILESYSTEM = every mount of the superblock (a bind mount included); FAN_OPEN_EXEC also raises FAN_OPEN. */
static void t_fan_mount_scope(void)
{
    mkd("ms1"); mkd("ms2"); touch("ms2/sib");
    char d1[128]; path(d1, "ms1");
    int ffd = fan_new();
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD | FAN_MARK_MOUNT, FAN_OPEN | FAN_CLOSE_WRITE, AT_FDCWD, d1);
    wr("ms2/sib", "z");
    int other = open("/tmp/.fanmnt-other", O_CREAT | O_WRONLY, 0644);
    if (other >= 0) { write(other, "z", 1); close(other); }
    struct fev g[32]; int n = ffd < 0 ? 0 : fan_drain(ffd, g, 32);
    int sib = 0, oth = 0;
    for (int i = 0; i < n; i++) { if (ends_with(g[i].link, "/ms2/sib")) sib++; if (ends_with(g[i].link, "/.fanmnt-other")) oth++; }
    fan_closeall(g, n);
    unlink("/tmp/.fanmnt-other");
    if (!(mr == 0 && sib >= 1 && oth == 0)) printf("  fan_mount_scope: mr=%d sib=%d oth=%d\n", mr, sib, oth);
    report("t_fan_mount_scope", mr == 0 && sib >= 1 && oth == 0);
    if (ffd >= 0) close(ffd);
}

static void t_fan_fs_scope(void)
{
    mkd("fs1"); mkd("fs2"); mkd("fsb"); touch("fs2/sib");
    char d1[128], d2[128], db[128]; path(d1, "fs1"); path(d2, "fs2"); path(db, "fsb");
    int bound = mount(d2, db, NULL, MS_BIND, NULL) == 0;
    int ffd = fan_new();
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD | FAN_MARK_FILESYSTEM, FAN_OPEN | FAN_CLOSE_WRITE, AT_FDCWD, bound ? db : d1);
    wr("fs2/sib", "z");
    struct fev g[32]; int n = ffd < 0 ? 0 : fan_drain(ffd, g, 32);
    int sib = 0;
    for (int i = 0; i < n; i++) if (ends_with(g[i].link, "/sib")) sib++;
    fan_closeall(g, n);
    if (bound) umount(db);
    if (!(mr == 0 && sib >= 1)) printf("  fan_fs_scope: bound=%d mr=%d sib=%d\n", bound, mr, sib);
    report("t_fan_fs_scope", mr == 0 && sib >= 1);
    if (ffd >= 0) close(ffd);
}

static void t_fan_exec_open(void)
{
    int ffd = fan_new();
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FAN_OPEN, AT_FDCWD, "/bin/true");
    pid_t c = fork();
    if (c == 0) { char *av[] = { "true", NULL }; execv("/bin/true", av); _exit(127); }
    int st = 0; waitpid(c, &st, 0);
    struct fev g[8]; int n = ffd < 0 ? 0 : fan_drain(ffd, g, 8);
    int hit = 0;
    for (int i = 0; i < n; i++) if ((g[i].mask & FAN_OPEN) && g[i].pid == c && ends_with(g[i].link, "/true")) hit++;
    fan_closeall(g, n);
    if (!(mr == 0 && hit >= 1)) printf("  fan_exec_open: mr=%d n=%d hit=%d\n", mr, n, hit);
    report("t_fan_exec_open", mr == 0 && hit >= 1);
    if (ffd >= 0) close(ffd);
}

/* Linux raises FAN_OPEN_PERM for the O_CREAT that creates the file (after creation); DENY keeps the file, fails the open. */
static void t_fanperm_create(void)
{
    char d[128], p[160]; path(d, "fpc"); mkdir(d, 0755); snprintf(p, sizeof p, "%s/new", d);
    int ffd = fan_perm_new(FAN_CLASS_CONTENT);
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD, FAN_OPEN_PERM | FAN_EVENT_ON_CHILD, AT_FDCWD, d);
    pid_t c = fork();
    if (c == 0) { int fd = open(p, O_CREAT | O_WRONLY, 0644); if (fd < 0) _exit(100 + (errno & 0x3f)); _exit(0); }
    struct fev g[1];
    int n = ffd < 0 ? 0 : fan_wait1(ffd, g);
    int ok = n == 1 && g[0].fd >= 0 && (g[0].mask & FAN_OPEN_PERM);
    if (ok) { fan_answer(ffd, g[0].fd, FAN_DENY); close(g[0].fd); }
    int st = 0; waitpid(c, &st, 0);
    int rc = WIFEXITED(st) ? WEXITSTATUS(st) : -1;
    struct stat sb; int ex = stat(p, &sb) == 0;
    if (!(mr == 0 && ok && rc == 100 + EPERM && ex)) printf("  fanperm_create: mr=%d n=%d ok=%d rc=%d ex=%d\n", mr, n, ok, rc, ex);
    report("t_fanperm_create", mr == 0 && ok && rc == 100 + EPERM && ex);
    if (ffd >= 0) close(ffd);
    unlink(p); rmdir(d);
}

static void t_fan_fs_fid(void)
{
    char d[128], p[160]; path(d, "ffs"); mkdir(d, 0755); snprintf(p, sizeof p, "%s/f1", d);
    struct stat dst; int sr = stat(d, &dst);
    int ffd = fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_DFID_NAME | FAN_NONBLOCK | FAN_CLOEXEC, O_RDONLY);
    int mr = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD | FAN_MARK_FILESYSTEM, FAN_CREATE | FAN_DELETE | FAN_ONDIR, AT_FDCWD, d);
    int mm = ffd < 0 ? -1 : fanotify_mark(ffd, FAN_MARK_ADD | FAN_MARK_MOUNT, FAN_CREATE, AT_FDCWD, d);
    int fd = open(p, O_CREAT | O_WRONLY, 0644); if (fd >= 0) close(fd);
    int u = unlink(p);
    struct fidev g[8]; int n = ffd < 0 ? 0 : fid_drain(ffd, g, 8);
    int ok = sr == 0 && mr == 0 && mm == -1 && errno == EINVAL && u == 0 && n == 2
          && (g[0].mask & FAN_CREATE) && (g[1].mask & FAN_DELETE)
          && g[0].nr >= 1 && fid_is(&g[0].r[0], 2, &dst, "f1") && g[1].nr >= 1 && fid_is(&g[1].r[0], 2, &dst, "f1");
    if (!ok) printf("  fan_fs_fid: mr=%d mm=%d n=%d m0=%llx m1=%llx\n", mr, mm, n, n > 0 ? g[0].mask : 0, n > 1 ? g[1].mask : 0);
    report("t_fan_fs_fid", ok);
    if (ffd >= 0) close(ffd);
    rmdir(d);
}

static void t_hl_cases(void)
{
    t_hl_self(); t_hl_unlink_one(); t_hl_link_after_watch();
    t_fan_basic(); t_fan_mount(); t_fan_child(); t_fan_remove_flush(); t_fan_einval(); t_fan_eperm();
    t_fanperm_deny(); t_fanperm_allow(); t_fanperm_access(); t_fanperm_close_allows(); t_fanperm_self(); t_fanperm_einval(); t_fan_ignore();
    t_fan_hardlink();
    t_fanfid_einval(); t_fanfid_dfid_name(); t_fanfid_dir_fid(); t_fanfid_self(); t_fhandle(); t_fanperm_exec(); t_fanperm_deny_errno();
    t_fan_rename(); t_fan_open_exec(); t_fan_mount_scope(); t_fan_fs_scope(); t_fan_exec_open(); t_fanperm_create(); t_fan_fs_fid();
}

static void l2_l3_cases(void)
{
    t_create(); t_delete(); t_move_pair(); t_moved_to(); t_silent_unrelated();
    t_dir_events(); t_dir_follow();
    t_rmdir_self(); t_move_self(); t_dir_rename_then_create(); t_symlink_link_mknod();
    t_modify_file(); t_modify_in_dir(); t_modify_trunc(); t_close_write(); t_close_dup();
    t_attrib_chmod(); t_access(); t_coalesce(); t_unlink_open(); t_unlink_self(); t_rename_over();
    t_umount(); t_cross_bind();
    t_hl_cases();
}

static void run_on(const char *dir, const char *tag)
{
    char saved[64]; snprintf(saved, sizeof saved, "%s", base);
    snprintf(base, sizeof base, "%s/inot2-%d", dir, getpid());
    mkdir(base, 0755); mkd("x"); mkd("y");
    tg = tag;
    l2_l3_cases();
    char cmd[128]; snprintf(cmd, sizeof cmd, "rm -rf %s", base);
    if (system(cmd)) {}
    tg = "";
    snprintf(base, sizeof base, "%s", saved);
}

static int knob_on(void)
{
    char b[4096]; int fd = open("/proc/cmdline", O_RDONLY);
    if (fd < 0) return 0;
    ssize_t n = read(fd, b, sizeof b - 1); close(fd);
    if (n < 0) return 0;
    b[n] = 0;
    return strstr(b, "vfs.inotify=0") == NULL;
}

static double now_s(void)
{
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

/* L5 microbenchmark: 4 KiB write loop and create+unlink loop in a directory,
 * with 0 and with 200 unrelated watches. `inotifytest bench <dir>`. */
static int bench(const char *dir)
{
    char p[256], q[300], buf[4096];
    memset(buf, 'x', sizeof buf);
    snprintf(p, sizeof p, "%s/benchf", dir);
    for (int round = 0; round < 2; round++) {
        int ifd = -1;
        if (round == 1) {
            ifd = inotify_init1(IN_NONBLOCK);
            for (int i = 0; i < 200; i++) {
                snprintf(q, sizeof q, "%s/bw%d", dir, i);
                mkdir(q, 0755);
                inotify_add_watch(ifd, q, IN_CREATE | IN_DELETE | IN_MOVED_TO | IN_MOVED_FROM);
            }
        }
        double best_w = 1e9, best_c = 1e9;
        for (int rep = 0; rep < 5; rep++) {
            int fd = open(p, O_CREAT | O_RDWR | O_TRUNC, 0644);
            double t0 = now_s();
            for (int i = 0; i < 20000; i++) pwrite(fd, buf, sizeof buf, (i & 63) * 4096);
            double t1 = now_s();
            close(fd);
            if (t1 - t0 < best_w) best_w = t1 - t0;
            t0 = now_s();
            for (int i = 0; i < 3000; i++) {
                snprintf(q, sizeof q, "%s/bc%d", dir, i & 7);
                int f = open(q, O_CREAT | O_WRONLY, 0644); close(f); unlink(q);
            }
            t1 = now_s();
            if (t1 - t0 < best_c) best_c = t1 - t0;
        }
        printf("BENCH watches=%d write20k_s=%.4f creunl3k_s=%.4f\n", round ? 200 : 0, best_w, best_c);
        unlink(p);
        if (round == 1) {
            for (int i = 0; i < 200; i++) { snprintf(q, sizeof q, "%s/bw%d", dir, i); rmdir(q); }
            close(ifd);
        }
    }
    return 0;
}

int main(int argc, char **argv)
{
    if (argc > 2 && !strcmp(argv[1], "bench")) return bench(argv[2]);
    int force = argc > 1 && !strcmp(argv[1], "force");
    int on = knob_on();
    setup();
    if (on || force) {
        t_create(); t_delete(); t_move_pair(); t_moved_to(); t_create_waitfile();
        t_layout(); t_einval_small(); t_fionread(); t_rmwatch_ignored();
        t_block_read(); t_silent_unrelated(); t_dir_events(); t_dir_follow();
        t_eacces(); t_generated();
        t_oneshot(); t_mask_add(); t_mask_create_eexist(); t_onlydir(); t_excl_unlink();
        t_dont_follow(); t_overflow(); t_proc_limits(); t_limit_instances(); t_limit_watches();
        t_limit_perkuid(); t_limit_userns(); t_userns_mntns();
        run_on("/run", "@tmpfs");
        run_on("/root", "@f2fs");
    } else {
        printf("real cases: SKIP (kernel booted with vfs.inotify=0)\n");
    }
    if (!on) t_killswitch(); else printf("t_killswitch: SKIP (knob on)\n");
    printf("inotifytest: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
