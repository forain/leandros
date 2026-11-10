// exitracetest: visibility of a just-reaped pid (H1) and of a just-renamed file seen
// by a freshly exec'd process (H2), the two orderings conmon + `podman container cleanup`
// depend on. Usage: exitracetest [N]. Prints "<case>: PASS|FAIL" and "exitracetest: ALL PASS".
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef PR_SET_CHILD_SUBREAPER
#define PR_SET_CHILD_SUBREAPER 36
#endif

// Child-mode: "probe-pid <pid>" => exit 0 if pid fully gone, else bit flags.
static int probe_pid(pid_t pid)
{
    int bad = 0;
    char p[64];
    struct stat st;
    if (kill(pid, 0) == 0 || errno != ESRCH) bad |= 1;
    snprintf(p, sizeof p, "/proc/%d/stat", pid);
    if (stat(p, &st) == 0) bad |= 2;
    snprintf(p, sizeof p, "/proc/%d", pid);
    if (stat(p, &st) == 0) bad |= 4;
    long fd = syscall(SYS_pidfd_open, pid, 0);
    if (fd >= 0) { bad |= 8; close(fd); }
    else if (errno != ESRCH) bad |= 16;
    return bad;
}

// Child-mode: "probe-file <path> <id>" => 0 if stat, open+read and readdir all see it.
static int probe_file(const char *path, const char *name)
{
    int bad = 0;
    struct stat st;
    if (stat(path, &st) != 0) bad |= 1;
    int fd = open(path, O_RDONLY);
    char b[8];
    if (fd < 0 || read(fd, b, sizeof b) <= 0) bad |= 2;
    if (fd >= 0) close(fd);
    char dir[256];
    snprintf(dir, sizeof dir, "%s", path);
    *strrchr(dir, '/') = 0;
    DIR *d = opendir(dir);
    int found = 0;
    struct dirent *e;
    while (d && (e = readdir(d))) if (!strcmp(e->d_name, name)) found = 1;
    if (d) closedir(d);
    if (!found) bad |= 4;
    return bad;
}

static pid_t spawn_probe(const char *self, char *a1, char *a2, char *a3)
{
    pid_t p = fork();
    if (p == 0) { execl(self, self, a1, a2, a3, (char *)0); _exit(99); }
    return p;
}

static int run_probe(const char *self, char *a1, char *a2, char *a3)
{
    int s;
    pid_t p = spawn_probe(self, a1, a2, a3);
    waitpid(p, &s, 0);
    return WIFEXITED(s) ? WEXITSTATUS(s) : 98;
}

// Child-mode: "lock-try <path> <w|r>" => exit 0 if F_SETLK was granted, 11 if EAGAIN.
static int lock_try(const char *path, int wr)
{
    int fd = open(path, O_RDWR);
    if (fd < 0) return 90;
    struct flock fl = { .l_type = wr ? F_WRLCK : F_RDLCK, .l_whence = SEEK_SET, .l_start = 1073741824, .l_len = 512 };
    int r = fcntl(fd, F_SETLK, &fl);
    return r == 0 ? 0 : (errno == EAGAIN || errno == EACCES ? 11 : 91);
}

static void *thr_lock(void *a)
{
    int *res = a;
    *res = lock_try("/root/exitrace.lk", 1);
    return 0;
}

// Separate opens of one file (as sqlite in two podman processes does) must share a
// lock domain: the inode, not the per-open slot of the mount server.
static int test_locks(const char *self)
{
    int bad = 0;
    int fd = open("/root/exitrace.lk", O_RDWR | O_CREAT | O_TRUNC, 0644);
    write(fd, "x", 1);
    struct flock fl = { .l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 1073741824, .l_len = 512 };
    if (fcntl(fd, F_SETLK, &fl) != 0) { printf("lock-setup failed errno %d\n", errno); return 1; }
    int r = run_probe(self, "lock-try", "/root/exitrace.lk", "w");
    if (r != 11) { bad++; printf("other process got EXCLUSIVE lock held elsewhere (rc %d)\n", r); }
    r = run_probe(self, "lock-try", "/root/exitrace.lk", "r");
    if (r != 11) { bad++; printf("other process got SHARED lock under EXCLUSIVE (rc %d)\n", r); }
    // Same process, another thread, another open: no conflict (POSIX locks belong to the process).
    pthread_t t;
    int tres = -1;
    pthread_create(&t, 0, thr_lock, &tres);
    pthread_join(t, 0);
    if (tres != 0) { bad++; printf("sibling thread conflicted with own process (rc %d)\n", tres); }
    close(fd);
    r = run_probe(self, "lock-try", "/root/exitrace.lk", "w");
    if (r != 0) { bad++; printf("lock still held after holder closed (rc %d)\n", r); }
    unlink("/root/exitrace.lk");
    return bad;
}

