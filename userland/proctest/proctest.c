/*
 * proctest — process accounting, /proc links and process-lifetime ABI, with
 * the results Linux gives. Builds for the host (cc proctest.c -lpthread) and
 * for LeandrOS (scripts/toolchain/<arch>-linux-musl-cc -static).
 *
 *   proctest [-d DIR]...   directories for the rename/unlink cases (default
 *                          /tmp, plus /root when root)
 *
 * accounting:
 *   wait4_rusage         wait4 fills utime/stime/maxrss/minflt/nvcsw for the
 *                        child (and the grandchild it waited for)
 *   getrusage_maxrss     ru_maxrss of the caller grows with what it touched
 *   majflt_cache_hit     faulting a file mapping whose pages are cached is a
 *                        minor fault (majflt unchanged)
 *   vfork_exec_faults    a vfork child that execs starts with its own fault counts
 *   majflt_cold          first touch of a never-read file is a major fault
 *   thread_faults        a thread's own minflt in task/<tid>/stat and
 *                        RUSAGE_THREAD, not its process's
 * /proc:
 *   status_groups        /proc/self/status with 30 supplementary groups ends
 *                        with Threads: (no truncation)          [root]
 *   sysstat_whole        /proc/sysstat reads to its `# overflow` trailer
 *   sysstat_subrows      per-ioctl-request (0xE000|sub) and user-fault (0xfffb)
 *                        census rows of our own thread group grow as we act
 *   lstat_magic          lstat of /proc/self/{exe,cwd,root,fd/0} is a symlink
 *   exec_proc_self_exe   execv("/proc/self/exe") runs this binary again
 *   fd_rename_follow     /proc/self/fd/N of a file and of a directory follow
 *                        a rename; openat on the renamed directory fd works
 *   fd_deleted           readlink of an unlinked file's / removed directory's
 *                        fd ends with " (deleted)"
 *   fd_reopen_other      open("/proc/<child>/fd/N") reopens the CHILD's fd N
 *                        (content read back); EACCES for another non-root
 *                        uid, ENOENT for a missing fd or pid
 * lifetime:
 *   clone3_exit_signal   clone3 exit_signal SIGUSR1: the parent gets SIGUSR1
 *                        and not SIGCHLD; plain waitpid is ECHILD, __WALL
 *                        reaps; exit_signal 0 sends nothing
 *   clone3_set_tid       set_tid picks the child's pid (root), EEXIST when
 *                        taken, EINVAL for a second level, EPERM unprivileged
 *   clone_pidfd_emfile   CLONE_PIDFD with a full table: EMFILE, no child
 *   close_range_unshare  CLOSE_RANGE_UNSHARE from a thread leaves the
 *                        sibling's descriptors open
 *   unshare_files_thread unshare(CLONE_FILES) in a thread: a private table;
 *                        its copy of a pipe end is freed at thread exit
 *   fork_unshared_socket fork from a thread with a private fd table: the
 *                        child holds no socket the thread's table lacks
 *                        (peer sees EOF), and RLIMIT_NOFILE reaches that table
 *   pdeathsig_thread     PR_SET_PDEATHSIG fires when the forking *thread*
 *                        exits, though its process lives on; PR_GET_PDEATHSIG
 *   pdeathsig_process    ... and when the parent process exits
 *   pdeathsig_leader_thread / _chain / _child_thread / _once
 *                        the signal per parent *thread* exit (leader too),
 *                        from any thread of the child, exactly once on the
 *                        last thread's raw exit (real-time signal, counted)
 *   stop_leaderless      SIGSTOP/SIGCONT of a child whose leader pthread_exited:
 *                        WUNTRACED/WCONTINUED + SIGCHLD CLD_STOPPED/CONTINUED
 *   stop_thread_sigchld  a worker thread takes SIGTSTP: the parent (not the
 *                        child) gets CLD_STOPPED
 *   clone_fs_threads     threads share cwd and umask; unshare(CLONE_FS) splits
 *   clone_fs_process     clone(CLONE_FS) without CLONE_VM shares cwd/umask
 *                        with the parent; fork does not
 *
 * Output: "<name>: PASS|FAIL ...|SKIP ..." per case and a summary
 * "proctest: N passed, M failed, K skipped". Exit status = failures.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <limits.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_clone3
#define SYS_clone3 435
#endif
#ifndef SYS_close_range
#define SYS_close_range 436
#endif
#ifndef CLONE_PIDFD
#define CLONE_PIDFD 0x1000
#endif
#ifndef CLOSE_RANGE_UNSHARE
#define CLOSE_RANGE_UNSHARE 2
#endif
#ifndef __WALL
#define __WALL 0x40000000
#endif
#ifndef __WCLONE
#define __WCLONE 0x80000000
#endif

static int npass, nfail, nskip;

static void pass(const char *n) { printf("%s: PASS\n", n); npass++; fflush(stdout); }
static void skip(const char *n, const char *why) { printf("%s: SKIP %s\n", n, why); nskip++; fflush(stdout); }
static void fail(const char *n, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
static void fail(const char *n, const char *fmt, ...) {
    va_list ap;
    printf("%s: FAIL ", n);
    va_start(ap, fmt); vprintf(fmt, ap); va_end(ap);
    printf("\n"); nfail++; fflush(stdout);
}

static long long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static double tv_s(struct timeval t) { return t.tv_sec + t.tv_usec / 1e6; }

/* Wait for `pid` up to `ms`, killing it on timeout. Returns the status, or -1. */
static int wait_for(pid_t pid, int ms, int opts) {
    long long end = now_ms() + ms;
    int st;
    for (;;) {
        pid_t r = waitpid(pid, &st, WNOHANG | opts);
        if (r == pid) return st;
        if (r < 0) return -2;
        if (now_ms() > end) { kill(pid, SIGKILL); waitpid(pid, &st, opts); return -1; }
        usleep(2000);
    }
}

static void burn_ms(int ms) {
    long long end = now_ms() + ms;
    volatile unsigned long x = 0;
    while (now_ms() < end) for (int i = 0; i < 10000; i++) x += i;
    (void)x;
}

static void touch_pages(size_t bytes) {
    char *p = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) return;
#ifdef MADV_NOHUGEPAGE
    madvise(p, bytes, MADV_NOHUGEPAGE); /* one fault per 4 KiB page, as counted below */
#endif
    for (size_t i = 0; i < bytes; i += 4096) p[i] = 1;
    munmap(p, bytes);
}

/* Fields 10 (minflt) and 12 (majflt) of a stat file. */
static int stat_faults(const char *path, unsigned long *minflt, unsigned long *majflt) {
    char buf[1024];
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n <= 0) return -1;
    buf[n] = 0;
    char *p = strrchr(buf, ')');
    if (!p) return -1;
    p += 2; /* state */
    unsigned long v[16];
    int k = 0;
    char *save = NULL;
    for (char *t = strtok_r(p, " ", &save); t && k < 16; t = strtok_r(NULL, " ", &save)) v[k++] = strtoul(t, NULL, 10);
    /* v[0]=state(3) v[1]=ppid ... field f is v[f-3] */
    if (k < 10) return -1;
    *minflt = v[10 - 3];
    *majflt = v[12 - 3];
    return 0;
}

/* ── accounting ──────────────────────────────────────────────────────────── */

static void wait4_rusage(void) {
    const char *n = "wait4_rusage";
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        /* A grandchild that burns CPU too: its time reaches us through the
         * child's wait. */
        pid_t g = fork();
        if (g == 0) { burn_ms(150); _exit(0); }
        burn_ms(200);
        touch_pages(16 << 20);
        for (int i = 0; i < 5; i++) usleep(2000);
        int st;
        waitpid(g, &st, 0);
        _exit(7);
    }
    struct rusage ru;
    memset(&ru, 0xff, sizeof ru);
    int st = 0;
    pid_t r = wait4(pid, &st, 0, &ru);
    if (r != pid || !WIFEXITED(st) || WEXITSTATUS(st) != 7) { fail(n, "wait4 r=%d st=%#x", (int)r, st); return; }
    double cpu = tv_s(ru.ru_utime) + tv_s(ru.ru_stime);
    if (cpu < 0.25 || cpu > 30) { fail(n, "utime+stime %.3f s (want >= 0.25: child 0.2 + grandchild 0.15)", cpu); return; }
    if (ru.ru_maxrss < 16 * 1024) { fail(n, "ru_maxrss %ld KiB (want >= 16384)", ru.ru_maxrss); return; }
    if (ru.ru_minflt < 4096) { fail(n, "ru_minflt %ld (want >= 4096 for 16 MiB touched)", ru.ru_minflt); return; }
    if (ru.ru_nvcsw < 5) { fail(n, "ru_nvcsw %ld (want >= 5 sleeps)", ru.ru_nvcsw); return; }
    if (ru.ru_nivcsw < 0 || ru.ru_majflt < 0) { fail(n, "nivcsw %ld majflt %ld", ru.ru_nivcsw, ru.ru_majflt); return; }
    pass(n);
}

static void getrusage_maxrss(void) {
    const char *n = "getrusage_maxrss";
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        struct rusage a, b;
        getrusage(RUSAGE_SELF, &a);
        touch_pages(32 << 20);
        getrusage(RUSAGE_SELF, &b);
        if (b.ru_maxrss < 32 * 1024) _exit(10);
        if (b.ru_maxrss < a.ru_maxrss) _exit(11);
        _exit(0);
    }
    int st = wait_for(pid, 20000, 0);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass(n);
    else fail(n, "child status %#x (10: maxrss below 32 MiB, 11: it shrank)", st);
    struct rusage c;
    getrusage(RUSAGE_CHILDREN, &c);
    if (c.ru_maxrss < 32 * 1024) fail("getrusage_maxrss_children", "RUSAGE_CHILDREN maxrss %ld KiB (want >= 32768)", c.ru_maxrss);
    else pass("getrusage_maxrss_children");
}

