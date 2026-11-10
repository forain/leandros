/*
 * bpftest - bpf(2) for the cgroup v2 device controller (lane bpfsys).
 * Replays crun's call sequence (LOAD with log retry, 7x QUERY, ATTACH with
 * BPF_F_ALLOW_MULTI, REPLACE, DETACH, GET_FD_BY_ID) and checks permissions,
 * attach rules, query counts and program-fd lifetime. Attaching changes no
 * device access yet (enforcement is lane bpfenf).
 * Output: "<name>: PASS|FAIL ..." per case and a summary line.
 * Builds for the host (cc) and LeandrOS (zig cc -target <arch>-linux-musl -static).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef SYS_bpf
# if defined(__aarch64__)
#  define SYS_bpf 280
# else
#  define SYS_bpf 321
# endif
#endif

#define PROG_LOAD 5
#define PROG_ATTACH 8
#define PROG_DETACH 9
#define GET_FD_BY_ID 13
#define PROG_QUERY 16
#define BPF_CGROUP_DEVICE 6
#define F_OVERRIDE 1
#define F_MULTI 2
#define F_REPLACE 4
#define Q_EFFECTIVE 1
#define CGR "/sys/fs/cgroup"

static int npass, nfail;
#define CHECK(c) do { if (!(c)) { printf("  check failed line %d: %s (errno %d)\n", __LINE__, #c, errno); return 1; } } while (0)
#define ERR(call, e) (((call) == -1 && errno == (e)))

struct insn { uint8_t code; uint8_t regs; int16_t off; int32_t imm; };
#define I(c, d, s, o, i) ((struct insn){ (c), (uint8_t)((d) | ((s) << 4)), (o), (i) })

/* crun's create_dev_bpf: preamble, then per rule the 8-insn block, then the tail. */
static int build(struct insn *p, int nrules, const int *r /* {type,acc,major,minor,allow}* */) {
    int n = 0;
    p[n++] = I(0x61, 2, 1, 0, 0);
    p[n++] = I(0x54, 2, 0, 0, 0xFFFF);
    p[n++] = I(0x61, 3, 1, 0, 0);
    p[n++] = I(0x74, 3, 0, 0, 16);
    p[n++] = I(0x61, 4, 1, 4, 0);
    p[n++] = I(0x61, 5, 1, 8, 0);
    for (int k = 0; k < nrules; k++) {
        const int *x = r + k * 5;
        p[n++] = I(0x55, 2, 0, 7, x[0]);
        p[n++] = I(0xbc, 1, 3, 0, 0);
        p[n++] = I(0x54, 1, 0, 0, x[1]);
        p[n++] = I(0x5d, 1, 3, 4, 0);
        p[n++] = I(0x55, 4, 0, 3, x[2]);
        p[n++] = I(0x55, 5, 0, 2, x[3]);
        p[n++] = I(0xb7, 0, 0, 0, x[4]);
        p[n++] = I(0x95, 0, 0, 0, 0);
    }
    p[n++] = I(0xb7, 0, 0, 0, 0);
    p[n++] = I(0x95, 0, 0, 0, 0);
    return n;
}

static long sb(int cmd, void *attr, size_t sz) { return syscall(SYS_bpf, cmd, attr, sz); }
static void put32(char *a, int off, uint32_t v) { memcpy(a + off, &v, 4); }
static void put64(char *a, int off, uint64_t v) { memcpy(a + off, &v, 8); }

static int load_x(const struct insn *p, int n, uint32_t type, uint32_t pflags, const char *lic,
                  uint32_t lvl, char *log, uint32_t logsz) {
    char a[128]; memset(a, 0, sizeof a);
    put32(a, 0, type); put32(a, 4, n); put64(a, 8, (uint64_t)(uintptr_t)p);
    put64(a, 16, (uint64_t)(uintptr_t)lic);
    put32(a, 24, lvl); put32(a, 28, logsz); put64(a, 32, (uint64_t)(uintptr_t)log);
    put32(a, 44, pflags);
    return (int)sb(PROG_LOAD, a, sizeof a);
}
static struct insn good_prog[64];
static int good_n;
static void init_prog(void) {
    static const int rules[] = { 2, 7, 1, 3, 1 };   /* allow c 1:3 rwm, then deny */
    good_n = build(good_prog, 1, rules);
}
static int load(void) { return load_x(good_prog, good_n, 15, 0, "GPL", 0, NULL, 0); }

