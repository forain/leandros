/*
 * idmaptest -- user-namespace uid/gid map behaviour. Static zig-cc build,
 * runs as root; each case prints "case: PASS|FAIL|SKIP". Non-identity map
 * cases SKIP when the map write returns EPERM (kernel booted without
 * idmap=test) and must PASS with it. Lanes append cases as separate functions.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <netinet/in.h>
#include <time.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int npass, nfail;

static void report(const char *name, int ok)
{
    printf("%s: %s\n", name, ok ? "PASS" : "FAIL");
    if (ok) npass++; else nfail++;
}
static void skip(const char *name, const char *why) { printf("%s: SKIP (%s)\n", name, why); }

static int write_file(const char *path, const char *s)
{
    int fd = open(path, O_WRONLY);
    if (fd < 0) return -1;
    ssize_t n = write(fd, s, strlen(s));
    int e = errno;
    close(fd);
    errno = e;
    return n == (ssize_t)strlen(s) ? 0 : -1;
}

/* Parent (ns 0, root) writes "0 <lower> 1" to uid_map and gid_map of pid. */
static int write_maps(pid_t pid, unsigned lower)
{
    char p[64], m[64];
    snprintf(m, sizeof m, "0 %u 1", lower);
    snprintf(p, sizeof p, "/proc/%d/uid_map", pid);
    if (write_file(p, m)) return -1;
    snprintf(p, sizeof p, "/proc/%d/gid_map", pid);
    if (write_file(p, m)) return -1;
    return 0;
}

/* ---- statx_matches_stat ------------------------------------------------- */
static void statx_matches_stat(void)
{
    const char *tmp = "/tmp/idmaptest-statx";
    int fd = open(tmp, O_CREAT | O_WRONLY, 0644);
    int ok = fd >= 0;
    if (ok) { close(fd); ok = chown(tmp, 1234, 4321) == 0; }
    const char *paths[] = { tmp, "/", "/etc/passwd", "/proc/self" };
    for (unsigned i = 0; ok && i < sizeof paths / sizeof *paths; i++) {
        struct stat st; struct statx sx;
        if (stat(paths[i], &st) || syscall(SYS_statx, AT_FDCWD, paths[i], 0, STATX_BASIC_STATS, &sx)) {
            ok = 0; break;
        }
        if (st.st_uid != sx.stx_uid || st.st_gid != sx.stx_gid) ok = 0;
    }
    unlink(tmp);
    report("statx_matches_stat", ok);
}

/* ---- proc_pid_owner ----------------------------------------------------- */
static void proc_pid_owner(void)
{
    int sync[2];
    pipe(sync);
    pid_t c = fork();
    if (c == 0) {
        close(sync[1]);
        if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)) _exit(1);
        char b; read(sync[0], &b, 1);
        _exit(0);
    }
    close(sync[0]);
    usleep(200000);
    char p[32]; struct stat st;
    snprintf(p, sizeof p, "/proc/%d", c);
    int ok = stat(p, &st) == 0 && st.st_uid == 1000 && st.st_gid == 1000;
    close(sync[1]);
    waitpid(c, NULL, 0);
    report("proc_pid_owner", ok);
}

