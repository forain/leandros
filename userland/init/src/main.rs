//! LeandrOS Init - userspace init program (PID 1)
//!
//! This is the first userspace program that runs and manages the system.
//! It mounts the F2FS root filesystem, pivots root, mounts /etc/fstab, then
//! supervises two logins: the graphical one (greetd -> cosmic-comp ->
//! cosmic-greeter -> COSMIC session) on the display, and a getty on the
//! serial console.

#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{
    write, STDOUT_FILENO, getpid, execve, sched_yield, mount, pivot_root, mkdir, chown,
    open, read, close, dup3, O_RDONLY, O_WRONLY, O_CREAT, O_TRUNC, O_APPEND,
    fork, wait4, setsid, ioctl, usleep, exit, clock_gettime, timespec,
};
use leandros_libc::syscall::{nr, syscall2, syscall4};

const O_CLOEXEC: usize = 0x8_0000;

const TIOCSCTTY: usize = 0x540E;

// ── Graphical login ──────────────────────────────────────────────────────────
//
// The default login is graphical: greetd drives cosmic-comp in kiosk mode with
// cosmic-greeter as its client, and a successful login hands off to a COSMIC
// session (ports/greetd, ports/cosmic-greeter). init owns that chain the way it
// owns the serial getty: forked once, supervised, respawned when it exits.
//
// The serial getty loop stays. A serial `login:` is what the QEMU driver and
// every test harness talk to, and it is also the recovery path when the
// graphical stack is broken — exactly the arrangement Linux has with a display
// manager on the seat and agetty on ttyS0.