static void majflt_cache_hit(const char *dir) {
    char n[96];
    snprintf(n, sizeof n, "majflt_cache_hit(%s)", dir);
    char path[256];
    snprintf(path, sizeof path, "%s/proctest-maj.%d", dir, (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) { skip(n, strerror(errno)); return; }
    size_t len = 2 << 20;
    char *buf = malloc(len);
    memset(buf, 0x5a, len);
    if (write(fd, buf, len) != (ssize_t)len) { fail(n, "write: %s", strerror(errno)); close(fd); unlink(path); free(buf); return; }
    free(buf);
    fsync(fd);
    /* Pass 1 brings every page into the cache (read(2) does that too). */
    char tmp[65536];
    lseek(fd, 0, SEEK_SET);
    while (read(fd, tmp, sizeof tmp) > 0) {}
    struct rusage a, b;
    unsigned long sum = 0;
    for (int pass_no = 0; pass_no < 2; pass_no++) {
        char *m = mmap(NULL, len, PROT_READ, MAP_PRIVATE, fd, 0);
        if (m == MAP_FAILED) { fail(n, "mmap: %s", strerror(errno)); close(fd); unlink(path); return; }
        getrusage(RUSAGE_SELF, &a);
        for (size_t i = 0; i < len; i += 4096) sum += (unsigned char)m[i];
        getrusage(RUSAGE_SELF, &b);
        munmap(m, len);
    }
    close(fd);
    unlink(path);
    if (sum != 2 * (len / 4096) * 0x5a) { fail(n, "content mismatch"); return; }
    long dmaj = b.ru_majflt - a.ru_majflt, dmin = b.ru_minflt - a.ru_minflt;
    if (dmaj != 0) { fail(n, "second pass over cached pages: %ld major faults (want 0), %ld minor", dmaj, dmin); return; }
    /* No minor-fault floor: a mapping may also be populated eagerly. */
    pass(n);
}

static volatile unsigned long thr_min_self;
static volatile int thr_tid;
static volatile int thr_go_exit;

static void *fault_thread(void *arg) {
    (void)arg;
    thr_tid = (int)syscall(SYS_gettid);
    touch_pages(8 << 20); /* 2048 faults */
    struct rusage r;
    getrusage(RUSAGE_THREAD, &r);
    thr_min_self = (unsigned long)r.ru_minflt;
    while (!thr_go_exit) usleep(1000);
    return NULL;
}

static void thread_faults(void) {
    const char *n = "thread_faults";
    thr_go_exit = 0; thr_tid = 0; thr_min_self = 0;
    pthread_t t;
    if (pthread_create(&t, NULL, fault_thread, NULL)) { fail(n, "pthread_create"); return; }
    while (!thr_min_self) usleep(1000);
    char p[128];
    unsigned long tmin = 0, tmaj = 0, mmin = 0, mmaj = 0, pmin = 0, pmaj = 0;
    snprintf(p, sizeof p, "/proc/self/task/%d/stat", thr_tid);
    int e1 = stat_faults(p, &tmin, &tmaj);
    snprintf(p, sizeof p, "/proc/self/task/%d/stat", (int)getpid());
    int e2 = stat_faults(p, &mmin, &mmaj);
    int e3 = stat_faults("/proc/self/stat", &pmin, &pmaj);
    struct rusage me;
    getrusage(RUSAGE_THREAD, &me);
    thr_go_exit = 1;
    pthread_join(t, NULL);
    if (e1 || e2 || e3) { fail(n, "stat reads %d %d %d", e1, e2, e3); return; }
    if (thr_min_self < 2000) { fail(n, "thread's RUSAGE_THREAD minflt %lu (want >= 2000)", thr_min_self); return; }
    if (tmin < 2000) { fail(n, "task/<tid>/stat minflt %lu (want >= 2000)", tmin); return; }
    if (mmin >= tmin) { fail(n, "main thread's task stat minflt %lu not below the faulting thread's %lu", mmin, tmin); return; }
    if ((unsigned long)me.ru_minflt >= tmin) { fail(n, "main RUSAGE_THREAD minflt %ld not below the thread's %lu", me.ru_minflt, tmin); return; }
    if (pmin < tmin + mmin) { fail(n, "process minflt %lu below thread sum %lu", pmin, tmin + mmin); return; }
    pass(n);
}

/* ── /proc ───────────────────────────────────────────────────────────────── */

static ssize_t read_whole(const char *path, char *buf, size_t cap) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    size_t len = 0;
    for (;;) {
        if (len == cap) { close(fd); return -2; }
        ssize_t r = read(fd, buf + len, cap - len);
        if (r < 0) { close(fd); return -1; }
        if (r == 0) break;
        len += (size_t)r;
    }
    close(fd);
    return (ssize_t)len;
}

static void status_groups(void) {
    const char *n = "status_groups";
    if (geteuid() != 0) { skip(n, "(needs root for setgroups)"); return; }
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        gid_t g[30];
        for (int i = 0; i < 30; i++) g[i] = 1000000000u + (gid_t)i;
        if (setgroups(30, g) != 0) _exit(10);
        static char buf[8192];
        ssize_t len = read_whole("/proc/self/status", buf, sizeof buf - 1);
        if (len <= 0) _exit(11);
        buf[len] = 0;
        char *gl = strstr(buf, "\nGroups:\t");
        if (!gl) _exit(12);
        for (int i = 0; i < 30; i++) {
            char want[16];
            snprintf(want, sizeof want, "%u ", 1000000000u + i);
            if (!strstr(gl, want)) _exit(13);
        }
        if (!strstr(buf, "\nThreads:\t1\n")) _exit(14);
        _exit(0);
    }
    int st = wait_for(pid, 10000, 0);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass(n);
    else fail(n, "child status %#x (13: a group missing, 14: no Threads: line = truncated)", st);
}

/* /sys/devices/system/cpu and /sys/devices/cpu: Firefox's sandbox broker AddTree()s
 * both (stat + S_ISDIR) and its content processes read present/possible/online. */
static void sys_devices_cpu(void) {
    const char *n = "sys_devices_cpu";
    struct stat st;
    static const char *dirs[] = { "/sys/devices/system/cpu", "/sys/devices/cpu", "/sys/devices/system/cpu/cpu0" };
    for (int i = 0; i < 3; i++) {
        if (stat(dirs[i], &st) != 0 || !S_ISDIR(st.st_mode)) { fail(n, "%s not a directory (errno %d)", dirs[i], errno); return; }
    }
    static const char *files[] = { "present", "possible", "online", "kernel_max" };
    char path[128], buf[64];
    long onln = sysconf(_SC_NPROCESSORS_ONLN);
    for (int i = 0; i < 4; i++) {
        snprintf(path, sizeof path, "/sys/devices/system/cpu/%s", files[i]);
        ssize_t r = read_whole(path, buf, sizeof buf - 1);
        if (r <= 0) { fail(n, "%s unreadable (%zd, errno %d)", path, r, errno); return; }
        buf[r] = 0;
        if (i == 3) { if (atol(buf) != onln - 1) { fail(n, "kernel_max '%s' vs %ld cpus", buf, onln); return; } }
        else {
            char want[32];
            snprintf(want, sizeof want, "0-%ld\n", onln - 1);
            if (onln > 1 ? strcmp(buf, want) != 0 : strcmp(buf, "0\n") != 0) { fail(n, "%s = '%s' (%ld cpus)", path, buf, onln); return; }
        }
    }
    DIR *d = opendir("/sys/devices/system/cpu");
    int saw0 = 0;
    struct dirent *e;
    while (d && (e = readdir(d))) if (!strcmp(e->d_name, "cpu0")) saw0 = 1;
    if (d) closedir(d);
    if (!saw0) { fail(n, "cpu0 not listed"); return; }
    pass(n);
}

static void sysstat_whole(void) {
    const char *n = "sysstat_whole";
    size_t cap = 16 << 20;
    char *buf = malloc(cap + 1);
    ssize_t len = read_whole("/proc/sysstat", buf, cap);
    if (len == -1) { free(buf); skip(n, "(no /proc/sysstat: not LeandrOS)"); return; }
    struct stat sst;
    if (access("/proc/sysstat", F_OK) != 0) { fail(n, "access F_OK: %s", strerror(errno)); free(buf); return; }
    if (access("/proc/sysstat", R_OK) != 0) { fail(n, "access R_OK: %s", strerror(errno)); free(buf); return; }
    if (stat("/proc/sysstat", &sst) != 0 || !S_ISREG(sst.st_mode)) { fail(n, "stat: %s mode %o", strerror(errno), sst.st_mode); free(buf); return; }
    if (len < 0) { free(buf); fail(n, "more than 16 MiB"); return; }
    buf[len] = 0;
    /* The trailer is the last line. */
    char *last = len > 1 ? buf + len - 2 : buf;
    while (last > buf && *last != '\n') last--;
    if (last != buf) last++;
    if (strncmp(last, "# overflow ", 11) != 0 || buf[len - 1] != '\n') { fail(n, "%zd bytes, last line not `# overflow N`", len); free(buf); return; }
    /* The census lines of our own thread group are there. */
    char want[32];
    snprintf(want, sizeof want, "\ns %d ", (int)getpid());
    if (!strstr(buf, want)) { fail(n, "no `s %d` line in %zd bytes", (int)getpid(), len); free(buf); return; }
    free(buf);
    pass(n);
}

