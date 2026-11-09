//! TTY server — POSIX terminal state (termios), job control and POSIX timers.
//!
//! # What is here
//!
//! An in-kernel library called directly from `syscall.rs`. Three separate
//! things live under this roof, sharing only the `Termios` type:
//!
//! * **The console's termios**, one record per process (`CONSOLE_TERMIOS`).
//!   fd 0/1/2 are hardwired to the serial console by the kernel's read/write
//!   fast paths and never appear in any fd table, so `tcgetattr`/`tcsetattr`/
//!   `isatty` on them are answered from here. `console_intercept_byte` is the
//!   ISIG half of that line discipline, called from the UART drain.
//! * **Job control** (`jobctl_ioctl`) — the console's foreground process group.
//! * **POSIX timers** — see below.
//!
//! Pseudo-terminals are [`pty`], a self-contained pool with its own full
//! termios line discipline. They do NOT use a private fd range: a pty end is
//! an ordinary VFS fd (`VnodeKind::Pty`), so it inherits dup/fork/poll/
//! SCM_RIGHTS for free. The dormant `TTY_FD_BASE` scheme that a second fd
//! class would have needed was deleted rather than revived — nothing ever
//! routed `TTY_OPEN` to it.
//!
//! # POSIX timers
//!
//! `timer_create`/`timer_settime`/`timer_gettime`/`timer_delete` are
//! implemented here as a small per-process timer table.  Each timer is a
//! deadline (absolute `sched::monotonic_ns()`, so sub-tick values neither
//! floor to "disarmed" nor fire early). Expiry is serviced from the timer
//! interrupt ([`service_timers_irq`], driven by the one-shot deadline IRQ the
//! arm path programs via `sched::register_poll_deadline`), so a process
//! parked in pause()/sigsuspend()/sigtimedwait()/any interruptible sleep is
//! woken by its own SIGALRM; syscall return ([`check_timers`]) remains a
//! fallback. The signal is process-directed (`deliver_signal_process`), as on
//! Linux for SIGEV_SIGNAL, alarm() and ITIMER_REAL.
//!
//! # Message encoding
//!
//! Arguments packed as little-endian u64 words in Message.data[], as in VFS.

#![no_std]

extern crate alloc;

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use ipc::Message;
use spin::Mutex;

mod defkeymap;
pub mod jobctl;
pub mod keyboard;
pub mod pty;
pub mod vt;

// ── Protocol tag constants ────────────────────────────────────────────────────

pub const TTY_IOCTL:      u64 = 0x43;

// POSIX timer protocol
pub const TIMER_CREATE:   u64 = 0x50;
pub const TIMER_SETTIME:  u64 = 0x51;
pub const TIMER_GETTIME:  u64 = 0x52;
pub const TIMER_DELETE:   u64 = 0x53;
pub const TIMER_GETOVERRUN: u64 = 0x54;

// ── Constants ─────────────────────────────────────────────────────────────────

// Per-process console termios and timer tables. 64 -> 512 (lane procpool,
// 2026-09-27): sized to the process limit, since every process can own one. Scans stop at a high-water mark (`timer_hwm`/`console_hwm`): slots are
// claimed first-free, so lookups cost the session's peak process count, not 512
// — `check_timers` runs on every syscall return and usually misses.
// Both are heap vectors grown to the peak (lane fdmax merge, 2026-10-12):
// at MAX_PROCESSES = 2048 the static arrays were 870 KiB of kernel image.
const MAX_PROCS:  usize = sched::runqueue::MAX_PROCESSES;

static TIMER_HWM:   AtomicUsize = AtomicUsize::new(0);
static CONSOLE_HWM: AtomicUsize = AtomicUsize::new(0);
#[inline] fn timer_hwm() -> usize { TIMER_HWM.load(Ordering::Relaxed).min(MAX_PROCS) }
#[inline] fn console_hwm() -> usize { CONSOLE_HWM.load(Ordering::Relaxed).min(MAX_PROCS) }
/// Only called with the owning table's lock held, so the max needs no CAS.
#[inline] fn note_claimed(hwm: &AtomicUsize, pos: usize) {
    if pos + 1 > hwm.load(Ordering::Relaxed) { hwm.store(pos + 1, Ordering::Relaxed); }
}

/// One serial line per table, the first time a per-process tty table is full.
fn report_tty_table_full(bit: u32, what: &str) {
    static REPORTED: AtomicU32 = AtomicU32::new(0);
    if REPORTED.fetch_or(bit, Ordering::Relaxed) & bit != 0 { return; }
    extern "C" { fn arch_serial_putc(c: u8); }
    let msg: &[&[u8]] = &[b"\n[TTY] per-process ", what.as_bytes(),
                          b" table FULL -- tty::MAX_PROCS, not RAM\n"];
    for part in msg { for &b in *part { unsafe { arch_serial_putc(b); } } }
}
const MAX_TIMERS: usize = 8;   // per process POSIX timers (timer_create)
/// The interval timers (`setitimer`/`alarm`) have reserved slots of their own
/// past the `timer_create` ones: ITIMER_REAL used to be "slot 0 if free", so a
/// process whose first timer_create took slot 0 had that timer re-armed by
/// its next alarm(). User handles (`slot + 1`) never reach these.
const REAL_SLOT: usize = MAX_TIMERS;
const VIRT_SLOT: usize = MAX_TIMERS + 1;
const PROF_SLOT: usize = MAX_TIMERS + 2;
const NSLOTS:    usize = MAX_TIMERS + 3;

/// What a timer's `deadline`/`interval` are measured on.
/// `monotonic_ns` — CLOCK_MONOTONIC/BOOTTIME, and CLOCK_REALTIME (a relative
/// arm runs on the monotonic clock as on Linux; an absolute one is translated
/// and follows clock steps, see `PosixTimer::abs_rt`).
const CK_MONO:   u8 = 0;
/// The process's CPU time, user + system (CLOCK_PROCESS_CPUTIME_ID, ITIMER_PROF).
const CK_PROF:   u8 = 1;
/// The process's user CPU time (ITIMER_VIRTUAL).
const CK_VIRT:   u8 = 2;
/// One thread's CPU time (CLOCK_THREAD_CPUTIME_ID: the creating thread's).
const CK_THREAD: u8 = 3;

/// `timer_settime`'s absolute-deadline flag.
const TIMER_ABSTIME: u64 = 1;

// ── termios structure (matches Linux struct termios) ──────────────────────────
// 36 bytes: c_iflag(4)+c_oflag(4)+c_cflag(4)+c_lflag(4)+c_line(1)+[3 pad]+c_cc[19]+[1 pad]
// For simplicity we store as 60 bytes (termios2 / larger buffer).

#[derive(Clone, Copy)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line:  u8,
    c_cc:    [u8; 19],
}

impl Termios {
    /// An all-zero `Termios`, for `static` initialisers only.
    ///
    /// Not a usable terminal state (no `ICANON`, no `ECHO`, no `c_cc`) — its
    /// entire job is to keep the array it initialises in `.bss`. A `const`
    /// initialiser with any non-zero byte forces the whole enclosing static
    /// into `.data`, and for the PTY pool that means every ring buffer as
    /// well: ~200 KiB of image. See TODO item 15 for what an inflated image
    /// cost once. Callers install real settings straight after.
    pub(crate) const fn zeroed() -> Self {
        Self { c_iflag: 0, c_oflag: 0, c_cflag: 0, c_lflag: 0, c_line: 0, c_cc: [0u8; 19] }
    }