/// The launcher the graphical login runs through. `/bin/greeter-real` exports
/// the render environment (`/bin/greeter-env`) and execs `/bin/greetd`; it is
/// the same script a hand-started greeter uses, so the two paths cannot drift.
const DM_LAUNCHER: &[u8] = b"/bin/greeter-real\0";
const DM_SHELL: &[u8] = b"/bin/sh\0";
/// The daemon itself. Its absence (an image built without the greetd
/// artifacts) means "text login", not an error.
const DM_DAEMON: &[u8] = b"/bin/greetd\0";
const DM_CONFIG: &[u8] = b"/etc/greetd/greetd.conf\0";
/// Opt-out marker: when this file exists the boot lands on the serial/text
/// login only. Persistent across boots (it lives on the root f2fs), so a
/// test image can be switched with one `touch` from a root shell.
const DM_TEXT_LOGIN_MARKER: &[u8] = b"/etc/leandros/text-login\0";
/// Where the greeter chain's stdout/stderr go. greetd runs with `vt = "none"`
/// and passes its own stdio to the greeter and to the session, so this one
/// file carries cosmic-comp, cosmic-greeter, cosmic-session and every applet.
/// It is not the console on purpose: the framebuffer console is what the
/// compositor is about to draw over, and painting thousands of tracing lines
/// into it costs a full-surface memmove per scrolled line.
const DM_LOG: &[u8] = b"/var/log/greetd.log\0";
/// The previous generation, kept when `DM_LOG` reaches `DM_LOG_CAP`.
const DM_LOG_OLD: &[u8] = b"/var/log/greetd.log.1\0";
/// Size cap of one log generation. The chain writes through a pipe to a
/// logger child (`run_dm_logger`), which rotates at this size, so the log
/// costs at most two generations of disk however hard something spams it.
/// A cosmic-panel spinning on a dead Wayland connection (virgl, 2026-09-25)
/// wrote 8.4 M lines, over a gigabyte, before the guest died.
const DM_LOG_CAP: usize = 8 << 20;
/// Memory-pressure guard (`mem_guard`): when MemAvailable stays below this
/// floor for `MEM_GUARD_STRIKES` consecutive checks `MEM_GUARD_PERIOD_SECS`
/// apart, init kills the graphical login's tree, as systemd-oomd kills a
/// session's cgroup. The floor is the larger of a fixed minimum and a
/// fraction of RAM.
const MEM_GUARD_MIN_KIB: u64 = 96 * 1024;
const MEM_GUARD_DIVISOR: u64 = 10;
const MEM_GUARD_PERIOD_SECS: u64 = 2;
const MEM_GUARD_STRIKES: u32 = 2;
const DM_PID_FILE: &[u8] = b"/run/greetd-init.pid\0";
/// Respawn spacing and ceiling. A greeter chain that dies at once (a missing
/// library, a compositor that cannot open the GPU) must not become a fork
/// storm on the same console the serial login is trying to use.
///
/// The delay doubles on every death that follows a short run — 3, 6, 12, 24,
/// then 30 s — and drops back to 3 s once a chain has stayed up for
/// `DM_STABLE_SECS`: a greeter that crashed once after an hour is restarted
/// promptly, one that dies on every start is throttled to the cap, and the
/// respawn ceiling counts only consecutive short-lived runs.
const DM_RESPAWN_DELAY_US: u32 = 3_000_000;
const DM_RESPAWN_DELAY_MAX_US: u32 = 30_000_000;
const DM_STABLE_SECS: u64 = 60;
const DM_MAX_RESPAWNS: u32 = 20;
/// greeter-real's "no GPU renderer" refusal (EX_CONFIG); see ports/greetd/data/gpu-env.
const DM_EXIT_NO_GPU: i32 = 78;

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    sched_yield();
    write_str("LeandrOS Init (PID 1) starting...\n");

    write_str("Init PID: ");
    write_u32(getpid() as u32);
    write_str("\n");

    // 1. Mount F2FS disk (/dev/vdb is device index 1, i.e., f2fs-data0.img)
    write_str("Mounting F2FS disk /dev/vdb to /mnt...\n");
    let mount_res = mount(
        b"/dev/vdb\0".as_ptr(),
        b"/mnt\0".as_ptr(),
        b"f2fs\0".as_ptr(),
        0,
        core::ptr::null()
    );

    if mount_res < 0 {
        write_str("ERROR: Failed to mount /dev/vdb to /mnt!\n");
        loop { sched_yield(); }
    }
    write_str("F2FS mounted successfully at /mnt!\n");

    // 2. Create old_root directory on F2FS mount for pivoting
    mkdir(b"/mnt/old_root\0".as_ptr(), 0o755);

    // 4. Pivot root to F2FS mounted filesystem
    write_str("Pivoting root to /mnt (old root at /mnt/old_root)...\n");
    let pivot_res = pivot_root(b"/mnt\0".as_ptr(), b"/mnt/old_root\0".as_ptr());
    if pivot_res < 0 {
        write_str("ERROR: pivot_root failed!\n");
        loop { sched_yield(); }
    }
    write_str("pivot_root successful! Root is now F2FS.\n");

    // 4b. Mount anything else listed in /etc/fstab (the "/" entry was just
    // handled above by the hardcoded bootstrap mount — fstab can't drive
    // that one since fstab itself lives on the filesystem being mounted).
    mount_from_fstab();

    // 4c. One XDG runtime directory per account, owned by it, before any login
    // can need one.
    seed_runtime_dirs();

    // 5. Graphical login, when the image carries one and nothing opted out.
    let graphical = graphical_login_wanted();
    let (mut dm_pid, mut dm_logger) = if graphical { spawn_display_manager() } else { (0, 0) };
    let mut dm_respawns: u32 = 0;
    let mut dm_delay_us: u32 = DM_RESPAWN_DELAY_US;
    let mut dm_started: u64 = monotonic_secs();

    // 6. Supervisor loop. The serial getty is respawned every time it exits so
    // a login shell that exits (or a crashing login) always gets a new console
    // session rather than leaving init with no controlling tty; the graphical
    // login is respawned the same way, with spacing and a ceiling. wait4(-1)
    // also reaps any orphan reparented to init, which is simply ignored.
    write_str("Starting getty loop...\n");
    let mut login_pid: i32 = spawn_login();
    let mut guard = MemGuard { last_check: 0, strikes: 0 };
    loop {
        let mut status = 0i32;
        // WNOHANG and a short sleep instead of a blocking wait4, so the
        // memory-pressure guard gets to run while nothing exits.
        const WNOHANG: i32 = 1;
        let pid = wait4(-1, &mut status, WNOHANG, core::ptr::null_mut());
        if pid == 0 {
            if dm_pid > 0 { mem_guard(&mut guard, dm_pid); }
            usleep(250_000);
            continue;
        }
        if pid == dm_logger {
            dm_logger = 0;
            continue;
        }
        if pid <= 0 {
            // No children at all (both spawns failed): back off and retry.
            usleep(1_000_000);
            if login_pid <= 0 { login_pid = spawn_login(); }
            continue;
        }
        if pid == login_pid {
            write_str("session ended, restarting login\n");
            usleep(1_000_000);
            login_pid = spawn_login();
        } else if dm_pid > 0 && pid == dm_pid {
            dm_pid = 0;
            // greetd is gone, but its session tree may not be: a greeter
            // whose compositor died spins on the broken Wayland socket
            // forever, at ~180 MiB and a full CPU apiece, and every respawn
            // adds another. systemd would kill the unit's cgroup here; we
            // kill what the kernel has reparented to us (see `sweep_strays`).
            sweep_strays(login_pid, dm_logger);
            // Exit 78 (EX_CONFIG) from /bin/greeter-real: /bin/gpu-env found
            // no hardware GL renderer and refused to start a software-rendered
            // COSMIC. Not a crash — respawning cannot fix it — so say it once,
            // loudly, on the console and keep only the text login.
            if status & 0x7f == 0 && (status >> 8) & 0xff == DM_EXIT_NO_GPU {
                write_str("\n");
                write_str("################################################################\n");
                write_str("## NO GPU RENDERER: the graphical login (COSMIC) was NOT started.\n");
                write_str("## COSMIC renders on the host GPU only (zink/Venus or virgl);\n");
                write_str("## this VM has no GL-capable virtio-gpu, or its GPU stack failed.\n");
                write_str("## Details: /var/log/greetd.log. Software rendering is opt-in:\n");
                write_str("##   touch /etc/leandros/allow-software-render\n");
                write_str("################################################################\n");
                continue;
            }
            let ran_secs = monotonic_secs().saturating_sub(dm_started);
            if ran_secs >= DM_STABLE_SECS {
                dm_respawns = 0;
                dm_delay_us = DM_RESPAWN_DELAY_US;
            }
            dm_respawns += 1;
            if dm_respawns > DM_MAX_RESPAWNS {
                write_str("graphical login exited too many times; leaving the text login only\n");
                continue;
            }
            write_str("graphical login exited after ");
            write_u32(ran_secs as u32);
            write_str(" s, restarting in ");
            write_u32(dm_delay_us / 1_000_000);
            write_str(" s (");
            write_u32(dm_respawns);
            write_str("/");
            write_u32(DM_MAX_RESPAWNS);
            write_str(")\n");
            usleep(dm_delay_us);
            dm_delay_us = (dm_delay_us.saturating_mul(2)).min(DM_RESPAWN_DELAY_MAX_US);
            if graphical_login_wanted() {
                let (p, l) = spawn_display_manager();
                dm_pid = p;
                if l > 0 { dm_logger = l; }
                dm_started = monotonic_secs();
            }
        }
    }
}

