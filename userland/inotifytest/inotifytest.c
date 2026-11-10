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
#include <sys/inotify.h>
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
    } else {
        printf("real cases: SKIP (kernel booted without vfs.inotify=1)\n");
    }
    if (!on) t_killswitch(); else printf("t_killswitch: SKIP (knob on)\n");
    printf("inotifytest: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