static int attach_x(int cgfd, int pfd, int atype, uint32_t flags, int rfd) {
    char a[128]; memset(a, 0, sizeof a);
    put32(a, 0, cgfd); put32(a, 4, pfd); put32(a, 8, atype); put32(a, 12, flags); put32(a, 16, rfd);
    return (int)sb(PROG_ATTACH, a, sizeof a);
}
static int attach(int cgfd, int pfd, uint32_t flags, int rfd) { return attach_x(cgfd, pfd, BPF_CGROUP_DEVICE, flags, rfd); }
static int detach(int cgfd, int pfd) {
    char a[128]; memset(a, 0, sizeof a);
    put32(a, 0, cgfd); put32(a, 4, pfd); put32(a, 8, BPF_CGROUP_DEVICE);
    return (int)sb(PROG_DETACH, a, sizeof a);
}
/* returns the syscall result; *cnt receives the written-back prog_cnt */
static int query(int cgfd, uint32_t qflags, uint32_t *ids, uint32_t cap, uint32_t *cnt, uint32_t *aflags) {
    char a[128]; memset(a, 0, sizeof a);
    put32(a, 0, cgfd); put32(a, 4, BPF_CGROUP_DEVICE); put32(a, 8, qflags);
    put64(a, 16, (uint64_t)(uintptr_t)ids); put32(a, 24, cap);
    put32(a, 12, 0xdead);
    int r = (int)sb(PROG_QUERY, a, sizeof a);
    if (cnt) memcpy(cnt, a + 24, 4);
    if (aflags) memcpy(aflags, a + 12, 4);
    return r;
}
static int by_id(uint32_t id) {
    char a[128]; memset(a, 0, sizeof a); put32(a, 0, id);
    return (int)sb(GET_FD_BY_ID, a, sizeof a);
}

static int cgn;
static char cgpath[64];
static int mkcg(const char *sub, int *fd) {
    snprintf(cgpath, sizeof cgpath, CGR "/bpft_%d_%s", (int)getpid(), sub);
    if (mkdir(cgpath, 0755) != 0) return -1;
    *fd = open(cgpath, O_DIRECTORY | O_PATH | O_CLOEXEC);
    return *fd < 0 ? -1 : 0;
}

/* one id of the single program attached to cgfd */
static uint32_t only_id(int cgfd) {
    uint32_t ids[4] = {0}, cnt = 99;
    if (query(cgfd, 0, ids, 4, &cnt, NULL) != 0 || cnt != 1) return 0;
    return ids[0];
}

static int t_load_valid(void) {
    int fd = load();
    CHECK(fd >= 0);
    int fl = fcntl(fd, F_GETFD);
    CHECK(fl >= 0 && (fl & FD_CLOEXEC));
    char lnk[64]; char path[32]; snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(path, lnk, sizeof lnk - 1);
    CHECK(n > 0); lnk[n] = 0;
    CHECK(strcmp(lnk, "anon_inode:bpf-prog") == 0);
    return 0;
}

