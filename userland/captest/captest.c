/*
 * captest — per-thread capability sets (capget/capset, PR_CAPBSET_*,
 * /proc/self/status Cap* lines, chroot gated on CAP_SYS_CHROOT). Run as root.
 * Each case runs in a forked child so dropping caps stays local.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/personality.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <time.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/mount.h>
#include <netinet/in.h>
#include <sys/time.h>

#define V3 0x20080522
#define CAP_SETPCAP 8
#ifndef PR_SET_CHILD_SUBREAPER
#define PR_SET_CHILD_SUBREAPER 36
#define PR_GET_CHILD_SUBREAPER 37
#endif
#ifndef PR_CAP_AMBIENT
#define PR_CAP_AMBIENT 47
#endif
#define AMB_IS_SET 1
#define AMB_RAISE 2
#define AMB_LOWER 3
#define AMB_CLEAR 4
#ifndef PR_GET_SECUREBITS
#define PR_GET_SECUREBITS 27
#define PR_SET_SECUREBITS 28
#endif
#define SB_NOROOT 1
#define SB_NOROOT_LOCKED 2
#define SB_NO_SETUID_FIXUP 4
#define SB_KEEP_CAPS 16
#define SB_NO_AMB_RAISE 64
#ifndef SYS_keyctl
#if defined(__aarch64__)
#define SYS_keyctl 219
#else
#define SYS_keyctl 250
#endif
#endif
#define CAP_SYS_CHROOT 18
struct hdr { unsigned v; int pid; };
struct dat { unsigned eff, prm, inh; };

static int cget(struct dat d[2]) { struct hdr h = { V3, 0 }; return syscall(SYS_capget, &h, d); }
static int cset(struct dat d[2]) { struct hdr h = { V3, 0 }; return syscall(SYS_capset, &h, d); }
static unsigned long long u64(unsigned lo, unsigned hi) { return lo | (unsigned long long)hi << 32; }

static int fails;
#define CHECK(c) do { if (!(c)) { printf("  FAIL line %d: %s (errno %d)\n", __LINE__, #c, errno); return 1; } } while (0)

static int status_hex(const char *key, unsigned long long *out)
{
    char buf[4096]; int fd = open("/proc/self/status", O_RDONLY);
    if (fd < 0) return -1;
    int n = read(fd, buf, sizeof buf - 1); close(fd);
    if (n <= 0) return -1;
    buf[n] = 0;
    char *p = strstr(buf, key);
    if (!p) return -1;
    *out = strtoull(p + strlen(key), 0, 16);
    return 0;
}

static int t_root_full(void)
{
    struct dat d[2]; CHECK(cget(d) == 0);
    CHECK(u64(d[0].eff, d[1].eff) == (1ULL << 41) - 1);
    CHECK(u64(d[0].prm, d[1].prm) == (1ULL << 41) - 1);
    CHECK(d[0].inh == 0 && d[1].inh == 0);
    CHECK(prctl(PR_CAPBSET_READ, CAP_SYS_CHROOT) == 1);
    CHECK(prctl(PR_CAPBSET_READ, 41) == -1 && errno == EINVAL);
    return 0;
}

static int t_status_matches(void)
{
    struct dat d[2]; CHECK(cget(d) == 0);
    unsigned long long e, p, i, b;
    CHECK(status_hex("CapEff:\t", &e) == 0 && status_hex("CapPrm:\t", &p) == 0);
    CHECK(status_hex("CapInh:\t", &i) == 0 && status_hex("CapBnd:\t", &b) == 0);
    CHECK(e == u64(d[0].eff, d[1].eff) && p == u64(d[0].prm, d[1].prm) && i == 0);
    CHECK(b == (1ULL << 41) - 1);
    return 0;
}

static int t_nonroot_empty(void)
{
    CHECK(setresuid(1000, 1000, 1000) == 0);
    struct dat d[2]; CHECK(cget(d) == 0);
    CHECK(d[0].eff == 0 && d[0].prm == 0 && d[1].eff == 0 && d[1].prm == 0);
    struct dat want[2] = { { 1u << CAP_SYS_CHROOT, 1u << CAP_SYS_CHROOT, 0 }, { 0, 0, 0 } };
    CHECK(cset(want) == -1 && errno == EPERM);
    struct dat zero[2] = { { 0, 0, 0 }, { 0, 0, 0 } };
    CHECK(cset(zero) == 0);
    CHECK(chroot("/") == -1 && errno == EPERM);
    return 0;
}

static int t_eff_subset(void)
{
    struct dat d[2] = { { 1u << CAP_SYS_CHROOT, 0, 0 }, { 0, 0, 0 } };
    CHECK(cset(d) == -1 && errno == EPERM); /* eff not within prm */
    return 0;
}