    /// Return a sensible default for a serial console.
    const fn default_console() -> Self {
        let mut cc = [0u8; 19];
        cc[0]  = 0x03; // VINTR  = Ctrl-C
        cc[1]  = 0x1C; // VQUIT  = Ctrl-\
        cc[10] = 0x1A; // VSUSP  = Ctrl-Z (Linux index; cc[9] kept for legacy)
        cc[2]  = 0x7F; // VERASE = DEL
        cc[3]  = 0x15; // VKILL  = Ctrl-U
        cc[4]  = 4;    // VEOF   = Ctrl-D (min bytes for raw read)
        cc[7]  = 0;    // VSTART
        cc[8]  = 0;    // VSTOP
        cc[9]  = 0x1A; // VSUSP  = Ctrl-Z
        cc[11] = 0x11; // VREPRINT = Ctrl-Q
        cc[12] = 0x12; // VDISCARD = Ctrl-R
        cc[13] = 0x17; // VWERASE  = Ctrl-W
        cc[14] = 0x16; // VLNEXT   = Ctrl-V
        Self {
            c_iflag: 0x0500, // ICRNL | IXON
            c_oflag: 0x0005, // OPOST | ONLCR
            c_cflag: 0x04BF, // CS8 | CREAD | HUPCL | CLOCAL
            c_lflag: 0x8A3B, // ISIG|ICANON|ECHO|ECHOE|ECHOK|IEXTEN|ECHOCTL|ECHOKE
            c_line:  0,
            c_cc:    cc,
        }
    }
}

// ── Hardwired console fds (0/1/2) ─────────────────────────────────────────────
// The kernel's sys_read/sys_write fast paths (and VFS's alloc_fd, which
// refuses to ever hand out 0-2) route stdin/stdout/stderr straight to the
// serial console without going through TTY_OPEN, so they never get a slot in
// TTY_TABLES. But isatty()/tcgetattr()/tcsetattr() on those fds still need to
// work — that's exactly what terminal apps (e.g. crossterm, used by
// `bottom`) probe before entering raw mode — so give them their own small
// per-pid termios record, keyed the same way as TTY_TABLES/TIMER_TABLES.
#[derive(Clone, Copy)]
struct ConsoleTermios {
    pid:     u32,
    in_use:  bool,
    termios: Termios,
}

impl ConsoleTermios {
    const fn empty() -> Self {
        Self { pid: 0, in_use: false, termios: Termios::default_console() }
    }
}

/// Grown one record at a time (up to `MAX_PROCS`), so it costs the peak
/// process count, not the process limit.
static CONSOLE_TERMIOS: Mutex<alloc::vec::Vec<ConsoleTermios>> = Mutex::new(alloc::vec::Vec::new());

/// Foreground process group of the console tty (0 = none set yet).
/// Maintained by TIOCSPGRP/TIOCSCTTY; consulted by the input-drain
/// intercept (`console_intercept_byte`) and TIOCGPGRP.
static CONSOLE_FG_PGID: Mutex<u32> = Mutex::new(0);

/// Session that holds the console as its controlling terminal (0 = none).
/// Set by `TIOCSCTTY` — `init`'s session child does that before exec'ing
/// `login`, so the serial login shell and its jobs are in it. Job control
/// (SIGTTIN/SIGTTOU, `tcsetpgrp`) applies only to processes of this session:
/// anything else that has the console on its stdio — the greeter chain,
/// which `setsid`s without claiming a terminal, or `init` itself — reads and
/// writes it as a plain device, exactly as on Linux.
static CONSOLE_SID: Mutex<u32> = Mutex::new(0);

/// The console's current foreground process group (0 = unset).
pub fn console_fg_pgid() -> u32 { *CONSOLE_FG_PGID.lock() }

/// Does session `sid` hold the console as its controlling terminal?
pub fn console_is_ctty_of(sid: u32) -> bool { sid != 0 && *CONSOLE_SID.lock() == sid }

/// Session `sid`'s controlling terminal as Linux numbers it, for
/// `/proc/<pid>/stat`: `(major, minor, foreground pgrp)`. A pty slave is
/// `/dev/pts/N` (136 + N/256, N%256), a VT session `/dev/ttyN` (4, N), the
/// console `/dev/console` (5, 1). None: the session has no terminal.
pub fn ctty_dev_of_sid(sid: u32) -> Option<(u32, u32, u32)> {
    if sid == 0 { return None; }
    if let Some((n, vt, pgrp)) = pty::ctty_of_sid(sid) {
        return Some(if vt != 0 { (4, vt as u32, pgrp) }
                    else { (136 + n as u32 / 256, n as u32 % 256, pgrp) });
    }
    if *CONSOLE_SID.lock() == sid {
        return Some((5, 1, *CONSOLE_FG_PGID.lock()));
    }
    None
}

/// `TOSTOP` of the console, read from the foreground leader's termios record
/// (the same record `console_intercept_byte` consults; the console default
/// has it clear).
fn console_tostop(fg: u32) -> bool {
    const TOSTOP: u32 = 0x0100;
    let tbl = CONSOLE_TERMIOS.lock();
    tbl[..console_hwm()].iter().find(|c| c.in_use && c.pid == fg)
        .map(|c| c.termios.c_lflag & TOSTOP != 0)
        .unwrap_or(false)
}

/// Job-control gate for a console `read` by the calling process: 0 to
/// proceed, `-EIO` or `-EINTR`. A background reader of its controlling
/// console is stopped with SIGTTIN *inside this call* and re-checked once
/// continued (see [`jobctl`]).
pub fn console_read_check() -> isize {
    loop {
        let (sid, fg) = (*CONSOLE_SID.lock(), *CONSOLE_FG_PGID.lock());
        match jobctl::check(sid, fg, jobctl::SIGTTIN) {
            jobctl::Verdict::Retry => continue,
            v => return jobctl::errno(v),
        }
    }
}

/// Job-control gate for a console `write`: SIGTTOU to a background writer,
/// only when the console has `TOSTOP`.
pub fn console_write_check() -> isize {
    loop {
        let (sid, fg) = (*CONSOLE_SID.lock(), *CONSOLE_FG_PGID.lock());
        if sid == 0 || fg == 0 { return 0; }
        match jobctl::check_write(sid, fg, console_tostop(fg)) {
            jobctl::Verdict::Retry => continue,
            v => return jobctl::errno(v),
        }
    }
}

