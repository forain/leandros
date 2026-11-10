/*
 * idmaptest -- user-namespace uid/gid map behaviour. Static zig-cc build,
 * runs as root; each case prints "case: PASS|FAIL|SKIP". Non-identity map
 * cases SKIP when the map write returns EPERM (kernel booted without
 * idmap=off) and must PASS by default. Lanes append cases as separate functions.
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
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <sys/un.h>
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
static int slurp_file(const char *path, char *buf, int cap);

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
        prctl(PR_SET_DUMPABLE, 1);   /* the id change cleared it; Linux shows root then */
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

/* ---- proc_owner_newuser: /proc/<child> of a fresh userns child ---------- */
/* newuidmap fstats /proc/<pid> and wants st_uid == its real uid (1000) before
 * the child's uid_map is written. */
static int owner_newuser_body(void)
{
    int a[2], b[2];
    if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)) return 1;
    pipe(a); pipe(b);
    pid_t c = fork();
    if (c == 0) {
        char x = 0;
        if (unshare(CLONE_NEWUSER)) { x = 'u'; write(b[1], &x, 1); _exit(1); }
        prctl(PR_SET_DUMPABLE, 1);
        write(b[1], &x, 1);
        read(a[0], &x, 1);
        _exit(0);
    }
    char x;
    int rc = 0;
    if (read(b[0], &x, 1) != 1 || x) rc = 2;
    else {
        char p[64]; struct stat st;
        snprintf(p, sizeof p, "/proc/%d", c);
        if (stat(p, &st) || st.st_uid != 1000 || st.st_gid != 1000) { rc = 3; printf("stat uid=%u gid=%u\n", st.st_uid, st.st_gid); }
        int fd = open(p, O_RDONLY | O_DIRECTORY);
        if (fd < 0 || fstat(fd, &st) || st.st_uid != 1000 || st.st_gid != 1000) { rc = rc ? rc : 4; printf("fstat fd=%d uid=%u\n", fd, st.st_uid); }
        int mfd = fd < 0 ? -1 : openat(fd, "uid_map", O_RDONLY);
        if (mfd < 0 || fstat(mfd, &st) || st.st_uid != 1000) rc = rc ? rc : 5;
        if (mfd >= 0) close(mfd);
        if (fd < 0 || fstatat(fd, "uid_map", &st, 0) || st.st_uid != 1000 || st.st_gid != 1000) rc = rc ? rc : 6;
        if (fd < 0 || fstatat(fd, ".", &st, 0) || st.st_uid != 1000) rc = rc ? rc : 7;
        if (fd >= 0) close(fd);
    }
    write(a[1], "x", 1);
    waitpid(c, NULL, 0);
    return rc;
}
static void proc_owner_newuser(void)
{
    pid_t m = fork();
    if (m == 0) _exit(owner_newuser_body());
    int st; waitpid(m, &st, 0);
    if (WIFEXITED(st) && WEXITSTATUS(st)) printf("proc_owner_newuser rc=%d\n", WEXITSTATUS(st));
    report("proc_owner_newuser", WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

/* A non-dumpable task shows root (task_dump_owner). */
static void proc_owner_nondumpable(void)
{
    int sync[2];
    pipe(sync);
    pid_t c = fork();
    if (c == 0) {
        close(sync[1]);
        if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)) _exit(1);
        prctl(PR_SET_DUMPABLE, 0);
        char b; read(sync[0], &b, 1);
        _exit(0);
    }
    close(sync[0]);
    usleep(200000);
    char p[32]; struct stat st;
    snprintf(p, sizeof p, "/proc/%d", c);
    int ok = stat(p, &st) == 0 && st.st_uid == 0 && st.st_gid == 0;
    close(sync[1]);
    waitpid(c, NULL, 0);
    report("proc_owner_nondumpable", ok);
}

/* ---- newuidmap_e2e: the setuid shadow helper writes a 2-extent map -------- */
static int newuidmap_body(void)
{
    int a[2], b[2];
    if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)) return 1;
    pipe(a); pipe(b);
    pid_t c = fork();
    if (c == 0) {
        char x = 0;
        if (unshare(CLONE_NEWUSER)) { x = 'u'; write(b[1], &x, 1); _exit(1); }
        prctl(PR_SET_DUMPABLE, 1);
        write(b[1], &x, 1);
        read(a[0], &x, 1);
        _exit(0);
    }
    char x; int rc = 0;
    if (read(b[0], &x, 1) != 1 || x) rc = 2;
    else {
        char pid[16];
        snprintf(pid, sizeof pid, "%d", c);
        pid_t h = fork();
        if (h == 0) {
            execl("/usr/bin/newuidmap", "newuidmap", pid, "0", "1000", "1", "1", "100000", "65536", (char *)NULL);
            _exit(127);
        }
        int st; waitpid(h, &st, 0);
        if (!WIFEXITED(st) || WEXITSTATUS(st)) rc = 10 + (WIFEXITED(st) ? WEXITSTATUS(st) : 99);
        else {
            char p[48], buf[256];
            snprintf(p, sizeof p, "/proc/%d/uid_map", c);
            if (slurp_file(p, buf, sizeof buf) < 0 || !strstr(buf, "100000") || !strstr(buf, "65536")) rc = 3;
        }
    }
    write(a[1], "x", 1);
    waitpid(c, NULL, 0);
    return rc;
}
static void newuidmap_e2e(void)
{
    struct stat st;
    if (stat("/usr/bin/newuidmap", &st)) { skip("newuidmap_e2e", "no /usr/bin/newuidmap"); return; }
    char cl[1024];
    if (slurp_file("/proc/cmdline", cl, sizeof cl) < 0 || strstr(cl, "idmap=off")) { skip("newuidmap_e2e", "idmap=off boot"); return; }
    pid_t m = fork();
    if (m == 0) _exit(newuidmap_body());
    int s; waitpid(m, &s, 0);
    int rc = WIFEXITED(s) ? WEXITSTATUS(s) : 99;
    if (rc) printf("newuidmap_e2e rc=%d\n", rc);
    report("newuidmap_e2e", rc == 0);
}


/* ---- e2e_subuid: newuidmap/newgidmap with /etc/subuid build 0->1000, 1->100000.. */
static char e2e_path[64];
static int e2e_body(void)
{
    int a[2], b[2];
    if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)) return 1;
    pipe(a); pipe(b);
    pid_t c = fork();
    if (c == 0) {
        char x = 0;
        if (unshare(CLONE_NEWUSER)) { x = 'u'; write(b[1], &x, 1); _exit(1); }
        prctl(PR_SET_DUMPABLE, 1);
        write(b[1], &x, 1);
        read(a[0], &x, 1);                 /* maps written */
        struct stat st;
        if (getuid() != 0 || getgid() != 0) _exit(20);
        if (setresgid(2, 2, 2) || setresuid(2, 2, 2)) _exit(21);
        if (getuid() != 2 || getgid() != 2) _exit(22);
        int fd = open(e2e_path, O_CREAT | O_WRONLY | O_EXCL, 0644);
        if (fd < 0) _exit(23);
        close(fd);
        if (stat(e2e_path, &st) || st.st_uid != 2 || st.st_gid != 2) _exit(24);
        _exit(0);
    }
    char x; int rc = 0;
    if (read(b[0], &x, 1) != 1 || x) rc = 2;
    else {
        char pid[16];
        snprintf(pid, sizeof pid, "%d", c);
        const char *tool[2] = { "/usr/bin/newuidmap", "/usr/bin/newgidmap" };
        for (int i = 0; i < 2 && !rc; i++) {
            pid_t h = fork();
            if (h == 0) {
                execl(tool[i], tool[i], pid, "0", "1000", "1", "1", "100000", "65536", (char *)NULL);
                _exit(127);
            }
            int st; waitpid(h, &st, 0);
            if (!WIFEXITED(st) || WEXITSTATUS(st)) rc = 10 + i * 10 + (WIFEXITED(st) ? WEXITSTATUS(st) : 99) % 10;
        }
    }
    write(a[1], "x", 1);
    int st; waitpid(c, &st, 0);
    if (!rc && (!WIFEXITED(st) || WEXITSTATUS(st))) rc = 40 + (WIFEXITED(st) ? WEXITSTATUS(st) : 99);
    if (!rc) {
        struct stat s;   /* ns 0 reads kernel ids: ns-uid 2 is kuid 100001 (1->100000) */
        if (stat(e2e_path, &s) || s.st_uid != 100001 || s.st_gid != 100001) { printf("e2e_subuid: init sees %u:%u\n", (unsigned)s.st_uid, (unsigned)s.st_gid); rc = 3; }
    }
    return rc;
}
static void e2e_subuid(void)
{
    struct stat st;
    if (stat("/usr/bin/newuidmap", &st) || stat("/usr/bin/newgidmap", &st)) { skip("e2e_subuid", "no newuidmap/newgidmap"); return; }
    char cl[1024];
    if (slurp_file("/proc/cmdline", cl, sizeof cl) < 0 || strstr(cl, "idmap=off")) { skip("e2e_subuid", "idmap=off boot"); return; }
    snprintf(e2e_path, sizeof e2e_path, "/tmp/e2e_subuid.%d", (int)getpid());
    unlink(e2e_path);
    pid_t m = fork();
    if (m == 0) _exit(e2e_body());
    int s; waitpid(m, &s, 0);
    int rc = WIFEXITED(s) ? WEXITSTATUS(s) : 99;
    unlink(e2e_path);
    if (rc) printf("e2e_subuid rc=%d\n", rc);
    report("e2e_subuid", rc == 0);
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
    if (m == 'n') { skip(name, "non-identity map refused (idmap=off)"); return 0; }
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
        && procsys_is("/proc/sys/user/max_user_namespaces", "32\n"));
}

