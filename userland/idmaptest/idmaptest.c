/*
 * idmaptest — user-namespace uid/gid map behaviour. Static zig cc, nstest
 * style: "PASS|FAIL|SKIP case" lines, runs as root.
 * Cases are appended per lane as separate functions (keeps merges trivial).
 *
 * idroot: ns_euid0_not_host, host_root_unaffected.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <netinet/in.h>
#include <time.h>
#include <unistd.h>

static int npass, nfail;

static void check(const char *name, int ok)
{
    printf("%s %s\n", ok ? "PASS" : "FAIL", name);
    if (ok) npass++; else nfail++;
}

/* Probes of host-root-only operations. None has a lasting effect. */
static int raw_socket_refused(void)
{
    int fd = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    if (fd >= 0) { close(fd); return 0; }
    return errno == EPERM || errno == EACCES;
}

static int alarm_timer_refused(void)   /* CAP_WAKE_ALARM */
{
    timer_t t;
    struct sigevent sev;
    memset(&sev, 0, sizeof sev);
    sev.sigev_notify = SIGEV_NONE;
    if (timer_create(CLOCK_REALTIME_ALARM, &sev, &t) == 0) { timer_delete(t); return 0; }
    return errno == EPERM;
}

static int reboot_refused(void)        /* CAP_SYS_BOOT is checked before the magics */
{
    long r = syscall(SYS_reboot, 0, 0, 0, 0);
    return r == -1 && errno == EPERM;
}

static int set_mm_refused(void)        /* prctl(PR_SET_MM, ...): host root only */
{
    long r = syscall(SYS_prctl, 35 /* PR_SET_MM */, 8 /* PR_SET_MM_ARG_START */, 0, 0, 0);
    return r == -1 && errno == EPERM;
}

static int run_child(int (*fn)(void))
{
    pid_t p = fork();
    if (p < 0) return 0;
    if (p == 0) _exit(fn() ? 0 : 1);
    int st = 0;
    waitpid(p, &st, 0);
    return WIFEXITED(st) && WEXITSTATUS(st) == 0;
}

/* Root that unshares CLONE_NEWUSER keeps euid 0 but is not host root. */
static int in_ns_all_refused(void)
{
    if (unshare(CLONE_NEWUSER) != 0) return 0;
    return raw_socket_refused() && alarm_timer_refused() && reboot_refused() && set_mm_refused();
}

static int host_all_allowed(void)
{
    /* Host root: raw socket and alarm timer succeed; reboot/set_mm get past
     * the privilege check (reboot: EINVAL on bad magic; set_mm: not EPERM). */
    int fd = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    if (fd < 0) return 0;
    close(fd);
    if (alarm_timer_refused()) return 0;
    if (reboot_refused()) return 0;
    if (set_mm_refused()) return 0;
    return 1;
}

static void case_ns_euid0_not_host(void)
{
    check("ns_euid0_not_host", run_child(in_ns_all_refused));
}

static void case_host_root_unaffected(void)
{
    check("host_root_unaffected", run_child(host_all_allowed));
}

int main(void)
{
    if (geteuid() != 0) { printf("SKIP idmaptest: not root\n"); return 0; }
    case_ns_euid0_not_host();
    case_host_root_unaffected();
    printf("idmaptest: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
