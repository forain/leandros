/*
 * privtest — privilege checks that must hold for an unprivileged process,
 * with the results Linux gives (credentials(7), kill(2), mount(2),
 * capabilities(7)). The same file builds for the host (cc privtest.c) and for
 * LeandrOS (zig cc -target <arch>-linux-musl -static), so every expectation
 * below is checked against a real Linux kernel first.
 *
 *   privtest            as a normal user: the unprivileged cases only
 *   privtest            as root: root-only positives (set-user-ID and
 *                       set-group-ID execve, no_new_privs), then the unprivileged
 *                       cases in a child that drops to PRIVTEST_UID (default
 *                       1000), plus the cross-uid cases that need a root
 *                       process to aim at (a root "victim" in the caller's
 *                       session, root-owned files in sticky /tmp)
 *
 * Output: "<name>: PASS" / "<name>: FAIL ..." per case, "<name>: SKIP ..."
 * where the environment cannot express the case, and a final summary line
 * "privtest: N passed, M failed, K skipped". Exit status = failures (capped).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <netinet/in.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/auxv.h>
#include <sys/fsuid.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_pivot_root
#define SYS_pivot_root __NR_pivot_root
#endif

static int npass, nfail, nskip;
static uid_t U = 1000;
static gid_t G = 1000;

static const char *ename(int e) {
    switch (e) {
    case 0: return "0";
    case EPERM: return "EPERM";
    case ENOENT: return "ENOENT";
    case ESRCH: return "ESRCH";
    case EACCES: return "EACCES";
    case EINVAL: return "EINVAL";
    case ENOSYS: return "ENOSYS";
    case EBUSY: return "EBUSY";
    case EEXIST: return "EEXIST";
    case EFAULT: return "EFAULT";
    case EROFS: return "EROFS";
    case ENOTDIR: return "ENOTDIR";
    case EADDRINUSE: return "EADDRINUSE";
    default: { static char b[16]; snprintf(b, sizeof b, "errno%d", e); return b; }
    }
}

/* `r` is a libc-style result (-1 + errno on failure). want_errno 0 = success. */
static void expect(const char *name, long r, int want_errno) {
    int got = r < 0 ? errno : 0;
    if (got == want_errno) { printf("%s: PASS\n", name); npass++; }
    else { printf("%s: FAIL (got %s, want %s)\n", name, ename(got), ename(want_errno)); nfail++; }
}
static void expect2(const char *name, long r, int want1, int want2) {
    int got = r < 0 ? errno : 0;
    if (got == want1 || got == want2) { printf("%s: PASS\n", name); npass++; }
    else { printf("%s: FAIL (got %s, want %s|%s)\n", name, ename(got), ename(want1), ename(want2)); nfail++; }
}
static void check(const char *name, int ok, const char *why) {
    if (ok) { printf("%s: PASS\n", name); npass++; }
    else { printf("%s: FAIL (%s)\n", name, why); nfail++; }
}
static void skip(const char *name, const char *why) { printf("%s: SKIP (%s)\n", name, why); nskip++; }

static pid_t spawn_sleeper(int own_pgrp) {
    /* Wait until the child has reset its dispositions: a signal sent while it
     * still carries an inherited SIG_IGN would be discarded. */
    int fds[2]; char ch;
    if (pipe(fds) < 0) return -1;
    pid_t p = fork();
    if (p == 0) {
        signal(SIGUSR1, SIG_DFL); signal(SIGTERM, SIG_DFL);
        if (own_pgrp) setpgid(0, 0);
        close(fds[0]); write(fds[1], "x", 1); close(fds[1]);
        for (;;) pause();
    }
    if (own_pgrp && p > 0) setpgid(p, p);
    close(fds[1]); if (p > 0) read(fds[0], &ch, 1); close(fds[0]);
    return p;
}
/* 1 when `p` (our child) is still alive, reaping it if it is not. */
static int child_alive(pid_t p) {
    int st;
    usleep(100000);
    return waitpid(p, &st, WNOHANG) == 0;
}
static int killed_by(pid_t p, int sig) {
    int st;
    for (int i = 0; i < 50; i++) {
        pid_t r = waitpid(p, &st, WNOHANG);
        if (r == p) return WIFSIGNALED(st) && WTERMSIG(st) == sig;
        usleep(20000);
    }
    kill(p, SIGKILL); waitpid(p, &st, 0);
    return 0;
}
static void reap(pid_t p) { int st; kill(p, SIGKILL); waitpid(p, &st, 0); }

static int drop_to(uid_t u, gid_t g) {
    if (setgroups(0, NULL) < 0) return -1;
    if (setresgid(g, g, g) < 0) return -1;
    if (setresuid(u, u, u) < 0) return -1;
    return 0;
}

/* Run fn in a forked child; returns the child's failure count (or 1 if it died). */
static int in_child(void (*fn)(void *), void *arg) {
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        npass = nfail = nskip = 0;
        fn(arg);
        fflush(stdout);
        _exit(nfail > 100 ? 100 : nfail);
    }
    int st; waitpid(p, &st, 0);
    if (WIFEXITED(st)) return WEXITSTATUS(st);
    printf("  child died with signal %d\n", WIFSIGNALED(st) ? WTERMSIG(st) : -1);
    return 1;
}

/* ── context shared by the unprivileged cases ─────────────────────────────── */
struct ctx {
    pid_t victim;      /* root process in our session (root mode), else 0 */
    pid_t victim_pg;   /* root process leading its own pgrp (root mode), else 0 */
    char rootfile[64]; /* root-owned 0644 file in /tmp (root mode), else "" */
    char rootsecret[64]; /* root-owned 0600 file in /tmp (root mode), else "" */
    long rootport;     /* LeandrOS queue port created by the root parent, else 0 */
};

static int other_pid(struct ctx *c) {
    /* A process owned by someone else: the root victim, else pid 1. */
    if (c->victim) return c->victim;
    return 1;
}

static void user_kill_cases(struct ctx *c) {
    pid_t o = other_pid(c);
    expect("kill_other_uid_probe_eperm", kill(o, 0), EPERM);
    expect("kill_other_uid_term_eperm", kill(o, SIGTERM), EPERM);
    expect("kill_nonexistent_esrch", kill(0x3ffffff0, 0), ESRCH);
    expect("tkill_other_uid_eperm", syscall(SYS_tkill, o, SIGTERM), EPERM);
    expect("tgkill_other_uid_eperm", syscall(SYS_tgkill, o, o, SIGTERM), EPERM);
    {
        siginfo_t si; memset(&si, 0, sizeof si);
        si.si_signo = SIGUSR1; si.si_code = SI_QUEUE; si.si_pid = getpid(); si.si_uid = getuid();
        expect("rt_sigqueueinfo_other_uid_eperm", syscall(SYS_rt_sigqueueinfo, o, SIGUSR1, &si), EPERM);
    }
    if (c->victim) {
        /* Linux (and init on LeandrOS) — init's own session differs from ours. */
        expect("kill_sigcont_same_session_ok", kill(c->victim, SIGCONT), 0);
        check("kill_sigcont_victim_survives", kill(c->victim, 0) == -1 && errno == EPERM, "victim gone");
    } else skip("kill_sigcont_same_session_ok", "needs root mode");
    if (kill(1, 0) == -1 && errno == ESRCH) skip("kill_sigcont_other_session_eperm", "no pid 1");
    else if (getsid(1) == getsid(0)) skip("kill_sigcont_other_session_eperm", "pid 1 shares our session");
    else expect("kill_sigcont_other_session_eperm", kill(1, SIGCONT), EPERM);
    if (c->victim_pg) {
        expect("killpg_other_uid_eperm", kill(-c->victim_pg, SIGTERM), EPERM);
        expect("killpg_other_uid_probe_eperm", kill(-c->victim_pg, 0), EPERM);
    } else skip("killpg_other_uid_eperm", "needs root mode");
    expect("killpg_nonexistent_esrch", kill(-0x3ffffff0, SIGTERM), ESRCH);

    /* Same uid, own child: allowed, and the signal lands. */
    pid_t k = spawn_sleeper(0);
    expect("kill_own_child_ok", kill(k, SIGTERM), 0);
    check("kill_own_child_delivered", killed_by(k, SIGTERM), "child not killed by SIGTERM");
    k = spawn_sleeper(0);
    expect("tgkill_wrong_tgid_esrch", syscall(SYS_tgkill, getpid(), k, 0), ESRCH);
    expect("tgkill_own_child_ok", syscall(SYS_tgkill, k, k, SIGTERM), 0);
    check("tgkill_own_child_delivered", killed_by(k, SIGTERM), "child not killed");
    k = spawn_sleeper(0);
    {
        siginfo_t si; memset(&si, 0, sizeof si);
        si.si_signo = SIGUSR1; si.si_code = SI_USER;
        expect("rt_sigqueueinfo_si_user_spoof_eperm", syscall(SYS_rt_sigqueueinfo, k, SIGUSR1, &si), EPERM);
        si.si_code = SI_QUEUE;
        expect("rt_sigqueueinfo_own_child_ok", syscall(SYS_rt_sigqueueinfo, k, SIGUSR1, &si), 0);
        check("rt_sigqueueinfo_delivered", killed_by(k, SIGUSR1), "child not killed by SIGUSR1");
    }
}

static void user_mount_cases(void) {
    expect("mount_tmpfs_eperm", mount("none", "/tmp", "tmpfs", 0, NULL), EPERM);
    expect("mount_remount_eperm", mount(NULL, "/", NULL, MS_REMOUNT | MS_RDONLY, NULL), EPERM);
    expect("umount2_eperm", umount2("/tmp", 0), EPERM);
    expect("umount2_root_eperm", umount2("/", MNT_DETACH), EPERM);
    expect("pivot_root_eperm", syscall(SYS_pivot_root, "/tmp", "/tmp"), EPERM);
    expect("chroot_eperm", chroot("/"), EPERM);
}

static void user_cred_cases(void) {
    uid_t u = getuid(); gid_t g = getgid();
    expect("setuid_0_eperm", setuid(0), EPERM);
    expect("setgid_0_eperm", setgid(0), EPERM);
    expect("seteuid_0_eperm", seteuid(0), EPERM);
    expect("setresuid_e0_eperm", setresuid(-1, 0, -1), EPERM);
    expect("setresuid_s0_eperm", setresuid(-1, -1, 0), EPERM);
    expect("setresgid_r0_eperm", setresgid(0, -1, -1), EPERM);
    expect("setreuid_e0_eperm", syscall(SYS_setreuid, -1, 0), EPERM);
    expect("setreuid_r0_eperm", syscall(SYS_setreuid, 0, -1), EPERM);
    expect("setregid_e0_eperm", syscall(SYS_setregid, -1, 0), EPERM);
    expect("setreuid_self_ok", syscall(SYS_setreuid, u, u), 0);
    expect("setregid_self_ok", syscall(SYS_setregid, g, g), 0);
    expect("setuid_self_ok", setuid(u), 0);
    gid_t zero = 0;
    expect("setgroups_eperm", setgroups(1, &zero), EPERM);
    expect("setgroups_empty_eperm", setgroups(0, NULL), EPERM);
    uid_t r, e, s;
    getresuid(&r, &e, &s);
    check("creds_unchanged", r == u && e == u && s == u, "uid changed");
}