/* ---- status_groups_view ------------------------------------------------- */
static void status_groups_view(void)
{
    pid_t c = fork();
    if (c == 0) {
        gid_t g[2] = { 1000, 20 };
        if (setgroups(2, g)) _exit(2);
        FILE *f = fopen("/proc/self/status", "r");
        char l[256];
        while (f && fgets(l, sizeof l, f))
            if (!strncmp(l, "Groups:", 7)) _exit(strstr(l, "1000") && strstr(l, "20") ? 0 : 3);
        _exit(4);
    }
    int st; waitpid(c, &st, 0);
    report("status_groups_view", WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

/* ---- knob cases: child unshares a userns, parent writes "0 100000 1" ----- */
static pid_t spawn_in_ns(int *go, int *res, int (*body)(void))
{
    int a[2], b[2];
    pipe(a); pipe(b);
    pid_t c = fork();
    if (c == 0) {
        close(a[1]); close(b[0]);
        char x = 0;
        if (unshare(CLONE_NEWUSER)) { x = 'u'; write(b[1], &x, 1); _exit(1); }
        write(b[1], &x, 1);            /* unshared */
        read(a[0], &x, 1);             /* maps written ('m') or refused ('n') */
        if (x != 'm') _exit(77);
        _exit(body());
    }
    close(a[0]); close(b[1]);
    *go = a[1]; *res = b[0];
    return c;
}

static int run_ns_case(const char *name, int (*body)(void))
{
    int go, res; char x;
    pid_t c = spawn_in_ns(&go, &res, body);
    if (read(res, &x, 1) != 1 || x != 0) { write(go, "n", 1); waitpid(c, NULL, 0); skip(name, "unshare failed"); return 0; }
    char m = write_maps(c, 100000) ? 'n' : 'm';
    write(go, &m, 1);
    int st; waitpid(c, &st, 0);
    if (m == 'n') { skip(name, "non-identity map refused (no idmap=test)"); return 0; }
    report(name, WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 1;
}

static int body_stat_unmapped(void)
{
    struct stat st;
    /* /etc/passwd is kuid 0 = unmapped here (ns 0 -> 100000 only). */
    if (stat("/etc/passwd", &st)) return 2;
    return st.st_uid == 65534 && st.st_gid == 65534 ? 0 : 3;
}
static void stat_unmapped_65534(void) { run_ns_case("stat_unmapped_65534", body_stat_unmapped); }

static volatile sig_atomic_t chld_uid = -2;
static void on_chld(int s, siginfo_t *si, void *u) { (void)s; (void)u; chld_uid = (sig_atomic_t)si->si_uid; }

static int body_sigchld(void)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_chld;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGCHLD, &sa, NULL);
    if (setresgid(0, 0, 0) || setresuid(0, 0, 0)) return 2;   /* ns-root = kuid 100000 */
    pid_t k = fork();
    if (k == 0) _exit(0);
    for (int i = 0; i < 200 && chld_uid == -2; i++) usleep(10000);
    waitpid(k, NULL, 0);
    /* We are ns-root (kuid 100000): the handler must see 0, not 100000. */
    return chld_uid == 0 ? 0 : 3;
}
static void sigchld_si_uid_view(void) { run_ns_case("sigchld_si_uid_view", body_sigchld); }

/* ---- idroot cases ---- */
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
    report("ns_euid0_not_host", run_child(in_ns_all_refused));
}

static void case_host_root_unaffected(void)
{
    report("host_root_unaffected", run_child(host_all_allowed));
}

/* ---- lane subids: /etc/subuid, /etc/subgid, procsys id files ---- */
static int slurp_file(const char *path, char *buf, int cap)
{
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    int n = read(fd, buf, cap - 1);
    close(fd);
    if (n < 0) return -1;
    buf[n] = 0;
    return n;
}

static int write_succeeds(const char *path)
{
    int fd = open(path, O_WRONLY);
    if (fd < 0) return 0;
    int w = write(fd, "1\n", 2);
    close(fd);
    return w == 2;
}

static void subuid_present(void)
{
    const char *files[2] = { "/etc/subuid", "/etc/subgid" };
    int ok = 1;
    for (int i = 0; i < 2; i++) {
        char b[128];
        struct stat st;
        if (slurp_file(files[i], b, sizeof b) < 0 || strncmp(b, "leandro:100000:65536\n", 21) != 0) ok = 0; /* first line; root range may follow */
        else if (stat(files[i], &st) != 0 || st.st_uid != 0 || st.st_gid != 0 || (st.st_mode & 07777) != 0644) ok = 0;
    }
    report("subuid_present", ok);
}

static int procsys_is(const char *path, const char *want)
{
    char b[32];
    return slurp_file(path, b, sizeof b) >= 0 && strcmp(b, want) == 0 && !write_succeeds(path);
}

static void procsys_overflow(void)
{
    report("procsys_overflow",
           procsys_is("/proc/sys/kernel/overflowuid", "65534\n")
        && procsys_is("/proc/sys/kernel/overflowgid", "65534\n")
        && procsys_is("/proc/sys/user/max_user_namespaces", "255\n"));
}

/* ===== lane idcore: composed map cases ===== */
#define SKIP 77
#define CHECK(c) do { if (!(c)) { printf("  FAIL line %d: %s (errno %d)\n", __LINE__, #c, errno); return 1; } } while (0)

static int knob; /* non-identity maps accepted (idmap=test) */

static int wr(const char *path, const char *txt)
{
    int fd = open(path, O_WRONLY);
    if (fd < 0) return -errno;
    ssize_t n = write(fd, txt, strlen(txt));
    int e = errno;
    close(fd);
    return n == (ssize_t)strlen(txt) ? 0 : -e;
}
static int wrp(pid_t pid, const char *file, const char *txt)
{
    char p[64];
    snprintf(p, sizeof p, "/proc/%d/%s", (int)pid, file);
    return wr(p, txt);
}
static int rd(const char *path, char *buf, size_t sz)
{
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    ssize_t n = read(fd, buf, sz - 1);
    close(fd);
    if (n < 0) return -1;
    buf[n] = 0;
    return (int)n;
}
static int rdp(pid_t pid, const char *file, char *buf, size_t sz)
{
    char p[64];
    snprintf(p, sizeof p, "/proc/%d/%s", (int)pid, file);
    return rd(p, buf, sz);
}