/* calls of census row `nr` of our own thread group, -1 if unreadable. */
static long long sysstat_calls(unsigned nr) {
    size_t cap = 16 << 20;
    char *buf = malloc(cap + 1);
    ssize_t len = read_whole("/proc/sysstat", buf, cap);
    if (len < 0) { free(buf); return -1; }
    buf[len] = 0;
    char want[48];
    snprintf(want, sizeof want, "\ns %d %u ", (int)getpid(), nr);
    char *at = strstr(buf, want);
    long long c = at ? strtoll(at + strlen(want), NULL, 10) : 0;
    free(buf);
    return c;
}

static void sysstat_subrows(void) {
    const char *n = "sysstat_subrows";
    const unsigned NR_FIONREAD = 0xE000 | (0x541B & 0x7ff), NR_UFAULT = 0xfffb;
    long long i0 = sysstat_calls(NR_FIONREAD), f0 = sysstat_calls(NR_UFAULT);
    if (i0 < 0) { skip(n, "(no /proc/sysstat: not LeandrOS)"); return; }
    int pfd[2];
    if (pipe(pfd) != 0) { fail(n, "pipe: %s", strerror(errno)); return; }
    for (int i = 0; i < 200; i++) { int q = 0; ioctl(pfd[0], FIONREAD, &q); }
    close(pfd[0]); close(pfd[1]);
    touch_pages(64 * 4096);
    long long i1 = sysstat_calls(NR_FIONREAD), f1 = sysstat_calls(NR_UFAULT);
    if (i1 - i0 < 200) { fail(n, "FIONREAD row %u grew %lld, want >= 200", NR_FIONREAD, i1 - i0); return; }
    if (f1 - f0 < 64) { fail(n, "user-fault row grew %lld, want >= 64", f1 - f0); return; }
    pass(n);
}

static void lstat_magic(void) {
    const char *n = "lstat_magic";
    const char *links[] = { "/proc/self/exe", "/proc/self/cwd", "/proc/self/root", "/proc/self/fd/0" };
    for (unsigned i = 0; i < sizeof links / sizeof *links; i++) {
        struct stat st;
        if (lstat(links[i], &st) != 0) { fail(n, "lstat %s: %s", links[i], strerror(errno)); return; }
        if (!S_ISLNK(st.st_mode)) { fail(n, "lstat %s: mode %o, not a symlink", links[i], st.st_mode); return; }
        if (st.st_uid != geteuid()) { fail(n, "lstat %s: uid %d (want %d)", links[i], (int)st.st_uid, (int)geteuid()); return; }
        if (fstatat(AT_FDCWD, links[i], &st, AT_SYMLINK_NOFOLLOW) != 0 || !S_ISLNK(st.st_mode)) {
            fail(n, "fstatat NOFOLLOW %s", links[i]); return;
        }
    }
    struct stat st;
    if (stat("/proc/self/exe", &st) != 0 || !S_ISREG(st.st_mode)) { fail(n, "stat /proc/self/exe does not follow"); return; }
    if (lstat("/proc/999999/exe", &st) == 0 || errno != ENOENT) { fail(n, "lstat of a dead pid's exe: %s (want ENOENT)", strerror(errno)); return; }
    pass(n);
}

static void exec_proc_self_exe(void) {
    const char *n = "exec_proc_self_exe";
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        char *argv[] = { "proctest", "--exec-check", NULL };
        execv("/proc/self/exe", argv);
        _exit(100 + (errno & 0x1f));
    }
    int st = wait_for(pid, 20000, 0);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 42) pass(n);
    else fail(n, "child status %#x (exit 100+errno: execv failed)", st);
}

static int fd_link(int fd, char *out, size_t cap) {
    char p[64];
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    ssize_t r = readlink(p, out, cap - 1);
    if (r < 0) return -1;
    out[r] = 0;
    return 0;
}

static void fd_rename_follow(const char *dir) {
    char n[96];
    snprintf(n, sizeof n, "fd_rename_follow(%s)", dir);
    char d1[256], d2[256], f[256], g[256], got[512], want[512];
    snprintf(d1, sizeof d1, "%s/proctest-d1.%d", dir, (int)getpid());
    snprintf(d2, sizeof d2, "%s/proctest-d2.%d", dir, (int)getpid());
    snprintf(f, sizeof f, "%s/f", d1);
    if (mkdir(d1, 0700) != 0) { skip(n, strerror(errno)); return; }
    int dfd = open(d1, O_RDONLY | O_DIRECTORY);
    int ffd = open(f, O_RDWR | O_CREAT, 0600);
    if (dfd < 0 || ffd < 0) { fail(n, "open: %s", strerror(errno)); goto out; }
    /* Resolve the directory as the kernel names it (a symlinked /tmp). */
    char real1[512];
    if (fd_link(dfd, real1, sizeof real1)) { fail(n, "readlink dir fd"); goto out; }
    if (rename(d1, d2) != 0) { fail(n, "rename dir: %s", strerror(errno)); goto out; }
    char base_dir[512];
    snprintf(base_dir, sizeof base_dir, "%s", real1);
    char *slash = strrchr(base_dir, '/');
    if (!slash) { fail(n, "odd path %s", real1); goto out_r; }
    snprintf(slash + 1, sizeof base_dir - (size_t)(slash + 1 - base_dir), "proctest-d2.%d", (int)getpid());
    if (fd_link(dfd, got, sizeof got) || strcmp(got, base_dir)) { fail(n, "dir fd reads %s (want %s)", got, base_dir); goto out_r; }
    snprintf(want, sizeof want, "%s/f", base_dir);
    if (fd_link(ffd, got, sizeof got) || strcmp(got, want)) { fail(n, "file fd reads %s (want %s)", got, want); goto out_r; }
    int nfd = openat(dfd, "new", O_RDWR | O_CREAT, 0600);
    if (nfd < 0) { fail(n, "openat on the renamed dir fd: %s", strerror(errno)); goto out_r; }
    close(nfd);
    snprintf(g, sizeof g, "%s/new", d2);
    if (access(g, F_OK) != 0) { fail(n, "openat created the file somewhere else"); goto out_r; }
    /* A file renamed within its directory. */
    char f2[256], g2[256];
    snprintf(f2, sizeof f2, "%s/f", d2);
    snprintf(g2, sizeof g2, "%s/g", d2);
    if (rename(f2, g2) != 0) { fail(n, "rename file: %s", strerror(errno)); goto out_r; }
    snprintf(want, sizeof want, "%s/g", base_dir);
    if (fd_link(ffd, got, sizeof got) || strcmp(got, want)) { fail(n, "file fd after its rename reads %s (want %s)", got, want); goto out_r; }
    unlink(g); unlink(g2);
    close(ffd); close(dfd); rmdir(d2);
    pass(n);
    return;
out_r:
    snprintf(g, sizeof g, "%s/new", d2); unlink(g);
    snprintf(g, sizeof g, "%s/f", d2); unlink(g);
    snprintf(g, sizeof g, "%s/g", d2); unlink(g);
    rmdir(d2);
out:
    if (ffd >= 0) close(ffd);
    if (dfd >= 0) close(dfd);
    unlink(f); rmdir(d1);
}

static int ends_with(const char *s, const char *suf) {
    size_t a = strlen(s), b = strlen(suf);
    return a >= b && !strcmp(s + a - b, suf);
}

static void fd_deleted(const char *dir) {
    char n[96];
    snprintf(n, sizeof n, "fd_deleted(%s)", dir);
    char d[256], f[256], got[512], before[512], want[600];
    snprintf(d, sizeof d, "%s/proctest-del.%d", dir, (int)getpid());
    snprintf(f, sizeof f, "%s/f", d);
    if (mkdir(d, 0700) != 0) { skip(n, strerror(errno)); return; }
    int dfd = open(d, O_RDONLY | O_DIRECTORY);
    int ffd = open(f, O_RDWR | O_CREAT, 0600);
    if (dfd < 0 || ffd < 0) { fail(n, "open: %s", strerror(errno)); if (ffd >= 0) close(ffd); if (dfd >= 0) close(dfd); unlink(f); rmdir(d); return; }
    if (write(ffd, "abc", 3) != 3) { fail(n, "write"); }
    fd_link(ffd, before, sizeof before);
    if (unlink(f) != 0) { fail(n, "unlink: %s", strerror(errno)); close(ffd); close(dfd); rmdir(d); return; }
    snprintf(want, sizeof want, "%s (deleted)", before);
    if (fd_link(ffd, got, sizeof got) || strcmp(got, want)) { fail(n, "unlinked file fd reads '%s' (want '%s')", got, want); close(ffd); close(dfd); rmdir(d); return; }
    char b3[4] = {0};
    if (pread(ffd, b3, 3, 0) != 3 || memcmp(b3, "abc", 3)) { fail(n, "unlinked file no longer readable"); }
    close(ffd);
    fd_link(dfd, before, sizeof before);
    if (rmdir(d) != 0) { fail(n, "rmdir: %s", strerror(errno)); close(dfd); return; }
    snprintf(want, sizeof want, "%s (deleted)", before);
    if (fd_link(dfd, got, sizeof got) || strcmp(got, want)) { fail(n, "removed dir fd reads '%s' (want '%s')", got, want); close(dfd); return; }
    errno = 0;
    int x = openat(dfd, "x", O_RDWR | O_CREAT, 0600);
    if (x >= 0 || errno != ENOENT) { fail(n, "openat O_CREAT in a removed dir: %d %s (want ENOENT)", x, strerror(errno)); if (x >= 0) close(x); close(dfd); return; }
    close(dfd);
    (void)ends_with;
    pass(n);
}

/* ── lifetime ────────────────────────────────────────────────────────────── */

struct clone_args_v2 {
    uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup;
};

static long clone3_fork(struct clone_args_v2 *a) {
    return syscall(SYS_clone3, a, sizeof *a);
}

static volatile sig_atomic_t got_usr1, got_chld;
static void on_usr1(int s) { (void)s; got_usr1++; }
static void on_chld(int s) { (void)s; got_chld++; }

