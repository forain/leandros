//! Net-child confinement for privilege separation (design-sshdprivsep.md sections 3
//! and 7/P4): the id/chroot drop sequence and the seccomp allow-list.
//!
//! The filter builder is pure (numbers come from the tables below, not from
//! `libc::SYS_*`), so the host unit crate checks both architectures with the
//! seccomp-bpf interpreter.

use seccomp_bpf::*;

pub const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
pub const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;

const PR_SET_NAME: u64 = 15;
const PROT_EXEC: u64 = 4;
const FIONREAD: u64 = 0x541b;
const FIONBIO: u64 = 0x5421;

/// Syscalls the net child (tokio current_thread + russh + ring on a connected
/// socket and two socketpairs) may make. Everything else kills the process.
/// prctl, ioctl and mprotect are filtered by argument in `filter`.
const COMMON: &[(&str, i32, i32)] = &[
    // (name, x86_64, aarch64)
    ("read", 0, 63),
    ("write", 1, 64),
    ("readv", 19, 65),
    ("writev", 20, 66),
    ("close", 3, 57),
    ("recvmsg", 47, 212),
    ("sendmsg", 46, 211),
    ("recvfrom", 45, 207),
    ("sendto", 44, 206),
    ("shutdown", 48, 210),
    ("getsockopt", 55, 209),
    ("setsockopt", 54, 208),
    ("getsockname", 51, 204),
    ("getpeername", 52, 205),
    ("accept4", 288, 242),
    ("epoll_create1", 291, 20),
    ("epoll_ctl", 233, 21),
    ("epoll_pwait", 281, 22),
    ("epoll_pwait2", 441, 441),
    ("ppoll", 271, 73),
    ("eventfd2", 290, 19),
    ("futex", 202, 98),
    ("clock_gettime", 228, 113),
    ("clock_getres", 229, 114),
    ("clock_nanosleep", 230, 115),
    ("nanosleep", 35, 101),
    ("gettimeofday", 96, 169),
    ("mmap", 9, 222),
    ("munmap", 11, 215),
    ("mremap", 25, 216),
    ("madvise", 28, 233),
    ("brk", 12, 214),
    ("rt_sigprocmask", 14, 135),
    ("rt_sigaction", 13, 134),
    ("rt_sigreturn", 15, 139),
    ("sigaltstack", 131, 132),
    ("getrandom", 318, 278),
    ("getpid", 39, 172),
    ("gettid", 186, 178),
    ("sched_yield", 24, 124),
    ("sched_getaffinity", 204, 123),
    ("set_tid_address", 218, 96),
    ("set_robust_list", 273, 99),
    ("rseq", 334, 293),
    ("exit", 60, 93),
    ("exit_group", 231, 94),
    ("fcntl", 72, 25),
    // russh-cryptovec locks key material pages; the tokio waker uses a socketpair.
    ("mlock", 149, 228),
    ("munlock", 150, 229),
    ("socketpair", 53, 199),
];
// x86_64 only (aarch64 has no poll): the legacy poll.
const X86_ONLY: &[i32] = &[7];
const NR_PRCTL: (i32, i32) = (157, 167);
const NR_IOCTL: (i32, i32) = (16, 29);
const NR_MPROTECT: (i32, i32) = (10, 226);

fn ins(code: u16, k: u32) -> SockFilter {
    SockFilter::stmt(code, k)
}