/// Line-discipline ISIG intercept, called from the per-tick UART drain for
/// every incoming console byte BEFORE it is queued as input. Returns true
/// when the byte was consumed as a signal (^C/^\/^Z → SIGINT/SIGQUIT/SIGTSTP
/// to the foreground process group).
///
/// The termios that governs is the foreground pgroup leader's console
/// record (a job's leader pid == its pgid in the setpgid(0,0) convention);
/// processes that never touched termios get the console default, which has
/// ISIG set — so ^C kills a plain busy child, while an interactive shell
/// that put ITS record in raw mode (ISIG off) while ITSELF foreground keeps
/// receiving ^C as an ordinary byte for its line editor.
pub fn console_intercept_byte(b: u8) -> bool {
    // Ctrl-T on the serial line dumps the run queue (`sched::dump_tasks`), the
    // one way to ask a wedged userspace where its threads are parked. Serial
    // only — the framebuffer console never comes through here — and taken
    // before the ISIG intercept so it works with no foreground process group.
    if b == 0x14 {
        sched::dump_tasks();
        return true;
    }
    let pgid = *CONSOLE_FG_PGID.lock();
    if pgid == 0 { return false; }
    let (lflag, cc) = {
        let tbl = CONSOLE_TERMIOS.lock();
        match tbl[..console_hwm()].iter().find(|c| c.in_use && c.pid == pgid) {
            Some(c) => (c.termios.c_lflag, c.termios.c_cc),
            None => {
                let d = Termios::default_console();
                (d.c_lflag, d.c_cc)
            }
        }
    };
    const ISIG: u32 = 0x1;
    if lflag & ISIG == 0 { return false; }
    let sig = if cc[0] != 0 && b == cc[0] {
        2  // VINTR → SIGINT
    } else if cc[1] != 0 && b == cc[1] {
        3  // VQUIT → SIGQUIT
    } else if cc[10] != 0 && b == cc[10] {
        20 // VSUSP → SIGTSTP
    } else {
        return false;
    };
    // SI_KERNEL, as in pty.rs: the terminal generated this, not a process.
    let _ = sched::kill_pgrp(pgid, sig, sched::SigInfo::KERNEL);
    true
}

/// Job-control ioctls shared by the console and slot paths. Returns None
/// for commands that belong to `termios_ioctl`.
fn jobctl_ioctl(cmd: usize, arg_ptr: usize) -> Option<Message> {
    const TIOCSCTTY: usize = 0x540E;
    const TIOCGPGRP: usize = 0x540F;
    const TIOCSPGRP: usize = 0x5410;
    const TIOCGSID:  usize = 0x5429;
    match cmd {
        TIOCGPGRP => {
            if arg_ptr == 0 { return Some(err_reply(-14)); }
            let fg = *CONSOLE_FG_PGID.lock();
            // Never report "no foreground pgrp": fall back to the caller's
            // own pgid so tcgetpgrp always looks sane during bring-up.
            let fg = if fg == 0 { sched::current_pgid() } else { fg };
            unsafe { core::ptr::write(arg_ptr as *mut u32, sched::pidns::out(fg)); }
            Some(ok_reply())
        }
        TIOCSPGRP => {
            if arg_ptr == 0 { return Some(err_reply(-14)); }
            let pgid = unsafe { core::ptr::read(arg_ptr as *const u32) };
            // The pgid is in the caller's pid-namespace view.
            let pgid = match sched::pidns::arg(pgid) { Ok(k) => k, Err(e) => return Some(err_reply(e as i32)) };
            // tcsetpgrp(3), Linux `tiocspgrp()`: the console must be the
            // caller's controlling terminal (ENOTTY), the new foreground
            // group must be a group of that session (EPERM), and a caller
            // that is itself in the background takes SIGTTOU first — unless
            // it ignores it, which is how every shell (brush included) does
            // its own hand-off. Before this the console accepted any pgid
            // from anyone, and a job could steal the terminal from under
            // the shell that spawned it.
            let (my_sid, _) = sched::current_sid_pgid();
            let sid = *CONSOLE_SID.lock();
            if sid == 0 || my_sid != sid { return Some(err_reply(-25)); } // ENOTTY
            if !sched::pgrp_in_session(pgid, sid) { return Some(err_reply(-1)); } // EPERM
            loop {
                let fg = *CONSOLE_FG_PGID.lock();
                match jobctl::check(sid, fg, jobctl::SIGTTOU) {
                    jobctl::Verdict::Retry => continue,
                    jobctl::Verdict::Proceed => break,
                    v => return Some(err_reply(jobctl::errno(v) as i32)),
                }
            }
            *CONSOLE_FG_PGID.lock() = pgid;
            Some(ok_reply())
        }
        TIOCSCTTY => {
            // Acquiring the controlling terminal also makes the caller's
            // process group foreground (matches how shells use it), and
            // records the caller's session as the one the console controls.
            let (sid, pgid) = sched::current_sid_pgid();
            *CONSOLE_SID.lock() = sid;
            *CONSOLE_FG_PGID.lock() = pgid;
            Some(ok_reply())
        }
        TIOCGSID => {
            if arg_ptr == 0 { return Some(err_reply(-14)); }
            unsafe { core::ptr::write(arg_ptr as *mut u32, sched::pidns::out(sched::current_sid())); }
            Some(ok_reply())
        }
        _ => None,
    }
}

fn get_or_create_console<'a>(pid: u32, tbl: &'a mut alloc::vec::Vec<ConsoleTermios>) -> Option<&'a mut ConsoleTermios> {
    if let Some(pos) = tbl[..console_hwm()].iter().position(|t| t.in_use && t.pid == pid) {
        return Some(&mut tbl[pos]);
    }
    let free = tbl.iter().position(|t| !t.in_use).or_else(|| {
        if tbl.len() >= MAX_PROCS || tbl.try_reserve(1).is_err() { return None; }
        tbl.push(ConsoleTermios::empty());
        Some(tbl.len() - 1)
    });
    let Some(pos) = free else {
        report_tty_table_full(1, "console-termios (ENOTTY)");
        return None;
    };
    note_claimed(&CONSOLE_HWM, pos);
    tbl[pos] = ConsoleTermios { pid, in_use: true, termios: Termios::default_console() };
    Some(&mut tbl[pos])
}

// ── POSIX timer table ─────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct PosixTimer {
    in_use:      bool,
    signo:       u32,
    interval:    u64, // repeat interval in ns (0 = one-shot)
    deadline:    u64, // absolute monotonic_ns deadline (0 = disarmed)
    /// Expirations folded into the pending signal (it was still pending,
    /// or periods were missed) since that signal was last dequeued.
    overrun:     u32,
    /// The count `overrun` held when the timer's signal was last dequeued
    /// (`timer_dequeued`): what that signal's `si_overrun` carried and what
    /// `timer_getoverrun` reports until the next one, as on Linux.
    overrun_last: u32,
    owner_pid:   u32,
    /// `sigevent.sigev_value` this timer was armed with (`timer_create(2)`),
    /// carried into the delivered signal's `si_value` (SIGEV_SIGNAL) — 0 for
    /// the reserved slot-0 real timer (`alarm`/`setitimer`), which has none.
    sigev_value: u64,
    /// SIGEV_THREAD_ID's target: 0 = process-directed (SIGEV_SIGNAL, or an
    /// interval timer), non-zero = deliver straight to this thread id
    /// instead of picking an unmasked thread in the process (sigevent(7)).
    target_tid:  u32,
    /// `CK_*`: the clock `deadline` and `interval` are read on.
    clock:       u8,
    /// Created on a wall clock (CLOCK_REALTIME and its alarm/TAI variants):
    /// TIMER_ABSTIME values are wall-clock instants.
    realtime:    bool,
    /// Armed with TIMER_ABSTIME on a wall clock: `deadline` is that instant
    /// translated onto `monotonic_ns`, and a clock step moves it
    /// (`clock_was_set`), as Linux re-programs absolute CLOCK_REALTIME timers.
    abs_rt:      bool,
    /// CK_THREAD: the thread whose CPU time this timer measures.
    cpu_tid:     u32,
}

impl PosixTimer {
    const fn new() -> Self {
        Self { in_use: false, signo: 0, interval: 0, deadline: 0, overrun: 0, overrun_last: 0, owner_pid: 0,
               sigev_value: 0, target_tid: 0, clock: CK_MONO, realtime: false, abs_rt: false,
               cpu_tid: 0 }
    }
}