static int t_load_rejects(void) {
    struct insn p[8];
    int n;
    CHECK(ERR(load_x(good_prog, good_n, 1, 0, "GPL", 0, NULL, 0), EINVAL));        /* socket filter */
    CHECK(ERR(load_x(good_prog, good_n, 15, 1, "GPL", 0, NULL, 0), EINVAL));       /* prog_flags */
    CHECK(ERR(load_x(good_prog, 0, 15, 0, "GPL", 0, NULL, 0), EINVAL));            /* no insns */
    CHECK(ERR(load_x(good_prog, 5000, 15, 0, "GPL", 0, NULL, 0), EINVAL));         /* too many */
    CHECK(ERR(load_x(good_prog, good_n, 15, 0, (const char *)1, 0, NULL, 0), EFAULT)); /* license */
    n = 0; p[n++] = I(0xb7, 0, 0, 0, 2); p[n++] = I(0x95, 0, 0, 0, 0);              /* r0 = 2 */
    CHECK(ERR(load_x(p, n, 15, 0, "GPL", 0, NULL, 0), EINVAL));
    n = 0; p[n++] = I(0xb7, 0, 0, 0, 1); p[n++] = I(0x05, 0, 0, -2, 0); p[n++] = I(0x95, 0, 0, 0, 0); /* back jump */
    CHECK(ERR(load_x(p, n, 15, 0, "GPL", 0, NULL, 0), EINVAL));
    n = 0; p[n++] = I(0x85, 0, 0, 0, 1); p[n++] = I(0x95, 0, 0, 0, 0);              /* call */
    CHECK(ERR(load_x(p, n, 15, 0, "GPL", 0, NULL, 0), EINVAL));
    n = 0; p[n++] = I(0x18, 0, 0, 0, 1); p[n++] = I(0, 0, 0, 0, 0); p[n++] = I(0x95, 0, 0, 0, 0); /* ld_imm64 */
    CHECK(ERR(load_x(p, n, 15, 0, "GPL", 0, NULL, 0), EINVAL));
    n = 0; p[n++] = I(0x0f, 0, 2, 0, 0); p[n++] = I(0x95, 0, 0, 0, 0);              /* r0 += r2 uninit */
    CHECK(ERR(load_x(p, n, 15, 0, "GPL", 0, NULL, 0), EACCES));
    n = 0; p[n++] = I(0xb7, 0, 0, 0, 1);                                            /* falls off the end */
    CHECK(ERR(load_x(p, n, 15, 0, "GPL", 0, NULL, 0), EINVAL));
    /* attr tail */
    char a[4097]; memset(a, 0, sizeof a);
    CHECK(ERR(sb(PROG_LOAD, a, 4097), E2BIG));
    char b[256]; memset(b, 0, sizeof b); b[255] = 1;
    CHECK(ERR(sb(PROG_LOAD, b, sizeof b), E2BIG));
    CHECK(ERR(sb(99, a, 128), EINVAL));
    CHECK(ERR(sb(0 /* MAP_CREATE */, a, 128), EINVAL));
    return 0;
}

static int t_load_log(void) {
    struct insn p[2] = { I(0xb7, 0, 0, 0, 2), I(0x95, 0, 0, 0, 0) };
    char log[8192]; memset(log, 'x', sizeof log);
    /* crun: plain failure first, then retry with a log */
    CHECK(ERR(load_x(p, 2, 15, 0, "GPL", 0, NULL, 0), EINVAL));
    CHECK(ERR(load_x(p, 2, 15, 0, "GPL", 1, log, sizeof log), EINVAL));
    CHECK(log[0] != 'x' && log[0] != 0);
    CHECK(memchr(log, 0, sizeof log) != NULL);
    CHECK(ERR(load_x(p, 2, 15, 0, "GPL", 1, NULL, sizeof log), EINVAL));   /* no buffer */
    CHECK(ERR(load_x(p, 2, 15, 0, "GPL", 1, log, 64), EINVAL));            /* too small */
    /* a valid program with a log must succeed and leave an empty log */
    int fd = load_x(good_prog, good_n, 15, 0, "GPL", 1, log, sizeof log);
    CHECK(fd >= 0 && log[0] == 0);
    return 0;
}

