//! Pseudo-terminal pairs — the kernel half of `/dev/ptmx` + `/dev/pts/N`.
//!
//! # Why this lives here and not in the VFS
//!
//! A PTY is two rings plus a *line discipline*, and the line discipline is
//! termios — which this crate already owns. The VFS owns fd allocation,
//! dup/fork refcounting and poll, which is why the *vnode* lives there and
//! calls in here: `VnodeKind::Pty { pair, is_master }` is deliberately shaped
//! like `VnodeKind::Pipe { ring, is_write }`, so a PTY inherits every fd-table
//! behaviour a pipe already has (dup2, fork, O_NONBLOCK, epoll, SCM_RIGHTS)
//! instead of needing a second fd class the way the dormant `TTY_FD_BASE`
//! range would have.
//!
//! # Direction naming
//!
//! Two rings, named for who *reads* them, because "input"/"output" flips
//! meaning depending on which end you stand on:
//!
//! * `to_slave` — the master writes here (what the user typed); the slave
//!   reads it. Everything interesting happens on the way in: canonical line
//!   editing, `ISIG`, echo.
//! * `to_master` — the slave writes here (what the program printed); the
//!   master reads it. Only `OPOST` processing applies, plus echo injected
//!   from the input path.
//!
//! # Canonical mode and where a line becomes visible
//!
//! In `ICANON` the bytes the master writes do **not** go into `to_slave`
//! directly — they accumulate in `canon`, and only a line terminator (or
//! `VEOF`) commits the whole line at once. That single invariant is what makes
//! the slave's poll answer honest: `to_slave.count > 0` means "a complete line
//! is readable", which is exactly the POSIX condition, so no separate
//! line-counting state is needed and a canonical reader can never be woken for
//! a half-typed line.
//!
//! # Hangup, and why the two ends report the end differently
//!
//! Closing the last master fd sends `SIGHUP` to the slave's foreground process
//! group *and* makes subsequent slave reads return EOF. Linux returns `EIO`
//! there; we return EOF as well as the signal because a shell exits reliably
//! on EOF, and an `EIO` a shell mishandles becomes a spin in `sys_read`'s
//! retry loop rather than a clean exit.
//!
//! The *master* side is the opposite, and for a concrete reason. When the last
//! slave closes, `master_read` returns `EIO` exactly as Linux does. An earlier
//! draft of this file returned EOF there too, arguing it was friendlier; that
//! reasoning is falsified by the consumer this subsystem exists for.
//! alacritty_terminal — which cosmic-term embeds — registers the master
//! **level-triggered** (`EPOLLIN|EPOLLOUT|EPOLLHUP|EPOLLERR|EPOLLPRI`) and
//! swallows `EIO` on purpose, waiting for `SIGCHLD` to tell it the child is
//! gone (`event_loop.rs:282-290`). A master that reports readable and then
//! returns 0 forever is not friendlier to it — it is an unbreakable 100% CPU
//! spin, because 0 is not an error it stops for and the readiness never
//! clears.

use spin::Mutex;

use crate::Termios;

/// Pairs available system-wide. Each pair is ~24 KiB of ring, so this is the
/// knob that decides the subsystem's BSS footprint; eight covers a terminal
/// with several tabs plus a login session.
pub const MAX_PTYS: usize = 8;

const IN_BUF: usize = 4096;
const OUT_BUF: usize = 16384;
const CANON_BUF: usize = 4096;

// ── termios bits used by the discipline ──────────────────────────────────────

const IGNCR: u32 = 0x0080;
const ICRNL: u32 = 0x0100;
const INLCR: u32 = 0x0040;
const ISTRIP: u32 = 0x0020;

const OPOST: u32 = 0x0001;
const ONLCR: u32 = 0x0004;
const OCRNL: u32 = 0x0008;

const ISIG: u32 = 0x0001;
const ICANON: u32 = 0x0002;
const ECHO: u32 = 0x0008;
const ECHOE: u32 = 0x0010;
const ECHOK: u32 = 0x0020;
const ECHONL: u32 = 0x0040;
const NOFLSH: u32 = 0x0080;
const ECHOCTL: u32 = 0x0200;

// `c_cc` indices, Linux order.
const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VSUSP: usize = 10;
const VEOL: usize = 11;
const VWERASE: usize = 14;
const VEOL2: usize = 16;

// ── poll bits ────────────────────────────────────────────────────────────────