/// Action for syscalls outside the list: KILL_PROCESS, or LOG (bring-up only).
pub fn build_filter(arch: u32, log_only: bool) -> Vec<SockFilter> {
    let x86 = arch == AUDIT_ARCH_X86_64;
    assert!(x86 || arch == AUDIT_ARCH_AARCH64);
    let pick = |a: (i32, i32)| if x86 { a.0 } else { a.1 } as u32;
    let deny = if log_only { SECCOMP_RET_LOG } else { SECCOMP_RET_KILL_PROCESS };
    let allow = SECCOMP_RET_ALLOW;
    let ld_nr = ins(BPF_LD | BPF_W | BPF_ABS, 0);
    let ld_arg = |n: u32| ins(BPF_LD | BPF_W | BPF_ABS, 16 + 8 * n); // low word of args[n]
    let mut f = vec![
        ins(BPF_LD | BPF_W | BPF_ABS, 4),
        SockFilter::jump(BPF_JMP | BPF_JEQ | BPF_K, arch, 1, 0),
        ins(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS), // wrong ABI: never log
        ld_nr,
    ];
    let mut simple: Vec<u32> = COMMON.iter().map(|&(_, x, a)| pick((x, a))).collect();
    if x86 {
        simple.extend(X86_ONLY.iter().map(|&n| n as u32));
    }
    for nr in simple {
        f.push(SockFilter::jump(BPF_JMP | BPF_JEQ | BPF_K, nr, 0, 1));
        f.push(ins(BPF_RET | BPF_K, allow));
    }
    // prctl: PR_SET_NAME only (the child renames itself after the filter is in).
    f.push(SockFilter::jump(BPF_JMP | BPF_JEQ | BPF_K, pick(NR_PRCTL), 0, 4));
    f.push(ld_arg(0));
    f.push(SockFilter::jump(BPF_JMP | BPF_JEQ | BPF_K, PR_SET_NAME as u32, 0, 1));
    f.push(ins(BPF_RET | BPF_K, allow));
    f.push(ins(BPF_RET | BPF_K, deny));
    // ioctl: FIONBIO / FIONREAD on the connection and the channels.
    f.push(SockFilter::jump(BPF_JMP | BPF_JEQ | BPF_K, pick(NR_IOCTL), 0, 6));
    f.push(ld_arg(1));
    f.push(SockFilter::jump(BPF_JMP | BPF_JEQ | BPF_K, FIONBIO as u32, 3, 0));
    f.push(SockFilter::jump(BPF_JMP | BPF_JEQ | BPF_K, FIONREAD as u32, 2, 0));
    // TIOCSWINSZ: window-change on the pty master the monitor handed over.
    f.push(SockFilter::jump(BPF_JMP | BPF_JEQ | BPF_K, 0x5414, 1, 0));
    f.push(ins(BPF_RET | BPF_K, deny));
    f.push(ins(BPF_RET | BPF_K, allow));
    // mprotect without PROT_EXEC (no JIT, no W^X flips).
    f.push(SockFilter::jump(BPF_JMP | BPF_JEQ | BPF_K, pick(NR_MPROTECT), 0, 4));
    f.push(ld_arg(2));
    f.push(SockFilter::jump(BPF_JMP | BPF_JSET | BPF_K, PROT_EXEC as u32, 1, 0));
    f.push(ins(BPF_RET | BPF_K, allow));
    f.push(ins(BPF_RET | BPF_K, deny));
    f.push(ins(BPF_RET | BPF_K, deny));
    f
}

#[cfg(target_os = "linux")]
pub fn host_arch() -> u32 {
    if cfg!(target_arch = "x86_64") { AUDIT_ARCH_X86_64 } else { AUDIT_ARCH_AARCH64 }
}

