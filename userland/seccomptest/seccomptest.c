/*
 * seccomptest — seccomp(2) / prctl(PR_*_SECCOMP) behaviour, with the results
 * Linux gives. Builds for the host (cc) and LeandrOS (zig cc -target
 * <arch>-linux-musl -static).
 *
 * Group S1 (kernel without an installable seccomp): the probe must say
 * "unsupported" so Firefox skips its sandbox rather than installing a filter:
 *   get_seccomp_disabled   PR_GET_SECCOMP == 0
 *   set_seccomp_einval     PR_SET_SECCOMP(SECCOMP_MODE_FILTER, prog) == EINVAL
 *   seccomp_set_filter     seccomp(SET_MODE_FILTER, 0, prog) == EINVAL
 *   seccomp_bad_op         seccomp(unknown op) == EINVAL
 *
 * Groups A-L (install, actions, SIGSYS, TSYNC, /proc) are added by the hook lane.
 * Output: "<name>: PASS|FAIL ...|SKIP ..." per case and a summary line.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_seccomp
# if defined(__aarch64__)
#  define SYS_seccomp 277
# else
#  define SYS_seccomp 317
# endif
#endif
#ifndef PR_GET_SECCOMP
# define PR_GET_SECCOMP 21
# define PR_SET_SECCOMP 22
#endif
#define SECCOMP_MODE_FILTER 2
#define SECCOMP_SET_MODE_FILTER 1

struct sf { uint16_t code; uint8_t jt, jf; uint32_t k; };
struct sfprog { uint16_t len; struct sf *filter; };

static int npass, nfail, nskip;
static void pass(const char *n) { printf("%s: PASS\n", n); npass++; fflush(stdout); }
static void fail(const char *n, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
static void fail(const char *n, const char *fmt, ...) {
    va_list ap;
    printf("%s: FAIL ", n);
    va_start(ap, fmt); vprintf(fmt, ap); va_end(ap);
    printf("\n"); nfail++; fflush(stdout);
}

/* BPF_RET|BPF_K, SECCOMP_RET_ALLOW */
static struct sf allow_all[1] = { { 0x06, 0, 0, 0x7fff0000u } };
static struct sfprog prog = { 1, allow_all };

static void expect_errno(const char *n, long r, int want) {
    if (r == -1 && errno == want) pass(n);
    else fail(n, "ret %ld errno %d, want -1/%d", r, errno, want);
}

int main(void) {
    long r = prctl(PR_GET_SECCOMP, 0, 0, 0, 0);
    if (r == 0) pass("get_seccomp_disabled"); else fail("get_seccomp_disabled", "ret %ld errno %d", r, errno);

    /* Must be a no-op failure: a filter that did install would make the
     * remaining cases (and the harness) die. NNP first so a kernel that
     * does support it would not refuse for lack of privilege. */
    prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    errno = 0;
    expect_errno("set_seccomp_einval", prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog, 0, 0), EINVAL);
    errno = 0;
    expect_errno("seccomp_set_filter", syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog), EINVAL);
    errno = 0;
    expect_errno("seccomp_bad_op", syscall(SYS_seccomp, 0xdead, 0, 0), EINVAL);

    printf("seccomptest: %d passed, %d failed, %d skipped\n", npass, nfail, nskip);
    return nfail;
}