static int t_drop_cannot_regain(void)
{
    struct dat d[2] = { { 1u << CAP_SYS_CHROOT, 1u << CAP_SYS_CHROOT, 0 }, { 0, 0, 0 } };
    CHECK(cset(d) == 0);
    CHECK(chroot("/") == 0);
    struct dat z[2] = { { 0, 0, 0 }, { 0, 0, 0 } };
    CHECK(cset(z) == 0);
    CHECK(chroot("/") == -1 && errno == EPERM);
    CHECK(cset(d) == -1 && errno == EPERM); /* permitted is gone */
    CHECK(cget(d) == 0 && d[0].eff == 0 && d[0].prm == 0);
    return 0;
}

static int t_sys_time_needs_cap(void)
{
    struct timespec now; CHECK(clock_gettime(CLOCK_REALTIME, &now) == 0);
    struct timeval tv = { now.tv_sec, now.tv_nsec / 1000 };
    struct dat all[2]; CHECK(cget(all) == 0);
    CHECK(clock_settime(CLOCK_REALTIME, &now) == 0); /* full-cap root works */
    unsigned e0 = all[0].prm & ~(1u << 25); /* CAP_SYS_TIME dropped from effective */
    struct dat d[2] = { { e0, all[0].prm, 0 }, { all[1].eff, all[1].prm, 0 } };
    CHECK(cset(d) == 0);
    CHECK(clock_settime(CLOCK_REALTIME, &now) == -1 && errno == EPERM);
    CHECK(settimeofday(&tv, NULL) == -1 && errno == EPERM);
    return 0;
}

static int t_drop_effective_only(void)
{
    struct dat all[2]; CHECK(cget(all) == 0);
    struct dat d[2] = { { 0, all[0].prm, 0 }, { 0, all[1].prm, 0 } };
    CHECK(cset(d) == 0);
    CHECK(chroot("/") == -1 && errno == EPERM);
    d[0].eff = 1u << CAP_SYS_CHROOT; /* raise again within permitted */
    CHECK(cset(d) == 0);
    CHECK(chroot("/") == 0);
    return 0;
}

static int t_fork_inherits(void)
{
    struct dat d[2] = { { 1u << CAP_SYS_CHROOT, 1u << CAP_SYS_CHROOT, 0 }, { 0, 0, 0 } };
    CHECK(cset(d) == 0);
    pid_t c = fork();
    if (c == 0) {
        struct dat k[2];
        if (cget(k) != 0 || k[0].eff != (1u << CAP_SYS_CHROOT) || k[0].prm != (1u << CAP_SYS_CHROOT)
            || k[1].eff || chroot("/") != 0) _exit(1);
        _exit(0);
    }
    int st; CHECK(waitpid(c, &st, 0) == c && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}

static int t_bset_drop(void)
{
    CHECK(prctl(PR_CAPBSET_DROP, CAP_SYS_CHROOT) == 0);
    CHECK(prctl(PR_CAPBSET_READ, CAP_SYS_CHROOT) == 0);
    unsigned long long b; CHECK(status_hex("CapBnd:\t", &b) == 0 && !(b >> CAP_SYS_CHROOT & 1));
    return 0;
}

static int t_exec_nonroot_clears(void)
{
    /* root keeps full caps across exec; dropping uid before exec clears them */
    CHECK(setresuid(1000, 1000, 1000) == 0);
    execl("/bin/captest", "captest", "--child-empty", (char *)0);
    return 1;
}

static int child_empty(void)
{
    struct dat d[2];
    return cget(d) == 0 && !d[0].eff && !d[0].prm && !d[1].eff && !d[1].prm ? 0 : 1;
}

static int drop_all(void)
{
    struct dat d[2]; memset(d, 0, sizeof d);
    return cset(d);
}

static int child_full(void)
{
    struct dat d[2];
    return cget(d) == 0 && u64(d[0].eff, d[1].eff) == (1ULL << 41) - 1
        && u64(d[0].prm, d[1].prm) == (1ULL << 41) - 1 ? 0 : 1;
}

static int t_exec_nnp_keeps_dropped(void)
{
    /* root, caps dropped, NNP set: exec must not regain anything */
    CHECK(drop_all() == 0);
    CHECK(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0);
    execl("/bin/captest", "captest", "--child-empty", (char *)0);
    return 1;
}

static int t_exec_no_nnp_regains(void)
{
    CHECK(drop_all() == 0);
    execl("/bin/captest", "captest", "--child-full", (char *)0);
    return 1;
}

#define CH (1u << CAP_SYS_CHROOT)

static int set_chroot_only(void)
{
    struct dat d[2] = { { CH, CH, CH }, { 0, 0, 0 } };
    return cset(d);
}

static int t_ambient_ops(void)
{
    CHECK(prctl(PR_CAP_AMBIENT, AMB_IS_SET, CAP_SYS_CHROOT, 0, 0) == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_RAISE, CAP_SYS_CHROOT, 0, 0) == -1 && errno == EPERM); /* not inheritable */
    CHECK(set_chroot_only() == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_RAISE, CAP_SYS_CHROOT, 0, 0) == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_IS_SET, CAP_SYS_CHROOT, 0, 0) == 1);
    unsigned long long a; CHECK(status_hex("CapAmb:\t", &a) == 0 && a == CH);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_LOWER, CAP_SYS_CHROOT, 0, 0) == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_IS_SET, CAP_SYS_CHROOT, 0, 0) == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_RAISE, CAP_SYS_CHROOT, 0, 0) == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_CLEAR, 0, 0, 0) == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_IS_SET, CAP_SYS_CHROOT, 0, 0) == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_IS_SET, 41, 0, 0) == -1 && errno == EINVAL);
    CHECK(prctl(PR_CAP_AMBIENT, 99, 0, 0, 0) == -1 && errno == EINVAL);
    return 0;
}