pub const POLLIN: u32 = 0x001;
pub const POLLOUT: u32 = 0x004;
pub const POLLERR: u32 = 0x008;
pub const POLLHUP: u32 = 0x010;

// ── ring ─────────────────────────────────────────────────────────────────────

struct Ring<const N: usize> {
    buf: [u8; N],
    r: usize,
    w: usize,
    count: usize,
}

impl<const N: usize> Ring<N> {
    const fn new() -> Self {
        Self { buf: [0u8; N], r: 0, w: 0, count: 0 }
    }

    fn put(&mut self, b: u8) -> bool {
        if self.count >= N {
            return false;
        }
        self.buf[self.w] = b;
        self.w = (self.w + 1) % N;
        self.count += 1;
        true
    }

    fn get(&mut self) -> Option<u8> {
        if self.count == 0 {
            return None;
        }
        let b = self.buf[self.r];
        self.r = (self.r + 1) % N;
        self.count -= 1;
        Some(b)
    }

    /// Byte at logical offset `i` from the read cursor, without consuming.
    fn peek(&self, i: usize) -> Option<u8> {
        if i >= self.count {
            return None;
        }
        Some(self.buf[(self.r + i) % N])
    }

    fn space(&self) -> usize {
        N - self.count
    }

    fn clear(&mut self) {
        self.r = 0;
        self.w = 0;
        self.count = 0;
    }
}

// ── a pair ───────────────────────────────────────────────────────────────────

struct Pty {
    in_use: bool,
    /// `TIOCSPTLCK` state. A freshly allocated pair is locked; `unlockpt(3)`
    /// clears it, and until then `open("/dev/pts/N")` is EIO. Real programs
    /// (and `openpty(3)`) always call it, so honouring the lock catches a
    /// mis-sequenced open rather than silently working.
    locked: bool,
    /// Open master fds. Reaching zero is a hangup for the slave side.
    master_refs: u32,
    /// Open slave fds. Reaching zero after at least one open is EOF for the
    /// master — that is how a terminal emulator learns its child is gone.
    slave_refs: u32,
    /// Set once any slave fd has ever been opened, so "never opened" and
    /// "opened and all closed" are distinguishable: the first must not look
    /// like EOF to a master that is still waiting for its child to start.
    slave_ever_opened: bool,
    /// Last master fd closed — the slave side is hung up.
    hungup: bool,
    /// A `VEOF` on an empty canonical line. Consumed by one slave read, which
    /// returns 0.
    eof_pending: bool,

    to_slave: Ring<IN_BUF>,
    to_master: Ring<OUT_BUF>,
    canon: [u8; CANON_BUF],
    canon_len: usize,

    termios: Termios,
    /// rows, cols, xpixel, ypixel
    winsize: [u16; 4],
    /// Foreground process group of this terminal (0 = unset).
    pgrp: u32,
    /// Session that has this pty as its controlling terminal (0 = none).
    sid: u32,
    /// Bumped on every state change that can create a poll edge.
    seq: u64,
}

impl Pty {
    const fn new() -> Self {
        Self {
            in_use: false,
            locked: false,
            master_refs: 0,
            slave_refs: 0,
            slave_ever_opened: false,
            hungup: false,
            eof_pending: false,
            to_slave: Ring::new(),
            to_master: Ring::new(),
            canon: [0u8; CANON_BUF],
            canon_len: 0,
            // Deliberately zeroed rather than `Termios::default_console()`:
            // a non-zero const initialiser would put every byte of all eight
            // pairs — rings included — into `.data` instead of `.bss`, which
            // is ~200 KiB of image for nothing. `alloc` installs the real
            // defaults. See TODO item 15 for what an inflated image cost once.
            termios: Termios::zeroed(),
            winsize: [0; 4],
            pgrp: 0,
            sid: 0,
            seq: 0,
        }
    }

    fn reset(&mut self) {
        self.locked = true;
        self.master_refs = 1;
        self.slave_refs = 0;
        self.slave_ever_opened = false;
        self.hungup = false;
        self.eof_pending = false;
        self.to_slave.clear();
        self.to_master.clear();
        self.canon_len = 0;
        self.termios = Termios::default_console();
        self.winsize = [24, 80, 0, 0];
        self.pgrp = 0;
        self.sid = 0;
        self.seq = self.seq.wrapping_add(1);
    }
}