static void clone3_exit_signal(void) {
    const char *n = "clone3_exit_signal";
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1; sigaction(SIGUSR1, &sa, NULL);
    sa.sa_handler = on_chld; sigaction(SIGCHLD, &sa, NULL);
    got_usr1 = got_chld = 0;
    struct clone_args_v2 a;
    memset(&a, 0, sizeof a);
    a.exit_signal = SIGUSR1;
    fflush(stdout);
    long pid = clone3_fork(&a);
    if (pid < 0) { fail(n, "clone3: %s", strerror(errno)); goto out; }
    if (pid == 0) { usleep(20000); _exit(3); }
    for (int i = 0; i < 2000 && !got_usr1; i++) usleep(1000);
    usleep(20000);
    int st;
    errno = 0;
    pid_t r = waitpid((pid_t)pid, &st, WNOHANG);
    if (!(r < 0 && errno == ECHILD)) { fail(n, "waitpid without __WCLONE: %d %s (want ECHILD)", (int)r, strerror(errno)); wait_for((pid_t)pid, 5000, __WALL); goto out; }
    st = wait_for((pid_t)pid, 5000, __WALL);
    if (st < 0 || !WIFEXITED(st) || WEXITSTATUS(st) != 3) { fail(n, "waitpid __WALL status %#x", st); goto out; }
    if (got_usr1 != 1 || got_chld != 0) { fail(n, "signals: SIGUSR1 %d SIGCHLD %d (want 1, 0)", (int)got_usr1, (int)got_chld); goto out; }
    /* exit_signal 0: nothing at all; __WCLONE waits for it. */
    a.exit_signal = 0;
    got_usr1 = got_chld = 0;
    pid = clone3_fork(&a);
    if (pid < 0) { fail(n, "clone3 sig 0: %s", strerror(errno)); goto out; }
    if (pid == 0) _exit(4);
    st = wait_for((pid_t)pid, 5000, __WCLONE);
    if (st < 0 || !WIFEXITED(st) || WEXITSTATUS(st) != 4) { fail(n, "exit_signal 0 child: __WCLONE status %#x", st); goto out; }
    usleep(20000);
    if (got_usr1 || got_chld) { fail(n, "exit_signal 0 sent SIGUSR1 %d SIGCHLD %d", (int)got_usr1, (int)got_chld); goto out; }
    /* A SIGCHLD child is not a __WCLONE child. */
    a.exit_signal = SIGCHLD;
    pid = clone3_fork(&a);
    if (pid == 0) _exit(5);
    usleep(30000);
    errno = 0;
    r = waitpid((pid_t)pid, &st, WNOHANG | __WCLONE);
    if (!(r < 0 && errno == ECHILD)) { fail(n, "__WCLONE on a SIGCHLD child: %d %s (want ECHILD)", (int)r, strerror(errno)); wait_for((pid_t)pid, 5000, 0); goto out; }
    st = wait_for((pid_t)pid, 5000, 0);
    if (st < 0 || !WIFEXITED(st) || WEXITSTATUS(st) != 5) { fail(n, "SIGCHLD child status %#x", st); goto out; }
    pass(n);
out:
    signal(SIGUSR1, SIG_DFL);
    signal(SIGCHLD, SIG_DFL);
}

static void clone3_set_tid(void) {
    const char *n = "clone3_set_tid";
    struct clone_args_v2 a;
    memset(&a, 0, sizeof a);
    a.exit_signal = SIGCHLD;
    pid_t want = 0;
    /* A free pid well above the current ones. */
    for (pid_t p = getpid() + 7919; p < getpid() + 7919 + 1000; p++) {
        if (kill(p, 0) != 0 && errno == ESRCH) { want = p; break; }
    }
    pid_t tid[2] = { want, 1 };
    a.set_tid = (uint64_t)(uintptr_t)tid;
    a.set_tid_size = 1;
    fflush(stdout);
    if (geteuid() != 0) {
        errno = 0;
        long r = clone3_fork(&a);
        if (r == 0) _exit(0);
        if (r > 0) { waitpid((pid_t)r, NULL, 0); fail(n, "unprivileged set_tid succeeded"); return; }
        if (errno != EPERM) { fail(n, "unprivileged: %s (want EPERM)", strerror(errno)); return; }
        pass(n);
        return;
    }
    long r = clone3_fork(&a);
    if (r == 0) _exit(getpid() == want ? 0 : 1);
    if (r < 0) { fail(n, "set_tid %d: %s", (int)want, strerror(errno)); return; }
    int st = wait_for((pid_t)r, 5000, 0);
    if (r != want || st < 0 || !WIFEXITED(st) || WEXITSTATUS(st) != 0) { fail(n, "got pid %ld (want %d), status %#x", r, (int)want, st); return; }
    tid[0] = getpid();
    errno = 0;
    r = clone3_fork(&a);
    if (r == 0) _exit(0);
    if (r > 0) { waitpid((pid_t)r, NULL, 0); fail(n, "set_tid of a live pid succeeded"); return; }
    if (errno != EEXIST) { fail(n, "set_tid of a live pid: %s (want EEXIST)", strerror(errno)); return; }
    tid[0] = want; a.set_tid_size = 2;
    errno = 0;
    r = clone3_fork(&a);
    if (r == 0) _exit(0);
    if (r > 0) { waitpid((pid_t)r, NULL, 0); fail(n, "two-level set_tid succeeded"); return; }
    if (errno != EINVAL) { fail(n, "two-level set_tid: %s (want EINVAL)", strerror(errno)); return; }
    pass(n);
}

static void clone_pidfd_emfile(void) {
    const char *n = "clone_pidfd_emfile";
    fflush(stdout);
    pid_t outer = fork();
    if (outer == 0) {
        struct rlimit rl = { 64, 64 };
        if (setrlimit(RLIMIT_NOFILE, &rl) != 0) _exit(10);
        int last = -1;
        for (;;) { int fd = open("/dev/null", O_RDONLY); if (fd < 0) break; last = fd; }
        if (errno != EMFILE || last < 0) _exit(11);
        struct clone_args_v2 a;
        memset(&a, 0, sizeof a);
        int pfd = -1;
        a.flags = CLONE_PIDFD;
        a.pidfd = (uint64_t)(uintptr_t)&pfd;
        a.exit_signal = SIGCHLD;
        errno = 0;
        long r = clone3_fork(&a);
        if (r == 0) _exit(0);
        if (r > 0) { waitpid((pid_t)r, NULL, 0); _exit(12); }
        if (errno != EMFILE) _exit(13);
        errno = 0;
        if (waitpid(-1, NULL, WNOHANG) != -1 || errno != ECHILD) _exit(14);
        _exit(0);
    }
    int st = wait_for(outer, 10000, 0);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass(n);
    else fail(n, "status %#x (12: child created, 13: not EMFILE, 14: a child exists)", st);
}

static volatile int cru_fd = -1, cru_done;
static void *cru_thread(void *arg) {
    (void)arg;
    cru_done = (int)syscall(SYS_close_range, cru_fd, cru_fd, CLOSE_RANGE_UNSHARE) == 0 ? 1 : -1;
    /* Closed for this thread. */
    if (fcntl(cru_fd, F_GETFD) != -1 || errno != EBADF) cru_done = -2;
    return NULL;
}

static void close_range_unshare(void) {
    const char *n = "close_range_unshare";
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        cru_fd = open("/dev/null", O_RDONLY);
        pthread_t t;
        pthread_create(&t, NULL, cru_thread, NULL);
        pthread_join(t, NULL);
        if (cru_done != 1) _exit(10 - cru_done);
        if (fcntl(cru_fd, F_GETFD) == -1) _exit(20);
        _exit(0);
    }
    int st = wait_for(pid, 10000, 0);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass(n);
    else fail(n, "status %#x (20: the sibling's fd was closed too)", st);
}

static int ufs_p[2];
static volatile int ufs_res;
static void *ufs_thread(void *arg) {
    (void)arg;
    if (unshare(CLONE_FILES) != 0) { ufs_res = -1; return NULL; }
    close(ufs_p[0]);
    if (fcntl(ufs_p[0], F_GETFD) != -1 || errno != EBADF) { ufs_res = -2; return NULL; }
    /* Hold the private copy of the write end while main closes its own. */
    usleep(50000);
    ufs_res = 1;
    return NULL;
}

static void unshare_files_thread(void) {
    const char *n = "unshare_files_thread";
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        /* Alone: a no-op. */
        if (unshare(CLONE_FILES) != 0) _exit(30);
        if (pipe(ufs_p) != 0) _exit(31);
        pthread_t t;
        if (pthread_create(&t, NULL, ufs_thread, NULL)) _exit(32);
        usleep(10000);
        close(ufs_p[1]);
        /* The thread's private table still holds a write end or is about to
         * be freed; main's read end must be open regardless. */
        if (fcntl(ufs_p[0], F_GETFD) == -1) _exit(33);
        pthread_join(t, NULL);
        if (ufs_res != 1) _exit(10 - ufs_res);
        /* The thread exited: its private table and write end are gone. */
        alarm(5);
        char c;
        if (read(ufs_p[0], &c, 1) != 0) _exit(34);
        _exit(0);
    }
    int st = wait_for(pid, 10000, 0);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass(n);
    else fail(n, "status %#x (34: no EOF after the thread exited, 33: main lost fd)", st);
}


static int fus_sv[2];
static volatile int fus_step, fus_res, fus_child;
static void *fus_thread(void *arg) {
    (void)arg;
    if (unshare(CLONE_FILES) != 0) { fus_res = -1; return NULL; }
    close(fus_sv[0]);               /* only the process table keeps it now */
    pid_t c = fork();
    if (c == 0) { usleep(1500000); _exit(0); }
    fus_child = c;
    fus_step = 1;
    while (fus_step < 2) usleep(2000);
    /* main lowered RLIMIT_NOFILE: this private table must obey it. */
    if (dup2(fus_sv[1], 40) != -1 || errno != EBADF) { fus_res = -2; return NULL; }
    fus_res = 1;
    return NULL;
}