static int t_ambient_trimmed_by_capset(void)
{
    CHECK(set_chroot_only() == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_RAISE, CAP_SYS_CHROOT, 0, 0) == 0);
    struct dat d[2] = { { CH, CH, 0 }, { 0, 0, 0 } }; /* inheritable gone */
    CHECK(cset(d) == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_IS_SET, CAP_SYS_CHROOT, 0, 0) == 0);
    return 0;
}

static int child_amb(void)
{
    struct dat d[2];
    unsigned long long a;
    if (cget(d) != 0 || geteuid() != 1000) return 1;
    if (d[0].eff != CH || d[0].prm != CH || d[1].eff || d[1].prm) return 2;
    if (status_hex("CapAmb:\t", &a) != 0 || a != CH) return 3;
    if (prctl(PR_GET_KEEPCAPS) != 0) return 4;
    return chroot("/") == 0 ? 0 : 5;
}

/* Linux: setresuid to an unrelated uid needs CAP_SETUID, so these keep it. */
static int set_chroot_setuid(void)
{
    struct dat d[2] = { { CH | 1u << 7, CH | 1u << 7, CH | 1u << 7 }, { 0, 0, 0 } };
    return cset(d);
}

static int t_ambient_exec_nonroot(void)
{
    CHECK(set_chroot_setuid() == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_RAISE, CAP_SYS_CHROOT, 0, 0) == 0);
    CHECK(prctl(PR_SET_KEEPCAPS, 1) == 0 && prctl(PR_GET_KEEPCAPS) == 1);
    CHECK(setresuid(1000, 1000, 1000) == 0);
    struct dat d[2]; CHECK(cget(d) == 0 && d[0].eff == 0 && d[0].prm == (CH | 1u << 7)); /* kept, effective cleared */
    execl("/bin/captest", "captest", "--child-amb", (char *)0);
    return 1;
}

static int t_ambient_cleared_without_keepcaps(void)
{
    CHECK(set_chroot_setuid() == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_RAISE, CAP_SYS_CHROOT, 0, 0) == 0);
    CHECK(setresuid(1000, 1000, 1000) == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_IS_SET, CAP_SYS_CHROOT, 0, 0) == 0);
    return 0;
}

static int t_keepcaps(void)
{
    CHECK(prctl(PR_GET_KEEPCAPS) == 0);
    CHECK(prctl(PR_SET_KEEPCAPS, 2) == -1 && errno == EINVAL);
    CHECK(prctl(PR_SET_KEEPCAPS, 1) == 0 && prctl(PR_GET_KEEPCAPS) == 1);
    CHECK(prctl(PR_GET_SECUREBITS) == SB_KEEP_CAPS);
    CHECK(setresuid(1000, 1000, 1000) == 0);
    struct dat d[2]; CHECK(cget(d) == 0 && d[0].prm != 0); /* permitted survived */
    return 0;
}