static PTYS: Mutex<[Pty; MAX_PTYS]> = Mutex::new([const { Pty::new() }; MAX_PTYS]);

/// Signals collected under the pool lock and delivered after it is dropped.
///
/// `sched::kill_pgrp` walks the run queue; taking that while holding `PTYS`
/// inverts the lock order every other server observes, and the input path can
/// generate a signal from inside a write. Collect, unlock, then deliver.
struct Pending(Option<(u32, u32)>);

impl Pending {
    fn fire(self) {
        if let Some((pgid, sig)) = self.0 {
            if pgid != 0 {
                let _ = sched::kill_pgrp(pgid, sig);
            }
        }
    }
}

// ── allocation / lifetime ────────────────────────────────────────────────────

/// Allocate a pair for a fresh `open("/dev/ptmx")`. Returns the pty number,
/// which is both the `TIOCGPTN` answer and the `N` in `/dev/pts/N`.
pub fn alloc() -> Option<usize> {
    let mut ptys = PTYS.lock();
    let idx = ptys.iter().position(|p| !p.in_use)?;
    ptys[idx].in_use = true;
    ptys[idx].reset();
    Some(idx)
}

/// True if `n` names a pair with a live master — the precondition for
/// `open("/dev/pts/n")` to resolve at all.
pub fn exists(n: usize) -> bool {
    if n >= MAX_PTYS {
        return false;
    }
    let ptys = PTYS.lock();
    ptys[n].in_use && ptys[n].master_refs > 0
}

/// The pair that owns session `sid` as its controlling terminal.
///
/// This is what `open("/dev/tty")` must resolve to. It matters more than it
/// looks: crossterm — and therefore reedline, and therefore `brush`'s whole
/// line editor — opens `/dev/tty` unconditionally to get a handle it can put
/// in raw mode independently of stdio, and writes its cursor-position query
/// there. Resolving that to the machine console instead of the caller's pty
/// does not fail, it *succeeds at the wrong terminal*: the shell paints its
/// prompt on the framebuffer and waits for a CPR reply that the terminal
/// emulator driving the pty never sees, while the emulator's window stays
/// blank. Only a pair with a live master counts, so a session whose emulator
/// has exited falls back to the console rather than resurrecting a dead pair.
pub fn ctty_for_sid(sid: u32) -> Option<usize> {
    if sid == 0 {
        return None;
    }
    let ptys = PTYS.lock();
    ptys.iter().position(|p| p.in_use && p.master_refs > 0 && p.sid == sid)
}

/// Every allocated pty number, for `getdents64("/dev/pts")`.
pub fn each_allocated(mut f: impl FnMut(usize)) {
    let ptys = PTYS.lock();
    for (i, p) in ptys.iter().enumerate() {
        if p.in_use && p.master_refs > 0 {
            f(i);
        }
    }
}

/// A new fd now refers to this end (dup, fork, SCM_RIGHTS, or the original
/// open).
pub fn add_ref(pair: usize, is_master: bool) {
    if pair >= MAX_PTYS {
        return;
    }
    let mut ptys = PTYS.lock();
    if !ptys[pair].in_use {
        return;
    }
    if is_master {
        ptys[pair].master_refs += 1;
    } else {
        ptys[pair].slave_refs += 1;
    }
}

/// Open the slave end. EIO while the pair is still `TIOCSPTLCK`-locked, ENXIO
/// if the master is gone.
pub fn slave_open(pair: usize) -> Result<(), i32> {
    if pair >= MAX_PTYS {
        return Err(-6); // ENXIO
    }
    let mut ptys = PTYS.lock();
    let p = &mut ptys[pair];
    if !p.in_use || p.master_refs == 0 {
        return Err(-6); // ENXIO
    }
    if p.locked {
        return Err(-5); // EIO — unlockpt(3) was not called
    }
    p.slave_refs += 1;
    p.slave_ever_opened = true;
    p.seq = p.seq.wrapping_add(1);
    Ok(())
}