static int t_crun_sequence(void) {
    int cg, pfd, p2;
    CHECK(mkcg("seq", &cg) == 0);
    pfd = load(); CHECK(pfd >= 0);
    uint32_t ids[4096], cnt;
    /* crun's read_all_progs: 7 queries, 64..4096, the last must not be ENOSPC */
    int last = 0;
    for (uint32_t c = 64; c <= 4096; c *= 2) {
        cnt = 77; last = query(cg, 0, ids, c, &cnt, NULL);
        CHECK(last == 0 && cnt == 0);
    }
    CHECK(attach(cg, pfd, F_MULTI, -1) == 0);
    cnt = 77; CHECK(query(cg, 0, ids, 4096, &cnt, NULL) == 0 && cnt == 1);
    uint32_t id1 = ids[0]; CHECK(id1 != 0);
    int g = by_id(id1); CHECK(g >= 0);                                      /* old prog fd */
    /* second run: attach the new program replacing the old one */
    p2 = load(); CHECK(p2 >= 0);
    CHECK(attach(cg, p2, F_MULTI | F_REPLACE, g) == 0);
    cnt = 77; CHECK(query(cg, 0, ids, 4096, &cnt, NULL) == 0 && cnt == 1 && ids[0] != id1);
    uint32_t id2 = ids[0];
    CHECK(ERR(attach(cg, p2, F_MULTI, -1), EEXIST));
    CHECK(ERR(attach(cg, p2, 0, -1), EPERM));                               /* different flags */
    CHECK(ERR(detach(cg, pfd), ENOENT));                                    /* replaced one */
    CHECK(detach(cg, p2) == 0);
    cnt = 77; CHECK(query(cg, 0, ids, 4096, &cnt, NULL) == 0 && cnt == 0);
    CHECK(ERR(detach(cg, p2), ENOENT));
    (void)id2;
    /* fresh attach after detach, then the crun "n_progs>0, not replaced" path */
    CHECK(attach(cg, pfd, F_MULTI, -1) == 0);
    CHECK(attach(cg, p2, F_MULTI, -1) == 0);
    cnt = 77; CHECK(query(cg, 0, ids, 4096, &cnt, NULL) == 0 && cnt == 2);
    CHECK(detach(cg, pfd) == 0 && detach(cg, p2) == 0);
    return 0;
}

static int t_query_counts(void) {
    int cg, a, b;
    CHECK(mkcg("q", &cg) == 0);
    a = load(); b = load(); CHECK(a >= 0 && b >= 0);
    CHECK(attach(cg, a, F_MULTI, -1) == 0 && attach(cg, b, F_MULTI, -1) == 0);
    uint32_t ids[8] = {0}, cnt, fl;
    cnt = 0; fl = 0xdead;
    CHECK(ERR(query(cg, 0, ids, 0, &cnt, &fl), ENOSPC) && cnt == 2 && fl == F_MULTI);
    memset(ids, 0, sizeof ids);
    CHECK(ERR(query(cg, 0, ids, 1, &cnt, NULL), ENOSPC) && cnt == 2 && ids[0] != 0 && ids[1] == 0);
    CHECK(query(cg, 0, NULL, 0, &cnt, NULL) == 0 && cnt == 2);               /* count only */
    CHECK(query(cg, 0, ids, 2, &cnt, NULL) == 0 && cnt == 2 && ids[0] != ids[1]);
    CHECK(query(cg, 0, ids, 8, &cnt, NULL) == 0 && cnt == 2);
    CHECK(ERR(query(cg, 2, ids, 8, &cnt, NULL), EINVAL));                    /* unknown query flag */
    /* effective list of a child cgroup inherits MULTI parents */
    int ch; char sub[64]; snprintf(sub, sizeof sub, "%s/c", cgpath);
    CHECK(mkdir(sub, 0755) == 0);
    ch = open(sub, O_DIRECTORY | O_PATH); CHECK(ch >= 0);
    CHECK(query(ch, 0, ids, 8, &cnt, NULL) == 0 && cnt == 0);
    CHECK(query(ch, Q_EFFECTIVE, ids, 8, &cnt, &fl) == 0 && cnt == 2 && fl == 0);
    rmdir(sub);
    return 0;
}