static void user_file_cases(struct ctx *c) {
    char f[64]; snprintf(f, sizeof f, "/tmp/privtest.%d.own", getpid());
    unlink(f);
    int fd = open(f, O_CREAT | O_WRONLY | O_EXCL, 0644);
    if (fd < 0) { skip("chown_*", "cannot create own file"); }
    else {
        close(fd);
        expect("chown_own_to_root_eperm", chown(f, 0, -1), EPERM);
        expect("chgrp_own_to_foreign_group_eperm", chown(f, -1, 0), EPERM);
        expect("chgrp_own_to_own_group_ok", chown(f, -1, getgid()), 0);
        expect("chmod_own_ok", chmod(f, 0600), 0);
        unlink(f);
    }
    expect("chmod_root_dir_eperm", chmod("/tmp", 01777), EPERM);
    expect("chown_root_dir_eperm", chown("/tmp", getuid(), -1), EPERM);
    expect("mknod_chr_eperm", mknod("/tmp/privtest.chr", S_IFCHR | 0600, makedev(1, 3)), EPERM);
    expect("mknod_blk_eperm", mknod("/tmp/privtest.blk", S_IFBLK | 0600, makedev(7, 0)), EPERM);
    char ff[64]; snprintf(ff, sizeof ff, "/tmp/privtest.%d.fifo", getpid());
    unlink(ff);
    expect("mknod_fifo_ok", mknod(ff, S_IFIFO | 0600, 0), 0);
    unlink(ff);

    if (c->rootfile[0]) {
        char d[80]; snprintf(d, sizeof d, "/tmp/privtest.%d.moved", getpid());
        char l[80]; snprintf(l, sizeof l, "/tmp/privtest.%d.lnk", getpid());
        expect("sticky_unlink_other_eperm", unlink(c->rootfile), EPERM);
        expect("sticky_rename_other_eperm", rename(c->rootfile, d), EPERM);
        expect("open_other_0644_wronly_eacces", open(c->rootfile, O_WRONLY), EACCES);
        expect("chmod_other_file_eperm", chmod(c->rootfile, 0666), EPERM);
        expect("chown_other_file_eperm", chown(c->rootfile, getuid(), -1), EPERM);
        expect("utimes_other_file_eperm", utimes(c->rootfile, (struct timeval[2]){{1,0},{1,0}}), EPERM);
        expect("open_other_0600_eacces", open(c->rootsecret, O_RDONLY), EACCES);
        /* fs.protected_hardlinks: a file we neither own nor can read+write. */
        expect("link_other_0600_eperm", link(c->rootsecret, l), EPERM);
        unlink(l); unlink(d);
    } else skip("sticky_*", "needs root mode");
}

static void user_misc_cases(void) {
    struct timeval tv; gettimeofday(&tv, NULL);
    expect("settimeofday_eperm", settimeofday(&tv, NULL), EPERM);
    struct timespec ts; clock_gettime(CLOCK_REALTIME, &ts);
    expect("clock_settime_eperm", clock_settime(CLOCK_REALTIME, &ts), EPERM);
    struct utsname un; uname(&un);
    expect("sethostname_eperm", sethostname(un.nodename, strlen(un.nodename)), EPERM);
    expect("setdomainname_eperm", setdomainname("x", 1), EPERM);
    expect("reboot_cad_on_eperm", syscall(SYS_reboot, 0xfee1dead, 672274793, 0x89ABCDEF, NULL), EPERM);
#if defined(__x86_64__)
    expect("iopl_eperm", syscall(SYS_iopl, 3), EPERM);
    expect("ioperm_eperm", syscall(SYS_ioperm, 0x80, 1, 1), EPERM);
#endif
    expect("kexec_load_eperm", syscall(SYS_kexec_load, 0, 0, NULL, 0), EPERM);
    expect("swapon_eperm", syscall(SYS_swapon, "/tmp/nonexistent-swap", 0), EPERM);
    expect("swapoff_eperm", syscall(SYS_swapoff, "/tmp/nonexistent-swap"), EPERM);
    expect("init_module_eperm", syscall(SYS_init_module, NULL, 0, ""), EPERM);
    expect("finit_module_eperm", syscall(SYS_finit_module, -1, "", 0), EPERM);
    expect("delete_module_eperm", syscall(SYS_delete_module, "nonexistent", 0), EPERM);
    expect("acct_eperm", syscall(SYS_acct, NULL), EPERM);
    expect("vhangup_eperm", syscall(SYS_vhangup), EPERM);

    /* capabilities(7): an unprivileged process holds none and cannot raise any. */
    struct { uint32_t version; int pid; } hdr = { 0x20080522, 0 };
    struct { uint32_t eff, perm, inh; } data[2];
    memset(data, 0xff, sizeof data);
    long r = syscall(SYS_capget, &hdr, data);
    check("capget_user_no_caps", r == 0 && data[0].eff == 0 && data[0].perm == 0 && data[1].eff == 0,
          "non-zero effective/permitted set");
    hdr.version = 0x20080522; hdr.pid = 0;
    memset(data, 0, sizeof data); data[0].eff = data[0].perm = 1u << 5; /* CAP_KILL */
    expect("capset_raise_eperm", syscall(SYS_capset, &hdr, data), EPERM);
    hdr.version = 0x20080522; hdr.pid = 0;
    memset(data, 0, sizeof data);
    expect("capset_empty_ok", syscall(SYS_capset, &hdr, data), 0);
}

static void user_sched_cases(struct ctx *c) {
    pid_t o = other_pid(c);
    /* Pin RLIMIT_NICE / RLIMIT_RTPRIO to 0 (lowering is always allowed) so the
     * host's pam_limits grants (e.g. an audio "realtime" group) don't apply. */
    struct rlimit z = { 0, 0 };
    expect("setrlimit_nice_lower_ok", setrlimit(RLIMIT_NICE, &z), 0);
    expect("setrlimit_rtprio_lower_ok", setrlimit(RLIMIT_RTPRIO, &z), 0);
    errno = 0;
    int cur = getpriority(PRIO_PROCESS, 0);
    int up = cur + 5 > 19 ? 19 : cur + 5;
    expect("setpriority_raise_nice_ok", setpriority(PRIO_PROCESS, 0, up), 0);
    expect("setpriority_lower_nice_eacces", setpriority(PRIO_PROCESS, 0, up - 1), EACCES);
    expect("setpriority_other_uid_eperm", setpriority(PRIO_PROCESS, o, 19), EPERM);
    /* Raw syscalls: musl's sched_setscheduler() is a deliberate ENOSYS stub. */
    struct sched_param sp = { .sched_priority = 1 };
    expect("sched_setscheduler_fifo_eperm", syscall(SYS_sched_setscheduler, 0, SCHED_FIFO, &sp), EPERM);
    expect("sched_setscheduler_rr_eperm", syscall(SYS_sched_setscheduler, 0, SCHED_RR, &sp), EPERM);
    sp.sched_priority = 0;
    expect("sched_setscheduler_other_self_ok", syscall(SYS_sched_setscheduler, 0, SCHED_OTHER, &sp), 0);
    expect("sched_setscheduler_other_uid_eperm", syscall(SYS_sched_setscheduler, o, SCHED_OTHER, &sp), EPERM);
    expect("sched_setscheduler_bad_policy_einval", syscall(SYS_sched_setscheduler, 0, 77, &sp), EINVAL);

    struct rlimit rl; getrlimit(RLIMIT_NOFILE, &rl);
    rlim_t v = rl.rlim_cur == RLIM_INFINITY ? 4096 : rl.rlim_cur;
    struct rlimit lo = { v, v };
    expect("setrlimit_lower_hard_ok", setrlimit(RLIMIT_NOFILE, &lo), 0);
    struct rlimit hi = { v, v + 1 };
    expect("setrlimit_raise_hard_eperm", setrlimit(RLIMIT_NOFILE, &hi), EPERM);
    struct rlimit inv = { v, v - 1 };
    expect("setrlimit_cur_gt_max_einval", setrlimit(RLIMIT_NOFILE, &inv), EINVAL);
    getrlimit(RLIMIT_NOFILE, &rl);
    check("getrlimit_reflects_lowered", rl.rlim_cur == v && rl.rlim_max == v, "limit not stored");
    struct rlimit old;
    expect("prlimit_other_uid_eperm", prlimit(o, RLIMIT_NOFILE, NULL, &old), EPERM);
}

static void user_pgid_cases(struct ctx *c) {
    expect("setpgid_not_child_esrch", setpgid(other_pid(c), other_pid(c)), ESRCH);
    expect("setpgid_foreign_session_group_eperm", setpgid(0, 1), EPERM);
    expect("setpgid_negative_einval", setpgid(0, -5), EINVAL);
    pid_t k = spawn_sleeper(0);
    expect("setpgid_own_child_ok", setpgid(k, k), 0);
    reap(k);
    /* A child that has become a session leader is in another session. */
    int pfd[2]; pipe(pfd);
    k = fork();
    if (k == 0) { setsid(); write(pfd[1], "x", 1); for (;;) pause(); }
    char ch; read(pfd[0], &ch, 1); close(pfd[0]); close(pfd[1]);
    expect("setpgid_child_other_session_eperm", setpgid(k, getpgrp()), EPERM);
    reap(k);
}

/* ── the audio device (LeandrOS only; SKIP elsewhere) ────────────────────────
 * /dev/pipewire is root:audio 0660 and single-writer (servers/pipewire). The
 * legacy players reach the same device through the audio server's IPC port
 * (auxv 258, SET_PARAMS = tag 0x100), which must apply the same checks. */
#define AUDIO_NODE "/dev/pipewire"
struct lmsg { uint64_t tag; uint32_t reply_port; uint8_t data[440]; uint64_t has_cap, cap; };
/* The audio server's answer to SET_PARAMS 44100/2 as a libc-style result
 * (-1 + errno), or -2 when there is no audio port. */
static long audio_ipc_set_params(void) {
    unsigned long port = getauxval(258);
    if (port == 0 || port == 0xffffffffUL) return -2;
    struct lmsg m; memset(&m, 0, sizeof m);
    m.tag = 0x100;
    uint32_t rate = 44100; memcpy(m.data, &rate, 4); m.data[4] = 2;
    if (syscall(513, (long)port, &m) < 0) return -1;
    int64_t rv; memcpy(&rv, m.data, 8);
    if (rv < 0) { errno = (int)-rv; return -1; }
    return 0;
}