/* ===== lane idcore: composed map cases ===== */
#define SKIP 77
#define CHECK(c) do { if (!(c)) { printf("  FAIL line %d: %s (errno %d)\n", __LINE__, #c, errno); return 1; } } while (0)

static int knob; /* non-identity maps accepted (default; refused only with idmap=off) */

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


/* ---- nested_map_composed: ns2 "0 100 10" under ns1 "100 200000 50" ------ */
static char nc_new[64], nc_pre[64];
static int nc_b(void *x)
{
    struct stat st;
    (void)x;
    CHECK(setresgid(3, 3, 3) == 0 && setresuid(3, 3, 3) == 0);
    int fd = open(nc_new, O_CREAT | O_WRONLY | O_EXCL, 0644);
    CHECK(fd >= 0);
    close(fd);
    CHECK(stat(nc_new, &st) == 0 && st.st_uid == 3 && st.st_gid == 3);   /* created: ns2 3 */
    CHECK(stat(nc_pre, &st) == 0 && st.st_uid == 7 && st.st_gid == 7);   /* kuid 200007 */
    return 0;
}
static int nc_a(void *x)
{
    struct stat st;
    int go; (void)x;
    pid_t b = spawn(-1, nc_b, NULL, &go);
    CHECK(b > 0);
    CHECK(wrp(b, "uid_map", "0 100 10\n") == 0);
    CHECK(wrp(b, "gid_map", "0 100 10\n") == 0);
    CHECK(fin(b, go) == 0);
    CHECK(stat(nc_new, &st) == 0 && st.st_uid == 103 && st.st_gid == 103);   /* ns1 names */
    CHECK(stat(nc_pre, &st) == 0 && st.st_uid == 107 && st.st_gid == 107);
    return 0;
}
static int nested_map_composed(void)
{
    struct stat st;
    if (!knob) return SKIP;
    snprintf(nc_new, sizeof nc_new, "/tmp/nmc_new.%d", (int)getpid());
    snprintf(nc_pre, sizeof nc_pre, "/tmp/nmc_pre.%d", (int)getpid());
    unlink(nc_new); unlink(nc_pre);
    int fd = open(nc_pre, O_CREAT | O_WRONLY, 0644);
    CHECK(fd >= 0);
    CHECK(fchown(fd, 200007, 200007) == 0);
    close(fd);
    int go; pid_t a = spawn(1000, nc_a, NULL, &go);
    CHECK(a > 0);
    CHECK(wrp(a, "uid_map", "5 1000 1\n100 200000 50\n") == 0);
    CHECK(wrp(a, "gid_map", "5 1000 1\n100 200000 50\n") == 0);
    int rc = fin(a, go);
    int ok = stat(nc_new, &st) == 0 && st.st_uid == 200003 && st.st_gid == 200003;   /* init: kernel ids */
    unlink(nc_new); unlink(nc_pre);
    CHECK(rc == 0);
    CHECK(ok);
    return 0;
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


/* ---- idsig: signals, rlimits, priorities and /proc/<pid>/ns/* across user namespaces -- */

/* Fork a helper that waits for `go`, becomes `uid` (>= 0) and then sleeps; writes 'r' when ready. */
static pid_t spawn_sleeper(int uid, int *go)
{
    int g[2], r[2];
    if (pipe(g) || pipe(r)) return -1;
    pid_t c = fork();
    if (c < 0) return -1;
    if (c == 0) {
        char b = 'r';
        close(g[1]); close(r[0]);
        if (uid >= 0 && (setresgid(uid, uid, uid) || setresuid(uid, uid, uid))) _exit(98);
        (void)!write(r[1], &b, 1);
        (void)!read(g[0], &b, 1);
        _exit(0);
    }
    close(g[0]); close(r[1]);
    char b = 0;
    int ok = read(r[0], &b, 1) == 1 && b == 'r';
    close(r[0]);
    if (!ok) { close(g[1]); kill(c, SIGKILL); waitpid(c, NULL, 0); return -1; }
    *go = g[1];
    return c;
}
static void reap_sleeper(pid_t c, int go) { close(go); kill(c, SIGKILL); waitpid(c, NULL, 0); }

/* In ns N1 (root = kuid 1000, "1 100000 65536"): a child of ours that setuid(1) is kuid 100000. */
static pid_t fork_subuid_child(int *go)
{
    int g[2], r[2];
    if (pipe(g) || pipe(r)) return -1;
    pid_t c = fork();
    if (c < 0) return -1;
    if (c == 0) {
        char b = 'r';
        close(g[1]); close(r[0]);
        if (setresuid(1, 1, 1)) _exit(98);
        /* setresuid clears dumpable; Linux __ptrace_may_access then needs CAP_SYS_PTRACE in mm->user_ns
         * (the init ns here: never exec'd inside the userns), so opt in like a real exec would. */
        prctl(PR_SET_DUMPABLE, 1);
        (void)!write(r[1], &b, 1);
        (void)!read(g[0], &b, 1);
        _exit(0);
    }
    close(g[0]); close(r[1]);
    char b = 0;
    int ok = read(r[0], &b, 1) == 1 && b == 'r';
    close(r[0]);
    if (!ok) { close(g[1]); kill(c, SIGKILL); waitpid(c, NULL, 0); return -1; }
    *go = g[1];
    return c;
}

static int c_kill_subuid(void *a)
{
    (void)a;
    int go; pid_t t = fork_subuid_child(&go);
    CHECK(t > 0);
    CHECK(kill(t, 0) == 0);                 /* CAP_KILL in the target's userns */
    CHECK(kill(t, SIGKILL) == 0);
    int st = 0;
    CHECK(waitpid(t, &st, 0) == t);
    close(go);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL);
    return 0;
}
static int kill_subuid_capkill_ns(void)
{
    if (!knob) return SKIP;
    int go; pid_t c = spawn(1000, c_kill_subuid, NULL, &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", PODMAN) == 0);
    return fin(c, go);
}

/* CAP_KILL in a child namespace is no power over a host task, even one whose numeric id (0) matches. */
static int c_kill_host(void *a)
{
    pid_t h = *(pid_t *)a;
    CHECK(kill(h, 0) == -1 && errno == EPERM);
    CHECK(kill(h, SIGKILL) == -1 && errno == EPERM);
    errno = 0;
    CHECK(setpriority(PRIO_PROCESS, h, 5) == -1 && errno == EPERM);
    return 0;
}
static int kill_host_same_kuid_denied(void)
{
    int hgo; pid_t h = spawn_sleeper(-1, &hgo);          /* host root */
    CHECK(h > 0);
    int go; pid_t c = spawn(1000, c_kill_host, &h, &go);   /* no map needed: caps exist in N1 */
    CHECK(c > 0);
    int r = fin(c, go);
    reap_sleeper(h, hgo);
    return r;
}

static int c_prlimit_child(void *a)
{
    pid_t h = *(pid_t *)a;
    struct rlimit rl;
    int go; pid_t t = fork_subuid_child(&go);
    CHECK(t > 0);
    CHECK(prlimit(t, RLIMIT_NOFILE, NULL, &rl) == 0);       /* CAP_SYS_PTRACE in the target's userns */
    CHECK(setpriority(PRIO_PROCESS, t, 5) == 0);            /* CAP_SYS_NICE likewise */
    close(go); kill(t, SIGKILL); waitpid(t, NULL, 0);
    CHECK(prlimit(h, RLIMIT_NOFILE, NULL, &rl) == -1 && errno == EPERM);   /* host root: no */
    return 0;
}
static int prlimit_ns_child(void)
{
    if (!knob) return SKIP;
    int hgo; pid_t h = spawn_sleeper(-1, &hgo);
    CHECK(h > 0);
    int go; pid_t c = spawn(1000, c_prlimit_child, &h, &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", PODMAN) == 0);
    int r = fin(c, go);
    reap_sleeper(h, hgo);
    return r;
}

/* ns-root opens the ns file of a task it is privileged over (a mapped sub-uid). */
static int c_opens_ns_mapped(void *a)
{
    (void)a;
    int go; pid_t t = fork_subuid_child(&go);
    CHECK(t > 0);
    char p[64];
    snprintf(p, sizeof p, "/proc/%d/ns/net", (int)t);
    int fd = open(p, O_RDONLY);
    CHECK(fd >= 0);
    close(fd);
    close(go); kill(t, SIGKILL); waitpid(t, NULL, 0);
    return 0;
}
static int nsroot_opens_ns_of_mapped_user(void)
{
    if (!knob) return SKIP;
    int go; pid_t c = spawn(1000, c_opens_ns_mapped, NULL, &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", PODMAN) == 0);
    return fin(c, go);
}

/* An unprivileged user cannot open root's /proc/1/ns/*; its own is fine. */
static int ns_open_needs_ptrace_access(void)
{
    if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)) return 98;
    int fd = open("/proc/1/ns/net", O_RDONLY);
    CHECK(fd < 0 && errno == EACCES);
    fd = open("/proc/self/ns/net", O_RDONLY);
    CHECK(fd >= 0);
    close(fd);
    return 0;
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
    prctl(PR_SET_DUMPABLE, 1);   /* the id change cleared it; /proc/<pid> shows root until then */
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
    int fd = open(p, O_WRONLY);   /* created by the parent: kuid 0 is unmapped here, create => EOVERFLOW */
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
    int pf = open(p, O_CREAT | O_WRONLY, 0644);
    if (pf < 0) return 1;
    close(pf);
    if (knob && chown(p, 100000, 100000)) { unlink(p); return 1; }
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
    CHECK(syscall(SYS_setfsgid, 6) >= 0);              /* fsgid kuid 0 is unmapped too: create => EOVERFLOW */
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
    const char *p = x;   /* created by the parent: an unmapped creator gets EOVERFLOW */
    CHECK(chown(p, 5, -1) == -1 && errno == EINVAL);
    CHECK(chown(p, -1, 5) == -1 && errno == EINVAL);
    CHECK(chown(p, -1, -1) == 0);
    return 0;
}
static int chown_unmapped_einval(void)
{
    const char *p = "/tmp/idxlate-unmapped";
    unlink(p);
    { int fd = open(p, O_CREAT | O_WRONLY, 0644); if (fd >= 0) close(fd); }
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


/* ---- idperm-x: filesystem permission gates (tmpfs /tmp) ------------------ */

#define MAP100 "0 100000 100\n"
#define PXD "/tmp/idpx"

static int px_setx(const char *p, const char *n, const char *v, size_t l)
{ return (int)syscall(SYS_setxattr, p, n, v, l, 0); }
static int px_getx(const char *p, const char *n)
{ char b[64]; return (int)syscall(SYS_getxattr, p, n, b, sizeof b); }
static int px_listhas(const char *p, const char *pfx)
{
    char b[512]; long n = syscall(SYS_listxattr, p, b, sizeof b);
    for (long i = 0; i < n; i += strlen(b + i) + 1) if (!strncmp(b + i, pfx, strlen(pfx))) return 1;
    return 0;
}
/* Host root makes a file/dir owned by kuid u:g (raw, ns 0). */
static int px_mk(const char *p, int dir, unsigned u, unsigned g, unsigned mode)
{
    if (dir) { if (mkdir(p, 0700) && errno != EEXIST) return -1; }
    else { int fd = open(p, O_CREAT | O_WRONLY, 0600); if (fd < 0) return -1; close(fd); }
    return chown(p, u, g) || chmod(p, mode) ? -1 : 0;
}
static void px_clean(void)
{
    (void)!system("rm -rf " PXD);
    mkdir(PXD, 0777); chmod(PXD, 0777);
}
/* ns child: becomes ns root (kuid 100000 once the map is written) */
static int px_nsroot(void) { return setresgid(0, 0, 0) || setresuid(0, 0, 0) ? 1 : 0; }

typedef int (*pxfn)(void);
static pxfn px_cur;
static int c_px(void *a) { (void)a; return px_cur(); }
/* Run fn in a fresh user ns of a kuid-0 child; maps (if any) written by us. */
static int px_run(pxfn fn, const char *map)
{
    px_cur = fn;
    int go; pid_t c = spawn(-1, c_px, NULL, &go);
    CHECK(c > 0);
    if (map) { CHECK(wrp(c, "uid_map", map) == 0); CHECK(wrp(c, "gid_map", map) == 0); }
    return fin(c, go);
}
static int px_hostuser(pxfn f)
{
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) {
        if (setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000)) _exit(97);
        _exit(f());
    }
    int st = 0; waitpid(c, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 100;
}

/* -- host root is unaffected -- */
static int host_root_fs_unaffected(void)
{
    struct timespec ts[2] = { { 5, 0 }, { 6, 0 } };
    struct stat st;
    px_clean();
    CHECK(px_mk(PXD "/f", 0, 1000, 1000, 0600) == 0);
    CHECK(px_mk(PXD "/d", 1, 1000, 1000, 0700) == 0);
    CHECK(px_mk(PXD "/d/g", 0, 1000, 1000, 0000) == 0);
    int fd = open(PXD "/f", O_RDWR); CHECK(fd >= 0); CHECK(write(fd, "x", 1) == 1); close(fd);
    fd = open(PXD "/d/g", O_RDWR); CHECK(fd >= 0); close(fd);
    CHECK(chmod(PXD "/f", 0640) == 0);
    CHECK(chown(PXD "/f", 7, 8) == 0);
    CHECK(utimensat(AT_FDCWD, PXD "/f", ts, 0) == 0);
    CHECK(stat(PXD "/f", &st) == 0 && st.st_uid == 7 && st.st_gid == 8 && (st.st_mode & 0777) == 0640);
    CHECK(px_setx(PXD "/f", "trusted.t", "1", 1) == 0);
    CHECK(px_getx(PXD "/f", "trusted.t") == 1 && px_listhas(PXD "/f", "trusted."));
    CHECK(px_setx(PXD "/f", "security.s", "1", 1) == 0);
    CHECK(unlink(PXD "/d/g") == 0);
    return 0;
}

/* -- trusted.* is host root only -- */
static int c_trusted(void)
{
    CHECK(px_setx(PXD "/t", "trusted.t", "2", 1) == -1 && errno == EPERM);
    CHECK(px_getx(PXD "/t", "trusted.t") == -1 && errno == ENODATA);
    CHECK(!px_listhas(PXD "/t", "trusted."));
    return 0;
}
static int trusted_xattr_host_only(void)
{
    px_clean();
    CHECK(px_mk(PXD "/t", 0, 0, 0, 0666) == 0);
    CHECK(px_setx(PXD "/t", "trusted.t", "1", 1) == 0);
    CHECK(px_getx(PXD "/t", "trusted.t") == 1 && px_listhas(PXD "/t", "trusted."));
    CHECK(px_hostuser(c_trusted) == 0);
    /* kuid 0 in a fresh user namespace holds caps there, but is not host root */
    return px_run(c_trusted, NULL);
}

/* -- security.capability needs the host: EPERM in ns != 0 -- */
static const char FCAP[20] = { 0, 0, 0, 2 };   /* VFS_CAP_REVISION_2, no caps */
static int c_fcap_ns(void)
{
    CHECK(px_setx(PXD "/c", "security.capability", FCAP, sizeof FCAP) == -1 && errno == EPERM);
    CHECK(px_setx(PXD "/c", "security.other", "1", 1) == -1 && errno == EPERM);
    return 0;
}
static int fcap_xattr_eperm_in_ns(void)
{
    px_clean();
    CHECK(px_mk(PXD "/c", 0, 0, 0, 0666) == 0);
    CHECK(px_setx(PXD "/c", "security.other", "1", 1) == 0);   /* host root still may */
    return px_run(c_fcap_ns, NULL);
}

/* -- creating a file whose owner is unmapped: EOVERFLOW -- */
static int c_create_unmapped(void)
{
    int fd = open(PXD "/n", O_CREAT | O_WRONLY, 0644);
    CHECK(fd < 0 && errno == EOVERFLOW);
    CHECK(mkdir(PXD "/nd", 0755) == -1 && errno == EOVERFLOW);
    CHECK(symlink("x", PXD "/ns") == -1 && errno == EOVERFLOW);
    return 0;
}
static int create_unmapped_eoverflow(void)
{
    px_clean();
    int r = px_run(c_create_unmapped, NULL);
    struct stat st;
    CHECK(stat(PXD "/n", &st) == -1 && stat(PXD "/nd", &st) == -1);
    return r;
}

/* -- ns root (kuid 100000, caps in the ns) over mapped / unmapped inodes -- */
static int c_chown_mapped(void)
{
    struct stat st;
    CHECK(px_nsroot() == 0);
    CHECK(chown(PXD "/f", 5, 42) == 0);
    CHECK(stat(PXD "/f", &st) == 0 && st.st_uid == 5 && st.st_gid == 42);
    return 0;
}
static int nsroot_chown_mapped(void)
{
    if (!knob) return SKIP;
    px_clean();
    CHECK(px_mk(PXD "/f", 0, 100000, 100000, 0644) == 0);
    CHECK(px_run(c_chown_mapped, MAP100) == 0);
    struct stat st;
    CHECK(stat(PXD "/f", &st) == 0 && st.st_uid == 100005 && st.st_gid == 100042);
    return 0;
}

static int c_chmod_sub(void)
{
    struct stat st;
    CHECK(px_nsroot() == 0);
    CHECK(chmod(PXD "/f", 0644) == 0);
    CHECK(stat(PXD "/f", &st) == 0 && (st.st_mode & 07777) == 0644);
    return 0;
}
static int nsroot_chmod_subuid_file(void)
{
    if (!knob) return SKIP;
    px_clean();
    CHECK(px_mk(PXD "/f", 0, 100007, 100007, 0600) == 0);
    return px_run(c_chmod_sub, MAP100);
}

static int c_write_sub(void)
{
    CHECK(px_nsroot() == 0);
    int fd = open(PXD "/d/f", O_RDWR); CHECK(fd >= 0);
    CHECK(write(fd, "hi", 2) == 2); close(fd);
    fd = open(PXD "/d/new", O_CREAT | O_WRONLY, 0600); CHECK(fd >= 0); close(fd);
    CHECK(unlink(PXD "/d/f") == 0);
    return 0;
}
static int nsroot_write_in_subuid_0700_dir(void)
{
    if (!knob) return SKIP;
    px_clean();
    CHECK(px_mk(PXD "/d", 1, 100003, 100003, 0700) == 0);
    CHECK(px_mk(PXD "/d/f", 0, 100003, 100003, 0600) == 0);
    return px_run(c_write_sub, MAP100);
}

static int c_untouchable(void)
{
    CHECK(px_nsroot() == 0);
    CHECK(open(PXD "/u", O_RDWR) == -1 && errno == EACCES);
    CHECK(open(PXD "/u", O_RDONLY) == -1 && errno == EACCES);
    CHECK(chmod(PXD "/u", 0777) == -1 && errno == EPERM);
    CHECK(chown(PXD "/u", 5, -1) == -1 && errno == EPERM);
    CHECK(open(PXD "/ud/f", O_RDONLY) == -1 && errno == EACCES);
    CHECK(unlink(PXD "/ud/f") == -1 && errno == EACCES);
    CHECK(open(PXD "/ud/new", O_CREAT | O_WRONLY, 0600) == -1 && errno == EACCES);
    CHECK(chmod(PXD "/r", 0777) == -1 && errno == EPERM);   /* host-root-owned */
    CHECK(px_setx(PXD "/u", "user.x", "1", 1) == -1);
    CHECK(px_setx(PXD "/r", "trusted.x", "1", 1) == -1 && errno == EPERM);
    return 0;
}
static int nsroot_cannot_touch_unmapped(void)
{
    if (!knob) return SKIP;
    px_clean();
    CHECK(px_mk(PXD "/u", 0, 2000, 2000, 0600) == 0);
    CHECK(px_mk(PXD "/ud", 1, 2000, 2000, 0700) == 0);
    CHECK(px_mk(PXD "/ud/f", 0, 2000, 2000, 0600) == 0);
    CHECK(px_mk(PXD "/r", 0, 0, 0, 0644) == 0);
    return px_run(c_untouchable, MAP100);
}

static int c_hu_subuid(void)
{
    CHECK(open(PXD "/f", O_RDWR) == -1 && errno == EACCES);
    CHECK(chmod(PXD "/f", 0666) == -1 && errno == EPERM);
    CHECK(chown(PXD "/f", 1000, -1) == -1 && errno == EPERM);
    CHECK(open(PXD "/d/f", O_RDONLY) == -1 && errno == EACCES);
    return 0;
}
static int host_user_cannot_touch_subuid(void)
{
    px_clean();
    CHECK(px_mk(PXD "/f", 0, 100000, 100000, 0600) == 0);
    CHECK(px_mk(PXD "/d", 1, 100000, 100000, 0700) == 0);
    CHECK(px_mk(PXD "/d/f", 0, 100000, 100000, 0644) == 0);
    return px_hostuser(c_hu_subuid);
}


/* ---- idperm-f2fs: the same gates on the f2fs root volume ------------------ */
#define PXF "/idpx-f2fs"
static int on_f2fs(void)
{
    struct statfs sf;
    return statfs(PXF, &sf) == 0 && sf.f_type == 0xF2F52010;
}
static void px_clean_f2(void)
{
    (void)!system("rm -rf " PXF);
    mkdir(PXF, 0777); chmod(PXF, 0777);
}
static int c_create_unmapped_f2(void)
{
    int fd = open(PXF "/n", O_CREAT | O_WRONLY, 0644);
    CHECK(fd < 0 && errno == EOVERFLOW);
    CHECK(mkdir(PXF "/nd", 0755) == -1 && errno == EOVERFLOW);
    CHECK(symlink("x", PXF "/ns") == -1 && errno == EOVERFLOW);
    return 0;
}
static int create_unmapped_eoverflow_f2(void)
{
    mkdir(PXF, 0777);
    CHECK(on_f2fs());
    px_clean_f2();
    int r = px_run(c_create_unmapped_f2, NULL);
    struct stat st;
    CHECK(stat(PXF "/n", &st) == -1 && stat(PXF "/nd", &st) == -1);
    return r;
}

/* -- ns root (kuid 100000, caps in the ns) over mapped / unmapped inodes -- */
static int c_chown_mapped_f2(void)
{
    struct stat st;
    CHECK(px_nsroot() == 0);
    CHECK(chown(PXF "/f", 5, 42) == 0);
    CHECK(stat(PXF "/f", &st) == 0 && st.st_uid == 5 && st.st_gid == 42);
    return 0;
}
static int nsroot_chown_mapped_f2(void)
{
    if (!knob) return SKIP;
    mkdir(PXF, 0777);
    CHECK(on_f2fs());
    px_clean_f2();
    CHECK(px_mk(PXF "/f", 0, 100000, 100000, 0644) == 0);
    CHECK(px_run(c_chown_mapped_f2, MAP100) == 0);
    struct stat st;
    CHECK(stat(PXF "/f", &st) == 0 && st.st_uid == 100005 && st.st_gid == 100042);
    return 0;
}

static int c_chmod_sub_f2(void)
{
    struct stat st;
    CHECK(px_nsroot() == 0);
    CHECK(chmod(PXF "/f", 0644) == 0);
    CHECK(stat(PXF "/f", &st) == 0 && (st.st_mode & 07777) == 0644);
    return 0;
}
static int nsroot_chmod_subuid_file_f2(void)
{
    if (!knob) return SKIP;
    mkdir(PXF, 0777);
    CHECK(on_f2fs());
    px_clean_f2();
    CHECK(px_mk(PXF "/f", 0, 100007, 100007, 0600) == 0);
    return px_run(c_chmod_sub_f2, MAP100);
}

static int c_write_sub_f2(void)
{
    CHECK(px_nsroot() == 0);
    int fd = open(PXF "/d/f", O_RDWR); CHECK(fd >= 0);
    CHECK(write(fd, "hi", 2) == 2); close(fd);
    fd = open(PXF "/d/new", O_CREAT | O_WRONLY, 0600); CHECK(fd >= 0); close(fd);
    CHECK(unlink(PXF "/d/f") == 0);
    return 0;
}
static int nsroot_write_in_subuid_0700_dir_f2(void)
{
    if (!knob) return SKIP;
    mkdir(PXF, 0777);
    CHECK(on_f2fs());
    px_clean_f2();
    CHECK(px_mk(PXF "/d", 1, 100003, 100003, 0700) == 0);
    CHECK(px_mk(PXF "/d/f", 0, 100003, 100003, 0600) == 0);
    return px_run(c_write_sub_f2, MAP100);
}

static int c_untouchable_f2(void)
{
    CHECK(px_nsroot() == 0);
    CHECK(open(PXF "/u", O_RDWR) == -1 && errno == EACCES);
    CHECK(open(PXF "/u", O_RDONLY) == -1 && errno == EACCES);
    CHECK(chmod(PXF "/u", 0777) == -1 && errno == EPERM);
    CHECK(chown(PXF "/u", 5, -1) == -1 && errno == EPERM);
    CHECK(open(PXF "/ud/f", O_RDONLY) == -1 && errno == EACCES);
    CHECK(unlink(PXF "/ud/f") == -1 && errno == EACCES);
    CHECK(open(PXF "/ud/new", O_CREAT | O_WRONLY, 0600) == -1 && errno == EACCES);
    CHECK(chmod(PXF "/r", 0777) == -1 && errno == EPERM);   /* host-root-owned */
    CHECK(px_setx(PXF "/u", "user.x", "1", 1) == -1);
    CHECK(px_setx(PXF "/r", "trusted.x", "1", 1) == -1 && errno == EPERM);
    return 0;
}
static int nsroot_cannot_touch_unmapped_f2(void)
{
    if (!knob) return SKIP;
    mkdir(PXF, 0777);
    CHECK(on_f2fs());
    px_clean_f2();
    CHECK(px_mk(PXF "/u", 0, 2000, 2000, 0600) == 0);
    CHECK(px_mk(PXF "/ud", 1, 2000, 2000, 0700) == 0);
    CHECK(px_mk(PXF "/ud/f", 0, 2000, 2000, 0600) == 0);
    CHECK(px_mk(PXF "/r", 0, 0, 0, 0644) == 0);
    return px_run(c_untouchable_f2, MAP100);
}

static int c_hu_subuid_f2(void)
{
    CHECK(open(PXF "/f", O_RDWR) == -1 && errno == EACCES);
    CHECK(chmod(PXF "/f", 0666) == -1 && errno == EPERM);
    CHECK(chown(PXF "/f", 1000, -1) == -1 && errno == EPERM);
    CHECK(open(PXF "/d/f", O_RDONLY) == -1 && errno == EACCES);
    return 0;
}
static int host_user_cannot_touch_subuid_f2(void)
{
    mkdir(PXF, 0777);
    CHECK(on_f2fs());
    px_clean_f2();
    CHECK(px_mk(PXF "/f", 0, 100000, 100000, 0600) == 0);
    CHECK(px_mk(PXF "/d", 1, 100000, 100000, 0700) == 0);
    CHECK(px_mk(PXF "/d/f", 0, 100000, 100000, 0644) == 0);
    return px_hostuser(c_hu_subuid_f2);
}


static int c_sticky(void)
{
    CHECK(px_nsroot() == 0);
    CHECK(unlink(PXD "/s/x") == 0);                                   /* owner mapped: FOWNER over it */
    CHECK(unlink(PXD "/s/y") == -1 && errno == EPERM);                /* owner unmapped */
    return 0;
}
static int sticky_fowner_ns(void)
{
    if (!knob) return SKIP;
    px_clean();
    CHECK(px_mk(PXD "/s", 1, 0, 0, 01777) == 0);
    CHECK(px_mk(PXD "/s/x", 0, 100003, 100003, 0644) == 0);
    CHECK(px_mk(PXD "/s/y", 0, 2000, 2000, 0644) == 0);
    return px_run(c_sticky, MAP100);
}

/* ---- SCM_CREDENTIALS / SO_PEERCRED through the user namespaces (knob) ---- */
#define MAP10 "0 100000 10\n"
static int send_cred(int fd, unsigned uid)
{
    struct ucred uc = { getpid(), uid, uid };
    char d = 'c';
    struct iovec iov = { &d, 1 };
    union { char b[CMSG_SPACE(sizeof uc)]; struct cmsghdr a; } u;
    memset(&u, 0, sizeof u);
    struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.b, .msg_controllen = sizeof u.b };
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_CREDENTIALS; c->cmsg_len = CMSG_LEN(sizeof uc);
    memcpy(CMSG_DATA(c), &uc, sizeof uc);
    return (int)sendmsg(fd, &m, 0);
}
static int c_scm_ok(void *a) { int fd = *(int *)a; CHECK(send_cred(fd, 0) == 1); return 0; }
static int c_scm_unmapped(void *a)
{
    int fd = *(int *)a;
    CHECK(send_cred(fd, 5000) == -1 && errno == EINVAL);
    return 0;
}
static int scm_run(int (*body)(void *), int want_recv)
{
    int sv[2], go;
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0);
    int one = 1;
    CHECK(setsockopt(sv[0], SOL_SOCKET, SO_PASSCRED, &one, sizeof one) == 0);
    pid_t c = spawn(1000, body, &sv[1], &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", MAP10) == 0);
    CHECK(wrp(c, "setgroups", "deny\n") == 0);
    CHECK(wrp(c, "gid_map", MAP10) == 0);
    CHECK(fin(c, go) == 0);
    if (want_recv) {
        char d; struct ucred uc;
        struct iovec iov = { &d, 1 };
        union { char b[CMSG_SPACE(sizeof uc)]; struct cmsghdr a; } u;
        struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.b, .msg_controllen = sizeof u.b };
        CHECK(recvmsg(sv[0], &m, 0) == 1);
        struct cmsghdr *h = CMSG_FIRSTHDR(&m);
        CHECK(h && h->cmsg_type == SCM_CREDENTIALS);
        memcpy(&uc, CMSG_DATA(h), sizeof uc);
        CHECK(uc.uid == 100000 && uc.gid == 100000);
        CHECK(uc.pid == c);
    }
    return 0;
}
static int scm_creds_in_translate(void) { if (!knob) return SKIP; return scm_run(c_scm_ok, 1); }
static int scm_creds_unmapped_einval(void) { if (!knob) return SKIP; return scm_run(c_scm_unmapped, 0); }