static int t_securebits(void)
{
    CHECK(prctl(PR_GET_SECUREBITS) == 0);
    CHECK(prctl(PR_SET_SECUREBITS, 1 << 8) == -1 && errno == EPERM); /* unknown bit */
    CHECK(prctl(PR_SET_SECUREBITS, SB_NOROOT | SB_NOROOT_LOCKED) == 0);
    CHECK(prctl(PR_GET_SECUREBITS) == (SB_NOROOT | SB_NOROOT_LOCKED));
    CHECK(prctl(PR_SET_SECUREBITS, 0) == -1 && errno == EPERM); /* locked */
    CHECK(prctl(PR_SET_SECUREBITS, SB_NOROOT | SB_NOROOT_LOCKED | SB_NO_AMB_RAISE) == 0);
    CHECK(set_chroot_only() == 0);
    CHECK(prctl(PR_CAP_AMBIENT, AMB_RAISE, CAP_SYS_CHROOT, 0, 0) == -1 && errno == EPERM);
    return 0;
}

static int t_securebits_needs_setpcap(void)
{
    struct dat all[2]; CHECK(cget(all) == 0);
    struct dat d[2] = { { all[0].eff & ~(1u << CAP_SETPCAP), all[0].prm, 0 }, { all[1].eff, all[1].prm, 0 } };
    CHECK(cset(d) == 0);
    CHECK(prctl(PR_SET_SECUREBITS, SB_KEEP_CAPS) == -1 && errno == EPERM);
    return 0;
}

static int t_noroot_exec(void)
{
    CHECK(prctl(PR_SET_SECUREBITS, SB_NOROOT) == 0);
    execl("/bin/captest", "captest", "--child-empty", (char *)0);
    return 1;
}

static int t_no_setuid_fixup(void)
{
    CHECK(prctl(PR_SET_SECUREBITS, SB_NO_SETUID_FIXUP) == 0);
    CHECK(setresuid(1000, 1000, 1000) == 0);
    struct dat d[2]; CHECK(cget(d) == 0);
    CHECK(u64(d[0].eff, d[1].eff) == (1ULL << 41) - 1); /* nothing was dropped */
    return 0;
}

static int t_subreaper_flag(void)
{
    int v = 7;
    CHECK(prctl(PR_GET_CHILD_SUBREAPER, &v) == 0 && v == 0);
    CHECK(prctl(PR_SET_CHILD_SUBREAPER, 1) == 0);
    CHECK(prctl(PR_GET_CHILD_SUBREAPER, &v) == 0 && v == 1);
    pid_t c = fork();
    if (c == 0) { int w = 7; _exit(prctl(PR_GET_CHILD_SUBREAPER, &w) == 0 && w == 0 ? 0 : 1); }
    int st; CHECK(waitpid(c, &st, 0) == c && WIFEXITED(st) && WEXITSTATUS(st) == 0); /* not inherited */
    CHECK(prctl(PR_SET_CHILD_SUBREAPER, 0) == 0);
    CHECK(prctl(PR_GET_CHILD_SUBREAPER, &v) == 0 && v == 0);
    return 0;
}

/* middle forks a grandchild and dies at once; the grandchild reports its
 * new parent and exits 7. */
static int orphan_run(int subreaper)
{
    int p[2];
    CHECK(pipe(p) == 0);
    if (subreaper) CHECK(prctl(PR_SET_CHILD_SUBREAPER, 1) == 0);
    pid_t me = getpid();
    pid_t m = fork();
    if (m == 0) {
        pid_t g = fork();
        if (g == 0) {
            pid_t was = getppid();
            for (int i = 0; i < 500 && getppid() == was; i++) usleep(10000);
            pid_t now = getppid();
            write(p[1], &now, sizeof now);
            _exit(7);
        }
        _exit(0);
    }
    int st; CHECK(waitpid(m, &st, 0) == m);
    pid_t got = 0;
    close(p[1]);
    CHECK(read(p[0], &got, sizeof got) == sizeof got);
    if (subreaper) {
        CHECK(got == me);
        pid_t w = waitpid(-1, &st, 0); /* the orphan is now OUR child */
        CHECK(w > 0 && WIFEXITED(st) && WEXITSTATUS(st) == 7);
    } else {
        CHECK(got != me && got > 0); /* init, not us */
    }
    return 0;
}
static int t_orphan_to_init(void) { return orphan_run(0); }
static int t_orphan_to_subreaper(void) { return orphan_run(1); }

