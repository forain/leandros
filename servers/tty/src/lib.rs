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
//! deadline (in scheduler ticks) checked on every syscall return and yielded
//! tick.  Expiry fires by calling `sched::deliver_signal`.
//!
//! # Message encoding
//!
//! Arguments packed as little-endian u64 words in Message.data[], as in VFS.

#![no_std]

use ipc::Message;
use spin::Mutex;

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

const MAX_PROCS:  usize = 64;
const MAX_TIMERS: usize = 8;   // per process POSIX timers

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

static CONSOLE_TERMIOS: Mutex<[ConsoleTermios; MAX_PROCS]> =
    Mutex::new([const { ConsoleTermios::empty() }; MAX_PROCS]);

/// Foreground process group of the console tty (0 = none set yet).
/// Maintained by TIOCSPGRP/TIOCSCTTY; consulted by the input-drain
/// intercept (`console_intercept_byte`) and TIOCGPGRP.
static CONSOLE_FG_PGID: Mutex<u32> = Mutex::new(0);

/// The console's current foreground process group (0 = unset).
pub fn console_fg_pgid() -> u32 { *CONSOLE_FG_PGID.lock() }

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
    let pgid = *CONSOLE_FG_PGID.lock();
    if pgid == 0 { return false; }
    let (lflag, cc) = {
        let tbl = CONSOLE_TERMIOS.lock();
        match tbl.iter().find(|c| c.in_use && c.pid == pgid) {
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
    let _ = sched::kill_pgrp(pgid, sig);
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
            unsafe { core::ptr::write(arg_ptr as *mut u32, fg); }
            Some(ok_reply())
        }
        TIOCSPGRP => {
            if arg_ptr == 0 { return Some(err_reply(-14)); }
            let pgid = unsafe { core::ptr::read(arg_ptr as *const u32) };
            *CONSOLE_FG_PGID.lock() = pgid;
            Some(ok_reply())
        }
        TIOCSCTTY => {
            // Acquiring the controlling terminal also makes the caller's
            // process group foreground (matches how shells use it).
            *CONSOLE_FG_PGID.lock() = sched::current_pgid();
            Some(ok_reply())
        }
        TIOCGSID => {
            if arg_ptr == 0 { return Some(err_reply(-14)); }
            unsafe { core::ptr::write(arg_ptr as *mut u32, sched::current_sid()); }
            Some(ok_reply())
        }
        _ => None,
    }
}

fn get_or_create_console<'a>(pid: u32, tbl: &'a mut [ConsoleTermios]) -> Option<&'a mut ConsoleTermios> {
    if let Some(pos) = tbl.iter().position(|t| t.in_use && t.pid == pid) {
        return Some(&mut tbl[pos]);
    }
    let pos = tbl.iter().position(|t| !t.in_use)?;
    tbl[pos] = ConsoleTermios { pid, in_use: true, termios: Termios::default_console() };
    Some(&mut tbl[pos])
}

// ── POSIX timer table ─────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct PosixTimer {
    in_use:    bool,
    signo:     u32,
    interval:  u64, // repeat interval in ticks (0 = one-shot)
    deadline:  u64, // absolute tick deadline (0 = disarmed)
    overrun:   u32, // extra expirations missed since last timer_getoverrun()
    owner_pid: u32,
}

impl PosixTimer {
    const fn new() -> Self {
        Self { in_use: false, signo: 0, interval: 0, deadline: 0, overrun: 0, owner_pid: 0 }
    }
}

#[derive(Clone, Copy)]
struct ProcTimerTable {
    pid:    u32,
    timers: [PosixTimer; MAX_TIMERS],
    in_use: bool,
}

impl ProcTimerTable {
    const fn empty() -> Self {
        Self { pid: 0, timers: [const { PosixTimer::new() }; MAX_TIMERS], in_use: false }
    }

    fn alloc(&mut self) -> Option<usize> {
        self.timers.iter().position(|t| !t.in_use)
    }
}

static TIMER_TABLES: Mutex<[ProcTimerTable; MAX_PROCS]> =
    Mutex::new([const { ProcTimerTable::empty() }; MAX_PROCS]);