/// The payload a timer's expiry carries: SI_TIMER with the handle, overrun
/// and `sigev_value` for a `timer_create` timer; SI_KERNEL for an interval
/// timer (Linux sends SIGALRM/SIGVTALRM/SIGPROF with SEND_SIG_PRIV).
fn timer_info(idx: usize, t: &PosixTimer) -> sched::SigInfo {
    if idx >= MAX_TIMERS { return sched::SigInfo::KERNEL; }
    sched::SigInfo::timer((idx as i32) + 1, t.overrun as i32, t.sigev_value)
}

#[derive(Clone, Copy)]
struct ProcTimerTable {
    pid:    u32,
    timers: [PosixTimer; NSLOTS],
    in_use: bool,
}

impl ProcTimerTable {
    const fn empty() -> Self {
        Self { pid: 0, timers: [const { PosixTimer::new() }; NSLOTS], in_use: false }
    }

    /// A free `timer_create` slot (never one of the reserved itimer slots).
    fn alloc(&mut self) -> Option<usize> {
        self.timers[..MAX_TIMERS].iter().position(|t| !t.in_use)
    }

    /// Whether any CPU-clock timer is armed (the return-to-user check only
    /// runs for groups that have one, see `sched::cpu_timers_note`).
    fn cpu_armed(&self) -> bool {
        self.timers.iter().any(|t| t.in_use && t.clock != CK_MONO && t.deadline != 0)
    }
}

/// Grown one table at a time (up to `MAX_PROCS`): 392 bytes each, so a static
/// array sized to the 2048-process limit was 784 KiB of .bss.
static TIMER_TABLES: Mutex<alloc::vec::Vec<ProcTimerTable>> = Mutex::new(alloc::vec::Vec::new());

/// Find `pid`'s timer table, allocating a fresh one if this is its first timer.
/// Returns `None` only if every one of `MAX_PROCS` process slots is already
/// in use by a different pid.
fn get_or_create_timer_table<'a>(pid: u32, tbls: &'a mut alloc::vec::Vec<ProcTimerTable>) -> Option<&'a mut ProcTimerTable> {
    if let Some(pos) = tbls[..timer_hwm()].iter().position(|t| t.in_use && t.pid == pid) {
        return Some(&mut tbls[pos]);
    }
    let free = tbls.iter().position(|t| !t.in_use).or_else(|| {
        if tbls.len() >= MAX_PROCS || tbls.try_reserve(1).is_err() { return None; }
        tbls.push(ProcTimerTable::empty());
        Some(tbls.len() - 1)
    });
    let Some(pos) = free else {
        report_tty_table_full(2, "timer (ENOMEM/EAGAIN)");
        return None;
    };
    note_claimed(&TIMER_HWM, pos);
    tbls[pos] = ProcTimerTable::empty();
    tbls[pos].in_use = true;
    tbls[pos].pid    = pid;
    Some(&mut tbls[pos])
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn arg(msg: &Message, n: usize) -> u64 {
    let off = n * 8;
    u64::from_le_bytes(msg.data[off..off + 8].try_into().unwrap_or([0u8; 8]))
}

/// Undo the `slot + 1` encoding `handle_timer_create` hands out as a
/// `timer_t` (see its doc comment). A raw value of 0 was never issued.
fn decode_timerid(raw: u64) -> Option<usize> {
    (raw as usize).checked_sub(1)
}

fn make_reply(v: i64) -> Message {
    let mut m = Message::empty();
    m.data[0..8].copy_from_slice(&(v as u64).to_le_bytes());
    m
}

fn ok_reply()        -> Message { make_reply(0) }
fn err_reply(e: i32) -> Message { make_reply(e as i64) }
fn val_reply(v: u64) -> Message { make_reply(v as i64) }

// ── Public dispatch ───────────────────────────────────────────────────────────

pub fn handle(msg: &Message, caller_pid: u32) -> Message {
    // Timer/console tables are per-process (keyed by TGID); any syscall can arrive
    // on a non-leader thread, so canonicalize at this single IPC choke point
    // (mirrors vfs::handle / net_server::handle).
    let caller_pid = sched::tgid_of(caller_pid);
    match msg.tag {
        TTY_IOCTL   => handle_ioctl(caller_pid, arg(msg,0) as usize,
                                    arg(msg,1) as usize, arg(msg,2) as usize),
        TIMER_CREATE  => handle_timer_create(caller_pid, arg(msg,0) as u32,
                                             arg(msg,1) as usize, arg(msg,2), arg(msg,3) as u32,
                                             arg(msg,4) as u32, arg(msg,5) as u32),
        // `timer_t` handles are `slot + 1` (see handle_timer_create) so a
        // valid handle never numerically equals NULL; undo that here at the
        // single IPC boundary that decodes a caller-supplied timer_t.
        TIMER_SETTIME => match decode_timerid(arg(msg,0)) {
            Some(id) => handle_timer_settime(caller_pid, id, arg(msg,1) as usize, arg(msg,2) as usize,
                                             arg(msg,3)),
            None => err_reply(-22),
        },
        TIMER_GETTIME => match decode_timerid(arg(msg,0)) {
            Some(id) => handle_timer_gettime(caller_pid, id, arg(msg,1) as usize),
            None => err_reply(-22),
        },
        TIMER_GETOVERRUN => match decode_timerid(arg(msg,0)) {
            Some(id) => handle_timer_getoverrun(caller_pid, id),
            None => err_reply(-22),
        },
        TIMER_DELETE => match decode_timerid(arg(msg,0)) {
            Some(id) => handle_timer_delete(caller_pid, id),
            None => err_reply(-22),
        },
        _ => err_reply(-38),
    }
}

/// Check and fire expired POSIX timers for `pid`.  Called on syscall return.
pub fn check_timers(pid: u32) {
    let pid = sched::tgid_of(pid); // TIMER_TABLES is per-process; called on every
    // syscall return under the running thread's raw tid — a worker-thread-armed
    // timer would otherwise be checked by nobody.
    let now = sched::monotonic_ns();
    let mut tbls = TIMER_TABLES.lock();
    let tbl = match tbls[..timer_hwm()].iter_mut().find(|t| t.in_use && t.pid == pid) {
        Some(t) => t, None => return,
    };
    for (idx, timer) in tbl.timers.iter_mut().enumerate() {
        if !timer.in_use || timer.deadline == 0 || timer.clock != CK_MONO { continue; }
        if now >= timer.deadline {
            // SI_TIMER — the payload a handler uses to tell a POSIX timer
            // expiry apart from someone kill()ing it the same signal. `idx+1`
            // matches the `timer_t` handle `handle_timer_create` handed out
            // (see `decode_timerid`); `sigev_value` is whatever `timer_create`
            // was armed with. Interval timers send SI_KERNEL (`timer_info`).
            if timer.signo != 0 {
                let info = timer_info(idx, timer);
                if timer.target_tid != 0 {
                    // SIGEV_THREAD_ID: deliver straight to the named thread,
                    // not the process (sigevent(7)) — a dead target is
                    // dropped silently, as Linux does once the thread is gone.
                    let _ = sched::deliver_signal(timer.target_tid, timer.signo, info);
                } else {
                    match sched::try_deliver_signal_process(timer.owner_pid, timer.signo, info) {
                        Some(1) => timer.overrun = timer.overrun.saturating_add(1),
                        Some(_) => {}
                        None if timer.signo == 18 => {
                            sched::deliver_signal_process(timer.owner_pid, 18, info);
                        }
                        None => continue, // contended: the deadline IRQ retries
                    }
                }
            }
            if timer.interval > 0 {
                // A process descheduled for a while can miss more than one
                // period; catch the deadline up to `now` in one step and
                // remember how many extra expirations were folded in so
                // timer_getoverrun() can report them (POSIX semantics).
                let missed = (now - timer.deadline) / timer.interval;
                timer.deadline += timer.interval * (missed + 1);
                timer.overrun = timer.overrun.saturating_add(missed as u32);
            } else {
                timer.deadline = 0; // disarm one-shot
            }
        }
    }
}