/// CLOCK_MONOTONIC in whole seconds (0 if the clock is unavailable).
unsafe fn monotonic_secs() -> u64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    if clock_gettime(1, &mut ts) != 0 { return 0; }
    ts.tv_sec as u64
}

/// Kill every process the kernel has handed to init that init did not start
/// and that is not part of the serial login's session — the remains of a
/// display-manager tree whose greetd has just exited.
///
/// The kernel reparents orphans to init (POSIX), so once greetd, its session
/// worker and the compositor are gone, the compositor's stranded clients are
/// init's children with a parent pid of init's own. They are found the way
/// `ps` finds anything: `/proc/loadavg`'s last field is the highest pid
/// allocated, `/proc/<pid>/stat` gives ppid and session. The serial login
/// (and a `nohup` job it left behind, which shares its session) is never
/// touched; neither is the login pid itself.
///
/// SIGKILL, not SIGTERM: a greeter spinning on a dead compositor socket is
/// past graceful shutdown, and nothing else in that tree survives greetd on
/// a systemd host either.
unsafe fn sweep_strays(login_pid: i32, logger_pid: i32) {
    const SIGKILL: usize = 9;
    let me = getpid() as u32;
    let login_sid = if login_pid > 0 { proc_stat(login_pid as u32).map(|(_, _, _, s)| s) } else { None };
    let mut killed = 0u32;
    // Killing a stray orphans ITS children onto init in turn (greetd gone
    // with the compositor still up: the compositor is a stray, the greeter
    // becomes one only once the compositor is dead), so sweep until a pass
    // finds nothing, with a bounded number of passes.
    let mut pass = 0;
    while pass < 5 {
        let last = proc_last_pid();
        let mut killed_this_pass = 0u32;
        let mut pid = 1u32;
        while pid <= last {
            // The log writer is spared: it exits by itself on EOF, once the
            // last process holding the chain's stdout is gone, and killing it
            // would lose the dying chain's last lines.
            if pid != me && pid as i32 != login_pid && pid as i32 != logger_pid {
                if let Some((state, ppid, _pgid, sid)) = proc_stat(pid) {
                    let protected = match login_sid { Some(s) => s == sid, None => false };
                    // A zombie is already dead and waiting for the wait4 loop.
                    if ppid == me && !protected && state != b'Z' {
                        syscall2(nr::KILL, pid as usize, SIGKILL);
                        killed_this_pass += 1;
                    }
                }
            }
            pid += 1;
        }
        killed += killed_this_pass;
        if killed_this_pass == 0 { break; }
        usleep(200_000);
        pass += 1;
    }
    if killed > 0 {
        write_str("killed ");
        write_u32(killed);
        write_str(" stray process(es) left by the graphical login\n");
    }
}