static void fork_unshared_socket(void) {
    const char *n = "fork_unshared_socket";
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, fus_sv) != 0) _exit(30);
        pthread_t t;
        if (pthread_create(&t, NULL, fus_thread, NULL)) _exit(31);
        while (fus_step < 1) usleep(2000);
        close(fus_sv[0]);           /* the process table's last reference */
        struct timeval tv = { 0, 600000 };
        setsockopt(fus_sv[1], SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
        char c;
        ssize_t r = read(fus_sv[1], &c, 1);   /* EOF, with the child alive */
        if (r != 0) _exit(32);
        struct rlimit rl = { 30, 30 };
        if (setrlimit(RLIMIT_NOFILE, &rl) != 0) _exit(33);
        fus_step = 2;
        pthread_join(t, NULL);
        if (fus_res != 1) _exit(10 - fus_res);
        kill(fus_child, SIGKILL);
        waitpid(fus_child, NULL, 0);
        _exit(0);
    }
    int st = wait_for(pid, 15000, 0);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass(n);
    else fail(n, "status %#x (32: peer saw no EOF - child holds the socket, 12: RLIMIT_NOFILE did not reach the thread's table)", st);
}

static volatile sig_atomic_t pdeath_got;
static void on_pdeath(int s) { (void)s; pdeath_got = 1; }

static void *pdeath_forker(void *arg) {
    int *pfd = arg;
    pid_t c = fork();
    if (c == 0) {
        struct sigaction sa;
        memset(&sa, 0, sizeof sa);
        sa.sa_handler = on_pdeath;
        sigaction(SIGUSR2, &sa, NULL);
        if (prctl(PR_SET_PDEATHSIG, SIGUSR2) != 0) _exit(10);
        int v = 0;
        if (prctl(PR_GET_PDEATHSIG, &v) != 0 || v != SIGUSR2) _exit(11);
        if (write(pfd[1], "r", 1) != 1) _exit(12);
        for (int i = 0; i < 3000 && !pdeath_got; i++) usleep(1000);
        _exit(pdeath_got ? 0 : 13);
    }
    char b;
    if (read(pfd[0], &b, 1) != 1) return (void *)(intptr_t)-1;
    return (void *)(intptr_t)c; /* the thread exits right after the child is armed */
}

static void pdeathsig_thread(void) {
    const char *n = "pdeathsig_thread";
    int pfd[2];
    if (pipe(pfd)) { fail(n, "pipe"); return; }
    fflush(stdout);
    pthread_t t;
    void *ret;
    pthread_create(&t, NULL, pdeath_forker, pfd);
    pthread_join(t, &ret);
    pid_t c = (pid_t)(intptr_t)ret;
    close(pfd[0]); close(pfd[1]);
    if (c <= 0) { fail(n, "fork in thread"); return; }
    int st = wait_for(c, 10000, 0);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass(n);
    else fail(n, "child status %#x (13: no signal when the forking thread exited; -2: not waitable by the process)", st);
}

static void pdeathsig_process(void) {
    const char *n = "pdeathsig_process";
    int pfd[2];
    if (pipe(pfd)) { fail(n, "pipe"); return; }
    fflush(stdout);
    pid_t mid = fork();
    if (mid == 0) {
        pid_t c = fork();
        if (c == 0) {
            close(pfd[0]);
            pid_t orig = getppid();
            struct sigaction sa;
            memset(&sa, 0, sizeof sa);
            sa.sa_handler = on_pdeath;
            sigaction(SIGUSR2, &sa, NULL);
            prctl(PR_SET_PDEATHSIG, SIGUSR2);
            if (getppid() != orig) _exit(1);
            kill(orig, SIGTERM);
            for (int i = 0; i < 3000 && !pdeath_got; i++) usleep(1000);
            char r = pdeath_got ? 'y' : 'n';
            if (write(pfd[1], &r, 1) != 1) _exit(2);
            _exit(0);
        }
        for (;;) pause();
    }
    close(pfd[1]);
    char r = 0;
    struct timeval tv = { 5, 0 };
    fd_set rf; FD_ZERO(&rf); FD_SET(pfd[0], &rf);
    if (select(pfd[0] + 1, &rf, NULL, NULL, &tv) == 1) { if (read(pfd[0], &r, 1) != 1) r = 0; }
    close(pfd[0]);
    wait_for(mid, 2000, 0);
    if (r == 'y') pass(n);
    else fail(n, "grandchild reported '%c' (n: no PDEATHSIG when its parent process died)", r ? r : '?');
}

static char cfs_buf[512];
static volatile int cfs_step;

static void *cfs_thread(void *arg) {
    const char *dir = arg;
    if (chdir(dir) != 0) { cfs_step = -1; return NULL; }
    umask(0027);
    cfs_step = 1;
    while (cfs_step != 2) usleep(1000);
    if (unshare(CLONE_FS) != 0) { cfs_step = -2; return NULL; }
    if (chdir("/") != 0) { cfs_step = -3; return NULL; }
    umask(0077);
    cfs_step = 3;
    return NULL;
}

static void clone_fs_threads(const char *dir) {
    const char *n = "clone_fs_threads";
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        if (chdir("/") != 0) _exit(9);
        umask(022);
        pthread_t t;
        pthread_create(&t, NULL, cfs_thread, (void *)dir);
        while (cfs_step == 0) usleep(1000);
        if (cfs_step < 0) _exit(10);
        char want[512];
        if (!getcwd(cfs_buf, sizeof cfs_buf)) _exit(11);
        int fd = open(dir, O_RDONLY | O_DIRECTORY);
        if (fd < 0) _exit(12);
        char p[64]; snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
        ssize_t l = readlink(p, want, sizeof want - 1);
        if (l < 0) _exit(13);
        want[l] = 0; close(fd);
        if (strcmp(cfs_buf, want)) _exit(14);           /* the thread's chdir is ours */
        mode_t m = umask(022); umask(m);
        if (m != 0027) _exit(15);                       /* and its umask */
        cfs_step = 2;
        pthread_join(t, NULL);
        if (cfs_step != 3) _exit(20 - cfs_step);
        if (!getcwd(cfs_buf, sizeof cfs_buf) || strcmp(cfs_buf, want)) _exit(16); /* split: still dir */
        m = umask(022); umask(m);
        if (m != 0027) _exit(17);
        _exit(0);
    }
    int st = wait_for(pid, 10000, 0);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass(n);
    else fail(n, "status %#x (14: cwd not shared, 15: umask not shared, 16/17: unshare did not split)", st);
}

static int cfs_child(void *arg) {
    const char *dir = arg;
    if (chdir(dir) != 0) return 1;
    umask(0037);
    return 0;
}

static void clone_fs_process(const char *dir) {
    const char *n = "clone_fs_process";
    fflush(stdout);
    pid_t outer = fork();
    if (outer == 0) {
        if (chdir("/") != 0) _exit(9);
        umask(022);
        size_t ssz = 64 << 10;
        char *stk = mmap(NULL, ssz, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        pid_t c = clone(cfs_child, stk + ssz, CLONE_FS | SIGCHLD, (void *)dir);
        if (c < 0) _exit(10);
        int st;
        if (waitpid(c, &st, 0) != c || !WIFEXITED(st) || WEXITSTATUS(st)) _exit(11);
        if (!getcwd(cfs_buf, sizeof cfs_buf) || !strcmp(cfs_buf, "/")) _exit(12);
        mode_t m = umask(022); umask(m);
        if (m != 0037) _exit(13);
        /* fork does not share. */
        if (chdir("/") != 0) _exit(14);
        pid_t f = fork();
        if (f == 0) { if (chdir(dir)) _exit(1); umask(0); _exit(0); }
        waitpid(f, &st, 0);
        if (!getcwd(cfs_buf, sizeof cfs_buf) || strcmp(cfs_buf, "/")) _exit(15);
        m = umask(022); umask(m);
        if (m != 0037) _exit(16);
        _exit(0);
    }
    int st = wait_for(outer, 10000, 0);
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) pass(n);
    else fail(n, "status %#x (12: CLONE_FS child's chdir not seen, 13: its umask, 15/16: fork shared)", st);
}

/* ---- PR_SET_PDEATHSIG across threads (L10.6a) ---------------------------
 * Linux sends the signal once per exit of the *parent thread* (also a
 * leader that pthread_exits), reads it from every thread of the child, and
 * sends it once when the parent's last thread leaves. A real-time signal
 * queues, so the deliveries can be counted. The child C reports
 * {got, getppid()} over a pipe every `step` ms; the outer process G reads. */
struct pdrec { int got; int ppid; };
static volatile sig_atomic_t pd_got;
static void on_pd_rt(int s) { (void)s; pd_got++; }

static void pd_child(int wfd, int sig, int arm_here, int nrec, int step_ms) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_pd_rt;
    sigaction(sig, &sa, NULL);
    if (arm_here && prctl(PR_SET_PDEATHSIG, sig) != 0) _exit(10);
    for (int i = 0; i < nrec; i++) {
        usleep(step_ms * 1000);
        struct pdrec r = { pd_got, getppid() };
        if (write(wfd, &r, sizeof r) != sizeof r) _exit(11);
    }
    _exit(0);
}

/* Read up to `max` records; returns how many arrived within 6 s. */
static int pd_read(int rfd, struct pdrec *out, int max) {
    int n = 0;
    while (n < max) {
        struct timeval tv = { 6, 0 };
        fd_set rf; FD_ZERO(&rf); FD_SET(rfd, &rf);
        if (select(rfd + 1, &rf, NULL, NULL, &tv) != 1) break;
        if (read(rfd, &out[n], sizeof out[n]) != sizeof out[n]) break;
        n++;
    }
    return n;
}