/// Lock-free hint: the earliest armed POSIX-timer/itimer deadline
/// (`u64::MAX` = none). Lowered by every arm (`fetch_min`), recomputed exactly
/// by [`service_timers_irq`]; a stale-low value only costs one extra pass.
static NEXT_TIMER_DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Earliest pending timer deadline, for the deadline IRQ's re-arm.
pub fn earliest_timer_deadline() -> u64 { NEXT_TIMER_DEADLINE.load(Ordering::Relaxed) }

/// Fire every POSIX timer / itimer due at `now`, in IRQ context (try-lock
/// only). Posting the signal also wakes the target, so a thread blocked in
/// pause()/sigsuspend()/sigtimedwait()/nanosleep()/poll() sees it now rather
/// than at its next syscall return. Returns false if a lock was contended and
/// a due timer was left armed for the caller's retry.
pub fn service_timers_irq(now: u64) -> bool {
    if NEXT_TIMER_DEADLINE.load(Ordering::Relaxed) > now { return true; }
    let mut tbls = match TIMER_TABLES.try_lock() { Some(t) => t, None => return false };
    let mut done = true;
    let mut next = u64::MAX;
    for tbl in tbls[..timer_hwm()].iter_mut().filter(|t| t.in_use) {
        for (idx, timer) in tbl.timers.iter_mut().enumerate() {
            // CPU-clock timers are not on the monotonic clock: the
            // return-to-user check (`service_cpu_timers`) runs them.
            if !timer.in_use || timer.deadline == 0 || timer.clock != CK_MONO { continue; }
            // SIGCONT's parent notification needs task context: leave it to
            // the syscall-return path, and keep it out of the hint so it
            // cannot become a retry storm.
            if timer.signo == 18 { continue; }
            if now >= timer.deadline {
                if timer.signo != 0 {
                    let info = timer_info(idx, timer);
                    let result = if timer.target_tid != 0 {
                        // SIGEV_THREAD_ID, IRQ-safe thread-targeted delivery
                        // (mirrors try_deliver_signal_process's try-lock below).
                        sched::try_deliver_signal(timer.target_tid, timer.signo, info)
                    } else {
                        sched::try_deliver_signal_process(timer.owner_pid, timer.signo, info)
                    };
                    match result {
                        // Coalesced into a still-pending instance: POSIX overrun.
                        Some(1) => timer.overrun = timer.overrun.saturating_add(1),
                        Some(_) => {}
                        None => {
                            done = false; // RUN_QUEUE contended: retry
                            next = next.min(timer.deadline);
                            continue;
                        }
                    }
                }
                if timer.interval > 0 {
                    let missed = (now - timer.deadline) / timer.interval;
                    timer.deadline += timer.interval * (missed + 1);
                    timer.overrun = timer.overrun.saturating_add(missed as u32);
                } else {
                    timer.deadline = 0;
                    continue;
                }
            }
            next = next.min(timer.deadline);
        }
    }
    NEXT_TIMER_DEADLINE.store(next, Ordering::Relaxed); // exact, under TIMER_TABLES
    done
}

/// Make sure `pid`'s reserved interval-timer slot `slot` exists (`alarm`,
/// `setitimer`): one per-process timer per kind that repeated calls re-arm,
/// never a fresh slot per call.
fn ensure_itimer(pid: u32, slot: usize) {
    let (signo, clock) = match slot {
        REAL_SLOT => (14, CK_MONO),   // SIGALRM
        VIRT_SLOT => (26, CK_VIRT),   // SIGVTALRM
        _         => (27, CK_PROF),   // SIGPROF
    };
    let mut tbls = TIMER_TABLES.lock();
    let tbl = match get_or_create_timer_table(pid, &mut *tbls) {
        Some(t) => t, None => return,
    };
    if !tbl.timers[slot].in_use {
        tbl.timers[slot] = PosixTimer { in_use: true, signo, owner_pid: pid, clock,
                                        ..PosixTimer::new() };
    }
}

/// Drop all terminal and timer state for a process on exit.
pub fn close_all(pid: u32) {
    let mut timers = TIMER_TABLES.lock();
    if let Some(t) = timers[..timer_hwm()].iter_mut().find(|t| t.in_use && t.pid == pid) {
        *t = ProcTimerTable::empty();
        sched::cpu_timers_note(pid, false);
    }
    let mut console = CONSOLE_TERMIOS.lock();
    if let Some(c) = console[..console_hwm()].iter_mut().find(|c| c.in_use && c.pid == pid) {
        *c = ConsoleTermios::empty();
    }
}

// ── Terminal ioctl handler ────────────────────────────────────────────────────

fn handle_ioctl(pid: u32, fd: usize, cmd: usize, arg_ptr: usize) -> Message {
    // Termios records are per-process: thread siblings must see the same
    // console state (same tgid canonicalization as the VFS/net servers).
    let pid = sched::tgid_of(pid);
    // Nothing above fd 2 reaches here as a terminal: pty ends are dispatched
    // by `sys_ioctl` straight to `pty::ioctl`, console proxies were folded to
    // fd 0 there, and everything else genuinely is not a terminal — including
    // for the job-control commands, which used to be answered for any fd.
    if fd > 2 { return err_reply(-25); } // ENOTTY
    if let Some(r) = jobctl_ioctl(cmd, arg_ptr) { return r; }
    // stdin/stdout/stderr have no fd-table entry at all — see ConsoleTermios.
    let mut console = CONSOLE_TERMIOS.lock();
    let c = match get_or_create_console(pid, &mut *console) {
        Some(c) => c, None => return err_reply(-25),
    };
    termios_ioctl(cmd, arg_ptr, &mut c.termios)
}