/// Drop one reference. The last master reference hangs the slave side up; the
/// last reference of either kind once both are gone frees the pair.
pub fn drop_ref(pair: usize, is_master: bool) {
    if pair >= MAX_PTYS {
        return;
    }
    let pending = {
        let mut ptys = PTYS.lock();
        let p = &mut ptys[pair];
        if !p.in_use {
            return;
        }
        if is_master {
            p.master_refs = p.master_refs.saturating_sub(1);
        } else {
            p.slave_refs = p.slave_refs.saturating_sub(1);
        }
        p.seq = p.seq.wrapping_add(1);

        let mut pending = Pending(None);
        if is_master && p.master_refs == 0 && !p.hungup {
            p.hungup = true;
            // The controlling terminal went away: SIGHUP the foreground job.
            // Without this the child of a closed terminal emulator survives
            // as an orphan reading a pty nobody will ever write again.
            pending = Pending(Some((p.pgrp, 1))); // SIGHUP
        }
        if p.master_refs == 0 && p.slave_refs == 0 {
            p.in_use = false;
            p.canon_len = 0;
            p.to_slave.clear();
            p.to_master.clear();
        }
        pending
    };
    pending.fire();
    sched::wake_poll();
}

// ── output side: slave writes, master reads ──────────────────────────────────

/// The slave wrote `count` bytes (a program's stdout). Applies `OPOST`
/// processing and queues them for the master.
///
/// # Safety
/// `buf` must be readable for `count` bytes in the caller's address space.
pub unsafe fn slave_write(pair: usize, buf: *const u8, count: usize) -> isize {
    if pair >= MAX_PTYS {
        return -5;
    }
    let mut ptys = PTYS.lock();
    let p = &mut ptys[pair];
    if !p.in_use {
        return -5; // EIO
    }
    if p.master_refs == 0 {
        // Nothing will ever read this. EPIPE mirrors a pipe with no reader,
        // and the caller's SIGPIPE machinery already understands it.
        return -32;
    }
    let oflag = p.termios.c_oflag;
    let mut n = 0usize;
    while n < count {
        let b = unsafe { *buf.add(n) };
        // Worst case one byte expands to two (ONLCR), so stop while there is
        // room for the expansion rather than truncating mid-translation.
        if p.to_master.space() < 2 {
            break;
        }
        if oflag & OPOST != 0 {
            if b == b'\n' && oflag & ONLCR != 0 {
                p.to_master.put(b'\r');
                p.to_master.put(b'\n');
                n += 1;
                continue;
            }
            if b == b'\r' && oflag & OCRNL != 0 {
                p.to_master.put(b'\n');
                n += 1;
                continue;
            }
        }
        p.to_master.put(b);
        n += 1;
    }
    if n == 0 && count > 0 {
        return -11; // EAGAIN — the master has not drained
    }
    p.seq = p.seq.wrapping_add(1);
    drop(ptys);
    sched::wake_poll();
    n as isize
}

/// The master reads what the slave printed (plus any echo).
///
/// # Safety
/// `buf` must be writable for `count` bytes in the caller's address space.
pub unsafe fn master_read(pair: usize, buf: *mut u8, count: usize) -> isize {
    if pair >= MAX_PTYS || count == 0 {
        return 0;
    }
    let mut ptys = PTYS.lock();
    let p = &mut ptys[pair];
    if !p.in_use {
        return 0;
    }
    if p.to_master.count == 0 {
        // A slave that was opened and then closed is a child that exited.
        // EIO, not EOF, and not for pedantry: alacritty polls this fd
        // level-triggered and treats EIO as "child gone, wait for SIGCHLD",
        // while a 0 leaves it readable-and-empty forever (see the module doc).
        // A slave that never opened yet is just a slow start — retry.
        return if p.slave_refs == 0 && p.slave_ever_opened { -5 } else { -11 };
    }
    let mut n = 0usize;
    while n < count {
        match p.to_master.get() {
            Some(b) => {
                unsafe { *buf.add(n) = b };
                n += 1;
            }
            None => break,
        }
    }
    p.seq = p.seq.wrapping_add(1);
    drop(ptys);
    // Draining frees space — a slave blocked writing a long output has a new
    // POLLOUT edge.
    sched::wake_poll();
    n as isize
}

// ── input side: master writes, slave reads ───────────────────────────────────

/// Echo one byte back toward the master, honouring `ECHOCTL`'s `^X` rendering.
fn echo(p: &mut Pty, b: u8) {
    let lflag = p.termios.c_lflag;
    if b < 0x20 && b != b'\n' && b != b'\t' && b != b'\r' && lflag & ECHOCTL != 0 {
        p.to_master.put(b'^');
        p.to_master.put(b + b'@');
        return;
    }
    if b == 0x7f && lflag & ECHOCTL != 0 {
        p.to_master.put(b'^');
        p.to_master.put(b'?');
        return;
    }
    if b == b'\n' {
        // The slave's OPOST is not on this path, so translate here or the
        // cursor stays in column N and every echoed line staircases.
        p.to_master.put(b'\r');
        p.to_master.put(b'\n');
        return;
    }
    p.to_master.put(b);
}