static int pd_wfd;
static pid_t pd_c;

/* leader_thread: P forks C from its main thread, then main pthread_exits. */
static void *pd_lt_worker(void *a) {
    (void)a;
    usleep(150000);
    pid_t r = waitpid(pd_c, NULL, 0);
    _exit(r == pd_c ? 0 : 1);
}
static void pdeathsig_leader_thread(void) {
    const char *n = "pdeathsig_leader_thread";
    int pfd[2];
    if (pipe(pfd)) { fail(n, "pipe"); return; }
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        close(pfd[0]);
        pd_c = fork();
        if (pd_c == 0) pd_child(pfd[1], SIGRTMIN + 1, 1, 8, 100);
        close(pfd[1]);
        usleep(30000); /* let C arm before the leader goes */
        pthread_t t;
        pthread_create(&t, NULL, pd_lt_worker, NULL);
        pthread_exit(NULL);
    }
    close(pfd[1]);
    struct pdrec r[8];
    int nr = pd_read(pfd[0], r, 8);
    close(pfd[0]);
    int st = wait_for(p, 10000, 0);
    if (nr < 8) { fail(n, "only %d reports", nr); return; }
    for (int i = 0; i < 8; i++)
        if (r[i].ppid != p) { fail(n, "rec %d getppid %d != %d", i, r[i].ppid, p); return; }
    if (r[7].got != 1) { fail(n, "got %d (want 1: the leader's pthread_exit)", r[7].got); return; }
    if (st < 0 || !WIFEXITED(st) || WEXITSTATUS(st) != 0) { fail(n, "worker waitpid(C) status %#x", st); return; }
    pass(n);
}

/* chain: W1 forks C and exits, then the leader pthread_exits, then W2 and
 * with it the process leave: three parent-thread exits, three signals. */
static void *pd_ch_w1(void *a) {
    (void)a;
    pd_c = fork();
    if (pd_c == 0) pd_child(pd_wfd, SIGRTMIN + 1, 1, 10, 50);
    usleep(50000);
    return NULL;
}
static void *pd_ch_w2(void *a) { (void)a; usleep(250000); _exit(0); }
static void pdeathsig_chain(void) {
    const char *n = "pdeathsig_chain";
    int pfd[2];
    if (pipe(pfd)) { fail(n, "pipe"); return; }
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        close(pfd[0]);
        pd_wfd = pfd[1];
        pthread_t t1, t2;
        pthread_create(&t2, NULL, pd_ch_w2, NULL);
        pthread_create(&t1, NULL, pd_ch_w1, NULL);
        usleep(120000);
        pthread_exit(NULL);
    }
    close(pfd[1]);
    struct pdrec r[10];
    int nr = pd_read(pfd[0], r, 10);
    close(pfd[0]);
    wait_for(p, 10000, 0);
    if (nr < 10) { fail(n, "only %d reports", nr); return; }
    for (int i = 0; i < 4; i++)
        if (r[i].ppid != p) { fail(n, "rec %d getppid %d != %d while the parent lives", i, r[i].ppid, p); return; }
    if (r[3].got != 2 || r[9].got != 3) { fail(n, "got %d at 200 ms, %d at the end (want 2, 3)", r[3].got, r[9].got); return; }
    pass(n);
}

/* child_thread: only a *worker thread* of C asked for the signal. */
static int pd_ready_fd;
static void *pd_ct_arm(void *a) {
    (void)a;
    prctl(PR_SET_PDEATHSIG, SIGRTMIN + 2);
    char b = 'r';
    if (write(pd_ready_fd, &b, 1) != 1) _exit(12);
    for (;;) pause();
}
static void pdeathsig_child_thread(void) {
    const char *n = "pdeathsig_child_thread";
    int rp[2], pfd[2];
    if (pipe(rp) || pipe(pfd)) { fail(n, "pipe"); return; }
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        close(pfd[0]);
        pid_t c = fork();
        if (c == 0) {
            close(rp[0]);
            pd_ready_fd = rp[1];
            struct sigaction sa;
            memset(&sa, 0, sizeof sa);
            sa.sa_handler = on_pd_rt;
            sigaction(SIGRTMIN + 2, &sa, NULL);
            pthread_t t;
            pthread_create(&t, NULL, pd_ct_arm, NULL);
            pd_child(pfd[1], SIGRTMIN + 2, 0, 4, 100);
        }
        close(rp[1]);
        char b;
        if (read(rp[0], &b, 1) != 1) _exit(1);
        usleep(50000);
        _exit(0);
    }
    close(pfd[1]); close(rp[0]); close(rp[1]);
    struct pdrec r[4];
    int nr = pd_read(pfd[0], r, 4);
    close(pfd[0]);
    wait_for(p, 10000, 0);
    if (nr < 4) { fail(n, "only %d reports", nr); return; }
    if (r[3].got != 1) { fail(n, "got %d (want 1: the worker thread's setting must fire)", r[3].got); return; }
    pass(n);
}

/* once: the forking thread is the group's last thread and leaves with a raw
 * exit: the signal arrives once, not once for the thread and once more for
 * the process. */
static void *pd_once_worker(void *a) {
    (void)a;
    usleep(50000);
    pd_c = fork();
    if (pd_c == 0) pd_child(pd_wfd, SIGRTMIN + 3, 1, 4, 100);
    usleep(30000); /* let the child arm before its parent thread goes */
    syscall(SYS_exit, 0);
    return NULL;
}
static void pdeathsig_once(void) {
    const char *n = "pdeathsig_once";
    int pfd[2];
    if (pipe(pfd)) { fail(n, "pipe"); return; }
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        close(pfd[0]);
        pd_wfd = pfd[1];
        pthread_t t;
        pthread_create(&t, NULL, pd_once_worker, NULL);
        pthread_exit(NULL);
    }
    close(pfd[1]);
    struct pdrec r[4];
    int nr = pd_read(pfd[0], r, 4);
    close(pfd[0]);
    wait_for(p, 10000, 0);
    if (nr < 4) { fail(n, "only %d reports", nr); return; }
    if (r[3].got != 1) { fail(n, "got %d (want exactly 1)", r[3].got); return; }
    pass(n);
}

/* ---- stop/continue reports of a child whose leader pthread_exited (L5.12a) */
static volatile sig_atomic_t chld_code;
static void on_chld_info(int s, siginfo_t *si, void *u) { (void)s; (void)u; chld_code = si->si_code; }

static void *sl_pauser(void *a) {
    int *pfd = a;
    pid_t tid = syscall(SYS_gettid);
    if (pfd[1] >= 0 && write(pfd[1], &tid, sizeof tid) != sizeof tid) _exit(12);
    sigset_t m; sigemptyset(&m); sigaddset(&m, SIGTSTP);
    pthread_sigmask(SIG_UNBLOCK, &m, NULL); /* only this thread can take SIGTSTP */
    for (;;) pause();
}

/* Poll waitpid(c, opts|WNOHANG) up to 1 s. */
static int sl_poll(pid_t c, int *st, int opts) {
    long long end = now_ms() + 1000;
    for (;;) {
        pid_t r = waitpid(c, st, opts | WNOHANG);
        if (r != 0) return r;
        if (now_ms() > end) return 0;
        usleep(2000);
    }
}

/* The SIGCHLD may land a moment after the wait status is visible (cross-CPU delivery). */
static void sl_chld_wait(void) {
    for (int i = 0; i < 100 && !chld_code; i++) usleep(2000);
}

static void stop_child_run(const char *n, int leaderless) {
    int pfd[2];
    if (pipe(pfd)) { fail(n, "pipe"); return; }
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        setpgid(0, 0); /* an orphaned group would discard SIGTSTP */
        close(pfd[0]);
        sigset_t m; sigemptyset(&m); sigaddset(&m, SIGTSTP);
        pthread_sigmask(SIG_BLOCK, &m, NULL);
        pthread_t t;
        static int targ[2];
        targ[0] = 0; targ[1] = leaderless ? -1 : pfd[1];
        pthread_create(&t, NULL, sl_pauser, targ);
        if (leaderless) pthread_exit(NULL);
        for (;;) pause();
    }
    close(pfd[1]);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_chld_info;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigaction(SIGCHLD, &sa, NULL);
    usleep(80000);
    int stopsig = SIGSTOP;
    chld_code = 0;
    if (leaderless) {
        kill(c, SIGSTOP);
    } else {
        pid_t tid = 0;
        if (read(pfd[0], &tid, sizeof tid) != sizeof tid) { fail(n, "no tid"); kill(c, SIGKILL); waitpid(c, NULL, 0); return; }
        stopsig = SIGTSTP;
        syscall(SYS_tgkill, c, tid, SIGTSTP);
    }
    close(pfd[0]);
    int st = 0;
    const char *err = NULL;
    char buf[96] = "";
    if (sl_poll(c, &st, WUNTRACED) != c) err = "stop not reported";
    else if (!WIFSTOPPED(st) || WSTOPSIG(st) != stopsig) { snprintf(buf, sizeof buf, "status %#x", st); err = buf; }
    else if (sl_chld_wait(), chld_code != CLD_STOPPED) { snprintf(buf, sizeof buf, "SIGCHLD si_code %d, not CLD_STOPPED", (int)chld_code); err = buf; }
    else if (waitpid(c, &st, WUNTRACED | WNOHANG) != 0) err = "stop reported twice";
    else {
        chld_code = 0;
        kill(c, SIGCONT);
        if (sl_poll(c, &st, WCONTINUED) != c || !WIFCONTINUED(st)) err = "continue not reported";
        else if (sl_chld_wait(), chld_code != CLD_CONTINUED) { snprintf(buf, sizeof buf, "SIGCHLD si_code %d, not CLD_CONTINUED", (int)chld_code); err = buf; }
    }
    kill(c, SIGKILL);
    int fst = 0;
    waitpid(c, &fst, 0);
    signal(SIGCHLD, SIG_DFL);
    if (err) fail(n, "%s", err);
    else if (!WIFSIGNALED(fst) || WTERMSIG(fst) != SIGKILL) fail(n, "final status %#x", fst);
    else pass(n);
}
static void stop_leaderless(void) { stop_child_run("stop_leaderless", 1); }
static void stop_thread_sigchld(void) { stop_child_run("stop_thread_sigchld", 0); }