static void user_dev_net_cases(void) {
    const char *blk[] = { "/dev/vda", "/dev/nvme0n1", "/dev/sda", NULL };
    const char *b = NULL;
    for (int i = 0; blk[i]; i++) if (access(blk[i], F_OK) == 0) { b = blk[i]; break; }
    if (b) {
        expect("open_blockdev_rdonly_eacces", open(b, O_RDONLY), EACCES);
        expect("open_blockdev_rdwr_eacces", open(b, O_RDWR), EACCES);
    } else skip("open_blockdev_*", "no block device node");
    if (access("/dev/loop-control", F_OK) == 0)
        expect("open_loop_control_eacces", open("/dev/loop-control", O_RDWR), EACCES);
    else skip("open_loop_control_eacces", "no /dev/loop-control");
    expect2("open_dev_mem_denied", open("/dev/mem", O_RDONLY), EACCES, ENOENT);
    /* Group-gated devices (root:input / root:video 0660): only meaningful for a
     * caller in neither group — the root-mode child, which drops every
     * supplementary group. */
    if (getgroups(0, NULL) == 0 && getegid() != 0) {
        const char *gated[] = { "/dev/input/event0", "/dev/dri/card0", "/dev/fb0", NULL };
        const char *names[] = { "open_evdev_no_group_eacces", "open_drm_card_no_group_eacces",
                                "open_fb0_no_group_eacces" };
        for (int i = 0; gated[i]; i++) {
            if (access(gated[i], F_OK) == 0) expect(names[i], open(gated[i], O_RDWR), EACCES);
            else skip(names[i], "no such node");
        }
    } else skip("open_evdev_no_group_eacces", "caller has supplementary groups");
    /* Not in `audio`: neither the node nor the IPC port may be used. */
    if (access(AUDIO_NODE, F_OK) != 0) skip("audio_*_no_group_eacces", "no " AUDIO_NODE);
    else if (getgroups(0, NULL) == 0 && getegid() != 0) {
        expect("audio_open_wronly_no_group_eacces", open(AUDIO_NODE, O_WRONLY), EACCES);
        long r = audio_ipc_set_params();
        if (r == -2) skip("audio_ipc_no_group_eacces", "no audio port in auxv");
        else expect("audio_ipc_no_group_eacces", r, EACCES);
    } else skip("audio_*_no_group_eacces", "caller has supplementary groups");

    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(80),
                             .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    expect("bind_tcp_port80_eacces", bind(s, (struct sockaddr *)&a, sizeof a), EACCES);
    a.sin_port = htons(0);
    expect("bind_tcp_port0_ok", bind(s, (struct sockaddr *)&a, sizeof a), 0);
    close(s);
    s = socket(AF_INET, SOCK_DGRAM, 0);
    a.sin_port = htons(53);
    expect("bind_udp_port53_eacces", bind(s, (struct sockaddr *)&a, sizeof a), EACCES);
    close(s);
    int r = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    expect("socket_raw_icmp_eperm", r, EPERM);
    if (r >= 0) close(r);
    r = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    expect("socket_dgram_icmp_ok", r, 0);
    if (r >= 0) close(r);
}

/* ── LeandrOS-private syscalls (SKIP on Linux, where auxv 256 is absent) ─────
 * SYS_SPAWN (510) and SYS_DBG_SERIAL_WRITE (590) are root-only. SYS_IPC_SEND
 * (511) / SYS_IPC_CALL (513) reach a server (handler) port or a queue port
 * whose owner the caller could ptrace — never another user's queue, and never
 * by naming that queue as the reply port of a server request (the server
 * would then write its answer there: a forged reply). */
#define LOS_SPAWN 510
#define LOS_IPC_SEND 511
#define LOS_IPC_CALL 513
#define LOS_PORT_CREATE 514
#define LOS_DBG_SERIAL_WRITE 590
static int on_leandros(void) { unsigned long v = getauxval(256); return v != 0 && v != 0xffffffffUL; }
static long los_send(long port, uint32_t reply) {
    struct lmsg m; memset(&m, 0, sizeof m);
    m.tag = 0x7e57; m.reply_port = reply;
    return syscall(LOS_IPC_SEND, port, &m, sizeof m);
}
static void user_leandros_cases(struct ctx *c) {
    if (!on_leandros()) { skip("los_*", "not LeandrOS"); return; }
    expect("los_spawn_non_root_eperm", syscall(LOS_SPAWN, 0x400000L, 0x800000L, 0L), EPERM);
    expect("los_dbg_serial_write_non_root_eperm", syscall(LOS_DBG_SERIAL_WRITE, "privtest\n", 9L), EPERM);
    long own = syscall(LOS_PORT_CREATE);
    check("los_port_create_ok", own > 0, "SYS_PORT_CREATE failed");
    if (own > 0) expect("los_ipc_send_own_port_ok", los_send(own, 0xffffffffu), 0);
    if (!c->rootport) { skip("los_ipc_*_root_port", "no root-owned port (not run as root)"); return; }
    expect("los_ipc_send_root_port_eperm", los_send(c->rootport, 0xffffffffu), EPERM);
    struct lmsg m; memset(&m, 0, sizeof m);
    expect("los_ipc_call_root_port_eperm", syscall(LOS_IPC_CALL, c->rootport, &m, sizeof m), EPERM);
    /* A server request whose reply would land in root's queue. The VFS port
     * itself is kernel-only (below), so aim at the audio server's port. */
    unsigned long ap = getauxval(258);
    if (ap == 0 || ap == 0xffffffffUL) skip("los_ipc_send_reply_to_root_port_eperm", "no audio port in auxv");
    else expect("los_ipc_send_reply_to_root_port_eperm", los_send((long)ap, (uint32_t)c->rootport), EPERM);
}

/* The VFS server trusts the buffer pointers in its requests: the kernel's own
 * syscall paths range-check them first and the in-kernel callers pass kernel
 * buffers. A raw IPC request from user code would skip that check (a VFS_READ
 * naming a kernel address is a kernel write), so user code may not send to
 * the VFS port at all. fd = -1 keeps an unfixed kernel from dereferencing. */
static void vfs_port_kernel_only(void) {
    if (!on_leandros()) { skip("los_ipc_vfs_port_*", "not LeandrOS"); return; }
    long vfs = (long)getauxval(256);
    struct lmsg m; memset(&m, 0, sizeof m);
    m.tag = 0x11; /* VFS_READ */
    m.reply_port = 0xffffffffu;
    uint64_t a[3] = { (uint64_t)-1, 0xffff800000000000ULL, 16 };
    memcpy(m.data, a, sizeof a);
    expect("los_ipc_call_vfs_port_eperm", syscall(LOS_IPC_CALL, vfs, &m, sizeof m), EPERM);
    memset(&m, 0, sizeof m); m.tag = 0x11; m.reply_port = 0xffffffffu; memcpy(m.data, a, sizeof a);
    expect("los_ipc_send_vfs_port_eperm", syscall(LOS_IPC_SEND, vfs, &m, sizeof m), EPERM);
}

/* AT_RANDOM must come from a real generator: distinct across execs, even
 * execs that land in the same timer tick (the old tick-derived bytes did
 * not). `privtest --atrandom` prints the 16 bytes as hex. */
static int atrandom_report(void) {
    const unsigned char *r = (const unsigned char *)getauxval(AT_RANDOM);
    if (!r) return 1;
    for (int i = 0; i < 16; i++) printf("%02x", r[i]);
    printf("\n");
    return 0;
}
static void atrandom_cases(void) {
    enum { N = 8 };
    char got[N][40]; int n = 0;
    char self[256] = "/proc/self/exe";
    ssize_t sl = readlink("/proc/self/exe", self, sizeof self - 1);
    if (sl > 0) self[sl] = 0; else strcpy(self, "/proc/self/exe");
    for (int i = 0; i < N; i++) {
        int pfd[2]; if (pipe(pfd) != 0) break;
        fflush(stdout);
        pid_t p = fork();
        if (p == 0) {
            dup2(pfd[1], 1); close(pfd[0]); close(pfd[1]);
            char *av[] = { "privtest", "--atrandom", NULL };
            execv(self, av);
            _exit(127);
        }
        close(pfd[1]);
        memset(got[n], 0, sizeof got[n]);
        ssize_t k = 0, t = 0;
        while (t < 39 && (k = read(pfd[0], got[n] + t, 39 - t)) > 0) t += k;
        close(pfd[0]);
        int st; waitpid(p, &st, 0);
        if (WIFEXITED(st) && WEXITSTATUS(st) == 0 && t >= 32) { got[n][32] = 0; n++; }
    }
    if (n < 2) { printf("  re-exec of %s failed\n", self); skip("atrandom_differs_across_execs", "cannot re-exec self"); return; }
    int dup = 0, zero = 0;
    for (int i = 0; i < n; i++) {
        if (!strcmp(got[i], "00000000000000000000000000000000")) zero = 1;
        for (int j = 0; j < i; j++) if (!strcmp(got[i], got[j])) dup = 1;
    }
    check("atrandom_differs_across_execs", !dup && !zero, "two execs saw the same AT_RANDOM bytes");
    if (dup) for (int i = 0; i < n; i++) printf("  %s\n", got[i]);
}

/* ── setsid and resource limits (setrlimit(2), setsid(2)) ────────────────────
 * Each limit is set in its own forked child, so nothing leaks into the other
 * cases. Every expectation here is Linux's. */

/* Wait up to `ms` for child `p`; returns the wait status, or -1 (then kills). */
static int wait_ms(pid_t p, int ms) {
    int st;
    for (int i = 0; i < ms / 10; i++) {
        pid_t r = waitpid(p, &st, WNOHANG);
        if (r == p) return st;
        usleep(10000);
    }
    kill(p, SIGKILL); waitpid(p, &st, 0);
    return -1;
}

static void setsid_child_cases(void *a) {
    (void)a;
    /* A fresh child is not a group leader: setsid works. */
    pid_t k = fork();
    if (k == 0) {
        pid_t s = setsid();
        _exit(s == getpid() && getsid(0) == getpid() && getpgrp() == getpid() ? 0 : 1);
    }
    int st = wait_ms(k, 5000);
    check("setsid_non_leader_ok", st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0, "setsid in a fresh child failed");
    /* A process-group leader may not: EPERM. */
    k = fork();
    if (k == 0) {
        if (setpgid(0, 0) != 0) _exit(2);
        errno = 0;
        long r = setsid();
        _exit(r == -1 && errno == EPERM ? 0 : 1);
    }
    st = wait_ms(k, 5000);
    check("setsid_group_leader_eperm", st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0, "setsid by a group leader succeeded");
    /* A session leader is a group leader too: a second setsid fails. */
    k = fork();
    if (k == 0) {
        if (setsid() < 0) _exit(2);
        errno = 0;
        long r = setsid();
        _exit(r == -1 && errno == EPERM ? 0 : 1);
    }
    st = wait_ms(k, 5000);
    check("setsid_twice_eperm", st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0, "second setsid succeeded");
}