/// Find `pid`'s timer table, allocating a fresh one if this is its first timer.
/// Returns `None` only if every one of `MAX_PROCS` process slots is already
/// in use by a different pid.
fn get_or_create_timer_table<'a>(pid: u32, tbls: &'a mut [ProcTimerTable]) -> Option<&'a mut ProcTimerTable> {
    if let Some(pos) = tbls.iter().position(|t| t.in_use && t.pid == pid) {
        return Some(&mut tbls[pos]);
    }
    let pos = tbls.iter().position(|t| !t.in_use)?;
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
                                             arg(msg,1) as usize),
        // `timer_t` handles are `slot + 1` (see handle_timer_create) so a
        // valid handle never numerically equals NULL; undo that here at the
        // single IPC boundary that decodes a caller-supplied timer_t.
        TIMER_SETTIME => match decode_timerid(arg(msg,0)) {
            Some(id) => handle_timer_settime(caller_pid, id, arg(msg,1) as usize, arg(msg,2) as usize),
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
    let now = sched::ticks();
    let mut tbls = TIMER_TABLES.lock();
    let tbl = match tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
        Some(t) => t, None => return,
    };
    for timer in tbl.timers.iter_mut() {
        if !timer.in_use || timer.deadline == 0 { continue; }
        if now >= timer.deadline {
            sched::deliver_signal(timer.owner_pid, timer.signo);
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

/// Ensure `pid` has its reserved slot-0 timer armed for `signo`, without
/// creating a fresh one if it already exists.  Used by `alarm()` and
/// `setitimer(ITIMER_REAL)`, both of which model a single per-process timer
/// that repeated calls rearm rather than a fresh POSIX timer each time —
/// calling `handle_timer_create` unconditionally would leak a new slot (out
/// of `MAX_TIMERS`) on every call, since it always allocates the first free
/// slot rather than reusing slot 0.
pub fn ensure_real_timer(pid: u32, signo: u32) {
    let pid = sched::tgid_of(pid); // per-process timer table
    let mut tbls = TIMER_TABLES.lock();
    let tbl = match get_or_create_timer_table(pid, &mut *tbls) {
        Some(t) => t, None => return,
    };
    if !tbl.timers[0].in_use {
        tbl.timers[0] = PosixTimer { in_use: true, signo, interval: 0, deadline: 0,
                                     overrun: 0, owner_pid: pid };
    }
}

/// Drop all terminal and timer state for a process on exit.
pub fn close_all(pid: u32) {
    let mut timers = TIMER_TABLES.lock();
    if let Some(t) = timers.iter_mut().find(|t| t.in_use && t.pid == pid) {
        *t = ProcTimerTable::empty();
    }
    let mut console = CONSOLE_TERMIOS.lock();
    if let Some(c) = console.iter_mut().find(|c| c.in_use && c.pid == pid) {
        *c = ConsoleTermios::empty();
    }
}

// ── Terminal ioctl handler ────────────────────────────────────────────────────

fn handle_ioctl(pid: u32, fd: usize, cmd: usize, arg_ptr: usize) -> Message {
    // Termios records are per-process: thread siblings must see the same
    // console state (same tgid canonicalization as the VFS/net servers).
    let pid = sched::tgid_of(pid);
    if let Some(r) = jobctl_ioctl(cmd, arg_ptr) { return r; }
    if fd <= 2 {
        // stdin/stdout/stderr have no fd-table entry at all — see ConsoleTermios.
        let mut console = CONSOLE_TERMIOS.lock();
        let c = match get_or_create_console(pid, &mut *console) {
            Some(c) => c, None => return err_reply(-25),
        };
        return termios_ioctl(cmd, arg_ptr, &mut c.termios);
    }

    // Nothing above fd 2 reaches here as a terminal: pty ends are dispatched by
    // `sys_ioctl` straight to `pty::ioctl`, and everything else genuinely is
    // not a terminal.
    err_reply(-25) // ENOTTY
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

const TICK_HZ: u64 = 100;
const NSEC_PER_TICK: u64 = 1_000_000_000 / TICK_HZ;

/// Encode `(interval_ticks, value_ticks)` as a 32-byte `struct itimerspec`
/// (`{ it_interval: timespec, it_value: timespec }`, each `{ tv_sec, tv_nsec }`
/// as `i64` pairs).
fn itimerspec_bytes(interval_ticks: u64, value_ticks: u64) -> [u8; 32] {
    let mut buf = [0u8; 32];
    buf[0..8].copy_from_slice(&((interval_ticks / TICK_HZ) as i64).to_ne_bytes());
    buf[8..16].copy_from_slice(&(((interval_ticks % TICK_HZ) * NSEC_PER_TICK) as i64).to_ne_bytes());
    buf[16..24].copy_from_slice(&((value_ticks / TICK_HZ) as i64).to_ne_bytes());
    buf[24..32].copy_from_slice(&(((value_ticks % TICK_HZ) * NSEC_PER_TICK) as i64).to_ne_bytes());
    buf
}

/// Write an itimerspec built from `(interval_ticks, value_ticks)` to user
/// memory via the safe user-buffer accessor — never dereference the raw
/// pointer directly, since it may sit on a CoW page a supervisor-mode fault
/// can't recover from (see `read_flock`/`write_flock` in the VFS server for
/// the same pattern, established during the Phase 6/7 hazard sweep).
fn write_itimerspec(ptr: usize, interval_ticks: u64, value_ticks: u64) -> bool {
    let buf = itimerspec_bytes(interval_ticks, value_ticks);
    sched::with_current_address_space(|as_| as_.write_user_buf(ptr, &buf)).unwrap_or(false)
}

fn handle_timer_create(pid: u32, signo: u32, timerid_ptr: usize) -> Message {
    let mut tbls = TIMER_TABLES.lock();
    let tbl = match get_or_create_timer_table(pid, &mut *tbls) {
        Some(t) => t, None => return err_reply(-12),
    };
    let slot = match tbl.alloc() { Some(s) => s, None => return err_reply(-11) };
    tbl.timers[slot] = PosixTimer { in_use: true, signo, interval: 0, deadline: 0,
                                    overrun: 0, owner_pid: pid };
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
        let ok = sched::with_current_address_space(|as_| {
            as_.write_user_buf(timerid_ptr, &((slot as u64) + 1).to_ne_bytes())
        }).unwrap_or(false);
        if !ok {
            // Roll back: the pointer turned out to be unmapped even though
            // it passed the earlier range check, so don't leak the slot.
            let mut tbls = TIMER_TABLES.lock();
            if let Some(tbl) = tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
                tbl.timers[slot] = PosixTimer::new();
            }
            return err_reply(-14);
        }
    }
    ok_reply()
}

/// Core rearm logic in tick units, decoupled from parsing a user-space
/// itimerspec pointer.  Kernel-internal callers (`alarm()`/`setitimer()`,
/// via [`set_real_itimer`]) already have tick counts in hand and must not
/// round-trip through a synthetic *user*-space pointer to reach this, since
/// [`write_itimerspec`]/`read_user_buf` resolve addresses through the
/// current task's own page tables. Returns the timer's previous
/// `(interval_ticks, remaining_ticks)` on success.
fn set_timer_ticks(pid: u32, timerid: usize, interval_ticks: u64, value_ticks: u64)
    -> Option<(u64, u64)>
{
    let mut tbls = TIMER_TABLES.lock();
    let tbl = tbls.iter_mut().find(|t| t.in_use && t.pid == pid)?;
    if timerid >= MAX_TIMERS || !tbl.timers[timerid].in_use { return None; }
    let old_interval = tbl.timers[timerid].interval;
    let now = sched::ticks();
    let old_remaining = {
        let dl = tbl.timers[timerid].deadline;
        if dl > now { dl - now } else { 0 }
    };
    tbl.timers[timerid].interval = interval_ticks;
    tbl.timers[timerid].deadline = if value_ticks > 0 { now + value_ticks } else { 0 };
    tbl.timers[timerid].overrun = 0;
    Some((old_interval, old_remaining))
}

/// Core read logic in tick units — see [`set_timer_ticks`] for why this is
/// split out from the user-pointer-parsing IPC handler.
fn get_timer_ticks(pid: u32, timerid: usize) -> Option<(u64, u64)> {
    let tbls = TIMER_TABLES.lock();
    let tbl = tbls.iter().find(|t| t.in_use && t.pid == pid)?;
    if timerid >= MAX_TIMERS || !tbl.timers[timerid].in_use { return None; }
    let interval_ticks = tbl.timers[timerid].interval;
    let now = sched::ticks();
    let remaining = {
        let dl = tbl.timers[timerid].deadline;
        if dl > now { dl - now } else { 0 }
    };
    Some((interval_ticks, remaining))
}

/// Direct (non-IPC) API for `setitimer(ITIMER_REAL, ...)`: arms the
/// reserved slot-0 timer and returns its previous `(interval_ticks,
/// remaining_ticks)`, creating the slot on first use.
pub fn set_real_itimer(pid: u32, interval_ticks: u64, value_ticks: u64) -> (u64, u64) {
    let pid = sched::tgid_of(pid); // per-process timer table
    ensure_real_timer(pid, 14 /* SIGALRM */);
    set_timer_ticks(pid, 0, interval_ticks, value_ticks).unwrap_or((0, 0))
}

/// Direct (non-IPC) API for `getitimer(ITIMER_REAL, ...)` / `alarm()`'s
/// "previous value" query.
pub fn get_real_itimer(pid: u32) -> (u64, u64) {
    let pid = sched::tgid_of(pid); // per-process timer table
    get_timer_ticks(pid, 0).unwrap_or((0, 0))
}

fn handle_timer_settime(pid: u32, timerid: usize, ispec_ptr: usize, ospec_ptr: usize)
    -> Message
{
    // struct itimerspec: { it_interval: timespec, it_value: timespec }
    // struct timespec:   { tv_sec: i64, tv_nsec: i64 } (16 bytes each)
    // Total: 32 bytes.  We convert to scheduler ticks (100 Hz assumed).
    if ispec_ptr == 0 { return err_reply(-14); }
    let mut ispec = [0u8; 32];
    let ok = sched::with_current_address_space(|as_| as_.read_user_buf(ispec_ptr, &mut ispec))
        .unwrap_or(false);
    if !ok { return err_reply(-14); }
    let interval_sec  = i64::from_ne_bytes(ispec[0..8].try_into().unwrap());
    let interval_nsec = i64::from_ne_bytes(ispec[8..16].try_into().unwrap());
    let value_sec     = i64::from_ne_bytes(ispec[16..24].try_into().unwrap());
    let value_nsec    = i64::from_ne_bytes(ispec[24..32].try_into().unwrap());

    let interval_ticks = (interval_sec as u64 * TICK_HZ)
                       + (interval_nsec as u64 / (1_000_000_000 / TICK_HZ));
    let value_ticks    = (value_sec as u64 * TICK_HZ)
                       + (value_nsec as u64 / (1_000_000_000 / TICK_HZ));

    let (old_interval, old_remaining) = match set_timer_ticks(pid, timerid, interval_ticks, value_ticks) {
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
    let (interval_ticks, remaining) = match get_timer_ticks(pid, timerid) {
        Some(v) => v,
        None => return err_reply(-22),
    };
    if write_itimerspec(ospec_ptr, interval_ticks, remaining) { ok_reply() } else { err_reply(-14) }
}

/// timer_getoverrun(timerid) — number of extra expirations folded into the
/// last delivered signal since this was last queried; resets to 0 on read.
fn handle_timer_getoverrun(pid: u32, timerid: usize) -> Message {
    let mut tbls = TIMER_TABLES.lock();
    match tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
        Some(tbl) if timerid < MAX_TIMERS && tbl.timers[timerid].in_use => {
            let overrun = tbl.timers[timerid].overrun;
            tbl.timers[timerid].overrun = 0;
            val_reply(overrun as u64)
        }
        _ => err_reply(-22),
    }
}

fn handle_timer_delete(pid: u32, timerid: usize) -> Message {
    let mut tbls = TIMER_TABLES.lock();
    if let Some(tbl) = tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
        if timerid < MAX_TIMERS { tbl.timers[timerid] = PosixTimer::new(); }
    }
    ok_reply()
}