/// `/proc/loadavg`'s last field: the highest pid allocated so far.
unsafe fn proc_last_pid() -> u32 {
    let mut buf = [0u8; 128];
    let n = read_file(b"/proc/loadavg\0", &mut buf);
    if n == 0 { return 0; }
    let line = &buf[..n];
    let end = line.iter().position(|&b| b == b'\n').unwrap_or(line.len());
    let line = &line[..end];
    let start = line.iter().rposition(|&b| b == b' ').map(|i| i + 1).unwrap_or(0);
    parse_u32(&line[start..]).unwrap_or(0)
}

/// `(state, ppid, pgid, sid)` from `/proc/<pid>/stat`, or `None` if `pid` is
/// not a live process.
unsafe fn proc_stat(pid: u32) -> Option<(u8, u32, u32, u32)> {
    let mut path = [0u8; 32];
    let mut p = 0;
    for &b in b"/proc/" { path[p] = b; p += 1; }
    p += fmt_u32(&mut path[p..], pid);
    for &b in b"/stat\0" { path[p] = b; p += 1; }
    let mut buf = [0u8; 256];
    let n = read_file(&path[..p], &mut buf);
    if n == 0 { return None; }
    // "pid (comm) state ppid pgid sid ..." — skip past the comm's closing paren.
    let line = &buf[..n];
    let close = line.iter().rposition(|&b| b == b')')?;
    let mut fields = line[close + 1..].split(|&b| b == b' ').filter(|f| !f.is_empty());
    let state = *fields.next()?.first()?;
    let ppid = parse_u32(fields.next()?)?;
    let pgid = parse_u32(fields.next()?)?;
    let sid = parse_u32(fields.next()?)?;
    Some((state, ppid, pgid, sid))
}

unsafe fn read_file(path: &[u8], buf: &mut [u8]) -> usize {
    let fd = open(path.as_ptr(), O_RDONLY, 0);
    if fd < 0 { return 0; }
    let mut total = 0usize;
    while total < buf.len() {
        let n = read(fd, buf.as_mut_ptr().add(total), buf.len() - total);
        if n <= 0 { break; }
        total += n as usize;
    }
    close(fd);
    total
}

fn fmt_u32(out: &mut [u8], mut n: u32) -> usize {
    let mut tmp = [0u8; 10];
    let mut i = 0;
    if n == 0 { tmp[0] = b'0'; i = 1; }
    while n > 0 { tmp[i] = b'0' + (n % 10) as u8; n /= 10; i += 1; }
    for j in 0..i { out[j] = tmp[i - 1 - j]; }
    i
}

/// Fork the serial getty: a fresh session running /bin/login. Returns the
/// child's pid, or -1 if the fork failed.
unsafe fn spawn_login() -> i32 {
    let pid = fork();
    if pid == 0 {
        run_session();
        // run_session only returns if both execve attempts failed.
        exit(1);
    } else if pid < 0 {
        write_str("ERROR: fork failed in getty loop\n");
    }
    pid
}

fn exists(path: &[u8]) -> bool {
    unsafe {
        let fd = open(path.as_ptr(), O_RDONLY, 0);
        if fd < 0 { return false; }
        close(fd);
        true
    }
}

/// The three-way decision: the daemon and its config are staged, and nobody
/// asked for a text login.
unsafe fn graphical_login_wanted() -> bool {
    if exists(DM_TEXT_LOGIN_MARKER) {
        write_str("graphical login disabled by /etc/leandros/text-login\n");
        return false;
    }
    if !exists(DM_DAEMON) || !exists(DM_CONFIG) {
        write_str("no greetd in this image; text login only\n");
        return false;
    }
    true
}