static void fsize_on(const char *tag, const char *path) {
    char n1[96], n2[96], n3[96], n4[96], n5[96];
    snprintf(n1, sizeof n1, "rlimit_fsize_%s_write_to_limit_ok", tag);
    snprintf(n2, sizeof n2, "rlimit_fsize_%s_write_past_efbig", tag);
    snprintf(n3, sizeof n3, "rlimit_fsize_%s_pwrite_straddle_short", tag);
    snprintf(n4, sizeof n4, "rlimit_fsize_%s_ftruncate_past_efbig", tag);
    snprintf(n5, sizeof n5, "rlimit_fsize_%s_sigxfsz_kills", tag);
    pid_t k = fork();
    if (k == 0) {
        npass = nfail = 0;
        signal(SIGXFSZ, SIG_IGN);
        struct rlimit rl = { 4096, RLIM_INFINITY };
        int fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0600);
        if (fd < 0 || setrlimit(RLIMIT_FSIZE, &rl) != 0) { printf("%s: FAIL (setup %s)\n", n1, ename(errno)); _exit(1); }
        static char buf[4096];
        memset(buf, 'a', sizeof buf);
        check(n1, write(fd, buf, 4096) == 4096, "write up to the limit was refused");
        errno = 0;
        expect(n2, write(fd, buf, 1), EFBIG);
        check(n3, pwrite(fd, buf, 200, 4000) == 96, "write straddling the limit not cut at it");
        expect(n4, ftruncate(fd, 8192), EFBIG);
        close(fd);
        unlink(path);
        _exit(nfail);
    }
    int st = wait_ms(k, 10000);
    nfail += st >= 0 && WIFEXITED(st) ? WEXITSTATUS(st) : 1;
    /* SIGXFSZ's default action terminates the writer. */
    k = fork();
    if (k == 0) {
        struct rlimit rl = { 1024, RLIM_INFINITY };
        int fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0600);
        if (fd < 0) _exit(3);
        unlink(path);
        setrlimit(RLIMIT_FSIZE, &rl);
        static char buf[2048];
        write(fd, buf, 1024);
        write(fd, buf, 1);
        _exit(0);
    }
    st = wait_ms(k, 10000);
    check(n5, st >= 0 && WIFSIGNALED(st) && WTERMSIG(st) == SIGXFSZ, "writer past RLIMIT_FSIZE not killed by SIGXFSZ");
    unlink(path);
}

static int xcpu_fd = -1;
static void xcpu_note(int s) { (void)s; write(xcpu_fd, "x", 1); }