/// Erase the last character of the pending canonical line, rubbing it out on
/// screen if `ECHOE`.
fn erase_one(p: &mut Pty) {
    if p.canon_len == 0 {
        return;
    }
    let b = p.canon[p.canon_len - 1];
    p.canon_len -= 1;
    if p.termios.c_lflag & ECHO == 0 {
        return;
    }
    if p.termios.c_lflag & ECHOE != 0 {
        // Control characters were echoed as two cells (`^C`), so rubbing one
        // out takes two backspaces or the line drifts.
        let cells = if (b < 0x20 && b != b'\t') || b == 0x7f {
            if p.termios.c_lflag & ECHOCTL != 0 { 2 } else { 1 }
        } else {
            1
        };
        for _ in 0..cells {
            p.to_master.put(0x08);
            p.to_master.put(b' ');
            p.to_master.put(0x08);
        }
    }
}

/// Commit the pending canonical line to the slave's queue.
fn commit_line(p: &mut Pty) {
    for i in 0..p.canon_len {
        if !p.to_slave.put(p.canon[i]) {
            break;
        }
    }
    p.canon_len = 0;
}

/// The master wrote `count` bytes (what the user typed). Runs the full input
/// discipline: `ISIG`, canonical editing, echo, CR/NL translation.
///
/// # Safety
/// `buf` must be readable for `count` bytes in the caller's address space.
pub unsafe fn master_write(pair: usize, buf: *const u8, count: usize) -> isize {
    if pair >= MAX_PTYS {
        return -5;
    }
    let pending;
    let n;
    {
        let mut ptys = PTYS.lock();
        let p = &mut ptys[pair];
        if !p.in_use {
            return -5; // EIO
        }
        let mut sig: Option<(u32, u32)> = None;
        let mut i = 0usize;
        while i < count {
            let iflag = p.termios.c_iflag;
            let lflag = p.termios.c_lflag;
            let cc = p.termios.c_cc;
            let mut b = unsafe { *buf.add(i) };

            if iflag & ISTRIP != 0 {
                b &= 0x7f;
            }
            if b == b'\r' {
                if iflag & IGNCR != 0 {
                    i += 1;
                    continue;
                }
                if iflag & ICRNL != 0 {
                    b = b'\n';
                }
            } else if b == b'\n' && iflag & INLCR != 0 {
                b = b'\r';
            }

            // ── ISIG ──
            if lflag & ISIG != 0 {
                let signo = if cc[VINTR] != 0 && b == cc[VINTR] {
                    Some(2) // SIGINT
                } else if cc[VQUIT] != 0 && b == cc[VQUIT] {
                    Some(3) // SIGQUIT
                } else if cc[VSUSP] != 0 && b == cc[VSUSP] {
                    Some(20) // SIGTSTP
                } else {
                    None
                };
                if let Some(s) = signo {
                    if lflag & NOFLSH == 0 {
                        p.to_slave.clear();
                        p.canon_len = 0;
                    }
                    if lflag & ECHO != 0 {
                        echo(p, b);
                    }
                    // One signal per write is enough: the burst that follows a
                    // held-down ^C would otherwise deliver a dozen.
                    if sig.is_none() {
                        sig = Some((p.pgrp, s));
                    }
                    i += 1;
                    continue;
                }
            }

            if lflag & ICANON != 0 {
                if cc[VERASE] != 0 && b == cc[VERASE] {
                    erase_one(p);
                    i += 1;
                    continue;
                }
                if cc[VKILL] != 0 && b == cc[VKILL] {
                    while p.canon_len > 0 {
                        erase_one(p);
                    }
                    if lflag & (ECHOK | ECHOE) != 0 && lflag & ECHO != 0 {
                        // ECHOE already rubbed the line out above.
                    }
                    i += 1;
                    continue;
                }
                if cc[VWERASE] != 0 && b == cc[VWERASE] {
                    while p.canon_len > 0 && p.canon[p.canon_len - 1] == b' ' {
                        erase_one(p);
                    }
                    while p.canon_len > 0 && p.canon[p.canon_len - 1] != b' ' {
                        erase_one(p);
                    }
                    i += 1;
                    continue;
                }
                if cc[VEOF] != 0 && b == cc[VEOF] {
                    if p.canon_len == 0 {
                        // ^D on an empty line is EOF, and it is *not* echoed —
                        // a shell that printed `^D` before exiting would be
                        // wrong on every terminal.
                        p.eof_pending = true;
                    } else {
                        commit_line(p);
                    }
                    i += 1;
                    continue;
                }
                let is_eol = b == b'\n'
                    || (cc[VEOL] != 0 && b == cc[VEOL])
                    || (cc[VEOL2] != 0 && b == cc[VEOL2]);
                if is_eol {
                    if p.canon_len < CANON_BUF {
                        p.canon[p.canon_len] = b;
                        p.canon_len += 1;
                    }
                    if lflag & ECHO != 0 || lflag & ECHONL != 0 {
                        echo(p, b);
                    }
                    commit_line(p);
                    i += 1;
                    continue;
                }
                if p.canon_len < CANON_BUF {
                    p.canon[p.canon_len] = b;
                    p.canon_len += 1;
                    if lflag & ECHO != 0 {
                        echo(p, b);
                    }
                }
                i += 1;
                continue;
            }

            // ── raw mode ──
            if p.to_slave.space() == 0 {
                break;
            }
            p.to_slave.put(b);
            if lflag & ECHO != 0 {
                echo(p, b);
            }
            i += 1;
        }
        n = i;
        if n > 0 {
            p.seq = p.seq.wrapping_add(1);
        }
        pending = Pending(sig);
    }
    pending.fire();
    if n > 0 {
        sched::wake_poll();
    }
    if n == 0 && count > 0 {
        return -11; // EAGAIN — input queue full, nothing consumed
    }
    n as isize
}