/// Fork the graphical login chain. Returns `(chain pid, logger pid)`; either
/// is -1 (chain) or 0 (logger) when it could not be started.
///
/// The child becomes a session leader with no controlling tty (greetd runs with
/// `vt = "none"`, and the compositor takes the display through DRM, not through
/// a tty), reads nothing (stdin is /dev/null) and writes its stdout/stderr to
/// a pipe drained by a logger child (`run_dm_logger`) into `DM_LOG`. It then
/// execs `/bin/sh /bin/greeter-real` with the minimal environment the launcher
/// itself completes.
///
/// WHY A PIPE, not the file itself: the log has to be bounded, and nothing in
/// the chain can bound it. Every COSMIC component's output reaches the file
/// through cosmic-session, whose launch-pad forwards each child line through
/// an unbounded channel. A child that spins printing an error therefore grows
/// cosmic-session's heap as well as the file whenever the file writes lag
/// behind (2026-09-25, virgl: ~110 MiB/min of user pages, guest OOM at
/// ~13.5 min). The logger reads the pipe in 64 KiB chunks and rotates the file
/// at `DM_LOG_CAP`. The memory side is `mem_guard`'s job.
unsafe fn spawn_display_manager() -> (i32, i32) {
    mkdir(b"/var\0".as_ptr(), 0o755);
    mkdir(b"/var/log\0".as_ptr(), 0o755);
    mkdir(b"/etc/leandros\0".as_ptr(), 0o755);

    // Log pipe. The read end goes to the logger, the write end becomes the
    // chain's stdout/stderr. init keeps neither end, or the logger could never
    // see EOF.
    let mut fds = [-1i32; 2];
    let have_pipe = syscall2(nr::PIPE2, fds.as_mut_ptr() as usize, O_CLOEXEC) == 0;
    let mut logger: i32 = 0;
    if have_pipe {
        let lp = fork();
        if lp == 0 {
            close(fds[1]);
            run_dm_logger(fds[0]);
        }
        if lp > 0 { logger = lp; } else { write_str("ERROR: fork failed for the greetd logger\n"); }
    }

    let pid = fork();
    if pid < 0 {
        write_str("ERROR: fork failed for the graphical login\n");
        if have_pipe { close(fds[0]); close(fds[1]); }
        return (pid, logger);
    }
    if pid > 0 {
        if have_pipe { close(fds[0]); close(fds[1]); }
        write_str("graphical login started (greetd, pid ");
        write_u32(pid as u32);
        write_str("), log at /var/log/greetd.log\n");
        write_pid_file(pid as u32);
        return (pid, logger);
    }
    // Child.
    setsid();
    let devnull = open(b"/dev/null\0".as_ptr(), O_RDONLY, 0);
    if devnull >= 0 {
        dup3(devnull, 0, 0);
        if devnull != 0 { close(devnull); }
    }
    if have_pipe && logger > 0 {
        // dup3 with no flags clears O_CLOEXEC on 1 and 2; the originals
        // close here (and on exec anyway).
        dup3(fds[1], 1, 0);
        dup3(fds[1], 2, 0);
        close(fds[0]);
        close(fds[1]);
    } else {
        // No pipe or no logger: the old direct file, unbounded but working.
        if have_pipe { close(fds[0]); close(fds[1]); }
        let log = open(DM_LOG.as_ptr(), O_WRONLY | O_CREAT | O_TRUNC | O_APPEND, 0o644);
        if log >= 0 {
            dup3(log, 1, 0);
            dup3(log, 2, 0);
            if log > 2 { close(log); }
        }
    }
    let argv: [*const u8; 3] = [DM_SHELL.as_ptr(), DM_LAUNCHER.as_ptr(), core::ptr::null()];
    let envp: [*const u8; 3] = [
        b"PATH=/usr/bin:/bin\0".as_ptr(),
        b"HOME=/root\0".as_ptr(),
        core::ptr::null(),
    ];
    execve(DM_SHELL.as_ptr(), argv.as_ptr(), envp.as_ptr());
    // Only reached if the exec failed; the supervisor sees the exit and
    // applies its ceiling.
    write_str("ERROR: execve /bin/sh /bin/greeter-real failed\n");
    exit(1);
}