static int t_hierarchy(void) {
    int cg, a, b;
    CHECK(mkcg("h", &cg) == 0);
    char sub[64]; snprintf(sub, sizeof sub, "%s/c", cgpath);
    CHECK(mkdir(sub, 0755) == 0);
    int ch = open(sub, O_DIRECTORY | O_PATH); CHECK(ch >= 0);
    a = load(); b = load(); CHECK(a >= 0 && b >= 0);
    CHECK(attach(cg, a, 0, -1) == 0);                                        /* strict parent */
    CHECK(ERR(attach(ch, b, F_MULTI, -1), EPERM));
    CHECK(ERR(attach(ch, b, F_OVERRIDE, -1), EPERM));
    CHECK(detach(cg, -1) == 0);                                              /* non-multi: fd optional */
    CHECK(attach(cg, a, F_OVERRIDE, -1) == 0);
    CHECK(attach(ch, b, F_OVERRIDE, -1) == 0);
    CHECK(attach(cg, b, F_OVERRIDE, -1) == 0);                               /* single prog replaced */
    uint32_t ids[4], cnt;
    CHECK(query(cg, 0, ids, 4, &cnt, NULL) == 0 && cnt == 1);
    CHECK(detach(ch, -1) == 0 && detach(cg, -1) == 0);
    CHECK(ERR(detach(cg, -1), ENOENT));
    CHECK(attach(cg, a, F_MULTI, -1) == 0);
    CHECK(attach(ch, b, F_MULTI, -1) == 0);
    CHECK(query(ch, Q_EFFECTIVE, ids, 4, &cnt, NULL) == 0 && cnt == 2);     /* parent first, then child */
    CHECK(query(cg, Q_EFFECTIVE, ids, 4, &cnt, NULL) == 0 && cnt == 1);
    rmdir(sub);
    return 0;
}

static int t_bad_targets(void) {
    int cg, pfd;
    CHECK(mkcg("bt", &cg) == 0);
    pfd = load(); CHECK(pfd >= 0);
    int nul = open("/dev/null", O_RDONLY); CHECK(nul >= 0);
    CHECK(ERR(attach(nul, pfd, F_MULTI, -1), EBADF));                        /* not a cgroup */
    CHECK(ERR(attach(999, pfd, F_MULTI, -1), EBADF));
    CHECK(ERR(attach(pfd, pfd, F_MULTI, -1), EBADF));
    CHECK(ERR(attach(cg, cg, F_MULTI, -1), EINVAL));                         /* prog fd is a dir */
    CHECK(ERR(attach(cg, 999, F_MULTI, -1), EBADF));
    CHECK(ERR(attach_x(cg, pfd, 0, F_MULTI, -1), EINVAL));                   /* wrong attach type */
    CHECK(ERR(attach(cg, pfd, F_MULTI | F_OVERRIDE, -1), EINVAL));
    CHECK(ERR(attach(cg, pfd, F_REPLACE, pfd), EINVAL));                     /* replace needs multi */
    CHECK(ERR(attach(cg, pfd, 8, -1), EINVAL));                              /* unknown flag */
    CHECK(ERR(attach(cg, pfd, F_MULTI | F_REPLACE, pfd), ENOENT));           /* nothing to replace */
    uint32_t ids[2], cnt;
    CHECK(ERR(query(nul, 0, ids, 2, &cnt, NULL), EBADF));
    CHECK(ERR(detach(cg, pfd), ENOENT));
    CHECK(ERR(by_id(0), ENOENT));
    CHECK(ERR(by_id(0x7ffffff0), ENOENT));
    return 0;
}

static int t_fd_lifetime(void) {
    int cg;
    CHECK(mkcg("life", &cg) == 0);
    int p = load(); CHECK(p >= 0);
    CHECK(attach(cg, p, F_MULTI, -1) == 0);
    uint32_t id = only_id(cg); CHECK(id != 0);
    int d = dup(p); CHECK(d >= 0);
    close(p);                                                                /* attached: survives */
    int g = by_id(id); CHECK(g >= 0);
    close(g);
    CHECK(detach(cg, d) == 0);
    int g2 = by_id(id); CHECK(g2 >= 0);                                      /* dup still holds it */
    close(d);
    int g3 = by_id(id); CHECK(g3 >= 0);                                      /* g2 holds it */
    close(g2); close(g3);
    CHECK(ERR(by_id(id), ENOENT));                                           /* last ref gone */
    /* attachment alone keeps a program alive; rmdir drops it */
    int q = load(); CHECK(q >= 0);
    CHECK(attach(cg, q, F_MULTI, -1) == 0);
    uint32_t id2 = only_id(cg); CHECK(id2 != 0);
    close(q);
    int h = by_id(id2); CHECK(h >= 0); close(h);
    char sub[64]; snprintf(sub, sizeof sub, "%s", cgpath);
    CHECK(rmdir(sub) == 0);
    CHECK(ERR(by_id(id2), ENOENT));
    /* fork shares the fd; the child's exit must not free it */
    int r = load(); CHECK(r >= 0);
    int cg2; CHECK(mkcg("life2", &cg2) == 0);
    CHECK(attach(cg2, r, F_MULTI, -1) == 0);
    uint32_t id3 = only_id(cg2);
    pid_t c = fork();
    if (c == 0) _exit(0);
    int st; waitpid(c, &st, 0);
    CHECK(by_id(id3) >= 0);
    rmdir(cgpath);
    return 0;
}

