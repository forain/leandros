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
    fork, wait4, setsid, ioctl, usleep, exit, clock_gettime, timespec, unlink,
};
use leandros_libc::syscall::{nr, syscall2, syscall4};

const O_CLOEXEC: usize = 0x8_0000;

const TIOCSCTTY: usize = 0x540E;

// ── Text logins on VT 2..6 ───────────────────────────────────────────────────
//
// Each of /dev/tty2../dev/tty6 is a terminal of its own (servers/tty/src/vt.rs,
// "Text sessions"), reachable with Ctrl+Alt+F2..F6 from anything — the COSMIC
// session included. A login runs on each, the way agetty runs on tty2..tty6 on
// a Linux box (systemd's autovt@ starts them on demand; these are tiny, so they
// simply start at boot). Respawned when the session ends, like the serial one.

/// First and last VT that gets a text login.
const VT_LOGIN_FIRST: u8 = 2;
const VT_LOGIN_LAST: u8 = 6;
const VT_LOGINS: usize = (VT_LOGIN_LAST - VT_LOGIN_FIRST + 1) as usize;
/// A VT login that exits within this many seconds of starting, this many times
/// in a row, is not respawned again — a node that cannot be opened must not
/// turn into a fork storm.
const VT_LOGIN_QUICK_SECS: u64 = 2;
const VT_LOGIN_MAX_QUICK: u32 = 5;

/// Pid of the login (later the shell it execs) on each VT; it is also the
/// session id of that VT's session, which `sweep_strays`/`pick_victim` use to
/// leave text sessions alone. 0 = not running.
static mut VT_LOGIN_PID: [i32; VT_LOGINS] = [0; VT_LOGINS];
static mut VT_LOGIN_STARTED: [u64; VT_LOGINS] = [0; VT_LOGINS];
static mut VT_LOGIN_QUICK: [u32; VT_LOGINS] = [0; VT_LOGINS];

/// True when `sid` is the session of a text login on VT 2..6.
unsafe fn is_vt_session(sid: u32) -> bool {
    let pids = &*core::ptr::addr_of!(VT_LOGIN_PID);
    pids.iter().any(|&p| p > 0 && p as u32 == sid)
}

/// Fork a login on `/dev/ttyN`: new session, the VT as its controlling
/// terminal and stdio, then `/bin/login`. Returns the pid, or -1.
unsafe fn spawn_vt_login(n: u8) -> i32 {
    let pid = fork();
    if pid == 0 {
        setsid();
        let path: [u8; 10] = [b'/', b'd', b'e', b'v', b'/', b't', b't', b'y', b'0' + n, 0];
        const O_RDWR: i32 = 2;
        let fd = open(path.as_ptr(), O_RDWR, 0);
        if fd < 0 { exit(1); }
        ioctl(fd, TIOCSCTTY, 0);
        for t in 0..3 { if fd != t { dup3(fd, t, 0); } }
        if fd > 2 { close(fd); }
        // agetty's issue line: which terminal this is.
        let banner: [u8; 21] = [b'\r', b'\n', b'L', b'e', b'a', b'n', b'd', b'r', b'O', b'S',
                                b' ', b'(', b't', b't', b'y', b'0' + n, b')', b'\r', b'\n',
                                b'\r', b'\n'];
        write(1, banner.as_ptr(), banner.len());
        let lpath = b"/bin/login\0";
        let argv: [*const u8; 2] = [lpath.as_ptr(), core::ptr::null()];
        let envp: [*const u8; 2] = [b"TERM=linux\0".as_ptr(), core::ptr::null()];
        execve(lpath.as_ptr(), argv.as_ptr(), envp.as_ptr());
        exit(1);
    }
    pid
}

/// Start (or restart) the login on VT slot `i`, unless it has been failing.
unsafe fn start_vt_login(i: usize) {
    let quick = &mut *core::ptr::addr_of_mut!(VT_LOGIN_QUICK);
    if quick[i] >= VT_LOGIN_MAX_QUICK { return; }
    let pid = spawn_vt_login(VT_LOGIN_FIRST + i as u8);
    (*core::ptr::addr_of_mut!(VT_LOGIN_PID))[i] = if pid > 0 { pid } else { 0 };
    (*core::ptr::addr_of_mut!(VT_LOGIN_STARTED))[i] = monotonic_secs();
}