/// The logger child: drain the chain's pipe into `DM_LOG`, rotating to
/// `DM_LOG_OLD` at `DM_LOG_CAP`, and exit on EOF, which comes when the last
/// process holding the chain's stdout/stderr is gone. Never returns.
unsafe fn run_dm_logger(rfd: i32) -> ! {
    static mut BUF: [u8; 65536] = [0; 65536];
    const AT_FDCWD: usize = -100isize as usize;
    // Leave the serial console alone: nothing here should print there except
    // a failure to open the log.
    let open_log = || open(DM_LOG.as_ptr(), O_WRONLY | O_CREAT | O_TRUNC | O_APPEND, 0o644);
    let mut log = open_log();
    if log < 0 {
        write_str("ERROR: greetd logger cannot open /var/log/greetd.log\n");
    }
    let mut size = 0usize;
    let mut rotations = 0u32;
    let buf = &mut *core::ptr::addr_of_mut!(BUF);
    loop {
        let n = read(rfd, buf.as_mut_ptr(), buf.len());
        if n == 0 { break; }
        if n < 0 {
            if leandros_libc::errno::get_errno() == 4 { continue; } // EINTR
            break;
        }
        let n = n as usize;
        if log >= 0 && size + n > DM_LOG_CAP {
            close(log);
            syscall4(nr::RENAMEAT, AT_FDCWD, DM_LOG.as_ptr() as usize,
                     AT_FDCWD, DM_LOG_OLD.as_ptr() as usize);
            log = open_log();
            size = 0;
            rotations = rotations.saturating_add(1);
            if log >= 0 {
                let mut line = [0u8; 96];
                let head = b"[init] greetd.log rotated (generation ";
                let mut p = head.len();
                line[..p].copy_from_slice(head);
                p += fmt_u32(&mut line[p..], rotations);
                let tail = b"; previous in greetd.log.1)\n";
                line[p..p + tail.len()].copy_from_slice(tail);
                p += tail.len();
                write(log, line.as_ptr(), p);
                size += p;
            }
        }
        if log < 0 { continue; } // keep draining so the writers never block
        let mut off = 0usize;
        while off < n {
            let w = write(log, buf.as_ptr().add(off), n - off);
            if w <= 0 { break; }
            off += w as usize;
        }
        size += n;
    }
    exit(0);
}

/// State of the memory-pressure guard between supervisor iterations.
struct MemGuard {
    last_check: u64,
    strikes: u32,
}

/// Kill the graphical login's tree when the guest is about to run out of
/// memory, instead of letting the kernel's allocator fail under everything.
///
/// systemd-oomd does the same at a session's cgroup. There is no per-process
/// RSS to pick a single victim by (`/proc/<pid>/status` VmRSS is a constant),
/// and the graphical tree is where every such runaway seen so far lived: a
/// greeter spinning on a dead compositor, or cosmic-session queueing the
/// lines of a panel spinning on a dead Wayland connection. SIGKILL goes to
/// greetd. The normal exit path then sweeps the orphans (`sweep_strays`) and
/// respawns the login under the usual backoff and ceiling.
unsafe fn mem_guard(g: &mut MemGuard, dm_pid: i32) {
    let now = monotonic_secs();
    if now < g.last_check + MEM_GUARD_PERIOD_SECS { return; }
    g.last_check = now;
    let (total, avail) = match meminfo_kib() { Some(v) => v, None => return };
    let floor = core::cmp::max(MEM_GUARD_MIN_KIB, total / MEM_GUARD_DIVISOR);
    if avail >= floor {
        g.strikes = 0;
        return;
    }
    g.strikes += 1;
    if g.strikes < MEM_GUARD_STRIKES { return; }
    g.strikes = 0;
    write_str("\n");
    write_str("################################################################\n");
    write_str("## MEMORY PRESSURE: MemAvailable ");
    write_u32((avail / 1024) as u32);
    write_str(" MiB < floor ");
    write_u32((floor / 1024) as u32);
    write_str(" MiB.\n");
    write_str("## Killing the graphical login (greetd pid ");
    write_u32(dm_pid as u32);
    write_str(") and its session;\n");
    write_str("## it restarts under the usual backoff. See /var/log/greetd.log.\n");
    write_str("################################################################\n");
    // The serial console is not always being read (a driver socket with no
    // client drops it), so leave the same fact in the log the chain wrote.
    let fd = open(DM_LOG.as_ptr(), O_WRONLY | O_APPEND, 0);
    if fd >= 0 {
        let mut line = [0u8; 128];
        let mut p = 0;
        let head = b"[init] MEMORY PRESSURE: MemAvailable ";
        line[..head.len()].copy_from_slice(head); p += head.len();
        p += fmt_u32(&mut line[p..], (avail / 1024) as u32);
        let mid = b" MiB < floor ";
        line[p..p + mid.len()].copy_from_slice(mid); p += mid.len();
        p += fmt_u32(&mut line[p..], (floor / 1024) as u32);
        let tail = b" MiB; killing the graphical login\n";
        line[p..p + tail.len()].copy_from_slice(tail); p += tail.len();
        write(fd, line.as_ptr(), p);
        close(fd);
    }
    syscall2(nr::KILL, dm_pid as usize, 9);
}