/// Copy a `struct termios`/`termios2` between userspace and `t`, for the
/// TCGETS/TCSETS family. Returns 0 or `-errno` rather than a `Message` because
/// both callers want a bare status: [`termios_ioctl`] wraps it for the
/// console's IPC reply, and [`pty::ioctl`] returns it directly.
///
/// A command this does not recognise is `-ENOTTY`, so a caller can use the
/// return value to decide whether to keep looking.
///
/// # Safety
/// `arg_ptr` is a userspace pointer; the caller must have validated it and
/// faulted it in (this runs with the pty pool lock held on the pty path).
pub(crate) unsafe fn termios_rw(cmd: usize, arg_ptr: usize, t: &mut Termios) -> isize {
    const TCGETS:     usize = 0x5401;
    const TCSETS:     usize = 0x5402;
    const TCSETSW:    usize = 0x5403;
    const TCSETSF:    usize = 0x5404;
    // termios2 variants (struct termios2 = termios + c_ispeed/c_ospeed).
    // rustix's linux_raw backend uses TCGETS2 for isatty() — answering
    // ENOTTY there made crossterm decide stdin isn't a terminal and fall
    // back to /dev/tty for its input path.
    const TCGETS2:    usize = 0x802C_542A;
    const TCSETS2:    usize = 0x402C_542B;
    const TCSETSW2:   usize = 0x402C_542C;
    const TCSETSF2:   usize = 0x402C_542D;

    if arg_ptr == 0 { return -14; } // EFAULT
    match cmd {
        // struct termios is 36 bytes on Linux; termios2 adds c_ispeed/c_ospeed
        // at offsets 36/40. The leading 36 bytes are identical, so the two
        // getters differ only in that tail.
        TCGETS | TCGETS2 => {
            unsafe {
                core::ptr::write(arg_ptr        as *mut u32, t.c_iflag);
                core::ptr::write((arg_ptr + 4)  as *mut u32, t.c_oflag);
                core::ptr::write((arg_ptr + 8)  as *mut u32, t.c_cflag);
                core::ptr::write((arg_ptr + 12) as *mut u32, t.c_lflag);
                core::ptr::write((arg_ptr + 16) as *mut u8,  t.c_line);
                core::ptr::copy_nonoverlapping(t.c_cc.as_ptr(), (arg_ptr + 17) as *mut u8, 19);
                if cmd == TCGETS2 {
                    core::ptr::write((arg_ptr + 36) as *mut u32, 38400);
                    core::ptr::write((arg_ptr + 40) as *mut u32, 38400);
                }
            }
            0
        }
        TCSETS | TCSETSW | TCSETSF | TCSETS2 | TCSETSW2 | TCSETSF2 => {
            unsafe {
                t.c_iflag = core::ptr::read(arg_ptr        as *const u32);
                t.c_oflag = core::ptr::read((arg_ptr + 4)  as *const u32);
                t.c_cflag = core::ptr::read((arg_ptr + 8)  as *const u32);
                t.c_lflag = core::ptr::read((arg_ptr + 12) as *const u32);
                t.c_line  = core::ptr::read((arg_ptr + 16) as *const u8);
                core::ptr::copy_nonoverlapping((arg_ptr + 17) as *const u8,
                                               t.c_cc.as_mut_ptr(), 19);
                // Speeds ignored — the serial console rate is fixed and a pty
                // has none.
            }
            0
        }
        _ => -25, // ENOTTY
    }
}

/// The console's terminal ioctls: the TCGETS/TCSETS family via [`termios_rw`],
/// plus a TIOCGWINSZ that must come from the framebuffer rather than from `t`.
fn termios_ioctl(cmd: usize, arg_ptr: usize, t: &mut Termios) -> Message {
    const TIOCGWINSZ: usize = 0x5413;

    if cmd == TIOCGWINSZ {
        // struct winsize { ws_row, ws_col, ws_xpixel, ws_ypixel } — 4xu16
        if arg_ptr == 0 { return err_reply(-14); }
        // Report the framebuffer's real cell grid: it is the primary
        // console, and a line editor told 80x24 wraps and repaints against
        // geometry the screen does not have.
        extern "C" { fn kernel_console_winsize(rows: *mut u16, cols: *mut u16); }
        let (mut rows, mut cols) = (24u16, 80u16);
        unsafe { kernel_console_winsize(&mut rows, &mut cols); }
        unsafe {
            core::ptr::write(arg_ptr       as *mut u16, rows);
            core::ptr::write((arg_ptr + 2) as *mut u16, cols);
            core::ptr::write((arg_ptr + 4) as *mut u16, 0);
            core::ptr::write((arg_ptr + 6) as *mut u16, 0);
        }
        return ok_reply();
    }
    make_reply(unsafe { termios_rw(cmd, arg_ptr, t) } as i64)
}

// ── POSIX timer handlers ──────────────────────────────────────────────────────

const NSEC_PER_SEC: u64 = 1_000_000_000;

/// Encode `(interval_ns, value_ns)` as a 32-byte `struct itimerspec`
/// (`{ it_interval: timespec, it_value: timespec }`, each `{ tv_sec, tv_nsec }`
/// as `i64` pairs).
fn itimerspec_bytes(interval_ns: u64, value_ns: u64) -> [u8; 32] {
    let mut buf = [0u8; 32];
    buf[0..8].copy_from_slice(&((interval_ns / NSEC_PER_SEC) as i64).to_ne_bytes());
    buf[8..16].copy_from_slice(&((interval_ns % NSEC_PER_SEC) as i64).to_ne_bytes());
    buf[16..24].copy_from_slice(&((value_ns / NSEC_PER_SEC) as i64).to_ne_bytes());
    buf[24..32].copy_from_slice(&((value_ns % NSEC_PER_SEC) as i64).to_ne_bytes());
    buf
}

/// Write an itimerspec built from `(interval_ticks, value_ticks)` to user
/// memory via the safe user-buffer accessor — never dereference the raw
/// pointer directly, since it may sit on a CoW page a supervisor-mode fault
/// can't recover from (see `read_flock`/`write_flock` in the VFS server for
/// the same pattern, established during the Phase 6/7 hazard sweep).
fn write_itimerspec(ptr: usize, interval_ns: u64, value_ns: u64) -> bool {
    let buf = itimerspec_bytes(interval_ns, value_ns);
    sched::with_current_address_space_mut(|as_| as_.write_user_buf(ptr, &buf)).unwrap_or(false)
}

/// `timer_create`'s clock ids (validated by the kernel's `sys_timer_create`
/// before they get here).
const CLOCK_REALTIME:           u32 = 0;
const CLOCK_PROCESS_CPUTIME_ID: u32 = 2;
const CLOCK_THREAD_CPUTIME_ID:  u32 = 3;
const CLOCK_REALTIME_ALARM:     u32 = 8;
const CLOCK_TAI:                u32 = 11;

fn handle_timer_create(pid: u32, signo: u32, timerid_ptr: usize, sigev_value: u64, target_tid: u32,
                       clockid: u32, caller_tid: u32) -> Message {
    let clock = match clockid {
        CLOCK_PROCESS_CPUTIME_ID => CK_PROF,
        CLOCK_THREAD_CPUTIME_ID  => CK_THREAD,
        _ => CK_MONO,
    };
    let realtime = matches!(clockid, CLOCK_REALTIME | CLOCK_REALTIME_ALARM | CLOCK_TAI);
    let mut tbls = TIMER_TABLES.lock();
    let tbl = match get_or_create_timer_table(pid, &mut *tbls) {
        Some(t) => t, None => return err_reply(-12),
    };
    let slot = match tbl.alloc() { Some(s) => s, None => return err_reply(-11) };
    tbl.timers[slot] = PosixTimer { in_use: true, signo, owner_pid: pid, sigev_value, target_tid,
                                    clock, realtime, cpu_tid: caller_tid, ..PosixTimer::new() };
    drop(tbls);
    if timerid_ptr != 0 {
        // `timer_t` is a pointer-sized opaque handle (8 bytes on both our
        // 64-bit targets) — writing only the low 4 bytes, as this used to,
        // left the caller's high 32 bits uninitialized, so a later
        // timer_settime()/timer_delete() call could pass back a garbage
        // 64-bit value that no longer matches the small in-table index.
        //
        // Handed out as `slot + 1`, never the bare table index: relibc's
        // timer_delete/settime/gettime/getoverrun all reject a NULL
        // `timer_t` as EFAULT, and a raw index of 0 (this process's very
        // first timer) *is* NULL once cast to `*mut c_void`. See
        // `handle_timer_settime`/etc. below, which undo the offset.
        let ok = sched::with_current_address_space_mut(|as_| {
            as_.write_user_buf(timerid_ptr, &((slot as u32) + 1).to_ne_bytes())
        }).unwrap_or(false);
        if !ok {
            // Roll back: the pointer turned out to be unmapped even though
            // it passed the earlier range check, so don't leak the slot.
            let mut tbls = TIMER_TABLES.lock();
            if let Some(tbl) = tbls[..timer_hwm()].iter_mut().find(|t| t.in_use && t.pid == pid) {
                tbl.timers[slot] = PosixTimer::new();
            }
            return err_reply(-14);
        }
    }
    ok_reply()
}