/*
 * spawn: fork a child that becomes uid/gid `id` (skipped if id < 0), unshares
 * a user namespace, tells the parent, waits for "go", then runs fn(arg) and
 * exits with its result. Returns the pid (or -1); *go is the parent's write end.
 */
static pid_t spawn(int id, int (*fn)(void *), void *arg, int *go)
{
    int g[2], r[2];
    if (pipe(g) || pipe(r)) return -1;
    pid_t c = fork();
    if (c < 0) return -1;
    if (c == 0) {
        char b = 'r';
        close(g[1]); close(r[0]);
        if (id >= 0 && (setresgid(id, id, id) || setresuid(id, id, id))) _exit(98);
        if (unshare(CLONE_NEWUSER)) {
            b = 'x';
            (void)!write(r[1], &b, 1);
            _exit(99);
        }
        (void)!write(r[1], &b, 1);
        (void)!read(g[0], &b, 1);
        _exit(fn ? fn(arg) : 0);
    }
    close(g[0]); close(r[1]);
    char b = 0;
    if (read(r[0], &b, 1) != 1 || b != 'r') { close(r[0]); close(g[1]); waitpid(c, NULL, 0); return -1; }
    close(r[0]);
    *go = g[1];
    return c;
}
static int fin(pid_t c, int go)
{
    int st = 0;
    (void)!write(go, "g", 1);
    close(go);
    waitpid(c, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 100;
}

/* ---- flat cases: one user namespace, maps written by the runner ---------- */

static int c_ident(void *a)
{
    char m[256];
    (void)a;
    CHECK(getuid() == 1000 && geteuid() == 1000);
    CHECK(rd("/proc/self/uid_map", m, sizeof m) > 0);
    CHECK(strcmp(m, "      1000       1000          1\n") == 0);
    return 0;
}
static int map_identity_still_ok(void)
{
    int go; pid_t c = spawn(1000, c_ident, NULL, &go);
    CHECK(c > 0);
    int r1 = wrp(c, "uid_map", "1000 1000 1\n");
    int r2 = wrp(c, "gid_map", "1000 1000 1\n");
    int rc = fin(c, go);
    CHECK(r1 == 0 && r2 == 0);
    return rc;
}

static int map_overlap_lower_einval(void)
{
    int go; pid_t c = spawn(1000, NULL, NULL, &go);
    CHECK(c > 0);
    int a = wrp(c, "uid_map", "1000 1000 5\n2000 1002 1\n");   /* lower overlap */
    int b = wrp(c, "uid_map", "1000 1000 5\n1002 2000 1\n");   /* upper overlap */
    int d = wrp(c, "uid_map", "1000 1000 5\n1004 1004 1\n");   /* both */
    fin(c, go);
    CHECK(a == -EINVAL);
    CHECK(b == -EINVAL);
    CHECK(d == -EINVAL);
    return 0;
}

static int lower_minus1_einval(void)
{
    int go; pid_t c = spawn(1000, NULL, NULL, &go);
    CHECK(c > 0);
    int a = wrp(c, "uid_map", "1000 4294967295 1\n");
    int b = wrp(c, "uid_map", "4294967295 1000 1\n");
    int d = wrp(c, "uid_map", "1000 4294967294 5\n");          /* wraps */
    int e = wrp(c, "uid_map", "1000 1000 0\n");                /* empty */
    fin(c, go);
    CHECK(a == -EINVAL && b == -EINVAL && d == -EINVAL && e == -EINVAL);
    return 0;
}

static int map_kuid0_eperm(void)
{
    /* refused for the privileged writer too, with or without the knob */
    int go; pid_t c = spawn(1000, NULL, NULL, &go);
    CHECK(c > 0);
    int a = wrp(c, "uid_map", "0 0 1\n");
    int b = wrp(c, "uid_map", "1000 0 1\n");
    int d = wrp(c, "uid_map", "5 0 10\n");
    int e = wrp(c, "gid_map", "0 0 1\n");
    fin(c, go);
    CHECK(a == -EPERM && b == -EPERM && d == -EPERM && e == -EPERM);
    return 0;
}

/* ---- unprivileged writers (the process writes its own maps) -------------- */

static int c_unpriv_foreign(void *a)
{
    (void)a;
    CHECK(wr("/proc/self/uid_map", "1001 1001 1\n") == -EPERM);        /* foreign id */
    CHECK(wr("/proc/self/uid_map", "1000 1000 2\n") == -EPERM);        /* count 2 */
    CHECK(wr("/proc/self/uid_map", "1000 1000 1\n1001 1001 1\n") == -EPERM); /* two extents */
    CHECK(wr("/proc/self/uid_map", "1000 1000 1\n") == 0);             /* own id */
    return 0;
}
static int unpriv_foreign_eperm(void)
{
    int go; pid_t c = spawn(1000, c_unpriv_foreign, NULL, &go);
    CHECK(c > 0);
    return fin(c, go);
}

static int c_gid_deny(void *a)
{
    char m[64];
    (void)a;
    CHECK(wr("/proc/self/gid_map", "1000 1000 1\n") == -EPERM);        /* before deny */
    CHECK(wr("/proc/self/setgroups", "deny") == 0);
    CHECK(wr("/proc/self/gid_map", "1000 1000 1\n") == 0);
    CHECK(wr("/proc/self/setgroups", "allow") == -EPERM);              /* after gid_map */
    CHECK(rd("/proc/self/setgroups", m, sizeof m) > 0 && strcmp(m, "deny\n") == 0);
    return 0;
}
static int gid_before_deny_eperm(void)
{
    int go; pid_t c = spawn(1000, c_gid_deny, NULL, &go);
    CHECK(c > 0);
    return fin(c, go);
}

static int c_nested_unmapped(void *a)
{
    (void)a;
    errno = 0;
    CHECK(unshare(CLONE_NEWUSER) == -1 && errno == EPERM);  /* euid/egid unmapped */
    return 0;
}
static int nested_create_unmapped_eperm(void)
{
    int go; pid_t c = spawn(1000, c_nested_unmapped, NULL, &go);   /* no map written */
    CHECK(c > 0);
    return fin(c, go);
}

static int c_unpriv_root(void *x)
{
    (void)x;
    CHECK(wr("/proc/self/uid_map", "0 1000 1\n") == 0);
    CHECK(wr("/proc/self/setgroups", "deny") == 0);
    CHECK(wr("/proc/self/gid_map", "0 1000 1\n") == 0);
    CHECK(getuid() == 0 && geteuid() == 0 && getgid() == 0 && getegid() == 0);
    return 0;
}
static int unpriv_root_map(void)
{
    if (!knob) return SKIP;
    int go; pid_t c = spawn(1000, c_unpriv_root, NULL, &go);
    CHECK(c > 0);
    return fin(c, go);
}

/* ---- knob cases with one namespace -------------------------------------- */

#define MULTI "         0     100000         10\n        10     200000         10\n"
#define PODMAN "         0       1000          1\n         1     100000      65536\n"
static int c_multi(void *x)
{
    char m[256];
    (void)x;
    CHECK(rd("/proc/self/uid_map", m, sizeof m) > 0);
    CHECK(strcmp(m, MULTI) == 0);
    CHECK(getuid() == 65534);   /* kuid 1000 is not mapped */
    return 0;
}
static int priv_multi_extent(void)
{
    if (!knob) return SKIP;
    int go; pid_t c = spawn(1000, c_multi, NULL, &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", "0 100000 10\n10 200000 10\n") == 0);
    char m[256];
    CHECK(rdp(c, "uid_map", m, sizeof m) > 0);
    CHECK(strcmp(m, MULTI) == 0);
    return fin(c, go);
}

static int c_podman(void *x)
{
    char m[256];
    (void)x;
    CHECK(rd("/proc/self/uid_map", m, sizeof m) > 0);
    CHECK(strcmp(m, PODMAN) == 0);
    CHECK(strstr(m, "4294967295") == NULL);
    CHECK(rd("/proc/self/gid_map", m, sizeof m) > 0);
    CHECK(strcmp(m, PODMAN) == 0);
    CHECK(getuid() == 0 && getgid() == 0);
    return 0;
}
static int podman_map_text(void)
{
    if (!knob) return SKIP;
    int go; pid_t c = spawn(1000, c_podman, NULL, &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", "0 1000 1\n1 100000 65536\n") == 0);
    CHECK(wrp(c, "gid_map", "0 1000 1\n1 100000 65536\n") == 0);
    return fin(c, go);
}

/* ---- nested cases: A in ns1 (maps by the runner), B in ns2 under A -------- */

struct nest {
    const char *a_uid, *a_gid;   /* what the runner writes for A (ns1) */
    int a_deny;                  /* runner writes setgroups=deny for ns1 first */
    const char *b_uid_first;     /* A tries this first for B: must be EPERM (or NULL) */
    const char *b_uid, *b_gid;   /* what A writes for B (ns2); NULL = B writes its own */
    int (*b_fn)(void *);
    int (*a_check)(pid_t b);     /* A's checks while B is alive */
    int pipes;                   /* nested_view rendezvous with the runner */
};
static const struct nest *NEST;
static int tell[2], ack[2];

static int a_body(void *arg)
{
    const struct nest *n = NEST;
    int go; (void)arg;
    pid_t b = spawn(-1, n->b_fn, NULL, &go);
    CHECK(b > 0);
    if (n->b_uid_first) CHECK(wrp(b, "uid_map", n->b_uid_first) == -EPERM);
    if (n->b_uid) CHECK(wrp(b, "uid_map", n->b_uid) == 0);
    if (n->b_gid) CHECK(wrp(b, "gid_map", n->b_gid) == 0);
    if (n->a_check) { int r = n->a_check(b); if (r) { fin(b, go); return r; } }
    CHECK(fin(b, go) == 0);
    return 0;
}
static int run_nest(const struct nest *n, int need_knob)
{
    if (need_knob && !knob) return SKIP;
    NEST = n;
    if (n->pipes) CHECK(pipe(tell) == 0 && pipe(ack) == 0);
    int go; pid_t a = spawn(1000, a_body, NULL, &go);
    CHECK(a > 0);
    if (n->a_deny) CHECK(wrp(a, "setgroups", "deny") == 0);
    CHECK(wrp(a, "uid_map", n->a_uid) == 0);
    CHECK(wrp(a, "gid_map", n->a_gid) == 0);
    if (n->pipes) {
        char m[256];
        pid_t b = 0;
        (void)!write(go, "g", 1);  /* release A; it then makes B and reports its pid */
        close(go);
        CHECK(read(tell[0], &b, sizeof b) == (ssize_t)sizeof b);
        /* the runner reads ns2's map from the init namespace: kernel ids */
        CHECK(rdp(a, "uid_map", m, sizeof m) > 0);
        CHECK(strcmp(m, "         5       1000          1\n       100     200000         50\n") == 0);
        CHECK(rdp(b, "uid_map", m, sizeof m) > 0);
        CHECK(strcmp(m, "         0     200000         10\n") == 0);
        (void)!write(ack[1], "k", 1);
        int st = 0; waitpid(a, &st, 0);
        return WIFEXITED(st) ? WEXITSTATUS(st) : 100;
    }
    return fin(a, go);
}

static int b_nothing(void *x) { (void)x; return 0; }
static int split_parent_eperm(void)
{
    /* ns1 has two adjacent identity extents; a child range spanning both is EPERM */
    static const struct nest n = {
        "1000 1000 5\n1005 1005 5\n", "1000 1000 10\n", 0,
        "1000 1000 10\n", "1000 1000 5\n", NULL, b_nothing, NULL, 0 };
    return run_nest(&n, 0);
}

static int b_sg_inherited(void *x)
{
    char m[64];
    (void)x;
    CHECK(rd("/proc/self/setgroups", m, sizeof m) > 0 && strcmp(m, "deny\n") == 0);
    CHECK(wr("/proc/self/setgroups", "allow") == -EPERM);
    CHECK(wr("/proc/self/gid_map", "1000 1000 1\n") == 0);   /* no explicit deny needed */
    errno = 0;
    CHECK(setgroups(0, NULL) == -1 && errno == EPERM);
    return 0;
}
static int setgroups_deny_inherited(void)
{
    static const struct nest n = {
        "1000 1000 1\n", "1000 1000 1\n", 1, NULL, NULL, NULL, b_sg_inherited, NULL, 0 };
    return run_nest(&n, 0);
}

static int b_translated(void *x)
{
    char m[256];
    (void)x;
    /* unprivileged self-write: lower 5 is ns1's name for kuid 1000 == our euid */
    CHECK(wr("/proc/self/uid_map", "7 5 1\n") == 0);
    CHECK(wr("/proc/self/setgroups", "deny") == 0);
    CHECK(wr("/proc/self/gid_map", "7 5 1\n") == 0);
    CHECK(rd("/proc/self/uid_map", m, sizeof m) > 0);
    CHECK(strcmp(m, "         7          5          1\n") == 0);
    CHECK(getuid() == 7 && getgid() == 7);
    return 0;
}
static int unpriv_translated_lower(void)
{
    static const struct nest n = {
        "5 1000 1\n", "5 1000 1\n", 0, NULL, NULL, NULL, b_translated, NULL, 0 };
    return run_nest(&n, 1);
}

static int b_view(void *x)
{
    char m[256];
    (void)x;
    CHECK(rd("/proc/self/uid_map", m, sizeof m) > 0);
    CHECK(strcmp(m, "         0        100         10\n") == 0);   /* reader in ns2 -> ns1 names */
    return 0;
}
static int a_view_check(pid_t b)
{
    char m[256];
    CHECK(rdp(b, "uid_map", m, sizeof m) > 0);
    CHECK(strcmp(m, "         0        100         10\n") == 0);   /* ns1 reads its child */
    CHECK(getuid() == 5);
    (void)!write(tell[1], &b, sizeof b);                            /* runner reads from init */
    char k; CHECK(read(ack[0], &k, 1) == 1);
    return 0;
}
static int nested_view(void)
{
    static const struct nest n = {
        "5 1000 1\n100 200000 50\n", "5 1000 1\n", 0, NULL,
        "0 100 10\n", NULL, b_view, a_view_check, 1 };
    return run_nest(&n, 1);
}

/* ---- idcaps: capability transitions are relative to the namespace root --- */

#define CAP_EXIT_NONE 20
#define CAP_EXIT_SOME 21
static char selfexe[256] = "/bin/idmaptest";

/* "--capprobe" image: exit CAP_EXIT_NONE if effective and permitted are empty. */
static int capprobe(void)
{
    char b[1024];
    unsigned long long eff = ~0ULL, prm = ~0ULL;
    if (rd("/proc/self/status", b, sizeof b) <= 0) return 99;
    char *p = strstr(b, "CapEff:");
    if (p) eff = strtoull(p + 7, NULL, 16);
    p = strstr(b, "CapPrm:");
    if (p) prm = strtoull(p + 7, NULL, 16);
    return (eff == 0 && prm == 0) ? CAP_EXIT_NONE : CAP_EXIT_SOME;
}
static int cap_exec(void)
{
    execl(selfexe, selfexe, "--capprobe", (char *)NULL);
    return 98;
}
static int caps_empty_now(void) { return capprobe() == CAP_EXIT_NONE; }

/* Host root that gave up its uid but kept caps (KEEPCAPS): exec must drop them. */
static int exec_drops_caps_host(void)
{
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) {
        if (prctl(PR_SET_KEEPCAPS, 1)) _exit(90);
        if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)) _exit(91);
        if (caps_empty_now()) _exit(92);          /* KEEPCAPS must have kept them */
        _exit(cap_exec());
    }
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == CAP_EXIT_NONE);
    return 0;
}