/// The slave reads what the master typed. In canonical mode this stops at the
/// first line terminator, which is what makes `read(0, buf, 4096)` from a
/// shell return one line rather than everything buffered.
///
/// # Safety
/// `buf` must be writable for `count` bytes in the caller's address space.
pub unsafe fn slave_read(pair: usize, buf: *mut u8, count: usize) -> isize {
    if pair >= MAX_PTYS || count == 0 {
        return 0;
    }
    let mut ptys = PTYS.lock();
    let p = &mut ptys[pair];
    if !p.in_use {
        return 0;
    }
    if p.to_slave.count == 0 {
        if p.eof_pending {
            p.eof_pending = false;
            return 0;
        }
        // Hangup reads as end-of-input; the SIGHUP raised at close time is the
        // part that actually terminates the job.
        return if p.hungup || p.master_refs == 0 { 0 } else { -11 };
    }
    let canon = p.termios.c_lflag & ICANON != 0;
    // How many bytes this read may take: everything queued in raw mode, up to
    // and including the first terminator in canonical mode.
    let avail = if canon {
        let mut k = 0usize;
        loop {
            match p.to_slave.peek(k) {
                Some(b) => {
                    k += 1;
                    if b == b'\n'
                        || (p.termios.c_cc[VEOL] != 0 && b == p.termios.c_cc[VEOL])
                        || (p.termios.c_cc[VEOL2] != 0 && b == p.termios.c_cc[VEOL2])
                    {
                        break;
                    }
                }
                None => break,
            }
        }
        k
    } else {
        p.to_slave.count
    };
    let mut n = 0usize;
    while n < count.min(avail) {
        match p.to_slave.get() {
            Some(b) => {
                unsafe { *buf.add(n) = b };
                n += 1;
            }
            None => break,
        }
    }
    p.seq = p.seq.wrapping_add(1);
    drop(ptys);
    sched::wake_poll();
    n as isize
}

// ── poll ─────────────────────────────────────────────────────────────────────

/// Poll mask for one end. Mirrors the read paths exactly, or an epoll consumer
/// wakes on a readability the following `read` denies.
pub fn poll_mask(pair: usize, is_master: bool) -> u32 {
    if pair >= MAX_PTYS {
        return POLLERR;
    }
    let ptys = PTYS.lock();
    let p = &ptys[pair];
    if !p.in_use {
        return POLLERR | POLLHUP;
    }
    let mut mask = 0;
    if is_master {
        if p.to_master.count > 0 {
            mask |= POLLIN;
        }
        if p.slave_refs == 0 && p.slave_ever_opened {
            mask |= POLLIN | POLLHUP;
        }
        if p.to_slave.space() > 0 {
            mask |= POLLOUT;
        }
    } else {
        if p.to_slave.count > 0 || p.eof_pending {
            mask |= POLLIN;
        }
        if p.hungup || p.master_refs == 0 {
            mask |= POLLIN | POLLHUP;
        }
        if p.to_master.space() > 1 {
            mask |= POLLOUT;
        }
    }
    mask
}