/// The current reading of a CPU-time clock (`CK_PROF`/`CK_VIRT`/`CK_THREAD`)
/// for a timer of process `tgid`. Takes RUN_QUEUE: callers may hold
/// TIMER_TABLES (the established TIMER_TABLES -> RUN_QUEUE order).
fn cpu_clock_now(clock: u8, tgid: u32, cpu_tid: u32) -> u64 {
    match clock {
        CK_THREAD => sched::thread_cpu_ns(cpu_tid),
        CK_VIRT => { let (c, s) = sched::process_cpu_sys_ns(tgid); c.saturating_sub(s) }
        _ => sched::process_cpu_sys_ns(tgid).0,
    }
}

/// Core (re)arm in nanoseconds, decoupled from parsing a user-space
/// itimerspec pointer (`setitimer`/`alarm` come through here from the
/// kernel with values in hand). `abs`: TIMER_ABSTIME — `value_ns` is an
/// instant on the timer's clock (a wall-clock one for a CLOCK_REALTIME
/// timer, translated onto `monotonic_ns` and marked to follow clock steps);
/// an instant already past fires at once. A zero `value_ns` disarms, with or
/// without the flag. Returns the timer's previous `(interval_ns, remaining_ns)`.
fn arm_timer(pid: u32, timerid: usize, interval_ns: u64, value_ns: u64, abs: bool)
    -> Option<(u64, u64)>
{
    let mut tbls = TIMER_TABLES.lock();
    let tbl = tbls[..timer_hwm()].iter_mut().find(|t| t.in_use && t.pid == pid)?;
    if timerid >= NSLOTS || !tbl.timers[timerid].in_use { return None; }
    let t = &mut tbl.timers[timerid];
    let now = if t.clock == CK_MONO { sched::monotonic_ns() } else { cpu_clock_now(t.clock, pid, t.cpu_tid) };
    let old_interval = t.interval;
    let old_remaining = t.deadline.saturating_sub(now);
    t.interval = interval_ns;
    t.overrun = 0;
    t.overrun_last = 0;
    t.abs_rt = false;
    t.deadline = if value_ns == 0 {
        0
    } else if abs {
        let target = if t.clock == CK_MONO && t.realtime {
            t.abs_rt = true;
            sched::realtime_to_monotonic_ns(value_ns)
        } else {
            value_ns
        };
        target.max(1) // past instants are due now; 0 would mean disarmed
    } else {
        now.saturating_add(value_ns)
    };
    let (deadline, clock) = (t.deadline, t.clock);
    if clock != CK_MONO {
        let armed = tbl.cpu_armed();
        sched::cpu_timers_note(pid, armed);
    } else if deadline != 0 {
        // Publish under TIMER_TABLES (the IRQ service recomputes the hint
        // under the same lock, so this cannot be clobbered), then arm this
        // CPU's one-shot timer for it.
        NEXT_TIMER_DEADLINE.fetch_min(deadline, Ordering::Relaxed);
        drop(tbls);
        sched::register_poll_deadline(deadline);
    }
    Some((old_interval, old_remaining))
}

/// Core read in nanoseconds: `(interval_ns, remaining_ns)` on the timer's
/// own clock — see [`arm_timer`].
fn get_timer_ns(pid: u32, timerid: usize) -> Option<(u64, u64)> {
    let tbls = TIMER_TABLES.lock();
    let tbl = tbls[..timer_hwm()].iter().find(|t| t.in_use && t.pid == pid)?;
    if timerid >= NSLOTS || !tbl.timers[timerid].in_use { return None; }
    let t = &tbl.timers[timerid];
    let now = if t.clock == CK_MONO { sched::monotonic_ns() } else { cpu_clock_now(t.clock, pid, t.cpu_tid) };
    // An armed timer that is due but not yet serviced still reads as armed
    // (1 ns), never as "disarmed".
    let remaining = if t.deadline == 0 { 0 } else { t.deadline.saturating_sub(now).max(1) };
    Some((t.interval, remaining))
}

/// `setitimer(which)` slot: ITIMER_REAL 0, ITIMER_VIRTUAL 1, ITIMER_PROF 2.
fn itimer_slot(which: u32) -> Option<usize> {
    match which { 0 => Some(REAL_SLOT), 1 => Some(VIRT_SLOT), 2 => Some(PROF_SLOT), _ => None }
}

/// Direct (non-IPC) API for `setitimer(which, ...)` / `alarm()`: arms the
/// reserved slot of interval timer `which` and returns its previous
/// `(interval_ns, remaining_ns)`, creating the slot on first use. `None`
/// for an unknown `which` (EINVAL).
pub fn set_itimer(pid: u32, which: u32, interval_ns: u64, value_ns: u64) -> Option<(u64, u64)> {
    let pid = sched::tgid_of(pid); // per-process timer table
    let slot = itimer_slot(which)?;
    ensure_itimer(pid, slot);
    Some(arm_timer(pid, slot, interval_ns, value_ns, false).unwrap_or((0, 0)))
}

/// Direct (non-IPC) API for `getitimer(which, ...)`.
pub fn get_itimer(pid: u32, which: u32) -> Option<(u64, u64)> {
    let pid = sched::tgid_of(pid); // per-process timer table
    let slot = itimer_slot(which)?;
    Some(get_timer_ns(pid, slot).unwrap_or((0, 0)))
}

/// `setitimer(ITIMER_REAL)` / `alarm()`.
pub fn set_real_itimer(pid: u32, interval_ns: u64, value_ns: u64) -> (u64, u64) {
    set_itimer(pid, 0, interval_ns, value_ns).unwrap_or((0, 0))
}

/// `getitimer(ITIMER_REAL)` / `alarm()`'s "previous value" query.
pub fn get_real_itimer(pid: u32) -> (u64, u64) {
    get_itimer(pid, 0).unwrap_or((0, 0))
}