#define PC_SOCK "/tmp/idscm-peercred.sock"
static int c_peercred(void *a)
{
    (void)a;
    struct sockaddr_un sa = { .sun_family = AF_UNIX };
    strcpy(sa.sun_path, PC_SOCK);
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(s >= 0 && connect(s, (struct sockaddr *)&sa, sizeof sa) == 0);
    struct ucred uc; socklen_t l = sizeof uc;
    CHECK(getsockopt(s, SOL_SOCKET, SO_PEERCRED, &uc, &l) == 0);
    CHECK(uc.uid == 65534 && uc.gid == 65534);      /* host root is unmapped here */
    char b; CHECK(read(s, &b, 1) == 1);
    return 0;
}
static int peercred_view(void)
{
    if (!knob) return SKIP;
    struct sockaddr_un sa = { .sun_family = AF_UNIX };
    strcpy(sa.sun_path, PC_SOCK);
    unlink(PC_SOCK);
    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(l >= 0 && bind(l, (struct sockaddr *)&sa, sizeof sa) == 0 && listen(l, 1) == 0);
    chmod(PC_SOCK, 0777);
    int go; pid_t c = spawn(1000, c_peercred, NULL, &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", MAP10) == 0);
    CHECK(wrp(c, "setgroups", "deny\n") == 0);
    CHECK(wrp(c, "gid_map", MAP10) == 0);
    (void)!write(go, "g", 1);
    int a = accept(l, NULL, NULL);
    CHECK(a >= 0);
    struct ucred uc; socklen_t n = sizeof uc;
    CHECK(getsockopt(a, SOL_SOCKET, SO_PEERCRED, &uc, &n) == 0);
    int ok = uc.uid == 1000;                         /* host reader: the peer's kuid */
    (void)!write(a, "x", 1);
    close(go);
    int st = 0; waitpid(c, &st, 0);
    unlink(PC_SOCK);
    CHECK(ok);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}

/* ---- idacl: POSIX ACL named entries are stored as kuids ------------------ */
static const char *acl_dir;
static char acl_path[64];
static void acl_put(unsigned char *b, int i, unsigned tag, unsigned perm, unsigned id)
{
    unsigned char *e = b + 4 + i * 8;
    e[0] = tag; e[1] = tag >> 8; e[2] = perm; e[3] = 0;
    memcpy(e + 4, &id, 4);
}
/* u::rw-, u:<id>:r--, g::r--, m::r--, o::--- */
static size_t acl_mk(unsigned char *b, unsigned id)
{
    b[0] = 2; b[1] = b[2] = b[3] = 0;
    acl_put(b, 0, 0x01, 6, 0xFFFFFFFFu);
    acl_put(b, 1, 0x02, 4, id);
    acl_put(b, 2, 0x04, 4, 0xFFFFFFFFu);
    acl_put(b, 3, 0x10, 4, 0xFFFFFFFFu);
    acl_put(b, 4, 0x20, 0, 0xFFFFFFFFu);
    return 4 + 5 * 8;
}
/* id of the ACL_USER entry (or -1) */
static long acl_user_id(const char *p)
{
    unsigned char b[128];
    long n = syscall(SYS_getxattr, p, "system.posix_acl_access", b, sizeof b);
    if (n < 4) return -1;
    for (long o = 4; o + 8 <= n; o += 8)
        if (b[o] == 0x02) { unsigned id; memcpy(&id, b + o + 4, 4); return id; }
    return -2;
}
static int c_acl_set(void)
{
    unsigned char b[64]; size_t l;
    CHECK(px_nsroot() == 0);
    l = acl_mk(b, 5);
    CHECK(px_setx(acl_path, "system.posix_acl_access", (char *)b, l) == 0);
    CHECK(acl_user_id(acl_path) == 5);                  /* ns reader: in-ns id */
    l = acl_mk(b, 1001);                                /* host uid: unmapped */
    CHECK(px_setx(acl_path, "system.posix_acl_access", (char *)b, l) == -1 && errno == EINVAL);
    CHECK(acl_user_id(acl_path) == 5);
    CHECK(px_setx(acl_path, "user.idacl", "v", 1) == 0);   /* user_xattr_ok_in_ns */
    char v[4];
    CHECK(syscall(SYS_getxattr, acl_path, "user.idacl", v, sizeof v) == 1 && v[0] == 'v');
    return 0;
}
static int c_acl_get(void)
{
    CHECK(px_nsroot() == 0);
    CHECK(acl_user_id(acl_path) == 65534);              /* kuid 1001: unmapped */
    return 0;
}
static int acl_run(const char *dir)
{
    unsigned char b[64]; size_t l;
    struct stat st;
    if (!knob) return SKIP;
    mkdir(dir, 0777); chmod(dir, 0777);
    snprintf(acl_path, sizeof acl_path, "%s/acl", dir);
    unlink(acl_path);
    CHECK(px_mk(acl_path, 0, 100000, 100000, 0644) == 0);
    CHECK(stat(acl_path, &st) == 0);
    CHECK(px_run(c_acl_set, MAP100) == 0);
    /* host (ns 0) sees the kuid */
    CHECK(acl_user_id(acl_path) == 100005);
    /* a kuid outside the reader's map reads as the overflow id */
    l = acl_mk(b, 1001);
    CHECK(px_setx(acl_path, "system.posix_acl_access", (char *)b, l) == 0);
    CHECK(acl_user_id(acl_path) == 1001);
    CHECK(px_run(c_acl_get, MAP100) == 0);
    unlink(acl_path);
    return 0;
}
static int acl_translated_tmpfs(void) { return acl_run(PXD); }
static int acl_translated_f2fs(void)
{
    mkdir(PXF, 0777);
    if (!on_f2fs()) return SKIP;
    return acl_run(PXF);
}

/* ---- idfix1: M1 scm pid, M2 oom_score_adj, M4 captured map ns, L1, L7, B-items ---- */
#include <sys/signalfd.h>
#define MAP1000 "1000 1000 1\n"

static int send_cred_pid(int fd, pid_t pid, unsigned uid)
{
    struct ucred uc = { pid, uid, uid };
    char d = 'c';
    struct iovec iov = { &d, 1 };
    union { char b[CMSG_SPACE(sizeof uc)]; struct cmsghdr a; } u;
    memset(&u, 0, sizeof u);
    struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.b, .msg_controllen = sizeof u.b };
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_CREDENTIALS; c->cmsg_len = CMSG_LEN(sizeof uc);
    memcpy(CMSG_DATA(c), &uc, sizeof uc);
    return (int)sendmsg(fd, &m, 0);
}
static int c_scm_forge(void *a)
{
    int fd = *(int *)a;
    CHECK(send_cred_pid(fd, 1, 1000) == -1 && errno == EPERM);      /* pid 1 of the host pidns */
    CHECK(send_cred_pid(fd, getpid(), 1000) == 1);                  /* own pid stays fine */
    return 0;
}
static int scm_pid_forge_hostpidns(void)
{
    int sv[2], go;
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0);
    pid_t c = spawn(1000, c_scm_forge, &sv[1], &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", MAP1000) == 0);
    CHECK(wrp(c, "setgroups", "deny\n") == 0);
    CHECK(wrp(c, "gid_map", MAP1000) == 0);
    CHECK(fin(c, go) == 0);
    return 0;
}