int main(int argc, char **argv)
{
    if (argc >= 4 && !strcmp(argv[1], "lock-try")) return lock_try(argv[2], argv[3][0] == 'w');
    if (argc >= 3 && !strcmp(argv[1], "probe-pid")) return probe_pid(atoi(argv[2]));
    if (argc >= 4 && !strcmp(argv[1], "probe-file")) return probe_file(argv[2], argv[3]);
    int n = argc > 1 ? atoi(argv[1]) : 300;
    const char *self = "/proc/self/exe";
    char self_path[256];
    ssize_t l = readlink(self, self_path, sizeof self_path - 1);
    if (l > 0) { self_path[l] = 0; self = self_path; }
    int fails = 0, f_direct = 0, f_exec = 0, f_sub = 0, f_ren = 0;
    prctl(PR_SET_CHILD_SUBREAPER, 1);
    mkdir("/run/exitrace", 0755);

    for (int i = 0; i < n; i++) {
        char ps[16], path[128], name[32], tmp[128];
        // H1a: direct child reaped, probe via exec'd process (conmon -> cleanup shape).
        pid_t c = fork();
        if (c == 0) _exit(0);
        waitpid(c, NULL, 0);
        snprintf(ps, sizeof ps, "%d", c);
        int b = run_probe(self, "probe-pid", ps, NULL);
        if (b) { f_exec++; if (f_exec <= 3) printf("reaped pid %d visible after exec probe, bits %d\n", c, b); }
        // H1b: probe in-process right after reap, from a forked sibling that does not exec.
        c = fork();
        if (c == 0) _exit(0);
        waitpid(c, NULL, 0);
        pid_t o = fork();
        if (o == 0) _exit(probe_pid(c));
        int s;
        waitpid(o, &s, 0);
        if (WEXITSTATUS(s)) { f_direct++; if (f_direct <= 3) printf("reaped pid %d visible in forked probe, bits %d\n", c, WEXITSTATUS(s)); }
        // H1c: grandchild orphaned to us (subreaper), reaped with waitpid(-1).
        int pp[2], gp[2];
        pipe(pp);
        pipe(gp);
        pid_t mid = fork();
        if (mid == 0) {
            pid_t g = fork();
            if (g == 0) { close(pp[1]); char x; read(pp[0], &x, 1); _exit(0); }
            write(gp[1], &g, sizeof g);
            _exit(0);
        }
        pid_t g;
        read(gp[0], &g, sizeof g);
        close(gp[0]); close(gp[1]);
        waitpid(mid, NULL, 0);
        write(pp[1], "x", 1);
        close(pp[0]); close(pp[1]);
        pid_t r = waitpid(-1, NULL, 0);
        if (r != g) { r = waitpid(g, NULL, 0); }
        snprintf(ps, sizeof ps, "%d", g);
        b = run_probe(self, "probe-pid", ps, NULL);
        if (b) { f_sub++; if (f_sub <= 3) printf("subreaper-reaped pid %d visible, bits %d\n", g, b); }
        // H2: tmp + fsync + rename, then fork+exec a reader.
        snprintf(name, sizeof name, "ex%d", i);
        snprintf(tmp, sizeof tmp, "/run/exitrace/.%s.tmp", name);
        snprintf(path, sizeof path, "/run/exitrace/%s", name);
        int fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC, 0644);
        write(fd, "0\n", 2);
        fsync(fd);
        close(fd);
        rename(tmp, path);
        b = run_probe(self, "probe-file", path, name);
        if (b) { f_ren++; if (f_ren <= 3) printf("renamed %s not visible after exec, bits %d\n", path, b); }
        unlink(path);
    }
    printf("reap-direct-exec-probe: %s (%d/%d)\n", f_exec ? "FAIL" : "PASS", f_exec, n);
    printf("reap-direct-fork-probe: %s (%d/%d)\n", f_direct ? "FAIL" : "PASS", f_direct, n);
    printf("reap-subreaper-exec-probe: %s (%d/%d)\n", f_sub ? "FAIL" : "PASS", f_sub, n);
    printf("rename-then-exec-reader: %s (%d/%d)\n", f_ren ? "FAIL" : "PASS", f_ren, n);
    int f_lock = test_locks(self);
    printf("fcntl-lock-cross-open: %s (%d)\n", f_lock ? "FAIL" : "PASS", f_lock);
    fails = f_exec + f_direct + f_sub + f_ren + f_lock;
    printf(fails ? "exitracetest: FAILED\n" : "exitracetest: ALL PASS\n");
    return fails != 0;
}