/// `pid` exited: if it was a VT login, respawn it and return true.
unsafe fn reap_vt_login(pid: i32) -> bool {
    let pids = &mut *core::ptr::addr_of_mut!(VT_LOGIN_PID);
    let i = match pids.iter().position(|&p| p == pid) { Some(i) => i, None => return false };
    pids[i] = 0;
    let quick = &mut *core::ptr::addr_of_mut!(VT_LOGIN_QUICK);
    let ran = monotonic_secs().saturating_sub((*core::ptr::addr_of!(VT_LOGIN_STARTED))[i]);
    if ran < VT_LOGIN_QUICK_SECS { quick[i] += 1; } else { quick[i] = 0; }
    if quick[i] >= VT_LOGIN_MAX_QUICK {
        write_str("text login on tty");
        write_u32((VT_LOGIN_FIRST as usize + i) as u32);
        write_str(" keeps failing; not respawning it\n");
        return true;
    }
    if quick[i] > 0 { usleep(500_000); }
    start_vt_login(i);
    true
}

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
/// floor for `MEM_GUARD_PERSIST_MS`, init kills the largest process on the
/// graphical side (see `mem_guard`), as systemd-oomd does. The floor is the
/// larger of a fixed minimum and a fraction of RAM.
const MEM_GUARD_MIN_KIB: u64 = 96 * 1024;
const MEM_GUARD_DIVISOR: u64 = 10;
/// Sampling period while memory is plentiful and not falling fast.
const MEM_GUARD_PERIOD_MS: u64 = 2000;
/// Sampling period once MemAvailable is near/below the floor or falling fast
/// enough to reach it within `MEM_GUARD_HORIZON_MS` — the supervisor loop's
/// own tick, so it costs no extra wakeups.
const MEM_GUARD_FAST_MS: u64 = 250;
const MEM_GUARD_HORIZON_MS: u64 = 2 * MEM_GUARD_PERIOD_MS + 1000;
/// How long MemAvailable must stay below the floor before a kill, unless it
/// is falling so fast that it would be gone within `MEM_GUARD_URGENT_MS`.
const MEM_GUARD_PERSIST_MS: u64 = 1000;
const MEM_GUARD_URGENT_MS: u64 = 2000;
/// After a kill, give the victim's teardown this long to return its memory
/// before judging again.
const MEM_GUARD_GRACE_MS: u64 = 2000;
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
    // /run is on the persistent root, so a pid file from an earlier boot
    // would name whatever process now has that pid (a `kill $(cat ...)` then
    // hits the serial login shell). It names a live greetd or nothing.
    unlink(DM_PID_FILE.as_ptr());
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
    for i in 0..VT_LOGINS { start_vt_login(i); }
    let mut guard = MemGuard { last_ms: 0, last_avail: 0, below_since: 0, fast: false,
                               grace_until: 0, victims: 0 };
    loop {
        let mut status = 0i32;
        // WNOHANG and a short sleep instead of a blocking wait4, so the
        // memory-pressure guard gets to run while nothing exits.
        const WNOHANG: i32 = 1;
        let pid = wait4(-1, &mut status, WNOHANG, core::ptr::null_mut());
        if pid == 0 {
            if dm_pid > 0 { mem_guard(&mut guard, dm_pid, login_pid, dm_logger); }
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
        if reap_vt_login(pid) {
            continue;
        }
        if pid == login_pid {
            write_str("session ended, restarting login\n");
            usleep(1_000_000);
            login_pid = spawn_login();
        } else if dm_pid > 0 && pid == dm_pid {
            dm_pid = 0;
            unlink(DM_PID_FILE.as_ptr());
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
unsafe fn monotonic_ms() -> u64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    if clock_gettime(1, &mut ts) != 0 { return 0; }
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

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
                    let protected = match login_sid { Some(s) => s == sid, None => false }
                        || is_vt_session(sid);
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
    /// Time (monotonic ms) and MemAvailable (KiB) of the previous sample.
    last_ms: u64,
    last_avail: u64,
    /// When MemAvailable first went below the floor (0: it is not).
    below_since: u64,
    /// Sample every `MEM_GUARD_FAST_MS` instead of `MEM_GUARD_PERIOD_MS`.
    fast: bool,
    /// No action before this time (a victim is still being torn down).
    grace_until: u64,
    /// Single processes killed in the current low-memory episode (reset once
    /// MemAvailable is back above the floor).
    victims: u32,
}

/// Single-process kills per low-memory episode before the guard gives up on
/// picking and kills the whole graphical login.
const MEM_GUARD_MAX_VICTIMS: u32 = 3;

/// Executables killed only when nothing else in the session is left to
/// pick: the compositor (every client dies with it) and greetd's own
/// processes (the session worker; killing it ends the whole login).
const MEM_GUARD_LAST: &[&[u8]] = &[b"cosmic-comp", b"greetd"];

/// Relieve memory pressure before the kernel's allocator fails under
/// everything, the way the kernel OOM killer and systemd-oomd do.
///
/// When MemAvailable stays below the floor for `MEM_GUARD_PERSIST_MS`,
/// the guard SIGKILLs the one process with the largest resident set
/// (`/proc/<pid>/statm`) among those that belong to the graphical side:
/// everything except init, the serial login's session, the log writer and
/// greetd itself, with the compositor and greetd's session worker taken
/// only if nothing else is left. It names the victim on the console and in
/// greetd.log. If memory is still low after `MEM_GUARD_MAX_VICTIMS` such
/// kills (or there is nothing to pick), it falls back to killing greetd,
/// whose exit path sweeps the rest of the tree and respawns the login under
/// the usual backoff.
///
/// Until 2026-09-26 the fallback was the only action: `/proc/<pid>/status`
/// reported a constant VmRSS, so there was no way to choose a victim.
///
/// Sampling is adaptive: every `MEM_GUARD_PERIOD_MS` while memory is
/// plentiful, every `MEM_GUARD_FAST_MS` once MemAvailable is within reach of
/// the floor at the rate it last fell. A fixed 2 s x 2 checks let a hog
/// allocating ~160 MiB/s exhaust the guest before the second check (x86_64
/// and aarch64 alike), which is what the fast mode is for.
unsafe fn mem_guard(g: &mut MemGuard, dm_pid: i32, login_pid: i32, logger_pid: i32) {
    let now = monotonic_ms();
    let period = if g.fast { MEM_GUARD_FAST_MS } else { MEM_GUARD_PERIOD_MS };
    if now < g.last_ms + period { return; }
    let (total, avail) = match meminfo_kib() { Some(v) => v, None => return };
    let dt = now - g.last_ms;
    // Rate MemAvailable fell since the last sample, KiB/s (0 if it rose).
    let falling = if g.last_ms != 0 && g.last_avail > avail && dt > 0 {
        (g.last_avail - avail) * 1000 / dt
    } else { 0 };
    g.last_ms = now;
    g.last_avail = avail;
    let floor = core::cmp::max(MEM_GUARD_MIN_KIB, total / MEM_GUARD_DIVISOR);
    if avail >= floor {
        g.below_since = 0;
        g.victims = 0;
        // Near the floor, or heading there before two slow samples could
        // confirm it: watch closely.
        g.fast = avail < floor + floor / 4
            || (falling > 0 && (avail - floor) * 1000 / falling < MEM_GUARD_HORIZON_MS);
        return;
    }
    g.fast = true;
    if g.below_since == 0 { g.below_since = now; }
    if now < g.grace_until { return; }
    let urgent = falling > 0 && avail * 1000 / falling < MEM_GUARD_URGENT_MS;
    if !urgent && now - g.below_since < MEM_GUARD_PERSIST_MS { return; }
    g.below_since = 0;
    g.grace_until = now + MEM_GUARD_GRACE_MS;

    let mut line = [0u8; 224];
    let mut p = 0;
    let put = |line: &mut [u8; 224], p: &mut usize, s: &[u8]| {
        let n = s.len().min(line.len() - *p);
        line[*p..*p + n].copy_from_slice(&s[..n]);
        *p += n;
    };
    put(&mut line, &mut p, b"[init] MEMORY PRESSURE: MemAvailable ");
    let mut num = [0u8; 10];
    let n = fmt_u32(&mut num, (avail / 1024) as u32); put(&mut line, &mut p, &num[..n]);
    put(&mut line, &mut p, b" MiB < floor ");
    let n = fmt_u32(&mut num, (floor / 1024) as u32); put(&mut line, &mut p, &num[..n]);
    put(&mut line, &mut p, b" MiB");
    if falling > 0 {
        put(&mut line, &mut p, b", falling ");
        let n = fmt_u32(&mut num, (falling / 1024) as u32); put(&mut line, &mut p, &num[..n]);
        put(&mut line, &mut p, b" MiB/s");
    }
    put(&mut line, &mut p, b"; ");

    let victim = if g.victims < MEM_GUARD_MAX_VICTIMS {
        pick_victim(dm_pid, login_pid, logger_pid)
    } else { None };
    match victim {
        Some(v) => {
            g.victims += 1;
            put(&mut line, &mut p, b"killed pid ");
            let n = fmt_u32(&mut num, v.pid); put(&mut line, &mut p, &num[..n]);
            put(&mut line, &mut p, b" (");
            put(&mut line, &mut p, &v.comm[..v.comm_len]);
            put(&mut line, &mut p, b"), RSS ");
            let n = fmt_u32(&mut num, (v.rss_kib / 1024) as u32); put(&mut line, &mut p, &num[..n]);
            put(&mut line, &mut p, b" MiB, the largest in the graphical session\n");
            syscall2(nr::KILL, v.pid as usize, 9);
        }
        None => {
            g.victims = 0;
            put(&mut line, &mut p, b"killing the graphical login (greetd pid ");
            let n = fmt_u32(&mut num, dm_pid as u32); put(&mut line, &mut p, &num[..n]);
            put(&mut line, &mut p, b") and its session\n");
            syscall2(nr::KILL, dm_pid as usize, 9);
        }
    }
    write_str("\n################################################################\n## ");
    write(STDOUT_FILENO, line.as_ptr().add(7), p - 7); // without the "[init] " prefix
    write_str("################################################################\n");
    // The serial console is not always being read (a driver socket with no
    // client drops it), so leave the same fact in the log the chain wrote.
    let fd = open(DM_LOG.as_ptr(), O_WRONLY | O_APPEND, 0);
    if fd >= 0 {
        write(fd, line.as_ptr(), p);
        close(fd);
    }
}

struct Victim { pid: u32, rss_kib: u64, comm: [u8; 16], comm_len: usize }

/// The largest-RSS process on the graphical side (see `mem_guard`), with
/// `MEM_GUARD_LAST` executables considered only when nothing else is.
unsafe fn pick_victim(dm_pid: i32, login_pid: i32, logger_pid: i32) -> Option<Victim> {
    let me = getpid() as u32;
    let login_sid = if login_pid > 0 { proc_stat(login_pid as u32).map(|(_, _, _, s)| s) } else { None };
    let last = proc_last_pid();
    let mut best: Option<Victim> = None;
    let mut best_last: Option<Victim> = None;
    let mut pid = 2u32;
    while pid <= last {
        let cur = pid;
        pid += 1;
        if cur == me || cur as i32 == login_pid || cur as i32 == logger_pid || cur as i32 == dm_pid {
            continue;
        }
        let (state, _ppid, _pgid, sid) = match proc_stat(cur) { Some(v) => v, None => continue };
        if state == b'Z' || login_sid == Some(sid) || is_vt_session(sid) { continue; }
        let rss_kib = match proc_rss_kib(cur) { Some(r) if r > 0 => r, _ => continue };
        let mut v = Victim { pid: cur, rss_kib, comm: [0; 16], comm_len: 0 };
        v.comm_len = proc_comm(cur, &mut v.comm);
        let is_last = MEM_GUARD_LAST.iter().any(|n| &v.comm[..v.comm_len] == *n);
        let slot = if is_last { &mut best_last } else { &mut best };
        if slot.as_ref().map_or(true, |b| rss_kib > b.rss_kib) { *slot = Some(v); }
    }
    best.or(best_last)
}

/// Resident set of `pid` in KiB (`/proc/<pid>/statm` field 2, pages).
unsafe fn proc_rss_kib(pid: u32) -> Option<u64> {
    let mut path = [0u8; 32];
    let mut p = 0;
    for &b in b"/proc/" { path[p] = b; p += 1; }
    p += fmt_u32(&mut path[p..], pid);
    for &b in b"/statm\0" { path[p] = b; p += 1; }
    let mut buf = [0u8; 128];
    let n = read_file(&path[..p], &mut buf);
    if n == 0 { return None; }
    let mut f = buf[..n].split(|&b| b == b' ' || b == b'\n').filter(|f| !f.is_empty());
    let _size = f.next()?;
    Some(parse_u32(f.next()?)? as u64 * 4)
}

/// The comm (executable name) from `/proc/<pid>/stat`, into `out`.
unsafe fn proc_comm(pid: u32, out: &mut [u8; 16]) -> usize {
    let mut path = [0u8; 32];
    let mut p = 0;
    for &b in b"/proc/" { path[p] = b; p += 1; }
    p += fmt_u32(&mut path[p..], pid);
    for &b in b"/stat\0" { path[p] = b; p += 1; }
    let mut buf = [0u8; 128];
    let n = read_file(&path[..p], &mut buf);
    let line = &buf[..n];
    let open = match line.iter().position(|&b| b == b'(') { Some(i) => i + 1, None => return 0 };
    let close = match line.iter().rposition(|&b| b == b')') { Some(i) => i, None => return 0 };
    if close < open { return 0; }
    let len = (close - open).min(out.len());
    out[..len].copy_from_slice(&line[open..open + len]);
    len
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