static void fd_reopen_other(void) {
    const char *n = "fd_reopen_other";
    int rp[2], ap[2];
    if (pipe(rp) || pipe(ap)) { fail(n, "pipe"); return; }
    pid_t c = fork();
    if (c == 0) {
        close(ap[1]); close(rp[0]);   /* so the parent's close(ap[1]) is the EOF */
        if (geteuid() == 0) { setgid(1000); setuid(1000); }
        int fd = syscall(SYS_memfd_create, "reopen", 0);
        if (fd < 0 || write(fd, "child-secret", 12) != 12) _exit(3);
        unsigned char b = (unsigned char)fd;
        if (write(rp[1], &b, 1) != 1) _exit(4);
        char x; (void)!read(ap[0], &x, 1);   /* pause until the parent is done */
        _exit(0);
    }
    unsigned char b = 0;
    if (read(rp[0], &b, 1) != 1) { fail(n, "child setup failed"); goto out; }
    char p[64], buf[32] = {0};
    snprintf(p, sizeof p, "/proc/%d/fd/%d", (int)c, (int)b);
    int fd = open(p, O_RDONLY);
    if (fd < 0) { fail(n, "open(%s): %s", p, strerror(errno)); goto out; }
    ssize_t r = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (r != 12 || memcmp(buf, "child-secret", 12)) { fail(n, "read %zd bytes '%s' (want child-secret)", r, buf); goto out; }
    snprintf(p, sizeof p, "/proc/%d/fd/%d", (int)c, 900);
    if (open(p, O_RDONLY) >= 0 || errno != ENOENT) { fail(n, "missing fd: errno %d (want ENOENT)", errno); goto out; }
    snprintf(p, sizeof p, "/proc/%d/fd/%d", 999999, (int)b);
    if (open(p, O_RDONLY) >= 0 || errno != ENOENT) { fail(n, "missing pid: errno %d (want ENOENT)", errno); goto out; }
    if (geteuid() == 0) {
        pid_t o = fork();
        if (o == 0) {
            setgid(2000); setuid(2000);
            snprintf(p, sizeof p, "/proc/%d/fd/%d", (int)c, (int)b);
            int f = open(p, O_RDONLY);
            _exit(f < 0 && errno == EACCES ? 0 : 1);
        }
        int st = 0; waitpid(o, &st, 0);
        if (!WIFEXITED(st) || WEXITSTATUS(st)) { fail(n, "other uid did not get EACCES"); goto out; }
    }
    pass(n);
out:
    close(ap[1]);
    waitpid(c, NULL, 0);
}

/* open("/proc/self/fd/N") of a pipe end is a new description on the same
 * pipe; the end follows the access mode, whichever end N was. */
static void fd_reopen_pipe(void) {
    const char *n = "fd_reopen_pipe";
    int pp[2];
    if (pipe(pp)) { fail(n, "pipe"); return; }
    char p[64], buf[16] = {0};
    snprintf(p, sizeof p, "/proc/self/fd/%d", pp[1]);
    int rd = open(p, O_RDONLY);
    if (rd < 0) { fail(n, "open(%s, O_RDONLY): %s", p, strerror(errno)); goto out; }
    if (write(pp[1], "hello", 5) != 5) { fail(n, "write"); close(rd); goto out; }
    ssize_t r = read(rd, buf, sizeof buf - 1);
    close(rd);
    if (r != 5 || memcmp(buf, "hello", 5)) { fail(n, "read %zd '%s' (want hello)", r, buf); goto out; }
    snprintf(p, sizeof p, "/proc/self/fd/%d", pp[0]);
    int wr = open(p, O_WRONLY);
    if (wr < 0) { fail(n, "open(%s, O_WRONLY): %s", p, strerror(errno)); goto out; }
    if (write(wr, "back", 4) != 4) { fail(n, "write via reopened write end"); close(wr); goto out; }
    close(wr);
    memset(buf, 0, sizeof buf);
    r = read(pp[0], buf, sizeof buf - 1);
    if (r != 4 || memcmp(buf, "back", 4)) { fail(n, "read %zd '%s' (want back)", r, buf); goto out; }
    pass(n);
out:
    close(pp[0]); close(pp[1]);
}

/* A file on a mounted filesystem is reopened by path with a fresh offset. */
static void fd_reopen_mounted(void) {
    const char *n = "fd_reopen_mounted";
    const char *f = "/bin/.proctest-reopen";
    int fd = open(f, O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (fd < 0) { skip(n, "(cannot create test file)"); return; }
    char p[64], buf[16] = {0};
    if (write(fd, "mounted-data", 12) != 12) { fail(n, "write"); goto out; }
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    int r2 = open(p, O_RDONLY);
    if (r2 < 0) { fail(n, "open(%s): %s", p, strerror(errno)); goto out; }
    ssize_t r = read(r2, buf, sizeof buf - 1);
    close(r2);
    if (r != 12 || memcmp(buf, "mounted-data", 12)) { fail(n, "read %zd '%s' (want mounted-data)", r, buf); goto out; }
    pass(n);
out:
    close(fd);
    unlink(f);
}

#ifndef PROCTEST_COLD
#define PROCTEST_COLD "/bin/proctest-cold.bin"   /* staged by mkfs-f2fs-populated.py */
#endif
/* First touch of a file nothing has read this boot is a MAJOR fault. Once
 * per boot: a /tmp marker (tmpfs, empty at boot) stops a rerun failing. */
static void majflt_cold(void) {
    const char *n = "majflt_cold";
    int fd = open(PROCTEST_COLD, O_RDONLY);
    if (fd < 0) { skip(n, "(no cold file: not LeandrOS image)"); return; }
    int mk = open("/tmp/.proctest-cold-used", O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (mk < 0) { close(fd); skip(n, "(cold file already read this boot)"); return; }
    close(mk);
    size_t len = 2 << 20;
    char *m = mmap(NULL, len, PROT_READ, MAP_PRIVATE, fd, 0);
    if (m == MAP_FAILED) { fail(n, "mmap: %s", strerror(errno)); close(fd); return; }
    unsigned long min0, maj0, min1, maj1;
    stat_faults("/proc/self/stat", &min0, &maj0);
    unsigned long sum = 0;
    for (size_t i = 0; i < len; i += 4096) sum += (unsigned char)m[i];
    stat_faults("/proc/self/stat", &min1, &maj1);
    munmap(m, len); close(fd);
    if (sum == 0) { fail(n, "file reads as zeros"); return; }
    if (maj1 <= maj0) { fail(n, "cold mmap read: majflt %lu -> %lu (want an increase), minflt %lu -> %lu", maj0, maj1, min0, min1); return; }
    pass(n);
}

/* A vfork child that execs starts with its own fault counters, not its
 * parent's: dirty many pages first so the parent's minflt is large. */
static void vfork_exec_faults(const char *self) {
    const char *n = "vfork_exec_faults";
    size_t len = 32 << 20;
    char *m = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) { fail(n, "mmap"); return; }
    for (size_t i = 0; i < len; i += 4096) m[i] = 1;   /* ~8192 minor faults */
    unsigned long pmin = 0, pmaj = 0;
    stat_faults("/proc/self/stat", &pmin, &pmaj);
    int pfd[2];
    if (pipe(pfd)) { fail(n, "pipe"); munmap(m, len); return; }
    fflush(stdout);
    pid_t c = vfork();
    if (c == 0) {
        dup2(pfd[1], 1);
        execl(self, self, "--print-faults", (char *)NULL);
        _exit(127);
    }
    close(pfd[1]);
    char buf[64] = {0};
    ssize_t r = read(pfd[0], buf, sizeof buf - 1);
    close(pfd[0]);
    int st = wait_for(c, 5000, 0);
    munmap(m, len);
    unsigned long cmin = 0, cmaj = 0;
    if (r <= 0 || sscanf(buf, "%lu %lu", &cmin, &cmaj) != 2 || st < 0) { fail(n, "child output '%s' st %#x", buf, st); return; }
    if (pmin < 8000) { fail(n, "parent minflt %lu: setup did not fault", pmin); return; }
    if (cmin >= pmin) { fail(n, "exec'd vfork child minflt %lu >= parent's %lu (inherited)", cmin, pmin); return; }
    pass(n);
}

/* aarch64 /proc/cpuinfo carries the Linux arm64 fields (podman's getCPUInfo
 * looks for "CPU architecture"). */
static void cpuinfo_arm64_fields(void) {
#ifdef __aarch64__
    static const char *want[] = { "processor", "BogoMIPS", "Features", "CPU implementer",
        "CPU architecture: 8", "CPU variant", "CPU part", "CPU revision" };
    char buf[4096];
    int fd = open("/proc/cpuinfo", O_RDONLY);
    ssize_t n = fd < 0 ? -1 : read(fd, buf, sizeof buf - 1);
    if (fd >= 0) close(fd);
    if (n <= 0) { fail("cpuinfo_arm64_fields", "read failed"); return; }
    buf[n] = 0;
    for (unsigned i = 0; i < sizeof want / sizeof *want; i++)
        if (!strstr(buf, want[i])) { fail("cpuinfo_arm64_fields", "missing %s", want[i]); return; }
    pass("cpuinfo_arm64_fields");
#endif
}


/* ── flatpak/ostree/bwrap procfs + tmpfile contract (lane flatfs) ───────────── */

/* `s` is a lowercase 8-4-4-4-12 UUID plus the newline Linux appends. */
static int uuid_ok(const char *s, ssize_t n) {
    if (n != 37 || s[36] != '\n') return 0;
    for (int i = 0; i < 36; i++) {
        if (i == 8 || i == 13 || i == 18 || i == 23) { if (s[i] != '-') return 0; }
        else if (!((s[i] >= '0' && s[i] <= '9') || (s[i] >= 'a' && s[i] <= 'f'))) return 0;
    }
    return 1;
}

static ssize_t slurp(const char *path, char *buf, size_t cap) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    ssize_t n = read(fd, buf, cap);
    close(fd);
    return n;
}