/// Bytes a `FIONREAD` on this end would report.
pub fn readable_bytes(pair: usize, is_master: bool) -> usize {
    if pair >= MAX_PTYS {
        return 0;
    }
    let ptys = PTYS.lock();
    let p = &ptys[pair];
    if !p.in_use {
        return 0;
    }
    if is_master {
        p.to_master.count
    } else {
        p.to_slave.count
    }
}

/// Poll-edge counter, for the same edge-triggered epoll bookkeeping pipes use.
pub fn seq(pair: usize) -> u64 {
    if pair >= MAX_PTYS {
        return 0;
    }
    PTYS.lock()[pair].seq
}

// ── ioctl ────────────────────────────────────────────────────────────────────

const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TCSETSW: usize = 0x5403;
const TCSETSF: usize = 0x5404;
const TCSBRK: usize = 0x5409;
const TCXONC: usize = 0x540A;
const TCFLSH: usize = 0x540B;
const TIOCSCTTY: usize = 0x540E;
const TIOCGPGRP: usize = 0x540F;
const TIOCSPGRP: usize = 0x5410;
const TIOCOUTQ: usize = 0x5411;
const TIOCGWINSZ: usize = 0x5413;
const TIOCSWINSZ: usize = 0x5414;
const FIONREAD: usize = 0x541B;
const TIOCNOTTY: usize = 0x5422;
const TIOCGSID: usize = 0x5429;
const TCGETS2: usize = 0x802C_542A;
const TCSETS2: usize = 0x402C_542B;
const TCSETSW2: usize = 0x402C_542C;
const TCSETSF2: usize = 0x402C_542D;
const TIOCGPTN: usize = 0x8004_5430;
const TIOCSPTLCK: usize = 0x4004_5431;
const TIOCGPTLCK: usize = 0x8004_5439;
const TIOCSIG: usize = 0x4004_5436;

