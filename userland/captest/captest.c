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
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define V3 0x20080522
#define CAP_SETPCAP 8
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

struct tc { const char *n; int (*f)(void); };
int main(int argc, char **argv)
{
    if (argc > 1 && !strcmp(argv[1], "--child-empty")) return child_empty();
    if (geteuid() != 0) { printf("SKIP: not root\n"); return 0; }
    struct tc cs[] = {
        { "root_full", t_root_full }, { "status_matches", t_status_matches },
        { "nonroot_empty", t_nonroot_empty }, { "eff_subset", t_eff_subset },
        { "drop_cannot_regain", t_drop_cannot_regain }, { "drop_effective_only", t_drop_effective_only },
        { "fork_inherits", t_fork_inherits }, { "bset_drop", t_bset_drop },
        { "exec_nonroot_clears", t_exec_nonroot_clears },
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