/// Install the allow-list (NO_NEW_PRIVS must already be set).
#[cfg(target_os = "linux")]
pub fn install_seccomp(log_only: bool) -> std::io::Result<()> {
    let f = build_filter(host_arch(), log_only);
    #[repr(C)]
    struct Prog {
        len: u16,
        filter: *const SockFilter,
    }
    let p = Prog { len: f.len() as u16, filter: f.as_ptr() };
    // PR_SET_SECCOMP = 22, SECCOMP_MODE_FILTER = 2
    if unsafe { libc::prctl(22, 2, &p as *const Prog, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Close every fd from 3 up, whatever its number, except those in `keep`.
/// close_range(2) per gap between kept fds; without it, walk up to the fd limit.
#[cfg(target_os = "linux")]
pub fn close_except(keep: &[i32]) {
    let mut ks: Vec<u32> = keep.iter().filter(|f| **f >= 3).map(|f| *f as u32).collect();
    ks.sort_unstable();
    let mut ok = true;
    let mut next = 3u32;
    for k in ks.iter().copied().chain(std::iter::once(u32::MAX)) {
        if k > next {
            let hi = if k == u32::MAX { u32::MAX } else { k - 1 };
            if unsafe { libc::syscall(libc::SYS_close_range, next, hi, 0u32) } != 0 {
                ok = false;
                break;
            }
        }
        next = k.saturating_add(1);
    }
    if !ok {
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) };
        let top = (lim.rlim_cur.min(1 << 20) as i32).max(1024);
        for fd in 3..top {
            if !keep.contains(&fd) {
                unsafe { libc::close(fd) };
            }
        }
    }
}

/// Drop to `uid`/`gid`, chroot into `dir`, set NNP, non-dumpable and rlimits.
/// Order matters: close every fd not in `keep` first, so the startup pipe and
/// the root log never survive into the unprivileged process.
#[cfg(target_os = "linux")]
pub fn confine(uid: u32, gid: u32, dir: &str, keep: &[i32]) -> Result<(), String> {
    let e = |what: &str| format!("{what}: {}", std::io::Error::last_os_error());
    unsafe {
        close_except(keep);
        let d = std::ffi::CString::new(dir).map_err(|_| "dir".to_string())?;
        // chroot while still root; ids last (chroot needs CAP_SYS_CHROOT).
        if libc::chroot(d.as_ptr()) != 0 {
            return Err(e("chroot"));
        }
        if libc::chdir(c"/".as_ptr()) != 0 {
            return Err(e("chdir"));
        }
        if libc::setgroups(0, std::ptr::null()) != 0 {
            return Err(e("setgroups"));
        }
        if libc::setresgid(gid, gid, gid) != 0 {
            return Err(e("setresgid"));
        }
        if libc::setresuid(uid, uid, uid) != 0 {
            return Err(e("setresuid"));
        }
        // Read back, and prove root cannot be regained.
        if libc::getuid() != uid || libc::geteuid() != uid || libc::getgid() != gid || libc::getegid() != gid {
            return Err("ids not dropped".into());
        }
        if libc::setuid(0) == 0 || libc::seteuid(0) == 0 {
            return Err("setuid(0) succeeded after the drop".into());
        }
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(e("NO_NEW_PRIVS"));
        }
        if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
            return Err(e("DUMPABLE"));
        }
        let core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &core);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nr(name: &str, arch: u32) -> i32 {
        let &(_, x, a) = COMMON.iter().find(|e| e.0 == name).unwrap();
        if arch == AUDIT_ARCH_X86_64 { x } else { a }
    }
    fn run_with(arch: u32, n: i32, args: [u64; 6], log: bool) -> u32 {
        let p = check_filter(&build_filter(arch, log)).expect("filter verifies");
        seccomp_bpf::run(&p, &SeccompData { nr: n, arch, instruction_pointer: 0, args })
    }

    #[test]
    fn allow_list_both_arches() {
        for arch in [AUDIT_ARCH_X86_64, AUDIT_ARCH_AARCH64] {
            for &(name, ..) in COMMON {
                assert_eq!(run_with(arch, nr(name, arch), [0; 6], false), SECCOMP_RET_ALLOW, "{name}");
            }
            // Forbidden: openat, socket, connect, bind, execve, clone, ptrace, kill, fork.
            let bad: &[i32] = if arch == AUDIT_ARCH_X86_64 {
                &[257, 41, 42, 49, 59, 56, 101, 62, 57, 2, 165]
            } else {
                &[56, 198, 203, 200, 221, 220, 117, 129, 40]
            };
            for &n in bad {
                assert_eq!(run_with(arch, n, [0; 6], false), SECCOMP_RET_KILL_PROCESS, "nr {n} arch {arch:x}");
                assert_eq!(run_with(arch, n, [0; 6], true), SECCOMP_RET_LOG);
            }
        }
    }

    #[test]
    fn wrong_arch_kills_even_in_log_mode() {
        let p = check_filter(&build_filter(AUDIT_ARCH_X86_64, true)).unwrap();
        let d = SeccompData { nr: 0, arch: AUDIT_ARCH_AARCH64, instruction_pointer: 0, args: [0; 6] };
        assert_eq!(seccomp_bpf::run(&p, &d), SECCOMP_RET_KILL_PROCESS);
    }

    #[test]
    fn argument_filters() {
        for arch in [AUDIT_ARCH_X86_64, AUDIT_ARCH_AARCH64] {
            let x = arch == AUDIT_ARCH_X86_64;
            let (prctl, ioctl, mprot) = (if x { 157 } else { 167 }, if x { 16 } else { 29 }, if x { 10 } else { 226 });
            assert_eq!(run_with(arch, prctl, [15, 0, 0, 0, 0, 0], false), SECCOMP_RET_ALLOW);
            assert_eq!(run_with(arch, prctl, [38, 1, 0, 0, 0, 0], false), SECCOMP_RET_KILL_PROCESS); // NNP
            assert_eq!(run_with(arch, prctl, [22, 2, 0, 0, 0, 0], false), SECCOMP_RET_KILL_PROCESS); // seccomp
            assert_eq!(run_with(arch, ioctl, [5, 0x5421, 0, 0, 0, 0], false), SECCOMP_RET_ALLOW);
            assert_eq!(run_with(arch, ioctl, [5, 0x5414, 0, 0, 0, 0], false), SECCOMP_RET_ALLOW);
            assert_eq!(run_with(arch, ioctl, [5, 0x5401, 0, 0, 0, 0], false), SECCOMP_RET_KILL_PROCESS); // TCGETS
            assert_eq!(run_with(arch, mprot, [0, 0, 3, 0, 0, 0], false), SECCOMP_RET_ALLOW);
            assert_eq!(run_with(arch, mprot, [0, 0, 5, 0, 0, 0], false), SECCOMP_RET_KILL_PROCESS); // PROT_EXEC
        }
    }
}