static void rlimit_child_cases(void *a) {
    (void)a;
    char path[96];
    /* RLIMIT_FSIZE on tmpfs (/tmp) and, when the cwd is writable, on its
     * filesystem (f2fs as /root on LeandrOS). */
    snprintf(path, sizeof path, "/tmp/privtest.%d.fsize", (int)getpid());
    fsize_on("tmp", path);
    snprintf(path, sizeof path, "privtest.%d.fsize", (int)getpid());
    int probe = open(path, O_CREAT | O_RDWR, 0600);
    if (probe >= 0) { close(probe); unlink(path); fsize_on("cwd", path); }
    else skip("rlimit_fsize_cwd_*", "cwd not writable");

    /* RLIMIT_NOFILE: the lowest free descriptor past the limit is EMFILE. */
    pid_t k = fork();
    if (k == 0) {
        struct rlimit rl = { 16, 16 };
        if (setrlimit(RLIMIT_NOFILE, &rl) != 0) _exit(2);
        int last = 0, err = 0;
        for (int i = 0; i < 64; i++) {
            int fd = open("/dev/null", O_RDONLY);
            if (fd < 0) { err = errno; break; }
            last = fd;
        }
        int d = dup2(0, 20);
        int derr = errno;
        _exit(err == EMFILE && last == 15 && d < 0 && derr == EBADF ? 0 : 1);
    }
    int st = wait_ms(k, 10000);
    check("rlimit_nofile_emfile", st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0, "open past RLIMIT_NOFILE not EMFILE");

    /* RLIMIT_NPROC: a non-root fork past it is EAGAIN. */
    if (getuid() != 0) {
        k = fork();
        if (k == 0) {
            struct rlimit rl = { 1, 1 };
            if (setrlimit(RLIMIT_NPROC, &rl) != 0) _exit(2);
            pid_t c = fork();
            if (c == 0) _exit(0);
            if (c > 0) { waitpid(c, NULL, 0); _exit(1); }
            _exit(errno == EAGAIN ? 0 : 1);
        }
        st = wait_ms(k, 10000);
        check("rlimit_nproc_fork_eagain", st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0, "fork past RLIMIT_NPROC not EAGAIN");
    } else skip("rlimit_nproc_fork_eagain", "root is exempt");

    /* RLIMIT_AS: a mapping that would take the address space past it. */
    k = fork();
    if (k == 0) {
        struct rlimit rl = { 256ul << 20, RLIM_INFINITY };
        if (setrlimit(RLIMIT_AS, &rl) != 0) _exit(2);
        void *big = mmap(NULL, 512ul << 20, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        int big_err = errno;
        void *small = mmap(NULL, 1ul << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        _exit(big == MAP_FAILED && big_err == ENOMEM && small != MAP_FAILED ? 0 : 1);
    }
    st = wait_ms(k, 10000);
    check("rlimit_as_mmap_enomem", st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0, "mmap past RLIMIT_AS not ENOMEM");

    /* RLIMIT_DATA: brk and private writable mappings count, read-only and
     * shared ones do not. */
    k = fork();
    if (k == 0) {
        struct rlimit rl = { 64ul << 20, RLIM_INFINITY };
        if (setrlimit(RLIMIT_DATA, &rl) != 0) _exit(2);
        void *cur = (void *)syscall(SYS_brk, 0);
        void *want = (char *)cur + (128ul << 20);
        void *got = (void *)syscall(SYS_brk, want);
        int f = 0;
        if (got == want) f |= 1;              /* brk past the limit moved */
        void *pw = mmap(NULL, 128ul << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (pw != MAP_FAILED || errno != ENOMEM) f |= 2;
        void *ro = mmap(NULL, 128ul << 20, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (ro == MAP_FAILED) f |= 4;
        void *sh = mmap(NULL, 128ul << 20, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
        if (sh == MAP_FAILED) f |= 8;
        void *ok = (void *)syscall(SYS_brk, (char *)cur + 4096);
        if (ok != (char *)cur + 4096) f |= 16;   /* small growth is fine */
        _exit(f);
    }
    st = wait_ms(k, 10000);
    check("rlimit_data_brk_mmap", st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0, "RLIMIT_DATA not applied as on Linux (bits in exit code)");
    if (st >= 0 && WIFEXITED(st) && WEXITSTATUS(st)) printf("   rlimit_data exit bits 0x%x\n", WEXITSTATUS(st));

    /* RLIMIT_STACK: an image exec'd under a 256 KiB limit overflows its
     * stack 1 MiB down (SIGSEGV); under 8 MiB it does not. */
    char self[256];
    ssize_t sl = readlink("/proc/self/exe", self, sizeof self - 1);
    if (sl <= 0) { skip("rlimit_stack_*", "no /proc/self/exe"); }
    else {
        self[sl] = 0;
        for (int big = 0; big < 2; big++) {
            k = fork();
            if (k == 0) {
                struct rlimit rl = { big ? (8ul << 20) : (256ul << 10), RLIM_INFINITY };
                if (setrlimit(RLIMIT_STACK, &rl) != 0) _exit(2);
                char *av[] = { self, "--stackprobe", NULL };
                execv(self, av);
                _exit(3);
            }
            st = wait_ms(k, 20000);
            if (big) check("rlimit_stack_8m_deep_ok", st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0, "1 MiB of stack failed under an 8 MiB limit");
            else check("rlimit_stack_256k_segv", st >= 0 && WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV, "1 MiB of stack under a 256 KiB limit did not SIGSEGV");
        }
    }

    /* RLIMIT_CPU: SIGXCPU at the soft limit, SIGKILL at the hard one. */
    k = fork();
    if (k == 0) {
        struct rlimit rl = { 1, 30 };
        if (setrlimit(RLIMIT_CPU, &rl) != 0) _exit(2);
        for (;;) __asm__ volatile("" ::: "memory");
    }
    st = wait_ms(k, 20000);
    check("rlimit_cpu_soft_sigxcpu", st >= 0 && WIFSIGNALED(st) && WTERMSIG(st) == SIGXCPU, "no SIGXCPU at the soft CPU limit");
    int pfd[2]; pipe(pfd);
    k = fork();
    if (k == 0) {
        close(pfd[0]);
        xcpu_fd = pfd[1];
        struct sigaction sa; memset(&sa, 0, sizeof sa);
        sa.sa_handler = xcpu_note;
        sigaction(SIGXCPU, &sa, NULL);
        struct rlimit rl = { 1, 2 };
        if (setrlimit(RLIMIT_CPU, &rl) != 0) _exit(2);
        for (;;) __asm__ volatile("" ::: "memory");
    }
    close(pfd[1]);
    st = wait_ms(k, 20000);
    char xb[8]; int nx = 0; ssize_t rr;
    fcntl(pfd[0], F_SETFL, O_NONBLOCK);
    while ((rr = read(pfd[0], xb, sizeof xb)) > 0) nx += (int)rr;
    close(pfd[0]);
    check("rlimit_cpu_hard_sigkill", st >= 0 && WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL && nx >= 1,
          "handled SIGXCPU not followed by SIGKILL at the hard limit");
}

/* --stackprobe: take 1 MiB of stack and touch it a page at a time, from the
 * top down (as a deep recursion would). */
static int __attribute__((noinline)) stackprobe(void) {
    volatile char *p = __builtin_alloca(1 << 20);
    for (long i = (1 << 20) - 4096; i >= 0; i -= 4096) p[i] = 1;
    return p[0] - 1;
}

static void user_session_rlimit_cases(void) {
    nfail += in_child(setsid_child_cases, NULL);
    nfail += in_child(rlimit_child_cases, NULL);
}
/* Root: RLIMIT_FSIZE on the cwd's filesystem (f2fs /root on LeandrOS),
 * which the unprivileged run usually cannot write. */
static void root_fsize_cwd(void *a) {
    (void)a;
    char path[96];
    snprintf(path, sizeof path, "privtest.%d.rootfsize", (int)getpid());
    fsize_on("root_cwd", path);
}

/* ── AArch64 kernel hardening (LeandrOS only): PAN and PXN ──────────────────
 * SYS_SECPROBE (591, root only) makes the kernel do what it must never do on
 * its own: load from a user page outside sched::uaccess (PAN must trap it),
 * and branch to user code (PXN must refuse the fetch). */
#define LOS_SECPROBE 591
#if defined(__aarch64__) || defined(__x86_64__)
static void pan_probe_child(void *a) {
    long *page = a;
    long r = syscall(LOS_SECPROBE, 1L, (long)page);
    printf("   secprobe raw load returned %ld\n", r);
    _exit(r == 1 ? 0 : 1);   /* 1: PAN trapped it (audit); 0: unhindered */
}
#endif
static void root_hardening_cases(void) {
    if (!on_leandros()) { skip("kernel_pan_*", "not LeandrOS"); return; }
#if defined(__aarch64__)
    long mode = syscall(LOS_SECPROBE, 0L, 0L);
    if (mode < 0) { check("kernel_secprobe", 0, "SYS_SECPROBE failed"); return; }
    long *page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    page[0] = 0x1234;
    if (!(mode & 1)) skip("kernel_pan_traps_raw_access", "no FEAT_PAN on this CPU (or pan=off)");
    else {
        fflush(stdout);
        pid_t k = fork();
        if (k == 0) pan_probe_child(page);
        int st = wait_ms(k, 10000);
        /* strict: the kernel kills the task; audit: the trap is counted and
         * the access let through. Either proves PAN fired. */
        int ok = st >= 0 && ((WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV) || (WIFEXITED(st) && WEXITSTATUS(st) == 0));
        check("kernel_pan_traps_raw_access", ok, "kernel read user memory with PAN not trapping it");
        printf("   PAN traps since boot: %ld (mode %s)\n", syscall(LOS_SECPROBE, 3L, 0L),
               (mode & 2) ? "strict" : "audit");
    }
    /* uaccess still works with PAN on: data through a pipe both ways. */
    {
        int up[2]; long back = 0;
        struct timespec ts = { 0, 1000 };
        int ok = pipe(up) == 0 && write(up[1], page, 8) == 8 && read(up[0], &back, 8) == 8 && back == 0x1234
                 && nanosleep(&ts, NULL) == 0;
        close(up[0]); close(up[1]);
        check("kernel_uaccess_under_pan_ok", ok, "copy to/from user broke");
    }
    /* PXN: the kernel must not execute a user code page. */
    uint32_t *code = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    code[0] = 0xd280bd80; /* mov x0, #0x5ec */
    code[1] = 0xd65f03c0; /* ret */
    __builtin___clear_cache((char *)code, (char *)(code + 2));
    mprotect(code, 4096, PROT_READ | PROT_EXEC);
    check("user_code_page_runs_at_el0", ((long (*)(void))code)() == 0x5ec, "EL0 could not run its own code page");
    /* EL0 cache maintenance, as a JIT does it (SCTLR_EL1.UCT/UCI): read
     * CTR_EL0, clean+invalidate a data line, invalidate an icache line.
     * Without the enables each traps to EL1 and the child dies (SIGILL). */
    fflush(stdout);
    pid_t cm = fork();
    if (cm == 0) {
        unsigned long ctr;
        __asm__ volatile("mrs %0, ctr_el0" : "=r"(ctr));
        __asm__ volatile("dc civac, %0\n dc cvau, %0\n dsb ish\n ic ivau, %1\n dsb ish\n isb"
                         :: "r"(page), "r"(code) : "memory");
        _exit(ctr != 0 ? 0 : 1);
    }
    int cst = wait_ms(cm, 10000);
    check("el0_cache_maintenance_ok", cst >= 0 && WIFEXITED(cst) && WEXITSTATUS(cst) == 0,
          "EL0 CTR_EL0/DC/IC trapped");
    fflush(stdout);
    pid_t k = fork();
    if (k == 0) {
        long r = syscall(LOS_SECPROBE, 2L, (long)code);
        printf("   secprobe kernel call of user code returned %#lx\n", r);
        _exit(1);
    }
    int st = wait_ms(k, 10000);
    check("kernel_pxn_refuses_user_code", st >= 0 && WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV,
          "kernel executed a user code page (PXN clear)");
    munmap(code, 4096); munmap(page, 4096);
#elif defined(__x86_64__)
    /* x86_64: SMAP is the PAN counterpart, SMEP the PXN one. */
    long mode = syscall(LOS_SECPROBE, 0L, 0L);
    if (mode < 0) { check("kernel_secprobe", 0, "SYS_SECPROBE failed"); return; }
    long *page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    page[0] = 0x1234;
    if (!(mode & 1)) skip("kernel_smap_traps_raw_access", "no SMAP on this CPU (or pan=off)");
    else {
        fflush(stdout);
        pid_t k = fork();
        if (k == 0) pan_probe_child(page);
        int st = wait_ms(k, 10000);
        int ok = st >= 0 && ((WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV) || (WIFEXITED(st) && WEXITSTATUS(st) == 0));
        check("kernel_smap_traps_raw_access", ok, "kernel read user memory with SMAP not trapping it");
        printf("   SMAP traps since boot: %ld (mode %s)\n", syscall(LOS_SECPROBE, 3L, 0L),
               (mode & 2) ? "strict" : "audit");
    }
    {
        int up[2]; long back = 0;
        struct timespec ts = { 0, 1000 };
        int ok = pipe(up) == 0 && write(up[1], page, 8) == 8 && read(up[0], &back, 8) == 8 && back == 0x1234
                 && nanosleep(&ts, NULL) == 0;
        close(up[0]); close(up[1]);
        check("kernel_uaccess_under_smap_ok", ok, "copy to/from user broke");
    }
    /* SMEP: the kernel must not execute a user code page. */
    unsigned char *code = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    static const unsigned char mov_ret[] = { 0xb8, 0xec, 0x05, 0x00, 0x00, 0xc3 }; /* mov eax, 0x5ec; ret */
    memcpy(code, mov_ret, sizeof mov_ret);
    mprotect(code, 4096, PROT_READ | PROT_EXEC);
    check("user_code_page_runs_at_ring3", ((long (*)(void))code)() == 0x5ec, "ring 3 could not run its own code page");
    fflush(stdout);
    pid_t k = fork();
    if (k == 0) {
        long r = syscall(LOS_SECPROBE, 2L, (long)code);
        printf("   secprobe kernel call of user code returned %#lx\n", r);
        _exit(1);
    }
    int st = wait_ms(k, 10000);
    check("kernel_smep_refuses_user_code", st >= 0 && WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV,
          "kernel executed a user code page (SMEP off)");
    munmap(code, 4096); munmap(page, 4096);
#else
    skip("kernel_pan_*", "aarch64 only");
#endif
}

static void user_cases(void *arg) {
    struct ctx *c = arg;
    printf("-- unprivileged cases as uid %d euid %d (pid %d)\n", (int)getuid(), (int)geteuid(), (int)getpid());
    user_kill_cases(c);
    user_mount_cases();
    user_misc_cases();
    user_file_cases(c);
    user_sched_cases(c);
    user_pgid_cases(c);
    user_dev_net_cases();
    user_leandros_cases(c);
    vfs_port_kernel_only();
    atrandom_cases();
    user_cred_cases();
    user_session_rlimit_cases();
}

/* ── root mode ─────────────────────────────────────────────────────────────── */

static void drop_and_run(void *arg) {
    if (drop_to(U, G) < 0) { printf("drop_to_uid: FAIL (errno %s)\n", ename(errno)); nfail++; return; }
    user_cases(arg);
}

/* kill(-pgrp) where the group mixes a root process and one of ours:
 * delivered to ours, EPERM swallowed, return 0, root process untouched. */
struct pg_arg { pid_t pg; pid_t root_member; };
static void mixed_pgrp_case(void *arg) {
    struct pg_arg *a = arg;
    if (setpgid(0, a->pg) < 0) { printf("killpg_mixed: FAIL (setpgid %s)\n", ename(errno)); nfail++; return; }
    if (drop_to(U, G) < 0) { printf("killpg_mixed: FAIL (drop)\n"); nfail++; return; }
    signal(SIGUSR1, SIG_IGN);
    pid_t k = spawn_sleeper(0);
    expect("killpg_mixed_group_ok", kill(-a->pg, SIGUSR1), 0);
    check("killpg_mixed_group_own_member_killed", killed_by(k, SIGUSR1), "own member not killed");
    check("killpg_mixed_group_root_member_alive", kill(a->root_member, 0) == -1 && errno == EPERM,
          "root member gone or probe allowed");
}

/* kill(-1) as a uid that owns only its own children: Linux signals what it
 * may, counts everything, and returns 0 even though most targets were EPERM. */
static void kill_all_case(void *arg) {
    pid_t root_member = *(pid_t *)arg;
    if (drop_to(4242, 4242) < 0) { printf("kill_minus1: FAIL (drop)\n"); nfail++; return; }
    signal(SIGUSR1, SIG_IGN);
    pid_t k = spawn_sleeper(0);
    expect("kill_minus1_user_ok", kill(-1, SIGUSR1), 0);
    check("kill_minus1_own_process_killed", killed_by(k, SIGUSR1), "own process not killed");
    check("kill_minus1_root_process_alive", kill(root_member, 0) == -1 && errno == EPERM,
          "root process gone");
}

/* Root: the node's metadata, and the single-writer rule (a second writer
 * gets EBUSY — by open or by IPC — readers are not writers, and the device
 * is free again once the holder's last fd closes). SKIPped while something
 * (a PipeWire session) holds the device. */
static void root_audio_cases(void) {
    struct stat st;
    if (stat(AUDIO_NODE, &st) != 0) { skip("root_audio_*", "no " AUDIO_NODE); return; }
    check("audio_node_root_audio_0660",
          S_ISCHR(st.st_mode) && (st.st_mode & 07777) == 0660 && st.st_uid == 0 && st.st_gid == 29
          && major(st.st_rdev) == 116,
          "want crw-rw---- root:audio(29), major 116");
    int a = open(AUDIO_NODE, O_WRONLY);
    if (a < 0 && errno == EBUSY) { skip("root_audio_exclusive_*", "device held (PipeWire session?)"); return; }
    expect("root_audio_open_first_writer_ok", a, 0);
    if (a < 0) return;
    expect("root_audio_open_second_writer_ebusy", open(AUDIO_NODE, O_WRONLY), EBUSY);
    int r = open(AUDIO_NODE, O_RDONLY);
    expect("root_audio_open_reader_ok", r, 0);
    if (r >= 0) close(r);
    long ip = audio_ipc_set_params();
    if (ip == -2) skip("root_audio_ipc_while_held_ebusy", "no audio port in auxv");
    else expect("root_audio_ipc_while_held_ebusy", ip, EBUSY);
    int d = dup(a);
    close(a);
    expect("root_audio_dup_keeps_device", open(AUDIO_NODE, O_WRONLY), EBUSY);
    close(d);
    int b = open(AUDIO_NODE, O_WRONLY);
    expect("root_audio_reopen_after_close_ok", b, 0);
    if (b >= 0) close(b);
}

static void root_cases(struct ctx *c) {
    printf("-- root cases (pid %d)\n", (int)getpid());
    root_audio_cases();
    struct { uint32_t version; int pid; } hdr = { 0x20080522, 0 };
    struct { uint32_t eff, perm, inh; } data[2];
    memset(data, 0, sizeof data);
    long r = syscall(SYS_capget, &hdr, data);
    check("capget_root_has_cap_kill_sys_admin",
          r == 0 && (data[0].eff & (1u << 5)) && (data[0].eff & (1u << 21)), "missing caps");
    expect("root_kill_probe_ok", kill(c->victim, 0), 0);
    expect("root_tgkill_probe_ok", syscall(SYS_tgkill, c->victim, c->victim, 0), 0);
    expect("root_setpriority_lower_nice_ok", setpriority(PRIO_PROCESS, c->victim, -5), 0);
    struct rlimit old;
    expect("root_prlimit_other_ok", prlimit(c->victim, RLIMIT_NOFILE, NULL, &old), 0);
    struct utsname un; uname(&un);
    expect("root_sethostname_same_ok", sethostname(un.nodename, strlen(un.nodename)), 0);
    if (on_leandros()) {
        expect("los_dbg_serial_write_root_ok", syscall(LOS_DBG_SERIAL_WRITE, "", 0L), 0);
        if (c->rootport) expect("los_ipc_send_root_own_port_ok", los_send(c->rootport, 0xffffffffu), 0);
    } else skip("los_root_*", "not LeandrOS");
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        int f = 0;
        struct rlimit rl; getrlimit(RLIMIT_NOFILE, &rl);
        rlim_t v = rl.rlim_cur == RLIM_INFINITY ? 4096 : rl.rlim_cur;
        struct rlimit lo = { v, v }, hi = { v, v + 100 };
        if (setrlimit(RLIMIT_NOFILE, &lo) != 0) f |= 1;
        if (setrlimit(RLIMIT_NOFILE, &hi) != 0) f |= 2;   /* CAP_SYS_RESOURCE */
        if (setresuid(U, U, 0) != 0) f |= 4;               /* root: any ids */
        if (setuid(0) != 0) f |= 8;                        /* euid back to saved 0 */
        if (geteuid() != 0) f |= 16;
        if (setresuid(0, 0, 0) != 0) f |= 32;
        gid_t gs[2] = { 5, 7 };
        if (setgroups(2, gs) != 0) f |= 64;
        _exit(f);
    }
    int st; waitpid(p, &st, 0);
    check("root_rlimit_creds_positive", WIFEXITED(st) && WEXITSTATUS(st) == 0,
          "a root-only operation failed (bitmask in exit code)");
    if (WIFEXITED(st) && WEXITSTATUS(st)) printf("  mask=0x%x\n", WEXITSTATUS(st));
    /* setuid() as euid 0 is permanent: all three ids change, no way back. */
    p = fork();
    if (p == 0) {
        if (setuid(U) != 0) _exit(1);
        uid_t r2, e2, s2; getresuid(&r2, &e2, &s2);
        if (r2 != U || e2 != U || s2 != U) _exit(2);
        if (setuid(0) == 0 || errno != EPERM) _exit(3);
        _exit(0);
    }
    waitpid(p, &st, 0);
    check("root_setuid_is_permanent", WIFEXITED(st) && WEXITSTATUS(st) == 0, "setuid(U) as root not permanent");
}

/* ── set-user-ID / set-group-ID execve ──────────────────────────────────────
 * The kernel ignored the S_ISUID/S_ISGID bits, so a setuid-root program (the
 * /bin/claude launcher) ran with its caller's ids. A copy of this binary is
 * exec'd as `privtest --ids R E S RG EG SG SECURE` and exits 0 only when
 * getresuid/getresgid/AT_SECURE match. */
#ifndef PR_SET_NO_NEW_PRIVS
#define PR_SET_NO_NEW_PRIVS 38
#define PR_GET_NO_NEW_PRIVS 39
#endif
static int ids_report(char **v) {
    uid_t r, e, s; gid_t rg, eg, sg;
    getresuid(&r, &e, &s); getresgid(&rg, &eg, &sg);
    unsigned long sec = getauxval(AT_SECURE);
    long want[7]; for (int i = 0; i < 7; i++) want[i] = atol(v[i]);
    long got[7] = { r, e, s, rg, eg, sg, (long)sec };
    for (int i = 0; i < 7; i++) if (got[i] != want[i]) {
        printf("  ids: got %ld %ld %ld / %ld %ld %ld secure %ld\n",
               got[0], got[1], got[2], got[3], got[4], got[5], got[6]);
        return 1;
    }
    return 0;
}
/* fork; in the child optionally keep root with saved uid U, drop to U/G,
 * set no_new_privs, then exec the copy expecting `want`; returns its status. */
static int exec_ids(const char *bin, int drop, int nnp, int saved_u, const char *want) {
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        if (saved_u && setresuid(0, 0, U) != 0) _exit(90);
        if (drop && drop_to(U, G) != 0) _exit(91);
        if (nnp) {
            if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) _exit(92);
            if (prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 1) _exit(93);
        }
        char w[160]; snprintf(w, sizeof w, "%s", want);
        char *av[10] = { (char *)bin, "--ids" }; int n = 2;
        for (char *t = strtok(w, " "); t && n < 9; t = strtok(NULL, " ")) av[n++] = t;
        av[n] = NULL;
        execv(bin, av);
        _exit(94);
    }
    int st; waitpid(p, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}
static void root_setid_exec_cases(void) {
    /* A disk filesystem: hosts commonly mount /tmp nosuid. */
    char bin[96]; snprintf(bin, sizeof bin, "/var/tmp/privtest-suid.%d", (int)getpid());
    if (mkdir("/var/tmp", 01777) == 0) chmod("/var/tmp", 01777);
    int in = open("/proc/self/exe", O_RDONLY), out = open(bin, O_CREAT | O_WRONLY | O_TRUNC, 0700);
    char buf[65536]; ssize_t n; int ok = in >= 0 && out >= 0;
    while (ok && (n = read(in, buf, sizeof buf)) > 0) ok = write(out, buf, n) == n;
    if (in >= 0) close(in);
    if (out >= 0) close(out);
    if (!ok || chown(bin, 0, 0) != 0) { skip("setuid_exec", "cannot copy the binary"); unlink(bin); return; }
    char w[160];
    chmod(bin, 04755);
    snprintf(w, sizeof w, "%u 0 0 %u %u %u 1", U, G, G, G);
    check("setuid_exec_gives_owner_euid", exec_ids(bin, 1, 0, 0, w) == 0, "euid/suid not the owner, or AT_SECURE clear");
    snprintf(w, sizeof w, "%u %u %u %u %u %u 0", U, U, U, G, G, G);
    check("setuid_exec_ignored_with_no_new_privs", exec_ids(bin, 1, 1, 0, w) == 0, "no_new_privs did not stop the set-user-ID");
    chmod(bin, 02755);
    snprintf(w, sizeof w, "%u %u %u %u 0 0 1", U, U, U, G);
    check("setgid_exec_gives_group_egid", exec_ids(bin, 1, 0, 0, w) == 0, "egid/sgid not the file's group");
    chmod(bin, 02745); /* S_ISGID without group execute: not set-group-ID */
    snprintf(w, sizeof w, "%u %u %u %u %u %u 0", U, U, U, G, G, G);
    check("setgid_without_gx_is_not_setgid", exec_ids(bin, 1, 0, 0, w) == 0, "S_ISGID without g+x granted the group");
    chmod(bin, 0755);
    /* No set-ID bit: the saved uid becomes the effective one (root, here). */
    check("exec_saved_uid_follows_euid", exec_ids(bin, 0, 0, 1, "0 0 0 0 0 0 0") == 0, "suid not reset to euid at exec");
    chmod(bin, 04755);
    expect("chown_root_file", chown(bin, 0, 0), 0);
    struct stat st;
    check("chown_drops_setuid_bit", stat(bin, &st) == 0 && !(st.st_mode & 04000), "S_ISUID survived chown");
    unlink(bin);
}

/* ── set-ID bits stripped by writes (Linux file_remove_privs) ───────────────
 * A write, ftruncate or O_TRUNC open by anyone but root clears S_ISUID, and
 * S_ISGID when group execute is set. Root (CAP_FSETID) keeps them. */
struct wr_arg { const char *path; int how; }; /* 0 write, 1 ftruncate, 2 O_TRUNC */
static void wr_as_user(void *a) {
    struct wr_arg *w = a;
    if (drop_to(U, G) != 0) _exit(90);
    int fd = open(w->path, O_WRONLY | (w->how == 2 ? O_TRUNC : 0));
    if (fd < 0) _exit(91);
    if (w->how == 0 && write(fd, "x", 1) != 1) _exit(92);
    if (w->how == 1 && ftruncate(fd, 0) != 0) _exit(93);
    close(fd);
}
static mode_t mode_of(const char *p) { struct stat st; return stat(p, &st) == 0 ? (st.st_mode & 07777) : 0; }
static void setid_write_cases_in(const char *dir, const char *fs) {
    char f[128], n[96]; snprintf(f, sizeof f, "%s/privtest-wr.%d", dir, (int)getpid());
    int fd = open(f, O_CREAT | O_WRONLY | O_TRUNC, 0600);
    if (fd < 0) { snprintf(n, sizeof n, "setid_write_%s", fs); skip(n, "cannot create the file"); return; }
    close(fd);
    static const char *how[] = { "write", "ftruncate", "otrunc" };
    for (int h = 0; h < 3; h++) {
        struct wr_arg w = { f, h };
        chmod(f, 06777);
        int st = in_child(wr_as_user, &w);
        snprintf(n, sizeof n, "setid_%s_by_user_strips_%s", how[h], fs);
        check(n, st == 0 && mode_of(f) == 0777, "S_ISUID/S_ISGID survived, or the user could not write");
    }
    /* S_ISGID without group execute is not set-group-ID: it stays. */
    chmod(f, 06767);
    struct wr_arg w = { f, 0 };
    int st = in_child(wr_as_user, &w);
    snprintf(n, sizeof n, "setgid_without_gx_survives_write_%s", fs);
    check(n, st == 0 && mode_of(f) == 02767, "mode not 02767 after the write");
    /* Root writes keep the bits. */
    chmod(f, 06777);
    fd = open(f, O_WRONLY | O_TRUNC);
    int ok = fd >= 0 && write(fd, "x", 1) == 1;
    if (fd >= 0) close(fd);
    snprintf(n, sizeof n, "setid_write_by_root_keeps_%s", fs);
    check(n, ok && mode_of(f) == 06777, "root's write stripped the set-ID bits");
    unlink(f);
}
static void root_setid_write_cases(void) {
    if (mkdir("/var/tmp", 01777) == 0) chmod("/var/tmp", 01777);
    setid_write_cases_in("/var/tmp", "disk");
    setid_write_cases_in("/tmp", "tmpfs");
}

/* ── nosuid mounts ────────────────────────────────────────────────────────────
 * exec ignores set-ID bits on an MS_NOSUID mount (Linux mnt_may_suid), and
 * /proc/mounts says so. A bind mount gets nosuid by remount (as on Linux);
 * a second, read-only mount of the root f2fs volume takes MS_NOSUID directly. */
#ifndef MS_NOSUID
#define MS_NOSUID 2
#endif
static int mounts_has_opt(const char *mp, const char *opt) {
    FILE *f = fopen("/proc/mounts", "r"); char line[1024]; int r = 0;
    if (!f) return 0;
    while (fgets(line, sizeof line, f)) {
        char d[256], m[256], t[64], o[256];
        if (sscanf(line, "%255s %255s %63s %255s", d, m, t, o) == 4 && !strcmp(m, mp)) {
            r = 0;
            for (char *tok = strtok(o, ","); tok; tok = strtok(NULL, ",")) if (!strcmp(tok, opt)) r = 1;
        }
    }
    fclose(f);
    return r;
}
static int copy_self(const char *dst, mode_t mode) {
    int in = open("/proc/self/exe", O_RDONLY), out = open(dst, O_CREAT | O_WRONLY | O_TRUNC, 0700);
    char buf[65536]; ssize_t n; int ok = in >= 0 && out >= 0;
    while (ok && (n = read(in, buf, sizeof buf)) > 0) ok = write(out, buf, n) == n;
    if (in >= 0) close(in);
    if (out >= 0) close(out);
    return ok && chown(dst, 0, 0) == 0 && chmod(dst, mode) == 0;
}
/* ── chmod by a non-member of the file's group drops S_ISGID ─────────────────
 * Linux setattr_prepare: without CAP_FSETID, chmod by a caller not in the
 * file's group clears S_ISGID; a member keeps it. tmpfs and the disk fs. */
struct sgid_arg { const char *path; int fd; };
static void sgid_chmod_child(void *a) {
    struct sgid_arg *s = a;
    if (drop_to(U, G) != 0) { nfail++; return; }
    int r = s->fd >= 0 ? fchmod(s->fd, 02755) : chmod(s->path, 02755);
    if (r != 0) nfail++;
}
static void sgid_chmod_cases_in(const char *dir, const char *fs) {
    char f[96], n[96]; struct stat st;
    snprintf(f, sizeof f, "%s/privtest-sgid.%d", dir, (int)getpid());
    int fd = open(f, O_CREAT | O_RDWR | O_TRUNC, 0755);
    if (fd < 0) { snprintf(n, sizeof n, "chmod_sgid_%s", fs); skip(n, "cannot create the file"); return; }
    struct sgid_arg a = { f, -1 };
    fchown(fd, U, 0); /* owner U, group root: U is not a member */
    snprintf(n, sizeof n, "chmod_nonmember_drops_sgid_%s", fs);
    check(n, in_child(sgid_chmod_child, &a) == 0 && stat(f, &st) == 0 && (st.st_mode & 07777) == 0755,
          "S_ISGID kept after chmod by a non-member");
    a.fd = fd;
    snprintf(n, sizeof n, "fchmod_nonmember_drops_sgid_%s", fs);
    check(n, in_child(sgid_chmod_child, &a) == 0 && stat(f, &st) == 0 && (st.st_mode & 07777) == 0755,
          "S_ISGID kept after fchmod by a non-member");
    a.fd = -1;
    fchown(fd, U, G);
    snprintf(n, sizeof n, "chmod_member_keeps_sgid_%s", fs);
    check(n, in_child(sgid_chmod_child, &a) == 0 && stat(f, &st) == 0 && (st.st_mode & 07777) == 02755,
          "S_ISGID dropped for a group member");
    fchown(fd, U, 0);
    chmod(f, 02755);
    snprintf(n, sizeof n, "chmod_root_keeps_sgid_%s", fs);
    check(n, stat(f, &st) == 0 && (st.st_mode & 07777) == 02755, "root's chmod lost S_ISGID");
    close(fd); unlink(f);
}
static void root_sgid_chmod_cases(void) {
    if (mkdir("/var/tmp", 01777) == 0) chmod("/var/tmp", 01777);
    sgid_chmod_cases_in("/var/tmp", "disk");
    sgid_chmod_cases_in("/tmp", "tmpfs");
}

/* ── tmpfs roots take nosuid / nodev / noexec ────────────────────────────────
 * /dev/shm is remounted (and mounted over) with the flags, then restored. */
#ifndef MS_NODEV
#define MS_NODEV 4
#endif
#ifndef MS_NOEXEC
#define MS_NOEXEC 8
#endif
static int exec_errno(const char *bin) {
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) { char *av[] = { (char *)bin, "--atrandom", NULL }; execv(bin, av); _exit(errno == EACCES ? 42 : 2); }
    int st; waitpid(p, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 3;
}
static void root_tmpfs_flag_cases(void) {
    char bin[96], w[160], plain[160];
    const char *mp = "/dev/shm";
    snprintf(w, sizeof w, "%u 0 0 %u %u %u 1", U, G, G, G);
    snprintf(plain, sizeof plain, "%u %u %u %u %u %u 0", U, U, U, G, G, G);
    snprintf(bin, sizeof bin, "%s/privtest-tf.%d", mp, (int)getpid());
    if (!copy_self(bin, 04755)) { skip("tmpfs_flags", "cannot copy the binary to /dev/shm"); unlink(bin); return; }
    check("tmpfs_suid_honoured", exec_ids(bin, 1, 0, 0, w) == 0, "set-user-ID ignored on a plain tmpfs");
    check("tmpfs_remount_nosuid_nodev", mount("tmpfs", mp, "tmpfs", MS_REMOUNT | MS_NOSUID | MS_NODEV, NULL) == 0,
          "remount,nosuid,nodev failed");
    check("tmpfs_nosuid_exec_ignores_setuid", exec_ids(bin, 1, 0, 0, plain) == 0, "set-user-ID honoured on a nosuid tmpfs");
    {
        struct statfs sf; int fd = open(bin, O_RDONLY);
        check("tmpfs_statfs_nosuid_nodev", statfs(mp, &sf) == 0 && (sf.f_flags & 6) == 6 && !(sf.f_flags & 8),
              "statfs f_flags lacks ST_NOSUID|ST_NODEV");
        check("tmpfs_fstatfs_nosuid_nodev", fd >= 0 && fstatfs(fd, &sf) == 0 && (sf.f_flags & 6) == 6,
              "fstatfs f_flags lacks ST_NOSUID|ST_NODEV");
        if (fd >= 0) close(fd);
    }
    check("tmpfs_nosuid_nodev_listed", mounts_has_opt(mp, "nosuid") && mounts_has_opt(mp, "nodev"),
          "/proc/mounts lacks nosuid/nodev");
    check("tmpfs_mount_noexec", mount("tmpfs", mp, "tmpfs", MS_NOEXEC, NULL) == 0, "mount -t tmpfs -o noexec failed");
    check("tmpfs_noexec_exec_eacces", exec_errno(bin) == 42, "execve on a noexec tmpfs was not EACCES");
    {
        struct statfs sf; int fd = open(bin, O_RDONLY);
        check("tmpfs_statfs_noexec", statfs(mp, &sf) == 0 && (sf.f_flags & 8) && !(sf.f_flags & 2),
              "statfs f_flags lacks ST_NOEXEC");
        check("tmpfs_fstatfs_noexec", fd >= 0 && fstatfs(fd, &sf) == 0 && (sf.f_flags & 8),
              "fstatfs f_flags lacks ST_NOEXEC");
        void *m = mmap(NULL, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd, 0);
        check("tmpfs_noexec_mmap_private_eperm", m == MAP_FAILED && errno == EPERM, "PROT_EXEC MAP_PRIVATE not EPERM");
        m = mmap(NULL, 4096, PROT_READ | PROT_EXEC, MAP_SHARED, fd, 0);
        check("tmpfs_noexec_mmap_shared_eperm", m == MAP_FAILED && errno == EPERM, "PROT_EXEC MAP_SHARED not EPERM");
        m = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
        check("tmpfs_noexec_mmap_read_ok", m != MAP_FAILED, "PROT_READ mapping refused on noexec");
        if (m != MAP_FAILED) {
            check("tmpfs_noexec_mprotect_exec_eperm", mprotect(m, 4096, PROT_READ | PROT_EXEC) != 0 && errno == EPERM,
                  "mprotect +PROT_EXEC not EPERM");
            munmap(m, 4096);
        }
        void *a = mmap(NULL, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        check("tmpfs_noexec_anon_exec_ok", a != MAP_FAILED, "anonymous PROT_EXEC refused");
        if (a != MAP_FAILED) munmap(a, 4096);
        if (fd >= 0) close(fd);
    }
    check("tmpfs_noexec_listed", mounts_has_opt(mp, "noexec") && !mounts_has_opt(mp, "nosuid"),
          "/proc/mounts options wrong after mount -o noexec");
    check("tmpfs_mount_elsewhere_einval", mount("tmpfs", "/var/tmp", "tmpfs", 0, NULL) != 0 && errno == EINVAL,
          "tmpfs mount on a non-root path did not fail with EINVAL");
    check("tmpfs_remount_clears_flags", mount(NULL, mp, NULL, MS_REMOUNT, NULL) == 0 && !mounts_has_opt(mp, "noexec"),
          "remount without flags kept noexec");
    {
        int fd = open(bin, O_RDONLY);
        void *m = mmap(NULL, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd, 0);
        check("tmpfs_mmap_exec_after_clear", m != MAP_FAILED, "PROT_EXEC mmap refused after the flags were cleared");
        if (m != MAP_FAILED) munmap(m, 4096);
        if (fd >= 0) close(fd);
    }
    check("tmpfs_exec_after_clear", exec_ids(bin, 1, 0, 0, w) == 0, "set-user-ID not honoured after the flags were cleared");
    unlink(bin);
}

static void root_nosuid_cases(void) {
    char bin[96], mp[96], w[160], plain[160];
    snprintf(w, sizeof w, "%u 0 0 %u %u %u 1", U, G, G, G);                 /* setuid honoured */
    snprintf(plain, sizeof plain, "%u %u %u %u %u %u 0", U, U, U, G, G, G); /* ignored */
    snprintf(bin, sizeof bin, "/var/tmp/privtest-ns.%d", (int)getpid());
    snprintf(mp, sizeof mp, "/var/tmp/privtest-nsb.%d", (int)getpid());
    if (!copy_self(bin, 04755)) { skip("nosuid_bind", "cannot copy the binary"); unlink(bin); return; }
    int fd = open(mp, O_CREAT | O_WRONLY, 0755); if (fd >= 0) close(fd);
    if (mount(bin, mp, NULL, MS_BIND, NULL) != 0) {
        skip("nosuid_bind", "bind mount failed");
    } else {
        int ok = mount(NULL, mp, NULL, MS_REMOUNT | MS_BIND | MS_NOSUID, NULL) == 0;
        check("nosuid_bind_remount", ok, "remount,bind,nosuid failed");
        check("nosuid_bind_exec_ignores_setuid", exec_ids(mp, 1, 0, 0, plain) == 0, "set-user-ID honoured on a nosuid mount");
        check("nosuid_bind_listed", mounts_has_opt(mp, "nosuid"), "/proc/mounts lacks nosuid");
        umount(mp);
    }
    unlink(mp);

    /* The root's f2fs device mounted again read-only (no extra disk needed). */
    char dev[256] = "", line[1024];
    FILE *f = fopen("/proc/mounts", "r");
    while (f && fgets(line, sizeof line, f)) {
        char d[256], m[256], t[64];
        if (sscanf(line, "%255s %255s %63s", d, m, t) == 3 && !strcmp(m, "/") && !strcmp(t, "f2fs"))
            snprintf(dev, sizeof dev, "%s", d);
    }
    if (f) fclose(f);
    char rbin[96], mnt[96], inner[200];
    snprintf(rbin, sizeof rbin, "/privtest-ns.%d", (int)getpid());
    snprintf(mnt, sizeof mnt, "/var/tmp/privtest-nsm.%d", (int)getpid());
    snprintf(inner, sizeof inner, "%s%s", mnt, rbin);
    if (!dev[0] || access(dev, F_OK) < 0) { skip("nosuid_mount", "root is not an f2fs block device"); unlink(bin); return; }
    if (!copy_self(rbin, 04755)) { skip("nosuid_mount", "cannot copy to /"); unlink(rbin); unlink(bin); return; }
    sync();
    mkdir(mnt, 0755);
    if (mount(dev, mnt, "f2fs", MS_RDONLY, NULL) == 0) {
        check("suid_mount_exec_honours_setuid", exec_ids(inner, 1, 0, 0, w) == 0, "control: set-user-ID not honoured");
        check("suid_mount_not_listed_nosuid", !mounts_has_opt(mnt, "nosuid"), "/proc/mounts says nosuid");
        umount(mnt);
    } else skip("suid_mount_exec_honours_setuid", "mount failed");
    if (mount(dev, mnt, "f2fs", MS_RDONLY | MS_NOSUID, NULL) == 0) {
        check("nosuid_mount_exec_ignores_setuid", exec_ids(inner, 1, 0, 0, plain) == 0, "set-user-ID honoured on a nosuid mount");
        check("nosuid_mount_listed", mounts_has_opt(mnt, "nosuid"), "/proc/mounts lacks nosuid");
        umount(mnt);
    } else check("nosuid_mount", 0, "mount -o ro,nosuid failed");
    rmdir(mnt);
    unlink(rbin);
    unlink(bin);
}

/* ── pidfd_getfd: the ptrace-attach check ─────────────────────────────────────
 * PTRACE_MODE_ATTACH_REALCREDS: the caller's real uid/gid must be every one
 * of the target's real/effective/saved ids, and the target dumpable. Same
 * real uid alone (a set-user-ID program the user started) is not enough. */
#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif
#ifndef SYS_pidfd_getfd
#define SYS_pidfd_getfd 438
#endif
#ifndef PR_SET_DUMPABLE
#define PR_SET_DUMPABLE 4
#define PR_GET_DUMPABLE 3
#endif
#define TARGET_FD 40
static pid_t spawn_target(int mode) {
    int fds[2]; char ch = 0;
    if (pipe(fds) < 0) return -1;
    pid_t p = fork();
    if (p == 0) {
        close(fds[0]);
        int ok = 1;
        if (mode == 1) ok = setgroups(0, NULL) == 0 && setresgid(G, G, G) == 0 && setresuid(U, 0, 0) == 0;
        if (mode >= 2) ok = drop_to(U, G) == 0;
        if (mode == 2) ok = ok && prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) == 0;
        if (mode == 3) ok = ok && prctl(PR_SET_DUMPABLE, 0, 0, 0, 0) == 0;
        /* mode 4: dropped ids only, which alone makes it non-dumpable. */
        if (mode == 4) ok = ok && prctl(PR_GET_DUMPABLE, 0, 0, 0, 0) == 0;
        /* The descriptor the attacker asks for: a pipe end at a fixed number. */
        int pp[2];
        ok = ok && pipe(pp) == 0 && dup2(pp[0], TARGET_FD) == TARGET_FD;
        write(fds[1], ok ? "y" : "n", 1); close(fds[1]);
        for (;;) pause();
    }
    close(fds[1]); if (p > 0) read(fds[0], &ch, 1); close(fds[0]);
    if (ch != 'y') { if (p > 0) reap(p); return -1; }
    return p;
}
struct getfd_arg { pid_t t[5]; };
static long getfd_of(pid_t t) {
    long pfd = syscall(SYS_pidfd_open, t, 0);
    if (pfd < 0) return -1;
    long r = syscall(SYS_pidfd_getfd, pfd, TARGET_FD, 0);
    int e = errno;
    if (r >= 0) close((int)r);
    close((int)pfd);
    errno = e;
    return r;
}
static void getfd_as_user(void *a) {
    struct getfd_arg *g = a;
    if (drop_to(U, G) != 0) { check("pidfd_getfd_drop", 0, "drop failed"); return; }
    expect("pidfd_getfd_setuid_target_eperm", getfd_of(g->t[1]), EPERM);
    expect("pidfd_getfd_same_user_ok", getfd_of(g->t[2]), 0);
    expect("pidfd_getfd_undumpable_eperm", getfd_of(g->t[3]), EPERM);
    expect("pidfd_getfd_after_id_change_eperm", getfd_of(g->t[4]), EPERM);
    expect("pidfd_getfd_root_target_eperm", getfd_of(getppid()), EPERM);
}
static void root_pidfd_getfd_cases(void) {
    struct getfd_arg g; memset(&g, 0, sizeof g);
    int ok = 1;
    for (int m = 1; m <= 4; m++) if ((g.t[m] = spawn_target(m)) <= 0) ok = 0;
    check("dumpable_cleared_by_id_change", g.t[4] > 0, "PR_GET_DUMPABLE not 0 after dropping ids");
    expect("prctl_set_dumpable_2_einval", prctl(PR_SET_DUMPABLE, 2, 0, 0, 0), EINVAL);
    if (ok) nfail += in_child(getfd_as_user, &g);
    else skip("pidfd_getfd", "target setup failed");
    for (int m = 1; m <= 4; m++) if (g.t[m] > 0) reap(g.t[m]);
}

/* ── fsuid / fsgid ────────────────────────────────────────────────────────────
 * setfsuid/setfsgid answer the previous filesystem id; file access and the
 * owner of a new file follow the fs ids, not the effective ones. */
static unsigned status_fsuid(void) {
    FILE *f = fopen("/proc/self/status", "r"); char line[256]; unsigned r = 0, a, b, c, d;
    while (f && fgets(line, sizeof line, f))
        if (sscanf(line, "Uid: %u %u %u %u", &a, &b, &c, &d) == 4) r = d;
    if (f) fclose(f);
    return r;
}
static void fsuid_root_child(void *a) {
    struct ctx *c = a;
    char f[96]; struct stat st;
    snprintf(f, sizeof f, "/tmp/privtest-fs.%d", (int)getpid());
    check("setfsuid_returns_previous", setfsuid(U) == 0, "setfsuid(U) did not answer 0");
    check("setfsuid_query", setfsuid(-1) == (int)U, "setfsuid(-1) did not answer U");
    check("setfsgid_returns_previous", setfsgid(G) == 0, "setfsgid(G) did not answer 0");
    check("setfsuid_keeps_euid", getuid() == 0 && geteuid() == 0, "real/effective uid changed");
    check("status_shows_fsuid", status_fsuid() == U, "/proc/self/status fs uid is not U");
    int fd = open(c->rootsecret, O_RDONLY);
    check("fsuid_used_for_access", fd < 0 && errno == EACCES, "root-only file opened with fsuid U");
    if (fd >= 0) close(fd);
    fd = open(f, O_CREAT | O_WRONLY | O_EXCL, 0644);
    check("fsuid_owns_new_file", fd >= 0 && fstat(fd, &st) == 0 && st.st_uid == U && st.st_gid == G,
          "new file not owned by fsuid/fsgid");
    if (fd >= 0) close(fd);
    unlink(f); /* sticky /tmp: still the owner (fsuid U) */
    check("setfsuid_back_to_root", setfsuid(0) == (int)U, "setfsuid(0) did not answer U");
    fd = open(c->rootsecret, O_RDONLY);
    check("fsuid_root_access_again", fd >= 0, "fsuid 0 cannot open the root-only file");
    if (fd >= 0) close(fd);
    /* fsuid follows every euid change. */
    if (setresuid(-1, U, -1) == 0) {
        check("fsuid_follows_euid", setfsuid(-1) == (int)U, "fsuid did not follow seteuid");
        setresuid(-1, 0, -1);
        check("fsuid_follows_euid_back", setfsuid(-1) == 0, "fsuid did not follow seteuid(0)");
    }
}
static void fsuid_user_child(void *a) {
    (void)a;
    if (drop_to(U, G) != 0) { check("fsuid_drop", 0, "drop failed"); return; }
    check("setfsuid_user_refused", setfsuid(0) == (int)U && setfsuid(-1) == (int)U, "unprivileged setfsuid(0) took effect");
    check("setfsgid_user_refused", setfsgid(0) == (int)G && setfsgid(-1) == (int)G, "unprivileged setfsgid(0) took effect");
}
static void root_fsuid_cases(struct ctx *c) {
    nfail += in_child(fsuid_root_child, c);
    nfail += in_child(fsuid_user_child, NULL);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc == 9 && !strcmp(argv[1], "--ids")) return ids_report(argv + 2);
    if (argc == 2 && !strcmp(argv[1], "--atrandom")) return atrandom_report();
    if (argc == 2 && !strcmp(argv[1], "--stackprobe")) return stackprobe();
    if (getenv("PRIVTEST_UID")) U = (uid_t)atoi(getenv("PRIVTEST_UID"));
    G = getenv("PRIVTEST_GID") ? (gid_t)atoi(getenv("PRIVTEST_GID")) : U;
    (void)argc; (void)argv;
    struct ctx c; memset(&c, 0, sizeof c);
    int fails = 0;
    if (geteuid() != 0) {
        user_cases(&c);
        fails = nfail;
    } else {
        signal(SIGUSR1, SIG_IGN); /* kill(-pgrp) cases must not hit us */
        c.victim = spawn_sleeper(0);
        c.victim_pg = spawn_sleeper(1);
        snprintf(c.rootfile, sizeof c.rootfile, "/tmp/privtest.%d.root", getpid());
        snprintf(c.rootsecret, sizeof c.rootsecret, "/tmp/privtest.%d.secret", getpid());
        int fd = open(c.rootfile, O_CREAT | O_WRONLY | O_TRUNC, 0644); if (fd >= 0) close(fd);
        chmod(c.rootfile, 0644);
        fd = open(c.rootsecret, O_CREAT | O_WRONLY | O_TRUNC, 0600); if (fd >= 0) close(fd);
        if (on_leandros()) {
            c.rootport = syscall(LOS_PORT_CREATE);
            if (c.rootport < 0) c.rootport = 0;
        }
        usleep(50000);
        root_cases(&c);
        root_setid_exec_cases();
        root_setid_write_cases();
        root_nosuid_cases();
        root_sgid_chmod_cases();
        root_tmpfs_flag_cases();
        root_pidfd_getfd_cases();
        root_fsuid_cases(&c);
        root_hardening_cases();
        nfail += in_child(root_fsize_cwd, NULL);
        fails += in_child(drop_and_run, &c);
        struct pg_arg pa = { c.victim_pg, c.victim_pg };
        fails += in_child(mixed_pgrp_case, &pa);
        fails += in_child(kill_all_case, &c.victim);
        check("victim_survived_all", child_alive(c.victim) && child_alive(c.victim_pg), "root victim died");
        fails += nfail;
        reap(c.victim); reap(c.victim_pg);
        unlink(c.rootfile); unlink(c.rootsecret);
    }
    printf("privtest: %s (%d failures)\n", fails ? "FAIL" : "PASS", fails);
    return fails > 100 ? 100 : fails;
}