static void random_uuid_files(void) {
    char a[64], b[64], u1[64], u2[64];
    ssize_t na = slurp("/proc/sys/kernel/random/boot_id", a, sizeof a);
    if (na < 0) { fail("boot_id", "open: %s", strerror(errno)); return; }
    ssize_t nb = slurp("/proc/sys/kernel/random/boot_id", b, sizeof b);
    if (!uuid_ok(a, na)) { fail("boot_id_format", "bad content (%zd bytes)", na); return; }
    if (nb != na || memcmp(a, b, na)) { fail("boot_id_stable", "two reads differ"); return; }
    struct stat st;
    if (stat("/proc/sys/kernel/random/boot_id", &st) || !S_ISREG(st.st_mode)) { fail("boot_id_stat", "not a regular file"); return; }
    pass("boot_id");
    ssize_t n1 = slurp("/proc/sys/kernel/random/uuid", u1, sizeof u1);
    ssize_t n2 = slurp("/proc/sys/kernel/random/uuid", u2, sizeof u2);
    if (!uuid_ok(u1, n1) || !uuid_ok(u2, n2)) { fail("random_uuid_format", "bad content (%zd, %zd)", n1, n2); return; }
    if (n1 == n2 && !memcmp(u1, u2, n1)) { fail("random_uuid_fresh", "two reads equal"); return; }
    pass("random_uuid");
}

/* /proc/<pid>/ns is a directory: bwrap opens it and openat()s each link. */
static int ns_dir_check(const char *dir, const char *what) {
    struct stat st;
    if (stat(dir, &st)) { fail(what, "stat %s: %s", dir, strerror(errno)); return 0; }
    if (!S_ISDIR(st.st_mode)) { fail(what, "%s is not a directory", dir); return 0; }
    DIR *d = opendir(dir);
    if (!d) { fail(what, "opendir %s: %s", dir, strerror(errno)); return 0; }
    static const char *want[] = { "user", "mnt", "pid", "net", "uts", "ipc", "cgroup" };
    int seen[7] = {0}, bad = 0;
    struct dirent *e;
    while ((e = readdir(d))) {
        if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
        int k = -1;
        for (int i = 0; i < 7; i++) if (!strcmp(e->d_name, want[i])) k = i;
        if (k >= 0) seen[k] = 1;
        else if (!strcmp(e->d_name, "pid_for_children")) continue;
        else if (!strcmp(e->d_name, "time") || !strcmp(e->d_name, "time_for_children")) continue;
        else bad = 1;
    }
    closedir(d);
    for (int i = 0; i < 7; i++) if (!seen[i]) { fail(what, "readdir lacks %s", want[i]); return 0; }
    if (bad) { fail(what, "readdir lists an unknown entry"); return 0; }
    int fd = open(dir, O_PATH | O_DIRECTORY | O_CLOEXEC);
    if (fd < 0) { fail(what, "open(O_PATH|O_DIRECTORY): %s", strerror(errno)); return 0; }
    int lfd = openat(fd, "user", O_RDONLY | O_CLOEXEC);
    if (lfd < 0) { fail(what, "openat(fd, user): %s", strerror(errno)); close(fd); return 0; }
    struct stat ls;
    if (fstat(lfd, &ls)) { fail(what, "fstat(user): %s", strerror(errno)); close(lfd); close(fd); return 0; }
    close(lfd);
    char tgt[64];
    ssize_t n = readlinkat(fd, "mnt", tgt, sizeof tgt - 1);
    close(fd);
    if (n < 5 || strncmp(tgt, "mnt:[", 5)) { fail(what, "readlinkat(mnt) = %zd", n); return 0; }
    return 1;
}

static void proc_ns_dir(void) {
    char p[64];
    snprintf(p, sizeof p, "/proc/%d/ns", (int)getpid());
    if (ns_dir_check("/proc/self/ns", "proc_self_ns_dir") && ns_dir_check(p, "proc_pid_ns_dir")) pass("proc_ns_dir");
}

/* Kernel keyrings and listmount(2): quiet ENOSYS, as Linux without them. */
static void quiet_enosys(void) {
#ifdef __aarch64__
    const long keyctl = 219, listmount = 458;
#else
    const long keyctl = 250, listmount = 458;
#endif
    errno = 0;
    long r = syscall(keyctl, 0 /* KEYCTL_GET_KEYRING_ID */, -3, 0, 0, 0);
    if (r != -1 || errno != ENOSYS) { fail("keyctl_enosys", "r=%ld errno=%d", r, errno); return; }
    errno = 0;
    r = syscall(listmount, 0, 0, 0, 0);
    if (r != -1 || errno != ENOSYS) { fail("listmount_enosys", "r=%ld errno=%d", r, errno); return; }
    pass("keyctl_listmount_enosys");
}

/* O_TMPFILE + linkat via /proc/self/fd (ostree's glnx_link_tmpfile_at). Either
 * the filesystem refuses O_TMPFILE the way Linux does when unsupported (callers
 * fall back to a named temp file), or the fd must be linkable. A fd that opens
 * but cannot be linked was the "linkat: ENOENT" of `flatpak remote-add`. */
static void tmpfile_link(const char *dir) {
    char name[96];
    snprintf(name, sizeof name, "tmpfile_link %s", dir);
    int fd = open(dir, O_TMPFILE | O_RDWR | O_CLOEXEC, 0600);
    if (fd < 0) {
        if (errno == EOPNOTSUPP || errno == EISDIR || errno == ENOSYS) { pass(name); return; }
        fail(name, "open(O_TMPFILE): %s", strerror(errno));
        return;
    }
    static const char data[] = "tmpfile-data";
    if (write(fd, data, sizeof data - 1) != (ssize_t)sizeof data - 1) { fail(name, "write: %s", strerror(errno)); close(fd); return; }
    char src[48], dst[PATH_MAX];
    snprintf(src, sizeof src, "/proc/self/fd/%d", fd);
    snprintf(dst, sizeof dst, "%s/.tmpfile-link-%d", dir, (int)getpid());
    unlink(dst);
    if (linkat(AT_FDCWD, src, AT_FDCWD, dst, AT_SYMLINK_FOLLOW) != 0) {
        fail(name, "linkat(%s): %s", src, strerror(errno));
        close(fd);
        return;
    }
    char back[32] = {0};
    ssize_t n = slurp(dst, back, sizeof back);
    unlink(dst);
    close(fd);
    if (n != (ssize_t)sizeof data - 1 || memcmp(back, data, n)) { fail(name, "linked file reads %zd bytes", n); return; }
    pass(name);
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--print-faults") == 0) {
        unsigned long mi = 0, ma = 0;
        stat_faults("/proc/self/stat", &mi, &ma);
        printf("%lu %lu\n", mi, ma);
        return 0;
    }
    if (argc > 1 && !strcmp(argv[1], "--exec-check")) return 42;
    const char *dirs[8];
    int ndirs = 0, o;
    while ((o = getopt(argc, argv, "d:")) != -1) {
        if (o == 'd' && ndirs < 8) dirs[ndirs++] = optarg;
        else { fprintf(stderr, "usage: proctest [-d dir]...\n"); return 2; }
    }
    if (ndirs == 0) {
        dirs[ndirs++] = "/tmp";
        if (geteuid() == 0) dirs[ndirs++] = "/root";
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("proctest: uid %d, %ld cpus\n", (int)geteuid(), sysconf(_SC_NPROCESSORS_ONLN));

    cpuinfo_arm64_fields();
    wait4_rusage();
    getrusage_maxrss();
    for (int i = 0; i < ndirs; i++) majflt_cache_hit(dirs[i]);
    majflt_cold();
    thread_faults();
    vfork_exec_faults("/proc/self/exe");

    random_uuid_files();
    proc_ns_dir();
    quiet_enosys();
    for (int i = 0; i < ndirs; i++) tmpfile_link(dirs[i]);

    status_groups();
    sys_devices_cpu();
    sysstat_whole();
    sysstat_subrows();
    lstat_magic();
    exec_proc_self_exe();
    for (int i = 0; i < ndirs; i++) { fd_rename_follow(dirs[i]); fd_deleted(dirs[i]); }

    fd_reopen_other();
    fd_reopen_pipe();
    fd_reopen_mounted();
    clone3_exit_signal();
    clone3_set_tid();
    clone_pidfd_emfile();
    close_range_unshare();
    unshare_files_thread();
    fork_unshared_socket();
    pdeathsig_thread();
    pdeathsig_process();
    pdeathsig_leader_thread();
    pdeathsig_chain();
    pdeathsig_child_thread();
    pdeathsig_once();
    stop_leaderless();
    stop_thread_sigchld();
    clone_fs_threads(dirs[0]);
    clone_fs_process(dirs[0]);

    printf("proctest: %d passed, %d failed, %d skipped\n", npass, nfail, nskip);
    return nfail;
}