/// Terminal ioctls on either end of a pair. Returns 0, a value, or `-errno`.
///
/// # Safety
/// `arg` is a userspace pointer for the commands that take one.
pub unsafe fn ioctl(pair: usize, is_master: bool, cmd: usize, arg: usize) -> isize {
    if pair >= MAX_PTYS {
        return -25; // ENOTTY
    }
    // Sampled before the pool lock is taken. `current_sid`/`current_pgid` both
    // take the scheduler's run-queue lock, and PTYS → RUN_QUEUE is exactly the
    // inversion the `Pending` dance at the end of this function exists to
    // avoid — see its doc comment. Cheap enough to read unconditionally.
    let cur_sid = sched::current_sid();
    let cur_pgid = sched::current_pgid();
    let pending;
    let rc;
    {
        let mut ptys = PTYS.lock();
        let p = &mut ptys[pair];
        if !p.in_use {
            return -25;
        }
        let mut sig: Option<(u32, u32)> = None;
        rc = match cmd {
            TIOCGPTN => {
                if arg == 0 {
                    -14
                } else {
                    unsafe { core::ptr::write(arg as *mut u32, pair as u32) };
                    0
                }
            }
            TIOCSPTLCK => {
                if arg == 0 {
                    -14
                } else {
                    p.locked = unsafe { core::ptr::read(arg as *const i32) } != 0;
                    0
                }
            }
            TIOCGPTLCK => {
                if arg == 0 {
                    -14
                } else {
                    unsafe { core::ptr::write(arg as *mut i32, p.locked as i32) };
                    0
                }
            }
            TIOCGWINSZ => {
                if arg == 0 {
                    -14
                } else {
                    for (i, v) in p.winsize.iter().enumerate() {
                        unsafe { core::ptr::write((arg + i * 2) as *mut u16, *v) };
                    }
                    0
                }
            }
            TIOCSWINSZ => {
                if arg == 0 {
                    -14
                } else {
                    let mut ws = [0u16; 4];
                    for (i, v) in ws.iter_mut().enumerate() {
                        *v = unsafe { core::ptr::read((arg + i * 2) as *const u16) };
                    }
                    let changed = ws != p.winsize;
                    p.winsize = ws;
                    // Resizing a terminal is the entire reason SIGWINCH
                    // exists; a full-screen program that never gets it keeps
                    // drawing to the old geometry.
                    if changed && p.pgrp != 0 {
                        sig = Some((p.pgrp, 28)); // SIGWINCH
                    }
                    0
                }
            }
            TIOCGPGRP => {
                if arg == 0 {
                    -14
                } else {
                    let fg = if p.pgrp == 0 { cur_pgid } else { p.pgrp };
                    unsafe { core::ptr::write(arg as *mut u32, fg) };
                    0
                }
            }
            TIOCSPGRP => {
                if arg == 0 {
                    -14
                } else {
                    p.pgrp = unsafe { core::ptr::read(arg as *const u32) };
                    0
                }
            }
            // ENOTTY on the master end, not a no-op. A terminal emulator holds
            // the master; letting it claim the pair as *its own* controlling
            // terminal would point `ctty_for_sid` at the pty the emulator is
            // driving, so the emulator's own `/dev/tty` would resolve to the
            // slave it feeds — a loop where it types at itself. Linux's ptmx
            // has no TIOCSCTTY either.
            TIOCSCTTY | TIOCNOTTY if is_master => -25,
            TIOCSCTTY => {
                // The slave becomes the caller's controlling terminal, and the
                // caller's process group becomes the foreground job — which is
                // what makes ^C and SIGWINCH have somewhere to go. `login_tty`
                // (and every terminal emulator open-coding it) calls setsid
                // first, so `cur_sid == cur_pgid == the caller's pid` here.
                p.sid = cur_sid;
                p.pgrp = cur_pgid;
                0
            }
            TIOCNOTTY => {
                p.sid = 0;
                p.pgrp = 0;
                0
            }
            TIOCGSID => {
                if arg == 0 {
                    -14
                } else {
                    let sid = if p.sid == 0 { cur_sid } else { p.sid };
                    unsafe { core::ptr::write(arg as *mut u32, sid) };
                    0
                }
            }
            TIOCSIG => {
                // A master asking for a signal on the slave's foreground job.
                if arg == 0 {
                    -14
                } else {
                    let s = unsafe { core::ptr::read(arg as *const i32) };
                    sig = Some((p.pgrp, s as u32));
                    0
                }
            }
            FIONREAD => {
                if arg == 0 {
                    -14
                } else {
                    let n = if is_master { p.to_master.count } else { p.to_slave.count };
                    unsafe { core::ptr::write(arg as *mut i32, n as i32) };
                    0
                }
            }
            TIOCOUTQ => {
                if arg == 0 {
                    -14
                } else {
                    let n = if is_master { p.to_slave.count } else { p.to_master.count };
                    unsafe { core::ptr::write(arg as *mut i32, n as i32) };
                    0
                }
            }
            TCFLSH => {
                // arg is a value, not a pointer: 0=input, 1=output, 2=both.
                match arg {
                    0 => {
                        p.to_slave.clear();
                        p.canon_len = 0;
                    }
                    1 => p.to_master.clear(),
                    _ => {
                        p.to_slave.clear();
                        p.to_master.clear();
                        p.canon_len = 0;
                    }
                }
                0
            }
            // Flow control and break are no-ops on a pty, but they must
            // *succeed*: `tcdrain`/`tcflow` failing with ENOTTY is what makes
            // a line-editing library decide it is not on a terminal.
            TCSBRK | TCXONC => 0,
            TCGETS | TCGETS2 | TCSETS | TCSETSW | TCSETSF | TCSETS2 | TCSETSW2 | TCSETSF2 => {
                if arg == 0 {
                    -14
                } else {
                    unsafe { crate::termios_rw(cmd, arg, &mut p.termios) }
                }
            }
            _ => -25, // ENOTTY
        };
        p.seq = p.seq.wrapping_add(1);
        pending = Pending(sig);
    }
    pending.fire();
    rc
}

/// The pty's current window size, for a `TIOCGWINSZ` answered elsewhere.
pub fn winsize(pair: usize) -> (u16, u16) {
    if pair >= MAX_PTYS {
        return (24, 80);
    }
    let ptys = PTYS.lock();
    (ptys[pair].winsize[0], ptys[pair].winsize[1])
}
