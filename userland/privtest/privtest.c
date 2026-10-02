/*
 * privtest — privilege checks that must hold for an unprivileged process,
 * with the results Linux gives (credentials(7), kill(2), mount(2),
 * capabilities(7)). The same file builds for the host (cc privtest.c) and for
 * LeandrOS (zig cc -target <arch>-linux-musl -static), so every expectation
 * below is checked against a real Linux kernel first.
 *
 *   privtest            as a normal user: the unprivileged cases only
 *   privtest            as root: root-only positives, then the unprivileged
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
#include <sys/mount.h>
#include <sys/auxv.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
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
    user_cred_cases();
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

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
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
        usleep(50000);
        root_cases(&c);
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