static int t_unpriv(void) {
    int cg, p;
    CHECK(mkcg("priv", &cg) == 0);
    p = load(); CHECK(p >= 0);
    CHECK(attach(cg, p, F_MULTI, -1) == 0);
    uint32_t id = only_id(cg); CHECK(id != 0);
    pid_t c = fork();
    if (c == 0) {
        if (setresuid(1000, 1000, 1000) != 0) _exit(77);
        uint32_t ids[2], cnt;
        if (!ERR(load(), EPERM)) _exit(1);
        if (!ERR(attach(cg, p, F_MULTI, -1), EPERM)) _exit(2);
        if (!ERR(detach(cg, p), EPERM)) _exit(3);
        if (!ERR(query(cg, 0, ids, 2, &cnt, NULL), EPERM)) _exit(4);
        if (!ERR(by_id(id), EPERM)) _exit(5);
        _exit(0);
    }
    int st; waitpid(c, &st, 0);
    if (WIFEXITED(st) && WEXITSTATUS(st) == 77) { printf("  cannot drop root, skipping\n"); return 0; }
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}

static int t_userns(void) {
    int cg, p;
    CHECK(mkcg("uns", &cg) == 0);
    p = load(); CHECK(p >= 0);
    CHECK(attach(cg, p, F_MULTI, -1) == 0);
    uint32_t id = only_id(cg); CHECK(id != 0);
    pid_t c = fork();
    if (c == 0) {
        if (unshare(CLONE_NEWUSER) != 0) _exit(77);
        /* root with full caps in a child userns: still EPERM (not the init userns) */
        uint32_t ids[2], cnt;
        if (!ERR(load(), EPERM)) _exit(1);
        if (!ERR(attach(cg, p, F_MULTI, -1), EPERM)) _exit(2);
        if (!ERR(detach(cg, p), EPERM)) _exit(3);
        if (!ERR(query(cg, 0, ids, 2, &cnt, NULL), EPERM)) _exit(4);
        if (!ERR(by_id(id), EPERM)) _exit(5);
        _exit(0);
    }
    int st; waitpid(c, &st, 0);
    if (WIFEXITED(st) && WEXITSTATUS(st) == 77) { printf("  unshare(NEWUSER) failed, skipping\n"); return 0; }
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 0;
}

static void case_run(const char *name, int (*fn)(void)) {
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) _exit(fn());
    int st = 0; waitpid(c, &st, 0);
    if (WIFEXITED(st) && WEXITSTATUS(st) == 0) { printf("%s: PASS\n", name); npass++; }
    else { printf("%s: FAIL status 0x%x\n", name, st); nfail++; }
    fflush(stdout);
}

int main(void) {
    init_prog();
    struct stat sb0;
    if (stat(CGR "/cgroup.controllers", &sb0) != 0) { printf("bpftest: no cgroup2 at " CGR ", skipping\n"); return 0; }
    case_run("load_valid", t_load_valid);
    case_run("load_rejects", t_load_rejects);
    case_run("load_log_retry", t_load_log);
    case_run("crun_sequence", t_crun_sequence);
    case_run("query_counts", t_query_counts);
    case_run("hierarchy", t_hierarchy);
    case_run("bad_targets", t_bad_targets);
    case_run("fd_lifetime", t_fd_lifetime);
    case_run("unpriv_eperm", t_unpriv);
    case_run("userns_eperm", t_userns);
    printf("bpftest: %d passed, %d failed\n", npass, nfail);
    return nfail != 0;
}