static int c_oom_lower(void *a)
{
    (void)a;
    CHECK(wr("/proc/self/oom_score_adj", "100\n") == 0);
    CHECK(wr("/proc/self/oom_score_adj", "-1000\n") == -EACCES);
    return 0;
}
static int oom_adj_lower_in_userns_eacces(void)
{
    int go;
    pid_t c = spawn(1000, c_oom_lower, NULL, &go);
    CHECK(c > 0);
    CHECK(fin(c, go) == 0);
    /* host root may still lower it */
    pid_t h = fork();
    if (h == 0) _exit(wr("/proc/self/oom_score_adj", "100\n") == 0 && wr("/proc/self/oom_score_adj", "-1000\n") == 0 ? 0 : 1);
    int st = 0; waitpid(h, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}

/* M4: the map file is bound to the namespace captured at open. */
static int nsmap_write_captured_ns(void)
{
    int go1, go2;
    pid_t c1 = spawn(1000, NULL, NULL, &go1);
    pid_t c2 = spawn(1000, NULL, NULL, &go2);
    CHECK(c1 > 0 && c2 > 0);
    char p[64];
    snprintf(p, sizeof p, "/proc/%d/uid_map", (int)c1);
    int fd = open(p, O_WRONLY);
    CHECK(fd >= 0);
    kill(c1, SIGKILL); close(go1); waitpid(c1, NULL, 0);          /* target gone and reaped */
    ssize_t n = write(fd, MAP1000, strlen(MAP1000));
    int e = errno;
    close(fd);
    CHECK(n == -1 && (e == ESRCH || e == EINVAL));
    char b[128];
    CHECK(rdp(c2, "uid_map", b, sizeof b) >= 0);
    CHECK(strchr(b, '1') == NULL);                                /* the other ns stayed unmapped */
    CHECK(wrp(c2, "uid_map", MAP1000) == 0);                      /* and is still writable */
    fin(c2, go2);
    return 0;
}

/* L1: kuid 0 inside a child userns (host root unshared, no map) is not host root. */
static int c_comm_other(void *a)
{
    char p[64]; snprintf(p, sizeof p, "/proc/%d/comm", *(int *)a);
    CHECK(wr(p, "zz\n") == -EACCES);
    return 0;
}
static int ns_kuid0_comm_eacces(void)
{
    int sg, ng;
    pid_t t = spawn_sleeper(1000, &sg);
    CHECK(t > 0);
    char p[64]; snprintf(p, sizeof p, "/proc/%d/comm", (int)t);
    CHECK(wr(p, "ok\n") == 0);                                    /* host root can */
    int tt = (int)t;
    pid_t c = spawn(-1, c_comm_other, &tt, &ng);
    CHECK(c > 0);
    int rc = fin(c, ng);
    reap_sleeper(t, sg);
    CHECK(rc == 0);
    return 0;
}

/* L7: per-kuid namespace quota; host root exempt. */
static int quota_run(int uid, int *made, int *err)
{
    int gos[64], n = 0; *err = 0;
    for (int i = 0; i < 48; i++) {
        int g[2], r[2];
        if (pipe(g) || pipe(r)) return -1;
        pid_t c = fork();
        if (c == 0) {
            char b = 0; close(g[1]); close(r[0]);
            if (uid >= 0 && (setresgid(uid, uid, uid) || setresuid(uid, uid, uid))) _exit(98);
            b = unshare(CLONE_NEWUSER) ? (char)errno : 0;
            (void)!write(r[1], &b, 1);
            if (b) _exit(0);
            (void)!read(g[0], &b, 1);
            _exit(0);
        }
        close(g[0]); close(r[1]);
        char b = 0;
        (void)!read(r[0], &b, 1); close(r[0]);
        if (b) { *err = b; close(g[1]); waitpid(c, NULL, 0); break; }
        gos[n++] = g[1];
    }
    *made = n;
    for (int i = 0; i < n; i++) close(gos[i]);  /* children exit on EOF */
    while (waitpid(-1, NULL, 0) > 0) {}
    return 0;
}
static int userns_quota_enospc(void)
{
    int made, err;
    CHECK(quota_run(1000, &made, &err) == 0);
    CHECK(err == ENOSPC && made >= 8 && made <= 32);
    CHECK(quota_run(-1, &made, &err) == 0);
    CHECK(err == 0 && made == 48);
    return 0;
}

/* (a) signalfd ssi_uid is in the reader's view. */
struct sfa { int rfd; };
static int c_sfd_uid(void *a)
{
    int rfd = ((struct sfa *)a)->rfd;
    sigset_t m; sigemptyset(&m); sigaddset(&m, SIGUSR1);
    sigprocmask(SIG_BLOCK, &m, NULL);
    int sfd = signalfd(-1, &m, 0);
    CHECK(sfd >= 0);
    (void)!write(rfd, "r", 1);
    struct signalfd_siginfo si;
    CHECK(read(sfd, &si, sizeof si) == (ssize_t)sizeof si);
    CHECK(si.ssi_uid == 65534);   /* sender (host root, kuid 0) is unmapped here */
    return 0;
}
static int signalfd_ssi_uid_view(void)
{
    int rp[2], go; CHECK(pipe(rp) == 0);
    struct sfa a = { rp[1] };
    pid_t c = spawn(1000, c_sfd_uid, &a, &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", MAP1000) == 0);
    CHECK(wrp(c, "setgroups", "deny\n") == 0);
    CHECK(wrp(c, "gid_map", MAP1000) == 0);
    (void)!write(go, "g", 1); close(go);
    char b; CHECK(read(rp[0], &b, 1) == 1);
    kill(c, SIGUSR1);
    int st = 0; waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}

/* (b) PRIO_USER `who` is a uid in the caller's namespace. */
static int c_prio_user(void *a)
{
    (void)a;
    errno = 0; getpriority(PRIO_USER, 7);
    CHECK(errno == 0);                 /* 7 is us (kuid 1000) */
    errno = 0; getpriority(PRIO_USER, 1000);
    CHECK(errno == ESRCH || errno == EINVAL);   /* 1000 is unmapped here */
    return 0;
}
static int prio_user_who_view(void)
{
    if (!knob) return SKIP;
    int go;
    pid_t c = spawn(1000, c_prio_user, NULL, &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", "7 1000 1\n") == 0);
    CHECK(fin(c, go) == 0);
    return 0;
}


/* ---- devpts uid=/gid= are translated through the mounter's userns -------- */
#include <sys/mount.h>
#include <sys/ioctl.h>
struct dpa { int up, dn, sv; char dir[64]; char opts[64]; };
static int c_devpts(void *a)
{
    struct dpa *d = a;
    CHECK(unshare(CLONE_NEWNS) == 0);
    int r = mount("none", d->dir, "devpts", 0, d->opts);
    char b = r == 0 ? 'o' : 'e';
    int e = r == 0 ? 0 : errno;
    if (r) { (void)!write(d->up, &b, 1); return e == EINVAL ? 0 : 50; }
    char p[96]; snprintf(p, sizeof p, "%s/ptmx", d->dir);
    int m = open(p, O_RDWR | O_NOCTTY);
    if (m < 0) { printf("  devpts ptmx open errno %d\n", errno); }
    CHECK(m >= 0);
    unsigned n = 0;
    CHECK(ioctl(m, TIOCGPTN, &n) == 0);
    struct stat st; snprintf(p, sizeof p, "%s/%u", d->dir, n);
    CHECK(stat(p, &st) == 0);
    int view = st.st_gid == 5 ? 0 : 1; /* ns-relative view must read 5 */
    { int z = 0; ioctl(m, 0x40045431, &z); }
    int sl = open(p, O_RDWR | O_NOCTTY);
    CHECK(sl >= 0);
    { char cb[CMSG_SPACE(sizeof(int))] = {0}; char x = 'x';
      struct iovec io = { &x, 1 };
      struct msghdr mh = { 0 }; mh.msg_iov = &io; mh.msg_iovlen = 1; mh.msg_control = cb; mh.msg_controllen = sizeof cb;
      struct cmsghdr *cm = CMSG_FIRSTHDR(&mh); cm->cmsg_level = SOL_SOCKET; cm->cmsg_type = SCM_RIGHTS; cm->cmsg_len = CMSG_LEN(sizeof(int));
      memcpy(CMSG_DATA(cm), &sl, sizeof sl);
      CHECK(sendmsg(d->sv, &mh, 0) == 1); }
    (void)!write(d->up, &b, 1); (void)!write(d->up, &n, sizeof n);
    char g; (void)!read(d->dn, &g, 1);
    return view;
}
static int devpts_run2(const char *opts, int want_mount, unsigned *kgid, int *dnw)
{
    if (!knob) return SKIP;
    struct dpa d; int up[2], dn[2];
    CHECK(pipe(up) == 0 && pipe(dn) == 0);
    *dnw = dn[1];
    int sv[2]; CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0);
    d.up = up[1]; d.dn = dn[0]; d.sv = sv[1];
    snprintf(d.dir, sizeof d.dir, "/tmp/dpm.%d", (int)getpid());
    snprintf(d.opts, sizeof d.opts, "%s", opts);
    mkdir(d.dir, 0755);
    int go; pid_t c = spawn(-1, c_devpts, &d, &go);
    CHECK(c > 0);
    CHECK(wrp(c, "uid_map", "0 100000 65536\n") == 0);
    wrp(c, "setgroups", "deny");
    CHECK(wrp(c, "gid_map", "0 100000 65536\n") == 0);
    (void)!write(go, "g", 1);
    char b = 0; CHECK(read(up[0], &b, 1) == 1);
    int ok = 1;
    if (want_mount) {
        unsigned n = 0; CHECK(b == 'o' && read(up[0], &n, sizeof n) == sizeof n);
        struct stat st; int sl = -1;
        { char cb[CMSG_SPACE(sizeof(int))] = {0}; char x;
          struct iovec io = { &x, 1 };
          struct msghdr mh = { 0 }; mh.msg_iov = &io; mh.msg_iovlen = 1; mh.msg_control = cb; mh.msg_controllen = sizeof cb;
          struct cmsghdr *cm;
          if (recvmsg(sv[0], &mh, 0) == 1 && (cm = CMSG_FIRSTHDR(&mh))) memcpy(&sl, CMSG_DATA(cm), sizeof sl); }
        ok = sl >= 0 && fstat(sl, &st) == 0;
        if (ok) *kgid = st.st_gid;
        (void)!write(dn[1], "x", 1);
    } else ok = b == 'e';
    int s; waitpid(c, &s, 0); rmdir(d.dir);
    CHECK(ok);
    return WIFEXITED(s) && WEXITSTATUS(s) == 0 ? 0 : 3;
}
static int devpts_run(const char *opts, int want_mount, unsigned *kgid)
{
    int dnw = -1;
    int r = devpts_run2(opts, want_mount, kgid, &dnw);
    if (dnw >= 0) close(dnw); /* releases a child still waiting */
    return r;
}
static int devpts_gid_translated(void)
{
    unsigned k = 0;
    int r = devpts_run("newinstance,ptmxmode=0666,gid=5", 1, &k);
    if (r) return r;
    printf("  devpts host kgid=%u\n", k);
    return k == 100005 ? 0 : 4;
}
static int devpts_gid_unmapped_einval(void) { return devpts_run("newinstance,gid=70000", 0, NULL); }
static int devpts_uid_unmapped_einval(void) { return devpts_run("newinstance,uid=70000", 0, NULL); }

static const struct { const char *name; int (*fn)(void); } cases[] = {
    { "devpts_gid_translated", devpts_gid_translated },
    { "devpts_gid_unmapped_einval", devpts_gid_unmapped_einval },
    { "devpts_uid_unmapped_einval", devpts_uid_unmapped_einval },
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
    { "nested_map_composed", nested_map_composed },
    { "podman_map_text", podman_map_text },
    { "unpriv_root_map", unpriv_root_map },
    { "chown_unmapped_einval", chown_unmapped_einval },
    { "setgroups_needs_gidmap", setgroups_needs_gidmap },
    { "setuid_maps_down", setuid_maps_down },
    { "chown_maps_down", chown_maps_down },
    { "setfsuid_ret_view", setfsuid_ret_view },
    { "getgroups_view", getgroups_view },
    { "exec_drops_caps_host", exec_drops_caps_host },
    { "exec_nsroot_keeps_caps", exec_nsroot_keeps_caps },
    { "setuid_drops_caps", setuid_drops_caps },
    { "exec_nonroot_nocaps", exec_nonroot_nocaps },
    { "kuid0_not_nsroot", kuid0_not_nsroot },
    { "kill_subuid_capkill_ns", kill_subuid_capkill_ns },
    { "kill_host_same_kuid_denied", kill_host_same_kuid_denied },
    { "nsroot_opens_ns_of_mapped_user", nsroot_opens_ns_of_mapped_user },
    { "prlimit_ns_child", prlimit_ns_child },
    { "ns_open_needs_ptrace_access", ns_open_needs_ptrace_access },
    { "host_root_fs_unaffected", host_root_fs_unaffected },
    { "trusted_xattr_host_only", trusted_xattr_host_only },
    { "fcap_xattr_eperm_in_ns", fcap_xattr_eperm_in_ns },
    { "create_unmapped_eoverflow", create_unmapped_eoverflow },
    { "create_unmapped_eoverflow_f2fs", create_unmapped_eoverflow_f2 },
    { "nsroot_chown_mapped", nsroot_chown_mapped },
    { "nsroot_chown_mapped_f2fs", nsroot_chown_mapped_f2 },
    { "nsroot_chmod_subuid_file", nsroot_chmod_subuid_file },
    { "nsroot_chmod_subuid_file_f2fs", nsroot_chmod_subuid_file_f2 },
    { "nsroot_write_in_subuid_0700_dir", nsroot_write_in_subuid_0700_dir },
    { "nsroot_write_in_subuid_0700_dir_f2fs", nsroot_write_in_subuid_0700_dir_f2 },
    { "nsroot_cannot_touch_unmapped", nsroot_cannot_touch_unmapped },
    { "nsroot_cannot_touch_unmapped_f2fs", nsroot_cannot_touch_unmapped_f2 },
    { "acl_named_set_translated", acl_translated_tmpfs },
    { "acl_named_set_translated_f2fs", acl_translated_f2fs },
    { "host_user_cannot_touch_subuid", host_user_cannot_touch_subuid },
    { "host_user_cannot_touch_subuid_f2fs", host_user_cannot_touch_subuid_f2 },
    { "sticky_fowner_ns", sticky_fowner_ns },
    { "scm_creds_in_translate", scm_creds_in_translate },
    { "scm_creds_unmapped_einval", scm_creds_unmapped_einval },
    { "peercred_view", peercred_view },
    { "scm_pid_forge_hostpidns", scm_pid_forge_hostpidns },
    { "oom_adj_lower_in_userns_eacces", oom_adj_lower_in_userns_eacces },
    { "nsmap_write_captured_ns", nsmap_write_captured_ns },
    { "ns_kuid0_comm_eacces", ns_kuid0_comm_eacces },
    { "userns_quota_enospc", userns_quota_enospc },
    { "signalfd_ssi_uid_view", signalfd_ssi_uid_view },
    { "prio_user_who_view", prio_user_who_view },
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
    char cl[1024];
    int off = slurp_file("/proc/cmdline", cl, sizeof cl) >= 0 && strstr(cl, "idmap=off");
    printf("idmaptest: non-identity maps %s\n", knob ? "ON (default)" : off ? "off (idmap=off, non-identity cases SKIP)" : "REFUSED without idmap=off");
    if (!knob && !off) { report("probe_nonidentity_accepted", 0); return; } /* a reverted flip must not pass as SKIP */
    for (unsigned i = 0; i < sizeof cases / sizeof cases[0]; i++) {
        fflush(stdout);
        pid_t p = fork();
        if (p == 0) _exit(cases[i].fn());
        int st = 0; waitpid(p, &st, 0);
        int rc = WIFEXITED(st) ? WEXITSTATUS(st) : 100;
        if (rc == SKIP) skip(cases[i].name, "non-identity map refused (idmap=off)");
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
    proc_owner_newuser();
    proc_owner_nondumpable();
    newuidmap_e2e();
    e2e_subuid();
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