#define CAPMAP "0 1000 1\n5 5 1\n"
static int c_exec_nsroot(void *a) { (void)a; return cap_exec(); }
static int exec_nsroot_keeps_caps(void)
{
    if (!knob) return SKIP;
    int go; pid_t c = spawn(1000, c_exec_nsroot, NULL, &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", CAPMAP) == 0);
    return fin(c, go) == CAP_EXIT_SOME ? 0 : 1;
}

static int c_setuid_drop(void *a)
{
    (void)a;
    CHECK(!caps_empty_now());
    CHECK(setresuid(5, 5, 5) == 0);
    CHECK(caps_empty_now());
    return 0;
}
static int setuid_drops_caps(void)
{
    if (!knob) return SKIP;
    int go; pid_t c = spawn(1000, c_setuid_drop, NULL, &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", CAPMAP) == 0);
    return fin(c, go);
}

static int c_exec_nonroot(void *a)
{
    (void)a;
    if (prctl(PR_SET_KEEPCAPS, 1)) return 90;
    if (setresuid(5, 5, 5)) return 91;
    if (caps_empty_now()) return 92;
    return cap_exec();
}
static int exec_nonroot_nocaps(void)
{
    if (!knob) return SKIP;
    int go; pid_t c = spawn(1000, c_exec_nonroot, NULL, &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", CAPMAP) == 0);
    return fin(c, go) == CAP_EXIT_NONE ? 0 : 1;
}

/* kuid 0 inside a namespace whose root is another kuid is not root there. */
static int kuid0_not_nsroot(void)
{
    if (!knob) return SKIP;
    int go; pid_t c = spawn(-1, c_exec_nsroot, NULL, &go);   /* stays kuid 0 */
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", CAPMAP) == 0);
    return fin(c, go) == CAP_EXIT_NONE ? 0 : 1;
}

/* ---- table -------------------------------------------------------------- */

/* ---- idxlate: ids coming in are translated (set*id, chown, setfs*id, groups) ---- */

#define MAP10 "0 100000 10\n"
static int xup[2], xdn[2];
static void x_sync(void) { char b = 's'; (void)!write(xup[1], &b, 1); (void)!read(xdn[0], &b, 1); }

/* Child (root, own user ns, maps "0 100000 10" written by us) runs fn, pauses
 * at x_sync() so check(pid, path) can look from ns 0, then exits. */
static int run_obs(int (*fn)(void *), int (*check)(pid_t, const char *), const char *path)
{
    if (!knob) return SKIP;
    if (pipe(xup) || pipe(xdn)) return 1;
    int go; pid_t c = spawn(-1, fn, (void *)path, &go);
    CHECK(c > 0);
    close(xup[1]); close(xdn[0]);
    CHECK(wrp(c, "uid_map", MAP10) == 0);
    CHECK(wrp(c, "gid_map", MAP10) == 0);
    (void)!write(go, "g", 1);
    close(go);
    char b = 0;
    int got = read(xup[0], &b, 1) == 1;
    int rc = got && check ? check(c, path) : 0;
    (void)!write(xdn[1], "d", 1);
    int st = 0; waitpid(c, &st, 0);
    CHECK(got);
    CHECK(rc == 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 100;
}

static int c_setuid_down(void *x)
{
    (void)x;
    CHECK(setuid(50) == -1 && errno == EINVAL);        /* unmapped */
    CHECK(setgid(5) == 0 && setuid(5) == 0);
    CHECK(getuid() == 5 && geteuid() == 5 && getgid() == 5);
    x_sync();
    return 0;
}
static int chk_owner_100005(pid_t c, const char *p)
{
    char q[64]; struct stat st;
    (void)p;
    snprintf(q, sizeof q, "/proc/%d", (int)c);
    if (stat(q, &st)) return 1;
    return st.st_uid == 100005 && st.st_gid == 100005 ? 0 : 2;
}
static int setuid_maps_down(void) { return run_obs(c_setuid_down, chk_owner_100005, NULL); }

static int c_chown_down(void *x)
{
    const char *p = x; struct stat st;
    int fd = open(p, O_CREAT | O_WRONLY, 0644);
    CHECK(fd >= 0);
    CHECK(chown(p, 50, -1) == -1 && errno == EINVAL);
    CHECK(chown(p, 5, 6) == 0);
    CHECK(stat(p, &st) == 0 && st.st_uid == 5 && st.st_gid == 6);
    CHECK(fchown(fd, 7, -1) == 0);
    CHECK(fstat(fd, &st) == 0 && st.st_uid == 7 && st.st_gid == 6);
    x_sync();
    close(fd);
    return 0;
}
static int chk_chown(pid_t c, const char *p)
{
    struct stat st; (void)c;
    if (stat(p, &st)) return 1;
    return st.st_uid == 100007 && st.st_gid == 100006 ? 0 : 2;
}
static int chown_maps_down(void)
{
    const char *p = "/tmp/idxlate-chown";
    unlink(p);
    int r = run_obs(c_chown_down, chk_chown, p);
    unlink(p);
    return r;
}

static int setfsuid(unsigned u) { return (int)syscall(SYS_setfsuid, u); }

static int c_fsuid(void *x)
{
    const char *p = x; struct stat st;
    CHECK(setfsuid(4) == 65534);                       /* old fsuid: kuid 0, unmapped here */
    CHECK(setfsuid(6) == 4);                           /* old fsuid as the caller sees it */
    CHECK(setfsuid(-1) == 6);
    CHECK(setfsuid(50) == 6);                          /* unmapped: unchanged */
    CHECK(setfsuid(-1) == 6);
    int fd = open(p, O_CREAT | O_WRONLY, 0644);
    CHECK(fd >= 0);
    CHECK(fstat(fd, &st) == 0 && st.st_uid == 6);
    x_sync();
    close(fd);
    return 0;
}
static int chk_fsuid(pid_t c, const char *p)
{
    struct stat st; (void)c;
    if (stat(p, &st)) return 1;
    return st.st_uid == 100006 ? 0 : 2;
}
static int setfsuid_ret_view(void)
{
    const char *p = "/tmp/idxlate-fsuid";
    unlink(p);
    int r = run_obs(c_fsuid, chk_fsuid, p);
    unlink(p);
    return r;
}

static int c_groups(void *x)
{
    gid_t g[4] = { 7, 8 }, o[4];
    (void)x;
    CHECK(setgroups(2, g) == 0);
    CHECK(getgroups(0, NULL) == 2);
    CHECK(getgroups(4, o) == 2 && o[0] == 7 && o[1] == 8);
    g[0] = 50;
    CHECK(setgroups(1, g) == -1 && errno == EINVAL);   /* unmapped */
    CHECK(getgroups(4, o) == 2 && o[0] == 7 && o[1] == 8);
    x_sync();
    return 0;
}
static int chk_groups(pid_t c, const char *p)
{
    char b[512]; (void)p;
    if (rdp(c, "status", b, sizeof b) <= 0) return 1;
    return strstr(b, "100007 100008") ? 0 : 2;
}
static int getgroups_view(void) { return run_obs(c_groups, chk_groups, NULL); }

/* No knob needed: an unmapped namespace takes no ids at all. */
static int c_unmapped_ids(void *x)
{
    const char *p = x;
    int fd = open(p, O_CREAT | O_WRONLY, 0644);
    CHECK(fd >= 0);
    close(fd);
    CHECK(chown(p, 5, -1) == -1 && errno == EINVAL);
    CHECK(chown(p, -1, 5) == -1 && errno == EINVAL);
    CHECK(chown(p, -1, -1) == 0);
    return 0;
}
static int chown_unmapped_einval(void)
{
    const char *p = "/tmp/idxlate-unmapped";
    unlink(p);
    int go; pid_t c = spawn(-1, c_unmapped_ids, (void *)p, &go);   /* no map written */
    CHECK(c > 0);
    int r = fin(c, go);
    unlink(p);
    return r;
}

static int c_sg_nomap(void *x)
{
    (void)x;
    CHECK(setgroups(0, NULL) == -1 && errno == EPERM);   /* gid_map not written yet */
    return 0;
}
static int setgroups_needs_gidmap(void)
{
    int go; pid_t c = spawn(-1, c_sg_nomap, NULL, &go);
    CHECK(c > 0);
    return fin(c, go);
}

static const struct { const char *name; int (*fn)(void); } cases[] = {
    { "map_identity_still_ok", map_identity_still_ok },
    { "map_overlap_lower_einval", map_overlap_lower_einval },
    { "lower_minus1_einval", lower_minus1_einval },
    { "split_parent_eperm", split_parent_eperm },
    { "unpriv_foreign_eperm", unpriv_foreign_eperm },
    { "unpriv_translated_lower", unpriv_translated_lower },
    { "gid_before_deny_eperm", gid_before_deny_eperm },
    { "setgroups_deny_inherited", setgroups_deny_inherited },
    { "nested_create_unmapped_eperm", nested_create_unmapped_eperm },
    { "map_kuid0_eperm", map_kuid0_eperm },
    { "priv_multi_extent", priv_multi_extent },
    { "nested_view", nested_view },
    { "podman_map_text", podman_map_text },
    { "unpriv_root_map", unpriv_root_map },
<<<<<<< HEAD
    { "chown_unmapped_einval", chown_unmapped_einval },
    { "setgroups_needs_gidmap", setgroups_needs_gidmap },
    { "setuid_maps_down", setuid_maps_down },
    { "chown_maps_down", chown_maps_down },
    { "setfsuid_ret_view", setfsuid_ret_view },
    { "getgroups_view", getgroups_view },
=======
    { "exec_drops_caps_host", exec_drops_caps_host },
    { "exec_nsroot_keeps_caps", exec_nsroot_keeps_caps },
    { "setuid_drops_caps", setuid_drops_caps },
    { "exec_nonroot_nocaps", exec_nonroot_nocaps },
    { "kuid0_not_nsroot", kuid0_not_nsroot },
>>>>>>> main
};

static int probe_knob(void)
{
    int go; pid_t c = spawn(1000, NULL, NULL, &go);
    if (c < 0) return 0;
    int ok = wrp(c, "uid_map", "7 1000 1\n") == 0;
    fin(c, go);
    return ok;
}


/* idcore: run the composed-map cases; they report through report()/skip(). */
static void idcore_cases(void)
{
    knob = probe_knob();
    printf("idmaptest: non-identity maps %s\n", knob ? "ON (idmap=test)" : "off (non-identity cases SKIP)");
    for (unsigned i = 0; i < sizeof cases / sizeof cases[0]; i++) {
        fflush(stdout);
        pid_t p = fork();
        if (p == 0) _exit(cases[i].fn());
        int st = 0; waitpid(p, &st, 0);
        int rc = WIFEXITED(st) ? WEXITSTATUS(st) : 100;
        if (rc == SKIP) skip(cases[i].name, "non-identity map refused (no idmap=test)");
        else report(cases[i].name, rc == 0);
    }
}

int main(int argc, char **argv)
{
    if (argc > 1 && strcmp(argv[1], "--capprobe") == 0) return capprobe();
    {
        ssize_t n = readlink("/proc/self/exe", selfexe, sizeof selfexe - 1);
        if (n > 0) selfexe[n] = 0;
    }
    statx_matches_stat();
    proc_pid_owner();
    status_groups_view();
    stat_unmapped_65534();
    sigchld_si_uid_view();
    case_ns_euid0_not_host();
    case_host_root_unaffected();
    subuid_present();
    procsys_overflow();
    idcore_cases();
    printf("idmaptest: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