/// `(MemTotal, MemAvailable)` in KiB from /proc/meminfo.
unsafe fn meminfo_kib() -> Option<(u64, u64)> {
    let mut buf = [0u8; 512];
    let n = read_file(b"/proc/meminfo\0", &mut buf);
    if n == 0 { return None; }
    let mut total = None;
    let mut avail = None;
    for line in buf[..n].split(|&b| b == b'\n') {
        let key_end = match line.iter().position(|&b| b == b':') { Some(i) => i, None => continue };
        let val = line[key_end + 1..].split(|&b| b == b' ').find(|f| !f.is_empty());
        let v = match val.and_then(parse_u32) { Some(v) => v as u64, None => continue };
        match &line[..key_end] {
            b"MemTotal" => total = Some(v),
            b"MemAvailable" => avail = Some(v),
            _ => {}
        }
    }
    Some((total?, avail?))
}

/// Record the supervised greetd pid so a root shell can `kill` the chain
/// (`kill $(cat /run/greetd-init.pid)`); with /etc/leandros/text-login in
/// place first, the supervisor then leaves it down.
unsafe fn write_pid_file(pid: u32) {
    let fd = open(DM_PID_FILE.as_ptr(), O_WRONLY | O_CREAT | O_TRUNC, 0o644);
    if fd < 0 { return; }
    let mut buf = [0u8; 11];
    let mut n = pid;
    let mut i = buf.len() - 1;
    buf[i] = b'\n';
    if n == 0 { i -= 1; buf[i] = b'0'; }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    write(fd, buf.as_ptr().add(i), buf.len() - i);
    close(fd);
}

/// Runs in the forked child: become a session leader, claim the console as
/// the controlling tty, then exec login (falling back to the raw shell so a
/// broken image still boots).
unsafe fn run_session() {
    setsid();
    ioctl(0, TIOCSCTTY, 0);

    let path = b"/bin/login\0";
    let argv: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
    let envp: [*const u8; 1] = [core::ptr::null()];
    execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());

    write_str("ERROR: execve /bin/login failed, falling back to /bin/shell\n");
    let path = b"/bin/shell\0";
    let argv: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
    execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());

    write_str("ERROR: execve /bin/shell failed!\n");
}

/// Create `/run/user/<uid>` (0700, owned uid:gid) for every account in
/// /etc/passwd except root, whose `/run/user/0` the VFS seeds at boot.
///
/// This is the job pam_systemd/logind does per login on Linux. Here it has to
/// happen in init, as root, because `/run/user` is a 0755 root tmpfs mount and
/// the VFS enforces that: the `mkdir -p "$XDG_RUNTIME_DIR"` in /etc/profile and
/// start-cosmic-leandros runs as the session user and is EACCES there. Without
/// this a uid-1000 session has nowhere to bind its wayland-N and session-bus
/// sockets and never reaches a desktop; the greeter account (uid 990) needs
/// its own for the greeter phase's sockets (see /etc/profile).
///
/// tmpfs is volatile, so this runs on every boot. The parse is deliberately
/// tolerant: a malformed line is skipped, and a directory that already exists
/// is simply re-owned.
///
/// Also seeds `/run/cosmic-greeter` for the `cosmic-greeter` account while
/// it is here reading /etc/passwd for exactly this reason. Upstream ships it
/// via tmpfiles.d (`debian/cosmic-greeter.tmpfiles`):
///   d /run/cosmic-greeter 0755 cosmic-greeter cosmic-greeter -
/// cosmic-config's system-scope store (used by the a11y/settings-daemon
/// glue linked into the greeter binary) lands there; without it the greeter
/// logs a missing-directory complaint every boot.
unsafe fn seed_runtime_dirs() {
    let fd = open(b"/etc/passwd\0".as_ptr(), O_RDONLY, 0);
    if fd < 0 {
        write_str("no /etc/passwd; skipping /run/user seeding\n");
        return;
    }
    let mut buf = [0u8; 8192];
    let mut total = 0usize;
    loop {
        if total >= buf.len() { break; }
        let n = read(fd, buf.as_mut_ptr().add(total), buf.len() - total);
        if n <= 0 { break; }
        total += n as usize;
    }
    close(fd);

    for line in buf[..total].split(|&b| b == b'\n') {
        if line.is_empty() || line[0] == b'#' { continue; }
        let mut fields = line.split(|&b| b == b':');
        let name = fields.next();
        let _pw = fields.next();
        let uid = match fields.next().and_then(parse_u32) { Some(u) => u, None => continue };
        let gid = match fields.next().and_then(parse_u32) { Some(g) => g, None => continue };
        if uid == 0 { continue; }

        if name == Some(&b"cosmic-greeter"[..]) {
            // Mode and ownership mirror upstream's tmpfiles.d line exactly
            // (0755, cosmic-greeter:cosmic-greeter) — see this function's
            // doc comment. uid/gid come from /etc/passwd rather than a
            // hardcoded 990 so a re-staged account still gets this right.
            mkdir(b"/run/cosmic-greeter\0".as_ptr(), 0o755);
            if chown(b"/run/cosmic-greeter\0".as_ptr(), uid, gid) != 0 {
                write_str("WARNING: chown of /run/cosmic-greeter failed\n");
            }
        }

        // "/run/user/" + decimal uid + NUL.
        let mut path = [0u8; 32];
        let prefix = b"/run/user/";
        path[..prefix.len()].copy_from_slice(prefix);
        let mut digits = [0u8; 10];
        let mut n = uid;
        let mut i = digits.len();
        if n == 0 { i -= 1; digits[i] = b'0'; }
        while n > 0 { i -= 1; digits[i] = b'0' + (n % 10) as u8; n /= 10; }
        let dl = digits.len() - i;
        path[prefix.len()..prefix.len() + dl].copy_from_slice(&digits[i..]);
        // path is NUL-terminated by the zeroed buffer.

        mkdir(path.as_ptr(), 0o700);
        if chown(path.as_ptr(), uid, gid) != 0 {
            write_str("WARNING: chown of /run/user/");
            write_u32(uid);
            write_str(" failed\n");
        }
    }
}

fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() || s.len() > 10 { return None; }
    let mut v: u32 = 0;
    for &b in s {
        if !b.is_ascii_digit() { return None; }
        v = v.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(v)
}

/// Read `/etc/fstab` and mount every entry except the root ("/") one, which
/// the hardcoded bootstrap mount above already handled.
unsafe fn mount_from_fstab() {
    let fd = open(b"/etc/fstab\0".as_ptr(), O_RDONLY, 0);
    if fd < 0 {
        return;
    }
    let mut buf = [0u8; 4096];
    let n = read(fd, buf.as_mut_ptr(), buf.len());
    close(fd);
    if n <= 0 {
        return;
    }
    let content = core::str::from_utf8(&buf[..n as usize]).unwrap_or("");

    for line in content.lines() {
        let entry = match leandros_libc::fstab::parse_line(line) {
            Some(e) => e,
            None => continue,
        };
        if entry.mountpoint() == "/" {
            continue;
        }

        write_str("Mounting ");
        write_str(entry.device());
        write_str(" at ");
        write_str(entry.mountpoint());
        write_str(" (");
        write_str(entry.fstype());
        write_str(")...\n");

        let mut mp_buf = [0u8; 65];
        let mp = entry.mountpoint();
        mp_buf[..mp.len()].copy_from_slice(mp.as_bytes());
        mkdir(mp_buf.as_ptr(), 0o755);

        let mut dev_buf = [0u8; 65];
        let dev = entry.device();
        dev_buf[..dev.len()].copy_from_slice(dev.as_bytes());

        let mut fst_buf = [0u8; 65];
        let fst = entry.fstype();
        fst_buf[..fst.len()].copy_from_slice(fst.as_bytes());

        let r = mount(dev_buf.as_ptr(), mp_buf.as_ptr(), fst_buf.as_ptr(), 0, core::ptr::null());
        if r < 0 {
            write_str("  WARNING: mount failed\n");
        }
    }
}

unsafe fn copy_file(src: &[u8], dst: &[u8]) -> bool {
    let fd_in = open(src.as_ptr(), O_RDONLY, 0);
    if fd_in < 0 {
        return false;
    }

    let fd_out = open(dst.as_ptr(), O_WRONLY | O_CREAT | O_TRUNC, 0o755);
    if fd_out < 0 {
        close(fd_in);
        return false;
    }

    let mut buf = [0u8; 4096];
    loop {
        let n = read(fd_in, buf.as_mut_ptr(), buf.len());
        if n < 0 {
            close(fd_in);
            close(fd_out);
            return false;
        }
        if n == 0 {
            break;
        }
        let mut written = 0;
        while written < n {
            let w = write(fd_out, buf.as_ptr().add(written as usize), (n - written) as usize);
            if w <= 0 {
                close(fd_in);
                close(fd_out);
                return false;
            }
            written += w;
        }
    }

    close(fd_in);
    close(fd_out);
    true
}

unsafe fn write_str(s: &str) {
    write(STDOUT_FILENO, s.as_ptr(), s.len());
}

unsafe fn write_u32(mut n: u32) {
    let mut buf = [0u8; 10];
    if n == 0 {
        write(STDOUT_FILENO, b"0".as_ptr(), 1);
        return;
    }
    let mut i = 10usize;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    write(STDOUT_FILENO, buf.as_ptr().add(i), 10 - i);
}