static int t_keyctl_enosys(void)
{
    /* KEYCTL_JOIN_SESSION_KEYRING (1): crun tolerates ENOSYS. */
    CHECK(syscall(SYS_keyctl, 1, (long)0, 0, 0, 0) == -1 && errno == ENOSYS);
    return 0;
}

static int t_personality(void)
{
    CHECK(personality(0xffffffff) == 0);
    CHECK(personality(ADDR_NO_RANDOMIZE) == 0);
    CHECK(personality(0xffffffff) == ADDR_NO_RANDOMIZE);
    pid_t c = fork();
    if (c == 0) _exit(personality(0xffffffff) == ADDR_NO_RANDOMIZE ? 0 : 1);
    int st; CHECK(waitpid(c, &st, 0) == c && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}

static int t_rlimits(void)
{
    int res[2] = { RLIMIT_NOFILE, RLIMIT_NPROC };
    for (int i = 0; i < 2; i++) {
        struct rlimit r = { 64, 256 }, g;
        CHECK(setrlimit(res[i], &r) == 0);
        CHECK(getrlimit(res[i], &g) == 0 && g.rlim_cur == 64 && g.rlim_max == 256);
    }
    return 0;
}

static int rd_oom(char *buf, int n)
{
    int fd = open("/proc/self/oom_score_adj", O_RDONLY);
    if (fd < 0) return -1;
    int r = read(fd, buf, n - 1);
    close(fd);
    if (r > 0) buf[r] = 0;
    return r;
}
static int wr_oom(const char *v)
{
    int fd = open("/proc/self/oom_score_adj", O_WRONLY);
    if (fd < 0) return -1;
    int r = write(fd, v, strlen(v));
    int e = errno;
    close(fd);
    errno = e;
    return r;
}

static int t_oom_score_adj(void)
{
    char b[32];
    CHECK(rd_oom(b, sizeof b) > 0 && atoi(b) == 0);
    CHECK(wr_oom("300\n") == 4);
    CHECK(rd_oom(b, sizeof b) > 0 && atoi(b) == 300);
    CHECK(wr_oom("1001") == -1 && errno == EINVAL);
    pid_t c = fork();
    if (c == 0) { char k[32]; _exit(rd_oom(k, sizeof k) > 0 && atoi(k) == 300 ? 0 : 1); }
    int st; CHECK(waitpid(c, &st, 0) == c && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    CHECK(wr_oom("-5") == 2); /* root: CAP_SYS_RESOURCE */
    CHECK(wr_oom("10") == 2);
    CHECK(setresuid(1000, 1000, 1000) == 0);
    CHECK(wr_oom("-20") == -1 && errno == EACCES);
    CHECK(wr_oom("20") == 2);
    return 0;
}

/* capaudit: drop `cap` from the effective set only (the permitted set keeps it). */
static int drop_cap(int cap)
{
    struct dat all[2]; if (cget(all) != 0) return -1;
    struct dat d[2] = { { all[0].eff & ~(cap < 32 ? 1u << cap : 0), all[0].prm, 0 },
                        { all[1].eff & ~(cap >= 32 ? 1u << (cap - 32) : 0), all[1].prm, 0 } };
    return cset(d);
}

/* Full-cap root: the call is not refused (ENOSYS is fine). Dropped: EPERM. */
#define NEEDS_CAP(name, cap, expr)                                       \
static int name(void)                                                    \
{                                                                        \
    errno = 0; long r = (expr);                                          \
    CHECK(!(r == -1 && errno == EPERM));                                 \
    CHECK(drop_cap(cap) == 0);                                           \
    errno = 0; r = (expr);                                               \
    CHECK(r == -1 && errno == EPERM);                                    \
    return 0;                                                            \
}
NEEDS_CAP(t_acct_needs_cap, 20, syscall(SYS_acct, NULL))
NEEDS_CAP(t_vhangup_needs_cap, 26, syscall(SYS_vhangup))
NEEDS_CAP(t_swapoff_needs_cap, 21, syscall(SYS_swapoff, "/nonexistent"))
NEEDS_CAP(t_mount_needs_cap, 21, mount("none", "/tmp", "tmpfs", MS_REMOUNT | MS_BIND, ""))

static int t_net_raw_needs_cap(void)
{
    int fd = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    CHECK(fd >= 0);
    close(fd);
    CHECK(drop_cap(13) == 0);
    errno = 0; fd = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    CHECK(fd == -1 && errno == EPERM);
    return 0;
}

/* A uid-1000 child: root needs CAP_KILL to signal it, CAP_SYS_PTRACE to read its /proc links. */
static int with_victim(int cap, int proc_link)
{
    int p[2]; CHECK(pipe(p) == 0);
    pid_t c = fork();
    if (c == 0) {
        if (setresuid(1000, 1000, 1000) != 0) _exit(2);
        char ok = 'x'; write(p[1], &ok, 1);
        pause(); _exit(0);
    }
    char b; CHECK(read(p[0], &b, 1) == 1);
    char path[64]; snprintf(path, sizeof path, "/proc/%d/exe", (int)c);
    char tgt[128];
    int full = proc_link ? (readlink(path, tgt, sizeof tgt) >= 0) : (kill(c, 0) == 0);
    CHECK(full);
    CHECK(drop_cap(cap) == 0);
    errno = 0;
    int r = proc_link ? (int)readlink(path, tgt, sizeof tgt) : kill(c, 0);
    int e = errno;
    kill(c, SIGKILL); /* may fail while the cap is dropped; the parent reaps via exit of this test */
    CHECK(r == -1 && (proc_link ? (e == EACCES || e == EPERM) : e == EPERM));
    return 0;
}
static int t_kill_needs_cap(void) { return with_victim(5, 0); }
static int t_ptrace_link_needs_cap(void) { return with_victim(19, 1); }

struct tc { const char *n; int (*f)(void); };
int main(int argc, char **argv)
{
    if (argc > 1 && !strcmp(argv[1], "--child-empty")) return child_empty();
    if (argc > 1 && !strcmp(argv[1], "--child-full")) return child_full();
    if (argc > 1 && !strcmp(argv[1], "--child-amb")) return child_amb();
    if (geteuid() != 0) { printf("SKIP: not root\n"); return 0; }
    struct tc cs[] = {
        { "root_full", t_root_full }, { "status_matches", t_status_matches },
        { "nonroot_empty", t_nonroot_empty }, { "eff_subset", t_eff_subset },
        { "drop_cannot_regain", t_drop_cannot_regain }, { "sys_time_needs_cap", t_sys_time_needs_cap }, { "drop_effective_only", t_drop_effective_only },
        { "fork_inherits", t_fork_inherits }, { "bset_drop", t_bset_drop },
        { "exec_nonroot_clears", t_exec_nonroot_clears },
        { "exec_nnp_keeps_dropped", t_exec_nnp_keeps_dropped },
        { "exec_no_nnp_regains", t_exec_no_nnp_regains },
        { "ambient_ops", t_ambient_ops }, { "ambient_trimmed_by_capset", t_ambient_trimmed_by_capset },
        { "ambient_exec_nonroot", t_ambient_exec_nonroot },
        { "ambient_cleared_without_keepcaps", t_ambient_cleared_without_keepcaps },
        { "keepcaps", t_keepcaps }, { "securebits", t_securebits },
        { "securebits_needs_setpcap", t_securebits_needs_setpcap },
        { "noroot_exec", t_noroot_exec }, { "no_setuid_fixup", t_no_setuid_fixup },
        { "subreaper_flag", t_subreaper_flag }, { "orphan_to_init", t_orphan_to_init },
        { "orphan_to_subreaper", t_orphan_to_subreaper }, { "keyctl_enosys", t_keyctl_enosys },
        { "personality", t_personality }, { "rlimits", t_rlimits }, { "oom_score_adj", t_oom_score_adj },
        { "acct_needs_cap", t_acct_needs_cap }, { "vhangup_needs_cap", t_vhangup_needs_cap },
        { "swapoff_needs_cap", t_swapoff_needs_cap }, { "mount_needs_cap", t_mount_needs_cap },
        { "net_raw_needs_cap", t_net_raw_needs_cap }, { "kill_needs_cap", t_kill_needs_cap },
        { "ptrace_link_needs_cap", t_ptrace_link_needs_cap },
    };
    for (unsigned i = 0; i < sizeof cs / sizeof *cs; i++) {
        pid_t c = fork();
        if (c == 0) _exit(cs[i].f());
        int st = 0; waitpid(c, &st, 0);
        int ok = WIFEXITED(st) && WEXITSTATUS(st) == 0;
        printf("%s %s\n", ok ? "PASS" : "FAIL", cs[i].n);
        if (!ok) fails++;
    }
    printf("captest: %d failed\n", fails);
    return fails ? 1 : 0;
}