/// Fire the due CPU-clock timers (CLOCK_PROCESS_CPUTIME_ID /
/// CLOCK_THREAD_CPUTIME_ID timers, ITIMER_VIRTUAL, ITIMER_PROF) of the
/// thread group running `tid`. Called on its way back to user space
/// (`sched::cpu_timer_check`, which this is registered with), only for
/// groups with such a timer armed: CPU time only advances while one of the
/// group's threads runs, so its own returns to user space — the syscall
/// return and the tick's preemption return alike — are where one can come
/// due. Task context: blocking locks.
pub fn service_cpu_timers(tid: u32) {
    let tgid = sched::tgid_of(tid);
    let (cpu, sys) = sched::process_cpu_sys_ns(tgid);
    let mut fire: [(u32, sched::SigInfo, u32); NSLOTS] = [(0, sched::SigInfo::NONE, 0); NSLOTS];
    let mut nfire = 0;
    {
        let mut tbls = TIMER_TABLES.lock();
        let Some(tbl) = tbls[..timer_hwm()].iter_mut().find(|t| t.in_use && t.pid == tgid) else {
            sched::cpu_timers_note(tgid, false);
            return;
        };
        for (idx, t) in tbl.timers.iter_mut().enumerate() {
            if !t.in_use || t.clock == CK_MONO || t.deadline == 0 { continue; }
            let now = match t.clock {
                CK_PROF => cpu,
                CK_VIRT => cpu.saturating_sub(sys),
                _ => sched::thread_cpu_ns(t.cpu_tid),
            };
            if now < t.deadline { continue; }
            if t.signo != 0 {
                fire[nfire] = (t.signo, timer_info(idx, t), t.target_tid);
                nfire += 1;
            }
            if t.interval > 0 {
                let missed = (now - t.deadline) / t.interval;
                t.deadline += t.interval * (missed + 1);
                t.overrun = t.overrun.saturating_add(missed as u32);
            } else {
                t.deadline = 0;
            }
        }
        let armed = tbl.cpu_armed();
        sched::cpu_timers_note(tgid, armed);
    }
    for &(signo, info, target) in &fire[..nfire] {
        if target != 0 {
            let _ = sched::deliver_signal(target, signo, info);
        } else {
            sched::deliver_signal_process(tgid, signo, info);
        }
    }
}

/// The wall clock was stepped (`clock_settime`/`settimeofday`): move every
/// absolute CLOCK_REALTIME timer so it still expires at its wall-clock
/// instant (wall = mono_old + old_off = mono_new + new_off), as Linux's
/// `clock_was_set` re-programs them. Relative ones keep their interval.
pub fn timers_clock_was_set(old_off: i64, new_off: i64) {
    if old_off == new_off { return; }
    let delta = new_off.wrapping_sub(old_off);
    let mut earliest = u64::MAX;
    {
        let mut tbls = TIMER_TABLES.lock();
        let hwm = timer_hwm();
        for tbl in tbls[..hwm].iter_mut().filter(|t| t.in_use) {
            for t in tbl.timers.iter_mut() {
                if !t.in_use || !t.abs_rt || t.deadline == 0 { continue; }
                t.deadline = (t.deadline as i64).saturating_sub(delta).max(1) as u64;
                earliest = earliest.min(t.deadline);
            }
        }
        if earliest != u64::MAX { NEXT_TIMER_DEADLINE.fetch_min(earliest, Ordering::Relaxed); }
    }
    if earliest != u64::MAX { sched::register_poll_deadline(earliest); }
}

fn handle_timer_settime(pid: u32, timerid: usize, ispec_ptr: usize, ospec_ptr: usize, flags: u64)
    -> Message
{
    // struct itimerspec: { it_interval: timespec, it_value: timespec }
    // struct timespec:   { tv_sec: i64, tv_nsec: i64 } (16 bytes each)
    // Total: 32 bytes, kept in nanoseconds (no tick flooring: a sub-10 ms
    // it_value used to floor to 0 ticks and silently DISARM the timer).
    if ispec_ptr == 0 { return err_reply(-14); }
    if timerid >= MAX_TIMERS { return err_reply(-22); }
    let mut ispec = [0u8; 32];
    let ok = sched::with_current_address_space(|as_| as_.read_user_buf(ispec_ptr, &mut ispec))
        .unwrap_or(false);
    if !ok { return err_reply(-14); }
    let interval_sec  = i64::from_ne_bytes(ispec[0..8].try_into().unwrap());
    let interval_nsec = i64::from_ne_bytes(ispec[8..16].try_into().unwrap());
    let value_sec     = i64::from_ne_bytes(ispec[16..24].try_into().unwrap());
    let value_nsec    = i64::from_ne_bytes(ispec[24..32].try_into().unwrap());

    if interval_sec < 0 || value_sec < 0
        || !(0..1_000_000_000).contains(&interval_nsec)
        || !(0..1_000_000_000).contains(&value_nsec) {
        return err_reply(-22); // EINVAL
    }
    let interval_ns = (interval_sec as u64).saturating_mul(NSEC_PER_SEC)
                        .saturating_add(interval_nsec as u64);
    let value_ns    = (value_sec as u64).saturating_mul(NSEC_PER_SEC)
                        .saturating_add(value_nsec as u64);

    let abs = flags & TIMER_ABSTIME != 0;
    let (old_interval, old_remaining) = match arm_timer(pid, timerid, interval_ns, value_ns, abs) {
        Some(v) => v,
        None => return err_reply(-22),
    };

    if ospec_ptr != 0 && !write_itimerspec(ospec_ptr, old_interval, old_remaining) {
        return err_reply(-14);
    }
    ok_reply()
}

fn handle_timer_gettime(pid: u32, timerid: usize, ospec_ptr: usize) -> Message {
    if ospec_ptr == 0 { return err_reply(-14); }
    if timerid >= MAX_TIMERS { return err_reply(-22); }
    let (interval_ns, remaining) = match get_timer_ns(pid, timerid) {
        Some(v) => v,
        None => return err_reply(-22),
    };
    if write_itimerspec(ospec_ptr, interval_ns, remaining) { ok_reply() } else { err_reply(-14) }
}

/// timer_getoverrun(timerid) — the overrun count of the timer's last
/// dequeued signal (the expirations folded into it), as on Linux; it changes
/// only when the next one is dequeued. It used to report a running count
/// that every call reset.
fn handle_timer_getoverrun(pid: u32, timerid: usize) -> Message {
    let tbls = TIMER_TABLES.lock();
    match tbls[..timer_hwm()].iter().find(|t| t.in_use && t.pid == pid) {
        Some(tbl) if timerid < MAX_TIMERS && tbl.timers[timerid].in_use =>
            val_reply(tbl.timers[timerid].overrun_last.min(i32::MAX as u32) as u64),
        _ => err_reply(-22),
    }
}

/// A signal of POSIX timer `timerid` (the `timer_t` handle, SI_TIMER's
/// `si_tid`) was just dequeued by thread group `tgid` — run through a handler,
/// accepted by sigtimedwait or read from a signalfd. Latches the overrun
/// count for `timer_getoverrun` and returns it for the dequeued siginfo's
/// `si_overrun` (Linux `posixtimer_rearm`). No locks held by the caller.
pub fn timer_dequeued(tgid: u32, timerid: i32) -> i32 {
    let Some(idx) = (timerid as usize).checked_sub(1) else { return 0 };
    if idx >= MAX_TIMERS { return 0; }
    let mut tbls = TIMER_TABLES.lock();
    match tbls[..timer_hwm()].iter_mut().find(|t| t.in_use && t.pid == tgid) {
        Some(tbl) if tbl.timers[idx].in_use => {
            let t = &mut tbl.timers[idx];
            t.overrun_last = t.overrun;
            t.overrun = 0;
            t.overrun_last.min(i32::MAX as u32) as i32
        }
        _ => 0,
    }
}

/// timer_delete(timerid): EINVAL for a handle that names no live timer, as
/// on Linux.
fn handle_timer_delete(pid: u32, timerid: usize) -> Message {
    let mut tbls = TIMER_TABLES.lock();
    match tbls[..timer_hwm()].iter_mut().find(|t| t.in_use && t.pid == pid) {
        Some(tbl) if timerid < MAX_TIMERS && tbl.timers[timerid].in_use => {
            tbl.timers[timerid] = PosixTimer::new();
            let armed = tbl.cpu_armed();
            sched::cpu_timers_note(pid, armed);
            ok_reply()
        }
        _ => err_reply(-22),
    }
}
